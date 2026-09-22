//! A data directory written by MemFork 0.1.1 opens unchanged in this build.
//!
//! `tests/fixtures/store-0.1.1/` was written by the 0.1.1 release's own code
//! (its README says how), and `expected.json` is what 0.1.1 read back from it.
//! This build must read back exactly the same branches, entries and commit ids,
//! must not rewrite the files just by opening them, and must go on producing
//! reproducible ids when it writes to them — attribution included.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use memfork::persist::{Options, Store};
use memfork::tools::dispatch::Session;
use serde_json::{json, Value as Json};

fn fixture() -> PathBuf {
    fixture_of("0.1.1")
}

fn fixture_of(version: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(format!("store-{version}"))
}

/// A private copy of the fixture's data directory: opening takes a lock and
/// writing appends, and neither may touch the checked-in files.
fn copy_of_fixture() -> (tempfile::TempDir, PathBuf) {
    copy_of("0.1.1")
}

fn copy_of(version: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["memfork.snapshot", "memfork.wal"] {
        std::fs::copy(fixture_of(version).join("data").join(name), dir.join(name)).unwrap();
    }
    (tmp, dir)
}

fn expected() -> Json {
    expected_of("0.1.1")
}

fn expected_of(version: &str) -> Json {
    let text = std::fs::read_to_string(fixture_of(version).join("expected.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// What a database holds, in the shape `expected.json` records.
fn describe(db: &memfork_core::Db) -> Json {
    describe_as(db, "memfork 0.1.1")
}

fn describe_as(db: &memfork_core::Db, written_by: &str) -> Json {
    let branches: Vec<Json> = db
        .branches()
        .iter()
        .map(|b| {
            let entries: Vec<Json> = db
                .list(&b.name, "", None)
                .unwrap()
                .iter()
                .map(|(k, e)| {
                    json!({
                        "key": k,
                        "value": String::from_utf8_lossy(&e.value),
                        "importance": e.importance,
                        "meta": e.meta,
                        "embedding": e.embedding,
                    })
                })
                .collect();
            let log: Vec<Json> = db
                .log(&b.name, None)
                .unwrap()
                .iter()
                .map(|l| {
                    json!({
                        "commit": l.id.to_hex(),
                        "seq": l.seq,
                        "parents": l.parents.iter().map(|p| p.to_hex()).collect::<Vec<_>>(),
                    })
                })
                .collect();
            json!({
                "name": b.name, "head": b.head.to_hex(), "seq": b.seq,
                "entries": entries, "log": log,
            })
        })
        .collect();
    json!({ "written_by": written_by, "branches": branches })
}

fn bytes(dir: &Path) -> Vec<Vec<u8>> {
    ["memfork.snapshot", "memfork.wal"]
        .iter()
        .map(|n| std::fs::read(dir.join(n)).unwrap())
        .collect()
}

#[test]
fn the_fixture_has_not_changed() {
    // A changed fixture would make every other test here meaningless. BLAKE3
    // here, since the crate already has it; the README lists SHA-256 for
    // checking by hand.
    for (name, expected) in [
        (
            "memfork.snapshot",
            "f4879638465b1df5a0a7d54fea34af242d7cf5a49029c9840058380e00cb4c1d",
        ),
        (
            "memfork.wal",
            "2a97e840f8e75feeb67c0b34ade899604ec45f8a1a7acbc6faf3ba1d9228bfb3",
        ),
    ] {
        let data = std::fs::read(fixture().join("data").join(name)).unwrap();
        assert_eq!(
            blake3::hash(&data).to_hex().as_str(),
            expected,
            "data/{name} changed"
        );
    }
}

#[test]
fn a_store_written_by_0_1_1_reads_back_exactly() {
    let (_tmp, dir) = copy_of_fixture();
    let before = bytes(&dir);

    // Default options, as a person upgrading would open it.
    let (db, store, recovery) = Store::open(&dir, Options::default()).unwrap();
    assert!(recovery.from_snapshot > 0, "{recovery:?}");
    assert!(recovery.from_wal > 0, "{recovery:?}");
    assert!(recovery.torn_tail.is_none(), "{recovery:?}");
    assert_eq!(describe(&db), expected());
    drop(db);
    drop(store);

    // Opening is reading: nothing was migrated or rewritten.
    assert_eq!(bytes(&dir), before, "opening rewrote the 0.1.1 files");
}

#[test]
fn writing_to_it_stays_reproducible_and_keeps_who_wrote_what() {
    let run = || {
        let (tmp, dir) = copy_of_fixture();
        let (db, store, _) = Store::open(&dir, Options::default()).unwrap();
        let session = Session::in_namespace(db.clone(), "project");
        session.set_writer("client-one");
        let put = session
            .call(
                "memfork_put",
                json!({ "key": "project:decision:upgrade", "value": "an upgrade reads the old store" })
                    .as_object()
                    .unwrap(),
            )
            .unwrap();
        let handoff = session
            .call(
                "memfork_handoff",
                json!({ "summary": "upgraded", "next": ["keep going"] })
                    .as_object()
                    .unwrap(),
            )
            .unwrap();
        drop(session);
        drop(db);
        drop(store);
        (tmp, dir, put["commit"].clone(), handoff["commit"].clone())
    };

    let (_a, dir, put, handoff) = run();
    let (_b, _, put_again, handoff_again) = run();
    assert_eq!(
        put, put_again,
        "the same write on the same store gave another id"
    );
    assert_eq!(handoff, handoff_again);
    // Fixed, so every OS must agree: the fixture's head, plus this write.
    assert_eq!(
        put,
        json!("58e8aed8d4569934d42c2cbcfc5d6be41ccbcc6073bbf50dca27b368a53c449e"),
        "the id of an attributed write on the 0.1.1 store changed"
    );

    // Reopened, the new writes and their attribution are there, and
    // everything 0.1.1 wrote is still exactly as it was.
    let (db, _store, _) = Store::open(&dir, Options::default()).unwrap();
    let entry = db.get("main", "project:decision:upgrade").unwrap().unwrap();
    assert_eq!(
        entry.meta.get(memfork_core::WRITTEN_BY).map(String::as_str),
        Some("client-one")
    );
    let brief = memfork::tools::handoff::briefing(&db, "main", "project").unwrap();
    assert_eq!(brief["latest_handoff"]["by"], "client-one");
    let old = expected();
    let now = describe(&db);
    let branch = |doc: &Json, name: &str| -> Json {
        doc["branches"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["name"] == name)
            .cloned()
            .unwrap()
    };
    for name in ["experiment", "open-branch"] {
        assert_eq!(branch(&now, name), branch(&old, name), "{name} changed");
    }
    let main_log = branch(&now, "main")["log"].as_array().unwrap().clone();
    let old_log = branch(&old, "main")["log"].as_array().unwrap().clone();
    assert_eq!(&main_log[2..], &old_log[..], "main's 0.1.1 history changed");
}

// ---- a store written by 0.2.1 ------------------------------------------------

#[test]
fn the_0_2_1_fixture_has_not_changed() {
    for (name, expected) in [
        (
            "memfork.snapshot",
            "7434fa9e418cbde5a56228bfcd8397be15c15151404894da696a515fd9c78b5a",
        ),
        (
            "memfork.wal",
            "a62d3cedcdf320f835c8d0e572d22af6b84227db9d6d3ee8b89427d46a239b00",
        ),
    ] {
        let data = std::fs::read(fixture_of("0.2.1").join("data").join(name)).unwrap();
        assert_eq!(
            blake3::hash(&data).to_hex().as_str(),
            expected,
            "data/{name} changed"
        );
    }
}

#[test]
fn a_store_written_by_0_2_1_reads_back_exactly_and_gains_nothing_by_being_read() {
    let (_tmp, dir) = copy_of("0.2.1");
    let before = bytes(&dir);
    let (db, store, recovery) = Store::open(&dir, Options::default()).unwrap();
    assert!(
        recovery.from_snapshot > 0 && recovery.from_wal > 0,
        "{recovery:?}"
    );
    assert_eq!(describe_as(&db, "memfork 0.2.1"), expected_of("0.2.1"));

    // Reading it the ways this version reads memory adds nothing: no lesson,
    // no fact record, no new key of any family, and the files are untouched.
    let session = Session::in_namespace(db.clone(), "shop");
    session.set_writer("reader");
    let args = |v: Json| v.as_object().unwrap().clone();
    session
        .call("memfork_resume", &args(json!({ "task": "refunds" })))
        .unwrap();
    session
        .call("memfork_search", &args(json!({ "text": "checkout" })))
        .unwrap();
    session
        .call("memfork_task", &args(json!({ "action": "list" })))
        .unwrap();
    assert_eq!(describe_as(&db, "memfork 0.2.1"), expected_of("0.2.1"));
    for branch in db.branches() {
        for (key, _) in db.list(&branch.name, "", None).unwrap() {
            assert!(
                !key.contains(":lesson:"),
                "{key} appeared on {}",
                branch.name
            );
        }
    }
    drop(session);
    drop(db);
    drop(store);
    assert_eq!(bytes(&dir), before, "reading rewrote the 0.2.1 files");
    assert!(
        !dir.join(memfork::sidecar::FILE).exists(),
        "reading made a side file"
    );
}
