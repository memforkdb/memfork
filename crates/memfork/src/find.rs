//! Finding memory by text (DESIGN §6.6).
//!
//! Agents cannot produce embeddings, so vector search is out of their reach.
//! This is plain ranked text search over keys and values: no model, no index,
//! no dependency, and the same ranking on every machine.
//!
//! **Integer arithmetic only.** A score is built from counts and a rarity
//! weight computed with integer bit arithmetic, never a floating-point
//! logarithm: two maths libraries may round a logarithm differently, and then
//! two operating systems would rank the same store differently.
//!
//! * Words are lowercased runs of letters and digits, in any script.
//! * Each query word is weighted by rarity: `1 + floor(log2(N / df))`, where
//!   `N` is how many entries were searched and `df` how many contain it.
//! * An entry earns, for each query word, its weight times four if the word
//!   is in the key, plus its weight times the occurrences in the value, up to
//!   three. A query word of three or more letters that only begins a word
//!   earns half, rounded down. Scores are kept doubled to stay whole.
//! * A query of two or more words appearing verbatim, case aside, in the key
//!   or the value adds eight times the largest weight.
//! * Highest score first; equal scores in key order.
//!
//! It is a scan: every entry under the prefix is read once per search. That is
//! milliseconds at ten thousand entries and a few hundred at a hundred
//! thousand; DESIGN §6.6 has the measurements.

use std::collections::{BTreeMap, BTreeSet};

/// Most results a search returns.
pub const MAX_RESULTS: usize = 50;

/// Results returned when the caller does not say.
pub const DEFAULT_RESULTS: usize = 10;

/// Characters of context in a result's snippet.
pub const SNIPPET_CHARS: usize = 200;

/// Lowercased runs of letters and digits.
pub fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            current.extend(c.to_lowercase());
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// `floor(log2(n))` for `n >= 1`, in integers.
fn log2_floor(n: usize) -> u64 {
    u64::from(usize::BITS - 1 - n.max(1).leading_zeros())
}

/// One document to score: its key and its text.
#[derive(Debug, Clone)]
pub struct Doc<'a> {
    /// The key.
    pub key: &'a str,
    /// The value, as text.
    pub text: &'a str,
}

/// A scored result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// The key.
    pub key: String,
    /// Its score, doubled so halves stay whole.
    pub score: u64,
    /// Where the value first matches, as a character offset.
    pub first_match: Option<usize>,
}

struct Prepared {
    key_words: BTreeSet<String>,
    value_words: BTreeMap<String, u64>,
    key_lower: String,
    text_lower: String,
}

fn prepare(doc: &Doc<'_>) -> Prepared {
    let mut value_words: BTreeMap<String, u64> = BTreeMap::new();
    for w in words(doc.text) {
        *value_words.entry(w).or_insert(0) += 1;
    }
    Prepared {
        key_words: words(doc.key).into_iter().collect(),
        value_words,
        key_lower: doc.key.to_lowercase(),
        text_lower: doc.text.to_lowercase(),
    }
}

/// How a query word matches a set of words: 2 for an exact match, 1 for a
/// prefix of three or more characters, 0 for none.
fn matches(term: &str, word: &str) -> u64 {
    if word == term {
        2
    } else if term.chars().count() >= 3 && word.starts_with(term) {
        1
    } else {
        0
    }
}

/// Rank `docs` against `query`. Only documents with a score above zero are
/// returned, best first, at most `limit`.
pub fn rank(docs: &[Doc<'_>], query: &str, limit: usize) -> Vec<Hit> {
    let terms: Vec<String> = {
        let mut seen = BTreeSet::new();
        words(query)
            .into_iter()
            .filter(|t| seen.insert(t.clone()))
            .collect()
    };
    let phrase = query.trim().to_lowercase();
    if terms.is_empty() && phrase.is_empty() {
        return Vec::new();
    }
    let prepared: Vec<Prepared> = docs.iter().map(prepare).collect();
    let n = prepared.len().max(1);

    // Rarity: in how many documents does each term match at all.
    let weights: Vec<u64> = terms
        .iter()
        .map(|t| {
            let df = prepared
                .iter()
                .filter(|p| {
                    p.key_words.iter().any(|w| matches(t, w) > 0)
                        || p.value_words.keys().any(|w| matches(t, w) > 0)
                })
                .count()
                .max(1);
            1 + log2_floor(n / df)
        })
        .collect();
    let top_weight = weights.iter().copied().max().unwrap_or(1);

    let mut hits: Vec<Hit> = docs
        .iter()
        .zip(&prepared)
        .filter_map(|(doc, p)| {
            let mut score = 0u64;
            for (t, w) in terms.iter().zip(&weights) {
                let in_key = p.key_words.iter().map(|k| matches(t, k)).max().unwrap_or(0);
                score += w * 4 * in_key;
                let in_value: u64 = p
                    .value_words
                    .iter()
                    .map(|(word, count)| matches(t, word) * count)
                    .sum();
                // Occurrences count up to three: a word repeated fifty times
                // is not fifty times as relevant.
                score += w * in_value.min(6);
            }
            if terms.len() > 1 && (p.key_lower.contains(&phrase) || p.text_lower.contains(&phrase))
            {
                score += 2 * 8 * top_weight;
            }
            (score > 0).then(|| Hit {
                key: doc.key.to_owned(),
                score,
                first_match: first_match(&p.text_lower, &phrase, &terms),
            })
        })
        .collect();
    hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.key.cmp(&b.key)));
    hits.truncate(limit);
    hits
}

/// Where the value first mentions the query, as a character offset.
fn first_match(text_lower: &str, phrase: &str, terms: &[String]) -> Option<usize> {
    let byte = std::iter::once(phrase)
        .chain(terms.iter().map(String::as_str))
        .filter(|t| !t.is_empty())
        .filter_map(|t| text_lower.find(t))
        .min()?;
    Some(text_lower[..byte].chars().count())
}

/// Up to [`SNIPPET_CHARS`] of `text` around character `at`, on one line,
/// with `…` where it was cut.
pub fn snippet(text: &str, at: Option<usize>) -> String {
    let flat: Vec<char> = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .collect();
    if flat.len() <= SNIPPET_CHARS {
        return flat.into_iter().collect();
    }
    // The offset was into the original text; after flattening whitespace it
    // can only have moved earlier, so it is still a fair place to centre on.
    let centre = at.unwrap_or(0).min(flat.len());
    let start = centre.saturating_sub(SNIPPET_CHARS / 4);
    let end = (start + SNIPPET_CHARS).min(flat.len());
    let start = end.saturating_sub(SNIPPET_CHARS);
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&flat[start..end]);
    if end < flat.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docs(pairs: &[(&'static str, &'static str)]) -> Vec<Doc<'static>> {
        pairs.iter().map(|(k, t)| Doc { key: k, text: t }).collect()
    }

    #[test]
    fn words_are_lowercased_letter_and_digit_runs_in_any_script() {
        assert_eq!(
            words("Auth lives in src/auth/login.rs!"),
            ["auth", "lives", "in", "src", "auth", "login", "rs"]
        );
        assert_eq!(words("Grüße, МИР 42"), ["grüße", "мир", "42"]);
    }

    #[test]
    fn integer_log2_is_exact() {
        assert_eq!(log2_floor(1), 0);
        assert_eq!(log2_floor(2), 1);
        assert_eq!(log2_floor(1023), 9);
        assert_eq!(log2_floor(1024), 10);
    }

    #[test]
    fn a_key_match_beats_a_passing_mention_and_rare_words_count_more() {
        let d = docs(&[
            ("shop:decision:auth", "use sessions, not tokens"),
            ("shop:note:1", "we discussed auth briefly"),
            ("shop:note:2", "the the the the"),
        ]);
        let hits = rank(&d, "auth", 10);
        assert_eq!(hits[0].key, "shop:decision:auth");
        assert_eq!(hits[1].key, "shop:note:1");
        assert_eq!(
            hits.len(),
            2,
            "an entry that does not match is not returned"
        );
    }

    #[test]
    fn prefixes_count_half_and_short_words_do_not_prefix() {
        let d = docs(&[("a", "authentication flow"), ("b", "auth flow")]);
        let hits = rank(&d, "auth", 10);
        assert_eq!(hits[0].key, "b");
        assert!(hits[1].score < hits[0].score);
        assert!(rank(&docs(&[("a", "authentication")]), "au", 10).is_empty());
    }

    #[test]
    fn the_whole_phrase_wins_and_ties_go_to_key_order() {
        let d = docs(&[
            ("z", "login is in src"),
            ("a", "src has the login"),
            ("m", "the login is in src"),
        ]);
        let hits = rank(&d, "login is in src", 10);
        let keys: Vec<&str> = hits.iter().map(|h| h.key.as_str()).collect();
        assert_eq!(keys[..2], ["m", "z"], "{hits:?}");
        let tie = rank(&docs(&[("b", "x"), ("a", "x")]), "x", 10);
        assert_eq!(tie[0].key, "a");
    }

    #[test]
    fn the_same_query_ranks_the_same_way_every_time() {
        let d = docs(&[
            ("k1", "alpha beta"),
            ("k2", "beta gamma"),
            ("k3", "alpha gamma beta"),
        ]);
        assert_eq!(rank(&d, "alpha beta", 10), rank(&d, "alpha beta", 10));
    }

    #[test]
    fn snippets_are_bounded_and_centred_on_the_match() {
        let long = format!("{} needle {}", "x ".repeat(300), "y ".repeat(300));
        let s = snippet(&long, long.find("needle"));
        assert!(s.contains("needle"), "{s}");
        assert!(s.chars().count() <= SNIPPET_CHARS + 2);
        assert!(s.starts_with('…') && s.ends_with('…'));
        assert_eq!(snippet("short\nline", None), "short line");
    }
}
