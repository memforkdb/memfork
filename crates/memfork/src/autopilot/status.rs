//! `memfork autopilot status`, and the autopilot part of `memfork doctor`:
//! what is in force for a repository, from the file, the policy, each
//! client's hooks file and, when a daemon is running, what it holds.
//!
//! Asking the daemon never starts one: a status report that started a
//! daemon would be a surprise, and a report with no daemon is still a
//! report.

use std::path::Path;

use serde_json::{json, Value as Json};

use super::config::{self, Read};
use crate::clients::hooks::Installed;
use crate::style::Style;

/// One client's hooks, as far as this repository has them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHooksState {
    /// The client.
    pub display: String,
    /// Its registry id.
    pub id: String,
    /// The file its hooks are kept in, relative to the repository.
    pub file: String,
    /// How much of MemFork's is there.
    pub installed: Installed,
}

/// The report.
#[derive(Debug, Clone)]
pub struct Report {
    /// The repository.
    pub root: std::path::PathBuf,
    /// Its namespace.
    pub namespace: String,
    /// What the file says.
    pub file: Read,
    /// Whether the machine policy allows autopilot at all.
    pub policy_allows: bool,
    /// Every client whose hook system is verified, with what this
    /// repository has of its hooks.
    pub clients: Vec<ClientHooksState>,
    /// Clients whose hook system is not verified: automatic forks never
    /// run through them.
    pub unverified: Vec<String>,
    /// What the daemon holds, when one is running.
    pub daemon: Option<Json>,
}

/// Build the report for the repository at `root`.
pub fn report(root: &Path, data_dir: Option<&str>) -> Report {
    let env = std::env::var(crate::namespace::NAMESPACE_ENV).ok();
    let namespace = crate::namespace::resolve(None, env.as_deref(), root)
        .map(|n| n.name)
        .unwrap_or_else(|_| crate::namespace::FALLBACK.to_owned());
    let mut clients = Vec::new();
    let mut unverified = Vec::new();
    for client in crate::clients::all() {
        match &client.hooks {
            Some(hooks) => {
                let path = hooks
                    .project_local
                    .split('/')
                    .fold(root.to_path_buf(), |p, part| p.join(part));
                clients.push(ClientHooksState {
                    display: client.display.clone(),
                    id: client.id.clone(),
                    file: hooks.project_local.clone(),
                    installed: crate::clients::hooks::installed(&path),
                });
            }
            None => unverified.push(client.display.clone()),
        }
    }
    Report {
        root: root.to_path_buf(),
        namespace: namespace.clone(),
        file: config::read(root),
        policy_allows: crate::policy::allows(crate::policy::Feature::Autopilot),
        clients,
        unverified,
        daemon: ask_daemon(data_dir, &namespace, root),
    }
}

/// What a running daemon holds for `namespace`; `None` when none is
/// running or it could not be asked. The branches git has now go with the
/// question, so the orphans come back current.
fn ask_daemon(data_dir: Option<&str>, namespace: &str, root: &Path) -> Option<Json> {
    let dir = crate::persist::datadir::choose(data_dir).ok()?.path;
    let endpoint = crate::persist::lock::owner(&dir)?;
    endpoint.port?;
    let daemon = crate::client::Daemon::new(&endpoint).ok()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    let mut request = json!({ "action": "status", "namespace": namespace });
    if let Some(repo) = super::git::Repo::open(root) {
        request["repo"] = json!(repo.key());
        request["git_branches"] = json!(repo.branches().into_iter().collect::<Vec<_>>());
    }
    let (status, answer) = runtime
        .block_on(daemon.post_within(super::PATH, &request, std::time::Duration::from_secs(5)))
        .ok()?;
    (status == 200).then_some(answer)
}

impl Report {
    /// The settings, when the file is there and usable.
    pub fn config(&self) -> Option<&config::Config> {
        self.file.config()
    }

    /// Whether memory follows the git branch here.
    pub fn follows(&self) -> bool {
        self.policy_allows && self.config().is_some_and(config::Config::follows)
    }

    /// Whether memory is forked before a risky step here, through at least
    /// one client's installed hooks.
    pub fn forks(&self) -> bool {
        self.policy_allows
            && self.config().is_some_and(config::Config::forks)
            && self.clients.iter().any(|c| c.installed != Installed::None)
    }

    /// One line, for `memfork doctor`.
    pub fn summary(&self) -> String {
        if !self.policy_allows {
            return "switched off by the machine policy".to_owned();
        }
        match &self.file {
            Read::Absent => format!(
                "off (no {} here; `memfork init --project --autopilot` switches it on)",
                config::FILE
            ),
            Read::Broken(why) => format!("off: {why}"),
            Read::Config(c) if !c.enabled => format!("off ({}: enabled = false)", config::FILE),
            Read::Config(c) => {
                let mut parts = Vec::new();
                if c.follow_git {
                    parts.push("memory follows the git branch".to_owned());
                }
                if c.fork_before_risky {
                    let with: Vec<String> = self
                        .clients
                        .iter()
                        .filter(|h| h.installed != Installed::None)
                        .map(|h| h.display.clone())
                        .collect();
                    parts.push(if with.is_empty() {
                        "forks before risky steps: no client's hooks installed".to_owned()
                    } else {
                        format!("forks before risky steps through {}", with.join(", "))
                    });
                }
                if parts.is_empty() {
                    "on, with both halves switched off in the file".to_owned()
                } else {
                    format!("on: {}", parts.join("; "))
                }
            }
        }
    }

    /// The memory branches whose git branch is gone, as the daemon last saw
    /// them, with why and the ways out.
    pub fn orphans(&self) -> Vec<Json> {
        self.daemon
            .as_ref()
            .and_then(|d| d["orphans"].as_array().cloned())
            .unwrap_or_default()
    }

    /// The full report, one line each, for a terminal.
    pub fn lines(&self, style: &Style) -> Vec<String> {
        let mut out = vec![format!(
            "autopilot for {} (project `{}`)",
            crate::style::path(&self.root),
            self.namespace
        )];
        let row = |label: &str, text: &str| format!("  {label:<13} {text}");
        out.push(row("state", &self.summary()));
        out.push(row(
            "file",
            &match &self.file {
                Read::Absent => format!("{} (absent)", config::FILE),
                Read::Broken(why) => format!("{} cannot be used: {why}", config::FILE),
                Read::Config(c) => format!(
                    "{}: enabled = {}, follow.git_branch = {}, fork.before_risky = {}",
                    config::FILE,
                    c.enabled,
                    c.follow_git,
                    c.fork_before_risky
                ),
            },
        ));
        if let Some(c) = self.config() {
            out.push(row(
                "check",
                &match &c.check {
                    Some(check) => format!("`{check}`, up to {} s", c.timeout_seconds),
                    None => "none: a command is judged by its own outcome, an edit sweep is kept as a fork".to_owned(),
                },
            ));
            out.push(row(
                "max files",
                &format!(
                    "{} distinct files in one sweep before memory is forked",
                    c.max_files
                ),
            ));
            if !c.extra_rules.is_empty() || !c.ignore_rules.is_empty() {
                out.push(row(
                    "rules",
                    &format!(
                        "{} of this project's own, {} built-in switched off",
                        c.extra_rules.len(),
                        c.ignore_rules.len()
                    ),
                ));
            }
        }
        out.push(row(
            "policy",
            if self.policy_allows {
                "allows autopilot"
            } else {
                "switches autopilot off machine-wide"
            },
        ));
        for client in &self.clients {
            let state = match client.installed {
                Installed::All => "hooks installed",
                Installed::Some => {
                    "hooks partly installed; run `memfork init --project --autopilot` again"
                }
                Installed::None => "hooks not installed",
            };
            out.push(row(&client.display, &format!("{state} ({})", client.file)));
        }
        if !self.unverified.is_empty() {
            out.push(row(
                "other clients",
                &format!(
                    "automatic forks are off for {}: their hook systems are unverified",
                    self.unverified.join(", ")
                ),
            ));
        }
        match &self.daemon {
            None => out.push(row(
                "daemon",
                "not running; open forks, orphans and the journal are shown while one is",
            )),
            Some(d) => {
                out.push(row("daemon", "connected"));
                let open = d["open"].as_array().cloned().unwrap_or_default();
                if open.is_empty() {
                    out.push(row("open forks", "none"));
                } else {
                    for fork in open {
                        out.push(row(
                            "open fork",
                            &format!(
                                "{} from {} before `{}` (rule: {}){}",
                                fork["fork"].as_str().unwrap_or("?"),
                                fork["parent"].as_str().unwrap_or("?"),
                                fork["action"].as_str().unwrap_or("?"),
                                fork["rule"].as_str().unwrap_or("?"),
                                fork["client"]
                                    .as_str()
                                    .map_or(String::new(), |c| format!(", {c}"))
                            ),
                        ));
                    }
                }
                let kept: Vec<&str> = d["kept_forks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Json::as_str)
                    .collect();
                if !kept.is_empty() {
                    out.push(row(
                        "kept forks",
                        &format!(
                            "{}: merge with `memfork merge <fork>` or discard with `memfork discard <fork> --lesson`",
                            kept.join(", ")
                        ),
                    ));
                }
                let orphans = self.orphans();
                if orphans.is_empty() {
                    out.push(row(
                        "orphans",
                        "none: every branch memory followed still exists in git",
                    ));
                } else {
                    for orphan in orphans {
                        out.push(row(
                            "orphan",
                            &format!(
                                "{} ({})",
                                style.warn(orphan["branch"].as_str().unwrap_or("?")),
                                orphan["why"].as_str().unwrap_or("")
                            ),
                        ));
                        for command in orphan["merge_then_discard"]
                            .as_array()
                            .into_iter()
                            .flatten()
                        {
                            out.push(format!(
                                "                  {}",
                                command.as_str().unwrap_or("")
                            ));
                        }
                        out.push(format!(
                            "                or {}",
                            orphan["discard"].as_str().unwrap_or("")
                        ));
                    }
                }
                let journal: Vec<&Json> = d["journal"].as_array().into_iter().flatten().collect();
                if journal.is_empty() {
                    out.push(row("journal", "nothing yet"));
                } else {
                    out.push(row("journal", "newest last"));
                    for entry in journal.iter().rev().take(8).rev() {
                        out.push(format!(
                            "    {}  {:<9} {}",
                            style.dim(&crate::events::local_clock(
                                entry["time"].as_str().unwrap_or("")
                            )),
                            entry["kind"].as_str().unwrap_or(""),
                            entry["detail"].as_str().unwrap_or("")
                        ));
                    }
                }
            }
        }
        out
    }

    /// The report as JSON.
    pub fn to_json(&self) -> Json {
        json!({
            "op": "autopilot status",
            "root": self.root.display().to_string(),
            "namespace": self.namespace,
            "state": self.summary(),
            "follows_git": self.follows(),
            "forks_before_risky": self.forks(),
            "policy_allows": self.policy_allows,
            "file": match &self.file {
                Read::Absent => json!({ "present": false }),
                Read::Broken(why) => json!({ "present": true, "error": why }),
                Read::Config(c) => json!({
                    "present": true,
                    "path": c.path.display().to_string(),
                    "enabled": c.enabled,
                    "follow_git": c.follow_git,
                    "fork_before_risky": c.fork_before_risky,
                    "check": c.check,
                    "timeout_seconds": c.timeout_seconds,
                    "max_files": c.max_files,
                    "extra_rules": c.extra_rules,
                    "ignore_rules": c.ignore_rules,
                }),
            },
            "clients": self.clients.iter().map(|c| json!({
                "id": c.id,
                "display": c.display,
                "file": c.file,
                "hooks": match c.installed {
                    Installed::All => "installed",
                    Installed::Some => "partial",
                    Installed::None => "not installed",
                },
            })).collect::<Vec<_>>(),
            "unverified_clients": self.unverified,
            "daemon": self.daemon,
        })
    }
}
