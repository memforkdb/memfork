//! Durability (DESIGN §4.5): the data directory, the lock, the log and the
//! snapshot, tied into one thing a caller can open.
//!
//! The default differs on purpose between the library and the binary. The
//! `memfork-core` library is in-memory unless the caller attaches a journal,
//! because a library should not start writing to someone's disk on its own.
//! The `memfork` binary is the opposite: it persists unless told not to, with
//! `--ephemeral` to turn it off. An agent's memory that forgets everything on
//! restart is not what anyone means by memory.

pub mod datadir;
pub mod lock;
pub mod snapshot;
pub mod wal;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use memfork_core::{Db, Journal, JournalError, Record};
use parking_lot::Mutex;

pub use datadir::{DataDir, Source};
pub use lock::{DirLock, Endpoint, LockError};
pub use snapshot::{Snapshot, DEFAULT_RETENTION};
pub use wal::{FsyncPolicy, WalError};

/// The write-ahead log's file name.
pub const WAL_FILE: &str = "memfork.wal";

/// How to open a data directory.
#[derive(Debug, Clone)]
pub struct Options {
    /// When to flush the log.
    pub fsync: FsyncPolicy,
    /// How many commits per branch to keep (DESIGN §4.4).
    pub retention: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            fsync: FsyncPolicy::default(),
            retention: DEFAULT_RETENTION,
        }
    }
}

/// Why a data directory could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// Somebody else has it.
    #[error(transparent)]
    Lock(#[from] LockError),
    /// The log could not be read or written.
    #[error(transparent)]
    Wal(#[from] WalError),
    /// The snapshot could not be read or written.
    #[error(transparent)]
    Snapshot(#[from] snapshot::SnapshotError),
    /// Replay produced something the engine rejected.
    #[error("the recorded history could not be replayed: {0}")]
    Replay(#[from] memfork_core::Error),
    /// There is nowhere to put the data.
    #[error(transparent)]
    NoDataDir(#[from] datadir::NoDataDir),
}

/// What happened while opening.
#[derive(Debug, Clone, Default)]
pub struct Recovery {
    /// How many records were replayed from the snapshot.
    pub from_snapshot: usize,
    /// How many records were replayed from the log.
    pub from_wal: usize,
    /// Set when the log ended mid-record and was truncated, with the reason.
    pub torn_tail: Option<String>,
}

impl Recovery {
    /// Whether anything at all was recovered.
    pub fn is_empty(&self) -> bool {
        self.from_snapshot == 0 && self.from_wal == 0
    }
}

/// An open, owned data directory.
///
/// Holds the lock for as long as it lives, and is the [`Journal`] the database
/// writes through.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    lock: DirLock,
    wal: Mutex<wal::Wal>,
    options: Options,
}

impl Store {
    /// Open a data directory and rebuild whatever is in it.
    ///
    /// Returns a database with its history replayed and the store attached as
    /// its journal, so every later change is recorded before it is visible.
    pub fn open(dir: &Path, options: Options) -> Result<(Db, Arc<Store>, Recovery), OpenError> {
        let lock = DirLock::acquire(dir)?;
        lock.publish(&Endpoint::for_this_process())?;

        let db = Db::new();
        let mut recovery = Recovery::default();

        // The snapshot is the state at the retention horizon...
        if let Some(snap) = snapshot::read(dir)? {
            recovery.from_snapshot = snap.records.len();
            snapshot::apply(&db, &snap)?;
        }

        // ...and the log is everything since, so time travel reaches back to
        // the horizon exactly as it did before the restart.
        let wal_path = dir.join(WAL_FILE);
        let replay = wal::read(&wal_path)?;
        recovery.from_wal = replay.records.len();
        recovery.torn_tail = replay.torn_tail.clone();
        for record in &replay.records {
            db.apply_record(record)?;
        }
        if replay.torn_tail.is_some() {
            // A process was killed mid-append. The records before it are
            // whole; the partial one never happened.
            wal::truncate(&wal_path, replay.valid_len)?;
        }

        let log = wal::Wal::open(&wal_path, options.fsync)?;
        let store = Arc::new(Store {
            dir: dir.to_path_buf(),
            lock,
            wal: Mutex::new(log),
            options,
        });
        db.set_journal(Some(store.clone() as Arc<dyn Journal>));
        Ok((db, store, recovery))
    }

    /// The directory this store owns.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// How it was opened.
    pub fn options(&self) -> &Options {
        &self.options
    }

    /// Publish daemon details alongside the lock.
    pub fn publish(&self, endpoint: &Endpoint) -> Result<(), LockError> {
        self.lock.publish(endpoint)
    }

    /// Flush the log, whatever the policy.
    pub fn flush(&self) -> Result<(), WalError> {
        self.wal.lock().flush()
    }

    /// How many records have been appended since opening.
    pub fn appended(&self) -> u64 {
        self.wal.lock().appended()
    }

    /// Fold history older than the retention horizon into the snapshot.
    ///
    /// The snapshot moves forward to the horizon and the log keeps everything
    /// after it, so what a restart can reach is unchanged: the retention limit
    /// decides that, not when compaction last ran.
    ///
    /// Returns whether anything was folded.
    pub fn compact(&self, db: &Db) -> Result<bool, OpenError> {
        let retention = self.options.retention;
        let branches = db.branches();
        let needed = branches.iter().any(|b| b.seq > retention);
        if !needed {
            return Ok(false);
        }

        // Rebuild, from the records we have, the state each branch was in at
        // its horizon, and keep everything after it.
        let mut horizon = std::collections::BTreeMap::new();
        for b in &branches {
            horizon.insert(b.name.clone(), b.seq.saturating_sub(retention));
        }

        let existing = snapshot::read(&self.dir)?.unwrap_or_default();
        let replay = wal::read(&self.dir.join(WAL_FILE))?;
        let all: Vec<Record> = existing.records.into_iter().chain(replay.records).collect();

        let (kept, carried) = split_at_horizon(all, &horizon);
        snapshot::write(
            &self.dir,
            &Snapshot {
                records: carried,
                horizon,
            },
        )?;

        // Replace the log with only the records after the horizon. The
        // snapshot is already on disk, so a crash here loses nothing: the next
        // open reads the new snapshot and whichever log survived.
        let log = self.wal.lock();
        let path = log.path().to_path_buf();
        drop(log);
        wal::reset(&path)?;
        let mut fresh = wal::Wal::open(&path, self.options.fsync)?;
        for record in &kept {
            fresh.append(record)?;
        }
        fresh.flush()?;
        *self.wal.lock() = fresh;
        Ok(true)
    }
}

/// Split records into those after each branch's horizon and those at or before.
///
/// A record with no sequence number — a branch being created or discarded, a
/// head moving — is carried into the snapshot, because the state at the
/// horizon depends on it.
fn split_at_horizon(
    records: Vec<Record>,
    horizon: &std::collections::BTreeMap<String, u64>,
) -> (Vec<Record>, Vec<Record>) {
    let mut after = Vec::new();
    let mut at_or_before = Vec::new();
    for record in records {
        let keep_after = match &record {
            Record::Commit { branch, seq, .. } => horizon.get(branch).is_some_and(|h| seq > h),
            _ => false,
        };
        if keep_after {
            after.push(record);
        } else {
            at_or_before.push(record);
        }
    }
    (after, at_or_before)
}

impl Journal for Store {
    fn append(&self, record: &Record) -> Result<(), JournalError> {
        self.wal
            .lock()
            .append(record)
            .map_err(|e| JournalError::new(e.to_string()))
    }
}
