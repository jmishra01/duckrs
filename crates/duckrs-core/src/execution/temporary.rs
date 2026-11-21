// Ported from duckdb/src/storage/temporary_file_manager.cpp, duckdb/src/storage/temporary_memory_manager.cpp,
// `DBConfig::ParseMemoryLimit` of duckdb/src/main/config.cpp and `StringUtil::TryParseFormattedBytes` of
// duckdb/src/common/string_util.cpp @ rust-port-baseline
//
//! Memory and temporary files of a query. C++ tracks every block through the buffer manager and
//! evicts what is not pinned, so any operator spills. Here the data of operators is not in blocks:
//! an operator that can spill asks [`QueryMemory`] for the bytes it is about to hold, and when it
//! gets a no, it writes some of what it holds to a [`SpillFile`] and goes on. The bytes are an
//! estimate ([`estimated_bytes`]), and tables, the query result and operators that do not spill are
//! not counted, so `memory_limit` bounds what spilling operators hold, not the process.
//!
//! The files go to `temp_directory`, which is made when the first one is and removed when the
//! query is done, if it was made for it. A file is removed once nothing reads it.

use crate::storage::log_writer::serialize_chunk;
use crate::storage::wal::deserialize_chunk;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::types::{LogicalType, PhysicalType, StringT};
use duckrs_common::vector::DataChunk;
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use parking_lot::Mutex;
use std::cell::RefCell;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// What a query's spilling operators may hold, and where they spill to (`TemporaryMemoryManager`).
#[derive(Debug)]
pub struct QueryMemory {
    limit: usize,
    used: AtomicUsize,
    directory: PathBuf,
    /// whether this query made the directory
    made_directory: Mutex<bool>,
    max_temp_size: Option<u64>,
    temp_size: AtomicU64,
    next_file: AtomicUsize,
    /// how many threads the query may run a pipeline on, for what it runs on the side (a subquery)
    threads: AtomicUsize,
    /// `MetaTransaction::start_timestamp` of the query's transaction, `i64::MIN` until it is known
    transaction_start: std::sync::atomic::AtomicI64,
}

impl QueryMemory {
    pub fn new(limit: usize, directory: PathBuf, max_temp_size: Option<u64>) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
            directory,
            made_directory: Mutex::new(false),
            max_temp_size,
            temp_size: AtomicU64::new(0),
            next_file: AtomicUsize::new(0),
            threads: AtomicUsize::new(1),
            transaction_start: std::sync::atomic::AtomicI64::new(i64::MIN),
        })
    }

    pub fn set_transaction_start(&self, micros: i64) {
        self.transaction_start.store(micros, Ordering::Relaxed);
    }

    /// The start of the transaction, in microseconds since the epoch: now if nobody said.
    pub fn transaction_start(&self) -> i64 {
        match self.transaction_start.load(Ordering::Relaxed) {
            i64::MIN => now_micros(),
            micros => micros,
        }
    }

    pub fn set_threads(&self, threads: usize) {
        self.threads.store(threads.max(1), Ordering::Relaxed);
    }

    pub fn threads(&self) -> usize {
        self.threads.load(Ordering::Relaxed)
    }

    /// The limit of the settings' defaults: 80% of the machine's memory, and `.tmp`.
    pub fn with_defaults() -> Arc<Self> {
        Self::new(default_memory_limit(), PathBuf::from(".tmp"), None)
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Takes `bytes` of the limit, if they are left.
    pub fn try_reserve(&self, bytes: usize) -> bool {
        self.used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|&total| total <= self.limit)
            })
            .is_ok()
    }

    /// Gives back what [`Self::try_reserve`] took.
    pub fn release(&self, bytes: usize) {
        let _ = self.used.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            Some(used.saturating_sub(bytes))
        });
    }

    /// A new file to spill to.
    pub fn spill_file(self: &Arc<Self>) -> Result<SpillFile> {
        {
            let mut made = self.made_directory.lock();
            if !self.directory.exists() {
                std::fs::create_dir_all(&self.directory).map_err(|e| {
                    Error::new(
                        ErrorKind::Io,
                        format!(
                            "Failed to create the temporary directory \"{}\": {e}",
                            self.directory.display()
                        ),
                    )
                })?;
                *made = true;
            }
        }
        let name = format!(
            "duckrs_temp_storage_{}_{}.tmp",
            std::process::id(),
            self.next_file.fetch_add(1, Ordering::Relaxed)
        );
        let path = self.directory.join(name);
        let file = File::create(&path).map_err(|e| {
            Error::new(
                ErrorKind::Io,
                format!("Failed to create the temporary file \"{}\": {e}", path.display()),
            )
        })?;
        Ok(SpillFile {
            memory: Arc::clone(self),
            path,
            writer: Some(BufWriter::new(file)),
            chunks: 0,
            bytes: 0,
        })
    }

    fn add_temp_bytes(&self, bytes: u64) -> Result<()> {
        let total = self.temp_size.fetch_add(bytes, Ordering::Relaxed) + bytes;
        match self.max_temp_size {
            Some(max) if total > max => Err(Error::new(
                ErrorKind::OutOfMemory,
                format!(
                    "failed to offload data block of size {} ({}/{} used).\nThis limit was set by the \
                     'max_temp_directory_size' setting.\nYou can adjust this setting, by using (for example) PRAGMA \
                     max_temp_directory_size='10GiB'",
                    format_bytes(bytes),
                    format_bytes(total - bytes),
                    format_bytes(max)
                ),
            )),
            _ => Ok(()),
        }
    }
}

impl Drop for QueryMemory {
    fn drop(&mut self) {
        // only when it is empty: another query may be using it
        if *self.made_directory.get_mut() {
            let _ = std::fs::remove_dir(&self.directory);
        }
    }
}

/// Microseconds since the epoch.
pub fn now_micros() -> i64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    elapsed.as_micros() as i64
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<QueryMemory>>> = const { RefCell::new(None) };
}

/// The memory of the query this thread runs.
pub fn current() -> Arc<QueryMemory> {
    CURRENT.with(|current| Arc::clone(current.borrow_mut().get_or_insert_with(QueryMemory::with_defaults)))
}

/// Runs `f` with `memory` as this thread's [`current`].
pub fn with_memory<T>(memory: Arc<QueryMemory>, f: impl FnOnce() -> T) -> T {
    let previous = CURRENT.with(|current| current.replace(Some(memory)));
    struct Restore(Option<Arc<QueryMemory>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            CURRENT.with(|current| *current.borrow_mut() = previous);
        }
    }
    let _restore = Restore(previous);
    f()
}

/// Chunks written one after another to a temporary file, and read back in that order.
#[derive(Debug)]
pub struct SpillFile {
    memory: Arc<QueryMemory>,
    path: PathBuf,
    writer: Option<BufWriter<File>>,
    chunks: usize,
    bytes: u64,
}

impl SpillFile {
    fn io_error(&self, e: std::io::Error) -> Error {
        Error::new(
            ErrorKind::Io,
            format!("Failed to write the temporary file \"{}\": {e}", self.path.display()),
        )
    }

    pub fn write(&mut self, chunk: &DataChunk) -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        let bytes = serialize_chunk(chunk)?;
        self.memory.add_temp_bytes(bytes.len() as u64 + 8)?;
        let writer = self
            .writer
            .as_mut()
            .expect("a spill file that is being read takes no writes");
        let result = writer
            .write_all(&(bytes.len() as u64).to_le_bytes())
            .and_then(|()| writer.write_all(&bytes));
        result.map_err(|e| self.io_error(e))?;
        self.chunks += 1;
        self.bytes += bytes.len() as u64 + 8;
        Ok(())
    }

    /// How many chunks went in.
    pub fn chunk_count(&self) -> usize {
        self.chunks
    }

    pub fn is_empty(&self) -> bool {
        self.chunks == 0
    }

    /// Reads the chunks back. The file goes away with the reader.
    pub fn into_reader(mut self) -> Result<SpillReader> {
        if let Some(writer) = self.writer.take() {
            writer.into_inner().map_err(|e| self.io_error(e.into_error()))?;
        }
        let file = File::open(&self.path).map_err(|e| self.io_error(e))?;
        Ok(SpillReader {
            reader: BufReader::new(file),
            left: self.chunks,
            file: self,
        })
    }
}

impl Drop for SpillFile {
    fn drop(&mut self) {
        self.writer = None;
        let _ = std::fs::remove_file(&self.path);
        self.memory.temp_size.fetch_sub(
            self.bytes.min(self.memory.temp_size.load(Ordering::Relaxed)),
            Ordering::Relaxed,
        );
    }
}

/// Reads the chunks of a [`SpillFile`] in order.
#[derive(Debug)]
pub struct SpillReader {
    file: SpillFile,
    reader: BufReader<File>,
    left: usize,
}

impl SpillReader {
    pub fn next_chunk(&mut self) -> Result<Option<DataChunk>> {
        if self.left == 0 {
            return Ok(None);
        }
        let mut length = [0; 8];
        let mut read = || -> std::io::Result<Vec<u8>> {
            self.reader.read_exact(&mut length)?;
            let mut bytes = vec![0; u64::from_le_bytes(length) as usize];
            self.reader.read_exact(&mut bytes)?;
            Ok(bytes)
        };
        let bytes = read().map_err(|e| {
            Error::new(
                ErrorKind::Io,
                format!(
                    "Failed to read the temporary file \"{}\": {e}",
                    self.file.path.display()
                ),
            )
        })?;
        self.left -= 1;
        deserialize_chunk(&bytes).map(Some)
    }
}

/// Files for the partitions of spilled rows. The rows of a partition gather until they fill a
/// vector, so that a file gets whole chunks.
#[derive(Debug)]
pub struct PartitionedSpill {
    partitions: Vec<Mutex<PartitionWriter>>,
}

#[derive(Debug, Default)]
struct PartitionWriter {
    file: Option<SpillFile>,
    rows: Option<ColumnDataCollection>,
}

impl PartitionWriter {
    fn flush(&mut self) -> Result<()> {
        let Some(rows) = self.rows.take() else {
            return Ok(());
        };
        if self.file.is_none() {
            self.file = Some(current().spill_file()?);
        }
        let file = self.file.as_mut().expect("just made");
        for chunk in rows.chunks() {
            file.write(chunk)?;
        }
        Ok(())
    }
}

impl PartitionedSpill {
    pub fn new(partitions: usize) -> Self {
        Self {
            partitions: (0..partitions).map(|_| Mutex::default()).collect(),
        }
    }

    /// Adds the rows of `chunk` to `partition`.
    pub fn write(&self, partition: usize, chunk: &DataChunk) -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        let mut writer = self.partitions[partition].lock();
        let rows = writer
            .rows
            .get_or_insert_with(|| ColumnDataCollection::new(chunk.types().to_vec()));
        rows.append(chunk);
        if rows.len() >= STANDARD_VECTOR_SIZE {
            writer.flush()?;
        }
        Ok(())
    }

    /// The files that rows went to, by partition.
    pub fn take(&self) -> Result<Vec<Option<SpillFile>>> {
        self.partitions
            .iter()
            .map(|writer| {
                let mut writer = writer.lock();
                writer.flush()?;
                Ok(writer.file.take())
            })
            .collect()
    }
}

/// Roughly how many bytes `chunk` holds: its fixed-size values, its strings, and a guess for each
/// row of a nested column.
pub fn estimated_bytes(chunk: &DataChunk) -> usize {
    let count = chunk.len();
    chunk
        .columns()
        .iter()
        .map(|column| {
            let ty: &LogicalType = column.logical_type();
            match ty.physical_type() {
                PhysicalType::Varchar => {
                    let mut flat = column.clone();
                    flat.flatten(count);
                    let strings = flat.data::<StringT>();
                    let heap: usize = strings
                        .iter()
                        .take(count)
                        .map(|s| s.len())
                        .filter(|&len| len > StringT::INLINE_LENGTH)
                        .sum();
                    count * std::mem::size_of::<StringT>() + heap
                }
                PhysicalType::List | PhysicalType::Struct | PhysicalType::Array => count * 64,
                physical => count * physical.size().max(1),
            }
        })
        .sum::<usize>()
        + count / 8
}

/// The machine's memory, if it can be found (`FileSystem::GetAvailableMemory`).
pub fn physical_memory() -> Option<u64> {
    #[cfg(unix)]
    {
        // SAFETY: sysconf only reads the configuration value it is asked for
        let (pages, page_size) = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE)) };
        if pages > 0 && page_size > 0 {
            return Some(pages as u64 * page_size as u64);
        }
    }
    None
}

/// `DBConfig::SetDefaultMaxMemory`: 80% of the machine's memory, or no limit when that is unknown.
pub fn default_memory_limit() -> usize {
    physical_memory().map_or(usize::MAX, |memory| (memory / 10 * 8) as usize)
}

/// What `max_temp_directory_size` says by default. duckrs puts no limit on it then.
pub const DEFAULT_TEMP_DIRECTORY_SIZE: &str = "90% of available disk space";

/// A size as a setting holds it: what [`format_bytes`] shows, where `PiB` (beyond what
/// [`parse_formatted_bytes`] takes) is no limit. `None` for no limit.
pub fn parse_setting_bytes(text: &str) -> Result<Option<u64>> {
    if text.ends_with(" PiB") || text == DEFAULT_TEMP_DIRECTORY_SIZE {
        return Ok(None);
    }
    parse_memory_limit(text)
}

/// `DBConfig::ParseMemoryLimit`: `None` for no limit.
pub fn parse_memory_limit(text: &str) -> Result<Option<u64>> {
    if text.starts_with('-') || text == "null" || text == "none" {
        return Ok(None);
    }
    match parse_formatted_bytes(text) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(message) if message == "Memory cannot be negative" => Ok(None),
        Err(message) => Err(Error::new(ErrorKind::Parser, message)),
    }
}

/// `StringUtil::TryParseFormattedBytes`.
pub fn parse_formatted_bytes(text: &str) -> std::result::Result<u64, String> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    let start = index;
    while index < bytes.len() && (bytes[index].is_ascii_digit() || b".eE-".contains(&bytes[index])) {
        index += 1;
    }
    if index == start {
        return Err("Memory must have a number (e.g. 1GB)".into());
    }
    let number = &text[start..index];
    let limit: f64 = number
        .parse()
        .map_err(|_| format!("Invalid memory limit: '{number}'"))?;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    let unit_start = index;
    while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    if limit < 0.0 {
        return Err("Memory cannot be negative".into());
    }
    let unit = text[unit_start..index].to_lowercase();
    let multiplier: u64 = match unit.as_str() {
        "byte" | "bytes" | "b" => 1,
        "kilobyte" | "kilobytes" | "kb" | "k" => 1000,
        "megabyte" | "megabytes" | "mb" | "m" => 1000 * 1000,
        "gigabyte" | "gigabytes" | "gb" | "g" => 1000 * 1000 * 1000,
        "terabyte" | "terabytes" | "tb" | "t" => 1000 * 1000 * 1000 * 1000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        _ => {
            return Err(format!(
                "Unknown unit for memory: '{unit}' (expected: KB, MB, GB, TB for 1000^i units or KiB, MiB, GiB, TiB \
                 for 1024^i units)"
            ));
        }
    };
    if number.bytes().all(|b| b.is_ascii_digit()) {
        let integer: u64 = number
            .parse()
            .map_err(|_| "Memory value out of range: value is too large".to_string())?;
        return integer
            .checked_mul(multiplier)
            .ok_or_else(|| "Memory value out of range: value is too large".to_string());
    }
    let result = limit * multiplier as f64;
    if result >= u64::MAX as f64 {
        return Err("Memory value out of range: value is too large".into());
    }
    Ok(result as u64)
}

/// `StringUtil::BytesToHumanReadableString` with 1024: the largest unit with a whole number, and
/// its first decimal, cut off rather than rounded.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["bytes", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut parts = [0u64; 6];
    parts[0] = bytes;
    for i in 1..6 {
        parts[i] = parts[i - 1] / 1024;
        parts[i - 1] %= 1024;
    }
    for i in (1..6).rev() {
        if parts[i] != 0 {
            return format!("{}.{} {}", parts[i], parts[i - 1] * 10 / 1024, UNITS[i]);
        }
    }
    if bytes == 1 {
        "1 byte".into()
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use duckrs_common::value::Value;

    #[test]
    fn memory_limits_parse_like_duckdb() {
        assert_eq!(parse_formatted_bytes("1GB"), Ok(1_000_000_000));
        assert_eq!(parse_formatted_bytes(" 300 MiB"), Ok(300 << 20));
        assert_eq!(parse_formatted_bytes("1.5kb"), Ok(1500));
        assert!(
            parse_formatted_bytes("100")
                .unwrap_err()
                .starts_with("Unknown unit for memory: ''")
        );
        assert!(parse_formatted_bytes("GB").is_err());
        assert_eq!(parse_memory_limit("-1").unwrap(), None);
        assert_eq!(parse_memory_limit("none").unwrap(), None);
        assert_eq!(format_bytes(300 << 20), "300.0 MiB");
        assert_eq!(format_bytes(1_000_000_000), "953.6 MiB");
        assert_eq!(format_bytes(1), "1 byte");
    }

    #[test]
    fn spill_files_give_back_their_chunks_and_go_away() {
        let directory = std::env::temp_dir().join(format!("duckrs_spill_test_{}", std::process::id()));
        let memory = QueryMemory::new(1000, directory.clone(), None);
        assert!(memory.try_reserve(600));
        assert!(!memory.try_reserve(600));
        memory.release(600);
        assert!(memory.try_reserve(600));
        let mut file = memory.spill_file().unwrap();
        let types = [LogicalType::INTEGER, LogicalType::VARCHAR];
        let mut chunk = DataChunk::new(&types);
        for i in 0..100 {
            let text = Value::varchar(format!("a string that is not inlined {i}"));
            chunk.append_row(&[Value::integer(i), text]).unwrap();
        }
        file.write(&chunk).unwrap();
        file.write(&chunk).unwrap();
        let path = file.path.clone();
        let mut reader = file.into_reader().unwrap();
        for _ in 0..2 {
            let read = reader.next_chunk().unwrap().unwrap();
            assert_eq!(read.len(), 100);
            assert_eq!(read.get_value(1, 99), chunk.get_value(1, 99));
        }
        assert!(reader.next_chunk().unwrap().is_none());
        drop(reader);
        assert!(!path.exists());
        drop(memory);
        assert!(!directory.exists());
    }

    #[test]
    fn the_temporary_directory_has_a_limit() {
        let directory = std::env::temp_dir().join(format!("duckrs_spill_limit_{}", std::process::id()));
        let memory = QueryMemory::new(1000, directory, Some(100));
        let mut file = memory.spill_file().unwrap();
        let mut chunk = DataChunk::new(&[LogicalType::BIGINT]);
        for i in 0..100 {
            chunk.append_row(&[Value::bigint(i)]).unwrap();
        }
        let error = file.write(&chunk).unwrap_err().to_string();
        assert!(error.contains("max_temp_directory_size"), "{error}");
    }
}
