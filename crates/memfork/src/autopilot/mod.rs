//! Autopilot: memory that protects itself (DESIGN §5.6).
//!
//! Two halves, each opt-in per project through `memfork-autopilot.toml`, off
//! by default, and never in the way of a git operation or an agent:
//!
//! * **Memory follows the git branch.** `memfork mcp` reads the repository's
//!   `HEAD` before every tool call it forwards. A switch in git switches the
//!   session to the memory branch of the same name, forking it from the
//!   branch it came from the first time; a merge in git, seen in the reflog,
//!   merges memory the same way and reports a conflict rather than forcing
//!   one. No git hook, and no git run by MemFork: [`git`].
//! * **Automatic forks before risky steps.** Through a client's own hooks,
//!   installed only by `memfork init --project --autopilot`, memory is forked
//!   before a command that matches the risky [`rules`] or before an edit
//!   sweep past a limit, and merged or discarded by the outcome: a check
//!   command the repository names, or the action's own exit status. With
//!   neither, the fork is kept and said so. [`hook`] is the command a client
//!   runs; it does nothing and says nothing when MemFork is not running.
//!
//! Everything either half does is recorded as `memfork-autopilot`: in the
//! event feed, in a journal beside the store the Brain shows, and as a note
//! in the next tool result the session receives. [`engine`] is the daemon's
//! side of both.

pub mod config;
pub mod git;
pub mod rules;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

/// Who autopilot's writes and events are recorded as.
pub const WRITER: &str = "memfork-autopilot";

/// The prefix of every branch autopilot forks: `autopilot/<parent>/<n>`.
pub const FORK_PREFIX: &str = "autopilot/";

/// Most journal entries kept per project, beside the store.
pub const MAX_JOURNAL: usize = 50;

/// The most of a command line kept in a lesson or a note.
pub const ACTION_CHARS: usize = 80;

/// The path a hook and a proxy post autopilot requests to.
pub const PATH: &str = "/autopilot";

/// Why memory was forked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkKind {
    /// A shell command matched a rule; settled when that command ends.
    Command,
    /// An edit sweep passed the file limit; settled when the agent stops.
    Edits,
}

/// The autopilot fork a session is on, until it is settled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenFork {
    /// The fork's name.
    pub name: String,
    /// The branch it was taken from, which it settles into.
    pub parent: String,
    /// The rule that fired, or `edits`.
    pub rule: String,
    /// The command, clipped, or a description of the sweep.
    pub action: String,
    /// The tool use that caused it, when the client gave one, so only that
    /// action's outcome settles it.
    pub origin: Option<String>,
    /// Which kind.
    pub kind: ForkKind,
}

impl OpenFork {
    /// As a status or a note shows it.
    pub fn to_json(&self) -> Json {
        json!({
            "fork": self.name,
            "parent": self.parent,
            "rule": self.rule,
            "action": self.action,
            "kind": self.kind,
        })
    }
}

/// What one session's autopilot remembers between calls.
#[derive(Debug, Default)]
pub struct SessionState {
    /// The fork it is on, if autopilot forked it.
    pub open: Option<OpenFork>,
    /// Files edited since the last settle, for the sweep limit.
    pub edited: std::collections::BTreeSet<String>,
    /// Notes for the next tool result.
    pub notes: Vec<Json>,
    /// Whether the detached-HEAD note has been given since HEAD was last on
    /// a branch.
    pub detached_noted: bool,
}

impl SessionState {
    /// Take the notes, leaving none.
    pub fn take_notes(&mut self) -> Vec<Json> {
        std::mem::take(&mut self.notes)
    }
}

/// One journal entry: what autopilot did, kept beside the store.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct JournalEntry {
    /// Its place among every side record, for order.
    pub order: u64,
    /// When, RFC 3339 in UTC. Beside the store, never in an id.
    pub time: String,
    /// `follow`, `merge`, `fork`, `merged`, `discarded`, `kept`,
    /// `conflict`, `detached`.
    pub kind: String,
    /// The branch involved.
    pub branch: Option<String>,
    /// A sentence.
    pub detail: String,
}

/// A command clipped for a lesson or a note: one line, at most
/// [`ACTION_CHARS`] characters.
pub fn clip_action(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= ACTION_CHARS {
        return one_line;
    }
    let mut out: String = one_line.chars().take(ACTION_CHARS - 1).collect();
    out.push('…');
    out
}

/// Whether a branch is one autopilot forked.
pub fn is_fork(name: &str) -> bool {
    name.starts_with(FORK_PREFIX)
}

/// The parent a fork settles into, given only its name: `autopilot/<parent>/<n>`.
pub fn parent_of_fork(name: &str) -> Option<&str> {
    let rest = name.strip_prefix(FORK_PREFIX)?;
    let (parent, number) = rest.rsplit_once('/')?;
    number.parse::<u64>().ok()?;
    (!parent.is_empty()).then_some(parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_action_is_one_short_line() {
        assert_eq!(
            clip_action("  npm   install\n  left-pad "),
            "npm install left-pad"
        );
        let long = clip_action(&"x".repeat(500));
        assert_eq!(long.chars().count(), ACTION_CHARS);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn a_forks_parent_is_read_from_its_name() {
        assert_eq!(parent_of_fork("autopilot/main/1"), Some("main"));
        assert_eq!(parent_of_fork("autopilot/feature/x/12"), Some("feature/x"));
        assert_eq!(parent_of_fork("autopilot/main"), None);
        assert_eq!(parent_of_fork("main"), None);
        assert!(is_fork("autopilot/main/1") && !is_fork("main"));
    }
}
