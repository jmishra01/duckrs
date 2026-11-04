// Ported from duckdb/src/common/string_util.cpp (SimilarityRating, TopNStrings,
// CandidatesMessage) and duckdb/third_party/jaro_winkler @ rust-port-baseline
//
//! String helpers for error messages: ranking the names a misspelled one may have meant.

/// The Jaro similarity of two strings, in [0, 1].
fn jaro_similarity(a: &[u8], b: &[u8]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut a_matched = vec![false; a.len()];
    let mut b_matched = vec![false; b.len()];
    let mut matches = 0usize;
    for (i, &c) in a.iter().enumerate() {
        let start = i.saturating_sub(window);
        let end = (i + window + 1).min(b.len());
        for j in start..end {
            if !b_matched[j] && b[j] == c {
                a_matched[i] = true;
                b_matched[j] = true;
                matches += 1;
                break;
            }
        }
    }
    if matches == 0 {
        return 0.0;
    }
    let a_matches = a.iter().zip(&a_matched).filter(|(_, m)| **m).map(|(c, _)| c);
    let b_matches = b.iter().zip(&b_matched).filter(|(_, m)| **m).map(|(c, _)| c);
    let transpositions = a_matches.zip(b_matches).filter(|(x, y)| x != y).count() / 2;
    let m = matches as f64;
    (m / a.len() as f64 + m / b.len() as f64 + (m - transpositions as f64) / m) / 3.0
}

/// `StringUtil::SimilarityRating`: Jaro-Winkler similarity (prefix weight 0.1, applied above
/// a Jaro score of 0.7).
pub fn similarity_rating(a: &str, b: &str) -> f64 {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let max_prefix = a.len().min(b.len()).min(4);
    let prefix = a.iter().zip(b).take(max_prefix).take_while(|(x, y)| x == y).count();
    let mut sim = jaro_similarity(a, b);
    if sim > 0.7 {
        sim += prefix as f64 * 0.1 * (1.0 - sim);
    }
    sim
}

/// `StringUtil::TopNStrings` (n = 5, threshold = 0.5): the best-scoring strings, best first,
/// ties broken case-insensitively. The best one is always included.
pub fn top_n_strings(scores: Vec<(String, f64)>) -> Vec<String> {
    top_n_strings_with(scores, 5, 0.5)
}

fn top_n_strings_with(mut scores: Vec<(String, f64)>, n: usize, threshold: f64) -> Vec<String> {
    if scores.is_empty() {
        return Vec::new();
    }
    scores.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    let mut result = vec![scores[0].0.clone()];
    for (name, score) in scores.into_iter().take(n).skip(1) {
        if score < threshold {
            break;
        }
        result.push(name);
    }
    result
}

/// `StringUtil::LevenshteinDistance` (case-insensitive), with a configurable substitution cost.
fn levenshtein_distance(a: &str, b: &str, not_equal_penalty: usize) -> usize {
    let (a, b) = (a.to_lowercase().into_bytes(), b.to_lowercase().into_bytes());
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut current = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let substitution = previous[j - 1] + if a[i - 1] == b[j - 1] { 0 } else { not_equal_penalty };
            current[j] = (previous[j] + 1).min(current[j - 1] + 1).min(substitution);
        }
        previous = current;
    }
    previous[b.len()]
}

/// `StringUtil::TopNLevenshtein` (n = 5, threshold = 5): the strings closest to `target`, where
/// a string longer than the target is compared by its prefix of the target's length.
pub fn top_n_levenshtein(strings: &[String], target: &str) -> Vec<String> {
    const THRESHOLD: usize = 5;
    let scores: Vec<(String, usize)> = strings
        .iter()
        .map(|s| {
            let compared = if target.len() < s.len() {
                s.get(..target.len()).unwrap_or(s)
            } else {
                s
            };
            (s.clone(), levenshtein_distance(compared, target, 3))
        })
        .collect();
    // normalize the distances to [0, 1] ratings
    let max_score = scores.iter().map(|(_, d)| *d).max().unwrap_or(0).max(THRESHOLD);
    let normalize = |d: usize| 1.0 - d as f64 / max_score as f64;
    let rated = scores.into_iter().map(|(s, d)| (s, normalize(d))).collect();
    top_n_strings_with(rated, 5, normalize(THRESHOLD))
}

/// `StringUtil::CandidatesErrorMessage`: the closest strings by Levenshtein distance.
pub fn candidates_error_message(strings: &[String], target: &str, label: &str) -> String {
    candidates_message(&top_n_levenshtein(strings, target), label)
}

/// `StringUtil::CandidatesMessage`: `\n<label>: "a", "b"`, or nothing without candidates.
pub fn candidates_message(candidates: &[String], label: &str) -> String {
    if candidates.is_empty() {
        return String::new();
    }
    let quoted: Vec<String> = candidates.iter().map(|c| format!("\"{c}\"")).collect();
    format!("\n{label}: {}", quoted.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jaro_winkler_matches_the_reference_values() {
        // classic examples
        assert!((similarity_rating("MARTHA", "MARHTA") - 0.9611).abs() < 1e-3);
        assert!((similarity_rating("DIXON", "DICKSONX") - 0.8133).abs() < 1e-3);
        assert_eq!(similarity_rating("abc", "abc"), 1.0);
        assert_eq!(similarity_rating("abc", "xyz"), 0.0);
    }

    #[test]
    fn levenshtein_suggestions() {
        let names: Vec<String> = ["threads", "temp_directory", "default_order"]
            .map(String::from)
            .to_vec();
        assert_eq!(top_n_levenshtein(&names, "thread")[0], "threads");
        assert_eq!(levenshtein_distance("abc", "abd", 3), 2);
    }

    #[test]
    fn top_n() {
        let scores = vec![("b".to_string(), 0.4), ("a".to_string(), 0.9), ("c".to_string(), 0.6)];
        assert_eq!(top_n_strings(scores), vec!["a", "c"]);
        assert_eq!(
            candidates_message(&["x".into(), "y".into()], "Candidate bindings"),
            "\nCandidate bindings: \"x\", \"y\""
        );
    }
}
