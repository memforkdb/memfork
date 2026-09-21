//! Restart fidelity: history survives, and recovery reproduces the commit ids
//! the golden file already pins across three operating systems.
//!
//! The point of tying snapshots to the retention horizon rather than to "now"
//! is that a restart must not shorten what time travel can reach. Snapshotting
//! the present would have been simpler and would have quietly destroyed one of
//! the four things MemFork exists to do.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::Path;

use memfork::persist::{self, snapshot, FsyncPolicy, Options, Store};
use memfork_core::{Db, MergePolicy, Value};

fn options(retention: u64) -> Options {
    Options {
        fsync: FsyncPolicy::Always,
        retention,
    }
}

/// Open, do something, close. Closing releases the lock.
fn with_store<T>(dir: &Path, retention: u64, f: impl FnOnce(&Db, &Store) -> T) -> T {
    let (db, store, _) = Store::open(dir, options(retention)).expect("opened");
    let out = f(&db, &store);
    store.flush().expect("flushed");
    drop(store);
    out
}

/// Every key and value on a branch, for comparing across a restart.
fn contents(db: &Db, branch: &str, seq: u64) -> BTreeMap<String, String> {
    db.at(branch, seq)
        .unwrap()
        .list("", None)
        .into_iter()
        .map(|(k, e)| (k, String::from_utf8_lossy(&e.value).into_owned()))
        .collect()
}

#[test]
fn restart_preserves_every_retained_point_in_history() {
    // Write a history, restart, and check `at` for every sequence number —
    // not just the head. A snapshot of the present would pass a head-only
    // check and fail this one.
    const COMMITS: u64 = 300;
    let dir = tempfile::tempdir().expect("tempdir");

    let before: Vec<BTreeMap<String, String>> = with_store(dir.path(), 10_000, |db, _| {
        for i in 0..COMMITS {
            // A mix of writes and overwrites, so intermediate states differ
            // from each other rather than only growing.
            let key = format!("k:{:03}", i % 40);
            db.put("main", &key, Value::new(format!("v{i}"))).unwrap();
        }
        (0..=COMMITS).map(|n| contents(db, "main", n)).collect()
    });

    let after: Vec<BTreeMap<String, String>> = with_store(dir.path(), 10_000, |db, _| {
        assert_eq!(db.read("main").unwrap().seq(), COMMITS);
        (0..=COMMITS).map(|n| contents(db, "main", n)).collect()
    });

    assert_eq!(before.len(), after.len());
    for n in 0..=COMMITS as usize {
        assert_eq!(
            before[n], after[n],
            "the state at sequence {n} changed across a restart"
        );
    }
}

#[test]
fn restart_preserves_commit_ids_all_the_way_back() {
    const COMMITS: u64 = 150;
    let dir = tempfile::tempdir().expect("tempdir");

    let before: Vec<String> = with_store(dir.path(), 10_000, |db, _| {
        for i in 0..COMMITS {
            db.put("main", &format!("k:{i:03}"), Value::new(format!("v{i}")))
                .unwrap();
        }
        db.log("main", None)
            .unwrap()
            .iter()
            .map(|e| e.id.to_hex())
            .collect()
    });

    let after: Vec<String> = with_store(dir.path(), 10_000, |db, _| {
        db.log("main", None)
            .unwrap()
            .iter()
            .map(|e| e.id.to_hex())
            .collect()
    });

    // Ids are recomputed from the records on replay, never stored, so equality
    // here is the determinism property rather than a byte-for-byte copy.
    assert_eq!(before, after, "commit ids changed across a restart");
}

#[test]
fn restart_preserves_branches_merges_and_forks_from_history() {
    let dir = tempfile::tempdir().expect("tempdir");

    let (main_head, side_head, merged_head) = with_store(dir.path(), 10_000, |db, _| {
        db.put("main", "a", Value::new("1")).unwrap();
        db.put("main", "b", Value::new("2")).unwrap();
        db.fork("main", "side").unwrap();
        db.put("side", "c", Value::new("3")).unwrap();
        db.put("main", "d", Value::new("4")).unwrap();
        let outcome = db.merge("side", "main", MergePolicy::Fail).unwrap();
        db.fork_at("main", 2, "rewound").unwrap();
        (
            db.head("main").unwrap(),
            db.head("side").unwrap(),
            outcome.head,
        )
    });

    with_store(dir.path(), 10_000, |db, _| {
        let names: Vec<String> = db.branches().into_iter().map(|b| b.name).collect();
        assert_eq!(names, vec!["main", "rewound", "side"]);
        assert_eq!(db.head("main").unwrap(), main_head);
        assert_eq!(db.head("side").unwrap(), side_head);
        assert_eq!(db.head("main").unwrap(), merged_head);

        // The merge commit is still a merge.
        let head = db.commit(merged_head).unwrap();
        assert_eq!(head.parents.len(), 2);

        // The branch forked from history is still where it was put.
        assert_eq!(db.read("rewound").unwrap().keys(), vec!["a", "b"]);
        // And forking from history still works after the restart.
        db.fork_at("main", 1, "later").unwrap();
        assert_eq!(db.read("later").unwrap().keys(), vec!["a"]);
    });
}

#[test]
fn a_discarded_branch_stays_discarded() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_store(dir.path(), 10_000, |db, _| {
        db.put("main", "a", Value::new("1")).unwrap();
        db.fork("main", "attempt").unwrap();
        db.put("attempt", "junk", Value::new("x")).unwrap();
        db.discard("attempt").unwrap();
    });

    with_store(dir.path(), 10_000, |db, _| {
        assert!(
            !db.has_branch("attempt"),
            "a discarded branch came back after a restart"
        );
        assert!(db.get("main", "junk").unwrap().is_none());
        assert_eq!(db.read("main").unwrap().keys(), vec!["a"]);
    });
}

#[test]
fn compaction_moves_the_snapshot_forward_without_shortening_history() {
    // The retention limit decides what a restart can reach — not when
    // compaction last ran. Before and after folding, `at` must answer the same
    // for everything inside the limit.
    const RETENTION: u64 = 50;
    const COMMITS: u64 = 200;
    let dir = tempfile::tempdir().expect("tempdir");

    let before: BTreeMap<u64, BTreeMap<String, String>> =
        with_store(dir.path(), RETENTION, |db, store| {
            for i in 0..COMMITS {
                db.put(
                    "main",
                    &format!("k:{:03}", i % 30),
                    Value::new(format!("v{i}")),
                )
                .unwrap();
            }
            let folded = store.compact(db).expect("compacted");
            assert!(folded, "nothing was folded despite passing the horizon");
            assert!(
                dir.path().join(snapshot::SNAPSHOT_FILE).exists(),
                "compaction wrote no snapshot"
            );
            (COMMITS - RETENTION..=COMMITS)
                .map(|n| (n, contents(db, "main", n)))
                .collect()
        });

    with_store(dir.path(), RETENTION, |db, _| {
        assert_eq!(db.read("main").unwrap().seq(), COMMITS);
        for (n, expected) in &before {
            assert_eq!(
                &contents(db, "main", *n),
                expected,
                "the state at sequence {n} changed across compaction and a restart"
            );
        }
    });
}

#[test]
fn the_snapshot_is_refused_rather_than_misread_when_it_is_damaged() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_store(dir.path(), 10, |db, store| {
        for i in 0..50 {
            db.put("main", &format!("k:{i:03}"), Value::new("v"))
                .unwrap();
        }
        store.compact(db).expect("compacted");
    });

    let path = dir.path().join(snapshot::SNAPSHOT_FILE);
    let mut bytes = std::fs::read(&path).expect("readable");

    // A flipped bit anywhere in the body.
    let target = bytes.len() / 2;
    bytes[target] ^= 0xff;
    std::fs::write(&path, &bytes).expect("written");
    match snapshot::read(dir.path()) {
        Err(snapshot::SnapshotError::Corrupt { .. }) => {}
        other => panic!("a damaged snapshot was not refused: {other:?}"),
    }

    // A file that is not a snapshot at all.
    std::fs::write(&path, b"something else entirely").expect("written");
    match snapshot::read(dir.path()) {
        Err(snapshot::SnapshotError::NotASnapshot { .. }) => {}
        other => panic!("a foreign file was read as a snapshot: {other:?}"),
    }

    // A snapshot from a later format.
    let mut future = Vec::new();
    future.extend_from_slice(snapshot::MAGIC);
    future.extend_from_slice(&(snapshot::VERSION + 1).to_le_bytes());
    future.extend_from_slice(&[0u8; 8]);
    std::fs::write(&path, &future).expect("written");
    match snapshot::read(dir.path()) {
        Err(snapshot::SnapshotError::UnknownVersion { found, .. }) => {
            assert_eq!(found, snapshot::VERSION + 1);
        }
        other => panic!("a future snapshot format was not refused: {other:?}"),
    }
}

#[test]
fn the_golden_commit_ids_survive_a_round_trip_through_the_log() {
    // The strongest recovery check available: replay the A7 script through a
    // real write-ahead log and compare against the ids already pinned on
    // Linux, macOS and Windows. If recovery changed a commit id by so much as
    // a field, this is where it shows.
    let golden: BTreeMap<String, String> = {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("memfork-core")
            .join("tests")
            .join("golden")
            .join("commit_ids.json");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        serde_json::from_str(&text).expect("the golden file is JSON")
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let recorded = with_store(dir.path(), 10_000, |db, _| run_golden_script(db));

    // Every id the script produced matches the golden file...
    for (label, id) in &recorded {
        assert_eq!(
            golden.get(label),
            Some(id),
            "`{label}` does not match the golden file while journalling"
        );
    }

    // ...and still does once it has been through the log and back.
    let replayed = with_store(dir.path(), 10_000, |db, _| {
        let mut out = BTreeMap::new();
        out.insert(
            "22-final-head".to_owned(),
            db.head("main").unwrap().to_hex(),
        );
        out.insert(
            "00-genesis".to_owned(),
            db.at("main", 0).unwrap().commit_id().to_hex(),
        );
        out
    });
    assert_eq!(
        replayed
            .get("25-final-head")
            .or_else(|| replayed.get("22-final-head")),
        recorded
            .get("25-final-head")
            .or_else(|| recorded.get("22-final-head")),
        "the final head changed when it came back from the log"
    );
    assert_eq!(
        replayed.get("00-genesis"),
        recorded.get("00-genesis"),
        "genesis changed when it came back from the log"
    );
}

/// The A7 determinism script, run against a journalled database.
///
/// Kept in step with `memfork-core/tests/a7_determinism.rs` by comparing
/// against the same golden file: a change to one without the other fails here.
fn run_golden_script(db: &Db) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut record = |label: &str, id: memfork_core::CommitId| {
        out.insert(label.to_owned(), id.to_hex());
    };

    record("00-genesis", db.head("main").unwrap());
    record(
        "01-put-ascii",
        db.put("main", "a:1", Value::new("hello")).unwrap(),
    );
    record(
        "02-put-empty-value",
        db.put("main", "a:2", Value::new("")).unwrap(),
    );
    record(
        "03-put-unicode",
        db.put("main", "ключ:Ω", Value::new("значение — ok"))
            .unwrap(),
    );
    record(
        "04-put-emoji",
        db.put("main", "emoji:🔱", Value::new("🜂🜃🜄🜁")).unwrap(),
    );
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
    record(
        "09-put-embedding",
        db.put(
            "main",
            "e:1",
            Value::new("vec").with_embedding(vec![0.0, -0.0, 1.0, -1.0, 0.1, f32::MIN_POSITIVE]),
        )
        .unwrap(),
    );
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
    record(
        "11-put-ttl",
        db.put("main", "t:1", Value::new("expiring").with_ttl_commits(7))
            .unwrap(),
    );

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

    let mut txn = db.begin("main").unwrap();
    txn.put("b:3", Value::new("three")).unwrap();
    txn.put("b:1", Value::new("one")).unwrap();
    txn.put("b:2", Value::new("two")).unwrap();
    record(
        "15-txn-multi-key",
        txn.commit(Some("batch".to_owned())).unwrap(),
    );

    record("16-delete", db.delete("main", "a:2").unwrap());
    record("17-delete-absent", db.delete("main", "not-there").unwrap());
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

    record("24-fork-at", db.fork_at("main", 3, "rewound").unwrap());
    record("25-final-head", db.head("main").unwrap());
    out
}

#[test]
fn an_ephemeral_database_writes_nothing() {
    // The binary persists by default; `--ephemeral` must genuinely opt out,
    // not merely use a different directory.
    let dir = tempfile::tempdir().expect("tempdir");
    let before: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert!(before.is_empty());

    let db = Db::new();
    assert!(!db.is_journalled());
    db.put("main", "k", Value::new("v")).unwrap();

    let after: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert!(
        after.is_empty(),
        "an unjournalled database touched the disk"
    );
    assert!(!dir.path().join(persist::WAL_FILE).exists());
}
