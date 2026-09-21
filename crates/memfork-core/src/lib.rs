//! The MemFork engine: branchable in-memory state.
//!
//! An embedded database for the memory an agent accumulates while it works.
//! Store and recall values, then **fork** the whole database before a risky
//! step, **merge** the fork if the attempt worked, **discard** it if it did
//! not, and **rewind** to how things were at any earlier point.
//!
//! Forking is constant time and constant space whatever the database holds,
//! because branches share structure rather than copying it. Nothing here uses
//! the operating system's `fork()`, copy-on-write pages or a filesystem
//! snapshot: branching is persistent data structures and nothing else, which
//! is why it behaves identically on Windows, macOS and Linux.
//!
//! This crate is the engine on its own — no I/O, no async, no network. The
//! [`memfork`](https://crates.io/crates/memfork) crate builds an MCP server, a
//! shared daemon and a command line on top of it. The project README has the
//! wider picture: <https://github.com/memforkdb/memfork>.
//!
//! # Example
//!
//! ```
//! use memfork_core::{Db, Value};
//!
//! let db = Db::new();
//! db.put("main", "note:1", Value::new("the plan so far"))?;
//!
//! // Fork before doing something that might not work out.
//! db.fork("main", "attempt")?;
//! db.put("attempt", "note:1", Value::new("a risky rewrite"))?;
//!
//! // The parent branch cannot see the attempt.
//! let on_main = db.get("main", "note:1")?;
//! assert_eq!(on_main.map(|e| e.value.clone()), Some("the plan so far".into()));
//!
//! // It did not work out: throw the whole attempt away.
//! db.discard("attempt")?;
//! # Ok::<(), memfork_core::Error>(())
//! ```
//!
//! Swap [`Db::discard`] for [`Db::merge`] and the change comes back to the
//! parent instead, resolved key by key against the point the branches
//! diverged.
//!
//! # What it guarantees
//!
//! - **Forking costs the same whatever it holds.** [`Db::fork`] copies a
//!   pointer, and the two branches share everything they have in common until
//!   one of them changes.
//! - **A fork is invisible until it is merged.** Writes on a branch cannot be
//!   seen from the branch it came from, and [`Db::discard`] leaves that branch
//!   exactly as it was.
//! - **You can read the past.** [`Db::at`] reads a branch as of any retained
//!   commit, and [`Db::fork_at`] branches from there.
//! - **A dropped transaction leaves nothing behind.** A [`Txn`] that is not
//!   committed moves no head, writes no commit and advances no counter.
//! - **The same operations give the same answers everywhere.** Commit ids are
//!   `blake3(parents ‖ message ‖ canonically encoded ops)`, and search breaks
//!   ties on the key, so ids and orderings match across machines and runs.
//!
//! # Where the data goes
//!
//! Nowhere, unless you ask. This crate keeps everything in memory and performs
//! no I/O of its own; durability is a [`Journal`] the caller supplies, which is
//! how the `memfork` binary writes a checksummed log without the engine ever
//! touching a file. Long-lived memory also needs a limit, so
//! [`Db::enforce_budget`] evicts by importance and recency together and records
//! what it dropped as [`Op::Evict`] rather than as an ordinary delete.
//!
//! The design is documented in
//! [`docs/DESIGN.md`](https://github.com/memforkdb/memfork/blob/main/docs/DESIGN.md).

#![warn(missing_docs)]

mod access;
mod commit;
mod db;
mod encode;
mod entry;
mod error;
mod evict;
mod id;
pub mod journal;
mod merge;
mod search;
mod store;
mod txn;
mod view;

pub use commit::{Commit, LogEntry};
pub use db::{validate_branch_name, Db, DEFAULT_BRANCH};
pub use entry::{validate_key, Entry, Op, Value, DEFAULT_IMPORTANCE, MAX_KEY_BYTES, WRITTEN_BY};
pub use error::{Error, Result};
pub use evict::{entry_size, score, Evicted, EvictionConfig, OnEvict};
pub use id::CommitId;
pub use journal::{DecodeError, Journal, JournalError, Record};
pub use merge::{MergeKind, MergeOutcome, MergePolicy};
pub use search::{cosine, SearchHit};
pub use store::{Root, Store};
pub use txn::{with_txn, Txn};
pub use view::{BranchInfo, Change, ChangeKind, ReadView};

/// The version of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The README is a contract, so its examples are compiled
/// and run as doctests rather than left to rot.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeExamples;

/// And the same for the one at the root of the repository, which is the one
/// most people read.
#[cfg(doctest)]
#[doc = include_str!("../../../README.md")]
pub struct ProjectReadmeExamples;
