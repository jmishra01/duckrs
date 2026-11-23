// Ported from duckdb/extension/parquet/parquet_geometry.cpp, include/parquet_geometry.hpp, the GEOMETRY case of
// ParquetWriter::SetSchemaProperties (parquet_writer.cpp) and GeoStatisticsState::WriteGeoStats
// (include/writer/parquet_write_stats.hpp) @ rust-port-baseline
//
//! What the writer says of GEOMETRY columns: the GeoParquet metadata (the `geo` key of the footer, as JSON), the
//! CRS of the Parquet `GEOMETRY` logical type, and the `GeospatialStatistics` of a column chunk.

use duckrs_common::geometry::GeometryStats;
use duckrs_common::geometry_crs::{CrsType, try_convert_projjson};
use duckrs_common::json::{self, Json};
use duckrs_common::types::LogicalType;
use duckrs_common::{Error, ErrorKind, Result};

use super::encoding::{Thrift, ttype};

/// `GeoParquetVersion`: which GeoParquet metadata a file gets, and whether GEOMETRY columns are Parquet's own
/// `GEOMETRY` type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GeoParquetVersion {
    /// GeoParquet 1.0 metadata, with the geometries as plain BLOBs (the widest support)
    #[default]
    V1,
    /// GeoParquet 2.0 metadata, with Parquet's `GEOMETRY` type and its statistics
    V2,
    /// GeoParquet 1.0 metadata, with Parquet's `GEOMETRY` type and its statistics
    Both,
    /// no GeoParquet metadata, only Parquet's `GEOMETRY` type and its statistics
    None,
}

impl GeoParquetVersion {
    /// The `GEOPARQUET_VERSION` option of COPY.
    pub fn parse(option: &str) -> Result<Self> {
        Ok(match option.to_uppercase().as_str() {
            "NONE" => Self::None,
            "V1" => Self::V1,
            "V2" => Self::V2,
            "BOTH" => Self::Both,
            _ => {
                return Err(Error::new(
                    ErrorKind::Binder,
                    "Expected geoparquet_version 'NONE', 'V1' or 'BOTH'",
                ));
            }
        })
    }
}

/// `SetSchemaProperties` of a GEOMETRY: the CRS of its `GEOMETRY` logical type, as PROJJSON if the catalog
/// can convert it, and as it is otherwise.
pub(super) fn geometry_crs_annotation(ty: &LogicalType) -> Option<String> {
    let crs = ty.crs()?;
    if crs.kind != CrsType::Projjson
        && let Some(lookup) = try_convert_projjson(crs)
    {
        return Some(lookup.definition);
    }
    Some(crs.definition.clone())
}

/// `GeoStatisticsState::WriteGeoStats`: a `GeospatialStatistics`, its bounding box if it has X and Y, and the
/// ISO WKB codes of its types.
pub(super) fn write_geospatial_statistics(t: &mut Thrift, stats: &GeometryStats) {
    let bbox = &stats.extent;
    if bbox.has_xy() {
        t.structure(1, |t| {
            t.double(1, bbox.x_min);
            t.double(2, bbox.x_max);
            t.double(3, bbox.y_min);
            t.double(4, bbox.y_max);
            if bbox.has_z() {
                t.double(5, bbox.z_min);
                t.double(6, bbox.z_max);
            }
            if bbox.has_m() {
                t.double(7, bbox.m_min);
                t.double(8, bbox.m_max);
            }
        });
    }
    let types = stats.wkb_types();
    t.list(2, ttype::I32, types.len());
    for code in types {
        t.element_i32(code);
    }
}

/// `GeoParquetColumnMetadata`, of what the writer needs (the encoding is always WKB).
#[derive(Debug, Clone)]
struct GeoParquetColumn {
    name: String,
    stats: GeometryStats,
    /// the CRS in PROJJSON, empty for the default (`OGC:CRS84`)
    projjson: String,
}

/// `GeoParquetFileMetadata`: the GEOMETRY columns of a file being written, in the order they were added.
#[derive(Debug, Clone)]
pub(super) struct GeoParquetFileMetadata {
    version: GeoParquetVersion,
    columns: Vec<GeoParquetColumn>,
}

impl GeoParquetFileMetadata {
    pub(super) fn new(version: GeoParquetVersion) -> Self {
        Self {
            version,
            columns: Vec::new(),
        }
    }

    /// `AddGeoParquetStats`: the statistics of a column chunk, and the CRS of a column met first.
    pub(super) fn add_stats(
        &mut self,
        column_name: &str,
        ty: &LogicalType,
        stats: &GeometryStats,
        version: GeoParquetVersion,
    ) -> Result<()> {
        if let Some(column) = self.columns.iter_mut().find(|c| c.name == column_name) {
            column.stats.merge(stats);
            return Ok(());
        }
        let mut column = GeoParquetColumn {
            name: column_name.to_string(),
            stats: GeometryStats::empty(),
            projjson: String::new(),
        };
        if ty.crs().is_some() {
            convert_crs(&mut column, ty, version)?;
        }
        column.stats.merge(stats);
        self.columns.push(column);
        Ok(())
    }

    /// `GeoParquetFileMetadata::Write`: the JSON of the `geo` key, or `None` if no column has any.
    pub(super) fn write(mut self) -> Result<Option<String>> {
        // GeoParquet has no M coordinates, so columns that have them are left out
        self.columns.retain(|column| !column.stats.extent.has_m());
        // the primary geometry column is the one added first
        let Some(primary) = self.columns.first().map(|column| column.name.clone()) else {
            return Ok(None);
        };
        let version = match self.version {
            GeoParquetVersion::V1 | GeoParquetVersion::Both => "1.0.0",
            GeoParquetVersion::V2 => "2.0.0",
            GeoParquetVersion::None => {
                return Err(Error::new(
                    ErrorKind::Internal,
                    "GeoParquetVersion::NONE should not write metadata",
                ));
            }
        };
        let mut columns = Vec::with_capacity(self.columns.len());
        for column in &self.columns {
            let mut fields = vec![
                ("encoding".to_string(), Json::Str("WKB".to_string())),
                (
                    "geometry_types".to_string(),
                    Json::Array(column.stats.type_names(false).into_iter().map(Json::Str).collect()),
                ),
            ];
            let bbox = &column.stats.extent;
            if bbox.has_xy() {
                let bounds = if bbox.has_z() {
                    vec![bbox.x_min, bbox.y_min, bbox.z_min, bbox.x_max, bbox.y_max, bbox.z_max]
                } else {
                    vec![bbox.x_min, bbox.y_min, bbox.x_max, bbox.y_max]
                };
                fields.push((
                    "bbox".to_string(),
                    Json::Array(bounds.into_iter().map(Json::Real).collect()),
                ));
            }
            if !column.projjson.is_empty() {
                match json::parse(&column.projjson) {
                    Ok(crs) => fields.push(("crs".to_string(), crs)),
                    // GeoParquet V2 allows CRSs that aren't PROJJSON, which are left out here
                    Err(_) if self.version == GeoParquetVersion::V2 => {}
                    Err(_) => {
                        return Err(Error::new(
                            ErrorKind::Serialization,
                            format!("Failed to parse CRS PROJJSON for column '{}'", column.name),
                        ));
                    }
                }
            }
            columns.push((column.name.clone(), Json::Object(fields)));
        }
        let root = Json::Object(vec![
            ("version".to_string(), Json::Str(version.to_string())),
            ("primary_column".to_string(), Json::Str(primary)),
            ("columns".to_string(), Json::Object(columns)),
        ]);
        Ok(Some(root.text()))
    }
}

/// `ConvertCRS`: the CRS of a column as PROJJSON, none for the default of GeoParquet (`OGC:CRS84`).
fn convert_crs(column: &mut GeoParquetColumn, ty: &LogicalType, version: GeoParquetVersion) -> Result<()> {
    let Some(crs) = ty.crs() else {
        return Ok(());
    };
    if crs.identifier().eq_ignore_ascii_case("OGC:CRS84") {
        return Ok(());
    }
    if crs.kind == CrsType::Projjson {
        column.projjson = crs.definition.clone();
        return Ok(());
    }
    if let Some(lookup) = try_convert_projjson(crs) {
        column.projjson = lookup.definition;
        return Ok(());
    }
    if version != GeoParquetVersion::V2 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "Cannot write GeoParquet V1 metadata for column '{}': GeoParquet V1 only supports PROJJSON CRS \
                 definitions",
                column.name
            ),
        ));
    }
    column.projjson = crs.definition.clone();
    Ok(())
}

/// `GeoParquetFileMetadata::TryRead`: the GEOMETRY columns of the `geo` metadata of a file, each with its CRS
/// in PROJJSON (empty for none), or `None` if the metadata isn't JSON.
pub(crate) fn try_read(metadata: &str) -> Result<Option<Vec<(String, String)>>> {
    let invalid = |message: String| Error::new(ErrorKind::InvalidInput, message);
    let Ok(root) = json::parse(metadata) else {
        return Ok(None);
    };
    let Json::Object(root) = &root else {
        return Err(invalid("Geoparquet metadata is not an object".to_string()));
    };
    fn get<'a>(object: &'a [(String, Json)], key: &str) -> Option<&'a Json> {
        object.iter().find(|(name, _)| name == key).map(|(_, value)| value)
    }
    let Some(Json::Str(version)) = get(root, "version") else {
        return Err(invalid("Geoparquet metadata does not have a version".to_string()));
    };
    // a breaking 3.0 version, should there be one
    if version.starts_with('3') {
        return Err(invalid(format!("Geoparquet version {version} is not supported")));
    }
    let Some(Json::Object(columns)) = get(root, "columns") else {
        return Err(invalid(
            "Geoparquet metadata does not have a columns object".to_string(),
        ));
    };
    let mut result: Vec<(String, String)> = Vec::new();
    for (name, column) in columns {
        let Json::Object(column) = column else {
            return Err(invalid(format!("Geoparquet column '{name}' is not an object")));
        };
        let Some(Json::Str(encoding)) = get(column, "encoding") else {
            return Err(invalid(format!("Geoparquet column '{name}' does not have an encoding")));
        };
        if !matches!(
            encoding.as_str(),
            "WKB" | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring" | "multipolygon"
        ) {
            return Err(invalid(format!(
                "Geoparquet column '{name}' has an unsupported encoding"
            )));
        }
        if !matches!(get(column, "geometry_types"), Some(Json::Array(_))) {
            return Err(invalid(format!(
                "Geoparquet column '{name}' does not have geometry types"
            )));
        }
        let projjson = match get(column, "crs") {
            Some(Json::Null) => String::new(),
            Some(crs @ Json::Object(_)) => crs.text(),
            Some(_) => return Err(invalid(format!("Geoparquet column '{name}' has invalid CRS"))),
            // without a CRS, the default: OGC:CRS84
            None => try_convert_projjson(&duckrs_common::geometry_crs::Crs::parse("OGC:CRS84"))
                .map(|crs| crs.definition)
                .unwrap_or_default(),
        };
        // `geometry_columns[column_name]`: a name met twice is one column
        match result.iter_mut().find(|(existing, _)| existing == name) {
            Some(entry) => entry.1 = projjson,
            None => result.push((name.clone(), projjson)),
        }
    }
    Ok(Some(result))
}
