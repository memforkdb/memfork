//! Project namespaces (DESIGN §6.3).
//!
//! One store serves every project on the machine, so what an agent records
//! about a project goes under that project's name: `<namespace>:decision:…`,
//! `<namespace>:task:…`, `<namespace>:handoff:…`. The namespace is worked out
//! from where the client was started — the repository's top-level directory
//! name, else the working directory's — and handed to the agent in the MCP
//! instructions, so no file has to be edited for it to learn the name.
//!
//! Only the handoff and resume tools use it. The raw tools take literal keys,
//! as they always have: nothing is prefixed behind the caller's back, and
//! everything stored before namespaces existed stays exactly where it was.
//!
//! Finding the repository never runs git. It walks up from the working
//! directory to the first `.git`, which is a directory in an ordinary clone
//! and a file in a worktree or submodule; either marks the top level.

use std::path::Path;

/// The environment variable that names the namespace outright.
pub const NAMESPACE_ENV: &str = "MEMFORK_NAMESPACE";

/// The namespace used when nothing better can be worked out.
pub const FALLBACK: &str = "default";

/// Longest namespace, in bytes.
pub const MAX_LEN: usize = 64;

/// The separator between the parts of a conventional key.
///
/// A colon, as `memfork_put` has always taught (`decision:42`), so a project's
/// keys are the same convention with one more part in front:
/// `myproject:decision:42`. A namespace can never contain one, so the prefix
/// `myproject:` matches that project's keys and nothing else.
pub const SEPARATOR: char = ':';

/// Where a namespace came from, for messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `--namespace`.
    Flag,
    /// [`NAMESPACE_ENV`].
    Environment,
    /// The repository's top-level directory.
    Repository,
    /// The working directory, outside any repository.
    WorkingDirectory,
    /// Nothing usable; [`FALLBACK`].
    Fallback,
}

/// A resolved namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Namespace {
    /// The name, already valid.
    pub name: String,
    /// How it was chosen.
    pub source: Source,
}

/// Work out the namespace for a session started in `cwd`.
///
/// An explicit name — the flag, then the environment variable — wins, and is
/// refused rather than altered if it is not a valid namespace: something a
/// person typed should not be silently changed into something else. A name
/// derived from a directory is sanitised instead, since nobody chose it.
pub fn resolve(flag: Option<&str>, env: Option<&str>, cwd: &Path) -> Result<Namespace, String> {
    for (given, source, what) in [
        (flag, Source::Flag, "--namespace"),
        (env, Source::Environment, NAMESPACE_ENV),
    ] {
        if let Some(raw) = given.map(str::trim).filter(|s| !s.is_empty()) {
            validate(raw).map_err(|why| {
                format!(
                    "{what} `{raw}` is not a usable namespace: {why}. Something like `{}` would be.",
                    sanitise(raw).unwrap_or_else(|| FALLBACK.to_owned())
                )
            })?;
            return Ok(Namespace {
                name: raw.to_owned(),
                source,
            });
        }
    }
    Ok(detect(cwd))
}

/// The namespace a directory implies, with no flag or variable involved.
pub fn detect(cwd: &Path) -> Namespace {
    let (dir, source) = match repository_root(cwd) {
        Some(root) => (root, Source::Repository),
        None => (cwd.to_path_buf(), Source::WorkingDirectory),
    };
    match dir.file_name().and_then(|n| n.to_str()).and_then(sanitise) {
        Some(name) => Namespace { name, source },
        None => Namespace {
            name: FALLBACK.to_owned(),
            source: Source::Fallback,
        },
    }
}

/// The top level of the repository containing `start`, found without git.
pub fn repository_root(start: &Path) -> Option<std::path::PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Whether `name` is a valid namespace as it stands.
pub fn validate(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("it is empty");
    }
    if name.len() > MAX_LEN {
        return Err("it is longer than 64 characters");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err("only lowercase letters, digits, `.`, `_` and `-` are allowed");
    }
    if name.starts_with(['.', '-']) || name.ends_with(['.', '-']) {
        return Err("it may not start or end with `.` or `-`");
    }
    Ok(())
}

/// Turn a directory name into a namespace, or `None` if nothing is left.
///
/// Lowercased; every run of characters outside `[a-z0-9._]`, dashes included,
/// becomes one `-`;
/// leading and trailing `.` and `-` are dropped; cut to [`MAX_LEN`].
pub fn sanitise(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars().flat_map(char::to_lowercase) {
        let kept = c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_');
        if kept {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            // A dash already in the name and a run of anything else are the
            // same thing afterwards: one dash.
            out.push('-');
        }
    }
    let trimmed = out.trim_matches(['.', '-']);
    let mut cut = trimmed.chars().take(MAX_LEN).collect::<String>();
    while cut.ends_with(['.', '-']) {
        cut.pop();
    }
    (!cut.is_empty()).then_some(cut)
}

/// The key prefix for everything of one kind in a namespace, such as
/// `myproject:decision:`.
pub fn prefix(namespace: &str, kind: &str) -> String {
    format!("{namespace}{SEPARATOR}{kind}{SEPARATOR}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_names_become_namespaces() {
        assert_eq!(sanitise("memfork").as_deref(), Some("memfork"));
        assert_eq!(sanitise("MemFork").as_deref(), Some("memfork"));
        assert_eq!(sanitise("My Project (2)").as_deref(), Some("my-project-2"));
        assert_eq!(sanitise("web_app.v2").as_deref(), Some("web_app.v2"));
        assert_eq!(sanitise("--odd--").as_deref(), Some("odd"));
        assert_eq!(sanitise("café-ünï").as_deref(), Some("caf-n"));
        assert_eq!(sanitise("项目"), None);
        assert_eq!(sanitise(""), None);
        let long = "a".repeat(100);
        assert_eq!(sanitise(&long).map(|s| s.len()), Some(MAX_LEN));
    }

    #[test]
    fn a_sanitised_name_is_always_valid() {
        for raw in ["memfork", "My Project", "a.b-c_d", "x--", "-.-y", "ÄÖÜ abc"] {
            if let Some(name) = sanitise(raw) {
                assert_eq!(validate(&name), Ok(()), "{raw} -> {name}");
                assert!(!name.contains(SEPARATOR));
            }
        }
    }

    #[test]
    fn explicit_names_are_checked_not_rewritten() {
        let here = Path::new(".");
        assert_eq!(
            resolve(Some("team-api"), Some("other"), here).unwrap(),
            Namespace {
                name: "team-api".to_owned(),
                source: Source::Flag
            }
        );
        assert_eq!(
            resolve(None, Some("other"), here).unwrap().source,
            Source::Environment
        );
        let err = resolve(Some("Team API"), None, here).unwrap_err();
        assert!(err.contains("--namespace `Team API`"), "{err}");
        assert!(err.contains("`team-api`"), "{err}");
        let err = resolve(None, Some("a:b"), here).unwrap_err();
        assert!(err.contains(NAMESPACE_ENV), "{err}");
    }

    #[test]
    fn blank_explicit_values_fall_through() {
        let dir = tempfile::tempdir().unwrap();
        let ns = resolve(Some("  "), Some(""), dir.path()).unwrap();
        assert_ne!(ns.source, Source::Flag);
        assert_ne!(ns.source, Source::Environment);
    }

    #[test]
    fn the_repository_root_names_the_namespace_from_any_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("Shop Front");
        let deep = repo.join("src").join("cart");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        for start in [&repo, &deep] {
            let ns = detect(start);
            assert_eq!(ns.name, "shop-front");
            assert_eq!(ns.source, Source::Repository);
        }
    }

    #[test]
    fn a_worktree_git_file_marks_the_top_level_too() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("feature-x");
        std::fs::create_dir_all(wt.join("docs")).unwrap();
        std::fs::write(
            wt.join(".git"),
            "gitdir: /elsewhere/.git/worktrees/feature-x\n",
        )
        .unwrap();
        let ns = detect(&wt.join("docs"));
        assert_eq!(ns.name, "feature-x");
        assert_eq!(ns.source, Source::Repository);
    }

    #[test]
    fn outside_a_repository_the_working_directory_names_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Scratch Notes");
        std::fs::create_dir_all(&dir).unwrap();
        // The temporary directory itself may sit inside a repository on some
        // machines; only assert when it does not.
        if repository_root(&dir).is_none() {
            let ns = detect(&dir);
            assert_eq!(ns.name, "scratch-notes");
            assert_eq!(ns.source, Source::WorkingDirectory);
        }
    }

    #[test]
    fn a_name_with_nothing_usable_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("项目");
        std::fs::create_dir_all(&dir).unwrap();
        if repository_root(&dir).is_none() {
            let ns = detect(&dir);
            assert_eq!(ns.name, FALLBACK);
            assert_eq!(ns.source, Source::Fallback);
        }
    }

    #[test]
    fn prefixes_use_the_colon_convention() {
        assert_eq!(prefix("shop", "decision"), "shop:decision:");
    }
}
