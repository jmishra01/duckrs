// Ported from duckdb/src/function/cast/geo_casts.cpp, `TryCastToGeometry` of
// duckdb/src/common/operator/cast_operators.cpp and
// duckdb/src/function/scalar/geometry/geometry_functions.cpp @ rust-port-baseline
//
//! Casts from and to GEOMETRY, and the `ST_` functions of the core.

use super::bit_functions::strict_rows;
use super::cast::{CastMode, conversion_error};
use super::nested::row_wise;
use super::{ScalarCallback, ScalarFunction, ScalarFunctionSet};
use crate::planner::expression::BoundExpr;
use duckrs_common::geometry;
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, Vector, VectorType};
use duckrs_common::{Error, ErrorKind, Result};

/// A GEOMETRY of `wkb`.
pub(super) fn geometry_value(ty: &LogicalType, wkb: Vec<u8>) -> Result<Value> {
    Value::new(ty.clone(), ValueData::Bytes(wkb))
}

/// `TryCastToGeometry`: a parse error is an error even of a TRY_CAST in C++ only when the caller
/// doesn't collect errors, which TRY_CAST does; text after a whole geometry is a failed cast.
fn from_text(text: &str, ty: &LogicalType, mode: CastMode) -> Result<Option<Value>> {
    match geometry::from_wkt(text) {
        Ok(wkb) => geometry_value(ty, wkb).map(Some),
        Err(_) if mode == CastMode::Try => Ok(None),
        Err(error) if error.trailing => Err(conversion_error(format!(
            "Could not convert string '{text}' to GEOMETRY"
        ))),
        Err(error) => Err(Error::new(ErrorKind::InvalidInput, error.full_message())),
    }
}

/// `TryCastBlobToGeometry`.
fn from_blob(bytes: &[u8], ty: &LogicalType, mode: CastMode) -> Result<Option<Value>> {
    match geometry::from_wkb(bytes) {
        Ok(wkb) => geometry_value(ty, wkb).map(Some),
        Err(_) if mode == CastMode::Try => Ok(None),
        Err(error) => Err(conversion_error(format!(
            "Could not convert BLOB to GEOMETRY: {}",
            error.message
        ))),
    }
}

/// `GeoCastSwitch`, `BlobToGeoCast` and the VARCHAR to GEOMETRY cast, row by row.
pub(super) fn geometry_cast(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
    let from = source.logical_type().clone();
    let target = result.logical_type().clone();
    let constant = source.vector_type() == VectorType::Constant;
    let rows = if constant { 1 } else { count };
    result.prepare_flat(rows.max(1));
    for i in 0..rows {
        let value = source.get_value(i);
        if value.is_null() {
            result.set_null(i, true);
            continue;
        }
        // `TryVectorNullCast`: the other casts only take NULLs
        let (ValueData::Bytes(bytes), true) = (
            value.data(),
            matches!(
                (from.id(), target.id()),
                (LogicalTypeId::Geometry, LogicalTypeId::Geometry | LogicalTypeId::Blob)
                    | (LogicalTypeId::Blob | LogicalTypeId::Varchar, LogicalTypeId::Geometry)
            ),
        ) else {
            return Err(conversion_error(format!(
                "Unimplemented type for cast ({from} -> {target})"
            )));
        };
        let cast = match (from.id(), target.id()) {
            (LogicalTypeId::Geometry, LogicalTypeId::Geometry) => Some(geometry_value(&target, bytes.clone())?),
            (LogicalTypeId::Geometry, LogicalTypeId::Blob) => Some(Value::blob(bytes.clone())),
            (LogicalTypeId::Blob, LogicalTypeId::Geometry) => from_blob(bytes, &target, mode)?,
            (LogicalTypeId::Varchar, LogicalTypeId::Geometry) => {
                from_text(&String::from_utf8_lossy(bytes), &target, mode)?
            }
            _ => unreachable!("checked above"),
        };
        match cast {
            Some(cast) => result.set_value(i, &cast)?,
            None => result.set_null(i, true),
        }
    }
    if constant {
        result.set_constant();
    }
    Ok(())
}

fn bytes_of(value: &Value) -> &[u8] {
    match value.data() {
        ValueData::Bytes(bytes) => bytes,
        _ => &[],
    }
}

/// `FromWKBFunction`: the errors are those of the WKB, without the cast's prefix.
fn st_geomfromwkb(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        geometry_value(&ty, geometry::from_wkb(bytes_of(&row[0]))?)
    })
}

fn st_aswkb(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::blob(bytes_of(&row[0]).to_vec())))
}

fn st_astext(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::varchar(geometry::to_wkt(bytes_of(&row[0]))?))
    })
}

/// `IntersectsExtentFunction`: whether the bounding boxes meet, in X and Y.
fn st_intersects_extent(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let (left, left_count) = geometry::extent(bytes_of(&row[0]))?;
        let (right, right_count) = geometry::extent(bytes_of(&row[1]))?;
        Ok(Value::boolean(
            left_count != 0 && right_count != 0 && left.intersects_xy(&right),
        ))
    })
}

/// `CRSFunction`: a GEOMETRY without a CRS has none.
fn st_crs(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let _ = args;
    for row in 0..args.len().max(1) {
        result.set_null(row, true);
    }
    Ok(())
}

/// `VertexExtractFunction`, with the check of `VertexExtractBind` on each row.
fn vertex_extract(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() || row[1].is_null() {
            return Ok(Value::null(LogicalType::DOUBLE));
        }
        let coordinate = row[1].try_to_string()?.to_lowercase();
        let index = match coordinate.as_str() {
            "x" => 0,
            "y" => 1,
            "z" => 2,
            "m" => 3,
            _ => {
                return Err(Error::new(
                    ErrorKind::Binder,
                    format!(
                        "vertex_extract: invalid vertex argument '{coordinate}', expected one of 'x', 'y', 'z', or 'm'"
                    ),
                ));
            }
        };
        Ok(match geometry::point_coordinate(bytes_of(&row[0]), index) {
            Some(value) => Value::double(value),
            None => Value::null(LogicalType::DOUBLE),
        })
    })
}

/// The functions on GEOMETRY of the core (`geometry_functions.cpp`).
pub(super) fn geometry_functions() -> Vec<ScalarFunctionSet> {
    let geometry = LogicalType::new(LogicalTypeId::Geometry);
    let definitions: Vec<(&'static str, Vec<LogicalType>, LogicalType, ScalarCallback)> = vec![
        (
            "st_geomfromwkb",
            vec![LogicalType::BLOB],
            geometry.clone(),
            st_geomfromwkb,
        ),
        ("st_aswkb", vec![geometry.clone()], LogicalType::BLOB, st_aswkb),
        ("st_asbinary", vec![geometry.clone()], LogicalType::BLOB, st_aswkb),
        ("st_astext", vec![geometry.clone()], LogicalType::VARCHAR, st_astext),
        ("st_aswkt", vec![geometry.clone()], LogicalType::VARCHAR, st_astext),
        (
            "st_intersects_extent",
            vec![geometry.clone(), geometry.clone()],
            LogicalType::BOOLEAN,
            st_intersects_extent,
        ),
        (
            "&&",
            vec![geometry.clone(), geometry.clone()],
            LogicalType::BOOLEAN,
            st_intersects_extent,
        ),
        ("st_crs", vec![geometry.clone()], LogicalType::VARCHAR, st_crs),
        (
            "vertex_extract",
            vec![geometry, LogicalType::VARCHAR],
            LogicalType::DOUBLE,
            vertex_extract,
        ),
    ];
    definitions
        .into_iter()
        .map(|(name, arguments, return_type, callback)| {
            let mut set = ScalarFunctionSet::new(name);
            set.add(ScalarFunction::new(name, arguments, return_type, callback));
            set
        })
        .collect()
}

/// `vertex_extract(child, key)`.
pub(super) fn bind_vertex_extract(child: BoundExpr, key: BoundExpr) -> BoundExpr {
    let key = key.cast_to(&LogicalType::VARCHAR, false);
    super::nested::bound_function("vertex_extract", vec![child, key], LogicalType::DOUBLE, vertex_extract)
}

/// `left && right` of two GEOMETRYs.
pub(super) fn bind_intersects_extent(left: BoundExpr, right: BoundExpr) -> BoundExpr {
    super::nested::bound_function(
        "st_intersects_extent",
        vec![left, right],
        LogicalType::BOOLEAN,
        st_intersects_extent,
    )
}

/// `vertex_extract(geom, coordinate)`, with `VertexExtractBind`'s checks of the coordinate.
pub(crate) fn bind_vertex_extract_call(children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let [geometry, coordinate] = <[BoundExpr; 2]>::try_from(children).map_err(|_| {
        Error::new(
            ErrorKind::Binder,
            "No function matches the given name and argument types 'vertex_extract'",
        )
    })?;
    let binder = |message: String| Error::new(ErrorKind::Binder, message);
    if !coordinate.is_foldable() {
        return Err(binder(
            "The coordinate argument in function vertex_extract must be a constant expression".to_string(),
        ));
    }
    let value = crate::execution::expression_executor::evaluate_scalar(&coordinate)?;
    if value.is_null() {
        return Err(binder(
            "The coordinate argument in function vertex_extract must not be NULL".to_string(),
        ));
    }
    let name = value.try_to_string()?.to_lowercase();
    if !matches!(name.as_str(), "x" | "y" | "z" | "m") {
        return Err(binder(format!(
            "vertex_extract: invalid vertex argument '{name}', expected one of 'x', 'y', 'z', or 'm'"
        )));
    }
    let geometry = geometry.cast_to(&LogicalType::new(LogicalTypeId::Geometry), false);
    Ok(bind_vertex_extract(geometry, BoundExpr::constant(Value::varchar(name))))
}
