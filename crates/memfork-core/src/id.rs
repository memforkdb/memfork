//! Content-addressed commit identifiers.

use core::fmt;

use crate::error::Error;

/// A BLAKE3-derived, content-addressed commit id (DESIGN §4.1).
///
/// `CommitId` is a pure function of the commit's parents and its canonically
/// encoded operations, so the same sequence of operations produces the same ids
/// on every operating system (DESIGN §3.6), which a golden file pins.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommitId(pub(crate) [u8; 32]);

impl CommitId {
    /// The raw 32 bytes of the id.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Rebuild an id from its raw bytes.
    ///
    /// Used when reading a journal record back. Nothing checks that the bytes
    /// are the hash of anything: replay recomputes every id it installs, and
    /// an id read from a record is only a reference to a commit replay has
    /// already rebuilt.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        CommitId(bytes)
    }

    /// The lowercase hex form, 64 characters.
    pub fn to_hex(self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push(hex_digit(b >> 4));
            s.push(hex_digit(b & 0x0f));
        }
        s
    }

    /// Parse a full 64-character lowercase or uppercase hex id.
    pub fn from_hex(s: &str) -> Result<Self, Error> {
        let bytes = s.as_bytes();
        if bytes.len() != 64 {
            return Err(Error::BadCommitId(s.to_owned()));
        }
        let mut out = [0u8; 32];
        for (i, pair) in bytes.chunks_exact(2).enumerate() {
            let hi = hex_value(pair[0]).ok_or_else(|| Error::BadCommitId(s.to_owned()))?;
            let lo = hex_value(pair[1]).ok_or_else(|| Error::BadCommitId(s.to_owned()))?;
            out[i] = (hi << 4) | lo;
        }
        Ok(CommitId(out))
    }
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'a' + nibble - 10) as char,
    }
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CommitId({})", self.to_hex())
    }
}
