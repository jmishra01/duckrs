// Ported from duckdb/src/parser/peg/transformer/{transform_pivot,transform_merge_into}.cpp, the pivot and
// NEAREST parts of transform_select.cpp and PEGTransformer::CreatePivotStatement @ rust-port-baseline
//
//! PIVOT / UNPIVOT (statements and table operators), MERGE INTO and NEAREST joins.

use super::super::{Transformer, not_implemented, parser_error};
use super::{has_subquery, is_scalar};
use crate::ast::*;
use crate::matcher::ParseNode;
use pintail_common::{Error, ErrorKind, Result};
use std::sync::atomic::{AtomicU64, Ordering};

/// A PIVOT column whose values are read from the data: it becomes a `CREATE TYPE ... AS ENUM`
/// statement that runs before the pivot (`CreatePivotEntry`).
pub(in crate::transformer) struct PivotEntry {
    enum_name: String,
    base: SelectNode,
    base_cte_map: CteMap,
    column: Expr,
    subquery: Option<Box<QueryNode>>,
    has_parameters: bool,
}

/// A unique name for a pivot ENUM. DuckDB uses a random UUID; a process-wide counter formatted as
/// one keeps the names unique and the output reproducible.
fn pivot_enum_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("__pivot_enum_00000000-0000-4000-8000-{n:012x}")
}

impl Transformer<'_> {
    /// Transforms a top-level statement: pivots that read their values from the data are
    /// preceded by the statements creating their ENUM types (`CreatePivotStatement`).
    pub fn top_level_statement(&mut self, node: &ParseNode) -> Result<Statement> {
        self.param_count = 0;
        self.named_params.clear();
        self.pivot_entries.clear();
        self.cte_stack.clear();
        self.window_clauses.clear();
        self.expression_depth = 0;
        self.stack_base = crate::matcher::stack_position();
        let statement = self.statement(node)?;
        if self.pivot_entries.is_empty() {
            return Ok(statement);
        }
        let mut statements = Vec::new();
        for entry in std::mem::take(&mut self.pivot_entries) {
            if entry.has_parameters {
                return Err(parser_error(format!(
                    "PIVOT statements with pivot elements extracted from the data cannot have parameters in their \
                     source.\nIn order to use parameters the PIVOT values must be manually specified, e.g.:\nPIVOT \
                     ... ON {} IN (val1, val2, ...)",
                    entry.column
                )));
            }
            statements.push(create_enum_statement(entry));
        }
        statements.push(statement);
        Ok(Statement::Multi(statements))
    }

    /// `PivotEntryCheck`: statements that are stored cannot run the extra ENUM statements.
    pub(in crate::transformer) fn pivot_entry_check(&self, kind: &str) -> Result<()> {
        if let Some(entry) = self.pivot_entries.first() {
            return Err(parser_error(format!(
                "PIVOT statements with pivot elements extracted from the data cannot be used in {kind}s.\nIn order \
                 to use PIVOT in a {kind} the PIVOT values must be manually specified, e.g.:\nPIVOT ... ON {} IN \
                 (val1, val2, ...)",
                entry.column
            )));
        }
        Ok(())
    }

    /// `ExtractCTEsRecursive`: the CTEs in scope, innermost first.
    fn ctes_in_scope(&self) -> CteMap {
        let mut map = CteMap::default();
        for scope in self.cte_stack.iter().rev() {
            for (name, info) in &scope.0 {
                if !map.0.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
                    map.0.push((name.clone(), info.clone()));
                }
            }
        }
        map
    }

    /// `TransformPivotInList`: column names and rows of them become constant values.
    fn pivot_in_list(&self, expr: &Expr, entry: &mut PivotColumnEntry) -> Result<bool> {
        let initial = entry.values.len();
        match &expr.kind {
            ExprKind::ColumnRef(names) => {
                if names.len() > 1 {
                    return Err(parser_error("PIVOT IN list cannot contain qualified column references"));
                }
                entry.values.push(Literal::string(names[0].clone()));
                Ok(true)
            }
            ExprKind::Function(f) if f.name.name == "row" && !f.args.is_empty() => {
                for arg in &f.args {
                    if !self.pivot_in_list(&arg.expr, entry)? {
                        entry.values.truncate(initial);
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// `PivotValueList <- PivotHeader 'IN' PivotValueTarget`.
    fn pivot_value_list(&mut self, node: &ParseNode) -> Result<PivotColumn> {
        let mut column = PivotColumn::default();
        let target = self.alt(self.req(node, "PivotValueTarget")?)?;
        if self.name(target) == "PivotEnumTarget" {
            column.pivot_enum = Self::text(target);
        } else {
            let list = self.req(self.req(target, "PivotTargetList")?, "TargetList")?;
            for target in self.target_list(list)? {
                let mut entry = PivotColumnEntry {
                    values: Vec::new(),
                    expr: None,
                    alias: target.alias.clone(),
                };
                if !self.pivot_in_list(&target, &mut entry)? || entry.values.is_empty() {
                    entry.expr = Some(target);
                }
                column.entries.push(entry);
            }
        }
        let header = self.expr(self.req(node, "PivotHeader")?)?;
        let is_row = |e: &Expr| matches!(&e.kind, ExprKind::Function(f) if f.name.name == "row");
        match header.kind {
            ExprKind::Function(f) if f.name.name == "row" => {
                let tuples = column
                    .entries
                    .iter()
                    .any(|e| e.values.len() > 1 || e.expr.as_ref().is_some_and(is_row));
                if tuples {
                    column.pivot_expressions.extend(f.args.into_iter().map(|a| a.expr));
                } else {
                    column.pivot_expressions.push(Expr {
                        kind: ExprKind::Function(f),
                        alias: header.alias,
                    });
                }
            }
            kind => column.pivot_expressions.push(Expr {
                kind,
                alias: header.alias,
            }),
        }
        Ok(column)
    }

    fn unpivot_header(&self, node: &ParseNode) -> Result<Vec<String>> {
        let header = self.alt(node)?;
        Ok(self.texts(header, "ColIdOrString"))
    }

    /// `UnpivotValueList <- UnpivotHeader 'IN' UnpivotTargetList`.
    fn unpivot_value_list(&mut self, node: &ParseNode) -> Result<PivotColumn> {
        let names = self.unpivot_header(self.req(node, "UnpivotHeader")?)?;
        if names.len() != 1 {
            return Err(parser_error(
                "UNPIVOT requires a single column name for the PIVOT IN clause",
            ));
        }
        let list = self.req(self.req(node, "UnpivotTargetList")?, "TargetList")?;
        let entries = self
            .target_list(list)?
            .into_iter()
            .map(|e| PivotColumnEntry {
                values: Vec::new(),
                alias: e.alias.clone(),
                expr: Some(e),
            })
            .collect();
        Ok(PivotColumn {
            unpivot_names: names,
            entries,
            ..PivotColumn::default()
        })
    }

    fn name_list(&self, node: &ParseNode) -> Result<Vec<String>> {
        Ok(self.texts(self.alt(node)?, "ColIdOrString"))
    }

    /// `TablePivotClause` / `TableUnpivotClause`, applied to `source`.
    pub(in crate::transformer) fn table_pivot(&mut self, node: &ParseNode, source: TableRef) -> Result<TableRef> {
        let mut pivot = PivotRef {
            source: Box::new(source),
            aggregates: Vec::new(),
            unpivot_names: Vec::new(),
            pivots: Vec::new(),
            groups: Vec::new(),
            include_nulls: false,
        };
        if self.name(node) == "TablePivotClause" {
            let body = self.req(node, "TablePivotClauseBody")?;
            pivot.aggregates = self.target_list(self.req(body, "TargetList")?)?;
            for list in self.subs(body, "PivotValueList") {
                pivot.pivots.push(self.pivot_value_list(list)?);
            }
            if let Some(group) = self.sub(body, "PivotGroupByList") {
                pivot.groups = self.name_list(self.req(group, "OptionalParensNameList")?)?;
            }
        } else {
            let body = self.req(node, "TableUnpivotClauseBody")?;
            pivot.unpivot_names = self.unpivot_header(self.req(body, "UnpivotHeader")?)?;
            for list in self.subs(body, "UnpivotValueList") {
                pivot.pivots.push(self.unpivot_value_list(list)?);
            }
            if pivot.pivots.len() > 1 {
                return Err(parser_error("UNPIVOT requires a single pivot element"));
            }
            if let Some(nulls) = self.sub(node, "IncludeOrExcludeNulls") {
                pivot.include_nulls = self.name(self.alt(nulls)?) == "IncludeNulls";
            }
        }
        let mut table = TableRef::from(TableRefKind::Pivot(pivot));
        if let Some((alias, columns)) = self.table_alias(node)? {
            table.alias = alias;
            table.column_name_alias = columns;
        }
        Ok(table)
    }

    /// Columns whose values come from the data get an ENUM type that is created first.
    fn add_pivot_entries(
        &mut self,
        columns: &mut [PivotColumn],
        source: &TableRef,
        has_parameters: bool,
    ) -> Result<()> {
        for column in columns.iter_mut() {
            if !column.pivot_enum.is_empty() || !column.entries.is_empty() {
                continue;
            }
            if column.pivot_expressions.len() != 1 {
                return Err(Error::new(
                    ErrorKind::Internal,
                    "PIVOT statement with multiple names in pivot entry!?",
                ));
            }
            let enum_name = pivot_enum_name();
            let base = SelectNode {
                from_table: source.clone(),
                ..SelectNode::default()
            };
            let base_cte_map = self.ctes_in_scope();
            self.pivot_entries.push(PivotEntry {
                enum_name: enum_name.clone(),
                base,
                base_cte_map,
                column: column.pivot_expressions[0].clone(),
                subquery: column.subquery.take(),
                has_parameters,
            });
            column.pivot_enum = enum_name;
        }
        Ok(())
    }

    /// `PivotStatement <- PivotKeyword TableRef PivotOn? PivotUsing? PivotGroupByList?`.
    pub(in crate::transformer) fn pivot_statement(&mut self, node: &ParseNode) -> Result<SelectStatement> {
        let params_before = self.param_count;
        let source = self.table_ref(self.req(node, "TableRef")?)?;
        let has_parameters = self.param_count > params_before;
        let on = self.sub(node, "PivotOn");
        let using = self.sub(node, "PivotUsing");
        let group = self.sub(node, "PivotGroupByList");
        if on.is_none() && using.is_none() && group.is_none() {
            return Err(parser_error("PIVOT requires an ON, USING or GROUP BY clause"));
        }
        let groups = match group {
            Some(g) => self.name_list(self.req(g, "OptionalParensNameList")?)?,
            None => Vec::new(),
        };
        let aggregates = match using {
            Some(u) => Some(self.target_list(self.req(u, "TargetList")?)?),
            None => None,
        };
        let Some(on) = on else {
            let mut select = SelectNode {
                from_table: source,
                ..SelectNode::default()
            };
            for group in &groups {
                let column = Expr::column(vec![group.clone()]);
                select.select_list.push(column.clone());
                select.groups.group_expressions.push(column);
            }
            if !groups.is_empty() {
                select.groups.grouping_sets.push((0..groups.len()).collect());
            }
            select.select_list.extend(aggregates.unwrap_or_default());
            return Ok(SelectStatement { node: select.into() });
        };
        let mut columns = Vec::new();
        for entry in self.subs(self.req(on, "PivotColumnList")?, "PivotColumnEntry") {
            let entry = self.alt(entry)?;
            columns.push(match self.name(entry) {
                "PivotColumnSubquery" => {
                    let expr = self.expr(self.req(entry, "BaseExpression")?)?;
                    let query = self.select_internal(self.req(entry, "SelectStatementInternal")?)?;
                    PivotColumn {
                        pivot_expressions: vec![expr],
                        subquery: Some(Box::new(query.node)),
                        ..PivotColumn::default()
                    }
                }
                "PivotValueList" => self.pivot_value_list(entry)?,
                _ => PivotColumn {
                    pivot_expressions: vec![self.expr(self.req(entry, "Expression")?)?],
                    ..PivotColumn::default()
                },
            });
        }
        for column in columns.iter_mut() {
            if column.pivot_expressions.len() > 1 && !column.entries.iter().any(|e| e.values.len() > 1) {
                let row = FunctionExpr::new("row", std::mem::take(&mut column.pivot_expressions));
                column.pivot_expressions.push(row.into());
            }
            for expr in &column.pivot_expressions {
                if is_scalar(expr) {
                    return Err(parser_error(format!("Cannot pivot on constant value \"{expr}\"")));
                }
                if has_subquery(expr) {
                    return Err(parser_error(format!("Cannot pivot on subquery \"{expr}\"")));
                }
            }
        }
        self.add_pivot_entries(&mut columns, &source, has_parameters)?;
        let pivot = PivotRef {
            source: Box::new(source),
            aggregates: aggregates.unwrap_or_else(|| vec![FunctionExpr::new("count_star", Vec::new()).into()]),
            unpivot_names: Vec::new(),
            pivots: columns,
            groups,
            include_nulls: false,
        };
        let select = SelectNode {
            select_list: vec![Expr::star()],
            from_table: TableRefKind::Pivot(pivot).into(),
            ..SelectNode::default()
        };
        Ok(SelectStatement { node: select.into() })
    }

    /// `UnpivotStatement <- UnpivotKeyword TableRef 'ON' TargetList IntoNameValues?`.
    pub(in crate::transformer) fn unpivot_statement(&mut self, node: &ParseNode) -> Result<SelectStatement> {
        let source = self.table_ref(self.req(node, "TableRef")?)?;
        let targets = self.target_list(self.req(node, "TargetList")?)?;
        let (name, value_names) = match self.sub(node, "IntoNameValues") {
            Some(into) => (
                Self::text(self.req(into, "ColIdOrString")?),
                self.name_list(self.req(into, "OptionalParensNameList")?)?,
            ),
            None => ("name".to_string(), vec!["value".to_string()]),
        };
        let entries = targets
            .into_iter()
            .map(|e| PivotColumnEntry {
                values: Vec::new(),
                alias: e.alias.clone(),
                expr: Some(e),
            })
            .collect();
        let pivot = PivotRef {
            source: Box::new(source),
            aggregates: Vec::new(),
            unpivot_names: value_names,
            pivots: vec![PivotColumn {
                unpivot_names: vec![name],
                entries,
                ..PivotColumn::default()
            }],
            groups: Vec::new(),
            include_nulls: false,
        };
        let select = SelectNode {
            select_list: vec![Expr::star()],
            from_table: TableRefKind::Pivot(pivot).into(),
            ..SelectNode::default()
        };
        Ok(SelectStatement { node: select.into() })
    }

    /// `MergeIntoStatement`.
    pub(in crate::transformer) fn merge(&mut self, node: &ParseNode) -> Result<MergeStatement> {
        let cte_map = self.cte_map(node)?;
        let target_node = self.req(node, "TargetOptAlias")?;
        let target = self.aliased_table(
            self.req(target_node, "BaseTableName")?,
            self.sub(target_node, "TargetAlias"),
        )?;
        let source = self.table_ref(self.req(self.req(node, "MergeIntoUsingClause")?, "TableRef")?)?;
        let qualifier = self.alt(self.req(node, "JoinQualifier")?)?;
        let (join_condition, using_columns) = if self.name(qualifier) == "OnClause" {
            (
                Some(Box::new(self.expr(self.req(qualifier, "Expression")?)?)),
                Vec::new(),
            )
        } else {
            (None, self.texts(qualifier, "UsingColumnName"))
        };
        let mut actions: Vec<(MergeCondition, MergeAction)> = Vec::new();
        let mut unconditional: Vec<MergeCondition> = Vec::new();
        for clause in self.subs(node, "MergeMatch") {
            let clause = self.alt(clause)?;
            let condition = if self.name(clause) == "MatchedClause" {
                MergeCondition::WhenMatched
            } else {
                match self.sub(clause, "BySourceOrTarget") {
                    Some(b) if self.name(self.alt(b)?) == "BySource" => MergeCondition::WhenNotMatchedBySource,
                    _ => MergeCondition::WhenNotMatchedByTarget,
                }
            };
            let mut action = self.merge_action(self.alt(self.req(clause, "MatchedClauseAction")?)?)?;
            if let Some(and) = self.sub(clause, "AndExpression") {
                action.condition = Some(Box::new(self.expr(self.req(and, "Expression")?)?));
            }
            if unconditional.contains(&condition) {
                let label = match condition {
                    MergeCondition::WhenMatched => "WHEN MATCHED",
                    MergeCondition::WhenNotMatchedByTarget => "WHEN NOT MATCHED",
                    MergeCondition::WhenNotMatchedBySource => "WHEN NOT MATCHED BY SOURCE",
                };
                return Err(parser_error(format!(
                    "Unconditional {label} clause was already defined - any following {label} clause would be \
                     unreachable"
                )));
            }
            if action.condition.is_none() {
                unconditional.push(condition);
            }
            actions.push((condition, action));
        }
        Ok(MergeStatement {
            cte_map,
            target: Box::new(target),
            source: Box::new(source),
            join_condition,
            using_columns,
            actions,
            returning: self.returning(node)?,
        })
    }

    fn merge_action(&mut self, node: &ParseNode) -> Result<MergeAction> {
        let mut action = MergeAction {
            action_type: MergeActionType::DoNothing,
            condition: None,
            update_info: None,
            by_name: false,
            insert_columns: Vec::new(),
            expressions: Vec::new(),
            default_values: false,
        };
        let by_name = |s: &Self, n: Option<&ParseNode>| -> Result<bool> {
            Ok(match n {
                Some(b) => s.name(s.alt(b)?) == "InsertByNameOrder",
                None => false,
            })
        };
        match self.name(node) {
            "UpdateMatchClause" => {
                action.action_type = MergeActionType::Update;
                if let Some(info) = self.sub(node, "UpdateMatchInfo") {
                    let info = self.alt(info)?;
                    if self.name(info) == "UpdateMatchSetAction" {
                        let set = self.req(self.req(info, "UpdateMatchSetClause")?, "UpdateMatchSetInfo")?;
                        if let Some(list) = self.sub(set, "UpdateSetClauseList") {
                            action.update_info = Some(self.update_set_list(list)?);
                        }
                    } else {
                        action.by_name = by_name(self, self.sub(info, "ByNameOrPosition"))?;
                    }
                }
            }
            "DeleteMatchClause" => action.action_type = MergeActionType::Delete,
            "InsertMatchClause" => {
                action.action_type = MergeActionType::Insert;
                if let Some(info) = self.sub(node, "InsertMatchInfo") {
                    let info = self.alt(info)?;
                    match self.name(info) {
                        "InsertValuesList" => {
                            if let Some(list) = self.sub(info, "InsertColumnList") {
                                action.insert_columns = self.texts(self.req(list, "ColumnList")?, "ColId");
                            }
                            action.expressions = self.exprs(info, "Expression")?;
                        }
                        "InsertDefaultValues" => action.default_values = true,
                        _ => action.by_name = by_name(self, self.sub(info, "ByNameOrPosition"))?,
                    }
                }
            }
            "DoNothingMatchClause" => {}
            _ => {
                action.action_type = MergeActionType::Error;
                if let Some(e) = self.opt_expr(node, "Expression")? {
                    action.expressions.push(e);
                }
            }
        }
        Ok(action)
    }

    /// `NearestJoinClause`: a join on the k nearest rows by a distance or similarity.
    pub(in crate::transformer) fn nearest_join(&mut self, node: &ParseNode) -> Result<JoinRef> {
        let form = self.alt(node)?;
        let join_type = self.join_type(self.sub(form, "JoinType"))?;
        if !matches!(join_type, JoinType::Inner | JoinType::Left) {
            return Err(parser_error(format!(
                "NEAREST BY only supports INNER and LEFT OUTER joins, not {}",
                join_type.name()
            )));
        }
        let right = if self.name(form) == "NearestJoinAliased" {
            self.table_ref(self.req(form, "TableRef")?)?
        } else {
            self.nearest_bare_table_ref(self.alt(self.req(form, "NearestBareTableRef")?)?)?
        };
        let mut count = 1;
        if let Some(n) = self.sub(form, "NumberLiteral") {
            let literal = Literal::number(Self::text(n));
            match literal.try_get_i64() {
                Some(c) if c >= 1 => count = c as u64,
                _ => {
                    return Err(parser_error(format!(
                        "NEAREST expects a positive integer literal, got \"{}\"",
                        literal_to_sql(&literal)
                    )));
                }
            }
        }
        let approx = match self.sub(form, "ApproxOrExact") {
            Some(a) => self.name(self.alt(a)?) == "NearestApprox",
            None => false,
        };
        let descending = self.name(self.alt(self.req(form, "DistanceOrSimilarity")?)?) == "NearestSimilarity";
        Ok(JoinRef {
            left: Box::new(TableRef::empty()),
            right: Box::new(right),
            condition: None,
            using_columns: Vec::new(),
            join_type,
            ref_type: JoinRefType::Nearest,
            is_implicit: false,
            nearest_count: count,
            nearest_descending: descending,
            nearest_approx: approx,
            ranking_expression: Some(Box::new(self.expr(self.req(form, "Expression")?)?)),
        })
    }

    fn nearest_bare_table_ref(&mut self, node: &ParseNode) -> Result<TableRef> {
        Ok(match self.name(node) {
            "NearestValuesRef" => TableRef::subquery(self.values_clause(self.req(node, "ValuesClause")?)?),
            "NearestTableSubquery" => {
                let s = self.subquery_reference(self.req(node, "SubqueryReference")?)?;
                TableRef::subquery(s)
            }
            "NearestBaseTableRef" => {
                let mut table = self.base_table_name(self.req(node, "BaseTableName")?)?;
                if self.sub(node, "AtClause").is_some() {
                    return Err(not_implemented(
                        "AT clauses on NEAREST BY join targets are not yet supported",
                    ));
                }
                if let Some(sample) = self.sub(node, "SampleClause") {
                    table.sample = Some(self.sample_entry(self.req(sample, "SampleEntry")?)?);
                }
                table
            }
            "NearestParensTableRef" => {
                let inner = self.table_ref(self.req(node, "TableRef")?)?;
                let Some(sample) = self.sub(node, "SampleClause") else {
                    return Ok(inner);
                };
                let select = SelectNode {
                    select_list: vec![Expr::star()],
                    from_table: inner,
                    ..SelectNode::default()
                };
                let mut table = TableRef::subquery(SelectStatement { node: select.into() });
                table.sample = Some(self.sample_entry(self.req(sample, "SampleEntry")?)?);
                table
            }
            _ => {
                let name = QualifiedName::from_parts(self.rule_texts(self.req(node, "QualifiedTableFunction")?));
                let mut function = FunctionExpr::new("", Vec::new());
                function.name = name;
                for arg in self.subs(self.req(node, "TableFunctionArguments")?, "FunctionArgument") {
                    function.args.push(self.function_argument(arg)?);
                }
                TableRefKind::TableFunction {
                    function: Box::new(function.into()),
                    with_ordinality: self.sub(node, "WithOrdinality").is_some(),
                }
                .into()
            }
        })
    }
}

/// `GenerateCreateEnumStmt`: `CREATE OR REPLACE TEMP TYPE <enum> AS ENUM (SELECT DISTINCT ...)`.
fn create_enum_statement(entry: PivotEntry) -> Statement {
    let query = match entry.subquery {
        Some(q) => *q,
        None => {
            let mut select = entry.base;
            select
                .select_list
                .push(Expr::cast(entry.column.clone(), CastType::Builtin("VARCHAR")));
            select.where_clause = Some(Box::new(Expr::operator(OperatorType::IsNotNull, vec![entry.column])));
            let mut node: QueryNode = select.into();
            node.cte_map = entry.base_cte_map;
            node.modifiers.push(ResultModifier::Distinct(Vec::new()));
            node.modifiers.push(ResultModifier::Order(vec![OrderByNode {
                order: OrderType::Ascending,
                nulls: NullOrder::Default,
                expr: Expr::constant(Literal::integer(1)),
            }]));
            node
        }
    };
    Statement::Create(Box::new(CreateInfo {
        name: QualifiedName::new(entry.enum_name),
        on_conflict: OnCreateConflict::Replace,
        temporary: true,
        kind: CreateKind::Type(CreateTypeKind::EnumQuery(Box::new(SelectStatement { node: query }))),
    }))
}
