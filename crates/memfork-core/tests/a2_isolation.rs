//! A2 — writes on a fork are invisible on the parent until merge.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{text, val};
use memfork_core::{Db, MergePolicy, Value};

#[test]
fn a2_writes_on_a_fork_are_invisible_on_the_parent() {
    let db = Db::new();
    db.put("main", "note:1", val("original")).unwrap();
    db.put("main", "note:2", val("kept")).unwrap();
    let parent_head = db.head("main").unwrap();

    db.fork("main", "attempt").unwrap();

    // Every kind of write: overwrite, insert, delete.
    db.put("attempt", "note:1", val("rewritten")).unwrap();
    db.put("attempt", "note:3", val("invented")).unwrap();
    db.delete("attempt", "note:2").unwrap();

    // The parent has not moved and cannot see any of it.
    assert_eq!(db.head("main").unwrap(), parent_head);
    assert_eq!(
        text(&db.get("main", "note:1").unwrap().unwrap()),
        "original"
    );
    assert_eq!(text(&db.get("main", "note:2").unwrap().unwrap()), "kept");
    assert!(db.get("main", "note:3").unwrap().is_none());
    assert_eq!(db.read("main").unwrap().len(), 2);

    // The fork sees all of it.
    assert_eq!(
        text(&db.get("attempt", "note:1").unwrap().unwrap()),
        "rewritten"
    );
    assert!(db.get("attempt", "note:2").unwrap().is_none());
    assert_eq!(
        text(&db.get("attempt", "note:3").unwrap().unwrap()),
        "invented"
    );

    // Only after a merge does the parent see the work.
    db.merge("attempt", "main", MergePolicy::Fail).unwrap();
    assert_ne!(db.head("main").unwrap(), parent_head);
    assert_eq!(
        text(&db.get("main", "note:1").unwrap().unwrap()),
        "rewritten"
    );
    assert!(db.get("main", "note:2").unwrap().is_none());
    assert_eq!(
        text(&db.get("main", "note:3").unwrap().unwrap()),
        "invented"
    );
}

#[test]
fn a2_isolation_holds_in_both_directions() {
    // Writing on the parent after the fork must not leak into the fork either.
    let db = Db::new();
    db.put("main", "shared", val("v0")).unwrap();
    db.fork("main", "side").unwrap();

    db.put("main", "shared", val("parent moved on")).unwrap();
    assert_eq!(text(&db.get("side", "shared").unwrap().unwrap()), "v0");

    db.put("side", "side-only", val("x")).unwrap();
    assert!(db.get("main", "side-only").unwrap().is_none());
}

#[test]
fn a2_a_view_taken_before_a_write_does_not_change_under_the_reader() {
    let db = Db::new();
    db.put("main", "k", val("before")).unwrap();
    let view = db.read("main").unwrap();

    db.put("main", "k", val("after")).unwrap();

    // The view is pinned to the commit it was taken from (DESIGN §4.3).
    assert_eq!(text(&view.get("k").unwrap()), "before");
    assert_eq!(text(&db.get("main", "k").unwrap().unwrap()), "after");
}

#[test]
fn a2_isolation_survives_an_embedding_and_metadata_write() {
    let db = Db::new();
    db.put(
        "main",
        "doc:1",
        Value::new("body")
            .with_embedding(vec![1.0, 0.0, 0.0])
            .with_importance(0.9)
            .with_meta("source", "parent"),
    )
    .unwrap();
    db.fork("main", "edit").unwrap();
    db.put(
        "edit",
        "doc:1",
        Value::new("body")
            .with_embedding(vec![0.0, 1.0, 0.0])
            .with_importance(0.1)
            .with_meta("source", "fork"),
    )
    .unwrap();

    let parent = db.get("main", "doc:1").unwrap().unwrap();
    assert_eq!(parent.embedding.as_deref(), Some(&[1.0, 0.0, 0.0][..]));
    assert_eq!(parent.importance, 0.9);
    assert_eq!(
        parent.meta.get("source").map(String::as_str),
        Some("parent")
    );
}
