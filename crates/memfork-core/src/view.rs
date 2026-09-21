//! Read-only views over a commit (DESIGN §4.2, §4.3).

use std::sync::Arc;

use crate::commit::Commit;
use crate::entry::Entry;
use crate::id::CommitId;
use crate::store::Store;

/// A consistent, immutable view of a branch at one commit.
///
/// A view holds an `Arc` to a commit that can never change, so readers never
/// block and never observe a half-applied change set (DESIGN §4.3). Holding a
/// view keeps that commit alive even if its branch is discarded meanwhile.
#[derive(Debug, Clone)]
pub struct ReadView {
    commit: Arc<Commit>,
}

impl ReadView {
    pub(crate) fn new(commit: Arc<Commit>) -> Self {
        Self { commit }
    }

    /// The commit this view is pinned to.
    pub fn commit(&self) -> &Arc<Commit> {
        &self.commit
    }

    /// The content address of the commit this view is pinned to.
    pub fn commit_id(&self) -> CommitId {
        self.commit.id
    }

    /// The sequence number of the commit this view is pinned to.
    pub fn seq(&self) -> u64 {
        self.commit.seq
    }

    /// Read one key.
    pub fn get(&self, key: &str) -> Option<Arc<Entry>> {
        self.commit.root.get(key).cloned()
    }

    /// Whether a key is present.
    pub fn contains(&self, key: &str) -> bool {
        self.commit.root.get(key).is_some()
    }

    /// How many keys the view holds.
    pub fn len(&self) -> usize {
        self.commit.root.len()
    }

    /// Whether the view holds no keys.
    pub fn is_empty(&self) -> bool {
        self.commit.root.is_empty()
    }

    /// Every key with the given prefix, in ascending key order, at most `limit`
    /// of them. An empty prefix lists everything.
    pub fn list(&self, prefix: &str, limit: Option<usize>) -> Vec<(String, Arc<Entry>)> {
        let it = self
            .commit
            .root
            .range_prefix(prefix)
            .map(|(k, v)| (k.clone(), Arc::clone(v)));
        match limit {
            Some(n) => it.take(n).collect(),
            None => it.collect(),
        }
    }

    /// Every key, in ascending order.
    pub fn keys(&self) -> Vec<String> {
        self.commit.root.iter().map(|(k, _)| k.clone()).collect()
    }
}

/// Summary of one branch (DESIGN §4.1).
#[derive(Debug, Clone)]
pub struct BranchInfo {
    /// The branch name.
    pub name: String,
    /// The commit the branch currently points at.
    pub head: CommitId,
    /// The sequence number of that commit.
    pub seq: u64,
    /// How many keys the branch holds.
    pub key_count: usize,
    /// Whether this is the database's default branch, which cannot be discarded.
    pub is_default: bool,
}

/// How a key differs between two commits (DESIGN §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// Present in `b`, absent in `a`.
    Added,
    /// Present in `a`, absent in `b`.
    Removed,
    /// Present in both with different content.
    Modified,
}

impl ChangeKind {
    /// A one-character marker, in the style of a diff.
    pub fn marker(self) -> char {
        match self {
            ChangeKind::Added => '+',
            ChangeKind::Removed => '-',
            ChangeKind::Modified => '~',
        }
    }
}

/// One entry of a diff, sorted by key with the rest of the diff.
#[derive(Debug, Clone)]
pub struct Change {
    /// The key that differs.
    pub key: String,
    /// How it differs.
    pub kind: ChangeKind,
}
