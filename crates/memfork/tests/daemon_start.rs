//! Starting the daemon: how long it may take, what is said while it does,
//! what is said when it cannot, and what it must never hold on to.
//!
//! Everything runs in a sandbox, and every daemon a test starts is stopped
//! when the sandbox goes. A daemon that cannot start is simulated through
//! `MEMFORK_LAUNCH`, which names the program the daemon is started as.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use memfork::daemon::{LOG_FILE, START_TIMEOUT_ENV};
use memfork::launch::LAUNCH_ENV;
use support::Sandbox;

/// `MEMFORK_LAUNCH` for a program and the arguments that go before `serve`.
fn launch_as(parts: &[&str]) -> String {
    serde_json::to_string(parts).unwrap()
}

fn memfork_path() -> String {
    support::memfork_binary().display().to_string()
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_daemon_that_dies_on_arrival_is_reported_at_once_with_its_own_words() {
    let sandbox = Sandbox::new();
    let started = Instant::now();
    let out = sandbox
        .command()
        .env(LAUNCH_ENV, launch_as(&[&memfork_path(), "--no-such-flag"]))
        .args(["ls"])
        .output()
        .unwrap();
    let err = stderr(&out);
    assert!(!out.status.success());
    // Not the full timeout: the process was seen to exit.
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    assert!(err.contains("did not start"), "{err}");
    assert!(err.contains("exited with code"), "{err}");
    // What was tried, where its output went, and its own complaint.
    assert!(
        err.contains("Tried:") && err.contains("--no-such-flag serve"),
        "{err}"
    );
    assert!(err.contains(LOG_FILE), "{err}");
    assert!(err.contains("unexpected argument"), "{err}");
    // And what to do next.
    assert!(err.contains("memfork doctor"), "{err}");
    assert!(err.contains(START_TIMEOUT_ENV), "{err}");
    assert!(sandbox.data().join(LOG_FILE).exists());
}

/// The full path of an interpreter that can run a one-line sleeper, if this
/// machine has one. A full path, because the sandbox's PATH is empty.
fn python() -> Option<String> {
    ["python3", "python", "py"]
        .into_iter()
        .find_map(|candidate| {
            let out = Command::new(candidate)
                .args(["-c", "import sys; print(sys.executable)"])
                .stderr(Stdio::null())
                .output()
                .ok()?;
            let path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            (out.status.success() && !path.is_empty()).then_some(path)
        })
}

#[test]
fn the_timeout_can_be_raised_or_lowered_and_a_slow_start_is_mentioned_once() {
    let Some(py) = python() else {
        eprintln!("no Python here to stand in for a daemon that never serves; skipped");
        return;
    };
    let sandbox = Sandbox::new();
    // A "daemon" that neither serves nor exits: every argument after the
    // script is ignored.
    let sleeper = launch_as(&[&py, "-c", "import time; time.sleep(30)"]);
    let started = Instant::now();
    let out = sandbox
        .command()
        .env(LAUNCH_ENV, &sleeper)
        .env(START_TIMEOUT_ENV, "5")
        .args(["ls"])
        .output()
        .unwrap();
    let waited = started.elapsed();
    let err = stderr(&out);
    assert!(!out.status.success());
    assert!(err.contains("did not start within 5s"), "{err}");
    assert!(err.contains("no endpoint file appeared"), "{err}");
    assert!(
        waited >= Duration::from_secs(5) && waited < Duration::from_secs(30),
        "{waited:?}"
    );
    // Past a few seconds, one plain line says it is still going.
    assert_eq!(
        err.matches("still starting the MemFork daemon").count(),
        1,
        "{err}"
    );
}

#[test]
fn an_unusable_timeout_is_refused_by_name() {
    let sandbox = Sandbox::new();
    for bad in ["soon", "0", "-5"] {
        let out = sandbox
            .command()
            .env(START_TIMEOUT_ENV, bad)
            .args(["ls"])
            .output()
            .unwrap();
        let err = stderr(&out);
        assert!(!out.status.success(), "{bad} was accepted");
        assert!(
            err.contains(START_TIMEOUT_ENV) && err.contains(bad),
            "{err}"
        );
        assert!(sandbox.owner().is_none(), "a daemon started anyway");
    }
}

#[test]
fn a_command_that_starts_the_daemon_returns_promptly_with_its_output_captured() {
    // `output()` waits for the child's stdout and stderr to close. If the
    // daemon this command starts had inherited either, it would hold them
    // open for its whole life and this would block until it idled out.
    let sandbox = Sandbox::new();
    for round in 0..2 {
        let started = Instant::now();
        let mut cmd = sandbox.command();
        cmd.args(["ls"]).stdin(Stdio::null());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(cmd.output());
        });
        let out = match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(out) => out.unwrap(),
            Err(_) => {
                // Release whatever is holding the pipes, then fail.
                let _ = memfork::daemon::stop(&sandbox.data());
                panic!(
                    "round {round}: the command's pipes stayed open for {:?}: the daemon \
                     it started is holding them",
                    started.elapsed()
                );
            }
        };
        assert!(out.status.success(), "{}", stderr(&out));
        // The pipes closed while the daemon is still running: it holds
        // neither.
        assert!(
            sandbox.owner().is_some(),
            "round {round}: no daemon running"
        );
        // Stop it, as `memfork stop` would, so the next round starts one again.
        let stopped = sandbox.command().arg("stop").output().unwrap();
        assert!(stopped.status.success(), "{}", stderr(&stopped));
        assert!(sandbox.wait_for_no_daemon(Duration::from_secs(20)));
    }
}
