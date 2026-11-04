// Ported from duckdb/src/planner/expression/*.cpp and
// duckdb/src/include/duckdb/planner/{column_binding,expression}.hpp @ rust-port-baseline
//
//! Bound expressions (`Expression` and its `Bound*Expression` subclasses): resolved names, a
//! known return type, and functions and casts chosen. Like the AST, the C++ class hierarchy is
//! an enum here.

use crate::catalog::SequenceCatalogEntry;
use crate::function::ScalarFunction;
use crate::function::aggregates::AggregateFunction;
use duckrs_common::sort_key::OrderModifiers;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_parser::ast::ComparisonOp;
use std::fmt;
use std::sync::Arc;

/// A column of a logical operator's output (`ColumnBinding`): which table index produced it,
/// and its position there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ColumnBinding {
    pub table_index: usize,
    pub column_index: usize,
}

impl ColumnBinding {
    pub fn new(table_index: usize, column_index: usize) -> Self {
        Self {
            table_index,
            column_index,
        }
    }
}

impl fmt::Display for ColumnBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#[{}.{}]", self.table_index, self.column_index)
    }
}

/// The operators of `BoundOperatorExpression` that slice 1 binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundOperator {
    Not,
    IsNull,
    IsNotNull,
    /// the first argument that is not NULL
    Coalesce,
    /// the argument, with NULL for the rows that fail
    Try,
}

/// What a call of a sequence function does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceOp {
    Next,
    Current,
    Set,
}

#[derive(Debug, Clone)]
pub enum BoundExprKind {
    /// `BoundConstantExpression`.
    Constant(Value),
    /// `BoundColumnRefExpression`: a column of an operator below, until the
    /// `ColumnBindingResolver` turns it into a [`BoundExprKind::Reference`].
    ColumnRef { binding: ColumnBinding, depth: usize },
    /// `BoundReferenceExpression`: a column of the input chunk, by index.
    Reference(usize),
    /// `BoundCastExpression` to the expression's return type.
    Cast { child: Box<BoundExpr>, try_cast: bool },
    /// `BoundComparisonExpression`: both sides have the same type.
    Comparison {
        op: ComparisonOp,
        left: Box<BoundExpr>,
        right: Box<BoundExpr>,
    },
    /// `BoundConjunctionExpression`: AND (`and == true`) or OR.
    Conjunction { and: bool, children: Vec<BoundExpr> },
    /// `BoundOperatorExpression`.
    Operator {
        op: BoundOperator,
        children: Vec<BoundExpr>,
    },
    /// `BoundCaseExpression`; a THEN or ELSE only runs on the rows that reach it.
    Case {
        checks: Vec<(BoundExpr, BoundExpr)>,
        else_expr: Box<BoundExpr>,
    },
    /// `BoundBetweenExpression`, inclusive on both sides.
    Between {
        input: Box<BoundExpr>,
        lower: Box<BoundExpr>,
        upper: Box<BoundExpr>,
    },
    /// `nextval`, `currval` or `setval` with the sequence bound (`NextvalBindData`); `arguments`
    /// are the ones after its name. It changes state, so it never folds.
    Sequence {
        op: SequenceOp,
        sequence: Arc<SequenceCatalogEntry>,
        arguments: Vec<BoundExpr>,
    },
    /// `BoundFunctionExpression` of a scalar function; the children have its argument types.
    Function {
        function: Arc<ScalarFunction>,
        children: Vec<BoundExpr>,
        is_operator: bool,
    },
}

/// An aggregate of a query (`BoundAggregateExpression`); expressions refer to its result
/// through a column of the aggregate's table index.
#[derive(Debug, Clone)]
pub struct BoundAggregate {
    pub function: Arc<AggregateFunction>,
    pub return_type: LogicalType,
    pub children: Vec<BoundExpr>,
    /// `FILTER (WHERE ...)`
    pub filter: Option<BoundExpr>,
    pub distinct: bool,
    /// `ORDER BY`: the order the aggregate sees its rows in
    pub order_by: Vec<(BoundExpr, OrderModifiers)>,
}

/// A bound expression (`Expression`).
#[derive(Debug, Clone)]
pub struct BoundExpr {
    pub kind: BoundExprKind,
    pub return_type: LogicalType,
    pub alias: String,
}

impl BoundExpr {
    pub fn new(kind: BoundExprKind, return_type: LogicalType) -> Self {
        Self {
            kind,
            return_type,
            alias: String::new(),
        }
    }

    pub fn constant(value: Value) -> Self {
        let ty = value.logical_type().clone();
        Self::new(BoundExprKind::Constant(value), ty)
    }

    pub fn column_ref(binding: ColumnBinding, ty: LogicalType, alias: impl Into<String>) -> Self {
        Self {
            kind: BoundExprKind::ColumnRef { binding, depth: 0 },
            return_type: ty,
            alias: alias.into(),
        }
    }

    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.alias = alias.into();
        self
    }

    /// The direct children.
    pub fn children(&self) -> Vec<&BoundExpr> {
        match &self.kind {
            BoundExprKind::Constant(_) | BoundExprKind::ColumnRef { .. } | BoundExprKind::Reference(_) => Vec::new(),
            BoundExprKind::Cast { child, .. } => vec![child],
            BoundExprKind::Comparison { left, right, .. } => vec![left, right],
            BoundExprKind::Between { input, lower, upper } => vec![input, lower, upper],
            BoundExprKind::Case { checks, else_expr } => {
                let mut children: Vec<&BoundExpr> = checks.iter().flat_map(|(when, then)| [when, then]).collect();
                children.push(else_expr);
                children
            }
            BoundExprKind::Conjunction { children, .. }
            | BoundExprKind::Operator { children, .. }
            | BoundExprKind::Sequence {
                arguments: children, ..
            }
            | BoundExprKind::Function { children, .. } => children.iter().collect(),
        }
    }

    pub fn children_mut(&mut self) -> Vec<&mut BoundExpr> {
        match &mut self.kind {
            BoundExprKind::Constant(_) | BoundExprKind::ColumnRef { .. } | BoundExprKind::Reference(_) => Vec::new(),
            BoundExprKind::Cast { child, .. } => vec![child],
            BoundExprKind::Comparison { left, right, .. } => vec![left, right],
            BoundExprKind::Between { input, lower, upper } => vec![input, lower, upper],
            BoundExprKind::Case { checks, else_expr } => {
                let mut children: Vec<&mut BoundExpr> =
                    checks.iter_mut().flat_map(|(when, then)| [when, then]).collect();
                children.push(else_expr);
                children
            }
            BoundExprKind::Conjunction { children, .. }
            | BoundExprKind::Operator { children, .. }
            | BoundExprKind::Sequence {
                arguments: children, ..
            }
            | BoundExprKind::Function { children, .. } => children.iter_mut().collect(),
        }
    }

    /// `Expression::IsFoldable`: whether the expression can be evaluated without input rows.
    pub fn is_foldable(&self) -> bool {
        match &self.kind {
            BoundExprKind::ColumnRef { .. } | BoundExprKind::Reference(_) | BoundExprKind::Sequence { .. } => false,
            _ => self.children().into_iter().all(BoundExpr::is_foldable),
        }
    }

    /// Visits this expression and all its descendants, parents first.
    pub fn walk_mut(&mut self, f: &mut impl FnMut(&mut BoundExpr)) {
        f(self);
        for child in self.children_mut() {
            child.walk_mut(f);
        }
    }

    /// `BoundCastExpression::AddCastToType`: casts the expression to `target` unless it already
    /// has that type.
    pub fn cast_to(self, target: &LogicalType, try_cast: bool) -> BoundExpr {
        if &self.return_type == target {
            return self;
        }
        let alias = self.alias.clone();
        let mut cast = BoundExpr::new(
            BoundExprKind::Cast {
                child: Box::new(self),
                try_cast,
            },
            target.clone(),
        );
        cast.alias = alias;
        cast
    }
}

impl fmt::Display for BoundExpr {
    /// `Expression::ToString`, as far as error messages need it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            BoundExprKind::Constant(value) if value.is_null() => f.write_str("NULL"),
            BoundExprKind::Constant(value) => {
                let text = value.try_to_string().unwrap_or_default();
                if value.logical_type().id() == duckrs_common::types::LogicalTypeId::Varchar {
                    write!(f, "'{}'", text.replace('\'', "''"))
                } else {
                    f.write_str(&text)
                }
            }
            BoundExprKind::ColumnRef { .. } | BoundExprKind::Reference(_) => f.write_str(&self.alias),
            BoundExprKind::Cast { child, try_cast } => {
                let name = if *try_cast { "TRY_CAST" } else { "CAST" };
                write!(f, "{name}({child} AS {})", self.return_type)
            }
            BoundExprKind::Comparison { op, left, right } => write!(f, "({left} {} {right})", op.operator()),
            BoundExprKind::Function {
                function,
                children,
                is_operator,
            } => match children.as_slice() {
                [left, right] if *is_operator => write!(f, "({left} {} {right})", function.name),
                [child] if *is_operator => write!(f, "{}{child}", function.name),
                _ => {
                    let args: Vec<String> = children.iter().map(|c| c.to_string()).collect();
                    write!(f, "{}({})", function.name, args.join(", "))
                }
            },
            _ => write!(f, "{:?}", self.kind),
        }
    }
}
