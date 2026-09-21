//! Snapshots, and the retention horizon they sit at (DESIGN §4.4, §4.5).
//!
//! A snapshot is not "the state now". It is **the state at the retention
//! horizon** — the oldest commit still being kept — and the write-ahead log
//! holds every commit after it. Recovery replays all of them, so `at`, `log`
//! and `fork_at` behave exactly the same before and after a restart, back to
//! the retention limit.
//!
//! Snapshotting the present would have been simpler and would have quietly
//! destroyed time travel across restarts, which is one of the four things
//! MemFork exists to do.
//!
//! The file is written to a temporary name, flushed, renamed over the old one,
//! and on Unix the directory entry is flushed too. On Windows that last step
//! has no equivalent and is a documented no-op.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use memfork_core::{Db, Record};

use super::lock::sync_dir;

/// The snapshot file's name.
pub const SNAPSHOT_FILE: &str = "memfork.snapshot";

/// Magic at the head of a snapshot.
pub const MAGIC: &[u8; 8] = b"MEMFSNAP";

/// The format version this build writes and reads.
pub const VERSION: u32 = 1;

/// How many commits per branch are kept by default (DESIGN §4.4).
pub const DEFAULT_RETENTION: u64 = 10_000;

/// Why a snapshot could not be used.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// The file could not be read or written.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted.
        context: String,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// The file is not a MemFork snapshot.
    #[error("{path} is not a MemFork snapshot (wrong magic bytes)")]
    NotASnapshot {
        /// The file that was opened.
        path: PathBuf,
    },
    /// The snapshot is of a version this build does not know.
    #[error(
        "{path} is a MemFork snapshot of format version {found}; \
         this build reads version {VERSION}"
    )]
    UnknownVersion {
        /// The file that was opened.
        path: PathBuf,
        /// The version it declares.
        found: u32,
    },
    /// The contents did not decode.
    #[error("the snapshot at {path} could not be read: {reason}")]
    Corrupt {
        /// The file that was opened.
        path: PathBuf,
        /// What went wrong.
        reason: String,
    },
}

fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> SnapshotError {
    let context = context.into();
    move |source| SnapshotError::Io { context, source }
}

/// A snapshot: the records needed to rebuild the state at the horizon.
///
/// Stored as journal records rather than as a bespoke state format, so there
/// is one encoding to get right and one decoder to trust. Replaying a snapshot
/// and replaying a log are the same operation.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Snapshot {
    /// The records that rebuild the state, in order.
    pub records: Vec<Record>,
    /// The sequence number each branch had reached at the horizon.
    pub horizon: BTreeMap<String, u64>,
}

/// Write a snapshot, atomically.
pub fn write(dir: &Path, snapshot: &Snapshot) -> Result<(), SnapshotError> {
    let final_path = dir.join(SNAPSHOT_FILE);
    let tmp_path = dir.join(format!("{SNAPSHOT_FILE}.tmp"));

    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&(snapshot.horizon.len() as u32).to_le_bytes());
    for (branch, seq) in &snapshot.horizon {
        let name = branch.as_bytes();
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(&seq.to_le_bytes());
    }
    bytes.extend_from_slice(&(snapshot.records.len() as u64).to_le_bytes());
    for record in &snapshot.records {
        let payload = memfork_core::journal::encode(record);
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&payload);
    }
    // One checksum over the whole file: a snapshot is written or it is not,
    // so there is no torn tail to recover from — only a file to reject.
    let digest = blake3::hash(&bytes);
    bytes.extend_from_slice(&digest.as_bytes()[..8]);

    {
        let mut file = std::fs::File::create(&tmp_path)
            .map_err(io(format!("cannot create {}", tmp_path.display())))?;
        file.write_all(&bytes)
            .map_err(io(format!("cannot write {}", tmp_path.display())))?;
        file.sync_all()
            .map_err(io(format!("cannot flush {}", tmp_path.display())))?;
    }
    std::fs::rename(&tmp_path, &final_path)
        .map_err(io(format!("cannot install {}", final_path.display())))?;
    // So the rename itself survives a power cut, not just the bytes it points
    // at. No-op on Windows.
    sync_dir(dir);
    Ok(())
}

/// Read a snapshot, if there is one.
pub fn read(dir: &Path) -> Result<Option<Snapshot>, SnapshotError> {
    let path = dir.join(SNAPSHOT_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(format!("cannot read {}", path.display()))(e)),
    };

    if bytes.len() < 16 || &bytes[..8] != MAGIC {
        return Err(SnapshotError::NotASnapshot { path });
    }
    let found = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if found != VERSION {
        return Err(SnapshotError::UnknownVersion { path, found });
    }

    let body = &bytes[..bytes.len() - 8];
    let stored = &bytes[bytes.len() - 8..];
    if &blake3::hash(body).as_bytes()[..8] != stored {
        return Err(SnapshotError::Corrupt {
            path,
            reason: "the checksum does not match; the file is incomplete or damaged".to_owned(),
        });
    }

    let mut r = Cursor {
        bytes: body,
        pos: 12,
        path: &path,
    };
    let branch_count = r.u32()?;
    let mut horizon = BTreeMap::new();
    for _ in 0..branch_count {
        let name = r.string()?;
        let seq = r.u64()?;
        horizon.insert(name, seq);
    }
    let record_count = r.u64()?;
    let mut records = Vec::with_capacity(record_count.min(1024) as usize);
    for _ in 0..record_count {
        let len = r.u32()? as usize;
        let payload = r.take(len)?;
        let record =
            memfork_core::journal::decode(payload).map_err(|e| SnapshotError::Corrupt {
                path: path.clone(),
                reason: format!("a record in the snapshot could not be read: {e}"),
            })?;
        records.push(record);
    }

    Ok(Some(Snapshot { records, horizon }))
}

/// Remove a snapshot, if there is one.
pub fn remove(dir: &Path) -> Result<(), SnapshotError> {
    match std::fs::remove_file(dir.join(SNAPSHOT_FILE)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io("cannot remove the snapshot")(e)),
    }
}

/// Rebuild a database from a snapshot.
pub fn apply(db: &Db, snapshot: &Snapshot) -> Result<(), memfork_core::Error> {
    for record in &snapshot.records {
        db.apply_record(record)?;
    }
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
    path: &'a Path,
}

impl Cursor<'_> {
    fn short(&self) -> SnapshotError {
        SnapshotError::Corrupt {
            path: self.path.to_path_buf(),
            reason: format!("the file ends unexpectedly at offset {}", self.pos),
        }
    }

    fn take(&mut self, n: usize) -> Result<&[u8], SnapshotError> {
        if self.bytes.len() - self.pos < n {
            return Err(self.short());
        }
        let out = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn u32(&mut self) -> Result<u32, SnapshotError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, SnapshotError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    fn string(&mut self) -> Result<String, SnapshotError> {
        let len = self.u32()? as usize;
        let b = self.take(len)?;
        String::from_utf8(b.to_vec()).map_err(|_| SnapshotError::Corrupt {
            path: self.path.to_path_buf(),
            reason: "a branch name in the snapshot is not valid UTF-8".to_owned(),
        })
    }
}
