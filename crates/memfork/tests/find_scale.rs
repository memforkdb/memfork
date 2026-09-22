//! Text search over many entries: bounded, deterministic, and how long it
//! takes. Timings are printed, never asserted; DESIGN §6.6 records them.
//!
//! `cargo test --release -p memfork --test find_scale -- --ignored --nocapture`
//! runs the 100,000-entry size as well.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Instant;

use memfork::tools::dispatch::Session;
use serde_json::{json, Value as Json};

const WORDS: &[&str] = &[
    "refund",
    "cart",
    "checkout",
    "payment",
    "invoice",
    "login",
    "session",
    "token",
    "cache",
    "queue",
    "retry",
    "timeout",
    "schema",
    "migration",
    "index",
    "shard",
    "lock",
    "webhook",
];

fn filled(n: usize) -> Session {
    let db = memfork_core::Db::new();
    // Written in batches, as a long-running project would have.
    let mut i = 0;
    while i < n {
        let mut txn = db.begin("main").unwrap();
        for j in i..(i + 1000).min(n) {
            let text = format!(
                "decision {j}: the {} path uses {} with {} because {}",
                WORDS[j % WORDS.len()],
                WORDS[(j / 7) % WORDS.len()],
                WORDS[(j / 49) % WORDS.len()],
                WORDS[(j * 13) % WORDS.len()],
            );
            txn.put(
                &format!("shop:decision:d{j:06}"),
                memfork_core::Value::new(text),
            )
            .unwrap();
        }
        txn.commit(Some(format!("batch {i}"))).unwrap();
        i += 1000;
    }
    Session::in_namespace(db, "shop")
}

fn search(session: &Session, text: &str, k: u64) -> Json {
    let args = json!({"text": text, "k": k});
    session
        .call("memfork_search", args.as_object().unwrap())
        .unwrap()
}

fn measure(n: usize) {
    let session = filled(n);
    for (query, k) in [
        ("refund", 10),
        ("cart checkout lock", 10),
        ("webhook retry", 50),
    ] {
        let first = search(&session, query, k);
        let start = Instant::now();
        let runs = 5;
        for _ in 0..runs {
            assert_eq!(
                search(&session, query, k),
                first,
                "the same search gave another answer"
            );
        }
        let each = start.elapsed() / runs;
        let hits = first["hits"].as_array().unwrap();
        assert!(hits.len() as u64 <= k);
        let scores: Vec<u64> = hits.iter().map(|h| h["score"].as_u64().unwrap()).collect();
        assert!(
            scores.windows(2).all(|w| w[0] >= w[1]),
            "not ranked: {scores:?}"
        );
        println!(
            "find: {n} entries, {query:?} k={k}: {} hits, {each:?} each",
            hits.len()
        );
    }
    // Asking for more than the ceiling gets the ceiling.
    let many = search(&session, "decision", 10_000);
    assert_eq!(many["hits"].as_array().unwrap().len(), 50);
}

#[test]
fn find_over_ten_thousand_entries() {
    measure(10_000);
}

#[test]
#[ignore = "slow in a debug build; run with --release --ignored for the DESIGN figures"]
fn find_over_a_hundred_thousand_entries() {
    measure(100_000);
}
