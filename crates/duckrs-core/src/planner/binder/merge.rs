// Ported from duckdb/src/planner/binder/statement/bind_merge_into.cpp @ rust-port-baseline
//
//! MERGE INTO. The source joins the target, and each action is bound against what its condition
//! makes visible: the source, the target, or both.

use super::expression::checked_cast;
use super::join::{conjunction_of, extract_join_conditions, primary_binding, using_binding};
use super::{BindContext, Binder, BoundStatement, StatementReturnType, UsingColumnSet, binder_error};
use crate::catalog::TableCatalogEntry;
use crate::planner::expression::{BoundExpr, ColumnBinding};
use crate::planner::logical::{BoundMergeAction, LogicalOperator, LogicalOperatorKind};
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{
    ComparisonOp, Expr, ExprKind, JoinType, MergeAction, MergeActionType, MergeCondition, MergeStatement,
    UpdateSetInfo, sql_quoted_identifier,
};

/// `CheckMergeAction`.
fn check_merge_action(condition: MergeCondition, action: MergeActionType) -> Result<()> {
    if condition == MergeCondition::WhenNotMatchedByTarget
        && matches!(action, MergeActionType::Update | MergeActionType::Delete)
    {
        return Err(Error::new(
            ErrorKind::Parser,
            "WHEN NOT MATCHED (BY TARGET) cannot be combined with UPDATE or DELETE actions - as there is no \
             corresponding row in the target to update or delete.\nDid you mean to use WHEN MATCHED or WHEN NOT \
             MATCHED BY SOURCE?",
        ));
    }
    Ok(())
}

fn condition_text(condition: MergeCondition) -> &'static str {
    match condition {
        MergeCondition::WhenMatched => "WHEN MATCHED",
        MergeCondition::WhenNotMatchedByTarget => "WHEN NOT MATCHED",
        MergeCondition::WhenNotMatchedBySource => "WHEN NOT MATCHED BY SOURCE",
    }
}

/// The columns of a side of the join.
fn source_columns(context: &BindContext) -> Vec<(String, String)> {
    context
        .bindings
        .iter()
        .flat_map(|b| b.names.iter().map(|n| (b.alias.clone(), n.clone())))
        .collect()
}

fn column_references(columns: &[(String, String)]) -> Vec<Expr> {
    columns
        .iter()
        .map(|(alias, name)| Expr::column(vec![alias.clone(), name.clone()]))
        .collect()
}

impl Binder<'_> {
    /// `BindInsertColumnList`: the stored column each listed name fills.
    fn merge_insert_columns(&self, table: &TableCatalogEntry, names: &[String]) -> Result<Vec<usize>> {
        let mut result: Vec<usize> = Vec::with_capacity(names.len());
        for (i, name) in names.iter().enumerate() {
            if names[..i].iter().any(|n| n.eq_ignore_ascii_case(name)) {
                return Err(binder_error(format!(
                    "Duplicate column name {} in INSERT",
                    sql_quoted_identifier(name)
                )));
            }
            if table.is_generated(name) {
                return Err(binder_error("Cannot insert into a generated column"));
            }
            let Some(index) = table.column_index(name) else {
                // `TableCatalogEntry::GetColumnIndex`
                let names: Vec<String> = table.logical_columns().iter().map(|c| c.name.to_string()).collect();
                return Err(binder_error(format!(
                    "Table {} does not have a column with name {}\n{}",
                    sql_quoted_identifier(&table.name),
                    sql_quoted_identifier(name),
                    duckrs_common::string_util::candidates_error_message(&names, name, "Did you mean")
                )));
            };
            result.push(index);
        }
        Ok(result)
    }

    /// `Binder::BindMergeAction`.
    fn bind_merge_action(
        &mut self,
        table: &TableCatalogEntry,
        condition: MergeCondition,
        action: &MergeAction,
        source: &[(String, String)],
    ) -> Result<BoundMergeAction> {
        let bound_condition = match &action.condition {
            Some(condition) => {
                // `ProjectionBinder` for the "WHERE clause" of the action
                let errors = (
                    std::mem::replace(&mut self.window_error, "WHERE clause cannot contain window functions!"),
                    std::mem::replace(
                        &mut self.aggregate_error,
                        "WHERE clause cannot contain aggregate functions",
                    ),
                );
                let bound = self.bind_expression(condition);
                (self.window_error, self.aggregate_error) = errors;
                Some(bound?.cast_to(&LogicalType::BOOLEAN, false))
            }
            None => None,
        };
        let mut text = condition_text(condition).to_string();
        if let Some(condition) = &action.condition {
            text += &format!(" AND {condition}");
        }
        let mut result = BoundMergeAction {
            action_type: action.action_type,
            condition: bound_condition,
            text,
            columns: Vec::new(),
            expressions: Vec::new(),
        };
        match action.action_type {
            MergeActionType::Update => {
                let info = match &action.update_info {
                    Some(info) => info.clone(),
                    None if action.by_name => UpdateSetInfo {
                        columns: source.iter().map(|(_, n)| n.clone()).collect(),
                        expressions: column_references(source),
                        condition: None,
                    },
                    None => {
                        if source.len() != table.columns.len() {
                            return Err(binder_error(format!(
                                "Data provided for UPDATE did not match column count in table - expected {} columns \
                                 but got {}",
                                table.columns.len(),
                                source.len()
                            )));
                        }
                        UpdateSetInfo {
                            columns: table.columns.iter().map(|c| c.name.clone()).collect(),
                            expressions: column_references(source),
                            condition: None,
                        }
                    }
                };
                for (name, value) in info.columns.iter().zip(&info.expressions) {
                    if table.is_generated(name) {
                        return Err(binder_error(format!(
                            "Cant update column \"{name}\" because it is a generated column!"
                        )));
                    }
                    let Some(index) = table.column_index(name) else {
                        // `BindUpdateSet`
                        let names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
                        return Err(binder_error(format!(
                            "Referenced update column {name} not found in table!\n{}",
                            duckrs_common::string_util::candidates_error_message(&names, name, "Did you mean")
                        )));
                    };
                    if result.columns.contains(&index) {
                        return Err(binder_error(format!("Multiple assignments to same column \"{name}\"")));
                    }
                    result.columns.push(index);
                    let column = &table.columns[index];
                    let bound = if matches!(value.kind, ExprKind::Default) {
                        match &column.default_value {
                            Some(default) => default.clone(),
                            None => BoundExpr::constant(Value::null(column.ty.clone())),
                        }
                    } else {
                        self.bind_expression(value)?
                    };
                    result.expressions.push(checked_cast(bound, &column.ty)?);
                }
            }
            MergeActionType::Insert => {
                let names: Vec<String> = if action.by_name {
                    source.iter().map(|(_, n)| n.clone()).collect()
                } else {
                    action.insert_columns.clone()
                };
                let listed = self.merge_insert_columns(table, &names)?;
                let default_values = action.default_values;
                let expressions = if !default_values && action.expressions.is_empty() {
                    column_references(source)
                } else {
                    action.expressions.clone()
                };
                let expected = if names.is_empty() {
                    table.columns.len()
                } else {
                    listed.len()
                };
                if !default_values && expected != expressions.len() {
                    let what = if names.is_empty() {
                        format!("table {} has {expected} columns", table.name)
                    } else {
                        format!(
                            "Column name/value mismatch for insert on {}: expected {expected} columns",
                            table.name
                        )
                    };
                    return Err(binder_error(format!(
                        "{what} but {} values were supplied",
                        expressions.len()
                    )));
                }
                let targets: Vec<usize> = if names.is_empty() {
                    (0..table.columns.len()).collect()
                } else {
                    listed
                };
                let mut values: Vec<Option<BoundExpr>> = vec![None; table.columns.len()];
                if !default_values {
                    for (target, expr) in targets.iter().zip(&expressions) {
                        let column = &table.columns[*target];
                        if matches!(expr.kind, ExprKind::Default) {
                            continue;
                        }
                        values[*target] = Some(checked_cast(self.bind_expression(expr)?, &column.ty)?);
                    }
                }
                for (value, column) in values.into_iter().zip(&table.columns) {
                    result.expressions.push(value.unwrap_or_else(|| {
                        column
                            .default_value
                            .clone()
                            .unwrap_or_else(|| BoundExpr::constant(Value::null(column.ty.clone())))
                    }));
                }
            }
            MergeActionType::Error => {
                for expr in &action.expressions {
                    let bound = self.bind_expression(expr)?;
                    result.expressions.push(bound.cast_to(&LogicalType::VARCHAR, false));
                }
            }
            MergeActionType::Delete | MergeActionType::DoNothing => {}
        }
        Ok(result)
    }

    /// `Binder::BindNode(MergeQueryNode)`.
    pub(super) fn bind_merge(&mut self, merge: &MergeStatement) -> Result<BoundStatement> {
        self.with_ctes(&merge.cte_map, |binder| binder.bind_merge_body(merge))
    }

    fn bind_merge_body(&mut self, merge: &MergeStatement) -> Result<BoundStatement> {
        let (target_plan, table, row_id) = self.bind_write_target(&merge.target, "Can only merge into base tables!")?;
        let target_context = std::mem::take(&mut self.bind_context);
        let mut source_plan = self.bind_table_ref(&merge.source)?;
        let source_context = std::mem::take(&mut self.bind_context);
        let source = source_columns(&source_context);

        let conditions = [
            MergeCondition::WhenNotMatchedByTarget,
            MergeCondition::WhenNotMatchedBySource,
            MergeCondition::WhenMatched,
        ];
        let actions_of = |condition: MergeCondition| merge.actions.iter().filter(move |(c, _)| *c == condition);
        let by_source = actions_of(MergeCondition::WhenNotMatchedBySource).next().is_some();
        let by_target = actions_of(MergeCondition::WhenNotMatchedByTarget).next().is_some();
        let join_type = match (by_source, by_target) {
            (true, true) => JoinType::Outer,
            (true, false) => JoinType::Right,
            (false, true) => JoinType::Left,
            (false, false) => JoinType::Inner,
        };

        // the marker tells a source row from the NULLs of an outer join
        let mut marker = None;
        if by_source {
            let table_index = self.generate_table_index();
            marker = Some(BoundExpr::column_ref(
                ColumnBinding::new(table_index, 0),
                LogicalType::INTEGER,
                "source_marker".to_string(),
            ));
            let row = LogicalOperator::new(
                LogicalOperatorKind::ExpressionGet {
                    function: None,
                    volatile: false,
                    table_index,
                    types: vec![LogicalType::INTEGER],
                    expressions: vec![vec![BoundExpr::constant(Value::integer(42))]],
                },
                vec![],
            );
            source_plan = LogicalOperator::new(LogicalOperatorKind::CrossProduct, vec![source_plan, row]);
        }

        let mut left_context = source_context.clone();
        let mut right_context = target_context.clone();
        let mut new_sets = Vec::new();
        let mut using_predicates = Vec::new();
        for column in &merge.using_columns {
            let left_alias = using_binding(&left_context, None, column, "left")?;
            let right_alias = using_binding(&right_context, None, column, "right")?;
            let left_column = self.bind_using_side(&mut left_context, &left_alias, column)?;
            let right_column = self.bind_using_side(&mut right_context, &right_alias, column)?;
            using_predicates.push(super::expression::bind_comparison(
                ComparisonOp::Equal,
                left_column,
                right_column,
            )?);
            let bindings = [&left_alias, &right_alias].into_iter().flatten().cloned().collect();
            new_sets.push(UsingColumnSet {
                column: column.clone(),
                primary: primary_binding(join_type, &left_alias, &right_alias),
                bindings,
            });
        }
        let mut predicates = using_predicates;
        self.bind_context.using_columns = new_sets;
        self.bind_context.add_context(source_context.clone());
        self.bind_context.add_context(target_context.clone());
        let joined = std::mem::take(&mut self.bind_context);
        self.bind_context = joined.clone();
        if let Some(condition) = &merge.join_condition {
            let bound = self.bind_expression(condition)?;
            predicates.insert(0, checked_cast(bound, &LogicalType::BOOLEAN)?);
        }

        let mut actions = Vec::new();
        for condition in conditions {
            let mut bound = Vec::new();
            self.bind_context = match condition {
                MergeCondition::WhenNotMatchedByTarget => source_context.clone(),
                MergeCondition::WhenNotMatchedBySource => target_context.clone(),
                MergeCondition::WhenMatched => joined.clone(),
            };
            for (_, action) in actions_of(condition) {
                check_merge_action(condition, action.action_type)?;
                bound.push(self.bind_merge_action(&table, condition, action, &source)?);
            }
            if !bound.is_empty() {
                actions.push((condition, bound));
            }
        }
        self.bind_context = joined;

        if predicates.is_empty() {
            predicates.push(BoundExpr::constant(Value::boolean(true)));
        }
        let (join_conditions, predicate) = extract_join_conditions(predicates, &source_plan, &target_plan);
        let predicate = predicate.into_iter().collect::<Vec<_>>();
        let plan = LogicalOperator::new(
            LogicalOperatorKind::Join {
                join_type,
                conditions: join_conditions,
                predicate: conjunction_of(predicate),
                algorithm: Default::default(),
            },
            vec![source_plan, target_plan],
        );
        let table_index = self.generate_table_index();
        let returning = !merge.returning.is_empty();
        let plan = LogicalOperator::new(
            LogicalOperatorKind::MergeInto {
                table: table.clone(),
                table_index,
                actions,
                row_id: BoundExpr::column_ref(row_id, LogicalType::BIGINT, "rowid".to_string()),
                marker,
                returning,
            },
            vec![plan],
        );
        if returning {
            let alias = Self::write_alias(&merge.target, &table);
            self.merge_action_column = true;
            let result = self.bind_returning(plan, &merge.returning, &table, table_index, alias);
            self.merge_action_column = false;
            return result;
        }
        Ok(BoundStatement {
            plan,
            names: vec!["Count".to_string()],
            types: vec![LogicalType::BIGINT],
            column_aliases: Vec::new(),
            return_type: StatementReturnType::ChangedRows,
        })
    }
}
