// Ported from duckdb/extension/tpch/dbgen/text.cpp @ rust-port-baseline
//
// Copyright owned by the Transaction Processing Performance Council; see LICENSE in this crate.
//
// The pseudo-text generator. Comments are random slices of a 300 MiB text pool, which is split into
// chunks. The C++ generates a chunk on first use; here all chunks are generated on all cores when
// the pool is made (`TextPool::generate_all`). Every chunk starts from a precomputed checkpoint (sentence offset +
// RNG state), so chunks are independent of each other. The C++ keeps the pool in one contiguous
// arena and makes sure both chunks a slice touches are generated; here each chunk also holds the
// bytes just past its end (`CHUNK_OVERLAP`), so any slice is contiguous within the chunk it starts
// in. The parse-based `txt_vp` / `txt_np` / `txt_sentence` and `dbg_text` are not ported: the C++
// generator uses only the `gen_*` routines and `dbg_text_source`.

use crate::bm_utils::Distribution;
use crate::dbgen_gunk::Dists;
use crate::dss::{DBGenContext, DssHuge, Seed, TEXT_POOL_SIZE, TEXT_SD};
use crate::rnd::dss_random;
use crate::text_pool_checkpoints::{
    TEXT_POOL_CHECKPOINTS, TEXT_POOL_CHUNK_COUNT, TEXT_POOL_CHUNK_SHIFT, TEXT_POOL_MAX_SENTENCE_LEN,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

const _: () = assert!(TEXT_POOL_SIZE == TEXT_POOL_CHUNK_COUNT << TEXT_POOL_CHUNK_SHIFT);

const CHUNK_SIZE: usize = 1 << TEXT_POOL_CHUNK_SHIFT;
/// Bytes past its end that a chunk also holds: more than the longest text slice (`PS_CMNT_LEN *
/// V_STR_HGH`, 198 bytes).
const CHUNK_OVERLAP: usize = 256;

/// The lookup tables weight -> word of the word distributions (`noun_index`, ...).
struct WordIndexes {
    nouns: Vec<&'static [u8]>,
    adjectives: Vec<&'static [u8]>,
    adverbs: Vec<&'static [u8]>,
    auxillaries: Vec<&'static [u8]>,
    verbs: Vec<&'static [u8]>,
    prepositions: Vec<&'static [u8]>,
}

/// `gen_index`: a lookup table for weight -> str
fn gen_index(s: &Distribution) -> Vec<&'static [u8]> {
    (0..=s.total_weight())
        .map(|w| {
            let mut i = 0;
            while s.list[i].weight < w {
                i += 1;
            }
            s.list[i].text.as_bytes()
        })
        .collect()
}

/// A sentence being generated: the bytes and the write position (`dest` of the C++).
struct Sentence {
    buf: [u8; TEXT_POOL_MAX_SENTENCE_LEN + 8],
    dest: usize,
}

impl Sentence {
    fn write_word(&mut self, word: &[u8]) {
        self.buf[self.dest..self.dest + word.len()].copy_from_slice(word);
        self.buf[self.dest + word.len()] = b' ';
        self.dest += word.len() + 1;
    }
}

struct SentenceGenerator<'a> {
    dists: &'a Dists,
    index: &'a WordIndexes,
}

impl SentenceGenerator<'_> {
    /// `gen_text`
    fn gen_text(&self, out: &mut Sentence, seed: &mut Seed, s: &Distribution) {
        let j = dss_random(1, s.total_weight(), seed);
        let mut i = 0;
        while s.list[i].weight < j {
            i += 1;
        }
        out.write_word(s.list[i].text.as_bytes());
    }

    /// `gen_text_index`
    fn gen_text_index(out: &mut Sentence, seed: &mut Seed, index: &[&[u8]], s: &Distribution) {
        let j = dss_random(1, s.total_weight(), seed);
        out.write_word(index[j as usize]);
    }

    /// The form of a phrase: how many of the first `forms` cumulative weights a draw exceeds.
    fn pick_form(s: &Distribution, forms: usize, seed: &mut Seed) -> usize {
        let j = dss_random(1, s.total_weight(), seed);
        s.list[..forms].iter().filter(|m| m.weight < j).count()
    }

    fn verb(&self, out: &mut Sentence, seed: &mut Seed) {
        Self::gen_text_index(out, seed, &self.index.verbs, &self.dists.verbs);
    }

    fn auxillary(&self, out: &mut Sentence, seed: &mut Seed) {
        Self::gen_text_index(out, seed, &self.index.auxillaries, &self.dists.auxillaries);
    }

    fn adverb(&self, out: &mut Sentence, seed: &mut Seed) {
        Self::gen_text_index(out, seed, &self.index.adverbs, &self.dists.adverbs);
    }

    fn adjective(&self, out: &mut Sentence, seed: &mut Seed) {
        Self::gen_text_index(out, seed, &self.index.adjectives, &self.dists.adjectives);
    }

    fn noun(&self, out: &mut Sentence, seed: &mut Seed) {
        Self::gen_text_index(out, seed, &self.index.nouns, &self.dists.nouns);
    }

    /// `gen_vp`
    fn gen_vp(&self, out: &mut Sentence, seed: &mut Seed) {
        match Self::pick_form(&self.dists.vp, 3, seed) {
            0 => self.verb(out, seed),
            1 => {
                self.auxillary(out, seed);
                self.verb(out, seed);
            }
            2 => {
                self.verb(out, seed);
                self.adverb(out, seed);
            }
            _ => {
                self.auxillary(out, seed);
                self.verb(out, seed);
                self.adverb(out, seed);
            }
        }
    }

    /// `gen_np`
    fn gen_np(&self, out: &mut Sentence, seed: &mut Seed) {
        match Self::pick_form(&self.dists.np, 3, seed) {
            0 => self.noun(out, seed),
            1 => {
                self.adjective(out, seed);
                self.noun(out, seed);
            }
            2 => {
                self.adjective(out, seed);
                out.buf[out.dest - 1] = b',';
                out.buf[out.dest] = b' ';
                out.dest += 1;
                self.adjective(out, seed);
                self.noun(out, seed);
            }
            _ => {
                self.adverb(out, seed);
                self.adjective(out, seed);
                self.noun(out, seed);
            }
        }
    }

    /// `gen_preposition`
    fn gen_preposition(&self, out: &mut Sentence, seed: &mut Seed) {
        Self::gen_text_index(out, seed, &self.index.prepositions, &self.dists.prepositions);
        out.buf[out.dest..out.dest + 4].copy_from_slice(b"the ");
        out.dest += 4;
        self.gen_np(out, seed);
    }

    /// `gen_terminator`: terminators abut the previous word
    fn gen_terminator(&self, out: &mut Sentence, seed: &mut Seed) {
        out.dest -= 1;
        self.gen_text(out, seed, &self.dists.terminators);
        out.dest -= 1;
    }

    /// `gen_sentence`: writes one sentence and the space after it to `out`, from its start.
    fn gen_sentence(&self, out: &mut Sentence, seed: &mut Seed) {
        out.dest = 0;
        match Self::pick_form(&self.dists.grammar, 4, seed) {
            0 => {
                self.gen_np(out, seed);
                self.gen_vp(out, seed);
            }
            1 => {
                self.gen_np(out, seed);
                self.gen_vp(out, seed);
                self.gen_preposition(out, seed);
            }
            2 => {
                self.gen_np(out, seed);
                self.gen_vp(out, seed);
                self.gen_np(out, seed);
            }
            3 => {
                self.gen_np(out, seed);
                self.gen_preposition(out, seed);
                self.gen_vp(out, seed);
                self.gen_np(out, seed);
            }
            _ => {
                self.gen_np(out, seed);
                self.gen_preposition(out, seed);
                self.gen_vp(out, seed);
                self.gen_preposition(out, seed);
            }
        }
        self.gen_terminator(out, seed);
        out.buf[out.dest] = b' ';
        out.dest += 1;
    }
}

/// The text pool (`text_pool` and its chunk states). Chunks are generated on first use, by
/// whichever generator reaches them first.
pub struct TextPool {
    dists: &'static Dists,
    index: WordIndexes,
    chunks: Box<[OnceLock<Box<[u8]>>]>,
}

impl TextPool {
    /// `init_text_pool`: the pool shared by the generators alive at the same time, or a new one.
    pub fn get(dists: &'static Dists, ctx: &DBGenContext) -> Arc<TextPool> {
        assert!(
            DssHuge::from(TEXT_POOL_CHECKPOINTS[0].seed) == ctx.seed[TEXT_SD].value
                && TEXT_POOL_CHECKPOINTS[0].offset == 0,
            "TPC-H text pool checkpoints do not match the text seed"
        );
        static SHARED: Mutex<Weak<TextPool>> = Mutex::new(Weak::new());
        let mut shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pool) = shared.upgrade() {
            return pool;
        }
        let pool = Arc::new(TextPool {
            dists,
            index: WordIndexes {
                nouns: gen_index(&dists.nouns),
                adjectives: gen_index(&dists.adjectives),
                adverbs: gen_index(&dists.adverbs),
                auxillaries: gen_index(&dists.auxillaries),
                verbs: gen_index(&dists.verbs),
                prepositions: gen_index(&dists.prepositions),
            },
            chunks: (0..TEXT_POOL_CHUNK_COUNT).map(|_| OnceLock::new()).collect(),
        });
        pool.generate_all();
        *shared = Arc::downgrade(&pool);
        pool
    }

    /// Generates every chunk, on all cores. The C++ generates each chunk when a slice first reaches
    /// it, by the thread that needs it; but from SF 0.01 up nearly every chunk is read, and the
    /// generator here runs on one thread.
    fn generate_all(&self) {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    loop {
                        let chunk = next.fetch_add(1, Ordering::Relaxed);
                        if chunk >= TEXT_POOL_CHUNK_COUNT {
                            break;
                        }
                        self.chunk(chunk);
                    }
                });
            }
        });
    }

    /// `gen_text_pool_chunk`: the bytes `[chunk_start, chunk_end + CHUNK_OVERLAP)` of the pool.
    fn gen_text_pool_chunk(&self, chunk: usize) -> Box<[u8]> {
        let start_checkpoint = &TEXT_POOL_CHECKPOINTS[chunk];
        let end_checkpoint = &TEXT_POOL_CHECKPOINTS[chunk + 1];
        let chunk_start = chunk << TEXT_POOL_CHUNK_SHIFT;
        let chunk_end = chunk_start + CHUNK_SIZE;
        let fill_end = (chunk_end + CHUNK_OVERLAP).min(TEXT_POOL_SIZE);
        let mut result = vec![0u8; fill_end - chunk_start].into_boxed_slice();

        let generator = SentenceGenerator {
            dists: self.dists,
            index: &self.index,
        };
        let mut seed = Seed {
            table: 0,
            value: DssHuge::from(start_checkpoint.seed),
            usage: 0,
            boundary: 0,
        };
        let mut sentence = Sentence {
            buf: [0; TEXT_POOL_MAX_SENTENCE_LEN + 8],
            dest: 0,
        };
        let mut pos = start_checkpoint.offset as usize;
        let mut sentence_start = pos;
        let mut sentence_seed = seed.value;
        while pos < fill_end {
            if pos < chunk_end {
                // the sentence covering the chunk end must match the next checkpoint
                sentence_start = pos;
                sentence_seed = seed.value;
            }
            generator.gen_sentence(&mut sentence, &mut seed);
            let length = sentence.dest;
            debug_assert!(length <= TEXT_POOL_MAX_SENTENCE_LEN);
            // copy the part of the sentence that falls inside this chunk
            let copy_begin = chunk_start.saturating_sub(pos);
            let copy_end = length.min(fill_end - pos);
            if copy_begin < copy_end {
                result[pos + copy_begin - chunk_start..pos + copy_end - chunk_start]
                    .copy_from_slice(&sentence.buf[copy_begin..copy_end]);
            }
            let previous = pos;
            pos += length;
            if previous < chunk_end && pos == chunk_end {
                // the next chunk starts exactly on a sentence boundary
                sentence_start = pos;
                sentence_seed = seed.value;
            }
        }
        assert!(
            end_checkpoint.offset as usize == sentence_start && DssHuge::from(end_checkpoint.seed) == sentence_seed,
            "TPC-H text pool checkpoint {} does not match the generated text",
            chunk + 1
        );
        result
    }

    fn chunk(&self, chunk: usize) -> &[u8] {
        self.chunks[chunk].get_or_init(|| self.gen_text_pool_chunk(chunk))
    }

    /// `dbg_text_source`: a random slice of the text pool.
    #[inline]
    pub fn dbg_text_source(&self, min: i32, max: i32, seed: &mut Seed) -> &[u8] {
        let hg_offset = dss_random(0, TEXT_POOL_SIZE as DssHuge - DssHuge::from(max), seed) as usize;
        let hg_length = dss_random(DssHuge::from(min), DssHuge::from(max), seed) as usize;
        debug_assert!(hg_length > 0 && hg_length < CHUNK_OVERLAP);
        let chunk = hg_offset >> TEXT_POOL_CHUNK_SHIFT;
        let local = hg_offset - (chunk << TEXT_POOL_CHUNK_SHIFT);
        &self.chunk(chunk)[local..local + hg_length]
    }
}
