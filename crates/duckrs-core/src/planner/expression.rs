// Ported from duckdb/src/planner/expression/*.cpp and
// duckdb/src/include/duckdb/planner/{column_binding,expression}.hpp @ rust-port-baseline
//
//! Bound expressions (`Expression` and its `Bound*Expression` subclasses): resolved names, a
//! known return type, and functions and casts chosen. Like the AST, the C++ class hierarchy is
//! an enum here.

use crate::function::ScalarFunction;
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
    /// `BoundFunctionExpression` of a scalar function; the children have its argument types.
    Function {
        function: Arc<ScalarFunction>,
        children: Vec<BoundExpr>,
        is_operator: bool,
    },
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
            BoundExprKind::Conjunction { children, .. }
            | BoundExprKind::Operator { children, .. }
            | BoundExprKind::Function { children, .. } => children.iter().collect(),
        }
    }

    pub fn children_mut(&mut self) -> Vec<&mut BoundExpr> {
        match &mut self.kind {
            BoundExprKind::Constant(_) | BoundExprKind::ColumnRef { .. } | BoundExprKind::Reference(_) => Vec::new(),
            BoundExprKind::Cast { child, .. } => vec![child],
            BoundExprKind::Comparison { left, right, .. } => vec![left, right],
            BoundExprKind::Conjunction { children, .. }
            | BoundExprKind::Operator { children, .. }
            | BoundExprKind::Function { children, .. } => children.iter_mut().collect(),
        }
    }

    /// `Expression::IsFoldable`: whether the expression can be evaluated without input rows.
    pub fn is_foldable(&self) -> bool {
        match &self.kind {
            BoundExprKind::ColumnRef { .. } | BoundExprKind::Reference(_) => false,
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
