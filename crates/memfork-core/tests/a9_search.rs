//! A9 — exact cosine top-k matches a naive reference, and tie order is stable.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::Rng;
use memfork_core::{Db, Value};

const DIM: usize = 32;
const ENTRIES: usize = 500;

/// A reference cosine written independently of the engine's.
fn reference_cosine(a: &[f32], b: &[f32]) -> f64 {
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for i in 0..a.len() {
        dot += f64::from(a[i]) * f64::from(b[i]);
        na += f64::from(a[i]) * f64::from(a[i]);
        nb += f64::from(b[i]) * f64::from(b[i]);
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

/// A naive top-k: score everything, sort by score descending then key ascending.
fn reference_top_k(items: &[(String, Vec<f32>)], query: &[f32], k: usize) -> Vec<String> {
    let mut scored: Vec<(String, f64)> = items
        .iter()
        .map(|(key, v)| (key.clone(), reference_cosine(query, v)))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.into_iter().take(k).map(|(key, _)| key).collect()
}

fn corpus(rng: &mut Rng, n: usize) -> Vec<(String, Vec<f32>)> {
    (0..n)
        .map(|i| (format!("doc:{i:04}"), rng.vector(DIM)))
        .collect()
}

fn load(db: &Db, branch: &str, items: &[(String, Vec<f32>)]) {
    let mut txn = db.begin(branch).unwrap();
    for (key, v) in items {
        txn.put(key, Value::new("body").with_embedding(v.clone()))
            .unwrap();
    }
    txn.commit(Some("load corpus".to_owned())).unwrap();
}

#[test]
fn a9_top_k_matches_a_naive_reference() {
    let mut rng = Rng::new(0xA9);
    let items = corpus(&mut rng, ENTRIES);
    let db = Db::new();
    load(&db, "main", &items);

    for _ in 0..40 {
        let query = rng.vector(DIM);
        for k in [1usize, 5, 25, ENTRIES, ENTRIES + 10] {
            let got: Vec<String> = db
                .search("main", &query, k, None)
                .unwrap()
                .into_iter()
                .map(|h| h.key)
                .collect();
            let want = reference_top_k(&items, &query, k);
            assert_eq!(got, want, "top-{k} disagreed with the naive reference");
        }
    }
}

#[test]
fn a9_scores_match_the_reference_to_float_precision() {
    let mut rng = Rng::new(7);
    let items = corpus(&mut rng, 64);
    let db = Db::new();
    load(&db, "main", &items);

    let query = rng.vector(DIM);
    for hit in db.search("main", &query, 64, None).unwrap() {
        let v = &items
            .iter()
            .find(|(k, _)| *k == hit.key)
            .expect("hit is in the corpus")
            .1;
        let want = reference_cosine(&query, v);
        assert!(
            (f64::from(hit.score) - want).abs() < 1e-5,
            "score for {} was {} against a reference of {want}",
            hit.key,
            hit.score
        );
    }
}

#[test]
fn a9_ties_break_on_the_key_ascending_and_are_stable() {
    let db = Db::new();
    // Twenty entries sharing one vector: every score is identical, so the
    // whole ordering is decided by the tie-break.
    let shared = vec![1.0f32, 0.0, 0.0, 0.0];
    let mut txn = db.begin("main").unwrap();
    for i in (0..20).rev() {
        txn.put(
            &format!("tie:{i:02}"),
            Value::new("x").with_embedding(shared.clone()),
        )
        .unwrap();
    }
    txn.commit(None).unwrap();

    let expected: Vec<String> = (0..5).map(|i| format!("tie:{i:02}")).collect();
    for _ in 0..25 {
        let got: Vec<String> = db
            .search("main", &shared, 5, None)
            .unwrap()
            .into_iter()
            .map(|h| h.key)
            .collect();
        assert_eq!(got, expected, "tie order is not stable");
    }
}

#[test]
fn a9_the_prefix_filter_restricts_the_candidates() {
    let mut rng = Rng::new(11);
    let db = Db::new();
    let mut txn = db.begin("main").unwrap();
    let mut wanted = Vec::new();
    for i in 0..60 {
        let v = rng.vector(DIM);
        let key = if i % 3 == 0 {
            format!("keep:{i:03}")
        } else {
            format!("drop:{i:03}")
        };
        if key.starts_with("keep:") {
            wanted.push((key.clone(), v.clone()));
        }
        txn.put(&key, Value::new("x").with_embedding(v)).unwrap();
    }
    txn.commit(None).unwrap();

    let query = rng.vector(DIM);
    let got: Vec<String> = db
        .search("main", &query, 5, Some("keep:"))
        .unwrap()
        .into_iter()
        .map(|h| h.key)
        .collect();
    assert_eq!(got, reference_top_k(&wanted, &query, 5));
    assert!(got.iter().all(|k| k.starts_with("keep:")));
}

#[test]
fn a9_search_is_per_branch_and_sees_the_branch_state() {
    let db = Db::new();
    let a = vec![1.0f32, 0.0];
    let b = vec![0.0f32, 1.0];
    db.put("main", "x", Value::new("v").with_embedding(a.clone()))
        .unwrap();
    db.fork("main", "side").unwrap();
    db.put("side", "y", Value::new("v").with_embedding(b.clone()))
        .unwrap();

    assert_eq!(db.search("main", &a, 10, None).unwrap().len(), 1);
    assert_eq!(db.search("side", &a, 10, None).unwrap().len(), 2);
    assert_eq!(db.search("side", &b, 1, None).unwrap()[0].key, "y");

    // And a past commit searches the state at that commit.
    assert_eq!(db.at("side", 1).unwrap().len(), 1);
}

#[test]
fn a9_entries_without_an_embedding_are_skipped() {
    let db = Db::new();
    db.put("main", "plain", Value::new("no vector")).unwrap();
    db.put(
        "main",
        "vectored",
        Value::new("v").with_embedding(vec![1.0, 0.0]),
    )
    .unwrap();

    let hits = db.search("main", &[1.0, 0.0], 10, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].key, "vectored");
}

#[test]
fn a9_degenerate_queries_are_handled_rather_than_panicking() {
    let db = Db::new();
    db.put(
        "main",
        "zero",
        Value::new("v").with_embedding(vec![0.0, 0.0]),
    )
    .unwrap();
    db.put(
        "main",
        "unit",
        Value::new("v").with_embedding(vec![1.0, 0.0]),
    )
    .unwrap();

    // A zero-magnitude entry scores 0 rather than NaN, so it sorts last.
    let hits = db.search("main", &[1.0, 0.0], 2, None).unwrap();
    assert_eq!(hits[0].key, "unit");
    assert_eq!(hits[1].key, "zero");
    assert_eq!(hits[1].score, 0.0);

    // k = 0 and an empty query return nothing rather than erroring.
    assert!(db.search("main", &[1.0, 0.0], 0, None).unwrap().is_empty());
    assert!(db.search("main", &[], 5, None).unwrap().is_empty());

    // A wrong-dimension query is rejected, not silently scored as zero.
    assert!(db.search("main", &[1.0, 0.0, 0.0], 5, None).is_err());
}

#[test]
fn a9_the_embedding_dimension_is_fixed_on_first_insert() {
    let db = Db::new();
    assert_eq!(db.embedding_dim(), None);
    db.put(
        "main",
        "a",
        Value::new("v").with_embedding(vec![1.0, 2.0, 3.0]),
    )
    .unwrap();
    assert_eq!(db.embedding_dim(), Some(3));

    assert_eq!(
        db.put("main", "b", Value::new("v").with_embedding(vec![1.0])),
        Err(memfork_core::Error::DimensionMismatch {
            expected: 3,
            got: 1
        })
    );
    // The rejected write left nothing behind.
    assert_eq!(db.read("main").unwrap().keys(), vec!["a"]);
}

#[test]
fn a9_search_results_are_identical_across_runs() {
    // The same corpus and query must give the same ordering every time, which
    // is what makes results reproducible across platforms.
    fn run() -> Vec<(String, u32)> {
        let mut rng = Rng::new(0x5EED);
        let items = corpus(&mut rng, 200);
        let db = Db::new();
        load(&db, "main", &items);
        let query = rng.vector(DIM);
        db.search("main", &query, 20, None)
            .unwrap()
            .into_iter()
            .map(|h| (h.key, h.score.to_bits()))
            .collect()
    }
    assert_eq!(run(), run());
}
