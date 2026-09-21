//! A5 — a dropped transaction changes nothing, including sequence counters.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{text, val};
use memfork_core::{Db, Error, Value};

/// Everything about a branch that a rollback must leave untouched.
fn snapshot(db: &Db, branch: &str) -> (memfork_core::CommitId, u64, usize, Vec<String>, usize) {
    let view = db.read(branch).unwrap();
    (
        view.commit_id(),
        view.seq(),
        view.len(),
        view.keys(),
        db.commit_count(),
    )
}

#[test]
fn a5_a_dropped_transaction_changes_nothing() {
    let db = Db::new();
    db.put("main", "keep", val("v")).unwrap();
    let before = snapshot(&db, "main");

    {
        let mut txn = db.begin("main").unwrap();
        txn.put("keep", val("clobbered")).unwrap();
        txn.put("added", val("should not survive")).unwrap();
        txn.delete("keep").unwrap();
        // Inside the transaction the writes are visible to the writer.
        assert!(txn.get("keep").is_none());
        // Dropped without committing.
    }

    assert_eq!(snapshot(&db, "main"), before);
    assert_eq!(text(&db.get("main", "keep").unwrap().unwrap()), "v");
    assert!(db.get("main", "added").unwrap().is_none());
}

#[test]
fn a5_an_explicit_rollback_changes_nothing() {
    let db = Db::new();
    db.put("main", "k", val("v")).unwrap();
    let before = snapshot(&db, "main");

    let mut txn = db.begin("main").unwrap();
    txn.put("k", val("other")).unwrap();
    txn.rollback();

    assert_eq!(snapshot(&db, "main"), before);
}

#[test]
fn a5_a_rollback_does_not_burn_a_sequence_number() {
    let db = Db::new();
    db.put("main", "a", val("1")).unwrap();
    assert_eq!(db.read("main").unwrap().seq(), 1);

    for _ in 0..10 {
        let mut txn = db.begin("main").unwrap();
        txn.put("doomed", val("x")).unwrap();
        drop(txn);
    }

    // The next real commit takes seq 2, not 12: rollbacks are not events.
    db.put("main", "b", val("2")).unwrap();
    assert_eq!(db.read("main").unwrap().seq(), 2);
    assert_eq!(db.log("main", None).unwrap().len(), 3); // genesis + 2
}

#[test]
fn a5_a_committed_transaction_is_all_or_nothing() {
    let db = Db::new();
    let mut txn = db.begin("main").unwrap();
    txn.put("a", val("1")).unwrap();
    txn.put("b", val("2")).unwrap();
    txn.put("c", val("3")).unwrap();
    txn.commit(Some("three at once".to_owned())).unwrap();

    // One commit, one sequence number, three keys.
    let view = db.read("main").unwrap();
    assert_eq!(view.seq(), 1);
    assert_eq!(view.keys(), vec!["a", "b", "c"]);
    assert_eq!(db.commit(view.commit_id()).unwrap().ops.len(), 3);

    // And the state before it is the empty genesis: no intermediate state was
    // ever visible.
    assert!(db.at("main", 0).unwrap().is_empty());
}

#[test]
fn a5_a_rejected_write_leaves_the_transaction_usable() {
    let db = Db::new();
    let mut txn = db.begin("main").unwrap();
    txn.put("good", val("1")).unwrap();
    assert_eq!(
        txn.put("bad", Value::new("x").with_importance(2.0)),
        Err(Error::BadImportance(2.0))
    );
    assert_eq!(txn.put("", val("x")), Err(Error::EmptyKey));
    txn.commit(None).unwrap();

    assert_eq!(db.read("main").unwrap().keys(), vec!["good"]);
}

#[test]
fn a5_an_empty_transaction_creates_no_commit() {
    let db = Db::new();
    db.put("main", "k", val("v")).unwrap();
    let before = snapshot(&db, "main");

    let txn = db.begin("main").unwrap();
    assert!(txn.is_empty());
    txn.commit(Some("nothing to say".to_owned())).unwrap();

    assert_eq!(snapshot(&db, "main"), before);
}

#[test]
fn a5_a_stale_transaction_is_rejected_and_the_winner_stands() {
    let db = Db::new();
    db.put("main", "k", val("v0")).unwrap();

    let mut stale = db.begin("main").unwrap();
    stale.put("k", val("from the stale txn")).unwrap();

    // Another writer commits first.
    db.put("main", "k", val("v1")).unwrap();
    let winner = db.head("main").unwrap();

    match stale.commit(None) {
        Err(Error::Conflict { actual, .. }) => assert_eq!(actual, winner),
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert_eq!(db.head("main").unwrap(), winner);
    assert_eq!(text(&db.get("main", "k").unwrap().unwrap()), "v1");
}
