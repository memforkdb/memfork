//! Error type for the MemFork engine.

use crate::id::CommitId;

/// Every fallible engine operation returns this error.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The named branch does not exist.
    #[error("no such branch: {0}")]
    NoSuchBranch(String),

    /// A branch with that name already exists.
    #[error("branch already exists: {0}")]
    BranchExists(String),

    /// `main` is the root of history and cannot be discarded (DESIGN §4.2).
    #[error("the default branch `{0}` cannot be discarded")]
    CannotDiscardDefaultBranch(String),

    /// The referenced commit is not in the graph.
    #[error("no such commit: {0}")]
    NoSuchCommit(CommitId),

    /// A commit id could not be parsed from hex.
    #[error("not a valid commit id: {0}")]
    BadCommitId(String),

    /// No commit on this branch has that sequence number.
    #[error("branch `{branch}` has no commit at seq {seq} (head is at seq {head_seq})")]
    NoSuchSeq {
        /// The branch that was queried.
        branch: String,
        /// The requested sequence number.
        seq: u64,
        /// The sequence number of the branch head.
        head_seq: u64,
    },

    /// Keys are UTF-8 and at most [`crate::MAX_KEY_BYTES`] bytes (DESIGN §4.1).
    #[error("key is {0} bytes, maximum is {max} bytes", max = crate::MAX_KEY_BYTES)]
    KeyTooLong(usize),

    /// Keys may not be empty.
    #[error("key is empty")]
    EmptyKey,

    /// Importance is a probability-like weight in `[0, 1]` (DESIGN §4.1).
    #[error("importance must be in [0, 1] and not NaN, got {0}")]
    BadImportance(f32),

    /// The embedding dimension is fixed per database on first insert (DESIGN §4.1).
    #[error("embedding has {got} dimensions, this database uses {expected}")]
    DimensionMismatch {
        /// The dimension this database was pinned to.
        expected: usize,
        /// The dimension that was supplied.
        got: usize,
    },

    /// An embedding was supplied with no values.
    #[error("embedding is empty")]
    EmptyEmbedding,

    /// The branch head moved while a transaction was open; retry (DESIGN §4.3).
    #[error("branch `{branch}` moved from {expected} to {actual} while the transaction was open")]
    Conflict {
        /// The branch the transaction was opened on.
        branch: String,
        /// The head the transaction was based on.
        expected: CommitId,
        /// The head the branch actually has now.
        actual: CommitId,
    },

    /// A `Fail`-policy merge found keys changed on both sides (DESIGN §4.2).
    #[error("merge has {} conflicting key(s): {}", .keys.len(), .keys.join(", "))]
    MergeConflict {
        /// The conflicting keys, sorted ascending.
        keys: Vec<String>,
    },

    /// The two branches share no common ancestor, so there is nothing to merge against.
    #[error("branches `{from_branch}` and `{into_branch}` have no common ancestor")]
    NoCommonAncestor {
        /// The branch being merged from.
        from_branch: String,
        /// The branch being merged into.
        into_branch: String,
    },

    /// A branch name did not meet the naming rules.
    #[error("invalid branch name `{0}`: use 1-255 bytes of printable, non-whitespace UTF-8")]
    BadBranchName(String),

    /// A change could not be recorded durably, so it did not happen.
    #[error("{0}")]
    Journal(String),

    /// A search query vector did not match the database dimension.
    #[error("query vector has {got} dimensions, this database uses {expected}")]
    BadQueryDimension {
        /// The dimension this database was pinned to.
        expected: usize,
        /// The dimension that was supplied.
        got: usize,
    },
}

/// Convenience alias.
pub type Result<T> = core::result::Result<T, Error>;
