//! A damaged file must give an error, never a panic or a huge allocation. Checksums catch damage
//! from the disk, so to get at the code behind them this changes bytes and then fixes the checksum,
//! as a file made to do harm would.

use duckrs::Database;
use duckrs_core::storage::checksum::checksum;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

const HEADERS: usize = 3 * 4096;

/// A small, seeded generator: a failure must come back on every run.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit as u64) as usize
    }
}

fn block_alloc_size(file: &[u8]) -> usize {
    u64::from_le_bytes(file[4096 + 40..4096 + 48].try_into().unwrap()) as usize
}

fn fuzz(fixture: &str, seed: u64, rounds: usize) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    let original = std::fs::read(path).unwrap();
    let block_size = block_alloc_size(&original);
    let blocks = (original.len() - HEADERS) / block_size;
    let directory = std::env::temp_dir().join(format!("duckrs-fuzz-{}-{fixture}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let target = directory.join("fuzzed.db");
    let mut random = Random(seed);

    for round in 0..rounds {
        let mut bytes = original.clone();
        let block = random.below(blocks);
        let start = HEADERS + block * block_size;
        // most of what parses is at the start of a sub-block or segment, so stay near one half the time
        for _ in 0..1 + random.below(8) {
            let offset = if random.below(2) == 0 {
                8 + random.below(block_size - 8)
            } else {
                8 + random.below(400)
            };
            bytes[start + offset] = match random.below(4) {
                0 => 0,
                1 => 0xFF,
                2 => 0x80,
                _ => random.next() as u8,
            };
        }
        let sum = checksum(&bytes[start + 8..start + block_size]);
        bytes[start..start + 8].copy_from_slice(&sum.to_le_bytes());
        std::fs::write(&target, &bytes).unwrap();

        let outcome = catch_unwind(AssertUnwindSafe(|| {
            if let Ok(database) = Database::open(&target, true) {
                // what opens has to work too
                let mut connection = database.connect().unwrap();
                let _ = connection.query("SELECT 1");
            }
        }));
        assert!(
            outcome.is_ok(),
            "seed {seed}, round {round}, block {block} of {fixture}"
        );
    }
    std::fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn compressed_blocks() {
    fuzz("compressed.db", 0x9E3779B97F4A7C15, 1500);
}

#[test]
fn bitpacked_blocks() {
    fuzz("bitpacked.db", 0xD1B54A32D192ED03, 300);
}

#[test]
fn uncompressed_blocks() {
    fuzz("compatible.db", 0x2545F4914F6CDD1D, 1000);
}
