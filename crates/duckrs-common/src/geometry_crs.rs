// Ported from duckdb/src/common/types/geometry_crs.cpp and
// duckdb/src/catalog/default/default_coordinate_systems.cpp @ rust-port-baseline
//
//! The coordinate reference system of a `GEOMETRY(crs)` type (`CoordinateReferenceSystem`).

use std::sync::OnceLock;

/// `CoordinateReferenceSystemType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrsType {
    Invalid = 0,
    Srid = 1,
    AuthCode = 2,
    Projjson = 3,
    Wkt2 = 4,
}

/// `CoordinateReferenceSystem`.
#[derive(Debug, Clone, Eq)]
pub struct Crs {
    pub kind: CrsType,
    pub definition: String,
    identifier: String,
}

/// `CoordinateReferenceSystem::Equals`: the same identifier, or the same definition.
impl PartialEq for Crs {
    fn eq(&self, other: &Self) -> bool {
        (!self.identifier.is_empty() && self.identifier == other.identifier)
            || (self.kind == other.kind && self.definition == other.definition)
    }
}

/// Equal CRSs can differ in each field, so none of them can be hashed.
impl std::hash::Hash for Crs {
    fn hash<H: std::hash::Hasher>(&self, _state: &mut H) {}
}

/// What reads a PROJJSON definition: its identifier, if it has one, and its text written out
/// again. The JSON reader lives with the JSON functions, which set it.
pub type ProjjsonParser = fn(&str) -> Option<(String, String)>;

static PROJJSON_PARSER: OnceLock<ProjjsonParser> = OnceLock::new();

/// Sets the reader of PROJJSON definitions, once.
pub fn set_projjson_parser(parser: ProjjsonParser) {
    let _ = PROJJSON_PARSER.set(parser);
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

impl Crs {
    /// `CoordinateReferenceSystem(definition)` (`ParseDefinition`): one that fails to parse is invalid,
    /// as `Deserialize` makes it.
    pub fn parse(definition: &str) -> Self {
        Self::try_parse(definition).unwrap_or_else(|_| Self {
            kind: CrsType::Invalid,
            definition: String::new(),
            identifier: String::new(),
        })
    }

    /// `ParseDefinition`, with the error of a broken WKT2 definition.
    pub fn try_parse(definition: &str) -> std::result::Result<Self, String> {
        if definition.bytes().all(is_space) {
            return Ok(Self {
                kind: CrsType::Invalid,
                definition: definition.to_string(),
                identifier: String::new(),
            });
        }
        if let Some(parser) = PROJJSON_PARSER.get()
            && let Some((identifier, normalized)) = parser(definition)
        {
            return Ok(Self {
                kind: CrsType::Projjson,
                definition: normalized,
                identifier,
            });
        }
        if let Some(code) = auth_code(definition) {
            return Ok(Self {
                kind: CrsType::AuthCode,
                definition: code,
                identifier: String::new(),
            });
        }
        if let Some(crs) = wkt2(definition)? {
            return Ok(crs);
        }
        Ok(Self {
            kind: CrsType::Srid,
            definition: definition.to_string(),
            identifier: String::new(),
        })
    }

    /// `GetIdentifier`.
    pub fn identifier(&self) -> &str {
        if self.identifier.is_empty() {
            &self.definition
        } else {
            &self.identifier
        }
    }

    /// `IsComplete`: whether the definition is all of it, not a name for one.
    pub fn is_complete(&self) -> bool {
        matches!(self.kind, CrsType::Projjson | CrsType::Wkt2)
    }

    /// Whether there is one (a GEOMETRY of an invalid CRS has none).
    pub fn is_valid(&self) -> bool {
        self.kind != CrsType::Invalid
    }
}

/// `TryParseAuthCode`: `AUTHORITY:CODE`, the authority letters and the code letters and digits.
fn auth_code(text: &str) -> Option<String> {
    let trimmed = text.trim_matches(|c: char| c.is_ascii() && is_space(c as u8));
    let (authority, code) = trimmed.split_once(':')?;
    (authority.bytes().all(|c| c.is_ascii_alphabetic()) && code.bytes().all(|c| c.is_ascii_alphanumeric()))
        .then(|| trimmed.to_string())
}

// ---------------------------------------------------------------------------------------------------------------
// WKT2:2019
// ---------------------------------------------------------------------------------------------------------------

enum Wkt {
    Keyword(String, Vec<Wkt>),
    Number(f64),
    Str(String),
}

/// `WKTParser`: a failure is no WKT2, as it is in C++ when parsing throws.
struct WktParser<'a> {
    text: &'a [u8],
    pos: usize,
    depth: u32,
}

impl WktParser<'_> {
    fn skip_whitespace(&mut self) {
        while self.pos < self.text.len() && is_space(self.text[self.pos]) {
            self.pos += 1;
        }
    }

    fn try_match(&mut self, c: u8) -> bool {
        if self.text.get(self.pos) == Some(&c) {
            self.pos += 1;
            self.skip_whitespace();
            return true;
        }
        false
    }

    /// `Match`, whose failure is an error of the CRS.
    fn expect(&mut self, c: u8) -> std::result::Result<(), String> {
        if self.try_match(c) {
            return Ok(());
        }
        Err(match self.text.get(self.pos) {
            None => format!("Expected '{}' but got end of input at position {}", c as char, self.pos),
            Some(&got) => format!(
                "Expected '{}' but got '{}' at position {}",
                c as char, got as char, self.pos
            ),
        })
    }

    /// `ParseNode`: a string, a keyword or a number, or `None` for none of those.
    fn node(&mut self) -> std::result::Result<Option<Wkt>, String> {
        self.depth += 1;
        if self.depth > 1001 {
            return Err("WKT input is too deeply nested to parse".to_string());
        }
        if self.try_match(b'"') {
            let start = self.pos;
            let mut result = Vec::new();
            loop {
                let Some(&c) = self.text.get(self.pos) else {
                    return Err(format!("Unterminated string starting at position {start}"));
                };
                if c == b'"' {
                    if self.text.get(self.pos + 1) == Some(&b'"') {
                        result.push(b'"');
                        self.pos += 2;
                        continue;
                    }
                    break;
                }
                result.push(c);
                self.pos += 1;
            }
            self.expect(b'"')?;
            return Ok(Some(Wkt::Str(String::from_utf8_lossy(&result).into_owned())));
        }
        let start = self.pos;
        if self
            .text
            .get(self.pos)
            .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
        {
            self.pos += 1;
            while self
                .text
                .get(self.pos)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
            {
                self.pos += 1;
            }
            let name = String::from_utf8_lossy(&self.text[start..self.pos]).into_owned();
            self.skip_whitespace();
            let mut children = Vec::new();
            for (open, close) in [(b'[', b']'), (b'(', b')')] {
                if self.try_match(open) {
                    while let Some(child) = self.node()? {
                        children.push(child);
                        if !self.try_match(b',') {
                            break;
                        }
                    }
                    self.expect(close)?;
                    break;
                }
            }
            return Ok(Some(Wkt::Keyword(name, children)));
        }
        let rest = &self.text[self.pos..];
        let mut end = 0;
        if end < rest.len() && (rest[end] == b'-' || rest[end] == b'+') {
            end += 1;
        }
        let digits = end;
        while end < rest.len() && (rest[end].is_ascii_digit() || rest[end] == b'.') {
            end += 1;
        }
        if end == digits {
            return Ok(None);
        }
        if end < rest.len() && (rest[end] == b'e' || rest[end] == b'E') {
            let mut exp = end + 1;
            if exp < rest.len() && (rest[exp] == b'-' || rest[exp] == b'+') {
                exp += 1;
            }
            let exp_digits = exp;
            while exp < rest.len() && rest[exp].is_ascii_digit() {
                exp += 1;
            }
            if exp > exp_digits {
                end = exp;
            }
        }
        let Some(value) = std::str::from_utf8(&rest[..end]).ok().and_then(|t| t.parse().ok()) else {
            return Ok(None);
        };
        self.pos += end;
        self.skip_whitespace();
        Ok(Some(Wkt::Number(value)))
    }
}

/// `TryExtractIDFromWKTNode`: `AUTHORITY:CODE` of an `ID[...]` child.
fn wkt_id(children: &[Wkt]) -> Option<String> {
    for child in children {
        let Wkt::Keyword(name, id) = child else { continue };
        if !name.eq_ignore_ascii_case("ID") || id.len() < 2 {
            continue;
        }
        let Wkt::Str(authority) = &id[0] else { continue };
        match &id[1] {
            Wkt::Number(number) => {
                if *number < 0.0 || *number >= u64::MAX as f64 {
                    continue;
                }
                return Some(format!("{authority}:{}", *number as u64));
            }
            Wkt::Str(code) => return Some(format!("{authority}:{code}")),
            Wkt::Keyword(..) => return None,
        }
    }
    None
}

const WKT2_KEYWORDS: [&str; 14] = [
    "BOUNDCRS",
    "COMPOUNDCRS",
    "ENGCRS",
    "ENGINEERINGCRS",
    "GEODCRS",
    "GEODETICCRS",
    "GEOGCRS",
    "GEOGRAPHICCRS",
    "PARAMETRICCRS",
    "PROJCRS",
    "PROJECTEDCRS",
    "TIMECRS",
    "VERTCRS",
    "VERTICALCRS",
];

/// `TryParseWKT2`: `Ok(None)` for text that isn't a WKT2 CRS, an error for one that is broken.
fn wkt2(text: &str) -> std::result::Result<Option<Crs>, String> {
    let mut parser = WktParser {
        text: text.as_bytes(),
        pos: 0,
        depth: 0,
    };
    parser.skip_whitespace();
    let Some(Wkt::Keyword(name, children)) = parser.node()? else {
        return Ok(None);
    };
    Ok(wkt2_crs(text, &name, &children))
}

fn wkt2_crs(text: &str, name: &str, children: &[Wkt]) -> Option<Crs> {
    if !WKT2_KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(name)) {
        return None;
    }
    let first = children.first()?;
    let identifier = if name.eq_ignore_ascii_case("BOUNDCRS") {
        let Wkt::Keyword(_, transform) = children.get(2)? else {
            return None;
        };
        let Wkt::Str(transform_name) = transform.first()? else {
            return None;
        };
        wkt_id(transform).unwrap_or_else(|| transform_name.clone())
    } else {
        let Wkt::Str(first_name) = first else {
            return None;
        };
        wkt_id(children).unwrap_or_else(|| first_name.clone())
    };
    Some(Crs {
        kind: CrsType::Wkt2,
        definition: text
            .trim_matches(|c: char| c.is_ascii() && is_space(c as u8))
            .to_string(),
        identifier,
    })
}

// ---------------------------------------------------------------------------------------------------------------
// The known coordinate systems
// ---------------------------------------------------------------------------------------------------------------

/// `DefaultCoordinateReferenceSystem`: the name, authority and code of those the catalog has.
const DEFAULTS: [(&str, &str, &str); 2] = [("OGC:CRS84", "OGC", "CRS84"), ("OGC:CRS83", "OGC", "CRS83")];

/// `OGC_CRS84_PROJJSON` and `OGC_CRS83_PROJJSON`: the PROJJSON definitions of [`DEFAULTS`], by name.
const DEFAULT_PROJJSON: [(&str, &str); 2] = [
    (
        "OGC:CRS84",
        r#"{"$schema":"https://proj.org/schemas/v0.7/projjson.schema.json","type":"GeographicCRS","name":"WGS 84 (CRS84)","datum_ensemble":{"name":"World Geodetic System 1984 ensemble","members":[{"name":"World Geodetic System 1984 (Transit)","id":{"authority":"EPSG","code":1166}},{"name":"World Geodetic System 1984 (G730)","id":{"authority":"EPSG","code":1152}},{"name":"World Geodetic System 1984 (G873)","id":{"authority":"EPSG","code":1153}},{"name":"World Geodetic System 1984 (G1150)","id":{"authority":"EPSG","code":1154}},{"name":"World Geodetic System 1984 (G1674)","id":{"authority":"EPSG","code":1155}},{"name":"World Geodetic System 1984 (G1762)","id":{"authority":"EPSG","code":1156}},{"name":"World Geodetic System 1984 (G2139)","id":{"authority":"EPSG","code":1309}},{"name":"World Geodetic System 1984 (G2296)","id":{"authority":"EPSG","code":1383}}],"ellipsoid":{"name":"WGS 84","semi_major_axis":6378137,"inverse_flattening":298.257223563},"accuracy":"2.0","id":{"authority":"EPSG","code":6326}},"coordinate_system":{"subtype":"ellipsoidal","axis":[{"name":"Geodetic longitude","abbreviation":"Lon","direction":"east","unit":"degree"},{"name":"Geodetic latitude","abbreviation":"Lat","direction":"north","unit":"degree"}]},"scope":"Not known.","area":"World.","bbox":{"south_latitude":-90,"west_longitude":-180,"north_latitude":90,"east_longitude":180},"id":{"authority":"OGC","code":"CRS84"}}"#,
    ),
    (
        "OGC:CRS83",
        r#"{"$schema":"https://proj.org/schemas/v0.7/projjson.schema.json","type":"GeographicCRS","name":"NAD83 (CRS83)","datum":{"type":"GeodeticReferenceFrame","name":"North American Datum 1983","ellipsoid":{"name":"GRS 1980","semi_major_axis":6378137,"inverse_flattening":298.257222101}},"coordinate_system":{"subtype":"ellipsoidal","axis":[{"name":"Geodetic longitude","abbreviation":"Lon","direction":"east","unit":"degree"},{"name":"Geodetic latitude","abbreviation":"Lat","direction":"north","unit":"degree"}]},"scope":"Not known.","area":"North America - onshore and offshore: Canada - Alberta; British Columbia; Manitoba; New Brunswick; Newfoundland and Labrador; Northwest Territories; Nova Scotia; Nunavut; Ontario; Prince Edward Island; Quebec; Saskatchewan; Yukon. Puerto Rico. United States (USA) - Alabama; Alaska; Arizona; Arkansas; California; Colorado; Connecticut; Delaware; Florida; Georgia; Hawaii; Idaho; Illinois; Indiana; Iowa; Kansas; Kentucky; Louisiana; Maine; Maryland; Massachusetts; Michigan; Minnesota; Mississippi; Missouri; Montana; Nebraska; Nevada; New Hampshire; New Jersey; New Mexico; New York; North Carolina; North Dakota; Ohio; Oklahoma; Oregon; Pennsylvania; Rhode Island; South Carolina; South Dakota; Tennessee; Texas; Utah; Vermont; Virginia; Washington; West Virginia; Wisconsin; Wyoming. US Virgin Islands. British Virgin Islands.","bbox":{"south_latitude":14.92,"west_longitude":167.65,"north_latitude":86.45,"east_longitude":-40.73},"id":{"authority":"OGC","code":"CRS83"}}"#,
    ),
];

/// `CoordinateReferenceSystem::TryConvert` to `PROJJSON`: a PROJJSON CRS as it is, a known one by
/// its definition, or `None` for one the catalog doesn't have a PROJJSON definition of.
pub fn try_convert_projjson(source: &Crs) -> Option<Crs> {
    if source.kind == CrsType::Projjson && source.is_complete() {
        return Some(source.clone());
    }
    let (_, definition) = DEFAULT_PROJJSON
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(source.identifier()))?;
    Some(Crs::parse(definition))
}

/// `CoordinateReferenceSystem::TryIdentify`: the shortest form of a known CRS, a complete one as it
/// is, or `None` for one nothing can tell.
pub fn try_identify(text: &str) -> std::result::Result<Option<Crs>, String> {
    let source = Crs::try_parse(text)?;
    if !source.is_valid() {
        return Ok(None);
    }
    if let Some((_, authority, code)) = DEFAULTS
        .iter()
        .find(|(name, _, _)| name.eq_ignore_ascii_case(source.identifier()))
    {
        return Ok(Some(Crs::parse(&format!("{authority}:{code}"))));
    }
    Ok(source.is_complete().then_some(source))
}
