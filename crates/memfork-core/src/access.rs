//! Access tracking: the unversioned side structure (DESIGN §4.4, v0.3).
//!
//! Eviction scores an entry partly on how recently it was read, but a read
//! must not become a write. If `last_access_seq` lived in the entry, every
//! read would rewrite the entry, land in the commit chain as an event, and put
//! read traffic into the commit ids — three separate ways of breaking things
//! that matter more than eviction accuracy does.
//!
//! So reads are recorded here instead, beside the commit graph and outside it.
//! The consequences are real and worth stating:
//!
//! - It is **not versioned**. Time travel and `fork_at` restore state, not
//!   reading history; an old view scores the same as the present.
//! - A fork **inherits a snapshot** of its parent's readings at the moment of
//!   the fork, and the two diverge from there.
//! - A discarded branch's readings **go with it**.
//! - It is **not durable**. After a restart everything looks unread until it
//!   is read again, which costs a little eviction accuracy and nothing else.
//!
//! Tracking is only switched on when a memory budget is configured. With no
//! budget there is nothing to score, so reads stay free and lock-free, and the
//! guarantee that readers never block holds exactly as DESIGN §4.3 states it.
//! With a budget, a read takes a brief per-branch lock to note one number.

use std::collections::BTreeMap;

use parking_lot::Mutex;

/// When each key was last read, per branch.
#[derive(Debug, Default)]
pub(crate) struct AccessLog {
    /// One lock per branch, so a reader on one branch never waits for a
    /// reader on another.
    branches: Mutex<BTreeMap<String, Mutex<BTreeMap<String, u64>>>>,
}

impl AccessLog {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Note that `keys` were read on `branch` at sequence `seq`.
    pub(crate) fn record(&self, branch: &str, seq: u64, keys: impl Iterator<Item = String>) {
        let mut keys = keys.peekable();
        if keys.peek().is_none() {
            return;
        }
        let branches = self.branches.lock();
        match branches.get(branch) {
            Some(entry) => {
                let mut map = entry.lock();
                for key in keys {
                    map.insert(key, seq);
                }
            }
            None => {
                drop(branches);
                let mut branches = self.branches.lock();
                let entry = branches.entry(branch.to_owned()).or_default();
                let mut map = entry.lock();
                for key in keys {
                    map.insert(key, seq);
                }
            }
        }
    }

    /// The readings for one branch, copied out for scoring.
    pub(crate) fn snapshot(&self, branch: &str) -> BTreeMap<String, u64> {
        self.branches
            .lock()
            .get(branch)
            .map(|m| m.lock().clone())
            .unwrap_or_default()
    }

    /// Give a new branch a copy of its parent's readings.
    pub(crate) fn fork(&self, from: &str, to: &str) {
        let copied = self.snapshot(from);
        if copied.is_empty() {
            return;
        }
        self.branches
            .lock()
            .insert(to.to_owned(), Mutex::new(copied));
    }

    /// Drop a branch's readings.
    pub(crate) fn forget(&self, branch: &str) {
        self.branches.lock().remove(branch);
    }

    /// Drop the readings for keys that no longer exist, so the structure does
    /// not outgrow the data it describes.
    pub(crate) fn retain(&self, branch: &str, keep: impl Fn(&str) -> bool) {
        if let Some(entry) = self.branches.lock().get(branch) {
            entry.lock().retain(|k, _| keep(k));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_are_recorded_per_branch() {
        let log = AccessLog::new();
        log.record("main", 5, ["a".to_owned()].into_iter());
        log.record("side", 9, ["a".to_owned()].into_iter());

        assert_eq!(log.snapshot("main").get("a"), Some(&5));
        assert_eq!(log.snapshot("side").get("a"), Some(&9));
        assert!(log.snapshot("nowhere").is_empty());
    }

    #[test]
    fn a_later_reading_replaces_an_earlier_one() {
        let log = AccessLog::new();
        log.record("main", 1, ["a".to_owned()].into_iter());
        log.record("main", 7, ["a".to_owned()].into_iter());
        assert_eq!(log.snapshot("main").get("a"), Some(&7));
    }

    #[test]
    fn a_fork_inherits_a_snapshot_and_then_diverges() {
        let log = AccessLog::new();
        log.record("main", 5, ["a".to_owned()].into_iter());
        log.fork("main", "side");
        assert_eq!(log.snapshot("side").get("a"), Some(&5));

        log.record("side", 10, ["a".to_owned()].into_iter());
        assert_eq!(log.snapshot("side").get("a"), Some(&10));
        assert_eq!(
            log.snapshot("main").get("a"),
            Some(&5),
            "the fork's reading leaked back to its parent"
        );
    }

    #[test]
    fn a_discarded_branch_takes_its_readings_with_it() {
        let log = AccessLog::new();
        log.record("side", 5, ["a".to_owned()].into_iter());
        log.forget("side");
        assert!(log.snapshot("side").is_empty());
    }

    #[test]
    fn readings_for_vanished_keys_are_dropped() {
        let log = AccessLog::new();
        log.record("main", 1, ["a".to_owned(), "b".to_owned()].into_iter());
        log.retain("main", |k| k == "a");
        let snap = log.snapshot("main");
        assert!(snap.contains_key("a"));
        assert!(!snap.contains_key("b"));
    }

    #[test]
    fn recording_nothing_costs_nothing() {
        let log = AccessLog::new();
        log.record("main", 1, std::iter::empty());
        assert!(log.snapshot("main").is_empty());
    }
}
