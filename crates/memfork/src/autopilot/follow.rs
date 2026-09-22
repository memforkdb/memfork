//! Memory follows the git branch: the proxy's side (DESIGN §5.6).
//!
//! `memfork mcp` is the one process that can see both the repository and
//! the session, so this runs there, before every tool call it forwards.
//! It reads `HEAD` and what the reflog gained since the last look, and
//! sends the daemon an observation only when something changed: the branch
//! moved, a merge was made, or this is the session's first call and the
//! daemon should land it on the right branch. The daemon does the switching
//! and merging; see [`super::engine`].
//!
//! The reflog cursor lives beside the store, keyed by the worktree, so a
//! merge made with no session connected is still seen by the next one. The
//! first observation of a repository that was never followed starts at the
//! end: nothing is replayed.

use std::path::Path;

use serde_json::{json, Value as Json};

use super::git::{Head, Repo};

/// What the proxy saw, to send to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// Where `HEAD` is.
    pub head: Head,
    /// The branch git moved from, when it moved and the reflog says.
    pub from: Option<String>,
    /// Merges git made since the last look: source and target.
    pub merges: Vec<(String, String)>,
    /// Where the reflog has been read to.
    pub cursor: u64,
    /// Every local branch, when the daemon should compare them with memory.
    pub branches: Option<Vec<String>>,
}

impl Observation {
    /// The request for the daemon's `follow` action.
    pub fn to_request(&self, session: &str, namespace: &str, repo: &Repo) -> Json {
        let (head, branch) = match &self.head {
            Head::Branch(name) => ("branch", Some(name.clone())),
            Head::Detached => ("detached", None),
            Head::Unknown => ("unknown", None),
        };
        json!({
            "action": "follow",
            "session": session,
            "namespace": namespace,
            "repo": repo.key(),
            "worktree": repo.worktree_key(),
            "head": head,
            "branch": branch,
            "from": self.from,
            "merges": self.merges.iter().map(|(s, t)| json!({ "source": s, "target": t })).collect::<Vec<_>>(),
            "cursor": self.cursor,
            "git_branches": self.branches,
        })
    }
}

/// One session's watch on its repository.
#[derive(Debug)]
pub struct Follower {
    repo: Repo,
    last: Option<Head>,
    cursor: Option<u64>,
    started: bool,
    sent: bool,
}

impl Follower {
    /// A follower for the repository at `root`, if there is one.
    pub fn open(root: &Path) -> Option<Follower> {
        Some(Follower {
            repo: Repo::open(root)?,
            last: None,
            cursor: None,
            started: false,
            sent: false,
        })
    }

    /// The repository.
    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    /// Whether the cursor has been taken from the daemon yet.
    pub fn started(&self) -> bool {
        self.started
    }

    /// Start from the cursor the daemon remembered, or from the end.
    pub fn start(&mut self, cursor: Option<u64>) {
        self.cursor = Some(cursor.unwrap_or_else(|| self.repo.reflog_since(None).cursor));
        self.started = true;
    }

    /// Forget the start, so the cursor is asked for again: after the daemon
    /// could not be told what was seen.
    pub fn reset(&mut self) {
        self.started = false;
        self.sent = false;
        self.last = None;
    }

    /// Look at the repository. Something to tell the daemon, or nothing new.
    pub fn observe(&mut self) -> Option<Observation> {
        let head = self.repo.head();
        let read = self.repo.reflog_since(self.cursor);
        self.cursor = Some(read.cursor);
        let changed = self.last.as_ref() != Some(&head);

        // The target of a merge is the branch HEAD was on when git made it,
        // which the checkouts in the same batch of lines say.
        let mut on: Option<String> = match &self.last {
            Some(Head::Branch(name)) => Some(name.clone()),
            _ => None,
        };
        let mut merges = Vec::new();
        for line in &read.lines {
            if let Some((_, to)) = line.checkout() {
                on = (!looks_like_commit(&to)).then_some(to);
            } else if let Some(source) = line.merged_branch() {
                let target = on.clone().or_else(|| match &head {
                    Head::Branch(name) => Some(name.clone()),
                    _ => None,
                });
                if let Some(target) = target {
                    merges.push((source, target));
                }
            }
        }

        let first = !self.sent;
        if !changed && merges.is_empty() && !first {
            return None;
        }
        let from = match &head {
            Head::Branch(name) if changed => self.repo.came_from(name),
            _ => None,
        };
        let branches = Some(self.repo.branches().into_iter().collect());
        self.last = Some(head.clone());
        self.sent = true;
        Some(Observation {
            head,
            from,
            merges,
            cursor: read.cursor,
            branches,
        })
    }
}

fn looks_like_commit(name: &str) -> bool {
    name.len() >= 40 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "3c0a724c98faa8a0e4eb04ac68a48d45dea5c5e2";

    fn line(message: &str) -> String {
        format!("{SHA} {SHA} t <t@t> 1790098567 +0530\t{message}\n")
    }

    fn repo(dir: &Path) -> std::path::PathBuf {
        let git = dir.join(".git");
        std::fs::create_dir_all(git.join("logs")).unwrap();
        std::fs::create_dir_all(git.join("refs").join("heads")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git.join("refs").join("heads").join("main"), SHA).unwrap();
        std::fs::write(git.join("logs").join("HEAD"), line("commit (initial): one")).unwrap();
        git
    }

    fn append(git: &Path, message: &str) {
        let log = git.join("logs").join("HEAD");
        let mut text = std::fs::read_to_string(&log).unwrap();
        text.push_str(&line(message));
        std::fs::write(log, text).unwrap();
    }

    #[test]
    fn the_first_look_says_where_head_is_and_then_only_changes_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let git = repo(dir.path());
        let mut f = Follower::open(dir.path()).unwrap();
        assert!(!f.started());
        f.start(None);
        let first = f.observe().unwrap();
        assert_eq!(first.head, Head::Branch("main".to_owned()));
        assert_eq!(first.from, None);
        assert!(first.merges.is_empty());
        assert_eq!(first.branches, Some(vec!["main".to_owned()]));
        assert_eq!(f.observe(), None);

        // A switch: the branch, and where it came from.
        std::fs::write(git.join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        append(&git, "checkout: moving from main to feature/x");
        let switched = f.observe().unwrap();
        assert_eq!(switched.head, Head::Branch("feature/x".to_owned()));
        assert_eq!(switched.from, Some("main".to_owned()));
        assert_eq!(f.observe(), None);

        // A merge on the branch HEAD was on when it happened, with no
        // switch: still reported.
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        append(&git, "checkout: moving from feature/x to main");
        append(&git, "merge feature/x: Fast-forward");
        let merged = f.observe().unwrap();
        assert_eq!(
            merged.merges,
            vec![("feature/x".to_owned(), "main".to_owned())]
        );
        assert_eq!(merged.from, Some("feature/x".to_owned()));
        append(&git, "merge other: Merge made by the 'ort' strategy.");
        let again = f.observe().unwrap();
        assert_eq!(again.merges, vec![("other".to_owned(), "main".to_owned())]);
        assert_eq!(again.from, None);

        let request = again.to_request("s1", "shop", f.repo());
        assert_eq!(request["action"], "follow");
        assert_eq!(request["head"], "branch");
        assert_eq!(request["branch"], "main");
        assert_eq!(request["merges"][0]["source"], "other");
        assert!(request["cursor"].as_u64().unwrap() > 0);
    }

    #[test]
    fn a_remembered_cursor_catches_a_merge_made_between_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let git = repo(dir.path());
        let mut earlier = Follower::open(dir.path()).unwrap();
        earlier.start(None);
        let seen = earlier.observe().unwrap();
        // Between sessions: a merge with nobody connected.
        append(&git, "merge feature/x: Fast-forward");
        let mut later = Follower::open(dir.path()).unwrap();
        later.start(Some(seen.cursor));
        let caught = later.observe().unwrap();
        assert_eq!(
            caught.merges,
            vec![("feature/x".to_owned(), "main".to_owned())]
        );
        // A session that starts from nowhere never replays it.
        let mut fresh = Follower::open(dir.path()).unwrap();
        fresh.start(None);
        assert!(fresh.observe().unwrap().merges.is_empty());
    }

    #[test]
    fn detached_head_is_reported_once_as_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let git = repo(dir.path());
        let mut f = Follower::open(dir.path()).unwrap();
        f.start(None);
        f.observe();
        std::fs::write(git.join("HEAD"), format!("{SHA}\n")).unwrap();
        append(&git, &format!("checkout: moving from main to {SHA}"));
        assert_eq!(f.observe().unwrap().head, Head::Detached);
        assert_eq!(f.observe(), None);
        // A merge made while detached has no branch to land on and is not
        // reported.
        append(&git, "merge feature/x: Fast-forward");
        assert_eq!(f.observe(), None);
        // Reset, and the next look starts over.
        f.reset();
        assert!(!f.started());
    }
}
