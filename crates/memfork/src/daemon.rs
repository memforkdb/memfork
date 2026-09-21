//! Finding, starting and stopping the daemon.
//!
//! A user should never have to run `memfork serve` by hand. Two clients at
//! once is the normal case, and the second one failing with "in use by process
//! N" would be a bug report, not a feature. So a persistent `memfork mcp`
//! finds the daemon or starts one, and then proxies to it either way.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::launch::Launch;
use crate::persist::{lock, Endpoint};

/// How long to wait for a daemon we started to publish its endpoint.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// How often to look while waiting.
const POLL: Duration = Duration::from_millis(25);

/// Why the daemon could not be used.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DaemonError {
    /// The daemon did not come up.
    #[error("the MemFork daemon did not start within {}s: {reason}", STARTUP_TIMEOUT.as_secs())]
    DidNotStart {
        /// What was observed.
        reason: String,
    },
    /// A daemon is running, but it is a different build.
    #[error(
        "a MemFork daemon from version {theirs} already owns {dir}, and this is \
         version {ours}.\n\
         Two versions must not share one store: the formats they write need not \
         match, and guessing is how data gets corrupted.\n\
         Run `memfork stop` to shut the running one down, then try again."
    )]
    VersionMismatch {
        /// The version that is running.
        theirs: String,
        /// The version that wanted to connect.
        ours: String,
        /// The data directory in question.
        dir: String,
    },
    /// Something went wrong starting the process.
    #[error("cannot start the MemFork daemon: {0}")]
    Spawn(String),
}

/// A daemon that is running and safe for this build to talk to.
pub fn usable(dir: &Path) -> Result<Option<Endpoint>, DaemonError> {
    let Some(endpoint) = lock::owner(dir) else {
        return Ok(None);
    };
    // An owner with no port is a single process holding the
    // directory, not a daemon. Treat it as "nothing to connect to" and let the
    // caller's own lock attempt produce the clear "in use" message.
    if endpoint.port.is_none() {
        return Ok(Some(endpoint));
    }
    check_version(dir, &endpoint)?;
    Ok(Some(endpoint))
}

/// Refuse to talk to a daemon from a different build.
///
/// Silently talking across versions is the failure that produces a corrupt
/// store and an unreproducible bug report. The formats two versions write need
/// not match, so the only safe answer is to say so and name the way out.
fn check_version(dir: &Path, endpoint: &Endpoint) -> Result<(), DaemonError> {
    let theirs = endpoint.memfork_version.as_deref().unwrap_or("unknown");
    if theirs != crate::VERSION {
        return Err(DaemonError::VersionMismatch {
            theirs: theirs.to_owned(),
            ours: crate::VERSION.to_owned(),
            dir: dir.display().to_string(),
        });
    }
    Ok(())
}

/// Get a daemon for this data directory, starting one if there is none.
///
/// Racing is expected: two clients starting at the same moment both find no
/// daemon and both spawn one. Exactly one wins the data directory lock; the
/// other exits without touching the log, and both callers then find the
/// winner's endpoint. So a failure to start is only a failure if no endpoint
/// appears at all.
pub fn ensure(dir: &Path, launch: &Launch, idle_seconds: u64) -> Result<Endpoint, DaemonError> {
    if let Some(endpoint) = usable(dir)? {
        if endpoint.port.is_some() {
            return Ok(endpoint);
        }
    }

    spawn(dir, launch, idle_seconds)?;

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut last = String::from("no endpoint file appeared");
    while Instant::now() < deadline {
        match usable(dir) {
            Ok(Some(endpoint)) if endpoint.port.is_some() => return Ok(endpoint),
            Ok(Some(_)) => last = "something owns the directory but is not serving".to_owned(),
            Ok(None) => {}
            // A version mismatch is a real answer, not something to wait out.
            Err(e @ DaemonError::VersionMismatch { .. }) => return Err(e),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(POLL);
    }
    Err(DaemonError::DidNotStart { reason: last })
}

/// Start a detached daemon.
///
/// Detached with safe standard-library calls only: a new process group on
/// Unix, `CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS` on Windows, and null
/// standard streams on both. The daemon must outlive the client that started
/// it and must never write to the client's stdout, which on a proxy is
/// carrying the MCP protocol.
fn spawn(dir: &Path, launch: &Launch, idle_seconds: u64) -> Result<(), DaemonError> {
    // Before anything is spawned: on Windows a new process inherits every
    // inheritable handle its parent holds, and this parent holds the client's
    // pipes. A daemon that inherited them would keep the client's stdout open
    // for its whole life, so a client waiting for that pipe to close would
    // wait for ever — after `memfork mcp` had already exited.
    #[cfg(windows)]
    windows_handles::stop_inheriting_std_handles();

    // Not `current_exe`: a MemFork installed from a wheel is running inside a
    // Python interpreter, and spawning that would start Python rather than a
    // daemon. `launch` knows the difference.
    let argv = launch.argv(&[
        "serve",
        "--data-dir",
        &dir.display().to_string(),
        "--port",
        "0",
        "--idle-timeout",
        &idle_seconds.to_string(),
    ]);

    // A front door that can start a process more cleanly than this one says
    // so. Only the Python one does: clearing the standard handles above is
    // enough for the binary, but a wheel install also carries duplicates of
    // the client's pipes that the launcher and the virtualenv redirector made,
    // and nothing here can name them to clear those. Python can refuse to pass
    // any of it on, so in that case Python starts the daemon.
    if let Some(helper) = spawn_helper() {
        return spawn_through(&helper, &argv);
    }

    let (program, args) = argv.split_first().ok_or_else(|| {
        DaemonError::Spawn("there is no command to start a daemon with".to_owned())
    })?;
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, so a Ctrl-C in the client's terminal does not
        // take the daemon with it.
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }

    command
        .spawn()
        .map(|_| ())
        .map_err(|e| DaemonError::Spawn(e.to_string()))
}

/// How a front door says it can start a process more cleanly than we can.
///
/// The value is a JSON array of strings, the command to run; the daemon's own
/// command line is appended to it as one JSON argument.
pub const SPAWN_VIA_ENV: &str = "MEMFORK_SPAWN_VIA";

fn spawn_helper() -> Option<Vec<String>> {
    let raw = std::env::var(SPAWN_VIA_ENV).ok()?;
    let parts: Vec<String> = serde_json::from_str(&raw).ok()?;
    (!parts.is_empty()).then_some(parts)
}

/// Start the daemon through the helper, and wait for the helper to finish.
///
/// Waiting is the point: the helper is what holds the handles this process
/// could not clear, and they are released when it exits. It starts the daemon
/// and returns immediately, so this is a wait of milliseconds.
fn spawn_through(helper: &[String], argv: &[String]) -> Result<(), DaemonError> {
    let command = serde_json::to_string(argv)
        .map_err(|e| DaemonError::Spawn(format!("cannot encode the daemon command: {e}")))?;
    let (program, args) = helper
        .split_first()
        .ok_or_else(|| DaemonError::Spawn("the spawn helper is empty".to_owned()))?;

    let output = std::process::Command::new(program)
        .args(args)
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| DaemonError::Spawn(format!("cannot run the spawn helper: {e}")))?;

    if output.status.success() {
        return Ok(());
    }
    Err(DaemonError::Spawn(format!(
        "the spawn helper failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// The one place in MemFork that needs the Windows API directly.
///
/// Unix has nothing to do here: the standard library opens every descriptor
/// close-on-exec, so a child gets only the three it is given. Windows inherits
/// by handle flag instead, and the standard library offers no way to say "not
/// this one" — so the flags are cleared here, with three FFI calls and no
/// allocation, rather than leaving the daemon holding a client's pipe.
#[cfg(windows)]
mod windows_handles {
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    /// Stop this process's standard handles from being inherited by children.
    ///
    /// Affects children started afterwards and nothing else: this process goes
    /// on reading and writing its own streams exactly as before.
    #[allow(unsafe_code)]
    pub(super) fn stop_inheriting_std_handles() {
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: `GetStdHandle` returns a handle this process owns, or an
            // invalid one, and `SetHandleInformation` is defined for both —
            // it fails, harmlessly, on the invalid case. No memory is read or
            // written through either, and neither can unwind.
            unsafe {
                let handle = GetStdHandle(which);
                if !handle.is_null() {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
    }
}

/// Post the shutdown request, over a plain socket.
///
/// Written by hand rather than with the HTTP client, because this has to work
/// from anywhere — including a `Drop`, and including inside an async runtime,
/// where building a second one panics. One request with no body and one status
/// line back is not worth a runtime.
fn post_shutdown(port: u16, token: &str) -> Result<(), String> {
    use std::io::{Read, Write};

    let address = format!("127.0.0.1:{port}");
    let mut stream = std::net::TcpStream::connect(&address)
        .map_err(|e| format!("cannot reach the daemon on {address}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("cannot set a timeout on the connection: {e}"))?;

    // CRLF, and written out rather than assembled with `\n`: HTTP/1.1 requires
    // it, and a request with bare newlines is refused by a strict parser.
    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n",
        path = crate::serve::SHUTDOWN_PATH,
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("cannot send the shutdown request: {e}"))?;

    let mut answer = String::new();
    // The daemon may close mid-answer as it shuts down, which is success, not
    // a failure: the request was delivered.
    let _ = stream.read_to_string(&mut answer);
    if answer.starts_with("HTTP/1.1 401") {
        return Err("the daemon rejected the token in its own endpoint file".to_owned());
    }
    Ok(())
}

/// Ask a running daemon to stop.
///
/// Returns what happened, in words fit for printing.
pub fn stop(dir: &Path) -> Result<String, String> {
    let Some(endpoint) = lock::owner(dir) else {
        return Ok(format!("nothing is running on {}", dir.display()));
    };
    let (Some(port), Some(token)) = (endpoint.port, endpoint.token.clone()) else {
        return Err(format!(
            "process {} owns {} but is not a daemon, so there is nothing to stop.\n\
             It will release the directory when it exits.",
            endpoint.pid,
            dir.display()
        ));
    };

    post_shutdown(port, &token)?;

    // Wait for it to let go, so `memfork stop` returning means stopped.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if lock::owner(dir).is_none() {
            return Ok(format!(
                "stopped the daemon (process {}) on {}",
                endpoint.pid,
                dir.display()
            ));
        }
        std::thread::sleep(POLL);
    }
    Err(format!(
        "asked process {} to stop, and it still holds {} ten seconds later",
        endpoint.pid,
        dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_running_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(usable(dir.path()).expect("checked").is_none());
        assert!(stop(dir.path())
            .expect("stop")
            .contains("nothing is running"));
    }

    #[test]
    fn a_daemon_from_another_version_is_refused_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = Endpoint {
            memfork_version: Some("0.0.1-from-the-past".to_owned()),
            port: Some(1234),
            token: Some("t".to_owned()),
            ..Endpoint::for_this_process()
        };
        match check_version(dir.path(), &endpoint) {
            Err(DaemonError::VersionMismatch { theirs, ours, .. }) => {
                assert_eq!(theirs, "0.0.1-from-the-past");
                assert_eq!(ours, crate::VERSION);
            }
            other => panic!("a foreign version was accepted: {other:?}"),
        }
    }

    #[test]
    fn a_daemon_that_declares_no_version_is_also_refused() {
        // An endpoint file from before versions were recorded. Assuming it is
        // compatible is the guess this check exists to avoid.
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = Endpoint {
            memfork_version: None,
            port: Some(1234),
            token: Some("t".to_owned()),
            ..Endpoint::for_this_process()
        };
        assert!(check_version(dir.path(), &endpoint).is_err());
    }

    #[test]
    fn our_own_version_is_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = Endpoint {
            memfork_version: Some(crate::VERSION.to_owned()),
            port: Some(1234),
            token: Some("t".to_owned()),
            ..Endpoint::for_this_process()
        };
        assert!(check_version(dir.path(), &endpoint).is_ok());
    }

    #[test]
    fn the_mismatch_message_says_what_to_do_about_it() {
        let err = DaemonError::VersionMismatch {
            theirs: "0.0.1".to_owned(),
            ours: "9.9.9".to_owned(),
            dir: "/tmp/x".to_owned(),
        };
        let text = err.to_string();
        assert!(text.contains("memfork stop"), "{text}");
        assert!(text.contains("0.0.1") && text.contains("9.9.9"), "{text}");
    }
}
