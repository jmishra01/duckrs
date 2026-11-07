// Ported from duckdb/src/common/file_system.cpp and duckdb/src/common/local_file_system.cpp
// @ rust-port-baseline
//
//! The local file system: globbing and whole-file reads. Compressed files, pipes and remote file
//! systems are not here yet, and a compressed file fails instead of reading as garbage.

use duckrs_common::{Error, ErrorKind, Result};
use std::path::Path;

fn io_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Io, message)
}

/// `FileSystem::HasGlob`.
fn has_glob(text: &str) -> bool {
    text.contains(['*', '?', '['])
}

/// `Glob` of `file_system.cpp`: `*`, `?` and `[...]`, within one path segment.
fn glob_match(name: &[u8], pattern: &[u8]) -> bool {
    let Some((&first, rest)) = pattern.split_first() else {
        return name.is_empty();
    };
    match first {
        b'*' => (0..=name.len()).any(|skip| glob_match(&name[skip..], rest)),
        b'?' => !name.is_empty() && glob_match(&name[1..], rest),
        b'[' => {
            let Some(end) = rest.iter().skip(1).position(|&c| c == b']').map(|p| p + 1) else {
                return name.first() == Some(&b'[') && glob_match(&name[1..], rest);
            };
            let Some((&c, tail)) = name.split_first() else {
                return false;
            };
            let (negated, set) = match rest[..end].split_first() {
                Some((b'!', set)) => (true, set),
                _ => (false, &rest[..end]),
            };
            let mut found = false;
            let mut i = 0;
            while i < set.len() {
                if i + 2 < set.len() && set[i + 1] == b'-' {
                    found |= (set[i]..=set[i + 2]).contains(&c);
                    i += 3;
                } else {
                    found |= set[i] == c;
                    i += 1;
                }
            }
            found != negated && glob_match(tail, &rest[end + 1..])
        }
        c => name.first() == Some(&c) && glob_match(&name[1..], rest),
    }
}

fn entries(directory: &str) -> Vec<(String, bool)> {
    let path = if directory.is_empty() { "." } else { directory };
    let Ok(listing) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    listing
        .flatten()
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (name, entry.path().is_dir())
        })
        .collect()
}

fn join(directory: &str, name: &str) -> String {
    match directory {
        "" => name.to_string(),
        d if d.ends_with('/') => format!("{d}{name}"),
        d => format!("{d}/{name}"),
    }
}

fn walk(directory: &str, segments: &[&str], result: &mut Vec<String>) {
    let Some((&segment, rest)) = segments.split_first() else {
        return;
    };
    if segment == "**" {
        walk(directory, rest, result);
        for (name, is_directory) in entries(directory) {
            if is_directory {
                walk(&join(directory, &name), segments, result);
            }
        }
        return;
    }
    if !has_glob(segment) {
        let path = join(directory, segment);
        if rest.is_empty() {
            if Path::new(&path).is_file() {
                result.push(path);
            }
        } else {
            walk(&path, rest, result);
        }
        return;
    }
    for (name, is_directory) in entries(directory) {
        if !glob_match(name.as_bytes(), segment.as_bytes()) {
            continue;
        }
        if rest.is_empty() {
            if !is_directory {
                result.push(join(directory, &name));
            }
        } else if is_directory {
            walk(&join(directory, &name), rest, result);
        }
    }
}

/// `FileSystem::Glob`: the files that `pattern` names, sorted.
pub fn glob(pattern: &str) -> Vec<String> {
    if !has_glob(pattern) {
        return if Path::new(pattern).is_file() {
            vec![pattern.to_string()]
        } else {
            Vec::new()
        };
    }
    let (root, relative) = match pattern.strip_prefix('/') {
        Some(relative) => ("/", relative),
        None => ("", pattern),
    };
    let segments: Vec<&str> = relative.split('/').collect();
    let mut result = Vec::new();
    walk(root, &segments, &mut result);
    result.sort();
    result.dedup();
    result
}

/// `FileCompressionType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Auto,
    Uncompressed,
    Gzip,
    Zstd,
}

impl Compression {
    /// `FileCompressionTypeFromString`.
    pub fn parse(text: &str) -> Result<Self> {
        match text.to_lowercase().as_str() {
            "auto" | "auto_detect" | "infer" => Ok(Self::Auto),
            "none" | "uncompressed" | "" => Ok(Self::Uncompressed),
            "gzip" => Ok(Self::Gzip),
            "zstd" => Ok(Self::Zstd),
            other => Err(Error::new(
                ErrorKind::NotImplemented,
                format!("Unrecognized file compression type \"{other}\""),
            )),
        }
    }

    /// `Auto`, settled by the name of the file.
    pub fn for_path(self, path: &str) -> Self {
        if self != Self::Auto {
            return self;
        }
        let lower = path.to_lowercase();
        if lower.ends_with(".gz") {
            Self::Gzip
        } else if lower.ends_with(".zst") {
            Self::Zstd
        } else {
            Self::Uncompressed
        }
    }
}

/// All of a file, decompressed.
pub fn read_file(path: &str, compression: Compression) -> Result<Vec<u8>> {
    let data = std::fs::read(path).map_err(|e| io_error(format!("Cannot open file \"{path}\": {e}")))?;
    // the file is read at once: one READ of all of it
    crate::logging::log_file_system(path, "OPEN", None);
    crate::logging::log_file_system(path, "READ", Some((data.len() as i64, 0)));
    crate::logging::log_file_system(path, "CLOSE", None);
    match compression.for_path(path) {
        Compression::Gzip => {
            if !data.starts_with(&[0x1f, 0x8b]) {
                return Err(io_error(format!("Input is not a GZIP stream: {path}")));
            }
            let mut result = Vec::new();
            std::io::Read::read_to_end(&mut flate2::read::MultiGzDecoder::new(data.as_slice()), &mut result)
                .map_err(|e| io_error(format!("Failed to decompress GZIP file \"{path}\": {e}")))?;
            Ok(result)
        }
        Compression::Zstd => Err(crate::unported("reading zstd files")),
        _ => Ok(data),
    }
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn patterns() {
        assert!(glob_match(b"a.csv", b"*.csv"));
        assert!(glob_match(b"a1.csv", b"a?.csv"));
        assert!(glob_match(b"a1.csv", b"a[0-9].csv"));
        assert!(!glob_match(b"ab.csv", b"a[!a-z].csv"));
        assert!(!glob_match(b"a.tsv", b"*.csv"));
    }
}
