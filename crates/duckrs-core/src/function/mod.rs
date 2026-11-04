// Ported from duckdb/src/function/{function,scalar_function,function_set}.cpp @ rust-port-baseline
//
//! Functions: scalar function signatures and sets, the built-in functions, casts, the implicit
//! cast and type unification rules, and overload resolution ([`binder`]).

pub mod aggregates;
pub mod binder;
mod bit_casts;
mod bit_functions;
pub mod cast;
pub mod cast_rules;
pub mod combine_types;
pub mod generic;
pub mod nested;
mod nested_casts;
pub(crate) mod operators;
mod string_nested;
pub(crate) mod time_casts;
pub mod variant;

use duckrs_common::Result;
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_common::vector::{DataChunk, Vector};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, OnceLock};

/// Executes a scalar function over the argument columns of `args` into `result`
/// (`scalar_function_t`).
pub type ScalarCallback = fn(args: &DataChunk, result: &mut Vector) -> Result<()>;

/// Derives the overload to run from the types of the arguments (`resolve_types_function_t`): the
/// DECIMAL operators take their width and scale from the arguments.
pub type ResolveTypesCallback = fn(function: &ScalarFunction, arguments: &[LogicalType]) -> Result<ScalarFunction>;

/// One overload of a scalar function (`ScalarFunction`).
pub struct ScalarFunction {
    pub name: String,
    pub arguments: Vec<LogicalType>,
    pub return_type: LogicalType,
    pub function: ScalarCallback,
    pub resolve_types: Option<ResolveTypesCallback>,
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
            resolve_types: None,
        }
    }

    /// `ScalarFunction::SetResolveTypesCallback`.
    pub fn with_resolve_types(mut self, resolve_types: ResolveTypesCallback) -> Self {
        self.resolve_types = Some(resolve_types);
        self
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

/// `nextval`, `currval` and `setval`. Their signatures resolve like any function's; once bound,
/// the binder replaces them with [`crate::planner::expression::BoundExprKind::Sequence`].
fn sequence_functions() -> Vec<ScalarFunctionSet> {
    fn bound_elsewhere(_: &DataChunk, _: &mut Vector) -> Result<()> {
        unreachable!("sequence functions execute as BoundExprKind::Sequence")
    }
    let function = |name: &str, arguments: Vec<LogicalType>| {
        ScalarFunction::new(name, arguments, LogicalType::BIGINT, bound_elsewhere)
    };
    let (name, value, is_called) = (LogicalType::VARCHAR, LogicalType::BIGINT, LogicalType::BOOLEAN);
    let mut sets = Vec::new();
    for sequence_function in ["nextval", "currval"] {
        let mut set = ScalarFunctionSet::new(sequence_function);
        set.add(function(sequence_function, vec![name.clone()]));
        sets.push(set);
    }
    let mut set = ScalarFunctionSet::new("setval");
    set.add(function("setval", vec![name.clone(), value.clone()]));
    set.add(function("setval", vec![name, value, is_called]));
    sets.push(set);
    sets
}

/// `typeof`: the binder replaces it with a constant ([`typeof_constant`]), so its signature only
/// has to resolve.
fn typeof_function() -> ScalarFunctionSet {
    fn bound_elsewhere(_: &DataChunk, _: &mut Vector) -> Result<()> {
        unreachable!("typeof is a constant once its argument has a type")
    }
    let mut set = ScalarFunctionSet::new("typeof");
    set.add(ScalarFunction::new(
        "typeof",
        vec![LogicalType::new(LogicalTypeId::Any)],
        LogicalType::VARCHAR,
        bound_elsewhere,
    ));
    set
}

/// `BindTypeOfFunctionExpression`: the name of the argument's type, which needs no evaluation.
pub fn typeof_constant(argument: &LogicalType) -> Value {
    Value::varchar(argument.to_string())
}

/// The built-in functions by lowercase name. In DuckDB they are catalog entries of the system
/// catalog; they move there when the catalog grows function entries.
pub struct BuiltinFunctions {
    scalar: HashMap<String, ScalarFunctionSet>,
    aggregate: HashMap<String, aggregates::AggregateFunctionSet>,
}

impl BuiltinFunctions {
    pub fn get() -> &'static BuiltinFunctions {
        static FUNCTIONS: OnceLock<BuiltinFunctions> = OnceLock::new();
        FUNCTIONS.get_or_init(|| {
            let mut functions = BuiltinFunctions {
                scalar: HashMap::new(),
                aggregate: HashMap::new(),
            };
            for set in operators::arithmetic_operators() {
                functions.scalar.insert(set.name.clone(), set);
            }
            for set in sequence_functions() {
                functions.scalar.insert(set.name.clone(), set);
            }
            for set in bit_functions::bit_functions() {
                functions.scalar.insert(set.name.clone(), set);
            }
            for set in generic::generic_functions() {
                match functions.scalar.get_mut(&set.name) {
                    Some(existing) => existing.functions.extend(set.functions),
                    None => {
                        functions.scalar.insert(set.name.clone(), set);
                    }
                }
            }
            let typeof_set = typeof_function();
            functions.scalar.insert(typeof_set.name.clone(), typeof_set);
            for set in aggregates::aggregate_functions() {
                functions.aggregate.insert(set.name.clone(), set);
            }
            functions
        })
    }

    pub fn scalar(&self, name: &str) -> Option<&ScalarFunctionSet> {
        self.scalar.get(&name.to_ascii_lowercase())
    }

    pub fn aggregate(&self, name: &str) -> Option<&aggregates::AggregateFunctionSet> {
        self.aggregate.get(&name.to_ascii_lowercase())
    }
}
