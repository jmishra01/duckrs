//! FSST: a table of up to 255 symbols of up to 8 bytes, each replaced by a one byte code. What the
//! table doesn't cover is escaped, a byte taking two. `third_party/fsst` of C++ trains the table the
//! same way, in rounds over a sample, though it needn't: any table that `duckdb_fsst_import` takes is
//! valid, and only the size of what comes out depends on it.

use std::collections::{HashMap, HashSet};

const MAX_SYMBOLS: usize = 255;
const MAX_LENGTH: usize = 8;
/// The code that says the next byte is itself.
const ESCAPE: u8 = 255;
/// `FSST_SAMPLETARGET`.
const SAMPLE_TARGET: usize = 1 << 14;
/// `FSST_SAMPLELINE`: a long string only gives so much of itself.
const SAMPLE_LINE: usize = 512;
/// `FSST_VERSION`.
const VERSION: u64 = 20190218;
/// How much of the sample each round sees, out of 128.
const ROUNDS: [usize; 5] = [8, 38, 68, 98, 128];
/// What a code is while training: a symbol, or past this a byte that was escaped.
const LITERALS: usize = 256;
const IDS: usize = LITERALS + 256;

/// Up to 8 bytes in a word, with zeros past the end.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Symbol {
    value: u64,
    length: u8,
}

impl Symbol {
    fn byte(byte: u8) -> Self {
        Symbol {
            value: u64::from(byte),
            length: 1,
        }
    }

    fn concat(self, other: Symbol) -> Option<Symbol> {
        let length = self.length + other.length;
        (usize::from(length) <= MAX_LENGTH).then(|| Symbol {
            value: self.value | other.value << (u32::from(self.length) * 8),
            length,
        })
    }

    fn mask(self) -> u64 {
        u64::MAX >> (64 - u32::from(self.length) * 8)
    }

    fn bytes(self) -> [u8; 8] {
        self.value.to_le_bytes()
    }
}

/// The first bytes of `data`, as many as fit a word.
fn word(data: &[u8]) -> u64 {
    let mut bytes = [0; 8];
    let length = data.len().min(MAX_LENGTH);
    bytes[..length].copy_from_slice(&data[..length]);
    u64::from_le_bytes(bytes)
}

struct LongSymbol {
    value: u64,
    mask: u64,
    length: u8,
    code: u8,
}

pub struct SymbolTable {
    /// By code, which `duckdb_fsst_import` numbers by length: 2 to 8 and then 1.
    symbols: Vec<Symbol>,
    /// Per byte, its code as a symbol of one byte.
    singles: [Option<u8>; 256],
    /// The longer symbols by their first two bytes, the longest first, as the longest match wins.
    long: Vec<LongSymbol>,
    /// Per two first bytes, where in `long` they start.
    starts: Vec<u32>,
}

impl SymbolTable {
    fn new(mut symbols: Vec<Symbol>) -> Self {
        symbols.sort_by_key(|symbol| {
            let rank = if symbol.length == 1 { 9 } else { symbol.length };
            (rank, symbol.value)
        });
        let mut singles = [None; 256];
        let mut long = Vec::new();
        for (code, symbol) in symbols.iter().enumerate() {
            if symbol.length == 1 {
                singles[symbol.value as usize] = Some(code as u8);
            } else {
                long.push(LongSymbol {
                    value: symbol.value,
                    mask: symbol.mask(),
                    length: symbol.length,
                    code: code as u8,
                });
            }
        }
        long.sort_by_key(|symbol| (symbol.value & 0xffff, std::cmp::Reverse(symbol.length)));
        let mut starts = vec![0u32; (1 << 16) + 1];
        for symbol in &long {
            starts[(symbol.value & 0xffff) as usize + 1] += 1;
        }
        for index in 1..starts.len() {
            starts[index] += starts[index - 1];
        }
        SymbolTable {
            symbols,
            singles,
            long,
            starts,
        }
    }

    /// The code at the start of `rest`, which isn't empty, and how much of it that takes.
    fn find(&self, rest: &[u8]) -> Option<(u8, usize)> {
        let value = word(rest);
        if rest.len() > 1 {
            let key = (value & 0xffff) as usize;
            let bucket = &self.long[self.starts[key] as usize..self.starts[key + 1] as usize];
            for symbol in bucket {
                if usize::from(symbol.length) <= rest.len() && value & symbol.mask == symbol.value {
                    return Some((symbol.code, usize::from(symbol.length)));
                }
            }
        }
        self.singles[usize::from(rest[0])].map(|code| (code, 1))
    }

    /// Replaces what `out` holds with `input`, compressed.
    pub fn compress(&self, input: &[u8], out: &mut Vec<u8>) {
        out.clear();
        let mut rest = input;
        while !rest.is_empty() {
            match self.find(rest) {
                Some((code, length)) => {
                    out.push(code);
                    rest = &rest[length..];
                }
                None => {
                    out.extend_from_slice(&[ESCAPE, rest[0]]);
                    rest = &rest[1..];
                }
            }
        }
    }

    /// `duckdb_fsst_export`.
    pub fn export(&self) -> Vec<u8> {
        let version = VERSION << 32 | (self.symbols.len() as u64) << 8 | 1;
        let mut out = version.to_le_bytes().to_vec();
        out.push(0); // not zero terminated
        let mut histogram = [0u8; MAX_LENGTH];
        for symbol in &self.symbols {
            histogram[usize::from(symbol.length) - 1] += 1;
        }
        out.extend_from_slice(&histogram);
        for symbol in &self.symbols {
            out.extend_from_slice(&symbol.bytes()[..usize::from(symbol.length)]);
        }
        out
    }

    /// `SymbolTable::buildSymbolTable`: from the sample as this table compresses it, what is used
    /// most and what is used most in a row, the best of which are the next table.
    fn next(&self, sample: &[&[u8]], fraction: usize) -> SymbolTable {
        let mut single = vec![0u32; IDS];
        let mut pairs = vec![0u32; IDS * IDS];
        for (index, string) in sample.iter().enumerate() {
            if fraction < 128 && index.wrapping_mul(0x9E37_79B1) >> 8 & 127 >= fraction {
                continue;
            }
            let mut previous = None;
            let mut rest = *string;
            while !rest.is_empty() {
                let (id, length) = match self.find(rest) {
                    Some((code, length)) => (usize::from(code), length),
                    None => (LITERALS + usize::from(rest[0]), 1),
                };
                single[id] += 1;
                if let Some(previous) = previous {
                    pairs[previous * IDS + id] += 1;
                }
                previous = Some(id);
                rest = &rest[length..];
            }
        }
        let symbol = |id: usize| match id.checked_sub(LITERALS) {
            Some(byte) => Symbol::byte(byte as u8),
            None => self.symbols[id],
        };
        // rare ones aren't worth it, which also improves the rest
        let threshold = (5 * fraction / 128) as u64;
        let mut gains: HashMap<Symbol, u64> = HashMap::new();
        let mut add = |symbol: Symbol, count: u64| {
            if count >= threshold {
                *gains.entry(symbol).or_default() += count * u64::from(symbol.length);
            }
        };
        let used: Vec<usize> = (0..IDS).filter(|&id| single[id] > 0).collect();
        for &first in &used {
            let left = symbol(first);
            // bytes of their own mean fewer escapes
            let weight = if left.length == 1 { 8 } else { 1 };
            add(left, u64::from(single[first]) * weight);
            // the last round only picks
            if fraction == 128 {
                continue;
            }
            for &second in &used {
                let count = pairs[first * IDS + second];
                if count > 0
                    && let Some(joined) = left.concat(symbol(second))
                {
                    add(joined, u64::from(count));
                }
            }
        }
        let mut best: Vec<(Symbol, u64)> = gains.into_iter().collect();
        best.sort_by_key(|&(symbol, gain)| (std::cmp::Reverse(gain), symbol.length, symbol.value));
        best.truncate(MAX_SYMBOLS);
        SymbolTable::new(best.into_iter().map(|(symbol, _)| symbol).collect())
    }

    /// `duckdb_fsst_create`, or `None` with nothing to train on.
    pub fn train(strings: &[&[u8]]) -> Option<SymbolTable> {
        let sample = sample(strings);
        if sample.is_empty() {
            return None;
        }
        let mut table = SymbolTable::new(Vec::new());
        for fraction in ROUNDS {
            table = table.next(&sample, fraction);
        }
        Some(table)
    }
}

/// Distinct strings from all over the input, the size of `SAMPLE_TARGET`.
fn sample<'a>(strings: &[&'a [u8]]) -> Vec<&'a [u8]> {
    let mut seen = HashSet::new();
    let mut sample = Vec::new();
    let mut size = 0;
    // spread out, as sorted strings differ from those next to them the least
    let step = (strings.len() / 4096).max(1);
    for start in 0..step {
        for string in strings.iter().skip(start).step_by(step) {
            let line = &string[..string.len().min(SAMPLE_LINE)];
            if !line.is_empty() && seen.insert(line) {
                sample.push(line);
                size += line.len();
                if size >= SAMPLE_TARGET {
                    return sample;
                }
            }
        }
    }
    sample
}

#[cfg(test)]
mod tests {
    use super::super::compression::fsst_roundtrip;
    use super::*;
    use proptest::prelude::*;

    fn compressed_size(table: &SymbolTable, strings: &[&[u8]]) -> usize {
        let mut out = Vec::new();
        strings
            .iter()
            .map(|string| {
                table.compress(string, &mut out);
                assert_eq!(fsst_roundtrip(&table.export(), &out), *string);
                out.len()
            })
            .sum()
    }

    #[test]
    fn text_gets_smaller() {
        let urls: Vec<String> = (0..2000).map(|i| format!("https://example.com/items/{i}")).collect();
        let strings: Vec<&[u8]> = urls.iter().map(|url| url.as_bytes()).collect();
        let table = SymbolTable::train(&strings).unwrap();
        let plain: usize = strings.iter().map(|string| string.len()).sum();
        assert!(compressed_size(&table, &strings) * 2 < plain);
    }

    #[test]
    fn all_lengths_go_in_the_order_of_their_codes() {
        let strings: [&[u8]; 3] = [b"a", b"abcdefgh", b"abcdefghabcdefgh"];
        let repeated: Vec<&[u8]> = strings.iter().copied().cycle().take(600).collect();
        let table = SymbolTable::train(&repeated).unwrap();
        assert!(table.symbols.is_sorted_by_key(|symbol| (symbol.length + 7) % 8));
        compressed_size(&table, &repeated);
    }

    #[test]
    fn nothing_to_train_on_is_no_table() {
        assert!(SymbolTable::train(&[b"".as_slice()]).is_none());
    }

    proptest! {
        /// Whatever a table is trained on, bytes it hasn't seen still come back.
        #[test]
        fn any_bytes_come_back(
            train in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..40), 1..60),
            other in proptest::collection::vec(any::<u8>(), 0..200),
        ) {
            let strings: Vec<&[u8]> = train.iter().map(Vec::as_slice).collect();
            if let Some(table) = SymbolTable::train(&strings) {
                compressed_size(&table, &strings);
                compressed_size(&table, &[&other]);
            }
        }
    }
}
