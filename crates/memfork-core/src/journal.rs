//! The write-ahead journal seam (DESIGN §4.5).
//!
//! The engine does no I/O. When a caller wants durability it supplies a
//! [`Journal`], and the engine hands it one [`Record`] per change *before* the
//! change becomes visible. If the process dies between the append and the
//! head swap, replaying the record reproduces the same state; if it dies
//! before the append, the change never happened. Either way there is no state
//! a reader could have seen that recovery cannot rebuild.
//!
//! **Commit ids are never written down.** A record carries the parents, the
//! message and the operations, and replay recomputes the id from them exactly
//! as the original commit did. That is what makes "recovery reproduces
//! identical commit ids" a property worth testing rather than a tautology
//! about copying bytes back.
//!
//! Encoding lives here, beside the record it encodes, but produces bytes
//! rather than writing them: where those bytes go is the caller's business.

use core::fmt;

use smallvec::SmallVec;

use crate::encode::Bytes;
use crate::entry::{Op, Value};
use crate::id::CommitId;

/// One durable change to the database.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    /// A commit was made on a branch.
    Commit {
        /// The branch whose head moved to this commit.
        branch: String,
        /// The commit's parents.
        parents: SmallVec<[CommitId; 2]>,
        /// The commit's sequence number on that branch.
        seq: u64,
        /// The commit message.
        message: Option<String>,
        /// The change set.
        ops: Vec<Op>,
    },
    /// A branch was created, pointing at an existing commit.
    BranchCreated {
        /// The new branch's name.
        name: String,
        /// The commit it points at.
        at: CommitId,
    },
    /// A branch was deleted.
    BranchDiscarded {
        /// The branch that was deleted.
        name: String,
    },
    /// A branch head moved without a new commit, as a fast-forward merge does.
    HeadMoved {
        /// The branch whose head moved.
        branch: String,
        /// Where it moved to.
        to: CommitId,
    },
}

/// Somewhere durable to put records.
///
/// `append` returns only when the record is as durable as the journal's policy
/// promises. A journal that buffers must say so in its own documentation,
/// because the engine treats a successful append as "this will survive".
pub trait Journal: Send + Sync + fmt::Debug {
    /// Record one change. An error aborts the change that provoked it.
    fn append(&self, record: &Record) -> Result<(), JournalError>;
}

/// Why a record could not be recorded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot record the change durably: {0}")]
pub struct JournalError(pub String);

impl JournalError {
    /// A journal failure with the given reason.
    pub fn new(reason: impl Into<String>) -> Self {
        JournalError(reason.into())
    }
}

/// Tag bytes. Explicit rather than derived from declaration order, so
/// reordering the enum cannot silently change the format.
mod tag {
    pub const COMMIT: u8 = 1;
    pub const BRANCH_CREATED: u8 = 2;
    pub const BRANCH_DISCARDED: u8 = 3;
    pub const HEAD_MOVED: u8 = 4;
}

/// Encode a record to bytes.
pub fn encode(record: &Record) -> Vec<u8> {
    let mut b = Bytes::new();
    match record {
        Record::Commit {
            branch,
            parents,
            seq,
            message,
            ops,
        } => {
            b.u8(tag::COMMIT);
            b.str(branch);
            b.u64(parents.len() as u64);
            for p in parents {
                b.commit_id(p);
            }
            b.u64(*seq);
            b.opt_str(message.as_deref());
            b.u64(ops.len() as u64);
            for op in ops {
                encode_op(&mut b, op);
            }
        }
        Record::BranchCreated { name, at } => {
            b.u8(tag::BRANCH_CREATED);
            b.str(name);
            b.commit_id(at);
        }
        Record::BranchDiscarded { name } => {
            b.u8(tag::BRANCH_DISCARDED);
            b.str(name);
        }
        Record::HeadMoved { branch, to } => {
            b.u8(tag::HEAD_MOVED);
            b.str(branch);
            b.commit_id(to);
        }
    }
    b.into_vec()
}

fn encode_op(b: &mut Bytes, op: &Op) {
    match op {
        Op::Put { key, value } => {
            b.u8(1);
            b.str(key);
            b.bytes(&value.value);
            match &value.embedding {
                None => b.u8(0),
                Some(v) => {
                    b.u8(1);
                    b.u64(v.len() as u64);
                    for x in v {
                        b.f32(*x);
                    }
                }
            }
            b.f32(value.importance);
            b.opt_u64(value.ttl_commits);
            b.u64(value.meta.len() as u64);
            for (k, v) in &value.meta {
                b.str(k);
                b.str(v);
            }
        }
        Op::Delete { key } => {
            b.u8(2);
            b.str(key);
        }
        Op::Evict { key } => {
            b.u8(3);
            b.str(key);
        }
    }
}

/// Why a record could not be read back.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The bytes ran out before the record did.
    #[error("the record is truncated: wanted {wanted} more byte(s) at offset {at}")]
    Truncated {
        /// Where the read stopped.
        at: usize,
        /// How many more bytes were needed.
        wanted: usize,
    },
    /// A tag byte named something this version does not know.
    #[error("unknown {what} tag {tag} at offset {at}")]
    UnknownTag {
        /// What was being read.
        what: &'static str,
        /// The tag that was found.
        tag: u8,
        /// Where it was found.
        at: usize,
    },
    /// A string was not UTF-8.
    #[error("a string in the record is not valid UTF-8, at offset {at}")]
    BadUtf8 {
        /// Where the string started.
        at: usize,
    },
    /// The record decoded, but bytes were left over.
    #[error("{extra} byte(s) left over after the record")]
    TrailingBytes {
        /// How many bytes were left.
        extra: usize,
    },
    /// A length field was larger than the data that follows could possibly be.
    #[error("a length field claims {claimed} items, which the record cannot hold")]
    ImplausibleLength {
        /// The length that was claimed.
        claimed: u64,
    },
}

/// Decode a record from bytes.
pub fn decode(bytes: &[u8]) -> Result<Record, DecodeError> {
    let mut r = Reader::new(bytes);
    let record = decode_record(&mut r)?;
    if r.remaining() != 0 {
        return Err(DecodeError::TrailingBytes {
            extra: r.remaining(),
        });
    }
    Ok(record)
}

fn decode_record(r: &mut Reader<'_>) -> Result<Record, DecodeError> {
    let at = r.pos;
    match r.u8()? {
        tag::COMMIT => {
            let branch = r.str()?;
            let count = r.count()?;
            let mut parents = SmallVec::new();
            for _ in 0..count {
                parents.push(r.commit_id()?);
            }
            let seq = r.u64()?;
            let message = r.opt_str()?;
            let op_count = r.count()?;
            let mut ops = Vec::with_capacity(op_count.min(1024));
            for _ in 0..op_count {
                ops.push(decode_op(r)?);
            }
            Ok(Record::Commit {
                branch,
                parents,
                seq,
                message,
                ops,
            })
        }
        tag::BRANCH_CREATED => Ok(Record::BranchCreated {
            name: r.str()?,
            at: r.commit_id()?,
        }),
        tag::BRANCH_DISCARDED => Ok(Record::BranchDiscarded { name: r.str()? }),
        tag::HEAD_MOVED => Ok(Record::HeadMoved {
            branch: r.str()?,
            to: r.commit_id()?,
        }),
        tag => Err(DecodeError::UnknownTag {
            what: "record",
            tag,
            at,
        }),
    }
}

fn decode_op(r: &mut Reader<'_>) -> Result<Op, DecodeError> {
    let at = r.pos;
    match r.u8()? {
        1 => {
            let key = r.str()?;
            let value = bytes::Bytes::from(r.bytes()?.to_vec());
            let embedding = match r.u8()? {
                0 => None,
                1 => {
                    let n = r.count()?;
                    let mut v = Vec::with_capacity(n.min(4096));
                    for _ in 0..n {
                        v.push(r.f32()?);
                    }
                    Some(v)
                }
                tag => {
                    return Err(DecodeError::UnknownTag {
                        what: "embedding presence",
                        tag,
                        at: r.pos,
                    })
                }
            };
            let importance = r.f32()?;
            let ttl_commits = r.opt_u64()?;
            let meta_count = r.count()?;
            let mut meta = std::collections::BTreeMap::new();
            for _ in 0..meta_count {
                let k = r.str()?;
                let v = r.str()?;
                meta.insert(k, v);
            }
            Ok(Op::Put {
                key,
                value: Value {
                    value,
                    embedding,
                    importance,
                    ttl_commits,
                    meta,
                },
            })
        }
        2 => Ok(Op::Delete { key: r.str()? }),
        3 => Ok(Op::Evict { key: r.str()? }),
        tag => Err(DecodeError::UnknownTag {
            what: "operation",
            tag,
            at,
        }),
    }
}

/// A bounds-checked cursor over a record's bytes.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.remaining() < n {
            return Err(DecodeError::Truncated {
                at: self.pos,
                wanted: n - self.remaining(),
            });
        }
        let out = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    fn f32(&mut self) -> Result<f32, DecodeError> {
        let b = self.take(4)?;
        let mut a = [0u8; 4];
        a.copy_from_slice(b);
        Ok(f32::from_bits(u32::from_le_bytes(a)))
    }

    /// A count of items still to be read.
    ///
    /// Rejected up front when it exceeds what the remaining bytes could hold,
    /// so a corrupted length cannot make the decoder allocate wildly before it
    /// notices the record is short.
    fn count(&mut self) -> Result<usize, DecodeError> {
        let claimed = self.u64()?;
        if claimed > self.remaining() as u64 {
            return Err(DecodeError::ImplausibleLength { claimed });
        }
        Ok(claimed as usize)
    }

    fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = self.count()?;
        self.take(n)
    }

    fn str(&mut self) -> Result<String, DecodeError> {
        let at = self.pos;
        let b = self.bytes()?;
        core::str::from_utf8(b)
            .map(str::to_owned)
            .map_err(|_| DecodeError::BadUtf8 { at })
    }

    fn opt_str(&mut self) -> Result<Option<String>, DecodeError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.str()?)),
            tag => Err(DecodeError::UnknownTag {
                what: "optional string presence",
                tag,
                at: self.pos,
            }),
        }
    }

    fn opt_u64(&mut self) -> Result<Option<u64>, DecodeError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.u64()?)),
            tag => Err(DecodeError::UnknownTag {
                what: "optional number presence",
                tag,
                at: self.pos,
            }),
        }
    }

    fn commit_id(&mut self) -> Result<CommitId, DecodeError> {
        let b = self.take(32)?;
        let mut a = [0u8; 32];
        a.copy_from_slice(b);
        Ok(CommitId::from_bytes(a))
    }
}
