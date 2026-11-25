//! Reads the objects of DuckDB's file format that are described by its serialization specs: parsed
//! expressions, query nodes, table references, constraints and `CreateInfo`.
//!
//! `generated.rs` follows the specs field by field (`tools/generate_serialization.py`), `custom.rs`
//! has the classes that C++ reads by hand, and `convert.rs` turns what they read into the AST.

pub mod convert;
pub mod custom;
pub mod generated;
pub mod plan;
pub mod to_stored;
pub mod verify;

use super::block_manager::corrupt;
use duckrs_common::serializer::{BinaryDeserializer, Deserialize};
use duckrs_common::{Error, Result};

pub(crate) fn not_ported(what: &str) -> Error {
    Error::not_implemented(format!(
        "reading {what} from a database file is not yet ported to duckrs"
    ))
}

/// `TypeExpressionReader`: the types of a catalog can be named by parsed expressions.
pub(crate) fn read_type_expression(d: &mut BinaryDeserializer<'_>) -> Result<String> {
    let expression = generated::ParsedExpression::read_value(d)?;
    Ok(convert::expression(expression)?.to_string())
}

/// A value that selects a subclass and that the format doesn't have.
fn unknown(class: &str, value: u64) -> Error {
    corrupt(format!("unknown {class} type {value}"))
}

/// `ColumnDataRef::Deserialize`: what is wrong with one, which has its collection or not.
pub(crate) fn column_data_ref(d: &mut BinaryDeserializer<'_>) -> Result<Error> {
    let missing = || {
        Error::new(
            duckrs_common::ErrorKind::Serialization,
            "ColumnDataRef is missing its collection",
        )
    };
    if !d.has_named_property(202, "collection")? {
        return Ok(missing());
    }
    if d.is_json() {
        d.set_tag("collection");
        if !d.begin_nullable()? {
            return Ok(missing());
        }
    }
    Ok(not_ported("column data references"))
}
