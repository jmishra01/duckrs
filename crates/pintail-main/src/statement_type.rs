// Ported from duckdb/src/include/duckdb/common/enums/statement_type.hpp @ rust-port-baseline

//! The type of a statement (`StatementType`) and what its result holds (`StatementReturnType`), as the C API
//! reports them for a result or a prepared statement.

use pintail_parser::ast::Statement;

pub use pintail_core::planner::binder::StatementReturnType;

/// `StatementType`: the kind of a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementType {
    Invalid,
    Select,
    Insert,
    Update,
    Create,
    Delete,
    Prepare,
    Execute,
    Alter,
    Transaction,
    Copy,
    Analyze,
    VariableSet,
    CreateFunc,
    Explain,
    Drop,
    Export,
    Pragma,
    Vacuum,
    Call,
    Set,
    Load,
    Relation,
    Extension,
    LogicalPlan,
    Attach,
    Detach,
    Multi,
    CopyDatabase,
    UpdateExtensions,
    MergeInto,
}

/// `StatementProperties::statement_type` and `return_type` of a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatementProperties {
    pub statement_type: StatementType,
    pub return_type: StatementReturnType,
}

fn changed_rows_unless_returning(returning: &[pintail_parser::ast::Expr]) -> StatementReturnType {
    if returning.is_empty() {
        StatementReturnType::ChangedRows
    } else {
        StatementReturnType::QueryResult
    }
}

/// The type and the return type the binder gives `statement`. `prepared` finds the statement an EXECUTE runs.
pub(crate) fn statement_properties(
    statement: &Statement,
    prepared: &dyn Fn(&str) -> Option<StatementProperties>,
) -> StatementProperties {
    use StatementReturnType::{ChangedRows, Nothing, QueryResult};
    use StatementType as T;
    let (statement_type, return_type) = match statement {
        Statement::Select(_) => (T::Select, QueryResult),
        Statement::Insert(insert) => (T::Insert, changed_rows_unless_returning(&insert.returning)),
        Statement::Update(update) => (T::Update, changed_rows_unless_returning(&update.returning)),
        Statement::Delete(delete) => (T::Delete, changed_rows_unless_returning(&delete.returning)),
        Statement::Merge(merge) => (T::MergeInto, changed_rows_unless_returning(&merge.returning)),
        Statement::CreateTable(info) if info.query.is_some() => (T::Create, ChangedRows),
        Statement::CreateTable(_) | Statement::Create(_) => (T::Create, Nothing),
        // `DEALLOCATE` is a DROP of a prepared statement
        Statement::Drop(_) | Statement::Deallocate(_) => (T::Drop, Nothing),
        Statement::Transaction { .. } => (T::Transaction, Nothing),
        Statement::Call(_) => (T::Call, QueryResult),
        Statement::SetVariable { .. } | Statement::ResetVariable { .. } => (T::Set, Nothing),
        Statement::Pragma(_) => (T::Pragma, QueryResult),
        Statement::Explain { .. } => (T::Explain, QueryResult),
        Statement::Prepare { .. } => (T::Prepare, Nothing),
        Statement::Execute { name, .. } => match prepared(name) {
            Some(properties) => return properties,
            None => (T::Execute, QueryResult),
        },
        Statement::Detach { .. } => (T::Detach, Nothing),
        Statement::Alter(_) => (T::Alter, Nothing),
        Statement::Copy(_) => (T::Copy, ChangedRows),
        Statement::CopyDatabase { .. } => (T::CopyDatabase, Nothing),
        Statement::Export { .. } => (T::Export, Nothing),
        Statement::Attach(_) => (T::Attach, Nothing),
        Statement::Vacuum { analyze: true, .. } => (T::Analyze, Nothing),
        Statement::Vacuum { .. } => (T::Vacuum, Nothing),
        Statement::Load(_) => (T::Load, Nothing),
        Statement::ExternalResource(_) | Statement::Connect(_) | Statement::Disconnect => (T::Extension, Nothing),
        Statement::UpdateExtensions(_) => (T::UpdateExtensions, QueryResult),
        Statement::Multi(statements) => match statements.last() {
            Some(last) => return statement_properties(last, prepared),
            None => (T::Multi, Nothing),
        },
    };
    StatementProperties {
        statement_type,
        return_type,
    }
}
