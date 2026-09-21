//! Stored values and the operations that produce them (DESIGN §4.1, §4.2).

use std::collections::BTreeMap;

use bytes::Bytes;

use crate::encode::Canonical;
use crate::error::{Error, Result};

/// Longest permitted key, in bytes (DESIGN §4.1).
pub const MAX_KEY_BYTES: usize = 1024;

/// Default importance for an entry that does not specify one (DESIGN §4.1).
pub const DEFAULT_IMPORTANCE: f32 = 0.5;

/// The metadata key that records who wrote an entry (DESIGN §4.1).
///
/// Set by whatever front end knows the writer, such as the MCP server, which
/// records the connected client's name. It is ordinary metadata, so it is part
/// of the operation and of the commit id: the same write from the same writer
/// is the same commit everywhere. It is not part of an entry's *content*,
/// though. Two writers storing the same value have not disagreed, so
/// [`Entry::content_eq`] ignores it, and merge and diff with it.
pub const WRITTEN_BY: &str = "memfork.by";

/// One stored record.
///
/// `value` is opaque bytes; JSON is a convention, not a requirement. The two
/// sequence numbers are logical clock readings, never wall-clock time, so that
/// a replay of the same operations reproduces the same entries byte for byte
/// (DESIGN §3.6).
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// The opaque stored value.
    pub value: Bytes,
    /// Optional vector for [`crate::Db::search`]. Fixed dimension per database.
    pub embedding: Option<Vec<f32>>,
    /// Retention weight in `[0, 1]`: how hard eviction tries to keep this.
    pub importance: f32,
    /// The branch sequence number at which this key was first written.
    pub created_seq: u64,
    /// The branch sequence number at which this entry was last written.
    pub last_access_seq: u64,
    /// Expiry measured in commits on the branch holding the entry.
    pub ttl_commits: Option<u64>,
    /// Caller metadata. Ordered, so it encodes deterministically.
    pub meta: BTreeMap<String, String>,
}

impl Entry {
    /// Compare the caller-visible content of two entries.
    ///
    /// The logical clock fields are deliberately excluded: two branches that
    /// wrote the same value at different sequence numbers have not made
    /// conflicting changes (DESIGN §4.2). So is [`WRITTEN_BY`], for the same
    /// reason: two writers storing the same value have not disagreed.
    pub fn content_eq(&self, other: &Entry) -> bool {
        fn described(meta: &BTreeMap<String, String>) -> impl Iterator<Item = (&String, &String)> {
            meta.iter().filter(|(k, _)| k.as_str() != WRITTEN_BY)
        }
        self.value == other.value
            && self.embedding == other.embedding
            && self.importance.to_bits() == other.importance.to_bits()
            && self.ttl_commits == other.ttl_commits
            && described(&self.meta).eq(described(&other.meta))
    }
}

/// The caller-supplied half of a write: everything except the logical clock.
#[derive(Debug, Clone, PartialEq)]
pub struct Value {
    /// The opaque stored value.
    pub value: Bytes,
    /// Optional vector for [`crate::Db::search`].
    pub embedding: Option<Vec<f32>>,
    /// Retention weight in `[0, 1]`.
    pub importance: f32,
    /// Expiry measured in commits on the branch holding the entry.
    pub ttl_commits: Option<u64>,
    /// Caller metadata.
    pub meta: BTreeMap<String, String>,
}

impl Value {
    /// A value with default importance and no embedding, TTL or metadata.
    pub fn new(value: impl Into<Bytes>) -> Self {
        Self {
            value: value.into(),
            embedding: None,
            importance: DEFAULT_IMPORTANCE,
            ttl_commits: None,
            meta: BTreeMap::new(),
        }
    }

    /// Attach an embedding.
    #[must_use]
    pub fn with_embedding(mut self, embedding: Vec<f32>) -> Self {
        self.embedding = Some(embedding);
        self
    }

    /// Set the retention weight.
    #[must_use]
    pub fn with_importance(mut self, importance: f32) -> Self {
        self.importance = importance;
        self
    }

    /// Set the commit-counted TTL.
    #[must_use]
    pub fn with_ttl_commits(mut self, ttl: u64) -> Self {
        self.ttl_commits = Some(ttl);
        self
    }

    /// Add one metadata pair.
    #[must_use]
    pub fn with_meta(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.meta.insert(k.into(), v.into());
        self
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if !(0.0..=1.0).contains(&self.importance) {
            return Err(Error::BadImportance(self.importance));
        }
        if let Some(e) = &self.embedding {
            if e.is_empty() {
                return Err(Error::EmptyEmbedding);
            }
        }
        Ok(())
    }

    pub(crate) fn into_entry(self, seq: u64, created_seq: u64) -> Entry {
        Entry {
            value: self.value,
            embedding: self.embedding,
            importance: self.importance,
            created_seq,
            last_access_seq: seq,
            ttl_commits: self.ttl_commits,
            meta: self.meta,
        }
    }
}

impl From<&Entry> for Value {
    fn from(e: &Entry) -> Self {
        Value {
            value: e.value.clone(),
            embedding: e.embedding.clone(),
            importance: e.importance,
            ttl_commits: e.ttl_commits,
            meta: e.meta.clone(),
        }
    }
}

/// Validate a key against the rules in DESIGN §4.1.
pub fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(Error::EmptyKey);
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(Error::KeyTooLong(key.len()));
    }
    Ok(())
}

/// One change recorded in a commit. The `ops` of a commit are its change set
/// and, taken together across history, the append-only event log (DESIGN §4.1).
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Write a key.
    Put {
        /// The key written.
        key: String,
        /// The value written.
        value: Value,
    },
    /// Remove a key.
    Delete {
        /// The key removed.
        key: String,
    },
    /// Remove a key to stay inside the memory budget (DESIGN §4.4).
    ///
    /// Recorded distinctly from [`Op::Delete`] so that eviction is visible in
    /// the log and reversible by time travel.
    Evict {
        /// The key evicted.
        key: String,
    },
}

impl Op {
    /// The key this operation applies to.
    pub fn key(&self) -> &str {
        match self {
            Op::Put { key, .. } | Op::Delete { key } | Op::Evict { key } => key,
        }
    }

    pub(crate) fn encode(&self, c: &mut Canonical) {
        match self {
            Op::Put { key, value } => {
                c.u8(1).str(key);
                c.bytes(&value.value);
                c.opt_f32_slice(value.embedding.as_deref());
                c.f32(value.importance);
                c.opt_u64(value.ttl_commits);
                c.u64(value.meta.len() as u64);
                for (k, v) in &value.meta {
                    c.str(k).str(v);
                }
            }
            Op::Delete { key } => {
                c.u8(2).str(key);
            }
            Op::Evict { key } => {
                c.u8(3).str(key);
            }
        }
    }
}
