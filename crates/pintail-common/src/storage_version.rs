//! Ported from duckdb/src/storage/storage_info.cpp and storage_info.hpp @ rust-port-baseline.
//!
//! `StorageVersion`: the version of the file format a database is written in, and the names DuckDB gives the
//! releases that write each one. Releases that share a format share its number, so comparisons go by number.

/// The storage format version being written; older versions get backwards-compatible output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StorageVersion(pub u64);

impl Default for StorageVersion {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl StorageVersion {
    pub const V0_10_2: Self = Self(64);
    pub const V0_10_3: Self = Self(64);
    pub const V1_0_0: Self = Self(64);
    pub const V1_1_0: Self = Self(64);
    pub const V1_1_1: Self = Self(64);
    pub const V1_1_2: Self = Self(64);
    pub const V1_1_3: Self = Self(64);
    pub const V1_2_0: Self = Self(65);
    pub const V1_2_1: Self = Self(65);
    pub const V1_2_2: Self = Self(65);
    pub const V1_3_0: Self = Self(66);
    pub const V1_3_1: Self = Self(66);
    pub const V1_3_2: Self = Self(66);
    pub const V1_4_0: Self = Self(67);
    pub const V1_4_1: Self = Self(67);
    pub const V1_4_2: Self = Self(67);
    pub const V1_4_3: Self = Self(67);
    pub const V1_4_4: Self = Self(67);
    pub const V1_4_5: Self = Self(67);
    pub const V1_5_0: Self = Self(68);
    pub const V1_5_1: Self = Self(68);
    pub const V1_5_2: Self = Self(68);
    pub const V1_5_3: Self = Self(68);
    pub const V1_5_4: Self = Self(68);
    pub const V1_5_5: Self = Self(68);
    pub const V1_5_6: Self = Self(68);
    pub const V2_0_0: Self = Self(69);
    pub const V2_0_1: Self = Self(69);
    pub const V2_1_0: Self = Self(70);
    /// `StorageVersion::LATEST`
    pub const LATEST: Self = Self(70);
    /// what a new database is written in (`DEFAULT_STORAGE_VERSION_INFO`)
    pub const DEFAULT: Self = Self::V2_0_0;

    /// `GetStorageVersion`: the version a release writes, by its name (`v1.2.0`, `latest`).
    pub fn from_name(name: &str) -> Option<Self> {
        if name == "latest" {
            return Some(Self::LATEST);
        }
        VERSIONS
            .iter()
            .find(|(n, _, _)| *n == name)
            .map(|(_, number, _)| Self(*number))
    }

    /// `GetStorageCandidates`: the names that [`Self::from_name`] knows.
    pub fn candidates() -> Vec<&'static str> {
        VERSIONS.iter().map(|(name, _, _)| *name).chain(["latest"]).collect()
    }

    /// `StorageVersionInfo::GetStorageVersionString`: the first release that writes this version.
    pub fn first_name(self) -> Option<&'static str> {
        VERSIONS
            .iter()
            .find(|(_, number, _)| *number == self.0)
            .map(|(name, _, _)| *name)
    }

    /// `GetStorageVersionName`, as `duckdb_databases()` tags it.
    pub fn display_name(self, suffix: bool) -> String {
        if self < Self::V1_2_0 {
            return "v1.0.0+".to_string();
        }
        match self.first_name() {
            Some(name) if suffix => format!("{name}+"),
            Some(name) => name.to_string(),
            None => "--UNKNOWN--".to_string(),
        }
    }

    /// `SingleFileBlockManager::GetVersionNumber`: what the main header of a new file says.
    pub fn main_header_number(self) -> u64 {
        if self < Self::V1_2_0 { Self::V0_10_2.0 } else { self.0 }
    }

    /// What the database header of a file says (`MainHeader::Write`): before v2.0.0 the serialization
    /// version of the release (`GetSerializationVersionDeprecated`), from then on the storage version.
    pub fn database_header_number(self) -> u64 {
        if self >= Self::V2_0_0 {
            return self.0;
        }
        let name = self.first_name().unwrap_or("v0.10.2");
        VERSIONS
            .iter()
            .find(|(n, _, _)| *n == name)
            .and_then(|(_, _, serialization)| *serialization)
            .unwrap_or(1)
    }

    /// `DatabaseHeader::SetStorageVersionInDatabaseHeader`, of a file before v2.0.0: the storage version of
    /// a serialization version.
    pub fn from_serialization_number(number: u64) -> Option<Self> {
        Some(match number {
            0..=3 | 64 => Self::V0_10_2,
            4 => Self::V1_2_0,
            5 => Self::V1_3_0,
            6 => Self::V1_4_0,
            7 => Self::V1_5_0,
            _ => return None,
        })
    }
}

/// `storage_version_info` and `serialization_version_info`: by release, its storage version and, from v0.10.0,
/// its serialization version.
const VERSIONS: &[(&str, u64, Option<u64>)] = &[
    ("v0.0.4", 1, None),
    ("v0.1.0", 1, None),
    ("v0.1.1", 1, None),
    ("v0.1.2", 1, None),
    ("v0.1.3", 1, None),
    ("v0.1.4", 1, None),
    ("v0.1.5", 1, None),
    ("v0.1.6", 1, None),
    ("v0.1.7", 1, None),
    ("v0.1.8", 1, None),
    ("v0.1.9", 1, None),
    ("v0.2.0", 1, None),
    ("v0.2.1", 1, None),
    ("v0.2.2", 4, None),
    ("v0.2.3", 6, None),
    ("v0.2.4", 11, None),
    ("v0.2.5", 13, None),
    ("v0.2.6", 15, None),
    ("v0.2.7", 17, None),
    ("v0.2.8", 18, None),
    ("v0.2.9", 21, None),
    ("v0.3.0", 25, None),
    ("v0.3.1", 27, None),
    ("v0.3.2", 31, None),
    ("v0.3.3", 33, None),
    ("v0.3.4", 33, None),
    ("v0.3.5", 33, None),
    ("v0.4.0", 33, None),
    ("v0.5.0", 38, None),
    ("v0.5.1", 38, None),
    ("v0.6.0", 39, None),
    ("v0.6.1", 39, None),
    ("v0.7.0", 43, None),
    ("v0.7.1", 43, None),
    ("v0.8.0", 51, None),
    ("v0.8.1", 51, None),
    ("v0.9.0", 64, None),
    ("v0.9.1", 64, None),
    ("v0.9.2", 64, None),
    ("v0.10.0", 64, Some(1)),
    ("v0.10.1", 64, Some(1)),
    ("v0.10.2", 64, Some(1)),
    ("v0.10.3", 64, Some(2)),
    ("v1.0.0", 64, Some(2)),
    ("v1.1.0", 64, Some(3)),
    ("v1.1.1", 64, Some(3)),
    ("v1.1.2", 64, Some(3)),
    ("v1.1.3", 64, Some(3)),
    ("v1.2.0", 65, Some(4)),
    ("v1.2.1", 65, Some(4)),
    ("v1.2.2", 65, Some(4)),
    ("v1.3.0", 66, Some(5)),
    ("v1.3.1", 66, Some(5)),
    ("v1.3.2", 66, Some(5)),
    ("v1.4.0", 67, Some(6)),
    ("v1.4.1", 67, Some(6)),
    ("v1.4.2", 67, Some(6)),
    ("v1.4.3", 67, Some(6)),
    ("v1.4.4", 67, Some(6)),
    ("v1.4.5", 67, Some(6)),
    ("v1.5.0", 68, Some(7)),
    ("v1.5.1", 68, Some(7)),
    ("v1.5.2", 68, Some(7)),
    ("v1.5.3", 68, Some(7)),
    ("v1.5.4", 68, Some(7)),
    ("v1.5.5", 68, Some(7)),
    ("v1.5.6", 68, Some(7)),
    ("v2.0.0", 69, Some(8)),
    ("v2.0.1", 69, Some(8)),
    ("v2.1.0", 70, Some(9)),
];
