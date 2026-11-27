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
use duckrs_common::vector::{SelectionVector, Vector};
use duckrs_common::{Error, ErrorKind, Result};
use std::fmt;
use std::sync::Arc;

/// What an aggregate computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateKind {
    CountStar,
    Count,
    SumInteger,
    /// `sum_no_overflow` of INTEGER or BIGINT: an `int64_t` accumulator (`IntegerSumOperation`)
    SumNoOverflow,
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
    /// a user-defined aggregate ([`AggregateFunction::user`])
    User,
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
    /// the callbacks of a user-defined aggregate ([`AggregateKind::User`])
    pub user: Option<super::udf::UserAggregate>,
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
                    user: None,
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
        // TIME averages like the timestamps (`DiscreteAverageOperation`), without infinities
        for ty in [
            LogicalType::TIMESTAMP,
            LogicalType::new(LogicalTypeId::TimestampTz),
            LogicalType::new(LogicalTypeId::TimestampNs),
            LogicalType::new(LogicalTypeId::TimestampTzNs),
            LogicalType::TIME,
        ] {
            overloads.push((
                vec![ty.clone()],
                ty,
                AggregateKind::Holistic(super::holistic::Holistic::AvgTimestamp),
            ));
        }
        let time_tz = LogicalType::new(LogicalTypeId::TimeTz);
        overloads.push((
            vec![time_tz.clone()],
            time_tz,
            AggregateKind::Holistic(super::holistic::Holistic::AvgTimeTz),
        ));
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
        // `SumNoOverflowFun`: INTEGER, BIGINT and DECIMAL
        AggregateFunctionSet::new(
            "sum_no_overflow",
            vec![
                (vec![LogicalType::INTEGER], LogicalType::HUGEINT, SumNoOverflow),
                (vec![LogicalType::BIGINT], LogicalType::HUGEINT, SumNoOverflow),
                (vec![decimal()], decimal(), SumDecimal),
            ],
        ),
        AggregateFunctionSet::new(
            "variant_group_object",
            vec![(
                vec![LogicalType::VARCHAR, LogicalType::new(LogicalTypeId::Variant)],
                LogicalType::new(LogicalTypeId::Variant),
                AggregateKind::Holistic(super::holistic::Holistic::VariantGroupObject),
            )],
        ),
        AggregateFunctionSet::new(
            "decimal_average",
            vec![(
                vec![decimal()],
                decimal(),
                AggregateKind::Holistic(super::holistic::Holistic::DecimalAverage),
            )],
        ),
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
            &["approx_quantile"],
            vec![(vec![any(), any()], any(), kind(ApproxQuantile))],
        ),
        of(
            &["reservoir_quantile"],
            vec![(vec![any(), any()], any(), kind(QuantileDisc))],
        ),
        of(&["mad"], {
            // `MadFun::GetFunctions`: the temporal types deviate by an INTERVAL
            let mut overloads = unary_double(Mad);
            for ty in [
                L::DATE,
                L::TIMESTAMP,
                L::TIME,
                LogicalType::new(LogicalTypeId::TimestampTz),
                LogicalType::new(LogicalTypeId::TimeTz),
            ] {
                overloads.push((vec![ty], L::INTERVAL, kind(Mad)));
            }
            overloads
        }),
        of(&["stddev_samp", "stddev"], unary_double(StddevSamp)),
        of(&["stddev_pop"], unary_double(StddevPop)),
        of(&["var_samp", "variance"], unary_double(VarSamp)),
        of(&["var_pop"], unary_double(VarPop)),
        of(&["skewness"], unary_double(Skewness)),
        of(&["kurtosis"], unary_double(Kurtosis)),
        of(&["kurtosis_pop"], unary_double(KurtosisPop)),
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
            vec![
                (vec![any(), any()], any(), kind(ArgMinNullsLast)),
                (vec![any(), any(), any()], any(), kind(ArgMinNullsLastN)),
            ],
        ),
        of(
            &["arg_max_nulls_last"],
            vec![
                (vec![any(), any()], any(), kind(ArgMaxNullsLast)),
                (vec![any(), any(), any()], any(), kind(ArgMaxNullsLastN)),
            ],
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
        of(
            &["histogram"],
            vec![
                (vec![any()], any(), kind(Histogram)),
                (vec![any(), any()], any(), kind(HistogramBins)),
            ],
        ),
        of(
            &["histogram_exact"],
            vec![(vec![any(), any()], any(), kind(HistogramExact))],
        ),
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
    /// the sort key of the current minimum or maximum, and its value; behind a pointer, like the
    /// value below, so that a state is no larger than a sum, which is what most states are
    Extreme(Option<Box<(Vec<u8>, Value)>>),
    /// whether a value was taken, and the value
    Value(Option<Box<Value>>),
    /// the values, NULLs included; `None` before the first row
    List(Option<Vec<Value>>),
    /// what a [`Holistic`] aggregate computes its result from
    Rows(Vec<Value>),
    /// the state of a [`Holistic`] aggregate as C++ keeps it, read from an `AGGREGATE_STATE`
    /// ([`super::holistic_states`]), as a value of its layout
    Exported(Box<Value>),
    /// the state of a user-defined aggregate, `None` until the group's first row
    User(Option<super::udf::UserAggregateState>),
}

impl AggregateKind {
    /// Whether threads gain from seeing the rows in parts: the result can't depend on their order,
    /// as `first` and `list` do, and the work is done on the way in. The aggregates that keep their
    /// rows do theirs at the end, so threads would only add merging.
    pub fn is_order_independent(self) -> bool {
        use AggregateKind::*;
        matches!(
            self,
            CountStar
                | Count
                | SumInteger
                | SumNoOverflow
                | SumDouble
                | SumDecimal
                | AvgInteger
                | AvgDouble
                | AvgDecimal
                | Min
                | Max
        )
    }

    /// `StateInitialize`.
    pub fn initial_state(self) -> AggregateState {
        use AggregateKind::*;
        match self {
            Holistic(_) => AggregateState::Rows(Vec::new()),
            CountStar | Count => AggregateState::Count(0),
            SumInteger | SumNoOverflow | SumDecimal | AvgInteger | AvgDecimal => {
                AggregateState::Integer { sum: 0, count: 0 }
            }
            SumDouble | AvgDouble => AggregateState::Double { sum: 0.0, count: 0 },
            Min | Max => AggregateState::Extreme(None),
            First | Last | AnyValue | SumBignum => AggregateState::Value(None),
            List => AggregateState::List(None),
            User => AggregateState::User(None),
        }
    }
}

// The state layouts below are ported from the `STATE_TYPE`s of duckdb/src/function/aggregate/
// distributive/{count,minmax,first_last_any}.cpp, extension/core_functions/aggregate/
// {distributive/sum.cpp,algebraic/avg.cpp} and duckdb/src/include/duckdb/function/aggregate/
// list_aggregate.hpp, and the field rules of aggregate_state_layout.hpp @ rust-port-baseline

/// Whether `sum` or `avg` of `argument` keeps an `int64_t` (`SumState<int64_t>`,
/// `AvgState<int64_t>`) rather than a `hugeint_t`: for BOOLEAN and SMALLINT, and DECIMALs of
/// a SMALLINT.
fn narrow_sum(argument: Option<&LogicalType>) -> bool {
    argument.is_some_and(|ty| match ty.id() {
        LogicalTypeId::Boolean | LogicalTypeId::SmallInt => true,
        LogicalTypeId::Decimal => ty.physical_type() == duckrs_common::types::PhysicalType::Int16,
        _ => false,
    })
}

/// `GetStateType`: the type C++ exports the state of the aggregate as, for the aggregates whose
/// state duckrs keeps the way C++ does. The others (BIGNUM sums and the aggregates that keep
/// their rows) export a state of their own ([`encode_state`]).
pub fn state_layout(kind: AggregateKind, arguments: &[LogicalType], result: &LogicalType) -> Option<LogicalType> {
    use AggregateKind::*;
    let sum_type = || {
        if narrow_sum(arguments.first()) {
            LogicalType::BIGINT
        } else {
            LogicalType::HUGEINT
        }
    };
    let avg = |value: LogicalType| {
        LogicalType::struct_type(vec![("count".into(), LogicalType::UBIGINT), ("value".into(), value)])
    };
    Some(match kind {
        // a user-defined state is opaque
        User => return None,
        // `int64_t`
        CountStar | Count => LogicalType::BIGINT,
        // `OptionalStateType<T>`
        SumInteger | SumNoOverflow | SumDecimal => sum_type(),
        SumDouble => LogicalType::DOUBLE,
        // `StructStateType<uint64_t, T>`
        AvgInteger | AvgDecimal => avg(sum_type()),
        AvgDouble => avg(LogicalType::DOUBLE),
        // `OptionalStateType<StateTypedValue<T, StateReturnType>>`, or a sort key of it
        Min | Max => result.clone(),
        // `OptionalStateType<StructStateType<OptionalStateType<...>>>`
        First | Last | AnyValue => LogicalType::struct_type(vec![("value".into(), result.clone())]),
        // `StateListType<StateReturnType>`
        List => result.clone(),
        Holistic(kind) => return super::holistic_states::layout(kind, arguments, result),
        SumBignum => return None,
    })
}

fn is_avg(kind: AggregateKind) -> bool {
    matches!(
        kind,
        AggregateKind::AvgInteger | AggregateKind::AvgDecimal | AggregateKind::AvgDouble
    )
}

/// `AggregateStateSerialization::SerializeStates`: a state as a value of its layout
/// ([`state_layout`]); an unset optional state is NULL, and so is an empty list.
fn state_to_value(kind: AggregateKind, state: &AggregateState, ty: &LogicalType) -> Result<Value> {
    state_to_value_of(kind, state, ty, &ty.state_layout())
}

/// [`state_to_value`] with the layout of `ty` worked out already, for many states of one type.
fn state_to_value_of(
    kind: AggregateKind,
    state: &AggregateState,
    ty: &LogicalType,
    layout: &LogicalType,
) -> Result<Value> {
    use AggregateState as S;
    let value = match state {
        S::Count(n) => Value::bigint(*n),
        S::Integer { sum, count } if is_avg(kind) => {
            let value = layout.struct_children()[1].1.clone();
            Value::struct_value(vec![
                ("count".into(), Value::ubigint(*count)),
                ("value".into(), integer_of(*sum, &value)?),
            ])
        }
        S::Double { sum, count } if is_avg(kind) => Value::struct_value(vec![
            ("count".into(), Value::ubigint(*count)),
            ("value".into(), Value::double(*sum)),
        ]),
        S::Integer { count: 0, .. } | S::Double { count: 0, .. } => Value::null(layout.clone()),
        S::Integer { sum, .. } => integer_of(*sum, layout)?,
        S::Double { sum, .. } => Value::double(*sum),
        S::Extreme(Some(extreme)) => extreme.1.clone(),
        // any_value skips NULLs, so its state is still unset after one
        S::Value(Some(value)) if value.is_null() && kind == AggregateKind::AnyValue => Value::null(layout.clone()),
        S::Value(Some(value)) => Value::struct_value(vec![("value".into(), Value::clone(value))]),
        S::List(Some(values)) if !values.is_empty() => Value::new(layout.clone(), ValueData::List(values.clone()))?,
        S::Extreme(None) | S::Value(None) | S::List(_) => Value::null(layout.clone()),
        S::Rows(rows) => match kind {
            // the buffer of an ordered aggregate, in arrival order
            AggregateKind::Holistic(super::holistic::Holistic::Sorted) if rows.is_empty() => {
                Value::null(layout.clone())
            }
            AggregateKind::Holistic(super::holistic::Holistic::Sorted) => {
                Value::new(layout.clone(), ValueData::List(rows.clone()))?
            }
            AggregateKind::Holistic(holistic) => super::holistic_states::from_rows(holistic, rows, layout)?,
            _ => return Err(state_failure()),
        },
        S::Exported(value) => Value::clone(value),
        S::User(_) => return Err(state_failure()),
    };
    if value.is_null() {
        return Ok(Value::null(ty.clone()));
    }
    Value::new(ty.clone(), value.data().clone())
}

/// What a typed kernel below reads from one state: `None` for a state it does not handle (the
/// caller then goes a value at a time), else the value to store, `None` for NULL.
type TypedSlot<T> = Option<Result<Option<T>>>;

/// Stores `read` of each of `states` in the typed data of `column` from its first row on.
/// `column` is a flat vector of at least `states.len()` valid rows, as `Vector::new` makes. False,
/// with the validity untouched, when `read` does not handle a state: the per-value path then
/// writes every row again.
fn write_states_typed<T: NativeType>(
    states: &[AggregateState],
    column: &mut Vector,
    read: impl Fn(&AggregateState) -> TypedSlot<T>,
) -> Result<bool> {
    let mut nulls = Vec::new();
    {
        let out = column.data_mut::<T>();
        if out.len() < states.len() {
            return Ok(false);
        }
        for (row, (slot, state)) in out.iter_mut().zip(states).enumerate() {
            match read(state) {
                None => return Ok(false),
                Some(value) => match value? {
                    Some(value) => *slot = value,
                    None => nulls.push(row),
                },
            }
        }
    }
    let validity = column.validity_mut();
    for row in nulls {
        validity.set_invalid(row);
    }
    Ok(true)
}

/// [`state_to_value_of`] of each of `states` on the typed data of `column`, for the states whose
/// layout is a number: a count's (BIGINT) and a sum's (`OptionalStateType<T>`, NULL until a row
/// was added). False for the others.
fn export_states_typed(kind: AggregateKind, states: &[AggregateState], column: &mut Vector) -> Result<bool> {
    use AggregateKind::*;
    use AggregateState as S;
    match (kind, column.physical_type()) {
        (CountStar | Count, PhysicalType::Int64) => write_states_typed::<i64>(states, column, |state| match state {
            S::Count(n) => Some(Ok(Some(*n))),
            _ => None,
        }),
        (SumInteger | SumNoOverflow | SumDecimal, PhysicalType::Int128) => {
            write_states_typed::<i128>(states, column, |state| match state {
                S::Integer { count: 0, .. } => Some(Ok(None)),
                S::Integer { sum, .. } => Some(Ok(Some(*sum))),
                _ => None,
            })
        }
        (SumInteger | SumNoOverflow | SumDecimal, PhysicalType::Int64) => {
            write_states_typed::<i64>(states, column, |state| match state {
                S::Integer { count: 0, .. } => Some(Ok(None)),
                S::Integer { sum, .. } => Some(i64::try_from(*sum).map(Some).map_err(|_| {
                    Error::new(
                        ErrorKind::OutOfRange,
                        format!("Overflow in aggregate state: {sum} does not fit in a BIGINT"),
                    )
                })),
                _ => None,
            })
        }
        (SumDouble, PhysicalType::Double) => write_states_typed::<f64>(states, column, |state| match state {
            S::Double { count: 0, .. } => Some(Ok(None)),
            S::Double { sum, .. } => Some(Ok(Some(*sum))),
            _ => None,
        }),
        _ => Ok(false),
    }
}

/// [`AggregateFunction::finalize`] of each of `states` on the typed data of `column`, for counts,
/// sums and averages whose result is of the type `finalize` makes. False for the others.
fn finalize_states_typed(
    kind: AggregateKind,
    states: &[AggregateState],
    return_type: &LogicalType,
    input: Option<&LogicalType>,
    column: &mut Vector,
) -> Result<bool> {
    use AggregateKind::*;
    use AggregateState as S;
    let max_width = duckrs_common::types::decimal::MAX_WIDTH;
    match kind {
        CountStar | Count if *return_type == LogicalType::BIGINT => {
            write_states_typed::<i64>(states, column, |state| match state {
                S::Count(n) => Some(Ok(Some(*n))),
                _ => None,
            })
        }
        SumInteger | SumNoOverflow if *return_type == LogicalType::HUGEINT => {
            write_states_typed::<i128>(states, column, |state| match state {
                S::Integer { count: 0, .. } => Some(Ok(None)),
                S::Integer { sum, .. } => Some(Ok(Some(*sum))),
                _ => None,
            })
        }
        SumDecimal => {
            let Some((_, scale)) = return_type.decimal_properties() else {
                return Ok(false);
            };
            if *return_type != LogicalType::decimal(max_width, scale) || column.physical_type() != PhysicalType::Int128
            {
                return Ok(false);
            }
            let limit = 10u128.pow(max_width as u32);
            write_states_typed::<i128>(states, column, |state| match state {
                S::Integer { count: 0, .. } => Some(Ok(None)),
                S::Integer { sum, .. } if sum.unsigned_abs() >= limit => Some(Err(Error::new(
                    ErrorKind::OutOfRange,
                    format!("Overflow in SUM of DECIMAL: the result does not fit in DECIMAL({max_width})"),
                ))),
                S::Integer { sum, .. } => Some(Ok(Some(*sum))),
                _ => None,
            })
        }
        AvgInteger if *return_type == LogicalType::DOUBLE => {
            write_states_typed::<f64>(states, column, |state| match state {
                S::Integer { count: 0, .. } => Some(Ok(None)),
                S::Integer { sum, count } => Some(Ok(Some(int_to_double(*sum) / *count as f64))),
                _ => None,
            })
        }
        AvgDecimal if *return_type == LogicalType::DOUBLE => {
            let Some((_, scale)) = input.and_then(|t| t.decimal_properties()) else {
                return Ok(false);
            };
            let power = uhuge_to_double(10u128.pow(scale as u32));
            write_states_typed::<f64>(states, column, |state| match state {
                S::Integer { count: 0, .. } => Some(Ok(None)),
                S::Integer { sum, count } => Some(Ok(Some(int_to_double(*sum) / (*count as f64 * power)))),
                _ => None,
            })
        }
        SumDouble | AvgDouble if *return_type == LogicalType::DOUBLE => {
            let average = kind == AvgDouble;
            write_states_typed::<f64>(states, column, |state| match state {
                S::Double { count: 0, .. } => Some(Ok(None)),
                S::Double { sum, count } if average => Some(Ok(Some(*sum / *count as f64))),
                S::Double { sum, .. } => Some(Ok(Some(*sum))),
                _ => None,
            })
        }
        _ => Ok(false),
    }
}

/// [`value_to_state_of`] of the first `count` rows of `vector`, a vector of exported states whose
/// layout is a number ([`export_states_typed`]), read from its typed data; and the rows that are
/// NULL. `None` for the other states.
fn import_states_typed(
    kind: AggregateKind,
    vector: &Vector,
    count: usize,
) -> Option<(Vec<AggregateState>, Vec<usize>)> {
    use AggregateKind::*;
    use AggregateState as S;
    let unified = vector.to_unified(count);
    let mut nulls = Vec::new();
    let mut read = |make: &dyn Fn(usize) -> AggregateState, initial: AggregateState| -> Vec<AggregateState> {
        (0..count)
            .map(|row| {
                let at = unified.sel.get(row);
                if unified.validity.is_valid(at) {
                    make(at)
                } else {
                    nulls.push(row);
                    initial.clone()
                }
            })
            .collect()
    };
    let states = match (kind, vector.physical_type()) {
        (CountStar | Count, PhysicalType::Int64) => {
            let data = unified.data::<i64>();
            read(&|at| S::Count(data[at]), S::Count(0))
        }
        (SumInteger | SumNoOverflow | SumDecimal, PhysicalType::Int128) => {
            let data = unified.data::<i128>();
            read(
                &|at| S::Integer {
                    sum: data[at],
                    count: 1,
                },
                kind.initial_state(),
            )
        }
        (SumInteger | SumNoOverflow | SumDecimal, PhysicalType::Int64) => {
            let data = unified.data::<i64>();
            read(
                &|at| S::Integer {
                    sum: i128::from(data[at]),
                    count: 1,
                },
                kind.initial_state(),
            )
        }
        (SumDouble, PhysicalType::Double) => {
            let data = unified.data::<f64>();
            read(
                &|at| S::Double {
                    sum: data[at],
                    count: 1,
                },
                kind.initial_state(),
            )
        }
        _ => return None,
    };
    Some((states, nulls))
}

/// An integral state field of type `ty` (BIGINT or HUGEINT).
fn integer_of(value: i128, ty: &LogicalType) -> Result<Value> {
    if ty.id() == LogicalTypeId::BigInt {
        let value = i64::try_from(value).map_err(|_| {
            Error::new(
                ErrorKind::OutOfRange,
                format!("Overflow in aggregate state: {value} does not fit in a BIGINT"),
            )
        })?;
        return Ok(Value::bigint(value));
    }
    Ok(Value::hugeint(value))
}

/// `AggregateStateSerialization::DeserializeStates`: the state a value of its layout holds. A
/// NULL state, and a NULL field, leave the empty state as it is.
fn value_to_state(kind: AggregateKind, value: &Value, ty: &LogicalType) -> Result<AggregateState> {
    value_to_state_of(kind, value, &ty.state_layout())
}

/// [`value_to_state`] with the layout of the state type worked out already, for many values of one type.
fn value_to_state_of(kind: AggregateKind, value: &Value, layout: &LogicalType) -> Result<AggregateState> {
    use AggregateKind::*;
    use AggregateState as S;
    let mut state = kind.initial_state();
    if value.is_null() {
        return Ok(state);
    }
    // the value of the state, as a value of its layout rather than of the state type
    let value = &Value::new(layout.clone(), value.data().clone())?;
    if let Holistic(_) = kind {
        return Ok(S::Exported(Box::new(value.clone())));
    }
    let integer = |value: &Value| value.as_i128().ok_or_else(state_failure);
    let double = |value: &Value| match value.data() {
        ValueData::Double(d) => Ok(*d),
        _ => Err(state_failure()),
    };
    match (&mut state, value.data()) {
        (S::Count(n), _) => *n = i64::try_from(integer(value)?).map_err(|_| state_failure())?,
        (S::Integer { .. } | S::Double { .. }, ValueData::Struct(fields)) if is_avg(kind) => {
            let [seen, total] = fields.as_slice() else {
                return Err(state_failure());
            };
            let seen = if seen.is_null() {
                0
            } else {
                u64::try_from(integer(seen)?).map_err(|_| state_failure())?
            };
            match &mut state {
                S::Integer { sum, count } => {
                    *count = seen;
                    if !total.is_null() {
                        *sum = integer(total)?;
                    }
                }
                S::Double { sum, count } => {
                    *count = seen;
                    if !total.is_null() {
                        *sum = double(total)?;
                    }
                }
                _ => unreachable!("an average state"),
            }
        }
        (S::Integer { sum, count }, _) => {
            *sum = integer(value)?;
            *count = 1;
        }
        (S::Double { sum, count }, _) => {
            *sum = double(value)?;
            *count = 1;
        }
        (S::Extreme(current), _) => {
            let key = value_keys(&Vector::constant(value), 1)?
                .pop()
                .ok_or_else(state_failure)?;
            *current = Some(Box::new((key, value.clone())));
        }
        (S::Value(current), ValueData::Struct(fields)) if matches!(kind, First | Last | AnyValue) => {
            let [inner] = fields.as_slice() else {
                return Err(state_failure());
            };
            *current = Some(Box::new(inner.clone()));
        }
        (S::List(current), ValueData::List(items)) => *current = Some(items.clone()),
        _ => return Err(state_failure()),
    }
    Ok(state)
}

/// A state as bytes, the value of an `AGGREGATE_STATE`: a tag and what the state holds.
fn encode_state(state: &AggregateState) -> Result<Vec<u8>> {
    let (tag, first, second) = match state {
        AggregateState::Count(n) => (0, Value::bigint(*n), Value::sql_null()),
        AggregateState::Integer { sum, count } => (1, Value::hugeint(*sum), Value::ubigint(*count)),
        AggregateState::Double { sum, count } => (2, Value::double(*sum), Value::ubigint(*count)),
        AggregateState::Extreme(Some(extreme)) => (3, Value::blob(extreme.0.clone()), extreme.1.clone()),
        AggregateState::Extreme(None) => (4, Value::sql_null(), Value::sql_null()),
        AggregateState::Value(Some(value)) => (5, Value::clone(value), Value::sql_null()),
        AggregateState::Value(None) => (6, Value::sql_null(), Value::sql_null()),
        AggregateState::List(Some(values)) => (7, values_list(values)?, Value::sql_null()),
        AggregateState::List(None) => (8, Value::sql_null(), Value::sql_null()),
        AggregateState::Rows(rows) => (9, values_list(rows)?, Value::sql_null()),
        AggregateState::Exported(_) | AggregateState::User(_) => return Err(state_failure()),
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
            ValueData::Bytes(key) => AggregateState::Extreme(Some(Box::new((key.clone(), second.clone())))),
            _ => return Err(state_failure()),
        },
        Some(4) => AggregateState::Extreme(None),
        Some(5) => AggregateState::Value(Some(Box::new(first.clone()))),
        Some(6) => AggregateState::Value(None),
        Some(7) => AggregateState::List(Some(list(first)?)),
        Some(8) => AggregateState::List(None),
        Some(9) => AggregateState::Rows(list(first)?),
        _ => return Err(state_failure()),
    })
}

/// `StateCombine`
pub fn combine_states(kind: AggregateKind, left: AggregateState, right: AggregateState) -> Result<AggregateState> {
    use AggregateState as S;
    let overflow = || Error::new(ErrorKind::OutOfRange, "Overflow in aggregate state combine");
    if kind == AggregateKind::User {
        return super::udf::combine_user_states(left, right);
    }
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
    Ok(resolve_state(ty)?.0)
}

/// The aggregate an `AGGREGATE_STATE` type is of, and its result. A state read from a database file says only the
/// function's name and the types it was bound with, so the aggregate is bound again (`BindExportedAggregate`).
pub fn resolve_state(ty: &LogicalType) -> Result<(Arc<AggregateFunction>, LogicalType)> {
    use crate::planner::expression::BoundExpr;
    let (name, overload, arguments, result) = ty.aggregate_state_info().ok_or_else(state_failure)?;
    let set = super::BuiltinFunctions::get()
        .aggregate(name)
        .ok_or_else(state_failure)?;
    if overload != duckrs_common::types::UNRESOLVED_OVERLOAD {
        let function = set
            .functions
            .get(overload as usize)
            .cloned()
            .ok_or_else(state_failure)?;
        return Ok((function, result.clone()));
    }
    let constants = ty.aggregate_state_constants();
    let placeholders = arguments
        .iter()
        .enumerate()
        .map(|(at, argument)| match constants.get(at).cloned().flatten() {
            Some(constant) if argument.id() == LogicalTypeId::Any => BoundExpr::constant(constant),
            Some(constant) => BoundExpr::constant(constant).cast_to(argument, false),
            None if argument.id() == LogicalTypeId::Any => BoundExpr::constant(Value::sql_null()),
            None => BoundExpr::constant(Value::null(argument.clone())),
        })
        .collect();
    let bound = super::binder::bind_aggregate_function(set, placeholders, None, false)?;
    // an argument kept as ANY binds to NULL: the result of a state that is its result (or holds it) is its layout's
    let mut result = bound.return_type;
    if result.id() == LogicalTypeId::SqlNull {
        let layout = ty.state_layout();
        result = match bound.function.kind {
            AggregateKind::Min | AggregateKind::Max | AggregateKind::List => layout,
            AggregateKind::First | AggregateKind::Last | AggregateKind::AnyValue => layout
                .struct_children()
                .first()
                .map_or(result, |(_, child)| child.clone()),
            _ => result,
        };
    }
    Ok((bound.function, result))
}

fn state_bytes(value: &Value) -> Result<&[u8]> {
    match value.data() {
        ValueData::Bytes(bytes) => Ok(bytes),
        _ => Err(state_failure()),
    }
}

/// The state a value of the `AGGREGATE_STATE` type `ty` holds, in either form.
fn read_state(kind: AggregateKind, value: &Value, ty: &LogicalType) -> Result<AggregateState> {
    if ty.is_aggregate_state() {
        value_to_state(kind, value, ty)
    } else {
        decode_state(state_bytes(value)?)
    }
}

/// A state as a value of the `AGGREGATE_STATE` type `ty`, in its form.
pub fn write_state(kind: AggregateKind, state: &AggregateState, ty: &LogicalType) -> Result<Value> {
    if ty.is_aggregate_state() {
        state_to_value(kind, state, ty)
    } else {
        Value::new(ty.clone(), ValueData::Bytes(encode_state(state)?))
    }
}

/// `finalize`: the result of the aggregate from a state.
pub fn finalize_state(value: &Value, ty: &LogicalType) -> Result<Value> {
    StateFinalizer::new(ty).finalize(value)
}

/// [`finalize_state`] for the values of one state type, as `finalize(state)` takes them a vector at a time: the
/// aggregate the type names and the layout of its states are found once, when the first value needs them.
pub struct StateFinalizer<'a> {
    ty: &'a LogicalType,
    /// the aggregate, its result type and the layout of its states
    resolved: std::cell::OnceCell<(Arc<AggregateFunction>, LogicalType, LogicalType)>,
}

impl<'a> StateFinalizer<'a> {
    pub fn new(ty: &'a LogicalType) -> Self {
        Self {
            ty,
            resolved: std::cell::OnceCell::new(),
        }
    }

    /// The aggregate the state type names, its result type and the layout of its states, found once.
    fn resolved(&self) -> Result<&(Arc<AggregateFunction>, LogicalType, LogicalType)> {
        Ok(match self.resolved.get() {
            Some(resolved) => resolved,
            None => {
                let (function, result) = resolve_state(self.ty)?;
                self.resolved.get_or_init(|| (function, result, self.ty.state_layout()))
            }
        })
    }

    /// [`finalize`](Self::finalize) of the first `count` rows of `states` into `result`, on the vectors' typed data,
    /// for the exported states of counts and sums. False, with `result` untouched, for the other states, which are
    /// finalized a value at a time.
    pub fn finalize_vector(&self, states: &Vector, count: usize, result: &mut Vector) -> Result<bool> {
        let ty = self.ty;
        if !ty.is_aggregate_state() || !ty.aggregate_state_orders().is_empty() {
            return Ok(false);
        }
        let (.., arguments, _) = ty.aggregate_state_info().ok_or_else(state_failure)?;
        let (function, result_type, _) = self.resolved()?;
        if function.user.is_some() || function.export.is_some() || result.logical_type() != result_type {
            return Ok(false);
        }
        let Some((imported, nulls)) = import_states_typed(function.kind, states, count) else {
            return Ok(false);
        };
        let mut column = Vector::new(result_type.clone(), count.max(1));
        if !finalize_states_typed(function.kind, &imported, result_type, arguments.first(), &mut column)? {
            return Ok(false);
        }
        // a NULL state finalizes to NULL, a count's included
        let validity = column.validity_mut();
        for row in nulls {
            validity.set_invalid(row);
        }
        *result = column;
        Ok(true)
    }

    pub fn finalize(&self, value: &Value) -> Result<Value> {
        let ty = self.ty;
        if !ty.aggregate_state_orders().is_empty() {
            return finalize_sorted(value, ty);
        }
        let (.., arguments, _) = ty.aggregate_state_info().ok_or_else(state_failure)?;
        let (function, result, layout) = self.resolved()?;
        if value.is_null() {
            return Ok(Value::null(result.clone()));
        }
        // a state of C++'s layout of an aggregate duckrs computes from its rows, with what its arguments were bound to
        if let AggregateKind::Holistic(holistic) = function.kind
            && ty.is_aggregate_state()
        {
            let layout = Value::new(layout.clone(), value.data().clone())?;
            return super::holistic_states::finalize(holistic, &layout, result, ty.aggregate_state_constants());
        }
        let state = if ty.is_aggregate_state() {
            value_to_state_of(function.kind, value, layout)?
        } else {
            read_state(function.kind, value, ty)?
        };
        function.finalize(&state, result, arguments.first())
    }
}

/// `SortedAggregateFunction::Finalize` of an exported state: the buffered rows sorted by their keys, then given to
/// the aggregate in that order. The argument columns come first in a row, and are read as the signature's types;
/// an argument bound to a constant is that constant.
fn finalize_sorted(value: &Value, ty: &LogicalType) -> Result<Value> {
    use crate::planner::expression::BoundExpr;
    let (name, _, arguments, _) = ty.aggregate_state_info().ok_or_else(state_failure)?;
    let constants = ty.aggregate_state_constants();
    let set = super::BuiltinFunctions::get()
        .aggregate(name)
        .ok_or_else(state_failure)?;
    // an ANY argument is of the type of its buffered column
    let columns: Vec<LogicalType> = ty
        .state_layout()
        .child_type()
        .map(|row| row.struct_children().iter().map(|(_, t)| t.clone()).collect())
        .unwrap_or_default();
    let arguments: Vec<LogicalType> = arguments
        .iter()
        .enumerate()
        .map(|(at, argument)| match (argument.id(), columns.get(at)) {
            (LogicalTypeId::Any, Some(column)) => column.clone(),
            _ => argument.clone(),
        })
        .collect();
    let placeholders = arguments
        .iter()
        .enumerate()
        .map(|(at, argument)| match constants.get(at).cloned().flatten() {
            Some(constant) => BoundExpr::constant(constant).cast_to(argument, false),
            None => BoundExpr::constant(Value::null(argument.clone())),
        })
        .collect();
    let bound = super::binder::bind_aggregate_function(set, placeholders, None, false)?;
    // an empty buffer (NULL) is the aggregate of no rows: count's is 0
    let empty = Vec::new();
    let items = match value.data() {
        ValueData::List(items) => items,
        _ => &empty,
    };
    // `SortedAggregateFunction::SortRows`: stable, by each key in turn
    let orders = ty.aggregate_state_orders();
    let column = |item: &Value, at: u32| match item.data() {
        ValueData::Struct(fields) => fields.get(at as usize).cloned().unwrap_or_else(Value::sql_null),
        _ => Value::sql_null(),
    };
    let mut order: Vec<usize> = (0..items.len()).collect();
    let mut failure = None;
    order.sort_by(|&a, &b| {
        for key in orders {
            let (x, y) = (column(&items[a], key.column), column(&items[b], key.column));
            let ordering = match (x.is_null(), y.is_null()) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => {
                    if key.nulls_first {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    }
                }
                (false, true) => {
                    if key.nulls_first {
                        std::cmp::Ordering::Greater
                    } else {
                        std::cmp::Ordering::Less
                    }
                }
                (false, false) => {
                    let ordering = super::holistic::compare(&x, &y, x.logical_type()).unwrap_or_else(|error| {
                        failure.get_or_insert(error);
                        std::cmp::Ordering::Equal
                    });
                    if key.descending { ordering.reverse() } else { ordering }
                }
            };
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
        }
        std::cmp::Ordering::Equal
    });
    if let Some(error) = failure {
        return Err(error);
    }
    // the arguments of each row, as the aggregate takes them
    let argument_of = |item: &Value, at: usize, ty: &LogicalType| -> Result<Value> {
        let value = match constants.get(at).cloned().flatten() {
            Some(constant) => constant,
            None => column(item, at as u32),
        };
        if value.logical_type() == ty || ty.id() == LogicalTypeId::Any {
            Ok(value)
        } else {
            crate::execution::parquet::cast_value(&value, ty)
        }
    };
    let function = bound.function.clone();
    let mut states = vec![function.kind.initial_state()];
    if let Some(child) = bound.children.first() {
        let input_type = child.return_type.clone();
        let packed = matches!(function.kind, AggregateKind::Holistic(_)) && arguments.len() > 1;
        let mut input = Vector::new(input_type.clone(), order.len().max(1));
        for (row, &at) in order.iter().enumerate() {
            let value = if packed {
                let fields = input_type
                    .struct_children()
                    .iter()
                    .enumerate()
                    .map(|(i, (_, field))| argument_of(&items[at], i, field))
                    .collect::<Result<Vec<_>>>()?;
                Value::new(input_type.clone(), ValueData::Struct(fields))?
            } else {
                argument_of(&items[at], 0, &input_type)?
            };
            input.set_value(row, &value)?;
        }
        let rows: Vec<(usize, usize)> = (0..order.len()).map(|row| (row, 0)).collect();
        function.update(Some(&input), order.len(), &rows, &mut states)?;
    } else {
        let rows: Vec<(usize, usize)> = (0..order.len()).map(|row| (row, 0)).collect();
        function.update(None, order.len(), &rows, &mut states)?;
    }
    function.finalize(&states[0], &bound.return_type, arguments.first())
}

/// `combine` and `combine_aggr`: a state of the rows of two others.
pub fn combine_state_values(left: &Value, right: &Value, ty: &LogicalType) -> Result<Value> {
    // `AggregateStateCombine`: a NULL state is an empty one, and two of them stay NULL
    if left.is_null() && right.is_null() {
        return Ok(Value::null(ty.clone()));
    }
    // the buffers of an ordered aggregate together: they are sorted when finalized, which merges their orders
    if !ty.aggregate_state_orders().is_empty() {
        let mut rows = Vec::new();
        for side in [left, right] {
            if let ValueData::List(items) = side.data() {
                rows.extend(items.iter().cloned());
            }
        }
        return Value::new(ty.clone(), ValueData::List(rows));
    }
    let function = state_function(ty)?;
    if !ty.is_aggregate_state() && (left.is_null() || right.is_null()) {
        return Ok(if left.is_null() { right } else { left }.clone());
    }
    let (left_state, right_state) = (
        read_state(function.kind, left, ty)?,
        read_state(function.kind, right, ty)?,
    );
    // the states C++ keeps for an aggregate duckrs computes from its rows, which may need what the arguments
    // were bound to
    if let AggregateKind::Holistic(holistic) = function.kind
        && ty.is_aggregate_state()
    {
        let layout = ty.state_layout();
        let as_layout = |state: AggregateState| match state {
            AggregateState::Exported(value) => Ok(*value),
            AggregateState::Rows(rows) => super::holistic_states::from_rows(holistic, &rows, &layout),
            _ => Err(state_failure()),
        };
        let combined = super::holistic_states::combine(
            holistic,
            &as_layout(left_state)?,
            &as_layout(right_state)?,
            &layout,
            ty.aggregate_state_constants(),
        )?;
        return write_state(function.kind, &AggregateState::Exported(Box::new(combined)), ty);
    }
    let combined = combine_states(function.kind, left_state, right_state)?;
    write_state(function.kind, &combined, ty)
}

/// `CombineAggrFinalize` of a group no state was combined into: the empty state, which is NULL
/// for the states of duckrs's own form.
pub fn empty_state_value(ty: &LogicalType) -> Result<Value> {
    if !ty.is_aggregate_state() || !ty.aggregate_state_orders().is_empty() {
        return Ok(Value::null(ty.clone()));
    }
    let kind = state_function(ty)?.kind;
    write_state(kind, &kind.initial_state(), ty)
}

/// A state combined with itself `times` times, by doubling. States that keep their rows can't
/// hold more than this many.
const MAX_REPEATED_ROWS: u128 = 100_000_000;

pub fn repeat_state(state: &Value, times: i128, ty: &LogicalType) -> Result<Value> {
    if times == 0 || state.is_null() {
        return Ok(Value::null(ty.clone()));
    }
    let kind = state_function(ty)?.kind;
    if let AggregateState::Rows(rows) = read_state(kind, state, ty)?
        && (rows.len() as u128).saturating_mul(times as u128) > MAX_REPEATED_ROWS
    {
        return Err(Error::new(ErrorKind::OutOfRange, "Overflow in aggregate state combine"));
    }
    // a state that buffers its values (a quantile's) grows with every repetition
    if let ValueData::List(items) = state.data()
        && ty.is_aggregate_state()
        && (items.len() as u128).saturating_mul(times as u128) > MAX_REPEATED_ROWS
    {
        return Err(Error::new(ErrorKind::OutOfRange, "Overflow in aggregate state combine"));
    }
    // `RepeatedCombine` of the averages that have one: an overflow is one of the repetition
    let averages = matches!(
        kind,
        AggregateKind::AvgInteger
            | AggregateKind::AvgDouble
            | AggregateKind::AvgDecimal
            | AggregateKind::Holistic(super::holistic::Holistic::AvgTimestamp | super::holistic::Holistic::AvgInterval)
    );
    let repeated = |error: Error| {
        if averages && error.message == "Overflow in aggregate state combine" {
            Error::new(ErrorKind::OutOfRange, "Overflow in repeated aggregate state combine")
        } else {
            error
        }
    };
    let mut result = Value::null(ty.clone());
    let mut power = state.clone();
    let mut left = times;
    while left > 0 {
        if left & 1 == 1 {
            result = combine_state_values(&result, &power, ty).map_err(repeated)?;
        }
        left >>= 1;
        if left > 0 {
            power = combine_state_values(&power, &power, ty).map_err(repeated)?;
        }
    }
    Ok(result)
}

/// A constant parameter of `to_aggregate_state`, as a value of the type of its argument in the signature (what the
/// argument is bound to: string_agg's separator is a VARCHAR, approx_quantile's fraction a FLOAT).
fn constant_of(value: Value, argument: Option<&LogicalType>) -> Result<Value> {
    match argument {
        Some(ty) if ty.id() != LogicalTypeId::Any && value.logical_type() != ty => {
            crate::execution::parquet::cast_value(&value, ty)
        }
        _ => Ok(value),
    }
}

/// `ExportAggregateFunction::Bind`: the aggregate gives its state.
pub fn export_state(
    function: &AggregateFunction,
    return_type: &LogicalType,
    arguments: &[LogicalType],
    constants: Vec<Option<Value>>,
) -> Result<(AggregateFunction, LogicalType)> {
    let not_exportable = || {
        Error::new(
            ErrorKind::Binder,
            format!("Cannot use EXPORT_STATE for non-combinable function {}", function.name),
        )
    };
    // the aggregates C++ gives no state type (`HasGetStateTypeCallback`)
    if let AggregateKind::Holistic(
        super::holistic::Holistic::Histogram
        | super::holistic::Holistic::HistogramBins
        | super::holistic::Holistic::HistogramExact
        | super::holistic::Holistic::Mode
        | super::holistic::Holistic::Entropy
        | super::holistic::Holistic::BitstringAgg,
    ) = function.kind
    {
        return Err(Error::new(
            ErrorKind::NotImplemented,
            format!(
                "Aggregate function {} does not have a state type callback defined - cannot export state",
                function.name
            ),
        ));
    }
    let set = super::BuiltinFunctions::get()
        .aggregate(&function.name)
        .ok_or_else(not_exportable)?;
    let overload = set
        .functions
        .iter()
        .position(|f| f.kind == function.kind)
        .ok_or_else(not_exportable)?;
    let ty = match state_layout(function.kind, arguments, return_type) {
        // `EncodeStateParameters`: an argument the aggregate takes as ANY stays ANY
        Some(layout) => {
            let recorded = arguments
                .iter()
                .zip(&function.arguments)
                .map(|(bound, declared)| {
                    // the aggregates duckrs computes from their rows bind their ANY parameters to the types they get,
                    // as C++'s binds of them do
                    let holistic = matches!(function.kind, AggregateKind::Holistic(_))
                        && function.kind != AggregateKind::Holistic(super::holistic::Holistic::ApproxCountDistinct);
                    if declared.id() == LogicalTypeId::Any && !holistic {
                        declared
                    } else {
                        bound
                    }
                    .clone()
                })
                .collect();
            LogicalType::aggregate_state_of(&layout, &function.name, overload as u32, recorded, return_type.clone())
                .with_state_constants(constants)
        }
        None => LogicalType::aggregate_state(&function.name, overload as u32, arguments.to_vec(), return_type.clone()),
    };
    let exported = AggregateFunction {
        name: function.name.clone(),
        arguments: function.arguments.clone(),
        return_type: ty.clone(),
        kind: function.kind,
        export: Some(ty.clone()),
        user: function.user.clone(),
    };
    Ok((exported, ty))
}

// Ported from `BindToAggregateState` and `BindExportedAggregate` of
// duckdb/src/function/scalar/system/aggregate_export.cpp @ rust-port-baseline

/// `to_aggregate_state(data, name, signature[, constant_parameters[, order_by]])`: `data` as a
/// state of the aggregate `name` bound with the types of `signature`. `resolve_type` reads a
/// type from its name.
pub fn bind_to_aggregate_state(
    mut children: Vec<crate::planner::expression::BoundExpr>,
    resolve_type: &dyn Fn(&str) -> Result<LogicalType>,
) -> Result<crate::planner::expression::BoundExpr> {
    use crate::execution::expression_executor::evaluate_scalar;
    use crate::planner::expression::BoundExpr;
    let binder = |message: String| Error::new(ErrorKind::Binder, message);
    if !(3..=5).contains(&children.len()) {
        let types: Vec<String> = children.iter().map(|c| c.return_type.to_string()).collect();
        return Err(binder(format!(
            "No function matches the given name and argument types 'to_aggregate_state({})'. You might need to add \
             explicit type casts.",
            types.join(", ")
        )));
    }
    for child in &children[1..] {
        if !child.is_foldable() {
            return Err(binder(
                "to_aggregate_state: the aggregate name, signature and constant parameters must be constant".into(),
            ));
        }
    }
    let non_null = |child: &BoundExpr, what: &str| -> Result<Value> {
        let value = evaluate_scalar(child)?;
        if value.is_null() {
            return Err(binder(format!("to_aggregate_state: the {what} cannot be NULL")));
        }
        Ok(value)
    };
    // the name may come as any type that casts to VARCHAR, a VARIANT among them
    let name = non_null(
        &children[1].clone().cast_to(&LogicalType::VARCHAR, false),
        "aggregate name",
    )?
    .try_to_string()?;
    let signature = non_null(&children[2], "signature")?;
    let ValueData::List(entries) = signature.data() else {
        if signature.logical_type().id() == LogicalTypeId::Variant {
            return Err(binder(
                "to_aggregate_state: the signature must be a list of types".into(),
            ));
        }
        let types: Vec<String> = children.iter().map(|c| c.return_type.to_string()).collect();
        return Err(binder(format!(
            "No function matches the given name and argument types 'to_aggregate_state({})'. You might need to add \
             explicit type casts.",
            types.join(", ")
        )));
    };
    // `ParseSignatureType`
    let mut argument_types = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.is_null() {
            return Err(binder(
                "to_aggregate_state: the signature cannot contain NULL values".into(),
            ));
        }
        let ty = match entry.data() {
            // a type by its name, or a TYPE value (`make_type`)
            ValueData::Bytes(_)
                if matches!(entry.logical_type().id(), LogicalTypeId::Varchar | LogicalTypeId::Type) =>
            {
                let name = entry.try_to_string()?;
                if name.eq_ignore_ascii_case("ANY") {
                    LogicalType::new(LogicalTypeId::Any)
                } else {
                    resolve_type(&name)?
                }
            }
            _ => {
                return Err(binder(
                    "to_aggregate_state: the signature must be a list of types".into(),
                ));
            }
        };
        argument_types.push(ty);
    }
    // `ParseOrderBys`: the ORDER BY of an ordered aggregate's state, by the columns of its buffered rows
    let order_value = if children.len() > 4 {
        evaluate_scalar(&children[4])?
    } else {
        Value::sql_null()
    };
    let mut orders: Vec<duckrs_common::types::StateOrder> = Vec::new();
    if children.len() > 4 {
        let state_type = &children[0].return_type;
        let column_count = match state_type.child_type() {
            Some(row) if state_type.id() == LogicalTypeId::List && row.id() == LogicalTypeId::Struct => {
                row.struct_children().len()
            }
            _ => {
                return Err(binder(
                    "to_aggregate_state: an ordered aggregate state value must be a LIST of STRUCTs (the buffer of \
                     values), e.g. [{'v0': ...}, ...]"
                        .into(),
                ));
            }
        };
        if argument_types.is_empty() {
            return Err(binder(format!(
                "to_aggregate_state: aggregate \"{name}\" has no arguments and cannot have an ordered aggregate state"
            )));
        }
        if argument_types.len() > column_count {
            return Err(binder(format!(
                "to_aggregate_state: argument count {} exceeds the number of state columns ({column_count})",
                argument_types.len()
            )));
        }
        if order_value.is_null() {
            return Err(binder(
                "to_aggregate_state: an ordered aggregate state must have at least one ORDER BY key".into(),
            ));
        }
        let ValueData::List(entries) = order_value.data() else {
            return Err(binder(
                "to_aggregate_state: the ORDER BY argument must be a list of {column, order} structs, e.g. \
                 [{'column': 1, 'order': 'DESC NULLS LAST'}]"
                    .into(),
            ));
        };
        for entry in entries {
            let ValueData::Struct(values) = entry.data() else {
                return Err(binder(
                    "to_aggregate_state: each ORDER BY entry must be a {column, order} struct".into(),
                ));
            };
            let fields = entry.logical_type().struct_children();
            let field = |name: &str| {
                fields
                    .iter()
                    .position(|(n, _)| n == name)
                    .map(|at| values[at].clone())
                    .unwrap_or_else(Value::sql_null)
            };
            let (column, order) = (field("column"), field("order"));
            if column.is_null() || order.is_null() {
                return Err(binder(
                    "to_aggregate_state: each ORDER BY entry must have a non-NULL 'column' and 'order'".into(),
                ));
            }
            if order.logical_type().id() != LogicalTypeId::Varchar {
                return Err(binder(
                    "to_aggregate_state: the 'order' of an ORDER BY entry must be a string, e.g. 'DESC NULLS LAST'"
                        .into(),
                ));
            }
            let column = crate::execution::parquet::cast_value(&column, &LogicalType::UINTEGER)?
                .as_i128()
                .unwrap_or(0) as u64;
            if column >= column_count as u64 {
                return Err(binder(format!(
                    "to_aggregate_state: ORDER BY column {column} is out of range (the state has {column_count} \
                     columns)"
                )));
            }
            let (descending, nulls_first) = duckrs_common::types::parse_order_modifiers(&order.try_to_string()?)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::Parser,
                        format!(
                            "Unrecognized order modifier \"{}\"",
                            order.try_to_string().unwrap_or_default()
                        ),
                    )
                })?;
            orders.push(duckrs_common::types::StateOrder {
                column: column as u32,
                descending,
                nulls_first,
            });
        }
        if orders.is_empty() {
            return Err(binder(
                "to_aggregate_state: an ordered aggregate state must have at least one ORDER BY key".into(),
            ));
        }
    }
    // `ParseConstantParameters`: one entry per argument, NULL for one not bound to a constant
    let mut constants: Vec<Option<Value>> = Vec::new();
    if children.len() > 3 {
        let given = evaluate_scalar(&children[3])?;
        if !given.is_null() {
            let ValueData::List(items) = given.data() else {
                return Err(binder(
                    "to_aggregate_state: the constant parameters must be a list with one entry per argument (use \
                     NULL for arguments that are not bound to a constant), e.g. [NULL, '|']"
                        .into(),
                ));
            };
            if items.len() != argument_types.len() {
                return Err(binder(format!(
                    "to_aggregate_state: the constant parameters list has {} entries but the aggregate has {} \
                     arguments - it must have exactly one entry per argument (use NULL for arguments that are not \
                     bound to a constant)",
                    items.len(),
                    argument_types.len()
                )));
            }
            constants = items
                .iter()
                .map(|item| (!item.is_null()).then(|| item.clone()))
                .collect();
        }
    }
    // `BindExportedAggregate`: the aggregate bound with NULLs of the signature's types
    let set = super::BuiltinFunctions::get()
        .aggregate(&name.to_lowercase())
        .ok_or_else(|| {
            Error::new(
                ErrorKind::Catalog,
                format!("Aggregate Function with name {name} does not exist!"),
            )
        })?;
    let placeholders = argument_types
        .iter()
        .enumerate()
        .map(|(at, ty)| {
            if let Some(Some(constant)) = constants.get(at) {
                return BoundExpr::constant(constant.clone()).cast_to(ty, false);
            }
            let ty = if ty.id() == LogicalTypeId::Any {
                LogicalType::SQLNULL
            } else {
                ty.clone()
            };
            BoundExpr::constant(Value::null(ty))
        })
        .collect();
    let bound = super::binder::bind_aggregate_function(set, placeholders, None, false).map_err(|error| {
        binder(format!(
            "Could not re-bind exported aggregate {name}: {}",
            error.message
        ))
    })?;
    for (index, (requested, declared)) in argument_types.iter().zip(&bound.function.arguments).enumerate() {
        if requested.id() == LogicalTypeId::Any && declared.id() != LogicalTypeId::Any {
            return Err(binder(format!(
                "Aggregate {name} requires a concrete type for signature argument {}",
                index + 1
            )));
        }
    }
    // the arguments of an aggregate that keeps its rows are packed into one STRUCT
    let packed = matches!(bound.function.kind, AggregateKind::Holistic(_)) && argument_types.len() > 1;
    let bound_arguments: Vec<LogicalType> = match bound.children.as_slice() {
        [child] if packed && child.return_type.id() == LogicalTypeId::Struct => child
            .return_type
            .struct_children()
            .iter()
            .map(|(_, t)| t.clone())
            .collect(),
        children => children.iter().map(|c| c.return_type.clone()).collect(),
    };
    let matches = bound_arguments.len() == argument_types.len()
        && bound_arguments
            .iter()
            .zip(&argument_types)
            .all(|(bound, requested)| requested.id() == LogicalTypeId::Any || bound == requested);
    if !matches {
        let list = |types: &[LogicalType]| types.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(", ");
        return Err(binder(format!(
            "Type mismatch for exported aggregate {name}: bound=[{}] requested=[{}]",
            list(&bound_arguments),
            list(&argument_types)
        )));
    }
    if !orders.is_empty() {
        // `CreateSortedAggregateStateType`: the buffered rows as given, the arguments read as the signature's types
        let constants = constants
            .into_iter()
            .enumerate()
            .map(|(at, c)| c.map(|v| constant_of(v, argument_types.get(at))).transpose())
            .collect::<Result<Vec<_>>>()?;
        let constants = if constants.iter().any(Option::is_some) {
            constants
        } else {
            Vec::new()
        };
        let layout = children[0].return_type.clone();
        let state_type = LogicalType::aggregate_state_of(
            &layout,
            &bound.function.name,
            duckrs_common::types::UNRESOLVED_OVERLOAD,
            argument_types.clone(),
            bound.return_type.clone(),
        )
        .with_state_constants(constants)
        .with_state_orders(orders);
        let data = children.swap_remove(0);
        return Ok(super::nested::bound_function(
            "to_aggregate_state",
            vec![data],
            state_type,
            to_aggregate_state,
        ));
    }
    if state_layout(bound.function.kind, &bound_arguments, &bound.return_type).is_none() {
        return Err(crate::unported(format!(
            "to_aggregate_state of {}",
            bound.function.name
        )));
    }
    let constants = if constants.iter().any(Option::is_some) {
        constants
            .into_iter()
            .enumerate()
            .map(|(at, c)| c.map(|v| constant_of(v, argument_types.get(at))).transpose())
            .collect::<Result<_>>()?
    } else {
        Vec::new()
    };
    let (_, state_type) = export_state(&bound.function, &bound.return_type, &bound_arguments, constants)?;
    let data = children.swap_remove(0).cast_to(&state_type.state_layout(), false);
    Ok(super::nested::bound_function(
        "to_aggregate_state",
        vec![data],
        state_type,
        to_aggregate_state,
    ))
}

/// `ToAggregateStateFunction`: the values, as states.
fn to_aggregate_state(args: &duckrs_common::vector::DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    let (function, _) = resolve_state(&ty)?;
    let (.., arguments, _) = ty.aggregate_state_info().ok_or_else(state_failure)?;
    let arguments = arguments.to_vec();
    super::nested::row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(ty.clone()));
        }
        // `validate_state`: what the fields of the layout cannot say
        if let AggregateKind::Holistic(holistic) = function.kind
            && ty.aggregate_state_orders().is_empty()
        {
            super::holistic_states::validate(
                holistic,
                &arguments,
                &Value::new(ty.state_layout(), row[0].data().clone())?,
            )?;
        }
        Value::new(ty.clone(), row[0].data().clone())
    })
}

/// Groups up to this many have their sums of a chunk in arrays of their own, which go into their states at once.
const LOCAL_SUM_GROUPS: usize = 1024;

fn add_integers<T: NativeType + Into<i128>>(
    input: &Vector,
    count: usize,
    rows: &[(usize, usize)],
    states: &mut [AggregateState],
) -> Result<()> {
    let u = input.to_unified(count);
    let data = u.data::<T>();
    // few groups: a sum and a count per group in a loop of its own, then into the states; where an addition
    // overflows, the rows go into the states one by one after all, for the error of the one that does
    if states.len() <= LOCAL_SUM_GROUPS && rows.len() >= states.len() {
        let mut sums = vec![0i128; states.len()];
        let mut seen = vec![0u64; states.len()];
        let mut overflow = false;
        // a sum of values of 64 bits or fewer can't overflow 128 bits; one of 128-bit values can
        let narrow = std::mem::size_of::<T>() <= std::mem::size_of::<i64>();
        if narrow {
            // rows in turn go to one of `BANKS` sums and counts of their group, so that an addition doesn't wait for
            // the one of the row before, which is often of the same group
            const BANKS: usize = 4;
            let groups = states.len();
            let mut bank_sums = vec![0i128; BANKS * groups];
            let mut bank_seen = vec![0u64; BANKS * groups];
            let validity = &u.validity;
            match (&u.sel, validity.all_valid()) {
                (SelectionVector::Identity, true) => {
                    for &(row, group) in rows {
                        let slot = (row & (BANKS - 1)) * groups + group;
                        bank_sums[slot] += data[row].into();
                        bank_seen[slot] += 1;
                    }
                }
                (sel, true) => match sel.indices() {
                    Some(indices) => {
                        for &(row, group) in rows {
                            let slot = (row & (BANKS - 1)) * groups + group;
                            bank_sums[slot] += data[indices[row] as usize].into();
                            bank_seen[slot] += 1;
                        }
                    }
                    None => {
                        for &(row, group) in rows {
                            let slot = (row & (BANKS - 1)) * groups + group;
                            bank_sums[slot] += data[sel.get(row)].into();
                            bank_seen[slot] += 1;
                        }
                    }
                },
                (sel, false) => {
                    for &(row, group) in rows {
                        let index = sel.get(row);
                        if validity.is_valid(index) {
                            let slot = (row & (BANKS - 1)) * groups + group;
                            bank_sums[slot] += data[index].into();
                            bank_seen[slot] += 1;
                        }
                    }
                }
            }
            for group in 0..groups {
                for bank in 0..BANKS {
                    sums[group] += bank_sums[bank * groups + group];
                    seen[group] += bank_seen[bank * groups + group];
                }
            }
        } else {
            for &(row, group) in rows {
                let index = u.sel.get(row);
                if !u.validity.is_valid(index) {
                    continue;
                }
                let value: i128 = data[index].into();
                match sums[group].checked_add(value) {
                    Some(sum) => sums[group] = sum,
                    None => {
                        overflow = true;
                        break;
                    }
                }
                seen[group] += 1;
            }
        }
        let mut totals = Vec::with_capacity(states.len());
        if !overflow {
            for (group, state) in states.iter().enumerate() {
                let AggregateState::Integer { sum, .. } = state else {
                    unreachable!("an integer state");
                };
                match sum.checked_add(sums[group]) {
                    Some(total) => totals.push(total),
                    None => {
                        overflow = true;
                        break;
                    }
                }
            }
        }
        if !overflow {
            for ((state, total), added) in states.iter_mut().zip(totals).zip(seen) {
                let AggregateState::Integer { sum, count: rows_seen } = state else {
                    unreachable!("an integer state");
                };
                *sum = total;
                *rows_seen += added;
            }
            return Ok(());
        }
    }
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
    // `dtime_tz_t::operator<`: a TIMETZ orders by its instant
    if input.logical_type().id() == LogicalTypeId::TimeTz {
        let mut instants = Vector::new(LogicalType::UBIGINT, count.max(1));
        for row in 0..count {
            let value = input.get_value(row);
            let instant = match value.data() {
                ValueData::Int64(bits) => Value::ubigint(duckrs_common::types::TimeTz(*bits as u64).sort_key()),
                _ => Value::null(LogicalType::UBIGINT),
            };
            instants.set_value(row, &instant)?;
        }
        return create_sort_keys(&[(&instants, modifiers)], count);
    }
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
        if self.kind == User && input.is_none() {
            let user = self.user.as_ref().expect("a user-defined aggregate has its callbacks");
            return user.update(self, None, count, rows, states);
        }
        let Some(input) = input else {
            // count_star counts rows, whatever they hold: for few groups, counted apart and then added to the states
            if states.len() == 1 {
                if let AggregateState::Count(n) = &mut states[0] {
                    *n += rows.len() as i64;
                }
                return Ok(());
            }
            if states.len() <= LOCAL_SUM_GROUPS && rows.len() >= states.len() {
                let mut counts = vec![0i64; states.len()];
                for &(_, group) in rows {
                    counts[group] += 1;
                }
                for (state, added) in states.iter_mut().zip(counts) {
                    if let AggregateState::Count(n) = state {
                        *n += added;
                    }
                }
                return Ok(());
            }
            for &(_, group) in rows {
                if let AggregateState::Count(n) = &mut states[group] {
                    *n += 1;
                }
            }
            return Ok(());
        };
        match self.kind {
            User => {
                let user = self.user.as_ref().expect("a user-defined aggregate has its callbacks");
                return user.update(self, Some(input), count, rows, states);
            }
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
            // the `int64_t` sum: an addition past it fails as the repeated combine of a constant does
            SumNoOverflow => {
                match input.physical_type() {
                    PhysicalType::Int32 => add_integers::<i32>(input, count, rows, states)?,
                    _ => add_integers::<i64>(input, count, rows, states)?,
                }
                for &(_, group) in rows {
                    if let AggregateState::Integer { sum, .. } = &states[group]
                        && i64::try_from(*sum).is_err()
                    {
                        return Err(Error::new(
                            ErrorKind::OutOfRange,
                            "Overflow in repeated aggregate state combine",
                        ));
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
                    *state = Some(Box::new(match state.take() {
                        Some(sum) => add_bignums(&sum, &value)?,
                        None => value,
                    }));
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
                    let better = current.as_ref().is_none_or(|extreme| {
                        if self.kind == Min {
                            keys[row] < extreme.0
                        } else {
                            keys[row] > extreme.0
                        }
                    });
                    if better {
                        *current = Some(Box::new((keys[row].clone(), input.get_value(row))));
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
                        _ => current.as_ref().is_none_or(|value| value.is_null()),
                    };
                    if take {
                        *current = Some(Box::new(input.get_value(row)));
                    }
                }
            }
        }
        Ok(())
    }

    /// [`finalize`](Self::finalize) of each of `states`, stored in `column` from its first row on. An exported
    /// state's layout is worked out once for all of them, rather than once for each state.
    pub fn finalize_into(
        &self,
        states: &[AggregateState],
        return_type: &LogicalType,
        input: Option<&LogicalType>,
        column: &mut Vector,
    ) -> Result<()> {
        if let Some(user) = &self.user
            && self.export.is_none()
        {
            return user.finalize(states, column);
        }
        if self.export.is_some() && return_type.is_aggregate_state() {
            if export_states_typed(self.kind, states, column)? {
                return Ok(());
            }
            let layout = return_type.state_layout();
            for (row, state) in states.iter().enumerate() {
                column.set_value(row, &state_to_value_of(self.kind, state, return_type, &layout)?)?;
            }
            return Ok(());
        }
        if self.export.is_none() && finalize_states_typed(self.kind, states, return_type, input, column)? {
            return Ok(());
        }
        for (row, state) in states.iter().enumerate() {
            column.set_value(row, &self.finalize(state, return_type, input)?)?;
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
        if let Some(user) = &self.user
            && self.export.is_none()
        {
            return user.finalize_one(state, return_type);
        }
        if self.export.is_some() {
            return write_state(self.kind, state, return_type);
        }
        let null = || Value::null(return_type.clone());
        Ok(match (self.kind, state) {
            (Holistic(kind), AggregateState::Rows(rows)) => finish(kind, rows, return_type)?,
            (Holistic(kind), AggregateState::Exported(value)) => {
                super::holistic_states::finalize(kind, value, return_type, &[])?
            }
            (_, AggregateState::Count(n)) => Value::bigint(*n),
            (_, AggregateState::List(None)) => null(),
            (_, AggregateState::List(Some(values))) => {
                Value::new(return_type.clone(), ValueData::List(values.clone()))?
            }
            (_, AggregateState::Integer { count: 0, .. } | AggregateState::Double { count: 0, .. }) => null(),
            (SumInteger | SumNoOverflow, AggregateState::Integer { sum, .. }) => Value::hugeint(*sum),
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
            (_, AggregateState::Extreme(Some(extreme))) => extreme.1.clone(),
            (_, AggregateState::Value(Some(value))) => Value::clone(value),
            (_, AggregateState::Extreme(None) | AggregateState::Value(None)) => null(),
            (kind, state) => unreachable!("{kind:?} with state {state:?}"),
        })
    }
}
