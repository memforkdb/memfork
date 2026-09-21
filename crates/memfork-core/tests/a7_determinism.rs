//! A7 — the same operation script yields identical commit ids on every OS.
//!
//! The script below exercises everything that could plausibly leak a platform
//! difference into a commit id: float importances and embeddings, non-ASCII
//! and emoji keys and values, metadata ordering, commit messages present,
//! absent and empty, multi-key transactions, deletes, forks, both conflict
//! policies and a merge commit with two parents.
//!
//! The expected ids live in `tests/golden/commit_ids.json`, which is checked in
//! and marked `-text` in `.gitattributes` so no platform rewrites its line
//! endings. CI runs this test on Linux, macOS and Windows against the same
//! file, so a drift on any one of them fails the build.
//!
//! To regenerate the golden file after a deliberate format change, run:
//!   MEMFORK_UPDATE_GOLDEN=1 cargo test -p memfork-core --test a7_determinism

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use memfork_core::{Db, MergePolicy, Value};

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("commit_ids.json")
}

/// Run the fixed script and return `label -> commit id hex`, in label order.
fn run_script() -> BTreeMap<String, String> {
    let db = Db::new();
    let mut out = BTreeMap::new();
    let mut record = |label: &str, id: memfork_core::CommitId| {
        out.insert(label.to_owned(), id.to_hex());
    };

    record("00-genesis", db.head("main").unwrap());

    // Plain values.
    record(
        "01-put-ascii",
        db.put("main", "a:1", Value::new("hello")).unwrap(),
    );
    record(
        "02-put-empty-value",
        db.put("main", "a:2", Value::new("")).unwrap(),
    );

    // Non-ASCII and emoji in both keys and values, and a multi-byte key that
    // sorts after an ASCII one.
    record(
        "03-put-unicode",
        db.put("main", "ключ:Ω", Value::new("значение — ok"))
            .unwrap(),
    );
    record(
        "04-put-emoji",
        db.put("main", "emoji:🔱", Value::new("🜂🜃🜄🜁")).unwrap(),
    );

    // Floats: an exact value, a value that is not representable in binary, a
    // subnormal and the extremes of the permitted range.
    record(
        "05-put-importance-exact",
        db.put("main", "f:1", Value::new("x").with_importance(0.5))
            .unwrap(),
    );
    record(
        "06-put-importance-inexact",
        db.put("main", "f:2", Value::new("x").with_importance(0.1))
            .unwrap(),
    );
    record(
        "07-put-importance-bounds",
        db.put("main", "f:3", Value::new("x").with_importance(0.0))
            .unwrap(),
    );
    record(
        "08-put-importance-one",
        db.put("main", "f:4", Value::new("x").with_importance(1.0))
            .unwrap(),
    );

    // Embeddings, including a subnormal and a negative zero, which have
    // distinct bit patterns and must hash distinctly.
    record(
        "09-put-embedding",
        db.put(
            "main",
            "e:1",
            Value::new("vec").with_embedding(vec![0.0, -0.0, 1.0, -1.0, 0.1, f32::MIN_POSITIVE]),
        )
        .unwrap(),
    );

    // Metadata: inserted out of order, and must hash in sorted order.
    record(
        "10-put-meta",
        db.put(
            "main",
            "m:1",
            Value::new("meta")
                .with_meta("zeta", "last")
                .with_meta("alpha", "first")
                .with_meta("Ω", "unicode key"),
        )
        .unwrap(),
    );

    // A TTL, which is an optional field and so exercises the option tag.
    record(
        "11-put-ttl",
        db.put("main", "t:1", Value::new("expiring").with_ttl_commits(7))
            .unwrap(),
    );

    // The same change under three different messages. Each is its own commit,
    // and a missing message is not an empty one.
    let mut txn = db.begin("main").unwrap();
    txn.put("msg:1", Value::new("same change")).unwrap();
    record(
        "12-message-some",
        txn.commit(Some("a message".to_owned())).unwrap(),
    );

    db.fork_at("main", 11, "msg-none").unwrap();
    let mut txn = db.begin("msg-none").unwrap();
    txn.put("msg:1", Value::new("same change")).unwrap();
    record("13-message-none", txn.commit(None).unwrap());

    db.fork_at("main", 11, "msg-empty").unwrap();
    let mut txn = db.begin("msg-empty").unwrap();
    txn.put("msg:1", Value::new("same change")).unwrap();
    record("14-message-empty", txn.commit(Some(String::new())).unwrap());

    // A multi-key transaction, staged out of order. The commit must hash the
    // same as if the keys had been staged in order.
    let mut txn = db.begin("main").unwrap();
    txn.put("b:3", Value::new("three")).unwrap();
    txn.put("b:1", Value::new("one")).unwrap();
    txn.put("b:2", Value::new("two")).unwrap();
    record(
        "15-txn-multi-key",
        txn.commit(Some("batch".to_owned())).unwrap(),
    );

    // A delete, and a delete of an absent key, which is a no-op.
    record("16-delete", db.delete("main", "a:2").unwrap());
    record("17-delete-absent", db.delete("main", "not-there").unwrap());

    // Branching, divergence and a clean merge.
    record("18-fork", db.fork("main", "side").unwrap());
    record(
        "19-side-put",
        db.put("side", "s:1", Value::new("side")).unwrap(),
    );
    record(
        "20-main-put",
        db.put("main", "a:1", Value::new("moved on")).unwrap(),
    );
    record(
        "21-merge-clean",
        db.merge("side", "main", MergePolicy::Fail).unwrap().head,
    );

    // A conflicting merge under each resolving policy, from a fresh fork.
    db.fork("main", "ours-side").unwrap();
    db.put("ours-side", "a:1", Value::new("their answer"))
        .unwrap();
    db.put("main", "a:1", Value::new("our answer")).unwrap();
    record(
        "22-merge-ours",
        db.merge("ours-side", "main", MergePolicy::Ours)
            .unwrap()
            .head,
    );

    db.fork("main", "theirs-side").unwrap();
    db.put("theirs-side", "a:1", Value::new("their second answer"))
        .unwrap();
    db.put("main", "a:1", Value::new("our second answer"))
        .unwrap();
    record(
        "23-merge-theirs",
        db.merge("theirs-side", "main", MergePolicy::Theirs)
            .unwrap()
            .head,
    );

    // A fork from history, which must reach an id already in the log.
    record("24-fork-at", db.fork_at("main", 3, "rewound").unwrap());

    // And the final head, which depends on every step above.
    record("25-final-head", db.head("main").unwrap());
    out
}

#[test]
fn a7_the_same_script_yields_the_same_commit_ids_everywhere() {
    let actual = run_script();
    let path = golden_path();

    if std::env::var_os("MEMFORK_UPDATE_GOLDEN").is_some() {
        let mut json = serde_json::to_string_pretty(&actual).unwrap();
        json.push('\n');
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Written with explicit `\n` only (DESIGN §7).
        std::fs::write(&path, json.replace("\r\n", "\n")).unwrap();
        eprintln!("wrote golden file {}", path.display());
        return;
    }

    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read the golden file {}: {e}. \
             Regenerate it with MEMFORK_UPDATE_GOLDEN=1.",
            path.display()
        )
    });
    let expected: BTreeMap<String, String> = serde_json::from_str(&raw).unwrap();

    for (label, want) in &expected {
        let got = actual
            .get(label)
            .unwrap_or_else(|| panic!("the script no longer produces the step `{label}`"));
        assert_eq!(
            got, want,
            "commit id for `{label}` differs from the golden file: \
             the engine is no longer deterministic across platforms"
        );
    }
    assert_eq!(
        actual.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>(),
        "the script and the golden file cover different steps"
    );
}

#[test]
fn a7_two_databases_running_the_same_script_agree() {
    assert_eq!(run_script(), run_script());
}

#[test]
fn a7_the_genesis_commit_is_the_same_in_every_database() {
    assert_eq!(
        Db::new().head("main").unwrap(),
        Db::new().head("main").unwrap()
    );
}

#[test]
fn a7_staging_order_does_not_affect_the_commit_id() {
    let forward = Db::new();
    let mut txn = forward.begin("main").unwrap();
    for i in 0..10 {
        txn.put(&format!("k{i}"), Value::new(format!("v{i}")))
            .unwrap();
    }
    let a = txn.commit(None).unwrap();

    let backward = Db::new();
    let mut txn = backward.begin("main").unwrap();
    for i in (0..10).rev() {
        txn.put(&format!("k{i}"), Value::new(format!("v{i}")))
            .unwrap();
    }
    let b = txn.commit(None).unwrap();

    assert_eq!(
        a, b,
        "the order keys were staged in leaked into the commit id"
    );
}

#[test]
fn a7_a_different_value_gives_a_different_id() {
    // The converse of determinism: ids must actually depend on the content.
    let one = Db::new();
    let a = one.put("main", "k", Value::new("v")).unwrap();
    let two = Db::new();
    let b = two.put("main", "k", Value::new("w")).unwrap();
    assert_ne!(a, b);

    // Including the parts of an entry that are easy to forget to hash.
    let three = Db::new();
    let c = three
        .put("main", "k", Value::new("v").with_importance(0.25))
        .unwrap();
    assert_ne!(a, c);

    let four = Db::new();
    let d = four
        .put("main", "k", Value::new("v").with_meta("a", "b"))
        .unwrap();
    assert_ne!(a, d);

    let five = Db::new();
    let e = five
        .put("main", "k", Value::new("v").with_ttl_commits(1))
        .unwrap();
    assert_ne!(a, e);

    let six = Db::new();
    let f = six
        .put("main", "k", Value::new("v").with_embedding(vec![0.0]))
        .unwrap();
    assert_ne!(a, f);
}

#[test]
fn a7_commit_ids_round_trip_through_hex() {
    let db = Db::new();
    let id = db.put("main", "k", Value::new("v")).unwrap();
    let hex = id.to_hex();
    assert_eq!(hex.len(), 64);
    assert_eq!(memfork_core::CommitId::from_hex(&hex).unwrap(), id);
    assert_eq!(
        memfork_core::CommitId::from_hex(&hex.to_uppercase()).unwrap(),
        id
    );
    assert!(memfork_core::CommitId::from_hex("nope").is_err());
    assert!(memfork_core::CommitId::from_hex(&"z".repeat(64)).is_err());
}

#[test]
fn a7_the_message_is_part_of_the_commit_id() {
    // The same change, from the same parent, under two different messages is
    // two different commits. Without this, a message would be data the content
    // address does not cover, and two commits that a reader can tell apart
    // would share an id.
    fn commit_with(message: Option<&str>) -> memfork_core::CommitId {
        let db = Db::new();
        let mut txn = db.begin("main").unwrap();
        txn.put("k", Value::new("v")).unwrap();
        txn.commit(message.map(str::to_owned)).unwrap()
    }

    let one = commit_with(Some("first attempt"));
    let two = commit_with(Some("second attempt"));
    assert_ne!(
        one, two,
        "two commits differing only in their message share an id"
    );

    // And the same message still gives the same id: the message is hashed, not
    // salted.
    assert_eq!(commit_with(Some("first attempt")), one);
}

#[test]
fn a7_no_message_is_not_an_empty_message() {
    fn commit_with(message: Option<String>) -> memfork_core::CommitId {
        let db = Db::new();
        let mut txn = db.begin("main").unwrap();
        txn.put("k", Value::new("v")).unwrap();
        txn.commit(message).unwrap()
    }

    let none = commit_with(None);
    let empty = commit_with(Some(String::new()));
    let real = commit_with(Some("a message".to_owned()));

    assert_ne!(
        none, empty,
        "a commit with no message hashes the same as one with an empty message"
    );
    assert_ne!(none, real);
    assert_ne!(empty, real);
}

#[test]
fn a7_the_message_is_hashed_at_every_level_that_makes_a_commit() {
    // Transactions are covered above; the auto-commit and merge paths build
    // their own messages, so they must feed them to the same hash.
    let db = Db::new();
    let head = db.put("main", "k", Value::new("v")).unwrap();
    let commit = db.commit(head).unwrap();
    assert_eq!(
        commit.id,
        memfork_core::Commit::compute_id(&commit.parents, commit.message.as_deref(), &commit.ops),
        "an auto-commit's stored id does not match its own contents"
    );

    db.fork("main", "side").unwrap();
    db.put("side", "s", Value::new("v")).unwrap();
    db.put("main", "m", Value::new("v")).unwrap();
    let merged = db.merge("side", "main", MergePolicy::Fail).unwrap();
    let commit = db.commit(merged.head).unwrap();
    assert_eq!(commit.parents.len(), 2);
    assert_eq!(
        commit.id,
        memfork_core::Commit::compute_id(&commit.parents, commit.message.as_deref(), &commit.ops),
        "a merge commit's stored id does not match its own contents"
    );
}

#[test]
fn a7_every_commit_in_a_history_matches_its_own_contents() {
    // A sweep over the whole golden script: recomputing the address of every
    // commit from its parents, message and ops must reproduce the id it is
    // filed under.
    let db = Db::new();
    db.put("main", "a", Value::new("1")).unwrap();
    db.fork("main", "side").unwrap();
    db.put("side", "b", Value::new("2")).unwrap();
    db.put("main", "c", Value::new("3")).unwrap();
    db.merge("side", "main", MergePolicy::Fail).unwrap();
    db.delete("main", "a").unwrap();

    for entry in db.log("main", None).unwrap() {
        let commit = db.commit(entry.id).unwrap();
        assert_eq!(
            commit.id,
            memfork_core::Commit::compute_id(
                &commit.parents,
                commit.message.as_deref(),
                &commit.ops
            ),
            "commit at seq {} does not match its own contents",
            commit.seq
        );
    }
}
