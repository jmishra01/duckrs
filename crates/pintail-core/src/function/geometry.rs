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
use pintail_common::types::{LogicalType, LogicalTypeId};
use pintail_common::value::{Value, ValueData};
use pintail_common::vector::{DataChunk, Vector, VectorType};
use pintail_common::{Error, ErrorKind, Result};
use pintail_common::{geometry, geometry_crs};

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
            (LogicalTypeId::Geometry, LogicalTypeId::Geometry) => {
                // `GeoCastSwitch`: two CRSs must be the same one
                if let (Some(source), Some(target)) = (from.crs(), target.crs())
                    && source != target
                {
                    return Err(Error::new(
                        ErrorKind::Binder,
                        format!(
                            "Cannot cast GEOMETRY with CRS '{}' to GEOMETRY with different CRS '{}'",
                            source.identifier(),
                            target.identifier()
                        ),
                    ));
                }
                Some(geometry_value(&target, bytes.clone())?)
            }
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

/// `AsGeoJSONFunction`.
fn st_asgeojson(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let mut text = String::new();
    pintail_common::executor::unary_str_to_str_fallible(args.column(0), result, args.len(), |wkb, out| {
        text.clear();
        pintail_common::geojson::to_geojson(wkb, &mut text)?;
        out.extend_from_slice(text.as_bytes());
        Ok(())
    })
}

/// `GeomFromGeoJSONFunction`: the GeoJSON read, made WKB, and checked as `Geometry::FromBinary` checks it.
fn st_geomfromgeojson(args: &DataChunk, result: &mut Vector) -> Result<()> {
    pintail_common::executor::unary_str_to_str_fallible(args.column(0), result, args.len(), |input, out| {
        out.extend_from_slice(&geometry_of_geojson(input)?);
        Ok(())
    })
}

/// The GEOMETRY of a GeoJSON text (`JSONGeometry::FromGeoJSON`).
pub(crate) fn geometry_of_geojson(input: &[u8]) -> Result<Vec<u8>> {
    let text = String::from_utf8_lossy(input);
    // a document nested deeply is read on a stack as deep as it
    super::json::with_stack_for(&text, || {
        let json = pintail_common::json::parse(&text).map_err(|e| {
            Error::new(
                ErrorKind::InvalidInput,
                pintail_common::json::parse_error_text(&text, &e),
            )
        })?;
        let wkb = pintail_common::geojson::from_geojson(&json)?;
        geometry::from_wkb(&wkb)
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
        // the JSON extension's `ST_AsGeoJSON` and `ST_GeomFromGeoJSON` (`json_geojson.cpp`)
        (
            "st_asgeojson",
            vec![geometry.clone()],
            super::json::json_type(),
            st_asgeojson,
        ),
        (
            "st_geomfromgeojson",
            vec![LogicalType::VARCHAR],
            geometry.clone(),
            st_geomfromgeojson,
        ),
        (
            "st_geomfromgeojson",
            vec![super::json::json_type()],
            geometry.clone(),
            st_geomfromgeojson,
        ),
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

fn no_match(name: &str, children: &[BoundExpr]) -> Error {
    let types: Vec<String> = children
        .iter()
        .map(|c| super::binder::expression_return_type(c).to_string())
        .collect();
    Error::new(
        ErrorKind::Binder,
        format!(
            "No function matches the given name and argument types '{name}({})'. You might need to add explicit type \
             casts.",
            types.join(", ")
        ),
    )
}

/// Whether `child` can be a GEOMETRY argument: one, a string literal or a NULL.
fn geometry_argument(child: &BoundExpr) -> bool {
    matches!(
        super::binder::expression_return_type(child).id(),
        LogicalTypeId::Geometry | LogicalTypeId::StringLiteral | LogicalTypeId::SqlNull
    )
}

/// `BindCRSFunctionExpression`: the CRS of the argument's type, as a constant.
pub(crate) fn bind_st_crs(children: Vec<BoundExpr>) -> Result<BoundExpr> {
    match children.as_slice() {
        [child] if geometry_argument(child) => Ok(BoundExpr::constant(match child.return_type.crs() {
            Some(crs) => Value::varchar(crs.definition.clone()),
            None => Value::null(LogicalType::VARCHAR),
        })),
        _ => Err(no_match("st_crs", &children)),
    }
}

/// `SetCRSBind`: a GEOMETRY of the CRS the constant names, as the catalog identifies it.
pub(crate) fn bind_st_setcrs(children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let [geometry, crs] = children.as_slice() else {
        return Err(no_match("st_setcrs", &children));
    };
    if !geometry_argument(geometry)
        || !matches!(
            super::binder::expression_return_type(crs).id(),
            LogicalTypeId::Varchar | LogicalTypeId::StringLiteral | LogicalTypeId::SqlNull
        )
    {
        return Err(no_match("st_setcrs", &children));
    }
    if !crs.is_foldable() {
        return Err(Error::new(
            ErrorKind::Binder,
            "The crs argument in function st_setcrs must be a constant expression",
        ));
    }
    let value = crate::execution::expression_executor::evaluate_scalar(crs)?;
    let target = if value.is_null() {
        LogicalType::new(LogicalTypeId::Geometry)
    } else {
        let text = value.try_to_string()?;
        let identified =
            geometry_crs::try_identify(&text).map_err(|message| Error::new(ErrorKind::InvalidInput, message))?;
        let crs = identified.unwrap_or_else(|| geometry_crs::Crs::parse(&text));
        LogicalType::geometry(Some(crs))
    };
    // the geometry stays as it is, only its type changes (`SetCRSFunction`)
    let input = geometry
        .clone()
        .cast_to(&LogicalType::new(LogicalTypeId::Geometry), false);
    Ok(super::nested::bound_function(
        "st_setcrs",
        vec![input, crs.clone()],
        target,
        st_setcrs,
    ))
}

fn st_setcrs(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(ty.clone()));
        }
        geometry_value(&ty, bytes_of(&row[0]).to_vec())
    })
}
