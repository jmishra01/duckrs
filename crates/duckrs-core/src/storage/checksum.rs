//! Ported from duckdb/src/common/checksum.cpp @ rust-port-baseline.
//!
//! The checksum of a block or header in a database file, which is part of the file format.

fn checksum_word(x: u64) -> u64 {
    x.wrapping_mul(0xbf58476d1ce4e5b9)
}

/// `ChecksumRemainder`: murmur2 over the last 0 to 7 bytes.
fn checksum_remainder(data: &[u8]) -> u64 {
    const M: u64 = 0xc6a4a7935bd1e995;
    const SEED: u64 = 0xe17a1465;
    const R: u32 = 47;

    let mut h = SEED ^ (data.len() as u64).wrapping_mul(M);
    let words = data.chunks_exact(8);
    let tail = words.remainder();
    for word in words {
        let mut k = u64::from_le_bytes(word.try_into().expect("a chunk of 8 bytes"));
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);
        h ^= k;
        h = h.wrapping_mul(M);
    }
    if !tail.is_empty() {
        for (i, &byte) in tail.iter().enumerate() {
            h ^= u64::from(byte) << (8 * i);
        }
        h = h.wrapping_mul(M);
    }
    h ^= h >> R;
    h = h.wrapping_mul(M);
    h ^= h >> R;
    h
}

/// `Checksum(const uint8_t *, size_t)`.
pub fn checksum(data: &[u8]) -> u64 {
    let mut result = 5381u64;
    let words = data.chunks_exact(8);
    let tail = words.remainder();
    for word in words {
        result ^= checksum_word(u64::from_le_bytes(word.try_into().expect("a chunk of 8 bytes")));
    }
    if !tail.is_empty() {
        result ^= checksum_remainder(tail);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::checksum;

    /// From DuckDB: both cover whole words and a remainder, which take different paths.
    #[test]
    fn matches_the_values_in_a_database_file() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../duckrs/tests/fixtures/compressed.db");
        let file = std::fs::read(path).unwrap();
        for header in file[..3 * 4096].chunks_exact(4096) {
            let stored = u64::from_le_bytes(header[..8].try_into().unwrap());
            assert_eq!(checksum(&header[8..]), stored);
        }
    }

    #[test]
    fn looks_at_every_byte() {
        let data = vec![0u8; 19];
        let base = checksum(&data);
        for i in 0..data.len() {
            let mut changed = data.clone();
            changed[i] = 1;
            assert_ne!(checksum(&changed), base, "byte {i}");
        }
    }
}
