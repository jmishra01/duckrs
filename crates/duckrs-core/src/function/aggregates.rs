// Ported from duckdb/src/function/aggregate/distributive/{count,minmax,first_last_any}.cpp and
// duckdb/extension/core_functions/aggregate/{distributive/sum,algebraic/avg}.cpp
// @ rust-port-baseline
//
//! The aggregate functions: `count`, `count_star`, `sum`, `avg`, `min`, `max`, `first`, `last`
//! and `any_value`. A function has a state per group; `update` feeds it a chunk's rows and
//! `finalize` turns the states into a result column.

use super::cast::{int_to_double, uhuge_to_double};
use super::holistic::{Holistic, finish};
use duckrs_common::sort_key::{NullOrder, OrderModifiers, OrderType, create_sort_keys};
use duckrs_common::types::{LogicalType, LogicalTypeId, NativeType, PhysicalType, bignum};
use duckrs_common::value::{Value, ValueData};
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
    SumBignum,
    List,
    AvgInteger,
    AvgDouble,
    AvgDecimal,
    Min,
    Max,
    First,
    Last,
    AnyValue,
    Holistic(Holistic),
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
            (AggregateKind::List, Some(ty)) => LogicalType::list(ty.clone()),
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
        let bignum = LogicalType::new(LogicalTypeId::Bignum);
        overloads.push((vec![bignum.clone()], bignum, SumBignum));
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
        AggregateFunctionSet::new("list", unary(List)),
        AggregateFunctionSet::new("array_agg", unary(List)),
        AggregateFunctionSet::new("first", unary(First)),
        AggregateFunctionSet::new("arbitrary", unary(First)),
        AggregateFunctionSet::new("last", unary(Last)),
        AggregateFunctionSet::new("any_value", unary(AnyValue)),
    ]
    .into_iter()
    .chain(holistic_functions())
    .collect()
}

fn holistic_functions() -> Vec<AggregateFunctionSet> {
    use Holistic::*;
    use LogicalType as L;
    let any = || LogicalType::new(LogicalTypeId::Any);
    let (v, d, b) = (L::VARCHAR, L::DOUBLE, L::BOOLEAN);
    let kind = AggregateKind::Holistic;
    let of = |names: &[&str], overloads: Vec<(Vec<LogicalType>, LogicalType, AggregateKind)>| {
        names
            .iter()
            .map(|name| AggregateFunctionSet::new(name, overloads.clone()))
            .collect::<Vec<_>>()
    };
    let unary_double = |h| vec![(vec![d.clone()], d.clone(), kind(h))];
    let binary_double = |h| vec![(vec![d.clone(), d.clone()], d.clone(), kind(h))];
    let same = |h| vec![(vec![any()], any(), kind(h))];
    let quantile = |h| vec![(vec![any()], any(), kind(h)), (vec![any(), any()], any(), kind(h))];
    let groups = vec![
        of(
            &["string_agg", "listagg", "group_concat"],
            vec![
                (vec![v.clone()], v.clone(), kind(StringAgg)),
                (vec![v.clone(), v.clone()], v.clone(), kind(StringAgg)),
            ],
        ),
        of(&["bool_and"], vec![(vec![b.clone()], b.clone(), kind(BoolAnd))]),
        of(&["bool_or"], vec![(vec![b.clone()], b.clone(), kind(BoolOr))]),
        of(&["bit_and"], same(BitAnd)),
        of(&["bit_or"], same(BitOr)),
        of(&["bit_xor"], same(BitXor)),
        of(&["product"], unary_double(Product)),
        of(&["mode"], same(Mode)),
        of(&["median"], same(Median)),
        of(&["quantile_cont"], quantile(QuantileCont)),
        of(
            &["quantile_disc", "quantile", "approx_quantile", "reservoir_quantile"],
            quantile(QuantileDisc),
        ),
        of(&["mad"], unary_double(Mad)),
        of(&["stddev_samp", "stddev"], unary_double(StddevSamp)),
        of(&["stddev_pop"], unary_double(StddevPop)),
        of(&["var_samp", "variance"], unary_double(VarSamp)),
        of(&["var_pop"], unary_double(VarPop)),
        of(&["skewness"], unary_double(Skewness)),
        of(&["kurtosis"], unary_double(Kurtosis)),
        of(&["entropy"], vec![(vec![any()], d.clone(), kind(Entropy))]),
        of(
            &["arg_min", "argmin", "min_by"],
            vec![(vec![any(), any()], any(), kind(ArgMin))],
        ),
        of(
            &["arg_max", "argmax", "max_by"],
            vec![(vec![any(), any()], any(), kind(ArgMax))],
        ),
        of(
            &["count_if", "countif"],
            vec![(vec![b.clone()], L::HUGEINT, kind(CountIf))],
        ),
        of(&["histogram"], same(Histogram)),
        of(
            &["approx_count_distinct"],
            vec![(vec![any()], L::BIGINT, kind(ApproxCountDistinct))],
        ),
        of(&["fsum", "sumkahan", "kahan_sum"], unary_double(Fsum)),
        of(&["favg"], unary_double(Favg)),
        of(&["geomean", "geometric_mean"], unary_double(GeoMean)),
        of(&["corr"], binary_double(Corr)),
        of(&["covar_pop"], binary_double(CovarPop)),
        of(&["covar_samp"], binary_double(CovarSamp)),
        of(
            &["regr_count"],
            vec![(vec![d.clone(), d.clone()], L::UINTEGER, kind(RegrCount))],
        ),
        of(&["regr_slope"], binary_double(RegrSlope)),
        of(&["regr_intercept"], binary_double(RegrIntercept)),
        of(&["regr_r2"], binary_double(RegrR2)),
        of(&["regr_avgx"], binary_double(RegrAvgX)),
        of(&["regr_avgy"], binary_double(RegrAvgY)),
        of(&["regr_sxx"], binary_double(RegrSxx)),
        of(&["regr_syy"], binary_double(RegrSyy)),
        of(&["regr_sxy"], binary_double(RegrSxy)),
    ];
    groups.into_iter().flatten().collect()
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
    /// the values, NULLs included; `None` before the first row
    List(Option<Vec<Value>>),
    /// what a [`Holistic`] aggregate computes its result from
    Rows(Vec<Value>),
}

impl AggregateKind {
    /// `StateInitialize`.
    pub fn initial_state(self) -> AggregateState {
        use AggregateKind::*;
        match self {
            Holistic(_) => AggregateState::Rows(Vec::new()),
            CountStar | Count => AggregateState::Count(0),
            SumInteger | SumDecimal | AvgInteger | AvgDecimal => AggregateState::Integer { sum: 0, count: 0 },
            SumDouble | AvgDouble => AggregateState::Double { sum: 0.0, count: 0 },
            Min | Max => AggregateState::Extreme(None),
            First | Last | AnyValue | SumBignum => AggregateState::Value(None),
            List => AggregateState::List(None),
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

/// `BignumOperation`: the running sum of `sum` over BIGNUM.
fn add_bignums(left: &Value, right: &Value) -> Result<Value> {
    let (ValueData::Bytes(a), ValueData::Bytes(b)) = (left.data(), right.data()) else {
        unreachable!("a BIGNUM is stored as bytes");
    };
    let sum = bignum::add(a, b).map_err(|message| Error::new(ErrorKind::OutOfRange, message))?;
    Value::new(left.logical_type().clone(), ValueData::Bytes(sum))
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
            Holistic(_) => {
                for &(row, group) in rows {
                    let AggregateState::Rows(state) = &mut states[group] else {
                        unreachable!("a holistic state");
                    };
                    state.push(input.get_value(row));
                }
            }
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
            List => {
                for &(row, group) in rows {
                    let AggregateState::List(state) = &mut states[group] else {
                        unreachable!("a list state");
                    };
                    state.get_or_insert_with(Vec::new).push(input.get_value(row));
                }
            }
            SumBignum => {
                for &(row, group) in rows {
                    let value = input.get_value(row);
                    if value.is_null() {
                        continue;
                    }
                    let AggregateState::Value(state) = &mut states[group] else {
                        unreachable!("a value state");
                    };
                    *state = Some(match state.take() {
                        Some(sum) => add_bignums(&sum, &value)?,
                        None => value,
                    });
                }
            }
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
            (Holistic(kind), AggregateState::Rows(rows)) => finish(kind, rows, return_type)?,
            (_, AggregateState::Count(n)) => Value::bigint(*n),
            (_, AggregateState::List(None)) => null(),
            (_, AggregateState::List(Some(values))) => {
                Value::new(return_type.clone(), ValueData::List(values.clone()))?
            }
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
