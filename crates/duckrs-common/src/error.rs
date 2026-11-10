// Ported from duckdb/src/common/exception.cpp @ rust-port-baseline

use std::fmt;

/// Mirrors DuckDB's `ExceptionType`. The display name must match the C++ `ExceptionTypeToString`
/// output, because sqllogictest `statement error` blocks match on the rendered message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    Invalid,
    OutOfRange,
    Conversion,
    UnknownType,
    Decimal,
    MismatchType,
    DivideByZero,
    ObjectSize,
    InvalidType,
    Serialization,
    Transaction,
    NotImplemented,
    Expression,
    Catalog,
    Parser,
    Planner,
    Scheduler,
    Executor,
    Constraint,
    Index,
    Stat,
    Connection,
    Syntax,
    Settings,
    Binder,
    Optimizer,
    NullPointer,
    Io,
    Interrupt,
    Fatal,
    Internal,
    InvalidInput,
    OutOfMemory,
    Permission,
    ParameterNotResolved,
    ParameterNotAllowed,
    Dependency,
    Http,
    MissingExtension,
    Autoload,
    Sequence,
    InvalidConfiguration,
    DataCorruption,
    ResourceInUse,
}

impl ErrorKind {
    pub fn name(self) -> &'static str {
        use ErrorKind::*;
        match self {
            Invalid => "Invalid",
            OutOfRange => "Out of Range",
            Conversion => "Conversion",
            UnknownType => "Unknown Type",
            Decimal => "Decimal",
            MismatchType => "Mismatch Type",
            DivideByZero => "Divide by Zero",
            ObjectSize => "Object Size",
            InvalidType => "Invalid type",
            Serialization => "Serialization",
            Transaction => "TransactionContext",
            NotImplemented => "Not implemented",
            Expression => "Expression",
            Catalog => "Catalog",
            Parser => "Parser",
            Planner => "Planner",
            Scheduler => "Scheduler",
            Executor => "Executor",
            Constraint => "Constraint",
            Index => "Index",
            Stat => "Stat",
            Connection => "Connection",
            Syntax => "Syntax",
            Settings => "Settings",
            Binder => "Binder",
            Optimizer => "Optimizer",
            NullPointer => "NullPointer",
            Io => "IO",
            Interrupt => "INTERRUPT",
            Fatal => "FATAL",
            Internal => "INTERNAL",
            InvalidInput => "Invalid Input",
            OutOfMemory => "Out of Memory",
            Permission => "Permission",
            ParameterNotResolved => "Parameter Not Resolved",
            ParameterNotAllowed => "Parameter Not Allowed",
            Dependency => "Dependency",
            Http => "HTTP",
            MissingExtension => "Missing Extension",
            Autoload => "Extension Autoloading",
            Sequence => "Sequence",
            InvalidConfiguration => "Invalid Configuration",
            DataCorruption => "Data Corruption",
            ResourceInUse => "Resource In Use",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    /// The byte offset in the query that the error is at (the `position` extra info of C++).
    pub position: Option<usize>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            position: None,
        }
    }

    pub fn with_position(mut self, position: usize) -> Self {
        self.position = Some(position);
        self
    }

    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotImplemented, message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} Error: {}", self.kind.name(), self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T, E = Error> = std::result::Result<T, E>;
