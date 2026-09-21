//! C1 — eviction respects the budget and the score order, and evicted entries
//! reach the `on_evict` hook.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::{Arc, Mutex};

use common::text;
use memfork_core::{entry_size, score, Db, Evicted, EvictionConfig, Op, Value};

/// A value of roughly `bytes` bytes.
fn sized(bytes: usize) -> Value {
    Value::new("x".repeat(bytes))
}

/// Collects what the hook was handed.
#[derive(Default)]
struct Collector(Mutex<Vec<Evicted>>);

impl Collector {
    fn hook(self: &Arc<Self>) -> memfork_core::OnEvict {
        let me = Arc::clone(self);
        Arc::new(move |evicted: &[Evicted]| {
            me.0.lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(evicted.iter().cloned());
        })
    }

    fn keys(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|e| e.key.clone())
            .collect()
    }

    fn entries(&self) -> Vec<Evicted> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// How many bytes a branch is holding, by the same accounting eviction uses.
fn held(db: &Db, branch: &str) -> usize {
    db.read(branch)
        .unwrap()
        .list("", None)
        .iter()
        .map(|(k, e)| entry_size(k, e))
        .sum()
}

#[test]
fn c1_nothing_is_evicted_without_a_budget() {
    // The library default: no budget, so nothing is ever thrown away and
    // reads stay free of any bookkeeping.
    let db = Db::new();
    assert!(db.eviction().is_none());
    for i in 0..200 {
        db.put("main", &format!("k:{i:03}"), sized(1000)).unwrap();
    }
    assert_eq!(db.read("main").unwrap().len(), 200);
    assert!(held(&db, "main") > 200_000);
}

#[test]
fn c1_a_branch_is_brought_back_under_its_budget() {
    let db = Db::new();
    db.set_eviction(Some(EvictionConfig::with_budget(20_000).half_life(50.0)));

    for i in 0..100 {
        db.put("main", &format!("k:{i:03}"), sized(500)).unwrap();
    }

    let after = held(&db, "main");
    assert!(
        after <= 20_000,
        "the branch is holding {after} bytes against a 20,000-byte budget"
    );
    // And it did not empty the branch to get there.
    assert!(
        db.read("main").unwrap().len() > 10,
        "eviction took far more than it needed to"
    );
}

#[test]
fn c1_the_lowest_scoring_entries_are_the_ones_that_go() {
    let db = Db::new();
    let collector = Arc::new(Collector::default());
    db.on_evict(Some(collector.hook()));

    // Fill with entries of equal size and equal, low importance...
    for i in 0..40 {
        db.put(
            "main",
            &format!("filler:{i:03}"),
            sized(400).with_importance(0.1),
        )
        .unwrap();
    }
    // ...and one that matters.
    db.put("main", "precious", sized(400).with_importance(1.0))
        .unwrap();

    let budget = held(&db, "main") / 2;
    db.set_eviction(Some(EvictionConfig::with_budget(budget).half_life(10.0)));
    // A write to provoke the check.
    db.put("main", "trigger", sized(10).with_importance(1.0))
        .unwrap();

    let view = db.read("main").unwrap();
    assert!(
        view.get("precious").is_some(),
        "the most important entry was evicted"
    );
    assert!(
        !collector.keys().iter().any(|k| k == "precious"),
        "the most important entry was offered to the hook"
    );
    assert!(
        collector.keys().iter().any(|k| k.starts_with("filler:")),
        "nothing was evicted at all"
    );

    // Every evicted entry scored no higher than every surviving one.
    let head = db.commit(db.head("main").unwrap()).unwrap();
    let worst_survivor = view
        .list("", None)
        .iter()
        .map(|(_, e)| score(e.importance, head.seq, e.last_access_seq, 10.0))
        .fold(f64::INFINITY, f64::min);
    let best_evicted = collector
        .entries()
        .iter()
        .map(|e| e.score)
        .fold(f64::NEG_INFINITY, f64::max);
    assert!(
        best_evicted <= worst_survivor + 1e-9,
        "an entry scoring {best_evicted} was evicted while one scoring {worst_survivor} stayed"
    );
}

#[test]
fn c1_the_hook_receives_the_entries_before_they_go() {
    // DESIGN §4.4: the hook gets them first, so a caller can consolidate them
    // somewhere durable rather than lose them.
    let db = Db::new();
    let collector = Arc::new(Collector::default());
    db.on_evict(Some(collector.hook()));

    for i in 0..30 {
        db.put(
            "main",
            &format!("k:{i:03}"),
            sized(400).with_importance(0.5).with_meta("origin", "c1"),
        )
        .unwrap();
    }
    db.set_eviction(Some(EvictionConfig::with_budget(held(&db, "main") / 3)));
    db.put("main", "trigger", sized(10)).unwrap();

    let evicted = collector.entries();
    assert!(!evicted.is_empty(), "the hook was never called");
    for e in &evicted {
        // The whole entry, not just the key: value, metadata and all.
        assert_eq!(text(&e.entry), "x".repeat(400));
        assert_eq!(e.entry.meta.get("origin").map(String::as_str), Some("c1"));
        assert!(e.bytes > 400);
        // And it really is gone afterwards.
        assert!(db.get("main", &e.key).unwrap().is_none());
    }
}

#[test]
fn c1_eviction_is_a_commit_that_says_what_it_was() {
    // Recorded as `Evict`, not `Delete`: the log should say the entry was
    // dropped to stay inside a budget, not that someone removed it.
    let db = Db::new();
    for i in 0..30 {
        db.put("main", &format!("k:{i:03}"), sized(400)).unwrap();
    }
    let before = db.read("main").unwrap().seq();

    db.set_eviction(Some(EvictionConfig::with_budget(held(&db, "main") / 3)));
    db.put("main", "trigger", sized(10)).unwrap();

    let log = db.log("main", None).unwrap();
    let eviction = log
        .iter()
        .find(|e| e.message.as_deref().is_some_and(|m| m.starts_with("evict")))
        .expect("eviction left no commit in the log");
    assert!(eviction.seq > before);

    let commit = db.commit(eviction.id).unwrap();
    assert!(!commit.ops.is_empty());
    for op in &commit.ops {
        assert!(
            matches!(op, Op::Evict { .. }),
            "eviction recorded {op:?} rather than an eviction"
        );
    }
}

#[test]
fn c1_time_travel_still_reaches_an_evicted_value() {
    // Eviction frees the head, not the history. While a commit is retained,
    // the value it held is still readable.
    let db = Db::new();
    db.put("main", "wanted", sized(400).with_importance(0.01))
        .unwrap();
    let written_at = db.read("main").unwrap().seq();

    for i in 0..40 {
        db.put(
            "main",
            &format!("filler:{i:03}"),
            sized(400).with_importance(1.0),
        )
        .unwrap();
    }
    db.set_eviction(Some(
        EvictionConfig::with_budget(held(&db, "main") / 4).half_life(5.0),
    ));
    db.put("main", "trigger", sized(10).with_importance(1.0))
        .unwrap();

    assert!(
        db.get("main", "wanted").unwrap().is_none(),
        "the least valuable entry was not evicted"
    );
    let past = db.at("main", written_at).unwrap();
    assert_eq!(
        text(&past.get("wanted").expect("history lost the evicted value")),
        "x".repeat(400)
    );
}

#[test]
fn c1_reading_an_entry_protects_it_from_eviction() {
    // The access side structure earning its keep: two identical entries, one
    // of which has been read, and the unread one goes.
    let db = Db::new();
    db.set_eviction(Some(EvictionConfig::with_budget(usize::MAX).half_life(5.0)));

    db.put("main", "read-often", sized(400).with_importance(0.5))
        .unwrap();
    db.put("main", "never-read", sized(400).with_importance(0.5))
        .unwrap();
    for i in 0..30 {
        db.put(
            "main",
            &format!("filler:{i:03}"),
            sized(400).with_importance(0.5),
        )
        .unwrap();
        // One of the two is read as the database moves on.
        db.get("main", "read-often").unwrap();
    }

    db.set_eviction(Some(
        EvictionConfig::with_budget(held(&db, "main") / 2).half_life(5.0),
    ));
    db.put("main", "trigger", sized(10).with_importance(1.0))
        .unwrap();

    assert!(
        db.get("main", "read-often").unwrap().is_some(),
        "an entry that was read constantly was evicted"
    );
    assert!(
        db.get("main", "never-read").unwrap().is_none(),
        "an identical entry that was never read survived"
    );
}

#[test]
fn c1_eviction_is_reproducible() {
    // Two databases given the same operations evict the same entries and end
    // at the same commit id, as everything else in the engine does.
    fn run() -> (Vec<String>, memfork_core::CommitId) {
        let db = Db::new();
        let collector = Arc::new(Collector::default());
        db.on_evict(Some(collector.hook()));
        for i in 0..40 {
            db.put(
                "main",
                &format!("k:{i:03}"),
                sized(400).with_importance((i % 7) as f32 / 10.0),
            )
            .unwrap();
        }
        db.set_eviction(Some(
            EvictionConfig::with_budget(held(&db, "main") / 2).half_life(20.0),
        ));
        db.put("main", "trigger", sized(10)).unwrap();
        (collector.keys(), db.head("main").unwrap())
    }

    let (first_keys, first_head) = run();
    let (second_keys, second_head) = run();
    assert!(!first_keys.is_empty());
    assert_eq!(first_keys, second_keys);
    assert_eq!(first_head, second_head);
}

#[test]
fn c1_a_branch_is_evicted_without_disturbing_its_neighbours() {
    let db = Db::new();
    for i in 0..30 {
        db.put("main", &format!("k:{i:03}"), sized(400)).unwrap();
    }
    db.fork("main", "side").unwrap();
    let side_head = db.head("side").unwrap();

    db.set_eviction(Some(EvictionConfig::with_budget(held(&db, "main") / 3)));
    db.put("main", "trigger", sized(10)).unwrap();

    assert!(db.read("main").unwrap().len() < 31);
    assert_eq!(
        db.head("side").unwrap(),
        side_head,
        "evicting one branch moved another"
    );
    assert_eq!(db.read("side").unwrap().len(), 30);
}
