//! `memfork init` — register the MCP server with the clients installed here.
//!
//! Two rules shape this:
//!
//! 1. **Prefer the client's own command.** If a client ships `mcp add`, that
//!    is what runs, so the client writes its own config in its own format.
//!    Editing a file by hand is the fallback for clients that ship no command,
//!    or whose command cannot express the requested scope.
//! 2. **Never damage a config.** Checking what is already registered is done
//!    by *reading* the config file — never by running the client's `list`,
//!    which on some clients health-checks by starting every server it knows
//!    about. Writing goes through [`clients::edit`], which changes only the
//!    MemFork entry and keeps a backup.
//!
//! Every run prints which method it used for each client, and re-running
//! changes nothing.

use std::path::PathBuf;

use crate::clients::{self, edit, Client, Registration, Scope};
use crate::launch::{self, Launch};

/// What `memfork init` will do, or did, for one client.
#[derive(Debug)]
pub enum Action {
    /// Run the client's own registration command.
    RunCommand {
        /// The executable's name, as the registry spells it. For display.
        binary: String,
        /// Where that name resolved to on `PATH`.
        ///
        /// The resolved path is what gets spawned, never the bare name. On
        /// Windows these commands are usually `.cmd` shims, and `CreateProcess`
        /// will not run a batch file looked up by bare name — `claude` would
        /// come back "program not found" on exactly the machines where it is
        /// installed.
        resolved: PathBuf,
        /// Its arguments.
        args: Vec<String>,
    },
    /// Remove a registration pointing at another MemFork, then add this one.
    ///
    /// Two commands rather than one because that is what these clients
    /// support: `mcp add` over an existing name either fails or duplicates,
    /// depending on the client, and neither is a fix.
    ReplaceCommand {
        /// The executable's name, as the registry spells it. For display.
        binary: String,
        /// Where that name resolved to on `PATH`.
        resolved: PathBuf,
        /// The removal, run first.
        remove: Vec<String>,
        /// The registration, run second.
        add: Vec<String>,
        /// What the old registration pointed at, when the client said.
        was: Option<String>,
    },
    /// Edit the client's config file.
    WriteFile(Box<edit::Edit>),
    /// The client already has this registration.
    AlreadyRegistered {
        /// How that was established: a command that was asked, or a file read.
        checked: clients::Checked,
    },
    /// The client points at another MemFork and this one cannot fix it.
    ///
    /// Only when the client documents no way to remove a registration.
    /// Guessing at a command that edits somebody's configuration is not a
    /// thing to do, so it says what to run instead.
    StaleElsewhere {
        /// What it points at, when the client said.
        was: Option<String>,
        /// What the person should run.
        fix: String,
    },
    /// The client does not appear to be installed.
    NotInstalled,
    /// MemFork will not register this client, and why.
    Refused(String),
}

impl Action {
    /// Whether carrying this out would change anything.
    pub fn changes_anything(&self) -> bool {
        matches!(
            self,
            Action::RunCommand { .. } | Action::ReplaceCommand { .. } | Action::WriteFile(_)
        )
    }
}

/// The plan for one client.
#[derive(Debug)]
pub struct ClientPlan {
    /// Registry id.
    pub id: String,
    /// Human-readable name.
    pub display: String,
    /// The scope this will actually use, which may differ from the one asked
    /// for when a client cannot do it.
    pub scope: Scope,
    /// What will happen.
    pub action: Action,
    /// Why, when the answer is surprising.
    pub note: Option<String>,
}

/// Work out what `memfork init` would do, without doing any of it.
pub fn plan(
    scope: Scope,
    only: Option<&str>,
    home: Option<&str>,
    launch: &Launch,
) -> Result<Vec<ClientPlan>, String> {
    let registry = clients::load()?;
    if let Some(id) = only {
        if !registry.iter().any(|c| c.id == id) {
            return Err(format!(
                "no client named `{id}`; known clients: {}",
                clients::ids().join(", ")
            ));
        }
    }
    let home = home.map(str::to_owned).or_else(clients::home_dir);

    Ok(registry
        .into_iter()
        .filter(|c| only.is_none_or(|id| c.id == id))
        .map(|c| plan_one(&c, scope, home.as_deref(), launch))
        .collect())
}

fn plan_one(client: &Client, scope: Scope, home: Option<&str>, launch: &Launch) -> ClientPlan {
    let mk = |scope, action, note| ClientPlan {
        id: client.id.clone(),
        display: client.display.clone(),
        scope,
        action,
        note,
    };

    let Some(home) = home else {
        return mk(
            scope,
            Action::Refused("cannot find your home directory".to_owned()),
            None,
        );
    };

    let cli_on_path = client
        .cli
        .as_ref()
        .and_then(|c| which(&c.binary).map(|path| (c, path)));
    if !is_installed(client, home) {
        return mk(scope, Action::NotInstalled, None);
    }

    // Already registered? Ask the client if it has a command, and only read a
    // file when it has not. The client owns its configuration: it may keep
    // servers somewhere the registry does not model, or rewrite what it was
    // given, and a file read around it would disagree with the client itself.
    let mut was: Option<String> = None;
    match clients::registration(client, scope, home, launch) {
        (Registration::Yes, checked) => {
            return mk(scope, Action::AlreadyRegistered { checked }, None);
        }
        // Registered, but at another MemFork. This is not "already done": the
        // client is launching a path that an install moved or removed, and
        // leaving it alone leaves the tools missing.
        (Registration::Stale { found }, _) => was = found,
        (Registration::No, _) => {}
        // Not knowing is not a reason to skip: try, and let the attempt say.
        (Registration::Unknown(_), _) => {}
    }

    // Preferred: the client's own command, if it can express this scope.
    if let Some((cli, resolved)) = &cli_on_path {
        if let Some(args) = cli.add_args(scope, launch) {
            if was.is_some() {
                let Some(remove) = cli.remove_args(scope) else {
                    return mk(
                        scope,
                        Action::StaleElsewhere {
                            was,
                            fix: format!(
                                "remove the `{}` server from {} yourself, then run                                  `memfork init` again",
                                clients::SERVER_NAME,
                                client.display
                            ),
                        },
                        None,
                    );
                };
                return mk(
                    scope,
                    Action::ReplaceCommand {
                        binary: cli.binary.clone(),
                        resolved: resolved.clone(),
                        remove,
                        add: args,
                        was,
                    },
                    None,
                );
            }
            return mk(
                scope,
                Action::RunCommand {
                    binary: cli.binary.clone(),
                    resolved: resolved.clone(),
                    args,
                },
                None,
            );
        }
    }

    // Otherwise edit the file, if this client allows it for this scope.
    let note = fallback_note(client, scope, cli_on_path.is_some());
    let (scope, note) = match (client.file_writable(scope), scope) {
        (true, s) => (s, note),
        // Refusing user scope means project scope is the honest alternative,
        // not doing nothing: the tools still work, just not everywhere.
        (false, Scope::User) => (Scope::Project, note),
        (false, Scope::Project) => {
            return mk(
                scope,
                Action::Refused("MemFork will not write this client's config".to_owned()),
                note,
            )
        }
    };

    let Some(file) = client.file.as_ref() else {
        return mk(
            scope,
            Action::Refused("this client has no config file MemFork can write".to_owned()),
            note,
        );
    };
    let Some(path) = file.path_here(scope, home) else {
        return mk(
            scope,
            Action::Refused(
                "MemFork does not write this client's configuration for this scope".to_owned(),
            ),
            note,
        );
    };
    match edit::plan(file, &path, launch) {
        Ok(e) if e.change == edit::Change::Unchanged => mk(
            scope,
            Action::AlreadyRegistered {
                checked: clients::Checked::File(path),
            },
            note,
        ),
        Ok(e) => mk(scope, Action::WriteFile(Box::new(e)), note),
        Err(reason) => mk(scope, Action::Refused(reason), note),
    }
}

/// Whether a client appears to be installed.
///
/// Its command is on `PATH`, it has left a config behind, or its own directory
/// exists. The last matters for a client that ships no command and has never
/// had an MCP server added, so its config file does not exist yet — without
/// it, a perfectly installed client would be reported as missing.
pub fn is_installed(client: &Client, home: &str) -> bool {
    let cli_on_path = client
        .cli
        .as_ref()
        .is_some_and(|c| which(&c.binary).is_some());
    let config_exists = |scope| {
        client
            .file
            .as_ref()
            .and_then(|f| f.path_here(scope, home))
            .is_some_and(|p| p.exists())
    };
    cli_on_path
        || config_exists(Scope::User)
        || config_exists(Scope::Project)
        || client.detect_dir_here(home).is_some_and(|p| p.is_dir())
}

/// Why a client is not being handled the way the caller asked.
fn fallback_note(client: &Client, scope: Scope, cli_present: bool) -> Option<String> {
    let cli = client.cli.as_ref()?;
    if cli_present {
        if cli.scope_flags(scope).is_none() {
            return Some(format!(
                "`{} mcp add` has no {} scope, so this goes through the config file instead",
                cli.binary,
                scope.as_str()
            ));
        }
        return None;
    }
    if !client.file_writable(Scope::User) {
        return Some(format!(
            "`{}` is not on PATH, and MemFork will not edit this client's user config \
             because it also holds session credentials — registering for this project \
             instead. Install the `{}` command and re-run for user scope.",
            cli.binary, cli.binary
        ));
    }
    Some(format!(
        "`{}` is not on PATH, so this goes through the config file instead",
        cli.binary
    ))
}

/// Carry out one client's plan.
///
/// Returns the path of the backup, and only that: a backup exists when MemFork
/// wrote a client's configuration file itself, and never when the client's own
/// command did the writing.
pub fn apply(plan: &ClientPlan) -> Result<Option<String>, String> {
    match &plan.action {
        Action::RunCommand {
            binary,
            resolved,
            args,
        } => {
            let output = std::process::Command::new(resolved)
                .args(args)
                .output()
                .map_err(|e| format!("cannot run `{binary}` ({}): {e}", resolved.display()))?;
            if output.status.success() {
                Ok(None)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                let detail = if stderr.trim().is_empty() {
                    stdout
                } else {
                    stderr
                };
                Err(format!(
                    "`{binary} {}` failed: {}",
                    args.join(" "),
                    detail.trim()
                ))
            }
        }
        Action::ReplaceCommand {
            binary,
            resolved,
            remove,
            add,
            ..
        } => {
            // The removal is allowed to fail: a client that has already lost
            // the entry is in the state the removal was for, and stopping
            // there would leave nothing registered at all.
            let _ = std::process::Command::new(resolved).args(remove).output();
            let output = std::process::Command::new(resolved)
                .args(add)
                .output()
                .map_err(|e| format!("`{binary} {}` could not be run: {e}", add.join(" ")))?;
            if output.status.success() {
                // No backup: the client wrote its own configuration, and what
                // it did with the old entry is its business. Saying otherwise
                // printed the command again under a label promising a file.
                return Ok(None);
            }
            let detail = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "`{binary} {}` failed: {}",
                add.join(" "),
                detail.trim()
            ))
        }
        Action::WriteFile(e) => Ok(edit::apply(e)?.map(|p| p.display().to_string())),
        Action::AlreadyRegistered { .. }
        | Action::StaleElsewhere { .. }
        | Action::NotInstalled
        | Action::Refused(_) => Ok(None),
    }
}

/// The command a client should be told to run.
///
/// Absolute, so a client that does not share this shell's `PATH` still finds
/// it — and resolved through [`launch`], so an install from a wheel registers
/// the command that actually starts MemFork rather than the Python
/// interpreter it happens to be running inside.
pub fn command_line() -> String {
    launch::resolve().display(clients::SERVER_ARGS)
}

/// Find an executable on `PATH`, including Windows' extensions.
pub fn which(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    // On Windows a bare name resolves through PATHEXT; elsewhere the file is
    // taken as-is. Both are checked the same way, by looking for the file.
    let extensions: Vec<String> = if cfg!(target_os = "windows") {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_ascii_lowercase())
            .collect()
    } else {
        Vec::new()
    };

    for dir in std::env::split_paths(&path) {
        let direct = dir.join(binary);
        if direct.is_file() {
            return Some(direct);
        }
        for ext in &extensions {
            let candidate = dir.join(format!("{binary}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_client_is_named_along_with_the_known_ones() {
        let err = plan(
            Scope::User,
            Some("nope"),
            Some("/home/ada"),
            &Launch::program("memfork"),
        )
        .expect_err("unknown client");
        assert!(err.contains("nope"), "{err}");
        assert!(err.contains("cursor"), "{err}");
    }

    #[test]
    fn selecting_one_client_plans_only_that_client() {
        let plans = plan(
            Scope::User,
            Some("cursor"),
            Some("/home/ada"),
            &Launch::program("memfork"),
        )
        .expect("planned");
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].id, "cursor");
    }

    #[test]
    fn which_finds_something_that_certainly_exists() {
        // Every supported platform has its own shell interpreter on PATH.
        let found = if cfg!(target_os = "windows") {
            which("cmd")
        } else {
            which("sh")
        };
        assert!(found.is_some(), "PATH lookup found nothing at all");
        assert!(which("a-binary-that-does-not-exist-anywhere").is_none());
    }
}
