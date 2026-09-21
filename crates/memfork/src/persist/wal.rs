//! The write-ahead log (DESIGN §4.5).
//!
//! ```text
//! header   magic "MEMFWAL\0" (8) | version u32le | reserved u32le
//! record   len u32le | checksum u64le | payload[len]
//!          checksum = blake3(len_le ‖ payload)[..8]
//! ```
//!
//! The magic and version are there so a future format change is *detectable*
//! rather than silently misread: a file that does not begin with them is
//! refused by name, and a version this build does not know is refused rather
//! than guessed at.
//!
//! The checksum covers the length as well as the payload. A checksum over the
//! payload alone would leave a corrupted length undetectable, and a corrupted
//! length is the dangerous one: it decides how many bytes get read and how
//! much memory gets reserved. The length is capped besides.
//!
//! **A torn tail is normal.** A process killed mid-append leaves a partial
//! record, which says nothing about the records before it. Reading stops at
//! the last record that checks out, and the file is truncated there. That is
//! not corruption and is not reported as such.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use memfork_core::{journal, Record};

/// Magic at the head of every WAL file.
pub const MAGIC: &[u8; 8] = b"MEMFWAL\0";

/// The format version this build writes and reads.
pub const VERSION: u32 = 1;

/// Length of the file header.
pub const HEADER_LEN: u64 = 16;

/// Bytes of framing around each record: the length and the checksum.
const FRAME_LEN: usize = 4 + 8;

/// Largest record this build will read.
///
/// A record is one commit's change set. Sixteen mebibytes is far more than a
/// plausible one and far less than an allocation worth worrying about, so a
/// corrupted length is refused rather than acted on.
pub const MAX_RECORD_LEN: u32 = 16 * 1024 * 1024;

/// When to ask the operating system to flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FsyncPolicy {
    /// Flush before every change becomes visible.
    ///
    /// The default. A change that MemFork has acknowledged has reached the
    /// disk, so nothing acknowledged is lost, not even to a power cut. It
    /// costs one flush per commit — on an SSD roughly 0.1 to 2 ms, so a few
    /// hundred to a few thousand commits a second, against the tens of
    /// thousands the in-memory engine manages. It risks nothing.
    #[default]
    Always,
    /// Flush at most once per interval.
    ///
    /// Batches the cost across commits. Risks losing the last window of
    /// acknowledged commits to a power cut or a kernel panic — but not to a
    /// process crash, since the writes have reached the operating system.
    Interval,
    /// Never flush explicitly.
    ///
    /// Fastest, and survives a process crash, because the operating system
    /// still has the bytes. Risks everything since the last snapshot if the
    /// machine itself stops.
    Never,
}

impl FsyncPolicy {
    /// Parse a `--fsync` value.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "always" => Some(FsyncPolicy::Always),
            "interval" => Some(FsyncPolicy::Interval),
            "never" => Some(FsyncPolicy::Never),
            _ => None,
        }
    }

    /// The name this policy parses from.
    pub fn as_str(self) -> &'static str {
        match self {
            FsyncPolicy::Always => "always",
            FsyncPolicy::Interval => "interval",
            FsyncPolicy::Never => "never",
        }
    }
}

/// Why the log could not be used.
#[derive(Debug, thiserror::Error)]
pub enum WalError {
    /// The file could not be read or written.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted.
        context: String,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// The file does not start with the MemFork WAL magic.
    #[error("{path} is not a MemFork write-ahead log (wrong magic bytes)")]
    NotAWal {
        /// The file that was opened.
        path: PathBuf,
    },
    /// The file is a WAL, but of a version this build does not know.
    #[error(
        "{path} is a MemFork write-ahead log of format version {found}; \
         this build reads version {VERSION}"
    )]
    UnknownVersion {
        /// The file that was opened.
        path: PathBuf,
        /// The version it declares.
        found: u32,
    },
    /// A record decoded as bytes but not as a record.
    #[error("the record at offset {at} is not a valid change: {source}")]
    BadRecord {
        /// Where the record started.
        at: u64,
        /// What the decoder said.
        #[source]
        source: journal::DecodeError,
    },
}

fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> WalError {
    let context = context.into();
    move |source| WalError::Io { context, source }
}

/// What reading a log produced.
#[derive(Debug)]
pub struct Replay {
    /// The records that were whole and checksummed correctly.
    pub records: Vec<Record>,
    /// Where the last good record ended. The file is valid up to here.
    pub valid_len: u64,
    /// Set when the file ended mid-record, with the reason.
    ///
    /// Expected after a crash, and not an error: the records before it are
    /// unaffected and the tail is discarded.
    pub torn_tail: Option<String>,
}

/// An append-only log of changes.
#[derive(Debug)]
pub struct Wal {
    path: PathBuf,
    file: File,
    policy: FsyncPolicy,
    /// How many records have been appended since the file was opened.
    appended: u64,
    /// Whether anything has been written since the last flush.
    dirty: bool,
}

impl Wal {
    /// Open a log for appending, creating it with a header if it is new.
    pub fn open(path: &Path, policy: FsyncPolicy) -> Result<Self, WalError> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(io(format!("cannot open {}", path.display())))?;

        let len = file
            .metadata()
            .map_err(io(format!("cannot inspect {}", path.display())))?
            .len();
        if len == 0 {
            write_header(&mut file, path)?;
        } else {
            read_header(&mut file, path)?;
        }
        file.seek(SeekFrom::End(0))
            .map_err(io(format!("cannot seek to the end of {}", path.display())))?;

        Ok(Wal {
            path: path.to_path_buf(),
            file,
            policy,
            appended: 0,
            dirty: false,
        })
    }

    /// The file this log writes to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many records have been appended since opening.
    pub fn appended(&self) -> u64 {
        self.appended
    }

    /// Append one record, flushing according to the policy.
    pub fn append(&mut self, record: &Record) -> Result<(), WalError> {
        let payload = journal::encode(record);
        let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
        if len > MAX_RECORD_LEN {
            return Err(WalError::Io {
                context: format!(
                    "a single change came to {} bytes, over the {MAX_RECORD_LEN}-byte limit",
                    payload.len()
                ),
                source: std::io::Error::new(std::io::ErrorKind::InvalidData, "record too large"),
            });
        }

        // One write call, so a crash can tear the record but cannot interleave
        // it with another.
        let mut framed = Vec::with_capacity(FRAME_LEN + payload.len());
        framed.extend_from_slice(&len.to_le_bytes());
        framed.extend_from_slice(&checksum(len, &payload).to_le_bytes());
        framed.extend_from_slice(&payload);

        self.file
            .write_all(&framed)
            .map_err(io(format!("cannot append to {}", self.path.display())))?;
        self.appended += 1;
        self.dirty = true;

        if self.policy == FsyncPolicy::Always {
            self.flush()?;
        }
        Ok(())
    }

    /// Flush to disk, whatever the policy.
    pub fn flush(&mut self) -> Result<(), WalError> {
        if !self.dirty {
            return Ok(());
        }
        self.file
            .sync_data()
            .map_err(io(format!("cannot flush {}", self.path.display())))?;
        self.dirty = false;
        Ok(())
    }

    /// Whether there is unflushed data.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
}

impl Drop for Wal {
    fn drop(&mut self) {
        // A clean shutdown should not lose a commit that a looser policy had
        // only buffered. A failure here cannot be reported, but the data is no
        // worse off than if the process had been killed.
        let _ = self.flush();
    }
}

/// `blake3(len ‖ payload)`, truncated to eight bytes.
///
/// BLAKE3 rather than a CRC because it is already a dependency: a checksum
/// crate would be a new one for no gain at these sizes. Eight bytes is far
/// beyond what accidental corruption defeats, and this is not a defence
/// against a deliberate attacker — anyone who can write to the log can write a
/// valid checksum too.
fn checksum(len: u32, payload: &[u8]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&len.to_le_bytes());
    hasher.update(payload);
    let hash = hasher.finalize();
    let mut eight = [0u8; 8];
    eight.copy_from_slice(&hash.as_bytes()[..8]);
    u64::from_le_bytes(eight)
}

fn write_header(file: &mut File, path: &Path) -> Result<(), WalError> {
    let mut header = Vec::with_capacity(HEADER_LEN as usize);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&VERSION.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    file.write_all(&header)
        .map_err(io(format!("cannot write the header of {}", path.display())))?;
    file.sync_data()
        .map_err(io(format!("cannot flush the header of {}", path.display())))
}

fn read_header(file: &mut File, path: &Path) -> Result<(), WalError> {
    file.seek(SeekFrom::Start(0))
        .map_err(io(format!("cannot seek in {}", path.display())))?;
    let mut header = [0u8; HEADER_LEN as usize];
    file.read_exact(&mut header).map_err(|source| {
        if source.kind() == std::io::ErrorKind::UnexpectedEof {
            WalError::NotAWal {
                path: path.to_path_buf(),
            }
        } else {
            WalError::Io {
                context: format!("cannot read the header of {}", path.display()),
                source,
            }
        }
    })?;
    if &header[..8] != MAGIC {
        return Err(WalError::NotAWal {
            path: path.to_path_buf(),
        });
    }
    let found = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    if found != VERSION {
        return Err(WalError::UnknownVersion {
            path: path.to_path_buf(),
            found,
        });
    }
    Ok(())
}

/// Read every whole record from a log.
///
/// Stops at the first record that is short or fails its checksum, reporting it
/// as a torn tail rather than an error. Truncation is left to the caller, so a
/// read-only inspection stays read-only.
pub fn read(path: &Path) -> Result<Replay, WalError> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Replay {
                records: Vec::new(),
                valid_len: 0,
                torn_tail: None,
            })
        }
        Err(e) => return Err(io(format!("cannot open {}", path.display()))(e)),
    };
    read_header(&mut file, path)?;

    let mut reader = BufReader::new(file);
    let mut records = Vec::new();
    let mut offset = HEADER_LEN;

    loop {
        let mut frame = [0u8; FRAME_LEN];
        match read_full(&mut reader, &mut frame) {
            Ok(FRAME_LEN) => {}
            Ok(0) => break, // a clean end
            Ok(short) => {
                return Ok(Replay {
                    records,
                    valid_len: offset,
                    torn_tail: Some(format!(
                        "the file ends {short} byte(s) into a record header at offset {offset}"
                    )),
                })
            }
            Err(e) => return Err(io(format!("cannot read {}", path.display()))(e)),
        }

        let len = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
        let stored = u64::from_le_bytes([
            frame[4], frame[5], frame[6], frame[7], frame[8], frame[9], frame[10], frame[11],
        ]);

        if len > MAX_RECORD_LEN {
            return Ok(Replay {
                records,
                valid_len: offset,
                torn_tail: Some(format!(
                    "the record at offset {offset} claims {len} bytes, over the \
                     {MAX_RECORD_LEN}-byte limit"
                )),
            });
        }

        let mut payload = vec![0u8; len as usize];
        match read_full(&mut reader, &mut payload) {
            Ok(n) if n as u32 == len => {}
            Ok(n) => {
                return Ok(Replay {
                    records,
                    valid_len: offset,
                    torn_tail: Some(format!(
                        "the record at offset {offset} wanted {len} bytes and the file has {n}"
                    )),
                })
            }
            Err(e) => return Err(io(format!("cannot read {}", path.display()))(e)),
        }

        if checksum(len, &payload) != stored {
            return Ok(Replay {
                records,
                valid_len: offset,
                torn_tail: Some(format!(
                    "the record at offset {offset} does not match its checksum"
                )),
            });
        }

        match journal::decode(&payload) {
            Ok(record) => records.push(record),
            // The bytes are intact — the checksum says so — but they do not
            // mean anything to this build. That is a different problem from a
            // torn tail and is reported as one.
            Err(source) => return Err(WalError::BadRecord { at: offset, source }),
        }
        offset += FRAME_LEN as u64 + u64::from(len);
    }

    Ok(Replay {
        records,
        valid_len: offset,
        torn_tail: None,
    })
}

/// Read until the buffer is full or the input ends, returning how much arrived.
fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Cut a log back to its last whole record.
pub fn truncate(path: &Path, valid_len: u64) -> Result<(), WalError> {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(io(format!("cannot open {} to truncate it", path.display())))?;
    file.set_len(valid_len)
        .map_err(io(format!("cannot truncate {}", path.display())))?;
    file.sync_all().map_err(io(format!(
        "cannot flush the truncation of {}",
        path.display()
    )))
}

/// Start a new, empty log, replacing whatever was there.
pub fn reset(path: &Path) -> Result<(), WalError> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(io(format!("cannot create {}", path.display())))?;
    write_header(&mut file, path)
}
