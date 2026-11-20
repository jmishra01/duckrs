// Ported from duckdb/src/planner/binder/query_node/bind_cte_node.cpp @ rust-port-baseline
//
//! INSERT, UPDATE, DELETE, MERGE and COPY as the bodies of the CTEs of a top-level query. C++
//! materializes each of them, and its pipeline dependencies run them to completion one after the
//! other before the query starts, whether the query reads them or not. Here the client does the
//! same: each one runs as a statement of its own, in the transaction of the query, and the query
//! (and the later CTEs) read its rows as a table of that name.

use super::*;
use crate::planner::trigger_scope::{self, TransitionTable, TriggerScope};
use duckrs_parser::ast::{CteInfo, CteMap, QueryNode, QueryNodeKind};

/// Whether a CTE body writes or copies, rather than reads.
pub(super) fn is_side_effecting(node: &QueryNode) -> bool {
    matches!(
        node.kind,
        QueryNodeKind::Insert(_)
            | QueryNodeKind::Update(_)
            | QueryNodeKind::Delete(_)
            | QueryNodeKind::Merge(_)
            | QueryNodeKind::Copy(_)
    )
}

/// The WITH clause of a top-level statement: of a SELECT, of the query of a COPY TO, or of an
/// INSERT, UPDATE or DELETE that comes after it.
fn cte_map(statement: &mut Statement) -> Option<&mut CteMap> {
    match statement {
        Statement::Select(select) => Some(&mut select.node.cte_map),
        Statement::Copy(copy) if !copy.is_from => copy.select.as_deref_mut().map(|node| &mut node.cte_map),
        Statement::Insert(insert) => Some(&mut insert.cte_map),
        Statement::Update(update) => Some(&mut update.cte_map),
        Statement::Delete(delete) => Some(&mut delete.cte_map),
        _ => None,
    }
}

/// The side-effecting CTEs of a top-level statement, in order, and the statement without them.
pub(super) fn split(statement: &Statement) -> Option<(Vec<(String, CteInfo)>, Statement)> {
    let has = match statement {
        Statement::Select(select) => &select.node.cte_map,
        Statement::Copy(copy) if !copy.is_from => &copy.select.as_deref()?.cte_map,
        Statement::Insert(insert) => &insert.cte_map,
        Statement::Update(update) => &update.cte_map,
        Statement::Delete(delete) => &delete.cte_map,
        _ => return None,
    };
    if !has.0.iter().any(|(_, cte)| is_side_effecting(&cte.query)) {
        return None;
    }
    let mut rest = statement.clone();
    let map = cte_map(&mut rest)?;
    let (dml, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut map.0)
        .into_iter()
        .partition(|(_, cte)| is_side_effecting(&cte.query));
    map.0 = kept;
    Some((dml, rest))
}

/// The statement a CTE body is.
fn body_statement(node: &QueryNode) -> Statement {
    match &node.kind {
        QueryNodeKind::Insert(insert) => {
            let mut insert = insert.clone();
            if !node.cte_map.0.is_empty() {
                insert.cte_map = node.cte_map.clone();
            }
            Statement::Insert(insert)
        }
        QueryNodeKind::Update(update) => {
            let mut update = update.clone();
            if !node.cte_map.0.is_empty() {
                update.cte_map = node.cte_map.clone();
            }
            Statement::Update(update)
        }
        QueryNodeKind::Delete(delete) => {
            let mut delete = delete.clone();
            if !node.cte_map.0.is_empty() {
                delete.cte_map = node.cte_map.clone();
            }
            Statement::Delete(delete)
        }
        QueryNodeKind::Merge(merge) => Statement::Merge(merge.clone()),
        QueryNodeKind::Copy(copy) => Statement::Copy(copy.clone()),
        _ => unreachable!("only side-effecting CTEs are run"),
    }
}

/// What the query sees: the tables already there, and `tables`.
fn scope_with(tables: &[TransitionTable]) -> Arc<TriggerScope> {
    let mut all = trigger_scope::current()
        .map(|scope| scope.tables.clone())
        .unwrap_or_default();
    all.extend(tables.iter().cloned());
    Arc::new(TriggerScope { tables: all, row: None })
}

/// The table a CTE's rows are read as, with its column aliases.
fn table_of(
    name: &str,
    cte: &CteInfo,
    names: Vec<String>,
    types: Vec<LogicalType>,
    rows: ColumnDataCollection,
) -> TransitionTable {
    let mut names = names;
    for (name, alias) in names.iter_mut().zip(&cte.aliases) {
        name.clone_from(alias);
    }
    TransitionTable {
        name: name.to_string(),
        names,
        types,
        rows: Arc::new(Mutex::new(rows)),
    }
}

impl ClientContext {
    /// Runs the side-effecting CTEs of `select`, then the rest of it; all of it or none.
    pub(super) fn run_with_dml_ctes(
        &mut self,
        ctes: &[(String, CteInfo)],
        rest: &Statement,
    ) -> Result<MaterializedQueryResult> {
        let implicit = self.transaction.is_none();
        if implicit {
            self.run_transaction_statement(TransactionType::Begin, TransactionModifier::Default)?;
        }
        let result = self.run_dml_ctes(ctes, rest);
        if implicit {
            let end = if result.is_ok() {
                TransactionType::Commit
            } else {
                TransactionType::Rollback
            };
            let ended = self.run_transaction_statement(end, TransactionModifier::Default);
            if result.is_ok() {
                ended?;
            }
        }
        result
    }

    fn run_dml_ctes(&mut self, ctes: &[(String, CteInfo)], rest: &Statement) -> Result<MaterializedQueryResult> {
        let mut tables = Vec::with_capacity(ctes.len());
        for (name, cte) in ctes {
            let statement = body_statement(&cte.query);
            let result = trigger_scope::with_scope(Some(scope_with(&tables)), || self.run_statement(&statement))?;
            tables.push(table_of(name, cte, result.names, result.types, result.collection));
        }
        let before = std::mem::replace(&mut self.after_dml_ctes, true);
        let result = trigger_scope::with_scope(Some(scope_with(&tables)), || self.run_statement(rest));
        self.after_dml_ctes = before;
        result
    }

    /// `prepare_statement` of a query with side-effecting CTEs: each body binds, its rows a table
    /// of the types it returns, for what comes after it.
    pub(super) fn bind_dml_ctes(
        &mut self,
        ctes: &[(String, CteInfo)],
        rest: &Statement,
        parameters: &Arc<ParameterValues>,
    ) -> Result<()> {
        let catalog = self.catalog();
        let mut tables = Vec::with_capacity(ctes.len());
        for (name, cte) in ctes {
            let statement = body_statement(&cte.query);
            let bound = trigger_scope::with_scope(Some(scope_with(&tables)), || {
                let mut binder = Binder::with_options(&catalog, self.bind_options());
                binder.parameters = Some(parameters.clone());
                binder.bind(&statement)
            })?;
            let rows = ColumnDataCollection::new(bound.types.clone());
            tables.push(table_of(name, cte, bound.names, bound.types, rows));
        }
        trigger_scope::with_scope(Some(scope_with(&tables)), || {
            let mut binder = Binder::with_options(&catalog, self.bind_options());
            binder.parameters = Some(parameters.clone());
            binder.bind(rest).map(drop)
        })
    }

    /// EXPLAIN of a query with side-effecting CTEs: a CTE node per body (`PhysicalCTE`) over the
    /// plan of the body and what comes after it.
    pub(super) fn explain_dml_ctes(
        &mut self,
        ctes: &[(String, CteInfo)],
        rest: &Statement,
        analyze: bool,
    ) -> Result<([explain::ExplainNode; 3], Vec<String>, Vec<LogicalType>)> {
        let mut tables = Vec::with_capacity(ctes.len());
        let mut bodies = Vec::with_capacity(ctes.len());
        for (name, cte) in ctes {
            let statement = body_statement(&cte.query);
            let (trees, names, types) =
                trigger_scope::with_scope(Some(scope_with(&tables)), || self.explain_trees(&statement, analyze))?;
            let rows = ColumnDataCollection::new(types.clone());
            tables.push(table_of(name, cte, names, types, rows));
            bodies.push((name.clone(), trees));
        }
        let before = std::mem::replace(&mut self.after_dml_ctes, true);
        let explained = trigger_scope::with_scope(Some(scope_with(&tables)), || self.explain_trees(rest, analyze));
        self.after_dml_ctes = before;
        let (mut trees, names, types) = explained?;
        for (name, body) in bodies.into_iter().rev() {
            let [logical, optimized, physical] = body;
            let [rest_logical, rest_optimized, rest_physical] = trees;
            trees = [
                explain::ExplainNode::dependent_cte(&name, logical, rest_logical),
                explain::ExplainNode::dependent_cte(&name, optimized, rest_optimized),
                explain::ExplainNode::dependent_cte(&name, physical, rest_physical),
            ];
        }
        Ok((trees, names, types))
    }
}
