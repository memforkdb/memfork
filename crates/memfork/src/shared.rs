//! What every session in one process shares besides the store: the task
//! board's leases, the side structure of statistics and fact hashes, and the
//! file hasher's cache.
//!
//! One per daemon, one per `--ephemeral` process. Nothing in it is part of
//! memory's history.

use std::sync::Arc;

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
}

impl Shared {
    /// One kept only in memory.
    pub fn in_memory() -> Arc<Self> {
        Arc::new(Shared::default())
    }
}
