//! Exact brute-force cosine similarity search (DESIGN §4.2).
//!
//! There is no approximate index here on purpose: DESIGN §4.2 defers a
//! branch-aware approximate index, which is still to do. Brute force over a
//! branch head is exact,
//! needs no index to keep in step with forks and merges, and — because the
//! accumulation order is fixed and ties break on the key — returns byte-identical
//! ordering on every platform.

use std::sync::Arc;

use crate::entry::Entry;
use crate::store::Store;
use crate::view::ReadView;

/// One search result.
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// The matching key.
    pub key: String,
    /// Cosine similarity with the query, in `[-1, 1]`.
    pub score: f32,
    /// The matching entry.
    pub entry: Arc<Entry>,
}

/// Cosine similarity of two equal-length vectors.
///
/// Returns 0 for a length mismatch or a zero-magnitude vector, so an entry can
/// never knock a real match out of the top k by being undefined.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    // A fixed left-to-right accumulation: float addition is not associative,
    // so the order is part of the contract: the same query must rank the
    // same way on every machine, every time.
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 || !denom.is_finite() {
        0.0
    } else {
        dot / denom
    }
}

/// Top `k` entries of a view by cosine similarity, highest first.
///
/// Entries without an embedding are skipped. Ties break on the key, ascending,
/// so the result is a total order and never depends on iteration order.
pub fn search(view: &ReadView, query: &[f32], k: usize, prefix: Option<&str>) -> Vec<SearchHit> {
    if k == 0 || query.is_empty() {
        return Vec::new();
    }
    let root = &view.commit().root;
    let mut hits: Vec<SearchHit> = match prefix {
        Some(p) => collect(root.range_prefix(p), query),
        None => collect(root.iter(), query),
    };
    hits.sort_by(|a, b| {
        // `total_cmp` is a total order over every f32 including NaN, so the
        // comparator can never be inconsistent.
        b.score.total_cmp(&a.score).then_with(|| a.key.cmp(&b.key))
    });
    hits.truncate(k);
    hits
}

fn collect<'a, I>(it: I, query: &[f32]) -> Vec<SearchHit>
where
    I: Iterator<Item = (&'a String, &'a Arc<Entry>)>,
{
    it.filter_map(|(key, entry)| {
        let embedding = entry.embedding.as_deref()?;
        Some(SearchHit {
            key: key.clone(),
            score: cosine(query, embedding),
            entry: Arc::clone(entry),
        })
    })
    .collect()
}
