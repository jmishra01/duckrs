// Ported from the GeoJSON Feature writer of duckdb/extension/json/json_functions/json_create.cpp
// (`JSONCopyToGeoJSONBind`, `JSONCopyToGeoJSONFunction`, `AddGeoJSONBoundingBoxes`) @ rust-port-baseline
//
//! What `COPY ... TO ... (FORMAT GEOJSON)` writes of a row: a GeoJSON Feature, with the geometry column as its
//! "geometry", the id column as its "id" and the other columns as its "properties". The JSON of a value is that
//! of `to_json`, which works a value at a time; a geometry is written as GeoJSON, also inside the properties.

use super::json::{Json, json_type, value_to_json};
use super::nested::{bound_function, row_wise};
use crate::planner::expression::BoundExpr;
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result};

pub const NAME: &str = "__internal_json_copy_to_geojson";

fn binder_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Binder, message.into())
}

/// Which columns of a row go where in its Feature (`JSONCopyToGeoJSONFunctionData`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FeatureLayout {
    /// the column that is the "geometry"; none writes a null geometry
    pub geometry: Option<usize>,
    /// the column that is the Feature-level "id"
    pub id: Option<usize>,
    /// the other columns, the "properties"
    pub properties: Vec<usize>,
    /// whether a "bbox" of the geometry goes with it
    pub bbox: bool,
    /// `DATEFORMAT` and `TIMESTAMPFORMAT` of the properties
    pub date_format: Option<String>,
    pub timestamp_format: Option<String>,
}

/// A column option: `None` was not given (the default), `Some("")` explicitly none, else the column named.
pub type ColumnOption = Option<String>;

/// `FindGeoJSONColumn`.
fn find_column(columns: &[(String, LogicalType)], name: &str, option: &str) -> Result<usize> {
    if let Some(index) = columns.iter().position(|(column, _)| column == name) {
        return Ok(index);
    }
    let available: Vec<&str> = columns.iter().map(|(column, _)| column.as_str()).collect();
    Err(binder_error(format!(
        "COPY ... TO ... (FORMAT GEOJSON) option {option} refers to column \"{name}\", which is not being written. \
         Available columns: {}",
        available.join(", ")
    )))
}

/// `JSONCopyToGeoJSONBind`: the geometry is the first GEOMETRY column unless named, the id a column named "id".
pub fn layout(
    columns: &[(String, LogicalType)],
    geometry_column: &ColumnOption,
    id_column: &ColumnOption,
    bbox: bool,
) -> Result<FeatureLayout> {
    let geometry = match geometry_column {
        Some(name) if name.is_empty() => None,
        Some(name) => {
            let index = find_column(columns, name, "GEOMETRY_COLUMN")?;
            if columns[index].1.id() != LogicalTypeId::Geometry {
                return Err(binder_error(format!(
                    "COPY ... TO ... (FORMAT GEOJSON) option GEOMETRY_COLUMN refers to column \"{name}\", which has \
                     type {} instead of GEOMETRY",
                    columns[index].1
                )));
            }
            Some(index)
        }
        None => match columns.iter().position(|(_, ty)| ty.id() == LogicalTypeId::Geometry) {
            Some(index) => Some(index),
            None => {
                return Err(binder_error(
                    "COPY ... TO ... (FORMAT GEOJSON) requires a GEOMETRY column, but none of the columns being \
                     written has type GEOMETRY.\n Use GEOMETRY_COLUMN NULL to write features without a geometry \
                     instead.",
                ));
            }
        },
    };
    let id = match id_column {
        Some(name) if name.is_empty() => None,
        Some(name) => Some(find_column(columns, name, "ID_COLUMN")?),
        None => columns.iter().position(|(column, _)| column == "id"),
    };
    if geometry.is_some() && geometry == id {
        return Err(binder_error(
            "COPY ... TO ... (FORMAT GEOJSON) cannot write the same column as both the geometry and the Feature id",
        ));
    }
    let properties = (0..columns.len())
        .filter(|index| Some(*index) != geometry && Some(*index) != id)
        .collect();
    Ok(FeatureLayout {
        geometry,
        id,
        properties,
        bbox,
        date_format: None,
        timestamp_format: None,
    })
}

/// `JSONGeometry::ToGeoJSON`: the GeoJSON of a geometry, as JSON text.
fn geometry_json(wkb: &[u8]) -> Result<Json> {
    let mut text = String::new();
    duckrs_common::geojson::to_geojson(wkb, &mut text)?;
    Ok(Json::Raw(text))
}

/// The JSON of a property value: as `to_json` makes it, but a geometry as GeoJSON (the writer overrides
/// `json_geometry_format`), and a date or timestamp in the formats of the COPY.
fn property_json(value: &Value, layout: &FeatureLayout) -> Result<Json> {
    const MICROS_PER_DAY: i64 = 86_400_000_000;
    if value.is_null() {
        return Ok(Json::Null);
    }
    let ty = value.logical_type();
    Ok(match (ty.id(), value.data()) {
        (LogicalTypeId::Geometry, ValueData::Bytes(wkb)) => geometry_json(wkb)?,
        (LogicalTypeId::Date, ValueData::Int32(days)) if layout.date_format.is_some() => Json::Str(
            super::dates::strftime_text(layout.date_format.as_deref().unwrap_or_default(), *days, 0, false)?,
        ),
        (LogicalTypeId::Timestamp | LogicalTypeId::TimestampTz, ValueData::Int64(micros))
            if layout.timestamp_format.is_some() =>
        {
            Json::Str(super::dates::strftime_text(
                layout.timestamp_format.as_deref().unwrap_or_default(),
                micros.div_euclid(MICROS_PER_DAY) as i32,
                micros.rem_euclid(MICROS_PER_DAY),
                ty.id() == LogicalTypeId::TimestampTz,
            )?)
        }
        (LogicalTypeId::Struct, ValueData::Struct(fields)) => Json::Object(
            ty.struct_children()
                .iter()
                .zip(fields)
                .map(|((name, _), field)| Ok((name.clone(), property_json(field, layout)?)))
                .collect::<Result<_>>()?,
        ),
        (LogicalTypeId::List | LogicalTypeId::Array, ValueData::List(items)) => Json::Array(
            items
                .iter()
                .map(|item| property_json(item, layout))
                .collect::<Result<_>>()?,
        ),
        _ => value_to_json(value)?,
    })
}

/// `AddGeoJSONBoundingBoxes`: all the minimums, then all the maximums, with Z where the geometry has it; nothing of
/// a geometry without a finite extent.
fn bounding_box(wkb: &[u8]) -> Result<Option<Json>> {
    let (extent, count) = duckrs_common::geometry::extent(wkb)?;
    if count == 0 || !extent.x_min.is_finite() || !extent.y_min.is_finite() {
        return Ok(None);
    }
    let has_z = extent.z_min.is_finite() && extent.z_max.is_finite();
    let mut bounds = vec![Json::Real(extent.x_min), Json::Real(extent.y_min)];
    if has_z {
        bounds.push(Json::Real(extent.z_min));
    }
    bounds.extend([Json::Real(extent.x_max), Json::Real(extent.y_max)]);
    if has_z {
        bounds.push(Json::Real(extent.z_max));
    }
    Ok(Some(Json::Array(bounds)))
}

/// `JSONCopyToGeoJSONFunction` of one row: its columns are `values`, named `names`.
pub fn feature(values: &[Value], names: &[String], layout: &FeatureLayout) -> Result<Json> {
    let mut members = vec![("type".to_string(), Json::Str("Feature".to_string()))];
    if let Some(id) = layout.id {
        members.push(("id".to_string(), property_json(&values[id], layout)?));
    }
    let geometry = layout.geometry.map(|at| &values[at]).filter(|value| !value.is_null());
    let wkb = geometry.and_then(|value| match value.data() {
        ValueData::Bytes(wkb) => Some(wkb.as_slice()),
        _ => None,
    });
    members.push((
        "geometry".to_string(),
        match wkb {
            Some(wkb) => geometry_json(wkb)?,
            None => Json::Null,
        },
    ));
    if layout.bbox
        && let Some(wkb) = wkb
        && let Some(bbox) = bounding_box(wkb)?
    {
        members.push(("bbox".to_string(), bbox));
    }
    let properties = layout
        .properties
        .iter()
        .map(|&at| Ok((names[at].clone(), property_json(&values[at], layout)?)))
        .collect::<Result<_>>()?;
    members.push(("properties".to_string(), Json::Object(properties)));
    Ok(Json::Object(members))
}

/// The value of a constant option of the function.
fn constant(expr: &BoundExpr) -> Result<Value> {
    crate::execution::expression_executor::evaluate_scalar(expr)
}

/// `TryGetGeoJSONColumnOption`: NULL is not given, else the VARCHAR names the column (or none, if empty).
fn column_option(expr: &BoundExpr) -> Result<ColumnOption> {
    if !expr.is_foldable() {
        return Err(binder_error(format!("{NAME} column options must be constants")));
    }
    let value = constant(expr)?;
    if value.is_null() {
        return Ok(None);
    }
    if !matches!(value.logical_type().id(), LogicalTypeId::Varchar | LogicalTypeId::StringLiteral) {
        return Err(binder_error(format!("{NAME} column options must be VARCHAR or NULL")));
    }
    Ok(Some(value.try_to_string()?))
}

/// The formats, constants or NULL.
fn format_option(expr: &BoundExpr) -> Result<Option<String>> {
    let value = constant(expr)?;
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(value.try_to_string()?))
}

/// `JSONCopyToGeoJSONBind` of `__internal_json_copy_to_geojson(payload, date_format, timestamp_format,
/// geometry_column, id_column, write_bbox)`: the layout is decided from the constants, and kept in the arguments.
pub fn bind(children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let internal = || binder_error(format!("{NAME} is for internal use only"));
    let [payload, date_format, timestamp_format, geometry_column, id_column, bbox] =
        <[BoundExpr; 6]>::try_from(children).map_err(|_| internal())?;
    if payload.return_type.id() != LogicalTypeId::Struct {
        return Err(internal());
    }
    let columns = payload.return_type.struct_children().to_vec();
    let geometry_column = column_option(&geometry_column)?;
    let id_column = column_option(&id_column)?;
    // `GetConstant`: a volatile write_bbox is no constant
    if !bbox.is_foldable() {
        return Err(binder_error(format!("{NAME} write_bbox must be a constant expression")));
    }
    let bbox_value = constant(&bbox.cast_to(&LogicalType::BOOLEAN, false))?;
    let write_bbox = matches!(bbox_value.data(), ValueData::Bool(true));
    let mut layout = layout(&columns, &geometry_column, &id_column, write_bbox)?;
    layout.date_format = format_option(&date_format)?;
    layout.timestamp_format = format_option(&timestamp_format)?;
    let text = |option: &ColumnOption| match option {
        Some(name) => Value::varchar(name.as_str()),
        None => Value::null(LogicalType::VARCHAR),
    };
    let format = |option: &Option<String>| match option {
        Some(format) => Value::varchar(format.as_str()),
        None => Value::null(LogicalType::VARCHAR),
    };
    let arguments = vec![
        payload,
        BoundExpr::constant(format(&layout.date_format)),
        BoundExpr::constant(format(&layout.timestamp_format)),
        BoundExpr::constant(text(&geometry_column)),
        BoundExpr::constant(text(&id_column)),
        BoundExpr::constant(Value::boolean(write_bbox)),
    ];
    Ok(bound_function(NAME, arguments, json_type(), copy_to_geojson))
}

/// `JSONCopyToGeoJSONFunction`: the Feature of each row of the payload.
fn copy_to_geojson(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let payload_type = args.column(0).logical_type().clone();
    let columns = payload_type.struct_children().to_vec();
    let names: Vec<String> = columns.iter().map(|(name, _)| name.clone()).collect();
    let option = |at: usize| -> Result<ColumnOption> {
        if args.is_empty() {
            return Ok(None);
        }
        let value = args.column(at).get_value(0);
        (!value.is_null()).then(|| value.try_to_string()).transpose()
    };
    let format = |at: usize| -> Result<Option<String>> { option(at) };
    let bbox = !args.is_empty() && matches!(args.column(5).get_value(0).data(), ValueData::Bool(true));
    let mut layout = layout(&columns, &option(3)?, &option(4)?, bbox)?;
    layout.date_format = format(1)?;
    layout.timestamp_format = format(2)?;
    let ty = json_type();
    row_wise(args, result, |row| {
        let ValueData::Struct(values) = row[0].data() else {
            return Ok(Value::null(ty.clone()));
        };
        Value::varchar(feature(values, &names, &layout)?.text()).reinterpret(&ty)
    })
}
