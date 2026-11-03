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

/// A transformed SQL statement. Only SELECT is transformed so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    Select(SelectStatement),
}

impl Display for Statement {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Statement::Select(s) => write!(f, "{s}"),
        }
    }
}
