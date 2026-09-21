//! A3 — discarding a branch leaves the parent byte-identical.
//!
//! "Byte-identical" is checked the strongest way available: the parent's head
//! commit id is content-addressed, so an unchanged id means an unchanged
//! history and an unchanged state.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{text, val};
use memfork_core::{Db, Error};

#[test]
fn a3_discard_leaves_the_parent_byte_identical() {
    let db = Db::new();
    db.put("main", "a", val("1")).unwrap();
    db.put("main", "b", val("2")).unwrap();

    let head_before = db.head("main").unwrap();
    let keys_before = db.read("main").unwrap().keys();
    let log_before: Vec<_> = db
        .log("main", None)
        .unwrap()
        .into_iter()
        .map(|e| e.id)
        .collect();

    db.fork("main", "attempt").unwrap();
    db.put("attempt", "a", val("clobbered")).unwrap();
    db.put("attempt", "c", val("garbage")).unwrap();
    db.delete("attempt", "b").unwrap();
    db.discard("attempt").unwrap();

    assert_eq!(db.head("main").unwrap(), head_before);
    assert_eq!(db.read("main").unwrap().keys(), keys_before);
    assert_eq!(
        db.log("main", None)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect::<Vec<_>>(),
        log_before
    );
    assert_eq!(text(&db.get("main", "a").unwrap().unwrap()), "1");
    assert_eq!(text(&db.get("main", "b").unwrap().unwrap()), "2");
    assert!(db.get("main", "c").unwrap().is_none());
    assert!(!db.has_branch("attempt"));
}

#[test]
fn a3_discard_frees_the_commits_only_that_branch_could_reach() {
    let db = Db::new();
    db.put("main", "a", val("1")).unwrap();
    let commits_before = db.commit_count();

    db.fork("main", "attempt").unwrap();
    for i in 0..25 {
        db.put("attempt", &format!("k{i}"), val("x")).unwrap();
    }
    assert_eq!(db.commit_count(), commits_before + 25);

    db.discard("attempt").unwrap();
    assert_eq!(
        db.commit_count(),
        commits_before,
        "discard left unreachable commits behind"
    );
}

#[test]
fn a3_a_view_handed_out_before_a_discard_stays_readable() {
    let db = Db::new();
    db.fork("main", "attempt").unwrap();
    db.put("attempt", "k", val("v")).unwrap();
    let view = db.read("attempt").unwrap();

    db.discard("attempt").unwrap();

    // The branch is gone, but the reader's view holds its commit alive.
    assert!(!db.has_branch("attempt"));
    assert_eq!(text(&view.get("k").unwrap()), "v");
}

#[test]
fn a3_the_default_branch_cannot_be_discarded() {
    let db = Db::new();
    assert_eq!(
        db.discard("main"),
        Err(Error::CannotDiscardDefaultBranch("main".to_owned()))
    );
    assert!(db.has_branch("main"));

    assert_eq!(
        db.discard("never-existed"),
        Err(Error::NoSuchBranch("never-existed".to_owned()))
    );
}

#[test]
fn a3_discard_keeps_commits_another_branch_still_needs() {
    let db = Db::new();
    db.put("main", "a", val("1")).unwrap();
    db.fork("main", "one").unwrap();
    db.put("one", "b", val("2")).unwrap();
    db.fork("one", "two").unwrap();
    db.put("two", "c", val("3")).unwrap();

    let two_head = db.head("two").unwrap();
    let one_head = db.head("one").unwrap();

    // `two` was forked from `one`, so discarding `one` must not free the
    // commits `two` still reaches through its first parent.
    db.discard("one").unwrap();
    assert_eq!(db.head("two").unwrap(), two_head);
    assert!(db.commit(one_head).is_ok());
    assert_eq!(db.read("two").unwrap().keys(), vec!["a", "b", "c"]);
}
