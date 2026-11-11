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
pub fn has_glob(text: &str) -> bool {
    text.contains(['*', '?', '['])
}

/// `Glob` of `file_system.cpp`: `*`, `?` and `[...]`, within one path segment.
/// `Glob`: whether `name` matches `pattern` of `*`, `?` and `[...]`.
pub fn glob_match(name: &[u8], pattern: &[u8]) -> bool {
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
/// `LocalFileSystem::IsPipe`.
pub fn is_pipe(path: &str) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(path).is_ok_and(|m| m.file_type().is_fifo())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

pub fn glob(pattern: &str) -> Vec<String> {
    if !has_glob(pattern) {
        // `LocalFileSystem::Glob`: a file that exists, or a pipe (`IsPipe`, e.g. `/dev/stdin`)
        return if Path::new(pattern).is_file() || is_pipe(pattern) {
            vec![pattern.to_string()]
        } else {
            Vec::new()
        };
    }
    let (root, relative) = match pattern.strip_prefix('/') {
        Some(relative) => ("/", relative),
        None => ("", pattern),
    };
    // `Path`: a backslash separates as a slash does, and empty segments are dropped
    let segments: Vec<&str> = relative.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    let mut result = Vec::new();
    walk(root, &segments, &mut result);
    result.sort();
    result.dedup();
    // a file whose name has glob characters in it (#4699)
    if result.is_empty() && Path::new(pattern).is_file() {
        result.push(pattern.to_string());
    }
    result
}

/// `FileSystem::GlobFileList` with `FALLBACK_GLOB`: a path that is no file and has no glob is read as
/// a directory of files with the reader's extension, as a partitioned COPY writes them.
pub fn glob_or_directory(pattern: &str, extension: &str) -> Vec<String> {
    let found = glob(pattern);
    if !found.is_empty() || has_glob(pattern) {
        return found;
    }
    glob(&join(&join(pattern, "**"), &format!("*.{extension}")))
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

/// `VirtualFileSystem::FindCompressionFileSystem`: a name that says the file is compressed in a
/// way no file system here reads (`CompressionFromExtension`) is an error, not plain text.
pub fn check_compression(path: &str, compression: Compression) -> Result<()> {
    if compression != Compression::Auto {
        return Ok(());
    }
    let lower = path.to_lowercase();
    let lower = lower.strip_suffix(".tmp").unwrap_or(&lower);
    let suffixes = [
        (".bz2", "bzip2"),
        (".xz", "xz"),
        (".lz4", "lz4"),
        (".br", "brotli"),
        (".lzma", "lzma"),
        (".zip", "zip"),
    ];
    match suffixes.iter().find(|(suffix, _)| lower.ends_with(suffix)) {
        Some((_, name)) => Err(Error::new(
            ErrorKind::NotImplemented,
            format!(
                "Attempting to open a compressed file, but the compression type is not supported (compression \
                 type \"{name}\")\nThe compression was detected from the name of \"{path}\"; set compression = \
                 'none' to treat it as uncompressed"
            ),
        )),
        None => Ok(()),
    }
}

/// All of a file, decompressed.
pub fn read_file(path: &str, compression: Compression) -> Result<Vec<u8>> {
    check_compression(path, compression)?;
    let data = std::fs::read(path).map_err(|e| io_error(format!("Cannot open file \"{path}\": {e}")))?;
    // the file is read at once: one READ of all of it
    crate::logging::log_file_system(path, "OPEN", None);
    crate::logging::log_file_system(path, "READ", Some((data.len() as i64, 0)));
    crate::execution::profiler::track_read(data.len());
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
        Compression::Zstd => zstd::stream::decode_all(data.as_slice())
            .map_err(|e| io_error(format!("Failed to decompress ZSTD file \"{path}\": {e}"))),
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

/// `FileSystem::ExpandPath`: a leading `~` is the home directory.
pub fn expand_path(path: &str) -> String {
    if (path == "~" || path.starts_with("~/"))
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}{}", &path[1..]);
    }
    path.to_string()
}

/// `FileSystem::IsRemoteFile`, by its scheme.
fn is_remote(path: &str) -> bool {
    path.contains("://")
}

/// `FileSystem::CanonicalizePath`: `.` and empty segments dropped, `..` resolved, without the file
/// system (symbolic links are not followed).
pub fn canonicalize_path(path: &str) -> String {
    if is_remote(path) {
        return path.to_string();
    }
    let mut stack: Vec<&str> = Vec::new();
    for element in path.split('/') {
        match element {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            _ => stack.push(element),
        }
    }
    let mut result = String::new();
    if path.starts_with('/') {
        result.push('/');
    }
    result += &stack.join("/");
    result
}

/// `DBConfig::SanitizeAllowedPath`.
pub fn sanitize_allowed_path(path: &str) -> String {
    canonicalize_path(&expand_path(path))
}

/// What a database with `enable_external_access` off may still read and write
/// (`allowed_paths` and `allowed_directories`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileAccess {
    pub paths: std::collections::BTreeSet<String>,
    /// each ending in `/`
    pub directories: std::collections::BTreeSet<String>,
}

impl FileAccess {
    /// `DBConfig::CanAccessFile`.
    pub fn can_access(&self, input: &str, directory: bool) -> bool {
        let mut path = sanitize_allowed_path(input);
        if self.paths.contains(&path) {
            return true;
        }
        if directory && !path.ends_with('/') {
            path.push('/');
        }
        self.directories
            .iter()
            .any(|allowed| path.starts_with(allowed.as_str()))
    }

    /// `OpenerFileSystem::VerifyCanAccessFile` and `VerifyCanAccessDirectory`.
    pub fn check(&self, path: &str, directory: bool) -> Result<()> {
        if self.can_access(path, directory) {
            return Ok(());
        }
        Err(Error::new(
            ErrorKind::Permission,
            format!(
                "Cannot access {} \"{path}\" - file system operations are disabled by configuration",
                if directory { "directory" } else { "file" }
            ),
        ))
    }
}
