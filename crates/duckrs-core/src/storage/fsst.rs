// Ported from duckdb/third_party/fsst/libfsst.cpp and libfsst.hpp @ rust-port-baseline
//
//! FSST: a table of up to 255 symbols of up to 8 bytes, each replaced by a one byte code; a byte the table
//! doesn't cover is escaped, taking two. This is libfsst's `duckdb_fsst_create` (the sample, the five rounds of
//! `buildSymbolTable` with its counters, `finalize`) and the scalar `compressBulk`, so that a table and what it
//! compresses to are those of C++, byte for byte: how many strings fit a block depends on them. It assumes
//! what C++ is built with on a 64-bit machine with a signed `char`.

use std::collections::HashMap;

/// `FSST_VERSION`.
const VERSION: u64 = 20190218;
/// `FSST_LEN_BITS`.
const LEN_BITS: u32 = 12;
/// `FSST_CODE_BASE`: below are the escaped bytes.
const CODE_BASE: usize = 256;
/// `FSST_CODE_MAX`.
const CODE_MAX: usize = 1 << 9;
/// `FSST_CODE_MASK`.
const CODE_MASK: u64 = (CODE_MAX - 1) as u64;
/// `FSST_HASH_LOG2SIZE`.
const HASH_TAB_SIZE: usize = 1 << 10;
/// `FSST_ICL_FREE`: a free bucket of the hash table.
const ICL_FREE: u64 = (15 << 28) | (CODE_MASK << 16);
/// `FSST_SAMPLETARGET`.
const SAMPLE_TARGET: usize = 1 << 14;
/// `FSST_SAMPLEMAXSZ`.
const SAMPLE_MAX_SIZE: usize = 2 * SAMPLE_TARGET;
/// `FSST_SAMPLELINE`.
const SAMPLE_LINE: usize = 512;

/// `FSST_HASH`.
fn fsst_hash(w: u64) -> u64 {
    const PRIME: u64 = 2971215073;
    let product = w.wrapping_mul(PRIME);
    product ^ (product >> 15)
}

/// `fsst_unaligned_load`: 8 bytes from `at`, which has them.
fn load(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(data[at..at + 8].try_into().expect("8 bytes"))
}

/// `Symbol`: up to 8 bytes in `num`, and `icl` = length << 28 | code << 16 | the bits past them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Symbol {
    num: u64,
    icl: u64,
}

impl Symbol {
    /// `Symbol(u8 c, u16 code)`.
    fn byte(c: u8, code: u64) -> Self {
        Symbol {
            num: u64::from(c),
            icl: (1 << 28) | (code << 16) | 56,
        }
    }

    /// `Symbol(const u8* begin, const u8* end)`: the first bytes of `input`, which isn't empty.
    fn of(input: &[u8]) -> Self {
        let length = input.len().min(8);
        let mut bytes = [0u8; 8];
        bytes[..length].copy_from_slice(&input[..length]);
        let mut symbol = Symbol {
            num: u64::from_le_bytes(bytes),
            icl: 0,
        };
        symbol.set_code_len(CODE_MAX as u64, length as u64);
        symbol
    }

    fn set_code_len(&mut self, code: u64, length: u64) {
        self.icl = (length << 28) | (code << 16) | ((8 - length) * 8);
    }

    fn length(self) -> usize {
        (self.icl >> 28) as usize
    }

    fn code(self) -> u64 {
        (self.icl >> 16) & CODE_MASK
    }

    /// `(u8) icl`: how far to shift a word down to the symbol's bytes.
    fn ignored(self) -> u32 {
        u32::from(self.icl as u8)
    }

    fn first(self) -> usize {
        (self.num & 0xFF) as usize
    }

    fn first2(self) -> usize {
        (self.num & 0xFFFF) as usize
    }

    /// `val.str[0]` as the `char` it is, signed.
    fn first_char(self) -> i32 {
        i32::from(self.num as u8 as i8)
    }

    fn hash(self) -> u64 {
        fsst_hash(self.num & 0xFF_FFFF)
    }

    /// `concat`.
    fn concat(self, other: Symbol) -> Symbol {
        let length = (self.length() + other.length()).min(8);
        let mut symbol = Symbol {
            num: (other.num << (8 * self.length())) | self.num,
            icl: 0,
        };
        symbol.set_code_len(CODE_MASK, length as u64);
        symbol
    }
}

fn mask(ignored: u32) -> u64 {
    u64::MAX >> ignored
}

/// `libfsst::SymbolTable`.
#[derive(Clone)]
pub struct SymbolTable {
    short_codes: Vec<u16>,
    byte_codes: [u16; 256],
    symbols: Vec<Symbol>,
    hash_tab: Vec<Symbol>,
    n_symbols: u16,
    suffix_lim: u16,
    terminator: u16,
    len_histo: [u16; 9],
}

impl SymbolTable {
    fn new() -> Self {
        let mut symbols = vec![Symbol::byte(0, CODE_MASK); CODE_MAX];
        for (i, symbol) in symbols.iter_mut().enumerate().take(CODE_BASE) {
            *symbol = Symbol::byte(i as u8, i as u64 | (1 << LEN_BITS));
        }
        SymbolTable {
            short_codes: (0..65536u32).map(|i| ((1 << LEN_BITS) | (i & 255)) as u16).collect(),
            byte_codes: std::array::from_fn(|i| ((1 << LEN_BITS) | i) as u16),
            symbols,
            hash_tab: vec![Symbol { num: 0, icl: ICL_FREE }; HASH_TAB_SIZE],
            n_symbols: 0,
            suffix_lim: CODE_MAX as u16,
            terminator: 0,
            len_histo: [0; 9],
        }
    }

    fn clear(&mut self) {
        self.len_histo = [0; 9];
        for i in CODE_BASE..CODE_BASE + usize::from(self.n_symbols) {
            let symbol = self.symbols[i];
            match symbol.length() {
                1 => {
                    let value = symbol.first();
                    self.byte_codes[value] = ((1 << LEN_BITS) | value) as u16;
                }
                2 => {
                    let value = symbol.first2();
                    self.short_codes[value] = ((1 << LEN_BITS) | (value & 255)) as u16;
                }
                _ => {
                    let index = symbol.hash() as usize & (HASH_TAB_SIZE - 1);
                    self.hash_tab[index] = Symbol { num: 0, icl: ICL_FREE };
                }
            }
        }
        self.n_symbols = 0;
    }

    fn hash_insert(&mut self, symbol: Symbol) -> bool {
        let index = symbol.hash() as usize & (HASH_TAB_SIZE - 1);
        if self.hash_tab[index].icl < ICL_FREE {
            return false;
        }
        self.hash_tab[index] = Symbol {
            num: symbol.num & mask(symbol.ignored()),
            icl: symbol.icl,
        };
        true
    }

    fn add(&mut self, mut symbol: Symbol) -> bool {
        let length = symbol.length();
        let code = CODE_BASE + usize::from(self.n_symbols);
        symbol.set_code_len(code as u64, length as u64);
        match length {
            1 => self.byte_codes[symbol.first()] = (code + (1 << LEN_BITS)) as u16,
            2 => self.short_codes[symbol.first2()] = (code + (2 << LEN_BITS)) as u16,
            _ => {
                if !self.hash_insert(symbol) {
                    return false;
                }
            }
        }
        self.symbols[code] = symbol;
        self.n_symbols += 1;
        self.len_histo[length - 1] += 1;
        true
    }

    /// `findLongestSymbol` of the bytes at the start of `input`, which isn't empty.
    fn find_longest_symbol(&self, input: &[u8]) -> usize {
        let symbol = Symbol::of(input);
        let bucket = self.hash_tab[symbol.hash() as usize & (HASH_TAB_SIZE - 1)];
        if bucket.icl <= symbol.icl && bucket.num == symbol.num & mask(bucket.ignored()) {
            return ((bucket.icl >> 16) & CODE_MASK) as usize;
        }
        if symbol.length() >= 2 {
            let code = u64::from(self.short_codes[symbol.first2()]) & CODE_MASK;
            if code >= CODE_BASE as u64 {
                return code as usize;
            }
        }
        (u64::from(self.byte_codes[symbol.first()]) & CODE_MASK) as usize
    }

    /// `finalize`: the codes renumbered by length, 2 to 8 and then 1, for a table that isn't zero terminated.
    fn finalize(&mut self) {
        let n = usize::from(self.n_symbols);
        let mut new_code = [0u8; 256];
        let mut rsum = [0u8; 8];
        let byte_lim = (self.n_symbols as u8).wrapping_sub(self.len_histo[0] as u8);
        rsum[0] = byte_lim;
        rsum[1] = 0;
        for i in 1..7 {
            rsum[i + 1] = rsum[i].wrapping_add(self.len_histo[i] as u8);
        }
        let mut suffix_lim = u16::from(rsum[1]);
        self.symbols[0] = self.symbols[CODE_BASE];
        let mut j = u32::from(rsum[2]);
        for i in 0..n {
            let mut s1 = self.symbols[CODE_BASE + i];
            let length = s1.length();
            let mut opt = if length == 2 { n } else { 0 };
            if opt > 0 {
                let first2 = s1.first2();
                let mut k = 0;
                while k < opt {
                    let s2 = self.symbols[CODE_BASE + k];
                    if k != i && s2.length() > 1 && first2 == s2.first2() {
                        opt = 0;
                    }
                    k += 1;
                }
                new_code[i] = if opt > 0 {
                    let code = suffix_lim as u8;
                    suffix_lim += 1;
                    code
                } else {
                    j -= 1;
                    j as u8
                };
            } else {
                new_code[i] = rsum[length - 1];
                rsum[length - 1] = rsum[length - 1].wrapping_add(1);
            }
            s1.set_code_len(u64::from(new_code[i]), length as u64);
            self.symbols[usize::from(new_code[i])] = s1;
        }
        self.suffix_lim = suffix_lim;
        for code in self.byte_codes.iter_mut() {
            *code = if usize::from(*code) & (CODE_MAX - 1) >= CODE_BASE {
                u16::from(new_code[usize::from(*code as u8)]) + (1 << LEN_BITS)
            } else {
                511 + (1 << LEN_BITS)
            };
        }
        for i in 0..65536 {
            let code = self.short_codes[i];
            self.short_codes[i] = if usize::from(code) & (CODE_MAX - 1) >= CODE_BASE {
                u16::from(new_code[usize::from(code as u8)]) + (code & (15 << LEN_BITS))
            } else {
                self.byte_codes[i & 0xFF]
            };
        }
        for i in 0..HASH_TAB_SIZE {
            if self.hash_tab[i].icl < ICL_FREE {
                self.hash_tab[i] = self.symbols[usize::from(new_code[self.hash_tab[i].code() as u8 as usize])];
            }
        }
    }

    /// `duckdb_fsst_create` of strings that aren't zero terminated; `None` where they are all empty.
    pub fn train(strings: &[&[u8]]) -> Option<SymbolTable> {
        if strings.iter().all(|string| string.is_empty()) {
            return None;
        }
        let sample = make_sample(strings);
        Some(build_symbol_table(&sample))
    }

    /// Replaces what `out` holds with `input`, compressed (`compressBulk` of one line).
    pub fn compress(&self, input: &[u8], out: &mut Vec<u8>) {
        out.clear();
        let byte_lim = (self.n_symbols as u8).wrapping_sub(self.len_histo[0] as u8);
        let main_end = input.len().saturating_sub(8);
        let stop = self.compress_variant(input, 0, main_end, byte_lim, out);
        // what the main pass left, at most 8 bytes, with the terminator after it
        let mut buffer = [0u8; 8 + 1 + 8];
        let tail = input.len() - stop;
        buffer[..tail].copy_from_slice(&input[stop..]);
        buffer[tail] = self.terminator as u8;
        self.compress_variant(&buffer, 0, tail, byte_lim, out);
    }

    /// `compressVariant` without the suffix or branch options, which give the same codes: from `cur` to `end`
    /// of `data`, which has 8 bytes past each position it compresses; where it stopped.
    fn compress_variant(&self, data: &[u8], mut cur: usize, end: usize, byte_lim: u8, out: &mut Vec<u8>) -> usize {
        while cur < end {
            let mut word = load(data, cur);
            let code = self.short_codes[(word & 0xFFFF) as usize];
            let position = word & 0xFF_FFFF;
            let symbol = self.hash_tab[fsst_hash(position) as usize & (HASH_TAB_SIZE - 1)];
            let escaped = word as u8;
            word &= mask(symbol.ignored());
            if symbol.icl < ICL_FREE && symbol.num == word {
                out.push(symbol.code() as u8);
                cur += symbol.length();
            } else if (code as u8) < byte_lim {
                out.push(code as u8);
                cur += 2;
            } else {
                out.push(code as u8);
                if usize::from(code) & CODE_BASE != 0 {
                    out.push(escaped);
                }
                cur += 1;
            }
        }
        cur
    }

    /// `duckdb_fsst_export`.
    pub fn export(&self) -> Vec<u8> {
        let version = (VERSION << 32)
            | (u64::from(self.suffix_lim) << 24)
            | (u64::from(self.terminator) << 16)
            | (u64::from(self.n_symbols) << 8)
            | 1;
        let mut out = version.to_le_bytes().to_vec();
        out.push(0);
        out.extend(self.len_histo[..8].iter().map(|count| *count as u8));
        for symbol in &self.symbols[..usize::from(self.n_symbols)] {
            out.extend_from_slice(&symbol.num.to_le_bytes()[..symbol.length()]);
        }
        out
    }
}

/// `makeSample`: all the lines when they are small, else chunks of lines picked at random, to `SAMPLE_TARGET`.
fn make_sample<'a>(strings: &[&'a [u8]]) -> Vec<&'a [u8]> {
    let total: usize = strings.iter().map(|string| string.len()).sum();
    if total < SAMPLE_TARGET {
        return strings.to_vec();
    }
    let lines = strings.len() as u64;
    let mut sample = Vec::new();
    let mut size = 0;
    let mut random = fsst_hash(4637947);
    while size < SAMPLE_TARGET {
        random = fsst_hash(random);
        let mut line = (random % lines) as usize;
        while strings[line].is_empty() {
            line += 1;
            if line == strings.len() {
                line = 0;
            }
        }
        let length = strings[line].len();
        let chunks = 1 + (length - 1) / SAMPLE_LINE;
        random = fsst_hash(random);
        let chunk = SAMPLE_LINE * (random % chunks as u64) as usize;
        let taken = (length - chunk).min(SAMPLE_LINE);
        sample.push(&strings[line][chunk..chunk + taken]);
        size += taken;
    }
    sample
}

/// `Counters` of a 64-bit build: 16-bit counts of single codes and 12-bit counts of pairs, each split into a high
/// and a low part, all in one block of memory as C++ has them, for the reads past an array that skip zeros.
struct Counters {
    memory: Vec<u8>,
}

const COUNT1_HIGH: usize = 0;
const COUNT1_LOW: usize = CODE_MAX;
const COUNT2_HIGH: usize = 2 * CODE_MAX;
const COUNT2_LOW: usize = COUNT2_HIGH + CODE_MAX * CODE_MAX / 2;
const COUNTERS_SIZE: usize = COUNT2_LOW + CODE_MAX * CODE_MAX;

impl Counters {
    fn new() -> Self {
        Counters {
            // the reads of 8 bytes can go past the end
            memory: vec![0; COUNTERS_SIZE + 8],
        }
    }

    fn reset(&mut self) {
        self.memory.fill(0);
    }

    fn count1_set(&mut self, pos1: usize, value: u16) {
        self.memory[COUNT1_LOW + pos1] = value as u8;
        self.memory[COUNT1_HIGH + pos1] = (value >> 8) as u8;
    }

    fn count1_inc(&mut self, pos1: usize) {
        let low = &mut self.memory[COUNT1_LOW + pos1];
        let was = *low;
        *low = was.wrapping_add(1);
        if was == 0 {
            let high = &mut self.memory[COUNT1_HIGH + pos1];
            *high = high.wrapping_add(1);
        }
    }

    fn count2_inc(&mut self, pos1: usize, pos2: usize) {
        let low = &mut self.memory[COUNT2_LOW + pos1 * CODE_MAX + pos2];
        let was = *low;
        *low = was.wrapping_add(1);
        if was == 0 {
            let high = &mut self.memory[COUNT2_HIGH + pos1 * (CODE_MAX / 2) + (pos2 >> 1)];
            *high = high.wrapping_add(1 << ((pos2 & 1) << 2));
        }
    }

    fn count1_get_next(&self, pos1: &mut usize) -> u32 {
        let mut high = load(&self.memory, COUNT1_HIGH + *pos1);
        let zero = if high != 0 {
            (high.trailing_zeros() >> 3) as usize
        } else {
            7
        };
        high = (high >> (zero << 3)) & 255;
        *pos1 += zero;
        if *pos1 >= CODE_MAX || high == 0 {
            return 0;
        }
        let low = u32::from(self.memory[COUNT1_LOW + *pos1]);
        if low != 0 {
            high -= 1;
        }
        ((high as u32) << 8) + low
    }

    fn count2_get_next(&self, pos1: usize, pos2: &mut usize) -> u32 {
        let mut high = load(&self.memory, COUNT2_HIGH + pos1 * (CODE_MAX / 2) + (*pos2 >> 1));
        high >>= (*pos2 & 1) << 2;
        let zero = if high != 0 {
            (high.trailing_zeros() >> 2) as usize
        } else {
            15 - (*pos2 & 1)
        };
        high = (high >> (zero << 2)) & 15;
        *pos2 += zero;
        if *pos2 >= CODE_MAX || high == 0 {
            return 0;
        }
        let low = u32::from(self.memory[COUNT2_LOW + pos1 * CODE_MAX + *pos2]);
        if low != 0 {
            high -= 1;
        }
        ((high as u32) << 8) + low
    }

    fn backup1(&self) -> Vec<u8> {
        self.memory[..2 * CODE_MAX].to_vec()
    }

    fn restore1(&mut self, backup: &[u8]) {
        self.memory[..2 * CODE_MAX].copy_from_slice(backup);
    }
}

fn is_escape_code(code: usize) -> bool {
    code < CODE_BASE
}

/// `compressCount`: the gain of compressing the sample with `st`, counting codes and pairs of codes.
fn compress_count(st: &SymbolTable, counters: &mut Counters, lines: &[&[u8]], sample_frac: usize) -> i64 {
    let rnd128 = |i: usize| 1 + (fsst_hash(((i + 1) * sample_frac) as u64) & 127) as usize;
    let mut gain: i32 = 0;
    for (i, line) in lines.iter().enumerate() {
        if sample_frac < 128 && rnd128(i) > sample_frac {
            continue;
        }
        let end = line.len();
        if end == 0 {
            continue;
        }
        let mut cur = 0;
        let mut start = 0;
        let mut code1 = st.find_longest_symbol(line);
        cur += st.symbols[code1].length();
        gain += st.symbols[code1].length() as i32 - (1 + i32::from(is_escape_code(code1)));
        loop {
            counters.count1_inc(code1);
            if st.symbols[code1].length() != 1 {
                counters.count1_inc(usize::from(line[start]));
            }
            if cur == end {
                break;
            }
            start = cur;
            let code2;
            if cur + 7 < end {
                let mut word = load(line, cur);
                let code = word & 0xFF_FFFF;
                let symbol = st.hash_tab[fsst_hash(code) as usize & (HASH_TAB_SIZE - 1)];
                let short = (u64::from(st.short_codes[(word & 0xFFFF) as usize]) & CODE_MASK) as usize;
                word &= mask(symbol.ignored());
                if symbol.icl < ICL_FREE && symbol.num == word {
                    code2 = symbol.code() as usize;
                    cur += symbol.length();
                } else if short >= CODE_BASE {
                    code2 = short;
                    cur += 2;
                } else {
                    code2 = (u64::from(st.byte_codes[(word & 0xFF) as usize]) & CODE_MASK) as usize;
                    cur += 1;
                }
            } else {
                code2 = st.find_longest_symbol(&line[cur..]);
                cur += st.symbols[code2].length();
            }
            gain += (cur - start) as i32 - (1 + i32::from(is_escape_code(code2)));
            if sample_frac < 128 {
                counters.count2_inc(code1, code2);
                if cur - start > 1 {
                    counters.count2_inc(code1, usize::from(line[start]));
                }
            }
            code1 = code2;
        }
    }
    i64::from(gain)
}

/// `makeTable`: the symbols with the most gain, from what was counted, become `st`.
fn make_table(st: &mut SymbolTable, counters: &mut Counters, sample_frac: usize) {
    // by bytes and length (`QSymbol::operator==`): the symbol and its gain
    let mut candidates: HashMap<(u64, usize), (Symbol, u32)> = HashMap::new();
    let terminator = if st.n_symbols > 0 {
        CODE_BASE
    } else {
        usize::from(st.terminator)
    };
    counters.count1_set(terminator, 65535);
    let mut add_or_inc = |symbol: Symbol, count: u64| {
        if count < (5 * sample_frac as u64) / 128 {
            return;
        }
        let mut gain = (count * symbol.length() as u64) as u32;
        let key = (symbol.num, symbol.length());
        if let Some((_, before)) = candidates.get(&key) {
            gain = gain.wrapping_add(*before);
        }
        candidates.insert(key, (symbol, gain));
    };
    let limit = CODE_BASE + usize::from(st.n_symbols);
    let terminator_char = i32::from(st.terminator);
    let mut pos1 = 0;
    while pos1 < limit {
        let count1 = counters.count1_get_next(&mut pos1);
        if count1 != 0 {
            let s1 = st.symbols[pos1];
            let weight = if s1.length() == 1 { 8 } else { 1 };
            add_or_inc(s1, weight * u64::from(count1));
            if !(sample_frac >= 128 || s1.length() == 8 || s1.first_char() == terminator_char) {
                let mut pos2 = 0;
                while pos2 < limit {
                    let count2 = counters.count2_get_next(pos1, &mut pos2);
                    if count2 != 0 {
                        let s2 = st.symbols[pos2];
                        let s3 = s1.concat(s2);
                        if s2.first_char() != terminator_char {
                            add_or_inc(s3, u64::from(count2));
                        }
                    }
                    pos2 += 1;
                }
            }
        }
        pos1 += 1;
    }
    // the priority queue: the most gain first, and of equal gain the lowest bytes
    let mut ranked: Vec<(Symbol, u32)> = candidates.into_values().collect();
    ranked.sort_by(|(a, gain_a), (b, gain_b)| gain_b.cmp(gain_a).then(a.num.cmp(&b.num)));
    st.clear();
    for (symbol, _) in ranked {
        if st.n_symbols >= 255 {
            break;
        }
        st.add(symbol);
    }
}

/// `buildSymbolTable`.
fn build_symbol_table(lines: &[&[u8]]) -> SymbolTable {
    let mut st = SymbolTable::new();
    let mut best_table = SymbolTable::new();
    let mut best_gain = -(SAMPLE_MAX_SIZE as i64);
    // the terminator: the lowest of the bytes that occur least
    let mut histogram = [0u16; 256];
    for line in lines {
        for byte in line.iter() {
            histogram[usize::from(*byte)] = histogram[usize::from(*byte)].wrapping_add(1);
        }
    }
    let mut min_size = SAMPLE_MAX_SIZE as u32;
    st.terminator = 256;
    for i in (0..256).rev() {
        if u32::from(histogram[i]) > min_size {
            continue;
        }
        st.terminator = i as u16;
        min_size = u32::from(histogram[i]);
    }
    let mut counters = Counters::new();
    let mut best_counters = counters.backup1();
    let mut sample_frac = 8;
    loop {
        counters.reset();
        let gain = compress_count(&st, &mut counters, lines, sample_frac);
        if gain >= best_gain {
            best_counters = counters.backup1();
            best_table = st.clone();
            best_gain = gain;
        }
        if sample_frac >= 128 {
            break;
        }
        make_table(&mut st, &mut counters, sample_frac);
        sample_frac += 30;
    }
    counters.restore1(&best_counters);
    make_table(&mut best_table, &mut counters, sample_frac);
    best_table.finalize();
    best_table
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
    fn codes_go_by_length() {
        let strings: [&[u8]; 3] = [b"a", b"abcdefgh", b"abcdefghabcdefgh"];
        let repeated: Vec<&[u8]> = strings.iter().copied().cycle().take(600).collect();
        let table = SymbolTable::train(&repeated).unwrap();
        let lengths: Vec<usize> = table.symbols[..usize::from(table.n_symbols)]
            .iter()
            .map(|s| s.length())
            .collect();
        // `duckdb_fsst_import` reads them as 2 to 8 bytes long, and then 1
        assert!(lengths.is_sorted_by_key(|length| if *length == 1 { 9 } else { *length }));
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
