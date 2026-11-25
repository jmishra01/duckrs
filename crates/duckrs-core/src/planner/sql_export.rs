// Ported from duckdb/src/planner/sql_export/*.cpp, duckdb/src/parser/expression/value_expression.cpp
// (`ConstantExpression::FromValue`) and duckdb/src/parser/peg/sql_formatter.cpp @ rust-port-baseline
//
//! `EXPLAIN (SQL)`: SQL that computes what an optimized logical plan computes
//! (`LogicalPlanSQLExporter`).
//!
//! Each operator becomes a relation: a SELECT whose columns are the operator's column bindings, in
//! order. A parent reads a relation through its columns. Where the parent's clause can go into the
//! relation's SELECT without changing what it means (a filter into WHERE, a projection into the
//! SELECT list, an aggregate over a plain scan), it does. Otherwise the relation becomes a subquery
//! `(...) AS rN(c0, c1, ...)`, as `CreateSubquery` makes it in C++.
//!
//! The plans differ from those of C++ in places (subqueries can remain plans of their own, there
//! are lateral joins), so this follows the design of the C++ exporter rather than its code.

use super::expression::{
    BoundAggregate, BoundExpr, BoundExprKind, BoundOperator, BoundWindow, ColumnBinding, LambdaKind, SequenceOp,
    SubqueryKind, WindowFunction,
};
use super::logical::{
    BoundOrderByNode, COLUMN_IDENTIFIER_ROW_ID, JoinCondition, LogicalOperator, LogicalOperatorKind as Op,
};
use crate::catalog::TableCatalogEntry;
use duckrs_common::sort_key::{NullOrder as SortNulls, OrderModifiers, OrderType as SortOrder};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_parser::ast::{
    self, CastType, ComparisonOp, CteInfo, CteMaterialize, Expr, ExprKind, FunctionArgument, FunctionExpr, GroupByNode,
    JoinRef, JoinRefType, JoinType, Literal, NullOrder, OperatorType, OrderByNode, OrderType, QualifiedName, QueryNode,
    QueryNodeKind, RecursiveCteNode, ResultModifier, SelectNode, SelectStatement, SetOpType, SetOperationNode,
    SubqueryType, TableRef, TableRefKind, WindowExpr,
};
use std::collections::{BTreeSet, HashMap};

/// Why a plan has no SQL (an `UNSUPPORTED_*` issue of C++).
pub type Unsupported = String;
type Res<T> = std::result::Result<T, Unsupported>;

/// The name of the database a table is in, to qualify it with.
pub type DatabaseOf<'a> = &'a dyn Fn(&TableCatalogEntry) -> Option<String>;

/// `Expression::CanThrow`, which the optimizer knows: passed in, as the planner sits below it.
pub type CanThrow<'a> = &'a dyn Fn(&BoundExpr) -> bool;

/// `LogicalPlanSQLExporter::Export`: the SQL of `plan`, its result columns named `output_names`
/// when given (they are not for a statement that reports a count).
pub fn export_plan(
    plan: &LogicalOperator,
    output_names: Option<&[String]>,
    database_of: DatabaseOf,
    can_throw: CanThrow,
) -> Res<String> {
    let mut exporter = Exporter {
        next_alias: 0,
        ctes: Vec::new(),
        working: Vec::new(),
        outer: Vec::new(),
        lambdas: Vec::new(),
        delims: Vec::new(),
        database_of,
        can_throw,
    };
    if let Some(statement) = exporter.statement(plan, output_names)? {
        return finish_sql(&statement);
    }
    let relation = exporter.export(plan)?;
    let mut node = match output_names {
        Some(names) => exporter.named_query(relation, names)?,
        None => exporter.query(relation),
    };
    if !exporter.ctes.is_empty() {
        let mut ctes = std::mem::take(&mut exporter.ctes);
        ctes.append(&mut node.cte_map.0);
        node.cte_map.0 = ctes;
    }
    finish_sql(&node.to_string())
}

/// The query of a plan, its columns named `names`, as a COPY or CREATE TABLE AS takes it.
pub fn export_query(
    plan: &LogicalOperator,
    names: &[String],
    database_of: DatabaseOf,
    can_throw: CanThrow,
) -> Res<QueryNode> {
    let mut exporter = Exporter {
        next_alias: 0,
        ctes: Vec::new(),
        working: Vec::new(),
        outer: Vec::new(),
        lambdas: Vec::new(),
        delims: Vec::new(),
        database_of,
        can_throw,
    };
    let relation = exporter.export(plan)?;
    let mut node = exporter.named_query(relation, names)?;
    if !exporter.ctes.is_empty() {
        let mut ctes = std::mem::take(&mut exporter.ctes);
        ctes.append(&mut node.cte_map.0);
        node.cte_map.0 = ctes;
    }
    Ok(node)
}

/// The SQL of a statement, checked and formatted.
pub fn finish_sql(sql: &str) -> Res<String> {
    if sql.len() > MAX_SQL_LENGTH {
        return Err("The SQL of the plan is too long".into());
    }
    Ok(duckrs_parser::sql_formatter::format_sql(
        sql,
        &duckrs_parser::sql_formatter::FormatterConfig::default(),
    ))
}

// ---------------------------------------------------------------------------------------------------------------
// Relations
// ---------------------------------------------------------------------------------------------------------------

/// What a column of a SELECT is, as far as moving it into another clause goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// A column of the FROM clause or a constant: it can go anywhere.
    Column,
    /// Computed from the row.
    Computed,
    /// An aggregate or a group of a grouped SELECT.
    Aggregate,
    /// The `__mark_join_marker` of a `JOIN BY (TYPE MARK)` in the FROM clause, which is there by
    /// that name only.
    Mark,
    Window,
    Unnest,
}

#[derive(Debug, Clone)]
struct Column {
    expr: Expr,
    kind: Kind,
    volatile: bool,
    /// computed by something that can fail (`CanThrow`): it stays where it was evaluated
    throws: bool,
}

impl Column {
    fn plain(expr: Expr) -> Self {
        Self {
            expr,
            kind: Kind::Column,
            volatile: false,
            throws: false,
        }
    }
}

/// A SELECT being built, the columns over `from`.
#[derive(Debug, Clone)]
struct Select {
    from: TableRef,
    filters: Vec<Expr>,
    /// whether a filter has effects (`HasSafePredicates`): more filters next to it could change
    /// how often it runs
    unsafe_filters: bool,
    /// with a GROUP BY: the group expressions, and the grouping sets over them
    groups: Option<GroupByNode>,
    having: Vec<Expr>,
    columns: Vec<Column>,
    distinct: bool,
    /// DISTINCT ON these, with `distinct`
    distinct_on: Vec<Expr>,
    /// USING SAMPLE, which comes after FROM and before WHERE
    sample: Option<ast::SampleOptions>,
    order: Vec<OrderByNode>,
    /// LIMIT, OFFSET, and whether the limit is a percentage
    limit: Option<(Option<Expr>, Option<Expr>, bool)>,
}

impl Select {
    fn new(from: TableRef, columns: Vec<Column>) -> Self {
        Self {
            from,
            filters: Vec::new(),
            unsafe_filters: false,
            groups: None,
            having: Vec::new(),
            columns,
            distinct: false,
            distinct_on: Vec::new(),
            sample: None,
            order: Vec::new(),
            limit: None,
        }
    }

    /// Without clauses past WHERE: what another clause can be added to.
    fn is_simple(&self) -> bool {
        self.groups.is_none() && !self.distinct && self.order.is_empty() && self.limit.is_none()
    }

    /// `PlainScope`: columns of the FROM clause, filtered.
    fn is_plain(&self) -> bool {
        // (a filter that can fail must not see the rows another one removes)
        self.is_simple()
            && self.sample.is_none()
            && !self.unsafe_filters
            && self.columns.iter().all(|c| c.kind == Kind::Column)
    }
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
enum Body {
    Select(Select),
    /// A set operation.
    Node(QueryNode),
}

#[derive(Debug, Clone)]
struct Field {
    binding: ColumnBinding,
    ty: LogicalType,
}

/// `LogicalPlanSQLExportRelation`: the SQL of an operator, a column per binding of it.
#[derive(Debug, Clone)]
struct Relation {
    body: Body,
    fields: Vec<Field>,
}

/// What expressions of an operator read: the SQL of the columns of its children.
#[derive(Default)]
struct Scope {
    columns: HashMap<ColumnBinding, (Column, LogicalType)>,
}

impl Scope {
    fn of(select: &Select, fields: &[Field]) -> Self {
        let mut scope = Self::default();
        scope.add(select, fields);
        scope
    }

    fn add(&mut self, select: &Select, fields: &[Field]) {
        for (column, field) in select.columns.iter().zip(fields) {
            self.columns.insert(field.binding, (column.clone(), field.ty.clone()));
        }
    }
}

fn field_name(i: usize) -> String {
    format!("c{i}")
}

fn conjoin(mut predicates: Vec<Expr>) -> Option<Box<Expr>> {
    match predicates.len() {
        0 => None,
        1 => predicates.pop().map(Box::new),
        _ => Some(Box::new(
            ExprKind::Conjunction {
                and: true,
                children: predicates,
            }
            .into(),
        )),
    }
}

fn system_function(name: &str, args: Vec<Expr>) -> Expr {
    let mut function = FunctionExpr::new(name, args);
    function.name.qualification = vec!["system".into(), "main".into()];
    function.into()
}

fn order_node(modifiers: OrderModifiers, expr: Expr) -> OrderByNode {
    OrderByNode {
        order: match modifiers.order {
            SortOrder::Ascending => OrderType::Ascending,
            SortOrder::Descending => OrderType::Descending,
        },
        nulls: match modifiers.nulls {
            SortNulls::NullsFirst => NullOrder::NullsFirst,
            SortNulls::NullsLast => NullOrder::NullsLast,
        },
        expr,
    }
}

fn cast(ty: &LogicalType, child: Expr, try_cast: bool) -> Expr {
    ExprKind::Cast {
        child: Box::new(child),
        target: CastType::Resolved(sql_cast_type(ty)),
        try_cast,
    }
    .into()
}

/// `SQLCastType`: a collation goes on with COLLATE, outside of the cast.
fn sql_cast_type(ty: &LogicalType) -> LogicalType {
    if ty.id() == LogicalTypeId::Varchar && ty.alias().is_none() {
        LogicalType::VARCHAR
    } else {
        ty.clone()
    }
}

fn collate(ty: &LogicalType, expr: Expr) -> Expr {
    match ty
        .collation()
        .filter(|c| !c.is_empty() && ty.id() == LogicalTypeId::Varchar)
    {
        Some(collation) => ExprKind::Collate {
            child: Box::new(expr),
            collation: collation.to_string(),
        }
        .into(),
        None => expr,
    }
}

/// Whether a VARCHAR below the top of `ty` has a collation (`HasNestedCollation`), which SQL can't
/// spell for a value.
fn has_nested_collation(ty: &LogicalType) -> bool {
    fn any_collation(ty: &LogicalType) -> bool {
        match ty.id() {
            LogicalTypeId::Varchar => ty.collation().is_some_and(|c| !c.is_empty()),
            LogicalTypeId::Struct => ty.struct_children().iter().any(|(_, t)| any_collation(t)),
            _ => ty.child_type().is_some_and(any_collation),
        }
    }
    ty.id() != LogicalTypeId::Varchar && any_collation(ty)
}

/// Whether evaluating `e` more or less often could be seen: it is volatile or raises errors.
fn has_effects(e: &BoundExpr) -> bool {
    fn raises(e: &BoundExpr) -> bool {
        matches!(&e.kind, BoundExprKind::Function { function, .. } if function.name == "error")
            || e.children().into_iter().any(raises)
    }
    e.is_volatile() || raises(e)
}

/// `HasCompositionBarrier`: an expression that decides whether, or for which rows, its input runs.
fn has_composition_barrier(e: &BoundExpr) -> bool {
    matches!(
        &e.kind,
        BoundExprKind::Case { .. }
            | BoundExprKind::ListLambda { .. }
            | BoundExprKind::Operator {
                op: BoundOperator::Try | BoundOperator::HasError | BoundOperator::Coalesce,
                ..
            }
    ) || e.children().into_iter().any(has_composition_barrier)
}

/// The columns of the FROM clause an expression reads.
fn read_bindings(expr: &BoundExpr, out: &mut Vec<ColumnBinding>) {
    if let BoundExprKind::ColumnRef { binding, depth: 0 } = &expr.kind {
        out.push(*binding);
    }
    for child in expr.children() {
        read_bindings(child, out);
    }
    if let BoundExprKind::Subquery { correlated, .. } = &expr.kind {
        for c in correlated {
            read_bindings(c, out);
        }
    }
}

struct Exporter<'a> {
    next_alias: usize,
    /// WITH entries, which go on the outermost query
    ctes: Vec<(String, CteInfo)>,
    /// the recursive CTEs being exported, by their working table, and how often it was read
    working: Vec<(usize, String, usize)>,
    /// the columns of enclosing queries, for correlated references
    outer: Vec<Scope>,
    /// what `Reference(i)` is in the body of a lambda
    lambdas: Vec<Vec<Expr>>,
    /// the duplicate-eliminated joins being exported, by their `delim`: the key values of a left row
    delims: Vec<(usize, Vec<Column>)>,
    database_of: DatabaseOf<'a>,
    can_throw: CanThrow<'a>,
}

impl Exporter<'_> {
    fn table_name(&self, table: &TableCatalogEntry) -> QualifiedName {
        let mut qualification = Vec::new();
        if let Some(database) = (self.database_of)(table) {
            qualification.push(database);
        }
        qualification.push(table.schema.clone());
        QualifiedName {
            qualification,
            name: table.name.clone(),
        }
    }

    fn take_ctes(&mut self) -> ast::CteMap {
        ast::CteMap(std::mem::take(&mut self.ctes))
    }

    /// The SQL of a data-modifying plan (`sql_export_dml.cpp`): INSERT of a query into the physical
    /// columns, UPDATE, DELETE and MERGE INTO of the rows the query finds by their row id. A
    /// projection over one with RETURNING is its RETURNING list (`MatchReturningRoot`).
    fn statement(&mut self, plan: &LogicalOperator, names: Option<&[String]>) -> Res<Option<String>> {
        if let Op::Projection { expressions, .. } = &plan.kind
            && let Some(child) = plan.children.first()
            && matches!(
                child.kind,
                Op::Insert { returning: true, .. }
                    | Op::Delete { returning: true, .. }
                    | Op::Update { returning: true, .. }
                    | Op::MergeInto { returning: true, .. }
            )
        {
            return self.dml(child, Some((expressions, names)));
        }
        self.dml(plan, None)
    }

    /// The RETURNING list: the columns of the table by name, and `merge_action`.
    fn returning(
        &mut self,
        table: &TableCatalogEntry,
        table_index: usize,
        returning: Option<(&[BoundExpr], Option<&[String]>)>,
    ) -> Res<Vec<Expr>> {
        let Some((expressions, names)) = returning else {
            return Ok(Vec::new());
        };
        let mut scope = Scope::default();
        for (i, column) in table.columns.iter().enumerate() {
            scope.columns.insert(
                ColumnBinding::new(table_index, i),
                (
                    Column::plain(Expr::column(vec![column.name.clone()])),
                    column.ty.clone(),
                ),
            );
        }
        scope.columns.insert(
            ColumnBinding::new(table_index, table.columns.len()),
            (
                Column::plain(Expr::column(vec!["merge_action".into()])),
                LogicalType::VARCHAR,
            ),
        );
        let mut list = Vec::new();
        for (i, e) in expressions.iter().enumerate() {
            let mut expr = self.expr(&scope, e)?;
            expr.alias = names.and_then(|n| n.get(i).cloned()).unwrap_or_else(|| e.alias.clone());
            list.push(expr);
        }
        Ok(list)
    }

    fn target(&self, table: &TableCatalogEntry) -> TableRef {
        TableRefKind::BaseTable {
            name: self.table_name(table),
            at_clause: None,
        }
        .into()
    }

    fn dml(
        &mut self,
        plan: &LogicalOperator,
        returning: Option<(&[BoundExpr], Option<&[String]>)>,
    ) -> Res<Option<String>> {
        match &plan.kind {
            Op::Insert {
                table,
                column_index_map,
                on_conflict,
                table_index,
                ..
            } => {
                let child = self.export_child(plan, 0)?;
                let types = child.fields.iter().map(|f| f.ty.clone()).collect::<Vec<_>>();
                let source = self.wrap(child);
                let mut columns = Vec::new();
                for (i, slot) in column_index_map.iter().enumerate() {
                    let column = match slot {
                        Some(j) => {
                            let mut column = source.columns[*j].clone();
                            if types[*j] != table.columns[i].ty {
                                column.expr = cast(&table.columns[i].ty, column.expr, false);
                            }
                            column
                        }
                        // what the INSERT gives a column it leaves out: its default
                        None => match &table.columns[i].default_value {
                            Some(default) => {
                                let expr = self.expr(&Scope::default(), default)?;
                                let expr = if default.return_type != table.columns[i].ty {
                                    cast(&table.columns[i].ty, expr, false)
                                } else {
                                    expr
                                };
                                Column::plain(expr)
                            }
                            None => Column::plain(self.constant(&Value::null(table.columns[i].ty.clone()))?),
                        },
                    };
                    columns.push(column);
                }
                let mut select = source;
                select.columns = columns;
                let names: Vec<String> = (0..select.columns.len()).map(field_name).collect();
                let node = finish(select, &names);
                let on_conflict = match on_conflict {
                    Some(conflict) => Some(self.on_conflict(table, conflict)?),
                    None => None,
                };
                let returning = self.returning(table, *table_index, returning)?;
                let statement = ast::InsertStatement {
                    cte_map: self.take_ctes(),
                    table: self.table_name(table),
                    table_alias: None,
                    by_name: false,
                    columns: table.columns.iter().map(|c| c.name.clone()).collect(),
                    values: ast::InsertValues::Select(Box::new(SelectStatement { node })),
                    on_conflict,
                    returning,
                };
                Ok(Some(statement.to_string()))
            }
            Op::Delete { table, table_index, .. } => {
                let child = self.export_child(plan, 0)?;
                let node = self.query(child);
                let condition = ExprKind::Subquery {
                    subquery: Box::new(SelectStatement { node }),
                    subquery_type: SubqueryType::Any,
                    child: Some(Box::new(Expr::column(vec!["rowid".into()]))),
                    comparison: ComparisonOp::Equal,
                };
                let returning = self.returning(table, *table_index, returning)?;
                let statement = ast::DeleteStatement {
                    cte_map: self.take_ctes(),
                    table: self.target(table),
                    using: Vec::new(),
                    condition: Some(Box::new(condition.into())),
                    returning,
                };
                Ok(Some(statement.to_string()))
            }
            Op::Update {
                table,
                columns,
                table_index,
                ..
            } => {
                let child = self.export_child(plan, 0)?;
                let count = child.fields.len();
                let alias = self.alias();
                let node = self.query(child);
                let mut from = TableRef::subquery(SelectStatement { node });
                from.alias = alias.clone();
                from.column_name_alias = (0..count).map(field_name).collect();
                let set_info = ast::UpdateSetInfo {
                    columns: columns.iter().map(|&c| table.columns[c].name.clone()).collect(),
                    expressions: (0..columns.len())
                        .map(|i| Expr::column(vec![alias.clone(), field_name(i)]))
                        .collect(),
                    condition: Some(Box::new(Expr::comparison(
                        ComparisonOp::Equal,
                        Expr::column(vec![table.name.clone(), "rowid".into()]),
                        Expr::column(vec![alias.clone(), field_name(count - 1)]),
                    ))),
                };
                let returning = self.returning(table, *table_index, returning)?;
                let statement = ast::UpdateStatement {
                    cte_map: self.take_ctes(),
                    table: self.target(table),
                    set_info,
                    from_table: Some(from),
                    returning,
                };
                Ok(Some(statement.to_string()))
            }
            Op::MergeInto {
                table,
                table_index,
                actions,
                row_id,
                marker,
                ..
            } => {
                if marker.is_some() {
                    return Err("WHEN NOT MATCHED BY SOURCE actions cannot be keyed by the exported row id".into());
                }
                let child = self.export_child(plan, 0)?;
                let fields = child.fields.clone();
                let count = fields.len();
                let alias = self.alias();
                let node = self.query(child);
                let mut from = TableRef::subquery(SelectStatement { node });
                from.alias = alias.clone();
                from.column_name_alias = (0..count).map(field_name).collect();
                let source_select = Select::new(
                    TableRef::empty(),
                    (0..count)
                        .map(|i| Column::plain(Expr::column(vec![alias.clone(), field_name(i)])))
                        .collect(),
                );
                let scope = Scope::of(&source_select, &fields);
                let on = Expr::comparison(
                    ComparisonOp::Equal,
                    Expr::column(vec![table.name.clone(), "rowid".into()]),
                    self.expr(&scope, row_id)?,
                );
                let mut sql_actions = Vec::new();
                for (condition, list) in actions {
                    for action in list {
                        let when = action.condition.as_ref().map(|c| self.expr(&scope, c)).transpose()?;
                        let expressions = action
                            .expressions
                            .iter()
                            .map(|e| self.expr(&scope, e))
                            .collect::<Res<Vec<_>>>()?;
                        let mut sql = ast::MergeAction {
                            action_type: action.action_type,
                            condition: when.map(Box::new),
                            update_info: None,
                            by_name: false,
                            insert_columns: Vec::new(),
                            expressions: Vec::new(),
                            default_values: false,
                        };
                        match action.action_type {
                            ast::MergeActionType::Update => {
                                sql.update_info = Some(ast::UpdateSetInfo {
                                    columns: action.columns.iter().map(|&c| table.columns[c].name.clone()).collect(),
                                    expressions,
                                    condition: None,
                                });
                            }
                            ast::MergeActionType::Insert => {
                                sql.insert_columns = table.columns.iter().map(|c| c.name.clone()).collect();
                                sql.expressions = expressions;
                            }
                            ast::MergeActionType::Error => sql.expressions = expressions,
                            ast::MergeActionType::Delete | ast::MergeActionType::DoNothing => {}
                        }
                        sql_actions.push((*condition, sql));
                    }
                }
                let returning = self.returning(table, *table_index, returning)?;
                let statement = ast::MergeStatement {
                    cte_map: self.take_ctes(),
                    target: Box::new(self.target(table)),
                    source: Box::new(from),
                    join_condition: Some(Box::new(on)),
                    using_columns: Vec::new(),
                    actions: sql_actions,
                    returning,
                };
                Ok(Some(statement.to_string()))
            }
            _ => Ok(None),
        }
    }

    /// ON CONFLICT: its SET expressions read the columns of the row, then those of `excluded`.
    fn on_conflict(
        &mut self,
        table: &TableCatalogEntry,
        conflict: &super::logical::OnConflict,
    ) -> Res<ast::OnConflictInfo> {
        let mut slots: Vec<Expr> = table
            .columns
            .iter()
            .map(|c| Expr::column(vec![table.name.clone(), c.name.clone()]))
            .collect();
        slots.extend(
            table
                .columns
                .iter()
                .map(|c| Expr::column(vec!["excluded".into(), c.name.clone()])),
        );
        self.lambdas.push(slots);
        let result = (|| -> Res<ast::OnConflictInfo> {
            let scope = Scope::default();
            let set_info = if conflict.do_update {
                let expressions = conflict
                    .set_expressions
                    .iter()
                    .map(|e| self.expr(&scope, e))
                    .collect::<Res<Vec<_>>>()?;
                let condition = conflict.condition.as_ref().map(|c| self.expr(&scope, c)).transpose()?;
                Some(ast::UpdateSetInfo {
                    columns: conflict
                        .set_columns
                        .iter()
                        .map(|&c| table.columns[c].name.clone())
                        .collect(),
                    expressions,
                    condition: condition.map(Box::new),
                })
            } else {
                None
            };
            let indexed_columns = match conflict.targets.as_slice() {
                [target] => target.iter().map(|&c| table.columns[c].name.clone()).collect(),
                _ => Vec::new(),
            };
            Ok(ast::OnConflictInfo {
                action: if conflict.do_update {
                    ast::OnConflictAction::Update
                } else {
                    ast::OnConflictAction::Nothing
                },
                indexed_columns,
                condition: None,
                set_info,
            })
        })();
        self.lambdas.pop();
        result
    }

    /// `NextRelationAlias`.
    fn alias(&mut self) -> String {
        let name = format!("r{}", self.next_alias);
        self.next_alias += 1;
        name
    }

    /// The SELECT of a relation, its columns `c0`, `c1`, ...
    fn query(&mut self, relation: Relation) -> QueryNode {
        match relation.body {
            Body::Node(node) => node,
            Body::Select(select) => {
                let names = (0..select.columns.len()).map(field_name).collect::<Vec<_>>();
                finish(select, &names)
            }
        }
    }

    /// `ApplyOutputNames`.
    fn named_query(&mut self, relation: Relation, names: &[String]) -> Res<QueryNode> {
        if names.len() != relation.fields.len() {
            return Err("Output name count does not match the exported plan".into());
        }
        let select = match relation.body {
            Body::Select(select) => select,
            body @ Body::Node(_) => self.wrap(Relation {
                body,
                fields: relation.fields,
            }),
        };
        Ok(finish(select, names))
    }

    /// `CreateSubquery`: the relation as a FROM clause, unless it is one already.
    fn wrap(&mut self, relation: Relation) -> Select {
        if let Body::Select(select) = &relation.body
            && select.is_plain()
            && !matches!(select.from.kind, TableRefKind::Empty)
        {
            let Body::Select(select) = relation.body else {
                unreachable!()
            };
            return select;
        }
        let count = relation.fields.len();
        let alias = self.alias();
        let node = self.query(relation);
        let mut from = TableRef::subquery(SelectStatement { node });
        from.alias = alias.clone();
        from.column_name_alias = (0..count).map(field_name).collect();
        let columns = (0..count)
            .map(|i| Column::plain(Expr::column(vec![alias.clone(), field_name(i)])))
            .collect();
        Select::new(from, columns)
    }

    /// The relation as a SELECT that clauses can be added to.
    fn simple(&mut self, relation: Relation) -> Select {
        match relation.body {
            Body::Select(select) if select.is_simple() => select,
            body => self.wrap(Relation {
                body,
                fields: relation.fields,
            }),
        }
    }

    fn export_child(&mut self, op: &LogicalOperator, i: usize) -> Res<Relation> {
        self.export(&op.children[i])
    }

    /// The fields of `op` (`CreateFields`).
    fn fields(op: &LogicalOperator) -> Vec<Field> {
        op.column_bindings()
            .into_iter()
            .zip(op.types())
            .map(|(binding, ty)| Field { binding, ty })
            .collect()
    }

    fn export(&mut self, op: &LogicalOperator) -> Res<Relation> {
        let fields = Self::fields(op);
        let body = match &op.kind {
            Op::Get {
                table,
                column_ids,
                filters,
                projection,
                table_index,
                extracts,
                ..
            } => self.get(
                table,
                *table_index,
                column_ids,
                filters,
                projection.as_deref(),
                extracts,
            )?,
            Op::Range {
                start,
                increment,
                count,
                ..
            } => {
                let end = start
                    .checked_add(increment.checked_mul(*count as i128).ok_or("range overflow")?)
                    .ok_or("range overflow")?;
                let fits = |v: i128| i64::try_from(v).is_ok();
                if !fits(*start) || !fits(end) || !fits(*increment) {
                    return Err("The range has no SQL spelling".into());
                }
                let mut bigint = |v: i128| self.constant(&Value::bigint(v as i64));
                let args = vec![bigint(*start)?, bigint(end)?, bigint(*increment)?];
                let alias = self.alias();
                let mut from: TableRef = TableRefKind::TableFunction {
                    function: Box::new(FunctionExpr::new("range", args).into()),
                    with_ordinality: false,
                }
                .into();
                from.alias = alias.clone();
                from.column_name_alias = vec![field_name(0)];
                Body::Select(Select::new(
                    from,
                    vec![Column::plain(Expr::column(vec![alias, field_name(0)]))],
                ))
            }
            Op::EmptyResult { types, .. } => {
                let columns = types
                    .iter()
                    .map(|ty| self.constant(&Value::null(ty.clone())).map(Column::plain))
                    .collect::<Res<Vec<_>>>()?;
                let mut select = Select::new(TableRef::empty(), columns);
                select.filters.push(Expr::constant(Literal::boolean(false)));
                Body::Select(select)
            }
            Op::DummyScan { .. } => Body::Select(Select::new(
                TableRef::empty(),
                vec![Column::plain(Expr::constant(Literal::null()))],
            )),
            Op::ExpressionGet { types, expressions, .. } => {
                if op
                    .children
                    .first()
                    .is_some_and(|c| !matches!(c.kind, Op::DummyScan { .. }))
                {
                    return Err("VALUES over a relation has no SQL export".into());
                }
                let scope = Scope::default();
                let mut rows = Vec::new();
                for row in expressions {
                    let mut values = Vec::new();
                    for (e, ty) in row.iter().zip(types) {
                        let mut value = self.expr(&scope, e)?;
                        if e.return_type != *ty {
                            value = cast(ty, value, false);
                        }
                        values.push(value);
                    }
                    rows.push(values);
                }
                self.values(rows, types.len())
            }
            Op::ChunkGet { types, chunks, .. } => {
                let total: usize = chunks.iter().map(|c| c.len()).sum();
                if total * types.len().max(1) > MAX_INLINE_VALUES {
                    return Err("The rows are too many to spell as VALUES".into());
                }
                let mut rows = Vec::new();
                for chunk in chunks.iter() {
                    for row in 0..chunk.len() {
                        let values = (0..types.len())
                            .map(|column| self.constant(&chunk.get_value(column, row)))
                            .collect::<Res<Vec<_>>>()?;
                        rows.push(values);
                    }
                }
                if rows.is_empty() {
                    let columns = types
                        .iter()
                        .map(|ty| self.constant(&Value::null(ty.clone())).map(Column::plain))
                        .collect::<Res<Vec<_>>>()?;
                    let mut select = Select::new(TableRef::empty(), columns);
                    select.filters.push(Expr::constant(Literal::boolean(false)));
                    Body::Select(select)
                } else {
                    self.values(rows, types.len())
                }
            }
            Op::Filter { expressions } => {
                let child = self.export_child(op, 0)?;
                self.filter(child, expressions)?
            }
            Op::Projection { expressions, .. } => {
                let child = self.export_child(op, 0)?;
                self.projection(child, expressions)?
            }
            Op::ColumnSelect { keep } => {
                let child = self.export_child(op, 0)?;
                let child_fields = child.fields.clone();
                let mut select = match child.body {
                    Body::Select(select) if !select.distinct => select,
                    body => self.wrap(Relation {
                        body,
                        fields: child_fields.clone(),
                    }),
                };
                let mut columns = Vec::new();
                for binding in keep {
                    let position = child_fields
                        .iter()
                        .position(|f| f.binding == *binding)
                        .ok_or("a column of the child")?;
                    columns.push(select.columns[position].clone());
                }
                select.columns = columns;
                Body::Select(select)
            }
            Op::Aggregate {
                groups,
                aggregates,
                grouping_sets,
                grouping_functions,
                ..
            } => {
                let child = self.export_child(op, 0)?;
                self.aggregate(child, groups, aggregates, grouping_sets, grouping_functions)?
            }
            Op::Window { windows, .. } => {
                let child = self.export_child(op, 0)?;
                self.window(child, windows)?
            }
            Op::Order { orders } => {
                let child = self.export_child(op, 0)?;
                Body::Select(self.order(child, orders)?)
            }
            Op::TopN {
                orders, limit, offset, ..
            } => {
                let child = self.export_child(op, 0)?;
                let mut select = self.order(child, orders)?;
                select.limit = Some(self.limit_values(Some(*limit), *offset, false)?);
                Body::Select(select)
            }
            Op::Limit { limit, offset, failure } => {
                if failure.is_some() {
                    return Err("A LIMIT that fails has no SQL export".into());
                }
                let child = self.export_child(op, 0)?;
                let mut select = self.limitable(child);
                select.limit = Some(self.limit_values(*limit, *offset, false)?);
                Body::Select(select)
            }
            Op::LimitPercent { percent, offset } => {
                let child = self.export_child(op, 0)?;
                let mut select = self.limitable(child);
                let limit = self.constant(&Value::double(*percent))?;
                let offset = (*offset > 0)
                    .then(|| self.constant(&Value::bigint(*offset as i64)))
                    .transpose()?;
                select.limit = Some((Some(limit), offset, true));
                Body::Select(select)
            }
            // `LogicalSecureView::ToSQL` names the view, which needs the mapping of its outputs
            Op::SecureView { .. } => return Err("secure view boundaries are not exported yet".into()),
            Op::Sample { options } => {
                let child = self.export_child(op, 0)?;
                let child_fields = child.fields.clone();
                let mut select = self.wrap(child);
                if !select.filters.is_empty() || select.sample.is_some() {
                    select = self.wrap(Relation {
                        body: Body::Select(select),
                        fields: child_fields,
                    });
                }
                select.sample = Some(options.clone());
                Body::Select(select)
            }
            Op::Distinct { targets, orders, .. } => {
                let mut child = self.export_child(op, 0)?;
                let child_fields = child.fields.clone();
                // the ORDER BY of a DISTINCT ON picks the row it keeps, so it goes on the SELECT
                if !orders.is_empty() {
                    child = Relation {
                        body: Body::Select(self.order(child, orders)?),
                        fields: child_fields.clone(),
                    };
                }
                let all_columns = targets.len() == child_fields.len()
                    && targets.iter().zip(&child_fields).all(|(t, f)| {
                        matches!(&t.kind, BoundExprKind::ColumnRef { binding, depth: 0 } if *binding == f.binding)
                    });
                // DISTINCT ON keeps the first row of each in the order of the ORDER BY of its SELECT
                let mut select = match child.body {
                    Body::Select(select)
                        if select.limit.is_none()
                            && !select.distinct
                            && (select.order.is_empty() || !all_columns)
                            && select.columns.iter().all(|c| !c.volatile) =>
                    {
                        select
                    }
                    body => self.wrap(Relation {
                        body,
                        fields: child_fields.clone(),
                    }),
                };
                if !all_columns {
                    let scope = Scope::of(&select, &child_fields);
                    let mut on = Vec::new();
                    for t in targets {
                        on.push(self.expr(&scope, t)?);
                    }
                    select.distinct_on = on;
                }
                select.distinct = true;
                Body::Select(select)
            }
            Op::Unnest { expressions, .. } => {
                let child = self.export_child(op, 0)?;
                let child_fields = child.fields.clone();
                let mut select = match child.body {
                    Body::Select(s) if s.is_simple() && s.columns.iter().all(|c| c.kind <= Kind::Computed) => s,
                    body => self.wrap(Relation {
                        body,
                        fields: child_fields.clone(),
                    }),
                };
                let scope = Scope::of(&select, &child_fields);
                for e in expressions {
                    let list = self.expr(&scope, e)?;
                    select.columns.push(Column {
                        expr: system_function("unnest", vec![list]),
                        kind: Kind::Unnest,
                        volatile: false,
                        throws: false,
                    });
                }
                Body::Select(select)
            }
            Op::Union { types, .. } => {
                let mut children = Vec::new();
                for child in &op.children {
                    let relation = self.export(child)?;
                    children.push(self.set_child(relation, types)?);
                }
                Body::Node(QueryNode {
                    kind: QueryNodeKind::SetOperation(SetOperationNode {
                        setop_type: SetOpType::Union,
                        setop_all: true,
                        children,
                    }),
                    modifiers: Vec::new(),
                    cte_map: Default::default(),
                })
            }
            Op::ExceptIntersect {
                types,
                except,
                all,
                keys,
                ..
            } => {
                // the keys are the collations of the column types, which binding the SQL gives again
                let _ = keys;
                let mut children = Vec::new();
                for child in &op.children {
                    let relation = self.export(child)?;
                    children.push(self.set_child(relation, types)?);
                }
                Body::Node(QueryNode {
                    kind: QueryNodeKind::SetOperation(SetOperationNode {
                        setop_type: if *except {
                            SetOpType::Except
                        } else {
                            SetOpType::Intersect
                        },
                        setop_all: *all,
                        children,
                    }),
                    modifiers: Vec::new(),
                    cte_map: Default::default(),
                })
            }
            Op::RecursiveCte {
                types,
                union_all,
                working,
                recurring,
                key,
                unresolved: recursive,
                ..
            } => {
                // USING KEY: the key columns, and the aggregates of the others, but for `last` of
                // a column, which is what a column USING KEY leaves out gets
                let mut key_targets: Vec<Expr> = key.keys.iter().map(|&i| Expr::column(vec![field_name(i)])).collect();
                for (column, aggregate) in &key.payload {
                    let mut arguments = Vec::new();
                    for child in &aggregate.children {
                        let mut child = child;
                        while let BoundExprKind::Cast { child: inner, .. } = &child.kind {
                            child = inner;
                        }
                        let argument = match &child.kind {
                            BoundExprKind::Reference(i) => Expr::column(vec![field_name(*i)]),
                            BoundExprKind::Constant(value) => self.constant(value)?,
                            _ => return Err("an aggregate of USING KEY over an expression has no SQL export".into()),
                        };
                        arguments.push(argument);
                    }
                    let implicit = aggregate.function.name == "last"
                        && matches!(aggregate.children.as_slice(), [c] if matches!(c.kind, BoundExprKind::Reference(i) if i == *column));
                    if implicit {
                        continue;
                    }
                    let mut call: Expr = FunctionExpr::new(aggregate.function.name.clone(), arguments).into();
                    call.alias = field_name(*column);
                    key_targets.push(call);
                }
                let name = self.alias();
                // `recurring.name` reads the rows so far
                self.working.push((
                    std::sync::Arc::as_ptr(recurring) as usize,
                    format!("recurring.{name}"),
                    0,
                ));
                let anchor = self.export_child(op, 0)?;
                let anchor = self.set_child(anchor, types)?;
                self.working
                    .push((std::sync::Arc::as_ptr(working) as usize, name.clone(), 0));
                let step = self.export(recursive);
                let (.., reads) = self.working.pop().expect("pushed above");
                let (.., recurring_reads) = self.working.pop().expect("pushed above");
                let reads = reads + recurring_reads;
                let mut step = step?;
                if reads == 0 {
                    // the optimizer took out the reads of the working table, which make it recursive:
                    // a count of its rows is one row whatever it holds
                    let step_fields = step.fields.clone();
                    let mut select = self.wrap(step);
                    let mut marker = self.named_source(&name, types.len());
                    marker.columns = vec![Column::plain(system_function("count_star", Vec::new()))];
                    marker.groups = Some(GroupByNode::default());
                    let marker = self.wrap(Relation {
                        body: Body::Select(marker),
                        fields: vec![Field {
                            binding: ColumnBinding::new(usize::MAX, 0),
                            ty: LogicalType::INTEGER,
                        }],
                    });
                    select.from = TableRefKind::Join(JoinRef {
                        left: Box::new(select.from),
                        right: Box::new(marker.from),
                        condition: None,
                        using_columns: Vec::new(),
                        join_type: JoinType::Inner,
                        ref_type: JoinRefType::Cross,
                        is_implicit: false,
                        nearest_count: 0,
                        nearest_descending: false,
                        nearest_approx: false,
                        ranking_expression: None,
                    })
                    .into();
                    step = Relation {
                        body: Body::Select(select),
                        fields: step_fields,
                    };
                }
                let step = self.set_child(step, types)?;
                let aliases: Vec<String> = (0..types.len()).map(field_name).collect();
                let cte = QueryNode {
                    kind: QueryNodeKind::RecursiveCte(RecursiveCteNode {
                        ctename: name.clone(),
                        union_all: *union_all,
                        left: Box::new(anchor),
                        right: Box::new(step),
                        aliases: aliases.clone(),
                        key_targets: key_targets.clone(),
                    }),
                    modifiers: Vec::new(),
                    cte_map: Default::default(),
                };
                self.ctes.push((
                    name.clone(),
                    CteInfo {
                        aliases,
                        query: Box::new(cte),
                        key_targets,
                        materialized: CteMaterialize::Default,
                    },
                ));
                Body::Select(self.named_source(&name, types.len()))
            }
            Op::WorkingTable { working, types, .. } => {
                let key = std::sync::Arc::as_ptr(working) as usize;
                let Some(entry) = self.working.iter_mut().find(|(k, ..)| *k == key) else {
                    return Err("A working table outside of its recursive CTE".into());
                };
                entry.2 += 1;
                let name = entry.1.clone();
                Body::Select(self.named_source(&name, types.len()))
            }
            Op::CrossProduct => {
                let left = self.export_child(op, 0)?;
                let right = self.export_child(op, 1)?;
                self.join(left, right, JoinType::Inner, JoinRefType::Cross, &[], None)?
            }
            Op::Join {
                join_type,
                conditions,
                predicate,
                ..
            } => {
                let left = self.export_child(op, 0)?;
                let right = self.export_child(op, 1)?;
                if *join_type == JoinType::Single || *join_type == JoinType::Mark {
                    return Err(format!("A {} join has no SQL export", join_type.name()));
                }
                self.join(
                    left,
                    right,
                    *join_type,
                    JoinRefType::Regular,
                    conditions,
                    predicate.as_ref(),
                )?
            }
            Op::AsOfJoin {
                join_type,
                conditions,
                left: left_expr,
                op: comparison,
                right: right_expr,
                predicate,
            } => {
                let left = self.export_child(op, 0)?;
                let right = self.export_child(op, 1)?;
                let inequality = BoundExpr::new(
                    BoundExprKind::Comparison {
                        op: *comparison,
                        left: Box::new(left_expr.clone()),
                        right: Box::new(right_expr.clone()),
                    },
                    LogicalType::BOOLEAN,
                );
                let predicate = match predicate {
                    Some(p) => BoundExpr::new(
                        BoundExprKind::Conjunction {
                            and: true,
                            children: vec![inequality, p.clone()],
                        },
                        LogicalType::BOOLEAN,
                    ),
                    None => inequality,
                };
                self.join(left, right, *join_type, JoinRefType::Asof, conditions, Some(&predicate))?
            }
            Op::PositionalJoin => {
                let left = self.export_child(op, 0)?;
                let right = self.export_child(op, 1)?;
                self.join(left, right, JoinType::Inner, JoinRefType::Positional, &[], None)?
            }
            Op::MarkJoin {
                conditions, predicate, ..
            } => {
                let left = self.export_child(op, 0)?;
                let right = self.export_child(op, 1)?;
                self.mark_join(left, right, conditions, predicate.as_ref())?
            }
            Op::LateralJoin {
                join_type,
                right,
                bindings,
                correlated,
                predicate,
            } => {
                let left = self.export_child(op, 0)?;
                self.lateral_join(left, right, *join_type, bindings, correlated, predicate.as_ref())?
            }
            Op::DelimJoin {
                join_type,
                conditions,
                predicate,
                delim,
                error_on_multiple_rows,
                ..
            } => {
                let left = self.export_child(op, 0)?;
                self.delim_join(
                    left,
                    op,
                    *join_type,
                    conditions,
                    predicate.as_ref(),
                    delim,
                    *error_on_multiple_rows,
                )?
            }
            Op::DelimGet { delim, .. } => {
                let key = std::sync::Arc::as_ptr(delim) as usize;
                let Some((_, values)) = self.delims.iter().find(|(k, _)| *k == key).cloned() else {
                    return Err("A duplicate-eliminated scan outside of its join".into());
                };
                Body::Select(Select::new(TableRef::empty(), values))
            }
            Op::ParquetScan { scan, table_index } => self.parquet_scan(scan, *table_index)?,
            Op::MatchRecognize { .. } => {
                return Err("MATCH_RECOGNIZE has no SQL export".into());
            }
            Op::CsvScan { .. } | Op::JsonScan { .. } => {
                return Err("The file scan has no SQL export".into());
            }
            // `ExportMaterializedCTE`: the definition is a WITH entry that runs once
            Op::MaterializedCte { data, .. } => {
                let name = self.alias();
                let types = op.children[0].types();
                let producer = self.export_child(op, 0)?;
                let query = self.set_child(producer, &types)?;
                let aliases: Vec<String> = (0..types.len()).map(field_name).collect();
                let at = self.ctes.len();
                self.ctes.push((
                    name.clone(),
                    CteInfo {
                        aliases,
                        query: Box::new(query),
                        key_targets: Vec::new(),
                        materialized: CteMaterialize::Always,
                    },
                ));
                self.working.push((std::sync::Arc::as_ptr(data) as usize, name, 0));
                let consumer = self.export_child(op, 1);
                let (.., reads) = self.working.pop().expect("pushed above");
                let consumer = consumer?;
                if reads == 0 {
                    self.ctes.remove(at);
                    return Err("SQL would discard the unreferenced CTE producer".into());
                }
                consumer.body
            }
            Op::CteRef { data, types, .. } => {
                let key = std::sync::Arc::as_ptr(data) as usize;
                let Some(entry) = self.working.iter_mut().find(|(k, ..)| *k == key) else {
                    return Err("A materialized CTE has no SQL export".into());
                };
                entry.2 += 1;
                let name = entry.1.clone();
                Body::Select(self.named_source(&name, types.len()))
            }
            Op::Insert { .. }
            | Op::Delete { .. }
            | Op::Update { .. }
            | Op::MergeInto { .. }
            | Op::CreateTableAs { .. }
            | Op::CreateTable { .. }
            | Op::CopyToFile { .. }
            | Op::CopyToParquet { .. } => {
                return Err("The data-modifying statement has no SQL export".into());
            }
        };
        let mut relation = Relation { body, fields };
        if let Body::Select(select) = &mut relation.body
            && select.columns.is_empty()
        {
            // `EnsureSQLProjection`: SQL needs a SELECT expression, even of a relation without columns
            select.columns.push(Column::plain(Expr::constant(Literal::null())));
        }
        Ok(relation)
    }

    /// `SELECT c0, ... FROM name`, a WITH entry.
    fn named_source(&mut self, name: &str, count: usize) -> Select {
        let alias = self.alias();
        let name = match name.split_once('.') {
            Some((schema, table)) => QualifiedName {
                qualification: vec![schema.to_string()],
                name: table.to_string(),
            },
            None => QualifiedName::new(name),
        };
        let mut from: TableRef = TableRefKind::BaseTable { name, at_clause: None }.into();
        from.alias = alias.clone();
        let columns = (0..count)
            .map(|i| Column::plain(Expr::column(vec![alias.clone(), field_name(i)])))
            .collect();
        Select::new(from, columns)
    }

    /// A child of a set operation: its columns of the set operation's types, without an ORDER BY
    /// or a LIMIT of its own, which would need parentheses.
    fn set_child(&mut self, relation: Relation, types: &[LogicalType]) -> Res<QueryNode> {
        let fields = relation.fields.clone();
        let select = match relation.body {
            Body::Select(s) if s.order.is_empty() && s.limit.is_none() => s,
            body => self.wrap(Relation { body, fields }),
        };
        let mut select = select;
        for ((column, field), ty) in select.columns.iter_mut().zip(&relation.fields).zip(types) {
            if field.ty != *ty {
                column.expr = cast(ty, column.expr.clone(), false);
            }
        }
        let names = (0..select.columns.len()).map(field_name).collect::<Vec<_>>();
        Ok(finish(select, &names))
    }

    fn values(&mut self, rows: Vec<Vec<Expr>>, count: usize) -> Body {
        let alias = self.alias();
        let mut from: TableRef = TableRefKind::ExpressionList(rows).into();
        from.alias = alias.clone();
        from.column_name_alias = (0..count).map(field_name).collect();
        let columns = (0..count)
            .map(|i| Column::plain(Expr::column(vec![alias.clone(), field_name(i)])))
            .collect();
        Body::Select(Select::new(from, columns))
    }

    fn get(
        &mut self,
        table: &TableCatalogEntry,
        table_index: usize,
        column_ids: &[usize],
        filters: &[BoundExpr],
        projection: Option<&[usize]>,
        extracts: &[Option<std::sync::Arc<super::logical::PushedExtract>>],
    ) -> Res<Body> {
        let alias = self.alias();
        let mut qualification = Vec::new();
        if let Some(database) = (self.database_of)(table) {
            qualification.push(database);
        }
        qualification.push(table.schema.clone());
        let mut from: TableRef = TableRefKind::BaseTable {
            name: QualifiedName {
                qualification,
                name: table.name.clone(),
            },
            at_clause: None,
        }
        .into();
        from.alias = alias.clone();
        // `ReconstructSQLSource`: the columns of the source go by their positions
        from.column_name_alias = (0..table.columns.len()).map(|i| format!("column{i}")).collect();
        let column_of = |id: usize| {
            // `TableFunctionColumn`: the `row_number` column is the window it stands for
            if id == super::logical::COLUMN_IDENTIFIER_ROW_NUMBER {
                let window = WindowExpr {
                    name: QualifiedName {
                        qualification: vec!["system".into(), "main".into()],
                        name: "row_number".into(),
                    },
                    start: ast::WindowBoundary::UnboundedPreceding,
                    end: ast::WindowBoundary::CurrentRowRange,
                    ..Default::default()
                };
                // a window: a WHERE on the scan goes in a SELECT around it
                return Column {
                    expr: ExprKind::Window(Box::new(window)).into(),
                    kind: Kind::Window,
                    volatile: false,
                    throws: false,
                };
            }
            let name = if id == COLUMN_IDENTIFIER_ROW_ID {
                "rowid".to_string()
            } else {
                format!("column{id}")
            };
            Column::plain(Expr::column(vec![alias.clone(), name]))
        };
        let type_of = |id: usize| {
            if id == COLUMN_IDENTIFIER_ROW_ID || id == super::logical::COLUMN_IDENTIFIER_ROW_NUMBER {
                LogicalType::BIGINT
            } else {
                table.columns[id].ty.clone()
            }
        };
        let mut scope = Scope::default();
        for (position, &id) in column_ids.iter().enumerate() {
            scope
                .columns
                .insert(ColumnBinding::new(table_index, position), (column_of(id), type_of(id)));
        }
        // a pushed field is the extract of its column
        for (position, extract) in extracts.iter().enumerate() {
            let Some(extract) = extract else { continue };
            let base = ColumnBinding::new(table_index, usize::MAX - position);
            let id = column_ids[position];
            scope.columns.insert(base, (column_of(id), type_of(id)));
            let mut expression = extract.expression.clone();
            expression.walk_mut(&mut |e| {
                if matches!(e.kind, BoundExprKind::Reference(0)) {
                    e.kind = BoundExprKind::ColumnRef {
                        binding: base,
                        depth: 0,
                    };
                }
            });
            let ty = expression.return_type.clone();
            let column = Column::plain(self.expr(&scope, &expression)?);
            scope
                .columns
                .insert(ColumnBinding::new(table_index, position), (column, ty));
        }
        let positions: Vec<usize> = match projection {
            Some(p) => p.to_vec(),
            None => (0..column_ids.len()).collect(),
        };
        let mut columns = Vec::with_capacity(positions.len());
        for &p in &positions {
            columns.push(match extracts.get(p) {
                Some(Some(_)) => scope.columns[&ColumnBinding::new(table_index, p)].0.clone(),
                _ => column_of(column_ids[p]),
            });
        }
        let mut select = Select::new(from, columns);
        for filter in filters {
            select.filters.push(self.expr(&scope, filter)?);
        }
        Ok(Body::Select(select))
    }

    /// `read_parquet` of the files it read (`ReconstructSQLSource`), for a scan of only their columns.
    fn parquet_scan(&mut self, scan: &crate::execution::parquet::ParquetScanData, table_index: usize) -> Res<Body> {
        use crate::execution::parquet::Int96As;
        if scan.file_row_number
            || scan.filename
            || !scan.hive.is_empty()
            || scan.schema.is_some()
            || scan.union_by_name
            || !scan.hive_overrides.is_empty()
            || scan.int96_as != Int96As::Timestamp
        {
            return Err("EXPLAIN (SQL) cannot render table function \"read_parquet\".".into());
        }
        let files = scan
            .files
            .iter()
            .map(|f| Expr::constant(Literal::string(f.clone())))
            .collect();
        let mut function = FunctionExpr::new("read_parquet", vec![system_function("list_value", files)]);
        if scan.binary_as_string {
            function.args.push(FunctionArgument {
                name: "binary_as_string".into(),
                expr: Expr::constant(Literal::boolean(true)),
            });
        }
        let count = scan.types.len();
        let alias = self.alias();
        let mut from: TableRef = TableRefKind::TableFunction {
            function: Box::new(function.into()),
            with_ordinality: false,
        }
        .into();
        from.alias = alias.clone();
        from.column_name_alias = (0..count).map(field_name).collect();
        let mut columns: Vec<Column> = (0..count)
            .map(|i| Column::plain(Expr::column(vec![alias.clone(), field_name(i)])))
            .collect();
        // the virtual columns are there by name only
        for column in &scan.virtual_columns {
            columns.push(Column::plain(Expr::column(vec![
                alias.clone(),
                column.name().to_string(),
            ])));
        }
        // the filters pushed into the scan (`table_filters`)
        let mut scope = Scope::default();
        for ((position, column), ty) in columns.iter().enumerate().zip(scan.output_types()) {
            scope
                .columns
                .insert(ColumnBinding::new(table_index, position), (column.clone(), ty));
        }
        // a pushed field is the extract of its column
        for (base, extract) in &scan.extracts {
            let binding = ColumnBinding::new(table_index, *base);
            let mut expression = extract.expression.clone();
            expression.walk_mut(&mut |e| {
                if matches!(e.kind, BoundExprKind::Reference(0)) {
                    e.kind = BoundExprKind::ColumnRef { binding, depth: 0 };
                }
            });
            let ty = expression.return_type.clone();
            let column = Column::plain(self.expr(&scope, &expression)?);
            scope
                .columns
                .insert(ColumnBinding::new(table_index, columns.len()), (column.clone(), ty));
            columns.push(column);
        }
        let mut select = Select::new(from, columns);
        for filter in &scan.filters {
            select.filters.push(self.expr(&scope, filter)?);
        }
        Ok(Body::Select(select))
    }

    fn filter(&mut self, child: Relation, expressions: &[BoundExpr]) -> Res<Body> {
        let fields = child.fields.clone();
        let mut select = self.simple(child);
        let mut read = Vec::new();
        for e in expressions {
            read_bindings(e, &mut read);
        }
        let positions: Vec<usize> = read
            .iter()
            .filter_map(|b| fields.iter().position(|f| f.binding == *b))
            .collect();
        // WHERE comes before what a window, an aggregate or UNNEST computes; a filter that has
        // effects or fails runs only on the rows that pass the filters below it
        let effects = expressions.iter().any(|e| has_effects(e) || (self.can_throw)(e));
        let movable = positions.iter().all(|&p| select.columns[p].kind == Kind::Column)
            && select.columns.iter().all(|c| c.kind <= Kind::Computed)
            && (!effects || select.filters.is_empty())
            && !select.unsafe_filters;
        if !movable {
            select = self.wrap(Relation {
                body: Body::Select(select),
                fields: fields.clone(),
            });
        }
        let scope = Scope::of(&select, &fields);
        for e in expressions {
            let predicate = self.expr(&scope, e)?;
            select.filters.push(predicate);
        }
        select.unsafe_filters |= effects;
        Ok(Body::Select(select))
    }

    /// Whether the columns at `positions` can be read again where `positions` says: none twice that
    /// gives something else each time, none that only a SELECT list can have.
    fn substitutable(select: &Select, read: &[usize], allow: Kind) -> bool {
        let mut seen = BTreeSet::new();
        read.iter().all(|&p| {
            let column = &select.columns[p];
            column.kind <= allow && (!column.volatile || seen.insert(p))
        })
    }

    fn projection(&mut self, child: Relation, expressions: &[BoundExpr]) -> Res<Body> {
        let fields = child.fields.clone();
        let fromless = matches!(&child.body, Body::Select(s) if matches!(s.from.kind, TableRefKind::Empty) && s.is_simple() && s.filters.is_empty());
        let mut select = match child.body {
            Body::Select(s) if !s.distinct => s,
            body => self.wrap(Relation {
                body,
                fields: fields.clone(),
            }),
        };
        let mut read = Vec::new();
        for e in expressions {
            read_bindings(e, &mut read);
        }
        let positions: Vec<usize> = read
            .iter()
            .filter_map(|b| fields.iter().position(|f| f.binding == *b))
            .collect();
        let pure = expressions.iter().all(|e| {
            matches!(
                e.kind,
                BoundExprKind::ColumnRef { depth: 0, .. } | BoundExprKind::Constant(_)
            )
        });
        // `ComposeProjection`: a CASE, TRY or COALESCE would decide whether a computed column runs, and
        // a column that can fail is only moved where it is read once, as it is
        let computed = |p: &usize| select.columns.get(*p).is_some_and(|c| c.kind != Kind::Column);
        let barrier = expressions.iter().any(has_composition_barrier) && positions.iter().any(computed);
        let failing = positions.iter().any(|&p| {
            select.columns.get(p).is_some_and(|c| c.throws) && {
                let uses = positions.iter().filter(|&&q| q == p).count();
                let forwarded = expressions.iter().any(|e| {
                    matches!(&e.kind, BoundExprKind::ColumnRef { binding, depth: 0 } if fields.get(p).is_some_and(|f| f.binding == *binding))
                });
                // (`HasEffectfulExpressions`: what has effects would see the rows the failure stops)
                uses != 1 || !forwarded || expressions.iter().any(has_effects)
            }
        });
        let ok = (fromless || !matches!(select.from.kind, TableRefKind::Empty) || select.columns.is_empty())
            && (select.limit.is_none() || pure)
            && !barrier
            && !failing
            && Self::substitutable(&select, &positions, Kind::Window);
        if !ok {
            select = self.wrap(Relation {
                body: Body::Select(select),
                fields: fields.clone(),
            });
        }
        let scope = Scope::of(&select, &fields);
        let mut columns = Vec::new();
        for e in expressions {
            columns.push(self.column(&scope, e)?);
        }
        select.columns = columns;
        Ok(Body::Select(select))
    }

    /// An expression as a column, with what it reads deciding its kind.
    fn column(&mut self, scope: &Scope, e: &BoundExpr) -> Res<Column> {
        let expr = self.expr(scope, e)?;
        let mut read = Vec::new();
        read_bindings(e, &mut read);
        let mut kind = if matches!(
            e.kind,
            BoundExprKind::ColumnRef { depth: 0, .. } | BoundExprKind::Constant(_)
        ) {
            Kind::Column
        } else {
            Kind::Computed
        };
        let mut volatile = e.is_volatile();
        let mut throws = kind != Kind::Column && (self.can_throw)(e);
        for b in &read {
            if let Some((column, _)) = scope.columns.get(b) {
                kind = kind.max(column.kind);
                volatile |= column.volatile;
                throws |= column.throws;
            }
        }
        Ok(Column {
            expr,
            kind,
            volatile,
            throws,
        })
    }

    fn aggregate(
        &mut self,
        child: Relation,
        groups: &[BoundExpr],
        aggregates: &[BoundAggregate],
        grouping_sets: &[BTreeSet<usize>],
        grouping_functions: &[Vec<usize>],
    ) -> Res<Body> {
        let fields = child.fields.clone();
        let mut select = match child.body {
            Body::Select(mut s) if s.groups.is_none() && !s.distinct && s.limit.is_none() => {
                // the order of the rows of an aggregate's input is not something it keeps
                s.order.clear();
                s
            }
            body => self.wrap(Relation {
                body,
                fields: fields.clone(),
            }),
        };
        let mut read = Vec::new();
        for g in groups {
            read_bindings(g, &mut read);
        }
        for a in aggregates {
            for c in a
                .children
                .iter()
                .chain(a.filter.iter())
                .chain(a.order_by.iter().map(|(e, _)| e))
            {
                read_bindings(c, &mut read);
            }
        }
        let positions: Vec<usize> = read
            .iter()
            .filter_map(|b| fields.iter().position(|f| f.binding == *b))
            .collect();
        // `ComposeInput`: a computed input that can fail, or that a CASE, TRY or COALESCE reads, keeps its SELECT
        let failing = positions.iter().any(|&p| select.columns.get(p).is_some_and(|c| c.throws));
        let barrier = groups
            .iter()
            .chain(aggregates.iter().flat_map(|a| a.children.iter().chain(a.filter.iter())))
            .any(has_composition_barrier)
            && positions.iter().any(|&p| select.columns.get(p).is_some_and(|c| c.kind != Kind::Column));
        if !Self::substitutable(&select, &positions, Kind::Computed)
            || matches!(select.from.kind, TableRefKind::Empty)
            || failing
            || barrier
        {
            select = self.wrap(Relation {
                body: Body::Select(select),
                fields: fields.clone(),
            });
        }
        let scope = Scope::of(&select, &fields);
        let mut group_exprs = Vec::new();
        for g in groups {
            group_exprs.push(self.expr(&scope, g)?);
        }
        let texts: BTreeSet<String> = group_exprs.iter().map(|g| g.to_string()).collect();
        if texts.len() != group_exprs.len() {
            // binding the SQL would make one group of them
            return Err("Distinct groups with the same SQL have no SQL export".into());
        }
        let mut columns: Vec<Column> = group_exprs
            .iter()
            .map(|g| Column {
                expr: g.clone(),
                kind: Kind::Aggregate,
                volatile: false,
                throws: false,
            })
            .collect();
        for a in aggregates {
            let expr = self.aggregate_call(&scope, a)?;
            columns.push(Column {
                expr,
                kind: Kind::Aggregate,
                volatile: false,
                throws: false,
            });
        }
        for function in grouping_functions {
            let children = function.iter().map(|&i| group_exprs[i].clone()).collect();
            columns.push(Column {
                expr: Expr::operator(OperatorType::Grouping, children),
                kind: Kind::Aggregate,
                volatile: false,
                throws: false,
            });
        }
        select.columns = columns;
        let sets = if grouping_sets.is_empty() {
            if groups.is_empty() && !aggregates.is_empty() {
                Vec::new()
            } else if groups.is_empty() {
                // a row even without aggregates: `GROUP BY ()`
                vec![BTreeSet::new()]
            } else {
                vec![(0..groups.len()).collect()]
            }
        } else {
            grouping_sets.to_vec()
        };
        select.groups = Some(GroupByNode {
            group_expressions: group_exprs,
            grouping_sets: sets,
        });
        Ok(Body::Select(select))
    }

    fn aggregate_call(&mut self, scope: &Scope, a: &BoundAggregate) -> Res<Expr> {
        if a.function.export.is_some() {
            return Err("EXPORT_STATE has no SQL export".into());
        }
        let mut args = Vec::new();
        for c in &a.children {
            args.push(self.expr(scope, c)?);
        }
        let mut function = FunctionExpr::new(a.function.name.clone(), args);
        function.name.qualification = vec!["system".into(), "main".into()];
        function.distinct = a.distinct;
        if let Some(filter) = &a.filter {
            function.filter = Some(Box::new(self.expr(scope, filter)?));
        }
        for (e, modifiers) in &a.order_by {
            function.order_by.push(order_node(*modifiers, self.expr(scope, e)?));
        }
        // `ExportAggregate`: a result type that binding specialized is restored with a cast
        let specialized = a.function.return_type != a.return_type
            && a.return_type.id() == LogicalTypeId::Decimal
            && a.function.return_type.id() == LogicalTypeId::Decimal;
        if specialized {
            return Ok(cast(&a.return_type, function.into(), false));
        }
        Ok(collate(&a.return_type, function.into()))
    }

    fn window(&mut self, child: Relation, windows: &[BoundWindow]) -> Res<Body> {
        let fields = child.fields.clone();
        let mut select = match child.body {
            Body::Select(s)
                if !s.distinct
                    && s.order.is_empty()
                    && s.limit.is_none()
                    && s.columns.iter().all(|c| c.kind <= Kind::Aggregate && !c.volatile) =>
            {
                s
            }
            body => self.wrap(Relation {
                body,
                fields: fields.clone(),
            }),
        };
        if matches!(select.from.kind, TableRefKind::Empty) && select.groups.is_none() {
            select = self.wrap(Relation {
                body: Body::Select(select),
                fields: fields.clone(),
            });
        }
        let scope = Scope::of(&select, &fields);
        for w in windows {
            let expr = self.window_call(&scope, w)?;
            select.columns.push(Column {
                expr,
                kind: Kind::Window,
                volatile: false,
                throws: false,
            });
        }
        Ok(Body::Select(select))
    }

    fn window_call(&mut self, scope: &Scope, w: &BoundWindow) -> Res<Expr> {
        use ast::WindowBoundary as B;
        let name = match w.function {
            WindowFunction::Aggregate => w.aggregate.as_ref().ok_or("window aggregate")?.name.clone(),
            WindowFunction::RowNumber => "row_number".into(),
            WindowFunction::Rank => "rank".into(),
            WindowFunction::DenseRank => "dense_rank".into(),
            WindowFunction::PercentRank => "percent_rank".into(),
            WindowFunction::CumeDist => "cume_dist".into(),
            WindowFunction::Ntile => "ntile".into(),
            WindowFunction::Lag => "lag".into(),
            WindowFunction::Lead => "lead".into(),
            WindowFunction::FirstValue => "first_value".into(),
            WindowFunction::LastValue => "last_value".into(),
            WindowFunction::NthValue => "nth_value".into(),
            WindowFunction::Fill => "fill".into(),
        };
        let mut window = WindowExpr {
            name: QualifiedName {
                qualification: vec!["system".into(), "main".into()],
                name,
            },
            ..Default::default()
        };
        for c in &w.children {
            window.args.push(FunctionArgument::positional(self.expr(scope, c)?));
        }
        for p in &w.partitions {
            window.partitions.push(self.expr(scope, p)?);
        }
        for (e, m) in &w.orders {
            window.orders.push(order_node(*m, self.expr(scope, e)?));
        }
        for (e, m) in &w.arg_orders {
            window.arg_orders.push(order_node(*m, self.expr(scope, e)?));
        }
        if let Some(filter) = &w.filter {
            window.filter = Some(Box::new(self.expr(scope, filter)?));
        }
        window.distinct = w.distinct;
        window.ignore_nulls = w.ignore_nulls;
        window.has_ignore_nulls = w.ignore_nulls;
        window.start = w.start;
        window.end = w.end;
        window.exclude = w.exclude;
        let range = |b: B| matches!(b, B::ExprPrecedingRange | B::ExprFollowingRange);
        let boundary = |this: &mut Self, b: B, e: &Option<BoundExpr>| -> Res<Option<Box<Expr>>> {
            let Some(e) = e else { return Ok(None) };
            if !range(b) {
                return Ok(Some(Box::new(this.expr(scope, e)?)));
            }
            // RANGE keeps the value at the boundary, the order key plus or minus the offset
            let BoundExprKind::Function { function, children, .. } = &uncast(e).kind else {
                return Err("A RANGE frame without its offset has no SQL export".into());
            };
            let order = w.orders.first().ok_or("RANGE frame without ORDER BY")?;
            let preceding = b == B::ExprPrecedingRange;
            let descending = order.1.order == SortOrder::Descending;
            let expected = if preceding != descending { "-" } else { "+" };
            if function.name != expected || children.len() != 2 || !same_expr(uncast(&children[0]), uncast(&order.0)) {
                return Err("A RANGE frame without its offset has no SQL export".into());
            }
            Ok(Some(Box::new(this.expr(scope, &children[1])?)))
        };
        window.start_expr = boundary(self, w.start, &w.start_expr)?;
        window.end_expr = boundary(self, w.end, &w.end_expr)?;
        Ok(collate(&w.return_type, ExprKind::Window(Box::new(window)).into()))
    }

    /// ORDER BY over a relation: the SELECT, with the order instead of any it had.
    fn order(&mut self, child: Relation, orders: &[BoundOrderByNode]) -> Res<Select> {
        let fields = child.fields.clone();
        let mut select = match child.body {
            Body::Select(s) if !s.distinct && s.limit.is_none() => s,
            body => self.wrap(Relation {
                body,
                fields: fields.clone(),
            }),
        };
        let mut read = Vec::new();
        for o in orders {
            read_bindings(&o.expr, &mut read);
        }
        let positions: Vec<usize> = read
            .iter()
            .filter_map(|b| fields.iter().position(|f| f.binding == *b))
            .collect();
        if !Self::substitutable(&select, &positions, Kind::Window)
            || positions.iter().any(|&p| select.columns[p].volatile)
            || (matches!(select.from.kind, TableRefKind::Empty) && select.groups.is_none())
        {
            select = self.wrap(Relation {
                body: Body::Select(select),
                fields: fields.clone(),
            });
        }
        let scope = Scope::of(&select, &fields);
        let mut order = Vec::new();
        for o in orders {
            let expr = self.expr(&scope, &o.expr)?;
            // a constant sorts nothing, and a bare integer would be a column position
            if matches!(o.expr.kind, BoundExprKind::Constant(_)) {
                continue;
            }
            order.push(order_node(o.modifiers, expr));
        }
        select.order = order;
        Ok(select)
    }

    fn limitable(&mut self, child: Relation) -> Select {
        let fields = child.fields.clone();
        match child.body {
            Body::Select(s) if s.limit.is_none() => s,
            body => self.wrap(Relation { body, fields }),
        }
    }

    fn limit_values(
        &mut self,
        limit: Option<u64>,
        offset: u64,
        percent: bool,
    ) -> Res<(Option<Expr>, Option<Expr>, bool)> {
        let as_bigint = |this: &mut Self, v: u64| -> Res<Expr> {
            match i64::try_from(v) {
                Ok(v) => this.constant(&Value::bigint(v)),
                Err(_) => Ok(Expr::constant(Literal::number(v.to_string()))),
            }
        };
        let limit = limit.map(|l| as_bigint(self, l)).transpose()?;
        let offset = (offset > 0).then(|| as_bigint(self, offset)).transpose()?;
        Ok((limit, offset, percent))
    }

    /// A child of a join: what goes into the FROM clause, the filters on it, and its columns.
    fn join_input(&mut self, relation: Relation, keep_filters: bool) -> (TableRef, Vec<Expr>, Vec<Column>) {
        let fields = relation.fields.clone();
        let mut select = match relation.body {
            Body::Select(s)
                if s.is_simple()
                    && s.sample.is_none()
                    && s.columns.iter().all(|c| c.kind <= Kind::Computed && !c.volatile)
                    && !matches!(s.from.kind, TableRefKind::Empty) =>
            {
                s
            }
            body => self.wrap(Relation {
                body,
                fields: fields.clone(),
            }),
        };
        if (!keep_filters || select.unsafe_filters) && !select.filters.is_empty() {
            select = self.wrap(Relation {
                body: Body::Select(select),
                fields,
            });
        }
        (select.from, select.filters, select.columns)
    }

    fn join(
        &mut self,
        left: Relation,
        right: Relation,
        join_type: JoinType,
        ref_type: JoinRefType,
        conditions: &[JoinCondition],
        predicate: Option<&BoundExpr>,
    ) -> Res<Body> {
        let (left_fields, right_fields) = (left.fields.clone(), right.fields.clone());
        // a filter of a side can go into WHERE where it removes rows the join would remove too, or
        // into ON for the side whose missing rows the join makes up
        let (left_keep, right_keep) = match (ref_type, join_type) {
            (JoinRefType::Positional, _) => (false, false),
            (JoinRefType::Asof, _) => (join_type == JoinType::Inner || join_type == JoinType::Left, false),
            (_, JoinType::Inner | JoinType::Semi | JoinType::Anti | JoinType::Left) => (true, true),
            (_, JoinType::Right | JoinType::RightSemi | JoinType::RightAnti) => (true, true),
            _ => (false, false),
        };
        let (left_from, left_filters, left_columns) = self.join_input(left, left_keep);
        let (right_from, right_filters, right_columns) = self.join_input(right, right_keep);
        let left_select = Select::new(TableRef::empty(), left_columns.clone());
        let right_select = Select::new(TableRef::empty(), right_columns.clone());
        let mut scope = Scope::of(&left_select, &left_fields);
        scope.add(&right_select, &right_fields);
        let mut on = Vec::new();
        for c in conditions {
            let op = c.comparison;
            on.push(Expr::comparison(
                op,
                self.expr(&scope, &c.left)?,
                self.expr(&scope, &c.right)?,
            ));
        }
        if let Some(p) = predicate {
            on.push(self.expr(&scope, p)?);
        }
        let mut filters = Vec::new();
        match join_type {
            JoinType::Inner => {
                filters.extend(left_filters);
                filters.extend(right_filters);
            }
            JoinType::Left | JoinType::Semi | JoinType::Anti => {
                filters.extend(left_filters);
                on.extend(right_filters);
            }
            JoinType::Right | JoinType::RightSemi | JoinType::RightAnti => {
                on.extend(left_filters);
                filters.extend(right_filters);
            }
            _ => {}
        }
        let (left_from, right_from, sql_type, columns) = match join_type {
            JoinType::Semi | JoinType::Anti => (left_from, right_from, join_type, left_columns),
            // the left side of a SEMI or ANTI join is the one it keeps
            JoinType::RightSemi => (right_from, left_from, JoinType::Semi, right_columns),
            JoinType::RightAnti => (right_from, left_from, JoinType::Anti, right_columns),
            _ => {
                let mut columns = left_columns;
                columns.extend(right_columns);
                (left_from, right_from, join_type, columns)
            }
        };
        let condition = if ref_type == JoinRefType::Cross || ref_type == JoinRefType::Positional {
            if !on.is_empty() {
                return Err("A condition on a cross product".into());
            }
            None
        } else {
            Some(conjoin(on).unwrap_or_else(|| Box::new(Expr::constant(Literal::boolean(true)))))
        };
        let from: TableRef = TableRefKind::Join(JoinRef {
            left: Box::new(left_from),
            right: Box::new(right_from),
            condition,
            using_columns: Vec::new(),
            join_type: sql_type,
            ref_type,
            is_implicit: false,
            nearest_count: 0,
            nearest_descending: false,
            nearest_approx: false,
            ranking_expression: None,
        })
        .into();
        let mut select = Select::new(from, columns);
        select.filters = filters;
        Ok(Body::Select(select))
    }

    /// A MARK join: the rows of the left side, and whether the right side has a match for each, as
    /// `JOIN BY (TYPE MARK)`, which binds to the same join.
    fn mark_join(
        &mut self,
        left: Relation,
        right: Relation,
        conditions: &[JoinCondition],
        predicate: Option<&BoundExpr>,
    ) -> Res<Body> {
        let (left_fields, right_fields) = (left.fields.clone(), right.fields.clone());
        if !conditions.is_empty() {
            // the ON of `JOIN BY (TYPE MARK)` binds to a predicate, and keys compare NULLs otherwise
            let mut select = self.markable(left);
            let inner = self.wrap(right);
            let mut scope = Scope::of(&select, &left_fields);
            scope.add(&inner, &right_fields);
            let mark = self.mark_value(&scope, &inner, conditions, predicate)?;
            select.columns.push(Column {
                expr: mark,
                kind: Kind::Computed,
                volatile: false,
                throws: false,
            });
            return Ok(Body::Select(select));
        }
        let (left_from, left_filters, left_columns) = self.join_input(left, true);
        let (right_from, _, right_columns) = self.join_input(right, false);
        let mut scope = Scope::of(&Select::new(TableRef::empty(), left_columns.clone()), &left_fields);
        scope.add(&Select::new(TableRef::empty(), right_columns), &right_fields);
        let mut on = Vec::new();
        for c in conditions {
            let op = c.comparison;
            on.push(Expr::comparison(
                op,
                self.expr(&scope, &c.left)?,
                self.expr(&scope, &c.right)?,
            ));
        }
        if let Some(p) = predicate {
            on.push(self.expr(&scope, p)?);
        }
        let from: TableRef = TableRefKind::Join(JoinRef {
            left: Box::new(left_from),
            right: Box::new(right_from),
            condition: Some(conjoin(on).unwrap_or_else(|| Box::new(Expr::constant(Literal::boolean(true))))),
            using_columns: Vec::new(),
            join_type: JoinType::Mark,
            ref_type: JoinRefType::Regular,
            is_implicit: false,
            nearest_count: 0,
            nearest_descending: false,
            nearest_approx: false,
            ranking_expression: None,
        })
        .into();
        let mut columns = left_columns;
        columns.push(Column {
            expr: Expr::column(vec!["__mark_join_marker".into()]),
            kind: Kind::Mark,
            volatile: false,
            throws: false,
        });
        let mut select = Select::new(from, columns);
        select.filters = left_filters;
        Ok(Body::Select(select))
    }

    /// The left side of a MARK or SINGLE join, which gets a column computed from each of its rows.
    fn markable(&mut self, left: Relation) -> Select {
        let fields = left.fields.clone();
        let select = self.simple(left);
        if select.columns.iter().any(|c| c.kind > Kind::Computed || c.volatile)
            || matches!(select.from.kind, TableRefKind::Empty)
        {
            return self.wrap(Relation {
                body: Body::Select(select),
                fields,
            });
        }
        select
    }

    /// `SELECT 1 FROM inner WHERE ...`, as EXISTS.
    fn exists(inner: &Select, predicates: Vec<Expr>) -> Expr {
        let mut sub = Select::new(
            inner.from.clone(),
            vec![Column::plain(Expr::constant(Literal::integer(1)))],
        );
        sub.filters = inner.filters.clone();
        sub.filters.extend(predicates);
        let node = finish(sub, &[]);
        ExprKind::Subquery {
            subquery: Box::new(SelectStatement { node }),
            subquery_type: SubqueryType::Exists,
            child: None,
            comparison: ComparisonOp::Equal,
        }
        .into()
    }

    /// The marker of a MARK join (`PhysicalJoin` with `JoinType::Mark`): TRUE if a right row with
    /// the keys of the left row satisfies the predicate, NULL if none does but for one it is NULL or
    /// a key compared with `=` is NULL while there are right rows, and FALSE otherwise.
    fn mark_value(
        &mut self,
        scope: &Scope,
        inner: &Select,
        conditions: &[JoinCondition],
        predicate: Option<&BoundExpr>,
    ) -> Res<Expr> {
        if let ([condition], None) = (conditions, predicate)
            && condition.comparison == ComparisonOp::Equal
        {
            // IN is exactly that
            let key = self.expr(scope, &condition.right)?;
            let mut sub = Select::new(inner.from.clone(), vec![Column::plain(key)]);
            sub.filters = inner.filters.clone();
            let node = finish(sub, &[]);
            let child = self.expr(scope, &condition.left)?;
            return Ok(ExprKind::Subquery {
                subquery: Box::new(SelectStatement { node }),
                subquery_type: SubqueryType::Any,
                child: Some(Box::new(child)),
                comparison: ComparisonOp::Equal,
            }
            .into());
        }
        let mut keys = Vec::new();
        let mut left_nulls = Vec::new();
        let mut right_nulls = Vec::new();
        for c in conditions {
            let l = self.expr(scope, &c.left)?;
            let r = self.expr(scope, &c.right)?;
            if !c.null_safe() {
                left_nulls.push(Expr::operator(OperatorType::IsNull, vec![l.clone()]));
                right_nulls.push(Expr::operator(OperatorType::IsNull, vec![r.clone()]));
            }
            let op = c.comparison;
            keys.push(Expr::comparison(op, l, r));
        }
        let predicate = predicate.map(|p| self.expr(scope, p)).transpose()?;
        let null = self.constant(&Value::null(LogicalType::BOOLEAN))?;
        let mut checks = Vec::new();
        let mut matched = keys.clone();
        matched.extend(predicate.clone());
        checks.push((Self::exists(inner, matched), Expr::constant(Literal::boolean(true))));
        if let Some(p) = predicate {
            let mut unknown = keys.clone();
            unknown.push(Expr::operator(OperatorType::IsNull, vec![p]));
            checks.push((Self::exists(inner, unknown), null.clone()));
        }
        if !left_nulls.is_empty() {
            let or = |children: Vec<Expr>| -> Expr {
                if children.len() == 1 {
                    children.into_iter().next().expect("one")
                } else {
                    ExprKind::Conjunction { and: false, children }.into()
                }
            };
            let mut any_null = left_nulls;
            any_null.push(Self::exists(inner, vec![or(right_nulls)]));
            let condition = ExprKind::Conjunction {
                and: true,
                children: vec![Self::exists(inner, Vec::new()), or(any_null)],
            }
            .into();
            checks.push((condition, null));
        }
        Ok(ExprKind::Case {
            checks,
            else_expr: Box::new(Expr::constant(Literal::boolean(false))),
        }
        .into())
    }

    /// A duplicate-eliminated join: the right side reads the keys of the left rows, so for one left
    /// row it is the right side of only its keys, a correlated subquery.
    #[allow(clippy::too_many_arguments)]
    fn delim_join(
        &mut self,
        left: Relation,
        op: &LogicalOperator,
        join_type: JoinType,
        conditions: &[JoinCondition],
        predicate: Option<&BoundExpr>,
        delim: &std::sync::Arc<parking_lot::Mutex<duckrs_common::column_data::ColumnDataCollection>>,
        error_on_multiple_rows: bool,
    ) -> Res<Body> {
        let left_fields = left.fields.clone();
        let mut select = self.markable(left);
        let left_scope = Scope::of(&select, &left_fields);
        let mut values = Vec::new();
        for c in conditions {
            values.push(self.column(&left_scope, &c.left)?);
        }
        self.delims.push((std::sync::Arc::as_ptr(delim) as usize, values));
        let right = self.export_child(op, 1);
        self.delims.pop();
        let right = right?;
        let right_fields = right.fields.clone();
        let inner = self.wrap(right);
        let mut scope = Scope::of(&select, &left_fields);
        scope.add(&inner, &right_fields);
        match join_type {
            JoinType::Mark => {
                let mark = self.mark_value(&scope, &inner, conditions, predicate)?;
                select.columns.push(Column {
                    expr: mark,
                    kind: Kind::Computed,
                    volatile: false,
                    throws: false,
                });
            }
            JoinType::Single => {
                let mut keys = Vec::new();
                for c in conditions {
                    let op = c.comparison;
                    keys.push(Expr::comparison(
                        op,
                        self.expr(&scope, &c.left)?,
                        self.expr(&scope, &c.right)?,
                    ));
                }
                if let Some(p) = predicate {
                    keys.push(self.expr(&scope, p)?);
                }
                // a scalar subquery per column: NULL without a row, an error with more than one
                for column in &inner.columns {
                    let mut sub = Select::new(inner.from.clone(), vec![column.clone()]);
                    sub.filters = inner.filters.clone();
                    sub.filters.extend(keys.iter().cloned());
                    if !error_on_multiple_rows {
                        sub.limit = Some(self.limit_values(Some(1), 0, false)?);
                    }
                    let node = finish(sub, &[]);
                    select.columns.push(Column {
                        expr: ExprKind::Subquery {
                            subquery: Box::new(SelectStatement { node }),
                            subquery_type: SubqueryType::Scalar,
                            child: None,
                            comparison: ComparisonOp::Equal,
                        }
                        .into(),
                        kind: Kind::Computed,
                        volatile: false,
                        throws: false,
                    });
                }
            }
            _ => {
                return Err(format!(
                    "A duplicate-eliminated {} join has no SQL export",
                    join_type.name()
                ));
            }
        }
        Ok(Body::Select(select))
    }

    fn lateral_join(
        &mut self,
        left: Relation,
        right: &LogicalOperator,
        join_type: JoinType,
        bindings: &[ColumnBinding],
        correlated: &[BoundExpr],
        predicate: Option<&BoundExpr>,
    ) -> Res<Body> {
        if !matches!(join_type, JoinType::Inner | JoinType::Left) {
            return Err(format!("A lateral {} join has no SQL export", join_type.name()));
        }
        let left_fields = left.fields.clone();
        let (left_from, left_filters, left_columns) = self.join_input(left, true);
        let left_select = Select::new(TableRef::empty(), left_columns.clone());
        let left_scope = Scope::of(&left_select, &left_fields);
        let mut outer = Scope::default();
        for (binding, e) in bindings.iter().zip(correlated) {
            let column = self.column(&left_scope, e)?;
            outer.columns.insert(*binding, (column, e.return_type.clone()));
        }
        self.outer.push(outer);
        let inner = self.export(right);
        self.outer.pop();
        let inner = inner?;
        let right_fields = inner.fields.clone();
        let count = right_fields.len();
        let alias = self.alias();
        let node = self.query(inner);
        let mut right_from = TableRef::subquery(SelectStatement { node });
        right_from.alias = alias.clone();
        right_from.column_name_alias = (0..count).map(field_name).collect();
        let right_columns: Vec<Column> = (0..count)
            .map(|i| Column::plain(Expr::column(vec![alias.clone(), field_name(i)])))
            .collect();
        let mut scope = left_scope;
        scope.add(&Select::new(TableRef::empty(), right_columns.clone()), &right_fields);
        let condition = match predicate {
            Some(p) => self.expr(&scope, p)?,
            None => Expr::constant(Literal::boolean(true)),
        };
        // `LATERAL`: a subquery that reads the columns of the left side
        let lateral = TableRef {
            kind: TableRefKind::Join(JoinRef {
                left: Box::new(left_from),
                right: Box::new(right_from),
                condition: Some(Box::new(condition)),
                using_columns: Vec::new(),
                join_type,
                ref_type: JoinRefType::Regular,
                is_implicit: false,
                nearest_count: 0,
                nearest_descending: false,
                nearest_approx: false,
                ranking_expression: None,
            }),
            alias: String::new(),
            column_name_alias: Vec::new(),
            sample: None,
            query_location: Default::default(),
        };
        let mut columns = left_columns;
        columns.extend(right_columns);
        let mut select = Select::new(lateral, columns);
        select.filters = left_filters;
        Ok(Body::Select(select))
    }

    // -----------------------------------------------------------------------------------------------------------
    // Expressions (`BoundExpressionSQLExporter`)
    // -----------------------------------------------------------------------------------------------------------

    fn lookup(&self, scope: &Scope, binding: ColumnBinding) -> Option<(Expr, LogicalType)> {
        if let Some((column, ty)) = scope.columns.get(&binding) {
            return Some((column.expr.clone(), ty.clone()));
        }
        self.outer
            .iter()
            .rev()
            .find_map(|s| s.columns.get(&binding))
            .map(|(column, ty)| (column.expr.clone(), ty.clone()))
    }

    /// `BoundExpressionSQLExportState::Export`.
    fn expr(&mut self, scope: &Scope, e: &BoundExpr) -> Res<Expr> {
        let expr = match &e.kind {
            BoundExprKind::Constant(value) => return self.constant(value),
            BoundExprKind::ColumnRef { binding, .. } => {
                let Some((mut expr, ty)) = self.lookup(scope, *binding) else {
                    return Err("The SQL column binding resolver has no matching entry".into());
                };
                expr.alias.clear();
                if !e.return_type.equals_including_collation(&ty) && e.return_type.id() == LogicalTypeId::Varchar {
                    if e.return_type.collation().is_none_or(str::is_empty) {
                        return Err("Clearing an input collation requires a SQL representation".into());
                    }
                    expr = collate(&e.return_type, expr);
                }
                return Ok(expr);
            }
            BoundExprKind::JoinFilter { .. } | BoundExprKind::TopNFilter { .. } => {
                return Err("A dynamic filter has no SQL export".into());
            }
            BoundExprKind::Reference(index) => {
                let Some(expr) = self.lambdas.last().and_then(|l| l.get(*index)) else {
                    return Err("A bound reference has no SQL export".into());
                };
                return Ok(expr.clone());
            }
            BoundExprKind::Cast { child, try_cast } => {
                if has_nested_collation(&e.return_type) {
                    return Err("This cast cannot preserve nested collations through SQL".into());
                }
                let child = self.expr(scope, child)?;
                cast(&e.return_type, child, *try_cast)
            }
            BoundExprKind::Comparison { op, left, right } => {
                Expr::comparison(*op, self.expr(scope, left)?, self.expr(scope, right)?)
            }
            BoundExprKind::Conjunction { and, children } => {
                let children = children.iter().map(|c| self.expr(scope, c)).collect::<Res<Vec<_>>>()?;
                ExprKind::Conjunction { and: *and, children }.into()
            }
            BoundExprKind::Operator { op, children } => {
                let children = children.iter().map(|c| self.expr(scope, c)).collect::<Res<Vec<_>>>()?;
                if *op == BoundOperator::HasError {
                    // spelled out, so that a macro of the same name cannot capture the replay
                    let mut call = FunctionExpr::new("has_error", children);
                    call.name.qualification = vec!["system".to_string(), "main".to_string()];
                    return Ok(Expr::from(ExprKind::Function(call)));
                }
                let op = match op {
                    BoundOperator::Not => OperatorType::Not,
                    BoundOperator::IsNull => OperatorType::IsNull,
                    BoundOperator::IsNotNull => OperatorType::IsNotNull,
                    BoundOperator::Coalesce => OperatorType::Coalesce,
                    BoundOperator::Try => OperatorType::Try,
                    BoundOperator::HasError => unreachable!("exported as a function"),
                    BoundOperator::In => OperatorType::In,
                    BoundOperator::NotIn => OperatorType::NotIn,
                };
                Expr::operator(op, children)
            }
            BoundExprKind::Case { checks, else_expr } => {
                let mut sql = Vec::new();
                for (when, then) in checks {
                    sql.push((self.expr(scope, when)?, self.expr(scope, then)?));
                }
                ExprKind::Case {
                    checks: sql,
                    else_expr: Box::new(self.expr(scope, else_expr)?),
                }
                .into()
            }
            BoundExprKind::Between { input, lower, upper } => ExprKind::Between {
                input: Box::new(self.expr(scope, input)?),
                lower: Box::new(self.expr(scope, lower)?),
                upper: Box::new(self.expr(scope, upper)?),
            }
            .into(),
            BoundExprKind::Sequence {
                op,
                sequence,
                arguments,
            } => {
                let name = match op {
                    SequenceOp::Next => "nextval",
                    SequenceOp::Current => "currval",
                    SequenceOp::Set => "setval",
                };
                let mut args = vec![Expr::constant(Literal::string(sequence.name.clone()))];
                for a in arguments {
                    args.push(self.expr(scope, a)?);
                }
                system_function(name, args)
            }
            BoundExprKind::Function { function, children, .. } => {
                // `TryExportCompressedMaterialization`: SQL has the values as they were, without the
                // optimizer's compression, so a compress or decompress function is its input
                if (function.name.starts_with("__internal_compress_")
                    || function.name.starts_with("__internal_decompress_"))
                    && let Some(input) = children.first()
                {
                    return self.expr(scope, input);
                }
                if let Some(expr) = self.extract_call(scope, &function.name, children)? {
                    return Ok(expr);
                }
                let mut args = Vec::new();
                for (i, c) in children.iter().enumerate() {
                    // what the binder needs as a constant stays a literal of its own type
                    let constant = CONSTANT_ARGUMENTS.iter().any(|(f, a)| *f == function.name && *a == i);
                    match &c.kind {
                        BoundExprKind::Constant(value) if constant && !value.is_null() => {
                            args.push(from_value(value)?.0);
                        }
                        _ => args.push(self.expr(scope, c)?),
                    }
                }
                // an operator is a call of its function too: the exported FunctionExpression is no operator
                if matches!(function.name.as_str(), "struct_pack" | "union_value") {
                    // the names of the arguments are those of the fields
                    let fields = e.return_type.struct_children();
                    let skip = usize::from(function.name == "union_value");
                    let mut call = FunctionExpr::new(function.name.clone(), Vec::new());
                    call.name.qualification = vec!["system".into(), "main".into()];
                    for (arg, (name, _)) in args.into_iter().zip(&fields[skip.min(fields.len())..]) {
                        call.args.push(FunctionArgument {
                            name: name.clone(),
                            expr: arg.with_alias(name.clone()),
                        });
                    }
                    call.into()
                } else {
                    system_function(&function.name, args)
                }
            }
            BoundExprKind::Subquery {
                subquery,
                child,
                correlated,
            } => {
                let Some(raw) = &subquery.raw else {
                    return Err("The subquery does not retain its plan".into());
                };
                if subquery.row || !subquery.error_on_multiple_rows {
                    return Err("The subquery has no SQL export".into());
                }
                let mut outer = Scope::default();
                for (binding, c) in subquery.correlated.iter().zip(correlated) {
                    let column = self.column(scope, c)?;
                    outer.columns.insert(*binding, (column, c.return_type.clone()));
                }
                // correlated columns the plan reads through the bindings of this query
                for (binding, value) in &scope.columns {
                    outer.columns.entry(*binding).or_insert_with(|| value.clone());
                }
                self.outer.push(outer);
                let relation = self.export(raw);
                self.outer.pop();
                let node = self.query(relation?);
                let statement = Box::new(SelectStatement { node });
                match subquery.kind {
                    SubqueryKind::Scalar => ExprKind::Subquery {
                        subquery: statement,
                        subquery_type: SubqueryType::Scalar,
                        child: None,
                        comparison: ComparisonOp::Equal,
                    }
                    .into(),
                    SubqueryKind::Exists => ExprKind::Subquery {
                        subquery: statement,
                        subquery_type: SubqueryType::Exists,
                        child: None,
                        comparison: ComparisonOp::Equal,
                    }
                    .into(),
                    SubqueryKind::Any(op) => {
                        let child = child.as_ref().ok_or("ANY without its child")?;
                        ExprKind::Subquery {
                            subquery: statement,
                            subquery_type: SubqueryType::Any,
                            child: Some(Box::new(self.expr(scope, child)?)),
                            comparison: op,
                        }
                        .into()
                    }
                }
            }
            BoundExprKind::ListLambda {
                kind,
                list,
                initial,
                captures,
                body,
            } => {
                let parameters = match kind {
                    LambdaKind::Transform | LambdaKind::Filter => 1,
                    LambdaKind::Reduce => 2,
                };
                let mut references = Vec::new();
                fn reads(e: &BoundExpr, out: &mut Vec<usize>) {
                    if let BoundExprKind::Reference(i) = e.kind {
                        out.push(i);
                    }
                    if !matches!(e.kind, BoundExprKind::ListLambda { .. }) {
                        for c in e.children() {
                            reads(c, out);
                        }
                    }
                }
                reads(body, &mut references);
                let with_index = references.contains(&parameters);
                let depth = self.lambdas.len();
                let mut names: Vec<String> = (0..parameters).map(|i| format!("l{depth}_{i}")).collect();
                if with_index {
                    names.push(format!("l{depth}_i"));
                }
                let mut slots: Vec<Expr> = names.iter().map(|n| Expr::column(vec![n.clone()])).collect();
                if !with_index {
                    slots.push(Expr::constant(Literal::null()));
                }
                for c in captures {
                    slots.push(self.expr(scope, c)?);
                }
                self.lambdas.push(slots);
                let body = self.expr(scope, body);
                self.lambdas.pop();
                let lambda = Expr::lambda(&names, body?);
                let name = match kind {
                    LambdaKind::Transform => "list_transform",
                    LambdaKind::Filter => "list_filter",
                    LambdaKind::Reduce => "list_reduce",
                };
                let mut args = vec![self.expr(scope, list)?, lambda];
                if let Some(initial) = initial {
                    args.push(self.expr(scope, initial)?);
                }
                system_function(name, args)
            }
        };
        let preserves =
            matches!(e.kind, BoundExprKind::Function { .. }) && e.return_type.id() != LogicalTypeId::Varchar;
        if preserves {
            return Ok(expr);
        }
        if has_nested_collation(&e.return_type) && !matches!(e.kind, BoundExprKind::Cast { .. }) {
            return Err("Changing nested collations requires a typed SQL representation".into());
        }
        Ok(collate(&e.return_type, expr))
    }

    /// `struct_extract` and `union_extract`, which bind the key to the position of the field: the
    /// key again (the unbind callbacks of C++).
    fn extract_call(&mut self, scope: &Scope, name: &str, children: &[BoundExpr]) -> Res<Option<Expr>> {
        if !matches!(name, "struct_extract" | "union_extract") || children.len() != 2 {
            return Ok(None);
        }
        let BoundExprKind::Constant(index) = &children[1].kind else {
            return Ok(None);
        };
        let Some(index) = index.as_i128().and_then(|i| usize::try_from(i).ok()) else {
            return Ok(None);
        };
        let ty = &children[0].return_type;
        let key = match (name, ty.id()) {
            ("struct_extract", LogicalTypeId::Tuple) => Expr::constant(Literal::integer(index as i64 + 1)),
            ("struct_extract", _) => {
                let Some((field, _)) = ty.struct_children().get(index) else {
                    return Ok(None);
                };
                Expr::constant(Literal::string(field.clone()))
            }
            _ => {
                let Some((member, _)) = ty.struct_children().get(index + 1) else {
                    return Ok(None);
                };
                Expr::constant(Literal::string(member.clone()))
            }
        };
        let child = self.expr(scope, &children[0])?;
        Ok(Some(system_function(name, vec![child, key])))
    }

    /// `ExportConstant`: the value, cast to its type.
    fn constant(&mut self, value: &Value) -> Res<Expr> {
        let ty = value.logical_type();
        let (expr, typed) = from_value(value)?;
        if typed || ty.id() == LogicalTypeId::SqlNull {
            return Ok(expr);
        }
        if ty.id() == LogicalTypeId::Varchar && ty.collation().is_some_and(|c| !c.is_empty()) {
            return Ok(expr);
        }
        Ok(cast(ty, expr, false))
    }
}

/// How many values a plan may spell out in its SQL, beyond which it has none.
const MAX_INLINE_VALUES: usize = 100_000;

/// How large a value spelled out in the SQL may be, in its bytes.
const MAX_VALUE_BYTES: usize = 64 << 10;

/// How many values an ENUM type spelled out in the SQL may have.
const MAX_ENUM_VALUES: usize = 4096;

/// How long the SQL of a plan may get.
const MAX_SQL_LENGTH: usize = 4 << 20;

/// The arguments that functions bind as constants (`ConstantSQLInput`): by function and position.
const CONSTANT_ARGUMENTS: &[(&str, usize)] = &[("struct_extract_at", 1), ("array_extract", 1), ("list_extract", 1)];

/// The expression under any casts.
fn uncast(e: &BoundExpr) -> &BoundExpr {
    match &e.kind {
        BoundExprKind::Cast { child, .. } => uncast(child),
        _ => e,
    }
}

/// Two expressions that compute the same.
fn same_expr(a: &BoundExpr, b: &BoundExpr) -> bool {
    format!("{:?}", a.kind) == format!("{:?}", b.kind)
}

/// The SELECT of `select`, its columns named `names`.
fn finish(select: Select, names: &[String]) -> QueryNode {
    let mut node = SelectNode {
        select_list: select
            .columns
            .into_iter()
            .enumerate()
            .map(|(i, c)| {
                let mut expr = c.expr;
                expr.alias = names.get(i).cloned().unwrap_or_default();
                expr
            })
            .collect(),
        from_table: select.from,
        where_clause: conjoin(select.filters),
        sample: select.sample,
        ..Default::default()
    };
    if let Some(groups) = select.groups {
        node.groups = groups;
    }
    node.having = conjoin(select.having);
    let mut modifiers = Vec::new();
    if select.distinct {
        modifiers.push(ResultModifier::Distinct(select.distinct_on));
    }
    if !select.order.is_empty() {
        modifiers.push(ResultModifier::Order(select.order));
    }
    if let Some((limit, offset, percent)) = select.limit {
        modifiers.push(ResultModifier::Limit {
            limit: limit.map(Box::new),
            offset: offset.map(Box::new),
            percent,
        });
    }
    QueryNode {
        kind: QueryNodeKind::Select(Box::new(node)),
        modifiers,
        cte_map: Default::default(),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Values (`ConstantExpression::FromValue`)
// ---------------------------------------------------------------------------------------------------------------

fn string_cast(value: &Value) -> Res<(Expr, bool)> {
    let text = value.try_to_string().map_err(|e| e.to_string())?;
    Ok((
        cast(value.logical_type(), Expr::constant(Literal::string(text)), false),
        true,
    ))
}

/// A value as SQL, and whether it is cast to its type already.
fn from_value(value: &Value) -> Res<(Expr, bool)> {
    use LogicalTypeId as T;
    let ty = value.logical_type();
    if ty.is_aggregate_state() {
        return Err("Aggregate state SQL parameters are not representable".into());
    }
    if ty.id() == T::Varchar && ty.collation().is_some_and(|c| !c.is_empty()) {
        let plain = value.reinterpret(&LogicalType::VARCHAR).map_err(|e| e.to_string())?;
        let (expr, _) = from_value(&plain)?;
        let expr = cast(&LogicalType::VARCHAR, expr, false);
        return Ok((collate(ty, expr), true));
    }
    if has_nested_collation(ty) {
        return Err("The nested value type has no SQL constructor".into());
    }
    if let ValueData::Bytes(bytes) = value.data()
        && bytes.len() > MAX_VALUE_BYTES
    {
        return Err("The value is too large to spell".into());
    }
    if value.is_null() {
        if ty.id() == T::SqlNull {
            return Ok((Expr::constant(Literal::null()), true));
        }
        return Ok((cast(ty, Expr::constant(Literal::null()), false), true));
    }
    let number = |text: String| Expr::constant(Literal::number(text));
    match (ty.id(), value.data()) {
        (T::Boolean, ValueData::Bool(b)) => Ok((Expr::constant(Literal::boolean(*b)), false)),
        (T::Varchar, ValueData::Bytes(bytes)) => {
            let text = String::from_utf8(bytes.clone()).map_err(|e| e.to_string())?;
            Ok((Expr::constant(Literal::string(text)), false))
        }
        (T::Float | T::Double, data) => {
            let d = match data {
                ValueData::Float(f) => *f as f64,
                ValueData::Double(d) => *d,
                _ => return string_cast(value),
            };
            if d.is_nan() || (d == 0.0 && d.is_sign_negative()) {
                return string_cast(value);
            }
            if d.is_infinite() {
                return Ok((number(if d < 0.0 { "-1e1000" } else { "1e1000" }.into()), false));
            }
            Ok((number(value.try_to_string().map_err(|e| e.to_string())?), false))
        }
        (
            T::TinyInt
            | T::SmallInt
            | T::Integer
            | T::BigInt
            | T::HugeInt
            | T::UTinyInt
            | T::USmallInt
            | T::UInteger
            | T::UBigInt
            | T::UHugeInt
            | T::Decimal,
            _,
        ) => {
            let text = value.try_to_string().map_err(|e| e.to_string())?;
            Ok((number(text), false))
        }
        (T::Struct, ValueData::Struct(children)) => {
            let names = ty.struct_children();
            let unnamed = names.iter().all(|(n, _)| n.is_empty());
            let mut args = Vec::new();
            for ((name, _), child) in names.iter().zip(children) {
                let (expr, _) = from_value(child)?;
                if unnamed {
                    args.push(FunctionArgument::positional(expr));
                } else {
                    args.push(FunctionArgument {
                        name: name.clone(),
                        expr: expr.with_alias(name.clone()),
                    });
                }
            }
            let mut function = FunctionExpr::new(if unnamed { "row" } else { "struct_pack" }, Vec::new());
            function.name.qualification = vec!["system".into(), "main".into()];
            function.args = args;
            Ok((function.into(), false))
        }
        (T::List | T::Array, ValueData::List(children)) => {
            let items = children
                .iter()
                .map(|c| from_value(c).map(|(e, _)| e))
                .collect::<Res<Vec<_>>>()?;
            let name = if ty.id() == T::Array && !items.is_empty() {
                "array_value"
            } else {
                "list_value"
            };
            Ok((cast(ty, system_function(name, items), false), true))
        }
        (T::Map, ValueData::List(entries)) => {
            let mut keys = Vec::new();
            let mut values = Vec::new();
            for entry in entries {
                let ValueData::Struct(pair) = entry.data() else {
                    return Err("A MAP entry that is not a pair".into());
                };
                keys.push(from_value(&pair[0])?.0);
                values.push(from_value(&pair[1])?.0);
            }
            let map = system_function(
                "map",
                vec![
                    system_function("list_value", keys),
                    system_function("list_value", values),
                ],
            );
            Ok((cast(ty, map, false), true))
        }
        (T::Geometry, _) if ty.crs().is_some() => Err("A GEOMETRY of a CRS has no SQL constructor".into()),
        (T::Enum, _) if ty.enum_values().is_empty() => Err("The nested value type has no SQL constructor".into()),
        (T::Enum, _) if ty.alias().is_none() && ty.enum_values().len() > MAX_ENUM_VALUES => {
            Err("The ENUM type is too large to spell".into())
        }
        (T::Type | T::Table | T::Pointer | T::Lambda | T::Validity | T::LegacyAggregateState, _) => {
            Err("The value type has no SQL constructor".into())
        }
        (T::Union, _) => Err("A UNION value has no SQL constructor".into()),
        _ => string_cast(value),
    }
}
