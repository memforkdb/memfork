//! Commits: the immutable nodes of the history graph (DESIGN §4.1).

use core::fmt;

use smallvec::SmallVec;

use crate::encode::Canonical;
use crate::entry::Op;
use crate::id::CommitId;
use crate::store::Root;

/// One immutable point in a branch's history.
///
/// A commit owns a whole root, but roots are structurally shared, so the
/// marginal cost of a commit is the path it rewrote, not the database.
#[derive(Clone)]
pub struct Commit {
    /// Content address: `blake3(parent ids ‖ message ‖ canonical-encoded ops)`.
    pub id: CommitId,
    /// Zero parents for the genesis commit, one normally, two for a merge.
    pub parents: SmallVec<[CommitId; 2]>,
    /// The state of the database after this commit.
    pub root: Root,
    /// Monotonic along the first-parent chain; the genesis commit is 0.
    pub seq: u64,
    /// Optional human-readable message. Not part of the content address.
    pub message: Option<String>,
    /// This commit's change set, sorted by key.
    pub ops: Vec<Op>,
}

impl Commit {
    /// Compute the content address of a commit from its parents, message and
    /// change set.
    ///
    /// DESIGN §4.1 defines the id as `blake3(parent ids ‖ message ‖
    /// canonical-encoded ops)`. `seq` and wall-clock time take no part, so the
    /// same change with the same message applied to the same parents is the
    /// same commit everywhere.
    ///
    /// The message is encoded as an optional length-prefixed string, so a
    /// commit with no message, one with an empty message and one with a real
    /// message are three different commits.
    pub fn compute_id(parents: &[CommitId], message: Option<&str>, ops: &[Op]) -> CommitId {
        let mut c = Canonical::new();
        c.u64(parents.len() as u64);
        for p in parents {
            c.commit_id(p);
        }
        c.opt_str(message);
        c.u64(ops.len() as u64);
        for op in ops {
            op.encode(&mut c);
        }
        c.finalize()
    }

    /// The first parent, i.e. the previous commit on this branch.
    pub fn first_parent(&self) -> Option<CommitId> {
        self.parents.first().copied()
    }

    /// The message every database's first commit carries.
    pub(crate) const GENESIS_MESSAGE: &'static str = "genesis";

    /// The genesis commit every database starts from. Identical in every
    /// database on every platform, since it has no parents, no operations and
    /// a fixed message.
    pub(crate) fn genesis() -> Self {
        let id = Commit::compute_id(&[], Some(Commit::GENESIS_MESSAGE), &[]);
        Commit {
            id,
            parents: SmallVec::new(),
            root: Root::new_sync(),
            seq: 0,
            message: Some(Commit::GENESIS_MESSAGE.to_owned()),
            ops: Vec::new(),
        }
    }
}

impl fmt::Debug for Commit {
    /// Summarizes rather than dumping the root, which may hold millions of keys.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Commit")
            .field("id", &self.id.to_hex())
            .field(
                "parents",
                &self.parents.iter().map(|p| p.to_hex()).collect::<Vec<_>>(),
            )
            .field("seq", &self.seq)
            .field("message", &self.message)
            .field("ops", &self.ops.len())
            .field("keys", &rpds::RedBlackTreeMap::size(&self.root))
            .finish()
    }
}

/// One line of `log` output (DESIGN §4.2).
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// The commit's content address.
    pub id: CommitId,
    /// The commit's sequence number on this branch.
    pub seq: u64,
    /// The commit's parents.
    pub parents: Vec<CommitId>,
    /// The commit message, if one was given.
    pub message: Option<String>,
    /// How many operations the commit's change set holds.
    pub op_count: usize,
    /// How many keys the database held after this commit.
    pub key_count: usize,
}
