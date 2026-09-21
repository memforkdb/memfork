//! A6 — `at(branch, n)` returns exactly the state after commit n, for every n
//! in a 1,000-commit randomized history.
//!
//! The property is checked against an independent reference: a plain
//! `BTreeMap` replayed one operation at a time, snapshotted after every commit.
//! If time travel and the replay ever disagree at any sequence number, the
//! property fails and proptest shrinks the operation script.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use memfork_core::{Db, Value};
use proptest::prelude::*;

/// How many commits each generated history has. Fixed, so the assertions
/// below can name a sequence number.
const COMMITS: usize = 1_000;

/// A small key space, so the history overwrites and deletes rather than only
/// growing. This is what actually exercises time travel.
const KEYS: usize = 24;

#[derive(Debug, Clone)]
enum Step {
    Put(usize, u8),
    Delete(usize),
    /// A multi-key transaction: still exactly one commit, one sequence number.
    Batch(Vec<(usize, u8)>),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => (0..KEYS, any::<u8>()).prop_map(|(k, v)| Step::Put(k, v)),
        2 => (0..KEYS).prop_map(Step::Delete),
        2 => proptest::collection::vec((0..KEYS, any::<u8>()), 1..5).prop_map(Step::Batch),
    ]
}

fn key(i: usize) -> String {
    format!("k:{i:03}")
}

proptest! {
    // Each case replays a thousand commits and then checks a thousand
    // time-travel reads, so a handful of cases is already a large amount of
    // work; the randomization across runs still covers the space.
    #![proptest_config(ProptestConfig {
        cases: 8,
        max_shrink_iters: 200,
        .. ProptestConfig::default()
    })]

    #[test]
    fn a6_at_returns_the_state_after_every_commit(
        script in proptest::collection::vec(step(), COMMITS)
    ) {
        let db = Db::new();

        // The reference: the expected key/value map after each sequence number.
        let mut reference: Vec<BTreeMap<String, u8>> = Vec::with_capacity(COMMITS + 1);
        let mut current: BTreeMap<String, u8> = BTreeMap::new();
        reference.push(current.clone());

        for st in &script {
            let mut txn = db.begin("main").unwrap();
            match st {
                Step::Put(k, v) => {
                    txn.put(&key(*k), Value::new(vec![*v])).unwrap();
                    current.insert(key(*k), *v);
                }
                Step::Delete(k) => {
                    txn.delete(&key(*k)).unwrap();
                    current.remove(&key(*k));
                }
                Step::Batch(pairs) => {
                    for (k, v) in pairs {
                        txn.put(&key(*k), Value::new(vec![*v])).unwrap();
                        current.insert(key(*k), *v);
                    }
                }
            }
            let before = db.head("main").unwrap();
            let after = txn.commit(None).unwrap();
            if after != before {
                // A commit happened, so this state gets its own sequence number.
                reference.push(current.clone());
            } else {
                // A no-op (deleting an absent key) creates no commit, so the
                // branch stays where it was and no new state is recorded.
                prop_assert_eq!(&current, reference.last().unwrap());
            }
        }

        let head_seq = db.read("main").unwrap().seq();
        prop_assert_eq!(head_seq as usize + 1, reference.len());

        for (seq, expected) in reference.iter().enumerate() {
            let view = db.at("main", seq as u64).unwrap();
            prop_assert_eq!(view.seq(), seq as u64);
            prop_assert_eq!(view.len(), expected.len());
            let actual: BTreeMap<String, u8> = view
                .list("", None)
                .into_iter()
                .map(|(k, e)| (k, e.value[0]))
                .collect();
            prop_assert_eq!(&actual, expected, "time travel disagreed at seq {}", seq);
        }
    }
}

#[test]
fn a6_at_is_exact_at_the_boundaries() {
    let db = Db::new();
    for i in 0..5 {
        db.put("main", "k", Value::new(format!("v{i}"))).unwrap();
    }

    // Sequence 0 is the empty genesis state.
    assert!(db.at("main", 0).unwrap().is_empty());
    for i in 0..5u64 {
        let view = db.at("main", i + 1).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&view.get("k").unwrap().value),
            format!("v{i}")
        );
    }
    // The head is readable by sequence number, and one past it is not.
    assert_eq!(
        db.at("main", 5).unwrap().commit_id(),
        db.head("main").unwrap()
    );
    assert!(db.at("main", 6).is_err());
}

#[test]
fn a6_fork_at_rewinds_without_disturbing_the_branch() {
    let db = Db::new();
    for i in 0..5 {
        db.put("main", &format!("k{i}"), Value::new("v")).unwrap();
    }
    let head_before = db.head("main").unwrap();

    db.fork_at("main", 2, "rewound").unwrap();
    assert_eq!(db.read("rewound").unwrap().keys(), vec!["k0", "k1"]);
    assert_eq!(db.head("main").unwrap(), head_before);

    // The rewound branch carries on from there with its own history.
    db.put("rewound", "different", Value::new("v")).unwrap();
    assert_eq!(db.read("rewound").unwrap().seq(), 3);
    assert!(db.get("main", "different").unwrap().is_none());
}

#[test]
fn a6_time_travel_reaches_across_a_merge() {
    let db = Db::new();
    db.put("main", "a", Value::new("1")).unwrap();
    db.fork("main", "side").unwrap();
    db.put("side", "b", Value::new("2")).unwrap();
    db.put("main", "c", Value::new("3")).unwrap();
    db.merge("side", "main", memfork_core::MergePolicy::Fail)
        .unwrap();

    // seq 3 is the merge commit; seq 2 is `main` before it.
    assert_eq!(db.at("main", 3).unwrap().keys(), vec!["a", "b", "c"]);
    assert_eq!(db.at("main", 2).unwrap().keys(), vec!["a", "c"]);
    assert_eq!(db.at("main", 1).unwrap().keys(), vec!["a"]);
}
