//! The persistent-map seam (DESIGN §4.1).
//!
//! `Root` is a persistent, structurally shared ordered map. Branching copies
//! nothing: a fork and its parent share every node until one of them is
//! written to, and a write copies only the path from the root to the changed
//! key. Nothing here uses OS `fork()`, copy-on-write pages or any other
//! platform snapshot trick — none of which would work the same way on three
//! operating systems.
//!
//! The concrete structure sits behind [`Store`] so it can be replaced without
//! touching the engine.

use std::sync::Arc;

use crate::entry::Entry;

/// A persistent ordered map from key to entry.
///
/// Implementations must iterate in ascending key order, because that ordering
/// is what makes commit ids and listings reproducible.
pub trait Store: Clone + core::fmt::Debug + Default + Send + Sync {
    /// Look a key up.
    fn get(&self, key: &str) -> Option<&Arc<Entry>>;

    /// Return a new map with `key` set, leaving `self` untouched.
    fn insert(&self, key: String, value: Arc<Entry>) -> Self;

    /// Return a new map with `key` removed, leaving `self` untouched.
    fn remove(&self, key: &str) -> Self;

    /// Set `key` in place. Equivalent to `*self = self.insert(..)`, but skips
    /// copying nodes this map holds exclusively.
    fn insert_mut(&mut self, key: String, value: Arc<Entry>);

    /// Remove `key` in place. Returns whether the key was present.
    fn remove_mut(&mut self, key: &str) -> bool;

    /// Number of entries.
    fn len(&self) -> usize;

    /// Whether the map is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// All entries, in ascending key order.
    fn iter(&self) -> impl Iterator<Item = (&String, &Arc<Entry>)>;

    /// All entries whose key starts with `prefix`, in ascending key order.
    fn range_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = (&'a String, &'a Arc<Entry>)>;

    /// Whether two maps are the same allocation, i.e. share all their nodes.
    ///
    /// Used by the test that shows forking copies nothing.
    fn shares_allocation_with(&self, other: &Self) -> bool;
}

/// The persistent ordered map used by the engine.
///
/// A red-black tree map with atomically reference-counted nodes: `O(log n)`
/// reads and writes, `O(1)` cloning, ascending-order iteration, and `Send +
/// Sync` so a root can be handed to any number of reader threads.
pub type Root = rpds::RedBlackTreeMapSync<String, Arc<Entry>>;

impl Store for Root {
    fn get(&self, key: &str) -> Option<&Arc<Entry>> {
        Root::get(self, key)
    }

    fn insert(&self, key: String, value: Arc<Entry>) -> Self {
        Root::insert(self, key, value)
    }

    fn remove(&self, key: &str) -> Self {
        Root::remove(self, key)
    }

    fn insert_mut(&mut self, key: String, value: Arc<Entry>) {
        Root::insert_mut(self, key, value);
    }

    fn remove_mut(&mut self, key: &str) -> bool {
        Root::remove_mut(self, key)
    }

    fn len(&self) -> usize {
        self.size()
    }

    fn iter(&self) -> impl Iterator<Item = (&String, &Arc<Entry>)> {
        Root::iter(self)
    }

    fn range_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = (&'a String, &'a Arc<Entry>)> {
        // Seek to the first key at or after the prefix, then stop at the first
        // key that no longer carries it. Walking forward from the seek point
        // avoids constructing an upper bound, which for a byte-incremented
        // prefix would not always be valid UTF-8.
        self.range::<String, _>(prefix.to_owned()..)
            .take_while(move |(k, _)| k.starts_with(prefix))
    }

    fn shares_allocation_with(&self, other: &Self) -> bool {
        self.ptr_eq(other)
    }
}
