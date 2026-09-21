//! The engine: branches, commits and the operations over them (DESIGN §4.2, §4.3).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use smallvec::SmallVec;

use crate::access::AccessLog;
use crate::commit::{Commit, LogEntry};
use crate::entry::{Entry, Op, Value};
use crate::error::{Error, Result};
use crate::evict::{self, EvictionConfig, OnEvict};
use crate::id::CommitId;
use crate::journal::{Journal, Record};
use crate::search::SearchHit;
use crate::store::{Root, Store};
use crate::txn::Txn;
use crate::view::{BranchInfo, Change, ChangeKind, ReadView};

/// The default branch name.
pub const DEFAULT_BRANCH: &str = "main";

/// How many times an auto-commit retries after losing an optimistic race.
///
/// Auto-commits serialize on the branch's writer lock, so the only thing that
/// can move the head underneath one is an explicit transaction. The bound is
/// generous because losing this race means another writer made progress.
const AUTO_COMMIT_RETRIES: usize = 1024;

/// How many discarded branches [`Db::discarded`] remembers.
pub const DISCARDS_REMEMBERED: usize = 100;

/// A branch that was discarded: what is left to say about it once its commits
/// are gone.
///
/// Discarding frees every commit only that branch could reach, so the commit
/// graph keeps no trace of it (DESIGN §4.2). This is the one thing that does,
/// for display: the name, where it forked from and how far it got. It is
/// rebuilt by replaying the log, because the log already records both the
/// branch's creation and its discard, and it takes no part in any commit id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discarded {
    /// The name it had.
    pub name: String,
    /// The commit it was forked from, which is still on some other branch.
    pub forked_at: CommitId,
    /// How many commits it had made on top of that.
    pub commits: u64,
}

/// A branch: a mutable pointer into the immutable commit graph.
///
/// Each branch carries its own locks, so writers on different branches never
/// wait on each other (DESIGN §4.3).
#[derive(Debug)]
pub(crate) struct BranchHandle {
    /// The head pointer. Held only for the instant of a compare-and-swap, so a
    /// reader never waits behind a writer that is still building a commit.
    pub(crate) head: Mutex<CommitId>,
    /// Serializes the engine's own read-modify-write operations — auto-commits
    /// and merges — on this branch.
    ///
    /// DESIGN §4.3 puts one writer on a branch, and an explicit [`Txn`] stays
    /// purely optimistic as the spec describes. But a caller that ignores that
    /// and writes from several threads at once should get correct behaviour
    /// rather than a retry budget that can run out, so the operations the
    /// engine drives end to end take a turn here instead of spinning.
    pub(crate) writer: Mutex<()>,
    /// The commit this branch was created at: the fork point, or genesis for
    /// the default branch.
    pub(crate) forked_at: CommitId,
}

pub(crate) struct DbInner {
    /// Every commit currently reachable from some branch.
    pub(crate) commits: RwLock<BTreeMap<CommitId, Arc<Commit>>>,
    /// Branch name to head pointer. Only creating and discarding branches
    /// takes the write lock.
    pub(crate) branches: RwLock<BTreeMap<String, Arc<BranchHandle>>>,
    /// The embedding dimension, pinned on the first insert that carries one.
    pub(crate) dim: RwLock<Option<usize>>,
    pub(crate) default_branch: String,
    /// Where changes are recorded before they become visible, if anywhere.
    pub(crate) journal: RwLock<Option<Arc<dyn Journal>>>,
    /// The memory budget, if one is set. `None` means never evict, which is
    /// also what keeps reads free of any bookkeeping.
    pub(crate) eviction: RwLock<Option<EvictionConfig>>,
    /// Where evicted entries go first.
    pub(crate) on_evict: RwLock<Option<OnEvict>>,
    /// Reading history, outside the commit chain (DESIGN §4.4).
    pub(crate) access: AccessLog,
    /// The most recently discarded branches, oldest first.
    pub(crate) discarded: Mutex<VecDeque<Discarded>>,
}

impl core::fmt::Debug for DbInner {
    /// Hand-written because the eviction hook is a closure, which cannot
    /// derive `Debug`. Reports what it is rather than dumping the graph.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DbInner")
            .field("default_branch", &self.default_branch)
            .field("branches", &self.branches.read().len())
            .field("commits", &self.commits.read().len())
            .field("embedding_dim", &*self.dim.read())
            .field("journalled", &self.journal.read().is_some())
            .field("eviction", &*self.eviction.read())
            .field("on_evict", &self.on_evict.read().is_some())
            .finish()
    }
}

/// An embedded, in-memory database with Git semantics.
///
/// `Db` is a handle: cloning it is cheap and every clone refers to the same
/// database. It is `Send + Sync`, and readers never block writers.
///
/// ```
/// use memfork_core::{Db, MergePolicy, Value};
///
/// let db = Db::new();
/// db.put("main", "plan:1", Value::new("draft"))?;
///
/// // Fork before a risky step; the parent cannot see the attempt.
/// db.fork("main", "try")?;
/// db.put("try", "plan:1", Value::new("rewritten"))?;
/// assert_eq!(db.get("main", "plan:1")?.map(|e| e.value.clone()), Some("draft".into()));
///
/// // It worked: fold it back in.
/// db.merge("try", "main", MergePolicy::Fail)?;
/// assert_eq!(db.get("main", "plan:1")?.map(|e| e.value.clone()), Some("rewritten".into()));
/// # Ok::<(), memfork_core::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct Db {
    pub(crate) inner: Arc<DbInner>,
}

/// Everything needed to build a commit, gathered so that the transaction and
/// the merge paths hand the engine one value rather than a row of positional
/// arguments.
#[derive(Debug)]
pub(crate) struct CommitDraft {
    pub(crate) parents: SmallVec<[CommitId; 2]>,
    pub(crate) root: Root,
    pub(crate) seq: u64,
    pub(crate) message: Option<String>,
    pub(crate) ops: Vec<Op>,
}

impl Default for Db {
    fn default() -> Self {
        Self::new()
    }
}

impl Db {
    /// Create a database with a single empty branch, `main`.
    pub fn new() -> Self {
        Self::build(DEFAULT_BRANCH)
    }

    /// Create a database whose default branch has a name other than `main`.
    pub fn with_default_branch(name: &str) -> Result<Self> {
        validate_branch_name(name)?;
        Ok(Self::build(name))
    }

    fn build(name: &str) -> Self {
        let genesis = Arc::new(Commit::genesis());
        let mut commits = BTreeMap::new();
        commits.insert(genesis.id, Arc::clone(&genesis));
        let mut branches = BTreeMap::new();
        branches.insert(
            name.to_owned(),
            Arc::new(BranchHandle {
                head: Mutex::new(genesis.id),
                writer: Mutex::new(()),
                forked_at: genesis.id,
            }),
        );
        Db {
            inner: Arc::new(DbInner {
                commits: RwLock::new(commits),
                branches: RwLock::new(branches),
                dim: RwLock::new(None),
                default_branch: name.to_owned(),
                journal: RwLock::new(None),
                eviction: RwLock::new(None),
                on_evict: RwLock::new(None),
                access: AccessLog::new(),
                discarded: Mutex::new(VecDeque::new()),
            }),
        }
    }

    /// The name of the branch that cannot be discarded.
    pub fn default_branch(&self) -> &str {
        &self.inner.default_branch
    }

    // ---- durability (DESIGN §4.5) --------------------------------------------

    /// Record every change to a journal before it becomes visible.
    ///
    /// The engine does no I/O of its own: the journal decides where the bytes
    /// go and how durable they are. Attached after recovery rather than at
    /// construction, because replay must not re-record what it is replaying.
    ///
    /// A library caller gets no journal unless they ask. The `memfork` binary
    /// attaches one by default, because an agent's memory that forgets
    /// everything on restart is not what anyone means by memory.
    pub fn set_journal(&self, journal: Option<Arc<dyn Journal>>) {
        *self.inner.journal.write() = journal;
    }

    /// Whether changes are being recorded.
    pub fn is_journalled(&self) -> bool {
        self.inner.journal.read().is_some()
    }

    fn record(&self, record: &Record) -> Result<()> {
        match self.inner.journal.read().as_ref() {
            None => Ok(()),
            Some(journal) => journal.append(record).map_err(|e| Error::Journal(e.0)),
        }
    }

    // ---- eviction (DESIGN §4.4) ----------------------------------------------

    /// Set a memory budget, or remove one.
    ///
    /// With no budget nothing is ever evicted and reads record nothing, so
    /// they stay lock-free. Setting one turns on the reading history that
    /// scoring needs, which costs a brief per-branch lock per read.
    pub fn set_eviction(&self, config: Option<EvictionConfig>) {
        *self.inner.eviction.write() = config;
    }

    /// The memory budget, if one is set.
    pub fn eviction(&self) -> Option<EvictionConfig> {
        *self.inner.eviction.read()
    }

    /// Be told about entries before they are evicted, so they can be kept
    /// somewhere else.
    pub fn on_evict(&self, hook: Option<OnEvict>) {
        *self.inner.on_evict.write() = hook;
    }

    /// Note that keys were read, if anything is scoring reads.
    pub(crate) fn note_read(&self, branch: &str, seq: u64, keys: impl Iterator<Item = String>) {
        if self.inner.eviction.read().is_some() {
            self.inner.access.record(branch, seq, keys);
        }
    }

    /// Evict from a branch until it is under budget, as its own commit.
    ///
    /// Called after a commit lands. Does nothing without a budget, and nothing
    /// when the branch is already under it, so the common path costs one
    /// lock-free read of the configuration.
    pub fn enforce_budget(&self, branch: &str) -> Result<Option<CommitId>> {
        let Some(config) = *self.inner.eviction.read() else {
            return Ok(None);
        };
        // Retry for the same reason a transaction does: another writer may
        // land a commit between choosing and committing.
        for _ in 0..AUTO_COMMIT_RETRIES {
            let head_id = self.head(branch)?;
            let head = self.commit(head_id)?;
            let access = self.inner.access.snapshot(branch);
            let chosen = evict::choose(head.root.iter(), head.seq, &access, &config);
            if chosen.is_empty() {
                return Ok(None);
            }

            // The hook sees them before they go, so it can keep what matters.
            if let Some(hook) = self.inner.on_evict.read().as_ref() {
                hook(&chosen);
            }

            let ops: Vec<Op> = chosen
                .iter()
                .map(|e| Op::Evict { key: e.key.clone() })
                .collect();
            let seq = head.seq + 1;
            let mut root = head.root.clone();
            crate::txn::apply_ops(&mut root, &ops, seq);

            let message = Some(format!("evict {} entr(ies) over budget", ops.len()));
            match self.commit_onto(
                branch,
                head_id,
                CommitDraft {
                    parents: smallvec::smallvec![head_id],
                    root,
                    seq,
                    message,
                    ops,
                },
            ) {
                Ok(id) => {
                    let gone: BTreeSet<String> = chosen.into_iter().map(|e| e.key).collect();
                    self.inner.access.retain(branch, |k| !gone.contains(k));
                    return Ok(Some(id));
                }
                Err(Error::Conflict { .. }) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    /// The embedding dimension this database is pinned to, if any entry has
    /// carried an embedding yet (DESIGN §4.1).
    pub fn embedding_dim(&self) -> Option<usize> {
        *self.inner.dim.read()
    }

    // ---- branch and commit lookup ------------------------------------------

    pub(crate) fn branch_handle(&self, branch: &str) -> Result<Arc<BranchHandle>> {
        self.inner
            .branches
            .read()
            .get(branch)
            .map(Arc::clone)
            .ok_or_else(|| Error::NoSuchBranch(branch.to_owned()))
    }

    /// The commit a branch currently points at.
    pub fn head(&self, branch: &str) -> Result<CommitId> {
        Ok(*self.branch_handle(branch)?.head.lock())
    }

    /// Look up a commit by id.
    pub fn commit(&self, id: CommitId) -> Result<Arc<Commit>> {
        self.inner
            .commits
            .read()
            .get(&id)
            .map(Arc::clone)
            .ok_or(Error::NoSuchCommit(id))
    }

    /// Whether a branch exists.
    pub fn has_branch(&self, branch: &str) -> bool {
        self.inner.branches.read().contains_key(branch)
    }

    /// Every branch, in ascending name order.
    pub fn branches(&self) -> Vec<BranchInfo> {
        let names: Vec<(String, Arc<BranchHandle>)> = self
            .inner
            .branches
            .read()
            .iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect();
        let commits = self.inner.commits.read();
        names
            .into_iter()
            .filter_map(|(name, handle)| {
                let head = *handle.head.lock();
                let commit = commits.get(&head)?;
                Some(BranchInfo {
                    is_default: name == self.inner.default_branch,
                    name,
                    head,
                    seq: commit.seq,
                    key_count: commit.root.len(),
                    forked_at: handle.forked_at,
                })
            })
            .collect()
    }

    /// A consistent view of a branch as it is right now (DESIGN §4.3).
    pub fn read(&self, branch: &str) -> Result<ReadView> {
        let head = self.head(branch)?;
        Ok(ReadView::new(self.commit(head)?))
    }

    /// A view of a branch as it was after commit number `seq` (DESIGN §4.2).
    ///
    /// Sequence numbers are monotonic along a branch's first-parent chain, so
    /// `seq` names exactly one commit: 0 is the empty genesis state and
    /// `head.seq` is the present.
    pub fn at(&self, branch: &str, seq: u64) -> Result<ReadView> {
        let head = self.commit(self.head(branch)?)?;
        if seq > head.seq {
            return Err(Error::NoSuchSeq {
                branch: branch.to_owned(),
                seq,
                head_seq: head.seq,
            });
        }
        let mut cur = head;
        while cur.seq > seq {
            let parent = cur.first_parent().ok_or_else(|| Error::NoSuchSeq {
                branch: branch.to_owned(),
                seq,
                head_seq: cur.seq,
            })?;
            cur = self.commit(parent)?;
        }
        Ok(ReadView::new(cur))
    }

    // ---- reads -------------------------------------------------------------

    /// Read one key from a branch head.
    pub fn get(&self, branch: &str, key: &str) -> Result<Option<Arc<Entry>>> {
        let view = self.read(branch)?;
        let found = view.get(key);
        if found.is_some() {
            self.note_read(branch, view.seq(), std::iter::once(key.to_owned()));
        }
        Ok(found)
    }

    /// List the keys of a branch that start with `prefix`, in ascending order.
    pub fn list(
        &self,
        branch: &str,
        prefix: &str,
        limit: Option<usize>,
    ) -> Result<Vec<(String, Arc<Entry>)>> {
        let view = self.read(branch)?;
        let found = view.list(prefix, limit);
        self.note_read(branch, view.seq(), found.iter().map(|(k, _)| k.clone()));
        Ok(found)
    }

    /// Exact brute-force cosine similarity search (DESIGN §4.2).
    pub fn search(
        &self,
        branch: &str,
        query: &[f32],
        k: usize,
        prefix: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        if let Some(expected) = self.embedding_dim() {
            if !query.is_empty() && query.len() != expected {
                return Err(Error::BadQueryDimension {
                    expected,
                    got: query.len(),
                });
            }
        }
        let view = self.read(branch)?;
        let hits = crate::search::search(&view, query, k, prefix);
        self.note_read(branch, view.seq(), hits.iter().map(|h| h.key.clone()));
        Ok(hits)
    }

    /// The first-parent history of a branch, newest first.
    pub fn log(&self, branch: &str, limit: Option<usize>) -> Result<Vec<LogEntry>> {
        let mut out = Vec::new();
        let mut cur = Some(self.head(branch)?);
        while let Some(id) = cur {
            if limit.is_some_and(|n| out.len() >= n) {
                break;
            }
            let commit = self.commit(id)?;
            out.push(LogEntry {
                id: commit.id,
                seq: commit.seq,
                parents: commit.parents.to_vec(),
                message: commit.message.clone(),
                op_count: commit.ops.len(),
                key_count: commit.root.len(),
            });
            cur = commit.first_parent();
        }
        Ok(out)
    }

    /// Resolve a branch name or a full hex commit id to a commit.
    pub fn resolve(&self, reference: &str) -> Result<Arc<Commit>> {
        if self.has_branch(reference) {
            return self.commit(self.head(reference)?);
        }
        match CommitId::from_hex(reference) {
            Ok(id) => self.commit(id),
            Err(_) => Err(Error::NoSuchBranch(reference.to_owned())),
        }
    }

    /// Key-level differences between two branches or commits, sorted by key.
    pub fn diff(&self, a: &str, b: &str) -> Result<Vec<Change>> {
        let ca = self.resolve(a)?;
        let cb = self.resolve(b)?;
        Ok(diff_roots(&ca.root, &cb.root))
    }

    // ---- writes ------------------------------------------------------------

    /// Open a transaction on a branch (DESIGN §4.2).
    ///
    /// The transaction records the branch head it was based on. Dropping it
    /// without committing is a rollback that leaves no trace.
    pub fn begin(&self, branch: &str) -> Result<Txn> {
        Txn::open(self.clone(), branch)
    }

    /// Write one key as a single-operation transaction.
    pub fn put(&self, branch: &str, key: &str, value: Value) -> Result<CommitId> {
        let id = self.auto_commit(branch, &format!("put {key}"), |txn| {
            txn.put(key, value.clone())
        })?;
        self.enforce_budget(branch)?;
        Ok(id)
    }

    /// Remove one key as a single-operation transaction.
    pub fn delete(&self, branch: &str, key: &str) -> Result<CommitId> {
        self.auto_commit(branch, &format!("delete {key}"), |txn| txn.delete(key))
    }

    /// Rebuild state from journal records, in the order they were written.
    ///
    /// Commit ids are recomputed, never read from the record, so a replay that
    /// produces the original ids is evidence the engine is deterministic
    /// rather than evidence that bytes were copied faithfully.
    pub fn apply_record(&self, record: &Record) -> Result<()> {
        match record {
            Record::BranchCreated { name, at } => {
                if self.has_branch(name) {
                    // Replaying onto a database that already has the branch is
                    // not an error: the default branch always exists.
                    return Ok(());
                }
                let _ = self.commit(*at)?;
                self.create_branch_unrecorded(name, *at)?;
                Ok(())
            }
            Record::BranchDiscarded { name } => {
                if !self.has_branch(name) {
                    return Ok(());
                }
                self.discard(name)
            }
            Record::HeadMoved { branch, to } => {
                let _ = self.commit(*to)?;
                let handle = self.branch_handle(branch)?;
                *handle.head.lock() = *to;
                Ok(())
            }
            Record::Commit {
                branch,
                parents,
                seq,
                message,
                ops,
            } => {
                let base = parents
                    .first()
                    .copied()
                    .ok_or_else(|| Error::Journal("a commit record has no parent".to_owned()))?;
                let base_commit = self.commit(base)?;
                let mut root = base_commit.root.clone();
                crate::txn::apply_ops(&mut root, ops, *seq);

                let id = Commit::compute_id(parents, message.as_deref(), ops);
                let commit = Arc::new(Commit {
                    id,
                    parents: parents.clone(),
                    root,
                    seq: *seq,
                    message: message.clone(),
                    ops: ops.clone(),
                });
                self.inner.commits.write().entry(id).or_insert(commit);
                if !self.has_branch(branch) {
                    self.create_branch_unrecorded(branch, id)?;
                } else {
                    let handle = self.branch_handle(branch)?;
                    *handle.head.lock() = id;
                }
                Ok(())
            }
        }
    }

    /// Run a closure in a transaction, retrying if another writer wins the race.
    ///
    /// Takes the branch's writer lock, so concurrent auto-commits take turns
    /// rather than racing each other; the retry loop is then only for a
    /// concurrent explicit transaction.
    fn auto_commit<F>(&self, branch: &str, message: &str, mut f: F) -> Result<CommitId>
    where
        F: FnMut(&mut Txn) -> Result<()>,
    {
        let handle = self.branch_handle(branch)?;
        let _writing = handle.writer.lock();
        let mut last = None;
        for _ in 0..AUTO_COMMIT_RETRIES {
            let mut txn = self.begin(branch)?;
            f(&mut txn)?;
            match txn.commit(Some(message.to_owned())) {
                Ok(id) => return Ok(id),
                Err(e @ Error::Conflict { .. }) => {
                    last = Some(e);
                    // The winner is mid-commit; give it the core rather than
                    // burning the retry budget against it.
                    std::thread::yield_now();
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| Error::NoSuchBranch(branch.to_owned())))
    }

    // ---- branching ---------------------------------------------------------

    /// Point a new branch at another branch's head (DESIGN §4.2).
    ///
    /// Constant time and constant space whatever the database holds: the new
    /// branch is one pointer into the same immutable commit, and the two
    /// branches go on sharing every node until one of them is written to.
    pub fn fork(&self, from: &str, new_branch: &str) -> Result<CommitId> {
        let head = self.head(from)?;
        self.create_branch(new_branch, head)?;
        // The new branch starts from what its parent had read (DESIGN §4.4):
        // the entries the parent valued are the ones the fork inherits.
        self.inner.access.fork(from, new_branch);
        Ok(head)
    }

    /// Point a new branch at a past commit of another branch (DESIGN §4.2).
    pub fn fork_at(&self, from: &str, seq: u64, new_branch: &str) -> Result<CommitId> {
        let head = self.at(from, seq)?.commit_id();
        self.create_branch(new_branch, head)?;
        Ok(head)
    }

    /// Point a new branch at an arbitrary commit.
    pub fn branch_from_commit(&self, new_branch: &str, at: CommitId) -> Result<CommitId> {
        // Fail before mutating anything if the commit is unknown.
        let _ = self.commit(at)?;
        self.create_branch(new_branch, at)?;
        Ok(at)
    }

    pub(crate) fn create_branch(&self, name: &str, head: CommitId) -> Result<()> {
        validate_branch_name(name)?;
        if self.has_branch(name) {
            return Err(Error::BranchExists(name.to_owned()));
        }
        self.record(&Record::BranchCreated {
            name: name.to_owned(),
            at: head,
        })?;
        self.create_branch_unrecorded(name, head)
    }

    /// Create a branch without recording it, which is what replay needs: the
    /// record being replayed is already on disk.
    fn create_branch_unrecorded(&self, name: &str, head: CommitId) -> Result<()> {
        validate_branch_name(name)?;
        let mut branches = self.inner.branches.write();
        if branches.contains_key(name) {
            return Err(Error::BranchExists(name.to_owned()));
        }
        branches.insert(
            name.to_owned(),
            Arc::new(BranchHandle {
                head: Mutex::new(head),
                writer: Mutex::new(()),
                forked_at: head,
            }),
        );
        Ok(())
    }

    /// Delete a branch and free every commit only it could reach (DESIGN §4.2).
    ///
    /// The default branch cannot be discarded. Any [`ReadView`] already handed
    /// out stays valid: it holds its commit alive by reference count.
    pub fn discard(&self, branch: &str) -> Result<()> {
        if branch == self.inner.default_branch {
            return Err(Error::CannotDiscardDefaultBranch(branch.to_owned()));
        }
        let mut branches = self.inner.branches.write();
        if !branches.contains_key(branch) {
            return Err(Error::NoSuchBranch(branch.to_owned()));
        }
        self.record(&Record::BranchDiscarded {
            name: branch.to_owned(),
        })?;
        if let Some(handle) = branches.remove(branch) {
            // Noted before the commits go, while both ends can still be read.
            let commits = self.inner.commits.read();
            let seq_of = |id: &CommitId| commits.get(id).map_or(0, |c| c.seq);
            let made = seq_of(&handle.head.lock()).saturating_sub(seq_of(&handle.forked_at));
            drop(commits);
            let mut discarded = self.inner.discarded.lock();
            if discarded.len() == DISCARDS_REMEMBERED {
                discarded.pop_front();
            }
            discarded.push_back(Discarded {
                name: branch.to_owned(),
                forked_at: handle.forked_at,
                commits: made,
            });
        }
        self.inner.access.forget(branch);
        let surviving_heads: Vec<CommitId> = branches.values().map(|h| *h.head.lock()).collect();
        drop(branches);
        self.collect_unreachable(&surviving_heads);
        Ok(())
    }

    /// Drop every commit no surviving branch can reach.
    fn collect_unreachable(&self, surviving_heads: &[CommitId]) {
        let mut commits = self.inner.commits.write();
        let mut reachable: BTreeSet<CommitId> = BTreeSet::new();
        let mut queue: VecDeque<CommitId> = surviving_heads.iter().copied().collect();
        while let Some(id) = queue.pop_front() {
            if !reachable.insert(id) {
                continue;
            }
            if let Some(commit) = commits.get(&id) {
                for parent in &commit.parents {
                    queue.push_back(*parent);
                }
            }
        }
        commits.retain(|id, _| reachable.contains(id));
    }

    /// The most recently discarded branches, oldest first, at most
    /// [`DISCARDS_REMEMBERED`] of them.
    pub fn discarded(&self) -> Vec<Discarded> {
        self.inner.discarded.lock().iter().cloned().collect()
    }

    /// Every commit the graph holds, in id order: what a history view draws.
    pub fn all_commits(&self) -> Vec<Arc<Commit>> {
        self.inner.commits.read().values().map(Arc::clone).collect()
    }

    /// How many commits the graph currently holds. Tests use this to show that
    /// a rolled-back transaction and a discarded branch leave nothing behind.
    pub fn commit_count(&self) -> usize {
        self.inner.commits.read().len()
    }

    // ---- internals used by txn and merge -----------------------------------

    /// Install a commit and move a branch head from `expected` to it, failing
    /// if another writer moved the head first (DESIGN §4.3).
    pub(crate) fn commit_onto(
        &self,
        branch: &str,
        expected: CommitId,
        draft: CommitDraft,
    ) -> Result<CommitId> {
        let CommitDraft {
            parents,
            root,
            seq,
            message,
            ops,
        } = draft;
        let id = Commit::compute_id(&parents, message.as_deref(), &ops);
        let commit = Arc::new(Commit {
            id,
            parents,
            root,
            seq,
            message,
            ops,
        });
        let handle = self.branch_handle(branch)?;
        let mut head = handle.head.lock();
        if *head != expected {
            return Err(Error::Conflict {
                branch: branch.to_owned(),
                expected,
                actual: *head,
            });
        }
        // Write-ahead, and inside the head lock: the race is already won, so
        // this records a commit that is certainly happening, and it happens
        // before any reader can see it. A crash between the two replays to the
        // same state. The cost is a flush while the branch is locked, which
        // serializes writers on that branch — which is the model anyway.
        self.record(&Record::Commit {
            branch: branch.to_owned(),
            parents: commit.parents.clone(),
            seq: commit.seq,
            message: commit.message.clone(),
            ops: commit.ops.clone(),
        })?;
        // Take the commit-index lock only once the race is won, so a commit is
        // never visible in the graph unless a branch actually points at it.
        self.inner.commits.write().entry(id).or_insert(commit);
        *head = id;
        Ok(id)
    }

    /// Move a branch head without creating a commit, for fast-forward merges.
    pub(crate) fn fast_forward(
        &self,
        branch: &str,
        expected: CommitId,
        to: CommitId,
    ) -> Result<CommitId> {
        let handle = self.branch_handle(branch)?;
        let mut head = handle.head.lock();
        if *head != expected {
            return Err(Error::Conflict {
                branch: branch.to_owned(),
                expected,
                actual: *head,
            });
        }
        self.record(&Record::HeadMoved {
            branch: branch.to_owned(),
            to,
        })?;
        *head = to;
        Ok(to)
    }

    /// Pin, or check, the database-wide embedding dimension (DESIGN §4.1).
    pub(crate) fn check_dim(&self, embedding: Option<&Vec<f32>>) -> Result<()> {
        let Some(e) = embedding else { return Ok(()) };
        let mut dim = self.inner.dim.write();
        match *dim {
            None => {
                *dim = Some(e.len());
                Ok(())
            }
            Some(expected) if expected == e.len() => Ok(()),
            Some(expected) => Err(Error::DimensionMismatch {
                expected,
                got: e.len(),
            }),
        }
    }

    /// The most recent commit that is an ancestor of both `a` and `b`.
    ///
    /// Breadth-first from `a`, taking parents in order, so the answer depends
    /// only on the shape of the graph and never on iteration order.
    pub(crate) fn common_ancestor(&self, a: CommitId, b: CommitId) -> Option<CommitId> {
        let ancestors_of_b = self.ancestors(b);
        let commits = self.inner.commits.read();
        let mut seen: BTreeSet<CommitId> = BTreeSet::new();
        let mut queue: VecDeque<CommitId> = VecDeque::new();
        queue.push_back(a);
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id) {
                continue;
            }
            if ancestors_of_b.contains(&id) {
                return Some(id);
            }
            if let Some(commit) = commits.get(&id) {
                for parent in &commit.parents {
                    queue.push_back(*parent);
                }
            }
        }
        None
    }

    fn ancestors(&self, id: CommitId) -> BTreeSet<CommitId> {
        let commits = self.inner.commits.read();
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::from([id]);
        while let Some(cur) = queue.pop_front() {
            if !seen.insert(cur) {
                continue;
            }
            if let Some(commit) = commits.get(&cur) {
                for parent in &commit.parents {
                    queue.push_back(*parent);
                }
            }
        }
        seen
    }
}

/// Key-level difference between two roots, sorted ascending by key.
pub(crate) fn diff_roots(a: &Root, b: &Root) -> Vec<Change> {
    let mut out = Vec::new();
    let mut ia = a.iter().peekable();
    let mut ib = b.iter().peekable();
    // Both sides iterate in ascending key order, so one merge pass is enough.
    loop {
        match (ia.peek(), ib.peek()) {
            (None, None) => break,
            (Some((ka, _)), None) => {
                out.push(Change {
                    key: (*ka).clone(),
                    kind: ChangeKind::Removed,
                });
                ia.next();
            }
            (None, Some((kb, _))) => {
                out.push(Change {
                    key: (*kb).clone(),
                    kind: ChangeKind::Added,
                });
                ib.next();
            }
            (Some((ka, va)), Some((kb, vb))) => match ka.cmp(kb) {
                core::cmp::Ordering::Less => {
                    out.push(Change {
                        key: (*ka).clone(),
                        kind: ChangeKind::Removed,
                    });
                    ia.next();
                }
                core::cmp::Ordering::Greater => {
                    out.push(Change {
                        key: (*kb).clone(),
                        kind: ChangeKind::Added,
                    });
                    ib.next();
                }
                core::cmp::Ordering::Equal => {
                    if !va.content_eq(vb) {
                        out.push(Change {
                            key: (*ka).clone(),
                            kind: ChangeKind::Modified,
                        });
                    }
                    ia.next();
                    ib.next();
                }
            },
        }
    }
    out
}

/// Branch names are 1-255 bytes of printable, non-whitespace UTF-8.
pub fn validate_branch_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 255
        && !name.chars().any(|c| c.is_whitespace() || c.is_control());
    if ok {
        Ok(())
    } else {
        Err(Error::BadBranchName(name.to_owned()))
    }
}
