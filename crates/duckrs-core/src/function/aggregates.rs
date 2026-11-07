// Ported from duckdb/src/function/aggregate/distributive/{count,minmax,first_last_any}.cpp and
// duckdb/extension/core_functions/aggregate/{distributive/sum,algebraic/avg}.cpp
// @ rust-port-baseline
//
//! The aggregate functions: `count`, `count_star`, `sum`, `avg`, `min`, `max`, `first`, `last`
//! and `any_value`. A function has a state per group; `update` feeds it a chunk's rows and
//! `finalize` turns the states into a result column.

use super::cast::{int_to_double, uhuge_to_double};
use super::holistic::{Holistic, finish};
use duckrs_common::serializer::{BinaryDeserializer, BinarySerializer, SerializationOptions};
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
    /// `EXPORT_STATE`: the result is the state, of this type
    pub export: Option<LogicalType>,
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
                    export: None,
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
        overloads.push((
            vec![LogicalType::INTERVAL],
            LogicalType::INTERVAL,
            AggregateKind::Holistic(super::holistic::Holistic::AvgInterval),
        ));
        for ty in [LogicalType::TIMESTAMP, LogicalType::new(LogicalTypeId::TimestampTz)] {
            overloads.push((
                vec![ty.clone()],
                ty,
                AggregateKind::Holistic(super::holistic::Holistic::AvgTimestamp),
            ));
        }
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
        AggregateFunctionSet::new("sum_no_overflow", sum()),
        AggregateFunctionSet::new("min", with_count(unary(Min), super::holistic::Holistic::MinN)),
        AggregateFunctionSet::new("max", with_count(unary(Max), super::holistic::Holistic::MaxN)),
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

/// `min(x, n)`: the overload beside `min(x)`.
fn with_count(
    mut overloads: Vec<(Vec<LogicalType>, LogicalType, AggregateKind)>,
    kind: Holistic,
) -> Vec<(Vec<LogicalType>, LogicalType, AggregateKind)> {
    let any = || LogicalType::new(LogicalTypeId::Any);
    overloads.push((vec![any(), LogicalType::BIGINT], any(), AggregateKind::Holistic(kind)));
    overloads
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
                (vec![any()], v.clone(), kind(StringAgg)),
                (vec![any(), v.clone()], v.clone(), kind(StringAgg)),
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
        of(&["quantile_disc", "quantile"], quantile(QuantileDisc)),
        of(
            &["approx_quantile", "reservoir_quantile"],
            vec![(vec![any(), any()], any(), kind(QuantileDisc))],
        ),
        of(&["mad"], unary_double(Mad)),
        of(&["stddev_samp", "stddev"], unary_double(StddevSamp)),
        of(&["stddev_pop"], unary_double(StddevPop)),
        of(&["var_samp", "variance"], unary_double(VarSamp)),
        of(&["var_pop"], unary_double(VarPop)),
        of(&["skewness"], unary_double(Skewness)),
        of(&["kurtosis"], unary_double(Kurtosis)),
        of(&["kurtosis_pop"], unary_double(KurtosisPop)),
        of(&["weighted_avg", "wavg"], binary_double(WeightedAvg)),
        of(&["sem"], unary_double(Sem)),
        of(&["entropy"], vec![(vec![any()], d.clone(), kind(Entropy))]),
        of(
            &["arg_min", "argmin", "min_by"],
            vec![
                (vec![any(), any()], any(), kind(ArgMin)),
                (vec![any(), any(), any()], any(), kind(ArgMinN)),
            ],
        ),
        of(
            &["arg_max", "argmax", "max_by"],
            vec![
                (vec![any(), any()], any(), kind(ArgMax)),
                (vec![any(), any(), any()], any(), kind(ArgMaxN)),
            ],
        ),
        of(&["arg_min_null"], vec![(vec![any(), any()], any(), kind(ArgMinNull))]),
        of(&["arg_max_null"], vec![(vec![any(), any()], any(), kind(ArgMaxNull))]),
        of(
            &["arg_min_nulls_last"],
            vec![(vec![any(), any()], any(), kind(ArgMinNullsLast))],
        ),
        of(
            &["arg_max_nulls_last"],
            vec![(vec![any(), any()], any(), kind(ArgMaxNullsLast))],
        ),
        of(
            &["combine_aggr"],
            vec![
                (vec![any()], any(), kind(CombineAggr)),
                (vec![any(), L::BIGINT], any(), kind(CombineAggr)),
            ],
        ),
        of(&["approx_top_k"], vec![(vec![any(), any()], any(), kind(ApproxTopK))]),
        of(
            &["bitstring_agg"],
            vec![
                (vec![any()], L::new(LogicalTypeId::Bit), kind(BitstringAgg)),
                (
                    vec![any(), any(), any()],
                    L::new(LogicalTypeId::Bit),
                    kind(BitstringAgg),
                ),
            ],
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

/// A state as bytes, the value of an `AGGREGATE_STATE`: a tag and what the state holds.
fn encode_state(state: &AggregateState) -> Result<Vec<u8>> {
    let (tag, first, second) = match state {
        AggregateState::Count(n) => (0, Value::bigint(*n), Value::sql_null()),
        AggregateState::Integer { sum, count } => (1, Value::hugeint(*sum), Value::ubigint(*count)),
        AggregateState::Double { sum, count } => (2, Value::double(*sum), Value::ubigint(*count)),
        AggregateState::Extreme(Some((key, value))) => (3, Value::blob(key.clone()), value.clone()),
        AggregateState::Extreme(None) => (4, Value::sql_null(), Value::sql_null()),
        AggregateState::Value(Some(value)) => (5, value.clone(), Value::sql_null()),
        AggregateState::Value(None) => (6, Value::sql_null(), Value::sql_null()),
        AggregateState::List(Some(values)) => (7, values_list(values)?, Value::sql_null()),
        AggregateState::List(None) => (8, Value::sql_null(), Value::sql_null()),
        AggregateState::Rows(rows) => (9, values_list(rows)?, Value::sql_null()),
    };
    let fields = vec![
        ("tag".to_string(), Value::utinyint(tag)),
        ("first".to_string(), first),
        ("second".to_string(), second),
    ];
    Ok(BinarySerializer::serialize(
        &Value::struct_value(fields),
        SerializationOptions::default(),
    ))
}

fn values_list(values: &[Value]) -> Result<Value> {
    let child = values
        .first()
        .map_or(LogicalType::SQLNULL, |v| v.logical_type().clone());
    Value::list(child, values.to_vec())
}

fn state_failure() -> Error {
    Error::new(ErrorKind::InvalidInput, "Invalid aggregate state")
}

/// The state that [`encode_state`] wrote.
fn decode_state(bytes: &[u8]) -> Result<AggregateState> {
    let value: Value = BinaryDeserializer::deserialize(bytes)?;
    let ValueData::Struct(fields) = value.data() else {
        return Err(state_failure());
    };
    let [tag, first, second] = fields.as_slice() else {
        return Err(state_failure());
    };
    let list = |value: &Value| -> Result<Vec<Value>> {
        match value.data() {
            ValueData::List(items) => Ok(items.clone()),
            _ => Err(state_failure()),
        }
    };
    let count = || second.as_i128().unwrap_or(0) as u64;
    Ok(match tag.as_i128() {
        Some(0) => AggregateState::Count(first.as_i128().unwrap_or(0) as i64),
        Some(1) => AggregateState::Integer {
            sum: first.as_i128().unwrap_or(0),
            count: count(),
        },
        Some(2) => AggregateState::Double {
            sum: match first.data() {
                ValueData::Double(sum) => *sum,
                _ => return Err(state_failure()),
            },
            count: count(),
        },
        Some(3) => match first.data() {
            ValueData::Bytes(key) => AggregateState::Extreme(Some((key.clone(), second.clone()))),
            _ => return Err(state_failure()),
        },
        Some(4) => AggregateState::Extreme(None),
        Some(5) => AggregateState::Value(Some(first.clone())),
        Some(6) => AggregateState::Value(None),
        Some(7) => AggregateState::List(Some(list(first)?)),
        Some(8) => AggregateState::List(None),
        Some(9) => AggregateState::Rows(list(first)?),
        _ => return Err(state_failure()),
    })
}

/// `StateCombine`
fn combine_states(kind: AggregateKind, left: AggregateState, right: AggregateState) -> Result<AggregateState> {
    use AggregateState as S;
    let overflow = || Error::new(ErrorKind::OutOfRange, "Overflow in aggregate state combine");
    Ok(match (left, right) {
        (S::Count(a), S::Count(b)) => S::Count(a.checked_add(b).ok_or_else(overflow)?),
        (S::Integer { sum: a, count: x }, S::Integer { sum: b, count: y }) => S::Integer {
            sum: a.checked_add(b).ok_or_else(overflow)?,
            count: x.checked_add(y).ok_or_else(overflow)?,
        },
        (S::Double { sum: a, count: x }, S::Double { sum: b, count: y }) => S::Double {
            sum: a + b,
            count: x.checked_add(y).ok_or_else(overflow)?,
        },
        (S::Extreme(None), other) | (other, S::Extreme(None)) => other,
        (S::Extreme(Some(a)), S::Extreme(Some(b))) => {
            let keep_left = if kind == AggregateKind::Min {
                a.0 <= b.0
            } else {
                a.0 >= b.0
            };
            S::Extreme(Some(if keep_left { a } else { b }))
        }
        (S::Value(None), other) | (other, S::Value(None)) => other,
        (S::Value(Some(a)), S::Value(Some(b))) => match kind {
            AggregateKind::Last => S::Value(Some(b)),
            AggregateKind::SumBignum => {
                return Err(crate::unported("combining states of the sum of BIGNUMs"));
            }
            _ => S::Value(Some(a)),
        },
        (S::List(None), other) | (other, S::List(None)) => other,
        (S::List(Some(mut a)), S::List(Some(b))) => {
            a.extend(b);
            S::List(Some(a))
        }
        (S::Rows(mut a), S::Rows(b)) => {
            a.extend(b);
            S::Rows(a)
        }
        _ => return Err(state_failure()),
    })
}

/// The aggregate an `AGGREGATE_STATE` type is of.
fn state_function(ty: &LogicalType) -> Result<Arc<AggregateFunction>> {
    let (name, overload, ..) = ty.aggregate_state_info().ok_or_else(state_failure)?;
    let set = super::BuiltinFunctions::get()
        .aggregate(name)
        .ok_or_else(state_failure)?;
    set.functions.get(overload as usize).cloned().ok_or_else(state_failure)
}

fn state_bytes(value: &Value) -> Result<&[u8]> {
    match value.data() {
        ValueData::Bytes(bytes) => Ok(bytes),
        _ => Err(state_failure()),
    }
}

/// `finalize`: the result of the aggregate from a state.
pub fn finalize_state(value: &Value, ty: &LogicalType) -> Result<Value> {
    let (.., arguments, result) = ty.aggregate_state_info().ok_or_else(state_failure)?;
    if value.is_null() {
        return Ok(Value::null(result.clone()));
    }
    let function = state_function(ty)?;
    function.finalize(&decode_state(state_bytes(value)?)?, result, arguments.first())
}

/// `combine` and `combine_aggr`: a state of the rows of two others.
pub fn combine_state_values(left: &Value, right: &Value, ty: &LogicalType) -> Result<Value> {
    if left.is_null() {
        return Ok(right.clone());
    }
    if right.is_null() {
        return Ok(left.clone());
    }
    let function = state_function(ty)?;
    let combined = combine_states(
        function.kind,
        decode_state(state_bytes(left)?)?,
        decode_state(state_bytes(right)?)?,
    )?;
    Value::new(ty.clone(), ValueData::Bytes(encode_state(&combined)?))
}

/// A state combined with itself `times` times, by doubling. States that keep their rows can't
/// hold more than this many.
const MAX_REPEATED_ROWS: u128 = 100_000_000;

pub fn repeat_state(state: &Value, times: i128, ty: &LogicalType) -> Result<Value> {
    if times == 0 || state.is_null() {
        return Ok(Value::null(ty.clone()));
    }
    if let AggregateState::Rows(rows) = decode_state(state_bytes(state)?)?
        && (rows.len() as u128).saturating_mul(times as u128) > MAX_REPEATED_ROWS
    {
        return Err(Error::new(ErrorKind::OutOfRange, "Overflow in aggregate state combine"));
    }
    let mut result = Value::null(ty.clone());
    let mut power = state.clone();
    let mut left = times;
    while left > 0 {
        if left & 1 == 1 {
            result = combine_state_values(&result, &power, ty)?;
        }
        left >>= 1;
        if left > 0 {
            power = combine_state_values(&power, &power, ty)?;
        }
    }
    Ok(result)
}

/// `ExportAggregateFunction::Bind`: the aggregate gives its state.
pub fn export_state(
    function: &AggregateFunction,
    return_type: &LogicalType,
    arguments: &[LogicalType],
) -> Result<(AggregateFunction, LogicalType)> {
    let not_exportable = || {
        Error::new(
            ErrorKind::Binder,
            format!("Cannot use EXPORT_STATE for non-combinable function {}", function.name),
        )
    };
    let set = super::BuiltinFunctions::get()
        .aggregate(&function.name)
        .ok_or_else(not_exportable)?;
    let overload = set
        .functions
        .iter()
        .position(|f| f.kind == function.kind)
        .ok_or_else(not_exportable)?;
    let ty = LogicalType::aggregate_state(&function.name, overload as u32, arguments.to_vec(), return_type.clone());
    let exported = AggregateFunction {
        name: function.name.clone(),
        arguments: function.arguments.clone(),
        return_type: ty.clone(),
        kind: function.kind,
        export: Some(ty.clone()),
    };
    Ok((exported, ty))
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
        self.update_keyed(input, count, rows, states, None)
    }

    /// `update`, with the `value_keys` of `input` when the caller has them. A window updates one
    /// state many times from the same input, so `min`/`max` must not encode all of it each time.
    pub fn update_keyed(
        &self,
        input: Option<&Vector>,
        count: usize,
        rows: &[(usize, usize)],
        states: &mut [AggregateState],
        keys: Option<&[Vec<u8>]>,
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
                let encoded;
                let keys = match keys {
                    Some(keys) => keys,
                    None => {
                        encoded = value_keys(input, count)?;
                        &encoded
                    }
                };
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
        if self.export.is_some() {
            return Value::new(return_type.clone(), ValueData::Bytes(encode_state(state)?));
        }
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
