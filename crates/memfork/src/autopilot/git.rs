//! Reading a git repository without running git (DESIGN §5.6).
//!
//! Everything memory-follows-the-branch needs is in a handful of files git
//! keeps in plain text: `HEAD` says which branch is checked out, `logs/HEAD`
//! records every move of `HEAD` with a message that names a merge, and
//! `refs/heads` with `packed-refs` list the branches that exist. Reading them
//! is cheaper than a process and never blocks a git operation, and a worktree
//! is found the same way git finds it: its `.git` is a file naming the real
//! directory, which holds its own `HEAD` and reflog and a `commondir` for the
//! refs it shares.
//!
//! Nothing here writes, and a test proves no `git` was ever run.

use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The most of a reflog read at once, from its end. A merge that scrolled
/// further than this since the last look is not seen, and the cursor moves
/// on: never blocking, never replaying.
pub const REFLOG_WINDOW: u64 = 64 * 1024;

/// Most branch names listed for one repository.
pub const MAX_BRANCHES: usize = 1000;

/// What `HEAD` points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// A branch, by its short name: `main`, `feature/x`.
    Branch(String),
    /// A commit, checked out directly.
    Detached,
    /// The file is missing or says something this reader does not know.
    Unknown,
}

/// One line of `logs/HEAD`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogLine {
    /// The message after the tab: `checkout: moving from a to b`, `merge b:
    /// Fast-forward`, `commit: ...`.
    pub message: String,
}

impl ReflogLine {
    /// The branches this line moved between, if it is a checkout.
    pub fn checkout(&self) -> Option<(String, String)> {
        let rest = self.message.strip_prefix("checkout: moving from ")?;
        let (from, to) = rest.rsplit_once(" to ")?;
        Some((from.to_owned(), to.to_owned()))
    }

    /// The branch this line merged in, if it is a merge git made on a
    /// branch: `merge <name>: Fast-forward` or `merge <name>: Merge made by
    /// ...`. A pull, a rebase, a squash and a cherry-pick are not merges of
    /// a local branch and are left alone.
    pub fn merged_branch(&self) -> Option<String> {
        let rest = self.message.strip_prefix("merge ")?;
        let (name, how) = rest.rsplit_once(": ")?;
        let how = how.trim();
        if how == "Fast-forward" || how.starts_with("Merge made by") {
            Some(name.to_owned())
        } else {
            None
        }
    }
}

/// What reading the reflog from a cursor found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReflogRead {
    /// The lines written since the cursor, oldest first.
    pub lines: Vec<ReflogLine>,
    /// Where the next read starts: the file's length now.
    pub cursor: u64,
    /// The file was shorter than the cursor (expired or rewritten), so the
    /// lines were not read and the cursor was set to the end.
    pub reset: bool,
}

/// A repository, found from a working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// The directory holding this worktree's `HEAD` and `logs/HEAD`.
    pub gitdir: PathBuf,
    /// The directory holding `refs/heads` and `packed-refs`, shared by every
    /// worktree of the repository.
    pub commondir: PathBuf,
}

impl Repo {
    /// The repository whose top level is `root`, if `root/.git` is a
    /// directory or a `gitdir:` file. Anything else is not a repository this
    /// reader understands.
    pub fn open(root: &Path) -> Option<Repo> {
        let dot_git = root.join(".git");
        let gitdir = if dot_git.is_dir() {
            dot_git
        } else {
            let text = std::fs::read_to_string(&dot_git).ok()?;
            let target = text.trim().strip_prefix("gitdir:")?.trim();
            let path = Path::new(target);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            }
        };
        if !gitdir.is_dir() {
            return None;
        }
        let commondir = match std::fs::read_to_string(gitdir.join("commondir")) {
            Ok(text) => {
                let path = Path::new(text.trim());
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    gitdir.join(path)
                }
            }
            Err(_) => gitdir.clone(),
        };
        Some(Repo { gitdir, commondir })
    }

    /// The key the shared refs are remembered under, the same for every
    /// worktree of one repository, with `/` separators on every OS.
    pub fn key(&self) -> String {
        let canonical = self
            .commondir
            .canonicalize()
            .unwrap_or_else(|_| self.commondir.clone());
        canonical.display().to_string().replace('\\', "/")
    }

    /// The key this worktree's own reflog is remembered under.
    pub fn worktree_key(&self) -> String {
        let canonical = self
            .gitdir
            .canonicalize()
            .unwrap_or_else(|_| self.gitdir.clone());
        canonical.display().to_string().replace('\\', "/")
    }

    /// What `HEAD` points at now.
    pub fn head(&self) -> Head {
        let Ok(text) = std::fs::read_to_string(self.gitdir.join("HEAD")) else {
            return Head::Unknown;
        };
        let text = text.trim();
        if let Some(reference) = text.strip_prefix("ref:") {
            return match reference.trim().strip_prefix("refs/heads/") {
                Some(name) if !name.is_empty() => Head::Branch(name.to_owned()),
                _ => Head::Unknown,
            };
        }
        if looks_like_commit(text) {
            return Head::Detached;
        }
        Head::Unknown
    }

    /// The lines of `logs/HEAD` written since `cursor`, at most
    /// [`REFLOG_WINDOW`] bytes of them. `None` means from the end: nothing
    /// is read and the cursor is set to the file's length.
    pub fn reflog_since(&self, cursor: Option<u64>) -> ReflogRead {
        let path = self.gitdir.join("logs").join("HEAD");
        let Ok(mut file) = std::fs::File::open(&path) else {
            return ReflogRead::default();
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let at_end = |reset| ReflogRead {
            lines: Vec::new(),
            cursor: len,
            reset,
        };
        let Some(cursor) = cursor else {
            return at_end(false);
        };
        if cursor > len {
            return at_end(true);
        }
        if cursor == len {
            return at_end(false);
        }
        let start = cursor.max(len.saturating_sub(REFLOG_WINDOW));
        let mut bytes = Vec::new();
        let read = file
            .seek(SeekFrom::Start(start))
            .and_then(|_| file.take(len - start).read_to_end(&mut bytes));
        if read.is_err() {
            return at_end(false);
        }
        let text = String::from_utf8_lossy(&bytes);
        // A read that started inside the window rather than at the cursor
        // begins mid-line; the first partial line is dropped.
        let text = if start > cursor {
            text.split_once('\n').map_or("", |(_, rest)| rest)
        } else {
            &text
        };
        let lines = text
            .lines()
            .filter_map(parse_reflog_line)
            .collect::<Vec<_>>();
        ReflogRead {
            lines,
            cursor: len,
            reset: false,
        }
    }

    /// The branch `HEAD` last moved from, if the newest checkout line says
    /// it moved to `to`. Read from the end of the reflog; the cursor is not
    /// involved.
    pub fn came_from(&self, to: &str) -> Option<String> {
        let read = self.reflog_since(Some(0));
        read.lines
            .iter()
            .rev()
            .find_map(ReflogLine::checkout)
            .filter(|(_, moved_to)| moved_to == to)
            .map(|(from, _)| from)
            .filter(|from| !looks_like_commit(from))
    }

    /// Every local branch, from `refs/heads` and `packed-refs`, at most
    /// [`MAX_BRANCHES`], in name order.
    pub fn branches(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let heads = self.commondir.join("refs").join("heads");
        walk(&heads, &heads, &mut out);
        if let Ok(packed) = std::fs::read_to_string(self.commondir.join("packed-refs")) {
            for line in packed.lines() {
                if line.starts_with('#') || line.starts_with('^') {
                    continue;
                }
                if let Some((_, name)) = line.split_once(' ') {
                    if let Some(short) = name.trim().strip_prefix("refs/heads/") {
                        if out.len() < MAX_BRANCHES {
                            out.insert(short.to_owned());
                        }
                    }
                }
            }
        }
        out
    }
}

fn walk(base: &Path, dir: &Path, out: &mut BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if out.len() >= MAX_BRANCHES {
            return;
        }
        if path.is_dir() {
            walk(base, &path, out);
        } else if let Ok(rel) = path.strip_prefix(base) {
            let name = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            if !name.is_empty() && !name.ends_with(".lock") {
                out.insert(name);
            }
        }
    }
}

fn parse_reflog_line(line: &str) -> Option<ReflogLine> {
    let (_, message) = line.split_once('\t')?;
    Some(ReflogLine {
        message: message.trim_end().to_owned(),
    })
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

    /// A repository laid out by hand, so this test needs no git.
    fn repo(dir: &Path) -> Repo {
        let git = dir.join(".git");
        std::fs::create_dir_all(git.join("logs")).unwrap();
        std::fs::create_dir_all(git.join("refs").join("heads").join("feature")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git.join("refs").join("heads").join("main"), SHA).unwrap();
        std::fs::write(
            git.join("refs").join("heads").join("feature").join("x"),
            SHA,
        )
        .unwrap();
        std::fs::write(
            git.join("packed-refs"),
            format!(
                "# pack-refs with: peeled\n{SHA} refs/heads/packed\n{SHA} refs/tags/v1\n^{SHA}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            git.join("logs").join("HEAD"),
            line("commit (initial): one") + &line("checkout: moving from main to feature/x"),
        )
        .unwrap();
        Repo::open(dir).unwrap()
    }

    #[test]
    fn head_is_a_branch_a_commit_or_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(dir.path());
        assert_eq!(repo.head(), Head::Branch("main".to_owned()));
        std::fs::write(repo.gitdir.join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(repo.head(), Head::Branch("feature/x".to_owned()));
        std::fs::write(repo.gitdir.join("HEAD"), format!("{SHA}\n")).unwrap();
        assert_eq!(repo.head(), Head::Detached);
        std::fs::write(repo.gitdir.join("HEAD"), "ref: refs/remotes/origin/main\n").unwrap();
        assert_eq!(repo.head(), Head::Unknown);
        std::fs::remove_file(repo.gitdir.join("HEAD")).unwrap();
        assert_eq!(repo.head(), Head::Unknown);
    }

    #[test]
    fn branches_come_from_loose_refs_and_packed_refs_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(dir.path());
        let names: Vec<String> = repo.branches().into_iter().collect();
        assert_eq!(names, ["feature/x", "main", "packed"]);
    }

    #[test]
    fn the_reflog_is_read_from_a_cursor_and_the_cursor_moves_to_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(dir.path());
        let log = repo.gitdir.join("logs").join("HEAD");
        let len = std::fs::metadata(&log).unwrap().len();

        // From nowhere: nothing is replayed, and the cursor is the end.
        let first = repo.reflog_since(None);
        assert!(first.lines.is_empty());
        assert_eq!(first.cursor, len);

        // Nothing new.
        let same = repo.reflog_since(Some(len));
        assert!(same.lines.is_empty() && !same.reset);

        // A merge appended is seen once.
        let mut text = std::fs::read_to_string(&log).unwrap();
        text.push_str(&line("merge feature/x: Fast-forward"));
        text.push_str(&line("merge other: Merge made by the 'ort' strategy."));
        text.push_str(&line("pull: Fast-forward"));
        text.push_str(&line("rebase (finish): returning to refs/heads/main"));
        std::fs::write(&log, &text).unwrap();
        let read = repo.reflog_since(Some(len));
        let merged: Vec<String> = read
            .lines
            .iter()
            .filter_map(ReflogLine::merged_branch)
            .collect();
        assert_eq!(merged, ["feature/x", "other"]);
        assert_eq!(read.cursor, text.len() as u64);

        // From the start, everything is there.
        assert_eq!(repo.reflog_since(Some(0)).lines.len(), 6);

        // A file shorter than the cursor was expired or rewritten: nothing
        // is guessed, the cursor is reset.
        std::fs::write(&log, line("commit: fresh")).unwrap();
        let reset = repo.reflog_since(Some(len));
        assert!(reset.reset && reset.lines.is_empty());
        assert_eq!(reset.cursor, line("commit: fresh").len() as u64);
    }

    #[test]
    fn only_the_last_window_of_a_long_reflog_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(dir.path());
        let log = repo.gitdir.join("logs").join("HEAD");
        let mut text = String::new();
        for n in 0..2000 {
            text.push_str(&line(&format!("commit: number {n}")));
        }
        text.push_str(&line("merge late: Fast-forward"));
        std::fs::write(&log, &text).unwrap();
        assert!(text.len() as u64 > REFLOG_WINDOW);
        let read = repo.reflog_since(Some(0));
        assert!(read.lines.len() < 2001, "the whole file was read");
        assert!(read.lines.iter().any(|l| l.merged_branch().is_some()));
        // No half line at the front.
        assert!(read
            .lines
            .first()
            .is_some_and(|l| l.message.starts_with("commit: number ")));
    }

    #[test]
    fn where_head_came_from_is_the_newest_checkout_line() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(dir.path());
        assert_eq!(repo.came_from("feature/x"), Some("main".to_owned()));
        // The newest checkout went elsewhere: no answer rather than a stale one.
        assert_eq!(repo.came_from("main"), None);
        // Coming back from a detached commit names no branch.
        let log = repo.gitdir.join("logs").join("HEAD");
        let mut text = std::fs::read_to_string(&log).unwrap();
        text.push_str(&line(&format!("checkout: moving from {SHA} to main")));
        std::fs::write(&log, text).unwrap();
        assert_eq!(repo.came_from("main"), None);
    }

    #[test]
    fn a_worktree_has_its_own_head_and_reflog_and_shares_the_refs() {
        let dir = tempfile::tempdir().unwrap();
        let main = repo(&dir.path().join("main"));
        let wt_dir = main.gitdir.join("worktrees").join("wt");
        std::fs::create_dir_all(wt_dir.join("logs")).unwrap();
        std::fs::write(wt_dir.join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        std::fs::write(wt_dir.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            wt_dir.join("logs").join("HEAD"),
            line("checkout: moving from main to feature/x"),
        )
        .unwrap();
        let checkout = dir.path().join("wt");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(
            checkout.join(".git"),
            format!("gitdir: {}\n", wt_dir.display()),
        )
        .unwrap();

        let wt = Repo::open(&checkout).unwrap();
        assert_eq!(wt.head(), Head::Branch("feature/x".to_owned()));
        assert_eq!(main.head(), Head::Branch("main".to_owned()));
        assert_eq!(wt.branches(), main.branches());
        assert_eq!(wt.key(), main.key());
        assert_ne!(wt.worktree_key(), main.worktree_key());
        assert_eq!(wt.reflog_since(Some(0)).lines.len(), 1);
    }

    #[test]
    fn a_directory_without_git_is_not_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Repo::open(dir.path()).is_none());
        std::fs::write(dir.path().join(".git"), "not a pointer").unwrap();
        assert!(Repo::open(dir.path()).is_none());
    }
}
