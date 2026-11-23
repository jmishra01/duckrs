// Ported from duckdb/src/planner/binder/statement/bind_create.cpp (`BindCreateTriggerInfo`),
// duckdb/src/planner/binder/query_node/bind_trigger_expansion.cpp and
// duckdb/src/catalog/catalog_entry/trigger_catalog_entry.cpp @ rust-port-baseline
//
//! Triggers: CREATE TRIGGER and DROP TRIGGER, and running them around an INSERT, UPDATE or DELETE.
//!
//! C++ expands a statement with triggers into a chain of data-modifying CTEs: the BEFORE bodies, the
//! statement with RETURNING *, then the AFTER bodies, which read the rows it returned as their
//! transition tables; FOR EACH ROW bodies run per row with the row as `new` or `old`. Here the
//! client runs that chain itself, one statement after the other in one transaction, with the rows
//! of the statement as transition tables the bodies bind to.

use super::*;
use crate::catalog::{
    DEFAULT_SCHEMA, TableCatalogEntry, TriggerCatalogEntry, TriggerEvent, TriggerForEach, TriggerTiming,
};
use crate::planner::statement_names::{body_statement, referenced_names};
use crate::planner::trigger_scope::{self, RowScope, TransitionTable, TriggerScope};
use duckrs_common::types::sql_identifier;
use duckrs_parser::ast::{QueryNode, QueryNodeKind, TableRef, TableRefKind};
use std::sync::atomic::Ordering as AtomicOrdering2;

/// The table an INSERT, UPDATE or DELETE writes, and what it does to it.
pub(super) struct TriggerTarget {
    pub schema: Arc<crate::catalog::SchemaCatalogEntry>,
    pub table: Arc<TableCatalogEntry>,
    pub event: TriggerEvent,
    /// UPDATE: the columns it sets
    pub set_columns: Vec<String>,
    /// what the statement calls the table
    pub alias: String,
}

fn not_implemented(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::NotImplemented, message)
}

fn binder_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Binder, message)
}

/// The table a body writes and the events it causes there: (qualification, name, events).
fn body_target(action: &QueryNode) -> Option<(Vec<String>, String, Vec<TriggerEvent>)> {
    let base = |table: &TableRef| match &table.kind {
        TableRefKind::BaseTable { name, .. } => Some((name.qualification.clone(), name.name.clone())),
        _ => None,
    };
    match &action.kind {
        QueryNodeKind::Insert(insert) => Some((
            insert.table.qualification.clone(),
            insert.table.name.clone(),
            vec![TriggerEvent::Insert],
        )),
        QueryNodeKind::Update(update) => base(&update.table).map(|(q, n)| (q, n, vec![TriggerEvent::Update])),
        QueryNodeKind::Delete(delete) => base(&delete.table).map(|(q, n)| (q, n, vec![TriggerEvent::Delete])),
        QueryNodeKind::Merge(merge) => base(&merge.target).map(|(q, n)| {
            (
                q,
                n,
                vec![TriggerEvent::Insert, TriggerEvent::Update, TriggerEvent::Delete],
            )
        }),
        _ => None,
    }
}

/// The columns `columns` of `result`, named `names`.
fn project(
    result: &MaterializedQueryResult,
    columns: std::ops::Range<usize>,
    names: Vec<String>,
) -> Result<MaterializedQueryResult> {
    let types = result.types[columns.clone()].to_vec();
    let mut collection = ColumnDataCollection::new(types.clone());
    for chunk in result.collection.chunks() {
        let part =
            duckrs_common::vector::DataChunk::from_columns(chunk.columns()[columns.clone()].to_vec(), chunk.len());
        collection.append(&part);
    }
    Ok(MaterializedQueryResult {
        names,
        types,
        collection,
    })
}

/// `InlineGeneratedColumnRefs`: `expr` with the generated columns it reads replaced by their
/// expressions, so that it reads only stored columns, whatever order they are declared in.
fn inline_generated(expr: &Expr, table: &TableCatalogEntry, depth: usize) -> Expr {
    let mut result = expr.clone();
    if depth > 64 {
        return result;
    }
    duckrs_parser::transformer::visit_expr_mut(&mut result, &mut |e: &mut Expr| {
        if let ExprKind::ColumnRef(names) = &e.kind
            && let [name] = names.as_slice()
            && let Some(generated) = table.generated.iter().find(|g| g.name.eq_ignore_ascii_case(name))
        {
            let ty = duckrs_parser::ast::CastType::Resolved(generated.ty.clone());
            *e = Expr::cast(inline_generated(&generated.expr, table, depth + 1), ty);
        }
    });
    result
}

/// How many rows the statement changed, by what it returned for the triggers.
fn rows_of(new_rows: &Option<MaterializedQueryResult>, old_rows: &Option<MaterializedQueryResult>) -> usize {
    new_rows
        .as_ref()
        .or(old_rows.as_ref())
        .map_or(0, |rows| rows.collection.len())
}

/// The schema with the entry `name`: the one it is qualified with, or the first of the search path
/// that has it.
fn table_schema(
    catalog: &crate::catalog::Catalog,
    qualification: &[String],
    name: &str,
) -> Option<Arc<crate::catalog::SchemaCatalogEntry>> {
    match qualification.last() {
        Some(schema) => catalog.get_schema(schema).filter(|s| s.get_entry(name).is_some()),
        None => [TEMP_SCHEMA, DEFAULT_SCHEMA]
            .iter()
            .filter_map(|schema| catalog.get_schema(schema))
            .find(|schema| schema.get_entry(name).is_some()),
    }
}

impl ClientContext {
    /// The table `statement` changes, if it is an INSERT, UPDATE or DELETE of a table there is.
    pub(super) fn trigger_target(&self, statement: &Statement) -> Option<TriggerTarget> {
        let (name, event, set_columns, alias) = match statement {
            Statement::Insert(insert) => (
                insert.table.clone(),
                TriggerEvent::Insert,
                Vec::new(),
                insert.table_alias.clone().unwrap_or_else(|| insert.table.name.clone()),
            ),
            Statement::Update(update) => {
                let TableRefKind::BaseTable { name, .. } = &update.table.kind else {
                    return None;
                };
                let alias = if update.table.alias.is_empty() {
                    name.name.clone()
                } else {
                    update.table.alias.clone()
                };
                (
                    name.clone(),
                    TriggerEvent::Update,
                    update.set_info.columns.clone(),
                    alias,
                )
            }
            Statement::Delete(delete) => {
                let TableRefKind::BaseTable { name, .. } = &delete.table.kind else {
                    return None;
                };
                let alias = if delete.table.alias.is_empty() {
                    name.name.clone()
                } else {
                    delete.table.alias.clone()
                };
                (name.clone(), TriggerEvent::Delete, Vec::new(), alias)
            }
            // COPY ... FROM inserts
            Statement::Copy(copy) if copy.is_from => (
                copy.table.clone(),
                TriggerEvent::Insert,
                Vec::new(),
                copy.table.name.clone(),
            ),
            _ => return None,
        };
        let schema = table_schema(&self.catalog(), &name.qualification, &name.name)?;
        let TableOrView::Table(table) = schema.get_entry(&name.name)? else {
            return None;
        };
        Some(TriggerTarget {
            schema,
            table,
            event,
            set_columns,
            alias,
        })
    }

    /// `Binder::TryExpandTriggers` and `TryExpandRowTriggers`: `statement` with the triggers of its
    /// table around it, or `None` if it has none to run.
    pub(super) fn run_with_triggers(&mut self, statement: &Statement) -> Result<Option<MaterializedQueryResult>> {
        if let Statement::Merge(merge) = statement
            && let TableRefKind::BaseTable { name, .. } = &merge.target.kind
            && let Some(schema) = table_schema(&self.catalog(), &name.qualification, &name.name)
            && !schema.triggers_on(&name.name).is_empty()
            && !self
                .trigger_tables
                .contains(&format!("{}.{}", schema.key(), name.name.to_lowercase()))
        {
            // a MERGE in a trigger body is fine, the triggers of its target don't run
            return Err(not_implemented("MERGE INTO is not supported on tables with triggers"));
        }
        let Some(target) = self.trigger_target(statement) else {
            return Ok(None);
        };
        let key = format!("{}.{}", target.schema.key(), target.table.name.to_lowercase());
        if self.trigger_tables.contains(&key) {
            return Ok(None);
        }
        let fires = |trigger: &TriggerCatalogEntry| {
            trigger.event == target.event
                && (trigger.columns.is_empty()
                    || trigger
                        .columns
                        .iter()
                        .any(|c| target.set_columns.iter().any(|s| s.eq_ignore_ascii_case(c))))
        };
        let triggers: Vec<Arc<TriggerCatalogEntry>> = target
            .schema
            .triggers_on(&target.table.name)
            .into_iter()
            .filter(|t| fires(t))
            .collect();
        if triggers.is_empty() {
            return Ok(None);
        }
        let row_triggers: Vec<_> = triggers
            .iter()
            .filter(|t| t.for_each == TriggerForEach::Row && t.timing == TriggerTiming::After)
            .cloned()
            .collect();
        let returning = match statement {
            Statement::Insert(insert) => insert.returning.clone(),
            Statement::Update(update) => update.returning.clone(),
            Statement::Delete(delete) => delete.returning.clone(),
            _ => Vec::new(),
        };
        // `BoundBodyTargetsTable` and `BoundBodyContainsTrigger`
        for trigger in &row_triggers {
            let Some((qualification, name, _)) = body_target(&trigger.action) else {
                continue;
            };
            let Some((schema, written)) = self.resolve_table(&target.schema, &qualification, &name) else {
                continue;
            };
            let (trigger_name, table_name) = (
                sql_quoted_identifier(&trigger.name),
                sql_quoted_identifier(&target.table.name),
            );
            if Arc::ptr_eq(&written, &target.table) {
                return Err(binder_error(format!(
                    "FOR EACH ROW trigger {trigger_name} on table {table_name} writes to the trigger table \
                     (self-referential triggers are not supported)"
                )));
            }
            if schema
                .triggers_on(&written.name)
                .iter()
                .any(|t| t.for_each == TriggerForEach::Row)
            {
                return Err(not_implemented(format!(
                    "FOR EACH ROW trigger {trigger_name} on table {table_name} writes to a table that has its own \
                     FOR EACH ROW trigger (cascading row triggers are not yet supported)"
                )));
            }
        }
        if !row_triggers.is_empty() {
            if let Statement::Insert(insert) = statement
                && insert.on_conflict.is_some()
            {
                return Err(not_implemented(
                    "ON CONFLICT is not yet supported on tables with FOR EACH ROW triggers",
                ));
            }
            if !returning.is_empty() {
                return Err(not_implemented(
                    "RETURNING is not yet supported on tables with FOR EACH ROW triggers",
                ));
            }
        }
        if let Statement::Insert(insert) = statement
            && insert
                .on_conflict
                .as_ref()
                .is_some_and(|c| c.action != duckrs_parser::ast::OnConflictAction::Nothing)
            && triggers
                .iter()
                .any(|t| t.timing == TriggerTiming::After && !t.referencing_new.is_empty())
        {
            return Err(not_implemented(
                "ON CONFLICT DO UPDATE is not yet supported with REFERENCING NEW TABLE AS triggers",
            ));
        }
        // one statement, so all of it or nothing
        let implicit = self.transaction.is_none();
        if implicit {
            self.run_transaction_statement(TransactionType::Begin, TransactionModifier::Default)?;
        }
        self.trigger_tables.push(key);
        let result = self.fire(statement, &target, &triggers, &returning);
        self.trigger_tables.pop();
        if implicit {
            let end = if result.is_ok() {
                TransactionType::Commit
            } else {
                TransactionType::Rollback
            };
            if self.transaction.is_some() {
                self.run_transaction_statement(end, TransactionModifier::Default)?;
            }
        }
        result.map(Some)
    }

    fn fire(
        &mut self,
        statement: &Statement,
        target: &TriggerTarget,
        triggers: &[Arc<TriggerCatalogEntry>],
        returning: &[Expr],
    ) -> Result<MaterializedQueryResult> {
        let statement_triggers = |timing| {
            triggers
                .iter()
                .filter(move |t| t.timing == timing && t.for_each == TriggerForEach::Statement)
        };
        for trigger in statement_triggers(TriggerTiming::Before) {
            self.run_body(trigger, None)?;
        }
        let after: Vec<_> = triggers.iter().filter(|t| t.timing == TriggerTiming::After).collect();
        let row_rows = after.iter().any(|t| t.for_each == TriggerForEach::Row);
        let wants_new = row_rows || after.iter().any(|t| !t.referencing_new.is_empty());
        let wants_old = row_rows || after.iter().any(|t| !t.referencing_old.is_empty());
        let (capture_new, capture_old) = match target.event {
            TriggerEvent::Insert => (wants_new || !returning.is_empty(), false),
            TriggerEvent::Delete => (false, wants_old || !returning.is_empty()),
            TriggerEvent::Update => (wants_new || !returning.is_empty(), wants_old),
        };
        // the rows an UPDATE changes, before it does
        let old_update = if capture_old && target.event == TriggerEvent::Update {
            Some(self.update_pre_image(statement, target)?)
        } else {
            None
        };
        let captures = capture_new || (capture_old && target.event == TriggerEvent::Delete);
        // RETURNING as the statement has it, then the stored columns for the triggers (the generated
        // ones are computed from them below)
        let stored = target.table.columns.len();
        let mut list = returning.to_vec();
        if captures {
            list.extend(
                target
                    .table
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| Expr::column(vec![c.name.clone()]).with_alias(format!("__duckdb_trigger_c{i}"))),
            );
        }
        let base = match statement.clone() {
            Statement::Insert(mut insert) => {
                insert.returning = list;
                Statement::Insert(insert)
            }
            Statement::Update(mut update) => {
                update.returning = list;
                Statement::Update(update)
            }
            Statement::Delete(mut delete) => {
                delete.returning = list;
                Statement::Delete(delete)
            }
            other => other,
        };
        if captures && matches!(base, Statement::Copy(_)) {
            // a COPY appends its rows, after those there are
            let before = self.count_rows(target)?;
            let result = self.run_statement(&base)?;
            let rows = self.rows_after(target, before)?;
            return self.after_base(
                statement,
                target,
                triggers,
                returning,
                result,
                Some(rows),
                old_update,
                true,
            );
        }
        let result = self.run_statement(&base)?;
        let (result, rows) = if captures {
            let total = result.names.len();
            let user = total - stored;
            let names: Vec<String> = target.table.columns.iter().map(|c| c.name.clone()).collect();
            let captured = project(&result, user..total, names)?;
            let user_result = project(&result, 0..user, result.names[..user].to_vec())?;
            (user_result, Some(self.logical_rows(&target.table, captured)?))
        } else {
            (result, None)
        };
        self.after_base(
            statement, target, triggers, returning, result, rows, old_update, captures,
        )
    }

    /// The AFTER triggers, once the statement ran, and what the statement returns.
    #[allow(clippy::too_many_arguments)]
    fn after_base(
        &mut self,
        statement: &Statement,
        target: &TriggerTarget,
        triggers: &[Arc<TriggerCatalogEntry>],
        returning: &[Expr],
        result: MaterializedQueryResult,
        rows: Option<MaterializedQueryResult>,
        old_update: Option<MaterializedQueryResult>,
        captures: bool,
    ) -> Result<MaterializedQueryResult> {
        let statement_triggers = |timing| {
            triggers
                .iter()
                .filter(move |t| t.timing == timing && t.for_each == TriggerForEach::Statement)
        };
        let copies = matches!(statement, Statement::Copy(_));
        let (new_rows, old_rows) = match target.event {
            TriggerEvent::Insert | TriggerEvent::Update => (rows, old_update),
            TriggerEvent::Delete => (None, rows),
        };
        for trigger in statement_triggers(TriggerTiming::After) {
            let mut scope = TriggerScope::default();
            for (alias, rows) in [
                (&trigger.referencing_new, &new_rows),
                (&trigger.referencing_old, &old_rows),
            ] {
                if alias.is_empty() {
                    continue;
                }
                let rows = rows.as_ref().expect("captured for the trigger");
                scope.tables.push(TransitionTable {
                    name: alias.clone(),
                    names: rows.names.clone(),
                    types: rows.types.clone(),
                    rows: Arc::new(parking_lot::Mutex::new(rows.collection.clone())),
                });
            }
            self.run_body(trigger, Some(scope))?;
        }
        for trigger in triggers
            .iter()
            .filter(|t| t.timing == TriggerTiming::After && t.for_each == TriggerForEach::Row)
        {
            let (rows, name) = match target.event {
                TriggerEvent::Delete => (old_rows.as_ref(), "old"),
                _ => (new_rows.as_ref(), "new"),
            };
            let Some(rows) = rows else { continue };
            for chunk in rows.collection.chunks() {
                for row in 0..chunk.len() {
                    let values = (0..chunk.column_count()).map(|c| chunk.get_value(c, row)).collect();
                    let scope = TriggerScope {
                        tables: Vec::new(),
                        row: Some(RowScope {
                            name: name.to_string(),
                            names: rows.names.clone(),
                            types: rows.types.clone(),
                            values: Some(values),
                            used: Default::default(),
                        }),
                    };
                    self.run_body(trigger, Some(scope))?;
                }
            }
        }
        if !returning.is_empty() || copies {
            return Ok(result);
        }
        if captures {
            let count = rows_of(&new_rows, &old_rows) as i64;
            let types = vec![LogicalType::BIGINT];
            let mut collection = ColumnDataCollection::new(types.clone());
            let mut chunk = duckrs_common::vector::DataChunk::new(&types);
            chunk.append_row(&[Value::bigint(count)])?;
            collection.append(&chunk);
            return Ok(MaterializedQueryResult {
                names: vec!["Count".into()],
                types,
                collection,
            });
        }
        Ok(result)
    }

    fn count_rows(&mut self, target: &TriggerTarget) -> Result<u64> {
        let sql = format!(
            "SELECT count(*) FROM {}.{}",
            sql_identifier(&target.schema.name),
            sql_identifier(&target.table.name)
        );
        let statement = duckrs_parser::parse_ast_with(&sql, &self.parser_options())?.remove(0);
        let result = self.run_statement(&statement)?;
        let chunk = result.collection.chunks().next().expect("a count");
        Ok(chunk.get_value(0, 0).as_i128().unwrap_or(0) as u64)
    }

    /// The stored columns of the rows of the table after the first `skip`, with its generated ones.
    fn rows_after(&mut self, target: &TriggerTarget, skip: u64) -> Result<MaterializedQueryResult> {
        let columns: Vec<String> = target.table.columns.iter().map(|c| sql_identifier(&c.name)).collect();
        let sql = format!(
            "SELECT {} FROM {}.{} OFFSET {skip}",
            columns.join(", "),
            sql_identifier(&target.schema.name),
            sql_identifier(&target.table.name)
        );
        let statement = duckrs_parser::parse_ast_with(&sql, &self.parser_options())?.remove(0);
        let stored = self.run_statement(&statement)?;
        self.logical_rows(&target.table, stored)
    }

    /// The rows an UPDATE changes, as they are before it (`OldCaptureColumnNames`): each once, however
    /// many rows of its FROM clause match it.
    fn update_pre_image(&mut self, statement: &Statement, target: &TriggerTarget) -> Result<MaterializedQueryResult> {
        let Statement::Update(update) = statement else {
            unreachable!("only for an UPDATE");
        };
        let alias = &target.alias;
        let rowid = Expr::column(vec![alias.clone(), "rowid".into()]);
        let mut inner = duckrs_parser::ast::SelectNode {
            select_list: vec![rowid.clone()],
            from_table: match &update.from_table {
                Some(from) => TableRef::from(TableRefKind::Join(duckrs_parser::ast::JoinRef {
                    left: Box::new(update.table.clone()),
                    right: Box::new(from.clone()),
                    condition: None,
                    using_columns: Vec::new(),
                    join_type: duckrs_parser::ast::JoinType::Inner,
                    ref_type: duckrs_parser::ast::JoinRefType::Cross,
                    is_implicit: true,
                    nearest_count: 0,
                    nearest_descending: false,
                    nearest_approx: false,
                    ranking_expression: None,
                })),
                None => update.table.clone(),
            },
            ..Default::default()
        };
        inner.where_clause = update.set_info.condition.clone();
        let inner = duckrs_parser::ast::SelectStatement { node: inner.into() };
        let outer = duckrs_parser::ast::SelectNode {
            select_list: target
                .table
                .columns
                .iter()
                .map(|c| Expr::column(vec![alias.clone(), c.name.clone()]))
                .collect(),
            from_table: update.table.clone(),
            where_clause: Some(Box::new(
                ExprKind::Subquery {
                    subquery: Box::new(inner),
                    subquery_type: duckrs_parser::ast::SubqueryType::Any,
                    child: Some(Box::new(rowid)),
                    comparison: duckrs_parser::ast::ComparisonOp::Equal,
                }
                .into(),
            )),
            ..Default::default()
        };
        let mut node: QueryNode = outer.into();
        node.cte_map = update.cte_map.clone();
        let select = Statement::Select(duckrs_parser::ast::SelectStatement { node });
        let stored = self.run_statement(&select)?;
        self.logical_rows(&target.table, stored)
    }

    /// The rows of the stored columns of `table`, with its generated columns computed from them,
    /// in the order of its columns (`MakeUpdateOldAliasCTE`).
    fn logical_rows(
        &mut self,
        table: &TableCatalogEntry,
        stored: MaterializedQueryResult,
    ) -> Result<MaterializedQueryResult> {
        if table.generated.is_empty() {
            return Ok(stored);
        }
        let name = "__duckdb_trigger_rows";
        let scope = TriggerScope {
            tables: vec![TransitionTable {
                name: name.into(),
                names: stored.names.clone(),
                types: stored.types.clone(),
                rows: Arc::new(parking_lot::Mutex::new(stored.collection)),
            }],
            row: None,
        };
        let select_list = table
            .logical_columns()
            .iter()
            .map(|column| match column.generated {
                Some(expr) => {
                    let ty = duckrs_parser::ast::CastType::Resolved(column.ty.clone());
                    Expr::cast(inline_generated(expr, table, 0), ty).with_alias(column.name.to_string())
                }
                None => Expr::column(vec![column.name.to_string()]),
            })
            .collect();
        let select = duckrs_parser::ast::SelectNode {
            select_list,
            from_table: TableRef::from(TableRefKind::BaseTable {
                name: duckrs_parser::ast::QualifiedName::new(name),
                at_clause: None,
            }),
            ..Default::default()
        };
        let select = Statement::Select(duckrs_parser::ast::SelectStatement { node: select.into() });
        trigger_scope::with_scope(Some(Arc::new(scope)), || self.run_statement(&select))
    }

    /// Runs the body of `trigger` with what it sees.
    fn run_body(&mut self, trigger: &TriggerCatalogEntry, scope: Option<TriggerScope>) -> Result<()> {
        let statement = body_statement(&trigger.action)?;
        let scope = Arc::new(scope.unwrap_or_default());
        trigger_scope::with_scope(Some(scope), || self.run_statement(&statement)).map(drop)
    }

    /// The database a trigger's table is named in, and the rest of the qualification.
    fn trigger_database(&self, qualification: &[String]) -> (Option<Arc<DatabaseInstance>>, Vec<String>) {
        match qualification {
            [database, rest @ ..] if !rest.is_empty() => match self.find_database(database) {
                Some(db) => (Some(db), rest.to_vec()),
                None => (None, qualification.to_vec()),
            },
            [one] if self.catalog().get_schema(one).is_none() => match self.find_database(one) {
                Some(db) => (Some(db), Vec::new()),
                None => (None, qualification.to_vec()),
            },
            _ => (None, qualification.to_vec()),
        }
    }

    /// Runs `body` in the database of the trigger's table, with the table's name in it.
    fn in_trigger_database<T>(
        &mut self,
        table: &TableRef,
        body: impl FnOnce(&mut Self, &duckrs_parser::ast::QualifiedName) -> Result<T>,
    ) -> Result<T> {
        let TableRefKind::BaseTable { name, .. } = &table.kind else {
            return Err(binder_error(
                "CREATE TRIGGER requires a base table, not a view or subquery",
            ));
        };
        let (database, rest) = self.trigger_database(&name.qualification);
        let local = duckrs_parser::ast::QualifiedName {
            qualification: rest,
            name: name.name.clone(),
        };
        match database {
            Some(db) if !Arc::ptr_eq(&db, &self.db) => {
                let previous = std::mem::replace(&mut self.db, db);
                let result = body(self, &local);
                self.db = previous;
                result
            }
            _ => body(self, &local),
        }
    }

    /// Binds `statement` as it would run, in the database it names.
    fn bind_body(&mut self, statement: &Statement) -> Result<()> {
        if let Some((database, routed)) = self.route_to_database(statement)? {
            let previous = std::mem::replace(&mut self.db, database);
            let result = self.bind_body(&routed);
            self.db = previous;
            return result;
        }
        let catalog = self.catalog();
        let mut binder = Binder::with_options(&catalog, self.bind_options());
        binder.session = Some(self.session_values());
        binder.bind(statement).map(drop)
    }

    /// `CREATE TRIGGER` (`Binder::BindCreateTriggerInfo` + `DuckSchemaEntry::CreateEntry`).
    pub(super) fn create_trigger(&mut self, info: &CreateInfo) -> Result<()> {
        let CreateKind::Trigger { table, .. } = &info.kind else {
            unreachable!("checked by the caller");
        };
        let written = match &table.kind {
            TableRefKind::BaseTable { name, .. } => {
                let mut parts = name.qualification.clone();
                parts.push(name.name.clone());
                parts
            }
            _ => Vec::new(),
        };
        let table = table.clone();
        self.in_trigger_database(&table, |this, local| this.create_trigger_in(info, local, written))
    }

    fn create_trigger_in(
        &mut self,
        info: &CreateInfo,
        table_name: &duckrs_parser::ast::QualifiedName,
        written: Vec<String>,
    ) -> Result<()> {
        let CreateKind::Trigger {
            timing,
            event,
            columns,
            table,
            referencing_new,
            referencing_old,
            for_each,
            action,
        } = &info.kind
        else {
            unreachable!("checked by the caller");
        };
        let _ = table;
        if info.temporary {
            // a trigger is temporary iff its base table is
            return Err(binder_error("Temporary triggers are not supported"));
        }
        let catalog = self.catalog();
        // `Catalog::GetEntry`, for its errors
        catalog.get_entry(
            table_name.qualification.last().map(String::as_str),
            None,
            &table_name.name,
        )?;
        let schema = table_schema(&catalog, &table_name.qualification, &table_name.name)
            .ok_or_else(|| crate::catalog::missing_entry("Table", &table_name.name))?;
        let base = match schema.get_entry(&table_name.name) {
            Some(TableOrView::Table(table)) => table,
            Some(TableOrView::View(_)) => {
                return Err(binder_error(
                    "CREATE TRIGGER requires a base table, not a view or subquery",
                ));
            }
            None => return Err(crate::catalog::missing_entry("Table", &table_name.name)),
        };
        let timing = TriggerTiming::from_name(timing);
        let event = TriggerEvent::from_name(event);
        let for_each = if *for_each == "ROW" {
            TriggerForEach::Row
        } else {
            TriggerForEach::Statement
        };
        let temporary = schema.name.eq_ignore_ascii_case(TEMP_SCHEMA);
        // `CREATE TRIGGER is only supported for storage versions v2.0.0 and higher`
        if !temporary
            && let Some(version) = catalog.storage_version()
            && version < duckrs_common::serializer::StorageVersion::V2_0_0
        {
            return Err(binder_error(
                "CREATE TRIGGER is only supported for storage versions v2.0.0 and higher.\nUse an in-memory \
                 database, ATTACH with (STORAGE_VERSION v2.0.0)",
            ));
        }
        if timing == TriggerTiming::InsteadOf {
            return Err(not_implemented("INSTEAD OF triggers are not yet supported"));
        }
        if event == TriggerEvent::Update {
            for column in columns {
                if base.column_index(column).is_none()
                    && !base.generated.iter().any(|g| g.name.eq_ignore_ascii_case(column))
                {
                    return Err(binder_error(format!(
                        "Column {} does not exist in table {}",
                        sql_quoted_identifier(column),
                        sql_quoted_identifier(&base.name)
                    )));
                }
            }
        }
        let referencing = !referencing_new.is_empty() || !referencing_old.is_empty();
        if for_each == TriggerForEach::Row && referencing {
            return Err(binder_error("REFERENCING is not valid for FOR EACH ROW triggers"));
        }
        if for_each == TriggerForEach::Row && timing != TriggerTiming::After {
            return Err(not_implemented("BEFORE FOR EACH ROW triggers are not yet supported"));
        }
        if for_each == TriggerForEach::Row && event == TriggerEvent::Update {
            return Err(not_implemented("UPDATE FOR EACH ROW triggers are not yet supported"));
        }
        if referencing && timing != TriggerTiming::After {
            return Err(binder_error(
                "Transition tables can only be specified for AFTER triggers",
            ));
        }
        if referencing && !columns.is_empty() {
            return Err(binder_error("UPDATE OF is not valid with transition tables"));
        }
        if !referencing_old.is_empty() && event == TriggerEvent::Insert {
            return Err(binder_error(
                "REFERENCING OLD TABLE AS is not valid for AFTER INSERT triggers",
            ));
        }
        if !referencing_new.is_empty() && event == TriggerEvent::Delete {
            return Err(binder_error(
                "REFERENCING NEW TABLE AS is not valid for AFTER DELETE triggers",
            ));
        }
        let replace = info.on_conflict == duckrs_parser::ast::OnCreateConflict::Replace;
        let mixing = schema.triggers_on(&base.name).iter().any(|t| {
            t.event == event && t.for_each != for_each && !(replace && t.name.eq_ignore_ascii_case(&info.name.name))
        });
        if mixing {
            return Err(not_implemented(
                "Mixing FOR EACH STATEMENT and FOR EACH ROW triggers on the same table is not yet supported",
            ));
        }
        let trigger_name = sql_quoted_identifier(&info.name.name);
        let table_quoted = sql_quoted_identifier(&base.name);
        // a body that leads back to the table is a cycle
        if let Some((qualification, name, events)) = body_target(action) {
            let mut visited = Vec::new();
            if self.reaches_table(&schema, &qualification, &name, &events, &base, &mut visited) {
                return Err(not_implemented(format!(
                    "Recursive trigger chains are not yet supported (trigger cycle detected through trigger \
                     {trigger_name} on table {table_quoted})"
                )));
            }
        }
        // the body binds as it will run, against transition tables of the table's columns
        let mut scope = TriggerScope::default();
        let names: Vec<String> = base.logical_columns().iter().map(|c| c.name.to_string()).collect();
        let types: Vec<LogicalType> = base.logical_columns().iter().map(|c| c.ty.clone()).collect();
        for alias in [referencing_new, referencing_old] {
            if !alias.is_empty() {
                scope.tables.push(TransitionTable {
                    name: alias.clone(),
                    names: names.clone(),
                    types: types.clone(),
                    rows: Arc::new(parking_lot::Mutex::new(ColumnDataCollection::new(types.clone()))),
                });
            }
        }
        if for_each == TriggerForEach::Row {
            if !base.generated.is_empty() {
                return Err(not_implemented(
                    "FOR EACH ROW triggers on tables with generated columns are not yet supported",
                ));
            }
            if matches!(action.kind, QueryNodeKind::Update(_)) {
                return Err(not_implemented(
                    "UPDATE trigger bodies in FOR EACH ROW triggers are not yet supported",
                ));
            }
            scope.row = Some(RowScope {
                name: if event == TriggerEvent::Delete { "old" } else { "new" }.into(),
                names: names.clone(),
                types: types.clone(),
                values: None,
                used: Default::default(),
            });
        }
        let mut statement = body_statement(action)?;
        let scope = Arc::new(scope);
        trigger_scope::with_scope(Some(scope.clone()), || self.bind_body(&statement))?;
        if let Some(row) = &scope.row {
            if !row.used.load(AtomicOrdering2::Relaxed) {
                return Err(binder_error(format!(
                    "FOR EACH ROW trigger {trigger_name} on table {table_quoted} must reference at least one NEW or \
                     OLD column in the trigger body (use FOR EACH STATEMENT if row data is not needed)"
                )));
            }
            if let Some((qualification, name, _)) = body_target(action)
                && let Some((target_schema, target)) = self.resolve_table(&schema, &qualification, &name)
                && target_schema
                    .triggers_on(&target.name)
                    .iter()
                    .any(|t| t.for_each == TriggerForEach::Row)
            {
                return Err(not_implemented(format!(
                    "FOR EACH ROW trigger {trigger_name} on table {table_quoted} writes to a table that has its own \
                     FOR EACH ROW trigger (cascading row triggers are not yet supported)"
                )));
            }
        }
        // what the body depends on: the tables and views it names, other than its transition tables
        let transition: Vec<&String> = [referencing_new, referencing_old]
            .into_iter()
            .filter(|a| !a.is_empty())
            .collect();
        let mut dependencies = Vec::new();
        let cte_names: Vec<String> = action.cte_map.0.iter().map(|(n, _)| n.to_lowercase()).collect();
        for name in referenced_names(&mut statement) {
            if name.qualification.is_empty()
                && (transition.iter().any(|t| t.eq_ignore_ascii_case(&name.name))
                    || cte_names.contains(&name.name.to_lowercase()))
            {
                continue;
            }
            // the temporary schema is a catalog of its own
            let temp_reference = name
                .qualification
                .first()
                .is_some_and(|q| q.eq_ignore_ascii_case(TEMP_SCHEMA));
            if temp_reference && !temporary {
                return Err(binder_error(format!(
                    "Trigger {trigger_name} cannot reference {} from a different catalog (temp)",
                    sql_quoted_identifier(&name.name)
                )));
            }
            if !temp_reference
                && (name.qualification.len() >= 2
                    || (name.qualification.len() == 1 && catalog.get_schema(&name.qualification[0]).is_none()))
            {
                let database = &name.qualification[0];
                if !database.eq_ignore_ascii_case(&self.db.name) && !database.eq_ignore_ascii_case(DEFAULT_CATALOG) {
                    return Err(binder_error(format!(
                        "Trigger {trigger_name} cannot reference {} from a different catalog ({database})",
                        sql_quoted_identifier(&name.name)
                    )));
                }
            }
            if !dependencies.iter().any(|d: &String| d.eq_ignore_ascii_case(&name.name)) {
                dependencies.push(name.name.clone());
            }
        }
        let trigger = TriggerCatalogEntry {
            name: info.name.name.clone(),
            table: base.name.clone(),
            table_written: written,
            timing,
            event,
            columns: columns.clone(),
            for_each,
            referencing_new: referencing_new.clone(),
            referencing_old: referencing_old.clone(),
            action: (**action).clone(),
            temporary,
            dependencies,
            comment: None,
        };
        schema
            .create_trigger(trigger, on_create_conflict(info.on_conflict))
            .map(drop)
    }

    /// The table `name` is, from a body in `schema`.
    fn resolve_table(
        &self,
        schema: &Arc<crate::catalog::SchemaCatalogEntry>,
        qualification: &[String],
        name: &str,
    ) -> Option<(Arc<crate::catalog::SchemaCatalogEntry>, Arc<TableCatalogEntry>)> {
        let catalog = self.catalog();
        let schema = match qualification.last() {
            None if schema.get_entry(name).is_some() => schema.clone(),
            _ => table_schema(&catalog, qualification, name)?,
        };
        match schema.get_entry(name)? {
            TableOrView::Table(table) => Some((schema, table)),
            TableOrView::View(_) => None,
        }
    }

    /// Whether writing `name` (by `events`) runs triggers that write `base` in the end
    /// (`trigger_creation_table`).
    fn reaches_table(
        &self,
        schema: &Arc<crate::catalog::SchemaCatalogEntry>,
        qualification: &[String],
        name: &str,
        events: &[TriggerEvent],
        base: &Arc<TableCatalogEntry>,
        visited: &mut Vec<String>,
    ) -> bool {
        let Some((target_schema, target)) = self.resolve_table(schema, qualification, name) else {
            return false;
        };
        if Arc::ptr_eq(&target, base) || (target.name.eq_ignore_ascii_case(&base.name) && target.schema == base.schema)
        {
            return true;
        }
        let key = format!("{}.{}", target_schema.key(), target.name.to_lowercase());
        if visited.contains(&key) {
            return false;
        }
        visited.push(key);
        for trigger in target_schema.triggers_on(&target.name) {
            if !events.contains(&trigger.event) {
                continue;
            }
            if let Some((q, n, e)) = body_target(&trigger.action)
                && self.reaches_table(&target_schema, &q, &n, &e, base, visited)
            {
                return true;
            }
        }
        false
    }

    /// `DependencyManager::CollectDependents` for triggers: the bodies that read or write `name` keep
    /// it from being dropped, unless CASCADE drops them too.
    pub(super) fn drop_trigger_dependents(
        &mut self,
        catalog: &crate::catalog::Catalog,
        name: &str,
        kind: &str,
        cascade: bool,
    ) -> Result<()> {
        let mut dependents = Vec::new();
        for schema in catalog.schemas() {
            for trigger in schema.trigger_entries() {
                if trigger.depends_on(name) {
                    dependents.push((schema.clone(), trigger));
                }
            }
        }
        if dependents.is_empty() {
            return Ok(());
        }
        if cascade {
            for (schema, trigger) in dependents {
                schema.drop_trigger(&trigger.table, &trigger.name);
            }
            return Ok(());
        }
        let mut message = format!("Cannot drop entry \"{name}\" because there are entries that depend on it.\n");
        for (_, trigger) in &dependents {
            message += &format!(
                "trigger \"{}\" on table \"{}\" depends on {} \"{name}\".\n",
                trigger.name,
                trigger.table,
                kind.to_lowercase()
            );
        }
        message += "Use DROP...CASCADE to drop all dependents.";
        Err(Error::new(ErrorKind::Dependency, message))
    }

    /// `DependencyManager::AlterObject` for triggers: a table a trigger body reads can't be altered
    /// but by adding a column, setting a default or a comment; the table of a trigger can be altered
    /// unless its body reads it, but not renamed, as the trigger knows it by its name.
    pub(super) fn check_trigger_alter(&self, info: &AlterInfo) -> Result<()> {
        let harmless = matches!(
            info.kind,
            AlterKind::AddColumn { .. }
                | AlterKind::SetDefault { .. }
                | AlterKind::SetComment { .. }
                | AlterKind::SetColumnComment { .. }
        );
        if harmless || matches!(info.kind, AlterKind::RenameView(_)) {
            return Ok(());
        }
        let catalog = self.catalog();
        let Some(schema) = table_schema(&catalog, &info.name.qualification, &info.name.name) else {
            return Ok(());
        };
        let name = &info.name.name;
        let renames = matches!(info.kind, AlterKind::RenameTable(_));
        let blocked = catalog.schemas().iter().any(|s| {
            s.trigger_entries().iter().any(|trigger| {
                trigger.depends_on(name)
                    || (Arc::ptr_eq(s, &schema)
                        && trigger.table.eq_ignore_ascii_case(name)
                        && (trigger.reads_own_table() || renames))
            })
        });
        if blocked {
            return Err(Error::new(
                ErrorKind::Dependency,
                format!(
                    "Cannot alter entry {} because there are entries that depend on it.",
                    sql_quoted_identifier(name)
                ),
            ));
        }
        Ok(())
    }

    /// `TriggerCatalogEntry::AlterEntry`: UPDATE OF follows the rename of a column.
    pub(super) fn alter_triggers(&self, info: &AlterInfo) {
        let AlterKind::RenameColumn { path, new_name } = &info.kind else {
            return;
        };
        let [old_name] = path.as_slice() else {
            return;
        };
        let catalog = self.catalog();
        let Some(schema) = table_schema(&catalog, &info.name.qualification, &info.name.name) else {
            return;
        };
        for trigger in schema.triggers_on(&info.name.name) {
            if !trigger.columns.iter().any(|c| c.eq_ignore_ascii_case(old_name)) {
                continue;
            }
            let mut changed = (*trigger).clone();
            for column in &mut changed.columns {
                if column.eq_ignore_ascii_case(old_name) {
                    column.clone_from(new_name);
                }
            }
            schema.replace_trigger(&trigger.table, changed);
        }
    }

    /// `DROP TRIGGER name ON table`.
    pub(super) fn drop_trigger(&mut self, info: &DropInfo) -> Result<()> {
        let Some(table) = info.trigger_table.clone() else {
            return Err(binder_error("DROP TRIGGER requires a table"));
        };
        self.in_trigger_database(&table, |this, local| this.drop_trigger_in(info, local))
    }

    fn drop_trigger_in(&mut self, info: &DropInfo, table_name: &duckrs_parser::ast::QualifiedName) -> Result<()> {
        let catalog = self.catalog();
        catalog.get_entry(
            table_name.qualification.last().map(String::as_str),
            None,
            &table_name.name,
        )?;
        let schema = table_schema(&catalog, &table_name.qualification, &table_name.name)
            .ok_or_else(|| crate::catalog::missing_entry("Table", &table_name.name))?;
        let Some(TableOrView::Table(base)) = schema.get_entry(&table_name.name) else {
            return Err(crate::catalog::missing_entry("Table", &table_name.name));
        };
        let exists = schema.get_trigger(&base.name, &info.name.name).is_some();
        if !exists {
            if info.if_exists {
                return Ok(());
            }
            return Err(Error::new(
                ErrorKind::Catalog,
                format!(
                    "Trigger with name \"{}\" does not exist on table \"{}\"",
                    info.name.name, base.name
                ),
            ));
        }
        schema.drop_trigger(&base.name, &info.name.name);
        Ok(())
    }
}
