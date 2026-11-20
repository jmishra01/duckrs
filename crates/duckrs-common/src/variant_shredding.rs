// Ported from duckdb/src/storage/table/variant/variant_shredding.cpp,
// duckdb/src/function/variant/variant_shredding.cpp and
// duckdb/src/common/types/variant/variant_iterator.cpp @ rust-port-baseline
//
//! Shredded VARIANT columns: per row group, the values are split into a typed part, of a type
//! inferred from them (or forced), and what does not fit it, which is stored unshredded.
//!
//! The typed part of a value of type `T` is `T` itself when every value has that type (or is NULL),
//! or else `STRUCT(typed_value T, untyped_value_index UINTEGER)`: the index (1-based) of where in the
//! row's unshredded values the value is, 0 when it is a missing field of an object, NULL when the value
//! is a VARIANT NULL or entirely in `typed_value`. Objects shred to a STRUCT of their fields, the fields
//! they have besides these being a leftover object; arrays shred to a LIST.

use std::collections::BTreeMap;

use crate::types::{LogicalType, LogicalTypeId, PhysicalType};
use crate::value::{Value, ValueData};
use crate::variant::{unshredded_row, UnshreddedRow, VariantNode};
use crate::{Error, ErrorKind, Result};

/// `VariantVisitor::MAX_RECURSION_DEPTH`.
const MAX_RECURSION_DEPTH: usize = 128;
/// `VariantLogicalType::ENUM_SIZE`.
const TYPE_COUNT: usize = 35;
const DECIMAL: usize = 15;
const OBJECT: usize = 29;
const ARRAY: usize = 30;

/// `VariantColumnStatsData`: what one position of the values holds.
struct ColumnStats {
    type_counts: [u64; TYPE_COUNT],
    total_count: u64,
    decimal_width: u8,
    decimal_scale: u8,
    decimal_consistent: bool,
    fields: BTreeMap<String, usize>,
    element: Option<usize>,
}

impl Default for ColumnStats {
    fn default() -> Self {
        Self {
            type_counts: [0; TYPE_COUNT],
            total_count: 0,
            decimal_width: 0,
            decimal_scale: 0,
            decimal_consistent: false,
            fields: BTreeMap::new(),
            element: None,
        }
    }
}

/// `VariantShreddingStats`: the positions of the values, the root first.
pub struct ShreddingStats {
    columns: Vec<ColumnStats>,
    too_deep: bool,
}

impl Default for ShreddingStats {
    fn default() -> Self {
        Self {
            columns: vec![ColumnStats::default()],
            too_deep: false,
        }
    }
}

impl ShreddingStats {
    /// `Update`, of the values of a row group; `None` is a NULL row.
    pub fn update<'a>(&mut self, rows: impl IntoIterator<Item = Option<&'a VariantNode>>) {
        for row in rows {
            self.visit(row.unwrap_or(&VariantNode::Null), 0, 0);
        }
    }

    fn visit(&mut self, node: &VariantNode, column: usize, depth: usize) {
        let code = usize::from(node.type_code());
        let stats = &mut self.columns[column];
        stats.type_counts[code] += 1;
        stats.total_count += 1;
        match node {
            VariantNode::Scalar(value) if code == DECIMAL => {
                let (width, scale) = value.logical_type().decimal_properties().unwrap_or((18, 3));
                if stats.type_counts[DECIMAL] == 1 {
                    (stats.decimal_width, stats.decimal_scale) = (width, scale);
                    stats.decimal_consistent = true;
                } else if stats.decimal_consistent && (width, scale) != (stats.decimal_width, stats.decimal_scale) {
                    stats.decimal_consistent = false;
                }
            }
            VariantNode::Array(items) => {
                if self.children_too_deep(items.len(), depth) {
                    return;
                }
                let element = match self.columns[column].element {
                    Some(element) => element,
                    None => {
                        self.columns.push(ColumnStats::default());
                        let element = self.columns.len() - 1;
                        self.columns[column].element = Some(element);
                        element
                    }
                };
                for item in items {
                    self.visit(item, element, depth + 1);
                }
            }
            VariantNode::Object(entries) => {
                if self.children_too_deep(entries.len(), depth) {
                    return;
                }
                for (key, child) in entries {
                    let field = match self.columns[column].fields.get(key) {
                        Some(&field) => field,
                        None => {
                            self.columns.push(ColumnStats::default());
                            let field = self.columns.len() - 1;
                            self.columns[column].fields.insert(key.clone(), field);
                            field
                        }
                    };
                    self.visit(child, field, depth + 1);
                }
            }
            _ => {}
        }
    }

    fn children_too_deep(&mut self, children: usize, depth: usize) -> bool {
        if children == 0 || depth + 1 < MAX_RECURSION_DEPTH {
            return false;
        }
        self.too_deep = true;
        true
    }

    /// `GetShreddedType`: the type of the typed part, if the values have one.
    pub fn shredded_type(&self, force_partial: bool) -> Option<LogicalType> {
        if self.too_deep {
            return None;
        }
        self.column_type(0, None, force_partial)
    }

    /// `GetShreddedTypeInternal`.
    fn column_type(&self, column: usize, parent_count: Option<u64>, force_partial: bool) -> Option<LogicalType> {
        let stats = &self.columns[column];
        let total = parent_count.unwrap_or(stats.total_count);
        let null_count = stats.type_counts[0];
        if null_count == stats.total_count {
            // all NULL: INTEGER
            return Some(wrap(LogicalType::INTEGER, !force_partial && null_count == total));
        }
        let mut max_count = 0;
        let mut type_index = 0;
        for i in 1..TYPE_COUNT {
            if i == DECIMAL && !stats.decimal_consistent {
                continue;
            }
            let count = stats.type_counts[i] + null_count;
            if max_count == 0 || count > max_count {
                max_count = count;
                type_index = i;
            }
        }
        if max_count == 0 {
            return None;
        }
        let consistent = !force_partial && max_count == total;
        match type_index {
            OBJECT => {
                let mut fields = Vec::new();
                let mut names = std::collections::HashSet::new();
                for (name, &child) in &stats.fields {
                    if name.is_empty() {
                        continue;
                    }
                    // STRUCT field names are case-insensitive, object keys are not
                    if !names.insert(name.to_lowercase()) {
                        return None;
                    }
                    if let Some(ty) = self.column_type(child, Some(total), force_partial) {
                        fields.push((name.clone(), ty));
                    }
                }
                if fields.is_empty() {
                    return None;
                }
                // objects are never fully consistent
                Some(wrap(LogicalType::struct_type(fields), false))
            }
            ARRAY => {
                let element = self.column_type(stats.element?, None, force_partial)?;
                Some(wrap(LogicalType::list(element), consistent))
            }
            DECIMAL => Some(wrap(
                LogicalType::decimal(stats.decimal_width, stats.decimal_scale),
                consistent,
            )),
            code => Some(wrap(LogicalType::new(scalar_type(code as u8)?), consistent)),
        }
    }
}

/// `SetShreddedType`.
fn wrap(typed: LogicalType, consistent: bool) -> LogicalType {
    if consistent && !typed.is_nested() {
        return typed;
    }
    let mut fields = vec![("typed_value".to_string(), typed)];
    if !consistent {
        fields.push(("untyped_value_index".to_string(), LogicalType::UINTEGER));
    }
    LogicalType::struct_type(fields)
}

/// `VariantStats::GetShreddingType`: the typed part a forced type gives, each level able to hold what does
/// not fit.
pub fn forced_shredding_type(ty: &LogicalType) -> LogicalType {
    let typed = match ty.id() {
        LogicalTypeId::List => LogicalType::list(forced_shredding_type(ty.child_type().expect("a list"))),
        LogicalTypeId::Struct => LogicalType::struct_type(
            ty.struct_children()
                .iter()
                .map(|(name, child)| (name.clone(), forced_shredding_type(child)))
                .collect(),
        ),
        _ => ty.clone(),
    };
    LogicalType::struct_type(vec![
        ("typed_value".to_string(), typed),
        ("untyped_value_index".to_string(), LogicalType::UINTEGER),
    ])
}

/// `ProduceShreddedType`: the SQL type of a scalar `VariantLogicalType`.
fn scalar_type(code: u8) -> Option<LogicalTypeId> {
    use LogicalTypeId::*;
    Some(match code {
        1 | 2 => Boolean,
        3 => TinyInt,
        4 => SmallInt,
        5 => Integer,
        6 => BigInt,
        7 => HugeInt,
        8 => UTinyInt,
        9 => USmallInt,
        10 => UInteger,
        11 => UBigInt,
        12 => UHugeInt,
        13 => Float,
        14 => Double,
        16 => Varchar,
        17 => Blob,
        18 => Uuid,
        19 => Date,
        20 => Time,
        21 => TimeNs,
        22 => TimestampSec,
        23 => TimestampMs,
        24 => Timestamp,
        25 => TimestampNs,
        26 => TimeTz,
        27 => TimestampTz,
        28 => Interval,
        31 => Bignum,
        32 => Bit,
        33 => Geometry,
        34 => TimestampTzNs,
        _ => return None,
    })
}

/// The physical type of the unscaled value of a DECIMAL of `width`.
fn decimal_physical(width: u8) -> PhysicalType {
    match width {
        0..=4 => PhysicalType::Int16,
        5..=9 => PhysicalType::Int32,
        10..=18 => PhysicalType::Int64,
        _ => PhysicalType::Int128,
    }
}

/// `ValueIsShredded`: whether the typed part of `typed` holds `node`.
fn fits(node: &VariantNode, typed: &LogicalType) -> bool {
    match (node, typed.id()) {
        (VariantNode::Object(_), LogicalTypeId::Struct) => true,
        (VariantNode::Array(_), LogicalTypeId::List) => true,
        (VariantNode::Scalar(value), LogicalTypeId::Decimal) => {
            let Some((width, scale)) = value.logical_type().decimal_properties() else {
                return false;
            };
            let (typed_width, typed_scale) = typed.decimal_properties().expect("a DECIMAL");
            typed.physical_type() == decimal_physical(width) && scale == typed_scale && width <= typed_width
        }
        (VariantNode::Scalar(value), id) => value.logical_type().id() == id && id != LogicalTypeId::Decimal,
        _ => false,
    }
}

fn shred_error(message: &str) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

/// The shredding of one row: its leftover values, and where the next one starts.
struct RowShredder {
    leftovers: Vec<VariantNode>,
    next_index: u32,
}

impl RowShredder {
    /// The 1-based index the leftover `node` gets.
    fn leftover(&mut self, node: VariantNode) -> u32 {
        let index = self.next_index + 1;
        self.next_index += node.value_count();
        self.leftovers.push(node);
        index
    }

    /// The typed part of `node` (`None`: a missing field) as a value of `ty`.
    fn shred(&mut self, node: Option<&VariantNode>, ty: &LogicalType) -> Result<Value> {
        if ty.id() != LogicalTypeId::Struct {
            // fully consistent: the value itself, or NULL
            return match node {
                None => Err(Error::new(
                    ErrorKind::Internal,
                    "Field is missing but untyped_value_index is not set",
                )),
                Some(VariantNode::Null) => Ok(Value::null(ty.clone())),
                Some(node) if fits(node, ty) => self.typed(node, ty),
                Some(_) => Err(shred_error(
                    "Failed to shred variant value - untyped_value was not set but inconsistent values were found",
                )),
            };
        }
        let fields = ty.struct_children();
        let typed_type = &fields[0].1;
        let has_untyped = fields.len() > 1;
        let (typed, untyped) = match node {
            None => {
                if !has_untyped {
                    return Err(Error::new(
                        ErrorKind::Internal,
                        "Field is missing but untyped_value_index is not set",
                    ));
                }
                (Value::null(typed_type.clone()), Value::uinteger(0))
            }
            Some(VariantNode::Null) => (Value::null(typed_type.clone()), Value::null(LogicalType::UINTEGER)),
            Some(node) if fits(node, typed_type) => {
                let typed = self.typed(node, typed_type)?;
                let mut untyped = Value::null(LogicalType::UINTEGER);
                if let (VariantNode::Object(entries), LogicalTypeId::Struct) = (node, typed_type.id()) {
                    let shredded = typed_type.struct_children();
                    let rest: Vec<(String, VariantNode)> = entries
                        .iter()
                        .filter(|(key, _)| !shredded.iter().any(|(name, _)| name == key))
                        .cloned()
                        .collect();
                    if !rest.is_empty() {
                        if !has_untyped {
                            return Err(shred_error(
                                "Failed to shred variant value - untyped_value was not set but inconsistent values were found",
                            ));
                        }
                        untyped = Value::uinteger(self.leftover(VariantNode::Object(rest)));
                    }
                }
                (typed, untyped)
            }
            Some(node) => {
                if !has_untyped {
                    return Err(shred_error(
                        "Failed to shred variant value - untyped_value was not set but inconsistent values were found",
                    ));
                }
                let index = self.leftover(node.clone());
                (Value::null(typed_type.clone()), Value::uinteger(index))
            }
        };
        let mut values = vec![typed];
        if has_untyped {
            values.push(untyped);
        }
        Value::new(ty.clone(), ValueData::Struct(values))
    }

    /// `WriteTypedValues`: `node`, which fits `ty`, as a value of it.
    fn typed(&mut self, node: &VariantNode, ty: &LogicalType) -> Result<Value> {
        match node {
            VariantNode::Object(entries) => {
                let mut values = Vec::new();
                for (name, field) in ty.struct_children() {
                    let child = entries.iter().find(|(key, _)| key == name).map(|(_, child)| child);
                    values.push(self.shred(child, field)?);
                }
                Value::new(ty.clone(), ValueData::Struct(values))
            }
            VariantNode::Array(items) => {
                let element = ty.child_type().expect("a list");
                let items = items
                    .iter()
                    .map(|item| self.shred(Some(item), element))
                    .collect::<Result<Vec<_>>>()?;
                Value::list(element.clone(), items)
            }
            VariantNode::Scalar(value) if ty.id() == LogicalTypeId::Decimal => {
                let (width, scale) = ty.decimal_properties().expect("a DECIMAL");
                Value::decimal(value.as_i128().unwrap_or(0), width, scale)
            }
            VariantNode::Scalar(value) => Ok(value.clone()),
            VariantNode::Null => Ok(Value::null(ty.clone())),
        }
    }
}

/// `ShredVariantData`: a row (`None` is a NULL row) as its unshredded leftovers, a row of
/// [`crate::variant::unshredded_type`], and its typed part, a value of `ty`.
pub fn shred_row(node: Option<&VariantNode>, ty: &LogicalType) -> Result<(Value, Value)> {
    let Some(node) = node else {
        return Ok((unshredded_row(&[])?.0, Value::null(ty.clone())));
    };
    let mut row = RowShredder {
        leftovers: Vec::new(),
        next_index: 0,
    };
    let typed = row.shred(Some(node), ty)?;
    let roots: Vec<&VariantNode> = row.leftovers.iter().collect();
    let (unshredded, _) = unshredded_row(&roots)?;
    Ok((unshredded, typed))
}

/// What a level of the typed part resolves to (`VariantNode::ResolveShredded`).
enum Resolved {
    Missing,
    Node(VariantNode),
}

/// The value a shredded row stands for: its typed part (`typed`, of `ty`) together with its leftovers;
/// `None` for a NULL row.
pub fn unshred_row(unshredded: &Value, typed: &Value, ty: &LogicalType) -> Result<Option<VariantNode>> {
    if typed.is_null() {
        return Ok(None);
    }
    let row = UnshreddedRow::parse(unshredded)?;
    Ok(match resolve(row.as_ref(), typed, ty)? {
        Resolved::Node(VariantNode::Null) | Resolved::Missing => None,
        Resolved::Node(node) => Some(node),
    })
}

fn leftover_at(row: Option<&UnshreddedRow>, index: u32) -> Result<VariantNode> {
    let row = row.ok_or_else(|| {
        Error::new(
            ErrorKind::Serialization,
            "Corrupted VARIANT value: a shredded value refers to an unshredded value that is not there",
        )
    })?;
    row.node(index - 1)
}

fn resolve(row: Option<&UnshreddedRow>, value: &Value, ty: &LogicalType) -> Result<Resolved> {
    if ty.id() != LogicalTypeId::Struct {
        // a fully consistent level: NULL is a VARIANT NULL
        if value.is_null() {
            return Ok(Resolved::Node(VariantNode::Null));
        }
        return typed_node(row, value, ty, 0).map(Resolved::Node);
    }
    let ValueData::Struct(parts) = value.data() else {
        return Ok(Resolved::Node(VariantNode::Null));
    };
    let fields = ty.struct_children();
    let typed_type = &fields[0].1;
    let untyped = parts.get(1).filter(|_| fields.len() > 1);
    let untyped_index = untyped.and_then(|v| if v.is_null() { None } else { v.as_i128() });
    if !parts[0].is_null() {
        let overlay = if typed_type.id() == LogicalTypeId::Struct {
            untyped_index.unwrap_or(0) as u32
        } else {
            0
        };
        return typed_node(row, &parts[0], typed_type, overlay).map(Resolved::Node);
    }
    Ok(match untyped_index {
        None => Resolved::Node(VariantNode::Null),
        Some(0) => Resolved::Missing,
        Some(index) => Resolved::Node(leftover_at(row, index as u32)?),
    })
}

/// A valid typed value of `ty` as the value it is, an object together with its leftover fields at `overlay`.
fn typed_node(row: Option<&UnshreddedRow>, value: &Value, ty: &LogicalType, overlay: u32) -> Result<VariantNode> {
    match (ty.id(), value.data()) {
        (LogicalTypeId::Struct, ValueData::Struct(parts)) => {
            let mut entries = Vec::new();
            for ((name, field), part) in ty.struct_children().iter().zip(parts) {
                if let Resolved::Node(node) = resolve(row, part, field)? {
                    entries.push((name.clone(), node));
                }
            }
            if overlay != 0 {
                match leftover_at(row, overlay)? {
                    VariantNode::Object(ref mut rest) => entries.append(rest),
                    _ => {
                        return Err(Error::new(
                            ErrorKind::Serialization,
                            "Corrupted VARIANT value: the leftover fields of a shredded object are no object",
                        ))
                    }
                }
            }
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(VariantNode::Object(entries))
        }
        (LogicalTypeId::List, ValueData::List(items)) => {
            let element = ty.child_type().expect("a list");
            let mut nodes = Vec::with_capacity(items.len());
            for item in items {
                nodes.push(match resolve(row, item, element)? {
                    Resolved::Node(node) => node,
                    Resolved::Missing => VariantNode::Null,
                });
            }
            Ok(VariantNode::Array(nodes))
        }
        _ => Ok(VariantNode::Scalar(value.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(value: Value) -> VariantNode {
        VariantNode::from_value(&value).unwrap()
    }

    fn round_trip(rows: &[Option<VariantNode>]) -> LogicalType {
        let mut stats = ShreddingStats::default();
        stats.update(rows.iter().map(Option::as_ref));
        let ty = stats.shredded_type(false).expect("a shredded type");
        for row in rows {
            let (unshredded, typed) = shred_row(row.as_ref(), &ty).unwrap();
            let back = unshred_row(&unshredded, &typed, &ty).unwrap();
            let expected = match row {
                Some(VariantNode::Null) | None => None,
                other => other.clone(),
            };
            assert_eq!(back, expected, "{ty}");
        }
        ty
    }

    #[test]
    fn integers_flatten() {
        let rows: Vec<_> = (0..10).map(|i| Some(node(Value::integer(i)))).collect();
        assert_eq!(round_trip(&rows), LogicalType::INTEGER);
    }

    #[test]
    fn inconsistent_values_keep_their_leftovers() {
        let mut rows: Vec<_> = (0..10).map(|i| Some(node(Value::integer(i)))).collect();
        rows.push(Some(node(Value::varchar("hello"))));
        rows.push(None);
        rows.push(Some(VariantNode::Null));
        let ty = round_trip(&rows);
        assert_eq!(ty.to_string(), "STRUCT(typed_value INTEGER, untyped_value_index UINTEGER)");
    }

    #[test]
    fn objects_and_arrays() {
        let rows: Vec<_> = (0..6)
            .map(|i| {
                let mut fields = vec![
                    ("a".to_string(), Value::integer(i)),
                    (
                        "b".to_string(),
                        Value::list(LogicalType::BIGINT, vec![Value::bigint(i as i64), Value::bigint(1)]).unwrap(),
                    ),
                ];
                if i % 2 == 0 {
                    fields.push(("c".to_string(), Value::varchar("x")));
                }
                if i == 3 {
                    fields.push(("rare".to_string(), Value::boolean(true)));
                    fields.retain(|(name, _)| name != "a");
                    fields.push(("a".to_string(), Value::varchar("not an int")));
                }
                Some(node(Value::struct_value(fields)))
            })
            .chain([Some(node(Value::varchar("no object")))])
            .collect();
        round_trip(&rows);
    }
}
