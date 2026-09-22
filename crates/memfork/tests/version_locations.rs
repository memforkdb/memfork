//! The version lives in exactly the places `docs/RELEASING.md` lists.
//!
//! A release bumps those three lines and nothing else. A fourth copy of the
//! version — a doc URL, a constant, a test expectation — is one a release
//! would forget, so any tracked file that names the current version outside
//! them fails here.
//!
//! Left out on purpose: `CHANGELOG.md`, which is a history and names every
//! version; `Cargo.lock`, which Cargo writes; and `tests/fixtures/`, which
//! hold stores written by earlier releases and say so, with `tests/compat.rs`,
//! which opens them by the version that wrote them.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Command;

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// The three places, as (file, what the line starts with).
const ALLOWED: &[(&str, &str)] = &[
    ("Cargo.toml", "version = "),
    (
        "Cargo.toml",
        "memfork-core = { path = \"crates/memfork-core\", version = ",
    ),
    (
        "crates/memfork-py/Cargo.toml",
        "memfork = { path = \"../memfork\", version = ",
    ),
];

#[test]
fn the_version_appears_only_where_the_release_guide_says() {
    let version = env!("CARGO_PKG_VERSION");
    let root = workspace();

    // Tracked files only: a developer's untracked notes are not the
    // repository's business.
    let listed = match Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["ls-files", "-z"])
        .output()
    {
        Ok(out) if out.status.success() => out.stdout,
        _ => {
            eprintln!("not a git checkout; nothing to check");
            return;
        }
    };
    let files: Vec<String> = String::from_utf8(listed)
        .unwrap()
        .split('\0')
        .filter(|f| !f.is_empty())
        .map(str::to_owned)
        .collect();

    let mut found_allowed = Vec::new();
    let mut stray = Vec::new();
    for file in &files {
        if file == "CHANGELOG.md"
            || file == "Cargo.lock"
            || file.contains("tests/fixtures/")
            || file.ends_with("tests/compat.rs")
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(root.join(file)) else {
            continue; // binary
        };
        for (n, line) in text.lines().enumerate() {
            if !mentions(line, version) {
                continue;
            }
            let trimmed = line.trim_start();
            if ALLOWED
                .iter()
                .any(|(f, start)| f == file && trimmed.starts_with(start))
            {
                found_allowed.push((file.clone(), trimmed.to_owned()));
            } else {
                stray.push(format!("{file}:{}: {line}", n + 1));
            }
        }
    }

    assert!(
        stray.is_empty(),
        "the version {version} appears outside the places RELEASING.md lists:\n{}",
        stray.join("\n")
    );
    assert_eq!(
        found_allowed.len(),
        ALLOWED.len(),
        "not all of RELEASING.md's places carry {version}: {found_allowed:?}"
    );
}

/// Whether `line` names `version` on its own: not as part of a longer number
/// such as `10.1.1` or `0.1.10`.
fn mentions(line: &str, version: &str) -> bool {
    let bytes = line.as_bytes();
    line.match_indices(version).any(|(at, _)| {
        let end = at + version.len();
        let before = at.checked_sub(1).map(|i| bytes[i]);
        let digit = |i: usize| bytes.get(i).is_some_and(u8::is_ascii_digit);
        // A dot before it, or a dot and a digit after it, make it part of a
        // longer number; a full stop after it does not.
        let longer_before = before.is_some_and(|b| b.is_ascii_digit() || b == b'.');
        let longer_after = digit(end) || (bytes.get(end) == Some(&b'.') && digit(end + 1));
        !longer_before && !longer_after
    })
}

#[test]
fn a_fourth_copy_would_be_caught() {
    assert!(mentions(
        r#"#![doc(html_root_url = "https://docs.rs/memfork-core/1.2.3")]"#,
        "1.2.3"
    ));
    assert!(mentions("release v1.2.3 is out", "1.2.3"));
    assert!(mentions("Released as 1.2.3.", "1.2.3"));
    assert!(!mentions("1.2.3.4", "1.2.3"));
    assert!(!mentions("11.2.3", "1.2.3"));
    assert!(!mentions("1.2.34", "1.2.3"));
}
