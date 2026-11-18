// Ported from duckdb/src/common/types/geometry.cpp @ rust-port-baseline
//
//! GEOMETRY values. They are WKB, always little endian and in ISO form (Z and M by adding 1000
//! and 2000 to the type), which is what `FromString` writes and `FromBinary` turns other WKB into.

use crate::{Error, ErrorKind, Result};

/// `Geometry::MAX_RECURSION_DEPTH`.
pub const MAX_RECURSION_DEPTH: usize = 16;

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

/// `GeometryType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryType {
    Point = 1,
    LineString = 2,
    Polygon = 3,
    MultiPoint = 4,
    MultiLineString = 5,
    MultiPolygon = 6,
    GeometryCollection = 7,
}

impl GeometryType {
    fn from_id(id: u32) -> Option<Self> {
        Some(match id {
            1 => Self::Point,
            2 => Self::LineString,
            3 => Self::Polygon,
            4 => Self::MultiPoint,
            5 => Self::MultiLineString,
            6 => Self::MultiPolygon,
            7 => Self::GeometryCollection,
            _ => return None,
        })
    }

    /// The upper case name, as WKT and `ST_GeometryType` spell it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Point => "POINT",
            Self::LineString => "LINESTRING",
            Self::Polygon => "POLYGON",
            Self::MultiPoint => "MULTIPOINT",
            Self::MultiLineString => "MULTILINESTRING",
            Self::MultiPolygon => "MULTIPOLYGON",
            Self::GeometryCollection => "GEOMETRYCOLLECTION",
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Reading WKB
// ---------------------------------------------------------------------------------------------------------------

/// `BlobReader`.
struct BlobReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BlobReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        let bytes = self
            .data
            .get(self.pos..self.pos + N)
            .ok_or_else(|| invalid(format!("Unexpected end of binary data at position {}", self.pos)))?;
        self.pos += N;
        Ok(bytes.try_into().expect("N bytes"))
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes::<1>()?[0])
    }

    fn u32(&mut self, le: bool) -> Result<u32> {
        let bytes = self.bytes::<4>()?;
        Ok(if le {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    }

    fn f64(&mut self, le: bool) -> Result<f64> {
        let bytes = self.bytes::<8>()?;
        Ok(if le {
            f64::from_le_bytes(bytes)
        } else {
            f64::from_be_bytes(bytes)
        })
    }

    fn skip(&mut self, size: usize) -> Result<()> {
        if self.pos.checked_add(size).is_none_or(|end| end > self.data.len()) {
            return Err(invalid(format!(
                "Skipping beyond end of binary data at position {}",
                self.pos
            )));
        }
        self.pos += size;
        Ok(())
    }

    fn at_end(&self) -> bool {
        self.pos >= self.data.len()
    }
}

/// What `AnalyzeWKB` tells of a WKB.
#[derive(Default)]
struct Analysis {
    any_be: bool,
    any_ewkb: bool,
    any_unknown: bool,
    trailing: bool,
}

/// A header: `(little endian, type id, flag id, has z, has m, has srid, any EWKB flag)`.
#[allow(clippy::type_complexity)]
fn read_header(reader: &mut BlobReader<'_>) -> Result<(bool, u32, u32, bool, bool, bool, bool)> {
    let le = reader.u8()? == 1;
    let meta = reader.u32(le)?;
    let type_id = (meta & 0xFFFF) % 1000;
    let flag_id = (meta & 0xFFFF) / 1000;
    let ext_z = meta & 0x8000_0000 != 0;
    let ext_m = meta & 0x4000_0000 != 0;
    let srid = meta & 0x2000_0000 != 0;
    let has_z = flag_id & 1 != 0 || ext_z;
    let has_m = flag_id & 2 != 0 || ext_m;
    Ok((le, type_id, flag_id, has_z, has_m, srid, srid || ext_z || ext_m))
}

/// `AnalyzeWKB`.
fn analyze(reader: &mut BlobReader<'_>) -> Result<Analysis> {
    let mut result = Analysis::default();
    // (parts remaining, the type the parts must have or 0)
    let mut collections: Vec<(u32, u32)> = Vec::new();
    let mut root = true;
    let (mut root_z, mut root_m) = (false, false);
    while root || !collections.is_empty() {
        let mut required = 0;
        if !root {
            let collection = collections.last_mut().expect("not empty");
            if collection.0 == 0 {
                collections.pop();
                continue;
            }
            collection.0 -= 1;
            required = collection.1;
            if collections.len() >= MAX_RECURSION_DEPTH {
                return Err(invalid(format!(
                    "Geometry exceeds maximum recursion depth of {MAX_RECURSION_DEPTH}"
                )));
            }
        }
        let (le, type_id, flag_id, has_z, has_m, srid, extended) = read_header(reader)?;
        if flag_id > 3 {
            return Err(invalid(format!("Unsupported geometry flag {flag_id} in WKB")));
        }
        if root {
            root_z = has_z;
            root_m = has_m;
            root = false;
        } else {
            if has_z != root_z || has_m != root_m {
                return Err(invalid(format!(
                    "Geometry has inconsistent Z/M dimensions, starting at position {}",
                    reader.pos
                )));
            }
            if required != 0 && type_id != required {
                return Err(invalid(format!(
                    "Unexpected geometry type {type_id} in a collection of type {} at position {}",
                    required + 3,
                    reader.pos
                )));
            }
        }
        if srid {
            reader.skip(4)?;
        }
        result.any_ewkb |= extended;
        result.any_be |= !le;
        let vertex = (2 + usize::from(has_z) + usize::from(has_m)) * 8;
        match type_id {
            1 => reader.skip(vertex)?,
            2 => {
                let count = reader.u32(le)? as usize;
                reader.skip(count.saturating_mul(vertex))?;
            }
            3 => {
                let rings = reader.u32(le)?;
                for _ in 0..rings {
                    let count = reader.u32(le)? as usize;
                    reader.skip(count.saturating_mul(vertex))?;
                }
            }
            4..=7 => {
                let parts = reader.u32(le)?;
                collections.push((parts, if type_id == 7 { 0 } else { type_id - 3 }));
            }
            _ => {
                result.any_unknown = true;
                return Ok(result);
            }
        }
    }
    result.trailing = !reader.at_end();
    Ok(result)
}

/// `ConvertWKB`: the WKB again, little endian and without the EWKB flags and SRIDs.
fn convert(reader: &mut BlobReader<'_>, out: &mut Vec<u8>) -> Result<()> {
    while !reader.at_end() {
        let le = reader.u8()? == 1;
        let meta = reader.u32(le)?;
        let type_id = (meta & 0xFFFF) % 1000;
        let flag_id = (meta & 0xFFFF) / 1000;
        let has_z = flag_id & 1 != 0 || meta & 0x8000_0000 != 0;
        let has_m = flag_id & 2 != 0 || meta & 0x4000_0000 != 0;
        if meta & 0x2000_0000 != 0 {
            reader.skip(4)?;
        }
        let width = 2 + usize::from(has_z) + usize::from(has_m);
        out.push(1);
        out.extend_from_slice(&(type_id + 1000 * u32::from(has_z) + 2000 * u32::from(has_m)).to_le_bytes());
        let vertices = |reader: &mut BlobReader<'_>, out: &mut Vec<u8>, count: usize| -> Result<()> {
            for _ in 0..count * width {
                out.extend_from_slice(&reader.f64(le)?.to_le_bytes());
            }
            Ok(())
        };
        match type_id {
            1 => vertices(reader, out, 1)?,
            2 => {
                let count = reader.u32(le)?;
                out.extend_from_slice(&count.to_le_bytes());
                vertices(reader, out, count as usize)?;
            }
            3 => {
                let rings = reader.u32(le)?;
                out.extend_from_slice(&rings.to_le_bytes());
                for _ in 0..rings {
                    let count = reader.u32(le)?;
                    out.extend_from_slice(&count.to_le_bytes());
                    vertices(reader, out, count as usize)?;
                }
            }
            _ => {
                let parts = reader.u32(le)?;
                out.extend_from_slice(&parts.to_le_bytes());
            }
        }
    }
    Ok(())
}

/// `Geometry::FromBinary`: a GEOMETRY of any WKB or EWKB, or the error that says why it isn't one.
pub fn from_wkb(wkb: &[u8]) -> Result<Vec<u8>> {
    let mut reader = BlobReader::new(wkb);
    let analysis = analyze(&mut reader)?;
    if analysis.trailing {
        return Err(invalid(format!("Unexpected trailing data at position {}", reader.pos)));
    }
    if analysis.any_unknown {
        return Err(invalid("Unsupported geometry type in WKB"));
    }
    if analysis.any_be || analysis.any_ewkb {
        let mut out = Vec::with_capacity(wkb.len());
        convert(&mut BlobReader::new(wkb), &mut out)?;
        return Ok(out);
    }
    Ok(wkb.to_vec())
}

// ---------------------------------------------------------------------------------------------------------------
// WKT
// ---------------------------------------------------------------------------------------------------------------

/// A failure to read WKT (`TextReader::SetError`), at a byte offset of the text.
#[derive(Debug, Clone)]
pub struct WktError {
    pub message: String,
    pub offset: usize,
    /// whether all of a geometry was read, and only text after it was left
    pub trailing: bool,
}

impl WktError {
    /// `TextReader::GetErrorMessage`.
    pub fn full_message(&self) -> String {
        format!("Failed to parse geometry: {} at offset {}", self.message, self.offset)
    }
}

/// `TextReader`.
struct TextReader<'a> {
    text: &'a [u8],
    pos: usize,
    error: Option<(String, usize)>,
}

type Parse<T> = std::result::Result<T, ()>;

impl TextReader<'_> {
    fn fail<T>(&mut self, message: impl Into<String>) -> Parse<T> {
        self.error = Some((message.into(), self.pos));
        Err(())
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.text.len() && matches!(self.text[self.pos], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
            self.pos += 1;
        }
    }

    fn try_match(&mut self, word: &str) -> bool {
        let word = word.as_bytes();
        let end = self.pos + word.len();
        if end <= self.text.len() && self.text[self.pos..end].eq_ignore_ascii_case(word) {
            self.pos = end;
            self.skip_whitespace();
            return true;
        }
        false
    }

    fn expect(&mut self, c: u8) -> Parse<()> {
        if self.try_match(std::str::from_utf8(&[c]).unwrap_or("")) {
            return Ok(());
        }
        match self.text.get(self.pos) {
            None => self.fail(format!("Expected '{}' but got end of input", c as char)),
            Some(&got) => self.fail(format!("Expected '{}' but got '{}'", c as char, got as char)),
        }
    }

    /// `MatchNumber`, as `fast_float::from_chars` reads one.
    fn number(&mut self) -> Parse<f64> {
        let rest = &self.text[self.pos..];
        let mut end = 0;
        let len = rest.len();
        if end < len && (rest[end] == b'-' || rest[end] == b'+') {
            end += 1;
        }
        let lower: Vec<u8> = rest[end..rest.len().min(end + 8)].to_ascii_lowercase();
        let special = if lower.starts_with(b"infinity") {
            Some(8)
        } else if lower.starts_with(b"inf") || lower.starts_with(b"nan") {
            Some(3)
        } else {
            None
        };
        if let Some(size) = special {
            end += size;
        } else {
            let digits_start = end;
            while end < len && rest[end].is_ascii_digit() {
                end += 1;
            }
            let mut digits = end - digits_start;
            if end < len && rest[end] == b'.' {
                end += 1;
                let fraction_start = end;
                while end < len && rest[end].is_ascii_digit() {
                    end += 1;
                }
                digits += end - fraction_start;
            }
            if digits == 0 {
                return self.fail("Expected number");
            }
            if end < len && (rest[end] == b'e' || rest[end] == b'E') {
                let mut exp_end = end + 1;
                if exp_end < len && (rest[exp_end] == b'-' || rest[exp_end] == b'+') {
                    exp_end += 1;
                }
                let exp_digits = exp_end;
                while exp_end < len && rest[exp_end].is_ascii_digit() {
                    exp_end += 1;
                }
                if exp_end > exp_digits {
                    end = exp_end;
                }
            }
        }
        let text = std::str::from_utf8(&rest[..end]).unwrap_or("");
        let value = match text.trim_start_matches('+').to_ascii_lowercase().as_str() {
            "nan" | "-nan" => f64::NAN,
            t => match t.parse::<f64>() {
                Ok(v) => v,
                Err(_) => return self.fail("Expected number"),
            },
        };
        self.pos += end;
        self.skip_whitespace();
        Ok(value)
    }

    fn at_end(&mut self) -> bool {
        self.skip_whitespace();
        self.pos >= self.text.len()
    }
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_f64(out: &mut Vec<u8>, value: f64) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// A reserved count, filled in once the parts are read.
fn reserve(out: &mut Vec<u8>) -> usize {
    out.extend_from_slice(&[0; 4]);
    out.len() - 4
}

fn fill(out: &mut [u8], at: usize, value: u32) {
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn header(out: &mut Vec<u8>, type_id: u32, has_z: bool, has_m: bool) {
    out.push(1);
    put_u32(
        out,
        type_id + if has_z { 1000 } else { 0 } + if has_m { 2000 } else { 0 },
    );
}

fn vertex(reader: &mut TextReader<'_>, out: &mut Vec<u8>, dims: usize) -> Parse<()> {
    for _ in 0..dims {
        let value = reader.number()?;
        put_f64(out, value);
    }
    Ok(())
}

/// `(vertex, vertex, ...)`, after the opening parenthesis is expected: the vertex count.
fn vertices(reader: &mut TextReader<'_>, out: &mut Vec<u8>, dims: usize) -> Parse<()> {
    let count = reserve(out);
    reader.expect(b'(')?;
    let mut n = 0;
    loop {
        vertex(reader, out, dims)?;
        n += 1;
        if !reader.try_match(",") {
            break;
        }
    }
    reader.expect(b')')?;
    fill(out, count, n);
    Ok(())
}

/// `((ring), (ring), ...)`.
fn rings(reader: &mut TextReader<'_>, out: &mut Vec<u8>, dims: usize) -> Parse<()> {
    let count = reserve(out);
    reader.expect(b'(')?;
    let mut n = 0;
    loop {
        vertices(reader, out, dims)?;
        n += 1;
        if !reader.try_match(",") {
            break;
        }
    }
    reader.expect(b')')?;
    fill(out, count, n);
    Ok(())
}

/// `FromStringRecursive`.
fn parse_geometry(reader: &mut TextReader<'_>, out: &mut Vec<u8>, depth: usize, parent: (bool, bool)) -> Parse<()> {
    if depth == MAX_RECURSION_DEPTH {
        return reader.fail(format!(
            "Geometry string exceeds maximum recursion depth of {MAX_RECURSION_DEPTH}"
        ));
    }
    reader.skip_whitespace();
    // EWKT: an SRID is read and dropped
    if reader.try_match("SRID") {
        reader.expect(b'=')?;
        reader.number()?;
        reader.expect(b';')?;
    }
    let kinds = [
        ("point", GeometryType::Point),
        ("linestring", GeometryType::LineString),
        ("polygon", GeometryType::Polygon),
        ("multipoint", GeometryType::MultiPoint),
        ("multilinestring", GeometryType::MultiLineString),
        ("multipolygon", GeometryType::MultiPolygon),
        ("geometrycollection", GeometryType::GeometryCollection),
    ];
    let Some(kind) = kinds
        .iter()
        .find(|(name, _)| reader.try_match(name))
        .map(|(_, kind)| *kind)
    else {
        return reader.fail("Unknown geometry type");
    };
    let has_z = reader.try_match("z");
    let has_m = reader.try_match("m");
    let empty = reader.try_match("empty");
    if depth != 0 && parent != (has_z, has_m) {
        return reader.fail("Geometry has inconsistent Z/M dimensions");
    }
    let dims = 2 + usize::from(has_z) + usize::from(has_m);
    header(out, kind as u32, has_z, has_m);
    match kind {
        GeometryType::Point => {
            if empty {
                (0..dims).for_each(|_| put_f64(out, f64::NAN));
            } else {
                reader.expect(b'(')?;
                vertex(reader, out, dims)?;
                reader.expect(b')')?;
            }
        }
        GeometryType::LineString => {
            if empty {
                put_u32(out, 0);
            } else {
                vertices(reader, out, dims)?;
            }
        }
        GeometryType::Polygon => {
            if empty {
                put_u32(out, 0);
            } else {
                rings(reader, out, dims)?;
            }
        }
        GeometryType::MultiPoint => {
            if empty {
                put_u32(out, 0);
            } else {
                let count = reserve(out);
                reader.expect(b'(')?;
                let mut n = 0;
                loop {
                    let paren = reader.try_match("(");
                    header(out, GeometryType::Point as u32, has_z, has_m);
                    if reader.try_match("EMPTY") {
                        (0..dims).for_each(|_| put_f64(out, f64::NAN));
                    } else {
                        vertex(reader, out, dims)?;
                    }
                    if paren {
                        reader.expect(b')')?;
                    }
                    n += 1;
                    if !reader.try_match(",") {
                        break;
                    }
                }
                reader.expect(b')')?;
                fill(out, count, n);
            }
        }
        GeometryType::MultiLineString | GeometryType::MultiPolygon => {
            if empty {
                put_u32(out, 0);
            } else {
                let part = if kind == GeometryType::MultiLineString {
                    GeometryType::LineString
                } else {
                    GeometryType::Polygon
                };
                let count = reserve(out);
                reader.expect(b'(')?;
                let mut n = 0;
                loop {
                    header(out, part as u32, has_z, has_m);
                    if reader.try_match("EMPTY") {
                        put_u32(out, 0);
                    } else if part == GeometryType::LineString {
                        vertices(reader, out, dims)?;
                    } else {
                        rings(reader, out, dims)?;
                    }
                    n += 1;
                    if !reader.try_match(",") {
                        break;
                    }
                }
                reader.expect(b')')?;
                fill(out, count, n);
            }
        }
        GeometryType::GeometryCollection => {
            if empty {
                put_u32(out, 0);
            } else {
                let count = reserve(out);
                reader.expect(b'(')?;
                let mut n = 0;
                loop {
                    parse_geometry(reader, out, depth + 1, (has_z, has_m))?;
                    n += 1;
                    if !reader.try_match(",") {
                        break;
                    }
                }
                reader.expect(b')')?;
                fill(out, count, n);
            }
        }
    }
    Ok(())
}

/// `Geometry::TryFromString`: the GEOMETRY of a WKT or EWKT text.
pub fn from_wkt(text: &str) -> std::result::Result<Vec<u8>, WktError> {
    let mut reader = TextReader {
        text: text.as_bytes(),
        pos: 0,
        error: None,
    };
    let mut out = Vec::new();
    let parsed = parse_geometry(&mut reader, &mut out, 0, (false, false));
    let trailing = parsed.is_ok() && !reader.at_end();
    if trailing {
        let _ = reader.fail::<()>("Unexpected trailing text");
    }
    match reader.error {
        Some((message, offset)) => Err(WktError {
            message,
            offset,
            trailing,
        }),
        None => Ok(out),
    }
}

/// `TextWriter::Write(double)`: fmt's shortest digits, without a trailing `.0`.
fn write_number(out: &mut String, value: f64) {
    if value.is_nan() {
        out.push_str("nan");
        return;
    }
    if value.is_infinite() {
        out.push_str(if value < 0.0 { "-inf" } else { "inf" });
        return;
    }
    let text = crate::fmt_float::format_finite(value, false);
    if !text.contains(['e', 'E']) && text.ends_with('0') {
        let mut text = text;
        text.pop();
        if text.ends_with('.') {
            text.pop();
        }
        out.push_str(&text);
    } else {
        out.push_str(&text);
    }
}

struct WktWriter<'a, 'b> {
    reader: &'a mut BlobReader<'b>,
    out: String,
}

impl WktWriter<'_, '_> {
    fn coordinates(&mut self, dims: usize) -> Result<()> {
        for d in 0..dims {
            if d > 0 {
                self.out.push(' ');
            }
            let value = self.reader.f64(true)?;
            write_number(&mut self.out, value);
        }
        Ok(())
    }

    /// The vertices of a LINESTRING or a ring, or EMPTY.
    fn vertex_list(&mut self, dims: usize) -> Result<()> {
        let count = self.reader.u32(true)?;
        if count == 0 {
            self.out.push_str("EMPTY");
            return Ok(());
        }
        self.out.push('(');
        for i in 0..count {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.coordinates(dims)?;
        }
        self.out.push(')');
        Ok(())
    }

    fn ring_list(&mut self, dims: usize) -> Result<()> {
        let count = self.reader.u32(true)?;
        if count == 0 {
            self.out.push_str("EMPTY");
            return Ok(());
        }
        self.out.push('(');
        for i in 0..count {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.vertex_list(dims)?;
        }
        self.out.push(')');
        Ok(())
    }

    /// The header of a part of a MULTI geometry, which must be a `part` of the same dimensions.
    fn part_header(&mut self, part: GeometryType, multi: GeometryType, dims: (bool, bool)) -> Result<()> {
        let order = self.reader.u8()?;
        if order != 1 {
            return Err(invalid(format!("Unsupported byte order {order} in WKB")));
        }
        let meta = self.reader.u32(true)?;
        let part_type = (meta & 0xFFFF) % 1000;
        let flag = (meta & 0xFFFF) / 1000;
        if part_type != part as u32 {
            return Err(invalid(format!(
                "Expected {} in {} but got {part_type}",
                part.name(),
                multi.name()
            )));
        }
        if dims != (flag & 1 != 0, flag & 2 != 0) {
            return Err(invalid(format!(
                "Geometry has inconsistent Z/M dimensions in {}, starting at position {}",
                multi.name(),
                self.reader.pos
            )));
        }
        Ok(())
    }

    /// `ToStringRecursive`.
    fn geometry(&mut self, depth: usize, parent: (bool, bool)) -> Result<()> {
        if depth == MAX_RECURSION_DEPTH {
            return Err(invalid(format!(
                "Geometry exceeds maximum recursion depth of {MAX_RECURSION_DEPTH}"
            )));
        }
        let order = self.reader.u8()?;
        if order != 1 {
            return Err(invalid(format!("Unsupported byte order {order} in WKB")));
        }
        let meta = self.reader.u32(true)?;
        let type_id = (meta & 0xFFFF) % 1000;
        let flag = (meta & 0xFFFF) / 1000;
        let (has_z, has_m) = (flag & 1 != 0, flag & 2 != 0);
        if depth != 0 && parent != (has_z, has_m) {
            return Err(invalid(format!(
                "Geometry has inconsistent Z/M dimensions, starting at position {}",
                self.reader.pos
            )));
        }
        let kind = GeometryType::from_id(type_id)
            .ok_or_else(|| invalid(format!("Unsupported geometry type {type_id} in WKB")))?;
        let dims = 2 + usize::from(has_z) + usize::from(has_m);
        self.out.push_str(kind.name());
        self.out.push_str(match (has_z, has_m) {
            (true, true) => " ZM ",
            (true, false) => " Z ",
            (false, true) => " M ",
            (false, false) => " ",
        });
        match kind {
            GeometryType::Point => {
                let mut values = [0f64; 4];
                for value in values.iter_mut().take(dims) {
                    *value = self.reader.f64(true)?;
                }
                if values[..dims].iter().all(|v| v.is_nan()) {
                    self.out.push_str("EMPTY");
                    return Ok(());
                }
                self.out.push('(');
                for (d, value) in values[..dims].iter().enumerate() {
                    if d > 0 {
                        self.out.push(' ');
                    }
                    write_number(&mut self.out, *value);
                }
                self.out.push(')');
            }
            GeometryType::LineString => self.vertex_list(dims)?,
            GeometryType::Polygon => self.ring_list(dims)?,
            GeometryType::MultiPoint => {
                let parts = self.reader.u32(true)?;
                if parts == 0 {
                    self.out.push_str("EMPTY");
                    return Ok(());
                }
                self.out.push('(');
                for i in 0..parts {
                    self.part_header(GeometryType::Point, kind, (has_z, has_m))?;
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    let mut values = [0f64; 4];
                    for value in values.iter_mut().take(dims) {
                        *value = self.reader.f64(true)?;
                    }
                    if values[..dims].iter().all(|v| v.is_nan()) {
                        self.out.push_str("EMPTY");
                        continue;
                    }
                    for (d, value) in values[..dims].iter().enumerate() {
                        if d > 0 {
                            self.out.push(' ');
                        }
                        write_number(&mut self.out, *value);
                    }
                }
                self.out.push(')');
            }
            GeometryType::MultiLineString | GeometryType::MultiPolygon => {
                let part = if kind == GeometryType::MultiLineString {
                    GeometryType::LineString
                } else {
                    GeometryType::Polygon
                };
                let parts = self.reader.u32(true)?;
                if parts == 0 {
                    self.out.push_str("EMPTY");
                    return Ok(());
                }
                self.out.push('(');
                for i in 0..parts {
                    if kind == GeometryType::MultiPolygon && i > 0 {
                        self.out.push_str(", ");
                    }
                    self.part_header(part, kind, (has_z, has_m))?;
                    if kind == GeometryType::MultiLineString && i > 0 {
                        self.out.push_str(", ");
                    }
                    if part == GeometryType::LineString {
                        self.vertex_list(dims)?;
                    } else {
                        self.ring_list(dims)?;
                    }
                }
                self.out.push(')');
            }
            GeometryType::GeometryCollection => {
                let parts = self.reader.u32(true)?;
                if parts == 0 {
                    self.out.push_str("EMPTY");
                    return Ok(());
                }
                self.out.push('(');
                for i in 0..parts {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.geometry(depth + 1, (has_z, has_m))?;
                }
                self.out.push(')');
            }
        }
        Ok(())
    }
}

/// `Geometry::ToString`: the WKT of a GEOMETRY.
pub fn to_wkt(wkb: &[u8]) -> Result<String> {
    let mut reader = BlobReader::new(wkb);
    let mut writer = WktWriter {
        reader: &mut reader,
        out: String::new(),
    };
    writer.geometry(0, (false, false))?;
    Ok(writer.out)
}

/// `Geometry::GetType`: the type of a GEOMETRY and whether it has Z and M.
pub fn geometry_type(wkb: &[u8]) -> Result<(GeometryType, bool, bool)> {
    let mut reader = BlobReader::new(wkb);
    let order = reader.u8()?;
    if order != 1 {
        return Err(invalid(format!("Unsupported byte order {order} in WKB")));
    }
    let meta = reader.u32(true)?;
    let type_id = (meta & 0xFFFF) % 1000;
    let flag = (meta & 0xFFFF) / 1000;
    let kind =
        GeometryType::from_id(type_id).ok_or_else(|| invalid(format!("Unsupported geometry type {type_id} in WKB")))?;
    if flag > 3 {
        return Err(invalid(format!("Unsupported geometry flag {flag} in WKB")));
    }
    Ok((kind, flag & 1 != 0, flag & 2 != 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for (text, expected) in [
            ("POINT(0 1)", "POINT (0 1)"),
            ("LINESTRING(0 0, 1 1, 2 2)", "LINESTRING (0 0, 1 1, 2 2)"),
            (
                "POLYGON((0 0, 4 0, 4 4, 0 4, 0 0))",
                "POLYGON ((0 0, 4 0, 4 4, 0 4, 0 0))",
            ),
            ("MULTIPOINT((1 1), (2 2), (3 3))", "MULTIPOINT (1 1, 2 2, 3 3)"),
            ("MULTIPOINT(1 1, 2 2, 3 3)", "MULTIPOINT (1 1, 2 2, 3 3)"),
            (
                "MULTILINESTRING((0 0, 1 1), (2 2, 3 3))",
                "MULTILINESTRING ((0 0, 1 1), (2 2, 3 3))",
            ),
            (
                "MULTIPOLYGON(((0 0, 4 0, 4 4, 0 4, 0 0)), ((5 5, 7 5, 7 7, 5 7, 5 5)))",
                "MULTIPOLYGON (((0 0, 4 0, 4 4, 0 4, 0 0)), ((5 5, 7 5, 7 7, 5 7, 5 5)))",
            ),
            (
                "GEOMETRYCOLLECTION(POINT(1 1), LINESTRING(0 0, 1 1))",
                "GEOMETRYCOLLECTION (POINT (1 1), LINESTRING (0 0, 1 1))",
            ),
            ("POINT EMPTY", "POINT EMPTY"),
            ("POINT Z (1 2 3)", "POINT Z (1 2 3)"),
            ("POINT ZM (1 2 3 4)", "POINT ZM (1 2 3 4)"),
            ("POINT(1.5 -2.25)", "POINT (1.5 -2.25)"),
        ] {
            let wkb = from_wkt(text).unwrap();
            assert_eq!(to_wkt(&wkb).unwrap(), expected, "{text}");
            assert_eq!(from_wkb(&wkb).unwrap(), wkb);
        }
    }

    #[test]
    fn errors() {
        let error = from_wkt("POINT(1 2").unwrap_err();
        assert_eq!(
            error.full_message(),
            "Failed to parse geometry: Expected ')' but got end of input at offset 9"
        );
        assert!(from_wkt("POINT(1 2) x").unwrap_err().trailing);
    }
}

/// `GeometryExtent`: the bounds of the vertices, in X, Y, Z and M.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Extent {
    pub x_min: f64,
    pub y_min: f64,
    pub z_min: f64,
    pub m_min: f64,
    pub x_max: f64,
    pub y_max: f64,
    pub z_max: f64,
    pub m_max: f64,
}

/// `MinValue` and `MaxValue`: a NaN carries over, as in C++.
fn min_value(a: f64, b: f64) -> f64 {
    if a < b { a } else { b }
}

fn max_value(a: f64, b: f64) -> f64 {
    if a > b { a } else { b }
}

impl Extent {
    /// `GeometryExtent::Empty`: nothing intersects it, and merging it changes nothing.
    pub fn empty() -> Self {
        Self {
            x_min: f64::INFINITY,
            y_min: f64::INFINITY,
            z_min: f64::INFINITY,
            m_min: f64::INFINITY,
            x_max: f64::NEG_INFINITY,
            y_max: f64::NEG_INFINITY,
            z_max: f64::NEG_INFINITY,
            m_max: f64::NEG_INFINITY,
        }
    }

    /// `GeometryExtent::Unknown`: any value.
    pub fn unknown() -> Self {
        Self {
            x_min: f64::NEG_INFINITY,
            y_min: f64::NEG_INFINITY,
            z_min: f64::NEG_INFINITY,
            m_min: f64::NEG_INFINITY,
            x_max: f64::INFINITY,
            y_max: f64::INFINITY,
            z_max: f64::INFINITY,
            m_max: f64::INFINITY,
        }
    }

    /// `GeometryExtent::IntersectsXY`.
    pub fn intersects_xy(&self, other: &Extent) -> bool {
        !(self.x_min > other.x_max || self.x_max < other.x_min || self.y_min > other.y_max || self.y_max < other.y_min)
    }

    /// `GeometryExtent::Merge`.
    pub fn merge(&mut self, other: &Extent) {
        self.x_min = min_value(self.x_min, other.x_min);
        self.y_min = min_value(self.y_min, other.y_min);
        self.z_min = min_value(self.z_min, other.z_min);
        self.m_min = min_value(self.m_min, other.m_min);
        self.x_max = max_value(self.x_max, other.x_max);
        self.y_max = max_value(self.y_max, other.y_max);
        self.z_max = max_value(self.z_max, other.z_max);
        self.m_max = max_value(self.m_max, other.m_max);
    }

    /// `GeometryExtent::Extend` by a vertex, its Z and M if it has them.
    fn extend(&mut self, vertex: &[f64], has_z: bool, has_m: bool) {
        self.x_min = min_value(self.x_min, vertex[0]);
        self.y_min = min_value(self.y_min, vertex[1]);
        self.x_max = max_value(self.x_max, vertex[0]);
        self.y_max = max_value(self.y_max, vertex[1]);
        if has_z {
            self.z_min = min_value(self.z_min, vertex[2]);
            self.z_max = max_value(self.z_max, vertex[2]);
        }
        if has_m {
            let m = vertex[2 + usize::from(has_z)];
            self.m_min = min_value(self.m_min, m);
            self.m_max = max_value(self.m_max, m);
        }
    }
}

/// `Geometry::GetExtent`: grows `extent` by the vertices of a GEOMETRY, and gives how many
/// count towards it (an empty POINT doesn't) and whether any part of it is empty.
pub fn extend_extent(wkb: &[u8], extent: &mut Extent) -> Result<(u32, bool)> {
    let mut reader = BlobReader::new(wkb);
    let mut count = 0;
    let mut any_empty = false;
    while !reader.at_end() {
        let order = reader.u8()?;
        if order != 1 {
            return Err(invalid(format!("Unsupported byte order {order} in WKB")));
        }
        let meta = reader.u32(true)?;
        let type_id = (meta & 0xFFFF) % 1000;
        let flag = (meta & 0xFFFF) / 1000;
        if !(1..=7).contains(&type_id) {
            return Err(invalid(format!("Unsupported geometry type {type_id} in WKB")));
        }
        if flag > 3 {
            return Err(invalid(format!("Unsupported geometry flag {flag} in WKB")));
        }
        let (has_z, has_m) = (flag & 1 != 0, flag & 2 != 0);
        let dims = 2 + usize::from(has_z) + usize::from(has_m);
        let mut vertices = |reader: &mut BlobReader<'_>, n: u32, check_nan: bool| -> Result<u32> {
            let mut counted = 0;
            for _ in 0..n {
                let mut values = [0f64; 4];
                for value in values.iter_mut().take(dims) {
                    *value = reader.f64(true)?;
                }
                if check_nan && values[..dims].iter().all(|v| v.is_nan()) {
                    continue;
                }
                extent.extend(&values, has_z, has_m);
                counted += 1;
            }
            Ok(counted)
        };
        match type_id {
            1 => {
                let counted = vertices(&mut reader, 1, true)?;
                if counted == 0 {
                    any_empty = true;
                }
                count += counted;
            }
            2 => {
                let n = reader.u32(true)?;
                if n == 0 {
                    any_empty = true;
                }
                count += vertices(&mut reader, n, false)?;
            }
            3 => {
                let rings = reader.u32(true)?;
                if rings == 0 {
                    any_empty = true;
                }
                for _ in 0..rings {
                    let n = reader.u32(true)?;
                    if n == 0 {
                        any_empty = true;
                    }
                    count += vertices(&mut reader, n, false)?;
                }
            }
            _ => {
                if reader.u32(true)? == 0 {
                    any_empty = true;
                }
            }
        }
    }
    Ok((count, any_empty))
}

/// The X and Y bounds of a GEOMETRY and how many vertices count towards them.
pub fn extent(wkb: &[u8]) -> Result<(Extent, u32)> {
    let mut extent = Extent::empty();
    let (count, _) = extend_extent(wkb, &mut extent)?;
    Ok((extent, count))
}

/// `GeometryStatsData`: the types, the extent and the kinds of emptiness of some GEOMETRYs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeometryStats {
    /// per vertex type (XY, XYZ, XYM, XYZM), a bit for each geometry type
    pub types: [u8; 4],
    pub extent: Extent,
    /// `GeometryStatsFlags`: empty geometry, non-empty geometry, empty part, non-empty part
    pub flags: u8,
}

pub const HAS_EMPTY_GEOM: u8 = 0x1;
pub const HAS_NON_EMPTY_GEOM: u8 = 0x2;
pub const HAS_EMPTY_PART: u8 = 0x4;
pub const HAS_NON_EMPTY_PART: u8 = 0x8;

impl GeometryStats {
    /// `SetEmpty`: of no GEOMETRY.
    pub fn empty() -> Self {
        Self {
            types: [0; 4],
            extent: Extent::empty(),
            flags: 0,
        }
    }

    /// `SetUnknown`: of any.
    pub fn unknown() -> Self {
        Self {
            types: [0xFF; 4],
            extent: Extent::unknown(),
            flags: 0x0F,
        }
    }

    /// `GeometryStatsData::Update`.
    pub fn update(&mut self, wkb: &[u8]) -> Result<()> {
        let (kind, has_z, has_m) = geometry_type(wkb)?;
        self.types[usize::from(has_z) + 2 * usize::from(has_m)] |= 1 << (kind as u8);
        let (count, any_empty) = extend_extent(wkb, &mut self.extent)?;
        self.flags |= if any_empty { HAS_EMPTY_PART } else { HAS_NON_EMPTY_PART };
        self.flags |= if count == 0 { HAS_EMPTY_GEOM } else { HAS_NON_EMPTY_GEOM };
        Ok(())
    }

    /// `GeometryStatsData::Merge`.
    pub fn merge(&mut self, other: &GeometryStats) {
        for (mine, theirs) in self.types.iter_mut().zip(other.types) {
            *mine |= theirs;
        }
        self.extent.merge(&other.extent);
        self.flags |= other.flags;
    }
}

/// `VertexExtractFunction`: coordinate `index` (X, Y, Z, M) of a POINT, if it has one that isn't NaN.
pub fn point_coordinate(wkb: &[u8], index: usize) -> Option<f64> {
    let meta = u32::from_le_bytes(wkb.get(1..5)?.try_into().ok()?);
    let type_id = (meta & 0xFFFF) % 1000;
    let flag = (meta & 0xFFFF) / 1000;
    let (has_z, has_m) = (flag & 1 != 0, flag & 2 != 0);
    if type_id != 1 {
        return None;
    }
    let slot = match index {
        0 | 1 => index,
        2 if has_z => 2,
        3 if has_m => 2 + usize::from(has_z),
        _ => return None,
    };
    let at = 5 + slot * 8;
    let value = f64::from_le_bytes(wkb.get(at..at + 8)?.try_into().ok()?);
    (!value.is_nan()).then_some(value)
}

// The spatial extension's format of GEOMETRYs (`Geometry::ToSpatialGeometry` / `FromSpatialGeometry`), which
// database files before v1.5.0 hold: an 8-byte header (the 0-based type of the root, flags for Z, M and a bounding
// box, padding), the box as floats for all but a point, then each part as a 0-based type and a count, with the
// vertices of points, line strings and the rings of polygons after it.

const SPATIAL_HAS_Z: u8 = 0x01;
const SPATIAL_HAS_M: u8 = 0x02;
const SPATIAL_HAS_BBOX: u8 = 0x04;

/// `GeometryExtent::ExtendAxis`: a NaN makes the axis unknown.
fn extend_axis(min: &mut f64, max: &mut f64, value: f64) {
    if value.is_nan() {
        *min = f64::NEG_INFINITY;
        *max = f64::INFINITY;
        return;
    }
    *min = min_value(*min, value);
    *max = max_value(*max, value);
}

/// `DoubleToFloatDown`: the greatest float not above it.
fn double_to_float_down(d: f64) -> f32 {
    if d > f64::from(f32::MAX) {
        return f32::MAX;
    }
    if d <= f64::from(f32::MIN) {
        return f32::MIN;
    }
    let f = d as f32;
    if f64::from(f) <= d { f } else { f.next_down() }
}

/// `DoubleToFloatUp`: the least float not below it.
fn double_to_float_up(d: f64) -> f32 {
    if d >= f64::from(f32::MAX) {
        return f32::MAX;
    }
    if d < f64::from(f32::MIN) {
        return f32::MIN;
    }
    let f = d as f32;
    if f64::from(f) >= d { f } else { f.next_up() }
}

struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let bytes = self
            .data
            .get(self.at..self.at + n)
            .ok_or_else(|| invalid("Unsupported geometry type in legacy geometry!"))?;
        self.at += n;
        Ok(bytes)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }
    fn at_end(&self) -> bool {
        self.at >= self.data.len()
    }
}

/// `Geometry::ToSpatialGeometry`: a GEOMETRY (little-endian ISO WKB) in the spatial extension's format.
pub fn to_spatial(wkb: &[u8]) -> Result<Vec<u8>> {
    // `ToSpatialGeometryAnalyze`: the root's type and dimensions, and the extent of the vertices
    let mut reader = Cursor { data: wkb, at: 0 };
    let mut root: Option<(u32, bool, bool)> = None;
    let mut extent = Extent::empty();
    while !reader.at_end() {
        if reader.u8()? != 1 {
            return Err(invalid(
                "Only little-endian legacy geometries are supported for conversion to spatial format!",
            ));
        }
        let type_id = reader.u32()?;
        let (flag, kind) = (type_id / 1000, type_id % 1000);
        let (has_z, has_m) = (flag & 1 != 0, flag & 2 != 0);
        match root {
            None => root = Some((kind, has_z, has_m)),
            Some((_, z, m)) => {
                if z != has_z {
                    return Err(invalid(
                        "All geometries in a spatial blob must have the same Z presence",
                    ));
                }
                if m != has_m {
                    return Err(invalid(
                        "All geometries in a spatial blob must have the same M presence",
                    ));
                }
            }
        }
        let dims = 2 + usize::from(has_z) + usize::from(has_m);
        let vertex = |reader: &mut Cursor<'_>, extent: &mut Extent, point: bool| -> Result<()> {
            let bytes = reader.take(dims * 8)?;
            let value = |i: usize| f64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().expect("8 bytes"));
            if point && (0..dims).all(|i| value(i).is_nan()) {
                return Ok(());
            }
            extend_axis(&mut extent.x_min, &mut extent.x_max, value(0));
            extend_axis(&mut extent.y_min, &mut extent.y_max, value(1));
            if has_z {
                extend_axis(&mut extent.z_min, &mut extent.z_max, value(2));
            }
            if has_m {
                let at = if has_z { 3 } else { 2 };
                extend_axis(&mut extent.m_min, &mut extent.m_max, value(at));
            }
            Ok(())
        };
        match kind {
            1 => vertex(&mut reader, &mut extent, true)?,
            2 => {
                for _ in 0..reader.u32()? {
                    vertex(&mut reader, &mut extent, false)?;
                }
            }
            3 => {
                for _ in 0..reader.u32()? {
                    for _ in 0..reader.u32()? {
                        vertex(&mut reader, &mut extent, false)?;
                    }
                }
            }
            4..=7 => {
                reader.u32()?;
            }
            _ => return Err(invalid("Unsupported geometry type in legacy geometry!")),
        }
    }
    let Some((root_type, has_z, has_m)) = root else {
        return Err(invalid("Unsupported geometry type in legacy geometry!"));
    };
    let bbox = root_type != 1;
    // `ToSpatialGeometryConvert`
    let mut out = Vec::with_capacity(wkb.len() + 32);
    let mut flags = 0u8;
    if has_z {
        flags |= SPATIAL_HAS_Z;
    }
    if has_m {
        flags |= SPATIAL_HAS_M;
    }
    if bbox {
        flags |= SPATIAL_HAS_BBOX;
    }
    out.push((root_type - 1) as u8);
    out.push(flags);
    out.extend_from_slice(&[0; 6]);
    if bbox {
        let mut bounds = vec![
            double_to_float_down(extent.x_min),
            double_to_float_down(extent.y_min),
            double_to_float_up(extent.x_max),
            double_to_float_up(extent.y_max),
        ];
        if has_z {
            bounds.extend([double_to_float_down(extent.z_min), double_to_float_up(extent.z_max)]);
        }
        if has_m {
            bounds.extend([double_to_float_down(extent.m_min), double_to_float_up(extent.m_max)]);
        }
        for bound in bounds {
            out.extend_from_slice(&bound.to_le_bytes());
        }
    }
    let width = (2 + usize::from(has_z) + usize::from(has_m)) * 8;
    let mut reader = Cursor { data: wkb, at: 0 };
    while !reader.at_end() {
        reader.u8()?;
        let kind = reader.u32()? % 1000;
        out.extend_from_slice(&(kind - 1).to_le_bytes());
        match kind {
            1 => {
                let vertex = reader.take(width)?;
                if vertex
                    .chunks_exact(8)
                    .all(|v| f64::from_le_bytes(v.try_into().expect("8 bytes")).is_nan())
                {
                    out.extend_from_slice(&0u32.to_le_bytes());
                } else {
                    out.extend_from_slice(&1u32.to_le_bytes());
                    out.extend_from_slice(vertex);
                }
            }
            2 => {
                let count = reader.u32()?;
                out.extend_from_slice(&count.to_le_bytes());
                out.extend_from_slice(reader.take(width * count as usize)?);
            }
            3 => {
                let rings = reader.u32()?;
                out.extend_from_slice(&rings.to_le_bytes());
                // the sizes of the rings, padded to 8 bytes, then their vertices
                let sizes_at = out.len();
                out.resize(sizes_at + 4 * (rings as usize + rings as usize % 2), 0);
                for ring in 0..rings as usize {
                    let size = reader.u32()?;
                    out[sizes_at + ring * 4..sizes_at + ring * 4 + 4].copy_from_slice(&size.to_le_bytes());
                    out.extend_from_slice(reader.take(width * size as usize)?);
                }
            }
            _ => out.extend_from_slice(&reader.u32()?.to_le_bytes()),
        }
    }
    Ok(out)
}

/// `Geometry::FromSpatialGeometry` (`FromLegacyGeometryConversion`): a GEOMETRY of one in the spatial extension's
/// format.
pub fn from_spatial(blob: &[u8]) -> Result<Vec<u8>> {
    let mut reader = Cursor { data: blob, at: 0 };
    reader.take(1)?;
    let flags = reader.u8()?;
    reader.take(6)?;
    let (has_z, has_m) = (flags & SPATIAL_HAS_Z != 0, flags & SPATIAL_HAS_M != 0);
    if flags & 0xC0 != 0 {
        return Err(invalid(
            "Cannot import an unknown version of the spatial geometry format!",
        ));
    }
    let dims = 2 + usize::from(has_z) + usize::from(has_m);
    if flags & SPATIAL_HAS_BBOX != 0 {
        reader.take(4 * 2 * dims)?;
    }
    let width = dims * 8;
    let mut out = Vec::with_capacity(blob.len());
    while !reader.at_end() {
        let kind = reader.u32()? + 1;
        let size = reader.u32()?;
        let meta = kind + if has_z { 1000 } else { 0 } + if has_m { 2000 } else { 0 };
        out.push(1);
        out.extend_from_slice(&meta.to_le_bytes());
        match kind {
            1 => {
                if size == 0 {
                    for _ in 0..dims {
                        out.extend_from_slice(&f64::NAN.to_le_bytes());
                    }
                } else {
                    out.extend_from_slice(reader.take(width)?);
                }
            }
            2 => {
                out.extend_from_slice(&size.to_le_bytes());
                out.extend_from_slice(reader.take(width * size as usize)?);
            }
            3 => {
                out.extend_from_slice(&size.to_le_bytes());
                let sizes = reader.take(4 * (size as usize + size as usize % 2))?;
                for ring in 0..size as usize {
                    let ring_size = u32::from_le_bytes(sizes[ring * 4..ring * 4 + 4].try_into().expect("4 bytes"));
                    out.extend_from_slice(&ring_size.to_le_bytes());
                    out.extend_from_slice(reader.take(width * ring_size as usize)?);
                }
            }
            4..=7 => out.extend_from_slice(&size.to_le_bytes()),
            _ => return Err(invalid("Unsupported geometry type in legacy geometry!")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod spatial_tests {
    use super::*;

    #[test]
    fn spatial_format_round_trips() {
        for text in [
            "POINT (1 2)",
            "POINT EMPTY",
            "LINESTRING (0 0, 1 1, 2 0)",
            "POLYGON ((0 0, 4 0, 4 4, 0 0), (1 1, 2 1, 1 2, 1 1), (3 3, 3.5 3, 3 3.5, 3 3))",
            "MULTIPOINT Z ((1 2 3), (4 5 6))",
            "GEOMETRYCOLLECTION (POINT (1 1), LINESTRING (0 0, 1 2))",
        ] {
            let wkb = from_wkt(text).unwrap();
            let spatial = to_spatial(&wkb).unwrap();
            assert_eq!(from_spatial(&spatial).unwrap(), wkb, "{text}");
        }
    }
}
