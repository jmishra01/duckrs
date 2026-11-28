// Ported from duckdb/src/common/row_operations/row_matcher.cpp @ rust-port-baseline

use super::collection::{NestedStores, RowPtr, TupleDataCollection, load, row_is_valid};
use super::layout::TupleDataLayout;
use crate::comparison::{ComparisonType, DuckOrd};
use crate::sort_key::{OrderModifiers, create_sort_keys};
use crate::types::{Interval, PhysicalType, StringT};
use crate::vector::{DataChunk, UnifiedFormat, Vector};
use crate::{Error, Result};

/// Compares the columns of a chunk against candidate rows (`RowMatcher`). Hash joins and
/// aggregates use it to confirm hash-table hits: column `i` of the chunk is compared with
/// column `i` of the row using `predicates[i]`.
#[derive(Debug, Clone)]
pub struct RowMatcher {
    predicates: Vec<ComparisonType>,
}

impl RowMatcher {
    pub fn new(layout: &TupleDataLayout, predicates: Vec<ComparisonType>) -> Result<Self> {
        assert!(
            predicates.len() <= layout.column_count(),
            "more predicates than columns"
        );
        for (ty, predicate) in layout.types().iter().zip(&predicates) {
            let equality = matches!(predicate, ComparisonType::Equal | ComparisonType::NotDistinctFrom);
            if ty.physical_type() == PhysicalType::Struct && !equality {
                return Err(Error::not_implemented(format!(
                    "{predicate:?} comparisons of STRUCT rows are not supported yet"
                )));
            }
        }
        Ok(Self { predicates })
    }

    /// Keeps in `sel` the chunk rows that match their candidate row (`rows[lhs_row]`), in order,
    /// and appends the others to `no_match` (`RowMatcher::Match`).
    ///
    /// # Safety
    ///
    /// `rows[i]` must be a row of `rows_from` for every `i` in `sel`.
    pub unsafe fn match_rows(
        &self,
        rows_from: &TupleDataCollection,
        lhs: &DataChunk,
        rows: &[RowPtr],
        sel: &mut Vec<u32>,
        no_match: Option<&mut Vec<u32>>,
    ) {
        let layout = rows_from.layout();
        let nested = rows_from.nested();
        let input = no_match.is_some().then(|| sel.clone());
        // a column at a time, as C++'s match functions go: each keeps the rows that match it so far
        for (col, &predicate) in self.predicates.iter().enumerate() {
            if sel.is_empty() {
                break;
            }
            let column = Prepared::new(&lhs.columns()[col], lhs.len());
            // SAFETY: forwarded to the caller.
            unsafe { match_column(layout, nested, col, predicate, &column, rows, sel) };
        }
        // the rows that missed, in the order they came in, whichever column they missed on
        if let (Some(no_match), Some(input)) = (no_match, input) {
            let mut kept = sel.iter().peekable();
            for idx in input {
                if kept.peek() == Some(&&idx) {
                    kept.next();
                } else {
                    no_match.push(idx);
                }
            }
        }
    }
}

/// Keeps in `sel` the rows `i` whose value of an LHS column matches column `col` of `rows[i]`: a loop over the
/// rows for the type of the column, which is looked at once, and equality without an order for `=` and
/// `IS NOT DISTINCT FROM`, the predicates of hash tables.
///
/// # Safety
///
/// `rows[i]` must be a live row laid out with `layout` for every `i` in `sel`.
unsafe fn match_column(
    layout: &TupleDataLayout,
    nested: &NestedStores,
    col: usize,
    predicate: ComparisonType,
    lhs: &Prepared,
    rows: &[RowPtr],
    sel: &mut Vec<u32>,
) {
    let Prepared::Value(u) = lhs else {
        // SAFETY: forwarded to the caller.
        sel.retain(|&idx| unsafe {
            match_value(layout, nested, col, predicate, lhs, idx as usize, rows[idx as usize])
        });
        return;
    };
    let can_have_null = layout.can_have_null();
    let equality = matches!(predicate, ComparisonType::Equal | ComparisonType::NotDistinctFrom);
    // whether a pair with a NULL matches: for `IS NOT DISTINCT FROM`, when both are
    let null_match =
        |lhs_null: bool, rhs_null: bool| predicate == ComparisonType::NotDistinctFrom && lhs_null == rhs_null;
    macro_rules! typed {
        ($t:ty) => {{
            let data = u.data::<$t>();
            sel.retain(|&idx| {
                let i = idx as usize;
                let lhs_idx = u.sel.get(i);
                let lhs_null = !u.validity.is_valid(lhs_idx);
                // SAFETY: the caller guarantees the row is alive and laid out with `layout`.
                let rhs_null = can_have_null && !unsafe { row_is_valid(rows[i].as_ptr(), col) };
                if equality {
                    if lhs_null || rhs_null {
                        return null_match(lhs_null, rhs_null);
                    }
                    // SAFETY: the column stores $t values, and the row is alive.
                    let rhs: $t = unsafe { load(layout, rows[i], col) };
                    return data[lhs_idx].duck_eq(&rhs);
                }
                predicate.evaluate(lhs_null, rhs_null, || {
                    // SAFETY: as above.
                    let rhs: $t = unsafe { load(layout, rows[i], col) };
                    data[lhs_idx].duck_cmp(&rhs)
                })
            });
        }};
    }
    match layout.types()[col].physical_type() {
        PhysicalType::Bool => typed!(bool),
        PhysicalType::Int8 => typed!(i8),
        PhysicalType::Int16 => typed!(i16),
        PhysicalType::Int32 => typed!(i32),
        PhysicalType::Int64 => typed!(i64),
        PhysicalType::Int128 => typed!(i128),
        PhysicalType::UInt8 => typed!(u8),
        PhysicalType::UInt16 => typed!(u16),
        PhysicalType::UInt32 => typed!(u32),
        PhysicalType::UInt64 => typed!(u64),
        PhysicalType::UInt128 => typed!(u128),
        PhysicalType::Float => typed!(f32),
        PhysicalType::Double => typed!(f64),
        PhysicalType::Interval => typed!(Interval),
        PhysicalType::Varchar => {
            let data = u.data::<StringT>();
            sel.retain(|&idx| {
                let i = idx as usize;
                let lhs_idx = u.sel.get(i);
                let lhs_null = !u.validity.is_valid(lhs_idx);
                // SAFETY: the caller guarantees the row is alive and laid out with `layout`.
                let rhs_null = can_have_null && !unsafe { row_is_valid(rows[i].as_ptr(), col) };
                if lhs_null || rhs_null {
                    return predicate.evaluate(lhs_null, rhs_null, || unreachable!("a NULL decides"));
                }
                // SAFETY: the column stores strings, and the row is alive.
                let rhs: StringT = unsafe { load(layout, rows[i], col) };
                if equality {
                    // SAFETY: the LHS string's bytes live in the format's buffer, and the row's long strings in its
                    // collection's buffer, alive with the row.
                    return unsafe { data[lhs_idx].equals(&rhs) };
                }
                predicate.evaluate(false, false, || {
                    // SAFETY: as above.
                    let (left, right) = unsafe { (data[lhs_idx].as_bytes(), rhs.as_bytes()) };
                    left.cmp(right)
                })
            });
        }
        other => unreachable!("{other} vectors hold no values"),
    }
}

/// An LHS column prepared for row-at-a-time comparison; STRUCTs are flattened once.
enum Prepared {
    Value(UnifiedFormat),
    Struct {
        validity: UnifiedFormat,
        children: Vec<Prepared>,
    },
    /// LIST/ARRAY values compare through their sort keys (`ASC NULLS LAST`), whose byte order is
    /// DuckDB's nested-value order: element-wise, NULL elements largest, shorter lists first.
    Nested {
        validity: UnifiedFormat,
        keys: Vec<Vec<u8>>,
    },
}

impl Prepared {
    fn new(column: &Vector, count: usize) -> Self {
        if matches!(column.physical_type(), PhysicalType::List | PhysicalType::Array) {
            return Self::Nested {
                validity: column.to_unified(count),
                keys: create_sort_keys(&[(column, OrderModifiers::ASC_NULLS_LAST)], count)
                    .expect("every type has sort keys"),
            };
        }
        if column.physical_type() != PhysicalType::Struct {
            return Self::Value(column.to_unified(count));
        }
        let mut flat = column.clone();
        flat.flatten(count);
        Self::Struct {
            validity: flat.to_unified(count),
            children: flat.struct_children().iter().map(|c| Prepared::new(c, count)).collect(),
        }
    }

    fn is_null(&self, i: usize) -> bool {
        let u = match self {
            Self::Value(u) | Self::Struct { validity: u, .. } | Self::Nested { validity: u, .. } => u,
        };
        !u.validity.is_valid(u.sel.get(i))
    }
}

/// Compares row `i` of an LHS column with column `col` of `row`.
///
/// # Safety
///
/// `row` must be a live row laid out with `layout`.
unsafe fn match_value(
    layout: &TupleDataLayout,
    nested: &NestedStores,
    col: usize,
    predicate: ComparisonType,
    lhs: &Prepared,
    i: usize,
    row: RowPtr,
) -> bool {
    let lhs_null = lhs.is_null(i);
    // SAFETY: forwarded to the caller.
    let rhs_null = layout.can_have_null() && !unsafe { row_is_valid(row.as_ptr(), col) };
    let lhs = match lhs {
        Prepared::Value(u) => u,
        Prepared::Nested { keys, .. } => {
            return predicate.evaluate(lhs_null, rhs_null, || {
                // SAFETY: a valid row's slot holds an index written by scatter.
                let index: u64 = unsafe { load(layout, row, col) };
                let store = &nested.lists[&col];
                keys[i].cmp(&store.keys[index as usize])
            });
        }
        Prepared::Struct { children, .. } => {
            // STRUCT equality: both structs non-NULL (or, for NOT DISTINCT FROM, both NULL), and
            // every field NOT DISTINCT FROM its counterpart.
            if lhs_null || rhs_null {
                return predicate == ComparisonType::NotDistinctFrom && lhs_null == rhs_null;
            }
            let child_layout = layout.struct_layout(col);
            // SAFETY: the struct's nested row lies at this column's offset.
            let child_row = RowPtr::from_raw(unsafe { row.as_ptr().add(layout.offsets()[col]) });
            return children.iter().enumerate().all(|(c, child)| {
                // SAFETY: nested row of child_layout, alive with its parent.
                unsafe {
                    match_value(
                        child_layout,
                        nested.structs.get(&col).unwrap_or(&NestedStores::default()),
                        c,
                        ComparisonType::NotDistinctFrom,
                        child,
                        i,
                        child_row,
                    )
                }
            });
        }
    };
    let lhs_idx = lhs.sel.get(i);
    macro_rules! cmp {
        ($t:ty) => {
            predicate.evaluate(lhs_null, rhs_null, || {
                // SAFETY: the column stores $t values; the caller guarantees the row is alive.
                let rhs: $t = unsafe { load(layout, row, col) };
                lhs.data::<$t>()[lhs_idx].duck_cmp(&rhs)
            })
        };
    }
    match layout.types()[col].physical_type() {
        PhysicalType::Bool => cmp!(bool),
        PhysicalType::Int8 => cmp!(i8),
        PhysicalType::Int16 => cmp!(i16),
        PhysicalType::Int32 => cmp!(i32),
        PhysicalType::Int64 => cmp!(i64),
        PhysicalType::Int128 => cmp!(i128),
        PhysicalType::UInt8 => cmp!(u8),
        PhysicalType::UInt16 => cmp!(u16),
        PhysicalType::UInt32 => cmp!(u32),
        PhysicalType::UInt64 => cmp!(u64),
        PhysicalType::UInt128 => cmp!(u128),
        PhysicalType::Float => cmp!(f32),
        PhysicalType::Double => cmp!(f64),
        PhysicalType::Interval => cmp!(Interval),
        PhysicalType::Varchar => predicate.evaluate(lhs_null, rhs_null, || {
            // SAFETY: the column stores strings; the caller guarantees the row is alive.
            let rhs: StringT = unsafe { load(layout, row, col) };
            // SAFETY: the row's long strings live in its collection's buffer, alive with the row.
            let rhs = unsafe { rhs.as_bytes() };
            lhs.str_at(i).cmp(rhs)
        }),
        other => unreachable!("{other} vectors hold no values"),
    }
}
