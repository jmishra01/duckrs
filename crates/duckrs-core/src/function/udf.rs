// Ported from the engine side of duckdb/src/main/capi/v1/{scalar_function,aggregate_function,table_function}-c.cpp
// (`CScalarFunctionBind`, `CAPIAggregateUpdate`, `CTableFunctionBind`, ...) and of
// duckdb/src/catalog/catalog_entry/{scalar,table}_function_catalog_entry.cpp (`MergeFunctionSet`) @ rust-port-baseline
//
//! User-defined functions: scalar, aggregate and table functions a client registers while the database runs
//! (`duckdb_register_scalar_function`, ...). DuckDB puts them in its system catalog
//! (`CreateScalarFunctionInfo` with `ALTER_ON_CONFLICT`); duckrs keeps them in a [`UserFunctions`] registry per
//! database instance, which the binder reads through [`crate::planner::binder::SessionValues::user_functions`].
//!
//! The engine calls a function only through the traits here, which whoever registers it implements (the C API
//! with C callbacks), and always on whole chunks and vectors.

pub use super::aggregates::{AggregateFunction, AggregateFunctionSet};
use super::aggregates::{AggregateKind, AggregateState};
use crate::planner::expression::BoundExpr;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::vector::{DataChunk, SelectionVector, Vector};
use duckrs_common::{Error, ErrorKind, Result};
use parking_lot::{Mutex, RwLock};
use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

// --------------------------------------------------------------------------------------------------------------
// Scalar functions
// --------------------------------------------------------------------------------------------------------------

/// What a scalar function's bind sees (`BindScalarFunctionInput`): the arguments, bound and cast to the parameters
/// of the overload, and the connection that binds it.
pub struct ScalarUdfBindInput<'a> {
    pub arguments: &'a [BoundExpr],
    pub connection_id: u64,
}

/// The callbacks of a user-defined scalar function.
pub trait ScalarUdf: Send + Sync {
    /// `CScalarFunctionBind`: binds a call, giving what runs it. An error is a binder error of the query.
    fn bind(&self, input: &ScalarUdfBindInput<'_>) -> Result<Arc<dyn ScalarUdfExecutor>>;
}

/// A bound call of a user-defined scalar function (its `FunctionData` and the `FunctionLocalState`s of the threads
/// that run it).
pub trait ScalarUdfExecutor: Send + Sync {
    /// `CAPIScalarFunction`: the result of the rows of `arguments` (one column per argument), into `result`, a flat
    /// vector of the return type with room for every row.
    fn execute(&self, arguments: &DataChunk, result: &mut Vector) -> Result<()>;
}

/// One overload of a user-defined scalar function.
#[derive(Clone)]
pub struct ScalarUdfOverload {
    pub name: String,
    pub arguments: Vec<LogicalType>,
    /// `SetVarArgs`: the type of any number of arguments after `arguments`
    pub varargs: Option<LogicalType>,
    pub return_type: LogicalType,
    /// `FunctionNullHandling::SPECIAL_HANDLING`: a NULL argument does not make the result NULL by itself
    pub special_handling: bool,
    /// `FunctionStability::VOLATILE`
    pub volatile: bool,
    pub function: Arc<dyn ScalarUdf>,
}

impl fmt::Debug for ScalarUdfOverload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut args: Vec<String> = self.arguments.iter().map(|a| a.to_string()).collect();
        if let Some(varargs) = &self.varargs {
            args.push(format!("{varargs}..."));
        }
        write!(f, "{}({}) -> {}", self.name, args.join(", "), self.return_type)
    }
}

impl ScalarUdfOverload {
    /// The parameters of a call with `count` arguments, if the overload takes that many.
    pub fn parameters_for(&self, count: usize) -> Option<Vec<LogicalType>> {
        match &self.varargs {
            None if count == self.arguments.len() => Some(self.arguments.clone()),
            Some(varargs) if count >= self.arguments.len() => {
                let mut parameters = self.arguments.clone();
                parameters.resize(count, varargs.clone());
                Some(parameters)
            }
            _ => None,
        }
    }

    /// `FunctionSignature::IsSameOverload`.
    fn same_overload(&self, other: &Self) -> bool {
        self.arguments == other.arguments && self.varargs == other.varargs
    }
}

/// The overloads of a user-defined scalar function name (`ScalarFunctionSet`).
#[derive(Debug, Clone)]
pub struct ScalarUdfSet {
    pub name: String,
    pub functions: Vec<ScalarUdfOverload>,
}

// --------------------------------------------------------------------------------------------------------------
// Aggregate functions
// --------------------------------------------------------------------------------------------------------------

/// The state of a user-defined aggregate in one group, as the function made it.
pub type UdfState = Box<dyn Any + Send>;

/// The callbacks of a user-defined aggregate function.
pub trait AggregateUdf: Send + Sync {
    /// `state_size` and `state_init`: a new state.
    fn new_state(&self) -> Result<UdfState>;
    /// `update`: adds the rows of `input` (one column per argument) to the states; row `i` goes to
    /// `states[rows[i]]`.
    fn update(&self, input: &DataChunk, states: &mut [&mut UdfState], rows: &[usize]) -> Result<()>;
    /// `combine`: `sources[i]` into `targets[i]`.
    fn combine(&self, sources: &mut [&mut UdfState], targets: &mut [&mut UdfState]) -> Result<()>;
    /// `finalize`: the result of `states[i]` into row `i` of `result`, a flat vector with room for every state.
    fn finalize(&self, states: &mut [&mut UdfState], result: &mut Vector) -> Result<()>;
}

/// [`AggregateFunction::user`]: the callbacks of a user-defined aggregate.
#[derive(Clone)]
pub struct UserAggregate(pub Arc<dyn AggregateUdf>);

impl fmt::Debug for UserAggregate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UserAggregate")
    }
}

/// [`AggregateState::User`]: a state of a user-defined aggregate, with the function that combines it. A copy of
/// the state is the same state, as the function has no way to copy one.
#[derive(Clone)]
pub struct UserAggregateState {
    function: UserAggregate,
    state: Arc<Mutex<UdfState>>,
}

impl fmt::Debug for UserAggregateState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UserAggregateState")
    }
}

impl UserAggregate {
    fn new_state(&self) -> Result<UserAggregateState> {
        Ok(UserAggregateState {
            function: self.clone(),
            state: Arc::new(Mutex::new(self.0.new_state()?)),
        })
    }

    /// The state of `slot`, made now if the group has none yet.
    fn state_of(&self, slot: &mut AggregateState) -> Result<UserAggregateState> {
        if let AggregateState::User(Some(state)) = slot {
            return Ok(state.clone());
        }
        let state = self.new_state()?;
        *slot = AggregateState::User(Some(state.clone()));
        Ok(state)
    }

    /// `CAPIAggregateUpdate`: the rows `rows` (row of `input`, group) into the states of their groups. The
    /// arguments of a function that takes several are the fields of a STRUCT `input`
    /// ([`AggregateFunction::arguments`]).
    pub(crate) fn update(
        &self,
        function: &AggregateFunction,
        input: Option<&Vector>,
        count: usize,
        rows: &[(usize, usize)],
        states: &mut [AggregateState],
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let selected = rows.len();
        let columns = match input {
            None => Vec::new(),
            Some(input) => {
                let mut input = input.clone();
                let in_order = selected == count && rows.iter().enumerate().all(|(i, &(row, _))| i == row);
                if !in_order {
                    let sel = SelectionVector::from_vec(rows.iter().map(|&(row, _)| row as u32).collect());
                    input.slice(&sel, selected);
                }
                input.flatten(selected);
                if function.arguments.len() > 1 {
                    input.struct_children().to_vec()
                } else {
                    vec![input]
                }
            }
        };
        let chunk = DataChunk::from_columns(columns, selected);
        // each group's state once, and for each row the position of its group among them
        let mut position_of: HashMap<usize, usize> = HashMap::new();
        let mut groups = Vec::new();
        let positions: Vec<usize> = rows
            .iter()
            .map(|&(_, group)| {
                *position_of.entry(group).or_insert_with(|| {
                    groups.push(group);
                    groups.len() - 1
                })
            })
            .collect();
        let handles = groups
            .iter()
            .map(|&group| self.state_of(&mut states[group]))
            .collect::<Result<Vec<_>>>()?;
        let mut guards: Vec<_> = handles.iter().map(|h| h.state.lock()).collect();
        let mut targets: Vec<&mut UdfState> = guards.iter_mut().map(|g| &mut **g).collect();
        self.0.update(&chunk, &mut targets, &positions)
    }

    /// `CAPIAggregateFinalize`: the results of `states`, from row 0 of `result` on.
    pub(crate) fn finalize(&self, states: &[AggregateState], result: &mut Vector) -> Result<()> {
        let handles = states
            .iter()
            .map(|state| match state {
                AggregateState::User(Some(state)) => Ok(state.clone()),
                _ => self.new_state(),
            })
            .collect::<Result<Vec<_>>>()?;
        let mut guards: Vec<_> = handles.iter().map(|h| h.state.lock()).collect();
        let mut targets: Vec<&mut UdfState> = guards.iter_mut().map(|g| &mut **g).collect();
        self.0.finalize(&mut targets, result)
    }

    /// [`Self::finalize`] of one state, as a value.
    pub(crate) fn finalize_one(&self, state: &AggregateState, return_type: &LogicalType) -> Result<Value> {
        let mut result = Vector::new(return_type.clone(), 1);
        self.finalize(std::slice::from_ref(state), &mut result)?;
        Ok(result.get_value(0))
    }
}

/// `CAPIAggregateCombine` of two states of a user-defined aggregate: `right` goes into `left`.
pub(crate) fn combine_user_states(left: AggregateState, right: AggregateState) -> Result<AggregateState> {
    match (left, right) {
        (AggregateState::User(Some(target)), AggregateState::User(Some(source))) => {
            if Arc::ptr_eq(&target.state, &source.state) {
                return Ok(AggregateState::User(Some(target)));
            }
            {
                let mut source_guard = source.state.lock();
                let mut target_guard = target.state.lock();
                target
                    .function
                    .0
                    .combine(&mut [&mut *source_guard], &mut [&mut *target_guard])?;
            }
            Ok(AggregateState::User(Some(target)))
        }
        (AggregateState::User(None), right) => Ok(right),
        (left, _) => Ok(left),
    }
}

// --------------------------------------------------------------------------------------------------------------
// Table functions
// --------------------------------------------------------------------------------------------------------------

/// What a table function's bind sees (`TableFunctionBindInput`): its parameters, cast to the types of the
/// overload, the named parameters the call gives (cast to their types), and the connection that binds it.
pub struct TableUdfBindInput {
    pub parameters: Vec<Value>,
    pub named_parameters: Vec<(String, Value)>,
    pub connection_id: u64,
}

/// What a table function's bind gives: its columns, how many rows it expects to give (and whether that is
/// exact), and what scans it.
pub struct TableUdfBinding {
    pub names: Vec<String>,
    pub types: Vec<LogicalType>,
    pub cardinality: Option<(u64, bool)>,
    pub scan: Arc<dyn TableUdfScan>,
}

/// The callbacks of a user-defined table function.
pub trait TableUdf: Send + Sync {
    /// `CTableFunctionBind`. An error is a binder error of the query.
    fn bind(&self, input: TableUdfBindInput) -> Result<TableUdfBinding>;
}

/// A bound call of a user-defined table function (its `FunctionData`).
pub trait TableUdfScan: Send + Sync {
    /// `CTableFunctionInit` and `CTableFunctionLocalInit`: a scan of the columns `column_ids`.
    fn init(&self, column_ids: &[usize]) -> Result<Box<dyn TableUdfRun>>;
}

/// A running scan of a user-defined table function (its global and local states).
pub trait TableUdfRun: Send {
    /// `CTableFunction`: the next rows into `output`, which is empty and has a column per column of the scan; no
    /// rows means the scan is done.
    fn next(&mut self, output: &mut DataChunk) -> Result<()>;
}

/// [`crate::planner::logical::LogicalOperatorKind::UserTableScan`]: a bound call of a user-defined table function.
#[derive(Clone)]
pub struct UserTableScan {
    /// the name of the function, which EXPLAIN shows
    pub name: String,
    pub scan: Arc<dyn TableUdfScan>,
    pub cardinality: Option<(u64, bool)>,
}

impl fmt::Debug for UserTableScan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UserTableScan({})", self.name)
    }
}

/// One overload of a user-defined table function.
#[derive(Clone)]
pub struct TableUdfOverload {
    pub name: String,
    /// the positional parameters
    pub arguments: Vec<LogicalType>,
    /// the named parameters (`duckdb_table_function_add_named_parameter`), in the order they were added
    pub named_parameters: Vec<(String, LogicalType)>,
    /// `projection_pushdown`: the function gives only the columns its init asks for
    pub projection_pushdown: bool,
    pub function: Arc<dyn TableUdf>,
}

impl fmt::Display for TableUdfOverload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let args: Vec<String> = self.arguments.iter().map(|a| a.to_string()).collect();
        write!(f, "{}({})", self.name, args.join(", "))
    }
}

impl fmt::Debug for TableUdfOverload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

/// The overloads of a user-defined table function name (`TableFunctionSet`).
#[derive(Debug, Clone)]
pub struct TableUdfSet {
    pub name: String,
    pub functions: Vec<Arc<TableUdfOverload>>,
}

// --------------------------------------------------------------------------------------------------------------
// The registry
// --------------------------------------------------------------------------------------------------------------

/// The user-defined functions of a database instance, by lower-case name (their entries of the system catalog).
#[derive(Default)]
pub struct UserFunctions {
    scalar: RwLock<HashMap<String, Arc<ScalarUdfSet>>>,
    aggregate: RwLock<HashMap<String, Arc<AggregateFunctionSet>>>,
    table: RwLock<HashMap<String, Arc<TableUdfSet>>>,
}

impl fmt::Debug for UserFunctions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserFunctions")
            .field("scalar", &self.scalar.read().keys().collect::<Vec<_>>())
            .field("aggregate", &self.aggregate.read().keys().collect::<Vec<_>>())
            .field("table", &self.table.read().keys().collect::<Vec<_>>())
            .finish()
    }
}

/// `CatalogException` of an entry that exists already.
fn exists(kind: &str, name: &str) -> Error {
    Error::new(
        ErrorKind::Catalog,
        format!("{kind} with name \"{name}\" already exists!"),
    )
}

impl UserFunctions {
    /// `CreateScalarFunctionInfo` with `ALTER_ON_CONFLICT`: the overloads join those of the name, each replacing
    /// one with the same parameters (`MergeFunctionSet` with `override`).
    pub fn register_scalar(&self, set: ScalarUdfSet) -> Result<()> {
        if set.functions.is_empty() {
            return Err(Error::new(ErrorKind::Internal, "a function set without functions"));
        }
        let key = set.name.to_ascii_lowercase();
        let mut scalar = self.scalar.write();
        let merged = match scalar.get(&key) {
            None => set,
            Some(existing) => {
                let mut merged = ScalarUdfSet::clone(existing);
                for function in set.functions {
                    match merged.functions.iter_mut().find(|f| f.same_overload(&function)) {
                        Some(slot) => *slot = function,
                        None => merged.functions.push(function),
                    }
                }
                merged
            }
        };
        scalar.insert(key, Arc::new(merged));
        Ok(())
    }

    /// `CreateAggregateFunctionInfo`: a name that is taken already is an error, as the catalog cannot alter an
    /// aggregate function entry. Every function of the set must be a user-defined one
    /// ([`AggregateFunction::user`]).
    pub fn register_aggregate(&self, set: AggregateFunctionSet) -> Result<()> {
        if set.functions.is_empty() || set.functions.iter().any(|f| f.user.is_none()) {
            return Err(Error::new(ErrorKind::Internal, "an aggregate set without user-defined functions"));
        }
        let key = set.name.to_ascii_lowercase();
        let mut aggregate = self.aggregate.write();
        if aggregate.contains_key(&key) {
            return Err(exists("Aggregate Function", &set.name));
        }
        aggregate.insert(key, Arc::new(set));
        Ok(())
    }

    /// `CreateTableFunctionInfo` with `ALTER_ON_CONFLICT`: the overloads join those of the name; one with the
    /// parameters of one there already takes its place.
    pub fn register_table(&self, set: TableUdfSet) -> Result<()> {
        if set.functions.is_empty() {
            return Err(Error::new(ErrorKind::Internal, "a function set without functions"));
        }
        let key = set.name.to_ascii_lowercase();
        let mut table = self.table.write();
        let merged = match table.get(&key) {
            None => set,
            Some(existing) => {
                let mut merged = TableUdfSet::clone(existing);
                for function in set.functions {
                    match merged.functions.iter_mut().find(|f| f.arguments == function.arguments) {
                        Some(slot) => *slot = function,
                        None => merged.functions.push(function),
                    }
                }
                merged
            }
        };
        table.insert(key, Arc::new(merged));
        Ok(())
    }

    pub fn scalar(&self, name: &str) -> Option<Arc<ScalarUdfSet>> {
        self.scalar.read().get(&name.to_ascii_lowercase()).cloned()
    }

    pub fn aggregate(&self, name: &str) -> Option<Arc<AggregateFunctionSet>> {
        self.aggregate.read().get(&name.to_ascii_lowercase()).cloned()
    }

    pub fn table(&self, name: &str) -> Option<Arc<TableUdfSet>> {
        self.table.read().get(&name.to_ascii_lowercase()).cloned()
    }
}

/// A user-defined aggregate function overload (`duckdb_create_aggregate_function`).
pub fn user_aggregate_function(
    name: impl Into<String>,
    arguments: Vec<LogicalType>,
    return_type: LogicalType,
    function: Arc<dyn AggregateUdf>,
) -> AggregateFunction {
    AggregateFunction {
        name: name.into(),
        arguments,
        return_type,
        kind: AggregateKind::User,
        export: None,
        user: Some(UserAggregate(function)),
    }
}

/// A set of user-defined aggregate overloads, for [`UserFunctions::register_aggregate`].
pub fn user_aggregate_set(name: impl Into<String>, functions: Vec<AggregateFunction>) -> AggregateFunctionSet {
    AggregateFunctionSet {
        name: name.into(),
        functions: functions.into_iter().map(Arc::new).collect(),
    }
}
