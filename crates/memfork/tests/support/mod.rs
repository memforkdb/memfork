//! Shared test scaffolding.
//!
//! Every test and every process a test starts runs against a temporary data
//! directory, and carries the guard that makes falling back to the real
//! per-user directory a hard failure. That guard exists because this went
//! wrong once: a change of default turned harmless tests into something
//! that wrote into the developer's own store. Being careful is not a control;
//! the guard is.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use memfork::persist::datadir::{DATA_DIR_ENV, FORBID_PER_USER_ENV};

/// A temporary data directory, and processes pointed at it.
pub struct Sandbox {
    dir: tempfile::TempDir,
    children: Vec<Child>,
    /// Kept apart from `dir`, so "nothing else was written" checks never see
    /// a process's error output.
    logs: tempfile::TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("data")).expect("data dir");
        std::fs::create_dir_all(dir.path().join("home")).expect("home dir");
        std::fs::create_dir_all(dir.path().join("bin")).expect("bin dir");
        Sandbox {
            dir,
            children: Vec::new(),
            logs: tempfile::tempdir().expect("logs dir"),
        }
    }

    /// The data directory every process in this sandbox uses.
    pub fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// A `memfork` command confined to this sandbox.
    ///
    /// Sets the data directory explicitly *and* forbids the real one, so a
    /// command that somehow ignores the first still cannot reach the second.
    /// `PATH` and the home directory are confined too: a test that asks about
    /// installed clients must see the shims it put there and nothing the
    /// developer happens to have installed.
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin("memfork"));
        cmd.env(DATA_DIR_ENV, self.data())
            .env(FORBID_PER_USER_ENV, "1")
            .env("MEMFORK_HOME", self.home())
            .env("PATH", self.path_value())
            // A namespace the developer happens to have set must not decide
            // what a test sees; tests that want one set it themselves.
            .env_remove(memfork::namespace::NAMESPACE_ENV)
            .current_dir(self.root());
        cmd
    }

    /// Start a process in this sandbox and keep the handle, so the test can
    /// kill it and so nothing outlives the test.
    pub fn spawn(&mut self, args: &[&str]) -> usize {
        let child = self
            .command()
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawned");
        self.children.push(child);
        self.children.len() - 1
    }

    /// Start a process like [`Sandbox::spawn`], keeping what it writes to
    /// stderr for [`Sandbox::stderr_of`], so a test that waits for it can say
    /// why it never came.
    pub fn spawn_keeping_stderr(&mut self, args: &[&str]) -> usize {
        let index = self.children.len();
        let log = std::fs::File::create(self.logs.path().join(format!("{index}.stderr")))
            .expect("stderr log");
        let child = self
            .command()
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .expect("spawned");
        self.children.push(child);
        index
    }

    /// What a process started with [`Sandbox::spawn_keeping_stderr`] has
    /// written to stderr so far.
    pub fn stderr_of(&self, index: usize) -> String {
        std::fs::read_to_string(self.logs.path().join(format!("{index}.stderr")))
            .unwrap_or_default()
    }

    /// Kill one of the processes this sandbox started.
    pub fn kill(&mut self, index: usize) {
        if let Some(child) = self.children.get_mut(index) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Whether a process this sandbox started is still running.
    pub fn is_running(&mut self, index: usize) -> bool {
        match self.children.get_mut(index) {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Wait for a process this sandbox started to exit.
    ///
    /// Releasing the data directory and finishing exiting are not the same
    /// instant: the lock goes when the store is dropped, and the process takes
    /// a moment longer to wind down. `memfork stop` promises the first, which
    /// is what a caller needs, so a test about the second has to wait for it.
    pub fn wait_for_exit(&mut self, index: usize, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if !self.is_running(index) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Who owns the data directory right now, if anyone.
    pub fn owner(&self) -> Option<memfork::persist::Endpoint> {
        memfork::persist::lock::owner(&self.data())
    }

    /// Wait for a daemon to be listening, and return its endpoint.
    pub fn wait_for_daemon(&self, within: Duration) -> Option<memfork::persist::Endpoint> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Some(endpoint) = self.owner() {
                if endpoint.port.is_some() {
                    return Some(endpoint);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    /// Wait for the data directory to have no owner.
    pub fn wait_for_no_daemon(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.owner().is_none() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Every file under the sandbox, for "nothing else was written" checks.
    pub fn files(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk(self.dir.path(), &mut out);
        out.sort();
        out
    }

    /// The home directory every process in this sandbox sees.
    pub fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    /// The only directory on the `PATH` these processes get.
    pub fn bin(&self) -> PathBuf {
        self.dir.path().join("bin")
    }

    /// `PATH` with just this sandbox's shims on it.
    pub fn path_value(&self) -> String {
        confined_path(&self.bin())
    }

    /// A fake client command that health-checks the server the way real ones
    /// do: by launching `memfork mcp`, feeding it `handshake` and reading the
    /// replies.
    ///
    /// That is the whole point of it. `claude mcp get` starts the server to
    /// see whether it answers, so anything `memfork mcp` does merely to shake
    /// hands — such as starting a daemon — happens every time another command
    /// asks a client a question about MemFork.
    ///
    /// It records that it ran, and reports MemFork as registered.
    pub fn add_probing_shim(&self, name: &str, handshake: &Path) {
        let marker = self.dir.path().join(format!("{name}.ran"));
        let binary = assert_cmd::cargo::cargo_bin("memfork");
        if cfg!(target_os = "windows") {
            let path = self.bin().join(format!("{name}.cmd"));
            std::fs::write(
                &path,
                format!(
                    "@echo off\r\n\
                     echo ran>\"{marker}\"\r\n\
                     \"{binary}\" mcp <\"{handshake}\" >nul 2>&1\r\n\
                     echo memfork\r\n\
                     exit /b 0\r\n",
                    marker = marker.display(),
                    binary = binary.display(),
                    handshake = handshake.display()
                ),
            )
            .expect("shim written");
        } else {
            let path = self.bin().join(name);
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\n\
                     echo ran > '{marker}'\n\
                     '{binary}' mcp < '{handshake}' >/dev/null 2>&1\n\
                     echo memfork\n\
                     exit 0\n",
                    marker = marker.display(),
                    binary = binary.display(),
                    handshake = handshake.display()
                ),
            )
            .expect("shim written");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("shim made executable");
            }
        }
    }

    /// Whether a shim added by `add_probing_shim` was actually invoked.
    pub fn shim_ran(&self, name: &str) -> bool {
        self.dir.path().join(format!("{name}.ran")).exists()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Nothing this test started may outlive it, or the next test inherits
        // a daemon holding a directory it knows nothing about.
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Including a daemon started by autostart, which has no handle here.
        let _ = memfork::daemon::stop(&self.data());
    }
}

/// A `PATH` holding one directory, plus what Windows needs in order to start
/// a process at all.
pub fn confined_path(bin: &Path) -> String {
    let mut entries = vec![bin.display().to_string()];
    if cfg!(target_os = "windows") {
        if let Ok(root) = std::env::var("SystemRoot") {
            entries.push(format!("{root}\\System32"));
        }
    }
    entries.join(if cfg!(target_os = "windows") {
        ";"
    } else {
        ":"
    })
}

/// The MemFork binary these tests run.
///
/// Named here rather than in a test, because `guard.rs` forbids a test naming
/// it: the rule that keeps every command confined is that `support` builds
/// them all.
pub fn memfork_binary() -> PathBuf {
    assert_cmd::cargo::cargo_bin("memfork")
}

/// A `memfork` command for a test that does not care where data goes.
///
/// Still confined, in every direction a command can reach out in: the guard is
/// set, the data directory is a temporary one shared by this test binary, and
/// `PATH` and the home directory point at empty temporary ones. Tests use this
/// rather than building a `Command` themselves, and
/// `no_test_builds_memfork_commands_directly` in `guard.rs` fails the build if
/// one does.
///
/// The `PATH` part is not caution for its own sake. `memfork doctor` asks each
/// installed client whether MemFork is registered by running that client's own
/// command, and a real client answers by *launching the registered server* —
/// so a test that ran doctor on the developer's `PATH` started a MemFork from
/// outside the build, pointed it at the test's own data directory, and waited
/// on it. That is how this suite came to hang.
pub fn memfork() -> Command {
    use std::sync::OnceLock;
    static SCRATCH: OnceLock<PathBuf> = OnceLock::new();
    let scratch = SCRATCH.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("memfork-tests-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("data"));
        let _ = std::fs::create_dir_all(dir.join("home"));
        let _ = std::fs::create_dir_all(dir.join("bin"));
        dir
    });

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin("memfork"));
    cmd.env(DATA_DIR_ENV, scratch.join("data"))
        .env(FORBID_PER_USER_ENV, "1")
        .env("MEMFORK_HOME", scratch.join("home"))
        .env("PATH", confined_path(&scratch.join("bin")))
        .env_remove(memfork::namespace::NAMESPACE_ENV);
    cmd
}

/// The same, as `assert_cmd`'s wrapper, for tests that assert on output.
pub fn memfork_assert() -> assert_cmd::Command {
    assert_cmd::Command::from_std(memfork())
}

/// Where the real per-user data directory would be, if the guard let us.
///
/// Used by the test that proves this suite cannot touch it.
pub fn real_per_user_dir() -> Option<PathBuf> {
    use memfork::clients::Os;
    use memfork::persist::datadir::{per_user, RealEnv};
    per_user(Os::current(), &RealEnv)
}
