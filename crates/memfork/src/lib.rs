//! The `memfork` command: an MCP server, a shared daemon and a command line.
//!
//! MemFork gives an agent branchable memory — store and recall things, fork
//! the whole of memory before a risky step, then merge the fork or discard it.
//! This crate is what a user installs; the engine underneath is
//! [`memfork_core`].
//!
//! Most people never call this library. They install the binary and run:
//!
//! ```sh
//! memfork init      # register the MCP server with the clients they have
//! memfork doctor    # what is installed, and what is talking to it
//! ```
//!
//! The library exists because the binary is a thin shell over it, which lets
//! each piece be tested directly rather than only through a subprocess — and
//! because the Python package on PyPI offers the same command through an entry
//! point, so the two must not drift apart.
//!
//! ```no_run
//! // What `memfork mcp` does, minus the argument parsing.
//! use std::sync::Arc;
//! use memfork::tools::dispatch::Session;
//!
//! # async fn example() -> Result<(), String> {
//! let session = Arc::new(Session::new(memfork_core::Db::new()));
//! memfork::mcp::serve_stdio(session).await
//! # }
//! ```
//!
//! # What is where
//!
//! - [`tools`] is the tool registry: one set of definitions serving the MCP
//!   server, the vendor function-calling formats and `memfork call`.
//! - [`mcp`] serves those tools over stdio, and [`serve`] serves them over
//!   loopback HTTP for the daemon.
//! - [`proxy`] is what a client actually talks to: it answers the handshake
//!   itself and forwards tool calls to the daemon, starting one if none is
//!   running.
//! - [`clients`] is the client adapter registry and the config editors behind
//!   [`init`], which registers MemFork with the MCP clients on the machine.
//! - [`doctor`] reports what this install is and what it is talking to.
//! - [`persist`] is the write-ahead log, snapshots and the lock that gives one
//!   process ownership of a data directory.
//! - [`launch`] works out how to start MemFork again, which is not the same
//!   question as "where is this executable" once a Python wheel is involved.
//! - [`run`] is the command line itself.
//!
//! # Memory is kept, and shared
//!
//! What an agent stores is written to a data directory and is still there next
//! time. Several clients can use one store at once: the first that needs it
//! starts a daemon that owns the directory, and the rest connect to that,
//! while each keeps its own current branch. `--ephemeral` keeps nothing and
//! shares nothing.
//!
//! The project README covers installation and the full command reference:
//! <https://github.com/memforkdb/memfork>. The design is in
//! [`docs/DESIGN.md`](https://github.com/memforkdb/memfork/blob/main/docs/DESIGN.md).

#![warn(missing_docs)]

pub mod board;
pub mod cli;
pub mod client;
pub mod clients;
pub mod daemon;
pub mod doctor;
pub mod events;
pub mod exec;
pub mod facts;
pub mod find;
pub mod history;
pub mod init;
pub mod launch;
pub mod lessons;
pub mod mcp;
pub mod namespace;
pub mod persist;
pub mod plans;
pub mod proxy;
pub mod render;
pub mod run;
pub mod secrets;
pub mod serve;
pub mod shared;
pub mod sidecar;
pub mod style;
pub mod tools;

/// The version of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
