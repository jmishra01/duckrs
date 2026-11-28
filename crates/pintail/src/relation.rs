// Ported from duckdb/src/main/relation.cpp and duckdb/src/main/relation/*.cpp @ rust-port-baseline
//
//! Relations (`Relation`): queries built a step at a time, each step a query over the one before it, which run
//! on a connection when asked for their rows.

use crate::{Connection, QueryResult, Result};

/// `JoinType` of [`Relation::join`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Outer,
    Semi,
    Anti,
}

impl JoinKind {
    fn sql(self) -> &'static str {
        match self {
            JoinKind::Inner => "INNER JOIN",
            JoinKind::Left => "LEFT JOIN",
            JoinKind::Right => "RIGHT JOIN",
            JoinKind::Outer => "FULL OUTER JOIN",
            JoinKind::Semi => "SEMI JOIN",
            JoinKind::Anti => "ANTI JOIN",
        }
    }
}

/// A query to run (`Relation`), as the SQL it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    sql: String,
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl Relation {
    /// `TableRelation`: the rows of a table or view.
    pub fn table(name: &str) -> Self {
        Self::from_sql(format!("SELECT * FROM {}", quoted(name)))
    }

    /// `TableRelation` of `schema.name`.
    pub fn table_in(schema: &str, name: &str) -> Self {
        Self::from_sql(format!("SELECT * FROM {}.{}", quoted(schema), quoted(name)))
    }

    /// `QueryRelation`: the rows of a query.
    pub fn from_sql(query: impl Into<String>) -> Self {
        Self { sql: query.into() }
    }

    /// `ValueRelation`: rows of expressions, with the names of their columns.
    pub fn values(rows: &[&[&str]], names: &[&str]) -> Self {
        let rows: Vec<String> = rows.iter().map(|row| format!("({})", row.join(", "))).collect();
        let names: Vec<String> = names.iter().map(|n| quoted(n)).collect();
        Self::from_sql(format!(
            "SELECT * FROM (VALUES {}) AS valueslist({})",
            rows.join(", "),
            names.join(", ")
        ))
    }

    /// The SQL of the relation (`Relation::ToString` of what it runs).
    pub fn sql(&self) -> &str {
        &self.sql
    }

    fn over(&self, query: impl FnOnce(&str) -> String) -> Self {
        Self::from_sql(query(&format!("({}) AS __relation", self.sql)))
    }

    /// `FilterRelation`: the rows for which `condition` holds.
    pub fn filter(&self, condition: &str) -> Self {
        self.over(|from| format!("SELECT * FROM {from} WHERE {condition}"))
    }

    /// `ProjectionRelation`: these expressions of each row.
    pub fn project(&self, expressions: &[&str]) -> Self {
        let list = expressions.join(", ");
        self.over(|from| format!("SELECT {list} FROM {from}"))
    }

    /// `AggregateRelation`: these expressions, grouped by `groups` (all rows are one group without any).
    pub fn aggregate(&self, expressions: &[&str], groups: &[&str]) -> Self {
        let list = expressions.join(", ");
        if groups.is_empty() {
            return self.over(|from| format!("SELECT {list} FROM {from}"));
        }
        let groups = groups.join(", ");
        self.over(|from| format!("SELECT {list} FROM {from} GROUP BY {groups}"))
    }

    /// `OrderRelation`: the rows in this order (`a DESC`, `b NULLS FIRST`, ...).
    pub fn order(&self, terms: &[&str]) -> Self {
        let terms = terms.join(", ");
        self.over(|from| format!("SELECT * FROM {from} ORDER BY {terms}"))
    }

    /// `LimitRelation`: at most `limit` rows, after `offset` of them.
    pub fn limit(&self, limit: u64, offset: u64) -> Self {
        self.over(|from| format!("SELECT * FROM {from} LIMIT {limit} OFFSET {offset}"))
    }

    /// `DistinctRelation`: each row once.
    pub fn distinct(&self) -> Self {
        self.over(|from| format!("SELECT DISTINCT * FROM {from}"))
    }

    /// `SubqueryRelation`: the relation under another name, which conditions of a join can use.
    pub fn alias(&self, name: &str) -> Self {
        Self::from_sql(format!("SELECT * FROM ({}) AS {}", self.sql, quoted(name)))
    }

    /// `JoinRelation`: the rows of both, joined on `condition`. A relation names its columns by its alias.
    pub fn join(&self, other: &Relation, left: &str, right: &str, condition: &str, kind: JoinKind) -> Self {
        Self::from_sql(format!(
            "SELECT * FROM ({}) AS {} {} ({}) AS {} ON {condition}",
            self.sql,
            quoted(left),
            kind.sql(),
            other.sql,
            quoted(right)
        ))
    }

    /// `CrossProductRelation`.
    pub fn cross_product(&self, other: &Relation) -> Self {
        Self::from_sql(format!(
            "SELECT * FROM ({}) AS __left, ({}) AS __right",
            self.sql, other.sql
        ))
    }

    fn set_operation(&self, other: &Relation, operation: &str) -> Self {
        Self::from_sql(format!("({}) {operation} ({})", self.sql, other.sql))
    }

    /// `SetOpRelation` UNION ALL (`Relation::Union`).
    pub fn union(&self, other: &Relation) -> Self {
        self.set_operation(other, "UNION ALL")
    }

    /// `SetOpRelation` EXCEPT.
    pub fn except(&self, other: &Relation) -> Self {
        self.set_operation(other, "EXCEPT")
    }

    /// `SetOpRelation` INTERSECT.
    pub fn intersect(&self, other: &Relation) -> Self {
        self.set_operation(other, "INTERSECT")
    }

    /// `Relation::Execute`: the rows.
    pub fn execute(&self, connection: &mut Connection) -> Result<QueryResult> {
        connection.query(&self.sql)
    }

    /// `Relation::Insert`: the rows into a table.
    pub fn insert_into(&self, connection: &mut Connection, table: &str) -> Result<()> {
        connection
            .query(&format!("INSERT INTO {} {}", quoted(table), self.sql))
            .map(drop)
    }

    /// `Relation::Create`: a new table of the rows.
    pub fn create_table(&self, connection: &mut Connection, table: &str) -> Result<()> {
        connection
            .query(&format!("CREATE TABLE {} AS {}", quoted(table), self.sql))
            .map(drop)
    }

    /// `Relation::CreateView`.
    pub fn create_view(&self, connection: &mut Connection, view: &str, replace: bool) -> Result<()> {
        let replace = if replace { "OR REPLACE " } else { "" };
        connection
            .query(&format!("CREATE {replace}VIEW {} AS {}", quoted(view), self.sql))
            .map(drop)
    }

    /// `Relation::Explain`.
    pub fn explain(&self, connection: &mut Connection) -> Result<QueryResult> {
        connection.query(&format!("EXPLAIN {}", self.sql))
    }
}
