//! Autopilot: memory follows the git branch, and memory is forked before a
//! risky step through a client's hooks.
//!
//! The repositories are real, made with the git on this machine, and the
//! moves of `HEAD` are real: `git switch`, `git merge`, `git checkout
//! --detach`, `git worktree add`, `git branch -d`. MemFork itself runs no
//! git, and a trapped `git` on the confined PATH proves it. Everything else
//! is the usual sandbox: a temporary data directory, home and PATH, the
//! guard against the real store, and a daemon that stops with the sandbox.
//!
//! Without git on the machine the git tests say so and pass; CI has it on
//! every runner.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use rmcp::model::{CallToolRequestParams, ClientConfig, Implementation};
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::RoleClient;
use serde_json::{json, Value as Json};
use support::Sandbox;

type Client = RunningService<RoleClient, ClientConfig>;

/// One daemon-starting test at a time, as in `handoff.rs`.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const AUTOPILOT_FILE: &str = memfork::autopilot::config::FILE;

// ---- git, real, found outside the sandbox ------------------------------------

/// The `git` this machine has, or `None` with a message.
fn git_binary() -> Option<PathBuf> {
    let found = Command::new("git").arg("--version").output().ok()?;
    if !found.status.success() {
        return None;
    }
    Some(PathBuf::from("git"))
}

/// Run git in `repo` with no user, system or global configuration in the
/// way, and fail loudly if it fails.
fn git(sandbox: &Sandbox, repo: &Path, args: &[&str]) -> String {
    let empty = sandbox.home().join("empty-gitconfig");
    let _ = std::fs::write(&empty, "");
    let out = Command::new(git_binary().expect("git"))
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=.git/hooks",
        ])
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", &empty)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("HOME", sandbox.home())
        .output()
        .expect("git ran");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A repository named `shop` on `main` with one commit, and autopilot on:
/// the autopilot file is committed, as the README says to, so every branch
/// made from here carries it.
fn repository(sandbox: &Sandbox) -> PathBuf {
    let repo = sandbox.root().join("shop");
    std::fs::create_dir_all(&repo).unwrap();
    git(sandbox, &repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "shop\n").unwrap();
    switch_on(&repo, None);
    git(sandbox, &repo, &["add", "."]);
    git(sandbox, &repo, &["commit", "-q", "-m", "one"]);
    repo
}

fn switch_on(repo: &Path, check: Option<&str>) {
    std::fs::write(
        repo.join(AUTOPILOT_FILE),
        memfork::autopilot::config::template(check),
    )
    .unwrap();
}

fn commit(sandbox: &Sandbox, repo: &Path, file: &str, text: &str) {
    std::fs::write(repo.join(file), text).unwrap();
    git(sandbox, repo, &["add", file]);
    git(sandbox, repo, &["commit", "-q", "-m", file]);
}

/// A `git` on the confined PATH that records whether anything ran it.
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

// ---- MCP sessions --------------------------------------------------------------

async fn connect(sandbox: &Sandbox, name: &str, cwd: &Path) -> Client {
    let mut std_cmd = sandbox.command();
    std_cmd.current_dir(cwd);
    let transport =
        TokioChildProcess::new(tokio::process::Command::from(std_cmd).configure(|cmd| {
            cmd.arg("mcp");
            cmd.stderr(Stdio::null());
        }))
        .expect("spawned `memfork mcp`");
    ClientConfig::new(Default::default(), Implementation::new(name, "1.0"))
        .serve(transport)
        .await
        .expect("the MCP handshake completed")
}

async fn call(client: &Client, tool: &str, args: Json) -> Json {
    let Json::Object(map) = args else {
        panic!("arguments must be an object")
    };
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(map))
        .await
        .unwrap_or_else(|e| panic!("`{tool}` failed: {e}"));
    assert_ne!(
        result.is_error,
        Some(true),
        "`{tool}` returned an error: {result:?}"
    );
    result.structured_content.expect("structured content")
}

async fn branch_of(client: &Client) -> (String, Vec<Json>) {
    let answer = call(client, "memfork_branches", json!({})).await;
    let notes = answer["autopilot"].as_array().cloned().unwrap_or_default();
    (answer["current_branch"].as_str().unwrap().to_owned(), notes)
}

fn kinds(notes: &[Json]) -> Vec<String> {
    notes
        .iter()
        .map(|n| n["kind"].as_str().unwrap_or("").to_owned())
        .collect()
}

// ---- the hook --------------------------------------------------------------------

fn hook(sandbox: &Sandbox, repo: &Path, event: &Json) -> Output {
    let mut child = sandbox
        .command()
        .args(["autopilot", "hook", "--client", "claude-code"])
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(event.to_string().as_bytes()).unwrap();
    }
    child.wait_with_output().unwrap()
}

fn pre_command(repo: &Path, command: &str, id: &str) -> Json {
    json!({
        "session_id": "s", "cwd": repo.display().to_string(), "hook_event_name": "PreToolUse",
        "tool_name": "Bash", "tool_input": { "command": command }, "tool_use_id": id,
    })
}

fn post_command(repo: &Path, command: &str, id: &str) -> Json {
    json!({
        "session_id": "s", "cwd": repo.display().to_string(), "hook_event_name": "PostToolUse",
        "tool_name": "Bash", "tool_input": { "command": command },
        "tool_response": { "stdout": "ok", "stderr": "", "interrupted": false, "isImage": false },
        "tool_use_id": id,
    })
}

fn failed_command(repo: &Path, command: &str, id: &str, error: &str) -> Json {
    json!({
        "session_id": "s", "cwd": repo.display().to_string(), "hook_event_name": "PostToolUseFailure",
        "tool_name": "Bash", "tool_input": { "command": command }, "tool_use_id": id,
        "error": error, "is_interrupt": false,
    })
}

fn pre_edit(repo: &Path, file: &str) -> Json {
    json!({
        "session_id": "s", "cwd": repo.display().to_string(), "hook_event_name": "PreToolUse",
        "tool_name": "Edit", "tool_input": { "file_path": repo.join(file).display().to_string() },
        "tool_use_id": format!("edit-{file}"),
    })
}

fn stop(repo: &Path) -> Json {
    json!({
        "session_id": "s", "cwd": repo.display().to_string(), "hook_event_name": "Stop",
        "stop_hook_active": false,
    })
}

fn silent(out: &Output) {
    assert!(out.status.success(), "the hook did not exit 0: {out:?}");
    assert!(out.stdout.is_empty(), "the hook wrote to stdout: {out:?}");
    assert!(out.stderr.is_empty(), "the hook wrote to stderr: {out:?}");
}

fn status_json(sandbox: &Sandbox, repo: &Path) -> Json {
    let out = sandbox
        .command()
        .args(["autopilot", "status", "--json"])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

// ---- 4a: memory follows the git branch ---------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn memory_follows_a_switch_forks_on_the_first_one_and_merges_with_git_without_running_it() {
    let _turn = ONE_AT_A_TIME.lock().await;
    if git_binary().is_none() {
        eprintln!("skipped: no git on this machine");
        return;
    }
    let sandbox = Sandbox::new();
    let marker = trap_git(&sandbox);
    let repo = repository(&sandbox);
    let agent = connect(&sandbox, "claude-code", &repo).await;

    // On main, memory is on main, and the first look says nothing.
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "main");
    assert!(notes.is_empty(), "{notes:?}");
    call(
        &agent,
        "memfork_put",
        json!({ "key": "shop:decision:db", "value": "postgres" }),
    )
    .await;

    // A new branch in git: memory forks one of the same name from main.
    git(&sandbox, &repo, &["switch", "-q", "-c", "feature/x"]);
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "feature/x");
    assert_eq!(kinds(&notes), ["follow"]);
    assert_eq!(notes[0]["from"], "main");
    assert!(notes[0]["note"].as_str().unwrap().contains("forked"));
    call(
        &agent,
        "memfork_put",
        json!({ "key": "shop:decision:cache", "value": "redis" }),
    )
    .await;
    commit(&sandbox, &repo, "cache.txt", "redis");

    // Back to main: a plain switch, and the branch's writes are not there.
    git(&sandbox, &repo, &["switch", "-q", "main"]);
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "main");
    assert_eq!(kinds(&notes), ["follow"]);
    let missing = call(
        &agent,
        "memfork_get",
        json!({ "key": "shop:decision:cache" }),
    )
    .await;
    assert_eq!(missing["found"], false, "{missing}");

    // Git merges the branch: so does memory.
    git(&sandbox, &repo, &["merge", "-q", "feature/x"]);
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "main");
    assert_eq!(kinds(&notes), ["merge"]);
    assert_eq!(notes[0]["source"], "feature/x");
    assert_eq!(notes[0]["target"], "main");
    let found = call(
        &agent,
        "memfork_get",
        json!({ "key": "shop:decision:cache" }),
    )
    .await;
    assert_eq!(found["value"], "redis");
    // Once: the next call carries nothing.
    let (_, notes) = branch_of(&agent).await;
    assert!(notes.is_empty(), "{notes:?}");

    // The feed says who did it: a second switch, watched as it happens.
    let feed = sandbox.root().join("feed.jsonl");
    let mut watcher = sandbox
        .command()
        .args(["watch", "--json", "--count", "50"])
        .current_dir(&repo)
        .stdout(Stdio::from(std::fs::File::create(&feed).unwrap()))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let watching = Instant::now();
    while std::fs::read_to_string(&feed)
        .unwrap_or_default()
        .is_empty()
    {
        assert!(
            watching.elapsed() < Duration::from_secs(10),
            "watch never said hello"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    git(&sandbox, &repo, &["switch", "-q", "-c", "feature/w"]);
    let (branch, _) = branch_of(&agent).await;
    assert_eq!(branch, "feature/w");
    let mut lines = String::new();
    while !lines.contains("\"operation\":\"follow\"") {
        assert!(
            watching.elapsed() < Duration::from_secs(10),
            "the feed never showed the follow: {lines}"
        );
        std::thread::sleep(Duration::from_millis(50));
        lines = std::fs::read_to_string(&feed).unwrap_or_default();
    }
    let _ = watcher.kill();
    let _ = watcher.wait();
    let event: Json = lines
        .lines()
        .filter_map(|l| serde_json::from_str::<Json>(l).ok())
        .find(|e| e["operation"] == "follow")
        .unwrap();
    assert_eq!(event["client"], "memfork-autopilot");
    assert_eq!(event["branch"], "feature/w");
    assert_eq!(event["detail"], "forked from main");
    git(&sandbox, &repo, &["switch", "-q", "main"]);
    let (_, _) = branch_of(&agent).await;

    // Git deleted the branch: memory keeps it and lists it, with why and
    // both ways out; nothing is discarded.
    git(&sandbox, &repo, &["branch", "-q", "-d", "feature/x"]);
    let (_, _) = branch_of(&agent).await;
    let status = status_json(&sandbox, &repo);
    assert_eq!(status["follows_git"], true);
    let orphans = status["daemon"]["orphans"].as_array().unwrap();
    assert_eq!(orphans.len(), 1, "{status}");
    assert_eq!(orphans[0]["branch"], "feature/x");
    assert!(orphans[0]["why"]
        .as_str()
        .unwrap()
        .contains("git branch deleted, or squash-merged"));
    assert_eq!(
        orphans[0]["merge_then_discard"][0],
        "memfork merge feature/x --branch main"
    );
    assert!(orphans[0]["discard"]
        .as_str()
        .unwrap()
        .starts_with("memfork discard feature/x --lesson"));
    let branches = call(&agent, "memfork_branches", json!({})).await;
    assert!(
        branches["branches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["name"] == "feature/x"),
        "discarded silently: {branches}"
    );
    // Doctor prints the same, short report included.
    let doctor = sandbox
        .command()
        .arg("doctor")
        .current_dir(&repo)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        text.contains("autopilot     on: memory follows the git branch"),
        "{text}"
    );
    assert!(text.contains("orphan        feature/x"), "{text}");
    assert!(
        text.contains("memfork merge feature/x --branch main"),
        "{text}"
    );

    // MemFork ran no git for any of it.
    assert!(!marker.exists(), "MemFork ran git");
    agent.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conflicting_git_merge_is_reported_and_memory_is_not_forced() {
    let _turn = ONE_AT_A_TIME.lock().await;
    if git_binary().is_none() {
        eprintln!("skipped: no git on this machine");
        return;
    }
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let agent = connect(&sandbox, "claude-code", &repo).await;
    let (_, _) = branch_of(&agent).await;
    call(
        &agent,
        "memfork_put",
        json!({ "key": "shop:decision:db", "value": "postgres" }),
    )
    .await;
    git(&sandbox, &repo, &["switch", "-q", "-c", "feature/x"]);
    let (branch, _) = branch_of(&agent).await;
    assert_eq!(branch, "feature/x");
    call(
        &agent,
        "memfork_put",
        json!({ "key": "shop:decision:db", "value": "sqlite" }),
    )
    .await;
    commit(&sandbox, &repo, "x.txt", "x");
    git(&sandbox, &repo, &["switch", "-q", "main"]);
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "main", "{notes:?}");
    assert_eq!(kinds(&notes), ["follow"]);
    call(
        &agent,
        "memfork_put",
        json!({ "key": "shop:decision:db", "value": "mysql" }),
    )
    .await;
    commit(&sandbox, &repo, "y.txt", "y");
    // A true merge in git, with a memory conflict underneath.
    git(
        &sandbox,
        &repo,
        &["merge", "-q", "--no-ff", "-m", "merge x", "feature/x"],
    );
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "main");
    assert_eq!(kinds(&notes), ["conflict"]);
    assert_eq!(notes[0]["conflicts"], json!(["shop:decision:db"]));
    assert!(notes[0]["hint"].as_str().unwrap().contains("memfork_merge"));
    let kept = call(&agent, "memfork_get", json!({ "key": "shop:decision:db" })).await;
    assert_eq!(kept["value"], "mysql", "memory was forced");
    agent.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn detached_head_keeps_memory_where_it_was_and_says_so_once() {
    let _turn = ONE_AT_A_TIME.lock().await;
    if git_binary().is_none() {
        eprintln!("skipped: no git on this machine");
        return;
    }
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let agent = connect(&sandbox, "claude-code", &repo).await;
    let (_, _) = branch_of(&agent).await;
    git(&sandbox, &repo, &["checkout", "-q", "--detach"]);
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "main");
    assert_eq!(kinds(&notes), ["detached"]);
    let (_, notes) = branch_of(&agent).await;
    assert!(notes.is_empty(), "said twice: {notes:?}");
    git(&sandbox, &repo, &["switch", "-q", "-c", "feature/y"]);
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "feature/y");
    assert_eq!(kinds(&notes), ["follow"]);
    agent.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn two_sessions_in_two_worktrees_each_follow_their_own_branch() {
    let _turn = ONE_AT_A_TIME.lock().await;
    if git_binary().is_none() {
        eprintln!("skipped: no git on this machine");
        return;
    }
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    git(&sandbox, &repo, &["branch", "-q", "feature/x"]);
    let worktree = sandbox.root().join("shop-wt");
    git(
        &sandbox,
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            worktree.to_str().unwrap(),
            "feature/x",
        ],
    );
    switch_on(&worktree, None);
    let a = connect(&sandbox, "claude-code", &repo).await;
    let b = connect(&sandbox, "codex-mcp-client", &worktree).await;
    let (branch_a, _) = branch_of(&a).await;
    let (branch_b, notes_b) = branch_of(&b).await;
    assert_eq!(branch_a, "main");
    assert_eq!(branch_b, "feature/x");
    assert_eq!(kinds(&notes_b), ["follow"]);
    // A switch in one worktree moves only its session.
    git(&sandbox, &repo, &["switch", "-q", "-c", "feature/z"]);
    let (branch_a, _) = branch_of(&a).await;
    let (branch_b, notes_b) = branch_of(&b).await;
    assert_eq!(branch_a, "feature/z");
    assert_eq!(branch_b, "feature/x");
    assert!(notes_b.is_empty(), "{notes_b:?}");
    a.cancel().await.unwrap();
    b.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merge_made_between_sessions_is_applied_by_the_next_session() {
    let _turn = ONE_AT_A_TIME.lock().await;
    if git_binary().is_none() {
        eprintln!("skipped: no git on this machine");
        return;
    }
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let first = connect(&sandbox, "claude-code", &repo).await;
    let (_, _) = branch_of(&first).await;
    git(&sandbox, &repo, &["switch", "-q", "-c", "feature/x"]);
    let (branch, _) = branch_of(&first).await;
    assert_eq!(branch, "feature/x");
    call(
        &first,
        "memfork_put",
        json!({ "key": "shop:decision:cache", "value": "redis" }),
    )
    .await;
    commit(&sandbox, &repo, "cache.txt", "redis");
    first.cancel().await.unwrap();

    // Nobody connected: the person merges in a terminal.
    git(&sandbox, &repo, &["switch", "-q", "main"]);
    git(&sandbox, &repo, &["merge", "-q", "feature/x"]);

    let second = connect(&sandbox, "claude-code", &repo).await;
    let (branch, notes) = branch_of(&second).await;
    assert_eq!(branch, "main");
    assert_eq!(kinds(&notes), ["merge"], "{notes:?}");
    let found = call(
        &second,
        "memfork_get",
        json!({ "key": "shop:decision:cache" }),
    )
    .await;
    assert_eq!(found["value"], "redis");
    second.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn off_by_default_off_by_the_file_and_off_by_the_policy() {
    let _turn = ONE_AT_A_TIME.lock().await;
    if git_binary().is_none() {
        eprintln!("skipped: no git on this machine");
        return;
    }
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    std::fs::remove_file(repo.join(AUTOPILOT_FILE)).unwrap();
    let agent = connect(&sandbox, "claude-code", &repo).await;
    let (_, _) = branch_of(&agent).await;
    git(&sandbox, &repo, &["switch", "-q", "-c", "feature/x"]);
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "main", "followed with no file");
    assert!(notes.is_empty());

    // The file switched off with one command: still nothing.
    switch_on(&repo, None);
    let off = sandbox
        .command()
        .args(["autopilot", "off"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(off.status.success(), "{off:?}");
    assert!(String::from_utf8_lossy(&off.stdout).contains("autopilot is off"));
    git(&sandbox, &repo, &["switch", "-q", "-c", "feature/y"]);
    let (branch, _) = branch_of(&agent).await;
    assert_eq!(branch, "main", "followed while off");

    // On again: it follows from here.
    let on = sandbox
        .command()
        .args(["autopilot", "on"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(on.status.success(), "{on:?}");
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "feature/y");
    assert_eq!(kinds(&notes), ["follow"]);
    agent.cancel().await.unwrap();

    // The machine policy wins over the file.
    let policy = sandbox.root().join("policy.toml");
    std::fs::write(&policy, "autopilot = false\n").unwrap();
    let mut std_cmd = sandbox.command();
    std_cmd
        .current_dir(&repo)
        .env(memfork::policy::EXTRA_ENV, &policy);
    let transport =
        TokioChildProcess::new(tokio::process::Command::from(std_cmd).configure(|cmd| {
            cmd.arg("mcp");
            cmd.stderr(Stdio::null());
        }))
        .unwrap();
    let ruled = ClientConfig::new(
        Default::default(),
        Implementation::new("claude-code", "1.0"),
    )
    .serve(transport)
    .await
    .unwrap();
    let (branch, notes) = branch_of(&ruled).await;
    assert_eq!(branch, "main");
    assert!(notes.is_empty());
    git(&sandbox, &repo, &["switch", "-q", "main"]);
    git(&sandbox, &repo, &["switch", "-q", "feature/x"]);
    let (branch, _) = branch_of(&ruled).await;
    assert_eq!(branch, "main", "followed under a policy that forbids it");
    let refused = sandbox
        .command()
        .args(["autopilot", "on"])
        .current_dir(&repo)
        .env(memfork::policy::EXTRA_ENV, &policy)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    ruled.cancel().await.unwrap();
}

// ---- 4b: automatic forks through a client's hooks ---------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_hook_forks_before_a_risky_command_and_the_outcome_settles_it() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let repo = sandbox.root().join("shop");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    switch_on(&repo, None);
    let agent = connect(&sandbox, "claude-code", &repo).await;
    let (branch, _) = branch_of(&agent).await;
    assert_eq!(branch, "main");
    call(
        &agent,
        "memfork_put",
        json!({ "key": "shop:decision:db", "value": "postgres" }),
    )
    .await;

    // Not risky: nothing.
    silent(&hook(
        &sandbox,
        &repo,
        &pre_command(&repo, "cargo test", "t0"),
    ));
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "main");
    assert!(notes.is_empty());

    // Risky: forked before it runs, and the session is on the fork.
    let started = Instant::now();
    silent(&hook(
        &sandbox,
        &repo,
        &pre_command(&repo, "npx prisma migrate dev", "t1"),
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "autopilot/main/1");
    assert_eq!(kinds(&notes), ["fork"]);
    assert_eq!(notes[0]["rule"], "migration");
    call(
        &agent,
        "memfork_put",
        json!({ "key": "shop:decision:schema", "value": "v2" }),
    )
    .await;

    // Another risky action does not nest.
    silent(&hook(
        &sandbox,
        &repo,
        &pre_command(&repo, "rm -rf build", "t2"),
    ));
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "autopilot/main/1");
    assert_eq!(kinds(&notes), ["protected"]);

    // The action worked: merged into main, session back on main.
    silent(&hook(
        &sandbox,
        &repo,
        &post_command(&repo, "npx prisma migrate dev", "t1"),
    ));
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "main");
    assert_eq!(kinds(&notes), ["settled"]);
    assert_eq!(notes[0]["result"], "merged");
    let found = call(
        &agent,
        "memfork_get",
        json!({ "key": "shop:decision:schema" }),
    )
    .await;
    assert_eq!(found["value"], "v2");

    // The action failed: discarded, with a lesson from data alone.
    silent(&hook(
        &sandbox,
        &repo,
        &pre_command(&repo, "rm -rf build", "t3"),
    ));
    let (branch, _) = branch_of(&agent).await;
    assert_eq!(branch, "autopilot/main/1");
    call(
        &agent,
        "memfork_put",
        json!({ "key": "shop:decision:doomed", "value": "x" }),
    )
    .await;
    silent(&hook(
        &sandbox,
        &repo,
        &failed_command(
            &repo,
            "rm -rf build",
            "t3",
            "Exit code 1\nrm: cannot remove 'build': Permission denied",
        ),
    ));
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "main");
    assert_eq!(notes[0]["result"], "discarded");
    assert_eq!(
        notes[0]["lesson"],
        "autopilot: `rm -rf build` (rule: recursive-delete) failed, exit 1: rm: cannot remove 'build': Permission denied"
    );
    let gone = call(
        &agent,
        "memfork_get",
        json!({ "key": "shop:decision:doomed" }),
    )
    .await;
    assert_eq!(gone["found"], false);
    let brief = call(&agent, "memfork_resume", json!({})).await;
    let lessons = brief["lessons"].as_array().unwrap();
    assert_eq!(lessons.len(), 1, "{brief}");
    assert_eq!(lessons[0]["by"], "memfork-autopilot");
    agent.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configured_check_decides_not_the_action() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let repo = sandbox.root().join("shop");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    // `exit 3` fails in cmd.exe and in sh alike.
    switch_on(&repo, Some("exit 3"));
    let agent = connect(&sandbox, "claude-code", &repo).await;
    let (_, _) = branch_of(&agent).await;
    silent(&hook(
        &sandbox,
        &repo,
        &pre_command(&repo, "npm install left-pad", "t1"),
    ));
    let (branch, _) = branch_of(&agent).await;
    assert_eq!(branch, "autopilot/main/1");
    // The command itself passed; the check failed: discarded.
    silent(&hook(
        &sandbox,
        &repo,
        &post_command(&repo, "npm install left-pad", "t1"),
    ));
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "main");
    assert_eq!(notes[0]["result"], "discarded");
    assert_eq!(
        notes[0]["lesson"],
        "autopilot: `npm install left-pad` (rule: dependency-change) failed exit 3, exit 3"
    );

    // The other way round: the command failed, the check passed: merged.
    switch_on(&repo, Some("exit 0"));
    silent(&hook(
        &sandbox,
        &repo,
        &pre_command(&repo, "npm install left-pad", "t2"),
    ));
    let (branch, _) = branch_of(&agent).await;
    assert_eq!(branch, "autopilot/main/1");
    silent(&hook(
        &sandbox,
        &repo,
        &failed_command(&repo, "npm install left-pad", "t2", "Exit code 1\nboom"),
    ));
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "main");
    assert_eq!(notes[0]["result"], "merged");
    agent.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_sweep_forks_past_the_limit_and_stop_keeps_it_without_a_check() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let repo = sandbox.root().join("shop");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    switch_on(&repo, None);
    let agent = connect(&sandbox, "claude-code", &repo).await;
    let (_, _) = branch_of(&agent).await;
    for n in 1..=5 {
        silent(&hook(
            &sandbox,
            &repo,
            &pre_edit(&repo, &format!("src/f{n}.rs")),
        ));
    }
    // The same file again is not a sixth.
    silent(&hook(&sandbox, &repo, &pre_edit(&repo, "src/f1.rs")));
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "main");
    assert!(notes.is_empty());
    silent(&hook(&sandbox, &repo, &pre_edit(&repo, "src/f6.rs")));
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "autopilot/main/1");
    assert_eq!(kinds(&notes), ["fork"]);
    assert_eq!(notes[0]["rule"], "edits");
    // A command's outcome does not settle an edit fork; the stop does, and
    // with no check it is kept and said.
    silent(&hook(
        &sandbox,
        &repo,
        &post_command(&repo, "cargo build", "t9"),
    ));
    let (branch, _) = branch_of(&agent).await;
    assert_eq!(branch, "autopilot/main/1");
    silent(&hook(&sandbox, &repo, &stop(&repo)));
    let (branch, notes) = branch_of(&agent).await;
    assert_eq!(branch, "autopilot/main/1");
    assert_eq!(notes[0]["result"], "kept");
    assert!(notes[0]["note"]
        .as_str()
        .unwrap()
        .contains("no check is configured"));
    let status = status_json(&sandbox, &repo);
    assert_eq!(status["daemon"]["kept_forks"], json!(["autopilot/main/1"]));
    assert_eq!(status["daemon"]["open"], json!([]));
    agent.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn two_sessions_of_one_client_are_both_forked_and_both_told() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let repo = sandbox.root().join("shop");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    switch_on(&repo, None);
    let a = connect(&sandbox, "claude-code", &repo).await;
    let b = connect(&sandbox, "claude-code", &repo).await;
    let other = connect(&sandbox, "codex-mcp-client", &repo).await;
    for s in [&a, &b, &other] {
        let (_, _) = branch_of(s).await;
    }
    silent(&hook(
        &sandbox,
        &repo,
        &pre_command(&repo, "git push --force", "t1"),
    ));
    let (branch_a, notes_a) = branch_of(&a).await;
    let (branch_b, notes_b) = branch_of(&b).await;
    let (branch_other, notes_other) = branch_of(&other).await;
    assert!(branch_a.starts_with("autopilot/main/"));
    assert!(branch_b.starts_with("autopilot/main/"));
    assert_ne!(branch_a, branch_b);
    assert_eq!(branch_other, "main");
    assert!(notes_other.is_empty());
    for notes in [&notes_a, &notes_b] {
        assert_eq!(kinds(notes), ["fork", "shared"]);
        assert!(notes[1]["note"]
            .as_str()
            .unwrap()
            .contains("2 sessions of this client in this project share this fork"));
    }
    silent(&hook(
        &sandbox,
        &repo,
        &post_command(&repo, "git push --force", "t1"),
    ));
    let (branch_a, _) = branch_of(&a).await;
    let (branch_b, _) = branch_of(&b).await;
    assert_eq!((branch_a.as_str(), branch_b.as_str()), ("main", "main"));
    for s in [a, b, other] {
        s.cancel().await.unwrap();
    }
}

#[test]
fn the_hook_fails_open_with_no_daemon_and_starts_none() {
    let sandbox = Sandbox::new();
    let repo = sandbox.root().join("shop");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    switch_on(&repo, None);
    let before = sandbox.files();
    let started = Instant::now();
    for event in [
        pre_command(&repo, "npx prisma migrate dev", "t1"),
        post_command(&repo, "npx prisma migrate dev", "t1"),
        failed_command(&repo, "rm -rf x", "t2", "Exit code 1"),
        pre_edit(&repo, "a.rs"),
        stop(&repo),
        json!({ "hook_event_name": "SessionStart", "cwd": repo.display().to_string() }),
        json!("not an object"),
    ] {
        silent(&hook(&sandbox, &repo, &event));
    }
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(sandbox.owner().is_none(), "a hook started a daemon");
    assert_eq!(sandbox.files(), before, "a hook wrote a file");
    // Garbage on stdin, and nothing on stdin.
    let mut child = sandbox
        .command()
        .args(["autopilot", "hook"])
        .current_dir(&repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdin.take());
    silent(&child.wait_with_output().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hook_fails_open_after_the_daemon_stops_mid_session() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let repo = sandbox.root().join("shop");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    switch_on(&repo, None);
    let agent = connect(&sandbox, "claude-code", &repo).await;
    let (_, _) = branch_of(&agent).await;
    silent(&hook(
        &sandbox,
        &repo,
        &pre_command(&repo, "rm -rf x", "t1"),
    ));
    let (branch, _) = branch_of(&agent).await;
    assert_eq!(branch, "autopilot/main/1");
    let stopped = sandbox.command().arg("stop").output().unwrap();
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(sandbox.wait_for_no_daemon(Duration::from_secs(10)));
    let started = Instant::now();
    silent(&hook(
        &sandbox,
        &repo,
        &post_command(&repo, "rm -rf x", "t1"),
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(sandbox.owner().is_none(), "a hook started a daemon");
    agent.cancel().await.unwrap();
}

// ---- install and removal ---------------------------------------------------------------

#[test]
fn init_project_autopilot_writes_the_file_and_the_hooks_and_removal_gives_back_every_byte() {
    let sandbox = Sandbox::new();
    let repo = sandbox.root().join("shop");
    std::fs::create_dir_all(repo.join(".git").join("hooks")).unwrap();
    std::fs::create_dir_all(repo.join(".husky")).unwrap();
    std::fs::write(
        repo.join(".git").join("hooks").join("pre-commit"),
        "#!/bin/sh\nlint\n",
    )
    .unwrap();
    std::fs::write(repo.join(".husky").join("pre-commit"), "npm test\n").unwrap();
    std::fs::create_dir_all(repo.join(".claude")).unwrap();
    let seed = "{\n  // personal settings\n  \"permissions\": {\"allow\": [\"Bash(npm test)\"],},\n  \"hooks\": {\n    \"PreToolUse\": [\n      {\"matcher\": \"Bash\", \"hooks\": [{\"type\": \"command\", \"command\": \"./lint.sh\"}]}\n    ]\n  }\n}\n";
    let settings = repo.join(".claude").join("settings.local.json");
    std::fs::write(&settings, seed).unwrap();
    std::fs::create_dir_all(sandbox.home().join(".claude")).unwrap();
    let snapshot = |root: &Path| -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        for entry in walkdir(root) {
            if entry.starts_with(root.join(".git")) || entry.starts_with(root.join(".husky")) {
                out.push((entry.clone(), std::fs::read(&entry).unwrap()));
            }
        }
        out
    };
    let git_before = snapshot(&repo);

    // Dry run: the plan, the diff, and nothing written.
    let dry = sandbox
        .command()
        .args(["init", "--project", "--autopilot", "--dry-run"])
        .current_dir(&repo)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&dry.stdout);
    assert!(
        dry.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    assert!(text.contains(AUTOPILOT_FILE), "{text}");
    assert!(text.contains(".claude/settings.local.json"), "{text}");
    assert!(
        text.contains("+  \"hooks\"") || text.contains("+    \"PostToolUse\""),
        "{text}"
    );
    assert!(!repo.join(AUTOPILOT_FILE).exists());
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), seed);

    // For real.
    let install = sandbox
        .command()
        .args(["init", "--project", "--autopilot"])
        .current_dir(&repo)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&install.stdout);
    assert!(
        install.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&install.stderr)
    );
    assert!(repo.join(AUTOPILOT_FILE).exists());
    assert!(matches!(
        memfork::autopilot::config::read(&repo),
        memfork::autopilot::config::Read::Config(_)
    ));
    assert_eq!(
        memfork::clients::hooks::installed(&settings),
        memfork::clients::hooks::Installed::All
    );
    let after = std::fs::read_to_string(&settings).unwrap();
    assert!(after.contains("// personal settings"));
    assert!(after.contains("\"./lint.sh\""));
    let doc: Json = serde_json::from_str(
        &after
            .replace("// personal settings\n", "")
            .replace("],},", "]},"),
    )
    .unwrap_or(Json::Null);
    if doc.is_object() {
        assert_eq!(doc["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
    }
    // The block is written too, into the file Claude Code reads.
    assert!(text.contains("read by Claude Code"), "{text}");
    // Again: up to date.
    let again = sandbox
        .command()
        .args(["init", "--project", "--autopilot"])
        .current_dir(&repo)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&again.stdout);
    assert!(text.contains("up to date"), "{text}");
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), after);
    // A check command written by hand survives a re-run.
    switch_on(&repo, Some("cargo test"));
    let kept = sandbox
        .command()
        .args(["init", "--project", "--autopilot"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(kept.status.success());
    assert_eq!(
        memfork::autopilot::config::read(&repo)
            .config()
            .unwrap()
            .check
            .as_deref(),
        Some("cargo test")
    );
    // Status says so, with no daemon.
    let status = status_json(&sandbox, &repo);
    assert_eq!(status["file"]["check"], "cargo test");
    assert_eq!(status["clients"][0]["hooks"], "installed");
    assert_eq!(status["forks_before_risky"], true);
    assert!(status["daemon"].is_null());

    // Removal: the file gone, the hooks file back to its bytes, git's own
    // hooks and the hook manager's directory untouched throughout.
    let remove = sandbox
        .command()
        .args(["init", "--project", "--autopilot", "--remove"])
        .current_dir(&repo)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&remove.stdout);
    assert!(
        remove.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&remove.stderr)
    );
    assert!(!repo.join(AUTOPILOT_FILE).exists());
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), seed);
    assert_eq!(snapshot(&repo), git_before);
    assert!(sandbox.owner().is_none(), "init started a daemon");
}

#[test]
fn the_rules_are_listed_and_a_command_is_judged() {
    let sandbox = Sandbox::new();
    let rules = sandbox
        .command()
        .args(["autopilot", "rules"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&rules.stdout);
    assert!(rules.status.success());
    for family in [
        "migrations",
        "destructive file operations",
        "history rewriting",
        "dependency changes",
        "database commands",
    ] {
        assert!(text.contains(family), "{text}");
    }
    let risky = sandbox
        .command()
        .args(["autopilot", "check", "git push --force origin main"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&risky.stdout).starts_with("risky: matches `history-rewrite`"));
    let safe = sandbox
        .command()
        .args(["autopilot", "check", "--json", "cargo test"])
        .output()
        .unwrap();
    let json: Json = serde_json::from_slice(&safe.stdout).unwrap();
    assert_eq!(json["risky"], false);
    // Outside a repository, the commands that need one say so.
    let status = sandbox
        .command()
        .args(["autopilot", "status"])
        .output()
        .unwrap();
    assert!(!status.status.success());
    assert!(String::from_utf8_lossy(&status.stderr).contains("not inside a repository"));
}

fn walkdir(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}
