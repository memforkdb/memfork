//! What every session in one process shares besides the store: the task
//! board's leases, the side structure of statistics and fact hashes, the
//! file hasher's cache, and the directory of connected sessions autopilot acts
//! on.
//!
//! One per daemon, one per `--ephemeral` process. Nothing in it is part of
//! memory's history.

use std::sync::Arc;

use crate::autopilot::engine::Directory;
use crate::board::Board;
use crate::facts::Hasher;
use crate::sidecar::Sidecar;

/// The shared side of a process.
#[derive(Debug, Default)]
pub struct Shared {
    /// Task claims and their leases.
    pub board: Board,
    /// Statistics and recorded fact hashes.
    pub sidecar: Sidecar,
    /// Content hashes of source files, cached.
    pub hasher: Hasher,
    /// The connected sessions, for autopilot.
    pub sessions: Directory,
}

impl Shared {
    /// One kept only in memory.
    pub fn in_memory() -> Arc<Self> {
        Arc::new(Shared::default())
    }
}
