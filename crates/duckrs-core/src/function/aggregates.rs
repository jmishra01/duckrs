// Ported from duckdb/src/function/aggregate/distributive/{count,minmax,first_last_any}.cpp and
// duckdb/extension/core_functions/aggregate/{distributive/sum,algebraic/avg}.cpp
// @ rust-port-baseline
//
//! The aggregate functions: `count`, `count_star`, `sum`, `avg`, `min`, `max`, `first`, `last`
//! and `any_value`. A function has a state per group; `update` feeds it a chunk's rows and
//! `finalize` turns the states into a result column.

use super::cast::{int_to_double, uhuge_to_double};
use duckrs_common::sort_key::{NullOrder, OrderModifiers, OrderType, create_sort_keys};
use duckrs_common::types::{LogicalType, LogicalTypeId, NativeType, PhysicalType};
use duckrs_common::value::Value;
use duckrs_common::vector::Vector;
use duckrs_common::{Error, ErrorKind, Result};
use std::fmt;
use std::sync::Arc;

/// What an aggregate computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateKind {
    CountStar,
    Count,
    SumInteger,
    SumDouble,
    SumDecimal,
    AvgInteger,
    AvgDouble,
    AvgDecimal,
    Min,
    Max,
    First,
    Last,
    AnyValue,
}

/// One overload of an aggregate function (`AggregateFunction`).
#[derive(Debug)]
pub struct AggregateFunction {
    pub name: String,
    pub arguments: Vec<LogicalType>,
    pub return_type: LogicalType,
    pub kind: AggregateKind,
}

impl fmt::Display for AggregateFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let args: Vec<String> = self.arguments.iter().map(|a| a.to_string()).collect();
        write!(f, "{}({}) -> {}", self.name, args.join(", "), self.return_type)
    }
}

impl AggregateFunction {
    /// The bind callbacks: the return type for an argument of the given type.
    pub fn resolve_return_type(&self, input: Option<&LogicalType>) -> LogicalType {
        match (self.kind, input) {
            (AggregateKind::Min | AggregateKind::Max | AggregateKind::First | AggregateKind::Last, Some(ty))
            | (AggregateKind::AnyValue, Some(ty)) => ty.clone(),
            // the sum of a DECIMAL is a DECIMAL(38)
            (AggregateKind::SumDecimal, Some(ty)) => {
                let (_, scale) = ty.decimal_properties().expect("a DECIMAL argument");
                LogicalType::decimal(duckrs_common::types::decimal::MAX_WIDTH, scale)
            }
            _ => self.return_type.clone(),
        }
    }
}

/// All overloads of an aggregate function name (`AggregateFunctionSet`).
#[derive(Debug)]
pub struct AggregateFunctionSet {
    pub name: String,
    pub functions: Vec<Arc<AggregateFunction>>,
}

impl AggregateFunctionSet {
    fn new(name: &str, overloads: Vec<(Vec<LogicalType>, LogicalType, AggregateKind)>) -> Self {
        let functions = overloads
            .into_iter()
            .map(|(arguments, return_type, kind)| {
                Arc::new(AggregateFunction {
                    name: name.to_string(),
                    arguments,
                    return_type,
                    kind,
                })
            })
            .collect();
        Self {
            name: name.to_string(),
            functions,
        }
    }
}

/// The built-in aggregates, with the overloads of `SumFun`, `AvgFun` and friends (the
/// temporal, INTERVAL and BIGNUM ones come later).
pub(super) fn aggregate_functions() -> Vec<AggregateFunctionSet> {
    use AggregateKind::*;
    let any = || LogicalType::new(LogicalTypeId::Any);
    let decimal = || LogicalType::new(LogicalTypeId::Decimal);
    let sum = || {
        let mut overloads = vec![(vec![decimal()], decimal(), SumDecimal)];
        for ty in [
            LogicalType::BOOLEAN,
            LogicalType::SMALLINT,
            LogicalType::INTEGER,
            LogicalType::BIGINT,
            LogicalType::HUGEINT,
        ] {
            overloads.push((vec![ty], LogicalType::HUGEINT, SumInteger));
        }
        overloads.push((vec![LogicalType::DOUBLE], LogicalType::DOUBLE, SumDouble));
        overloads
    };
    let avg = || {
        let mut overloads = vec![(vec![decimal()], LogicalType::DOUBLE, AvgDecimal)];
        for ty in [
            LogicalType::SMALLINT,
            LogicalType::INTEGER,
            LogicalType::BIGINT,
            LogicalType::HUGEINT,
        ] {
            overloads.push((vec![ty], LogicalType::DOUBLE, AvgInteger));
        }
        overloads.push((vec![LogicalType::DOUBLE], LogicalType::DOUBLE, AvgDouble));
        overloads
    };
    let unary = |kind| vec![(vec![any()], any(), kind)];
    vec![
        AggregateFunctionSet::new("count_star", vec![(vec![], LogicalType::BIGINT, CountStar)]),
        AggregateFunctionSet::new(
            "count",
            vec![
                (vec![any()], LogicalType::BIGINT, Count),
                (vec![], LogicalType::BIGINT, CountStar),
            ],
        ),
        AggregateFunctionSet::new("sum", sum()),
        AggregateFunctionSet::new("avg", avg()),
        AggregateFunctionSet::new("mean", avg()),
        AggregateFunctionSet::new("min", unary(Min)),
        AggregateFunctionSet::new("max", unary(Max)),
        AggregateFunctionSet::new("first", unary(First)),
        AggregateFunctionSet::new("arbitrary", unary(First)),
        AggregateFunctionSet::new("last", unary(Last)),
        AggregateFunctionSet::new("any_value", unary(AnyValue)),
    ]
}

/// The state of one aggregate in one group.
#[derive(Debug, Clone)]
pub enum AggregateState {
    Count(i64),
    Integer {
        sum: i128,
        count: u64,
    },
    Double {
        sum: f64,
        count: u64,
    },
    /// the sort key of the current minimum or maximum, and its value
    Extreme(Option<(Vec<u8>, Value)>),
    /// whether a value was taken, and the value
    Value(Option<Value>),
}

impl AggregateKind {
    /// `StateInitialize`.
    pub fn initial_state(self) -> AggregateState {
        use AggregateKind::*;
        match self {
            CountStar | Count => AggregateState::Count(0),
            SumInteger | SumDecimal | AvgInteger | AvgDecimal => AggregateState::Integer { sum: 0, count: 0 },
            SumDouble | AvgDouble => AggregateState::Double { sum: 0.0, count: 0 },
            Min | Max => AggregateState::Extreme(None),
            First | Last | AnyValue => AggregateState::Value(None),
        }
    }
}

fn add_integers<T: NativeType + Into<i128>>(
    input: &Vector,
    count: usize,
    rows: &[(usize, usize)],
    states: &mut [AggregateState],
) -> Result<()> {
    let u = input.to_unified(count);
    let data = u.data::<T>();
    for &(row, group) in rows {
        let index = u.sel.get(row);
        if !u.validity.is_valid(index) {
            continue;
        }
        let AggregateState::Integer { sum, count: seen } = &mut states[group] else {
            unreachable!("an integer state");
        };
        let value: i128 = data[index].into();
        *sum = sum.checked_add(value).ok_or_else(|| {
            Error::new(
                ErrorKind::OutOfRange,
                format!("Overflow in HUGEINT addition: {sum} + {value}"),
            )
        })?;
        *seen += 1;
    }
    Ok(())
}

/// The sort key both `min`/`max` and DISTINCT compare values by.
pub fn value_keys(input: &Vector, count: usize) -> Result<Vec<Vec<u8>>> {
    let modifiers = OrderModifiers {
        order: OrderType::Ascending,
        nulls: NullOrder::NullsLast,
    };
    create_sort_keys(&[(input, modifiers)], count)
}

impl AggregateFunction {
    /// `update`: adds `rows` (row of `input`, group) to the states of their groups. Rows are
    /// given explicitly because a FILTER or DISTINCT may skip some.
    pub fn update(
        &self,
        input: Option<&Vector>,
        count: usize,
        rows: &[(usize, usize)],
        states: &mut [AggregateState],
    ) -> Result<()> {
        use AggregateKind::*;
        let Some(input) = input else {
            // count_star counts rows, whatever they hold
            for &(_, group) in rows {
                if let AggregateState::Count(n) = &mut states[group] {
                    *n += 1;
                }
            }
            return Ok(());
        };
        match self.kind {
            CountStar => unreachable!("count_star takes no argument"),
            Count => {
                let u = input.to_unified(count);
                for &(row, group) in rows {
                    if u.is_valid(row)
                        && let AggregateState::Count(n) = &mut states[group]
                    {
                        *n += 1;
                    }
                }
            }
            SumInteger | SumDecimal | AvgInteger | AvgDecimal => match input.physical_type() {
                PhysicalType::Bool => add_integers::<bool>(input, count, rows, states)?,
                PhysicalType::Int16 => add_integers::<i16>(input, count, rows, states)?,
                PhysicalType::Int32 => add_integers::<i32>(input, count, rows, states)?,
                PhysicalType::Int64 => add_integers::<i64>(input, count, rows, states)?,
                PhysicalType::Int128 => add_integers::<i128>(input, count, rows, states)?,
                other => unreachable!("integer aggregate over {other}"),
            },
            SumDouble | AvgDouble => {
                let u = input.to_unified(count);
                let data = u.data::<f64>();
                for &(row, group) in rows {
                    let index = u.sel.get(row);
                    if u.validity.is_valid(index)
                        && let AggregateState::Double { sum, count: seen } = &mut states[group]
                    {
                        *sum += data[index];
                        *seen += 1;
                    }
                }
            }
            Min | Max => {
                let keys = value_keys(input, count)?;
                let u = input.to_unified(count);
                for &(row, group) in rows {
                    if !u.is_valid(row) {
                        continue;
                    }
                    let AggregateState::Extreme(current) = &mut states[group] else {
                        unreachable!("a min/max state");
                    };
                    let better = current.as_ref().is_none_or(|(key, _)| {
                        if self.kind == Min {
                            keys[row] < *key
                        } else {
                            keys[row] > *key
                        }
                    });
                    if better {
                        *current = Some((keys[row].clone(), input.get_value(row)));
                    }
                }
            }
            First | Last | AnyValue => {
                for &(row, group) in rows {
                    let AggregateState::Value(current) = &mut states[group] else {
                        unreachable!("a value state");
                    };
                    let take = match self.kind {
                        First => current.is_none(),
                        Last => true,
                        // any_value skips NULLs
                        _ => current.as_ref().is_none_or(Value::is_null),
                    };
                    if take {
                        *current = Some(input.get_value(row));
                    }
                }
            }
        }
        Ok(())
    }

    /// `finalize`: the result of a state, of the bound `return_type`.
    pub fn finalize(
        &self,
        state: &AggregateState,
        return_type: &LogicalType,
        input: Option<&LogicalType>,
    ) -> Result<Value> {
        use AggregateKind::*;
        let null = || Value::null(return_type.clone());
        Ok(match (self.kind, state) {
            (_, AggregateState::Count(n)) => Value::bigint(*n),
            (_, AggregateState::Integer { count: 0, .. } | AggregateState::Double { count: 0, .. }) => null(),
            (SumInteger, AggregateState::Integer { sum, .. }) => Value::hugeint(*sum),
            (SumDecimal, AggregateState::Integer { sum, .. }) => {
                let max_width = duckrs_common::types::decimal::MAX_WIDTH;
                let (_, scale) = return_type.decimal_properties().expect("a DECIMAL result");
                if sum.unsigned_abs() >= 10u128.pow(max_width as u32) {
                    return Err(Error::new(
                        ErrorKind::OutOfRange,
                        format!("Overflow in SUM of DECIMAL: the result does not fit in DECIMAL({max_width})"),
                    ));
                }
                Value::decimal(*sum, max_width, scale)?
            }
            (AvgInteger, AggregateState::Integer { sum, count }) => Value::double(int_to_double(*sum) / *count as f64),
            (AvgDecimal, AggregateState::Integer { sum, count }) => {
                let (_, scale) = input.and_then(|t| t.decimal_properties()).expect("a DECIMAL argument");
                let divisor = *count as f64 * uhuge_to_double(10u128.pow(scale as u32));
                Value::double(int_to_double(*sum) / divisor)
            }
            (SumDouble, AggregateState::Double { sum, .. }) => Value::double(*sum),
            (AvgDouble, AggregateState::Double { sum, count }) => Value::double(*sum / *count as f64),
            (_, AggregateState::Extreme(Some((_, value)))) | (_, AggregateState::Value(Some(value))) => value.clone(),
            (_, AggregateState::Extreme(None) | AggregateState::Value(None)) => null(),
            (kind, state) => unreachable!("{kind:?} with state {state:?}"),
        })
    }
}
