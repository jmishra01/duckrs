// Ported from duckdb/src/planner/expression/*.cpp and
// duckdb/src/include/duckdb/planner/{column_binding,expression}.hpp @ rust-port-baseline
//
//! Bound expressions (`Expression` and its `Bound*Expression` subclasses): resolved names, a
//! known return type, and functions and casts chosen. Like the AST, the C++ class hierarchy is
//! an enum here.

use crate::catalog::SequenceCatalogEntry;
use crate::function::ScalarFunction;
use crate::function::aggregates::AggregateFunction;
use crate::planner::logical::LogicalOperator;
use duckrs_common::Result;
use duckrs_common::sort_key::OrderModifiers;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_parser::ast::{ComparisonOp, WindowBoundary, WindowExclude};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
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
    /// `OPERATOR_HAS_ERROR`: whether evaluating the argument fails, per row
    HasError,
    /// `COMPARE_IN`: whether the first argument equals one of the others. The binder only keeps a
    /// list that `InClauseRewriter` can turn into a MARK join; it spells out the others as `=` ORs
    In,
    /// `COMPARE_NOT_IN`
    NotIn,
}

impl BoundOperator {
    pub fn is_in(self) -> bool {
        matches!(self, BoundOperator::In | BoundOperator::NotIn)
    }
}

/// What a subquery asks of its rows (`SubqueryType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubqueryKind {
    /// its only value
    Scalar,
    /// whether there are rows
    Exists,
    /// whether the comparison with the child holds for any of its rows
    Any(ComparisonOp),
}

/// A subquery (`BoundSubqueryExpression`). DuckDB plans it into the query with a join, which
/// for a correlated one takes flattening it. `planner::subquery` does that where it can. The rest
/// stay a plan of their own, which runs once for each combination of values of its correlated
/// columns, with them as constants.
#[derive(Debug)]
pub struct BoundSubquery {
    pub kind: SubqueryKind,
    /// has its column bindings resolved; correlated columns stay
    pub plan: LogicalOperator,
    /// the plan before the optimizer and resolving bindings, to flatten it into the query around
    /// it; `None` if that is not an option
    pub raw: Option<LogicalOperator>,
    /// the correlated columns, as the plan refers to them
    pub correlated: Vec<ColumnBinding>,
    /// a volatile plan runs for every row, and not per combination of correlated values
    pub volatile: bool,
    /// `scalar_subquery_error_on_multiple_rows`
    pub error_on_multiple_rows: bool,
    /// whether a row is compared to the columns of the subquery as a row of them, column by column
    /// (`ExtractSubqueryChildren`), and not to a struct the subquery returns
    pub row: bool,
    /// what ran so far, by the sort keys of the correlated values
    pub results: Mutex<HashMap<Vec<u8>, Arc<Result<SubqueryResult>>>>,
}

/// The rows of a subquery that ran.
#[derive(Debug)]
pub struct SubqueryResult {
    pub rows: u64,
    /// the only value of a scalar subquery, or a row of them
    pub first: Option<Value>,
    /// the sort keys of the non-NULL values, for ANY
    pub keys: HashSet<Vec<u8>>,
    pub has_null: bool,
    /// the rows of a row subquery, which compare column by column
    pub rows_values: Vec<Value>,
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
    /// `list_transform`, `list_filter` or `list_reduce`. `body` reads the chunk the operator
    /// builds: the parameters and the index, then the `captures`, which are in the input's.
    ListLambda {
        kind: LambdaKind,
        list: Box<BoundExpr>,
        initial: Option<Box<BoundExpr>>,
        captures: Vec<BoundExpr>,
        body: Box<BoundExpr>,
    },
    /// `BoundColumnRefExpression`: a column of an operator below, until the
    /// `ColumnBindingResolver` turns it into a [`BoundExprKind::Reference`].
    ColumnRef { binding: ColumnBinding, depth: usize },
    /// `BoundReferenceExpression`: a column of the input chunk, by index.
    Reference(usize),
    /// A dynamic filter a hash join pushes into a scan below its probe side (`JoinFilterPushdown`):
    /// whether `child` can equal a key of the build side. Until the build side is done, or when
    /// it lets most rows through, every row passes.
    JoinFilter {
        child: Box<BoundExpr>,
        filter: Arc<crate::execution::join_filter::JoinFilter>,
        /// a NULL of `child` passes: it is a value a cast failed on, for the query to fail on
        keep_null: bool,
    },
    /// The dynamic filter a Top-N pushes into a scan (`TopN::PushdownDynamicFilters`): whether `child`, the sort
    /// key, can be among the first rows. Until the Top-N has a boundary every row passes.
    TopNFilter {
        child: Box<BoundExpr>,
        filter: Arc<crate::execution::join_filter::TopNFilter>,
    },
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
    /// A subquery; `child` is what ANY compares with its rows, and `correlated` the values of
    /// `subquery.correlated` for a row of this query.
    Subquery {
        subquery: Arc<BoundSubquery>,
        child: Option<Box<BoundExpr>>,
        correlated: Vec<BoundExpr>,
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
    /// what DISTINCT tells rows apart by, if not the argument itself: its collated value
    pub distinct_key: Option<BoundExpr>,
    /// the `ORDER BY` keys (by position) that are not arguments: DISTINCT tells rows apart by
    /// them too (`MultiStageAggregateRewriter::GetOrderExpressions`)
    pub distinct_order: Vec<usize>,
}

/// The functions that give something else on each call (`FunctionStability::VOLATILE`).
pub(crate) const VOLATILE_FUNCTIONS: &[&str] = &[
    "random",
    "vector_type",
    "write_log",
    "stats",
    "sleep_ms",
    "setseed",
    "uuid",
    "gen_random_uuid",
    "uuidv4",
    "uuidv7",
];

/// The table functions computed while binding that are `FunctionStability::VOLATILE`: they read
/// live state, so two calls in one query are not the same rows.
pub(crate) const VOLATILE_TABLE_FUNCTIONS: &[&str] = &["duckdb_memory"];

/// The functions that give the same in one query, but not in the next (`FunctionStability::CONSISTENT_WITHIN_QUERY`):
/// they may move as other expressions do, but are not worked out when the query is planned.
pub(crate) const CONSISTENT_WITHIN_QUERY_FUNCTIONS: &[&str] = &[
    "now",
    "get_current_timestamp",
    "transaction_timestamp",
    "current_timestamp",
    "current_date",
    "today",
    "in_search_path",
];

/// What a lambda function does with its lambda.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LambdaKind {
    Transform,
    Filter,
    Reduce,
}

/// What a window computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFunction {
    Aggregate,
    RowNumber,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
    Ntile,
    Lag,
    Lead,
    FirstValue,
    LastValue,
    NthValue,
    Fill,
}

/// `BoundWindowExpression`.
#[derive(Debug, Clone)]
pub struct BoundWindow {
    pub function: WindowFunction,
    /// With `WindowFunction::Aggregate`.
    pub aggregate: Option<Arc<AggregateFunction>>,
    pub return_type: LogicalType,
    pub children: Vec<BoundExpr>,
    pub filter: Option<BoundExpr>,
    pub distinct: bool,
    pub ignore_nulls: bool,
    pub partitions: Vec<BoundExpr>,
    pub orders: Vec<(BoundExpr, OrderModifiers)>,
    /// The ORDER BY among the arguments.
    pub arg_orders: Vec<(BoundExpr, OrderModifiers)>,
    pub start: WindowBoundary,
    pub end: WindowBoundary,
    /// With RANGE the value of the order at the boundary; otherwise the offset.
    pub start_expr: Option<BoundExpr>,
    pub end_expr: Option<BoundExpr>,
    pub exclude: WindowExclude,
}

impl BoundWindow {
    pub fn expressions_mut(&mut self) -> Vec<&mut BoundExpr> {
        self.children
            .iter_mut()
            .chain(self.filter.iter_mut())
            .chain(self.partitions.iter_mut())
            .chain(self.orders.iter_mut().map(|(expr, _)| expr))
            .chain(self.arg_orders.iter_mut().map(|(expr, _)| expr))
            .chain(self.start_expr.iter_mut())
            .chain(self.end_expr.iter_mut())
            .collect()
    }
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
            BoundExprKind::Cast { child, .. }
            | BoundExprKind::JoinFilter { child, .. }
            | BoundExprKind::TopNFilter { child, .. } => vec![child],
            BoundExprKind::Subquery { child, correlated, .. } => child.iter().map(|c| &**c).chain(correlated).collect(),
            BoundExprKind::Comparison { left, right, .. } => vec![left, right],
            BoundExprKind::Between { input, lower, upper } => vec![input, lower, upper],
            BoundExprKind::ListLambda {
                list,
                initial,
                captures,
                ..
            } => {
                let mut children = vec![&**list];
                children.extend(initial.iter().map(|i| &**i));
                children.extend(captures.iter());
                children
            }
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
            BoundExprKind::Cast { child, .. }
            | BoundExprKind::JoinFilter { child, .. }
            | BoundExprKind::TopNFilter { child, .. } => vec![child],
            BoundExprKind::Subquery { child, correlated, .. } => {
                child.iter_mut().map(|c| &mut **c).chain(correlated).collect()
            }
            BoundExprKind::Comparison { left, right, .. } => vec![left, right],
            BoundExprKind::Between { input, lower, upper } => vec![input, lower, upper],
            BoundExprKind::ListLambda {
                list,
                initial,
                captures,
                ..
            } => {
                let mut children: Vec<&mut BoundExpr> = vec![&mut **list];
                children.extend(initial.iter_mut().map(|i| &mut **i));
                children.extend(captures.iter_mut());
                children
            }
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
    /// `Expression::IsVolatile`: whether running it twice can differ.
    pub fn is_volatile(&self) -> bool {
        let volatile = match &self.kind {
            BoundExprKind::Sequence { .. } => true,
            BoundExprKind::Function { function, .. } => VOLATILE_FUNCTIONS.contains(&function.name.as_str()),
            // the body has a scope of its own, so it is not among the children
            BoundExprKind::ListLambda { body, .. } => body.is_volatile(),
            _ => false,
        };
        volatile || self.children().into_iter().any(BoundExpr::is_volatile)
    }

    pub fn is_foldable(&self) -> bool {
        if self.is_volatile() {
            return false;
        }
        match &self.kind {
            BoundExprKind::ColumnRef { .. }
            | BoundExprKind::Reference(_)
            | BoundExprKind::Sequence { .. }
            | BoundExprKind::Subquery { .. } => false,
            BoundExprKind::Function { function, .. }
                if CONSISTENT_WITHIN_QUERY_FUNCTIONS.contains(&function.name.as_str()) =>
            {
                false
            }
            _ => self.children().into_iter().all(BoundExpr::is_foldable),
        }
    }

    /// Replaces the correlated columns of `columns` with their values, also in subqueries.
    pub fn substitute_columns(&mut self, columns: &[(ColumnBinding, Value)]) {
        self.walk_mut(&mut |e| match &mut e.kind {
            BoundExprKind::ColumnRef { binding, depth } if *depth > 0 => {
                if let Some((_, value)) = columns.iter().find(|(b, _)| b == binding) {
                    e.kind = BoundExprKind::Constant(value.clone());
                }
            }
            BoundExprKind::Subquery { subquery, .. }
                if subquery.correlated.iter().any(|c| columns.iter().any(|(b, _)| b == c)) =>
            {
                let mut plan = subquery.plan.clone();
                plan.substitute_columns(columns);
                *subquery = Arc::new(BoundSubquery {
                    kind: subquery.kind,
                    plan,
                    raw: None,
                    correlated: subquery.correlated.clone(),
                    volatile: subquery.volatile,
                    error_on_multiple_rows: subquery.error_on_multiple_rows,
                    row: subquery.row,
                    results: Mutex::default(),
                });
            }
            _ => {}
        });
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
            // `BoundFunctionExpression::ToString` of `nextval('name')` and its kin
            BoundExprKind::Sequence { op, sequence, arguments } => {
                let name = match op {
                    SequenceOp::Next => "nextval",
                    SequenceOp::Current => "currval",
                    SequenceOp::Set => "setval",
                };
                write!(f, "{name}('{}'", sequence.name.replace('\'', "''"))?;
                for argument in arguments {
                    write!(f, ", {argument}")?;
                }
                f.write_str(")")
            }
            // `Value::ToSQLString`: a NULL with its type, unless it has none
            BoundExprKind::Constant(value) if value.is_null() => {
                if value.logical_type().id() == duckrs_common::types::LogicalTypeId::SqlNull {
                    f.write_str("NULL")
                } else {
                    write!(f, "NULL::{}", value.logical_type())
                }
            }
            BoundExprKind::Constant(value) => {
                use duckrs_common::types::LogicalTypeId as T;
                let text = value.try_to_string().unwrap_or_default();
                // `Value::ToSQLString`
                match value.logical_type().id() {
                    T::Varchar | T::Enum => write!(f, "'{}'", text.replace('\'', "''")),
                    T::Uuid
                    | T::Date
                    | T::Time
                    | T::TimeNs
                    | T::Timestamp
                    | T::TimeTz
                    | T::TimestampTz
                    | T::TimestampTzNs
                    | T::TimestampSec
                    | T::TimestampMs
                    | T::TimestampNs
                    | T::Interval
                    | T::Blob
                    | T::Bit => write!(f, "'{text}'::{}", value.logical_type()),
                    _ => f.write_str(&text),
                }
            }
            _ if !self.alias.is_empty()
                && matches!(self.kind, BoundExprKind::ColumnRef { .. } | BoundExprKind::Reference(_)) =>
            {
                f.write_str(&self.alias)
            }
            // `ColumnBinding::ToString` and `BoundReferenceExpression::ToString` without an alias
            BoundExprKind::ColumnRef { binding, .. } => {
                write!(f, "#[{}.{}]", binding.table_index, binding.column_index)
            }
            BoundExprKind::Reference(index) => write!(f, "#{index}"),
            BoundExprKind::JoinFilter { child, .. } | BoundExprKind::TopNFilter { child, .. } => {
                write!(f, "optional: Dynamic Filter ({child})")
            }
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
                    // `KeywordHelper::WriteOptionallyQuoted`: a name that is no identifier is quoted
                    let plain = function.name.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
                        && function.name.chars().all(|c| c.is_alphanumeric() || c == '_');
                    if plain {
                        write!(f, "{}({})", function.name, args.join(", "))
                    } else {
                        write!(f, "\"{}\"({})", function.name.replace('"', "\"\""), args.join(", "))
                    }
                }
            },
            BoundExprKind::Conjunction { and, children } => {
                let parts: Vec<String> = children.iter().map(|c| c.to_string()).collect();
                write!(f, "({})", parts.join(if *and { " AND " } else { " OR " }))
            }
            BoundExprKind::Operator { op, children } => {
                let parts: Vec<String> = children.iter().map(|c| c.to_string()).collect();
                let all = parts.join(", ");
                match op {
                    BoundOperator::Not => write!(f, "(NOT {all})"),
                    BoundOperator::IsNull => write!(f, "({all} IS NULL)"),
                    BoundOperator::IsNotNull => write!(f, "({all} IS NOT NULL)"),
                    BoundOperator::Coalesce => write!(f, "COALESCE({all})"),
                    BoundOperator::Try => write!(f, "TRY({all})"),
                    BoundOperator::HasError => write!(f, "has_error({all})"),
                    BoundOperator::In | BoundOperator::NotIn => {
                        let list: Vec<&str> = parts[1..].iter().map(String::as_str).collect();
                        let keyword = if *op == BoundOperator::In { "IN" } else { "NOT IN" };
                        write!(f, "({} {keyword} ({}))", parts[0], list.join(", "))
                    }
                }
            }
            BoundExprKind::Between { input, lower, upper } => write!(f, "({input} BETWEEN {lower} AND {upper})"),
            BoundExprKind::Case { checks, else_expr } => {
                f.write_str("CASE")?;
                for (when, then) in checks {
                    write!(f, " WHEN {when} THEN {then}")?;
                }
                write!(f, " ELSE {else_expr} END")
            }
            BoundExprKind::Subquery { .. } => f.write_str("SUBQUERY"),
            // `BoundFunctionExpression::ToString` of a lambda function: the list, the lambda with the
            // names of its parameters, and the initial value
            BoundExprKind::ListLambda {
                kind,
                list,
                initial,
                body,
                ..
            } => {
                let name = match kind {
                    LambdaKind::Transform => "list_apply",
                    LambdaKind::Filter => "list_filter",
                    LambdaKind::Reduce => "list_reduce",
                };
                let mut names: Vec<String> = Vec::new();
                let slots = if *kind == LambdaKind::Reduce { 3 } else { 2 };
                for slot in 0..slots {
                    let mut found = None;
                    body.clone().walk_mut(&mut |e| {
                        if let BoundExprKind::Reference(at) = e.kind
                            && at == slot
                            && found.is_none()
                            && !e.alias.is_empty()
                        {
                            found = Some(e.alias.clone());
                        }
                    });
                    match found {
                        Some(name) => names.push(name),
                        None if slot == 0 => names.push("x".to_string()),
                        None => break,
                    }
                }
                write!(f, "{name}({list}, lambda {}: {body}", names.join(", "))?;
                if let Some(initial) = initial {
                    write!(f, ", {initial}")?;
                }
                f.write_str(")")
            }
            _ => write!(f, "{:?}", self.kind),
        }
    }
}

/// `x IN (a, b, ...)` as `x = a OR x = b ...`, and `x NOT IN (...)` as the NOT of that, of the
/// `children` of an IN: the input, then the values.
pub fn in_as_comparisons(negated: bool, children: &[BoundExpr]) -> BoundExpr {
    let input = &children[0];
    let comparisons = children[1..]
        .iter()
        .map(|value| {
            BoundExpr::new(
                BoundExprKind::Comparison {
                    op: ComparisonOp::Equal,
                    left: Box::new(input.clone()),
                    right: Box::new(value.clone()),
                },
                LogicalType::BOOLEAN,
            )
        })
        .collect();
    let result = BoundExpr::new(
        BoundExprKind::Conjunction {
            and: false,
            children: comparisons,
        },
        LogicalType::BOOLEAN,
    );
    if negated {
        BoundExpr::new(
            BoundExprKind::Operator {
                op: BoundOperator::Not,
                children: vec![result],
            },
            LogicalType::BOOLEAN,
        )
    } else {
        result
    }
}
