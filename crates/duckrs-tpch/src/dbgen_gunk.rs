// Ported from duckdb/extension/tpch/dbgen/dbgen_gunk.cpp @ rust-port-baseline
//
// Copyright owned by the Transaction Processing Performance Council; see LICENSE in this crate.
//
// The C++ keeps the distributions in globals that `load_dists` fills and `cleanup_dists` frees;
// here they are one immutable value, loaded once and shared by every generator. The text pool that
// `load_dists` also sets up is `text::TextPool`.

use crate::bm_utils::{Distribution, read_dist};
use std::sync::OnceLock;

/// The distributions of dists.dss that dbgen reads (the C++ also loads `category` and `articles`,
/// which nothing reads).
#[derive(Debug)]
pub struct Dists {
    pub p_cntr_set: Distribution,
    pub colors: Distribution,
    pub p_types_set: Distribution,
    pub nations: Distribution,
    pub regions: Distribution,
    pub o_priority_set: Distribution,
    pub l_instruct_set: Distribution,
    pub l_smode_set: Distribution,
    pub l_rflag_set: Distribution,
    pub c_mseg_set: Distribution,
    // the distributions that contain text generation
    pub nouns: Distribution,
    pub verbs: Distribution,
    pub adjectives: Distribution,
    pub adverbs: Distribution,
    pub auxillaries: Distribution,
    pub terminators: Distribution,
    pub prepositions: Distribution,
    pub grammar: Distribution,
    pub np: Distribution,
    pub vp: Distribution,
}

/// `load_dists` (without the text pool).
pub fn load_dists() -> &'static Dists {
    static DISTS: OnceLock<Dists> = OnceLock::new();
    DISTS.get_or_init(|| Dists {
        p_cntr_set: read_dist("p_cntr"),
        colors: read_dist("colors"),
        p_types_set: read_dist("p_types"),
        nations: read_dist("nations"),
        regions: read_dist("regions"),
        o_priority_set: read_dist("o_oprio"),
        l_instruct_set: read_dist("instruct"),
        l_smode_set: read_dist("smode"),
        l_rflag_set: read_dist("rflag"),
        c_mseg_set: read_dist("msegmnt"),
        nouns: read_dist("nouns"),
        verbs: read_dist("verbs"),
        adjectives: read_dist("adjectives"),
        adverbs: read_dist("adverbs"),
        auxillaries: read_dist("auxillaries"),
        terminators: read_dist("terminators"),
        prepositions: read_dist("prepositions"),
        grammar: read_dist("grammar"),
        np: read_dist("np"),
        vp: read_dist("vp"),
    })
}
