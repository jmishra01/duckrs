// Ported from duckdb/src/parser/{expression,query_node,tableref,statement}/*.cpp and the ToString
// templates in duckdb/src/include/duckdb/parser/expression/*.hpp @ rust-port-baseline
//
//! The parsed (unbound) AST and its SQL rendering. `Display` reproduces DuckDB's `ToString`, so
//! rendering a statement and parsing it again yields the same tree.

use crate::keywords;
use std::collections::BTreeSet;
use std::fmt::{self, Display, Formatter, Write};

/// `SQLIdentifier`: an identifier, double-quoted when it would not read back as itself.
pub fn sql_identifier(text: &str) -> String {
    let b = text.as_bytes();
    let ambiguous_after_dot = b.len() > 1 && b[0] == b'_' && b[1].is_ascii_digit();
    let plain = b
        .iter()
        .enumerate()
        .all(|(i, &c)| c.is_ascii_lowercase() || c.is_ascii_uppercase() || c == b'_' || (i > 0 && c.is_ascii_digit()));
    if text.is_empty() || ambiguous_after_dot || !plain || keywords::is_keyword(text) {
        quoted(text, '"')
    } else {
        text.to_string()
    }
}

/// `SQLString`: a single-quoted string literal.
pub fn sql_string(text: &str) -> String {
    quoted(text, '\'')
}

fn quoted(text: &str, quote: char) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for c in text.chars() {
        if c == quote {
            out.push(quote);
        }
        out.push(c);
    }
    out.push(quote);
    out
}

fn join<T>(items: &[T], sep: &str, f: impl Fn(&T) -> String) -> String {
    items.iter().map(f).collect::<Vec<_>>().join(sep)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiteralKind {
    Null,
    Boolean,
    /// Digits with an optional leading '-' and '_' separators.
    Integer,
    /// Number text with a '.' and/or an exponent.
    Numeric,
    String,
    Hex,
    Bit,
}

/// A literal as written in the query (`Literal`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Literal {
    pub kind: LiteralKind,
    pub text: String,
}

impl Literal {
    pub fn null() -> Self {
        Self {
            kind: LiteralKind::Null,
            text: String::new(),
        }
    }
    pub fn boolean(value: bool) -> Self {
        Self {
            kind: LiteralKind::Boolean,
            text: if value { "true" } else { "false" }.into(),
        }
    }
    pub fn integer(value: i64) -> Self {
        Self {
            kind: LiteralKind::Integer,
            text: value.to_string(),
        }
    }
    /// Classifies number text as INTEGER or NUMERIC.
    pub fn number(text: impl Into<String>) -> Self {
        let text = text.into();
        let kind = if text.contains(['.', 'e', 'E']) {
            LiteralKind::Numeric
        } else {
            LiteralKind::Integer
        };
        Self { kind, text }
    }
    pub fn string(text: impl Into<String>) -> Self {
        Self {
            kind: LiteralKind::String,
            text: text.into(),
        }
    }
    pub fn is_numeric(&self) -> bool {
        matches!(self.kind, LiteralKind::Integer | LiteralKind::Numeric)
    }
    /// The value of an INTEGER literal that fits in an `i64`.
    pub fn try_get_i64(&self) -> Option<i64> {
        if self.kind != LiteralKind::Integer {
            return None;
        }
        let digits: String = self.text.chars().filter(|&c| c != '_').collect();
        digits.parse().ok()
    }
    /// Negates a numeric literal by toggling the leading '-'.
    pub fn negate(&self) -> Self {
        let text = match self.text.strip_prefix('-') {
            Some(rest) => rest.to_string(),
            None => format!("-{}", self.text),
        };
        Self { kind: self.kind, text }
    }
}

impl Display for Literal {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.kind {
            LiteralKind::Null => f.write_str("NULL"),
            LiteralKind::Boolean | LiteralKind::Integer | LiteralKind::Numeric => f.write_str(&self.text),
            LiteralKind::String => f.write_str(&sql_string(&self.text)),
            LiteralKind::Hex => write!(f, "X'{}'", self.text),
            LiteralKind::Bit => write!(f, "B'{}'", self.text),
        }
    }
}

/// A possibly qualified name (`QualifiedName`): `qualification` holds catalog/schema components,
/// which may be empty (and are then not rendered).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QualifiedName {
    pub qualification: Vec<String>,
    pub name: String,
}

impl QualifiedName {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            qualification: Vec::new(),
            name: name.into(),
        }
    }
    /// The name, leaving out a lone `main` schema (`QualifiedNameToStringMode::HIDE_DEFAULT_SCHEMA`).
    pub fn to_string_hide_default_schema(&self) -> String {
        let mut out = String::new();
        for (i, component) in self.qualification.iter().enumerate() {
            if component.is_empty()
                || (out.is_empty() && i + 1 == self.qualification.len() && component.eq_ignore_ascii_case("main"))
            {
                continue;
            }
            out += &sql_identifier(component);
            out.push('.');
        }
        out + &sql_identifier(&self.name)
    }

    /// A name from its components, the last being the name itself.
    pub fn from_parts(mut parts: Vec<String>) -> Self {
        let name = parts.pop().unwrap_or_default();
        Self {
            qualification: parts,
            name,
        }
    }

    pub fn qualification_to_string(&self) -> String {
        let mut out = String::new();
        for component in self.qualification.iter().filter(|c| !c.is_empty()) {
            out += &sql_identifier(component);
            out.push('.');
        }
        out
    }
}

impl Display for QualifiedName {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.qualification_to_string(), sql_identifier(&self.name))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComparisonOp {
    Equal,
    NotEqual,
    LessThan,
    GreaterThan,
    LessThanOrEqual,
    GreaterThanOrEqual,
    DistinctFrom,
    NotDistinctFrom,
}

impl ComparisonOp {
    /// `ExpressionTypeToOperator`.
    pub fn operator(self) -> &'static str {
        match self {
            Self::Equal => "=",
            Self::NotEqual => "!=",
            Self::LessThan => "<",
            Self::GreaterThan => ">",
            Self::LessThanOrEqual => "<=",
            Self::GreaterThanOrEqual => ">=",
            Self::DistinctFrom => "IS DISTINCT FROM",
            Self::NotDistinctFrom => "IS NOT DISTINCT FROM",
        }
    }
    /// `OperatorToExpressionType`: the comparison an operator string names, if any.
    pub fn from_operator(op: &str) -> Option<Self> {
        Some(match op {
            "=" | "==" => Self::Equal,
            "!=" | "<>" => Self::NotEqual,
            "<" => Self::LessThan,
            ">" => Self::GreaterThan,
            "<=" => Self::LessThanOrEqual,
            ">=" => Self::GreaterThanOrEqual,
            _ => return None,
        })
    }
    /// `NegateComparisonExpression`.
    pub fn negate(self) -> Self {
        match self {
            Self::Equal => Self::NotEqual,
            Self::NotEqual => Self::Equal,
            Self::LessThan => Self::GreaterThanOrEqual,
            Self::GreaterThan => Self::LessThanOrEqual,
            Self::LessThanOrEqual => Self::GreaterThan,
            Self::GreaterThanOrEqual => Self::LessThan,
            Self::DistinctFrom => Self::NotDistinctFrom,
            Self::NotDistinctFrom => Self::DistinctFrom,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorType {
    Not,
    IsNull,
    IsNotNull,
    In,
    NotIn,
    Coalesce,
    Unpack,
    Try,
    ArrayExtract,
    ArraySlice,
    StructExtract,
    ArrayConstructor,
    Grouping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubqueryType {
    Scalar,
    Exists,
    NotExists,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LambdaSyntax {
    Arrow,
    Keyword,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OrderType {
    #[default]
    Default,
    Ascending,
    Descending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NullOrder {
    #[default]
    Default,
    NullsFirst,
    NullsLast,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderByNode {
    pub order: OrderType,
    pub nulls: NullOrder,
    pub expr: Expr,
}

impl Display for OrderByNode {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.expr)?;
        match self.order {
            OrderType::Ascending => f.write_str(" ASC")?,
            OrderType::Descending => f.write_str(" DESC")?,
            OrderType::Default => {}
        }
        match self.nulls {
            NullOrder::NullsFirst => f.write_str(" NULLS FIRST"),
            NullOrder::NullsLast => f.write_str(" NULLS LAST"),
            NullOrder::Default => Ok(()),
        }
    }
}

/// A function argument, optionally named (`name := expr`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionArgument {
    pub name: String,
    pub expr: Expr,
}

impl FunctionArgument {
    pub fn positional(expr: Expr) -> Self {
        Self {
            name: String::new(),
            expr,
        }
    }
}

impl Display for FunctionArgument {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.name.is_empty() {
            write!(f, "{}", self.expr)
        } else {
            write!(f, "{} := {}", sql_identifier(&self.name), self.expr)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionExpr {
    pub name: QualifiedName,
    pub args: Vec<FunctionArgument>,
    pub filter: Option<Box<Expr>>,
    pub order_by: Vec<OrderByNode>,
    pub distinct: bool,
    pub is_operator: bool,
    pub export_state: bool,
}

impl FunctionExpr {
    pub fn new(name: impl Into<String>, args: Vec<Expr>) -> Self {
        Self {
            name: QualifiedName::new(name),
            args: args.into_iter().map(FunctionArgument::positional).collect(),
            filter: None,
            order_by: Vec::new(),
            distinct: false,
            is_operator: false,
            export_state: false,
        }
    }
    pub fn operator(name: impl Into<String>, args: Vec<Expr>) -> Self {
        Self {
            is_operator: true,
            ..Self::new(name, args)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowBoundary {
    Invalid,
    UnboundedPreceding,
    UnboundedFollowing,
    CurrentRowRange,
    CurrentRowRows,
    CurrentRowGroups,
    ExprPrecedingRows,
    ExprPrecedingRange,
    ExprPrecedingGroups,
    ExprFollowingRows,
    ExprFollowingRange,
    ExprFollowingGroups,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowExclude {
    NoOther,
    CurrentRow,
    Group,
    Ties,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowExpr {
    pub name: QualifiedName,
    pub args: Vec<FunctionArgument>,
    pub partitions: Vec<Expr>,
    pub orders: Vec<OrderByNode>,
    pub arg_orders: Vec<OrderByNode>,
    pub start: WindowBoundary,
    pub end: WindowBoundary,
    pub start_expr: Option<Box<Expr>>,
    pub end_expr: Option<Box<Expr>>,
    pub exclude: WindowExclude,
    pub filter: Option<Box<Expr>>,
    pub distinct: bool,
    pub ignore_nulls: bool,
    pub has_ignore_nulls: bool,
}

impl Default for WindowExpr {
    fn default() -> Self {
        Self {
            name: QualifiedName::default(),
            args: Vec::new(),
            partitions: Vec::new(),
            orders: Vec::new(),
            arg_orders: Vec::new(),
            start: WindowBoundary::Invalid,
            end: WindowBoundary::Invalid,
            start_expr: None,
            end_expr: None,
            exclude: WindowExclude::NoOther,
            filter: None,
            distinct: false,
            ignore_nulls: false,
            has_ignore_nulls: false,
        }
    }
}

/// A STAR's EXCLUDE/RENAME column: an optionally qualified column name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualifiedColumnName {
    pub qualification: Vec<String>,
    pub column: String,
}

impl Display for QualifiedColumnName {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for q in self.qualification.iter().filter(|q| !q.is_empty()) {
            write!(f, "{}.", sql_identifier(q))?;
        }
        f.write_str(&sql_identifier(&self.column))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StarExpr {
    pub relation_name: String,
    pub exclude_list: Vec<QualifiedColumnName>,
    pub replace_list: Vec<(String, Expr)>,
    pub rename_list: Vec<(QualifiedColumnName, String)>,
    /// `COLUMNS(expr)`.
    pub expr: Option<Box<Expr>>,
    pub columns: bool,
}

/// A parsed type (`TypeExpression`): a name with type parameters, which are types or constants.
/// STRUCT and UNION members carry their names as aliases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeExpr {
    pub name: QualifiedName,
    pub children: Vec<Expr>,
}

/// The target type of a cast: a built-in type the transformer chose, or a parsed type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CastType {
    Builtin(&'static str),
    Parsed(Box<Expr>),
}

impl Display for CastType {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            CastType::Builtin(name) => f.write_str(name),
            CastType::Parsed(expr) if matches!(expr.kind, ExprKind::Type(_)) => write!(f, "{expr}"),
            CastType::Parsed(expr) => write!(f, "({expr})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprKind {
    Constant(Literal),
    ColumnRef(Vec<String>),
    Star(StarExpr),
    Function(FunctionExpr),
    Window(Box<WindowExpr>),
    Comparison {
        op: ComparisonOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// AND (`and == true`) or OR over two or more children.
    Conjunction {
        and: bool,
        children: Vec<Expr>,
    },
    Operator {
        op: OperatorType,
        children: Vec<Expr>,
    },
    Between {
        input: Box<Expr>,
        lower: Box<Expr>,
        upper: Box<Expr>,
    },
    Case {
        checks: Vec<(Expr, Expr)>,
        else_expr: Box<Expr>,
    },
    Cast {
        child: Box<Expr>,
        target: CastType,
        try_cast: bool,
    },
    Collate {
        child: Box<Expr>,
        collation: String,
    },
    Subquery {
        subquery: Box<SelectStatement>,
        subquery_type: SubqueryType,
        child: Option<Box<Expr>>,
        comparison: ComparisonOp,
    },
    Lambda {
        lhs: Box<Expr>,
        expr: Box<Expr>,
        syntax: LambdaSyntax,
    },
    Parameter(String),
    Positional(u64),
    Default,
    Type(TypeExpr),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr {
    pub kind: ExprKind,
    pub alias: String,
}

impl From<ExprKind> for Expr {
    fn from(kind: ExprKind) -> Self {
        Self {
            kind,
            alias: String::new(),
        }
    }
}

impl From<FunctionExpr> for Expr {
    fn from(f: FunctionExpr) -> Self {
        ExprKind::Function(f).into()
    }
}

impl Expr {
    pub fn constant(literal: Literal) -> Self {
        ExprKind::Constant(literal).into()
    }
    pub fn column(names: Vec<String>) -> Self {
        ExprKind::ColumnRef(names).into()
    }
    pub fn operator(op: OperatorType, children: Vec<Expr>) -> Self {
        ExprKind::Operator { op, children }.into()
    }
    pub fn not_op(child: Expr) -> Self {
        Self::operator(OperatorType::Not, vec![child])
    }
    pub fn comparison(op: ComparisonOp, left: Expr, right: Expr) -> Self {
        ExprKind::Comparison {
            op,
            left: Box::new(left),
            right: Box::new(right),
        }
        .into()
    }
    pub fn cast(child: Expr, target: CastType) -> Self {
        ExprKind::Cast {
            child: Box::new(child),
            target,
            try_cast: false,
        }
        .into()
    }
    pub fn star() -> Self {
        ExprKind::Star(StarExpr::default()).into()
    }
    pub fn lambda(params: &[String], body: Expr) -> Self {
        let lhs = if params.len() == 1 {
            Expr::column(vec![params[0].clone()])
        } else {
            FunctionExpr::new("row", params.iter().map(|p| Expr::column(vec![p.clone()])).collect()).into()
        };
        ExprKind::Lambda {
            lhs: Box::new(lhs),
            expr: Box::new(body),
            syntax: LambdaSyntax::Keyword,
        }
        .into()
    }
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.alias = alias.into();
        self
    }
    /// `ExpressionIsEmptyStar`: a plain `*` without modifiers.
    pub fn is_empty_star(&self) -> bool {
        matches!(&self.kind, ExprKind::Star(s) if s.exclude_list.is_empty() && s.replace_list.is_empty()
            && s.rename_list.is_empty() && s.expr.is_none() && !s.columns)
    }
}

/// `OperatorExpression::IsEmptySliceBound`.
fn is_empty_slice_bound(rendered: &str) -> bool {
    rendered == "list_value()" || rendered == "[]"
}

fn window_units(boundary: WindowBoundary) -> &'static str {
    use WindowBoundary::*;
    match boundary {
        CurrentRowRange | ExprPrecedingRange | ExprFollowingRange => "RANGE",
        CurrentRowGroups | ExprPrecedingGroups | ExprFollowingGroups => "GROUPS",
        _ => "ROWS",
    }
}

impl Display for WindowExpr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        use WindowBoundary::*;
        let mut r = format!(
            "{}{}(",
            self.name.qualification_to_string(),
            sql_identifier(&self.name.name)
        );
        r += &self
            .args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                if i == 0 && self.distinct {
                    format!("DISTINCT {a}")
                } else {
                    a.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        if !self.arg_orders.is_empty() {
            r += " ORDER BY ";
            r += &join(&self.arg_orders, ", ", |o| o.to_string());
        }
        if self.ignore_nulls {
            r += " IGNORE NULLS";
        }
        if let Some(filter) = &self.filter {
            write!(r, ") FILTER (WHERE {filter}")?;
        }
        r += ") OVER (";
        let mut sep = "";
        if !self.partitions.is_empty() {
            r += "PARTITION BY ";
            r += &join(&self.partitions, ", ", |p| p.to_string());
            sep = " ";
        }
        if !self.orders.is_empty() {
            r += sep;
            r += "ORDER BY ";
            r += &join(&self.orders, ", ", |o| o.to_string());
            sep = " ";
        }
        let mut units = "ROWS";
        let start_expr = || self.start_expr.as_ref().map(|e| e.to_string()).unwrap_or_default();
        let end_expr = || self.end_expr.as_ref().map(|e| e.to_string()).unwrap_or_default();
        let mut from = String::new();
        match self.start {
            CurrentRowRange | CurrentRowRows | CurrentRowGroups => {
                from = "CURRENT ROW".into();
                units = window_units(self.start);
            }
            UnboundedPreceding => {
                if self.end != CurrentRowRange {
                    from = "UNBOUNDED PRECEDING".into();
                }
            }
            ExprPrecedingRows | ExprPrecedingRange | ExprPrecedingGroups => {
                from = format!("{} PRECEDING", start_expr());
                units = window_units(self.start);
            }
            ExprFollowingRows | ExprFollowingRange | ExprFollowingGroups => {
                from = format!("{} FOLLOWING", start_expr());
                units = window_units(self.start);
            }
            UnboundedFollowing | Invalid => {}
        }
        let mut to = String::new();
        match self.end {
            CurrentRowRange => {
                if self.start != UnboundedPreceding {
                    to = "CURRENT ROW".into();
                    units = "RANGE";
                }
            }
            CurrentRowRows | CurrentRowGroups => {
                to = "CURRENT ROW".into();
                units = window_units(self.end);
            }
            UnboundedPreceding => to = "UNBOUNDED PRECEDING".into(),
            UnboundedFollowing => to = "UNBOUNDED FOLLOWING".into(),
            ExprPrecedingRows | ExprPrecedingRange | ExprPrecedingGroups => {
                to = format!("{} PRECEDING", end_expr());
                units = window_units(self.end);
            }
            ExprFollowingRows | ExprFollowingRange | ExprFollowingGroups => {
                to = format!("{} FOLLOWING", end_expr());
                units = window_units(self.end);
            }
            Invalid => {}
        }
        if self.exclude != WindowExclude::NoOther {
            if from.is_empty() {
                from = "UNBOUNDED PRECEDING".into();
            }
            if to.is_empty() {
                to = "CURRENT ROW".into();
                units = "RANGE";
            }
        }
        if !from.is_empty() || !to.is_empty() {
            r += sep;
            r += units;
        }
        if !from.is_empty() && !to.is_empty() {
            write!(r, " BETWEEN {from} AND {to}")?;
        } else if !from.is_empty() {
            write!(r, " {from}")?;
        } else if !to.is_empty() {
            write!(r, " {to}")?;
        }
        match self.exclude {
            WindowExclude::NoOther => {}
            WindowExclude::CurrentRow => r += " EXCLUDE CURRENT ROW",
            WindowExclude::Group => r += " EXCLUDE GROUP",
            WindowExclude::Ties => r += " EXCLUDE TIES",
        }
        r += ")";
        f.write_str(&r)
    }
}

impl Display for StarExpr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if let Some(expr) = &self.expr {
            return write!(f, "COLUMNS({expr})");
        }
        if self.columns {
            f.write_str("COLUMNS(")?;
        }
        if self.relation_name.is_empty() {
            f.write_str("*")?;
        } else {
            write!(f, "{}.*", sql_identifier(&self.relation_name))?;
        }
        if !self.exclude_list.is_empty() {
            write!(f, " EXCLUDE ({})", join(&self.exclude_list, ", ", |e| e.to_string()))?;
        }
        if !self.replace_list.is_empty() {
            let entries = join(&self.replace_list, ", ", |(name, e)| {
                format!("{e} AS {}", sql_identifier(name))
            });
            write!(f, " REPLACE ({entries})")?;
        }
        if !self.rename_list.is_empty() {
            let entries = join(&self.rename_list, ", ", |(c, name)| {
                format!("{c} AS {}", sql_identifier(name))
            });
            write!(f, " RENAME ({entries})")?;
        }
        if self.columns {
            f.write_str(")")?;
        }
        Ok(())
    }
}

impl Display for TypeExpr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let qual = self.name.qualification_to_string();
        let name = &self.name.name;
        let upper = name.to_ascii_uppercase();
        let params = &self.children;
        let unqualified = qual.is_empty();
        if unqualified && upper == "LIST" && params.len() == 1 {
            return write!(f, "{}[]", params[0]);
        }
        if unqualified && upper == "ARRAY" && params.len() == 2 {
            return write!(f, "{}[{}]", params[0], params[1]);
        }
        if unqualified && (upper == "STRUCT" || upper == "UNION") {
            if params.is_empty() {
                return f.write_str(&upper);
            }
            let members = join(params, ", ", |p| format!("{} {p}", sql_identifier(&p.alias)));
            return write!(f, "{upper}({members})");
        }
        if unqualified && upper == "MAP" && params.len() == 2 {
            return write!(f, "MAP({}, {})", params[0], params[1]);
        }
        if unqualified
            && upper == "VARCHAR"
            && let Some(Expr {
                kind: ExprKind::Constant(collation),
                alias,
            }) = params.last()
            && alias == "collation"
        {
            return write!(f, "VARCHAR({})", sql_string(&collation.text));
        }
        if unqualified && upper == "INTERVAL" && !params.is_empty() {
            return f.write_str("INTERVAL");
        }
        let mut result = qual;
        if is_builtin_type_name(name) {
            result += name;
        } else {
            result += &sql_identifier(name);
        }
        if params.is_empty() {
            f.write_str(&result)
        } else {
            write!(f, "{result}({})", join(params, ", ", |p| p.to_string()))
        }
    }
}

/// Whether `name` is one of DuckDB's built-in type names (`DefaultTypeGenerator::GetDefaultType`),
/// other than NULL.
pub fn is_builtin_type_name(name: &str) -> bool {
    const BUILTIN: &[&str] = &[
        "decimal",
        "dec",
        "numeric",
        "time",
        "time_ns",
        "date",
        "timestamp",
        "datetime",
        "timestamp_us",
        "timestamp_ms",
        "timestamp_ns",
        "timestamp_s",
        "timestamptz",
        "timestamptz_ns",
        "timetz",
        "interval",
        "varchar",
        "bpchar",
        "string",
        "char",
        "nvarchar",
        "text",
        "blob",
        "bytea",
        "varbinary",
        "binary",
        "hugeint",
        "int128",
        "uhugeint",
        "uint128",
        "bigint",
        "oid",
        "long",
        "int8",
        "int64",
        "ubigint",
        "uint64",
        "integer",
        "int",
        "int4",
        "signed",
        "integral",
        "int32",
        "uinteger",
        "uint32",
        "smallint",
        "int2",
        "short",
        "int16",
        "usmallint",
        "uint16",
        "tinyint",
        "int1",
        "utinyint",
        "uint8",
        "struct",
        "row",
        "tuple",
        "list",
        "array",
        "map",
        "union",
        "bit",
        "bitstring",
        "variant",
        "bignum",
        "varint",
        "boolean",
        "bool",
        "logical",
        "uuid",
        "guid",
        "enum",
        "float",
        "real",
        "float4",
        "double",
        "float8",
        "geometry",
        "type",
    ];
    BUILTIN.iter().any(|b| b.eq_ignore_ascii_case(name))
}

impl Display for Expr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ExprKind::Constant(literal) => write!(f, "{literal}"),
            ExprKind::ColumnRef(names) => f.write_str(&join(names, ".", |n| sql_identifier(n))),
            ExprKind::Star(star) => write!(f, "{star}"),
            ExprKind::Function(func) => fmt_function(func, f),
            ExprKind::Window(window) => write!(f, "{window}"),
            ExprKind::Comparison { op, left, right } => write!(f, "({left} {} {right})", op.operator()),
            ExprKind::Conjunction { and, children } => {
                let op = if *and { " AND " } else { " OR " };
                write!(f, "({})", join(children, op, |c| c.to_string()))
            }
            ExprKind::Operator { op, children } => fmt_operator(*op, children, f),
            ExprKind::Between { input, lower, upper } => write!(f, "({input} BETWEEN {lower} AND {upper})"),
            ExprKind::Case { checks, else_expr } => {
                f.write_str("CASE ")?;
                for (when, then) in checks {
                    write!(f, " WHEN ({when}) THEN ({then})")?;
                }
                write!(f, " ELSE {else_expr} END")
            }
            ExprKind::Cast {
                child,
                target,
                try_cast,
            } => write!(
                f,
                "{}({child} AS {target})",
                if *try_cast { "TRY_CAST" } else { "CAST" }
            ),
            ExprKind::Collate { child, collation } => write!(f, "{child} COLLATE {}", sql_identifier(collation)),
            ExprKind::Subquery {
                subquery,
                subquery_type,
                child,
                comparison,
            } => match subquery_type {
                SubqueryType::Any => {
                    let child = child.as_ref().map(|c| c.to_string()).unwrap_or_default();
                    write!(f, "({child} {} ANY({subquery}))", comparison.operator())
                }
                SubqueryType::Exists => write!(f, "EXISTS({subquery})"),
                SubqueryType::NotExists => write!(f, "NOT EXISTS({subquery})"),
                SubqueryType::Scalar => write!(f, "({subquery})"),
            },
            ExprKind::Lambda { lhs, expr, syntax } => match syntax {
                LambdaSyntax::Arrow => write!(f, "({lhs} -> {expr})"),
                LambdaSyntax::Keyword => {
                    let params = match &lhs.kind {
                        ExprKind::Function(func) => join(&func.args, ", ", |a| a.expr.to_string()),
                        _ => lhs.to_string(),
                    };
                    write!(f, "(lambda {params}: {expr})")
                }
            },
            ExprKind::Parameter(id) => write!(f, "${id}"),
            ExprKind::Positional(index) => write!(f, "#{index}"),
            ExprKind::Default => f.write_str("DEFAULT"),
            ExprKind::Type(ty) => write!(f, "{ty}"),
        }
    }
}

fn fmt_function(func: &FunctionExpr, f: &mut Formatter<'_>) -> fmt::Result {
    let name = &func.name.name;
    if func.is_operator {
        if func.args.len() == 1 {
            if let Some(postfix) = name.strip_suffix("__postfix") {
                return write!(f, "(({}){postfix})", func.args[0]);
            }
            return write!(f, "({name}({}))", func.args[0]);
        }
        if func.args.len() == 2 {
            return write!(f, "({} {name} {})", func.args[0], func.args[1]);
        }
    }
    write!(f, "{}{}(", func.name.qualification_to_string(), sql_identifier(name))?;
    if func.distinct {
        f.write_str("DISTINCT ")?;
    }
    f.write_str(&join(&func.args, ", ", |a| a.to_string()))?;
    if !func.order_by.is_empty() {
        if func.args.is_empty() && name.eq_ignore_ascii_case("mode") {
            f.write_str(") WITHIN GROUP (")?;
        }
        write!(f, " ORDER BY {}", join(&func.order_by, ", ", |o| o.to_string()))?;
    }
    f.write_str(")")?;
    if let Some(filter) = &func.filter {
        write!(f, " FILTER (WHERE {filter})")?;
    }
    if func.export_state {
        f.write_str(" EXPORT_STATE")?;
    }
    Ok(())
}

fn fmt_operator(op: OperatorType, children: &[Expr], f: &mut Formatter<'_>) -> fmt::Result {
    let list = || join(children, ", ", |c| c.to_string());
    match op {
        OperatorType::In | OperatorType::NotIn => {
            let op = if op == OperatorType::In { " IN " } else { " NOT IN " };
            write!(
                f,
                "({}{op}({}))",
                children[0],
                join(&children[1..], ", ", |c| c.to_string())
            )
        }
        OperatorType::Unpack => write!(f, "UNPACK({})", children[0]),
        OperatorType::Try => write!(f, "TRY({})", children[0]),
        OperatorType::Not => write!(f, "(NOT {})", list()),
        OperatorType::Grouping => write!(f, "GROUPING({})", list()),
        OperatorType::Coalesce => write!(f, "COALESCE({})", list()),
        OperatorType::IsNull => write!(f, "({} IS NULL)", children[0]),
        OperatorType::IsNotNull => write!(f, "({} IS NOT NULL)", children[0]),
        OperatorType::ArrayExtract => write!(f, "{}[{}]", children[0], children[1]),
        OperatorType::ArraySlice => {
            let mut begin = children[1].to_string();
            if is_empty_slice_bound(&begin) {
                begin.clear();
            }
            let mut end = children[2].to_string();
            if is_empty_slice_bound(&end) {
                end = if children.len() == 4 { "-".into() } else { String::new() };
            }
            if children.len() == 4 {
                write!(f, "{}[{begin}:{end}:{}]", children[0], children[3])
            } else {
                write!(f, "{}[{begin}:{end}]", children[0])
            }
        }
        OperatorType::StructExtract => match &children[1].kind {
            ExprKind::Constant(literal) => {
                let rendered = literal.to_string();
                let field = &rendered[1..rendered.len() - 1];
                write!(f, "({}).{}", children[0], sql_identifier(field))
            }
            _ => Ok(()),
        },
        OperatorType::ArrayConstructor => write!(f, "(ARRAY[{}])", list()),
    }
}

// ---------------------------------------------------------------------------------------------
// Query nodes, table references and statements
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Outer,
    Semi,
    Anti,
    Mark,
    Single,
    RightSemi,
    RightAnti,
}

impl JoinType {
    pub fn name(self) -> &'static str {
        match self {
            Self::Inner => "INNER",
            Self::Left => "LEFT",
            Self::Right => "RIGHT",
            Self::Outer => "FULL",
            Self::Semi => "SEMI",
            Self::Anti => "ANTI",
            Self::Mark => "MARK",
            Self::Single => "SINGLE",
            Self::RightSemi => "RIGHT_SEMI",
            Self::RightAnti => "RIGHT_ANTI",
        }
    }
    /// `EnumUtil::FromString<JoinType>` (case-insensitive).
    pub fn from_name(name: &str) -> Option<Self> {
        [
            Self::Inner,
            Self::Left,
            Self::Right,
            Self::Outer,
            Self::Semi,
            Self::Anti,
            Self::Mark,
            Self::Single,
            Self::RightSemi,
            Self::RightAnti,
        ]
        .into_iter()
        .find(|t| t.name().eq_ignore_ascii_case(name))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinRefType {
    Regular,
    Natural,
    Cross,
    Positional,
    Asof,
    Nearest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinRef {
    pub left: Box<TableRef>,
    pub right: Box<TableRef>,
    pub condition: Option<Box<Expr>>,
    pub using_columns: Vec<String>,
    pub join_type: JoinType,
    pub ref_type: JoinRefType,
    /// A comma in the FROM list.
    pub is_implicit: bool,
    pub nearest_count: u64,
    pub nearest_descending: bool,
    pub nearest_approx: bool,
    pub ranking_expression: Option<Box<Expr>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleMethod {
    System,
    Bernoulli,
    Reservoir,
}

impl SampleMethod {
    pub fn name(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Bernoulli => "Bernoulli",
            Self::Reservoir => "Reservoir",
        }
    }
}

/// `SampleOptions`; the size is the rendered value (a DOUBLE percentage or a BIGINT row count).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleOptions {
    pub sample_size: String,
    pub is_percentage: bool,
    pub method: SampleMethod,
    pub seed: Option<u64>,
    pub repeatable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtClause {
    pub unit: String,
    pub expr: Box<Expr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableRefKind {
    Empty,
    BaseTable {
        name: QualifiedName,
        at_clause: Option<AtClause>,
    },
    Join(JoinRef),
    Subquery(Box<SelectStatement>),
    TableFunction {
        function: Box<Expr>,
        with_ordinality: bool,
    },
    ExpressionList(Vec<Vec<Expr>>),
    /// SHOW / DESCRIBE / SUMMARIZE.
    Show(ShowRef),
    /// PIVOT or UNPIVOT of `source`.
    Pivot(PivotRef),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub kind: TableRefKind,
    pub alias: String,
    pub column_name_alias: Vec<String>,
    pub sample: Option<SampleOptions>,
}

impl From<TableRefKind> for TableRef {
    fn from(kind: TableRefKind) -> Self {
        Self {
            kind,
            alias: String::new(),
            column_name_alias: Vec::new(),
            sample: None,
        }
    }
}

impl TableRef {
    pub fn empty() -> Self {
        TableRefKind::Empty.into()
    }
    pub fn subquery(statement: SelectStatement) -> Self {
        TableRefKind::Subquery(Box::new(statement)).into()
    }

    fn alias_to_string(&self, column_name_alias: &[String]) -> String {
        let mut r = String::new();
        if !self.alias.is_empty() {
            r += " AS ";
            r += &sql_identifier(&self.alias);
        }
        if !column_name_alias.is_empty() {
            r += &format!("({})", join(column_name_alias, ", ", |c| sql_identifier(c)));
        }
        r
    }

    fn sample_to_string(&self) -> String {
        match &self.sample {
            Some(s) => {
                let mut r = format!(
                    " TABLESAMPLE {}({} {})",
                    s.method.name(),
                    s.sample_size,
                    if s.is_percentage { "PERCENT" } else { "ROWS" }
                );
                if let Some(seed) = s.seed {
                    r += &format!("REPEATABLE ({seed})");
                }
                r
            }
            None => String::new(),
        }
    }

    fn base_to_string(&self, result: String, column_name_alias: &[String]) -> String {
        result + &self.alias_to_string(column_name_alias) + &self.sample_to_string()
    }
}

impl Display for TableRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let text = match &self.kind {
            TableRefKind::Empty => String::new(),
            TableRefKind::BaseTable { name, at_clause } => {
                let mut r = name.to_string() + &self.alias_to_string(&self.column_name_alias);
                if let Some(at) = at_clause {
                    r += &format!(" AT ({} => {})", at.unit, at.expr);
                }
                r + &self.sample_to_string()
            }
            TableRefKind::Join(join) => fmt_join(join),
            TableRefKind::Subquery(statement) => self.base_to_string(format!("({statement})"), &self.column_name_alias),
            TableRefKind::TableFunction {
                function,
                with_ordinality,
            } => {
                let mut r = function.to_string();
                if *with_ordinality {
                    r += " WITH ORDINALITY";
                }
                self.base_to_string(r, &self.column_name_alias)
            }
            TableRefKind::Show(show) => show.to_string(),
            TableRefKind::Pivot(pivot) => {
                let mut r = format!("{pivot}");
                if !self.alias.is_empty() {
                    r += &format!(" AS {}", sql_identifier(&self.alias));
                    if !self.column_name_alias.is_empty() {
                        r += &format!("({})", join(&self.column_name_alias, ", ", |c| sql_identifier(c)));
                    }
                }
                r
            }
            TableRefKind::ExpressionList(values) => {
                let rows = join(values, ", ", |row| format!("({})", join(row, ", ", |e| e.to_string())));
                self.base_to_string(format!("(VALUES {rows})"), &[])
            }
        };
        f.write_str(&text)
    }
}

fn fmt_join(j: &JoinRef) -> String {
    let mut r = String::new();
    if !j.is_implicit {
        r.push('(');
    }
    r += &j.left.to_string();
    r.push(' ');
    let standard = matches!(
        j.join_type,
        JoinType::Inner | JoinType::Left | JoinType::Right | JoinType::Outer | JoinType::Semi | JoinType::Anti
    );
    match j.ref_type {
        JoinRefType::Regular if standard => r += &format!("{} JOIN ", j.join_type.name()),
        JoinRefType::Regular => r += &format!("JOIN BY (TYPE {}) ", j.join_type.name()),
        JoinRefType::Natural => r += &format!("NATURAL {} JOIN ", j.join_type.name()),
        JoinRefType::Asof => r += &format!("ASOF {} JOIN ", j.join_type.name()),
        JoinRefType::Cross => r += if j.is_implicit { ", " } else { "CROSS JOIN " },
        JoinRefType::Positional => r += "POSITIONAL JOIN ",
        JoinRefType::Nearest => r += &format!("{} JOIN ", j.join_type.name()),
    }
    r += &j.right.to_string();
    if j.ref_type == JoinRefType::Nearest {
        r += if j.nearest_approx { " APPROX" } else { " EXACT" };
        r += &format!(" NEAREST {}", j.nearest_count);
        r += if j.nearest_descending {
            " BY SIMILARITY "
        } else {
            " BY DISTANCE "
        };
        if let Some(e) = &j.ranking_expression {
            r += &e.to_string();
        }
        if !j.is_implicit {
            r.push(')');
        }
        return r;
    }
    if let Some(condition) = &j.condition {
        r += &format!(" ON ({condition})");
    } else if !j.using_columns.is_empty() {
        r += &format!(" USING ({})", join(&j.using_columns, ", ", |c| sql_identifier(c)));
    }
    if !j.is_implicit {
        r.push(')');
    }
    r
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultModifier {
    Order(Vec<OrderByNode>),
    Limit {
        limit: Option<Box<Expr>>,
        offset: Option<Box<Expr>>,
        percent: bool,
    },
    Distinct(Vec<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CteMaterialize {
    #[default]
    Default,
    Always,
    Never,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CteInfo {
    pub aliases: Vec<String>,
    pub query: Box<QueryNode>,
    pub key_targets: Vec<Expr>,
    pub materialized: CteMaterialize,
}

/// The WITH clause, in definition order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CteMap(pub Vec<(String, CteInfo)>);

impl Display for CteMap {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return Ok(());
        }
        let recursive = self
            .0
            .iter()
            .any(|(_, cte)| matches!(cte.query.kind, QueryNodeKind::RecursiveCte(_)));
        f.write_str("WITH ")?;
        if recursive {
            f.write_str("RECURSIVE ")?;
        }
        for (i, (name, cte)) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            f.write_str(&sql_identifier(name))?;
            if !cte.aliases.is_empty() {
                write!(f, " ({})", join(&cte.aliases, ", ", |a| sql_identifier(a)))?;
            }
            if !cte.key_targets.is_empty() {
                let keys = join(&cte.key_targets, ", ", |k| {
                    if k.alias.is_empty() {
                        k.to_string()
                    } else {
                        format!("{k} AS {}", sql_identifier(&k.alias))
                    }
                });
                write!(f, " USING KEY ({keys}) ")?;
            }
            match cte.materialized {
                CteMaterialize::Always => f.write_str(" AS MATERIALIZED (")?,
                CteMaterialize::Never => f.write_str(" AS NOT MATERIALIZED (")?,
                CteMaterialize::Default => f.write_str(" AS (")?,
            }
            write!(f, "{})", cte.query)?;
        }
        f.write_str(" ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupByNode {
    pub group_expressions: Vec<Expr>,
    pub grouping_sets: Vec<BTreeSet<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectNode {
    pub select_list: Vec<Expr>,
    pub from_table: TableRef,
    pub where_clause: Option<Box<Expr>>,
    pub groups: GroupByNode,
    pub having: Option<Box<Expr>>,
    pub qualify: Option<Box<Expr>>,
    /// `GROUP BY ALL`.
    pub force_aggregates: bool,
    pub sample: Option<SampleOptions>,
}

impl Default for SelectNode {
    fn default() -> Self {
        Self {
            select_list: Vec::new(),
            from_table: TableRef::empty(),
            where_clause: None,
            groups: GroupByNode::default(),
            having: None,
            qualify: None,
            force_aggregates: false,
            sample: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOpType {
    Union,
    UnionByName,
    Except,
    Intersect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetOperationNode {
    pub setop_type: SetOpType,
    pub setop_all: bool,
    pub children: Vec<QueryNode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveCteNode {
    pub ctename: String,
    pub union_all: bool,
    pub left: Box<QueryNode>,
    pub right: Box<QueryNode>,
    pub aliases: Vec<String>,
    pub key_targets: Vec<Expr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryNodeKind {
    Select(Box<SelectNode>),
    SetOperation(SetOperationNode),
    RecursiveCte(RecursiveCteNode),
    /// INSERT, UPDATE or DELETE as the body of a CTE.
    Insert(Box<InsertStatement>),
    Update(Box<UpdateStatement>),
    Delete(Box<DeleteStatement>),
    Merge(Box<MergeStatement>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryNode {
    pub kind: QueryNodeKind,
    pub modifiers: Vec<ResultModifier>,
    pub cte_map: CteMap,
}

impl From<SelectNode> for QueryNode {
    fn from(node: SelectNode) -> Self {
        QueryNodeKind::Select(Box::new(node)).into()
    }
}

impl From<QueryNodeKind> for QueryNode {
    fn from(kind: QueryNodeKind) -> Self {
        Self {
            kind,
            modifiers: Vec::new(),
            cte_map: CteMap::default(),
        }
    }
}

impl QueryNode {
    fn modifiers_to_string(&self) -> String {
        let mut r = String::new();
        for modifier in &self.modifiers {
            match modifier {
                ResultModifier::Order(orders) => {
                    r += " ORDER BY ";
                    r += &join(orders, ", ", |o| o.to_string());
                }
                ResultModifier::Limit { limit, offset, percent } => {
                    if let Some(limit) = limit {
                        if *percent {
                            r += &format!(" LIMIT ({limit}) %");
                        } else {
                            r += &format!(" LIMIT {limit}");
                        }
                    }
                    if let Some(offset) = offset {
                        r += &format!(" OFFSET {offset}");
                    }
                }
                ResultModifier::Distinct(_) => {}
            }
        }
        r
    }
}

impl Display for QueryNode {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            QueryNodeKind::Select(node) => {
                if let TableRefKind::Show(show) = &node.from_table.kind {
                    return write!(f, "{show}");
                }
                let mut r = self.cte_map.to_string();
                r += "SELECT ";
                for modifier in &self.modifiers {
                    if let ResultModifier::Distinct(on) = modifier {
                        r += "DISTINCT ";
                        if !on.is_empty() {
                            r += &format!("ON ({}) ", join(on, ", ", |e| e.to_string()));
                        }
                    }
                }
                r += &join(&node.select_list, ", ", |e| {
                    if e.alias.is_empty() {
                        e.to_string()
                    } else {
                        format!("{e} AS {}", sql_identifier(&e.alias))
                    }
                });
                if node.from_table.kind != TableRefKind::Empty {
                    r += &format!(" FROM {}", node.from_table);
                }
                if let Some(w) = &node.where_clause {
                    r += &format!(" WHERE {w}");
                }
                let sets = &node.groups.grouping_sets;
                if !sets.is_empty() {
                    r += " GROUP BY ";
                    let multiple = sets.len() > 1;
                    if multiple {
                        r += "GROUPING SETS (";
                    }
                    for (i, set) in sets.iter().enumerate() {
                        if i > 0 {
                            r += ",";
                        }
                        if set.is_empty() {
                            r += "()";
                            continue;
                        }
                        if multiple {
                            r += "(";
                        }
                        r += &set
                            .iter()
                            .map(|&g| node.groups.group_expressions[g].to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        if multiple {
                            r += ")";
                        }
                    }
                    if multiple {
                        r += ")";
                    }
                } else if node.force_aggregates {
                    r += " GROUP BY ALL";
                }
                if let Some(h) = &node.having {
                    r += &format!(" HAVING {h}");
                }
                if let Some(q) = &node.qualify {
                    r += &format!(" QUALIFY {q}");
                }
                if let Some(s) = &node.sample {
                    r += &format!(" USING SAMPLE {}", s.sample_size);
                    if s.is_percentage {
                        r += "%";
                    }
                    r += &format!(" ({}", s.method.name());
                    if let Some(seed) = s.seed {
                        r += &format!(", {seed}");
                    }
                    r += ")";
                }
                write!(f, "{r}{}", self.modifiers_to_string())
            }
            QueryNodeKind::SetOperation(node) => {
                write!(f, "{}({}) ", self.cte_map, node.children[0])?;
                let op = match (node.setop_type, node.setop_all) {
                    (SetOpType::Union, false) => "UNION",
                    (SetOpType::Union, true) => "UNION ALL",
                    (SetOpType::UnionByName, false) => "UNION BY NAME",
                    (SetOpType::UnionByName, true) => "UNION ALL BY NAME",
                    (SetOpType::Except, false) => "EXCEPT",
                    (SetOpType::Except, true) => "EXCEPT ALL",
                    (SetOpType::Intersect, false) => "INTERSECT",
                    (SetOpType::Intersect, true) => "INTERSECT ALL",
                };
                for child in &node.children[1..] {
                    write!(f, "{op} ({child})")?;
                }
                f.write_str(&self.modifiers_to_string())
            }
            QueryNodeKind::RecursiveCte(node) => {
                write!(f, "{}({}) UNION ", self.cte_map, node.left)?;
                if node.union_all {
                    f.write_str("ALL ")?;
                }
                write!(f, "({})", node.right)
            }
            QueryNodeKind::Insert(node) => write!(f, "{node}"),
            QueryNodeKind::Update(node) => write!(f, "{node}"),
            QueryNodeKind::Delete(node) => write!(f, "{node}"),
            QueryNodeKind::Merge(node) => write!(f, "{node}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectStatement {
    pub node: QueryNode,
}

impl Display for SelectStatement {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.node)
    }
}

fn returning_to_string(list: &[Expr]) -> String {
    if list.is_empty() {
        return String::new();
    }
    let cols = join(list, ", ", |e| {
        if e.alias.is_empty() {
            e.to_string()
        } else {
            format!("{e} AS {}", sql_identifier(&e.alias))
        }
    });
    format!(" RETURNING {cols}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnConflictAction {
    Nothing,
    Update,
    /// `INSERT OR REPLACE`.
    Replace,
}

/// `SET col = expr, ...` with an optional WHERE (`UpdateSetInfo`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateSetInfo {
    pub columns: Vec<String>,
    pub expressions: Vec<Expr>,
    pub condition: Option<Box<Expr>>,
}

impl Display for UpdateSetInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let sets: Vec<String> = self
            .columns
            .iter()
            .zip(&self.expressions)
            .map(|(c, e)| format!("{} = {e}", sql_identifier(c)))
            .collect();
        write!(f, "SET {}", sets.join(", "))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnConflictInfo {
    pub action: OnConflictAction,
    pub indexed_columns: Vec<String>,
    pub condition: Option<Box<Expr>>,
    pub set_info: Option<UpdateSetInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertValues {
    Select(Box<SelectStatement>),
    DefaultValues,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertStatement {
    pub cte_map: CteMap,
    pub table: QualifiedName,
    /// The target's alias; like DuckDB, only kept when there is a conflict clause.
    pub table_alias: Option<String>,
    pub by_name: bool,
    pub columns: Vec<String>,
    pub values: InsertValues,
    pub on_conflict: Option<OnConflictInfo>,
    pub returning: Vec<Expr>,
}

/// `InsertQueryNode::GetValuesList`: a plain `SELECT * FROM (VALUES ...)`.
fn values_list(statement: &SelectStatement) -> Option<&Vec<Vec<Expr>>> {
    let node = &statement.node;
    let QueryNodeKind::Select(select) = &node.kind else {
        return None;
    };
    if select.where_clause.is_some() || select.qualify.is_some() || select.having.is_some() {
        return None;
    }
    if !node.cte_map.0.is_empty() || !select.groups.grouping_sets.is_empty() || select.force_aggregates {
        return None;
    }
    if select.select_list.len() != 1 || !matches!(select.select_list[0].kind, ExprKind::Star(_)) {
        return None;
    }
    match &select.from_table.kind {
        TableRefKind::ExpressionList(values) => Some(values),
        _ => None,
    }
}

impl Display for InsertStatement {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut r = self.cte_map.to_string();
        r += "INSERT";
        let or_replace = matches!(&self.on_conflict, Some(c) if c.action == OnConflictAction::Replace);
        if or_replace {
            r += " OR REPLACE";
        }
        r += " INTO ";
        r += &self.table.to_string();
        if let Some(alias) = self.table_alias.as_ref().filter(|a| !a.is_empty()) {
            r += &format!(" AS {}", sql_identifier(alias));
        }
        if self.by_name {
            r += " BY NAME";
        }
        if !self.columns.is_empty() {
            r += &format!(" ({})", join(&self.columns, ", ", |c| sql_identifier(c)));
        }
        r += " ";
        match &self.values {
            InsertValues::Select(select) => match values_list(select) {
                Some(rows) => {
                    let rows = join(rows, ", ", |row| format!("({})", join(row, ", ", |e| e.to_string())));
                    r += &format!("(VALUES {rows})");
                }
                None => r += &select.to_string(),
            },
            InsertValues::DefaultValues => r += "DEFAULT VALUES",
        }
        if let Some(conflict) = self.on_conflict.as_ref().filter(|_| !or_replace) {
            r += " ON CONFLICT ";
            if !conflict.indexed_columns.is_empty() {
                r += &format!("({} )", join(&conflict.indexed_columns, ", ", |c| c.to_lowercase()));
            }
            if let Some(condition) = &conflict.condition {
                r += &format!(" WHERE {condition}");
            }
            r += match conflict.action {
                OnConflictAction::Nothing => " DO NOTHING",
                _ => " DO UPDATE",
            };
            if let Some(set) = &conflict.set_info {
                let sets: Vec<String> = set
                    .columns
                    .iter()
                    .zip(&set.expressions)
                    .map(|(c, e)| format!("{} = {e}", c.to_lowercase()))
                    .collect();
                r += &format!(" SET {}", sets.join(", "));
                if let Some(condition) = &set.condition {
                    r += &format!(" WHERE {condition}");
                }
            }
        }
        r += &returning_to_string(&self.returning);
        f.write_str(&r)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateStatement {
    pub cte_map: CteMap,
    pub table: TableRef,
    pub set_info: UpdateSetInfo,
    pub from_table: Option<TableRef>,
    pub returning: Vec<Expr>,
}

impl Display for UpdateStatement {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}UPDATE {} {}", self.cte_map, self.table, self.set_info)?;
        if let Some(from) = &self.from_table {
            write!(f, " FROM {from}")?;
        }
        if let Some(condition) = &self.set_info.condition {
            write!(f, " WHERE {condition}")?;
        }
        f.write_str(&returning_to_string(&self.returning))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteStatement {
    pub cte_map: CteMap,
    pub table: TableRef,
    pub using: Vec<TableRef>,
    pub condition: Option<Box<Expr>>,
    pub returning: Vec<Expr>,
}

impl Display for DeleteStatement {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}DELETE FROM {}", self.cte_map, self.table)?;
        if !self.using.is_empty() {
            write!(f, " USING {}", join(&self.using, ", ", |t| t.to_string()))?;
        }
        if let Some(condition) = &self.condition {
            write!(f, " WHERE {condition}")?;
        }
        f.write_str(&returning_to_string(&self.returning))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnCreateConflict {
    Error,
    Ignore,
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintCheckMode {
    Default,
    Immediate,
    Deferred,
}

impl ConstraintCheckMode {
    fn suffix(self) -> &'static str {
        match self {
            Self::Default => "",
            Self::Immediate => " IMMEDIATE",
            Self::Deferred => " DEFERRED",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Constraint {
    /// NOT NULL on the column at this index.
    NotNull(usize),
    /// UNIQUE or PRIMARY KEY, on the column at `index` (a column constraint) or on `columns`.
    Unique {
        index: Option<usize>,
        columns: Vec<String>,
        primary_key: bool,
        check_mode: ConstraintCheckMode,
    },
    Check(Box<Expr>),
    ForeignKey {
        fk_columns: Vec<String>,
        pk_columns: Vec<String>,
        schema: String,
        table: String,
    },
}

impl Display for Constraint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Constraint::NotNull(_) => f.write_str("NOT NULL"),
            Constraint::Unique {
                columns,
                primary_key,
                check_mode,
                ..
            } => write!(
                f,
                "{}({}){}",
                if *primary_key { "PRIMARY KEY" } else { "UNIQUE" },
                join(columns, ", ", |c| sql_identifier(c)),
                check_mode.suffix()
            ),
            Constraint::Check(expr) => write!(f, "CHECK({expr})"),
            Constraint::ForeignKey {
                fk_columns,
                pk_columns,
                schema,
                table,
            } => {
                write!(
                    f,
                    "FOREIGN KEY ({}) REFERENCES ",
                    join(fk_columns, ", ", |c| sql_identifier(c))
                )?;
                if !schema.is_empty() && !schema.eq_ignore_ascii_case("main") {
                    write!(f, "{}.", sql_identifier(schema))?;
                }
                f.write_str(&sql_identifier(table))?;
                if !pk_columns.is_empty() {
                    write!(f, "({})", join(pk_columns, ", ", |c| sql_identifier(c)))?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDefinition {
    pub name: String,
    /// `None` for a generated column without a type (`ANY`) or a CREATE TABLE AS column name.
    pub column_type: Option<CastType>,
    /// The expression of a generated column, cast to `column_type` when one is given.
    pub generated: Option<Box<Expr>>,
    pub default_value: Option<Box<Expr>>,
    /// `CompressionTypeToString` of the chosen compression.
    pub compression: Option<&'static str>,
}

impl Display for ColumnDefinition {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{} ", sql_identifier(&self.name))?;
        if let Some(ty) = &self.column_type {
            write!(f, "{ty}")?;
        }
        if let Some(generated) = &self.generated {
            let expr = match (&self.column_type, &generated.kind) {
                (Some(_), ExprKind::Cast { child, .. }) => child,
                _ => generated,
            };
            write!(f, " GENERATED ALWAYS AS({expr})")?;
        } else if let Some(default) = &self.default_value {
            write!(f, " DEFAULT({default})")?;
        }
        if let Some(compression) = self.compression {
            write!(f, " USING COMPRESSION {compression}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTableInfo {
    pub name: QualifiedName,
    pub on_conflict: OnCreateConflict,
    pub temporary: bool,
    pub columns: Vec<ColumnDefinition>,
    pub constraints: Vec<Constraint>,
    /// CREATE TABLE ... AS query.
    pub query: Option<Box<SelectStatement>>,
    pub partition_keys: Vec<Expr>,
    pub sort_keys: Vec<Expr>,
    /// WITH (...) options, in input order.
    pub options: Vec<(String, Expr)>,
}

impl CreateTableInfo {
    /// `TableCatalogEntry::ColumnsToSQL`.
    fn columns_to_sql(&self) -> String {
        let mut not_null = BTreeSet::new();
        let mut unique: Vec<(usize, ConstraintCheckMode)> = Vec::new();
        let mut pk: Vec<(usize, ConstraintCheckMode)> = Vec::new();
        let mut multi_key_pks: Vec<String> = Vec::new();
        let mut extra = Vec::new();
        for constraint in &self.constraints {
            match constraint {
                Constraint::NotNull(i) => {
                    not_null.insert(*i);
                }
                Constraint::Unique {
                    index: Some(i),
                    primary_key,
                    check_mode,
                    ..
                } => {
                    if *primary_key {
                        pk.push((*i, *check_mode));
                    } else {
                        unique.push((*i, *check_mode));
                    }
                }
                Constraint::Unique {
                    columns, primary_key, ..
                } => {
                    if *primary_key {
                        multi_key_pks.extend(columns.iter().cloned());
                    }
                    extra.push(constraint.to_string());
                }
                _ => extra.push(constraint.to_string()),
            }
        }
        let mut r = String::from("(");
        for (i, column) in self.columns.iter().enumerate() {
            if i > 0 {
                r += ", ";
            }
            r += &column.to_string();
            let is_single_pk = pk.iter().any(|(c, _)| *c == i);
            let is_multi_pk = multi_key_pks.iter().any(|c| c.eq_ignore_ascii_case(&column.name));
            if not_null.contains(&i) && !is_single_pk && !is_multi_pk {
                r += " NOT NULL";
            }
            for (_, mode) in pk.iter().filter(|(c, _)| *c == i) {
                r += " PRIMARY KEY";
                r += mode.suffix();
            }
            for (_, mode) in unique.iter().filter(|(c, _)| *c == i) {
                r += " UNIQUE";
                r += mode.suffix();
            }
        }
        for e in extra {
            r += ", ";
            r += &e;
        }
        r + ")"
    }

    fn extra_options_to_string(&self) -> String {
        let mut r = String::new();
        if !self.partition_keys.is_empty() {
            r += &format!(
                " PARTITIONED BY ({})",
                join(&self.partition_keys, ",", |e| e.to_string())
            );
        }
        if !self.sort_keys.is_empty() {
            r += &format!(" SORTED BY ({})", join(&self.sort_keys, ",", |e| e.to_string()));
        }
        if !self.options.is_empty() {
            r += &format!(
                " WITH ({})",
                join(&self.options, ",", |(k, v)| format!("{}={v}", sql_string(k)))
            );
        }
        r
    }
}

impl Display for CreateTableInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut r = String::from("CREATE");
        if self.on_conflict == OnCreateConflict::Replace {
            r += " OR REPLACE";
        }
        if self.temporary {
            r += " TEMP";
        }
        r += " TABLE ";
        if self.on_conflict == OnCreateConflict::Ignore {
            r += " IF NOT EXISTS ";
        }
        let mut name = self.name.clone();
        if self.temporary && name.qualification.len() >= 2 {
            name.qualification.remove(0);
        }
        r += &name.to_string_hide_default_schema();
        match &self.query {
            Some(query) => {
                if !self.columns.is_empty() {
                    r += "(";
                    r += &join(&self.columns, ", ", |c| format!("{} ", sql_identifier(&c.name)));
                    r += ")";
                }
                r += &self.extra_options_to_string();
                r += &format!(" AS {query}");
            }
            None => {
                r += &self.columns_to_sql();
                r += &self.extra_options_to_string();
                r += ";";
            }
        }
        f.write_str(&r)
    }
}

/// `DropInfo`; `entry_type` is `ParseInfo::TypeToString` of the dropped catalog entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropInfo {
    pub entry_type: &'static str,
    pub name: QualifiedName,
    pub if_exists: bool,
    pub cascade: bool,
    /// DROP TRIGGER ... ON table.
    pub trigger_table: Option<Box<TableRef>>,
    /// DROP SECRET ... FROM storage.
    pub secret_storage: String,
    /// DROP [TEMPORARY | PERSISTENT] SECRET.
    pub secret_persist: Option<&'static str>,
}

impl Display for DropInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "DROP {}", self.entry_type)?;
        if self.if_exists {
            f.write_str(" IF EXISTS")?;
        }
        write!(f, " {}", self.name.to_string_hide_default_schema())?;
        if let Some(table) = &self.trigger_table {
            write!(f, " ON {table}")?;
        }
        if !self.secret_storage.is_empty() {
            write!(f, " FROM {}", sql_identifier(&self.secret_storage))?;
        }
        if self.cascade {
            f.write_str(" CASCADE")?;
        }
        f.write_str(";")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionType {
    Begin,
    Commit,
    Rollback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionModifier {
    Default,
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetScope {
    Automatic,
    Local,
    Session,
    Global,
    Variable,
}

impl SetScope {
    fn name(self) -> &'static str {
        match self {
            Self::Automatic => "",
            Self::Local => "LOCAL",
            Self::Session => "SESSION",
            Self::Global => "GLOBAL",
            Self::Variable => "VARIABLE",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PragmaInfo {
    pub name: String,
    pub parameters: Vec<Expr>,
    pub named_parameters: Vec<(String, Expr)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplainType {
    Standard,
    Analyze,
    Sql,
}

/// A transformed SQL statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    Select(SelectStatement),
    Insert(Box<InsertStatement>),
    Update(Box<UpdateStatement>),
    Delete(Box<DeleteStatement>),
    CreateTable(Box<CreateTableInfo>),
    Drop(Box<DropInfo>),
    Transaction {
        transaction_type: TransactionType,
        modifier: TransactionModifier,
    },
    /// CALL of a table function.
    Call(Box<Expr>),
    SetVariable {
        name: String,
        value: Box<Expr>,
        scope: SetScope,
    },
    ResetVariable {
        name: String,
        scope: SetScope,
    },
    Pragma(PragmaInfo),
    Explain {
        statement: Box<Statement>,
        explain_type: ExplainType,
        /// The lowercase output format; "default" when not given.
        format: String,
    },
    /// CREATE of a view, schema, sequence, index, macro or type.
    Create(Box<CreateInfo>),
    Prepare {
        name: String,
        statement: Box<Statement>,
    },
    Execute {
        name: String,
        /// Parameters by name (`1`, `2`, ... for positional ones).
        values: Vec<(String, Expr)>,
    },
    Deallocate(String),
    Detach {
        name: String,
        if_exists: bool,
    },
    Alter(Box<AlterInfo>),
    Copy(Box<CopyInfo>),
    /// COPY FROM DATABASE a TO b (SCHEMA | DATA).
    CopyDatabase {
        from: String,
        to: String,
        schema_only: bool,
    },
    Export {
        database: String,
        info: Box<CopyInfo>,
    },
    Attach(Box<AttachInfo>),
    /// VACUUM and ANALYZE.
    Vacuum {
        analyze: bool,
        table: Option<Box<TableRef>>,
        columns: Vec<String>,
    },
    Load(Box<LoadInfo>),
    Merge(Box<MergeStatement>),
    /// Statements one statement was rewritten into (`MultiStatement`).
    Multi(Vec<Statement>),
}

impl Display for Statement {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Statement::Select(s) => write!(f, "{s}"),
            Statement::Insert(s) => write!(f, "{s}"),
            Statement::Update(s) => write!(f, "{s}"),
            Statement::Delete(s) => write!(f, "{s}"),
            Statement::CreateTable(s) => write!(f, "{s}"),
            Statement::Drop(s) => write!(f, "{s}"),
            Statement::Transaction {
                transaction_type,
                modifier,
            } => {
                f.write_str(match transaction_type {
                    TransactionType::Begin => "BEGIN",
                    TransactionType::Commit => "COMMIT",
                    TransactionType::Rollback => "ROLLBACK",
                })?;
                f.write_str(match modifier {
                    TransactionModifier::Default => "",
                    TransactionModifier::ReadOnly => " READ ONLY",
                    TransactionModifier::ReadWrite => " READ WRITE",
                })?;
                f.write_str(";")
            }
            Statement::Call(function) => write!(f, "CALL {function};"),
            Statement::SetVariable { name, value, scope } => write!(f, "SET {} {name} TO {value};", scope.name()),
            Statement::ResetVariable { name, scope } => write!(f, "RESET {} {name};", scope.name()),
            Statement::Pragma(info) => {
                write!(f, "PRAGMA {}", sql_identifier(&info.name))?;
                if !info.parameters.is_empty() {
                    write!(f, "({})", join(&info.parameters, ", ", |p| p.to_string()))?;
                }
                f.write_str(";")
            }
            Statement::Create(info) => write!(f, "{info}"),
            Statement::Prepare { name, statement } => write!(f, "PREPARE {} AS {statement}", sql_identifier(name)),
            Statement::Execute { name, values } => {
                write!(f, "EXECUTE {}", sql_identifier(name))?;
                if !values.is_empty() {
                    let values = join(values, ", ", |(n, v)| format!("{} := {v}", sql_identifier(n)));
                    write!(f, "({values})")?;
                }
                f.write_str(";")
            }
            Statement::Deallocate(name) => write!(f, "DEALLOCATE PREPARE {};", sql_identifier(name)),
            Statement::Alter(info) => write!(f, "{info}"),
            Statement::Copy(info) => write!(f, "{info}"),
            Statement::CopyDatabase { from, to, schema_only } => write!(
                f,
                "COPY FROM DATABASE {} TO {} ({})",
                sql_identifier(from),
                sql_identifier(to),
                if *schema_only { "SCHEMA" } else { "DATA" }
            ),
            Statement::Export { database, info } => {
                f.write_str("EXPORT DATABASE")?;
                if !database.is_empty() {
                    write!(f, " {} TO", sql_identifier(database))?;
                }
                write!(f, " {}{};", sql_string(&info.file_path), info.options_to_string())
            }
            Statement::Attach(info) => write!(f, "{info}"),
            Statement::Vacuum {
                analyze,
                table,
                columns,
            } => {
                f.write_str("VACUUM")?;
                if *analyze {
                    f.write_str(" ANALYZE")?;
                }
                if let Some(table) = table {
                    write!(f, " {table}")?;
                    if !columns.is_empty() {
                        write!(f, "({})", join(columns, ", ", |c| sql_identifier(c)))?;
                    }
                }
                f.write_str(";")
            }
            Statement::Load(info) => write!(f, "{info}"),
            Statement::Merge(merge) => write!(f, "{merge}"),
            Statement::Multi(statements) => {
                write!(f, "{};", join(statements, ";", |s| s.to_string()))
            }
            Statement::Detach { name, if_exists } => {
                f.write_str("DETACH DATABASE")?;
                if *if_exists {
                    f.write_str(" IF EXISTS")?;
                }
                write!(f, " {};", sql_identifier(name))
            }
            Statement::Explain {
                statement,
                explain_type,
                format,
            } => {
                f.write_str("EXPLAIN")?;
                let options = match explain_type {
                    ExplainType::Sql => "(SQL)".to_string(),
                    _ => {
                        let mut parts = Vec::new();
                        if *explain_type == ExplainType::Analyze {
                            parts.push("ANALYZE".to_string());
                        }
                        if format != "default" {
                            parts.push(format!("FORMAT {}", format.to_uppercase()));
                        }
                        if parts.is_empty() {
                            String::new()
                        } else {
                            format!("({})", parts.join(", "))
                        }
                    }
                };
                if !options.is_empty() {
                    write!(f, " {options}")?;
                }
                write!(f, " {statement}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroParameter {
    pub name: String,
    pub parameter_type: Option<CastType>,
    pub default: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacroBody {
    Scalar(Box<Expr>),
    Table(Box<QueryNode>),
}

/// One overload of a macro (`MacroFunction`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroFunction {
    pub parameters: Vec<MacroParameter>,
    pub return_type: Option<CastType>,
    pub body: MacroBody,
}

impl Display for MacroFunction {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let params = join(&self.parameters, ", ", |p| {
            let mut s = sql_identifier(&p.name);
            if let Some(ty) = &p.parameter_type {
                s += &format!(" {ty}");
            }
            if let Some(default) = &p.default {
                s += &format!(" := {default}");
            }
            s
        });
        write!(f, "({params})")?;
        if let Some(ty) = &self.return_type {
            write!(f, " RETURNS {ty}")?;
        }
        f.write_str(" AS ")?;
        match &self.body {
            MacroBody::Scalar(expr) => write!(f, "({expr})"),
            MacroBody::Table(query) => write!(f, "TABLE ({query})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateTypeKind {
    Enum(Vec<String>),
    /// `ENUM (SELECT ...)`.
    EnumQuery(Box<SelectStatement>),
    Alias(CastType),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateKind {
    View {
        secure: bool,
        aliases: Vec<String>,
        defer_binding: bool,
        query: Box<SelectStatement>,
    },
    /// The schema's full path is `name.qualification` followed by `name.name`.
    Schema {
        options: Vec<(String, Expr)>,
    },
    Sequence {
        increment: i64,
        min_value: i64,
        max_value: i64,
        start_value: i64,
        cycle: bool,
    },
    /// `name` holds the table's qualification and the index name.
    Index {
        table: String,
        unique: bool,
        index_type: String,
        expressions: Vec<Expr>,
        options: Vec<(String, Literal)>,
    },
    Macro {
        table_macro: bool,
        macros: Vec<MacroFunction>,
    },
    Type(CreateTypeKind),
    /// `name.name` is the trigger name.
    Trigger {
        timing: &'static str,
        event: &'static str,
        columns: Vec<String>,
        table: Box<TableRef>,
        referencing_new: String,
        referencing_old: String,
        for_each: &'static str,
        action: Box<QueryNode>,
    },
    /// `name.name` is the secret name.
    Secret {
        persistent: bool,
        storage: String,
        provider: Option<Box<Expr>>,
        secret_type: Option<Box<Expr>>,
        scope: Option<Box<Expr>>,
        options: Vec<(String, Expr)>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateInfo {
    pub name: QualifiedName,
    pub on_conflict: OnCreateConflict,
    pub temporary: bool,
    pub kind: CreateKind,
}

impl CreateInfo {
    /// `CreateInfo::GetCreatePrefix`.
    fn prefix(&self, entry: &str) -> String {
        let mut r = String::from("CREATE");
        if self.on_conflict == OnCreateConflict::Replace {
            r += " OR REPLACE";
        }
        if self.temporary {
            r += " TEMP";
        }
        r += &format!(" {entry} ");
        if self.on_conflict == OnCreateConflict::Ignore {
            r += " IF NOT EXISTS ";
        }
        r
    }

    /// `CreateInfo::QualifiedNameToString`: temporary entries leave out the catalog.
    fn qualified_name(&self) -> String {
        let mut name = self.name.clone();
        if self.temporary && name.qualification.len() >= 2 {
            name.qualification.remove(0);
        }
        name.to_string_hide_default_schema()
    }
}

/// `CreateIndexInfo::ExpressionsToList`: drops the table qualification the transformer added.
fn index_expression(expr: &Expr, table: &str) -> String {
    match &expr.kind {
        ExprKind::ColumnRef(names) if names.len() == 2 && names[0].eq_ignore_ascii_case(table) => {
            sql_identifier(&names[1])
        }
        ExprKind::ColumnRef(names) if names.len() == 1 => expr.to_string(),
        _ => format!("({expr})"),
    }
}

impl Display for CreateInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            CreateKind::View {
                secure,
                aliases,
                defer_binding,
                query,
            } => {
                f.write_str(&self.prefix(if *secure { "SECURE VIEW" } else { "VIEW" }))?;
                f.write_str(&self.qualified_name())?;
                if !aliases.is_empty() {
                    write!(f, " ({})", join(aliases, ", ", |a| sql_identifier(a)))?;
                }
                if *defer_binding {
                    f.write_str(" WITH (DEFER_BINDING)")?;
                }
                write!(f, " AS {query};")
            }
            CreateKind::Schema { options } => {
                let mut path = self.name.qualification.clone();
                path.push(self.name.name.clone());
                let mut qualified = QualifiedName::from_parts(path).to_string();
                if !options.is_empty() {
                    let opts = join(options, ", ", |(k, v)| format!("{}={v}", sql_string(k)));
                    qualified += &format!(" WITH ({opts})");
                }
                let temp = if self.temporary { "TEMPORARY " } else { "" };
                match self.on_conflict {
                    OnCreateConflict::Ignore => write!(f, "CREATE {temp}SCHEMA IF NOT EXISTS {qualified};"),
                    OnCreateConflict::Replace => write!(f, "CREATE OR REPLACE {temp}SCHEMA {qualified};"),
                    OnCreateConflict::Error => write!(f, "CREATE {temp}SCHEMA {qualified};"),
                }
            }
            CreateKind::Sequence {
                increment,
                min_value,
                max_value,
                start_value,
                cycle,
            } => {
                f.write_str("CREATE")?;
                if self.on_conflict == OnCreateConflict::Replace {
                    f.write_str(" OR REPLACE")?;
                }
                if self.temporary {
                    f.write_str(" TEMPORARY")?;
                }
                f.write_str(" SEQUENCE ")?;
                if self.on_conflict == OnCreateConflict::Ignore {
                    f.write_str(" IF NOT EXISTS ")?;
                }
                write!(
                    f,
                    "{} INCREMENT BY {increment} MINVALUE {min_value} MAXVALUE {max_value} START {start_value} {};",
                    self.qualified_name(),
                    if *cycle { "CYCLE" } else { "NO CYCLE" }
                )
            }
            CreateKind::Index {
                table,
                unique,
                index_type,
                expressions,
                options,
            } => {
                f.write_str("CREATE")?;
                if *unique {
                    f.write_str(" UNIQUE")?;
                }
                f.write_str(" INDEX ")?;
                if self.on_conflict == OnCreateConflict::Ignore {
                    f.write_str("IF NOT EXISTS ")?;
                }
                write!(f, "{} ON ", sql_identifier(&self.name.name))?;
                let mut table_name = QualifiedName {
                    qualification: self.name.qualification.clone(),
                    name: table.clone(),
                };
                if self.temporary && table_name.qualification.len() >= 2 {
                    table_name.qualification.remove(0);
                }
                f.write_str(&table_name.to_string_hide_default_schema())?;
                if index_type != "ART" {
                    write!(f, " USING {} ", sql_identifier(index_type))?;
                }
                write!(f, "({})", join(expressions, ", ", |e| index_expression(e, table)))?;
                if !options.is_empty() {
                    let opts = join(options, ", ", |(k, v)| {
                        if v.kind == LiteralKind::Null {
                            sql_identifier(k)
                        } else {
                            format!("{} = {v}", sql_identifier(k))
                        }
                    });
                    write!(f, " WITH ({opts} )")?;
                }
                f.write_str(";")
            }
            CreateKind::Macro { macros, .. } => {
                f.write_str(&self.prefix("MACRO"))?;
                write!(
                    f,
                    "{} {};",
                    self.qualified_name(),
                    join(macros, ", ", |m| m.to_string())
                )
            }
            CreateKind::Trigger {
                timing,
                event,
                columns,
                table,
                referencing_new,
                referencing_old,
                for_each,
                action,
            } => {
                f.write_str("CREATE")?;
                if self.on_conflict == OnCreateConflict::Replace {
                    f.write_str(" OR REPLACE")?;
                }
                f.write_str(" TRIGGER ")?;
                if self.on_conflict == OnCreateConflict::Ignore {
                    f.write_str("IF NOT EXISTS ")?;
                }
                if let Some(schema) = self.name.qualification.last() {
                    write!(f, "{}.", sql_identifier(schema))?;
                }
                write!(f, "{} {timing} {event}", sql_identifier(&self.name.name))?;
                if *event == "UPDATE" && !columns.is_empty() {
                    write!(f, " OF {}", join(columns, ", ", |c| sql_identifier(c)))?;
                }
                write!(f, " ON {table}")?;
                if !referencing_new.is_empty() || !referencing_old.is_empty() {
                    f.write_str(" REFERENCING")?;
                    if !referencing_new.is_empty() {
                        write!(f, " NEW TABLE AS {}", sql_identifier(referencing_new))?;
                    }
                    if !referencing_old.is_empty() {
                        write!(f, " OLD TABLE AS {}", sql_identifier(referencing_old))?;
                    }
                }
                write!(f, " FOR EACH {for_each} {action};")
            }
            CreateKind::Secret {
                persistent,
                storage,
                provider,
                secret_type,
                scope,
                options,
            } => {
                f.write_str(&self.prefix(if *persistent { "PERSISTENT SECRET" } else { "SECRET" }))?;
                if !self.name.name.is_empty() {
                    write!(f, " {}", sql_identifier(&self.name.name))?;
                }
                if !storage.is_empty() {
                    write!(f, " IN{}", sql_identifier(storage))?;
                }
                let mut list = Vec::new();
                if let Some(p) = provider {
                    list.push(format!("PROVIDER {p}"));
                }
                if let Some(t) = secret_type {
                    list.push(format!("TYPE {t}"));
                }
                if let Some(s) = scope {
                    list.push(format!("SCOPE {s}"));
                }
                list.extend(options.iter().map(|(k, v)| format!("{k} {v}")));
                write!(f, "({})", list.join(", "))
            }
            CreateKind::Type(kind) => {
                f.write_str(&self.prefix("TYPE"))?;
                f.write_str(&self.qualified_name())?;
                match kind {
                    CreateTypeKind::Enum(values) => {
                        write!(f, " AS ENUM ( {} )", join(values, ", ", |v| sql_string(v)))?
                    }
                    CreateTypeKind::EnumQuery(query) => write!(f, " AS ENUM ({query})")?,
                    CreateTypeKind::Alias(ty) => write!(f, " AS {ty}")?,
                }
                f.write_str(";")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowType {
    Show,
    Describe,
    Summary,
    ShowFrom,
    /// A form the binder handles specially: all tables, or `"databases"` and the like.
    ShowSpecial,
}

/// `ShowRef`: what SHOW, DESCRIBE or SUMMARIZE look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowRef {
    pub show_type: ShowType,
    /// The named table or setting; special forms keep their name pre-quoted (e.g. `"tables"`).
    pub name: Option<QualifiedName>,
    /// SHOW TABLES FROM [catalog.]schema.
    pub catalog: String,
    pub schema: String,
    /// The query whose columns are described.
    pub query: Option<Box<QueryNode>>,
}

impl Display for ShowRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let name = self.name.as_ref().filter(|n| !n.name.is_empty());
        if self.show_type == ShowType::Show {
            f.write_str("SHOW ")?;
            if let Some(name) = name {
                return write!(f, "{name}");
            }
            if let Some(query) = &self.query {
                return write!(f, "({query})");
            }
            return Ok(());
        }
        match self.show_type {
            ShowType::Summary => f.write_str("SUMMARIZE ")?,
            ShowType::ShowFrom => {
                f.write_str("SHOW TABLES FROM ")?;
                if !self.catalog.is_empty() {
                    f.write_str(&sql_identifier(&self.catalog))?;
                    if !self.schema.is_empty() {
                        f.write_str(".")?;
                    }
                }
                f.write_str(&sql_identifier(&self.schema))?;
            }
            _ => f.write_str("DESCRIBE ")?,
        }
        if let Some(query) = &self.query {
            write!(f, "({query})")
        } else if self.show_type == ShowType::ShowSpecial {
            match name {
                Some(n) if n.name != "__show_tables_expanded" => f.write_str(&n.name),
                _ => Ok(()),
            }
        } else if let Some(name) = name {
            write!(f, "{name}")
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlterKind {
    RenameTable(String),
    RenameView(String),
    /// RENAME COLUMN of a column or a nested field.
    RenameColumn {
        path: Vec<String>,
        new_name: String,
    },
    AddColumn {
        column: ColumnDefinition,
        if_not_exists: bool,
        not_null: bool,
        unique: bool,
    },
    /// ADD COLUMN of a nested field below `parent`.
    AddField {
        parent: Vec<String>,
        column: ColumnDefinition,
        if_not_exists: bool,
    },
    RemoveColumn {
        path: Vec<String>,
        if_exists: bool,
        cascade: bool,
    },
    ChangeColumnType {
        path: Vec<String>,
        target: Option<CastType>,
        using: Option<Box<Expr>>,
    },
    /// SET DEFAULT, or DROP DEFAULT when `None`.
    SetDefault {
        path: Vec<String>,
        expression: Option<Box<Expr>>,
    },
    SetNotNull(Vec<String>),
    DropNotNull(Vec<String>),
    AddConstraint(Constraint),
    /// Empty to reset.
    SetPartitionedBy(Vec<Expr>),
    SetSortedBy(Vec<OrderByNode>),
    SetTableOptions(Vec<(String, Expr)>),
    ResetTableOptions(Vec<String>),
    /// The schema path is `name.qualification` plus `name.name`.
    SetSchemaOptions(Vec<(String, Expr)>),
    ResetSchemaOptions(Vec<String>),
    /// `name.name` is the catalog.
    RenameDatabase(String),
    /// ALTER SEQUENCE ... OWNED BY schema.name.
    ChangeOwnership {
        owner_schema: String,
        owner_name: String,
    },
    /// COMMENT ON <entry_type> name IS value.
    SetComment {
        entry_type: &'static str,
        value: Literal,
    },
    /// COMMENT ON COLUMN table.column IS value.
    SetColumnComment {
        column: String,
        value: Literal,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterInfo {
    pub name: QualifiedName,
    pub if_exists: bool,
    pub kind: AlterKind,
}

fn path_to_string(path: &[String]) -> String {
    join(path, ".", |p| sql_identifier(p))
}

fn options_to_string(options: &[(String, Expr)]) -> String {
    join(options, ", ", |(k, v)| format!("{}={v}", sql_string(k)))
}

impl Display for AlterInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let if_exists = if self.if_exists { "IF EXISTS " } else { "" };
        let table = format!("ALTER TABLE {if_exists}{}", self.name.to_string_hide_default_schema());
        match &self.kind {
            AlterKind::RenameTable(new_name) => write!(f, "{table} RENAME TO {};", sql_identifier(new_name)),
            AlterKind::RenameView(new_name) => write!(
                f,
                "ALTER VIEW {if_exists}{} RENAME TO {};",
                self.name.to_string_hide_default_schema(),
                sql_identifier(new_name)
            ),
            AlterKind::RenameColumn { path, new_name } => write!(
                f,
                "{table} RENAME COLUMN {} TO {};",
                path_to_string(path),
                sql_identifier(new_name)
            ),
            AlterKind::AddColumn {
                column,
                if_not_exists,
                not_null,
                unique,
            } => {
                write!(f, "{table} ADD COLUMN")?;
                if *if_not_exists {
                    f.write_str(" IF NOT EXISTS")?;
                }
                write!(f, " {} ", sql_identifier(&column.name))?;
                if let Some(ty) = &column.column_type {
                    write!(f, "{ty}")?;
                }
                if *not_null {
                    f.write_str(" NOT NULL")?;
                }
                if *unique {
                    f.write_str(" UNIQUE")?;
                }
                if let Some(default) = &column.default_value {
                    write!(f, " DEFAULT {default}")?;
                }
                if let Some(compression) = column.compression {
                    write!(f, " USING COMPRESSION {compression}")?;
                }
                f.write_str(";")
            }
            AlterKind::AddField {
                parent,
                column,
                if_not_exists,
            } => {
                write!(f, "{table} ADD COLUMN ")?;
                if *if_not_exists {
                    f.write_str("IF NOT EXISTS ")?;
                }
                for p in parent {
                    write!(f, "{}.", sql_identifier(p))?;
                }
                write!(f, "{column};")
            }
            AlterKind::RemoveColumn {
                path,
                if_exists: column_if_exists,
                cascade,
            } => {
                write!(f, "{table} DROP COLUMN ")?;
                if *column_if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                f.write_str(&path_to_string(path))?;
                if *cascade {
                    f.write_str(" CASCADE")?;
                }
                f.write_str(";")
            }
            AlterKind::ChangeColumnType { path, target, using } => {
                write!(f, "{table} ALTER COLUMN {} TYPE ", path_to_string(path))?;
                if let Some(ty) = target {
                    write!(f, "{ty}")?;
                }
                if let Some(using) = using {
                    write!(f, " USING {using}")?;
                }
                f.write_str(";")
            }
            AlterKind::SetDefault { path, expression } => {
                write!(f, "{table} ALTER COLUMN {}", path_to_string(path))?;
                match expression {
                    Some(e) => write!(f, " SET DEFAULT {e};"),
                    None => f.write_str(" DROP DEFAULT;"),
                }
            }
            AlterKind::SetNotNull(path) => write!(f, "{table} ALTER COLUMN {} SET NOT NULL;", path_to_string(path)),
            AlterKind::DropNotNull(path) => write!(f, "{table} ALTER COLUMN {} DROP NOT NULL;", path_to_string(path)),
            AlterKind::AddConstraint(constraint) => write!(f, "{table} ADD {constraint};"),
            AlterKind::SetPartitionedBy(keys) if keys.is_empty() => write!(f, "{table} RESET PARTITIONED BY"),
            AlterKind::SetPartitionedBy(keys) => {
                write!(
                    f,
                    "{table} SET PARTITIONED BY ({})",
                    join(keys, ", ", |k| k.to_string())
                )
            }
            AlterKind::SetSortedBy(orders) if orders.is_empty() => write!(f, "{table} RESET SORTED BY"),
            AlterKind::SetSortedBy(orders) => {
                write!(f, "{table} SET SORTED BY ({})", join(orders, ", ", |o| o.to_string()))
            }
            AlterKind::SetTableOptions(options) => write!(f, "{table} SET ({})", options_to_string(options)),
            AlterKind::ResetTableOptions(names) => {
                write!(f, "{table} RESET ({})", join(names, ", ", |n| sql_string(n)))
            }
            AlterKind::SetSchemaOptions(_) | AlterKind::ResetSchemaOptions(_) => {
                let mut path = self.name.qualification.clone();
                path.push(self.name.name.clone());
                write!(f, "ALTER SCHEMA {if_exists}{}", path_to_string(&path))?;
                match &self.kind {
                    AlterKind::SetSchemaOptions(options) => write!(f, " SET ({})", options_to_string(options)),
                    AlterKind::ResetSchemaOptions(names) => {
                        write!(f, " RESET ({})", join(names, ", ", |n| sql_string(n)))
                    }
                    _ => unreachable!(),
                }
            }
            AlterKind::RenameDatabase(new_name) => write!(
                f,
                "ALTER DATABASE {if_exists}{} SET ALIAS TO {}",
                sql_identifier(&self.name.name),
                sql_identifier(new_name)
            ),
            AlterKind::ChangeOwnership {
                owner_schema,
                owner_name,
            } => {
                let owner = QualifiedName {
                    qualification: vec![owner_schema.clone()],
                    name: owner_name.clone(),
                };
                write!(
                    f,
                    "ALTER SEQUENCE {if_exists}{} OWNED BY {};",
                    self.name.to_string_hide_default_schema(),
                    owner.to_string_hide_default_schema()
                )
            }
            AlterKind::SetComment { entry_type, value } => write!(
                f,
                "COMMENT ON {entry_type} {} IS {value};",
                self.name.to_string_hide_default_schema()
            ),
            AlterKind::SetColumnComment { column, value } => write!(
                f,
                "COMMENT ON COLUMN {}.{} IS {value};",
                self.name.to_string_hide_default_schema(),
                sql_identifier(column)
            ),
        }
    }
}

/// `Value::ToSQLString` of a literal's value.
pub fn literal_to_sql(literal: &Literal) -> String {
    match literal.kind {
        LiteralKind::Integer => {
            let digits: String = literal.text.chars().filter(|&c| c != '_').collect();
            match digits.parse::<i128>() {
                Ok(v) => v.to_string(),
                Err(_) => digits,
            }
        }
        LiteralKind::Numeric => {
            let digits: String = literal.text.chars().filter(|&c| c != '_').collect();
            if digits.contains(['e', 'E']) {
                return digits
                    .parse::<f64>()
                    .ok()
                    .and_then(|v| duckrs_common::value::Value::double(v).try_to_string().ok())
                    .unwrap_or(digits);
            }
            let (negative, unsigned) = match digits.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, digits.as_str()),
            };
            let (int, frac) = unsigned.split_once('.').unwrap_or((unsigned, ""));
            let int = int.trim_start_matches('0');
            let mut out = String::new();
            if negative {
                out.push('-');
            }
            out += if int.is_empty() { "0" } else { int };
            if !frac.is_empty() {
                out.push('.');
                out += frac;
            }
            out
        }
        LiteralKind::Hex => {
            let bytes: String = literal
                .text
                .as_bytes()
                .chunks(2)
                .map(|pair| format!("\\x{}", String::from_utf8_lossy(pair).to_uppercase()))
                .collect();
            format!("'{bytes}'::BLOB")
        }
        LiteralKind::Bit => format!("'{}'::BIT", literal.text),
        _ => literal.to_string(),
    }
}

/// `CopyInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CopyInfo {
    pub table: QualifiedName,
    pub select_list: Vec<String>,
    /// COPY (query) TO ...
    pub select: Option<Box<QueryNode>>,
    pub is_from: bool,
    pub file_path: String,
    pub file_path_expression: Option<Box<Expr>>,
    /// The format; rendered only when given explicitly.
    pub format: String,
    pub format_given: bool,
    /// Options kept as expressions (`None` for a bare flag like HEADER).
    pub parsed_options: Vec<(String, Option<Expr>)>,
    pub options: Vec<(String, Vec<Literal>)>,
}

impl CopyInfo {
    /// `CopyInfo::CopyOptionsToString`: options in sorted order.
    pub fn options_to_string(&self) -> String {
        if !self.format_given && self.options.is_empty() && self.parsed_options.is_empty() {
            return String::new();
        }
        let mut parts = Vec::new();
        if self.format_given && !self.format.is_empty() {
            parts.push(format!(" FORMAT {}", sql_string(&self.format)));
        }
        let mut parsed: Vec<_> = self.parsed_options.iter().collect();
        parsed.sort_by_key(|(name, _)| name.to_lowercase());
        for (name, expr) in parsed {
            match expr {
                Some(e) => parts.push(format!("{name} {e}")),
                None => parts.push(name.clone()),
            }
        }
        let mut options: Vec<_> = self.options.iter().collect();
        options.sort_by_key(|(name, _)| name.to_lowercase());
        for (name, values) in options {
            parts.push(match values.len() {
                0 => format!("{name} "),
                1 => format!("{name} {}", literal_to_sql(&values[0])),
                _ => format!("{name} ( {} )", join(values, ", ", literal_to_sql)),
            });
        }
        format!(" ({} )", parts.join(", "))
    }

    fn table_part(&self) -> String {
        let mut r = self.table.to_string_hide_default_schema();
        if !self.select_list.is_empty() {
            r += &format!(" ({} )", join(&self.select_list, ", ", |c| sql_identifier(c)));
        }
        r
    }
}

impl Display for CopyInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("COPY ")?;
        if self.is_from {
            write!(f, "{} FROM", self.table_part())?;
        } else {
            match &self.select {
                Some(query) => write!(f, "({query})")?,
                None => f.write_str(&self.table_part())?,
            }
            f.write_str(" TO ")?;
        }
        match &self.file_path_expression {
            Some(e) if self.file_path.is_empty() => write!(f, "({e})")?,
            _ => write!(f, " {}", sql_string(&self.file_path))?,
        }
        write!(f, "{};", self.options_to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachInfo {
    pub on_conflict: OnCreateConflict,
    pub path: Box<Expr>,
    pub name: String,
    pub parsed_options: Vec<(String, Expr)>,
    pub options: Vec<(String, Literal)>,
}

impl Display for AttachInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("ATTACH")?;
        match self.on_conflict {
            OnCreateConflict::Ignore => f.write_str(" IF NOT EXISTS")?,
            OnCreateConflict::Replace => f.write_str(" OR REPLACE")?,
            OnCreateConflict::Error => {}
        }
        write!(f, " DATABASE {}", self.path)?;
        if !self.name.is_empty() {
            write!(f, " AS {}", sql_identifier(&self.name))?;
        }
        if !self.parsed_options.is_empty() || !self.options.is_empty() {
            let mut parts: Vec<String> = self
                .parsed_options
                .iter()
                .map(|(n, e)| format!("{} {e}", sql_identifier(n)))
                .collect();
            parts.extend(
                self.options
                    .iter()
                    .map(|(n, v)| format!("{} {}", sql_identifier(n), literal_to_sql(v))),
            );
            write!(f, " ({})", parts.join(", "))?;
        }
        f.write_str(";")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadType {
    Load,
    Install,
    ForceInstall,
    CreateRepository,
    DropRepository,
}

/// `LoadInfo`: LOAD, INSTALL and extension repositories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadInfo {
    pub load_type: LoadType,
    pub load_after_install: bool,
    pub filename: String,
    pub repository: String,
    pub repo_is_alias: bool,
    pub alias: String,
    pub version: String,
    pub repository_url: String,
    pub public_keys: Vec<String>,
    pub on_conflict: OnCreateConflict,
    pub missing_ok: bool,
}

impl Display for LoadInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.load_type {
            LoadType::CreateRepository => {
                f.write_str("CREATE ")?;
                if self.on_conflict == OnCreateConflict::Replace {
                    f.write_str("OR REPLACE ")?;
                }
                f.write_str("EXTENSION REPOSITORY ")?;
                if self.on_conflict == OnCreateConflict::Ignore {
                    f.write_str("IF NOT EXISTS ")?;
                }
                write!(
                    f,
                    "{} WITH PREFIX {}",
                    sql_identifier(&self.repository),
                    sql_string(&self.repository_url)
                )?;
                if !self.public_keys.is_empty() {
                    let keyword = if self.public_keys.len() > 1 { "KEYS" } else { "KEY" };
                    write!(
                        f,
                        " USING PUBLIC {keyword} {}",
                        join(&self.public_keys, ", ", |k| sql_string(k))
                    )?;
                }
                return f.write_str(";");
            }
            LoadType::DropRepository => {
                f.write_str("DROP EXTENSION REPOSITORY ")?;
                if self.missing_ok {
                    f.write_str("IF EXISTS ")?;
                }
                return write!(f, "{};", sql_identifier(&self.repository));
            }
            LoadType::Load => f.write_str("LOAD")?,
            LoadType::Install | LoadType::ForceInstall => {
                if self.load_type == LoadType::ForceInstall {
                    f.write_str("FORCE ")?;
                }
                f.write_str("INSTALL")?;
                if self.load_after_install {
                    f.write_str(" AND LOAD")?;
                }
            }
        }
        write!(f, " {}", sql_string(&self.filename))?;
        if !self.repository.is_empty() {
            if self.repo_is_alias {
                write!(f, " FROM {}", sql_identifier(&self.repository))?;
            } else {
                write!(f, " FROM {}", sql_string(&self.repository))?;
            }
        }
        if !self.alias.is_empty() {
            write!(f, " AS {}", sql_identifier(&self.alias))?;
        }
        f.write_str(";")
    }
}

/// An entry of a PIVOT IN list: constant values (from column names or a row of them), or an
/// expression kept whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PivotColumnEntry {
    pub values: Vec<Literal>,
    pub expr: Option<Expr>,
    pub alias: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PivotColumn {
    pub pivot_expressions: Vec<Expr>,
    pub unpivot_names: Vec<String>,
    pub entries: Vec<PivotColumnEntry>,
    /// The ENUM type that lists the values, when the IN list is not written out.
    pub pivot_enum: String,
    pub subquery: Option<Box<QueryNode>>,
}

impl Display for PivotColumn {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if !self.unpivot_names.is_empty() {
            if self.unpivot_names.len() == 1 {
                f.write_str(&sql_identifier(&self.unpivot_names[0]))?;
            } else {
                write!(f, "({})", join(&self.unpivot_names, ", ", |n| sql_identifier(n)))?;
            }
        } else if !self.pivot_expressions.is_empty() {
            write!(f, "({})", join(&self.pivot_expressions, ", ", |e| e.to_string()))?;
        }
        f.write_str(" IN ")?;
        if !self.pivot_enum.is_empty() {
            return f.write_str(&sql_identifier(&self.pivot_enum));
        }
        let entries = join(&self.entries, ", ", |entry| {
            let mut r = match &entry.expr {
                Some(e) => e.to_string(),
                None if entry.values.len() == 1 => literal_to_sql(&entry.values[0]),
                None => format!("({})", join(&entry.values, ", ", literal_to_sql)),
            };
            if !entry.alias.is_empty() {
                r += &format!(" AS {}", sql_identifier(&entry.alias));
            }
            r
        });
        write!(f, "({entries})")
    }
}

/// `PivotRef`: PIVOT when `aggregates` is non-empty, UNPIVOT otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PivotRef {
    pub source: Box<TableRef>,
    pub aggregates: Vec<Expr>,
    pub unpivot_names: Vec<String>,
    pub pivots: Vec<PivotColumn>,
    pub groups: Vec<String>,
    pub include_nulls: bool,
}

impl Display for PivotRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.source)?;
        if !self.aggregates.is_empty() {
            let aggregates = join(&self.aggregates, ", ", |a| {
                if a.alias.is_empty() {
                    a.to_string()
                } else {
                    format!("{a} AS {}", sql_identifier(&a.alias))
                }
            });
            write!(f, " PIVOT ({aggregates}")?;
        } else {
            f.write_str(" UNPIVOT ")?;
            if self.include_nulls {
                f.write_str("INCLUDE NULLS ")?;
            }
            if self.unpivot_names.len() == 1 {
                write!(f, "({}", sql_identifier(&self.unpivot_names[0]))?;
            } else {
                write!(f, "(({})", join(&self.unpivot_names, ", ", |n| sql_identifier(n)))?;
            }
        }
        f.write_str(" FOR")?;
        for pivot in &self.pivots {
            write!(f, " {pivot}")?;
        }
        if !self.groups.is_empty() {
            write!(f, " GROUP BY {}", join(&self.groups, ", ", |g| sql_identifier(g)))?;
        }
        f.write_str(")")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MergeCondition {
    WhenMatched,
    WhenNotMatchedBySource,
    WhenNotMatchedByTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeActionType {
    Update,
    Delete,
    Insert,
    DoNothing,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeAction {
    pub action_type: MergeActionType,
    pub condition: Option<Box<Expr>>,
    /// UPDATE SET ...; `None` for `UPDATE SET *` or a bare UPDATE.
    pub update_info: Option<UpdateSetInfo>,
    pub by_name: bool,
    pub insert_columns: Vec<String>,
    /// INSERT VALUES (...) or the ERROR message.
    pub expressions: Vec<Expr>,
    pub default_values: bool,
}

impl Display for MergeAction {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if let Some(condition) = &self.condition {
            write!(f, " AND {condition} ")?;
        }
        f.write_str("THEN ")?;
        match self.action_type {
            MergeActionType::Update => {
                f.write_str("UPDATE ")?;
                if self.by_name {
                    f.write_str("BY NAME ")?;
                }
                if let Some(info) = &self.update_info {
                    write!(f, "{info}")?;
                }
                Ok(())
            }
            MergeActionType::Delete => f.write_str("DELETE"),
            MergeActionType::Insert => {
                f.write_str("INSERT ")?;
                if self.by_name {
                    f.write_str("BY NAME ")?;
                }
                if !self.insert_columns.is_empty() {
                    write!(f, "({}) ", join(&self.insert_columns, ", ", |c| sql_identifier(c)))?;
                }
                if self.expressions.is_empty() {
                    f.write_str(if self.default_values { "DEFAULT VALUES" } else { "*" })
                } else {
                    write!(f, "VALUES ({})", join(&self.expressions, ", ", |e| e.to_string()))
                }
            }
            MergeActionType::DoNothing => f.write_str("DO NOTHING"),
            MergeActionType::Error => {
                f.write_str("ERROR")?;
                if let Some(e) = self.expressions.first() {
                    write!(f, " {e}")?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeStatement {
    pub cte_map: CteMap,
    pub target: Box<TableRef>,
    pub source: Box<TableRef>,
    pub join_condition: Option<Box<Expr>>,
    pub using_columns: Vec<String>,
    /// Actions in the order written; rendered grouped by condition, as DuckDB stores them.
    pub actions: Vec<(MergeCondition, MergeAction)>,
    pub returning: Vec<Expr>,
}

impl Display for MergeStatement {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}MERGE INTO {} USING {}", self.cte_map, self.target, self.source)?;
        match &self.join_condition {
            Some(condition) => write!(f, " ON {condition}")?,
            None => write!(f, " USING ({})", self.using_columns.join(", "))?,
        }
        for condition in [
            MergeCondition::WhenMatched,
            MergeCondition::WhenNotMatchedBySource,
            MergeCondition::WhenNotMatchedByTarget,
        ] {
            let label = match condition {
                MergeCondition::WhenMatched => "WHEN MATCHED",
                MergeCondition::WhenNotMatchedByTarget => "WHEN NOT MATCHED",
                MergeCondition::WhenNotMatchedBySource => "WHEN NOT MATCHED BY SOURCE",
            };
            for (_, action) in self.actions.iter().filter(|(c, _)| *c == condition) {
                write!(f, " {label} {action}")?;
            }
        }
        f.write_str(&returning_to_string(&self.returning))
    }
}
