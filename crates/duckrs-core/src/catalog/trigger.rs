// Ported from duckdb/src/catalog/catalog_entry/trigger_catalog_entry.cpp and
// duckdb/src/include/duckdb/common/enums/trigger_type.hpp @ rust-port-baseline
//
//! `TriggerCatalogEntry`: a trigger, kept in the schema of its table.

use duckrs_common::types::sql_identifier;
use duckrs_parser::ast::QueryNode;

/// `TriggerTiming`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerTiming {
    Before = 0,
    After = 1,
    InsteadOf = 2,
}

impl TriggerTiming {
    pub fn name(self) -> &'static str {
        match self {
            Self::Before => "BEFORE",
            Self::After => "AFTER",
            Self::InsteadOf => "INSTEAD_OF",
        }
    }

    pub fn from_name(name: &str) -> Self {
        match name {
            "BEFORE" => Self::Before,
            "AFTER" => Self::After,
            _ => Self::InsteadOf,
        }
    }
}

/// `TriggerEventType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerEvent {
    Insert = 0,
    Delete = 1,
    Update = 2,
}

impl TriggerEvent {
    pub fn name(self) -> &'static str {
        match self {
            Self::Insert => "INSERT",
            Self::Delete => "DELETE",
            Self::Update => "UPDATE",
        }
    }

    /// `EnumUtil::ToString(TriggerEventType)`.
    pub fn enum_name(self) -> &'static str {
        match self {
            Self::Insert => "INSERT_EVENT",
            Self::Delete => "DELETE_EVENT",
            Self::Update => "UPDATE_EVENT",
        }
    }

    pub fn from_name(name: &str) -> Self {
        match name {
            "INSERT" => Self::Insert,
            "DELETE" => Self::Delete,
            _ => Self::Update,
        }
    }
}

/// `TriggerForEach`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerForEach {
    Statement = 0,
    Row = 1,
}

impl TriggerForEach {
    pub fn name(self) -> &'static str {
        match self {
            Self::Statement => "STATEMENT",
            Self::Row => "ROW",
        }
    }
}

/// `TriggerCatalogEntry`.
#[derive(Debug, Clone)]
pub struct TriggerCatalogEntry {
    pub name: String,
    /// The table the trigger is on, in the same schema.
    pub table: String,
    /// What the CREATE TRIGGER called the table, which its SQL keeps.
    pub table_written: Vec<String>,
    pub timing: TriggerTiming,
    pub event: TriggerEvent,
    /// UPDATE OF these columns; empty for any.
    pub columns: Vec<String>,
    pub for_each: TriggerForEach,
    /// REFERENCING NEW TABLE AS / OLD TABLE AS; empty without one.
    pub referencing_new: String,
    pub referencing_old: String,
    /// The INSERT, UPDATE, DELETE or MERGE INTO that the trigger runs.
    pub action: QueryNode,
    pub temporary: bool,
    /// The tables and views the body reads or writes, by name: they can't be dropped, or altered,
    /// while the trigger is there (its own table can be dropped, and takes the trigger along).
    pub dependencies: Vec<String>,
    pub comment: Option<String>,
}

impl TriggerCatalogEntry {
    /// Whether it fires for `event` of its table, run `timing` and `for_each`.
    pub fn fires(&self, timing: TriggerTiming, event: TriggerEvent, for_each: TriggerForEach) -> bool {
        self.timing == timing && self.event == event && self.for_each == for_each
    }

    /// Whether the body reads its own table, which blocks altering it.
    pub fn reads_own_table(&self) -> bool {
        self.dependencies.iter().any(|d| d.eq_ignore_ascii_case(&self.table))
    }

    /// Whether `name` is something else the body depends on.
    pub fn depends_on(&self, name: &str) -> bool {
        !name.eq_ignore_ascii_case(&self.table) && self.dependencies.iter().any(|d| d.eq_ignore_ascii_case(name))
    }

    /// `TriggerCatalogEntry::ToSQL`.
    pub fn to_sql(&self) -> String {
        let mut sql = format!(
            "CREATE TRIGGER {} {} {}",
            sql_identifier(&self.name),
            self.timing.name(),
            self.event.name()
        );
        if self.event == TriggerEvent::Update && !self.columns.is_empty() {
            let columns: Vec<String> = self.columns.iter().map(|c| sql_identifier(c)).collect();
            sql += &format!(" OF {}", columns.join(", "));
        }
        sql += " ON ";
        // `QualifiedNameToStringMode::HIDE_DEFAULT_SCHEMA`
        let name = duckrs_parser::ast::QualifiedName::from_parts(self.table_written.clone());
        sql += &name.to_string_hide_default_schema();
        if !self.referencing_new.is_empty() || !self.referencing_old.is_empty() {
            sql += " REFERENCING";
            if !self.referencing_new.is_empty() {
                sql += &format!(" NEW TABLE AS {}", sql_identifier(&self.referencing_new));
            }
            if !self.referencing_old.is_empty() {
                sql += &format!(" OLD TABLE AS {}", sql_identifier(&self.referencing_old));
            }
        }
        sql += &format!(" FOR EACH {} {};", self.for_each.name(), self.action);
        sql
    }
}
