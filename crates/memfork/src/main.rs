//! The `memfork` binary (DESIGN §5).
//!
//! A shell, and deliberately nothing more. Everything the command does lives
//! in [`memfork::run`], because the wheel on PyPI offers the same command
//! through a Python entry point and the two must not drift apart.

use std::process::ExitCode;

fn main() -> ExitCode {
    memfork::run::from_env()
}
