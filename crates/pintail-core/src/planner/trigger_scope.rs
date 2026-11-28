// Ported from duckdb/src/planner/binder/query_node/bind_trigger_expansion.cpp @ rust-port-baseline
//
//! What the body of a trigger sees while it runs: the transition tables of REFERENCING NEW TABLE
//! and OLD TABLE (which C++ makes CTEs of the body), and for FOR EACH ROW the row as `new` or `old`
//! (`SetupRowScope`). The statements of a body bind while it is set.

use pintail_common::column_data::ColumnDataCollection;
use pintail_common::types::LogicalType;
use pintail_common::value::Value;
use parking_lot::Mutex;
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A transition table: the rows a statement inserted, deleted or changed.
#[derive(Debug, Clone)]
pub struct TransitionTable {
    pub name: String,
    pub names: Vec<String>,
    pub types: Vec<LogicalType>,
    pub rows: Arc<Mutex<ColumnDataCollection>>,
}

/// The row of a FOR EACH ROW trigger, as `name.column`.
#[derive(Debug, Default)]
pub struct RowScope {
    pub name: String,
    pub names: Vec<String>,
    pub types: Vec<LogicalType>,
    /// the values, or `None` while a CREATE TRIGGER checks the body
    pub values: Option<Vec<Value>>,
    /// whether the body read a column of the row
    pub used: AtomicBool,
}

#[derive(Debug, Default)]
pub struct TriggerScope {
    pub tables: Vec<TransitionTable>,
    pub row: Option<RowScope>,
}

thread_local! {
    static SCOPE: RefCell<Option<Arc<TriggerScope>>> = const { RefCell::new(None) };
}

/// Runs `body` with `scope` as what the statements it binds see.
pub fn with_scope<T>(scope: Option<Arc<TriggerScope>>, body: impl FnOnce() -> T) -> T {
    let previous = SCOPE.with(|s| std::mem::replace(&mut *s.borrow_mut(), scope));
    let result = body();
    SCOPE.with(|s| *s.borrow_mut() = previous);
    result
}

pub fn current() -> Option<Arc<TriggerScope>> {
    SCOPE.with(|s| s.borrow().clone())
}

/// `new.column` or `old.column` (or the bare column) of the row of a FOR EACH ROW trigger: its
/// value, or a NULL of its type while the body is only checked.
pub fn row_value(names: &[String]) -> Option<Value> {
    let scope = current()?;
    let row = scope.row.as_ref()?;
    let column = match names {
        [scope_name, column] if scope_name.eq_ignore_ascii_case(&row.name) => column,
        [column] => column,
        _ => return None,
    };
    let index = row.names.iter().position(|n| n.eq_ignore_ascii_case(column))?;
    row.used.store(true, Ordering::Relaxed);
    Some(match &row.values {
        Some(values) => values[index].clone(),
        None => Value::null(row.types[index].clone()),
    })
}
