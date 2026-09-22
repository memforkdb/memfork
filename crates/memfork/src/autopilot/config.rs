//! A project's autopilot file, `memfork-autopilot.toml` (DESIGN §5.6).
//!
//! Autopilot is off until a repository holds this file, and the file is the
//! only place its settings are kept: which half is on, the check command, how
//! many files an edit sweep may touch before memory is forked. The check
//! command runs on the machine, so it lives in the repository like a plan's
//! acceptance command does, never in shared memory. `memfork init --project
//! --autopilot` writes it, `memfork autopilot on|off` flips it, and every
//! reader treats a file it cannot parse as autopilot off and says so.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The file, at the repository's top level.
pub const FILE: &str = "memfork-autopilot.toml";

/// Seconds a check command may take unless the file says otherwise.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 300;

/// The longest a check command may be given.
pub const MAX_TIMEOUT_SECONDS: u64 = 3600;

/// Distinct files an edit sweep may touch before memory is forked, unless
/// the file says otherwise.
pub const DEFAULT_MAX_FILES: u64 = 5;

/// Longest check command, in characters.
pub const MAX_CHECK_CHARS: usize = 1000;

/// The file as written, every key optional.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
struct FileShape {
    enabled: Option<bool>,
    follow: Follow,
    fork: Fork,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
struct Follow {
    git_branch: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
struct Fork {
    before_risky: Option<bool>,
    check: Option<String>,
    timeout_seconds: Option<u64>,
    max_files: Option<u64>,
    extra_rules: Vec<String>,
    ignore_rules: Vec<String>,
}

/// A project's settings, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Where it was read from.
    pub path: PathBuf,
    /// The master switch.
    pub enabled: bool,
    /// Memory follows the git branch.
    pub follow_git: bool,
    /// Memory is forked before a risky action, where a client's hooks are
    /// installed.
    pub fork_before_risky: bool,
    /// The command that decides whether a risky action worked, if one.
    pub check: Option<String>,
    /// Seconds the check may take.
    pub timeout_seconds: u64,
    /// Distinct files an edit sweep may touch before memory is forked.
    pub max_files: u64,
    /// Patterns of this project's own.
    pub extra_rules: Vec<String>,
    /// Built-in rules switched off by name.
    pub ignore_rules: Vec<String>,
}

impl Config {
    /// Whether the follow half is in force.
    pub fn follows(&self) -> bool {
        self.enabled && self.follow_git
    }

    /// Whether the fork half is in force.
    pub fn forks(&self) -> bool {
        self.enabled && self.fork_before_risky
    }

    /// The check's time limit.
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_seconds.clamp(1, MAX_TIMEOUT_SECONDS))
    }
}

/// What reading a project's file found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    /// There is no file: autopilot is off here.
    Absent,
    /// The file could not be used, and why. Autopilot is off, and every
    /// command that reports on it says so.
    Broken(String),
    /// The settings.
    Config(Config),
}

impl Read {
    /// The settings, if the file is there and usable.
    pub fn config(&self) -> Option<&Config> {
        match self {
            Read::Config(c) => Some(c),
            _ => None,
        }
    }
}

/// Read the file at the top of `root`.
pub fn read(root: &Path) -> Read {
    let path = root.join(FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Read::Absent,
        Err(e) => return Read::Broken(format!("{} cannot be read: {e}", path.display())),
    };
    match parse(&path, &text) {
        Ok(config) => Read::Config(config),
        Err(why) => Read::Broken(why),
    }
}

fn parse(path: &Path, text: &str) -> Result<Config, String> {
    let shape: FileShape = toml::from_str(text)
        .map_err(|e| format!("{} is not a usable autopilot file: {e}", path.display()))?;
    let check = shape
        .fork
        .check
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_owned);
    if check
        .as_ref()
        .is_some_and(|c| c.chars().count() > MAX_CHECK_CHARS)
    {
        return Err(format!(
            "{}: the check command is over {MAX_CHECK_CHARS} characters",
            path.display()
        ));
    }
    if check.as_ref().is_some_and(|c| c.contains('\n')) {
        return Err(format!(
            "{}: the check command must be one line",
            path.display()
        ));
    }
    Ok(Config {
        path: path.to_path_buf(),
        enabled: shape.enabled.unwrap_or(true),
        follow_git: shape.follow.git_branch.unwrap_or(true),
        fork_before_risky: shape.fork.before_risky.unwrap_or(true),
        check,
        timeout_seconds: shape
            .fork
            .timeout_seconds
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
            .clamp(1, MAX_TIMEOUT_SECONDS),
        max_files: shape.fork.max_files.unwrap_or(DEFAULT_MAX_FILES).max(1),
        extra_rules: shape.fork.extra_rules,
        ignore_rules: shape.fork.ignore_rules,
    })
}

/// The file `memfork init --project --autopilot` writes: every key present,
/// with what it means beside it.
pub fn template(check: Option<&str>) -> String {
    let check_line = match check {
        Some(command) => format!("check = {}", toml_string(command)),
        None => "# check = \"cargo test\"".to_owned(),
    };
    format!(
        "\
# MemFork autopilot for this repository, written by `memfork init --project
# --autopilot`. Delete the file, or run `memfork autopilot off`, to stop it.
# `memfork autopilot status` reports what is in force.
enabled = true

[follow]
# Memory follows the git branch: switching branches in git switches the
# memory branch of the same name, forking it from the branch you came from
# the first time; merging a branch in git merges its memory the same way.
git_branch = true

[fork]
# Memory is forked before a risky action and merged or discarded by the
# outcome. Only where a client's own hooks are installed (`memfork autopilot
# status` says which); nothing here runs without them.
before_risky = true
# The command that decides whether a risky action worked. It runs in this
# repository, and only because this file names it. Without one, a shell
# command is judged by its own exit status and an edit sweep is kept as a
# fork for you to merge or discard.
{check_line}
# Seconds the check may take before it counts as failed.
timeout_seconds = {DEFAULT_TIMEOUT_SECONDS}
# An edit sweep touching more than this many distinct files is risky.
max_files = {DEFAULT_MAX_FILES}
# Regular expressions of this project's own, and built-in rules switched off
# by name. `memfork autopilot rules` lists the built-in ones.
extra_rules = []
ignore_rules = []
"
    )
}

fn toml_string(text: &str) -> String {
    toml_edit::Value::from(text).to_string()
}

/// Set `enabled` in an existing file, keeping everything else as it was.
/// Returns the new text.
pub fn set_enabled(text: &str, on: bool) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| format!("the autopilot file is not valid TOML: {e}"))?;
    doc["enabled"] = toml_edit::value(on);
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_parses_to_the_defaults_and_toggles_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, template(None)).unwrap();
        let Read::Config(config) = read(dir.path()) else {
            panic!("the template did not parse");
        };
        assert!(config.enabled && config.follow_git && config.fork_before_risky);
        assert_eq!(config.check, None);
        assert_eq!(config.timeout_seconds, DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(config.max_files, DEFAULT_MAX_FILES);

        let text = std::fs::read_to_string(&path).unwrap();
        let off = set_enabled(&text, false).unwrap();
        std::fs::write(&path, &off).unwrap();
        let config = read(dir.path());
        assert!(!config.config().unwrap().enabled);
        assert!(!config.config().unwrap().follows());
        // Only that key changed: the comments and the rest are as they were.
        assert_eq!(
            off.replace("enabled = false", "enabled = true"),
            text,
            "more than the switch changed"
        );

        let with_check = template(Some("cargo test --workspace"));
        std::fs::write(&path, with_check).unwrap();
        assert_eq!(
            read(dir.path()).config().unwrap().check.as_deref(),
            Some("cargo test --workspace")
        );
    }

    #[test]
    fn absent_means_off_and_broken_says_why() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), Read::Absent);
        std::fs::write(dir.path().join(FILE), "enabled = \"yes\"\n").unwrap();
        assert!(matches!(read(dir.path()), Read::Broken(_)));
        std::fs::write(dir.path().join(FILE), "[fork]\ncheck = \"a\\nb\"\n").unwrap();
        assert!(matches!(read(dir.path()), Read::Broken(why) if why.contains("one line")));
        std::fs::write(dir.path().join(FILE), "unknown = 1\n").unwrap();
        assert!(matches!(read(dir.path()), Read::Broken(_)));
    }

    #[test]
    fn values_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE),
            "[fork]\ntimeout_seconds = 999999\nmax_files = 0\ncheck = \"  \"\n",
        )
        .unwrap();
        let config = read(dir.path());
        let config = config.config().unwrap();
        assert_eq!(config.timeout_seconds, MAX_TIMEOUT_SECONDS);
        assert_eq!(config.max_files, 1);
        assert_eq!(config.check, None);
    }
}
