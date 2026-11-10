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
pub(crate) mod dates;
pub use dates::date_part_call_of;
pub mod extras;
pub mod generic;
pub(crate) mod geometry;
pub(crate) mod holistic;
pub mod json;
mod lists;
pub(crate) mod math;
pub mod nested;
mod nested_casts;
pub(crate) mod operators;
mod printf;
pub(crate) mod regexp;
mod string_nested;
mod strings;
pub mod test_all_types;
pub mod test_vector_types;
pub(crate) mod time_casts;
pub mod variant;

use crate::planner::expression::{BoundExpr, BoundExprKind};
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

/// `constant_or_null`: the first argument, or NULL where one of the others is.
fn constant_or_null_function() -> ScalarFunctionSet {
    fn resolve(function: &ScalarFunction, arguments: &[LogicalType]) -> Result<ScalarFunction> {
        Ok(ScalarFunction::new(
            function.name.clone(),
            arguments.to_vec(),
            arguments[0].clone(),
            function.function,
        ))
    }
    let mut set = ScalarFunctionSet::new("constant_or_null");
    for count in 2..=5 {
        let arguments = vec![LogicalType::new(LogicalTypeId::Any); count];
        let function = ScalarFunction::new(
            "constant_or_null",
            arguments,
            LogicalType::BOOLEAN,
            run_constant_or_null,
        );
        set.add(function.with_resolve_types(resolve));
    }
    set
}

fn run_constant_or_null(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let count = args.len();
    let columns = args.columns();
    let null = Value::null(result.logical_type().clone());
    let formats: Vec<_> = columns[1..].iter().map(|c| c.to_unified(count)).collect();
    result.prepare_flat(count.max(1));
    for row in 0..count {
        if formats.iter().all(|format| format.is_valid(row)) {
            result.set_value(row, &columns[0].get_value(row))?;
        } else {
            result.set_value(row, &null)?;
        }
    }
    Ok(())
}

/// A call of `constant_or_null` with these arguments, of which there are at most five.
pub fn constant_or_null(children: Vec<BoundExpr>) -> BoundExpr {
    let arguments: Vec<LogicalType> = children.iter().map(|c| c.return_type.clone()).collect();
    let return_type = arguments[0].clone();
    let function = ScalarFunction::new("constant_or_null", arguments, return_type.clone(), run_constant_or_null);
    BoundExpr::new(
        BoundExprKind::Function {
            function: Arc::new(function),
            children,
            is_operator: false,
        },
        return_type,
    )
}

/// A call of `prefix`, `suffix` or `contains` of strings.
pub fn string_needle_call(name: &str, text: BoundExpr, needle: BoundExpr) -> BoundExpr {
    let children = vec![text, needle];
    let callback = match BuiltinFunctions::get().scalar(name) {
        Some(set) if name != "contains" => set.functions[0].function,
        _ => generic::contains,
    };
    nested::bound_function(name, children, LogicalType::BOOLEAN, callback)
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

// Ported from duckdb/extension/core_functions/scalar/generic/stats.cpp and the `ToStruct` of
// duckdb/src/storage/statistics/{base,numeric}_stats.cpp @ rust-port-baseline

/// Whether `stats()` of a value of this type can be told: those whose statistics are the
/// numeric ones (`NumericStats`), which is what duckrs keeps. The others (strings, nested
/// types, VARIANT) have statistics of more kinds than duckrs works out.
pub fn stats_supported(ty: &LogicalType) -> bool {
    ty.id() == LogicalTypeId::SqlNull
        || (crate::optimizer::statistics::has_ordered_statistics(ty) && ty.id() != LogicalTypeId::Varchar)
}

/// `stats(expression)`: the statistics of its argument, which the statistics propagator works
/// out (`StatsPropagateStats`) and passes on as a second, constant argument
/// ([`stats_with_value`]). Until then, as with the optimizer off, it is NULL.
fn stats_function() -> ScalarFunctionSet {
    fn unfilled(args: &DataChunk, result: &mut Vector) -> Result<()> {
        let ty = args.columns()[0].logical_type();
        if !stats_supported(ty) {
            return Err(crate::unported(format!("stats() of a {ty} value")));
        }
        *result = Vector::constant_null(result.logical_type().clone());
        Ok(())
    }
    fn resolve(function: &ScalarFunction, arguments: &[LogicalType]) -> Result<ScalarFunction> {
        Ok(ScalarFunction::new(
            function.name.clone(),
            arguments.to_vec(),
            function.return_type.clone(),
            function.function,
        ))
    }
    let mut set = ScalarFunctionSet::new("stats");
    let function = ScalarFunction::new(
        "stats",
        vec![LogicalType::new(LogicalTypeId::Any)],
        LogicalType::new(LogicalTypeId::Variant),
        unfilled,
    );
    set.add(function.with_resolve_types(resolve));
    set
}

/// The `stats` function once the propagator knows the statistics: they are its second argument.
pub fn stats_with_value(argument: LogicalType) -> ScalarFunction {
    fn filled(args: &DataChunk, result: &mut Vector) -> Result<()> {
        *result = args.columns()[1].clone();
        Ok(())
    }
    let variant = LogicalType::new(LogicalTypeId::Variant);
    ScalarFunction::new("stats", vec![argument, variant.clone()], variant, filled)
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
            json::register_projjson_parser();
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
            for set in bit_functions::bit_functions()
                .into_iter()
                .chain(geometry::geometry_functions())
            {
                functions.scalar.insert(set.name.clone(), set);
            }
            for set in generic::generic_functions()
                .into_iter()
                .chain(bit_functions::integer_bit_functions())
            {
                match functions.scalar.get_mut(&set.name) {
                    Some(existing) => existing.functions.extend(set.functions),
                    None => {
                        functions.scalar.insert(set.name.clone(), set);
                    }
                }
            }
            for set in math::math_functions()
                .into_iter()
                .chain(strings::string_functions())
                .chain(regexp::regexp_functions())
                .chain(dates::date_functions())
            {
                match functions.scalar.get_mut(&set.name) {
                    Some(existing) => {
                        // the overloads that exist already stay
                        for function in set.functions {
                            if !existing.functions.iter().any(|f| f.arguments == function.arguments) {
                                existing.functions.push(function);
                            }
                        }
                    }
                    None => {
                        functions.scalar.insert(set.name.clone(), set);
                    }
                }
            }
            // the operators under their function names
            for (operator, alias) in [
                ("+", "add"),
                ("-", "subtract"),
                ("*", "multiply"),
                ("//", "divide"),
                ("%", "mod"),
            ] {
                let functions_of_operator = functions.scalar[operator].functions.clone();
                functions.scalar.insert(
                    alias.to_string(),
                    ScalarFunctionSet {
                        name: alias.to_string(),
                        functions: functions_of_operator,
                    },
                );
            }
            let set = constant_or_null_function();
            functions.scalar.insert(set.name.clone(), set);
            let typeof_set = typeof_function();
            functions.scalar.insert(typeof_set.name.clone(), typeof_set);
            let stats_set = stats_function();
            functions.scalar.insert(stats_set.name.clone(), stats_set);
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

    /// The scalar function sets, by name.
    pub fn scalar_sets(&self) -> Vec<&ScalarFunctionSet> {
        let mut sets: Vec<_> = self.scalar.values().collect();
        sets.sort_by(|a, b| a.name.cmp(&b.name));
        sets
    }

    /// The aggregate function sets, by name.
    pub fn aggregate_sets(&self) -> Vec<&aggregates::AggregateFunctionSet> {
        let mut sets: Vec<_> = self.aggregate.values().collect();
        sets.sort_by(|a, b| a.name.cmp(&b.name));
        sets
    }
}
