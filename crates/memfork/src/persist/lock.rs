//! Single-writer enforcement, and where to find a running daemon.
//!
//! Two files, deliberately:
//!
//! - **`memfork.lock`** is empty and stays empty. Its only job is to be held
//!   under an advisory exclusive lock for as long as a process owns the data
//!   directory.
//! - **`memfork.endpoint`** is ordinary JSON, never locked, holding the owning
//!   process's id and — once the daemon exists — its port and token.
//!
//! They are separate because on Windows an exclusive `LockFileEx` range
//! blocks *reads* as well as writes, so a client that needed the token would
//! be unable to read the very file proving the daemon was alive. Keeping the
//! lock on an empty file and the facts beside it avoids that entirely, and
//! costs nothing on Unix.
//!
//! Locking uses `std::fs::File::try_lock`, so there is no locking crate to
//! depend on. On Windows it is `LockFileEx`, which blocks *reads* of the
//! locked range as well as writes — verified, not assumed: reading a locked
//! file there fails with "another process has locked a portion of the file".
//! That is precisely why the token does not live in the locked file.
//!
//! **Staleness needs no heuristics.** Every operating system releases an
//! advisory lock when the holding process dies, however it dies. So acquiring
//! the lock *is* proof that the previous owner is gone — no process-liveness
//! guessing, no start-time comparison, and the same answer on all three
//! platforms. An endpoint file left behind by a dead owner is then deleted.
//!
//! On permissions, the two platforms are not equal and this does not pretend
//! otherwise. On Unix the endpoint file is created `0600`. On Windows setting
//! an explicit ACL needs `unsafe` Win32 calls, which this phase does not
//! allow, so the file relies on living inside `%LOCALAPPDATA%`, which is
//! already user-scoped by default. That is weaker: an administrator, or
//! anything running as the user, can read it.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file that is locked, and holds nothing.
pub const LOCK_FILE: &str = "memfork.lock";

/// The file that holds the facts, and is never locked.
pub const ENDPOINT_FILE: &str = "memfork.endpoint";

/// What a running MemFork process publishes about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// Format version of this file.
    pub version: u32,
    /// The process that owns the data directory.
    pub pid: u32,
    /// The daemon's port, once there is a daemon to have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The bearer token a client must present, alongside the port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// When the owner started, in seconds since the epoch. Informational.
    pub started_unix: u64,
    /// Which build of MemFork is running.
    ///
    /// So a client can refuse to talk to a daemon from another version rather
    /// than assume the two agree about the formats they write. Optional only
    /// because an endpoint file written before this field existed has none,
    /// and that absence is itself a mismatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memfork_version: Option<String>,
}

/// The version this build writes.
pub const ENDPOINT_VERSION: u32 = 1;

impl Endpoint {
    /// An endpoint for this process, with no daemon details yet.
    pub fn for_this_process() -> Self {
        Endpoint {
            version: ENDPOINT_VERSION,
            pid: std::process::id(),
            port: None,
            token: None,
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            memfork_version: Some(crate::VERSION.to_owned()),
        }
    }
}

/// Why the data directory could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another live process holds it.
    #[error(
        "this data directory is in use by process {pid}{}.\n\
         Only one process can write to a MemFork data directory at a time.",
        .hint.as_deref().unwrap_or("")
    )]
    InUse {
        /// The owning process id.
        pid: u32,
        /// Anything else worth saying about the owner.
        hint: Option<String>,
    },
    /// Something went wrong with the files themselves.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted.
        context: String,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
}

fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> LockError {
    let context = context.into();
    move |source| LockError::Io { context, source }
}

/// Ownership of a data directory, held until dropped.
#[derive(Debug)]
pub struct DirLock {
    dir: PathBuf,
    /// Holding the handle is what holds the lock; the operating system
    /// releases it when this closes, including when the process dies.
    file: File,
}

impl DirLock {
    /// Take exclusive ownership of a data directory.
    ///
    /// Fails with [`LockError::InUse`], naming the owner, if another live
    /// process has it.
    pub fn acquire(dir: &Path) -> Result<Self, LockError> {
        std::fs::create_dir_all(dir).map_err(io(format!("cannot create {}", dir.display())))?;
        let path = dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io(format!("cannot open {}", path.display())))?;

        match file.try_lock() {
            Ok(()) => {}
            Err(_) => {
                // Someone else holds it. Say who, if they said.
                let existing = read_endpoint(dir);
                return Err(LockError::InUse {
                    pid: existing.as_ref().map_or(0, |e| e.pid),
                    hint: existing
                        .as_ref()
                        .and_then(|e| e.port.map(|p| format!(", serving on 127.0.0.1:{p}"))),
                });
            }
        }

        // The lock was free, so any endpoint file is from an owner that died.
        let _ = std::fs::remove_file(dir.join(ENDPOINT_FILE));

        Ok(DirLock {
            dir: dir.to_path_buf(),
            file,
        })
    }

    /// The directory this lock owns.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Publish what this process is, and where to reach it.
    ///
    /// Written to a temporary file, flushed, then renamed, so a reader sees
    /// either the old endpoint or the new one and never half of either.
    pub fn publish(&self, endpoint: &Endpoint) -> Result<(), LockError> {
        let final_path = self.dir.join(ENDPOINT_FILE);
        let tmp_path = self.dir.join(format!("{ENDPOINT_FILE}.tmp"));

        let mut json = serde_json::to_string_pretty(endpoint).map_err(|e| LockError::Io {
            context: "cannot render the endpoint file".to_owned(),
            source: std::io::Error::other(e),
        })?;
        json.push('\n');

        {
            let mut file = create_private(&tmp_path)?;
            file.write_all(json.as_bytes())
                .map_err(io(format!("cannot write {}", tmp_path.display())))?;
            file.sync_all()
                .map_err(io(format!("cannot flush {}", tmp_path.display())))?;
        }
        std::fs::rename(&tmp_path, &final_path)
            .map_err(io(format!("cannot install {}", final_path.display())))?;
        sync_dir(&self.dir);
        Ok(())
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        // Tidy up so the next process does not have to. If this does not run —
        // because the process was killed — the next `acquire` deletes the
        // endpoint itself, having proved the owner is gone by taking the lock.
        let _ = std::fs::remove_file(self.dir.join(ENDPOINT_FILE));
        let _ = self.file.unlock();
    }
}

/// Create a file only this user can read, as far as the platform allows.
///
/// Unix gets `0600` at creation, so there is no window in which the file is
/// readable. Windows inherits the directory's ACL: inside `%LOCALAPPDATA%`
/// that is already user-scoped, and tightening it further needs `unsafe` Win32
/// calls this phase does not permit. The difference is documented rather than
/// papered over.
fn create_private(path: &Path) -> Result<File, LockError> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(io(format!("cannot create {}", path.display())))
}

/// Flush a directory entry, so a rename survives a power cut.
///
/// A no-op on Windows, which has no handle for a directory that `FlushFileBuffers`
/// accepts; `ReplaceFile`-style atomicity there is provided by the filesystem.
pub fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    {
        if let Ok(handle) = File::open(dir) {
            let _ = handle.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

/// Read the endpoint file, if it is there and makes sense.
pub fn read_endpoint(dir: &Path) -> Option<Endpoint> {
    let text = std::fs::read_to_string(dir.join(ENDPOINT_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether a live process owns this data directory.
///
/// Answered by trying the lock: if it can be taken, nobody holds it. The lock
/// is released immediately, so this only reports a moment ago — which is all
/// any such question can report.
pub fn owner(dir: &Path) -> Option<Endpoint> {
    let path = dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(false)
        .open(&path)
        .ok()?;
    match file.try_lock() {
        Ok(()) => {
            // It was free: nobody owns it, and any endpoint file is stale.
            let _ = file.unlock();
            let _ = std::fs::remove_file(dir.join(ENDPOINT_FILE));
            None
        }
        // Held by someone. Only now is the endpoint file worth believing.
        Err(_) => read_endpoint(dir),
    }
}

/// A fresh bearer token: 32 random bytes, hex encoded.
pub fn new_token() -> Result<String, LockError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| LockError::Io {
        context: "cannot read random bytes for the daemon token".to_owned(),
        source: std::io::Error::other(e),
    })?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_keeps_a_second_owner_out_and_names_the_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let held = DirLock::acquire(dir.path()).expect("first acquire");
        held.publish(&Endpoint::for_this_process())
            .expect("published");

        match DirLock::acquire(dir.path()) {
            Err(LockError::InUse { pid, .. }) => assert_eq!(pid, std::process::id()),
            other => panic!("a second owner got in: {other:?}"),
        }

        drop(held);
        // Once released, the directory is available again.
        DirLock::acquire(dir.path()).expect("acquire after release");
    }

    #[test]
    fn a_released_lock_takes_the_endpoint_file_with_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let held = DirLock::acquire(dir.path()).expect("acquired");
        held.publish(&Endpoint::for_this_process())
            .expect("published");
        assert!(dir.path().join(ENDPOINT_FILE).exists());

        drop(held);
        assert!(
            !dir.path().join(ENDPOINT_FILE).exists(),
            "a stale endpoint was left behind"
        );
    }

    #[test]
    fn an_endpoint_left_by_a_dead_owner_is_ignored_and_removed() {
        // Simulates the file a killed process leaves: the endpoint is there,
        // but nothing holds the lock. Trusting it would send a client at a
        // port nobody is listening on.
        let dir = tempfile::tempdir().expect("tempdir");
        DirLock::acquire(dir.path())
            .expect("acquired")
            .publish(&Endpoint {
                version: ENDPOINT_VERSION,
                pid: 999_999,
                port: Some(1234),
                token: Some("stale".to_owned()),
                started_unix: 0,
                memfork_version: Some(crate::VERSION.to_owned()),
            })
            .expect("published");
        // Put it back by hand: dropping the lock above removed it.
        std::fs::write(
            dir.path().join(ENDPOINT_FILE),
            r#"{"version":1,"pid":999999,"port":1234,"token":"stale","started_unix":0}"#,
        )
        .expect("written");

        assert!(
            owner(dir.path()).is_none(),
            "a stale endpoint was reported as a live owner"
        );
        assert!(
            !dir.path().join(ENDPOINT_FILE).exists(),
            "the stale endpoint was not cleaned up"
        );
    }

    #[test]
    fn a_live_owner_is_reported_with_its_details() {
        let dir = tempfile::tempdir().expect("tempdir");
        let held = DirLock::acquire(dir.path()).expect("acquired");
        let mut endpoint = Endpoint::for_this_process();
        endpoint.port = Some(4321);
        endpoint.token = Some("a-token".to_owned());
        held.publish(&endpoint).expect("published");

        let seen = owner(dir.path()).expect("an owner");
        assert_eq!(seen.pid, std::process::id());
        assert_eq!(seen.port, Some(4321));
        assert_eq!(seen.token.as_deref(), Some("a-token"));
    }

    #[test]
    fn the_endpoint_file_is_readable_while_the_lock_is_held() {
        // The reason the lock and the endpoint are separate files: on Windows
        // an exclusive lock on a range blocks reads of it, so a client could
        // not read a token stored inside the locked file.
        let dir = tempfile::tempdir().expect("tempdir");
        let held = DirLock::acquire(dir.path()).expect("acquired");
        held.publish(&Endpoint::for_this_process())
            .expect("published");

        let text = std::fs::read_to_string(dir.path().join(ENDPOINT_FILE))
            .expect("the endpoint file is readable while the lock is held");
        assert!(text.contains("\"pid\""));
    }

    #[test]
    #[cfg(unix)]
    fn the_endpoint_file_is_private_to_this_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let held = DirLock::acquire(dir.path()).expect("acquired");
        held.publish(&Endpoint::for_this_process())
            .expect("published");

        let mode = std::fs::metadata(dir.path().join(ENDPOINT_FILE))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the token file is readable by others");
    }

    #[test]
    fn the_endpoint_says_which_build_wrote_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let held = DirLock::acquire(dir.path()).expect("acquired");
        held.publish(&Endpoint::for_this_process())
            .expect("published");
        let seen = read_endpoint(dir.path()).expect("an endpoint");
        assert_eq!(seen.memfork_version.as_deref(), Some(crate::VERSION));
    }

    #[test]
    fn tokens_are_long_and_do_not_repeat() {
        let a = new_token().expect("a token");
        let b = new_token().expect("another token");
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn an_unreadable_endpoint_file_is_not_a_crash() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(ENDPOINT_FILE), "not json at all").expect("written");
        assert!(read_endpoint(dir.path()).is_none());
    }
}
