//! Ported from duckdb/src/storage/table/variant/variant_shredding.cpp, duckdb/src/function/variant/
//! variant_shredding.cpp and duckdb/src/common/types/variant/variant_iterator.cpp @ rust-port-baseline.
//!
//! VARIANT shredding: what a checkpoint stores a VARIANT column as when its values have a common
//! shape. Next to the unshredded VARIANT, a "shredded" column holds the values that have the shape's
//! type, typed. Its type at each level is either that type, flattened, when every value there has it
//! (or is NULL), or `STRUCT(typed_value <type>, untyped_value_index UINTEGER)` when some don't: those
//! go to the unshredded part of the row, which `untyped_value_index` points into (1-based, 0 for a
//! missing object field). An OBJECT always has `untyped_value_index`, for the fields its type lacks.

use super::{Physical, VariantNode, corrupt_variant, unshredded_row};
use crate::types::{LogicalType, LogicalTypeId};
use crate::value::{Value, ValueData};
use crate::{Error, ErrorKind, Result};

/// `VariantVisitor::MAX_RECURSION_DEPTH`: values nested deeper keep their column unshredded.
const MAX_RECURSION_DEPTH: usize = 128;

/// `VariantLogicalType`, whose order decides between types of equal counts.
const VARIANT_NULL: usize = 0;
const BOOL_TRUE: usize = 1;
const BOOL_FALSE: usize = 2;
const DECIMAL: usize = 15;
const OBJECT: usize = 29;
const ARRAY: usize = 30;
const TYPE_COUNT: usize = 35;

/// `VariantLogicalType` of the scalars, by their position: the type they shred on is the inverse.
const SCALAR_CODES: [(LogicalTypeId, usize); 32] = {
    use LogicalTypeId::*;
    [
        (TinyInt, 3),
        (SmallInt, 4),
        (Integer, 5),
        (BigInt, 6),
        (HugeInt, 7),
        (UTinyInt, 8),
        (USmallInt, 9),
        (UInteger, 10),
        (UBigInt, 11),
        (UHugeInt, 12),
        (Float, 13),
        (Double, 14),
        (Decimal, DECIMAL),
        (Varchar, 16),
        (Blob, 17),
        (Uuid, 18),
        (Date, 19),
        (Time, 20),
        (TimeNs, 21),
        (TimestampSec, 22),
        (TimestampMs, 23),
        (Timestamp, 24),
        (TimestampNs, 25),
        (TimeTz, 26),
        (TimestampTz, 27),
        (Interval, 28),
        (Bignum, 31),
        (Bit, 32),
        (Geometry, 33),
        (TimestampTzNs, 34),
        // BOOLEAN takes two codes, by its value; listed for the inverse
        (Boolean, BOOL_TRUE),
        (Boolean, BOOL_FALSE),
    ]
};

/// The `VariantLogicalType` of a value.
fn type_code(node: &VariantNode) -> Result<usize> {
    Ok(match node {
        VariantNode::Null => VARIANT_NULL,
        VariantNode::Object(_) => OBJECT,
        VariantNode::Array(_) => ARRAY,
        VariantNode::Scalar(value) => match value.data() {
            ValueData::Bool(true) => BOOL_TRUE,
            ValueData::Bool(false) => BOOL_FALSE,
            _ => SCALAR_CODES
                .iter()
                .find(|(id, _)| *id == value.logical_type().id())
                .map(|(_, code)| *code)
                .ok_or_else(corrupt_variant)?,
        },
    })
}

/// `ProduceShreddedType`.
fn shredded_scalar_type(code: usize) -> LogicalType {
    let id = SCALAR_CODES
        .iter()
        .find(|(_, c)| *c == code)
        .map(|(id, _)| *id)
        .expect("a scalar code");
    LogicalType::new(id)
}

/// `VariantColumnStatsData`: of the values at one place in the values (the top, an object field, the
/// elements of an array), how many have each type.
#[derive(Default)]
struct ColumnStats {
    type_counts: Vec<usize>,
    total_count: usize,
    decimal: Option<(u8, u8)>,
    decimal_consistent: bool,
    /// the columns of the fields of objects here, in the order first seen
    fields: Vec<(String, usize)>,
    element: Option<usize>,
}

impl ColumnStats {
    fn new() -> Self {
        Self {
            type_counts: vec![0; TYPE_COUNT],
            ..Self::default()
        }
    }
}

/// `VariantShreddingStats`: what the values of a column are, to pick the type to shred them on.
pub struct ShreddingStats {
    columns: Vec<ColumnStats>,
    too_deep: bool,
}

impl Default for ShreddingStats {
    fn default() -> Self {
        Self::new()
    }
}

impl ShreddingStats {
    pub fn new() -> Self {
        Self {
            columns: vec![ColumnStats::new()],
            too_deep: false,
        }
    }

    /// `VariantShreddingStats::Update` with one value of the column (a NULL row is a VARIANT NULL).
    pub fn update(&mut self, node: &VariantNode) -> Result<()> {
        self.visit(node, 0, 0)
    }

    fn visit(&mut self, node: &VariantNode, depth: usize, column: usize) -> Result<()> {
        let code = type_code(node)?;
        let stats = &mut self.columns[column];
        stats.type_counts[code] += 1;
        stats.total_count += 1;
        match node {
            // `VisitDecimal`: shredded on only when all of them have one width and scale
            VariantNode::Scalar(value) if code == DECIMAL => {
                let properties = value.logical_type().decimal_properties();
                if stats.type_counts[DECIMAL] == 1 {
                    stats.decimal = properties;
                    stats.decimal_consistent = true;
                } else if stats.decimal_consistent && stats.decimal != properties {
                    stats.decimal_consistent = false;
                }
            }
            VariantNode::Array(items) => {
                if self.children_too_deep(items.len(), depth) {
                    return Ok(());
                }
                let element = self.element_of(column);
                for item in items {
                    self.visit(item, depth + 1, element)?;
                }
            }
            VariantNode::Object(entries) => {
                if self.children_too_deep(entries.len(), depth) {
                    return Ok(());
                }
                for (key, child) in entries {
                    let field = self.field_of(column, key);
                    self.visit(child, depth + 1, field)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// `ChildrenTooDeep`.
    fn children_too_deep(&mut self, children: usize, depth: usize) -> bool {
        if children == 0 || depth + 1 < MAX_RECURSION_DEPTH {
            return false;
        }
        self.too_deep = true;
        true
    }

    fn element_of(&mut self, column: usize) -> usize {
        if let Some(element) = self.columns[column].element {
            return element;
        }
        let element = self.columns.len();
        self.columns.push(ColumnStats::new());
        self.columns[column].element = Some(element);
        element
    }

    fn field_of(&mut self, column: usize, key: &str) -> usize {
        if let Some((_, field)) = self.columns[column].fields.iter().find(|(name, _)| name == key) {
            return *field;
        }
        let field = self.columns.len();
        self.columns.push(ColumnStats::new());
        self.columns[column].fields.push((key.to_string(), field));
        field
    }

    /// `VariantShreddingStats::GetShreddedType`: the type of the shredded column, if the values have a
    /// shape to shred on.
    pub fn shredded_type(&self) -> Option<LogicalType> {
        if self.too_deep {
            return None;
        }
        self.shredded_type_of(0, None)
    }

    /// `GetShreddedTypeInternal`.
    fn shredded_type_of(&self, column: usize, parent_count: Option<usize>) -> Option<LogicalType> {
        let stats = &self.columns[column];
        let total = parent_count.unwrap_or(stats.total_count);
        let nulls = stats.type_counts[VARIANT_NULL];
        if nulls == stats.total_count {
            // all NULL: INTEGER
            return Some(wrap(LogicalType::INTEGER, nulls == total));
        }
        let mut max_count = 0;
        let mut code = 0;
        for i in 1..TYPE_COUNT {
            if i == DECIMAL && !stats.decimal_consistent {
                continue;
            }
            let count = stats.type_counts[i] + nulls;
            if max_count == 0 || count > max_count {
                max_count = count;
                code = i;
            }
        }
        if max_count == 0 {
            return None;
        }
        let fully_consistent = max_count == total;
        match code {
            OBJECT => {
                let mut fields: Vec<(String, LogicalType)> = Vec::new();
                for (name, field) in &stats.fields {
                    if name.is_empty() {
                        continue;
                    }
                    // STRUCT fields are case-insensitive, object keys are not
                    if fields.iter().any(|(other, _)| other.eq_ignore_ascii_case(name)) {
                        return None;
                    }
                    if let Some(ty) = self.shredded_type_of(*field, Some(total)) {
                        fields.push((name.clone(), ty));
                    }
                }
                if fields.is_empty() {
                    return None;
                }
                // an object is never fully consistent: it can have more fields
                Some(wrap(LogicalType::struct_type(fields), false))
            }
            ARRAY => {
                let element = self.shredded_type_of(stats.element?, None)?;
                Some(wrap(LogicalType::list(element), fully_consistent))
            }
            DECIMAL => {
                let (width, scale) = stats.decimal?;
                Some(wrap(LogicalType::decimal(width, scale), fully_consistent))
            }
            code => Some(wrap(shredded_scalar_type(code), fully_consistent)),
        }
    }
}

/// `VariantStats::GetShreddingType`: the type `force_variant_shredding` names, every level of it able to
/// hold values of other types.
pub fn forced_shredded_type(ty: &LogicalType) -> LogicalType {
    let inner = match ty.id() {
        LogicalTypeId::Struct => LogicalType::struct_type(
            ty.struct_children()
                .iter()
                .map(|(name, child)| (name.clone(), forced_shredded_type(child)))
                .collect(),
        ),
        LogicalTypeId::List => LogicalType::list(forced_shredded_type(ty.child_type().expect("a LIST has a child"))),
        _ => ty.clone(),
    };
    wrap(inner, false)
}

/// `SetShreddedType`: a primitive type of values that all have it is the column's type itself.
fn wrap(typed_value: LogicalType, fully_consistent: bool) -> LogicalType {
    if fully_consistent && !typed_value.is_nested() {
        return typed_value;
    }
    let mut fields = vec![("typed_value".to_string(), typed_value)];
    if !fully_consistent {
        fields.push(("untyped_value_index".to_string(), LogicalType::UINTEGER));
    }
    LogicalType::struct_type(fields)
}

fn is_wrapper(ty: &LogicalType) -> bool {
    ty.id() == LogicalTypeId::Struct
}

/// `VariantShreddingState::ValueIsShredded`: whether a value is of the type it would be shredded on.
fn has_type(node: &VariantNode, ty: &LogicalType) -> bool {
    match (node, ty.id()) {
        (VariantNode::Object(_), LogicalTypeId::Struct) => true,
        (VariantNode::Array(_), LogicalTypeId::List) => true,
        (VariantNode::Scalar(value), LogicalTypeId::Decimal) => {
            // the unscaled value is stored, so only those of its scale that fit its width
            let (Some((width, scale)), Some((max_width, own_scale))) =
                (value.logical_type().decimal_properties(), ty.decimal_properties())
            else {
                return false;
            };
            value.logical_type().id() == LogicalTypeId::Decimal
                && value.logical_type().physical_type() == ty.physical_type()
                && scale == own_scale
                && width <= max_width
        }
        (VariantNode::Scalar(value), id) => value.logical_type().id() == id,
        _ => false,
    }
}

/// The values one row of a shredded column takes in the unshredded part: `VariantNormalizer` writes
/// one per value, nested ones included.
fn value_count(node: &VariantNode) -> u32 {
    match node {
        VariantNode::Object(entries) => 1 + entries.iter().map(|(_, child)| value_count(child)).sum::<u32>(),
        VariantNode::Array(items) => 1 + items.iter().map(value_count).sum::<u32>(),
        _ => 1,
    }
}

/// `VariantColumnData::ShredVariantData` of one row: the value of the shredded column, of `ty`, and
/// the row of the unshredded column (NULL when all of the value is shredded).
pub fn shred(node: &VariantNode, ty: &LogicalType) -> Result<(Value, Value)> {
    let mut shredder = Shredder::default();
    let shredded = shredder.shred_value(Some(node), ty)?;
    let unshredded = if shredder.fragments.is_empty() {
        Value::null(super::unshredded_type())
    } else {
        let roots: Vec<&VariantNode> = shredder.fragments.iter().collect();
        unshredded_row(&roots)?
    };
    Ok((shredded, unshredded))
}

#[derive(Default)]
struct Shredder {
    /// the values that go to the unshredded part of the row, in order
    fragments: Vec<VariantNode>,
    /// how many values the fragments take so far
    values: u32,
}

impl Shredder {
    /// A value that goes to the unshredded part: its 1-based index there.
    fn unshredded(&mut self, node: VariantNode) -> Value {
        let index = self.values + 1;
        self.values += value_count(&node);
        self.fragments.push(node);
        Value::uinteger(index)
    }

    /// `WriteVariantValues` of a value, `None` for a field its object lacks.
    fn shred_value(&mut self, node: Option<&VariantNode>, ty: &LogicalType) -> Result<Value> {
        if !is_wrapper(ty) {
            // flattened: every value here has the type, or is NULL
            return match node {
                Some(node) if has_type(node, ty) => self.typed(node, ty),
                Some(VariantNode::Null) | None => Ok(Value::null(ty.clone())),
                Some(_) => Err(inconsistent()),
            };
        }
        let fields = ty.struct_children();
        let typed_ty = &fields[0].1;
        let has_untyped = fields.len() > 1;
        let (typed, untyped) = match node {
            None => (Value::null(typed_ty.clone()), Value::uinteger(0)),
            Some(VariantNode::Null) => (Value::null(typed_ty.clone()), Value::null(LogicalType::UINTEGER)),
            Some(node) if has_type(node, typed_ty) => {
                let typed = self.typed(node, typed_ty)?;
                // the fields of an object that its type lacks stay unshredded
                let untyped = match node {
                    VariantNode::Object(entries) => {
                        let names = typed_ty.struct_children();
                        let rest: Vec<(String, VariantNode)> = entries
                            .iter()
                            .filter(|(key, _)| !names.iter().any(|(name, _)| name == key))
                            .cloned()
                            .collect();
                        if rest.is_empty() {
                            Value::null(LogicalType::UINTEGER)
                        } else if has_untyped {
                            self.unshredded(VariantNode::Object(rest))
                        } else {
                            return Err(inconsistent());
                        }
                    }
                    _ => Value::null(LogicalType::UINTEGER),
                };
                (typed, untyped)
            }
            Some(node) => {
                if !has_untyped {
                    return Err(inconsistent());
                }
                (Value::null(typed_ty.clone()), self.unshredded(node.clone()))
            }
        };
        let mut children = vec![typed];
        if has_untyped {
            children.push(untyped);
        }
        Value::new(ty.clone(), ValueData::Struct(children))
    }

    /// `WriteTypedValues`: a value of the type it is shredded on.
    fn typed(&mut self, node: &VariantNode, ty: &LogicalType) -> Result<Value> {
        match node {
            VariantNode::Object(entries) => {
                let fields = ty.struct_children();
                let mut children = Vec::with_capacity(fields.len());
                for (name, field_ty) in fields {
                    let child = entries.iter().find(|(key, _)| key == name).map(|(_, child)| child);
                    children.push(self.shred_value(child, field_ty)?);
                }
                Value::new(ty.clone(), ValueData::Struct(children))
            }
            VariantNode::Array(items) => {
                let element = ty.child_type().ok_or_else(corrupt_variant)?;
                let items = items
                    .iter()
                    .map(|item| self.shred_value(Some(item), element))
                    .collect::<Result<Vec<_>>>()?;
                Value::list(element.clone(), items)
            }
            // a DECIMAL of a smaller width is the same unscaled value
            VariantNode::Scalar(value) if ty.id() == LogicalTypeId::Decimal => value.reinterpret(ty),
            VariantNode::Scalar(value) => Ok(value.clone()),
            VariantNode::Null => Ok(Value::null(ty.clone())),
        }
    }
}

fn inconsistent() -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        "Failed to shred variant value - untyped_value was not set but inconsistent values were found",
    )
}

/// `VariantUtils::UnshredVariantData` of one row: the value that the row of the shredded column (of
/// `ty`) and that of the unshredded one stand for.
pub fn unshred(shredded: &Value, ty: &LogicalType, unshredded: &Value) -> Result<VariantNode> {
    let reader = Unshredder {
        physical: Physical::of_row(unshredded)?,
    };
    Ok(reader.resolve(shredded, ty)?.unwrap_or(VariantNode::Null))
}

struct Unshredder {
    physical: Option<Physical>,
}

impl Unshredder {
    /// A value of the unshredded part, by its 0-based index there.
    fn unshredded(&self, index: u32) -> Result<VariantNode> {
        self.physical.as_ref().ok_or_else(corrupt_variant)?.node(index, 0)
    }

    /// `VariantNode::ResolveShredded`: `None` for a missing object field.
    fn resolve(&self, value: &Value, ty: &LogicalType) -> Result<Option<VariantNode>> {
        if !is_wrapper(ty) {
            // a NULL in a flattened column is a VARIANT NULL
            if value.is_null() {
                return Ok(Some(VariantNode::Null));
            }
            return self.typed(value, ty).map(Some);
        }
        let ValueData::Struct(fields) = value.data() else {
            return Ok(Some(VariantNode::Null));
        };
        let typed_ty = &ty.struct_children()[0].1;
        let index = |value: &Value| -> Result<Option<u32>> {
            match value.data() {
                ValueData::Null => Ok(None),
                _ => value.as_i128().map(|v| Some(v as u32)).ok_or_else(corrupt_variant),
            }
        };
        let untyped = match fields.get(1) {
            Some(untyped) => index(untyped)?,
            None => None,
        };
        let typed = fields.first().ok_or_else(corrupt_variant)?;
        if !typed.is_null() {
            let mut node = self.typed(typed, typed_ty)?;
            // the fields of an object that its type lacks
            if let (VariantNode::Object(entries), Some(overlay)) = (&mut node, untyped.filter(|&i| i > 0)) {
                let mut rest = self.unshredded(overlay - 1)?;
                let VariantNode::Object(rest) = &mut rest else {
                    return Err(corrupt_variant());
                };
                entries.append(rest);
                entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            }
            return Ok(Some(node));
        }
        match untyped {
            None => Ok(Some(VariantNode::Null)),
            Some(0) => Ok(None),
            Some(index) => self.unshredded(index - 1).map(Some),
        }
    }

    fn typed(&self, value: &Value, ty: &LogicalType) -> Result<VariantNode> {
        Ok(match value.data() {
            ValueData::Struct(fields) if ty.id() == LogicalTypeId::Struct => {
                let mut entries = Vec::with_capacity(fields.len());
                for ((name, field_ty), field) in ty.struct_children().iter().zip(fields) {
                    if let Some(child) = self.resolve(field, field_ty)? {
                        entries.push((name.clone(), child));
                    }
                }
                entries.sort_by(|(a, _), (b, _)| a.cmp(b));
                VariantNode::Object(entries)
            }
            ValueData::List(items) => {
                let element = ty.child_type().ok_or_else(corrupt_variant)?;
                let items = items
                    .iter()
                    .map(|item| Ok(self.resolve(item, element)?.unwrap_or(VariantNode::Null)))
                    .collect::<Result<Vec<_>>>()?;
                VariantNode::Array(items)
            }
            _ => VariantNode::Scalar(value.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(v: i32) -> VariantNode {
        VariantNode::Scalar(Value::integer(v))
    }

    fn object(entries: &[(&str, VariantNode)]) -> VariantNode {
        VariantNode::Object(entries.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
    }

    fn type_of(nodes: &[VariantNode]) -> Option<LogicalType> {
        let mut stats = ShreddingStats::new();
        for node in nodes {
            stats.update(node).unwrap();
        }
        stats.shredded_type()
    }

    fn roundtrip(nodes: &[VariantNode]) {
        let ty = type_of(nodes).expect("a type");
        for node in nodes {
            let (shredded, unshredded) = shred(node, &ty).unwrap();
            assert_eq!(&unshred(&shredded, &ty, &unshredded).unwrap(), node, "{ty}");
        }
    }

    #[test]
    fn integers_shred_flat() {
        assert_eq!(
            type_of(&[int(1), int(2), VariantNode::Null]),
            Some(LogicalType::INTEGER)
        );
        roundtrip(&[int(1), int(2), VariantNode::Null]);
    }

    #[test]
    fn other_values_go_to_the_untyped_part() {
        let nodes = [int(1), int(2), VariantNode::Scalar(Value::varchar("x"))];
        let ty = type_of(&nodes).unwrap();
        assert_eq!(
            ty.to_string(),
            "STRUCT(typed_value INTEGER, untyped_value_index UINTEGER)"
        );
        roundtrip(&nodes);
    }

    #[test]
    fn objects_keep_their_other_fields() {
        let nodes = [
            object(&[("a", int(1)), ("b", int(2))]),
            object(&[("a", int(3))]),
            object(&[("a", VariantNode::Scalar(Value::varchar("s"))), ("c", int(9))]),
            int(4),
            VariantNode::Array(vec![int(1), int(2)]),
        ];
        roundtrip(&nodes);
    }

    #[test]
    fn arrays_shred_their_elements() {
        let nodes = [
            VariantNode::Array(vec![int(1), int(2)]),
            VariantNode::Array(vec![
                int(3),
                VariantNode::Scalar(Value::varchar("x")),
                VariantNode::Null,
            ]),
        ];
        roundtrip(&nodes);
    }

    #[test]
    fn booleans_count_true_and_false_apart() {
        let t = VariantNode::Scalar(Value::boolean(true));
        let f = VariantNode::Scalar(Value::boolean(false));
        let ty = type_of(&[t.clone(), f.clone()]).unwrap();
        assert_eq!(
            ty.to_string(),
            "STRUCT(typed_value BOOLEAN, untyped_value_index UINTEGER)"
        );
        roundtrip(&[t, f]);
    }
}
