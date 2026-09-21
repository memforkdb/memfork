//! Three-way merge at key level (DESIGN §4.2).

use std::collections::BTreeSet;

use smallvec::smallvec;

use crate::db::{CommitDraft, Db};
use crate::entry::{Op, Value};
use crate::error::{Error, Result};
use crate::id::CommitId;
use crate::store::Root;
use crate::txn::apply_ops;

/// What to do with a key both branches changed (DESIGN §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MergePolicy {
    /// Change nothing and report the conflicting keys. The default: an agent
    /// should have to say which side wins.
    #[default]
    Fail,
    /// Keep the target branch's value.
    Ours,
    /// Take the source branch's value.
    Theirs,
}

impl MergePolicy {
    /// Parse `fail`, `ours` or `theirs`, case-insensitively.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "fail" => Some(MergePolicy::Fail),
            "ours" => Some(MergePolicy::Ours),
            "theirs" => Some(MergePolicy::Theirs),
            _ => None,
        }
    }

    /// The name this policy parses from.
    pub fn as_str(self) -> &'static str {
        match self {
            MergePolicy::Fail => "fail",
            MergePolicy::Ours => "ours",
            MergePolicy::Theirs => "theirs",
        }
    }
}

/// How a merge resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeKind {
    /// The source was already an ancestor of the target; nothing to do.
    UpToDate,
    /// The target had not moved since the fork, so its head simply advanced.
    FastForward,
    /// A merge commit with both heads as parents was created.
    Merged,
}

impl MergeKind {
    /// The name this outcome goes by, in the CLI, the tools and the bindings.
    ///
    /// One spelling in one place: three copies of this match drifted into
    /// three different words the last time it was written out by hand.
    pub fn as_str(self) -> &'static str {
        match self {
            MergeKind::UpToDate => "up-to-date",
            MergeKind::FastForward => "fast-forward",
            MergeKind::Merged => "merged",
        }
    }
}

/// The result of a successful merge.
#[derive(Debug, Clone)]
pub struct MergeOutcome {
    /// The branch that was merged into.
    pub target: String,
    /// The branch that was merged from.
    pub source: String,
    /// The target's head after the merge.
    pub head: CommitId,
    /// The common ancestor the three-way merge was computed against.
    pub base: CommitId,
    /// How the merge resolved.
    pub kind: MergeKind,
    /// Keys the merge changed on the target, sorted ascending.
    pub changed: Vec<String>,
    /// Keys both sides had changed, sorted ascending. Always empty under
    /// [`MergePolicy::Fail`], which errors instead.
    pub conflicts: Vec<String>,
}

impl Db {
    /// Merge `source` into `target` against their common ancestor (DESIGN §4.2).
    ///
    /// A key is in conflict only when both sides changed it to different
    /// content. Under [`MergePolicy::Fail`] a conflict returns
    /// [`Error::MergeConflict`] with every conflicting key and changes
    /// nothing at all.
    pub fn merge(&self, source: &str, target: &str, policy: MergePolicy) -> Result<MergeOutcome> {
        // A merge is a read-modify-write on the target, so it takes the
        // target's turn at writing rather than racing single-key writes.
        let target_handle = self.branch_handle(target)?;
        let _writing = target_handle.writer.lock();

        let source_head = self.head(source)?;
        let target_head = self.head(target)?;

        let base = self
            .common_ancestor(target_head, source_head)
            .ok_or_else(|| Error::NoCommonAncestor {
                from_branch: source.to_owned(),
                into_branch: target.to_owned(),
            })?;

        if base == source_head {
            // Everything on the source is already in the target's history.
            return Ok(MergeOutcome {
                target: target.to_owned(),
                source: source.to_owned(),
                head: target_head,
                base,
                kind: MergeKind::UpToDate,
                changed: Vec::new(),
                conflicts: Vec::new(),
            });
        }

        let base_root = self.commit(base)?.root.clone();
        let ours = self.commit(target_head)?.root.clone();
        let theirs = self.commit(source_head)?.root.clone();

        if base == target_head {
            // The target has not moved since the fork: advance it. Nothing is
            // recomputed, so this stays O(1) however large the branches are.
            let changed = crate::db::diff_roots(&ours, &theirs)
                .into_iter()
                .map(|c| c.key)
                .collect();
            let head = self.fast_forward(target, target_head, source_head)?;
            return Ok(MergeOutcome {
                target: target.to_owned(),
                source: source.to_owned(),
                head,
                base,
                kind: MergeKind::FastForward,
                changed,
                conflicts: Vec::new(),
            });
        }

        let plan = plan_merge(&base_root, &ours, &theirs, policy);
        if policy == MergePolicy::Fail && !plan.conflicts.is_empty() {
            return Err(Error::MergeConflict {
                keys: plan.conflicts,
            });
        }

        if plan.ops.is_empty() {
            // Both sides made the same change, or only the target moved. There
            // is still real history to record, so a merge commit is made below
            // only when there is something to say; otherwise the target stands.
            return Ok(MergeOutcome {
                target: target.to_owned(),
                source: source.to_owned(),
                head: target_head,
                base,
                kind: MergeKind::UpToDate,
                changed: Vec::new(),
                conflicts: plan.conflicts,
            });
        }

        let seq = self.commit(target_head)?.seq + 1;
        let mut root = ours;
        apply_ops(&mut root, &plan.ops, seq);
        let changed: Vec<String> = plan.ops.iter().map(|op| op.key().to_owned()).collect();

        let head = self.commit_onto(
            target,
            target_head,
            CommitDraft {
                parents: smallvec![target_head, source_head],
                root,
                seq,
                message: Some(format!(
                    "merge {source} into {target} ({})",
                    policy.as_str()
                )),
                ops: plan.ops,
            },
        )?;

        Ok(MergeOutcome {
            target: target.to_owned(),
            source: source.to_owned(),
            head,
            base,
            kind: MergeKind::Merged,
            changed,
            conflicts: plan.conflicts,
        })
    }
}

#[derive(Debug)]
struct MergePlan {
    /// Changes to apply to the target, sorted ascending by key.
    ops: Vec<Op>,
    /// Keys both sides changed differently, sorted ascending.
    conflicts: Vec<String>,
}

/// Compute the change set that folds `theirs` into `ours` relative to `base`.
fn plan_merge(base: &Root, ours: &Root, theirs: &Root, policy: MergePolicy) -> MergePlan {
    // The candidate set is every key that either side touched. Collected into
    // a BTreeSet so the plan is emitted in ascending key order regardless of
    // which side contributed the key.
    let mut candidates: BTreeSet<&String> = BTreeSet::new();
    for (k, v) in base.iter() {
        if ours.get(k).is_none_or(|o| !o.content_eq(v))
            || theirs.get(k).is_none_or(|t| !t.content_eq(v))
        {
            candidates.insert(k);
        }
    }
    for (k, v) in ours.iter() {
        if base.get(k).is_none_or(|b| !b.content_eq(v)) {
            candidates.insert(k);
        }
    }
    for (k, v) in theirs.iter() {
        if base.get(k).is_none_or(|b| !b.content_eq(v)) {
            candidates.insert(k);
        }
    }

    let mut ops = Vec::new();
    let mut conflicts = Vec::new();

    for key in candidates {
        let b = base.get(key);
        let o = ours.get(key);
        let t = theirs.get(key);

        let ours_changed = !same(b, o);
        let theirs_changed = !same(b, t);

        let take_theirs = match (ours_changed, theirs_changed) {
            // Only the source moved: take it.
            (false, true) => true,
            // Only the target moved, or neither did: leave the target alone.
            (_, false) => false,
            // Both moved. Identical changes are not a conflict.
            (true, true) => {
                if same(o, t) {
                    false
                } else {
                    conflicts.push(key.clone());
                    policy == MergePolicy::Theirs
                }
            }
        };

        if !take_theirs {
            continue;
        }
        match t {
            Some(entry) => ops.push(Op::Put {
                key: key.clone(),
                value: Value::from(entry.as_ref()),
            }),
            None => ops.push(Op::Delete { key: key.clone() }),
        }
    }

    MergePlan { ops, conflicts }
}

fn same(
    a: Option<&std::sync::Arc<crate::entry::Entry>>,
    b: Option<&std::sync::Arc<crate::entry::Entry>>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.content_eq(y),
        _ => false,
    }
}
