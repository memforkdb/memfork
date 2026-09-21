//! Canonical byte encoding used for content-addressed commit ids (DESIGN §4.1).
//!
//! The encoding is written by hand rather than delegated to a serialization
//! library, so a commit id can never change because a dependency changed how it
//! lays bytes out. Every variable-length value is length-prefixed and every
//! integer is little-endian, so the encoded form is identical on every target.
//!
//! Bytes are streamed straight into the hasher rather than collected into a
//! buffer, so hashing a commit costs no extra allocation however large its
//! change set is.

use crate::id::CommitId;

/// Domain separator, so a commit hash can never collide with some other hash
/// MemFork may compute later.
const DOMAIN: &[u8] = b"memfork.commit.v1\0";

/// A canonical-encoding hasher.
#[derive(Debug, Clone)]
pub(crate) struct Canonical {
    hasher: blake3::Hasher,
}

impl Canonical {
    pub(crate) fn new() -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(DOMAIN);
        Self { hasher }
    }

    pub(crate) fn finalize(&self) -> CommitId {
        CommitId(*self.hasher.finalize().as_bytes())
    }

    pub(crate) fn u8(&mut self, v: u8) -> &mut Self {
        self.hasher.update(&[v]);
        self
    }

    pub(crate) fn u64(&mut self, v: u64) -> &mut Self {
        self.hasher.update(&v.to_le_bytes());
        self
    }

    /// IEEE-754 bit pattern, so the encoding does not depend on float
    /// formatting. NaN is normalized to one quiet pattern so that two entries
    /// which compare equal always encode equal.
    pub(crate) fn f32(&mut self, v: f32) -> &mut Self {
        let bits = if v.is_nan() {
            f32::NAN.to_bits()
        } else {
            v.to_bits()
        };
        self.hasher.update(&bits.to_le_bytes());
        self
    }

    pub(crate) fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u64(v.len() as u64);
        self.hasher.update(v);
        self
    }

    pub(crate) fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }

    pub(crate) fn commit_id(&mut self, v: &CommitId) -> &mut Self {
        self.hasher.update(&v.0);
        self
    }

    /// An optional string. The `0`/`1` tag is emitted before the length, so
    /// `None` and `Some("")` encode differently and therefore hash
    /// differently: a commit with no message is not a commit with an empty one.
    pub(crate) fn opt_str(&mut self, v: Option<&str>) -> &mut Self {
        match v {
            None => self.u8(0),
            Some(s) => self.u8(1).str(s),
        }
    }

    pub(crate) fn opt_u64(&mut self, v: Option<u64>) -> &mut Self {
        match v {
            None => self.u8(0),
            Some(x) => self.u8(1).u64(x),
        }
    }

    pub(crate) fn opt_f32_slice(&mut self, v: Option<&[f32]>) -> &mut Self {
        match v {
            None => self.u8(0),
            Some(xs) => {
                self.u8(1).u64(xs.len() as u64);
                for x in xs {
                    self.f32(*x);
                }
                self
            }
        }
    }
}

/// The same canonical layout, written to a buffer instead of a hasher.
///
/// Commit ids stream into BLAKE3 and never need the bytes back; journal
/// records do. The two writers share a layout by construction — every method
/// here has a counterpart above with the same name and the same ordering — so
/// a change to one is an obvious omission in the other.
#[derive(Debug, Default)]
pub(crate) struct Bytes {
    buf: Vec<u8>,
}

impl Bytes {
    pub(crate) fn new() -> Self {
        Bytes { buf: Vec::new() }
    }

    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn f32(&mut self, v: f32) {
        let bits = if v.is_nan() {
            f32::NAN.to_bits()
        } else {
            v.to_bits()
        };
        self.buf.extend_from_slice(&bits.to_le_bytes());
    }

    pub(crate) fn bytes(&mut self, v: &[u8]) {
        self.u64(v.len() as u64);
        self.buf.extend_from_slice(v);
    }

    pub(crate) fn str(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }

    pub(crate) fn opt_str(&mut self, v: Option<&str>) {
        match v {
            None => self.u8(0),
            Some(s) => {
                self.u8(1);
                self.str(s);
            }
        }
    }

    pub(crate) fn opt_u64(&mut self, v: Option<u64>) {
        match v {
            None => self.u8(0),
            Some(x) => {
                self.u8(1);
                self.u64(x);
            }
        }
    }

    pub(crate) fn commit_id(&mut self, v: &CommitId) {
        self.buf.extend_from_slice(v.as_bytes());
    }
}
