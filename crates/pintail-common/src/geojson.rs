// Ported from duckdb/extension/json/json_functions/json_geojson.cpp @ rust-port-baseline
//
//! GeoJSON of a GEOMETRY (`JSONGeometry::ToGeoJSON`), and the GEOMETRY of GeoJSON (`JSONGeometry::FromGeoJSON`).

use crate::geometry::{GeometryType, MAX_RECURSION_DEPTH};
use crate::json::{Json, write_real, write_string};
use crate::{Error, ErrorKind, Result};

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

/// `GeometryTypeToGeoJSONName`.
fn geojson_name(ty: GeometryType) -> &'static str {
    match ty {
        GeometryType::Point => "Point",
        GeometryType::LineString => "LineString",
        GeometryType::Polygon => "Polygon",
        GeometryType::MultiPoint => "MultiPoint",
        GeometryType::MultiLineString => "MultiLineString",
        GeometryType::MultiPolygon => "MultiPolygon",
        GeometryType::GeometryCollection => "GeometryCollection",
    }
}

/// `GeoJSONNameToGeometryType`.
fn geometry_type_of(name: &str) -> Option<GeometryType> {
    use GeometryType as G;
    [
        G::Point,
        G::LineString,
        G::Polygon,
        G::MultiPoint,
        G::MultiLineString,
        G::MultiPolygon,
        G::GeometryCollection,
    ]
    .into_iter()
    .find(|ty| geojson_name(*ty) == name)
}

// ---------------------------------------------------------------------------------------------------------------
// WKB -> GeoJSON
// ---------------------------------------------------------------------------------------------------------------

/// `WKBReader`.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        let bytes = self
            .data
            .get(self.pos..self.pos + N)
            .ok_or_else(|| invalid("Unexpected end of WKB data while converting to GeoJSON"))?;
        self.pos += N;
        Ok(bytes.try_into().expect("N bytes"))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes::<4>()?))
    }

    fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.bytes::<8>()?))
    }
}

/// `WKBHeader`: the type, and whether it has Z and M.
struct Header {
    ty: GeometryType,
    has_z: bool,
    has_m: bool,
}

/// `ReadHeader`.
fn read_header(reader: &mut Reader) -> Result<Header> {
    let order = reader.bytes::<1>()?[0];
    if order != 1 {
        return Err(invalid(format!("Unsupported byte order {order} in WKB")));
    }
    let meta = reader.u32()?;
    let type_id = (meta & 0xFFFF) % 1000;
    let flag = (meta & 0xFFFF) / 1000;
    let Some(ty) = geometry_type_from_id(type_id) else {
        return Err(invalid(format!("Unsupported geometry type {type_id} in WKB")));
    };
    if flag > 3 {
        return Err(invalid(format!("Unsupported geometry flag {flag} in WKB")));
    }
    Ok(Header {
        ty,
        has_z: flag & 1 != 0,
        has_m: flag & 2 != 0,
    })
}

fn geometry_type_from_id(id: u32) -> Option<GeometryType> {
    use GeometryType as G;
    Some(match id {
        1 => G::Point,
        2 => G::LineString,
        3 => G::Polygon,
        4 => G::MultiPoint,
        5 => G::MultiLineString,
        6 => G::MultiPolygon,
        7 => G::GeometryCollection,
        _ => return None,
    })
}

/// `ReadPosition`: X, Y and Z (0 without one); GeoJSON has no M. An empty point is all NaN.
fn read_position(reader: &mut Reader, header: &Header) -> Result<([f64; 3], bool)> {
    let x = reader.f64()?;
    let y = reader.f64()?;
    let z = if header.has_z { reader.f64()? } else { 0.0 };
    if header.has_m {
        reader.f64()?;
    }
    Ok(([x, y, z], x.is_nan() && y.is_nan()))
}

/// `AppendOrdinates`, as a position array.
fn write_position(out: &mut String, position: &[f64; 3], has_z: bool) {
    out.push('[');
    write_real(out, position[0]);
    out.push(',');
    write_real(out, position[1]);
    if has_z {
        out.push(',');
        write_real(out, position[2]);
    }
    out.push(']');
}

/// `ReadVertexArray`: an array of the positions of a length-prefixed run.
fn vertex_array(reader: &mut Reader, header: &Header, out: &mut String) -> Result<()> {
    let count = reader.u32()?;
    out.push('[');
    for i in 0..count {
        if i > 0 {
            out.push(',');
        }
        let (position, _) = read_position(reader, header)?;
        write_position(out, &position, header.has_z);
    }
    out.push(']');
    Ok(())
}

/// `ToGeoJSONRecursive`.
fn to_geojson_at(reader: &mut Reader, out: &mut String, depth: usize) -> Result<()> {
    if depth == MAX_RECURSION_DEPTH {
        return Err(invalid(format!(
            "Geometry exceeds maximum recursion depth of {MAX_RECURSION_DEPTH}"
        )));
    }
    let header = read_header(reader)?;
    out.push_str("{\"type\":");
    write_string(out, geojson_name(header.ty));
    if header.ty == GeometryType::GeometryCollection {
        out.push_str(",\"geometries\":[");
        let count = reader.u32()?;
        for i in 0..count {
            if i > 0 {
                out.push(',');
            }
            to_geojson_at(reader, out, depth + 1)?;
        }
        out.push_str("]}");
        return Ok(());
    }
    out.push_str(",\"coordinates\":");
    match header.ty {
        GeometryType::Point => {
            let (position, empty) = read_position(reader, &header)?;
            if empty {
                out.push_str("[]");
            } else {
                write_position(out, &position, header.has_z);
            }
        }
        GeometryType::LineString => vertex_array(reader, &header, out)?,
        GeometryType::Polygon => {
            let rings = reader.u32()?;
            out.push('[');
            for i in 0..rings {
                if i > 0 {
                    out.push(',');
                }
                vertex_array(reader, &header, out)?;
            }
            out.push(']');
        }
        GeometryType::MultiPoint => {
            // a flat array of positions, without the empty points, which GeoJSON can't write
            let parts = reader.u32()?;
            out.push('[');
            let mut first = true;
            for _ in 0..parts {
                let part = read_header(reader)?;
                let (position, empty) = read_position(reader, &part)?;
                if empty {
                    continue;
                }
                if !first {
                    out.push(',');
                }
                first = false;
                write_position(out, &position, part.has_z);
            }
            out.push(']');
        }
        GeometryType::MultiLineString => {
            let parts = reader.u32()?;
            out.push('[');
            for i in 0..parts {
                if i > 0 {
                    out.push(',');
                }
                let part = read_header(reader)?;
                vertex_array(reader, &part, out)?;
            }
            out.push(']');
        }
        GeometryType::MultiPolygon => {
            let parts = reader.u32()?;
            out.push('[');
            for i in 0..parts {
                if i > 0 {
                    out.push(',');
                }
                let part = read_header(reader)?;
                let rings = reader.u32()?;
                out.push('[');
                for j in 0..rings {
                    if j > 0 {
                        out.push(',');
                    }
                    vertex_array(reader, &part, out)?;
                }
                out.push(']');
            }
            out.push(']');
        }
        GeometryType::GeometryCollection => unreachable!("handled above"),
    }
    out.push('}');
    Ok(())
}

/// `JSONGeometry::ToGeoJSON`: the GeoJSON text of a GEOMETRY's WKB, appended to `out`.
pub fn to_geojson(wkb: &[u8], out: &mut String) -> Result<()> {
    to_geojson_at(&mut Reader { data: wkb, pos: 0 }, out, 0)
}

// ---------------------------------------------------------------------------------------------------------------
// GeoJSON -> WKB
// ---------------------------------------------------------------------------------------------------------------

fn member<'a>(object: &'a [(String, Json)], key: &str) -> Option<&'a Json> {
    object.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn is_number(json: &Json) -> bool {
    matches!(json, Json::UInt(_) | Json::Int(_) | Json::Real(_) | Json::Raw(_))
}

/// `TryGetOrdinate`: a number, or a number kept as its text.
fn ordinate(json: &Json) -> Option<f64> {
    match json {
        Json::UInt(x) => Some(*x as f64),
        Json::Int(x) => Some(*x as f64),
        Json::Real(x) => Some(*x),
        Json::Raw(text) => text.parse().ok(),
        _ => None,
    }
}

/// `CoordinatesHaveZ`: whether a position of this coordinate tree has a third ordinate.
fn coordinates_have_z(coordinates: &Json, depth: usize) -> bool {
    let Json::Array(items) = coordinates else {
        return false;
    };
    if depth == MAX_RECURSION_DEPTH {
        return false;
    }
    for item in items {
        if is_number(item) {
            return items.len() >= 3;
        }
        if coordinates_have_z(item, depth + 1) {
            return true;
        }
    }
    false
}

/// `GeometryHasZ`: one 3D position makes the whole geometry XYZ.
fn geometry_has_z(json: &Json, depth: usize) -> bool {
    let Json::Object(object) = json else {
        return false;
    };
    if depth == MAX_RECURSION_DEPTH {
        return false;
    }
    if let Some(Json::Array(geometries)) = member(object, "geometries") {
        return geometries.iter().any(|child| geometry_has_z(child, depth + 1));
    }
    member(object, "coordinates").is_some_and(|coordinates| coordinates_have_z(coordinates, depth))
}

/// `WKBWriter`, with `WriteHeader`.
struct Writer {
    out: Vec<u8>,
    has_z: bool,
}

impl Writer {
    fn header(&mut self, ty: GeometryType) {
        self.out.push(1);
        let code = ty as u32 + if self.has_z { 1000 } else { 0 };
        self.out.extend_from_slice(&code.to_le_bytes());
    }

    fn count(&mut self, count: usize) -> Result<()> {
        let count = u32::try_from(count).map_err(|_| invalid("Invalid GeoJSON: too many elements"))?;
        self.out.extend_from_slice(&count.to_le_bytes());
        Ok(())
    }

    fn f64(&mut self, value: f64) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    /// `WritePosition`: X and Y, and Z (0 if it has none) in a geometry with Z; a fourth ordinate is ignored.
    fn position(&mut self, json: &Json, context: &str) -> Result<()> {
        let items = match json {
            Json::Array(items) if items.len() >= 2 => items,
            _ => {
                return Err(invalid(format!(
                    "Invalid GeoJSON: {context} position must be an array of at least 2 numbers"
                )));
            }
        };
        let dims = if self.has_z { 3 } else { 2 };
        let mut ordinates = [0.0; 3];
        for (slot, item) in ordinates.iter_mut().zip(items).take(dims) {
            *slot = ordinate(item)
                .ok_or_else(|| invalid(format!("Invalid GeoJSON: {context} position must contain only numbers")))?;
        }
        for value in &ordinates[..dims] {
            self.f64(*value);
        }
        Ok(())
    }

    /// `WriteVertexArray`.
    fn vertex_array(&mut self, json: &Json, context: &str) -> Result<()> {
        let Json::Array(items) = json else {
            return Err(invalid(format!(
                "Invalid GeoJSON: {context} coordinates are nested too shallowly"
            )));
        };
        self.count(items.len())?;
        for item in items {
            self.position(item, context)?;
        }
        Ok(())
    }

    /// `FromGeoJSONRecursive`.
    fn geometry(&mut self, json: &Json, depth: usize) -> Result<()> {
        if depth == MAX_RECURSION_DEPTH {
            return Err(invalid(format!(
                "GeoJSON geometry exceeds maximum recursion depth of {MAX_RECURSION_DEPTH}"
            )));
        }
        let Json::Object(object) = json else {
            return Err(invalid("Invalid GeoJSON: geometry must be an object"));
        };
        let Some(Json::Str(name)) = member(object, "type") else {
            return Err(invalid("Invalid GeoJSON: geometry is missing a \"type\" string"));
        };
        let ty = geometry_type_of(name)
            .ok_or_else(|| invalid(format!("Invalid GeoJSON: unknown geometry type \"{name}\"")))?;
        let context = geojson_name(ty);
        self.header(ty);
        if ty == GeometryType::GeometryCollection {
            let Some(Json::Array(geometries)) = member(object, "geometries") else {
                return Err(invalid(
                    "Invalid GeoJSON: GeometryCollection requires a \"geometries\" array",
                ));
            };
            self.count(geometries.len())?;
            for child in geometries {
                self.geometry(child, depth + 1)?;
            }
            return Ok(());
        }
        let Some(coordinates @ Json::Array(items)) = member(object, "coordinates") else {
            return Err(invalid(format!(
                "Invalid GeoJSON: {context} requires a \"coordinates\" array"
            )));
        };
        match ty {
            GeometryType::Point if items.is_empty() => {
                // an empty point is all NaN
                let dims = if self.has_z { 3 } else { 2 };
                for _ in 0..dims {
                    self.f64(f64::NAN);
                }
            }
            GeometryType::Point => self.position(coordinates, context)?,
            GeometryType::LineString => self.vertex_array(coordinates, context)?,
            GeometryType::Polygon => {
                self.count(items.len())?;
                for ring in items {
                    self.vertex_array(ring, context)?;
                }
            }
            GeometryType::MultiPoint => {
                self.count(items.len())?;
                for item in items {
                    self.header(GeometryType::Point);
                    self.position(item, context)?;
                }
            }
            GeometryType::MultiLineString => {
                self.count(items.len())?;
                for item in items {
                    self.header(GeometryType::LineString);
                    self.vertex_array(item, context)?;
                }
            }
            GeometryType::MultiPolygon => {
                self.count(items.len())?;
                for item in items {
                    self.header(GeometryType::Polygon);
                    let Json::Array(rings) = item else {
                        return Err(invalid(format!(
                            "Invalid GeoJSON: {context} coordinates are nested too shallowly"
                        )));
                    };
                    self.count(rings.len())?;
                    for ring in rings {
                        self.vertex_array(ring, context)?;
                    }
                }
            }
            GeometryType::GeometryCollection => unreachable!("handled above"),
        }
        Ok(())
    }
}

/// `JSONGeometry::FromGeoJSON`: the WKB of a GeoJSON geometry, before `Geometry::FromBinary` checks it.
pub fn from_geojson(json: &Json) -> Result<Vec<u8>> {
    let mut writer = Writer {
        out: Vec::new(),
        has_z: geometry_has_z(json, 0),
    };
    writer.geometry(json, 0)?;
    Ok(writer.out)
}

/// `JSONGeometry::IsGeoJSONGeometry`: an object with a geometry type and its coordinates or geometries.
pub fn is_geojson_geometry(json: &Json) -> bool {
    let Json::Object(object) = json else {
        return false;
    };
    let Some(Json::Str(name)) = member(object, "type") else {
        return false;
    };
    let Some(ty) = geometry_type_of(name) else {
        return false;
    };
    let key = if ty == GeometryType::GeometryCollection {
        "geometries"
    } else {
        "coordinates"
    };
    matches!(member(object, key), Some(Json::Array(_)))
}
