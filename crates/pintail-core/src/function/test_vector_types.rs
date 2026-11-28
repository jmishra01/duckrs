// Ported from duckdb/src/function/table/system/test_vector_types.cpp @ rust-port-baseline
//
//! `test_vector_types(...)`: the values of `test_all_types()` for the given types, in chunks of
//! each kind of vector: flat, constant, dictionary and sequence. Tests use it to run functions
//! on every vector representation.
//!
//! The sequence chunk holds real sequence vectors for integer columns. Nested columns get the
//! values C++ gives them there (a struct or list of integers counts 3, 5, 7), but as flat
//! vectors, since pintail keeps sequences only at the top level.

use super::test_all_types::{TestColumn, test_all_types};
use pintail_common::types::{LogicalType, LogicalTypeId};
use pintail_common::value::{Value, ValueData};
use pintail_common::vector::{DataChunk, SelectionVector, Vector};
use pintail_common::{Error, ErrorKind, Result};
use std::collections::HashMap;

/// `TestVectorFlat::TEST_VECTOR_CARDINALITY`.
const CARDINALITY: usize = 3;

fn unimplemented(ty: &LogicalType) -> Error {
    Error::new(
        ErrorKind::NotImplemented,
        format!("Unimplemented type for test_vector_types {ty}"),
    )
}

/// `TestVectorTypesBind`: VARIANT is not supported, anywhere in a type.
pub fn check_type(ty: &LogicalType) -> Result<()> {
    if contains_variant(ty) {
        return Err(Error::new(
            ErrorKind::NotImplemented,
            "Unimplemented type for test_vector_types",
        ));
    }
    Ok(())
}

fn contains_variant(ty: &LogicalType) -> bool {
    ty.id() == LogicalTypeId::Variant
        || ty.child_type().is_some_and(contains_variant)
        || ty.struct_children().iter().any(|(_, child)| contains_variant(child))
}

/// The columns of `test_all_types()` by type id; the first of an id wins, as in the C++ map.
struct TestTypes(HashMap<LogicalTypeId, TestColumn>);

impl TestTypes {
    fn new() -> Result<Self> {
        let mut map = HashMap::new();
        for column in test_all_types(false, false)? {
            map.entry(column.min.logical_type().id()).or_insert(column);
        }
        Ok(Self(map))
    }

    /// min, max and NULL of a type that is not nested.
    fn leaf(&self, ty: &LogicalType) -> Result<Vec<Value>> {
        match self.0.get(&ty.id()) {
            Some(column) if column.min.logical_type() == ty => {
                Ok(vec![column.min.clone(), column.max.clone(), Value::null(ty.clone())])
            }
            _ => Err(unimplemented(ty)),
        }
    }

    /// `TestVectorFlat::GenerateValues`: three values of `ty`.
    fn flat_values(&self, ty: &LogicalType) -> Result<Vec<Value>> {
        match ty.id() {
            LogicalTypeId::Struct => {
                let mut rows: Vec<Vec<(String, Value)>> = vec![Vec::new(); CARDINALITY];
                for (name, child) in ty.struct_children() {
                    for (row, value) in rows.iter_mut().zip(self.flat_values(child)?) {
                        row.push((name.clone(), value));
                    }
                }
                Ok(rows.into_iter().map(Value::struct_value).collect())
            }
            LogicalTypeId::Map => {
                let entry = ty.child_type().expect("a MAP has an entry type");
                let entries = self.flat_values(entry)?;
                let map = |entry: &Value| Value::new(ty.clone(), ValueData::List(vec![entry.clone()]));
                Ok(vec![map(&entries[0])?, Value::null(ty.clone()), map(&entries[1])?])
            }
            LogicalTypeId::List => {
                let child = ty.child_type().expect("a LIST has a child type");
                let values = self.flat_values(child)?;
                Ok(vec![
                    Value::list(child.clone(), vec![values[0].clone(), values[1].clone()])?,
                    Value::list(child.clone(), Vec::new())?,
                    Value::list(child.clone(), vec![values[2].clone()])?,
                ])
            }
            // its physical type is STRUCT in C++, which builds no UNION value from that
            LogicalTypeId::Union => Err(unimplemented(ty)),
            _ => self.leaf(ty),
        }
    }

    /// The values `TestVectorSequence::GenerateVector` gives `ty`.
    fn sequence_values(&self, ty: &LogicalType) -> Result<Vec<Value>> {
        if is_sequence_type(ty) {
            let sequence = Vector::sequence(ty.clone(), 3, 2)?;
            return Ok((0..CARDINALITY).map(|row| sequence.get_value(row)).collect());
        }
        match ty.id() {
            LogicalTypeId::Struct => {
                let mut rows: Vec<Vec<(String, Value)>> = vec![Vec::new(); CARDINALITY];
                for (name, child) in ty.struct_children() {
                    for (row, value) in rows.iter_mut().zip(self.sequence_values(child)?) {
                        row.push((name.clone(), value));
                    }
                }
                Ok(rows.into_iter().map(Value::struct_value).collect())
            }
            LogicalTypeId::List => {
                let child = ty.child_type().expect("a LIST has a child type");
                let values = self.sequence_values(child)?;
                Ok(vec![
                    Value::list(child.clone(), vec![values[0].clone(), values[1].clone()])?,
                    Value::list(child.clone(), Vec::new())?,
                    Value::list(child.clone(), vec![values[2].clone()])?,
                ])
            }
            LogicalTypeId::Map | LogicalTypeId::Union => Err(unimplemented(ty)),
            _ => self.leaf(ty),
        }
    }
}

/// The types `TestVectorSequence` gives a sequence vector.
fn is_sequence_type(ty: &LogicalType) -> bool {
    use LogicalTypeId::*;
    matches!(
        ty.id(),
        TinyInt | SmallInt | Integer | BigInt | UTinyInt | USmallInt | UInteger | UBigInt
    )
}

fn flat_chunk(types: &[LogicalType], columns: &[Vec<Value>]) -> Result<DataChunk> {
    let mut chunk = DataChunk::with_capacity(types, CARDINALITY);
    for row in 0..CARDINALITY {
        let values: Vec<Value> = columns.iter().map(|column| column[row].clone()).collect();
        chunk.append_row(&values)?;
    }
    Ok(chunk)
}

/// `TestVectorTypesInit`: the chunks of each kind of vector, flattened if `all_flat`.
pub fn test_vector_chunks(types: &[LogicalType], all_flat: bool) -> Result<Vec<DataChunk>> {
    let test_types = TestTypes::new()?;
    let values = types
        .iter()
        .map(|ty| test_types.flat_values(ty))
        .collect::<Result<Vec<_>>>()?;
    let mut chunks = Vec::new();
    // TestVectorFlat
    chunks.push(flat_chunk(types, &values)?);
    // TestVectorConstant: the first row, as constant vectors
    let constants = values.iter().map(|column| Vector::constant(&column[0])).collect();
    chunks.push(DataChunk::from_columns(constants, CARDINALITY));
    // TestVectorDictionary: rows 1 and 2 of a flat chunk
    let mut dictionary = flat_chunk(types, &values)?;
    dictionary.slice(&SelectionVector::from_vec(vec![1, 2]), 2);
    chunks.push(dictionary);
    // TestVectorSequence, which leaves out every chunk with a MAP column
    if types.iter().all(|ty| ty.id() != LogicalTypeId::Map) {
        let mut columns = Vec::with_capacity(types.len());
        for ty in types {
            if is_sequence_type(ty) {
                columns.push(Vector::sequence(ty.clone(), 3, 2)?);
            } else {
                let mut vector = Vector::new(ty.clone(), CARDINALITY);
                for (row, value) in test_types.sequence_values(ty)?.iter().enumerate() {
                    vector.set_value(row, value)?;
                }
                columns.push(vector);
            }
        }
        chunks.push(DataChunk::from_columns(columns, CARDINALITY));
    }
    if all_flat {
        chunks.iter_mut().for_each(DataChunk::flatten);
    }
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pintail_common::vector::VectorType;

    fn text(value: Value) -> String {
        if value.is_null() {
            "NULL".into()
        } else {
            value.try_to_string().unwrap()
        }
    }

    #[test]
    fn chunks_of_each_vector_type() {
        let int_list = LogicalType::list(LogicalType::INTEGER);
        let chunks = test_vector_chunks(&[LogicalType::INTEGER, int_list.clone()], false).unwrap();
        let kinds: Vec<_> = chunks.iter().map(|c| c.column(0).vector_type()).collect();
        assert_eq!(
            kinds,
            [
                VectorType::Flat,
                VectorType::Constant,
                VectorType::Dictionary,
                VectorType::Sequence
            ]
        );
        let rows: Vec<String> = chunks
            .iter()
            .flat_map(|c| (0..c.len()).map(move |r| format!("{} {}", text(c.get_value(0, r)), text(c.get_value(1, r)))))
            .collect();
        assert_eq!(
            rows,
            [
                "-2147483648 [-2147483648, 2147483647]",
                "2147483647 []",
                "NULL [NULL]",
                "-2147483648 [-2147483648, 2147483647]",
                "-2147483648 [-2147483648, 2147483647]",
                "-2147483648 [-2147483648, 2147483647]",
                "2147483647 []",
                "NULL [NULL]",
                "3 [3, 5]",
                "5 []",
                "7 [7]",
            ]
        );
        let flat = test_vector_chunks(&[LogicalType::INTEGER], true).unwrap();
        assert!(flat.iter().all(|c| c.column(0).vector_type() == VectorType::Flat));
        // a MAP column leaves out the sequence chunk
        let map = LogicalType::map(LogicalType::VARCHAR, LogicalType::INTEGER);
        assert_eq!(test_vector_chunks(&[map], false).unwrap().len(), 3);
        assert!(check_type(&LogicalType::list(LogicalType::new(LogicalTypeId::Variant))).is_err());
    }
}
