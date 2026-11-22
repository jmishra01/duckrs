//! Ported from duckdb/src/storage/single_file_block_manager.cpp and
//! duckdb/src/include/duckdb/storage/storage_info.hpp @ rust-port-baseline.
//!
//! A database file is three 4 KiB headers (one main header, then two database headers, of which
//! the one with the higher iteration is the current one) followed by blocks. Each header and block
//! starts with the checksum of the rest of it. Nothing here interprets what is in a block.

use super::checksum::checksum;
use super::encryption::{Cipher, ENCRYPTED_BLOCK_HEADER_SIZE, EncryptionRequest, FileEncryption, Version};
use duckrs_common::serializer::StorageVersion;
use duckrs_common::{Error, ErrorKind, Result};
use parking_lot::Mutex;
use std::collections::{BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const FILE_HEADER_SIZE: usize = 4096;
/// `sizeof(idx_t)`: the checksum in front of a block.
pub const BLOCK_HEADER_SIZE: usize = 8;
pub const DEFAULT_BLOCK_ALLOC_SIZE: u64 = 262144;
const MIN_BLOCK_ALLOC_SIZE: u64 = 16384;
const MAX_BLOCK_ALLOC_SIZE: u64 = DEFAULT_BLOCK_ALLOC_SIZE;
/// `BLOCK_START`: where block 0 is in the file.
const BLOCK_START: u64 = FILE_HEADER_SIZE as u64 * 3;
const MAGIC_BYTES: &[u8; 4] = b"DUCK";
const MAX_VERSION_SIZE: usize = 32;
const FLAG_COUNT: usize = 4;
const ENCRYPTED_DATABASE_FLAG: u64 = 1;
/// `VERSION_NUMBER_LOWER` and `VERSION_NUMBER_UPPER`.
const VERSION_NUMBER_LOWER: u64 = 64;
const VERSION_NUMBER_UPPER: u64 = 70;
/// `StorageVersion::DEPRECATED`: from v2.0.0 on the main header no longer says the version.
const DEPRECATED_VERSION_NUMBER: u64 = 999;
const STANDARD_VECTOR_SIZE: u64 = 2048;
/// `INVALID_BLOCK`.
pub const INVALID_BLOCK: u64 = u64::MAX;

/// What the main header of a file in `version` says (`MainHeader::Write`): from v2.0.0 on it gives up the
/// version for the one in the database headers.
fn main_header_version_number(version: StorageVersion) -> u64 {
    if version >= StorageVersion::V2_0_0 {
        DEPRECATED_VERSION_NUMBER
    } else {
        version.main_header_number()
    }
}
/// `MainHeader::DB_IDENTIFIER_LEN`.
pub const DB_IDENTIFIER_LEN: usize = 16;
/// `MainHeader::ENCRYPTION_METADATA_LEN`.
const ENCRYPTION_METADATA_LEN: usize = 8;

pub(super) fn corrupt(message: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::DataCorruption, format!("Corrupt database file: {message}"))
}

/// Damage that C++ words in full itself.
pub(super) fn corrupted(message: &str) -> Error {
    Error::new(ErrorKind::DataCorruption, message)
}

fn io_error(path: &Path, error: std::io::Error) -> Error {
    Error::new(ErrorKind::Io, format!("{}: {error}", path.display()))
}

fn invalid_file(path: &Path) -> Error {
    Error::new(
        ErrorKind::Io,
        format!(
            "The file \"{}\" exists, but it is not a valid DuckDB database file!",
            path.display()
        ),
    )
}

fn u64_at(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().expect("8 bytes"))
}

/// `MainHeader`.
#[derive(Debug, Clone)]
pub struct MainHeader {
    pub version_number: u64,
    pub flags: [u64; FLAG_COUNT],
    pub library_git_desc: String,
    pub library_git_hash: String,
    /// Tells a write-ahead log of this file from those of others, and salts the key of an encrypted one.
    pub db_identifier: [u8; DB_IDENTIFIER_LEN],
    /// of an encrypted file: how it is encrypted ([`FileEncryption::metadata`]), and the canary with its nonce and tag
    encryption_metadata: [u8; ENCRYPTION_METADATA_LEN],
    canary: [u8; 8],
    canary_nonce: [u8; 12],
    canary_tag: [u8; 16],
}

/// Where the parts of the main header are, past its checksum.
const DESC_AT: usize = 12 + FLAG_COUNT * 8;
const METADATA_AT: usize = DESC_AT + 2 * MAX_VERSION_SIZE;
const IDENTIFIER_AT: usize = METADATA_AT + ENCRYPTION_METADATA_LEN;
const CANARY_AT: usize = IDENTIFIER_AT + DB_IDENTIFIER_LEN;
const CANARY_NONCE_AT: usize = CANARY_AT + 8;
const CANARY_TAG_AT: usize = CANARY_NONCE_AT + 12;

impl MainHeader {
    fn read(data: &[u8]) -> Result<Self> {
        let version_number = u64_at(data, 4);
        if version_number != DEPRECATED_VERSION_NUMBER
            && !(VERSION_NUMBER_LOWER..=VERSION_NUMBER_UPPER).contains(&version_number)
        {
            return Err(Error::new(
                ErrorKind::Io,
                format!(
                    "Trying to read a database file with storage version number {version_number}, but we can only \
                     read storage versions between {VERSION_NUMBER_LOWER} and {VERSION_NUMBER_UPPER}."
                ),
            ));
        }
        let mut flags = [0; FLAG_COUNT];
        for (i, flag) in flags.iter_mut().enumerate() {
            *flag = u64_at(data, 12 + i * 8);
        }
        let text = |offset: usize| {
            let bytes = &data[offset..offset + MAX_VERSION_SIZE];
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            String::from_utf8_lossy(&bytes[..end]).into_owned()
        };
        let array = |at: usize, len: usize| &data[at..at + len];
        Ok(Self {
            version_number,
            flags,
            library_git_desc: text(DESC_AT),
            library_git_hash: text(DESC_AT + MAX_VERSION_SIZE),
            db_identifier: array(IDENTIFIER_AT, DB_IDENTIFIER_LEN).try_into().expect("16 bytes"),
            encryption_metadata: array(METADATA_AT, ENCRYPTION_METADATA_LEN).try_into().expect("8 bytes"),
            canary: array(CANARY_AT, 8).try_into().expect("8 bytes"),
            canary_nonce: array(CANARY_NONCE_AT, 12).try_into().expect("12 bytes"),
            canary_tag: array(CANARY_TAG_AT, 16).try_into().expect("16 bytes"),
        })
    }

    pub fn is_encrypted(&self) -> bool {
        self.flags[0] & ENCRYPTED_DATABASE_FLAG != 0
    }

    /// `SingleFileBlockManager::CheckAndAddEncryptionKey` and the checks of `LoadExistingDatabase`: the encryption of
    /// the file, if it is encrypted and `request` opens it.
    fn encryption(
        &self,
        path: &Path,
        request: Option<&EncryptionRequest>,
        read_only: bool,
    ) -> Result<Option<FileEncryption>> {
        let catalog_error = |message: String| Error::new(ErrorKind::Catalog, message);
        let path = path.display();
        if !self.is_encrypted() {
            if request.is_some() {
                return Err(catalog_error(format!(
                    "A key is explicitly specified, but database \"{path}\" is not encrypted"
                )));
            }
            return Ok(None);
        }
        let Some(request) = request else {
            return Err(catalog_error(format!(
                "Cannot open encrypted database \"{path}\" without a key"
            )));
        };
        if !read_only && !request.writable {
            return Err(super::encryption::read_only_crypto());
        }
        let invalid = || Error::new(ErrorKind::InvalidInput, "No valid encryption version found!");
        let version = Version::from_number(self.encryption_metadata[3]).ok_or_else(invalid)?;
        let stored = Cipher::from_number(self.encryption_metadata[2]).ok_or_else(invalid)?;
        let key = super::encryption::derive_key(&request.key, &self.db_identifier);
        let encryption = FileEncryption::new(key, stored, version);
        if !encryption.check_canary(&self.canary, &self.canary_nonce, &self.canary_tag) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Wrong encryption key used to open the database file",
            ));
        }
        if let Some(given) = request.cipher
            && given != stored
        {
            return Err(catalog_error(format!(
                "Cannot open encrypted database \"{path}\" with a different cipher ({}) than the one used to create \
                 it ({})",
                given.name(),
                stored.name()
            )));
        }
        // a cipher other than GCM has to be asked for, so that a file can't be downgraded to it unnoticed
        if stored != Cipher::Gcm && request.cipher.is_none() {
            return Err(catalog_error(format!(
                "Cannot open encrypted database \"{path}\" without explicitly specifying the encryption cipher for \
                 security reasons. Please make sure you understand the security implications and re-attach the \
                 database specifying the desired cipher."
            )));
        }
        Ok(Some(encryption))
    }
}

/// `DatabaseHeader`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseHeader {
    pub iteration: u64,
    /// A `MetaBlockPointer`'s `block_pointer`, or `INVALID_BLOCK`.
    pub meta_block: u64,
    pub free_list: u64,
    pub block_count: u64,
    pub block_alloc_size: u64,
    pub vector_size: u64,
    pub storage_compatibility: u64,
}

impl DatabaseHeader {
    fn read(data: &[u8]) -> Result<Self> {
        let mut header = Self {
            iteration: u64_at(data, 0),
            meta_block: u64_at(data, 8),
            free_list: u64_at(data, 16),
            block_count: u64_at(data, 24),
            block_alloc_size: u64_at(data, 32),
            vector_size: u64_at(data, 40),
            storage_compatibility: u64_at(data, 48),
        };
        if header.block_alloc_size == 0 {
            header.block_alloc_size = DEFAULT_BLOCK_ALLOC_SIZE;
        }
        if header.vector_size == 0 {
            header.vector_size = STANDARD_VECTOR_SIZE;
        }
        if header.vector_size != STANDARD_VECTOR_SIZE {
            return Err(Error::new(
                ErrorKind::Io,
                format!(
                    "Cannot read database file: DuckDB's compiled vector size is {STANDARD_VECTOR_SIZE} bytes, but \
                     the file has a vector size of {} bytes.",
                    header.vector_size
                ),
            ));
        }
        Ok(header)
    }

    fn write(&self, target: &mut [u8]) {
        let fields = [
            self.iteration,
            self.meta_block,
            self.free_list,
            self.block_count,
            self.block_alloc_size,
            self.vector_size,
            self.storage_compatibility,
        ];
        for (i, field) in fields.iter().enumerate() {
            target[i * 8..i * 8 + 8].copy_from_slice(&field.to_le_bytes());
        }
    }
}

fn verify_block_alloc_size(size: u64) -> Result<()> {
    if !size.is_power_of_two() || !(MIN_BLOCK_ALLOC_SIZE..=MAX_BLOCK_ALLOC_SIZE).contains(&size) {
        return Err(corrupt(format!("invalid block allocation size {size}")));
    }
    Ok(())
}

/// Reads and writes the blocks of one file (`SingleFileBlockManager`).
#[derive(Debug)]
pub struct SingleFileBlockManager {
    path: PathBuf,
    file: Mutex<File>,
    /// the file again, which threads read blocks of at once, each at its place (`pread`)
    reader: File,
    main_header: MainHeader,
    /// what the main header says of the version now, after `rewrite_main_header`
    main_version: std::sync::atomic::AtomicU64,
    state: Mutex<State>,
    /// of an encrypted file: how, with the key
    encryption: Option<FileEncryption>,
}

#[derive(Debug)]
struct State {
    /// The headers are written in turn: this one is current, the next goes to the other slot.
    active_header: usize,
    header: DatabaseHeader,
    /// `free_list`.
    free_list: BTreeSet<u64>,
    /// `multi_use_blocks`: blocks that several segments or tables share.
    multi_use_blocks: HashMap<u64, u32>,
    /// `MetadataManager::blocks`: the metadata blocks, with which of their sub-blocks are free.
    metadata_blocks: std::collections::BTreeMap<u64, u64>,
}

impl SingleFileBlockManager {
    /// `LoadExistingDatabase`, up to the free list, which takes the metadata layer to read. `encryption` is what
    /// `ATTACH` gave to open an encrypted file with.
    pub fn open(path: &Path, read_only: bool, encryption: Option<&EncryptionRequest>) -> Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(path)
            .map_err(|e| io_error(path, e))?;
        let size = file.metadata().map_err(|e| io_error(path, e))?.len();
        if size < 12 {
            return Err(invalid_file(path));
        }
        let mut magic = [0; 12];
        file.read_exact(&mut magic).map_err(|e| io_error(path, e))?;
        if &magic[8..12] != MAGIC_BYTES {
            return Err(invalid_file(path));
        }
        let mut buffer = vec![0; FILE_HEADER_SIZE];
        // the main header is never encrypted
        read_checked(&mut file, path, 0, &mut buffer, None)?;
        let main_header = MainHeader::read(&buffer[BLOCK_HEADER_SIZE..])?;
        let encryption = main_header.encryption(path, encryption, read_only)?;
        let header_size = header_size(encryption.as_ref());
        read_checked(
            &mut file,
            path,
            FILE_HEADER_SIZE as u64,
            &mut buffer,
            encryption.as_ref(),
        )?;
        let h1 = DatabaseHeader::read(&buffer[header_size..])?;
        read_checked(
            &mut file,
            path,
            FILE_HEADER_SIZE as u64 * 2,
            &mut buffer,
            encryption.as_ref(),
        )?;
        let h2 = DatabaseHeader::read(&buffer[header_size..])?;
        let (active_header, header) = if h1.iteration > h2.iteration { (0, h1) } else { (1, h2) };
        verify_block_alloc_size(header.block_alloc_size)?;
        let reader = file.try_clone().map_err(|e| io_error(path, e))?;
        Ok(Self {
            path: path.to_path_buf(),
            file: Mutex::new(file),
            reader,
            main_version: std::sync::atomic::AtomicU64::new(main_header.version_number),
            main_header,
            state: Mutex::new(State {
                active_header,
                header,
                free_list: BTreeSet::new(),
                multi_use_blocks: HashMap::new(),
                metadata_blocks: std::collections::BTreeMap::new(),
            }),
            encryption,
        })
    }

    /// `CreateNewDatabase`: a file with no blocks in `version`, which DuckDB can open too, encrypted as `encryption`
    /// asks (with GCM unless it names a cipher).
    pub fn create(
        path: &Path,
        block_alloc_size: u64,
        version: StorageVersion,
        encryption: Option<&EncryptionRequest>,
    ) -> Result<Self> {
        verify_block_alloc_size(block_alloc_size)?;
        if encryption.is_some_and(|request| !request.writable) {
            return Err(super::encryption::read_only_crypto());
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| io_error(path, e))?;

        let mut buffer = vec![0; FILE_HEADER_SIZE];
        let data = &mut buffer[BLOCK_HEADER_SIZE..];
        data[..4].copy_from_slice(MAGIC_BYTES);
        data[4..12].copy_from_slice(&main_header_version_number(version).to_le_bytes());
        let description = concat!("duckrs ", env!("CARGO_PKG_VERSION")).as_bytes();
        data[DESC_AT..DESC_AT + description.len()].copy_from_slice(description);
        // the identifier is there from v1.4.0 on, and salts the key of an encrypted file
        let mut identifier = [0; DB_IDENTIFIER_LEN];
        if let Some(request) = encryption {
            super::encryption::random_bytes(&mut identifier)?;
            let key = super::encryption::derive_key(&request.key, &identifier);
            let file_encryption = FileEncryption::new(key, request.cipher.unwrap_or(Cipher::Gcm), request.version);
            data[12..20].copy_from_slice(&ENCRYPTED_DATABASE_FLAG.to_le_bytes());
            data[METADATA_AT..METADATA_AT + ENCRYPTION_METADATA_LEN].copy_from_slice(&file_encryption.metadata());
            let (canary, nonce, tag) = file_encryption.canary()?;
            data[CANARY_AT..CANARY_AT + 8].copy_from_slice(&canary);
            data[CANARY_NONCE_AT..CANARY_NONCE_AT + 12].copy_from_slice(&nonce);
            data[CANARY_TAG_AT..CANARY_TAG_AT + 16].copy_from_slice(&tag);
        } else if version >= StorageVersion::V1_4_0 {
            identifier = new_db_identifier(path);
        }
        data[IDENTIFIER_AT..IDENTIFIER_AT + DB_IDENTIFIER_LEN].copy_from_slice(&identifier);
        write_checked(&mut file, path, 0, &mut buffer, None)?;
        let main_header = MainHeader::read(&buffer[BLOCK_HEADER_SIZE..])?;
        let file_encryption = main_header.encryption(path, encryption, false)?;

        let header = DatabaseHeader {
            iteration: 0,
            meta_block: INVALID_BLOCK,
            free_list: INVALID_BLOCK,
            block_count: 0,
            block_alloc_size,
            vector_size: STANDARD_VECTOR_SIZE,
            storage_compatibility: version.database_header_number(),
        };
        for slot in 1..=2 {
            buffer.fill(0);
            header.write(&mut buffer[header_size(file_encryption.as_ref())..]);
            write_checked(
                &mut file,
                path,
                FILE_HEADER_SIZE as u64 * slot,
                &mut buffer,
                file_encryption.as_ref(),
            )?;
        }
        file.sync_all().map_err(|e| io_error(path, e))?;
        drop(file);
        Self::open(path, false, encryption)
    }

    /// How the file is encrypted, if it is.
    pub fn encryption(&self) -> Option<&FileEncryption> {
        self.encryption.as_ref()
    }

    /// `GetBlockHeaderSize`: what is in front of what a block holds.
    fn header_size(&self) -> usize {
        header_size(self.encryption.as_ref())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn main_header(&self) -> &MainHeader {
        &self.main_header
    }

    pub fn header(&self) -> DatabaseHeader {
        self.state.lock().header.clone()
    }

    pub fn block_alloc_size(&self) -> usize {
        self.state.lock().header.block_alloc_size as usize
    }

    /// `GetBlockSize`: what a block holds.
    pub fn block_size(&self) -> usize {
        self.block_alloc_size() - self.header_size()
    }

    pub fn free_list(&self) -> Vec<u64> {
        self.state.lock().free_list.iter().copied().collect()
    }

    pub fn multi_use_blocks(&self) -> HashMap<u64, u32> {
        self.state.lock().multi_use_blocks.clone()
    }

    pub fn set_free_list(&self, free_list: BTreeSet<u64>, multi_use_blocks: HashMap<u64, u32>) {
        let mut state = self.state.lock();
        state.free_list = free_list;
        state.multi_use_blocks = multi_use_blocks;
    }

    /// `MarkBlockAsUsed`: a block a log says holds rows is no longer free, and the file has at least the blocks up
    /// to it.
    pub fn mark_used(&self, block: u64) {
        let mut state = self.state.lock();
        while state.header.block_count < block {
            let free = state.header.block_count;
            state.free_list.insert(free);
            state.header.block_count += 1;
        }
        if state.header.block_count == block {
            state.header.block_count += 1;
        }
        state.free_list.remove(&block);
    }

    /// `GetFreeBlockId`: the lowest free block, or one past the others, which is used from now on.
    pub fn allocate_free_block(&self) -> u64 {
        let mut state = self.state.lock();
        match state.free_list.pop_first() {
            Some(block) => block,
            None => {
                state.header.block_count += 1;
                state.header.block_count - 1
            }
        }
    }

    /// The metadata blocks and the free sub-blocks of each, as a mask.
    pub fn metadata_blocks(&self) -> std::collections::BTreeMap<u64, u64> {
        self.state.lock().metadata_blocks.clone()
    }

    pub fn set_metadata_blocks(&self, blocks: std::collections::BTreeMap<u64, u64>) {
        self.state.lock().metadata_blocks = blocks;
    }

    /// Large inserts write their blocks ahead of the commit, past the blocks that the last checkpoint
    /// knows of, and name them in the log. `MarkBlockAsUsed` of each comes down to this when nothing
    /// is going to be written.
    pub fn include_blocks_past_checkpoint(&self) -> Result<()> {
        let length = self
            .file
            .lock()
            .metadata()
            .map_err(|error| io_error(&self.path, error))?
            .len();
        let mut state = self.state.lock();
        let blocks = length.saturating_sub(BLOCK_START) / state.header.block_alloc_size;
        state.header.block_count = state.header.block_count.max(blocks);
        Ok(())
    }

    fn location(&self, block_id: u64) -> Result<u64> {
        let state = self.state.lock();
        if block_id >= state.header.block_count {
            return Err(corrupt(format!(
                "block {block_id} is past the end of the file, which has {} blocks",
                state.header.block_count
            )));
        }
        Ok(BLOCK_START + block_id * state.header.block_alloc_size)
    }

    /// `Read`: the block without its checksum, which is verified.
    pub fn read_block(&self, block_id: u64) -> Result<Vec<u8>> {
        let location = self.location(block_id)?;
        if let Some(encryption) = &self.encryption {
            let mut block = vec![0; self.block_alloc_size()];
            self.read_at(location, &mut block)?;
            encryption.decrypt_block(&mut block)?;
            verify_checksum(&block, ENCRYPTED_BLOCK_HEADER_SIZE, location)?;
            return Ok(block.split_off(ENCRYPTED_BLOCK_HEADER_SIZE));
        }
        let mut sum = [0; BLOCK_HEADER_SIZE];
        let mut buffer = vec![0; self.block_alloc_size() - BLOCK_HEADER_SIZE];
        self.read_at(location, &mut sum)?;
        self.read_at(location + BLOCK_HEADER_SIZE as u64, &mut buffer)?;
        let (stored, computed) = (u64::from_le_bytes(sum), checksum(&buffer));
        if stored != computed {
            return Err(corrupt(format!(
                "computed checksum {computed} does not match stored checksum {stored} in block at location {location}"
            )));
        }
        Ok(buffer)
    }

    /// The bytes of the file at `location`, read with no lock held.
    #[cfg(unix)]
    fn read_at(&self, location: u64, buffer: &mut [u8]) -> Result<()> {
        use std::os::unix::fs::FileExt;
        crate::execution::profiler::track_read(buffer.len());
        self.reader
            .read_exact_at(buffer, location)
            .map_err(|e| self.read_error(e, location))
    }

    #[cfg(not(unix))]
    fn read_at(&self, location: u64, buffer: &mut [u8]) -> Result<()> {
        let mut file = self.file.lock();
        file.seek(SeekFrom::Start(location))
            .map_err(|e| io_error(&self.path, e))?;
        crate::execution::profiler::track_read(buffer.len());
        file.read_exact(buffer).map_err(|e| self.read_error(e, location))
    }

    fn read_error(&self, error: std::io::Error, location: u64) -> Error {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            corrupt(format!("the file ends before the block at location {location}"))
        } else {
            io_error(&self.path, error)
        }
    }

    /// `Write`: `data` is what a block holds. The block must be in the current header's count or
    /// be a new one, which grows it.
    pub fn write_block(&self, block_id: u64, data: &[u8]) -> Result<()> {
        let alloc_size = self.block_alloc_size();
        let header_size = self.header_size();
        if data.len() != alloc_size - header_size {
            return Err(Error::new(
                ErrorKind::Internal,
                format!("a block holds {} bytes, not {}", alloc_size - header_size, data.len()),
            ));
        }
        let mut buffer = vec![0; alloc_size];
        buffer[header_size..].copy_from_slice(data);
        let location = BLOCK_START + block_id * alloc_size as u64;
        write_checked(
            &mut self.file.lock(),
            &self.path,
            location,
            &mut buffer,
            self.encryption.as_ref(),
        )?;
        let mut state = self.state.lock();
        state.header.block_count = state.header.block_count.max(block_id + 1);
        Ok(())
    }

    /// `WriteHeader`: makes `header` the current one. The blocks it needs must be on disk, and they
    /// are synced before it goes out, so that a crash leaves either this header or the last.
    /// `DatabaseHeader::SetStorageVersionInDatabaseHeader`: the version the file is in.
    pub fn storage_version(&self) -> Result<StorageVersion> {
        let read = self.header().storage_compatibility;
        let main = self.main_version.load(std::sync::atomic::Ordering::SeqCst);
        if main == DEPRECATED_VERSION_NUMBER || read >= StorageVersion::V2_0_0.0 {
            return match read {
                69 | 70 => Ok(StorageVersion(read)),
                _ => Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("Unsupported Storage Version '{read}' in the database header!"),
                )),
            };
        }
        StorageVersion::from_serialization_number(read).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("Deprecated Serialization Version '{read}' is not found!"),
            )
        })
    }

    /// `RewriteMainHeader`: the main header of a file whose version changes, to say the new one.
    pub fn rewrite_main_header(&self, version: StorageVersion) -> Result<()> {
        let number = main_header_version_number(version);
        if self.main_version.load(std::sync::atomic::Ordering::SeqCst) == number {
            return Ok(());
        }
        let mut file = self.file.lock();
        let mut buffer = vec![0; FILE_HEADER_SIZE];
        read_checked(&mut file, &self.path, 0, &mut buffer, None)?;
        buffer[BLOCK_HEADER_SIZE + 4..BLOCK_HEADER_SIZE + 12].copy_from_slice(&number.to_le_bytes());
        write_checked(&mut file, &self.path, 0, &mut buffer, None)?;
        drop(file);
        self.main_version.store(number, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    pub fn write_header(&self, mut header: DatabaseHeader) -> Result<()> {
        let mut file = self.file.lock();
        file.sync_all().map_err(|e| io_error(&self.path, e))?;
        let mut state = self.state.lock();
        header.iteration = state.header.iteration + 1;
        let slot = 1 - state.active_header;
        let mut buffer = vec![0; FILE_HEADER_SIZE];
        header.write(&mut buffer[self.header_size()..]);
        let location = FILE_HEADER_SIZE as u64 * (slot as u64 + 1);
        write_checked(&mut file, &self.path, location, &mut buffer, self.encryption.as_ref())?;
        file.sync_all().map_err(|e| io_error(&self.path, e))?;
        state.active_header = slot;
        state.header = header;
        Ok(())
    }
}

/// `GenerateDBIdentifier`: only has to tell files apart, for logs that end up with the wrong one.
fn new_db_identifier(path: &Path) -> [u8; DB_IDENTIFIER_LEN] {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    let time = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |t| t.as_nanos());
    let mut result = [0; DB_IDENTIFIER_LEN];
    for (part, bytes) in result.chunks_exact_mut(8).enumerate() {
        let hash = BuildHasherDefault::<DefaultHasher>::default().hash_one((part, time, std::process::id(), path));
        bytes.copy_from_slice(&hash.to_le_bytes());
    }
    result
}

/// `GetBlockHeaderSize`: the checksum, and in front of it the nonce and tag of an encrypted file.
fn header_size(encryption: Option<&FileEncryption>) -> usize {
    if encryption.is_some() {
        ENCRYPTED_BLOCK_HEADER_SIZE
    } else {
        BLOCK_HEADER_SIZE
    }
}

/// `CheckChecksum`: the checksum in the 8 bytes before `header_size` is that of what follows.
fn verify_checksum(block: &[u8], header_size: usize, location: u64) -> Result<()> {
    let stored = u64_at(block, header_size - BLOCK_HEADER_SIZE);
    let computed = checksum(&block[header_size..]);
    if stored != computed {
        return Err(corrupt(format!(
            "computed checksum {computed} does not match stored checksum {stored} in block at location {location}"
        )));
    }
    Ok(())
}

/// `ReadAndChecksum`, decrypting an encrypted file's header or block.
fn read_checked(
    file: &mut File,
    path: &Path,
    location: u64,
    buffer: &mut [u8],
    encryption: Option<&FileEncryption>,
) -> Result<()> {
    file.seek(SeekFrom::Start(location)).map_err(|e| io_error(path, e))?;
    crate::execution::profiler::track_read(buffer.len());
    file.read_exact(buffer).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            corrupt(format!("the file ends before the block at location {location}"))
        } else {
            io_error(path, e)
        }
    })?;
    if let Some(encryption) = encryption {
        encryption.decrypt_block(buffer)?;
    }
    verify_checksum(buffer, header_size(encryption), location)
}

/// `ChecksumAndWrite`, encrypting the header or block of an encrypted file (`buffer` holding it at its header size).
fn write_checked(
    file: &mut File,
    path: &Path,
    location: u64,
    buffer: &mut [u8],
    encryption: Option<&FileEncryption>,
) -> Result<()> {
    let header_size = header_size(encryption);
    let sum = checksum(&buffer[header_size..]);
    buffer[header_size - BLOCK_HEADER_SIZE..header_size].copy_from_slice(&sum.to_le_bytes());
    if let Some(encryption) = encryption {
        encryption.encrypt_block(buffer)?;
    }
    file.seek(SeekFrom::Start(location)).map_err(|e| io_error(path, e))?;
    crate::execution::profiler::track_write(buffer.len());
    file.write_all(buffer).map_err(|e| io_error(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("duckrs-blocks-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_new_file_has_no_blocks() {
        let directory = directory("new");
        let path = directory.join("new.db");
        drop(SingleFileBlockManager::create(&path, 16384, StorageVersion::DEFAULT, None).unwrap());
        let blocks = SingleFileBlockManager::open(&path, true, None).unwrap();
        let header = blocks.header();
        assert_eq!(header.iteration, 0);
        assert_eq!(header.meta_block, INVALID_BLOCK);
        assert_eq!(header.block_count, 0);
        assert_eq!(blocks.block_size(), 16384 - BLOCK_HEADER_SIZE);
        assert!(blocks.main_header().library_git_desc.starts_with("duckrs"));
        assert!(blocks.read_block(0).is_err());
        assert!(
            SingleFileBlockManager::create(&path, 16384, StorageVersion::DEFAULT, None).is_err(),
            "it is there"
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn blocks_and_headers_survive_reopening_and_headers_take_turns() {
        let directory = directory("blocks");
        let path = directory.join("blocks.db");
        let blocks = SingleFileBlockManager::create(&path, 16384, StorageVersion::DEFAULT, None).unwrap();
        let data = vec![7; blocks.block_size()];
        blocks.write_block(0, &data).unwrap();
        blocks.write_block(1, &vec![9; blocks.block_size()]).unwrap();
        assert!(blocks.write_block(2, &[1, 2, 3]).is_err());

        let mut header = blocks.header();
        header.meta_block = 1;
        blocks.write_header(header.clone()).unwrap();
        header.meta_block = 0;
        blocks.write_header(header).unwrap();
        drop(blocks);

        let blocks = SingleFileBlockManager::open(&path, true, None).unwrap();
        assert_eq!(blocks.header().iteration, 2);
        assert_eq!(blocks.header().meta_block, 0);
        assert_eq!(blocks.header().block_count, 2);
        assert_eq!(blocks.read_block(0).unwrap(), data);

        // the last header that made it to disk wins, so a torn one takes us back a step
        let mut bytes = std::fs::read(&path).unwrap();
        let newest = FILE_HEADER_SIZE * if blocks.state.lock().active_header == 0 { 1 } else { 2 };
        drop(blocks);
        bytes[newest + BLOCK_HEADER_SIZE] ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            SingleFileBlockManager::open(&path, true, None).is_err(),
            "C++ gives up here too"
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn invalid_block_sizes_are_refused() {
        let directory = directory("size");
        for size in [0, 1000, 16385, 524288] {
            assert!(
                SingleFileBlockManager::create(&directory.join("x.db"), size, StorageVersion::DEFAULT, None).is_err()
            );
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
