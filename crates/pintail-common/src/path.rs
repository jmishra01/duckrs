// Ported from duckdb/src/common/path.cpp and duckdb/src/include/duckdb/common/path.hpp
// @ rust-port-baseline (the parts that are not Windows-only)
//
//! A path parsed into a scheme, an authority, an anchor and normalized segments, which joins onto
//! another as `path_join` and the file systems do.

use crate::{Error, ErrorKind, Result};

/// `Path`: `scheme + authority + anchor`, then the segments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Path {
    scheme: String,
    authority: String,
    anchor: String,
    segments: Vec<String>,
    separator: char,
    is_absolute: bool,
    has_trailing_separator: bool,
}

fn is_path_separator(c: char) -> bool {
    c == '/' || c == '\\'
}

fn invalid(message: String) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

/// `MaybeAppendSegment`: a segment that is not `.`, where `..` takes the one before it along.
fn maybe_append_segment(segments: &mut Vec<String>, segment: &str) {
    if segment.is_empty() || segment == "." {
        return;
    }
    if segment == ".." && segments.last().is_some_and(|last| last != "..") {
        segments.pop();
    } else {
        segments.push(segment.to_string());
    }
}

/// `SegmentAndNormalizePath`.
fn segment_and_normalize(path: &str) -> Vec<String> {
    let mut segments = Vec::new();
    for segment in path.split(is_path_separator) {
        maybe_append_segment(&mut segments, segment);
    }
    segments
}

impl Path {
    /// `Path::FromString`.
    pub fn from_string(raw: &str) -> Result<Path> {
        let mut parsed = Path {
            separator: '/',
            ..Path::default()
        };
        let first_slash = raw.find('/');
        let scheme_pos = raw.find("://");
        if let Some(position) = first_slash {
            parsed.separator = raw[position..].chars().next().unwrap_or('/');
        }
        let path_offset = if raw.starts_with("file:/") {
            parsed.parse_file_schemes(raw)?
        } else if let Some(scheme) = scheme_pos
            && scheme > 1
            && first_slash.is_none_or(|slash| scheme < slash)
        {
            parsed.parse_uri_scheme(raw)
        } else {
            parsed.parse_file_path_tail(raw, 0)
        };
        parsed.normalize_segments(raw, path_offset);
        if raw.ends_with(is_path_separator) {
            parsed.has_trailing_separator = true;
        }
        Ok(parsed)
    }

    /// `Path::ToString`.
    pub fn to_path_string(&self) -> String {
        let mut result = format!("{}{}{}", self.scheme, self.authority, self.anchor);
        result += &self.segments.join(&self.separator.to_string());
        if self.has_trailing_separator && !self.segments.is_empty() {
            result.push(self.separator);
        }
        if result.is_empty() {
            result.push('.');
        }
        result
    }

    pub fn is_absolute(&self) -> bool {
        self.is_absolute
    }

    /// `Path::Join`: `rhs` onto this one. An absolute `rhs` only joins onto a path it extends.
    pub fn join(&self, rhs: &Path) -> Result<Path> {
        let mut lhs = self.clone();
        let prefix = rhs.segments.len() >= lhs.segments.len() && rhs.segments.starts_with(&lhs.segments);
        if !rhs.is_absolute {
            let mut joined: Vec<String> = Vec::with_capacity(lhs.segments.len() + rhs.segments.len());
            for segment in lhs.segments.iter().chain(&rhs.segments) {
                if segment == ".." && joined.last().is_some_and(|last| last != "..") {
                    joined.pop();
                } else {
                    joined.push(segment.clone());
                }
            }
            if lhs.is_absolute {
                let leading = joined.iter().take_while(|s| *s == "..").count();
                joined.drain(..leading);
            }
            lhs.segments = joined;
        } else if lhs.authority == rhs.authority && prefix && lhs.scheme == rhs.scheme && lhs.anchor == rhs.anchor {
            if lhs.segments.len() < rhs.segments.len() {
                lhs.segments = rhs.segments.clone();
            }
        } else {
            return Err(invalid(format!(
                "Path: cannot join incompatible paths: \"{}\" onto \"{}\"",
                rhs.to_path_string(),
                lhs.to_path_string()
            )));
        }
        lhs.has_trailing_separator = rhs.has_trailing_separator;
        Ok(lhs)
    }

    /// `Path::Join(const string &)`.
    pub fn join_str(&self, rhs: &str) -> Result<Path> {
        self.join(&Path::from_string(rhs)?)
    }

    /// `Path::NormalizeSegments`.
    fn normalize_segments(&mut self, raw: &str, path_offset: usize) {
        // URI schemes are lowercase
        if !self.scheme.is_empty() && !self.scheme.starts_with(is_path_separator) {
            self.scheme = self.scheme.to_lowercase();
        }
        if !self.anchor.is_empty() && self.anchor.starts_with(is_path_separator) {
            self.anchor = self.separator.to_string();
        }
        self.segments.clear();
        for segment in segment_and_normalize(&raw[path_offset..]) {
            // an absolute path can't go above its root
            if self.is_absolute && self.segments.is_empty() && segment == ".." {
                continue;
            }
            self.segments.push(segment);
        }
    }

    /// `Path::ParseURIScheme`: `proto://authority/path`, always absolute.
    fn parse_uri_scheme(&mut self, input: &str) -> usize {
        self.is_absolute = true;
        let authority_begin = input.find("://").expect("a scheme") + 3;
        self.scheme = input[..authority_begin].to_string();
        let path_begin = input[authority_begin..].find('/').map(|p| p + authority_begin);
        self.authority = input[authority_begin..path_begin.unwrap_or(input.len())].to_string();
        self.anchor = "/".to_string();
        path_begin.map_or(input.len(), |p| p + 1)
    }

    /// `Path::ParseFilePathTail`.
    fn parse_file_path_tail(&mut self, input: &str, start: usize) -> usize {
        if input[start..].starts_with(is_path_separator) {
            self.anchor.push(self.separator);
            self.is_absolute = true;
            return start + 1;
        }
        start
    }

    /// `Path::ParseFileSchemes`: `file:/`, `file:///` and `file://localhost/`.
    fn parse_file_schemes(&mut self, input: &str) -> Result<usize> {
        self.is_absolute = true;
        let bytes = input.as_bytes();
        let path_begin = if bytes.len() >= 8 && bytes[6] == b'/' && bytes[7] == b'/' {
            self.scheme = "file://".to_string();
            self.anchor = "/".to_string();
            8
        } else if bytes.len() >= 7 && bytes[6] == b'/' {
            let begin = self.parse_uri_scheme(input);
            if self.authority.to_lowercase() != "localhost" {
                return Err(invalid(format!(
                    "Path: file:// scheme only supports localhost authority, got: {}",
                    self.authority
                )));
            }
            begin
        } else {
            self.scheme = "file:".to_string();
            self.anchor = "/".to_string();
            6
        };
        Ok(self.parse_file_path_tail(input, path_begin))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(base: &str, rest: &[&str]) -> String {
        let mut path = Path::from_string(base).unwrap();
        for part in rest {
            path = path.join_str(part).unwrap();
        }
        path.to_path_string()
    }

    #[test]
    fn joins_and_normalizes() {
        assert_eq!(join("dir//subdir/", &["./file"]), "dir/subdir/file");
        assert_eq!(join("dir/subdir", &["../sibling"]), "dir/sibling");
        assert_eq!(join("dir/..", &["../.."]), "../..");
        assert_eq!(join("/usr/local", &["../.."]), "/");
        assert_eq!(join("file:/usr/local", &["../bin"]), "file:/usr/bin");
        assert_eq!(join("s3://foo", &["bar/baz"]), "s3://foo/bar/baz");
        assert_eq!(join("s3://foo", &[".."]), "s3://foo/");
        assert!(Path::from_string("dir").unwrap().join_str("/abs/path").is_err());
    }
}
