//! How to start another MemFork.
//!
//! Two things need this and neither can guess: the proxy, which starts the
//! daemon, and `memfork init`, which tells a client what to run. Both used
//! `current_exe()`, which is right exactly when MemFork is its own binary.
//!
//! It is not, when MemFork was installed from a wheel. `pip install memfork`
//! puts a `memfork` command on the path that runs inside a Python process, so
//! `current_exe()` is a Python interpreter. Spawning that would start Python,
//! not a daemon, and registering it with a client would give the client a
//! command that does nothing.
//!
//! So a launch is a program *and* its leading arguments, and the Python front
//! door says what they are through [`LAUNCH_ENV`]. Everything else is a
//! fallback for the ordinary case, where the answer really is this executable.

use std::path::{Path, PathBuf};

/// How a front door says what to run to get another MemFork.
///
/// The value is a JSON array of strings: the program, then any arguments that
/// come before the subcommand — `["/venv/bin/python", "-m", "memfork"]`, or
/// just `["/venv/bin/memfork"]`.
pub const LAUNCH_ENV: &str = "MEMFORK_LAUNCH";

/// A command that starts MemFork, minus the subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// The program to run.
    pub program: PathBuf,
    /// Arguments that come before the subcommand.
    pub args: Vec<String>,
}

impl Launch {
    /// A launch that is just a program.
    pub fn program(path: impl Into<PathBuf>) -> Self {
        Launch {
            program: path.into(),
            args: Vec::new(),
        }
    }

    /// The whole command line, with `extra` appended.
    pub fn argv(&self, extra: &[&str]) -> Vec<String> {
        let mut out = vec![self.program.display().to_string()];
        out.extend(self.args.iter().cloned());
        out.extend(extra.iter().map(|a| (*a).to_owned()));
        out
    }

    /// The command line as a person would type it, for messages.
    pub fn display(&self, extra: &[&str]) -> String {
        self.argv(extra)
            .into_iter()
            .map(|part| {
                if part.contains(' ') {
                    format!("\"{part}\"")
                } else {
                    part
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// A [`std::process::Command`] for this launch, with `extra` appended.
    pub fn command(&self, extra: &[&str]) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.args).args(extra);
        cmd
    }

    /// Whether some text a client printed refers to this launch.
    ///
    /// Used to tell a registration that points here from one that points at
    /// another copy of MemFork. Windows compares paths without case, and a
    /// client may print the path with either separator or in quotes, so the
    /// comparison is deliberately loose: it asks whether the program appears
    /// in the text at all.
    pub fn is_named_in(&self, text: &str) -> bool {
        let needle = normalise(&self.program.display().to_string());
        !needle.is_empty() && normalise(text).contains(&needle)
    }
}

/// Compare paths the way both operating systems would.
fn normalise(text: &str) -> String {
    let text = text.replace('\\', "/");
    if cfg!(target_os = "windows") {
        text.to_ascii_lowercase()
    } else {
        text
    }
}

/// Work out how to start another MemFork.
pub fn resolve() -> Launch {
    from_env()
        .or_else(from_current_exe)
        .unwrap_or_else(fallback)
}

/// What a front door said, if one did.
fn from_env() -> Option<Launch> {
    let raw = std::env::var(LAUNCH_ENV).ok()?;
    let parts: Vec<String> = serde_json::from_str(&raw).ok()?;
    let (program, args) = parts.split_first()?;
    Some(Launch {
        program: PathBuf::from(program),
        args: args.to_vec(),
    })
}

/// This executable, when this executable is MemFork.
fn from_current_exe() -> Option<Launch> {
    let exe = std::env::current_exe().ok()?;
    if is_python(&exe) {
        // Running inside an interpreter with nobody having said how to get
        // back out. The console script a wheel installs sits beside the
        // interpreter, so look there before giving up on the path.
        return beside(&exe).or_else(|| crate::init::which("memfork").map(Launch::program));
    }
    Some(Launch::program(exe))
}

/// A `memfork` command installed alongside this interpreter.
fn beside(exe: &Path) -> Option<Launch> {
    let dir = exe.parent()?;
    // A virtualenv keeps scripts in `bin` on Unix and `Scripts` on Windows,
    // which is where the interpreter lives too — except for the base install
    // on Windows, where the interpreter is one level up.
    let candidates = [dir.to_path_buf(), dir.join("Scripts"), dir.join("bin")];
    let names: &[&str] = if cfg!(target_os = "windows") {
        &["memfork.exe"]
    } else {
        &["memfork"]
    };
    for dir in candidates {
        for name in names {
            let path = dir.join(name);
            if path.is_file() {
                return Some(Launch::program(path));
            }
        }
    }
    None
}

/// Last resort: the name, and hope it is on the path.
fn fallback() -> Launch {
    crate::init::which("memfork")
        .map(Launch::program)
        .unwrap_or_else(|| Launch::program("memfork"))
}

fn is_python(exe: &Path) -> bool {
    exe.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .is_some_and(|stem| stem.starts_with("python") || stem == "uv" || stem == "uvx")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set `MEMFORK_LAUNCH` for one test.
    ///
    /// The environment is process-wide, so these tests take a lock rather than
    /// racing each other into a wrong answer.
    fn with_env<T>(value: Option<&str>, body: impl FnOnce() -> T) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var(LAUNCH_ENV).ok();
        match value {
            Some(v) => std::env::set_var(LAUNCH_ENV, v),
            None => std::env::remove_var(LAUNCH_ENV),
        }
        let out = body();
        match previous {
            Some(v) => std::env::set_var(LAUNCH_ENV, v),
            None => std::env::remove_var(LAUNCH_ENV),
        }
        out
    }

    #[test]
    fn a_front_door_can_say_how_to_launch() {
        let launch = with_env(Some(r#"["/venv/bin/python","-m","memfork"]"#), resolve);
        assert_eq!(launch.program, PathBuf::from("/venv/bin/python"));
        assert_eq!(launch.args, vec!["-m".to_owned(), "memfork".to_owned()]);
        assert_eq!(
            launch.argv(&["mcp"]),
            vec!["/venv/bin/python", "-m", "memfork", "mcp"]
        );
    }

    #[test]
    fn nonsense_in_the_environment_is_ignored_rather_than_obeyed() {
        // A malformed value must not take the whole command down: fall back to
        // the ordinary answer, which is this executable.
        for bad in [r#"not json"#, r#"[]"#, r#"{"program":"x"}"#, r#""x""#] {
            let launch = with_env(Some(bad), resolve);
            assert!(
                !launch.program.as_os_str().is_empty(),
                "`{bad}` produced no launch at all"
            );
        }
    }

    #[test]
    fn without_a_front_door_it_is_this_executable() {
        let launch = with_env(None, resolve);
        let exe = std::env::current_exe().expect("current exe");
        // The test binary is not Python, so this is the simple case.
        assert_eq!(launch.program, exe);
        assert!(launch.args.is_empty());
    }

    #[test]
    fn a_launch_knows_whether_a_client_is_pointing_at_it() {
        let launch = Launch::program("/home/a/.memfork/bin/memfork");
        assert!(launch.is_named_in("Command: /home/a/.memfork/bin/memfork mcp"));
        assert!(!launch.is_named_in("Command: /usr/local/bin/memfork mcp"));

        // Windows prints backslashes, and its paths do not care about case.
        let windows = Launch::program(r"C:\Users\a\AppData\Local\memfork\bin\memfork.exe");
        assert_eq!(
            windows.is_named_in(r"Command: c:\users\a\appdata\local\memfork\bin\memfork.exe mcp"),
            cfg!(target_os = "windows"),
            "case sensitivity did not match the platform"
        );
    }

    #[test]
    fn a_command_line_is_quoted_when_it_has_to_be() {
        let launch = Launch::program("/Program Files/memfork");
        assert_eq!(launch.display(&["mcp"]), "\"/Program Files/memfork\" mcp");
    }
}
