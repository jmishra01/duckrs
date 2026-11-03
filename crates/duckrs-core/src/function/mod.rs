// Ported from duckdb/src/function/{function,scalar_function,function_set}.cpp @ rust-port-baseline
//
//! Functions: scalar function signatures and sets, the built-in functions, casts, the implicit
//! cast and type unification rules, and overload resolution ([`binder`]).

pub mod binder;
pub mod cast;
pub mod cast_rules;
pub mod combine_types;
mod operators;

use duckrs_common::Result;
use duckrs_common::types::LogicalType;
use duckrs_common::vector::{DataChunk, Vector};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, OnceLock};

/// Executes a scalar function over the argument columns of `args` into `result`
/// (`scalar_function_t`).
pub type ScalarCallback = fn(args: &DataChunk, result: &mut Vector) -> Result<()>;

/// One overload of a scalar function (`ScalarFunction`).
pub struct ScalarFunction {
    pub name: String,
    pub arguments: Vec<LogicalType>,
    pub return_type: LogicalType,
    pub function: ScalarCallback,
}

impl ScalarFunction {
    pub fn new(
        name: impl Into<String>,
        arguments: Vec<LogicalType>,
        return_type: LogicalType,
        function: ScalarCallback,
    ) -> Self {
        Self {
            name: name.into(),
            arguments,
            return_type,
            function,
        }
    }
}

impl fmt::Debug for ScalarFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl fmt::Display for ScalarFunction {
    /// `SimpleFunction::ToString`: `name(T1, T2) -> R`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let args: Vec<String> = self.arguments.iter().map(|a| a.to_string()).collect();
        write!(f, "{}({}) -> {}", self.name, args.join(", "), self.return_type)
    }
}

/// All overloads of a scalar function name (`ScalarFunctionSet`).
#[derive(Debug)]
pub struct ScalarFunctionSet {
    pub name: String,
    pub functions: Vec<Arc<ScalarFunction>>,
}

impl ScalarFunctionSet {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            functions: Vec::new(),
        }
    }

    pub fn add(&mut self, function: ScalarFunction) {
        self.functions.push(Arc::new(function));
    }
}

/// The built-in functions by lowercase name. In DuckDB they are catalog entries of the system
/// catalog; they move there when the catalog grows function entries.
pub struct BuiltinFunctions {
    scalar: HashMap<String, ScalarFunctionSet>,
}

impl BuiltinFunctions {
    pub fn get() -> &'static BuiltinFunctions {
        static FUNCTIONS: OnceLock<BuiltinFunctions> = OnceLock::new();
        FUNCTIONS.get_or_init(|| {
            let mut functions = BuiltinFunctions { scalar: HashMap::new() };
            for set in operators::arithmetic_operators() {
                functions.scalar.insert(set.name.clone(), set);
            }
            functions
        })
    }

    pub fn scalar(&self, name: &str) -> Option<&ScalarFunctionSet> {
        self.scalar.get(&name.to_ascii_lowercase())
    }
}
