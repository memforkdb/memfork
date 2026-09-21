//! Who wrote an entry: recorded in its metadata, part of the commit, but not
//! part of what the entry says.
//!
//! The MCP server records the connected client's name under `memfork.by`. That
//! has to leave three things true: the same write from the same writer is the
//! same commit on every OS; two writers storing the same value are not in
//! conflict; and a store written before attribution existed reads unchanged.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use memfork_core::{ChangeKind, Db, MergePolicy, Value, WRITTEN_BY};

fn by(writer: &str, value: &str) -> Value {
    Value::new(value.to_owned()).with_meta(WRITTEN_BY, writer)
}

#[test]
fn the_same_write_by_the_same_writer_is_the_same_commit() {
    let a = Db::new();
    let b = Db::new();
    let ia = a
        .put("main", "demo:decision:db", by("client-one", "use a WAL"))
        .unwrap();
    let ib = b
        .put("main", "demo:decision:db", by("client-one", "use a WAL"))
        .unwrap();
    assert_eq!(ia, ib);

    // Fixed here, so a change to how attribution is encoded fails on every OS
    // rather than silently producing different ids on each.
    assert_eq!(
        ia.to_hex(),
        "3b063afe4a4481ef92f541732a5c50b1949bcfa7ea2defb7b8dd8754b5534322"
    );
}

#[test]
fn a_different_writer_is_a_different_commit() {
    let a = Db::new();
    let b = Db::new();
    let ia = a.put("main", "k", by("client-one", "v")).unwrap();
    let ib = b.put("main", "k", by("client-two", "v")).unwrap();
    assert_ne!(ia, ib, "who wrote it is part of the operation");
}

#[test]
fn two_writers_storing_the_same_value_do_not_conflict() {
    let db = Db::new();
    db.fork("main", "other").unwrap();
    db.put("main", "k", by("client-one", "same")).unwrap();
    db.put("other", "k", by("client-two", "same")).unwrap();

    let outcome = db.merge("other", "main", MergePolicy::Fail).unwrap();
    assert!(outcome.conflicts.is_empty(), "{:?}", outcome.conflicts);
    // The target keeps its own entry, attribution included.
    let kept = db.get("main", "k").unwrap().unwrap();
    assert_eq!(
        kept.meta.get(WRITTEN_BY).map(String::as_str),
        Some("client-one")
    );
}

#[test]
fn different_values_still_conflict_whoever_wrote_them() {
    let db = Db::new();
    db.fork("main", "other").unwrap();
    db.put("main", "k", by("client-one", "left")).unwrap();
    db.put("other", "k", by("client-one", "right")).unwrap();
    let err = db.merge("other", "main", MergePolicy::Fail).unwrap_err();
    assert!(
        matches!(err, memfork_core::Error::MergeConflict { .. }),
        "{err}"
    );
}

#[test]
fn diff_does_not_report_a_rewrite_that_only_changed_the_writer() {
    let db = Db::new();
    db.put("main", "k", by("client-one", "v")).unwrap();
    db.fork("main", "other").unwrap();
    db.put("other", "k", by("client-two", "v")).unwrap();
    db.put("other", "changed", by("client-two", "new")).unwrap();

    let changes = db.diff("main", "other").unwrap();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].key, "changed");
    assert_eq!(changes[0].kind, ChangeKind::Added);
}

#[test]
fn other_metadata_is_still_content() {
    let db = Db::new();
    db.fork("main", "other").unwrap();
    db.put("main", "k", by("client-one", "v").with_meta("tag", "a"))
        .unwrap();
    db.put("other", "k", by("client-one", "v").with_meta("tag", "b"))
        .unwrap();
    assert!(db.merge("other", "main", MergePolicy::Fail).is_err());
}
