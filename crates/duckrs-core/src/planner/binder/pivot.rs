// Ported from duckdb/src/planner/binder/tableref/bind_pivot.cpp @ rust-port-baseline
//
//! PIVOT and UNPIVOT, as queries over their source. DuckDB switches to a pivot operator over
//! `list()` aggregates past `pivot_filter_threshold` values; that is only faster, so here every
//! pivot is a set of filtered aggregates.

use super::star::{deduplicate_columns, literal_text, value_text};
use super::{Binder, binder_error};
use crate::execution::expression_executor::evaluate_scalar;
use crate::function::BuiltinFunctions;
use crate::planner::logical::LogicalOperator;
use duckrs_common::Result;
use duckrs_common::types::LogicalTypeId;
use duckrs_common::value::Value;
use duckrs_parser::ast::{
    CastType, ComparisonOp, Expr, ExprKind, FunctionExpr, Literal, OperatorType, PivotColumn, PivotRef, QueryNode,
    SelectNode, SelectStatement, TableRef, TableRefKind,
};
use duckrs_parser::transformer::visit_expr_mut;
use std::collections::HashSet;

const PLACEHOLDER: &str = "__internal_pivot_aggregate_placeholder";

/// `PivotValueElement`.
#[derive(Clone, Default)]
struct PivotValue {
    values: Vec<Value>,
    name: String,
}

/// An entry of an IN list once its values are known.
struct PivotEntry {
    values: Vec<Value>,
    alias: String,
}

fn is_aggregate(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Function(f) if BuiltinFunctions::get().aggregate(&f.name.name).is_some())
}

/// `ExtractPivotAggregateExpression`: takes the aggregates out of `expr`, leaving placeholders.
fn extract_aggregates(expr: &mut Expr) -> Result<Vec<Expr>> {
    let mut aggregates = Vec::new();
    let mut column = false;
    visit_expr_mut(expr, &mut |e| {
        if is_aggregate(e) {
            let index = aggregates.len();
            let placeholder = Expr::column(vec![PLACEHOLDER.to_string(), index.to_string()]);
            aggregates.push(std::mem::replace(e, placeholder));
        } else if matches!(e.kind, ExprKind::ColumnRef(_)) {
            column = true;
        }
    });
    let mut unplaced = false;
    visit_expr_mut(expr, &mut |e| {
        if matches!(&e.kind, ExprKind::ColumnRef(names) if names[0] != PLACEHOLDER) {
            unplaced = true;
        }
    });
    if unplaced {
        return Err(binder_error(
            "Columns can only be referenced within the aggregate of a PIVOT expression",
        ));
    }
    Ok(aggregates)
}

fn restore_aggregate(expr: &mut Expr, aggregate: &Expr) {
    let alias = expr.alias.clone();
    visit_expr_mut(expr, &mut |e| {
        if matches!(&e.kind, ExprKind::ColumnRef(names) if names[0] == PLACEHOLDER) {
            *e = aggregate.clone();
        }
    });
    if !alias.is_empty() {
        expr.alias = alias;
    }
}

pub(crate) fn has_subquery(expr: &Expr) -> bool {
    let mut found = false;
    visit_expr_mut(&mut expr.clone(), &mut |e| {
        found |= matches!(e.kind, ExprKind::Subquery { .. })
    });
    found
}

fn has_window(expr: &Expr) -> bool {
    let mut found = false;
    visit_expr_mut(&mut expr.clone(), &mut |e| {
        found |= matches!(e.kind, ExprKind::Window(_))
    });
    found
}

/// `ExtractPivotExpressions`.
fn handle_columns(expr: &Expr, handled: &mut HashSet<String>) -> Result<()> {
    let mut qualified = false;
    visit_expr_mut(&mut expr.clone(), &mut |e| {
        if let ExprKind::ColumnRef(names) = &e.kind {
            qualified |= names.len() > 1;
            handled.insert(names.last().expect("a name").to_lowercase());
        }
    });
    if qualified {
        return Err(binder_error("PIVOT expression cannot contain qualified columns"));
    }
    Ok(())
}

/// `ConstructPivots`.
fn construct_pivots(
    columns: &[Vec<PivotEntry>],
    index: usize,
    current: &PivotValue,
    result: &mut Vec<PivotValue>,
) -> Result<()> {
    let last = index + 1 == columns.len();
    for entry in &columns[index] {
        let mut value = current.clone();
        let mut name = entry.alias.clone();
        for item in &entry.values {
            value.values.push(item.clone());
            if entry.alias.is_empty() {
                if !name.is_empty() {
                    name.push('_');
                }
                name += &value_text(item)?;
            }
        }
        value.name = if current.name.is_empty() {
            name
        } else {
            format!("{}_{name}", current.name)
        };
        if value.name.is_empty() {
            value.name = "''".to_string();
        }
        if last {
            result.push(value);
        } else {
            construct_pivots(columns, index + 1, &value, result)?;
        }
    }
    Ok(())
}

fn column_name(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::ColumnRef(names) => names.last().cloned().expect("a name"),
        _ => expr.alias.clone(),
    }
}

/// `ConstructInitialGrouping`.
fn initial_grouping(pivot: &PivotRef, source: TableRef, all_columns: &[Expr], handled: &HashSet<String>) -> SelectNode {
    let mut select = SelectNode {
        from_table: source,
        ..SelectNode::default()
    };
    let names: Vec<String> = if pivot.groups.is_empty() {
        all_columns
            .iter()
            .map(column_name)
            .filter(|n| !handled.contains(&n.to_lowercase()))
            .collect()
    } else {
        pivot.groups.clone()
    };
    for (i, name) in names.iter().enumerate() {
        select
            .groups
            .group_expressions
            .push(Expr::constant(Literal::integer(i as i64 + 1)));
        select.select_list.push(Expr::column(vec![name.clone()]));
    }
    if !names.is_empty() {
        select.groups.grouping_sets.push((0..names.len()).collect());
    }
    select
}

/// `TryExtractUnpivotList`.
fn unpivot_list(expr: &Expr, names: &mut Vec<String>) -> bool {
    let initial = names.len();
    match &expr.kind {
        ExprKind::ColumnRef(columns) if columns.len() == 1 => {
            names.push(columns[0].clone());
            true
        }
        ExprKind::Constant(literal) => {
            names.push(literal_text(literal));
            true
        }
        ExprKind::Function(f) if f.name.name == "row" && !f.args.is_empty() => {
            for argument in &f.args {
                if !unpivot_list(&argument.expr, names) {
                    names.truncate(initial);
                    return false;
                }
            }
            true
        }
        _ => false,
    }
}

/// `ExtractUnpivotColumnName`.
fn unpivot_column_names(expr: &Expr, names: &mut Vec<String>) -> Result<()> {
    if has_subquery(expr) {
        return Err(binder_error("UNPIVOT list cannot contain subqueries"));
    }
    visit_expr_mut(&mut expr.clone(), &mut |e| {
        if let ExprKind::ColumnRef(columns) = &e.kind {
            names.push(columns.last().expect("a name").clone());
        }
    });
    Ok(())
}

struct UnpivotEntry {
    alias: String,
    column_names: Vec<String>,
    expressions: Vec<Expr>,
}

fn select_statement(node: SelectNode) -> SelectStatement {
    let node: QueryNode = node.into();
    SelectStatement { node }
}

impl Binder<'_> {
    /// `BindPivotConstantInList` + `BindPivotInList`.
    fn pivot_in_list(&self, expr: &Expr, values: &mut Vec<Value>) -> Result<()> {
        match &expr.kind {
            ExprKind::ColumnRef(names) => {
                if names.len() > 1 {
                    return Err(binder_error("PIVOT IN list cannot contain qualified column references"));
                }
                values.push(Value::varchar(names[0].clone()));
            }
            ExprKind::Function(f) if f.name.name == "row" => {
                for argument in &f.args {
                    self.pivot_in_list(&argument.expr, values)?;
                }
            }
            _ => {
                let mut binder = Binder::new(self.catalog);
                binder.parameters = self.parameters.clone();
                let bound = binder.bind_expression(expr)?;
                if !bound.is_foldable() {
                    return Err(binder_error("PIVOT IN list must contain constant expressions"));
                }
                values.push(evaluate_scalar(&bound)?);
            }
        }
        Ok(())
    }

    /// The entries of a pivot column, from its IN list or its ENUM.
    fn pivot_entries(&self, column: &PivotColumn) -> Result<Vec<PivotEntry>> {
        let mut entries = Vec::with_capacity(column.entries.len());
        for entry in &column.entries {
            let mut values: Vec<Value> = entry
                .values
                .iter()
                .map(|literal| match literal.kind {
                    duckrs_parser::ast::LiteralKind::Null => Value::sql_null(),
                    _ => Value::varchar(literal.text.clone()),
                })
                .collect();
            if values.is_empty() {
                let Some(expr) = &entry.expr else {
                    return Err(binder_error("PIVOT IN list - expected an expression or value"));
                };
                self.pivot_in_list(expr, &mut values)?;
            }
            if values.len() != column.pivot_expressions.len() {
                return Err(binder_error(format!(
                    "PIVOT IN list - inconsistent amount of rows - expected {} but got {}",
                    column.pivot_expressions.len(),
                    values.len()
                )));
            }
            entries.push(PivotEntry {
                values,
                alias: entry.alias.clone(),
            });
        }
        if !column.pivot_enum.is_empty() {
            let search = self.search_schema.as_deref();
            let Some(ty) = self.catalog.get_type(None, search, &column.pivot_enum)? else {
                // a built-in type: ENUM without values is incomplete, the others are no ENUM
                if column.pivot_enum.eq_ignore_ascii_case("enum") {
                    return Err(binder_error("ENUM type is incomplete"));
                }
                if duckrs_parser::ast::is_builtin_type_name(&column.pivot_enum) {
                    return Err(binder_error(format!(
                        "Pivot must reference an ENUM type: {} is of type \"{}\"",
                        column.pivot_enum,
                        column.pivot_enum.to_uppercase()
                    )));
                }
                return Err(crate::catalog::missing_entry("Type", &column.pivot_enum));
            };
            if ty.id() != LogicalTypeId::Enum {
                return Err(binder_error(format!(
                    "Pivot must reference an ENUM type: {} is of type \"{ty}\"",
                    column.pivot_enum
                )));
            }
            for value in ty.enum_values() {
                entries.push(PivotEntry {
                    values: vec![Value::varchar(value.clone())],
                    alias: value.clone(),
                });
            }
        }
        let mut seen = HashSet::new();
        for entry in &entries {
            let text = if entry.values.len() == 1 {
                value_text(&entry.values[0])?
            } else {
                let items = entry.values.iter().map(value_text).collect::<Result<Vec<_>>>()?;
                format!("[{}]", items.join(", "))
            };
            // NULL is not the string 'NULL'
            let nulls: Vec<bool> = entry.values.iter().map(Value::is_null).collect();
            if !seen.insert((text.clone(), nulls)) {
                return Err(binder_error(format!(
                    "The value \"{text}\" was specified multiple times in the IN clause"
                )));
            }
        }
        Ok(entries)
    }

    /// `Binder::BindPivot`: a filtered aggregate per pivot value and aggregate.
    fn bind_pivot_node(&self, pivot: &PivotRef, source: TableRef, all_columns: &[Expr]) -> Result<SelectNode> {
        let mut handled = HashSet::new();
        let mut templates = Vec::with_capacity(pivot.aggregates.len());
        for aggregate in &pivot.aggregates {
            if has_subquery(aggregate) {
                return Err(binder_error("Pivot expression cannot contain subqueries"));
            }
            if has_window(aggregate) {
                return Err(binder_error("Pivot expression cannot contain window functions"));
            }
            let mut template = aggregate.clone();
            let found = extract_aggregates(&mut template)?;
            if found.len() != 1 {
                let detail = match found.len() {
                    0 => "but no aggregates were found".to_string(),
                    n => format!("but {n} were found"),
                };
                return Err(binder_error(format!(
                    "Pivot expression must contain exactly one aggregate, {detail}"
                )));
            }
            handle_columns(&found[0], &mut handled)?;
            templates.push((template, found.into_iter().next().expect("one aggregate")));
        }
        // `BindBoundPivot`: identical aggregates are computed once, so they need one name
        let mut distinct: Vec<Expr> = Vec::new();
        for aggregate in &pivot.aggregates {
            let mut unnamed = aggregate.clone();
            unnamed.alias.clear();
            if !distinct.contains(&unnamed) {
                distinct.push(unnamed);
            }
        }
        // an aggregate without an alias is named by its expression
        let mut names: Vec<String> = Vec::new();
        for (aggregate, unnamed) in pivot.aggregates.iter().map(|a| {
            let mut unnamed = a.clone();
            unnamed.alias.clear();
            (a, unnamed)
        }) {
            let name = if aggregate.alias.is_empty() {
                unnamed.to_string()
            } else {
                aggregate.alias.clone()
            };
            if !names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                names.push(name);
            }
        }
        if distinct.len() < pivot.aggregates.len() && names.len() != distinct.len() {
            return Err(binder_error(
                "PIVOT: identical aggregates must have the same name - use a single aggregate, or make the \
                 aggregates differ",
            ));
        }
        let mut columns = Vec::with_capacity(pivot.pivots.len());
        let mut total: u128 = 1;
        for column in &pivot.pivots {
            let entries = self.pivot_entries(column)?;
            total = total.saturating_mul(entries.len() as u128);
            for expr in &column.pivot_expressions {
                handle_columns(expr, &mut handled)?;
            }
            columns.push(entries);
        }
        let limit = self.options.pivot_limit;
        if total >= limit as u128 {
            return Err(binder_error(format!(
                "Pivot column limit of {limit} exceeded. Use SET pivot_limit=X to increase the limit."
            )));
        }
        let mut pivot_values = Vec::new();
        construct_pivots(&columns, 0, &PivotValue::default(), &mut pivot_values)?;

        let mut select = initial_grouping(pivot, source, all_columns, &handled);
        let expressions: Vec<&Expr> = pivot.pivots.iter().flat_map(|p| &p.pivot_expressions).collect();
        for pivot_value in &pivot_values {
            let mut conditions = Vec::with_capacity(expressions.len());
            for (expr, value) in expressions.iter().zip(&pivot_value.values) {
                let literal = if value.is_null() {
                    Expr::cast(Expr::constant(Literal::null()), CastType::Builtin("VARCHAR"))
                } else {
                    Expr::constant(Literal::string(value.try_to_string()?))
                };
                conditions.push(Expr::comparison(
                    ComparisonOp::NotDistinctFrom,
                    Expr::cast((*expr).clone(), CastType::Builtin("VARCHAR")),
                    literal,
                ));
            }
            let filter: Option<Expr> = match conditions.len() {
                0 => None,
                1 => conditions.pop(),
                _ => Some(
                    ExprKind::Conjunction {
                        and: true,
                        children: conditions,
                    }
                    .into(),
                ),
            };
            for (original, (template, aggregate)) in pivot.aggregates.iter().zip(&templates) {
                let mut aggregate = aggregate.clone();
                if let (ExprKind::Function(function), Some(filter)) = (&mut aggregate.kind, &filter) {
                    function.filter = Some(Box::new(match function.filter.take() {
                        Some(existing) => ExprKind::Conjunction {
                            and: true,
                            children: vec![*existing, filter.clone()],
                        }
                        .into(),
                        None => filter.clone(),
                    }));
                }
                let mut expr = template.clone();
                restore_aggregate(&mut expr, &aggregate);
                let mut name = pivot_value.name.clone();
                if pivot.aggregates.len() > 1 || !original.alias.is_empty() {
                    name.push('_');
                    if original.alias.is_empty() {
                        name += &original.to_string();
                    } else {
                        name += &original.alias;
                    }
                }
                expr.alias = name;
                select.select_list.push(expr);
            }
        }
        Ok(select)
    }

    /// `Binder::ExtractUnpivotEntries`.
    fn unpivot_entries(&self, star_binder: &Binder, pivot: &PivotColumn) -> Result<Vec<UnpivotEntry>> {
        let mut result = Vec::new();
        for entry in &pivot.entries {
            let expr = entry.expr.as_ref();
            let mut names = Vec::new();
            for value in &entry.values {
                names.push(literal_text(value));
            }
            if expr.is_none_or(|e| unpivot_list(e, &mut names)) {
                let mut expressions = Vec::with_capacity(names.len());
                for name in names {
                    if name.is_empty() {
                        return Err(binder_error("UNPIVOT - empty column name not supported"));
                    }
                    expressions.push(Expr::column(vec![name]));
                }
                result.push(UnpivotEntry {
                    alias: entry.alias.clone(),
                    column_names: Vec::new(),
                    expressions,
                });
                continue;
            }
            let mut expanded = Vec::new();
            star_binder.expand_star_expression(expr.expect("checked above"), &mut expanded)?;
            for expr in expanded {
                result.push(UnpivotEntry {
                    alias: expr.alias.clone(),
                    column_names: Vec::new(),
                    expressions: vec![expr],
                });
            }
        }
        Ok(result)
    }

    /// `Binder::BindUnpivot`: the columns as lists, unnested.
    fn bind_unpivot_node(
        &self,
        star_binder: &Binder,
        pivot: &PivotRef,
        table_ref: &TableRef,
        source: TableRef,
        all_columns: Vec<Expr>,
    ) -> Result<SelectNode> {
        let mut select_node = SelectNode {
            from_table: source,
            ..SelectNode::default()
        };
        let unpivot = &pivot.pivots[0];
        let mut entries = self.unpivot_entries(star_binder, unpivot)?;
        if entries.is_empty() {
            return Err(binder_error(
                "UNPIVOT clause must unpivot on at least one column - zero were provided",
            ));
        }
        let mut handled: Vec<String> = Vec::new();
        for entry in &mut entries {
            for expr in &entry.expressions {
                let mut names = Vec::new();
                unpivot_column_names(expr, &mut names)?;
                if names.is_empty() {
                    return Err(binder_error(format!(
                        "UNPIVOT clause must contain exactly one column - expression \"{expr}\" does not contain any"
                    )));
                }
                if names.len() > 1 {
                    return Err(binder_error(format!(
                        "UNPIVOT clause must contain exactly one column - expression \"{expr}\" contains multiple ({})",
                        names.join(", ")
                    )));
                }
                let name = names.remove(0);
                if !handled.iter().any(|h| h.eq_ignore_ascii_case(&name)) {
                    handled.push(name.clone());
                }
                entry.column_names.push(name);
            }
        }
        let mut select_names = Vec::new();
        let mut name_map: Vec<String> = Vec::new();
        for column in all_columns {
            let name = column_name(&column);
            match handled.iter().position(|h| h.eq_ignore_ascii_case(&name)) {
                None => {
                    select_names.push(name);
                    select_node.select_list.push(column);
                }
                Some(position) => {
                    handled.remove(position);
                    name_map.push(name);
                }
            }
        }
        if let Some(missing) = handled.first() {
            return Err(binder_error(format!(
                "Column {missing} referenced in UNPIVOT but no matching entry was found in the table"
            )));
        }
        let mut unpivot_names = Vec::with_capacity(entries.len());
        for entry in &entries {
            let mapped: Vec<String> = entry
                .column_names
                .iter()
                .map(|c| {
                    name_map
                        .iter()
                        .find(|n| n.eq_ignore_ascii_case(c))
                        .cloned()
                        .unwrap_or_else(|| c.clone())
                })
                .collect();
            unpivot_names.push(if entry.alias.is_empty() {
                mapped.join("_")
            } else {
                entry.alias.clone()
            });
        }
        let width = entries[0].expressions.len();
        if let Some(entry) = entries.iter().find(|e| e.expressions.len() != width) {
            return Err(binder_error(format!(
                "UNPIVOT value count mismatch - entry has {} values, but expected all entries to have {width} values",
                entry.expressions.len()
            )));
        }
        let lists: Vec<Vec<Expr>> = (0..width)
            .map(|i| entries.iter().map(|e| e.expressions[i].clone()).collect())
            .collect();
        let column_count = select_names.len();
        select_names.push("unpivot_names".to_string());
        for i in 0..lists.len() {
            select_names.push(if i > 0 {
                format!("unpivot_list_{}", i + 1)
            } else {
                "unpivot_list".to_string()
            });
        }
        deduplicate_columns(&mut select_names);
        let names_list: Vec<Expr> = unpivot_names
            .into_iter()
            .map(|n| Expr::constant(Literal::string(n)))
            .collect();
        select_node.select_list.push(
            Expr::from(FunctionExpr::new("list_value", names_list)).with_alias(select_names[column_count].clone()),
        );
        if pivot.unpivot_names.len() != lists.len() {
            return Err(binder_error(format!(
                "UNPIVOT name count mismatch - got {} names but {} expressions",
                pivot.unpivot_names.len(),
                lists.len()
            )));
        }
        for (i, list) in lists.into_iter().enumerate() {
            select_node.select_list.push(
                Expr::from(FunctionExpr::new("unpivot_list", list))
                    .with_alias(select_names[column_count + 1 + i].clone()),
            );
        }
        let mut subquery = TableRef::subquery(select_statement(select_node));
        subquery.alias = "unpivot".to_string();
        let mut result_node = SelectNode {
            from_table: subquery,
            ..SelectNode::default()
        };
        for name in &select_names[..column_count] {
            result_node.select_list.push(Expr::column(vec![name.clone()]));
        }
        let exclude_nulls = !pivot.include_nulls;
        let unnest =
            |name: &str| -> Expr { FunctionExpr::new("unnest", vec![Expr::column(vec![name.to_string()])]).into() };
        let output_name = unpivot.unpivot_names[0].clone();
        result_node
            .select_list
            .push(unnest(&select_names[column_count]).with_alias(if exclude_nulls {
                "__unpivot_name".to_string()
            } else {
                output_name.clone()
            }));
        let mut value_aliases = Vec::new();
        for i in 0..width {
            let name = table_ref
                .column_name_alias
                .get(i)
                .unwrap_or(&pivot.unpivot_names[i])
                .clone();
            let value = unnest(&select_names[column_count + 1 + i]);
            if exclude_nulls {
                let internal = format!("__unpivot_value_{i}");
                result_node.select_list.push(value.with_alias(internal.clone()));
                value_aliases.push((internal, name));
            } else {
                result_node.select_list.push(value.with_alias(name));
            }
        }
        if !exclude_nulls {
            return Ok(result_node);
        }
        let mut filtered = SelectNode::default();
        for name in &select_names[..column_count] {
            filtered.select_list.push(Expr::column(vec![name.clone()]));
        }
        filtered
            .select_list
            .push(Expr::column(vec!["__unpivot_name".to_string()]).with_alias(output_name));
        let mut conditions = Vec::new();
        for (internal, name) in value_aliases {
            filtered
                .select_list
                .push(Expr::column(vec![internal.clone()]).with_alias(name));
            conditions.push(Expr::operator(
                OperatorType::IsNotNull,
                vec![Expr::column(vec![internal])],
            ));
        }
        filtered.where_clause = Some(Box::new(if conditions.len() == 1 {
            conditions.remove(0)
        } else {
            ExprKind::Conjunction {
                and: true,
                children: conditions,
            }
            .into()
        }));
        let mut inner = TableRef::subquery(select_statement(result_node));
        inner.alias = "unpivot_unnested".to_string();
        filtered.from_table = inner;
        Ok(filtered)
    }

    /// `Binder::Bind(PivotRef)`.
    pub(super) fn bind_pivot(&mut self, table_ref: &TableRef, pivot: &PivotRef) -> Result<LogicalOperator> {
        let mut source = (*pivot.source).clone();
        if matches!(source.kind, TableRefKind::Subquery(_)) && source.alias.is_empty() {
            source.alias = format!("__internal_pivot_alias_{}", self.generate_table_index());
        }
        let mut star_binder = self.child_binder();
        let correlated = self.correlated.lock().len();
        star_binder.bind_table_ref(&source)?;
        // `BindBoundPivot`: the source of a PIVOT reads no columns of an enclosing query
        if !pivot.aggregates.is_empty() && self.correlated.lock().len() > correlated {
            return Err(binder_error("PIVOT is not supported in correlated subqueries yet"));
        }
        let mut all_columns = Vec::new();
        star_binder.expand_star_expression(&Expr::star(), &mut all_columns)?;
        let node = if pivot.aggregates.is_empty() {
            self.bind_unpivot_node(&star_binder, pivot, table_ref, source, all_columns)?
        } else {
            self.bind_pivot_node(pivot, source, &all_columns)?
        };
        let mut binder = self.child_binder();
        let bound = binder.bind_select_statement(&select_statement(node))?;
        self.next_table_index = binder.next_table_index;
        let alias = if table_ref.alias.is_empty() {
            "__unnamed_pivot".to_string()
        } else {
            table_ref.alias.clone()
        };
        self.add_query_binding(alias, bound, &table_ref.column_name_alias)
    }
}
