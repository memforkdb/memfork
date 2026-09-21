//! C2 — a process killed mid-write recovers to a valid state with identical
//! commit ids up to the last durable commit.
//!
//! The kill is real. `Child::kill` is `TerminateProcess` on Windows and
//! `SIGKILL` on Unix: no unwinding, no destructors, no flush on the way out —
//! the same thing an operator or an out-of-memory killer does. Simulating a
//! crash by closing a file cleanly would test nothing, because the interesting
//! failures are exactly the ones a clean close does not produce.
//!
//! The writer appends each commit id to a progress file *after* the commit is
//! durable, so anything in that file is something MemFork promised. Recovery
//! has to honour every one of them.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use memfork::persist::{self, wal, FsyncPolicy, Options};

mod support;

/// A data directory and the progress file beside it.
struct Crash {
    dir: tempfile::TempDir,
}

impl Crash {
    fn new() -> Self {
        Crash {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn progress(&self) -> PathBuf {
        self.dir.path().join("progress.txt")
    }

    /// Start a writer that commits until something stops it.
    fn spawn_writer(&self, fsync: &str) -> Child {
        support::memfork()
            .arg("crash-writer")
            .arg("--data-dir")
            .arg(self.data())
            .arg("--progress")
            .arg(self.progress())
            .arg("--fsync")
            .arg(fsync)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the crash writer started")
    }

    /// The commit ids the writer said were durable.
    fn promised(&self) -> Vec<String> {
        std::fs::read_to_string(self.progress())
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| l.len() == 64)
            .map(str::to_owned)
            .collect()
    }

    /// Wait until the writer has promised at least `n` commits.
    fn wait_for(&self, n: usize, within: Duration) -> usize {
        let start = Instant::now();
        loop {
            let got = self.promised().len();
            if got >= n {
                return got;
            }
            if start.elapsed() > within {
                return got;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Reopen the data directory and hand back the recovered database.
    fn recover(&self) -> (memfork_core::Db, persist::Recovery) {
        let (db, store, recovery) = persist::Store::open(
            &self.data(),
            Options {
                fsync: FsyncPolicy::Always,
                retention: 10_000,
            },
        )
        .expect("the data directory reopened after the crash");
        // Drop the store so the lock is released for the next open.
        drop(store);
        (db, recovery)
    }
}

/// Kill a child the way an operating system does, and wait for it to be gone.
fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn c2_a_killed_writer_leaves_every_promised_commit_recoverable() {
    let crash = Crash::new();
    let mut writer = crash.spawn_writer("always");

    // Let it get properly under way, then kill it mid-write.
    let written = crash.wait_for(25, Duration::from_secs(30));
    assert!(written >= 25, "the writer only managed {written} commits");
    kill(&mut writer);

    let promised = crash.promised();
    assert!(!promised.is_empty());

    let (db, recovery) = crash.recover();
    let log = db.log("main", None).unwrap();
    let recovered: Vec<String> = log.iter().map(|e| e.id.to_hex()).collect();

    // Every commit the writer promised is present, with the same id it had
    // before the crash. Ids are recomputed on replay, never read back, so
    // this is a statement about determinism rather than about copying bytes.
    for id in &promised {
        assert!(
            recovered.contains(id),
            "commit {id} was promised as durable and did not survive"
        );
    }
    assert!(recovery.from_wal >= promised.len());

    // And the state matches: one key per promised commit, plus genesis.
    let view = db.read("main").unwrap();
    assert!(view.len() >= promised.len());
    assert_eq!(view.seq() as usize, log.len() - 1);
}

#[test]
fn c2_the_recovered_history_is_in_the_right_order() {
    let crash = Crash::new();
    let mut writer = crash.spawn_writer("always");
    crash.wait_for(20, Duration::from_secs(30));
    kill(&mut writer);

    let promised = crash.promised();
    let (db, _) = crash.recover();

    // The log is newest-first; the promises are oldest-first.
    let mut recovered: Vec<String> = db
        .log("main", None)
        .unwrap()
        .iter()
        .map(|e| e.id.to_hex())
        .collect();
    recovered.reverse();
    // Genesis is not a promise, so line them up from the first promise on.
    let start = recovered
        .iter()
        .position(|id| id == &promised[0])
        .expect("the first promised commit is in the history");
    assert_eq!(
        &recovered[start..start + promised.len()],
        promised.as_slice(),
        "the recovered history is not the order it was written in"
    );
}

#[test]
fn c2_a_torn_tail_is_truncated_rather_than_treated_as_corruption() {
    // The tail is what a kill actually leaves behind. Everything before it is
    // whole, and reporting the whole log as corrupt would throw away a
    // perfectly good history because its last few bytes are missing.
    let crash = Crash::new();
    let mut writer = crash.spawn_writer("always");
    crash.wait_for(15, Duration::from_secs(30));
    kill(&mut writer);

    let wal_path = crash.data().join(persist::WAL_FILE);
    let whole = std::fs::read(&wal_path).expect("the log is readable");
    let promised = crash.promised();

    // Cut the file mid-record, which is what a kill during a write produces.
    for cut in [1usize, 7, 20] {
        std::fs::write(&wal_path, &whole[..whole.len() - cut]).expect("truncated");
        let replay = wal::read(&wal_path).expect("a torn log is still readable");
        assert!(
            replay.torn_tail.is_some(),
            "cutting {cut} byte(s) off was not noticed"
        );
        assert!(
            !replay.records.is_empty(),
            "a torn tail threw away the whole history"
        );
        assert!(replay.valid_len < (whole.len() - cut) as u64);
    }

    // And opening it truncates the tail and keeps the rest.
    std::fs::write(&wal_path, &whole[..whole.len() - 7]).expect("truncated");
    let (db, recovery) = crash.recover();
    assert!(
        recovery.torn_tail.is_some(),
        "the torn tail went unreported"
    );
    let recovered: Vec<String> = db
        .log("main", None)
        .unwrap()
        .iter()
        .map(|e| e.id.to_hex())
        .collect();
    // All but at most the last promise survives; the torn one never happened.
    let lost = promised.iter().filter(|id| !recovered.contains(id)).count();
    assert!(
        lost <= 1,
        "{lost} promised commits were lost to a torn tail"
    );
}

#[test]
fn c2_a_damaged_record_stops_the_replay_without_losing_what_came_before() {
    // A flipped bit inside a record, rather than a missing tail. The checksum
    // catches it, and everything before it is still good.
    let crash = Crash::new();
    let mut writer = crash.spawn_writer("always");
    crash.wait_for(15, Duration::from_secs(30));
    kill(&mut writer);

    let wal_path = crash.data().join(persist::WAL_FILE);
    let mut bytes = std::fs::read(&wal_path).expect("readable");
    let whole = wal::read(&wal_path).expect("readable").records.len();
    assert!(whole > 5);

    // Corrupt a byte well into the file, but not in the first record.
    let target = bytes.len() / 2;
    bytes[target] ^= 0xff;
    std::fs::write(&wal_path, &bytes).expect("written");

    let replay = wal::read(&wal_path).expect("a damaged log is still readable");
    assert!(
        replay.torn_tail.is_some(),
        "a flipped bit inside a record went unnoticed"
    );
    assert!(
        !replay.records.is_empty(),
        "one damaged record threw away every record before it"
    );
    assert!(replay.records.len() < whole);
}

#[test]
fn c2_the_log_announces_what_it_is() {
    // Magic and version, so a future format change is detectable rather than
    // silently misread as this one.
    let crash = Crash::new();
    let mut writer = crash.spawn_writer("always");
    crash.wait_for(5, Duration::from_secs(30));
    kill(&mut writer);

    let wal_path = crash.data().join(persist::WAL_FILE);
    let bytes = std::fs::read(&wal_path).expect("readable");
    assert_eq!(&bytes[..8], wal::MAGIC);
    assert_eq!(
        u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        wal::VERSION
    );

    // A file that is not a log is refused by name.
    let not_a_wal = crash.dir.path().join("other.bin");
    std::fs::write(&not_a_wal, b"this is something else entirely").expect("written");
    match wal::read(&not_a_wal) {
        Err(wal::WalError::NotAWal { .. }) => {}
        other => panic!("a foreign file was not refused: {other:?}"),
    }

    // A log from a future version is refused rather than guessed at.
    let future = crash.dir.path().join("future.wal");
    let mut header = Vec::new();
    header.extend_from_slice(wal::MAGIC);
    header.extend_from_slice(&(wal::VERSION + 1).to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    std::fs::write(&future, &header).expect("written");
    match wal::read(&future) {
        Err(wal::WalError::UnknownVersion { found, .. }) => assert_eq!(found, wal::VERSION + 1),
        other => panic!("a future format was not refused: {other:?}"),
    }
}

#[test]
fn c2_a_second_process_is_told_who_has_the_directory() {
    // Until the daemon exists, one process at a time — and the one that is
    // turned away should be told why and by whom, not handed an I/O error.
    let crash = Crash::new();
    let mut writer = crash.spawn_writer("always");
    crash.wait_for(3, Duration::from_secs(30));

    let err = persist::Store::open(&crash.data(), Options::default())
        .expect_err("a second process got in");
    let message = err.to_string();
    assert!(message.contains("in use by process"), "{message}");
    assert!(
        message.contains(&writer.id().to_string()),
        "the error does not name the owner: {message}"
    );

    kill(&mut writer);
    // With the owner gone, the lock is free again — no stale-lock cleanup
    // needed, because the operating system released it when the process died.
    let (_, store, _) = persist::Store::open(&crash.data(), Options::default())
        .expect("the directory is available once its owner dies");
    drop(store);
}

#[test]
fn c2_repeated_kills_at_different_moments_all_recover() {
    // One kill lands at one point in the write cycle. Several, at different
    // moments, land in different places — including, with luck, inside a
    // write rather than between two.
    for (round, pause) in [(0, 60u64), (1, 130), (2, 210), (3, 290)] {
        let crash = Crash::new();
        let mut writer = crash.spawn_writer("always");
        std::thread::sleep(Duration::from_millis(pause));
        kill(&mut writer);

        let promised = crash.promised();
        let (db, _) = crash.recover();
        let recovered: Vec<String> = db
            .log("main", None)
            .unwrap()
            .iter()
            .map(|e| e.id.to_hex())
            .collect();
        for id in &promised {
            assert!(
                recovered.contains(id),
                "round {round}: commit {id} was promised and did not survive"
            );
        }
        // Whatever survived, the database is coherent.
        assert_eq!(
            db.read("main").unwrap().seq() as usize,
            recovered.len() - 1,
            "round {round}: the head and the history disagree"
        );
    }
}

#[test]
fn c2_an_empty_data_directory_opens_cleanly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path: &Path = dir.path();
    let (db, store, recovery) =
        persist::Store::open(path, Options::default()).expect("a fresh directory opens");
    assert!(recovery.is_empty());
    assert!(recovery.torn_tail.is_none());
    assert_eq!(db.read("main").unwrap().len(), 0);
    assert!(path.join(persist::WAL_FILE).exists());
    drop(store);
}
