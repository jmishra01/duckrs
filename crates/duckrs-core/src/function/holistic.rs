// Ported from the semantics of duckdb/extension/core_functions/aggregate/**/*.cpp
// @ rust-port-baseline
//
//! The aggregates that keep their rows. C++ has a state per aggregate, and sketches for the
//! approximate ones; these gather the values and compute the result at the end, which is exact
//! and easy to get right, and costs memory until Phase 13 brings the states.

use super::aggregates::value_keys;
use crate::execution::expression_executor::evaluate_scalar;
use crate::planner::expression::BoundExpr;
use duckrs_common::types::{Interval, LogicalType, LogicalTypeId, bit};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::Vector;
use duckrs_common::{Error, ErrorKind, Result};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holistic {
    StringAgg,
    BoolAnd,
    BoolOr,
    BitAnd,
    BitOr,
    BitXor,
    Product,
    Mode,
    Median,
    QuantileCont,
    QuantileDisc,
    Mad,
    StddevSamp,
    StddevPop,
    VarSamp,
    VarPop,
    Skewness,
    Kurtosis,
    KurtosisPop,
    Sem,
    AvgInterval,
    AvgTimestamp,
    /// `TimeTZAverageOperation`: the average instant, at +00.
    AvgTimeTz,
    Entropy,
    /// `decimal_average`: the average at the input's scale, rounded half to even
    DecimalAverage,
    /// `variant_group_object(key, value)`: a VARIANT object of the rows
    VariantGroupObject,
    ArgMin,
    ArgMax,
    /// `arg_min_null`: the row of the extreme, whether its value is NULL or not
    ArgMinNull,
    ArgMaxNull,
    /// `arg_min_nulls_last`: NULL values only if nothing else
    ArgMinNullsLast,
    ArgMaxNullsLast,
    /// `min(x, n)`
    MinN,
    MaxN,
    ArgMinN,
    ArgMaxN,
    /// `arg_min_nulls_last(x, by, n)`: the rows of NULL keys after all others
    ArgMinNullsLastN,
    ArgMaxNullsLastN,
    ApproxTopK,
    BitstringAgg,
    /// `combine_aggr`: a state of the states of its rows
    CombineAggr,
    CountIf,
    Histogram,
    /// `histogram(x, bins)`: how many values fall at or below each bin and above the one before
    HistogramBins,
    /// `histogram_exact(x, bins)`: how many values equal each bin
    HistogramExact,
    ApproxCountDistinct,
    Fsum,
    Favg,
    GeoMean,
    Corr,
    CovarPop,
    CovarSamp,
    RegrCount,
    RegrSlope,
    RegrIntercept,
    RegrR2,
    RegrAvgX,
    RegrAvgY,
    RegrSxx,
    RegrSyy,
    RegrSxy,
    /// `approx_quantile`: a t-digest of the values ([`super::holistic_states`])
    ApproxQuantile,
    /// `SortedAggregateFunction` for EXPORT_STATE: the rows of an ordered aggregate, buffered as they arrive, with
    /// their sort keys; they are sorted when the state is finalized
    Sorted,
}

/// `ApproxQuantileFun::GetFunctions`: the type of the overload `approx_quantile` of an `input` binds to, of one
/// quantile or of a list of them. The input is cast to it, and the result is of it.
pub fn approx_quantile_type(input: &LogicalType, list: bool) -> Result<LogicalType> {
    use LogicalTypeId as L;
    Ok(match input.id() {
        L::Decimal => input.clone(),
        L::TinyInt if list => LogicalType::new(L::TinyInt),
        L::TinyInt | L::UTinyInt | L::SmallInt => LogicalType::SMALLINT,
        L::USmallInt | L::Integer => LogicalType::INTEGER,
        L::UInteger | L::BigInt => LogicalType::BIGINT,
        L::UBigInt | L::HugeInt => LogicalType::HUGEINT,
        L::Float if list => LogicalType::FLOAT,
        L::Float | L::Double | L::SqlNull | L::IntegerLiteral => {
            if input.id() == L::IntegerLiteral {
                LogicalType::INTEGER
            } else {
                LogicalType::DOUBLE
            }
        }
        L::Date | L::Time | L::TimeTz | L::Timestamp | L::TimestampTz => input.clone(),
        L::TimeNs | L::TimestampNs | L::TimestampTzNs if !list => input.clone(),
        // the list overloads have no nanoseconds: the implicit cast to the microsecond types
        L::TimeNs => LogicalType::TIME,
        L::TimestampNs => LogicalType::TIMESTAMP,
        L::TimestampTzNs => LogicalType::new(L::TimestampTz),
        // `GetApproxQuantileWidenedTimestamp`: the TIMESTAMP implementation, which keeps their range
        L::TimestampSec | L::TimestampMs => LogicalType::TIMESTAMP,
        _ => {
            return Err(Error::new(
                ErrorKind::Binder,
                format!(
                    "No function matches the given name and argument types 'approx_quantile({input}, {})'. You might \
                     need to add explicit type casts.",
                    if list { "FLOAT[]" } else { "FLOAT" }
                ),
            ));
        }
    })
}

/// The type of `quantile_cont` and `median` of a column of type `input`.
/// `CanInterpolate`: the numbers, the temporal types and INTERVAL.
fn can_interpolate(ty: &LogicalType) -> bool {
    use LogicalTypeId as L;
    ty.is_integral()
        || matches!(
            ty.id(),
            L::Float
                | L::Double
                | L::Decimal
                | L::Date
                | L::Timestamp
                | L::TimestampTz
                | L::TimestampTzNs
                | L::TimestampSec
                | L::TimestampMs
                | L::TimestampNs
                | L::Time
                | L::TimeTz
                | L::Interval
        )
}

fn interpolated_type(input: &LogicalType) -> LogicalType {
    match input.id() {
        LogicalTypeId::Date => LogicalType::TIMESTAMP,
        LogicalTypeId::Float | LogicalTypeId::Double | LogicalTypeId::Decimal | LogicalTypeId::Timestamp => {
            input.clone()
        }
        LogicalTypeId::TimestampTz | LogicalTypeId::Time | LogicalTypeId::Interval => input.clone(),
        id if LogicalType::new(id).is_integral() => LogicalType::DOUBLE,
        _ => input.clone(),
    }
}

impl Holistic {
    /// The return type for arguments of these types.
    pub fn return_type(self, declared: &LogicalType, arguments: &[LogicalType]) -> LogicalType {
        use Holistic::*;
        let input = arguments.first();
        let quantile = |base: LogicalType| match arguments.get(1).map(LogicalType::id) {
            Some(LogicalTypeId::List | LogicalTypeId::Array) => LogicalType::list(base),
            _ => base,
        };
        match (self, input) {
            (Median, Some(ty)) => interpolated_type(ty),
            (QuantileCont, Some(ty)) => quantile(interpolated_type(ty)),
            (QuantileDisc, Some(ty)) => quantile(ty.clone()),
            (ApproxQuantile, Some(ty)) => quantile(ty.clone()),
            (Histogram | HistogramBins | HistogramExact, Some(ty)) => {
                LogicalType::map(ty.clone(), LogicalType::UBIGINT)
            }
            (
                Mode | BitAnd | BitOr | BitXor | ArgMin | ArgMax | ArgMinNull | ArgMaxNull | ArgMinNullsLast
                | ArgMaxNullsLast | AvgTimestamp | AvgTimeTz | DecimalAverage,
                Some(ty),
            ) => ty.clone(),
            (CombineAggr, Some(ty)) => ty.clone(),
            (MinN | MaxN | ArgMinN | ArgMaxN | ArgMinNullsLastN | ArgMaxNullsLastN | ApproxTopK, Some(ty)) => {
                LogicalType::list(ty.clone())
            }
            _ => declared.clone(),
        }
    }
}

fn out_of_range(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::OutOfRange, message)
}

fn number(value: &Value) -> Option<f64> {
    match value.data() {
        ValueData::Null => None,
        ValueData::Double(v) => Some(*v),
        ValueData::Float(v) => Some(f64::from(*v)),
        _ if value.logical_type().id() == LogicalTypeId::Decimal => value.try_to_string().ok()?.parse().ok(),
        _ => value.as_i128().map(|v| v as f64),
    }
}

/// A number of any numeric type, as a double.
pub(super) fn number_of(value: &Value) -> Option<f64> {
    number(value)
}

fn numbers(rows: &[Value]) -> Vec<f64> {
    rows.iter().filter_map(number).collect()
}

fn cast(value: Value, ty: &LogicalType) -> Result<Value> {
    if value.logical_type() == ty {
        return Ok(value);
    }
    evaluate_scalar(&BoundExpr::constant(value).cast_to(ty, false))
}

/// The members of a row of an aggregate of several arguments, which arrive as a STRUCT.
fn fields(row: &Value) -> &[Value] {
    match row.data() {
        ValueData::Struct(fields) => fields,
        _ => std::slice::from_ref(row),
    }
}

/// Ascending sort keys of `values`, all of type `ty`.
fn sort_keys(values: &[Value], ty: &LogicalType) -> Result<Vec<Vec<u8>>> {
    let mut vector = Vector::new(ty.clone(), values.len().max(1));
    for (row, value) in values.iter().enumerate() {
        vector.set_value(row, value)?;
    }
    value_keys(&vector, values.len())
}

fn sorted(values: Vec<Value>) -> Result<Vec<Value>> {
    let Some(first) = values.first() else {
        return Ok(values);
    };
    let keys = sort_keys(&values, &first.logical_type().clone())?;
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|a, b| keys[*a].cmp(&keys[*b]));
    Ok(order.into_iter().map(|at| values[at].clone()).collect())
}

/// `InterpolateInt64`.
fn interpolate_i64(lo: i64, d: f64, hi: i64) -> i64 {
    let (lower, upper) = (lo.min(hi), lo.max(hi));
    let offset = ((i128::from(hi) - i128::from(lo)) as f64 * d).round() as i128;
    (i128::from(lo) + offset).clamp(i128::from(lower), i128::from(upper)) as i64
}

/// `Interval::GetMicro`.
pub(super) fn interval_micros(interval: &Interval) -> Result<i64> {
    let per_day = Interval::MICROS_PER_DAY;
    let conversion = |from: &str| {
        Error::new(
            ErrorKind::Conversion,
            format!("Could not convert {from} to Microseconds"),
        )
    };
    let months = i64::from(interval.months)
        .checked_mul(Interval::DAYS_PER_MONTH * per_day)
        .ok_or_else(|| conversion("Month"))?;
    let days = i64::from(interval.days)
        .checked_mul(per_day)
        .ok_or_else(|| conversion("Day"))?;
    months
        .checked_add(days)
        .and_then(|sum| sum.checked_add(interval.micros))
        .ok_or_else(|| conversion("Interval"))
}

/// `InterpolateOperator`: the value a fraction `d` of the way from `lo` to `hi`, both of type `ty`.
fn interpolate(lo: &Value, d: f64, hi: &Value, ty: &LogicalType) -> Result<Value> {
    // `InterpolateOperator::Operation(dtime_tz_t)`: between the instants, as a time at +00
    if ty.id() == LogicalTypeId::TimeTz
        && let (ValueData::Int64(a), ValueData::Int64(b)) = (lo.data(), hi.data())
    {
        use duckrs_common::types::{Time, TimeTz};
        let (lo_key, hi_key) = (TimeTz(*a as u64).sort_key(), TimeTz(*b as u64).sort_key());
        if lo_key == hi_key {
            return Ok(lo.clone());
        }
        let (lo_micros, hi_micros) = ((lo_key >> 24) as f64, (hi_key >> 24) as f64);
        let mut micros = (lo_micros * (1.0 - d) + hi_micros * d).round() as i64;
        micros -= i64::from(TimeTz::MAX_OFFSET) * 1_000_000;
        micros = micros.rem_euclid(Interval::MICROS_PER_DAY);
        return Ok(Value::time_tz(TimeTz::new(Time(micros), 0)));
    }
    match (lo.data(), hi.data()) {
        (ValueData::Interval(lo), ValueData::Interval(hi)) => {
            let (lo, hi) = (i128::from(interval_micros(lo)?), i128::from(interval_micros(hi)?));
            let micros = lo + ((hi - lo) as f64 * d).round() as i128;
            let per_day = i128::from(Interval::MICROS_PER_DAY);
            Ok(Value::interval(Interval::new(
                0,
                (micros / per_day) as i32,
                (micros % per_day) as i64,
            )))
        }
        (ValueData::Int64(a), ValueData::Int64(b))
            if !matches!(ty.id(), LogicalTypeId::Double | LogicalTypeId::Decimal) =>
        {
            if a == b {
                return Ok(lo.clone());
            }
            if matches!(
                ty.id(),
                LogicalTypeId::Timestamp
                    | LogicalTypeId::TimestampTz
                    | LogicalTypeId::TimestampNs
                    | LogicalTypeId::TimestampMs
                    | LogicalTypeId::TimestampSec
            ) && [a, b].iter().any(|v| v.abs() == i64::MAX)
            {
                return Err(Error::new(
                    ErrorKind::Conversion,
                    "Cannot interpolate infinite TIMESTAMP values",
                ));
            }
            Value::new(ty.clone(), ValueData::Int64(interpolate_i64(*a, d, *b)))
        }
        _ if ty.id() == LogicalTypeId::Decimal => {
            // `InterpolateOperator`: over the integers a DECIMAL stores, and truncating
            let (Some(a), Some(b)) = (lo.as_i128(), hi.as_i128()) else {
                return Ok(lo.clone());
            };
            let (width, scale) = ty.decimal_properties().expect("a DECIMAL");
            // a DECIMAL stored as a HUGEINT interpolates as one, from its exact difference
            if ty.physical_type() == duckrs_common::types::PhysicalType::Int128
                && let Some(value) = b.checked_sub(a).and_then(|delta| a.checked_add((delta as f64 * d) as i128))
            {
                return Value::decimal(value, width, scale);
            }
            Value::decimal((a as f64 + (b as f64 - a as f64) * d) as i128, width, scale)
        }
        // `InterpolateOperator::Operation(hugeint_t)`: the offset from the exact difference, truncated
        _ if ty.id() == LogicalTypeId::HugeInt
            && let (Some(a), Some(b)) = (lo.as_i128(), hi.as_i128()) =>
        {
            let value = b
                .checked_sub(a)
                .and_then(|delta| a.checked_add((delta as f64 * d) as i128))
                .unwrap_or((a as f64 + (b as f64 - a as f64) * d) as i128);
            Value::new(ty.clone(), ValueData::Int128(value))
        }
        _ => {
            let (Some(a), Some(b)) = (number(lo), number(hi)) else {
                return Ok(lo.clone());
            };
            let mixed = if ty.id() == LogicalTypeId::Double {
                a * (1.0 - d) + b * d
            } else {
                a + (b - a) * d
            };
            cast(Value::double(mixed), ty)
        }
    }
}

/// `QuantileInterpolator<true>::Index`: `ceil(n * q)`, snapped to an integer that the rounding of
/// `q` missed.
fn discrete_index(count: usize, q: f64) -> usize {
    let n = count as f64;
    let scaled = n * q;
    let nearest = scaled.round();
    let tolerance = n * 4.0 * f64::EPSILON;
    let ceiled = if (scaled - nearest).abs() <= tolerance {
        nearest
    } else {
        scaled.ceil()
    };
    (ceiled as usize).max(1) - 1
}

/// `QuantileInterpolator`: the value at fraction `q` of `values`, sorted in the quantile's order.
fn quantile(values: &[Value], q: f64, continuous: bool, ty: &LogicalType) -> Result<Value> {
    if !continuous {
        return cast(
            values[discrete_index(values.len(), q).min(values.len() - 1)].clone(),
            ty,
        );
    }
    let rank = (values.len() as f64 - 1.0) * q;
    let (low, high) = (rank.floor(), rank.ceil());
    let lo = cast(values[low as usize].clone(), ty)?;
    if low == high {
        return Ok(lo);
    }
    let hi = cast(values[high as usize].clone(), ty)?;
    interpolate(&lo, rank - low, &hi, ty)
}

/// `NegatePercentileFractions`: a descending ordered-set aggregate is a negative fraction. The
/// sign of a zero fraction has to survive too.
pub fn negate_fractions(parameter: &Value, descending: bool) -> Result<Value> {
    let negate = |value: &Value| match number(value) {
        Some(fraction) if !(0.0..=1.0).contains(&fraction) => Err(Error::new(
            ErrorKind::Binder,
            "PERCENTILEs can only take parameters in the range [0, 1]",
        )),
        Some(fraction) if descending => Ok(Value::double(-fraction)),
        _ => Ok(value.clone()),
    };
    match parameter.data() {
        ValueData::List(items) => {
            let negated = items.iter().map(negate).collect::<Result<Vec<_>>>()?;
            let child = negated
                .first()
                .map_or(LogicalType::DOUBLE, |v| v.logical_type().clone());
            Value::new(LogicalType::list(child), ValueData::List(negated))
        }
        _ => negate(parameter),
    }
}

/// `CheckQuantile` over a constant parameter of a quantile aggregate.
pub fn check_quantile_parameter(parameter: &Value) -> Result<()> {
    let items = match parameter.data() {
        ValueData::List(items) => items.as_slice(),
        _ => std::slice::from_ref(parameter),
    };
    let (mut ascending, mut descending) = (false, false);
    for item in items {
        let Some(fraction) = number(item) else {
            return Err(Error::new(ErrorKind::Binder, "QUANTILE parameter cannot be NULL"));
        };
        if !(-1.0..=1.0).contains(&fraction) && !fraction.is_nan() {
            return Err(Error::new(
                ErrorKind::Binder,
                "QUANTILE can only take parameters in the range [-1, 1]",
            ));
        }
        if fraction.is_nan() {
            return Err(Error::new(ErrorKind::Binder, "QUANTILE parameter cannot be NaN"));
        }
        ascending |= fraction > 0.0;
        descending |= fraction.is_sign_negative();
    }
    if ascending && descending {
        return Err(Error::new(
            ErrorKind::Binder,
            "QUANTILE parameters must have consistent signs",
        ));
    }
    Ok(())
}

/// The fractions of a quantile, whether they are a list, and whether the order is descending.
fn fractions(row: &Value) -> Result<(Vec<f64>, bool, bool)> {
    let parameter = fields(row).get(1).cloned().unwrap_or_else(|| Value::double(0.5));
    let (items, is_list) = match parameter.data() {
        ValueData::List(items) => (items.clone(), true),
        _ => (vec![parameter.clone()], false),
    };
    check_quantile_parameter(&parameter)?;
    let fractions: Vec<f64> = items.iter().filter_map(number).collect();
    let descending = fractions.iter().any(|q| q.is_sign_negative());
    Ok((fractions.iter().map(|q| q.abs()).collect(), is_list, descending))
}

/// `QuantileInterpolator<true>::Index` on a DECIMAL fraction computes `n * 10^scale` with an
/// overflow check, which a scale above 18 can trip.
fn check_decimal_rank(row: &Value, count: usize) -> Result<()> {
    let Some(parameter) = fields(row).get(1) else {
        return Ok(());
    };
    let parameter = match parameter.data() {
        ValueData::List(items) if !items.is_empty() => &items[0],
        _ => parameter,
    };
    let Some((_, scale)) = parameter.logical_type().decimal_properties() else {
        return Ok(());
    };
    let limit = 10i128.pow(38);
    match (count as i128).checked_mul(10i128.pow(scale.into())) {
        Some(scaled) if scaled < limit => Ok(()),
        _ => Err(out_of_range(format!(
            "Overflow in multiplication of DECIMAL(38) ({count} * {})",
            10i128.pow(scale.into())
        ))),
    }
}

fn quantiles(rows: &[Value], ty: &LogicalType, continuous: bool) -> Result<Value> {
    let Some(first) = rows.first() else {
        return Ok(Value::null(ty.clone()));
    };
    let (fractions, is_list, descending) = fractions(first)?;
    let values: Vec<Value> = rows
        .iter()
        .map(|row| fields(row)[0].clone())
        .filter(|v| !v.is_null())
        .collect();
    if values.is_empty() {
        return Ok(Value::null(ty.clone()));
    }
    if !continuous {
        check_decimal_rank(first, values.len())?;
    }
    let mut values = sorted(values)?;
    if descending {
        values.reverse();
    }
    if is_list {
        let child = ty.child_type().expect("a list of quantiles");
        let items = fractions
            .iter()
            .map(|q| quantile(&values, *q, continuous, child))
            .collect::<Result<Vec<_>>>()?;
        return Value::new(ty.clone(), ValueData::List(items));
    }
    quantile(&values, fractions[0], continuous, ty)
}

/// `MedianAbsoluteDeviationOperation` over DATE, TIMESTAMP and TIME: the median of the values,
/// as `quantile_cont` has it, and the median of the deviations from it as INTERVALs
/// (`MadAccessor`).
fn temporal_mad(rows: &[Value]) -> Result<Value> {
    // a TIME WITH TIME ZONE goes as the TIME its bits are (`dtime_t` over `dtime_tz_t`)
    let values: Vec<Value> = rows
        .iter()
        .filter(|v| !v.is_null())
        .map(|v| match (v.logical_type().id(), v.data()) {
            (LogicalTypeId::TimeTz, ValueData::Int64(bits)) => Value::new(LogicalType::TIME, ValueData::Int64(*bits)),
            _ => Ok(v.clone()),
        })
        .collect::<Result<_>>()?;
    if values.is_empty() {
        return Ok(Value::null(LogicalType::INTERVAL));
    }
    let median_type = interpolated_type(values[0].logical_type());
    let values = sorted(values)?;
    let median = quantile(&values, 0.5, true, &median_type)?;
    let micros = |value: &Value| -> Result<i64> {
        match cast(value.clone(), &median_type)?.data() {
            ValueData::Int64(micros) => Ok(*micros),
            _ => Err(Error::new(ErrorKind::Internal, "a temporal value")),
        }
    };
    let center = micros(&median)?;
    let mut deviations = Vec::with_capacity(values.len());
    for value in &values {
        let delta = micros(value)?
            .checked_sub(center)
            .ok_or_else(|| out_of_range("Overflow in timestamp subtraction"))?;
        let delta = delta
            .checked_abs()
            .ok_or_else(|| out_of_range(format!("Overflow on abs({delta})")))?;
        // `Interval::FromMicro`
        let per_day = Interval::MICROS_PER_DAY;
        deviations.push(Value::interval(Interval::new(
            0,
            (delta / per_day) as i32,
            delta % per_day,
        )));
    }
    let deviations = sorted(deviations)?;
    quantile(&deviations, 0.5, true, &LogicalType::INTERVAL)
}

fn median_of(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        (values[middle - 1] + values[middle]) / 2.0
    }
}

/// The count, mean and central moments up to the 4th.
fn moments(values: &[f64]) -> (f64, f64, f64, f64, f64) {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let central = |power: i32| values.iter().map(|v| (v - mean).powi(power)).sum::<f64>();
    (n, mean, central(2), central(3), central(4))
}

/// `KurtosisState`: the count, the mean and the central moments M2..M4, updated one value at a
/// time (Terriberry / Pébay), so that they are exactly zero for constant input.
fn online_moments(values: &[f64]) -> (f64, f64, f64, f64, f64) {
    let (mut count, mut mean, mut m2, mut m3, mut m4) = (0u64, 0.0, 0.0, 0.0, 0.0);
    for &value in values {
        let n1 = count as f64;
        count += 1;
        let n = count as f64;
        let delta = value - mean;
        let delta_n = delta / n;
        let delta_n2 = delta_n * delta_n;
        let term1 = delta * delta_n * n1;
        mean += delta_n;
        m4 += term1 * delta_n2 * (n * n - 3.0 * n + 3.0) + 6.0 * delta_n2 * m2 - 4.0 * delta_n * m3;
        m3 += term1 * delta_n * (n - 2.0) - 3.0 * delta_n * m2;
        m2 += term1;
    }
    (count as f64, mean, m2, m3, m4)
}

/// `KurtosisOperation::Finalize`: NULL for too few values or no variance.
fn kurtosis(values: &[f64], bias_correction: bool) -> Result<Value> {
    let null = || Value::null(LogicalType::DOUBLE);
    let (n, _, m2, _, m4) = online_moments(values);
    if n <= 1.0 || (bias_correction && n <= 3.0) || m2 <= 0.0 {
        return Ok(null());
    }
    let m4_over_m2_sqr = n * (m4 / m2) / m2;
    let target = if bias_correction {
        (n - 1.0) * ((n + 1.0) * m4_over_m2_sqr - 3.0 * (n - 1.0)) / ((n - 2.0) * (n - 3.0))
    } else {
        m4_over_m2_sqr - 3.0
    };
    if !target.is_finite() {
        return Err(out_of_range("Kurtosis is out of range!"));
    }
    Ok(Value::double(target))
}

fn checked(name: &str, value: f64) -> Result<Value> {
    if value.is_finite() {
        Ok(Value::double(value))
    } else {
        Err(out_of_range(format!("{name} is out of range!")))
    }
}

/// The pairs of an aggregate of (y, x), without those with a NULL.
fn pairs(rows: &[Value]) -> Vec<(f64, f64)> {
    rows.iter()
        .filter_map(|row| {
            let values = fields(row);
            Some((number(values.first()?)?, number(values.get(1)?)?))
        })
        .collect()
}

struct Regression {
    n: f64,
    y: f64,
    x: f64,
    sxx: f64,
    syy: f64,
    sxy: f64,
}

fn regression(pairs: &[(f64, f64)]) -> Regression {
    let n = pairs.len() as f64;
    let y = pairs.iter().map(|p| p.0).sum::<f64>() / n;
    let x = pairs.iter().map(|p| p.1).sum::<f64>() / n;
    Regression {
        n,
        y,
        x,
        sxx: pairs.iter().map(|p| (p.1 - x).powi(2)).sum(),
        syy: pairs.iter().map(|p| (p.0 - y).powi(2)).sum(),
        sxy: pairs.iter().map(|p| (p.1 - x) * (p.0 - y)).sum(),
    }
}

fn arg_extreme(rows: &[Value], ty: &LogicalType, greatest: bool) -> Result<Value> {
    let chosen: Vec<&Value> = rows
        .iter()
        .filter(|row| !fields(row)[0].is_null() && !fields(row)[1].is_null())
        .collect();
    let Some(first) = chosen.first() else {
        return Ok(Value::null(ty.clone()));
    };
    let by: Vec<Value> = chosen.iter().map(|row| fields(row)[1].clone()).collect();
    let keys = sort_keys(&by, fields(first)[1].logical_type())?;
    let mut best = 0;
    for at in 1..keys.len() {
        let better = if greatest {
            keys[at] > keys[best]
        } else {
            keys[at] < keys[best]
        };
        if better {
            best = at;
        }
    }
    Ok(fields(chosen[best])[0].clone())
}

/// `arg_min_null` and `arg_min_nulls_last`: the rows with a key; the value may be NULL.
fn arg_extreme_nulls(rows: &[Value], ty: &LogicalType, greatest: bool, nulls_last: bool) -> Result<Value> {
    let chosen: Vec<&Value> = rows.iter().filter(|row| !fields(row)[1].is_null()).collect();
    let Some(first) = chosen.first() else {
        // NULL keys come last, so one of them is what is left
        return Ok(match rows.first() {
            Some(row) if nulls_last => fields(row)[0].clone(),
            _ => Value::null(ty.clone()),
        });
    };
    let by: Vec<Value> = chosen.iter().map(|row| fields(row)[1].clone()).collect();
    let keys = sort_keys(&by, fields(first)[1].logical_type())?;
    let mut best = 0;
    for at in 1..keys.len() {
        if (greatest && keys[at] > keys[best]) || (!greatest && keys[at] < keys[best]) {
            best = at;
        }
    }
    Ok(fields(chosen[best])[0].clone())
}

fn positive_n(value: &Value, name: &str) -> Result<usize> {
    if value.is_null() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("Invalid input for {name}: n value cannot be NULL"),
        ));
    }
    let n = value.as_i128().unwrap_or(0);
    if n <= 0 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("Invalid input for {name}: n value must be > 0"),
        ));
    }
    if n >= 1_000_000 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("Invalid input for {name}: n value must be < 1000000"),
        ));
    }
    Ok(n as usize)
}

/// `min(x, n)` and `arg_min(a, b, n)`: the `n` values with the smallest keys, smallest first.
fn extremes(rows: &[Value], ty: &LogicalType, greatest: bool, value: usize, key: usize, count: usize) -> Result<Value> {
    let chosen: Vec<&Value> = rows.iter().filter(|row| !fields(row)[key].is_null()).collect();
    let Some(first) = chosen.first() else {
        return Ok(Value::null(ty.clone()));
    };
    let name = if value == key { "MIN/MAX" } else { "arg_min/arg_max" };
    let n = positive_n(&fields(first)[count], name)?;
    let by: Vec<Value> = chosen.iter().map(|row| fields(row)[key].clone()).collect();
    let keys = sort_keys(&by, fields(first)[key].logical_type())?;
    // C++ keeps a heap of the n rows: ties come out in the heap's order.
    let order = super::std_heap::top_n(keys.len(), n, |a, b| {
        if greatest {
            keys[*a] > keys[*b]
        } else {
            keys[*a] < keys[*b]
        }
    });
    let items = order
        .into_iter()
        .map(|at| fields(chosen[at])[value].clone())
        .collect();
    Value::new(ty.clone(), ValueData::List(items))
}

/// `arg_min_nulls_last(x, by, n)` and `arg_max_nulls_last`: the n rows of the least (or greatest)
/// keys, those with a NULL key last.
fn extremes_nulls_last(rows: &[Value], ty: &LogicalType, greatest: bool) -> Result<Value> {
    let Some(first) = rows.first() else {
        return Ok(Value::null(ty.clone()));
    };
    let n = positive_n(&fields(first)[2], "arg_min/arg_max")?;
    let (valued, nulls): (Vec<&Value>, Vec<&Value>) = rows.iter().partition(|row| !fields(row)[1].is_null());
    let mut ordered: Vec<&Value> = Vec::with_capacity(rows.len());
    if let Some(head) = valued.first() {
        let by: Vec<Value> = valued.iter().map(|row| fields(row)[1].clone()).collect();
        let keys = sort_keys(&by, fields(head)[1].logical_type())?;
        let mut order: Vec<usize> = (0..keys.len()).collect();
        order.sort_by(|a, b| {
            if greatest {
                keys[*b].cmp(&keys[*a])
            } else {
                keys[*a].cmp(&keys[*b])
            }
        });
        ordered.extend(order.into_iter().map(|at| valued[at]));
    }
    ordered.extend(nulls);
    let items = ordered.into_iter().take(n).map(|row| fields(row)[0].clone()).collect();
    Value::new(ty.clone(), ValueData::List(items))
}

/// `approx_top_k`, exactly: the most frequent values.
fn top_k(rows: &[Value], ty: &LogicalType) -> Result<Value> {
    let chosen: Vec<&Value> = rows.iter().filter(|row| !fields(row)[0].is_null()).collect();
    let Some(first) = chosen.first() else {
        return Ok(Value::null(ty.clone()));
    };
    let Some(k) = fields(first)[1].as_i128() else {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Invalid input for approx_top_k: k value cannot be NULL",
        ));
    };
    if k <= 0 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Invalid input for approx_top_k: k value must be > 0",
        ));
    }
    if k >= 1_000_000 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Invalid input for approx_top_k: k value must be < 1000000",
        ));
    }
    let values: Vec<Value> = chosen.iter().map(|row| fields(row)[0].clone()).collect();
    let keys = sort_keys(&values, values[0].logical_type())?;
    let mut counts: HashMap<&[u8], (usize, usize)> = HashMap::new();
    for (at, key) in keys.iter().enumerate() {
        counts.entry(key.as_slice()).or_insert((0, at)).0 += 1;
    }
    let mut ranked: Vec<(usize, usize)> = counts.into_values().collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let items = ranked
        .into_iter()
        .take(k as usize)
        .map(|(_, at)| values[at].clone())
        .collect();
    Value::new(ty.clone(), ValueData::List(items))
}

/// `bitstring_agg(x, min, max)`: a BIT with a bit set for each value, from `min` to `max`.
fn bitstring(rows: &[Value], ty: &LogicalType) -> Result<Value> {
    let numbers: Vec<(i128, Option<(i128, i128)>)> = rows
        .iter()
        .filter(|row| !fields(row)[0].is_null())
        .map(|row| {
            let parts = fields(row);
            let range = match parts {
                [_, low, high] => low.as_i128().zip(high.as_i128()),
                _ => None,
            };
            (parts[0].as_i128().unwrap_or(0), range)
        })
        .collect();
    let Some((_, given)) = numbers.first() else {
        return Ok(Value::null(ty.clone()));
    };
    let (low, high) = match given {
        // `BitStringAggOperation::GetRange`
        Some((low, high)) if low > high => {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("Invalid explicit bitstring range: Minimum ({low}) > maximum ({high})"),
            ));
        }
        Some(range) => *range,
        None => (
            numbers.iter().map(|n| n.0).min().unwrap_or(0),
            numbers.iter().map(|n| n.0).max().unwrap_or(0),
        ),
    };
    let size = high - low + 1;
    if !(1..=100_000_000).contains(&size) {
        return Err(out_of_range(format!(
            "The range between min and max value ({low} <-> {high}) is too large for bitstring aggregation"
        )));
    }
    let mut text = vec![b'0'; size as usize];
    for (value, _) in &numbers {
        if *value < low || *value > high {
            return Err(out_of_range(format!(
                "Value {value} is outside of provided min and max range ({low} <-> {high})"
            )));
        }
        text[(*value - low) as usize] = b'1';
    }
    let text = String::from_utf8(text).unwrap_or_default();
    let mut source = Vector::new(LogicalType::VARCHAR, 1);
    source.set_value(0, &Value::varchar(text))?;
    let mut target = Vector::new(ty.clone(), 1);
    super::cast::cast_vector(&source, &mut target, 1, super::cast::CastMode::Strict)?;
    Ok(target.get_value(0))
}

fn mode(rows: &[Value], ty: &LogicalType) -> Result<Value> {
    let values: Vec<Value> = rows.iter().filter(|v| !v.is_null()).cloned().collect();
    if values.is_empty() {
        return Ok(Value::null(ty.clone()));
    }
    let keys = sort_keys(&values, ty)?;
    let mut counts: HashMap<&Vec<u8>, (usize, usize)> = HashMap::new();
    for (at, key) in keys.iter().enumerate() {
        counts.entry(key).or_insert((0, at)).0 += 1;
    }
    let (_, first) = counts
        .values()
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
        .expect("there are values");
    Ok(values[*first].clone())
}

fn histogram(rows: &[Value], ty: &LogicalType) -> Result<Value> {
    let values: Vec<Value> = rows.iter().filter(|v| !v.is_null()).cloned().collect();
    if values.is_empty() {
        return Ok(Value::null(ty.clone()));
    }
    let values = sorted(values)?;
    let key_type = values[0].logical_type().clone();
    let keys = sort_keys(&values, &key_type)?;
    let entry_type = ty.child_type().expect("a MAP has entries").clone();
    let mut entries = Vec::new();
    let mut at = 0;
    while at < values.len() {
        let same = keys[at..].iter().take_while(|k| **k == keys[at]).count();
        entries.push(Value::new(
            entry_type.clone(),
            ValueData::Struct(vec![values[at].clone(), Value::ubigint(same as u64)]),
        )?);
        at += same;
    }
    Value::new(ty.clone(), ValueData::List(entries))
}

// Ported from duckdb/extension/core_functions/aggregate/nested/binned_histogram.cpp @ rust-port-baseline
/// `HistogramBinFunction`: the counts per bin of the bins the first row gives, and of the values above all of them
/// (or, `exact`, equal to none of them) under the key of the "other values" bucket.
fn binned_histogram(rows: &[Value], ty: &LogicalType, exact: bool) -> Result<Value> {
    let chosen: Vec<&Value> = rows.iter().filter(|row| !fields(row)[0].is_null()).collect();
    let Some(first) = chosen.first() else {
        return Ok(Value::null(ty.clone()));
    };
    let bins = &fields(first)[1];
    let ValueData::List(boundaries) = bins.data() else {
        return Err(Error::new(ErrorKind::Binder, "Histogram bin list cannot be NULL"));
    };
    if boundaries.iter().any(Value::is_null) {
        return Err(Error::new(ErrorKind::Binder, "Histogram bin entry cannot be NULL"));
    }
    let key_type = ty
        .child_type()
        .and_then(|entry| entry.struct_children().first())
        .map(|(_, t)| t.clone())
        .unwrap_or(LogicalType::SQLNULL);
    let mut boundaries = sorted(boundaries.clone())?;
    let mut keys = sort_keys(&boundaries, &key_type)?;
    let mut at = 1;
    while at < boundaries.len() {
        if keys[at] == keys[at - 1] {
            boundaries.remove(at);
            keys.remove(at);
        } else {
            at += 1;
        }
    }
    let mut counts = vec![0u64; boundaries.len() + 1];
    let values: Vec<Value> = chosen.iter().map(|row| fields(row)[0].clone()).collect();
    for key in sort_keys(&values, &key_type)? {
        let bin = keys.partition_point(|k| *k < key);
        let bin = if exact && keys.get(bin) != Some(&key) {
            keys.len()
        } else {
            bin
        };
        counts[bin] += 1;
    }
    let entry_type = ty.child_type().expect("a MAP has entries").clone();
    let mut entries = Vec::with_capacity(counts.len());
    for (boundary, count) in boundaries.iter().zip(&counts) {
        entries.push(Value::new(
            entry_type.clone(),
            ValueData::Struct(vec![boundary.clone(), Value::ubigint(*count)]),
        )?);
    }
    let others = counts[boundaries.len()];
    if others > 0
        && let Some(other) = other_bucket_value(&key_type)?
    {
        let other_key = sort_keys(std::slice::from_ref(&other), &key_type)?;
        if keys.iter().any(|k| *k == other_key[0]) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "Histogram bin {} collides with the key of the \"other values\" bucket, which has entries - use a \
                     different bin value",
                    BoundExpr::constant(other.clone())
                ),
            ));
        }
        entries.push(Value::new(
            entry_type,
            ValueData::Struct(vec![other, Value::ubigint(others)]),
        )?);
    }
    Value::new(ty.clone(), ValueData::List(entries))
}

/// How two values of `ty` order, as their sort keys do.
pub(super) fn compare(left: &Value, right: &Value, ty: &LogicalType) -> Result<std::cmp::Ordering> {
    let keys = sort_keys(&[left.clone(), right.clone()], ty)?;
    Ok(keys[0].cmp(&keys[1]))
}

/// `Value::NotDistinctFrom` of two values of `ty`.
pub(super) fn not_distinct(left: &Value, right: &Value, ty: &LogicalType) -> Result<bool> {
    let keys = sort_keys(&[left.clone(), right.clone()], ty)?;
    Ok(keys[0] == keys[1])
}

/// `SupportsOtherBucket` + `OtherBucketValue`: the key the values in no bin are counted under, if the type has one.
pub(super) fn other_bucket_value(ty: &LogicalType) -> Result<Option<Value>> {
    use LogicalTypeId as L;
    if ty.alias().is_some() {
        return Ok(None);
    }
    let cast = |text: &str| crate::execution::parquet::cast_value(&Value::varchar(text), ty).map(Some);
    match ty.id() {
        L::TinyInt => Ok(Some(Value::tinyint(i8::MAX))),
        L::SmallInt => Ok(Some(Value::smallint(i16::MAX))),
        L::Integer => Ok(Some(Value::integer(i32::MAX))),
        L::BigInt => Ok(Some(Value::bigint(i64::MAX))),
        L::HugeInt => Ok(Some(Value::hugeint(i128::MAX))),
        L::UTinyInt => Ok(Some(Value::new(ty.clone(), ValueData::UInt8(u8::MAX))?)),
        L::USmallInt => Ok(Some(Value::new(ty.clone(), ValueData::UInt16(u16::MAX))?)),
        L::UInteger => Ok(Some(Value::new(ty.clone(), ValueData::UInt32(u32::MAX))?)),
        L::UBigInt => Ok(Some(Value::ubigint(u64::MAX))),
        L::UHugeInt => Ok(Some(Value::new(ty.clone(), ValueData::UInt128(u128::MAX))?)),
        L::Decimal => {
            let (width, scale) = ty.decimal_properties().unwrap_or((18, 3));
            let digits = "9".repeat(usize::from(width - scale));
            let fraction = "9".repeat(usize::from(scale));
            let text = if scale == 0 {
                digits
            } else {
                format!("{digits}.{fraction}")
            };
            cast(&text)
        }
        L::Time => cast("24:00:00"),
        L::TimeTz => cast("24:00:00-15:59:59"),
        L::Date | L::Timestamp | L::TimestampTz | L::TimestampSec | L::TimestampMs | L::TimestampNs => cast("infinity"),
        L::Float => Ok(Some(Value::float(f32::INFINITY))),
        L::Double => Ok(Some(Value::double(f64::INFINITY))),
        L::Varchar => Ok(Some(Value::varchar(""))),
        L::Blob => Ok(Some(Value::blob(Vec::new()))),
        L::Struct => {
            let fields = ty
                .struct_children()
                .iter()
                .map(|(_, t)| Value::null(t.clone()))
                .collect();
            Ok(Some(Value::new(ty.clone(), ValueData::Struct(fields))?))
        }
        L::List => Ok(Some(Value::new(ty.clone(), ValueData::List(Vec::new()))?)),
        _ => Ok(None),
    }
}

fn distinct_count(rows: &[Value]) -> Result<i64> {
    let values: Vec<Value> = rows.iter().filter(|v| !v.is_null()).cloned().collect();
    let Some(first) = values.first() else {
        return Ok(0);
    };
    let mut keys = sort_keys(&values, &first.logical_type().clone())?;
    keys.sort();
    keys.dedup();
    Ok(keys.len() as i64)
}

/// `bit_and` and the like over BIT: every value must have the same length.
fn bit_strings(rows: &[Value], ty: &LogicalType, name: &str, combine: fn(u8, u8) -> u8) -> Result<Value> {
    let mut result: Option<Vec<u8>> = None;
    for row in rows {
        let ValueData::Bytes(value) = row.data() else {
            continue;
        };
        result = Some(match result {
            None => value.clone(),
            Some(bits) if bit::bit_length(&bits) == bit::bit_length(value) => bit::bitwise(&bits, value, combine),
            Some(_) => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("Cannot {name} bit strings of different sizes"),
                ));
            }
        });
    }
    result.map_or_else(
        || Ok(Value::null(ty.clone())),
        |bits| Value::new(ty.clone(), ValueData::Bytes(bits)),
    )
}

fn bits(rows: &[Value], ty: &LogicalType, combine: fn(u128, u128) -> u128) -> Result<Value> {
    let mut result: Option<u128> = None;
    for row in rows {
        if let Some(value) = row.as_i128() {
            let value = value as u128;
            result = Some(result.map_or(value, |bits| combine(bits, value)));
        }
    }
    match result {
        Some(bits) => cast(Value::hugeint(bits as i128), ty),
        None => Ok(Value::null(ty.clone())),
    }
}

/// `StateFinalize`.
pub fn finish(kind: Holistic, rows: &[Value], ty: &LogicalType) -> Result<Value> {
    use Holistic::*;
    // the aggregates whose state is C++'s: the rows folded and finalized as C++ updates and finalizes it, so the
    // result is C++'s to the last bit
    if matches!(
        kind,
        StddevSamp
            | StddevPop
            | VarSamp
            | VarPop
            | Sem
            | CovarPop
            | CovarSamp
            | Corr
            | Skewness
            | Kurtosis
            | KurtosisPop
            | Fsum
            | Favg
            | Product
            | RegrSlope
            | RegrIntercept
            | RegrR2
            | RegrSxx
            | RegrSyy
            | RegrSxy
            | RegrAvgX
            | RegrAvgY
            | ApproxCountDistinct
            | ApproxQuantile
    ) && let Some(layout) = super::holistic_states::layout(kind, &[], ty)
    {
        let state = super::holistic_states::from_rows(kind, rows, &layout)?;
        // what the arguments after the first are, which the rows carry (approx_quantile's fractions)
        let constants: Vec<Option<Value>> = match rows.first().map(Value::data) {
            Some(ValueData::Struct(values)) if kind == ApproxQuantile => {
                values.iter().map(|v| Some(v.clone())).collect()
            }
            _ => Vec::new(),
        };
        return super::holistic_states::finalize(kind, &state, ty, &constants);
    }
    let null = || Value::null(ty.clone());
    Ok(match kind {
        StringAgg => {
            let mut out: Option<String> = None;
            for row in rows {
                let values = fields(row);
                if values[0].is_null() {
                    continue;
                }
                let text = values[0].try_to_string()?;
                match &mut out {
                    None => out = Some(text),
                    Some(joined) => {
                        match values.get(1) {
                            // `StringAggBind`: a NULL separator leaves the bound one, a comma
                            Some(separator) if separator.is_null() => joined.push(','),
                            Some(separator) => *joined += &separator.try_to_string()?,
                            None => joined.push(','),
                        }
                        *joined += &text;
                    }
                }
            }
            out.map_or_else(null, Value::varchar)
        }
        BoolAnd | BoolOr => {
            let flags: Vec<bool> = rows
                .iter()
                .filter_map(|row| match row.data() {
                    ValueData::Bool(flag) => Some(*flag),
                    _ => None,
                })
                .collect();
            if flags.is_empty() {
                null()
            } else if kind == BoolAnd {
                Value::boolean(flags.iter().all(|f| *f))
            } else {
                Value::boolean(flags.iter().any(|f| *f))
            }
        }
        BitAnd if ty.id() == LogicalTypeId::Bit => bit_strings(rows, ty, "AND", |a, b| a & b)?,
        BitOr if ty.id() == LogicalTypeId::Bit => bit_strings(rows, ty, "OR", |a, b| a | b)?,
        BitXor if ty.id() == LogicalTypeId::Bit => bit_strings(rows, ty, "XOR", |a, b| a ^ b)?,
        BitAnd => bits(rows, ty, |a, b| a & b)?,
        BitOr => bits(rows, ty, |a, b| a | b)?,
        BitXor => bits(rows, ty, |a, b| a ^ b)?,
        Product => {
            let values = numbers(rows);
            if values.is_empty() {
                null()
            } else {
                Value::double(values.iter().product())
            }
        }
        Mode => mode(rows, ty)?,
        // `MedianFunction`: a type that cannot be interpolated takes the discrete median. Its rows are
        // its values (a STRUCT value is not the arguments of a quantile)
        Median => {
            let rows: Vec<Value> = rows.iter().map(|r| Value::struct_value(vec![("a0".into(), r.clone())])).collect();
            quantiles(&rows, ty, can_interpolate(ty))?
        }
        QuantileCont => quantiles(rows, ty, true)?,
        QuantileDisc => quantiles(rows, ty, false)?,
        Mad if ty.id() == LogicalTypeId::Interval => temporal_mad(rows)?,
        Mad => {
            let mut values = numbers(rows);
            if values.is_empty() {
                return Ok(null());
            }
            let median = median_of(&mut values);
            let mut deviations: Vec<f64> = values.iter().map(|v| (v - median).abs()).collect();
            Value::double(median_of(&mut deviations))
        }
        StddevSamp | StddevPop | VarSamp | VarPop => {
            let values = numbers(rows);
            let sample = matches!(kind, StddevSamp | VarSamp);
            if values.is_empty() || (sample && values.len() < 2) {
                return Ok(null());
            }
            let (n, _, square, _, _) = moments(&values);
            let variance = square / if sample { n - 1.0 } else { n };
            match kind {
                // IEEE-754: an overflowing variance is infinite (`STDDevBaseOperation`)
                StddevSamp | StddevPop => Value::double(variance.sqrt()),
                _ => Value::double(variance),
            }
        }
        Sem => {
            let values = numbers(rows);
            if values.is_empty() {
                return Ok(null());
            }
            let (n, _, square, _, _) = moments(&values);
            Value::double((square / n).sqrt() / n.sqrt())
        }
        AvgInterval => {
            let (mut months, mut days, mut micros, mut count) = (0i32, 0i32, 0i64, 0i64);
            for row in rows {
                let ValueData::Interval(value) = row.data() else {
                    continue;
                };
                let overflow = || out_of_range("Overflow in addition of INT32");
                months = months.checked_add(value.months).ok_or_else(overflow)?;
                days = days.checked_add(value.days).ok_or_else(overflow)?;
                micros = micros
                    .checked_add(value.micros)
                    .ok_or_else(|| out_of_range("Overflow in addition of INT64"))?;
                count += 1;
            }
            if count == 0 {
                return Ok(null());
            }
            let (months, days) = (i64::from(months), i64::from(days));
            let months_remainder = (months % count) * Interval::DAYS_PER_MONTH;
            let day_total = days / count + months_remainder / count;
            let days_remainder = days % count + months_remainder % count;
            let shifted = i128::from(days_remainder) * i128::from(Interval::MICROS_PER_DAY) / i128::from(count);
            let micro_total = micros / count + micros % count + shifted as i64;
            Value::interval(Interval::new((months / count) as i32, day_total as i32, micro_total))
        }
        AvgTimestamp => {
            let values: Vec<i64> = rows
                .iter()
                .filter_map(|row| match row.data() {
                    ValueData::Int64(value) => Some(*value),
                    _ => None,
                })
                .collect();
            if values.is_empty() {
                return Ok(null());
            }
            let positive = values.contains(&i64::MAX);
            let negative = values.contains(&-i64::MAX);
            let average = match (positive, negative) {
                (true, true) => return Ok(null()),
                (true, false) => i64::MAX,
                (false, true) => -i64::MAX,
                _ => {
                    let sum: i128 = values.iter().map(|v| i128::from(*v)).sum();
                    let count = values.len() as i128;
                    (sum / count + i128::from(sum % count > count / 2)) as i64
                }
            };
            Value::new(ty.clone(), ValueData::Int64(average))?
        }
        AvgTimeTz => {
            use duckrs_common::types::{Time, TimeTz};
            // `Time::NormalizeTimeTZ`: the time at +00, within the day
            let micros: Vec<i128> = rows
                .iter()
                .filter_map(|row| match row.data() {
                    ValueData::Int64(bits) => {
                        let value = TimeTz(*bits as u64);
                        let utc = value.time().0 - i64::from(value.offset()) * 1_000_000;
                        Some(i128::from(utc.rem_euclid(Interval::MICROS_PER_DAY)))
                    }
                    _ => None,
                })
                .collect();
            if micros.is_empty() {
                return Ok(null());
            }
            let sum: i128 = micros.iter().sum();
            let count = micros.len() as i128;
            let average = sum / count + i128::from(sum % count > count / 2);
            Value::time_tz(TimeTz::new(Time(average as i64), 0))
        }
        Skewness => {
            let values = numbers(rows);
            if values.len() <= 2 {
                return Ok(null());
            }
            // the running central moments stay exactly 0 over constant input
            let (mut seen, mut mean, mut m2, mut m3) = (0.0, 0.0, 0.0, 0.0);
            for value in &values {
                let previous = seen;
                seen += 1.0;
                let delta = value - mean;
                let delta_n = delta / seen;
                let term = delta * delta_n * previous;
                mean += delta_n;
                m3 += term * delta_n * (seen - 2.0) - 3.0 * delta_n * m2;
                m2 += term;
            }
            if m2 <= 0.0 {
                return Ok(null());
            }
            let n = seen;
            let g1 = n.sqrt() * (m3 / m2) / m2.sqrt();
            checked("SKEW", (n * (n - 1.0)).sqrt() / (n - 2.0) * g1)?
        }
        Kurtosis => kurtosis(&numbers(rows), true)?,
        Entropy => {
            let values: Vec<Value> = rows.iter().filter(|v| !v.is_null()).cloned().collect();
            if values.is_empty() {
                return Ok(Value::double(0.0));
            }
            let keys = sort_keys(&values, &values[0].logical_type().clone())?;
            let mut counts: HashMap<&Vec<u8>, f64> = HashMap::new();
            for key in &keys {
                *counts.entry(key).or_insert(0.0) += 1.0;
            }
            let total = keys.len() as f64;
            Value::double(counts.values().map(|c| -(c / total) * (c / total).log2()).sum())
        }
        DecimalAverage => {
            // `DecimalAvgOperation`: an i128 sum and a count, then round half to even
            let mut sum: i128 = 0;
            let mut count: u128 = 0;
            for row in rows.iter().filter(|v| !v.is_null()) {
                let value = row.as_i128().unwrap_or(0);
                sum = sum.checked_add(value).ok_or_else(|| {
                    out_of_range("decimal_average: intermediate sum overflowed HUGEINT range")
                })?;
                count += 1;
            }
            if count == 0 {
                return Ok(null());
            }
            let negative = sum < 0;
            let magnitude = sum.unsigned_abs();
            let (mut quotient, remainder) = (magnitude / count, magnitude % count);
            let distance = count - remainder;
            if distance < remainder || (distance == remainder && quotient % 2 == 1) {
                quotient += 1;
            }
            let signed = if negative {
                (quotient as i128).wrapping_neg()
            } else {
                quotient as i128
            };
            let (width, scale) = ty.decimal_properties().unwrap_or((38, 0));
            Value::decimal(signed, width, scale)?
        }
        VariantGroupObject => {
            use duckrs_common::variant::VariantNode;
            if rows.is_empty() {
                return Ok(null());
            }
            let mut children: Vec<(String, VariantNode)> = Vec::with_capacity(rows.len());
            for row in rows {
                let values = fields(row);
                if values[0].is_null() {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "variant_group_object key cannot be NULL",
                    ));
                }
                let key = values[0].try_to_string()?;
                let value = match values[1].data() {
                    ValueData::Bytes(bytes) if !values[1].is_null() => VariantNode::decode(bytes)?,
                    _ => VariantNode::Null,
                };
                children.push((key, value));
            }
            children.sort_by(|a, b| a.0.cmp(&b.0));
            if let Some(pair) = children.windows(2).find(|pair| pair[0].0 == pair[1].0) {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("variant_group_object contains duplicate key \"{}\"", pair[0].0),
                ));
            }
            Value::new(ty.clone(), ValueData::Bytes(VariantNode::Object(children).encode()))?
        }
        ArgMin => arg_extreme(rows, ty, false)?,
        ArgMax => arg_extreme(rows, ty, true)?,
        ArgMinNull => arg_extreme_nulls(rows, ty, false, false)?,
        ArgMaxNull => arg_extreme_nulls(rows, ty, true, false)?,
        ArgMinNullsLast => arg_extreme_nulls(rows, ty, false, true)?,
        ArgMaxNullsLast => arg_extreme_nulls(rows, ty, true, true)?,
        MinN => extremes(rows, ty, false, 0, 0, 1)?,
        MaxN => extremes(rows, ty, true, 0, 0, 1)?,
        ArgMinN => extremes(rows, ty, false, 0, 1, 2)?,
        ArgMaxN => extremes(rows, ty, true, 0, 1, 2)?,
        ArgMinNullsLastN => extremes_nulls_last(rows, ty, false)?,
        ArgMaxNullsLastN => extremes_nulls_last(rows, ty, true)?,
        ApproxTopK => top_k(rows, ty)?,
        BitstringAgg => bitstring(rows, ty)?,
        CombineAggr => {
            let mut combined = super::aggregates::empty_state_value(ty)?;
            for row in rows {
                // a state counts as often as its multiplicity says; a state of C++'s layout can be a
                // STRUCT itself, and is told apart by its type
                let is_state = row.logical_type().aggregate_state_info().is_some();
                let (state, times) = match row.data() {
                    ValueData::Struct(parts) if !is_state => (&parts[0], parts[1].as_i128().unwrap_or(0)),
                    _ => (row, 1),
                };
                if times < 0 {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "combine_aggr multiplicity must be non-negative",
                    ));
                }
                let scaled = super::aggregates::repeat_state(state, times, ty)?;
                combined = super::aggregates::combine_state_values(&combined, &scaled, ty)?;
            }
            combined
        }
        KurtosisPop => kurtosis(&numbers(rows), false)?,
        CountIf if rows.is_empty() => null(),
        CountIf => {
            let count = rows
                .iter()
                .filter(|r| matches!(r.data(), ValueData::Bool(true)))
                .count();
            Value::hugeint(count as i128)
        }
        Histogram => histogram(rows, ty)?,
        ApproxQuantile => unreachable!("computed from its t-digest above"),
        Sorted if rows.is_empty() => null(),
        Sorted => Value::new(ty.state_layout(), ValueData::List(rows.to_vec()))?,
        HistogramBins => binned_histogram(rows, ty, false)?,
        HistogramExact => binned_histogram(rows, ty, true)?,
        ApproxCountDistinct => Value::bigint(distinct_count(rows)?),
        Fsum | Favg => {
            let values = numbers(rows);
            if values.is_empty() {
                return Ok(null());
            }
            let (mut sum, mut compensation) = (0.0, 0.0);
            for value in &values {
                let next = sum + value;
                compensation += if f64::abs(sum) >= f64::abs(*value) {
                    (sum - next) + value
                } else {
                    (value - next) + sum
                };
                sum = next;
            }
            let total = sum + compensation;
            Value::double(if kind == Fsum {
                total
            } else {
                total / values.len() as f64
            })
        }
        GeoMean => {
            let values = numbers(rows);
            if values.is_empty() {
                return Ok(null());
            }
            Value::double((values.iter().map(|v| v.ln()).sum::<f64>() / values.len() as f64).exp())
        }
        Corr | CovarPop | CovarSamp | RegrSlope | RegrIntercept | RegrR2 | RegrAvgX | RegrAvgY | RegrSxx | RegrSyy
        | RegrSxy => {
            let pairs = pairs(rows);
            if pairs.is_empty() {
                return Ok(null());
            }
            let r = regression(&pairs);
            // a single pair has no spread at all, whatever rounding leaves in the sums
            let (var_x, var_y) = if r.n > 1.0 {
                (r.sxx / r.n, r.syy / r.n)
            } else {
                (0.0, 0.0)
            };
            let slope = if var_x != 0.0 { (r.sxy / r.n) / var_x } else { f64::NAN };
            match kind {
                Corr => {
                    let spread = var_x.sqrt() * var_y.sqrt();
                    Value::double(if spread != 0.0 {
                        (r.sxy / r.n) / spread
                    } else {
                        f64::NAN
                    })
                }
                CovarPop => Value::double(r.sxy / r.n),
                CovarSamp if r.n < 2.0 => null(),
                CovarSamp => Value::double(r.sxy / (r.n - 1.0)),
                RegrSlope => Value::double(slope),
                RegrIntercept if slope.is_nan() => null(),
                RegrIntercept => Value::double(r.y - slope * r.x),
                RegrR2 if r.sxx == 0.0 => null(),
                RegrR2 if r.syy == 0.0 => Value::double(1.0),
                RegrR2 => Value::double(r.sxy * r.sxy / (r.sxx * r.syy)),
                RegrAvgX => Value::double(r.x),
                RegrAvgY => Value::double(r.y),
                RegrSxx => Value::double(r.sxx),
                RegrSyy => Value::double(r.syy),
                _ => Value::double(r.sxy),
            }
        }
        RegrCount => Value::uinteger(pairs(rows).len() as u32),
    })
}
