//! Atomic multi-key transactions with real rollback (DESIGN §4.2, §4.3).

use std::collections::BTreeMap;
use std::sync::Arc;

use smallvec::smallvec;

use crate::db::{CommitDraft, Db};
use crate::entry::{validate_key, Entry, Op, Value};
use crate::error::{Error, Result};
use crate::id::CommitId;
use crate::store::Root;

#[derive(Debug, Clone)]
enum Staged {
    Put(Value),
    Delete,
}

/// A set of writes that become visible all at once, or not at all.
///
/// A transaction reads the branch head once, when it is opened, and stages its
/// writes privately. [`Txn::commit`] swaps the branch head to a new commit in
/// one step, and fails with [`Error::Conflict`] if another writer moved the
/// head meanwhile; the caller then retries. Dropping a transaction without
/// committing is a rollback that leaves no commit, no head movement and no
/// change to any sequence counter.
#[derive(Debug)]
pub struct Txn {
    db: Db,
    branch: String,
    base: CommitId,
    base_root: Root,
    base_seq: u64,
    staged: BTreeMap<String, Staged>,
}

impl Txn {
    pub(crate) fn open(db: Db, branch: &str) -> Result<Self> {
        let base = db.head(branch)?;
        let commit = db.commit(base)?;
        Ok(Txn {
            db,
            branch: branch.to_owned(),
            base,
            base_root: commit.root.clone(),
            base_seq: commit.seq,
            staged: BTreeMap::new(),
        })
    }

    /// The branch this transaction writes to.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// The commit this transaction was opened against.
    pub fn base(&self) -> CommitId {
        self.base
    }

    /// Whether anything has been staged yet.
    pub fn is_empty(&self) -> bool {
        self.staged.is_empty()
    }

    /// Stage a write.
    pub fn put(&mut self, key: &str, value: Value) -> Result<()> {
        validate_key(key)?;
        value.validate()?;
        self.db.check_dim(value.embedding.as_ref())?;
        self.staged.insert(key.to_owned(), Staged::Put(value));
        Ok(())
    }

    /// Stage a delete.
    pub fn delete(&mut self, key: &str) -> Result<()> {
        validate_key(key)?;
        self.staged.insert(key.to_owned(), Staged::Delete);
        Ok(())
    }

    /// Read a key as this transaction sees it: staged writes first, then the
    /// branch as it stood when the transaction was opened.
    pub fn get(&self, key: &str) -> Option<Arc<Entry>> {
        match self.staged.get(key) {
            Some(Staged::Delete) => None,
            Some(Staged::Put(v)) => {
                let created = self
                    .base_root
                    .get(key)
                    .map_or(self.base_seq + 1, |e| e.created_seq);
                Some(Arc::new(v.clone().into_entry(self.base_seq + 1, created)))
            }
            None => self.base_root.get(key).cloned(),
        }
    }

    /// Discard every staged write. Equivalent to dropping the transaction, and
    /// spelled out for callers who want the intent on the page.
    pub fn rollback(self) {
        drop(self);
    }

    /// Apply every staged write as one commit and move the branch head.
    ///
    /// A transaction that stages nothing, or whose writes all turn out to be
    /// no-ops, creates no commit and returns the unchanged head.
    pub fn commit(self, message: Option<String>) -> Result<CommitId> {
        let seq = self.base_seq + 1;
        let mut ops: Vec<Op> = Vec::with_capacity(self.staged.len());
        // `staged` is a BTreeMap, so operations are emitted in ascending key
        // order. That ordering is part of what makes commit ids reproducible.
        for (key, staged) in &self.staged {
            match staged {
                Staged::Put(value) => ops.push(Op::Put {
                    key: key.clone(),
                    value: value.clone(),
                }),
                Staged::Delete => {
                    // Deleting a key that is not there is not an event.
                    if self.base_root.get(key).is_some() {
                        ops.push(Op::Delete { key: key.clone() });
                    }
                }
            }
        }
        if ops.is_empty() {
            return Ok(self.base);
        }

        let mut root = self.base_root.clone();
        apply_ops(&mut root, &ops, seq);

        self.db.commit_onto(
            &self.branch,
            self.base,
            CommitDraft {
                parents: smallvec![self.base],
                root,
                seq,
                message,
                ops,
            },
        )
    }
}

/// Apply a change set to a root in place.
///
/// `created_seq` is preserved when a key is overwritten, so it keeps meaning
/// "when this key first appeared on this branch".
pub(crate) fn apply_ops(root: &mut Root, ops: &[Op], seq: u64) {
    for op in ops {
        match op {
            Op::Put { key, value } => {
                let created = root.get(key).map_or(seq, |e| e.created_seq);
                root.insert_mut(
                    key.clone(),
                    Arc::new(value.clone().into_entry(seq, created)),
                );
            }
            Op::Delete { key } | Op::Evict { key } => {
                root.remove_mut(key);
            }
        }
    }
}

impl Drop for Txn {
    fn drop(&mut self) {
        // Nothing to undo: a transaction never touches shared state until
        // `commit` wins the head swap. The impl exists to make that explicit.
    }
}

/// Convenience: run a closure in a transaction, retrying on optimistic
/// conflicts, and commit it.
pub fn with_txn<F>(
    db: &Db,
    branch: &str,
    message: &str,
    retries: usize,
    mut f: F,
) -> Result<CommitId>
where
    F: FnMut(&mut Txn) -> Result<()>,
{
    let mut last = None;
    for _ in 0..retries.max(1) {
        let mut txn = db.begin(branch)?;
        f(&mut txn)?;
        match txn.commit(Some(message.to_owned())) {
            Ok(id) => return Ok(id),
            Err(e @ Error::Conflict { .. }) => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| Error::NoSuchBranch(branch.to_owned())))
}
