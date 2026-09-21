//! `memfork init --project` — one managed block in each client's own
//! instruction file (DESIGN §6.4).
//!
//! Every client reads a file of instructions from the repository it is working
//! in, and each has its own name for it. This writes the same short block into
//! the files the chosen clients read, so each of them is told the same routine:
//! resume when you start, record decisions as you go, hand off before you
//! stop, fork before anything risky.
//!
//! **Managed** is the contract. The block sits between two marker comments and
//! nothing outside them is ever touched:
//!
//! * a file that lacks the block gets it appended, after a blank line;
//! * a file that has it gets only what is between the markers replaced;
//! * `--remove` takes out only the block (and the blank line an append put
//!   before it);
//! * a file that does not exist is created holding only the block;
//! * line endings and a byte-order mark are kept as the file had them.
//!
//! Which files is data: each client's entry in the registry lists what it
//! reads, and [`crate::clients::instruction_files`] picks the fewest that reach
//! every chosen client. No code here knows a client or a file name. It never
//! runs git: the repository is found by looking for `.git`, and what happens to
//! the files afterwards — committing them or not — is the person's business.

use std::path::{Path, PathBuf};

/// The line that opens the block.
pub const BEGIN: &str =
    "<!-- memfork:begin (managed by `memfork init --project`; edits between these markers are replaced) -->";

/// The line that closes the block.
pub const END: &str = "<!-- memfork:end -->";

/// How a begin marker is recognised: its wording may change between versions,
/// and an older block must still be found and replaced.
const BEGIN_TAG: &str = "<!-- memfork:begin";

/// The instruction file most clients share, for tests and messages.
pub const AGENTS_FILE: &str = "AGENTS.md";

/// The block's body: the same words for every client.
const BODY: &str = "\
## Shared memory: MemFork

This repository's working memory lives in MemFork, which every AI tool used \
here can reach through its MCP tools. Other agents, from any vendor, read what \
you record and continue from it, so record as you go rather than at the end.

- When you start, call `memfork_resume` to pick up what earlier agents decided \
and did.
- As you work, store each decision with its reason using `memfork_put` under \
`<project>:decision:<topic>`, where `<project>` is the namespace MemFork names \
when you connect.
- Before you stop or hand over, call `memfork_handoff` with what is done, what \
comes next and what is blocking.
- Before anything risky, call `memfork_fork`. If it worked, `memfork_merge`; if \
it did not, `memfork_discard`.";

/// What would happen to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// It does not exist and will be created holding only the block.
    Create,
    /// The block will be added after what is there.
    Append,
    /// The block is there and will be replaced.
    Update,
    /// The block is there and already says this.
    Unchanged,
    /// The block will be taken out.
    Remove,
    /// There is no block to take out.
    Absent,
}

impl Change {
    /// Whether the file is written.
    pub fn writes(self) -> bool {
        matches!(
            self,
            Change::Create | Change::Append | Change::Update | Change::Remove
        )
    }

    /// A word for it, present tense.
    pub fn verb(self) -> &'static str {
        match self {
            Change::Create => "create",
            Change::Append => "add block",
            Change::Update => "update block",
            Change::Unchanged => "up to date",
            Change::Remove => "remove block",
            Change::Absent => "no block",
        }
    }

    /// The same, once done.
    pub fn done(self) -> &'static str {
        match self {
            Change::Create => "created",
            Change::Append => "added block",
            Change::Update => "updated block",
            Change::Remove => "removed block",
            other => other.verb(),
        }
    }
}

/// The plan for one file.
#[derive(Debug, Clone)]
pub struct FilePlan {
    /// Relative to the repository root, with `/` separators.
    pub relative: String,
    /// Where it is.
    pub path: PathBuf,
    /// The display names of the clients that read it.
    pub clients: Vec<String>,
    /// What will happen.
    pub change: Change,
    /// Its text now, `None` if it does not exist.
    pub before: Option<String>,
    /// Its text afterwards.
    pub after: String,
}

impl FilePlan {
    /// The exact change, as a unified diff; empty when nothing changes.
    pub fn diff(&self) -> String {
        if !self.change.writes() {
            return String::new();
        }
        let before = self.before.as_deref().unwrap_or("");
        let old_name = match self.before {
            Some(_) => format!("a/{}", self.relative),
            None => "/dev/null".to_owned(),
        };
        similar::TextDiff::from_lines(before, &self.after)
            .unified_diff()
            .context_radius(3)
            .header(&old_name, &format!("b/{}", self.relative))
            .to_string()
    }

    /// Whether the file holds nothing but whitespace afterwards.
    pub fn left_empty(&self) -> bool {
        self.change == Change::Remove && self.after.trim().is_empty()
    }
}

/// Work out what to do to each file. `files` pairs a path relative to `root`
/// with the display names of the clients it serves.
pub fn plan(
    root: &Path,
    files: &[(String, Vec<String>)],
    remove: bool,
) -> Result<Vec<FilePlan>, String> {
    files
        .iter()
        .map(|(relative, clients)| {
            let path = relative
                .split('/')
                .fold(root.to_path_buf(), |p, part| p.join(part));
            let before = match std::fs::read(&path) {
                Ok(bytes) => Some(String::from_utf8(bytes).map_err(|_| {
                    format!("{relative} is not UTF-8 text, so MemFork will not edit it")
                })?),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("cannot read {relative}: {e}")),
            };
            let (change, after) = match &before {
                None if remove => (Change::Absent, String::new()),
                None => (Change::Create, format!("{}\n", block("\n"))),
                Some(text) => edit(text, remove).map_err(|why| format!("{relative}: {why}"))?,
            };
            Ok(FilePlan {
                relative: relative.clone(),
                path,
                clients: clients.clone(),
                change,
                before,
                after,
            })
        })
        .collect()
}

/// Carry out one file's plan.
pub fn apply(plan: &FilePlan) -> Result<(), String> {
    if !plan.change.writes() {
        return Ok(());
    }
    if let Some(parent) = plan.path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    // Written beside the file and renamed over it, so a failure part-way
    // leaves the file as it was rather than half-written.
    let tmp = plan.path.with_extension("memfork-tmp");
    std::fs::write(&tmp, &plan.after)
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &plan.path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot replace {}: {e}", plan.path.display())
    })
}

/// The block, with the given line ending, and no ending after the last line.
fn block(nl: &str) -> String {
    let mut out = String::new();
    out.push_str(BEGIN);
    out.push_str(nl);
    for line in BODY.lines() {
        out.push_str(line);
        out.push_str(nl);
    }
    out.push_str(END);
    out
}

/// Where an existing block is: byte offsets of the start of its begin line
/// and the end of its end line, including that line's ending.
fn locate(text: &str) -> Result<Option<(usize, usize)>, String> {
    let mut begin = None;
    let mut found = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\r', '\n']).trim();
        if bare.starts_with(BEGIN_TAG) {
            if begin.is_some() || found.is_some() {
                return Err(
                    "it has more than one MemFork block; leave one and run this again".to_owned(),
                );
            }
            begin = Some(offset);
        } else if bare == END {
            match begin.take() {
                Some(start) => found = Some((start, offset + line.len())),
                None => {
                    return Err(
                        "it has a MemFork end marker with no begin marker; fix it by hand"
                            .to_owned(),
                    )
                }
            }
        }
        offset += line.len();
    }
    if begin.is_some() {
        return Err("it has a MemFork begin marker with no end marker; fix it by hand".to_owned());
    }
    Ok(found)
}

/// The new text of an existing file.
fn edit(text: &str, remove: bool) -> Result<(Change, String), String> {
    let (bom, body) = match text.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", text),
    };
    let nl = if body.contains("\r\n") { "\r\n" } else { "\n" };
    let fresh = block(nl);

    match (locate(body)?, remove) {
        (Some((start, end)), false) => {
            let old = &body[start..end];
            let ending = if old.ends_with('\n') { nl } else { "" };
            let replacement = format!("{fresh}{ending}");
            if old == replacement {
                return Ok((Change::Unchanged, text.to_owned()));
            }
            Ok((
                Change::Update,
                format!("{bom}{}{replacement}{}", &body[..start], &body[end..]),
            ))
        }
        (Some((start, end)), true) => {
            let mut head = &body[..start];
            let tail = &body[end..];
            // At the end of the file, the blank line an append put before the
            // block goes with it, so adding and removing leaves the file as
            // it was.
            if tail.is_empty() {
                let doubled = format!("{nl}{nl}");
                if head.ends_with(&doubled) {
                    head = &head[..head.len() - nl.len()];
                }
            }
            Ok((Change::Remove, format!("{bom}{head}{tail}")))
        }
        (None, true) => Ok((Change::Absent, text.to_owned())),
        (None, false) => {
            let separator = if body.is_empty() {
                ""
            } else if body.ends_with('\n') {
                nl
            } else {
                // The last line had no ending; it needs one before the blank
                // line that separates the block.
                if nl == "\r\n" {
                    "\r\n\r\n"
                } else {
                    "\n\n"
                }
            };
            Ok((Change::Append, format!("{bom}{body}{separator}{fresh}{nl}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_block_follows_what_is_there() {
        let (change, after) = edit("# Project\n\nOur rules.\n", false).unwrap();
        assert_eq!(change, Change::Append);
        assert!(after.starts_with("# Project\n\nOur rules.\n\n<!-- memfork:begin"));
        assert!(after.ends_with("<!-- memfork:end -->\n"));
    }

    #[test]
    fn running_again_changes_nothing() {
        let (_, once) = edit("# Project\n", false).unwrap();
        let (change, twice) = edit(&once, false).unwrap();
        assert_eq!(change, Change::Unchanged);
        assert_eq!(once, twice);
    }

    #[test]
    fn only_the_block_is_replaced() {
        let outdated = format!(
            "intro\n\n<!-- memfork:begin (an older wording) -->\nold words\n{END}\n\noutro\n"
        );
        let (change, after) = edit(&outdated, false).unwrap();
        assert_eq!(change, Change::Update);
        assert!(after.starts_with("intro\n\n<!-- memfork:begin (managed"));
        assert!(after.ends_with(&format!("{END}\n\noutro\n")));
        assert!(!after.contains("old words"));
    }

    #[test]
    fn adding_then_removing_gives_back_the_original() {
        for original in ["# Rules\n\nBe kind.\n", "", "one line\n", "a\r\nb\r\n"] {
            let (_, added) = edit(original, false).unwrap();
            let (change, removed) = edit(&added, true).unwrap();
            assert_eq!(change, Change::Remove);
            assert_eq!(removed, original, "round trip of {original:?}");
        }
    }

    #[test]
    fn crlf_files_stay_crlf_and_a_bom_stays() {
        let (_, after) = edit("\u{feff}# Rules\r\n", false).unwrap();
        assert!(after.starts_with("\u{feff}# Rules\r\n\r\n<!-- memfork:begin"));
        let bare_lf = after.replace("\r\n", "");
        assert!(!bare_lf.contains('\n'), "a bare LF crept in");
    }

    #[test]
    fn a_file_with_no_final_newline_gets_one_before_the_block() {
        let (_, after) = edit("last line", false).unwrap();
        assert!(after.starts_with("last line\n\n<!-- memfork:begin"));
    }

    #[test]
    fn a_block_in_the_middle_is_removed_without_touching_either_side() {
        let text = format!("top\n\n{}\n\nbottom\n", block("\n"));
        let (_, after) = edit(&text, true).unwrap();
        assert_eq!(after, "top\n\n\nbottom\n");
    }

    #[test]
    fn broken_markers_are_refused_not_guessed_at() {
        assert!(edit("<!-- memfork:begin -->\nno end\n", false)
            .unwrap_err()
            .contains("no end marker"));
        assert!(edit(&format!("{END}\n"), false)
            .unwrap_err()
            .contains("no begin marker"));
        let two = format!("{}\n{}\n", block("\n"), block("\n"));
        assert!(edit(&two, false).unwrap_err().contains("more than one"));
    }

    #[test]
    fn removing_from_a_file_without_a_block_changes_nothing() {
        let (change, after) = edit("mine\n", true).unwrap();
        assert_eq!(change, Change::Absent);
        assert_eq!(after, "mine\n");
    }

    #[test]
    fn the_block_says_the_routine_and_names_no_vendor() {
        let text = block("\n").to_ascii_lowercase();
        for step in [
            "memfork_resume",
            "memfork_put",
            "memfork_handoff",
            "memfork_fork",
            "memfork_merge",
            "memfork_discard",
            ":decision:",
        ] {
            assert!(text.contains(step), "no `{step}`");
        }
        for vendor in [
            "claude",
            "anthropic",
            "openai",
            "codex",
            "cursor",
            "gemini",
            "google",
            "grok",
            "xai",
        ] {
            assert!(!text.contains(vendor), "mentions `{vendor}`");
        }
    }

    #[test]
    fn the_diff_shows_exactly_the_change() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("NOTES.md"), "keep me\n").unwrap();
        let plans = plan(
            tmp.path(),
            &[
                ("NOTES.md".to_owned(), vec!["A".to_owned()]),
                ("sub/NEW.md".to_owned(), vec!["B".to_owned()]),
            ],
            false,
        )
        .unwrap();
        let existing = plans[0].diff();
        assert!(
            existing.starts_with("--- a/NOTES.md\n+++ b/NOTES.md\n"),
            "{existing}"
        );
        assert!(existing.contains(" keep me\n"), "{existing}");
        assert!(existing.contains("+<!-- memfork:begin"), "{existing}");
        assert!(!existing.contains("-keep me"), "{existing}");
        let created = plans[1].diff();
        assert!(
            created.starts_with("--- /dev/null\n+++ b/sub/NEW.md\n"),
            "{created}"
        );

        for p in &plans {
            apply(p).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("sub").join("NEW.md")).unwrap(),
            plans[1].after
        );
        assert!(!tmp.path().join("NOTES.memfork-tmp").exists());
    }
}
