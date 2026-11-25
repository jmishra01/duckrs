// Ported from the `STATE_TYPE`s, `Operation`s, `Combine`s and `Finalize`s of
// duckdb/extension/core_functions/aggregate/distributive/{bool,product}.cpp,
// include/core_functions/aggregate/algebraic/{stddev,covar,corr}.hpp,
// include/core_functions/aggregate/sum_helpers.hpp and aggregate/algebraic/avg.cpp @ rust-port-baseline
//
//! The states C++ exports for aggregates duckrs computes from their rows ([`Holistic`]). The rows
//! are folded into the state the way C++ updates it, one at a time and in order, so the exported
//! state is C++'s; a state read back is combined and finalized as C++ does.

use super::holistic::Holistic;
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::{Error, ErrorKind, Result};

fn failure() -> Error {
    Error::new(ErrorKind::InvalidInput, "Invalid aggregate state")
}

fn overflow() -> Error {
    Error::new(ErrorKind::OutOfRange, "Overflow in aggregate state combine")
}

fn struct_of(fields: &[(&str, LogicalType)]) -> LogicalType {
    LogicalType::struct_type(fields.iter().map(|(n, t)| (n.to_string(), t.clone())).collect())
}

fn stddev_type() -> LogicalType {
    let d = LogicalType::DOUBLE;
    struct_of(&[("count", LogicalType::UBIGINT), ("mean", d.clone()), ("dsquared", d)])
}

fn covar_type() -> LogicalType {
    let d = LogicalType::DOUBLE;
    struct_of(&[
        ("count", LogicalType::UBIGINT),
        ("meanx", d.clone()),
        ("meany", d.clone()),
        ("co_moment", d),
    ])
}

fn corr_type() -> LogicalType {
    struct_of(&[
        ("cov_pop", covar_type()),
        ("dev_pop_x", stddev_type()),
        ("dev_pop_y", stddev_type()),
    ])
}

fn slope_type() -> LogicalType {
    struct_of(&[("cov_pop", covar_type()), ("var_pop", stddev_type())])
}

/// `GetStateType` of the aggregates below for arguments and a result of these types; `None` for the others.
pub fn layout(kind: Holistic, arguments: &[LogicalType], result: &LogicalType) -> Option<LogicalType> {
    use Holistic::*;
    let d = LogicalType::DOUBLE;
    Some(match kind {
        // `ArgMinMaxState`: `OptionalStateType<StructStateType<OptionalStateType<StateTypedValue<A, StateReturnType>>,
        // OptionalStateType<StateTypedValue<B, StateInputType<1>>>>>`; the sort keys C++ keeps of nested values are
        // exported as the values
        ArgMin | ArgMax | ArgMinNull | ArgMaxNull | ArgMinNullsLast | ArgMaxNullsLast => {
            struct_of(&[("arg", result.clone()), ("by", arguments.get(1)?.clone())])
        }
        // `OptionalStateType<bool>`
        BoolAnd | BoolOr => LogicalType::BOOLEAN,
        // `BitState<T>`: `OptionalStateType<T>` of an integer (a BIT keeps its state opaque)
        BitAnd | BitOr | BitXor
            if result.is_integral() && result.id() != duckrs_common::types::LogicalTypeId::UHugeInt =>
        {
            result.clone()
        }
        // `StringAggState`: `OptionalStateType<StateString<StateReturnType>>`
        StringAgg => LogicalType::VARCHAR,
        // `OptionalStateType<double>`
        Product => d,
        // `StddevState`
        StddevSamp | StddevPop | VarSamp | VarPop | Sem => stddev_type(),
        // `CovarState`
        CovarPop | CovarSamp => covar_type(),
        // `CorrState`
        Corr => corr_type(),
        // `AvgState<hugeint_t>` (`TimestampAverageStateLayout`, `TimeAverageStateLayout`)
        AvgTimestamp | AvgTimeTz => struct_of(&[("count", LogicalType::UBIGINT), ("value", LogicalType::HUGEINT)]),
        // `IntervalAvgState`
        AvgInterval => struct_of(&[("count", LogicalType::BIGINT), ("value", LogicalType::INTERVAL)]),
        // `QuantileStateLayout`: the values buffered, `StateListType<StateInputType<0>>`
        QuantileCont | QuantileDisc | Median | Mad => LogicalType::list(arguments.first()?.clone()),
        // `ApproxQuantileExportType`: the count, the exact bounds and the centroids of the t-digest
        ApproxQuantile => struct_of(&[
            ("count", LogicalType::UBIGINT),
            ("min", d.clone()),
            ("max", d.clone()),
            (
                "centroids",
                LogicalType::list(struct_of(&[("mean", d.clone()), ("weight", d.clone())])),
            ),
        ]),
        // `ApproxCountDistinctExportType`: the sketch itself
        ApproxCountDistinct => struct_of(&[
            ("precision", LogicalType::UTINYINT),
            ("registers", LogicalType::list(LogicalType::UTINYINT)),
        ]),
        // `uint64_t`
        RegrCount => LogicalType::UBIGINT,
        // `RegrState`
        RegrAvgX | RegrAvgY => struct_of(&[("sum", d.clone()), ("count", LogicalType::UBIGINT)]),
        // `RegrSState`
        RegrSxx | RegrSyy => struct_of(&[("count", LogicalType::UBIGINT), ("var_pop", stddev_type())]),
        // `RegrSXyState`
        RegrSxy => struct_of(&[("count", LogicalType::UBIGINT), ("cov_pop", covar_type())]),
        // `RegrSlopeState`
        RegrSlope => slope_type(),
        // `RegrInterceptState`
        RegrIntercept => struct_of(&[
            ("count", LogicalType::UBIGINT),
            ("sum_x", d.clone()),
            ("sum_y", d.clone()),
            ("slope", slope_type()),
        ]),
        // `RegrR2State`
        RegrR2 => struct_of(&[
            ("corr", corr_type()),
            ("var_pop_x", stddev_type()),
            ("var_pop_y", stddev_type()),
        ]),
        // `SkewState`
        Skewness => struct_of(&[
            ("n", LogicalType::UBIGINT),
            ("mean", d.clone()),
            ("m2", d.clone()),
            ("m3", d.clone()),
        ]),
        // `KurtosisState`
        Kurtosis | KurtosisPop => struct_of(&[
            ("n", LogicalType::UBIGINT),
            ("mean", d.clone()),
            ("m2", d.clone()),
            ("m3", d.clone()),
            ("m4", d.clone()),
        ]),
        // `KahanSumState`: `OptionalStateType<StructStateType<double, double>>`
        Fsum => struct_of(&[("value", d.clone()), ("err", d)]),
        // `KahanAvgState`
        Favg => struct_of(&[("count", LogicalType::UBIGINT), ("value", d.clone()), ("err", d)]),
        _ => return None,
    })
}

#[derive(Clone, Copy, Default)]
struct Dev {
    count: u64,
    mean: f64,
    dsquared: f64,
}

impl Dev {
    /// `STDDevBaseOperation::Execute`
    fn update(&mut self, input: f64) {
        self.count += 1;
        let differential = (input - self.mean) / self.count as f64;
        let mean = self.mean + differential;
        self.dsquared += (input - mean) * (input - self.mean);
        self.mean = mean;
    }

    /// `STDDevBaseOperation::Combine`
    fn combine(&mut self, source: &Dev) -> Result<()> {
        if self.count == 0 {
            *self = *source;
        } else if source.count > 0 {
            let count = self.count.checked_add(source.count).ok_or_else(overflow)?;
            let (target_count, source_count, total) = (self.count as f64, source.count as f64, count as f64);
            let delta = source.mean - self.mean;
            let mean = (source_count / total).mul_add(delta, self.mean);
            self.dsquared = source.dsquared + self.dsquared + delta * delta * source_count * target_count / total;
            self.mean = mean;
            self.count = count;
        }
        Ok(())
    }

    fn value(&self) -> Value {
        Value::struct_value(vec![
            ("count".into(), Value::ubigint(self.count)),
            ("mean".into(), Value::double(self.mean)),
            ("dsquared".into(), Value::double(self.dsquared)),
        ])
    }

    fn read(value: &Value) -> Result<Dev> {
        let [count, mean, dsquared] = fields(value)? else {
            return Err(failure());
        };
        Ok(Dev {
            count: unsigned(count)?,
            mean: double(mean)?,
            dsquared: double(dsquared)?,
        })
    }

    fn variance(&self) -> f64 {
        if self.count > 1 {
            self.dsquared / self.count as f64
        } else {
            0.0
        }
    }

    fn population(&self) -> f64 {
        if self.count > 1 {
            (self.dsquared / self.count as f64).sqrt()
        } else {
            0.0
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Covar {
    count: u64,
    meanx: f64,
    meany: f64,
    co_moment: f64,
}

impl Covar {
    /// `CovarOperation::Operation`
    fn update(&mut self, y: f64, x: f64) {
        self.count += 1;
        let n = self.count as f64;
        let dx = x - self.meanx;
        let meanx = self.meanx + dx / n;
        let dy = y - self.meany;
        let meany = self.meany + dy / n;
        self.co_moment += dx * (y - meany);
        self.meanx = meanx;
        self.meany = meany;
    }

    /// `CovarOperation::Combine`
    fn combine(&mut self, source: &Covar) -> Result<()> {
        if self.count == 0 {
            *self = *source;
        } else if source.count > 0 {
            let count = self.count.checked_add(source.count).ok_or_else(overflow)?;
            let (target_count, source_count, total) = (self.count as f64, source.count as f64, count as f64);
            let meanx = (source_count * source.meanx + target_count * self.meanx) / total;
            let meany = (source_count * source.meany + target_count * self.meany) / total;
            let deltax = self.meanx - source.meanx;
            let deltay = self.meany - source.meany;
            self.co_moment = source.co_moment + self.co_moment + deltax * deltay * source_count * target_count / total;
            self.meanx = meanx;
            self.meany = meany;
            self.count = count;
        }
        Ok(())
    }

    fn value(&self) -> Value {
        Value::struct_value(vec![
            ("count".into(), Value::ubigint(self.count)),
            ("meanx".into(), Value::double(self.meanx)),
            ("meany".into(), Value::double(self.meany)),
            ("co_moment".into(), Value::double(self.co_moment)),
        ])
    }

    fn read(value: &Value) -> Result<Covar> {
        let [count, meanx, meany, co_moment] = fields(value)? else {
            return Err(failure());
        };
        Ok(Covar {
            count: unsigned(count)?,
            meanx: double(meanx)?,
            meany: double(meany)?,
            co_moment: double(co_moment)?,
        })
    }
}

/// `SkewState` and `KurtosisState`: online central moments; skewness keeps no fourth one.
#[derive(Clone, Copy, Default)]
struct Moments {
    n: u64,
    mean: f64,
    m2: f64,
    m3: f64,
    m4: f64,
}

impl Moments {
    /// `KurtosisOperation::Operation` (and `SkewnessOperation::Operation`, which is the same without `m4`)
    fn update(&mut self, input: f64) {
        let n1 = self.n as f64;
        self.n += 1;
        let n = self.n as f64;
        let delta = input - self.mean;
        let delta_n = delta / n;
        let delta_n2 = delta_n * delta_n;
        let term1 = delta * delta_n * n1;
        self.mean += delta_n;
        self.m4 += term1 * delta_n2 * (n * n - 3.0 * n + 3.0) + 6.0 * delta_n2 * self.m2 - 4.0 * delta_n * self.m3;
        self.m3 += term1 * delta_n * (n - 2.0) - 3.0 * delta_n * self.m2;
        self.m2 += term1;
    }

    /// `KurtosisOperation::Combine`
    fn combine(&mut self, source: &Moments) -> Result<()> {
        if source.n == 0 {
            return Ok(());
        }
        if self.n == 0 {
            *self = *source;
            return Ok(());
        }
        let count = self.n.checked_add(source.n).ok_or_else(overflow)?;
        let (na, nb, n) = (self.n as f64, source.n as f64, count as f64);
        let delta = source.mean - self.mean;
        let delta_n = delta / n;
        let delta_n2 = delta_n * delta_n;
        self.m4 += source.m4
            + delta * delta_n * delta_n2 * na * nb * (na * na - na * nb + nb * nb)
            + 6.0 * delta_n2 * (na * na * source.m2 + nb * nb * self.m2)
            + 4.0 * delta_n * (na * source.m3 - nb * self.m3);
        self.m3 += source.m3 + delta * delta_n2 * na * nb * (na - nb) + 3.0 * delta_n * (na * source.m2 - nb * self.m2);
        self.m2 += source.m2 + delta * delta_n * na * nb;
        self.mean += delta_n * nb;
        self.n = count;
        Ok(())
    }

    fn value(&self, fourth: bool) -> Value {
        let mut fields = vec![
            ("n".into(), Value::ubigint(self.n)),
            ("mean".into(), Value::double(self.mean)),
            ("m2".into(), Value::double(self.m2)),
            ("m3".into(), Value::double(self.m3)),
        ];
        if fourth {
            fields.push(("m4".into(), Value::double(self.m4)));
        }
        Value::struct_value(fields)
    }

    fn read(value: &Value) -> Result<Moments> {
        let parts = fields(value)?;
        let (Some(n), Some(mean), Some(m2), Some(m3)) = (parts.first(), parts.get(1), parts.get(2), parts.get(3))
        else {
            return Err(failure());
        };
        Ok(Moments {
            n: unsigned(n)?,
            mean: double(mean)?,
            m2: double(m2)?,
            m3: double(m3)?,
            m4: parts.get(4).map_or(Ok(0.0), double)?,
        })
    }
}

fn corr_value(covar: &Covar, dev_x: &Dev, dev_y: &Dev) -> Value {
    Value::struct_value(vec![
        ("cov_pop".into(), covar.value()),
        ("dev_pop_x".into(), dev_x.value()),
        ("dev_pop_y".into(), dev_y.value()),
    ])
}

fn corr_read(value: &Value) -> Result<(Covar, Dev, Dev)> {
    let [covar, dev_x, dev_y] = fields(value)? else {
        return Err(failure());
    };
    Ok((Covar::read(covar)?, Dev::read(dev_x)?, Dev::read(dev_y)?))
}

/// `CorrOperation::Finalize`
fn corr_finalize(covar: &Covar, dev_x: &Dev, dev_y: &Dev) -> Option<f64> {
    if covar.count == 0 || dev_x.count == 0 || dev_y.count == 0 {
        return None;
    }
    let cov = covar.co_moment / covar.count as f64;
    let (std_x, std_y) = (dev_x.population(), dev_y.population());
    Some(if std_x * std_y != 0.0 {
        cov / (std_x * std_y)
    } else {
        f64::NAN
    })
}

fn slope_value(covar: &Covar, var: &Dev) -> Value {
    Value::struct_value(vec![("cov_pop".into(), covar.value()), ("var_pop".into(), var.value())])
}

fn slope_read(value: &Value) -> Result<(Covar, Dev)> {
    let [covar, var] = fields(value)? else {
        return Err(failure());
    };
    Ok((Covar::read(covar)?, Dev::read(var)?))
}

/// `RegrSlopeOperation::Finalize`
fn slope_finalize(covar: &Covar, var: &Dev) -> Option<f64> {
    if covar.count == 0 || var.count == 0 {
        return None;
    }
    let cov = covar.co_moment / covar.count as f64;
    let var_pop = var.variance();
    Some(if var_pop != 0.0 { cov / var_pop } else { f64::NAN })
}

/// `KahanAddInternal`
fn kahan_add(input: f64, summed: &mut f64, err: &mut f64) {
    let new = *summed + input;
    let input_part = new - *summed;
    let rounding = (*summed - (new - input_part)) + (input - input_part);
    *err -= rounding;
    *summed = new;
}

/// `KahanFinalize`
fn kahan_finalize(summed: f64, err: f64) -> f64 {
    if summed.is_finite() { summed - err } else { summed }
}

/// A state, in the form of its layout.
enum State {
    Bool(Option<bool>),
    /// `BitState`: unset, or the bits so far
    Bits(Option<i128>),
    Text(Option<String>),
    Product(Option<f64>),
    Dev(Dev),
    Covar(Covar),
    Corr(Covar, Dev, Dev),
    RegrCount(u64),
    /// the sum and the count; of x or of y
    RegrAvg(f64, u64),
    RegrS(u64, Dev),
    RegrSxy(u64, Covar),
    RegrSlope(Covar, Dev),
    /// count, sum of x, sum of y, and the slope's state
    RegrIntercept(u64, f64, f64, Covar, Dev),
    /// the correlation's state, and the variances of x and y
    RegrR2(Covar, Dev, Dev, Dev, Dev),
    /// the count and the sum of the microseconds; a state that saw an infinity has the maximum count and the
    /// infinities as its value (`TimestampAverageOperation`)
    AvgDiscrete(u64, i128),
    AvgInterval(i64, duckrs_common::types::Interval),
    Sketch(super::hyperloglog::HyperLogLog),
    /// `ApproxQuantileState`: the values seen, their digest, and the fractions asked for when they came as rows
    Digest(u64, super::tdigest::TDigest, Option<Value>),
    /// the values a quantile buffers, NULLs left out
    Buffer(Vec<Value>),
    /// the row recorded, as its argument and its "by" value, NULLs included
    ArgExtreme(Option<(Value, Value)>),
    /// the moments, and whether the fourth is kept (kurtosis) or not (skewness)
    Moments(Moments, bool),
    /// `KahanSumState`: unset, or the sum and its error
    Kahan(Option<(f64, f64)>),
    KahanAvg(u64, f64, f64),
}

/// `BitAndOperation`, `BitOrOperation` and `BitXorOperation` of two values.
fn bits(kind: Holistic, current: i128, input: i128) -> i128 {
    match kind {
        Holistic::BitAnd => current & input,
        Holistic::BitOr => current | input,
        _ => current ^ input,
    }
}

fn empty(kind: Holistic) -> State {
    use Holistic::*;
    match kind {
        BoolAnd | BoolOr => State::Bool(None),
        BitAnd | BitOr | BitXor => State::Bits(None),
        StringAgg => State::Text(None),
        Product => State::Product(None),
        CovarPop | CovarSamp => State::Covar(Covar::default()),
        Corr => State::Corr(Covar::default(), Dev::default(), Dev::default()),
        Fsum => State::Kahan(None),
        Skewness => State::Moments(Moments::default(), false),
        RegrCount => State::RegrCount(0),
        ApproxCountDistinct => State::Sketch(super::hyperloglog::HyperLogLog::default()),
        ApproxQuantile => State::Digest(0, super::tdigest::TDigest::new(DIGEST_COMPRESSION), None),
        QuantileCont | QuantileDisc | Median | Mad => State::Buffer(Vec::new()),
        AvgTimestamp | AvgTimeTz => State::AvgDiscrete(0, 0),
        AvgInterval => State::AvgInterval(0, duckrs_common::types::Interval::default()),
        ArgMin | ArgMax | ArgMinNull | ArgMaxNull | ArgMinNullsLast | ArgMaxNullsLast => State::ArgExtreme(None),
        RegrAvgX | RegrAvgY => State::RegrAvg(0.0, 0),
        RegrSxx | RegrSyy => State::RegrS(0, Dev::default()),
        RegrSxy => State::RegrSxy(0, Covar::default()),
        RegrSlope => State::RegrSlope(Covar::default(), Dev::default()),
        RegrIntercept => State::RegrIntercept(0, 0.0, 0.0, Covar::default(), Dev::default()),
        RegrR2 => State::RegrR2(
            Covar::default(),
            Dev::default(),
            Dev::default(),
            Dev::default(),
            Dev::default(),
        ),
        Kurtosis | KurtosisPop => State::Moments(Moments::default(), true),
        Favg => State::KahanAvg(0, 0.0, 0.0),
        _ => State::Dev(Dev::default()),
    }
}

fn fields(value: &Value) -> Result<&[Value]> {
    match value.data() {
        ValueData::Struct(fields) => Ok(fields),
        _ => Err(failure()),
    }
}

fn double(value: &Value) -> Result<f64> {
    match value.data() {
        ValueData::Double(v) => Ok(*v),
        ValueData::Null => Ok(0.0),
        _ => Err(failure()),
    }
}

fn unsigned(value: &Value) -> Result<u64> {
    match value.data() {
        ValueData::Null => Ok(0),
        _ => value.as_i128().and_then(|v| u64::try_from(v).ok()).ok_or_else(failure),
    }
}

fn number(value: &Value) -> Option<f64> {
    match value.data() {
        ValueData::Null => None,
        ValueData::Double(v) => Some(*v),
        ValueData::Float(v) => Some(f64::from(*v)),
        ValueData::Bool(v) => Some(if *v { 1.0 } else { 0.0 }),
        _ if value.logical_type().id() == LogicalTypeId::Decimal => value.try_to_string().ok()?.parse().ok(),
        _ => value.as_i128().map(|v| v as f64),
    }
}

/// The members of a row of an aggregate of two arguments, as a pair of numbers.
fn pair(row: &Value) -> Option<(f64, f64)> {
    let ValueData::Struct(values) = row.data() else {
        return None;
    };
    Some((number(values.first()?)?, number(values.get(1)?)?))
}

/// `StringAggFunction::PerformOperation`
fn append(state: &mut Option<String>, text: &str, separator: &str) {
    match state {
        None => *state = Some(text.to_string()),
        Some(joined) => {
            joined.push_str(separator);
            joined.push_str(text);
        }
    }
}

const INFINITE_COUNT: u64 = u64::MAX;
const POSITIVE_INFINITY: i128 = 1;
const NEGATIVE_INFINITY: i128 = 2;

/// `TimestampAverageOperation::AddInfinities`
fn add_infinities(count: &mut u64, value: &mut i128, infinities: i128) {
    if *count != INFINITE_COUNT {
        *count = INFINITE_COUNT;
        *value = 0;
    }
    *value |= infinities;
}

/// The microseconds a TIMETZ is at +00, within the day (`Time::NormalizeTimeTZ`).
fn normalized_time_tz(bits: i64) -> i128 {
    use duckrs_common::types::TimeTz;
    let value = TimeTz(bits as u64);
    let utc = value.time().0 - i64::from(value.offset()) * 1_000_000;
    i128::from(utc.rem_euclid(duckrs_common::types::Interval::MICROS_PER_DAY))
}

/// `COMPARATOR::Operation(left, right)` of arg_min (`LessThan`) or arg_max (`GreaterThan`).
fn better(kind: Holistic, left: &Value, right: &Value) -> Result<bool> {
    use Holistic::*;
    let ordering = super::holistic::compare(left, right, left.logical_type())?;
    Ok(match kind {
        ArgMax | ArgMaxNull | ArgMaxNullsLast => ordering == std::cmp::Ordering::Greater,
        _ => ordering == std::cmp::Ordering::Less,
    })
}

/// `ArgMinMaxBase::Operation`: whether the row may start the state, by the null handling of the function.
fn may_start(kind: Holistic, arg: &Value, by: &Value) -> bool {
    use Holistic::*;
    match kind {
        ArgMin | ArgMax => !arg.is_null() && !by.is_null(),
        ArgMinNull | ArgMaxNull => !by.is_null(),
        _ => true,
    }
}

/// The state the rows update an empty state to.
fn fold(kind: Holistic, rows: &[Value]) -> Result<State> {
    use Holistic::*;
    let mut state = empty(kind);
    // `ApproxQuantileOperation::Operation`: the values that are finite as doubles
    if let State::Digest(count, digest, fractions) = &mut state {
        for row in rows {
            let ValueData::Struct(values) = row.data() else {
                continue;
            };
            if fractions.is_none() {
                *fractions = values.get(1).cloned();
            }
            let Some(value) = encode_quantile_input(&values[0]) else {
                continue;
            };
            if !value.is_finite() {
                continue;
            }
            digest.add_value(value);
            *count += 1;
        }
        return Ok(state);
    }
    // `ApproxCountDistinctUpdateFunction`: the hashes of the values, as `VectorOperations::Hash` gives them
    if let State::Sketch(sketch) = &mut state {
        let values: Vec<&Value> = rows.iter().filter(|v| !v.is_null()).collect();
        if let Some(first) = values.first() {
            let mut vector = duckrs_common::vector::Vector::new(first.logical_type().clone(), values.len());
            for (at, value) in values.iter().enumerate() {
                vector.set_value(at, value)?;
            }
            let hashes = duckrs_common::hash::hash_vector(&vector, values.len())?;
            for at in 0..values.len() {
                if let Some(hash) = hashes.get_value(at).as_i128() {
                    sketch.insert(hash as u64);
                }
            }
        }
        return Ok(state);
    }
    for row in rows {
        match &mut state {
            State::ArgExtreme(current) => {
                let ValueData::Struct(values) = row.data() else {
                    continue;
                };
                let (arg, by) = (&values[0], &values[1]);
                match current {
                    None => {
                        if may_start(kind, arg, by) {
                            *current = Some((arg.clone(), by.clone()));
                        }
                    }
                    // `ArgMinMaxBase::Execute`
                    Some((current_arg, current_by)) => {
                        if !by.is_null()
                            && (current_by.is_null() || better(kind, by, current_by)?)
                            && (!matches!(kind, ArgMin | ArgMax) || !arg.is_null())
                        {
                            *current_arg = arg.clone();
                            *current_by = by.clone();
                        }
                    }
                }
            }
            State::Text(text) => {
                let (input, separator) = match row.data() {
                    ValueData::Struct(values) => (&values[0], values.get(1)),
                    _ => (row, None),
                };
                if input.is_null() {
                    continue;
                }
                let separator = match separator {
                    // `StringAggBind`: a NULL separator leaves the bound one, a comma
                    Some(separator) if separator.is_null() => ",".to_string(),
                    Some(separator) => separator.try_to_string()?,
                    None => ",".to_string(),
                };
                append(text, &input.try_to_string()?, &separator);
            }
            State::Bits(value) => {
                let Some(input) = row.as_i128() else { continue };
                *value = Some(match *value {
                    None => input,
                    Some(current) => bits(kind, current, input),
                });
            }
            State::Bool(value) => {
                let ValueData::Bool(input) = row.data() else { continue };
                let current = value.unwrap_or(kind == BoolAnd);
                *value = Some(if kind == BoolAnd {
                    current && *input
                } else {
                    current || *input
                });
            }
            State::Product(value) => {
                let Some(input) = number(row) else { continue };
                *value = Some(value.unwrap_or(1.0) * input);
            }
            State::Dev(dev) => {
                let Some(input) = number(row) else { continue };
                dev.update(input);
            }
            State::Covar(covar) => {
                let Some((y, x)) = pair(row) else { continue };
                covar.update(y, x);
            }
            State::Corr(covar, dev_x, dev_y) => {
                let Some((y, x)) = pair(row) else { continue };
                covar.update(y, x);
                dev_x.update(x);
                dev_y.update(y);
            }
            State::Moments(moments, _) => {
                let Some(input) = number(row) else { continue };
                moments.update(input);
            }
            State::Sketch(_) | State::Digest(..) => unreachable!("folded above"),
            State::Buffer(values) => {
                // the arguments of a quantile come packed; a median's row is its value
                let input = match row.data() {
                    ValueData::Struct(fields) if kind != Median => &fields[0],
                    _ => row,
                };
                if !input.is_null() {
                    values.push(input.clone());
                }
            }
            State::AvgDiscrete(count, value) => {
                let ValueData::Int64(input) = row.data() else { continue };
                let input = if kind == AvgTimeTz {
                    normalized_time_tz(*input)
                } else {
                    i128::from(*input)
                };
                // TIME has no infinities, and its maximum is no timestamp's
                let infinite = kind == AvgTimestamp
                    && row.logical_type().id() != LogicalTypeId::Time
                    && (input == i128::from(i64::MAX) || input == -i128::from(i64::MAX));
                if infinite {
                    add_infinities(
                        count,
                        value,
                        if input > 0 {
                            POSITIVE_INFINITY
                        } else {
                            NEGATIVE_INFINITY
                        },
                    );
                } else if *count != INFINITE_COUNT {
                    *count += 1;
                    *value += input;
                }
            }
            State::AvgInterval(count, value) => {
                let ValueData::Interval(input) = row.data() else {
                    continue;
                };
                *count += 1;
                *value = add_intervals(*value, *input)?;
            }
            State::RegrCount(count) => {
                if pair(row).is_some() {
                    *count += 1;
                }
            }
            State::RegrAvg(sum, count) => {
                let Some((y, x)) = pair(row) else { continue };
                *sum += if kind == RegrAvgX { x } else { y };
                *count += 1;
            }
            State::RegrS(count, dev) => {
                let Some((y, x)) = pair(row) else { continue };
                *count += 1;
                dev.update(if kind == RegrSxx { x } else { y });
            }
            State::RegrSxy(count, covar) => {
                let Some((y, x)) = pair(row) else { continue };
                *count += 1;
                covar.update(y, x);
            }
            State::RegrSlope(covar, var) => {
                let Some((y, x)) = pair(row) else { continue };
                covar.update(y, x);
                var.update(x);
            }
            State::RegrIntercept(count, sum_x, sum_y, covar, var) => {
                let Some((y, x)) = pair(row) else { continue };
                *count += 1;
                *sum_x += x;
                *sum_y += y;
                covar.update(y, x);
                var.update(x);
            }
            State::RegrR2(covar, dev_x, dev_y, var_x, var_y) => {
                let Some((y, x)) = pair(row) else { continue };
                covar.update(y, x);
                dev_x.update(x);
                dev_y.update(y);
                var_x.update(x);
                var_y.update(y);
            }
            State::Kahan(state) => {
                let Some(input) = number(row) else { continue };
                let (summed, err) = state.get_or_insert((0.0, 0.0));
                kahan_add(input, summed, err);
            }
            State::KahanAvg(count, summed, err) => {
                let Some(input) = number(row) else { continue };
                *count += 1;
                kahan_add(input, summed, err);
            }
        }
    }
    Ok(state)
}

/// The state a value of its layout holds; a NULL is the empty state.
fn read(kind: Holistic, value: &Value) -> Result<State> {
    if value.is_null() {
        return Ok(empty(kind));
    }
    Ok(match empty(kind) {
        State::Text(_) => State::Text(Some(value.try_to_string()?)),
        State::Bool(_) => match value.data() {
            ValueData::Bool(v) => State::Bool(Some(*v)),
            _ => return Err(failure()),
        },
        State::Bits(_) => State::Bits(Some(value.as_i128().ok_or_else(failure)?)),
        State::Product(_) => State::Product(Some(double(value)?)),
        State::Dev(_) => State::Dev(Dev::read(value)?),
        State::Covar(_) => State::Covar(Covar::read(value)?),
        State::Corr(..) => {
            let (covar, dev_x, dev_y) = corr_read(value)?;
            State::Corr(covar, dev_x, dev_y)
        }
        State::Moments(_, fourth) => State::Moments(Moments::read(value)?, fourth),
        State::RegrCount(_) => State::RegrCount(unsigned(value)?),
        // `ApproxQuantileImportState`
        State::Digest(..) => {
            let invalid = |message: &str| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!("Invalid approx_quantile state - {message}"),
                )
            };
            let [count, min, max, centroids] = fields(value)? else {
                return Err(failure());
            };
            let ValueData::List(centroids) = centroids.data() else {
                return Err(invalid("the state fields cannot be NULL"));
            };
            if count.is_null() || min.is_null() || max.is_null() {
                return Err(invalid("the state fields cannot be NULL"));
            }
            let count = unsigned(count)?;
            if count != 0 && centroids.is_empty() {
                return Err(invalid("non-zero count requires at least one centroid"));
            }
            let mut parsed: Vec<super::tdigest::Centroid> = Vec::with_capacity(centroids.len());
            let mut total = 0.0;
            for centroid in centroids {
                let ValueData::Struct(parts) = centroid.data() else {
                    return Err(invalid("the centroids cannot be NULL"));
                };
                let [mean, weight] = parts.as_slice() else {
                    return Err(failure());
                };
                if mean.is_null() || weight.is_null() {
                    return Err(invalid("the centroids cannot be NULL"));
                }
                let (mean, weight) = (double(mean)?, double(weight)?);
                if mean.is_nan() || !weight.is_finite() || weight <= 0.0 {
                    return Err(invalid("centroids must have a mean and a positive finite weight"));
                }
                if parsed.last().is_some_and(|last| mean < last.mean()) {
                    return Err(invalid("centroids must be sorted by their mean"));
                }
                total += weight;
                parsed.push(super::tdigest::Centroid::new(mean, weight));
            }
            if !total.is_finite() {
                return Err(invalid("the total centroid weight must be finite"));
            }
            let (min, max) = (double(min)?, double(max)?);
            if min.is_nan() || max.is_nan() {
                return Err(invalid("min and max cannot be NaN"));
            }
            State::Digest(
                count,
                super::tdigest::TDigest::from_parts(DIGEST_COMPRESSION, min, max, parsed),
                None,
            )
        }
        // `ApproxCountDistinctImportState`
        State::Sketch(_) => {
            let invalid = |message: String| Error::new(ErrorKind::InvalidInput, message);
            let [precision, registers] = fields(value)? else {
                return Err(failure());
            };
            let ValueData::List(registers) = registers.data() else {
                return Err(invalid(
                    "Invalid approx_count_distinct state - the state fields cannot be NULL".into(),
                ));
            };
            if precision.is_null() {
                return Err(invalid(
                    "Invalid approx_count_distinct state - the state fields cannot be NULL".into(),
                ));
            }
            let precision = precision.as_i128().unwrap_or(0);
            if precision != super::hyperloglog::PRECISION as i128 {
                return Err(invalid(format!(
                    "Invalid approx_count_distinct state - expected precision {}, got {precision}",
                    super::hyperloglog::PRECISION
                )));
            }
            if registers.len() != super::hyperloglog::M {
                return Err(invalid(format!(
                    "Invalid approx_count_distinct state - expected {} registers, got {}",
                    super::hyperloglog::M,
                    registers.len()
                )));
            }
            let mut sketch = super::hyperloglog::HyperLogLog::default();
            for (at, register) in registers.iter().enumerate() {
                let Some(value) = register.as_i128() else {
                    return Err(invalid(
                        "Invalid approx_count_distinct state - the registers cannot be NULL".into(),
                    ));
                };
                let limit = super::hyperloglog::HyperLogLog::MAX_REGISTER;
                if value > i128::from(limit) {
                    return Err(invalid(format!(
                        "Invalid approx_count_distinct state - register value {value} exceeds {limit}"
                    )));
                }
                sketch.registers[at] = value as u8;
            }
            State::Sketch(sketch)
        }
        State::Buffer(_) => match value.data() {
            ValueData::List(items) => {
                // `reject_null_elements`: the finalize assumes every buffered value is valid
                if items.iter().any(Value::is_null) {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "Invalid aggregate state - the list cannot contain NULL values",
                    ));
                }
                State::Buffer(items.clone())
            }
            _ => return Err(failure()),
        },
        State::AvgDiscrete(..) => {
            let [count, sum] = fields(value)? else {
                return Err(failure());
            };
            State::AvgDiscrete(unsigned(count)?, sum.as_i128().unwrap_or(0))
        }
        State::AvgInterval(..) => {
            let [count, sum] = fields(value)? else {
                return Err(failure());
            };
            let interval = match sum.data() {
                ValueData::Interval(i) => *i,
                ValueData::Null => duckrs_common::types::Interval::default(),
                _ => return Err(failure()),
            };
            State::AvgInterval(count.as_i128().unwrap_or(0) as i64, interval)
        }
        State::ArgExtreme(_) => {
            let [arg, by] = fields(value)? else {
                return Err(failure());
            };
            State::ArgExtreme(Some((arg.clone(), by.clone())))
        }
        State::RegrAvg(..) => {
            let [sum, count] = fields(value)? else {
                return Err(failure());
            };
            State::RegrAvg(double(sum)?, unsigned(count)?)
        }
        State::RegrS(..) => {
            let [count, dev] = fields(value)? else {
                return Err(failure());
            };
            State::RegrS(unsigned(count)?, Dev::read(dev)?)
        }
        State::RegrSxy(..) => {
            let [count, covar] = fields(value)? else {
                return Err(failure());
            };
            State::RegrSxy(unsigned(count)?, Covar::read(covar)?)
        }
        State::RegrSlope(..) => {
            let (covar, var) = slope_read(value)?;
            State::RegrSlope(covar, var)
        }
        State::RegrIntercept(..) => {
            let [count, sum_x, sum_y, slope] = fields(value)? else {
                return Err(failure());
            };
            let (covar, var) = slope_read(slope)?;
            State::RegrIntercept(unsigned(count)?, double(sum_x)?, double(sum_y)?, covar, var)
        }
        State::RegrR2(..) => {
            let [corr, var_x, var_y] = fields(value)? else {
                return Err(failure());
            };
            let (covar, dev_x, dev_y) = corr_read(corr)?;
            State::RegrR2(covar, dev_x, dev_y, Dev::read(var_x)?, Dev::read(var_y)?)
        }
        State::Kahan(_) => {
            let [summed, err] = fields(value)? else {
                return Err(failure());
            };
            State::Kahan(Some((double(summed)?, double(err)?)))
        }
        State::KahanAvg(..) => {
            let [count, summed, err] = fields(value)? else {
                return Err(failure());
            };
            State::KahanAvg(unsigned(count)?, double(summed)?, double(err)?)
        }
    })
}

/// The value of the layout `ty` a state is; an unset optional state is NULL.
fn write(state: &State, ty: &LogicalType) -> Result<Value> {
    let value = match state {
        State::ArgExtreme(Some((arg, by))) => {
            Value::struct_value(vec![("arg".into(), arg.clone()), ("by".into(), by.clone())])
        }
        State::Bool(None)
        | State::Bits(None)
        | State::Product(None)
        | State::Kahan(None)
        | State::Text(None)
        | State::ArgExtreme(None) => {
            return Ok(Value::null(ty.clone()));
        }
        State::Bits(Some(v)) => {
            return crate::function::cast::cast_value(&Value::hugeint(*v), &ty.state_layout());
        }
        State::Text(Some(text)) => Value::varchar(text.clone()),
        State::Bool(Some(v)) => Value::boolean(*v),
        State::Product(Some(v)) => Value::double(*v),
        State::Dev(dev) => dev.value(),
        State::Covar(covar) => covar.value(),
        State::Corr(covar, dev_x, dev_y) => corr_value(covar, dev_x, dev_y),
        State::Moments(moments, fourth) => moments.value(*fourth),
        State::RegrCount(count) => Value::ubigint(*count),
        // `ApproxQuantileExportState`: no values is NULL; the unprocessed values are folded in first
        State::Digest(0, ..) => return Ok(Value::null(ty.clone())),
        State::Digest(count, digest, _) => {
            let mut digest = digest.clone();
            digest.compress();
            let centroid = struct_of(&[("mean", LogicalType::DOUBLE), ("weight", LogicalType::DOUBLE)]);
            let centroids = digest
                .processed()
                .iter()
                .map(|c| {
                    Value::new(
                        centroid.clone(),
                        ValueData::Struct(vec![Value::double(c.mean()), Value::double(c.weight())]),
                    )
                })
                .collect::<Result<Vec<_>>>()?;
            Value::struct_value(vec![
                ("count".into(), Value::ubigint(*count)),
                ("min".into(), Value::double(digest.min())),
                ("max".into(), Value::double(digest.max())),
                (
                    "centroids".into(),
                    Value::new(LogicalType::list(centroid), ValueData::List(centroids))?,
                ),
            ])
        }
        // an empty sketch is exported as it is: approx_count_distinct of no values is 0, not NULL
        State::Sketch(sketch) => Value::struct_value(vec![
            (
                "precision".into(),
                Value::new(
                    LogicalType::UTINYINT,
                    ValueData::UInt8(super::hyperloglog::PRECISION as u8),
                )?,
            ),
            (
                "registers".into(),
                Value::new(
                    LogicalType::list(LogicalType::UTINYINT),
                    ValueData::List(
                        sketch
                            .registers
                            .iter()
                            .map(|r| Value::new(LogicalType::UTINYINT, ValueData::UInt8(*r)))
                            .collect::<Result<Vec<_>>>()?,
                    ),
                )?,
            ),
        ]),
        State::Buffer(values) if values.is_empty() => return Ok(Value::null(ty.clone())),
        State::Buffer(values) => {
            let element = ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
            Value::new(LogicalType::list(element), ValueData::List(values.clone()))?
        }
        State::AvgDiscrete(count, sum) => Value::struct_value(vec![
            ("count".into(), Value::ubigint(*count)),
            ("value".into(), Value::hugeint(*sum)),
        ]),
        State::AvgInterval(count, sum) => Value::struct_value(vec![
            ("count".into(), Value::bigint(*count)),
            ("value".into(), Value::interval(*sum)),
        ]),
        State::RegrAvg(sum, count) => Value::struct_value(vec![
            ("sum".into(), Value::double(*sum)),
            ("count".into(), Value::ubigint(*count)),
        ]),
        State::RegrS(count, dev) => Value::struct_value(vec![
            ("count".into(), Value::ubigint(*count)),
            ("var_pop".into(), dev.value()),
        ]),
        State::RegrSxy(count, covar) => Value::struct_value(vec![
            ("count".into(), Value::ubigint(*count)),
            ("cov_pop".into(), covar.value()),
        ]),
        State::RegrSlope(covar, var) => slope_value(covar, var),
        State::RegrIntercept(count, sum_x, sum_y, covar, var) => Value::struct_value(vec![
            ("count".into(), Value::ubigint(*count)),
            ("sum_x".into(), Value::double(*sum_x)),
            ("sum_y".into(), Value::double(*sum_y)),
            ("slope".into(), slope_value(covar, var)),
        ]),
        State::RegrR2(covar, dev_x, dev_y, var_x, var_y) => Value::struct_value(vec![
            ("corr".into(), corr_value(covar, dev_x, dev_y)),
            ("var_pop_x".into(), var_x.value()),
            ("var_pop_y".into(), var_y.value()),
        ]),
        State::Kahan(Some((summed, err))) => Value::struct_value(vec![
            ("value".into(), Value::double(*summed)),
            ("err".into(), Value::double(*err)),
        ]),
        State::KahanAvg(count, summed, err) => Value::struct_value(vec![
            ("count".into(), Value::ubigint(*count)),
            ("value".into(), Value::double(*summed)),
            ("err".into(), Value::double(*err)),
        ]),
    };
    Value::new(ty.clone(), value.data().clone())
}

fn add_intervals(
    left: duckrs_common::types::Interval,
    right: duckrs_common::types::Interval,
) -> Result<duckrs_common::types::Interval> {
    let overflow = || Error::new(ErrorKind::OutOfRange, "Overflow in interval addition");
    Ok(duckrs_common::types::Interval::new(
        left.months.checked_add(right.months).ok_or_else(overflow)?,
        left.days.checked_add(right.days).ok_or_else(overflow)?,
        left.micros.checked_add(right.micros).ok_or_else(overflow)?,
    ))
}

/// `IntervalAverageOperation::Finalize`: the remainders shifted to the smaller units, as Postgres does.
fn average_interval(value: duckrs_common::types::Interval, count: i64) -> duckrs_common::types::Interval {
    let mut target = duckrs_common::types::Interval::new(
        (i64::from(value.months) / count) as i32,
        (i64::from(value.days) / count) as i32,
        value.micros / count,
    );
    let months_remainder = (i64::from(value.months) % count) * 30;
    let mut days_remainder = i64::from(value.days) % count;
    let mut micros_remainder = value.micros % count;
    target.days += (months_remainder / count) as i32;
    days_remainder += months_remainder % count;
    let shifted = i128::from(days_remainder) * i128::from(duckrs_common::types::Interval::MICROS_PER_DAY);
    micros_remainder += (shifted / i128::from(count)) as i64;
    target.micros += micros_remainder;
    target
}

/// `validate_state` of the layouts that have one: what an imported state cannot hold.
pub fn validate(kind: Holistic, arguments: &[LogicalType], value: &Value) -> Result<()> {
    if value.is_null() {
        return Ok(());
    }
    let State::AvgDiscrete(count, sum) = read(kind, value)? else {
        return Ok(());
    };
    let time = matches!(
        arguments.first().map(LogicalType::id),
        Some(LogicalTypeId::Time | LogicalTypeId::TimeTz)
    );
    if time {
        // `ValidateTimeAverageStates`
        if count != 0
            && (sum < 0 || sum > i128::from(count) * i128::from(duckrs_common::types::Interval::MICROS_PER_DAY))
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("Invalid avg state - the average of {sum} over {count} values is not a valid time"),
            ));
        }
    } else if count == INFINITE_COUNT && !(POSITIVE_INFINITY..=POSITIVE_INFINITY | NEGATIVE_INFINITY).contains(&sum) {
        // `ValidateTimestampAverageStates`
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "Invalid avg state - with a count of {count} the value must be 1 (infinity), 2 (-infinity) or 3 \
                 (both), not {sum}"
            ),
        ));
    }
    Ok(())
}

/// The compression `ApproxQuantileOperation` builds its digests with.
const DIGEST_COMPRESSION: f64 = 100.0;

/// `ApproxQuantileCoding::Encode`: a value as the double the digest keeps; `None` for a NULL.
fn encode_quantile_input(value: &Value) -> Option<f64> {
    use LogicalTypeId as L;
    if value.is_null() {
        return None;
    }
    match (value.logical_type().id(), value.data()) {
        (L::Float, ValueData::Float(v)) => Some(f64::from(*v)),
        (L::Double, ValueData::Double(v)) => Some(*v),
        // the sort key of a TIMETZ, which orders the instants
        (L::TimeTz, ValueData::Int64(bits)) => Some(duckrs_common::types::TimeTz(*bits as u64).sort_key() as f64),
        _ => value.as_i128().map(|v| v as f64),
    }
}

/// `ApproxQuantileCoding::Decode`: the double the digest gives, as a value of `ty`, clamped to what the type holds
/// since the result is approximate.
fn decode_quantile_output(source: f64, ty: &LogicalType) -> Result<Value> {
    use LogicalTypeId as L;
    // `TryCast` of a double to an integer rounds to the nearest, ties to even
    let integer = |min: i128, max: i128| -> i128 {
        let rounded = source.round_ties_even();
        // a cast of a double to i128 saturates, so the bounds are compared exactly
        if rounded.is_nan() {
            0
        } else {
            (rounded as i128).clamp(min, max)
        }
    };
    let physical = |ty: &LogicalType| match ty.physical_type() {
        duckrs_common::types::PhysicalType::Int8 => (i128::from(i8::MIN), i128::from(i8::MAX)),
        duckrs_common::types::PhysicalType::Int16 => (i128::from(i16::MIN), i128::from(i16::MAX)),
        duckrs_common::types::PhysicalType::Int32 => (i128::from(i32::MIN), i128::from(i32::MAX)),
        duckrs_common::types::PhysicalType::Int64 => (i128::from(i64::MIN), i128::from(i64::MAX)),
        _ => (i128::MIN, i128::MAX),
    };
    Ok(match ty.id() {
        L::Double => Value::double(source),
        L::Float => Value::float(source as f32),
        L::Time => {
            let micros = integer(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
            Value::new(ty.clone(), ValueData::Int64(micros.clamp(0, Interval_MICROS_PER_DAY)))?
        }
        L::TimeTz => {
            use duckrs_common::types::{Time, TimeTz};
            const MAX_OFFSET: i64 = 16 * 60 * 60 - 1;
            let sort_key = integer(0, i128::from(u64::MAX)) as u64;
            let micros = (sort_key >> 24) as i64 - MAX_OFFSET * 1_000_000;
            let micros = micros.rem_euclid(Interval_MICROS_PER_DAY);
            Value::time_tz(TimeTz::new(Time(micros), 0))
        }
        // the infinities are the largest and smallest timestamps
        L::Timestamp | L::TimestampTz | L::TimestampNs | L::TimestampTzNs | L::TimestampSec | L::TimestampMs => {
            let value = integer(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
            Value::new(ty.clone(), ValueData::Int64(value.clamp(-i64::MAX, i64::MAX)))?
        }
        L::Decimal => {
            let (min, max) = physical(ty);
            let mut value = integer(min, max);
            let width = ty.decimal_properties().map_or(18, |(w, _)| w);
            let limit = 10i128.pow(u32::from(width)) - 1;
            value = value.clamp(-limit, limit);
            let data = match ty.physical_type() {
                duckrs_common::types::PhysicalType::Int16 => ValueData::Int16(value as i16),
                duckrs_common::types::PhysicalType::Int32 => ValueData::Int32(value as i32),
                duckrs_common::types::PhysicalType::Int64 => ValueData::Int64(value as i64),
                _ => ValueData::Int128(value),
            };
            Value::new(ty.clone(), data)?
        }
        _ => {
            let (min, max) = physical(ty);
            let value = integer(min, max);
            let data = match ty.physical_type() {
                duckrs_common::types::PhysicalType::Int8 => ValueData::Int8(value as i8),
                duckrs_common::types::PhysicalType::Int16 => ValueData::Int16(value as i16),
                duckrs_common::types::PhysicalType::Int32 => ValueData::Int32(value as i32),
                duckrs_common::types::PhysicalType::Int64 => ValueData::Int64(value as i64),
                _ => ValueData::Int128(value),
            };
            Value::new(ty.clone(), data)?
        }
    })
}

#[allow(non_upper_case_globals)]
const Interval_MICROS_PER_DAY: i64 = duckrs_common::types::Interval::MICROS_PER_DAY;

/// The state of `rows`, as a value of the layout `ty`.
pub fn from_rows(kind: Holistic, rows: &[Value], ty: &LogicalType) -> Result<Value> {
    write(&fold(kind, rows)?, ty)
}

/// `Combine`: the state of two states of the layout `ty`, `right` combined into `left`. `constants` are what the
/// arguments were bound to (string_agg's separator).
pub fn combine(
    kind: Holistic,
    left: &Value,
    right: &Value,
    ty: &LogicalType,
    constants: &[Option<Value>],
) -> Result<Value> {
    use Holistic::*;
    let (mut target, source) = (read(kind, left)?, read(kind, right)?);
    match (&mut target, source) {
        (State::Text(target), State::Text(source)) => {
            if let Some(source) = source {
                let separator = match constants.get(1) {
                    Some(Some(separator)) => separator.try_to_string()?,
                    Some(None) => String::new(),
                    None => ",".to_string(),
                };
                append(target, &source, &separator);
            }
        }
        (State::Bits(target), State::Bits(source)) => {
            if let Some(source) = source {
                *target = Some(match *target {
                    None => source,
                    Some(current) => bits(kind, current, source),
                });
            }
        }
        (State::Bool(target), State::Bool(source)) => {
            if let Some(source) = source {
                let current = target.unwrap_or(kind == BoolAnd);
                *target = Some(if kind == BoolAnd {
                    current && source
                } else {
                    current || source
                });
            }
        }
        (State::Product(target), State::Product(source)) => {
            if let Some(source) = source {
                *target = Some(target.unwrap_or(1.0) * source);
            }
        }
        (State::Dev(target), State::Dev(source)) => target.combine(&source)?,
        (State::Covar(target), State::Covar(source)) => target.combine(&source)?,
        (State::Corr(covar, dev_x, dev_y), State::Corr(c, x, y)) => {
            covar.combine(&c)?;
            dev_x.combine(&x)?;
            dev_y.combine(&y)?;
        }
        (State::Moments(target, _), State::Moments(source, _)) => target.combine(&source)?,
        // `ArgMinMaxBase::Combine`
        (State::ArgExtreme(target), State::ArgExtreme(source)) => {
            if let Some((arg, by)) = source {
                let replace = match target {
                    None => true,
                    Some((_, current)) => current.is_null() || (!by.is_null() && better(kind, &by, current)?),
                };
                if replace {
                    *target = Some((arg, by));
                }
            }
        }
        (State::AvgDiscrete(count, sum), State::AvgDiscrete(c, v)) => {
            if kind == AvgTimestamp && c == INFINITE_COUNT {
                add_infinities(count, sum, v);
            } else if c != 0 && *count != INFINITE_COUNT {
                if *count == 0 {
                    *sum = 0;
                }
                *count = count
                    .checked_add(c)
                    .filter(|n| *n != INFINITE_COUNT)
                    .ok_or_else(overflow)?;
                *sum = sum.checked_add(v).ok_or_else(overflow)?;
            }
        }
        (State::AvgInterval(count, sum), State::AvgInterval(c, v)) => {
            *count += c;
            *sum = add_intervals(*sum, v)?;
        }
        (State::Buffer(target), State::Buffer(source)) => target.extend(source),
        (State::Sketch(target), State::Sketch(source)) => target.merge(&source),
        // `ApproxQuantileOperation::Combine`
        (State::Digest(count, digest, _), State::Digest(source_count, source, _)) => {
            if source_count != 0 {
                *count = count.checked_add(source_count).ok_or_else(overflow)?;
                digest.merge(&source);
            }
        }
        (State::RegrCount(target), State::RegrCount(source)) => {
            *target = target.checked_add(source).ok_or_else(overflow)?;
        }
        (State::RegrAvg(sum, count), State::RegrAvg(s, c)) => {
            *sum += s;
            *count += c;
        }
        (State::RegrS(count, dev), State::RegrS(c, d)) => {
            *count = count.checked_add(c).ok_or_else(overflow)?;
            dev.combine(&d)?;
        }
        (State::RegrSxy(count, covar), State::RegrSxy(c, v)) => {
            covar.combine(&v)?;
            *count = count.checked_add(c).ok_or_else(overflow)?;
        }
        (State::RegrSlope(covar, var), State::RegrSlope(c, v)) => {
            covar.combine(&c)?;
            var.combine(&v)?;
        }
        (State::RegrIntercept(count, sum_x, sum_y, covar, var), State::RegrIntercept(n, sx, sy, c, v)) => {
            *count += n;
            *sum_x += sx;
            *sum_y += sy;
            covar.combine(&c)?;
            var.combine(&v)?;
        }
        (State::RegrR2(covar, dev_x, dev_y, var_x, var_y), State::RegrR2(c, dx, dy, vx, vy)) => {
            covar.combine(&c)?;
            dev_x.combine(&dx)?;
            dev_y.combine(&dy)?;
            var_x.combine(&vx)?;
            var_y.combine(&vy)?;
        }
        (State::Kahan(target), State::Kahan(source)) => {
            if let Some((value, err)) = source {
                let (summed, target_err) = target.get_or_insert((0.0, 0.0));
                kahan_add(value, summed, target_err);
                *target_err += err;
            }
        }
        (State::KahanAvg(count, summed, target_err), State::KahanAvg(c, value, err)) => {
            *count = count.checked_add(c).ok_or_else(overflow)?;
            kahan_add(value, summed, target_err);
            *target_err += err;
        }
        _ => return Err(failure()),
    }
    write(&target, ty)
}

/// `Finalize`: the result of the aggregate from a state of its layout; `constants` are what its arguments were bound
/// to (a quantile's fraction).
pub fn finalize(kind: Holistic, value: &Value, result: &LogicalType, constants: &[Option<Value>]) -> Result<Value> {
    use Holistic::*;
    let null = || Value::null(result.clone());
    Ok(match read(kind, value)? {
        State::Sketch(sketch) => Value::bigint(sketch.count()),
        // `ApproxQuantileScalarOperation::Finalize` and `ApproxQuantileListOperation::Finalize`
        State::Digest(0, ..) => null(),
        State::Digest(_, mut digest, fractions) => {
            let fractions = constants.get(1).cloned().flatten().or(fractions).ok_or_else(failure)?;
            digest.compress();
            // the fractions are FLOATs, as the bind data keeps them
            let mut at = |q: &Value, ty: &LogicalType| -> Result<Value> {
                let q = super::holistic::number_of(q).ok_or_else(failure)? as f32;
                decode_quantile_output(digest.quantile(f64::from(q)), ty)
            };
            match (result.id(), fractions.data()) {
                (LogicalTypeId::List, ValueData::List(items)) => {
                    let element = result.child_type().cloned().unwrap_or(LogicalType::DOUBLE);
                    let values = items.iter().map(|q| at(q, &element)).collect::<Result<Vec<_>>>()?;
                    Value::new(result.clone(), ValueData::List(values))?
                }
                _ => at(&fractions, result)?,
            }
        }
        State::Buffer(values) => {
            // the rows the quantile would have seen: the values, with the fraction it was bound to
            let rows: Vec<Value> = match constants.get(1).cloned().flatten() {
                Some(fraction) => values
                    .into_iter()
                    .map(|v| Value::struct_value(vec![("a0".into(), v), ("a1".into(), fraction.clone())]))
                    .collect(),
                None => values,
            };
            super::holistic::finish(kind, &rows, result)?
        }
        State::Bool(v) => v.map_or_else(null, Value::boolean),
        State::Bits(v) => match v {
            Some(v) => crate::function::cast::cast_value(&Value::hugeint(v), result)?,
            None => null(),
        },
        State::Text(v) => v.map_or_else(null, Value::varchar),
        State::Product(v) => v.map_or_else(null, Value::double),
        State::Dev(dev) => {
            let n = dev.count as f64;
            let value = match kind {
                VarSamp if dev.count > 1 => dev.dsquared / (n - 1.0),
                StddevSamp if dev.count > 1 => (dev.dsquared / (n - 1.0)).sqrt(),
                VarPop if dev.count > 0 => {
                    if dev.count > 1 {
                        dev.dsquared / n
                    } else {
                        0.0
                    }
                }
                StddevPop if dev.count > 0 => dev.population(),
                Sem if dev.count > 0 => (dev.dsquared / n).sqrt() / n.sqrt(),
                _ => return Ok(null()),
            };
            Value::double(value)
        }
        State::Covar(covar) => match kind {
            CovarPop if covar.count > 0 => Value::double(covar.co_moment / covar.count as f64),
            CovarSamp if covar.count > 1 => Value::double(covar.co_moment / (covar.count - 1) as f64),
            _ => null(),
        },
        State::Corr(covar, dev_x, dev_y) => corr_finalize(&covar, &dev_x, &dev_y).map_or_else(null, Value::double),
        State::ArgExtreme(state) => match state {
            Some((arg, _)) if !arg.is_null() => {
                if arg.logical_type() == result {
                    arg
                } else {
                    crate::execution::parquet::cast_value(&arg, result)?
                }
            }
            _ => null(),
        },
        State::AvgDiscrete(0, _) => null(),
        State::AvgDiscrete(INFINITE_COUNT, flags) if kind == AvgTimestamp => match flags {
            POSITIVE_INFINITY => Value::new(result.clone(), ValueData::Int64(i64::MAX))?,
            NEGATIVE_INFINITY => Value::new(result.clone(), ValueData::Int64(-i64::MAX))?,
            _ => null(),
        },
        State::AvgDiscrete(count, sum) => {
            // `DiscreteAverageOperation::Finalize`: rounded
            let count = i128::from(count);
            let (quotient, remainder) = (sum.div_euclid(count), sum.rem_euclid(count));
            let average = quotient + i128::from(remainder > count / 2);
            let average = i64::try_from(average).map_err(|_| overflow())?;
            if kind == AvgTimeTz {
                use duckrs_common::types::{Time, TimeTz};
                Value::time_tz(TimeTz::new(Time(average), 0))
            } else {
                Value::new(result.clone(), ValueData::Int64(average))?
            }
        }
        State::AvgInterval(0, _) => null(),
        State::AvgInterval(count, sum) => Value::interval(average_interval(sum, count)),
        State::RegrCount(count) => {
            // `RegrCountFunction::Finalize`: into the UINTEGER the aggregate gives
            let count = u32::try_from(count).map_err(|_| {
                Error::new(
                    ErrorKind::OutOfRange,
                    format!("regr_count result {count} does not fit in the result type"),
                )
            })?;
            Value::new(result.clone(), ValueData::UInt32(count))?
        }
        State::RegrAvg(_, 0) => null(),
        State::RegrAvg(sum, count) => Value::double(sum / count as f64),
        State::RegrS(_, dev) if dev.count == 0 => null(),
        State::RegrS(count, dev) => {
            u32::try_from(count).map_err(|_| {
                Error::new(
                    ErrorKind::OutOfRange,
                    format!("regr_count result {count} does not fit in the result type"),
                )
            })?;
            Value::double(count as f64 * dev.variance())
        }
        State::RegrSxy(_, covar) if covar.count == 0 => null(),
        State::RegrSxy(count, covar) => Value::double(count as f64 * (covar.co_moment / covar.count as f64)),
        State::RegrSlope(covar, var) => slope_finalize(&covar, &var).map_or_else(null, Value::double),
        State::RegrIntercept(0, ..) => null(),
        State::RegrIntercept(count, sum_x, sum_y, covar, var) => match slope_finalize(&covar, &var) {
            None => null(),
            Some(slope) if slope.is_nan() => null(),
            Some(slope) => Value::double(sum_y / count as f64 - slope * (sum_x / count as f64)),
        },
        State::RegrR2(covar, dev_x, dev_y, var_x, var_y) => {
            if var_x.variance() == 0.0 {
                null()
            } else if var_y.variance() == 0.0 {
                Value::double(1.0)
            } else {
                corr_finalize(&covar, &dev_x, &dev_y).map_or_else(null, |r| Value::double(r.powi(2)))
            }
        }
        State::Moments(m, false) => {
            // `SkewnessOperation::Finalize`
            if m.n <= 2 || m.m2 <= 0.0 {
                return Ok(null());
            }
            let n = m.n as f64;
            let g1 = n.sqrt() * (m.m3 / m.m2) / m.m2.sqrt();
            let target = (n * (n - 1.0)).sqrt() / (n - 2.0) * g1;
            if !target.is_finite() {
                return Err(Error::new(ErrorKind::OutOfRange, "SKEW is out of range!"));
            }
            Value::double(target)
        }
        State::Moments(m, true) => {
            // `KurtosisOperation::Finalize`
            let bias_corrected = kind == Kurtosis;
            if m.n <= 1 || (bias_corrected && m.n <= 3) || m.m2 <= 0.0 {
                return Ok(null());
            }
            let n = m.n as f64;
            let ratio = n * (m.m4 / m.m2) / m.m2;
            let target = if bias_corrected {
                (n - 1.0) * ((n + 1.0) * ratio - 3.0 * (n - 1.0)) / ((n - 2.0) * (n - 3.0))
            } else {
                ratio - 3.0
            };
            if !target.is_finite() {
                return Err(Error::new(ErrorKind::OutOfRange, "Kurtosis is out of range!"));
            }
            Value::double(target)
        }
        State::Kahan(v) => v.map_or_else(null, |(summed, err)| Value::double(kahan_finalize(summed, err))),
        State::KahanAvg(0, ..) => null(),
        State::KahanAvg(count, summed, err) => Value::double(kahan_finalize(summed, err) / count as f64),
    })
}
