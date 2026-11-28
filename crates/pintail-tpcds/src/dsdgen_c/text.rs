// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/text.cpp @ rust-port-baseline

//! Random English-like text, made of sentences drawn from the grammar distributions.

use super::Ctx;
use super::constants::DIST_UNIFORM;

/// A buffer `gen_text` reuses for its sentences.
#[derive(Default)]
pub(crate) struct State {
    sentence: Vec<u8>,
}

impl Ctx {
    /// `mk_sentence`
    fn mk_sentence(&mut self, verbiage: &mut Vec<u8>, stream: i32) {
        verbiage.clear();
        let (syntax, _) = self.pick_distribution_str("sentences", 1, 1, stream);
        for &c in syntax {
            let dist = match c {
                b'N' => "nouns",
                b'V' => "verbs",
                b'J' => "adjectives",
                b'D' => "adverbs",
                b'X' => "auxiliaries",
                b'P' => "prepositions",
                b'A' => "articles",
                b'T' => "terminators",
                _ => {
                    verbiage.push(c);
                    continue;
                }
            };
            let (word, _) = self.pick_distribution_str(dist, 1, 1, stream);
            verbiage.extend_from_slice(word);
        }
    }

    /// `gen_text`: `min..=max` characters of sentences.
    pub(crate) fn gen_text(&mut self, dest: &mut Vec<u8>, min: i32, max: i32, stream: i32) {
        let mut target_len = self.genrand_integer(DIST_UNIFORM, min, max, 0, stream);
        let mut capitalize = true;
        let mut sentence = std::mem::take(&mut self.text.sentence);
        dest.clear();
        while target_len > 0 {
            self.mk_sentence(&mut sentence, stream);
            if capitalize {
                sentence[0] = sentence[0].to_ascii_uppercase();
            }
            let generated_length = sentence.len() as i32;
            capitalize = sentence[sentence.len() - 1] == b'.';
            if target_len <= generated_length {
                sentence.truncate(target_len as usize);
            }
            dest.extend_from_slice(&sentence);
            target_len -= generated_length;
            if target_len > 0 {
                dest.push(b' ');
                target_len -= 1;
            }
        }
        self.text.sentence = sentence;
    }
}
