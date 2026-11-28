// Ported from duckdb/src/storage/table/row_group.cpp (CheckZonemapSegments) and
// duckdb/src/planner/filter/expression_filter.cpp (CheckStatistics) @ rust-port-baseline
//
//! Zone maps: the smallest and largest value of a column per row group, which tell a scan with
//! filters which groups it need not read. A table in a database file has them in its metadata.
//! Rows in memory change and only the catalog knows when, so for those they are worked out per
//! column when a filter first needs them, and thrown away by `TableCatalogEntry::write_storage`.
//!
//! A group is skipped only if no row of it can pass, so anything not understood keeps it. C++ also
//! prunes through many functions with statistics of their own (`contains`, `substr`, ...); here it
//! is comparisons, `BETWEEN`, NULL checks, AND, OR and functions that keep the order of values.

use super::physical::group_keys;
use crate::catalog::{ColumnZone, TableCatalogEntry, TableCursor};
use crate::execution::expression_executor::try_evaluate_scalar;
use crate::optimizer::statistics::{compare_values, has_ordered_statistics};
use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator};
use crate::planner::logical::COLUMN_IDENTIFIER_ROW_ID;
use duckrs_common::STANDARD_VECTOR_SIZE;
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_parser::ast::ComparisonOp;
use std::cmp::Ordering;
use std::sync::Arc;

/// `StringStatsData::MAX_STRING_MINMAX_SIZE`: the bytes of the bounds of strings C++ keeps.
pub const STRING_STATISTICS_BYTES: usize = 12;

/// Functions that do not give NULL for a NULL argument (`FunctionNullHandling::SPECIAL_HANDLING`): the one
/// value of a group with NULLs says nothing of their result. Of the others, a NULL gives NULL, which
/// a range leaves out.
pub(crate) const SPECIAL_NULL_HANDLING: &[&str] = &[
    "hash",
    "coalesce",
    "concat",
    "concat_ws",
    "ifnull",
    "nullif",
    "list_value",
    "array_value",
    "struct_pack",
    "row",
    "map",
    "constant_or_null",
    "to_json",
    "json_object",
    "json_array",
    "typeof",
    "error",
    "list_concat",
    "array_concat",
    "map_concat",
    "greatest",
    "least",
    "list_position",
    "list_indexof",
    "array_position",
    "array_indexof",
    "list_contains",
    "list_has",
    "array_contains",
    "array_has",
    "list_filter",
    "list_transform",
    "list_zip",
    "list_resize",
    "list_intersect",
    "array_slice",
    "list_slice",
    "map_entries",
    "map_keys",
    "map_values",
    "map_contains",
    "map_extract",
    "struct_insert",
    "struct_update",
    "struct_contains",
    "remap_struct",
    "string_split",
    "str_split",
    "strftime",
    "invoke",
    "create_sort_key",
    "stats",
    "regexp_matches",
    "regexp_replace",
    "regexp_extract",
];

/// The date part `part` of `value`, if it can be worked out.
pub(crate) fn evaluate_part(part: &str, value: &Value) -> Option<Value> {
    let call = crate::function::dates::bind_date_function(
        "date_part",
        vec![
            BoundExpr::constant(Value::varchar(part)),
            BoundExpr::constant(value.clone()),
        ],
    )
    .ok()?;
    try_evaluate_scalar(&call).filter(|value| !value.is_null())
}

/// `DEFAULT_ROW_GROUP_SIZE`.
pub const ROW_GROUP_SIZE: usize = 122_880;
/// The chunks of a row group, which all but the last chunk of a table fill.
pub const CHUNKS_PER_ROW_GROUP: usize = ROW_GROUP_SIZE / STANDARD_VECTOR_SIZE;

/// The row groups of `rows` rows of `table`.
pub fn row_group_count(table: &TableCatalogEntry, rows: usize) -> usize {
    rows.div_ceil(table.row_group_size().max(1))
}

/// Functions whose result does not go down when their argument goes up.
const MONOTONE_FUNCTIONS: &[&str] = &[
    "floor",
    "ceil",
    "ceiling",
    "year",
    "decade",
    "century",
    "millennium",
    "sign",
];

/// `ArgProperties` of the functions that declare one: the argument whose order the result follows (`true`) or
/// reverses (`false`) while the others are constant (`PropagateMonotoneBounds`).
const MONOTONE_ARGUMENTS: &[(&str, usize, bool)] = &[
    ("atan", 0, true),
    ("asinh", 0, true),
    ("cbrt", 0, true),
    ("degrees", 0, true),
    ("radians", 0, true),
    ("ln", 0, true),
    ("log10", 0, true),
    ("log2", 0, true),
    ("sinh", 0, true),
    ("sqrt", 0, true),
    ("tanh", 0, true),
    ("trunc", 0, true),
    ("round", 0, true),
    ("round_even", 0, true),
    ("roundbankers", 0, true),
    ("epoch", 0, true),
    ("epoch_ms", 0, true),
    ("epoch_us", 0, true),
    ("epoch_ns", 0, true),
    ("era", 0, true),
    ("isoyear", 0, true),
    ("last_day", 0, true),
    ("normalized_interval", 0, true),
    ("to_timestamp", 0, true),
    ("make_date", 0, true),
    ("make_timestamp", 0, true),
    ("make_timestamp_ns", 0, true),
    ("make_timestamp_ms", 0, true),
    ("date_trunc", 1, true),
    ("datetrunc", 1, true),
    ("time_bucket", 1, true),
    ("date_diff", 1, false),
    ("date_diff", 2, true),
    ("datediff", 1, false),
    ("datediff", 2, true),
    ("date_sub", 1, false),
    ("date_sub", 2, true),
    ("datesub", 1, false),
    ("datesub", 2, true),
];

/// The zones of a column, by the keys in which its values sort as they do in a comparison.
fn compute_zones(table: &TableCatalogEntry, column: usize, known: &[ColumnZone]) -> Vec<ColumnZone> {
    // intervals sort by their normalized form, as they compare
    let ty = &table.columns[column].ty;
    if let Some(element) = nested_element_type(ty) {
        return compute_nested_zones(table, column, &element);
    }
    // UUIDs and ENUMs keep the statistics of the integers they are stored as
    if !has_ordered_statistics(ty)
        && !matches!(
            ty.id(),
            LogicalTypeId::Interval | LogicalTypeId::Uuid | LogicalTypeId::Enum
        )
    {
        return Vec::new();
    }
    if let Some(source) = table
        .source()
        .filter(|source| TableCursor::is_aligned(&**source, table.row_group_size()))
    {
        // what the file knows of its row groups: a query need not read those that it doesn't want
        return source.zones(column).unwrap_or_default();
    }
    let mut zones = known.to_vec();
    let chunks_per_group = table.chunks_per_row_group();
    let first_chunk = known.len() * chunks_per_group;
    let read = table.for_each_chunk(column, |index, vector, count| {
        if index < first_chunk {
            return;
        }
        if index % chunks_per_group == 0 {
            zones.push(ColumnZone::default());
        }
        let zone = zones.last_mut().expect("just pushed one");
        add_to_zone(zone, vector, count);
    });
    // with what can't be read there is nothing to go by, which means reading every group
    if read.is_err() {
        return Vec::new();
    }
    zones
}

/// What `count` values of `vector` add to `zone`: their bounds by the keys they sort by, NULLs, and the lengths of
/// strings.
fn add_to_zone(zone: &mut ColumnZone, vector: &duckrs_common::vector::Vector, count: usize) {
    let Ok(keys) = group_keys(&[vector], count) else {
        zone.unknown = true;
        return;
    };
    let validity = vector.to_unified(count);
    let (mut smallest, mut largest): (Option<usize>, Option<usize>) = (None, None);
    for (row, key) in keys.iter().enumerate() {
        if !validity.is_valid(row) {
            zone.has_null = true;
            continue;
        }
        if smallest.is_none_or(|at| key < &keys[at]) {
            smallest = Some(row);
        }
        if largest.is_none_or(|at| key > &keys[at]) {
            largest = Some(row);
        }
    }
    let merge = |current: &mut Option<Value>, row: Option<usize>, keep: Ordering| {
        let Some(value) = row.map(|row| vector.get_value(row)) else {
            return;
        };
        let replace = current.as_ref().is_none_or(|old| {
            let op = match keep {
                Ordering::Less => ComparisonOp::LessThan,
                _ => ComparisonOp::GreaterThan,
            };
            compare_values(op, &value, old) == Some(true)
        });
        if replace {
            *current = Some(value);
        }
    };
    merge(&mut zone.min, smallest, Ordering::Less);
    merge(&mut zone.max, largest, Ordering::Greater);
    // the lengths of strings (`StringStats` keeps the longest and whether there is unicode)
    if vector.logical_type().id() == LogicalTypeId::Varchar {
        for row in (0..count).filter(|&row| validity.is_valid(row)) {
            let Ok(text) = vector.get_value(row).try_to_string() else {
                continue;
            };
            let length = text.len() as u64;
            let (low, high) = zone.lengths.unwrap_or((length, length));
            zone.lengths = Some((low.min(length), high.max(length)));
            zone.has_unicode |= !text.is_ascii();
        }
    }
}

/// `ColumnSegment` statistics, which a scan checks per vector (`RowGroup::CheckZonemapSegments`): a zone per vector
/// of a column of a table in memory, or of the field `extract` of a STRUCT column. Bounds where the values have
/// ordered statistics, otherwise only whether there are NULLs. None for tables in files.
pub(crate) fn vector_zones_of(
    table: &TableCatalogEntry,
    column: usize,
    extract: Option<&crate::planner::logical::PushedExtract>,
) -> Arc<Vec<ColumnZone>> {
    let key = (column, extract.map(|e| e.names.join(".")).unwrap_or_default());
    if let Some(source) = table.source() {
        return file_vector_zones(table, &*source, column, extract, key);
    }
    let vectors = table.row_count().div_ceil(duckrs_common::STANDARD_VECTOR_SIZE);
    let (version, known) = {
        let mut cached = table.statistics.lock();
        if cached.unreported {
            cached.forget();
        }
        let known = cached.vector_zones.get(&key).cloned().unwrap_or_default();
        if known.len() == vectors {
            return known;
        }
        (cached.version, known)
    };
    let mut zones = known.to_vec();
    let first = zones.len();
    let read = table.for_each_chunk(column, |index, vector, count| {
        if index < first {
            return;
        }
        let field;
        let vector = match extract {
            Some(extract) => {
                let input = duckrs_common::vector::DataChunk::from_columns(vec![vector.clone()], count);
                match crate::execution::expression_executor::execute(&extract.expression, &input) {
                    Ok(result) => {
                        field = result;
                        &field
                    }
                    Err(_) => {
                        zones.push(ColumnZone {
                            unknown: true,
                            ..ColumnZone::default()
                        });
                        return;
                    }
                }
            }
            None => vector,
        };
        let ty = vector.logical_type();
        let mut zone = ColumnZone::default();
        if has_ordered_statistics(ty)
            || matches!(
                ty.id(),
                LogicalTypeId::Interval | LogicalTypeId::Uuid | LogicalTypeId::Enum
            )
        {
            add_to_zone(&mut zone, vector, count);
        } else {
            // values without ordered statistics (nested ones among them): nothing to go by
            zone.unknown = true;
        }
        zones.push(zone);
    });
    if read.is_err() {
        return Arc::default();
    }
    let zones = Arc::new(zones);
    let mut cached = table.statistics.lock();
    if cached.version == version {
        cached.vector_zones.insert(key, zones.clone());
    }
    zones
}

/// [`vector_zones_of`] of a table in a file: the zones of the segments over each vector, joined.
fn file_vector_zones(
    table: &TableCatalogEntry,
    source: &dyn crate::catalog::TableSource,
    column: usize,
    extract: Option<&crate::planner::logical::PushedExtract>,
    key: (usize, String),
) -> Arc<Vec<ColumnZone>> {
    let version = {
        let cached = table.statistics.lock();
        if let Some(zones) = cached.vector_zones.get(&key) {
            return zones.clone();
        }
        cached.version
    };
    // the fields of a STRUCT the extracts lead to, by position
    let mut path = Vec::new();
    if let Some(extract) = extract {
        let mut expr = &extract.expression;
        while let BoundExprKind::Function { function, children, .. } = &expr.kind {
            if function.name != "struct_extract" || children.len() != 2 {
                return Arc::default();
            }
            let BoundExprKind::Constant(key) = &children[1].kind else {
                return Arc::default();
            };
            let fields = children[0].return_type.struct_children();
            let field = match key.as_i128() {
                Some(index) => usize::try_from(index).ok(),
                None => key
                    .try_to_string()
                    .ok()
                    .and_then(|name| fields.iter().position(|(n, _)| n.eq_ignore_ascii_case(&name))),
            };
            let Some(field) = field.filter(|&f| f < fields.len()) else {
                return Arc::default();
            };
            path.push(field);
            expr = &children[0];
        }
        if !matches!(expr.kind, BoundExprKind::Reference(0)) {
            return Arc::default();
        }
        path.reverse();
    }
    let Ok(segments) = source.segment_zones(column, &path) else {
        return Arc::default();
    };
    // a scan numbers the rows that are there, the segments the rows as stored: with rows deleted (holes), a
    // vector's rows are those stored from the first one's row id to the last one's
    let rows = table.row_count();
    let stored_rows = source.row_count();
    let ids = source.row_ids();
    let vectors = rows.div_ceil(duckrs_common::STANDARD_VECTOR_SIZE);
    let mut zones = Vec::with_capacity(vectors);
    let mut at = 0;
    for vector in 0..vectors {
        let (live_first, live_last) = (
            vector * duckrs_common::STANDARD_VECTOR_SIZE,
            ((vector + 1) * duckrs_common::STANDARD_VECTOR_SIZE).min(rows),
        );
        // rows appended to the table since it was read: nothing is known of them here
        if live_last > stored_rows {
            zones.push(ColumnZone {
                unknown: true,
                ..ColumnZone::default()
            });
            continue;
        }
        let (first, last) = (ids.id(live_first) as usize, ids.id(live_last - 1) as usize + 1);
        while at < segments.len() && segments[at].0 + segments[at].1 <= first {
            at += 1;
        }
        let mut zone: Option<ColumnZone> = None;
        let mut covered = first;
        for (start, count, segment) in segments[at..].iter().take_while(|(start, ..)| *start < last) {
            if *start > covered {
                break;
            }
            covered = start + count;
            zone = Some(match zone {
                None => segment.clone(),
                Some(joined) => {
                    let is = |op, a: &Value, b: &Value| compare_values(op, a, b) == Some(true);
                    ColumnZone {
                        min: match (joined.min, segment.min.clone()) {
                            (Some(a), Some(b)) => Some(if is(ComparisonOp::LessThan, &b, &a) { b } else { a }),
                            (a, b) => a.or(b),
                        },
                        max: match (joined.max, segment.max.clone()) {
                            (Some(a), Some(b)) => Some(if is(ComparisonOp::GreaterThan, &b, &a) { b } else { a }),
                            (a, b) => a.or(b),
                        },
                        has_null: joined.has_null || segment.has_null,
                        unknown: joined.unknown || segment.unknown,
                        ..ColumnZone::default()
                    }
                }
            });
        }
        // rows no segment covers: nothing is known of them
        zones.push(zone.filter(|_| covered >= last).unwrap_or(ColumnZone {
            unknown: true,
            ..ColumnZone::default()
        }));
    }
    let zones = Arc::new(zones);
    let mut cached = table.statistics.lock();
    if cached.version == version {
        cached.vector_zones.insert(key, zones.clone());
    }
    zones
}

/// Whether `value` lies outside of the bounds of `elements` (no element at all counts as outside). Of
/// strings and blobs only the 12 bytes C++ keeps of the bounds are compared.
fn outside_elements(value: &Value, elements: &ColumnZone) -> bool {
    let (Some(min), Some(max)) = (&elements.min, &elements.max) else {
        return true;
    };
    let cut = |v: &Value| -> Value {
        match v.data() {
            duckrs_common::value::ValueData::Bytes(bytes) if bytes.len() > STRING_STATISTICS_BYTES => {
                let mut cut = bytes[..STRING_STATISTICS_BYTES].to_vec();
                // a truncated bound stands for all strings it is a prefix of
                if std::ptr::eq(v, max) {
                    cut.push(0xff);
                }
                Value::new(v.logical_type().clone(), duckrs_common::value::ValueData::Bytes(cut))
                    .unwrap_or_else(|_| v.clone())
            }
            _ => v.clone(),
        }
    };
    let (min, max) = (cut(min), cut(max));
    compare_values(ComparisonOp::LessThan, value, &min) == Some(true)
        || compare_values(ComparisonOp::GreaterThan, value, &max) == Some(true)
}

/// The type of what the zone of a nested column's `elements` describes: the elements of a LIST or
/// ARRAY, the values of a MAP; `None` for other types, or elements without ordered statistics.
fn nested_element_type(ty: &LogicalType) -> Option<LogicalType> {
    let element = match ty.id() {
        LogicalTypeId::List | LogicalTypeId::Array => ty.child_type()?.clone(),
        LogicalTypeId::Map => ty.child_type()?.struct_children().get(1)?.1.clone(),
        _ => return None,
    };
    (has_ordered_statistics(&element) || matches!(element.id(), LogicalTypeId::Blob | LogicalTypeId::Interval))
        .then_some(element)
}

/// The zones of a nested column: what is NULL, and the bounds of its elements.
fn compute_nested_zones(table: &TableCatalogEntry, column: usize, element: &LogicalType) -> Vec<ColumnZone> {
    let mut zones: Vec<ColumnZone> = Vec::new();
    let map = table.columns[column].ty.id() == LogicalTypeId::Map;
    let chunks_per_group = table.chunks_per_row_group();
    let read = table.for_each_chunk(column, |index, vector, count| {
        if index % chunks_per_group == 0 {
            zones.push(ColumnZone {
                // the elements of the group are known until one is not
                elements: Some(Box::default()),
                ..ColumnZone::default()
            });
        }
        let zone = zones.last_mut().expect("just pushed one");
        for row in 0..count {
            let value = vector.get_value(row);
            if value.is_null() {
                zone.has_null = true;
                continue;
            }
            let duckrs_common::value::ValueData::List(items) = value.data() else {
                zone.elements = None;
                continue;
            };
            let Some(elements) = zone.elements.as_mut() else {
                continue;
            };
            for item in items {
                let item = match (map, item.data()) {
                    (true, duckrs_common::value::ValueData::Struct(fields)) if fields.len() == 2 => &fields[1],
                    (true, _) => {
                        zone.elements = None;
                        break;
                    }
                    (false, _) => item,
                };
                if item.is_null() {
                    elements.has_null = true;
                    continue;
                }
                if item.logical_type() != element {
                    zone.elements = None;
                    break;
                }
                if elements
                    .min
                    .as_ref()
                    .is_none_or(|min| compare_values(ComparisonOp::LessThan, item, min) == Some(true))
                {
                    elements.min = Some(item.clone());
                }
                if elements
                    .max
                    .as_ref()
                    .is_none_or(|max| compare_values(ComparisonOp::GreaterThan, item, max) == Some(true))
                {
                    elements.max = Some(item.clone());
                }
            }
        }
    });
    if read.is_err() {
        return Vec::new();
    }
    // the bounds of the column itself are not known, only its elements' (where those could be told)
    for zone in &mut zones {
        zone.unknown = zone.elements.is_none();
    }
    zones
}

/// `compute_zones`, once per state of the rows.
pub(crate) fn zones_of(table: &TableCatalogEntry, column: usize) -> Arc<Vec<ColumnZone>> {
    let groups = row_group_count(table, table.row_count());
    let (version, known) = {
        let mut cached = table.statistics.lock();
        if cached.unreported {
            cached.forget();
        }
        let known = cached.zones.get(&column).cloned().unwrap_or_default();
        if known.len() == groups {
            return known;
        }
        (cached.version, known)
    };
    let zones = Arc::new(compute_zones(table, column, &known));
    let mut cached = table.statistics.lock();
    if cached.version == version {
        cached.zones.insert(column, zones.clone());
    }
    zones
}

/// The argument of a call of `name` that its result follows, in order or reversed, while the others are constant.
/// Whether a function of one argument follows the order of it (`true`) or reverses it (`false`),
/// where its `ArgProperties` say it does either.
pub(crate) fn unary_monotonicity(name: &str) -> Option<bool> {
    if MONOTONE_FUNCTIONS.contains(&name) {
        return Some(true);
    }
    MONOTONE_ARGUMENTS
        .iter()
        .find(|(function, at, _)| *function == name && *at == 0)
        .map(|&(_, _, increasing)| increasing)
}

fn monotone_argument(name: &str, children: &[BoundExpr]) -> Option<(usize, bool)> {
    MONOTONE_ARGUMENTS
        .iter()
        .find(|(function, at, _)| {
            *function == name
                && *at < children.len()
                && !children[*at].is_foldable()
                && children.iter().enumerate().all(|(i, c)| i == *at || c.is_foldable())
        })
        .map(|&(_, at, increasing)| (at, increasing))
}

/// The values an expression can have in a group.
enum Range {
    /// nothing is known
    Unknown,
    /// only NULL
    Null,
    Bounds(Value, Value),
}

/// The zones of a scan's output columns for one group: what is known of the column at each position.
struct Group<'a> {
    zone: &'a dyn Fn(usize) -> Option<ColumnZone>,
    /// the statistics of the VARIANT columns, where they are known
    variant: Option<&'a dyn Fn(usize) -> Option<crate::catalog::ColumnStatistics>>,
}

/// The zone of the output column `index` of a scan of `table` reading `column_ids`, in row group `group`.
fn table_zone(
    table: &TableCatalogEntry,
    column_ids: &[usize],
    group: usize,
    rows: usize,
    index: usize,
) -> Option<ColumnZone> {
    let column = *column_ids.get(index)?;
    if column == COLUMN_IDENTIFIER_ROW_ID {
        let size = table.row_group_size();
        let first = group * size;
        let last = (first + size).min(rows).checked_sub(1)?;
        let ids = table.row_ids(rows);
        return Some(ColumnZone {
            min: Some(Value::bigint(ids.id(first) as i64)),
            max: Some(Value::bigint(ids.id(last) as i64)),
            ..ColumnZone::default()
        });
    }
    let zones = zones_of(table, column);
    let zone = zones.get(group)?;
    (!zone.unknown).then(|| zone.clone())
}

/// Whether no row can make `filter` true, where `zone` gives what is known of the column at each position of
/// the rows `filter` reads (`ExpressionFilter::CheckStatistics`).
pub fn never_true(zone: &dyn Fn(usize) -> Option<ColumnZone>, filter: &BoundExpr) -> bool {
    Group { zone, variant: None }.never_true(filter)
}

/// [`never_true`], where `variant` gives the statistics of the VARIANT columns, which the paths of them cast to
/// a type have bounds by (`VariantExtractPropagateStats`, `StatisticsPropagateVariant`).
pub fn never_true_with_variants(
    zone: &dyn Fn(usize) -> Option<ColumnZone>,
    variant: &dyn Fn(usize) -> Option<crate::catalog::ColumnStatistics>,
    filter: &BoundExpr,
) -> bool {
    Group {
        zone,
        variant: Some(variant),
    }
    .never_true(filter)
}

impl Group<'_> {
    fn zone(&self, index: usize) -> Option<ColumnZone> {
        (self.zone)(index)
    }

    fn column_range(&self, index: usize) -> Range {
        match self.zone(index) {
            None => Range::Unknown,
            // a nested column: only its elements have bounds
            Some(ColumnZone { elements: Some(_), .. }) => Range::Unknown,
            Some(ColumnZone {
                min: Some(min),
                max: Some(max),
                ..
            }) => Range::Bounds(min, max),
            Some(_) => Range::Null,
        }
    }

    fn range(&self, expr: &BoundExpr) -> Range {
        if let Some(variant) = self.variant
            && matches!(expr.kind, BoundExprKind::Cast { try_cast: false, .. })
            && let Some(stats) = crate::optimizer::variant_statistics::variant_path_statistics(expr, variant)
        {
            return match (stats.min, stats.max) {
                _ if !stats.can_be_valid => Range::Null,
                (Some(min), Some(max)) => Range::Bounds(min, max),
                _ => Range::Unknown,
            };
        }
        match &expr.kind {
            BoundExprKind::Reference(index) => self.column_range(*index),
            BoundExprKind::Cast { child, .. } if child.return_type == expr.return_type => self.range(child),
            // a timestamp in another unit sorts the same; a value that doesn't fit leaves the range unknown
            BoundExprKind::Cast { child, try_cast: false }
                if (is_timestamp(&child.return_type) && is_timestamp(&expr.return_type))
                    || (is_timestamp_tz(&child.return_type) && is_timestamp_tz(&expr.return_type)) =>
            {
                // the infinities sort above the smallest values, and a cast to another unit moves those: of bounds
                // that hold one, nothing is known
                let infinite = |value: &Value| value.try_to_string().is_ok_and(|text| text.contains("infinity"));
                match self.through(expr, child) {
                    Range::Bounds(min, max)
                        if [&min, &max].into_iter().any(infinite)
                            || matches!(self.range(child), Range::Bounds(ref a, ref b) if infinite(a) || infinite(b)) =>
                    {
                        Range::Unknown
                    }
                    other => other,
                }
            }
            // a TRY_CAST gives NULL for what doesn't fit, and the order of the rest; of bounds that don't fit
            // nothing is known
            BoundExprKind::Cast { child, .. } if keeps_order(&child.return_type, &expr.return_type) => {
                self.through(expr, child)
            }
            // the date parts of an INTERVAL do not follow its order (1 year and 400 days)
            BoundExprKind::Function { function, children, .. }
                if children.len() == 1
                    && MONOTONE_FUNCTIONS.contains(&function.name.as_str())
                    && children[0].return_type.id() != LogicalTypeId::Interval =>
            {
                self.through(expr, &children[0])
            }
            // what follows the order of a timestamp does not follow that of an INTERVAL: epoch counts 365.25 days
            // a year where the comparison counts 360
            BoundExprKind::Function { function, children, .. }
                if monotone_argument(&function.name, children)
                    .is_some_and(|(at, _)| children[at].return_type.id() != LogicalTypeId::Interval) =>
            {
                let (varying, increasing) = monotone_argument(&function.name, children).expect("checked");
                self.monotone(expr, varying, increasing)
            }
            // a cast of NULL is NULL, whether it could convert the values or not
            BoundExprKind::Cast { child, .. } if matches!(self.range(child), Range::Null) => Range::Null,
            BoundExprKind::Constant(value) if value.is_null() => Range::Null,
            BoundExprKind::Constant(value) => Range::Bounds(value.clone(), value.clone()),
            // `PropagateCaseExpression`: what any branch gives
            BoundExprKind::Case { checks, else_expr } => {
                let branches = checks.iter().map(|(_, then)| then).chain(std::iter::once(&**else_expr));
                self.union(branches)
            }
            BoundExprKind::Function { function, children, .. } => self.function_range(expr, &function.name, children),
            BoundExprKind::Operator {
                op: BoundOperator::Coalesce,
                children,
            } => self.union(children.iter()),
            _ => Range::Unknown,
        }
    }

    /// Whether `x != a AND x != b ...` among `conjuncts` leaves no value of the range of an integer column in
    /// the group (a NOT IN list that covers it).
    fn excludes_every_value(&self, conjuncts: &[BoundExpr]) -> bool {
        let mut excluded: std::collections::HashMap<usize, std::collections::HashSet<i128>> = Default::default();
        for conjunct in conjuncts {
            let BoundExprKind::Comparison {
                op: ComparisonOp::NotEqual,
                left,
                right,
            } = &conjunct.kind
            else {
                continue;
            };
            let (index, constant) = match (&left.kind, &right.kind) {
                (BoundExprKind::Reference(index), BoundExprKind::Constant(c))
                | (BoundExprKind::Constant(c), BoundExprKind::Reference(index)) => (*index, c),
                _ => continue,
            };
            if let Some(value) = constant.as_i128() {
                excluded.entry(index).or_default().insert(value);
            }
        }
        excluded.into_iter().any(|(index, values)| {
            let Some(zone) = self.zone(index) else {
                return false;
            };
            let (Some(min), Some(max)) = (zone.min.as_ref(), zone.max.as_ref()) else {
                return false;
            };
            if !min.logical_type().is_integral() {
                return false;
            }
            let (Some(min), Some(max)) = (min.as_i128(), max.as_i128()) else {
                return false;
            };
            // a range wider than `i128` (HUGEINT values at both ends) is never covered by the excluded values
            let narrow = max.checked_sub(min).is_some_and(|width| width >= 0 && width < values.len() as i128);
            narrow && (min..=max).all(|v| values.contains(&v))
        })
    }

    /// The zone of the elements of a LIST or ARRAY column in the group, or of the values of a MAP
    /// (also through `map_values`).
    fn elements(&self, expr: &BoundExpr) -> Option<ColumnZone> {
        match &expr.kind {
            BoundExprKind::Reference(index) => self.zone(*index)?.elements.map(|zone| *zone),
            BoundExprKind::Function { function, children, .. }
                if function.name == "map_values" && children.len() == 1 =>
            {
                self.elements(&children[0])
                    .filter(|_| children[0].return_type.id() == LogicalTypeId::Map)
            }
            // `list_transform(l, x -> f(x))`: f of the elements' range, for a lambda that reads nothing else
            BoundExprKind::ListLambda {
                kind: crate::planner::expression::LambdaKind::Transform,
                list,
                captures,
                body,
                ..
            } if captures.is_empty() => {
                let inner = self.elements(list)?;
                let zone = move |index: usize| (index == 0).then(|| inner.clone());
                let range = Group {
                    zone: &zone,
                    variant: None,
                }
                .range(body);
                match range {
                    Range::Bounds(min, max) => Some(ColumnZone {
                        min: Some(min),
                        max: Some(max),
                        ..ColumnZone::default()
                    }),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The fewest and most bytes of the strings of a column in the group, and whether any is unicode.
    fn lengths(&self, expr: &BoundExpr) -> Option<(u64, u64, bool)> {
        let BoundExprKind::Reference(index) = &expr.kind else {
            return None;
        };
        let zone = self.zone(*index)?;
        let (low, high) = zone.lengths?;
        Some((low, high, zone.has_unicode))
    }

    /// Whether `expr` can be NULL in the group: a column with NULLs, or anything that is not known never to be.
    fn may_be_null(&self, expr: &BoundExpr) -> bool {
        match &expr.kind {
            BoundExprKind::Constant(value) => value.is_null(),
            BoundExprKind::Reference(index) => self.zone(*index).is_none_or(|zone| zone.has_null),
            BoundExprKind::Cast { child, try_cast: false } => self.may_be_null(child),
            _ => true,
        }
    }

    /// The range of what is any of `exprs`.
    fn union<'e>(&self, exprs: impl Iterator<Item = &'e BoundExpr>) -> Range {
        let is = |op, a: &Value, b: &Value| compare_values(op, a, b) == Some(true);
        let mut result = Range::Null;
        for expr in exprs {
            result = match (result, self.range(expr)) {
                (Range::Unknown, _) | (_, Range::Unknown) => return Range::Unknown,
                (Range::Null, other) | (other, Range::Null) => other,
                (Range::Bounds(a_min, a_max), Range::Bounds(b_min, b_max)) => Range::Bounds(
                    if is(ComparisonOp::LessThan, &b_min, &a_min) {
                        b_min
                    } else {
                        a_min
                    },
                    if is(ComparisonOp::GreaterThan, &b_max, &a_max) {
                        b_max
                    } else {
                        a_max
                    },
                ),
            };
        }
        result
    }

    /// The statistics of a function of the columns (`FunctionStatistics` of `coalesce`, `greatest`, `least`, the
    /// arithmetic operators and the date parts).
    fn function_range(&self, expr: &BoundExpr, name: &str, children: &[BoundExpr]) -> Range {
        let is = |op, a: &Value, b: &Value| compare_values(op, a, b) == Some(true);
        let constant = |e: &BoundExpr| match &e.kind {
            BoundExprKind::Constant(value) if !value.is_null() => Some(value.clone()),
            _ => None,
        };
        let sign = |value: &Value| -> Option<Ordering> {
            let zero = crate::execution::parquet::try_cast_value(&Value::integer(0), value.logical_type())?;
            if is(ComparisonOp::GreaterThan, value, &zero) {
                Some(Ordering::Greater)
            } else if is(ComparisonOp::LessThan, value, &zero) {
                Some(Ordering::Less)
            } else {
                Some(Ordering::Equal)
            }
        };
        match (name, children) {
            ("coalesce", _) => self.union(children.iter()),
            // `abs` keeps the order of values that are not negative, and reverses that of values that are not positive
            ("abs", [child]) => match self.range(child) {
                Range::Bounds(min, _) if sign(&min).is_some_and(|s| s != Ordering::Less) => {
                    self.monotone(expr, 0, true)
                }
                Range::Bounds(_, max) if sign(&max).is_some_and(|s| s != Ordering::Greater) => {
                    self.monotone(expr, 0, false)
                }
                Range::Bounds(min, max) => {
                    let BoundExprKind::Function { .. } = &expr.kind else {
                        return Range::Unknown;
                    };
                    let at = |value: &Value| {
                        let mut call = expr.clone();
                        if let BoundExprKind::Function { children, .. } = &mut call.kind {
                            children[0] = BoundExpr::constant(value.clone());
                        }
                        try_evaluate_scalar(&call).filter(|result| !result.is_null())
                    };
                    let zero = crate::execution::parquet::try_cast_value(&Value::integer(0), &expr.return_type);
                    match (at(&min), at(&max), zero) {
                        (Some(low), Some(high), Some(zero)) => Range::Bounds(
                            zero,
                            if is(ComparisonOp::GreaterThan, &low, &high) {
                                low
                            } else {
                                high
                            },
                        ),
                        _ => Range::Unknown,
                    }
                }
                other => other,
            },
            // `s || ''` and `replace(s, a, a)` are `s`
            ("||", [input, other]) | ("||", [other, input])
                if constant(other).and_then(|v| v.try_to_string().ok()).as_deref() == Some("") =>
            {
                self.range(input)
            }
            ("replace", [input, from, to])
                if constant(from).is_some()
                    && constant(from).map(|v| v.try_to_string().ok())
                        == constant(to).map(|v| v.try_to_string().ok()) =>
            {
                self.range(input)
            }
            // `PropagateModuloStats` of integers: the bounds of the dividend and the divisor
            ("%", [input, divisor]) if expr.return_type.is_integral() => {
                let (Range::Bounds(lmin, lmax), Range::Bounds(rmin, rmax)) = (self.range(input), self.range(divisor))
                else {
                    return Range::Unknown;
                };
                let null_on_zero =
                    matches!(&expr.kind, BoundExprKind::Function { function, .. } if function.null_on_zero);
                let bounds = match (lmin.as_i128(), lmax.as_i128(), rmin.as_i128(), rmax.as_i128()) {
                    (Some(a), Some(b), Some(c), Some(d)) => crate::optimizer::statistics::integer_modulo_bounds(
                        (a, b),
                        (c, d),
                        &expr.return_type,
                        null_on_zero,
                    ),
                    _ => None,
                };
                let bound = |n: i128| crate::execution::parquet::try_cast_value(&Value::hugeint(n), &expr.return_type);
                match bounds.map(|(min, max)| (bound(min), bound(max))) {
                    Some((Some(min), Some(max))) => Range::Bounds(min, max),
                    _ => Range::Unknown,
                }
            }
            // the lengths of strings (`StringStats`): in bytes, and in characters, which without unicode
            // are bytes and with it at least a quarter of them
            (
                "length" | "len" | "char_length" | "character_length" | "length_grapheme" | "strlen" | "octet_length",
                [input],
            ) if self.lengths(input).is_some() => {
                let (low, high, unicode) = self.lengths(input).expect("checked");
                let bytes = matches!(name, "strlen" | "octet_length");
                let low = if bytes || !unicode {
                    low
                } else if name == "length_grapheme" {
                    0
                } else {
                    low.div_ceil(4)
                };
                let bound =
                    |n: u64| crate::execution::parquet::try_cast_value(&Value::bigint(n as i64), &expr.return_type);
                match (bound(low), bound(high)) {
                    (Some(low), Some(high)) => Range::Bounds(low, high),
                    _ => Range::Unknown,
                }
            }
            // a needle in the prefix that all strings of the group share (of the bytes C++ keeps of the bounds)
            // is in all of them
            ("contains", [input, needle])
                if constant(needle)
                    .and_then(|n| n.try_to_string().ok())
                    .is_some_and(|needle| {
                        let Range::Bounds(min, max) = self.range(input) else {
                            return false;
                        };
                        let (Ok(min), Ok(max)) = (min.try_to_string(), max.try_to_string()) else {
                            return false;
                        };
                        let shared = min
                            .bytes()
                            .zip(max.bytes())
                            .take(STRING_STATISTICS_BYTES)
                            .take_while(|(a, b)| a == b)
                            .count();
                        min.get(..shared).is_some_and(|prefix| prefix.contains(needle.as_str()))
                    }) =>
            {
                Range::Bounds(Value::boolean(true), Value::boolean(true))
            }
            // a needle longer than every string is in none of them
            ("contains" | "suffix" | "ends_with", [input, needle])
                if self.lengths(input).is_some()
                    && constant(needle)
                        .and_then(|n| n.try_to_string().ok())
                        .is_some_and(|n| n.len() as u64 > self.lengths(input).expect("checked").1) =>
            {
                Range::Bounds(Value::boolean(false), Value::boolean(false))
            }
            // `upper` and `lower` work on each character: strings that share a prefix (of the 12 bytes C++
            // keeps of the bounds) give strings that share its converted prefix
            ("upper" | "lower" | "ucase" | "lcase", [input]) => {
                let Range::Bounds(min, max) = self.range(input) else {
                    return self.range(input);
                };
                let (Ok(min), Ok(max)) = (min.try_to_string(), max.try_to_string()) else {
                    return Range::Unknown;
                };
                let shared: String = min
                    .chars()
                    .zip(max.chars())
                    .take_while(|(a, b)| a == b)
                    .map(|(a, _)| a)
                    .scan(0, |bytes, c| {
                        *bytes += c.len_utf8();
                        (*bytes <= STRING_STATISTICS_BYTES).then_some(c)
                    })
                    .collect();
                if shared.is_empty() || !shared.is_ascii() {
                    return Range::Unknown;
                }
                let prefix = if matches!(name, "upper" | "ucase") {
                    shared.to_ascii_uppercase()
                } else {
                    shared.to_ascii_lowercase()
                };
                let mut above = prefix.clone();
                above.push(char::MAX);
                Range::Bounds(Value::varchar(prefix), Value::varchar(above))
            }
            // comparisons of intervals go by their normalized form already
            ("normalized_interval", [_]) => self.monotone(expr, 0, true),
            // `substring(s, k)` of strings that share their first k - 1 characters sorts as the strings do
            // `s[:]` is `s`, and `s[:n]` a prefix of it, which sorts as it does
            ("array_slice" | "list_slice", [input, start, end])
                if expr.return_type.id() == LogicalTypeId::Varchar
                    && start.return_type.id() == LogicalTypeId::List
                    && matches!(end.kind, BoundExprKind::Constant(_)) =>
            {
                if end.return_type.id() == LogicalTypeId::List {
                    self.range(input)
                } else {
                    self.monotone(expr, 0, true)
                }
            }
            ("substring" | "substr" | "array_slice" | "list_slice", [input, start, rest @ ..])
                if expr.return_type.id() == LogicalTypeId::Varchar
                    && rest.iter().all(|r| matches!(r.kind, BoundExprKind::Constant(_)))
                    && constant(start).and_then(|v| v.as_i128()).is_some_and(|k| k >= 0)
                    && matches!(self.range(input), Range::Bounds(ref min, ref max) if {
                        let k = (constant(start).and_then(|v| v.as_i128()).expect("checked") as usize).max(1);
                        match (min.try_to_string(), max.try_to_string()) {
                            (Ok(min), Ok(max)) => {
                                let shared = min.chars().zip(max.chars()).take_while(|(a, b)| a == b).count();
                                // C++ knows the shared prefix only within the 12 bytes it keeps
                                shared >= k - 1 && min.chars().count() >= k - 1 && k - 1 <= STRING_STATISTICS_BYTES
                            }
                            _ => false,
                        }
                    }) =>
            {
                self.monotone(expr, 0, true)
            }
            // a time part of values that share the larger units follows them: `hour` within a day,
            // `minute` within an hour, `second` and the smaller ones within a minute
            ("hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond", [input]) => {
                match self.range(input) {
                    Range::Bounds(min, max) => {
                        let larger: &[&str] = match name {
                            "hour" => &["year", "doy"],
                            "minute" => &["year", "doy", "hour"],
                            _ => &["year", "doy", "hour", "minute"],
                        };
                        let same = larger.iter().all(|part| {
                            super::zone_map::evaluate_part(part, &min) == super::zone_map::evaluate_part(part, &max)
                        });
                        if same {
                            self.monotone(expr, 0, true)
                        } else {
                            Range::Unknown
                        }
                    }
                    other => other,
                }
            }
            // `week` of dates in one ISO year follows them
            ("week" | "weekofyear", [input]) => match self.range(input) {
                Range::Bounds(min, max) => {
                    let iso_year = |value: &Value| super::zone_map::evaluate_part("isoyear", value);
                    match (iso_year(&min), iso_year(&max)) {
                        (Some(a), Some(b)) if a == b => self.monotone(expr, 0, true),
                        _ => Range::Unknown,
                    }
                }
                other => other,
            },
            // an element of a LIST or ARRAY, or a value of a MAP, lies within what the group's elements are
            ("array_extract" | "list_extract" | "list_element" | "map_extract_value", [input, _])
                if self.elements(input).is_some()
                    && (input.return_type.id() == LogicalTypeId::Map
                        || expr.return_type == *input.return_type.child_type().expect("a nested type")) =>
            {
                let elements = self.elements(input).expect("checked");
                match (elements.min, elements.max) {
                    (Some(min), Some(max)) => Range::Bounds(min, max),
                    _ => Range::Null,
                }
            }
            // the position of a value outside of the bounds of the elements is NULL
            ("list_position" | "list_indexof" | "array_position" | "array_indexof", [input, value])
                if self.elements(input).is_some() && constant(value).is_some() =>
            {
                let elements = self.elements(input).expect("checked");
                let value = constant(value).expect("checked");
                let outside = outside_elements(&value, &elements);
                if outside { Range::Null } else { Range::Unknown }
            }
            // a list holds no value outside of the bounds of its elements
            ("list_contains" | "list_has" | "array_contains" | "array_has" | "contains", [input, value])
                if self.elements(input).is_some() && constant(value).is_some() =>
            {
                let elements = self.elements(input).expect("checked");
                let value = constant(value).expect("checked");
                let outside = outside_elements(&value, &elements);
                if outside {
                    Range::Bounds(Value::boolean(false), Value::boolean(false))
                } else {
                    Range::Unknown
                }
            }
            // `isnan`: NaN sorts above every number, so the bounds tell
            ("isnan", [input]) => match self.range(input) {
                Range::Bounds(min, max) => {
                    let nan = |v: &Value| match v.data() {
                        duckrs_common::value::ValueData::Float(f) => Some(f.is_nan()),
                        duckrs_common::value::ValueData::Double(d) => Some(d.is_nan()),
                        _ => None,
                    };
                    match (nan(&min), nan(&max)) {
                        (Some(low), Some(high)) => Range::Bounds(Value::boolean(low), Value::boolean(high)),
                        _ => Range::Unknown,
                    }
                }
                other => other,
            },
            // `PowerStats`: a power above 0 of what is never negative grows with it
            ("power" | "pow", [base, exponent])
                if constant(exponent).is_some_and(|e| sign(&e) == Some(Ordering::Greater))
                    && matches!(self.range(base), Range::Bounds(min, _) if sign(&min) != Some(Ordering::Less)) =>
            {
                self.monotone(expr, 0, true)
            }
            // `PropagatePowStats`, where the rule above does not apply
            ("pow" | "power" | "**" | "^", [base, exponent]) => {
                let float = |value: &Value| value.try_to_string().ok()?.parse::<f64>().ok();
                let Range::Bounds(exponent_min, exponent_max) = self.range(exponent) else {
                    return Range::Unknown;
                };
                let base = match self.range(base) {
                    Range::Bounds(min, max) => float(&min).zip(float(&max)),
                    _ => None,
                };
                let bounds = float(&exponent_min)
                    .zip(float(&exponent_max))
                    .and_then(|exponent| crate::optimizer::statistics::power_bounds(base, exponent));
                match bounds {
                    Some((min, max)) => Range::Bounds(Value::double(min), Value::double(max)),
                    None => Range::Unknown,
                }
            }
            ("__internal_barrier", [child]) => self.range(child),
            // a prefix of a string sorts as the string does
            ("left" | "substring" | "substr", [_, ..])
                if children[1..].iter().all(|c| constant(c).is_some())
                    // a negative count of `left` drops characters from the end: no prefix
                    && ((name == "left" && constant(&children[1]).and_then(|v| v.as_i128()).is_some_and(|n| n >= 0))
                        || constant(&children[1]).and_then(|v| v.try_to_string().ok()).as_deref() == Some("1")) =>
            {
                self.monotone(expr, 0, true)
            }
            ("greatest" | "least", [_, ..]) => {
                let greatest = name == "greatest";
                // `greatest` and `least` skip NULLs: where an argument can be NULL, the result can be any other one
                if children.iter().any(|child| self.may_be_null(child)) {
                    return self.union(children.iter());
                }
                let mut bounds: Option<(Value, Value)> = None;
                for child in children {
                    let Range::Bounds(min, max) = self.range(child) else {
                        return Range::Unknown;
                    };
                    bounds = Some(match bounds {
                        None => (min, max),
                        Some((a, b)) => {
                            let pick = |x: Value, y: Value| {
                                let op = if greatest {
                                    ComparisonOp::GreaterThan
                                } else {
                                    ComparisonOp::LessThan
                                };
                                if is(op, &y, &x) { y } else { x }
                            };
                            (pick(a, min), pick(b, max))
                        }
                    });
                }
                bounds.map_or(Range::Unknown, |(min, max)| Range::Bounds(min, max))
            }
            // a function of one column and a constant that keeps or reverses the order of values
            // adding months clamps to the end of the month, which does not keep the order of timestamps
            ("+" | "-", [left, right])
                if [left, right].into_iter().any(|side| {
                    constant(side).is_some_and(
                        |c| matches!(c.data(), duckrs_common::value::ValueData::Interval(i) if i.months != 0),
                    )
                }) =>
            {
                Range::Unknown
            }
            // `PropagateNumericStats` of two varying operands: the sum (or difference) of their bounds, where it
            // cannot overflow
            ("+" | "-", [left, right])
                if constant(left).is_none()
                    && constant(right).is_none()
                    && matches!(
                        expr.return_type.id(),
                        LogicalTypeId::Float
                            | LogicalTypeId::Double
                            | LogicalTypeId::TinyInt
                            | LogicalTypeId::SmallInt
                            | LogicalTypeId::Integer
                            | LogicalTypeId::BigInt
                    ) =>
            {
                let (Range::Bounds(left_min, left_max), Range::Bounds(right_min, right_max)) =
                    (self.range(left), self.range(right))
                else {
                    return Range::Unknown;
                };
                let (low, high) = if name == "+" {
                    ((&left_min, &right_min), (&left_max, &right_max))
                } else {
                    ((&left_min, &right_max), (&left_max, &right_min))
                };
                let combine = |a: &Value, b: &Value| -> Option<Value> {
                    let ty = &expr.return_type;
                    let text = if matches!(ty.id(), LogicalTypeId::Float | LogicalTypeId::Double) {
                        let (a, b) = (
                            a.try_to_string().ok()?.parse::<f64>().ok()?,
                            b.try_to_string().ok()?.parse::<f64>().ok()?,
                        );
                        let result = if name == "+" { a + b } else { a - b };
                        if result.is_nan() {
                            "nan".to_string()
                        } else {
                            result.to_string()
                        }
                    } else {
                        let (a, b) = (a.as_i128()?, b.as_i128()?);
                        (if name == "+" {
                            a.checked_add(b)?
                        } else {
                            a.checked_sub(b)?
                        })
                        .to_string()
                    };
                    // an integer result outside its type is an overflow, which the bounds cannot tell
                    crate::execution::parquet::try_cast_value(&Value::varchar(text), ty)
                };
                match (combine(low.0, low.1), combine(high.0, high.1)) {
                    (Some(min), Some(max)) => Range::Bounds(min, max),
                    _ => Range::Unknown,
                }
            }
            ("+" | "-" | "*" | "/" | "//", [left, right]) => {
                let (varying, increasing) = match (constant(left), constant(right)) {
                    (None, Some(c)) => match (name, sign(&c)) {
                        ("+" | "-", _) => (0, true),
                        ("*" | "/" | "//", Some(Ordering::Greater)) => (0, true),
                        ("*" | "/" | "//", Some(Ordering::Less)) => (0, false),
                        _ => return Range::Unknown,
                    },
                    (Some(c), None) => match (name, sign(&c)) {
                        ("+", _) => (1, true),
                        ("-", _) => (1, false),
                        ("*", Some(Ordering::Greater)) => (1, true),
                        ("*", Some(Ordering::Less)) => (1, false),
                        // a constant that isn't negative over divisors that are all positive, or all negative: the
                        // larger the divisor, the smaller the quotient
                        ("/" | "//", Some(Ordering::Greater | Ordering::Equal)) => {
                            let Range::Bounds(min, max) = self.range(right) else {
                                return Range::Unknown;
                            };
                            if sign(&min) != Some(Ordering::Greater) && sign(&max) != Some(Ordering::Less) {
                                return Range::Unknown;
                            }
                            (1, false)
                        }
                        _ => return Range::Unknown,
                    },
                    _ => return Range::Unknown,
                };
                self.monotone(expr, varying, increasing)
            }
            // `DatePart::MonthOperator` and the like: within one year, the parts of the bounds
            ("month" | "quarter" | "dayofyear" | "doy" | "day" | "dayofmonth", [child]) => match self.range(child) {
                Range::Bounds(min, max) => {
                    // the year of a DATE or TIMESTAMP is the start of its text, the month follows it
                    let within = if matches!(name, "day" | "dayofmonth") { 7 } else { 4 };
                    let year = |value: &Value| {
                        value
                            .try_to_string()
                            .ok()
                            .map(|text| text.chars().take(within).collect::<String>())
                    };
                    if year(&min).is_some() && year(&min) == year(&max) {
                        self.monotone(expr, 0, true)
                    } else {
                        Range::Unknown
                    }
                }
                other => other,
            },
            // a deterministic function of what is one value in the group: that function of it
            _ if !expr.is_volatile() && !children.is_empty() => {
                // a column of the group with one value and no NULL, or a constant (a NULL in the group
                // would give the function of NULL too)
                let mut arguments = Vec::with_capacity(children.len());
                for child in children {
                    match &child.kind {
                        BoundExprKind::Constant(_) => arguments.push(child.clone()),
                        BoundExprKind::Reference(index) => match self.zone(*index) {
                            Some(ColumnZone {
                                min: Some(min),
                                max: Some(max),
                                has_null,
                                ..
                            }) if (!has_null || !SPECIAL_NULL_HANDLING.contains(&name))
                                && compare_values(ComparisonOp::Equal, &min, &max) == Some(true)
                                // C++ keeps 12 bytes of a string bound, which do not tell a longer one
                                && min.try_to_string().map_or(true, |text| {
                                    min.logical_type().id() != LogicalTypeId::Varchar || text.len() <= STRING_STATISTICS_BYTES
                                })
                                // the sign of a floating point zero is not in the statistics
                                && !matches!(min.data(), duckrs_common::value::ValueData::Float(f) if *f == 0.0)
                                && !matches!(min.data(), duckrs_common::value::ValueData::Double(d) if *d == 0.0) =>
                            {
                                arguments.push(BoundExpr::constant(min))
                            }
                            // a group of only NULLs
                            Some(ColumnZone {
                                min: None,
                                max: None,
                                has_null: true,
                                ..
                            }) => arguments.push(BoundExpr::constant(Value::null(child.return_type.clone()))),
                            _ => return Range::Unknown,
                        },
                        _ => return Range::Unknown,
                    }
                }
                let BoundExprKind::Function {
                    function, is_operator, ..
                } = &expr.kind
                else {
                    return Range::Unknown;
                };
                let call = BoundExpr::new(
                    BoundExprKind::Function {
                        function: function.clone(),
                        children: arguments,
                        is_operator: *is_operator,
                    },
                    expr.return_type.clone(),
                );
                match crate::execution::expression_executor::try_evaluate_scalar(&call) {
                    Some(value) if value.is_null() => Range::Null,
                    Some(value) => Range::Bounds(value.clone(), value),
                    None => Range::Unknown,
                }
            }
            _ => Range::Unknown,
        }
    }

    /// The range of `expr`, whose child `varying` is the one that changes, in or against the order of its values.
    fn monotone(&self, expr: &BoundExpr, varying: usize, increasing: bool) -> Range {
        let BoundExprKind::Function { children, .. } = &expr.kind else {
            return Range::Unknown;
        };
        let at = |value: &Value| {
            let mut call = expr.clone();
            if let BoundExprKind::Function { children, .. } = &mut call.kind {
                children[varying] = BoundExpr::constant(value.clone());
            }
            try_evaluate_scalar(&call).filter(|result| !result.is_null())
        };
        match self.range(&children[varying]) {
            Range::Bounds(min, max) => match (at(&min), at(&max)) {
                (Some(low), Some(high)) if increasing => Range::Bounds(low, high),
                (Some(low), Some(high)) => Range::Bounds(high, low),
                _ => Range::Unknown,
            },
            other => other,
        }
    }

    /// The range of `expr`, a function of `child` that keeps the order of values.
    fn through(&self, expr: &BoundExpr, child: &BoundExpr) -> Range {
        let at = |value: &Value| {
            let mut call = expr.clone();
            for slot in call.children_mut() {
                *slot = BoundExpr::constant(value.clone());
            }
            try_evaluate_scalar(&call).filter(|result| !result.is_null())
        };
        match self.range(child) {
            Range::Bounds(min, max) => match (at(&min), at(&max)) {
                (Some(min), Some(max)) => Range::Bounds(min, max),
                _ => Range::Unknown,
            },
            other => other,
        }
    }

    /// Whether no row of the group can make `expr` true.
    fn never_true(&self, expr: &BoundExpr) -> bool {
        match &expr.kind {
            // `BarrierStats`: the statistics of what is in it
            BoundExprKind::Function { function, children, .. }
                if function.name == "__internal_barrier" && children.len() == 1 =>
            {
                self.never_true(&children[0])
            }
            // a BOOLEAN column itself: no row of the group is true
            BoundExprKind::Reference(index) if expr.return_type.id() == LogicalTypeId::Boolean => {
                self.zone(*index).is_some_and(|zone| match &zone.max {
                    None => true,
                    Some(max) => max.data() == &duckrs_common::value::ValueData::Bool(false),
                })
            }
            // an IN list as its comparisons
            BoundExprKind::Operator {
                op: op @ (BoundOperator::In | BoundOperator::NotIn),
                children,
            } if children.len() > 1 => self.never_true(&crate::planner::expression::in_as_comparisons(
                *op == BoundOperator::NotIn,
                children,
            )),
            BoundExprKind::Operator {
                op: BoundOperator::Not,
                children,
            } if children.len() == 1
                && matches!(&children[0].kind, BoundExprKind::Operator { op: BoundOperator::In, children } if children.len() > 1) =>
            {
                let BoundExprKind::Operator { children, .. } = &children[0].kind else {
                    unreachable!("checked")
                };
                self.never_true(&crate::planner::expression::in_as_comparisons(true, children))
            }
            // `NOT prefix(s, 'ab')`: every string of the group starts with it (a NULL gives NULL)
            BoundExprKind::Operator {
                op: BoundOperator::Not,
                children,
            } if children.len() == 1
                && matches!(&children[0].kind, BoundExprKind::Function { function, children, .. }
                    if matches!(function.name.as_str(), "prefix" | "starts_with" | "^@") && children.len() == 2) =>
            {
                let BoundExprKind::Function { children, .. } = &children[0].kind else {
                    unreachable!("checked")
                };
                let BoundExprKind::Constant(prefix) = &children[1].kind else {
                    return false;
                };
                let (Ok(prefix), Range::Bounds(min, max)) = (prefix.try_to_string(), self.range(&children[0])) else {
                    return false;
                };
                let (Ok(min), Ok(max)) = (min.try_to_string(), max.try_to_string()) else {
                    return false;
                };
                // C++'s string statistics keep 12 bytes of the bounds, which prove no longer prefix
                prefix.len() <= STRING_STATISTICS_BYTES && min.starts_with(&prefix) && max.starts_with(&prefix)
            }
            // `NOT f(..)` of a BOOLEAN function that is true for every value of the group (NULL stays NULL)
            BoundExprKind::Operator {
                op: BoundOperator::Not,
                children,
            } if children.len() == 1
                && matches!(children[0].kind, BoundExprKind::Function { .. })
                && children[0].return_type.id() == LogicalTypeId::Boolean
                && matches!(self.range(&children[0]), Range::Bounds(ref min, _) if min.data() == &duckrs_common::value::ValueData::Bool(true)) =>
            {
                true
            }
            BoundExprKind::Operator {
                op: BoundOperator::Not,
                children,
            } if children.len() == 1 && negated(without_noop_casts(&children[0])).is_some() => {
                self.never_true(&negated(without_noop_casts(&children[0])).expect("checked"))
            }
            BoundExprKind::Conjunction { and: true, children } => {
                children.iter().any(|c| self.never_true(c)) || self.excludes_every_value(children)
            }
            BoundExprKind::Conjunction { and: false, children } => children.iter().all(|c| self.never_true(c)),
            // `(x > 5) = true`: the comparison itself
            BoundExprKind::Comparison {
                op: ComparisonOp::Equal | ComparisonOp::NotDistinctFrom,
                left,
                right,
            } if left.return_type.id() == LogicalTypeId::Boolean
                && matches!(
                    without_noop_casts(left).kind,
                    BoundExprKind::Comparison { .. }
                        | BoundExprKind::Between { .. }
                        | BoundExprKind::Conjunction { .. }
                        | BoundExprKind::Reference(_)
                )
                && matches!(&right.kind, BoundExprKind::Constant(value) if value.data() == &duckrs_common::value::ValueData::Bool(true)) =>
            {
                self.never_true(without_noop_casts(left))
            }
            // `(x > 5) = false` and `(x > 5) IS FALSE`: the comparison never false
            BoundExprKind::Comparison {
                op: ComparisonOp::Equal | ComparisonOp::NotDistinctFrom,
                left,
                right,
            } if left.return_type.id() == LogicalTypeId::Boolean
                && matches!(&right.kind, BoundExprKind::Constant(value) if value.data() == &duckrs_common::value::ValueData::Bool(false))
                && !matches!(without_noop_casts(left).kind, BoundExprKind::Reference(_))
                && negated(without_noop_casts(left)).is_some() =>
            {
                self.never_true(&negated(without_noop_casts(left)).expect("checked"))
            }
            // `x IS DISTINCT FROM c`: every row of the group is `c`
            BoundExprKind::Comparison {
                op: ComparisonOp::DistinctFrom,
                left,
                right,
            } => match (&left.kind, &right.kind) {
                (_, BoundExprKind::Constant(constant)) | (BoundExprKind::Constant(constant), _)
                    if !constant.is_null() =>
                {
                    let column = if matches!(left.kind, BoundExprKind::Constant(_)) {
                        right
                    } else {
                        left
                    };
                    let zone = match &column.kind {
                        BoundExprKind::Reference(index) => self.zone(*index),
                        _ => None,
                    };
                    let is = |a: &Value| compare_values(ComparisonOp::Equal, a, constant) == Some(true);
                    zone.is_some_and(|zone| {
                        !zone.has_null && zone.min.as_ref().is_some_and(is) && zone.max.as_ref().is_some_and(is)
                    })
                }
                _ => false,
            },
            // `x IS NOT DISTINCT FROM NULL`: a group without NULLs
            BoundExprKind::Comparison {
                op: ComparisonOp::NotDistinctFrom,
                left,
                right,
            } if matches!(&right.kind, BoundExprKind::Constant(value) if value.is_null())
                && matches!(left.kind, BoundExprKind::Reference(_)) =>
            {
                let BoundExprKind::Reference(index) = left.kind else {
                    unreachable!("checked")
                };
                self.zone(index).is_some_and(|zone| !zone.has_null)
            }
            BoundExprKind::Comparison { op, left, right } => match (&left.kind, &right.kind) {
                // `IS NOT DISTINCT FROM` a value is `=` it, but for NULLs
                (_, BoundExprKind::Constant(constant))
                    if *op == ComparisonOp::NotDistinctFrom && !constant.is_null() =>
                {
                    self.comparison_fails(ComparisonOp::Equal, left, constant)
                }
                (_, BoundExprKind::Constant(constant)) => self.comparison_fails(*op, left, constant),
                (BoundExprKind::Constant(constant), _) => self.comparison_fails(flipped(*op), right, constant),
                // two expressions of the columns: their ranges (`PropagateComparison`)
                _ => self.ranges_fail(*op, left, right),
            },
            // `constant_or_null(false, ..)` is false or NULL
            BoundExprKind::Function { function, children, .. }
                if function.name == "constant_or_null"
                    && matches!(&children[0].kind, BoundExprKind::Constant(v) if v.data() == &duckrs_common::value::ValueData::Bool(false)) =>
            {
                true
            }
            // a BOOLEAN function whose range in the group is only false or NULL
            BoundExprKind::Function { function, .. }
                if expr.return_type.id() == LogicalTypeId::Boolean
                    && !matches!(function.name.as_str(), "prefix" | "starts_with" | "^@")
                    && matches!(self.range(expr), Range::Null | Range::Bounds(_, _))
                    && match self.range(expr) {
                        Range::Null => true,
                        Range::Bounds(_, max) => max.data() == &duckrs_common::value::ValueData::Bool(false),
                        Range::Unknown => false,
                    } =>
            {
                true
            }
            // `prefix(s, 'ab')` / `starts_with`: s from 'ab' up to what follows all strings that start with it
            BoundExprKind::Function { function, children, .. }
                if matches!(function.name.as_str(), "prefix" | "starts_with" | "^@") && children.len() == 2 =>
            {
                let BoundExprKind::Constant(prefix) = &children[1].kind else {
                    return false;
                };
                let Ok(prefix) = prefix.try_to_string() else {
                    return false;
                };
                if prefix.is_empty() {
                    return false;
                }
                let Range::Bounds(min, max) = self.range(&children[0]) else {
                    return matches!(self.range(&children[0]), Range::Null);
                };
                let (Ok(min), Ok(max)) = (min.try_to_string(), max.try_to_string()) else {
                    return false;
                };
                let (min, max, prefix) = (min.as_bytes(), max.as_bytes(), prefix.as_bytes());
                // every string of the group sorts after the prefix's range, or before it
                let after = min > prefix && !min.starts_with(prefix);
                let before = max < prefix;
                after || before
            }
            BoundExprKind::Between { input, lower, upper } => {
                let bound = |bound: &BoundExpr, op| match &bound.kind {
                    BoundExprKind::Constant(constant) => self.comparison_fails(op, input, constant),
                    _ => false,
                };
                bound(lower, ComparisonOp::GreaterThanOrEqual) || bound(upper, ComparisonOp::LessThanOrEqual)
            }
            BoundExprKind::Operator { op, children } => {
                let zone = match children.first().map(|c| &c.kind) {
                    Some(BoundExprKind::Reference(index)) => self.zone(*index),
                    _ => None,
                };
                match (op, zone) {
                    (BoundOperator::IsNull, Some(zone)) => !zone.has_null,
                    (BoundOperator::IsNotNull, Some(zone)) => zone.min.is_none() && zone.elements.is_none(),
                    // a function that is only NULL in the group
                    (BoundOperator::IsNotNull, None) if children.len() == 1 => {
                        matches!(children[0].kind, BoundExprKind::Function { .. })
                            && matches!(self.range(&children[0]), Range::Null)
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// Whether `left op right` can not hold for any two values of their ranges.
    fn ranges_fail(&self, op: ComparisonOp, left: &BoundExpr, right: &BoundExpr) -> bool {
        // `StringStats`: of the bounds of strings C++ keeps a prefix, and prefixes that are the same
        // where one was cut decide nothing
        let cut = |value: &Value| match value.data() {
            duckrs_common::value::ValueData::Bytes(bytes) if bytes.len() > STRING_STATISTICS_BYTES => {
                Some(bytes[..STRING_STATISTICS_BYTES].to_vec())
            }
            _ => None,
        };
        let is = |op, a: &Value, b: &Value| {
            let (cut_a, cut_b) = (cut(a), cut(b));
            if cut_a.is_some() || cut_b.is_some() {
                let bytes = |v: &Value| match v.data() {
                    duckrs_common::value::ValueData::Bytes(bytes) => Some(bytes.clone()),
                    _ => None,
                };
                let (Some(x), Some(y)) = (bytes(a), bytes(b)) else {
                    return false;
                };
                let (x, y) = (x.as_slice(), y.as_slice());
                let x = cut_a.as_deref().unwrap_or(x);
                let y = cut_b.as_deref().unwrap_or(y);
                if x == y {
                    return false;
                }
                let order = x.cmp(y);
                return match op {
                    ComparisonOp::LessThan | ComparisonOp::LessThanOrEqual => order.is_lt(),
                    ComparisonOp::GreaterThan | ComparisonOp::GreaterThanOrEqual => order.is_gt(),
                    _ => false,
                };
            }
            compare_values(op, a, b) == Some(true)
        };
        let (Range::Bounds(l_min, l_max), Range::Bounds(r_min, r_max)) = (self.range(left), self.range(right)) else {
            return false;
        };
        match op {
            ComparisonOp::Equal => {
                is(ComparisonOp::LessThan, &l_max, &r_min) || is(ComparisonOp::GreaterThan, &l_min, &r_max)
            }
            ComparisonOp::NotEqual => {
                is(ComparisonOp::Equal, &l_min, &l_max)
                    && is(ComparisonOp::Equal, &r_min, &r_max)
                    && is(ComparisonOp::Equal, &l_min, &r_min)
            }
            ComparisonOp::LessThan => is(ComparisonOp::GreaterThanOrEqual, &l_min, &r_max),
            ComparisonOp::LessThanOrEqual => is(ComparisonOp::GreaterThan, &l_min, &r_max),
            ComparisonOp::GreaterThan => is(ComparisonOp::LessThanOrEqual, &l_max, &r_min),
            ComparisonOp::GreaterThanOrEqual => is(ComparisonOp::LessThan, &l_max, &r_min),
            _ => false,
        }
    }

    /// Whether `expr op constant` can not hold in the group.
    fn comparison_fails(&self, op: ComparisonOp, expr: &BoundExpr, constant: &Value) -> bool {
        let is = |op, a: &Value, b: &Value| compare_values(op, a, b) == Some(true);
        match (op, self.range(expr)) {
            (ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom, _) => false,
            (_, _) if constant.is_null() => true,
            (_, Range::Unknown) => false,
            (_, Range::Null) => true,
            (ComparisonOp::Equal, Range::Bounds(min, max)) => {
                is(ComparisonOp::GreaterThan, &min, constant) || is(ComparisonOp::LessThan, &max, constant)
            }
            (ComparisonOp::NotEqual, Range::Bounds(min, max)) => {
                is(ComparisonOp::Equal, &min, constant) && is(ComparisonOp::Equal, &max, constant)
            }
            (ComparisonOp::LessThan, Range::Bounds(min, _)) => is(ComparisonOp::GreaterThanOrEqual, &min, constant),
            (ComparisonOp::LessThanOrEqual, Range::Bounds(min, _)) => is(ComparisonOp::GreaterThan, &min, constant),
            (ComparisonOp::GreaterThan, Range::Bounds(_, max)) => is(ComparisonOp::LessThanOrEqual, &max, constant),
            (ComparisonOp::GreaterThanOrEqual, Range::Bounds(_, max)) => is(ComparisonOp::LessThan, &max, constant),
        }
    }
}

/// A timestamp of any unit, without a time zone.
fn is_timestamp(ty: &LogicalType) -> bool {
    use LogicalTypeId::*;
    matches!(ty.id(), TimestampSec | TimestampMs | Timestamp | TimestampNs)
}

/// A timestamp with a time zone, of any unit.
fn is_timestamp_tz(ty: &LogicalType) -> bool {
    matches!(ty.id(), LogicalTypeId::TimestampTz | LogicalTypeId::TimestampTzNs)
}

/// `expr` without the casts around it that change nothing.
fn without_noop_casts(mut expr: &BoundExpr) -> &BoundExpr {
    while let BoundExprKind::Cast { child, .. } = &expr.kind
        && child.return_type == expr.return_type
    {
        expr = child;
    }
    expr
}

/// `NOT expr` for a comparison of values (true, false or NULL as `expr` is), or a conjunction of them; `None` for
/// anything else.
fn negated(expr: &BoundExpr) -> Option<BoundExpr> {
    let kind = match &expr.kind {
        BoundExprKind::Comparison { op, left, right } => {
            let op = match op {
                ComparisonOp::Equal => ComparisonOp::NotEqual,
                ComparisonOp::NotEqual => ComparisonOp::Equal,
                ComparisonOp::LessThan => ComparisonOp::GreaterThanOrEqual,
                ComparisonOp::GreaterThanOrEqual => ComparisonOp::LessThan,
                ComparisonOp::GreaterThan => ComparisonOp::LessThanOrEqual,
                ComparisonOp::LessThanOrEqual => ComparisonOp::GreaterThan,
                _ => return None,
            };
            BoundExprKind::Comparison {
                op,
                left: left.clone(),
                right: right.clone(),
            }
        }
        BoundExprKind::Reference(_) if expr.return_type.id() == LogicalTypeId::Boolean => BoundExprKind::Comparison {
            op: ComparisonOp::Equal,
            left: Box::new(expr.clone()),
            right: Box::new(BoundExpr::constant(Value::boolean(false))),
        },
        BoundExprKind::Conjunction { and, children } => BoundExprKind::Conjunction {
            and: !and,
            children: children.iter().map(negated).collect::<Option<Vec<_>>>()?,
        },
        _ => return None,
    };
    Some(BoundExpr::new(kind, expr.return_type.clone()))
}

/// `a op b` as `b op a`.
fn flipped(op: ComparisonOp) -> ComparisonOp {
    match op {
        ComparisonOp::LessThan => ComparisonOp::GreaterThan,
        ComparisonOp::GreaterThan => ComparisonOp::LessThan,
        ComparisonOp::LessThanOrEqual => ComparisonOp::GreaterThanOrEqual,
        ComparisonOp::GreaterThanOrEqual => ComparisonOp::LessThanOrEqual,
        other => other,
    }
}

/// Whether a cast of `source` to `target` keeps the order of values and cannot fail on the ones
/// a filter in a scan gets to see, which `can_throw` lets through.
fn keeps_order(source: &LogicalType, target: &LogicalType) -> bool {
    use LogicalTypeId::*;
    (source.is_numeric() && target.is_numeric()) || matches!((source.id(), target.id()), (Date, Timestamp))
}

/// Whether the rows of row group `group` of the table all fail `filter`.
pub fn group_never_true(
    table: &TableCatalogEntry,
    column_ids: &[usize],
    group: usize,
    rows: usize,
    filter: &BoundExpr,
) -> bool {
    let zone = |index| table_zone(table, column_ids, group, rows, index);
    never_true(&zone, filter)
}

/// Per row group of the table, whether a scan with `filter` must read it.
pub fn groups_to_scan(
    table: &TableCatalogEntry,
    column_ids: &[usize],
    filter: Option<&BoundExpr>,
    rows: usize,
) -> Vec<bool> {
    let groups = row_group_count(table, rows);
    let Some(filter) = filter else {
        return vec![true; groups];
    };
    if groups < 2 {
        return vec![true; groups];
    }
    (0..groups)
        .map(|group| {
            let zone = |index| table_zone(table, column_ids, group, rows, index);
            !never_true(&zone, filter)
        })
        .collect()
}
