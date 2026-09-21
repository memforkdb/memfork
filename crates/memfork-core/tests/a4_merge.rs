//! A4 — three-way merge: clean merges, `Fail` reporting, `Ours` and `Theirs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{text, val};
use memfork_core::{Db, Error, MergeKind, MergePolicy};

/// `main` and `side` both move, touching different keys.
fn diverged() -> Db {
    let db = Db::new();
    db.put("main", "shared", val("base")).unwrap();
    db.put("main", "only-main", val("m0")).unwrap();
    db.fork("main", "side").unwrap();
    db.put("main", "only-main", val("m1")).unwrap();
    db.put("side", "only-side", val("s1")).unwrap();
    db
}

#[test]
fn a4_non_conflicting_changes_merge_cleanly() {
    let db = diverged();
    let outcome = db.merge("side", "main", MergePolicy::Fail).unwrap();

    assert_eq!(outcome.kind, MergeKind::Merged);
    assert!(outcome.conflicts.is_empty());
    assert_eq!(outcome.changed, vec!["only-side"]);

    assert_eq!(text(&db.get("main", "shared").unwrap().unwrap()), "base");
    assert_eq!(text(&db.get("main", "only-main").unwrap().unwrap()), "m1");
    assert_eq!(text(&db.get("main", "only-side").unwrap().unwrap()), "s1");

    // The merge commit records both heads as parents.
    let head = db.commit(db.head("main").unwrap()).unwrap();
    assert_eq!(head.parents.len(), 2);
    assert_eq!(head.parents[1], db.head("side").unwrap());
}

#[test]
fn a4_a_delete_on_one_side_merges_cleanly() {
    let db = Db::new();
    db.put("main", "a", val("1")).unwrap();
    db.put("main", "b", val("2")).unwrap();
    db.fork("main", "side").unwrap();
    db.delete("side", "a").unwrap();
    db.put("main", "c", val("3")).unwrap();

    db.merge("side", "main", MergePolicy::Fail).unwrap();
    assert!(db.get("main", "a").unwrap().is_none());
    assert_eq!(db.read("main").unwrap().keys(), vec!["b", "c"]);
}

#[test]
fn a4_fail_reports_every_conflict_and_changes_nothing() {
    let db = Db::new();
    db.put("main", "x", val("base")).unwrap();
    db.put("main", "y", val("base")).unwrap();
    db.put("main", "z", val("base")).unwrap();
    db.fork("main", "side").unwrap();

    // Two keys changed differently on both sides; one changed only on `side`.
    db.put("main", "x", val("ours")).unwrap();
    db.put("main", "y", val("ours")).unwrap();
    db.put("side", "x", val("theirs")).unwrap();
    db.put("side", "y", val("theirs")).unwrap();
    db.put("side", "z", val("theirs")).unwrap();

    let head_before = db.head("main").unwrap();
    let commits_before = db.commit_count();

    let err = db.merge("side", "main", MergePolicy::Fail).unwrap_err();
    assert_eq!(
        err,
        Error::MergeConflict {
            keys: vec!["x".to_owned(), "y".to_owned()]
        },
        "conflicts must be reported sorted, and only the real ones"
    );

    // Nothing changed: not the head, not the values, not the commit graph.
    assert_eq!(db.head("main").unwrap(), head_before);
    assert_eq!(db.commit_count(), commits_before);
    assert_eq!(text(&db.get("main", "x").unwrap().unwrap()), "ours");
    assert_eq!(text(&db.get("main", "y").unwrap().unwrap()), "ours");
    assert_eq!(text(&db.get("main", "z").unwrap().unwrap()), "base");
}

#[test]
fn a4_ours_keeps_the_target_side_and_still_takes_the_rest() {
    let db = Db::new();
    db.put("main", "x", val("base")).unwrap();
    db.fork("main", "side").unwrap();
    db.put("main", "x", val("ours")).unwrap();
    db.put("side", "x", val("theirs")).unwrap();
    db.put("side", "new", val("theirs-only")).unwrap();

    let outcome = db.merge("side", "main", MergePolicy::Ours).unwrap();
    assert_eq!(outcome.conflicts, vec!["x"]);
    assert_eq!(text(&db.get("main", "x").unwrap().unwrap()), "ours");
    assert_eq!(
        text(&db.get("main", "new").unwrap().unwrap()),
        "theirs-only",
        "a non-conflicting key must still be merged under `ours`"
    );
}

#[test]
fn a4_theirs_takes_the_source_side_and_still_keeps_the_rest() {
    let db = Db::new();
    db.put("main", "x", val("base")).unwrap();
    db.fork("main", "side").unwrap();
    db.put("main", "x", val("ours")).unwrap();
    db.put("main", "mine", val("ours-only")).unwrap();
    db.put("side", "x", val("theirs")).unwrap();

    let outcome = db.merge("side", "main", MergePolicy::Theirs).unwrap();
    assert_eq!(outcome.conflicts, vec!["x"]);
    assert_eq!(text(&db.get("main", "x").unwrap().unwrap()), "theirs");
    assert_eq!(
        text(&db.get("main", "mine").unwrap().unwrap()),
        "ours-only",
        "a key only the target changed must survive `theirs`"
    );
}

#[test]
fn a4_the_same_change_on_both_sides_is_not_a_conflict() {
    let db = Db::new();
    db.put("main", "x", val("base")).unwrap();
    db.fork("main", "side").unwrap();
    db.put("main", "x", val("agreed")).unwrap();
    db.put("side", "x", val("agreed")).unwrap();

    let outcome = db.merge("side", "main", MergePolicy::Fail).unwrap();
    assert!(outcome.conflicts.is_empty());
    assert_eq!(text(&db.get("main", "x").unwrap().unwrap()), "agreed");
}

#[test]
fn a4_a_delete_against_an_edit_is_a_conflict() {
    let db = Db::new();
    db.put("main", "x", val("base")).unwrap();
    db.fork("main", "side").unwrap();
    db.put("main", "x", val("edited")).unwrap();
    db.delete("side", "x").unwrap();
    db.put("side", "other", val("forces a merge commit"))
        .unwrap();

    assert_eq!(
        db.merge("side", "main", MergePolicy::Fail).unwrap_err(),
        Error::MergeConflict {
            keys: vec!["x".to_owned()]
        }
    );

    db.merge("side", "main", MergePolicy::Theirs).unwrap();
    assert!(db.get("main", "x").unwrap().is_none());
}

#[test]
fn a4_an_untouched_target_fast_forwards() {
    let db = Db::new();
    db.put("main", "a", val("1")).unwrap();
    db.fork("main", "side").unwrap();
    db.put("side", "b", val("2")).unwrap();

    let outcome = db.merge("side", "main", MergePolicy::Fail).unwrap();
    assert_eq!(outcome.kind, MergeKind::FastForward);
    assert_eq!(db.head("main").unwrap(), db.head("side").unwrap());
    assert_eq!(db.read("main").unwrap().keys(), vec!["a", "b"]);
}

#[test]
fn a4_merging_an_ancestor_is_a_no_op() {
    let db = Db::new();
    db.put("main", "a", val("1")).unwrap();
    db.fork("main", "side").unwrap();
    db.put("main", "b", val("2")).unwrap();

    let head_before = db.head("main").unwrap();
    let outcome = db.merge("side", "main", MergePolicy::Fail).unwrap();
    assert_eq!(outcome.kind, MergeKind::UpToDate);
    assert_eq!(db.head("main").unwrap(), head_before);
}

#[test]
fn a4_merging_is_repeatable_and_deterministic() {
    // Two databases given the same operations must reach the same merge commit.
    let left = diverged();
    let right = diverged();
    let a = left.merge("side", "main", MergePolicy::Fail).unwrap();
    let b = right.merge("side", "main", MergePolicy::Fail).unwrap();
    assert_eq!(a.head, b.head);
    assert_eq!(a.base, b.base);
}
