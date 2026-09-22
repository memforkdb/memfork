//! `memfork init --project`: one managed block in each client's instruction
//! file, and nothing else touched.
//!
//! Every test works in a repository created inside a temporary sandbox, with
//! an empty PATH and a temporary home, so the clients "installed" are exactly
//! the ones a test pretends are. The instruction files are created at run
//! time; none is checked in.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::path::{Path, PathBuf};
use std::process::Output;

use memfork::init::project::{BEGIN, END};
use support::Sandbox;

fn repository(sandbox: &Sandbox) -> PathBuf {
    let repo = sandbox.root().join("shop");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    repo
}

fn init(sandbox: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    let mut cmd = sandbox.command();
    cmd.current_dir(cwd).args(["init", "--project"]).args(args);
    cmd.output().unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// Pretend a client is installed, the way `memfork init` detects one: by the
/// directory it creates for itself in the home directory.
fn install(sandbox: &Sandbox, dir: &str) {
    std::fs::create_dir_all(sandbox.home().join(dir)).unwrap();
}

/// A `git` on PATH that records whether anything ran it.
fn trap_git(sandbox: &Sandbox) -> PathBuf {
    let marker = sandbox.root().join("git.ran");
    if cfg!(windows) {
        std::fs::write(
            sandbox.bin().join("git.cmd"),
            format!("@echo off\r\necho ran>\"{}\"\r\n", marker.display()),
        )
        .unwrap();
    } else {
        let path = sandbox.bin().join("git");
        std::fs::write(
            &path,
            format!("#!/bin/sh\necho ran > '{}'\n", marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    marker
}

#[test]
fn installed_clients_get_their_own_files() {
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    install(&sandbox, ".codex");
    install(&sandbox, ".gemini");

    let out = init(&sandbox, &repo, &[]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("AGENTS.md"), "{text}");
    assert!(text.contains("read by Codex CLI"), "{text}");
    assert!(text.contains("GEMINI.md"), "{text}");
    assert!(text.contains("read by Gemini CLI"), "{text}");

    for name in ["AGENTS.md", "GEMINI.md"] {
        let written = read(&repo.join(name));
        assert!(written.starts_with(BEGIN), "{name}: {written}");
        assert!(written.trim_end().ends_with(END), "{name}: {written}");
        assert!(written.contains("memfork_resume"), "{name}");
    }
    // A client that is not installed gets nothing.
    assert!(!repo.join("CLAUDE.md").exists());
}

#[test]
fn nothing_installed_is_an_error_that_says_what_to_do() {
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let out = init(&sandbox, &repo, &[]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("--client"), "{err}");
    assert!(err.contains("--all"), "{err}");
    assert_eq!(
        std::fs::read_dir(&repo).unwrap().count(),
        1,
        "only .git remains"
    );
}

#[test]
fn named_clients_are_written_whether_installed_or_not() {
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let out = init(
        &sandbox,
        &repo,
        &["--client", "claude-code", "--client", "cursor"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(repo.join("CLAUDE.md").exists());
    assert!(repo.join("AGENTS.md").exists());
    assert!(!repo.join("GEMINI.md").exists());

    let bad = init(&sandbox, &repo, &["--client", "no-such-client"]);
    assert!(!bad.status.success());
    assert!(stderr(&bad).contains("known clients"), "{}", stderr(&bad));
}

#[test]
fn only_the_block_is_ever_touched() {
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let original = "# How we work\r\n\r\nRun the tests before pushing.\r\n";
    std::fs::write(repo.join("AGENTS.md"), original).unwrap();

    // Added after what is there, in the file's own line endings.
    let out = init(&sandbox, &repo, &["--client", "codex"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let added = read(&repo.join("AGENTS.md"));
    assert!(added.starts_with(original), "{added:?}");
    assert!(
        !added.replace("\r\n", "").contains('\n'),
        "a bare LF crept in"
    );
    assert!(stdout(&out).contains("added block"), "{}", stdout(&out));

    // Edits outside the block survive a re-run; the block is put back as it
    // should be.
    let edited = added
        .replace("Run the tests", "Always run the tests")
        .replace("memfork_resume", "something_else");
    std::fs::write(repo.join("AGENTS.md"), &edited).unwrap();
    let out = init(&sandbox, &repo, &["--client", "codex"]);
    assert!(stdout(&out).contains("updated block"), "{}", stdout(&out));
    let rerun = read(&repo.join("AGENTS.md"));
    assert!(rerun.contains("Always run the tests"));
    assert!(rerun.contains("memfork_resume"));
    assert!(!rerun.contains("something_else"));

    // A second run with nothing to change changes nothing.
    let out = init(&sandbox, &repo, &["--client", "codex"]);
    assert!(stdout(&out).contains("up to date"), "{}", stdout(&out));
    assert_eq!(read(&repo.join("AGENTS.md")), rerun);

    // And removal leaves exactly what the person wrote.
    let out = init(&sandbox, &repo, &["--client", "codex", "--remove"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        read(&repo.join("AGENTS.md")),
        original.replace("Run the tests", "Always run the tests")
    );
}

#[test]
fn removing_a_block_from_a_file_it_created_leaves_an_empty_file_and_says_so() {
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    init(&sandbox, &repo, &["--client", "gemini-cli"]);
    let out = init(&sandbox, &repo, &["--client", "gemini-cli", "--remove"]);
    assert!(out.status.success());
    assert_eq!(read(&repo.join("GEMINI.md")), "");
    assert!(
        stdout(&out).contains("holds nothing else"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn dry_run_shows_every_file_and_the_exact_diff_and_writes_nothing() {
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    std::fs::write(repo.join("CLAUDE.md"), "Existing notes.\n").unwrap();

    let out = init(&sandbox, &repo, &["--all", "--dry-run"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    // One block per file, and each file names the clients that read it.
    assert!(
        text.contains("read by Cursor, Codex CLI, Grok Build, Cline, OpenCode, Qwen Code"),
        "{text}"
    );
    assert!(text.contains("read by Claude Code"), "{text}");
    assert!(text.contains("read by Gemini CLI"), "{text}");
    assert!(text.contains("--- a/CLAUDE.md"), "{text}");
    assert!(text.contains("       Existing notes."), "{text}");
    assert!(text.contains("--- /dev/null"), "{text}");
    assert!(text.contains(&format!("+{END}")), "{text}");
    assert!(text.contains("Nothing was written"), "{text}");

    assert_eq!(read(&repo.join("CLAUDE.md")), "Existing notes.\n");
    assert!(!repo.join("AGENTS.md").exists());
    assert!(!repo.join("GEMINI.md").exists());
}

#[test]
fn json_output_lists_files_clients_and_diffs() {
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let out = init(
        &sandbox,
        &repo,
        &["--client", "codex", "--dry-run", "--json"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["dry_run"], true);
    assert_eq!(doc["files"][0]["file"], "AGENTS.md");
    assert_eq!(doc["files"][0]["clients"][0], "Codex CLI");
    assert_eq!(doc["files"][0]["change"], "create");
    assert!(doc["files"][0]["diff"]
        .as_str()
        .unwrap()
        .contains("+++ b/AGENTS.md"));
}

#[test]
fn it_works_from_anywhere_inside_the_repository_and_never_runs_git() {
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let deep = repo.join("src").join("cart");
    std::fs::create_dir_all(&deep).unwrap();
    let ran = trap_git(&sandbox);

    let out = init(&sandbox, &deep, &["--client", "codex"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(repo.join("AGENTS.md").exists(), "written at the top level");
    assert!(!deep.join("AGENTS.md").exists());
    assert!(!ran.exists(), "git was run");
}

#[test]
fn outside_a_repository_it_refuses() {
    let sandbox = Sandbox::new();
    let plain = sandbox.root().join("not-a-repo");
    std::fs::create_dir_all(&plain).unwrap();
    if memfork::namespace::repository_root(&plain).is_some() {
        return; // the temporary directory is itself inside a repository here
    }
    let out = init(&sandbox, &plain, &["--client", "codex"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("not inside a repository"),
        "{}",
        stderr(&out)
    );
    assert!(!plain.join("AGENTS.md").exists());
}

#[test]
fn the_flags_that_only_mean_something_with_project_require_it() {
    let sandbox = Sandbox::new();
    for flag in ["--all", "--remove"] {
        let out = sandbox.command().args(["init", flag]).output().unwrap();
        assert!(
            !out.status.success(),
            "{flag} was accepted without --project"
        );
    }
}

#[test]
fn plain_init_never_edits_project_files() {
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    install(&sandbox, ".codex");
    install(&sandbox, ".gemini");
    std::fs::write(repo.join("AGENTS.md"), "mine\n").unwrap();

    let out = sandbox
        .command()
        .current_dir(&repo)
        .args(["init", "--dry-run"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let out = sandbox
        .command()
        .current_dir(&repo)
        .args(["init"])
        .output()
        .unwrap();
    let _ = out; // registration may or may not succeed here; either way:
    assert_eq!(read(&repo.join("AGENTS.md")), "mine\n");
    assert!(!repo.join("GEMINI.md").exists());
    assert!(!repo.join("CLAUDE.md").exists());
}
