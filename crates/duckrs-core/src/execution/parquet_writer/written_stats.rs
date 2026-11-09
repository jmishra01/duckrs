//! `RETURN_STATS`: what a Parquet file holds, by column (`ParquetStatsAccumulator` and the
//! `ColumnStatsUnifier`s of `parquet_writer.cpp`).

use std::collections::BTreeMap;

use duckrs_common::types::{LogicalType, LogicalTypeId, PhysicalType, Time, TimeTz, Timestamp};
use duckrs_common::value::{Value, ValueData};

use super::{Convert, FooterStats, Leaf, LeafKind, Node, REPEATED, Stats};

/// What the file a COPY wrote holds (`CopyFunctionFileStatistics`).
#[derive(Debug, Clone, Default)]
pub struct WrittenFileStats {
    pub row_count: u64,
    pub file_size_bytes: u64,
    pub footer_size_bytes: u64,
    /// by quoted column path, by statistic, sorted
    pub column_statistics: BTreeMap<String, BTreeMap<String, Value>>,
    pub row_group_count: u64,
}

/// How a column's statistics compare and print (`GetBaseStatsUnifier`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnifyKind {
    Signed,
    Unsigned,
    Float,
    Double,
    String,
    Blob,
    Uuid,
    TimeTz,
    /// `DecimalStatsUnifier<hugeint_t>`: big-endian bytes
    FixedDecimal,
    /// `TIMESTAMP_S`, which the file holds in microseconds
    TimestampSec,
    /// `NullStatsUnifier`
    Null,
}

fn unify_kind(leaf: &Leaf) -> UnifyKind {
    use LogicalTypeId as T;
    match &leaf.kind {
        LeafKind::Boolean => UnifyKind::Signed,
        LeafKind::Enum(_) => UnifyKind::Null,
        LeafKind::FixedDecimal => UnifyKind::FixedDecimal,
        LeafKind::Standard { convert, .. } => match convert {
            Convert::I32 | Convert::I64 => match leaf.schema.ty.id() {
                T::Interval | T::Bit => UnifyKind::Null,
                _ => UnifyKind::Signed,
            },
            Convert::U32 | Convert::U64 => UnifyKind::Unsigned,
            Convert::TimestampSec => UnifyKind::TimestampSec,
            Convert::TimeTz => UnifyKind::TimeTz,
            Convert::Double => UnifyKind::Double,
            Convert::F32 => UnifyKind::Float,
            Convert::F64 => UnifyKind::Double,
            Convert::Bytes | Convert::Text => match leaf.schema.ty.id() {
                T::Blob => UnifyKind::Blob,
                T::Varchar => UnifyKind::String,
                _ => UnifyKind::Null,
            },
            Convert::Uuid => UnifyKind::Uuid,
            Convert::Int96 { .. } | Convert::Interval => UnifyKind::Null,
        },
    }
}

/// `ColumnStatsUnifier`: the statistics of a leaf column over the row groups of the file.
#[derive(Debug, Clone)]
struct Unifier {
    kind: UnifyKind,
    ty: LogicalType,
    column_name: String,
    global_min: Vec<u8>,
    global_max: Vec<u8>,
    min_is_set: bool,
    max_is_set: bool,
    all_min_max_set: bool,
    min_is_exact: bool,
    max_is_exact: bool,
    null_count: u64,
    all_nulls_set: bool,
    can_have_nan: bool,
    has_nan: bool,
    nan_count: u64,
    column_size_bytes: u64,
    num_values: u64,
}

impl Unifier {
    fn new(leaf: &Leaf, column_name: String) -> Self {
        Self {
            kind: unify_kind(leaf),
            ty: leaf.schema.ty.clone(),
            column_name,
            global_min: Vec::new(),
            global_max: Vec::new(),
            min_is_set: false,
            max_is_set: false,
            all_min_max_set: true,
            min_is_exact: true,
            max_is_exact: true,
            null_count: 0,
            all_nulls_set: true,
            can_have_nan: false,
            has_nan: false,
            nan_count: 0,
            column_size_bytes: 0,
            num_values: 0,
        }
    }

    /// `LessThan` of two statistics of this column.
    fn less(&self, a: &[u8], b: &[u8]) -> bool {
        match self.kind {
            UnifyKind::Signed | UnifyKind::TimestampSec | UnifyKind::TimeTz => signed(a) < signed(b),
            UnifyKind::Unsigned => unsigned(a) < unsigned(b),
            UnifyKind::Float => f32_of(a) < f32_of(b),
            UnifyKind::Double => f64_of(a) < f64_of(b),
            UnifyKind::FixedDecimal => big_endian(a) < big_endian(b),
            UnifyKind::String | UnifyKind::Blob | UnifyKind::Uuid => a < b,
            UnifyKind::Null => false,
        }
    }

    /// `UnifyMinMax`.
    fn unify_min_max(&mut self, min: &[u8], max: &[u8]) {
        if self.kind == UnifyKind::Null {
            return;
        }
        if !self.min_is_set {
            self.global_min = min.to_vec();
            self.min_is_set = true;
        } else if self.less(min, &self.global_min) {
            self.global_min = min.to_vec();
        }
        if !self.max_is_set {
            self.global_max = max.to_vec();
            self.max_is_set = true;
        } else if self.less(&self.global_max, max) {
            self.global_max = max.to_vec();
        }
    }

    /// `StatsToString`; `None` where C++ gives the empty string.
    fn stats_to_string(&self, bytes: &[u8]) -> Option<String> {
        if bytes.is_empty() && !matches!(self.kind, UnifyKind::String | UnifyKind::Blob) {
            return None;
        }
        let shown = |value: Value| value.try_to_string().ok();
        match self.kind {
            UnifyKind::Null => None,
            UnifyKind::String => Some(String::from_utf8_lossy(bytes).into_owned()),
            UnifyKind::Blob => Some(bytes.iter().map(|b| format!("{b:02X}")).collect()),
            UnifyKind::Uuid => {
                if bytes.len() != 16 {
                    return None;
                }
                let mut result = String::new();
                for (i, byte) in bytes.iter().enumerate() {
                    if matches!(i, 4 | 6 | 8 | 10) {
                        result.push('-');
                    }
                    result += &format!("{byte:02x}");
                }
                Some(result)
            }
            UnifyKind::TimeTz => shown(Value::time_tz(TimeTz::new(Time(signed(bytes) as i64), 0))),
            UnifyKind::TimestampSec => shown(Value::timestamp(Timestamp(signed(bytes) as i64))),
            UnifyKind::Float => shown(Value::float(f32_of(bytes))),
            UnifyKind::Double => shown(Value::double(f64_of(bytes))),
            UnifyKind::FixedDecimal => self.integer_text(big_endian(bytes)),
            UnifyKind::Signed => self.integer_text(signed(bytes)),
            UnifyKind::Unsigned => self.integer_text(unsigned(bytes) as i128),
        }
    }

    /// An integer of the file as a value of the column's type.
    fn integer_text(&self, value: i128) -> Option<String> {
        if self.ty.id() == LogicalTypeId::Boolean {
            return Some(value.to_string());
        }
        if self.ty.id() == LogicalTypeId::Decimal {
            let (width, scale) = self.ty.decimal_properties()?;
            return Value::decimal(value, width, scale).ok()?.try_to_string().ok();
        }
        let data = match self.ty.physical_type() {
            PhysicalType::Int8 => ValueData::Int8(value as i8),
            PhysicalType::Int16 => ValueData::Int16(value as i16),
            PhysicalType::Int32 => ValueData::Int32(value as i32),
            PhysicalType::Int64 => ValueData::Int64(value as i64),
            PhysicalType::UInt8 => ValueData::UInt8(value as u8),
            PhysicalType::UInt16 => ValueData::UInt16(value as u16),
            PhysicalType::UInt32 => ValueData::UInt32(value as u32),
            PhysicalType::UInt64 => ValueData::UInt64(value as u64),
            _ => return Some(value.to_string()),
        };
        Value::new(self.ty.clone(), data).ok()?.try_to_string().ok()
    }
}

fn signed(bytes: &[u8]) -> i128 {
    match bytes.len() {
        1 => i128::from(bytes[0] as i8),
        4 => i128::from(i32::from_le_bytes(bytes.try_into().unwrap_or_default())),
        8 => i128::from(i64::from_le_bytes(bytes.try_into().unwrap_or_default())),
        _ => 0,
    }
}

fn unsigned(bytes: &[u8]) -> u64 {
    match bytes.len() {
        4 => u64::from(u32::from_le_bytes(bytes.try_into().unwrap_or_default())),
        8 => u64::from_le_bytes(bytes.try_into().unwrap_or_default()),
        _ => 0,
    }
}

fn f32_of(bytes: &[u8]) -> f32 {
    bytes.try_into().map(f32::from_le_bytes).unwrap_or(0.0)
}

fn f64_of(bytes: &[u8]) -> f64 {
    bytes.try_into().map(f64::from_le_bytes).unwrap_or(0.0)
}

/// Big-endian two's complement, as FLBA decimals are.
fn big_endian(bytes: &[u8]) -> i128 {
    if bytes.is_empty() || bytes.len() > 16 {
        return 0;
    }
    let mut value: i128 = if bytes[0] & 0x80 != 0 { -1 } else { 0 };
    for &byte in bytes {
        value = (value << 8) | i128::from(byte);
    }
    value
}

/// `SQLQuotedIdentifier`.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The NULLs and values of a STRUCT or LIST column.
#[derive(Debug, Clone, Default)]
struct NestedStats {
    /// `None` for the REPEATED groups that C++ has no statistics of (the `key_value` of a MAP)
    column_name: Option<String>,
    null_count: u64,
    num_values: u64,
}

/// `ParquetStatsAccumulator`.
#[derive(Debug, Clone, Default)]
pub(super) struct StatsAccumulator {
    unifiers: Vec<Unifier>,
    /// by the nested columns in pre-order
    nested: Vec<NestedStats>,
}

impl StatsAccumulator {
    /// `GetStatsUnifier` for every column.
    pub(super) fn new(nodes: &[Node]) -> Self {
        let mut result = Self::default();
        for node in nodes {
            result.add(node, String::new());
        }
        result
    }

    fn add(&mut self, node: &Node, mut base_name: String) {
        let schema = match node {
            Node::Leaf(leaf) => &leaf.schema,
            Node::Struct { schema, .. } | Node::List { schema, .. } | Node::Array { schema, .. } => schema,
        };
        let repeated = schema.repetition == REPEATED;
        if !repeated {
            if !base_name.is_empty() {
                base_name.push('.');
            }
            base_name += &quoted(&schema.name);
        }
        match node {
            Node::Leaf(leaf) => self.unifiers.push(Unifier::new(leaf, base_name)),
            Node::Struct { children, .. } => {
                self.nested.push(NestedStats {
                    column_name: (!repeated).then(|| base_name.clone()),
                    ..Default::default()
                });
                for child in children {
                    self.add(child, base_name.clone());
                }
            }
            Node::List { child, .. } | Node::Array { child, .. } => {
                self.nested.push(NestedStats {
                    column_name: (!repeated).then(|| base_name.clone()),
                    ..Default::default()
                });
                self.add(child, base_name);
            }
        }
    }

    /// `FlushNestedColumnStats`.
    pub(super) fn flush_nested(&mut self, index: usize, null_count: usize, num_values: usize) {
        if let Some(entry) = self.nested.get_mut(index) {
            entry.null_count += null_count as u64;
            entry.num_values += num_values as u64;
        }
    }

    /// `FlushColumnStats`: a column chunk of a row group.
    pub(super) fn flush_column(
        &mut self,
        column: usize,
        footer: Option<&FooterStats>,
        writer_stats: Option<&Stats>,
        num_values: i64,
        compressed_size: i64,
    ) {
        let Some(unifier) = self.unifiers.get_mut(column) else {
            return;
        };
        let mut has_nan = false;
        if let Some(stats) = writer_stats {
            unifier.can_have_nan = matches!(stats, Stats::Float { .. });
            has_nan = stats.nan_count() > 0;
            unifier.has_nan |= has_nan;
            unifier.nan_count += stats.nan_count();
        }
        let Some(footer) = footer else {
            return;
        };
        match writer_stats {
            Some(stats) if has_nan && stats.has_stats() => {
                let (min_exact, max_exact) = stats.exact();
                unifier.unify_min_max(&stats.min_value(), &stats.max_value());
                unifier.min_is_exact &= min_exact;
                unifier.max_is_exact &= max_exact;
            }
            _ => {
                if let (Some(min), Some(max)) = (&footer.min_value, &footer.max_value) {
                    unifier.unify_min_max(min, max);
                    unifier.min_is_exact &= footer.is_min_value_exact == Some(true);
                    unifier.max_is_exact &= footer.is_max_value_exact == Some(true);
                } else if footer.null_count == Some(num_values) {
                    // a row group of only NULLs has no bounds to add
                } else {
                    unifier.all_min_max_set = false;
                }
            }
        }
        match footer.null_count {
            Some(nulls) => unifier.null_count += nulls as u64,
            None => unifier.all_nulls_set = false,
        }
        unifier.column_size_bytes += compressed_size as u64;
        unifier.num_values += num_values as u64;
    }

    /// `GatherWrittenStatistics`.
    pub(super) fn gather(&self) -> BTreeMap<String, BTreeMap<String, Value>> {
        let ubigint = |v: u64| Value::ubigint(v);
        let mut result = BTreeMap::new();
        for nested in &self.nested {
            let Some(name) = &nested.column_name else {
                continue;
            };
            let mut stats = BTreeMap::new();
            stats.insert("null_count".to_string(), ubigint(nested.null_count));
            stats.insert("num_values".to_string(), ubigint(nested.num_values));
            result.insert(name.clone(), stats);
        }
        for unifier in &self.unifiers {
            let mut stats = BTreeMap::new();
            stats.insert("column_size_bytes".to_string(), ubigint(unifier.column_size_bytes));
            stats.insert("num_values".to_string(), ubigint(unifier.num_values));
            if unifier.all_min_max_set {
                if unifier.min_is_set {
                    let text = unifier.stats_to_string(&unifier.global_min).unwrap_or_default();
                    stats.insert("min".to_string(), Value::varchar(text));
                    stats.insert("min_is_exact".to_string(), Value::boolean(unifier.min_is_exact));
                }
                if unifier.max_is_set {
                    let text = unifier.stats_to_string(&unifier.global_max).unwrap_or_default();
                    stats.insert("max".to_string(), Value::varchar(text));
                    stats.insert("max_is_exact".to_string(), Value::boolean(unifier.max_is_exact));
                }
            }
            if unifier.all_nulls_set {
                stats.insert("null_count".to_string(), ubigint(unifier.null_count));
            }
            if unifier.can_have_nan {
                stats.insert("has_nan".to_string(), Value::boolean(unifier.has_nan));
                stats.insert("nan_count".to_string(), ubigint(unifier.nan_count));
            }
            result.insert(unifier.column_name.clone(), stats);
        }
        result
    }
}
