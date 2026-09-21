//! Seeing what MemFork is doing: `memfork watch`, the command line on the
//! shared store, history as a tree, and the rules for colour and motion.
//!
//! Everything runs in a sandbox: its own data directory, home and PATH, with
//! the guard against the real data directory set, and every daemon a test
//! starts is stopped when the sandbox goes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::io::{BufRead, BufReader, Write as _};
use std::process::{Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, ClientConfig, Implementation};
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::RoleClient;
use serde_json::{json, Value as Json};
use support::Sandbox;

fn run(sandbox: &Sandbox, args: &[&str]) -> Output {
    sandbox.command().args(args).output().unwrap()
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn has_escape(bytes: &[u8]) -> bool {
    bytes.contains(&0x1b)
}

// ---- the command line works on the shared store -----------------------------

#[test]
fn separate_commands_share_the_store_through_the_daemon() {
    let sandbox = Sandbox::new();
    ok(&run(&sandbox, &["put", "plan:1", "ship on Friday"]));
    assert!(
        sandbox.owner().is_some(),
        "the first command started a daemon"
    );
    assert_eq!(
        ok(&run(&sandbox, &["get", "plan:1"])).trim(),
        "ship on Friday"
    );

    // Recorded as coming from the command line.
    let doc: Json =
        serde_json::from_str(&ok(&run(&sandbox, &["--json", "get", "plan:1"]))).unwrap();
    assert_eq!(doc["meta"]["memfork.by"], "memfork-cli");

    // A refusal from the store comes back as an error, not a success.
    let out = run(&sandbox, &["get", "plan:1", "--branch", "nope"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no such branch"));
}

#[test]
fn ephemeral_runs_one_command_in_memory_and_starts_nothing() {
    let sandbox = Sandbox::new();
    let before = sandbox.files();
    let out = run(&sandbox, &["--ephemeral", "put", "k", "v"]);
    assert!(ok(&out).starts_with("put k on main"));
    assert!(sandbox.owner().is_none(), "--ephemeral started a daemon");
    assert_eq!(sandbox.files(), before, "--ephemeral wrote something");
    // And it really is alone: the next command sees nothing.
    let out = run(&sandbox, &["--ephemeral", "get", "k"]);
    assert_eq!(ok(&out), "");
}

#[test]
fn call_uses_the_shared_store_too() {
    let sandbox = Sandbox::new();
    ok(&run(
        &sandbox,
        &["call", "memfork_put", r#"{"key":"a","value":"1"}"#],
    ));
    let got: Json = serde_json::from_str(&ok(&run(
        &sandbox,
        &["call", "memfork_get", r#"{"key":"a"}"#],
    )))
    .unwrap();
    assert_eq!(got["value"], "1");
    assert_eq!(got["written_by"], "memfork-cli");
}

// ---- history as a tree -------------------------------------------------------

#[test]
fn log_graph_draws_forks_merges_and_discards_from_the_daemon() {
    let sandbox = Sandbox::new();
    for args in [
        vec!["put", "a", "1"],
        vec!["fork", "feature"],
        vec!["put", "f", "2", "--branch", "feature"],
        vec!["put", "m", "3"],
        vec!["merge", "feature"],
        vec!["fork", "attempt"],
        vec!["put", "x", "4", "--branch", "attempt"],
        vec!["discard", "attempt"],
    ] {
        ok(&run(&sandbox, &args));
    }
    let text = ok(&run(&sandbox, &["log", "--graph"]));
    assert!(text.contains("[main]"), "{text}");
    assert!(text.contains("[feature]"), "{text}");
    assert!(text.contains("merge"), "{text}");
    assert!(text.contains("fork point"), "{text}");
    assert!(text.contains("discarded attempt (1 commit)"), "{text}");
    assert!(text.contains("by memfork-cli"), "{text}");
    assert!(
        text.is_ascii() && !text.contains('\x1b'),
        "not a terminal: plain\n{text}"
    );

    let branches = ok(&run(&sandbox, &["branches"]));
    assert!(branches.contains("[default]"), "{branches}");
    assert!(
        branches.contains("feature") && branches.contains("behind main"),
        "{branches}"
    );

    let doc: Json =
        serde_json::from_str(&ok(&run(&sandbox, &["--json", "log", "--graph"]))).unwrap();
    assert_eq!(doc["graph"]["discarded"][0]["name"], "attempt");
}

// ---- colour and motion -------------------------------------------------------

#[test]
fn colour_follows_the_rules() {
    let sandbox = Sandbox::new();
    let branches = |extra: &[&str], env: &[(&str, &str)]| {
        let mut cmd = sandbox.command();
        cmd.arg("--ephemeral").args(extra).arg("branches");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.env_remove("CI").env_remove("NO_COLOR");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success());
        out
    };

    // Not a terminal: no colour unless asked.
    assert!(
        !has_escape(&branches(&[], &[]).stdout),
        "auto coloured a pipe"
    );
    assert!(!has_escape(&branches(&["--color", "never"], &[]).stdout));
    // Asked: colour, over a pipe, over CI and over NO_COLOR.
    assert!(has_escape(&branches(&["--color", "always"], &[]).stdout));
    assert!(has_escape(
        &branches(&["--color", "always"], &[("CI", "true")]).stdout
    ));
    assert!(has_escape(
        &branches(&["--color", "always"], &[("NO_COLOR", "1")]).stdout
    ));
    assert!(!has_escape(&branches(&[], &[("NO_COLOR", "1")]).stdout));
    // Never over --json, whatever was asked.
    let json = branches(&["--color", "always", "--json"], &[]);
    assert!(!has_escape(&json.stdout), "--json was coloured");
    serde_json::from_slice::<Json>(&json.stdout).expect("still JSON");
}

#[test]
fn mcp_is_never_coloured_even_when_asked() {
    let sandbox = Sandbox::new();
    let mut child = sandbox
        .command()
        .args(["--color", "always", "mcp", "--ephemeral"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"t","version":"1"}}}}}}"#).unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
        )
        .unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"memfork_branches","arguments":{{}}}}}}"#).unwrap();
    }
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("\"id\":2"),
        "no answer"
    );
    assert!(
        !has_escape(&out.stdout),
        "the protocol stream carried a colour code"
    );
    assert!(!has_escape(&out.stderr), "mcp coloured its diagnostics");
}

#[test]
fn nothing_animates_when_stderr_is_not_a_terminal() {
    let sandbox = Sandbox::new();
    // Starting the daemon is a real wait, so it is the moment a spinner
    // would appear. Here it must be one plain line.
    let out = sandbox
        .command()
        .args(["--color", "always", "put", "k", "v"])
        .output()
        .unwrap();
    ok(&out);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains('\r'), "a spinner was drawn: {err:?}");
    assert!(err.contains("starting the MemFork daemon"), "{err:?}");
    // And a command with nothing to wait for says nothing at all.
    let out = sandbox.command().args(["get", "k"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
}

// ---- memfork watch -----------------------------------------------------------

type Client = RunningService<RoleClient, ClientConfig>;

async fn client(sandbox: &Sandbox, name: &str) -> Client {
    let std_cmd = sandbox.command();
    let transport =
        TokioChildProcess::new(tokio::process::Command::from(std_cmd).configure(|cmd| {
            cmd.args(["mcp", "--namespace", "shop"]);
            cmd.stderr(Stdio::null());
        }))
        .unwrap();
    ClientConfig::new(Default::default(), Implementation::new(name, "1"))
        .serve(transport)
        .await
        .unwrap()
}

async fn call(c: &Client, tool: &str, args: Json) {
    let Json::Object(map) = args else { panic!() };
    c.call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(map))
        .await
        .unwrap();
}

/// Start `memfork watch` with `args`, and a thread handing its lines over.
fn watch(sandbox: &Sandbox, args: &[&str]) -> (std::process::Child, mpsc::Receiver<String>) {
    let mut child = sandbox
        .command()
        .arg("watch")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (child, rx)
}

fn next(rx: &mpsc::Receiver<String>) -> String {
    rx.recv_timeout(Duration::from_secs(30))
        .expect("watch printed nothing within 30 s")
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_shows_who_did_what_including_handoffs() {
    let sandbox = Sandbox::new();
    // A daemon first, so nothing happens before the watcher is listening.
    ok(&run(&sandbox, &["put", "warm", "up"]));
    let (mut watcher, lines) = watch(&sandbox, &["--json", "--count", "6"]);
    let hello: Json = serde_json::from_str(&next(&lines)).unwrap();
    assert_eq!(hello["kind"], "hello");
    assert_eq!(hello["version"], env!("CARGO_PKG_VERSION"));

    let first = client(&sandbox, "claude-code").await;
    call(&first, "memfork_handoff", json!({ "summary": "half done" })).await;
    let second = client(&sandbox, "codex-mcp-client").await;
    call(&second, "memfork_resume", json!({})).await;
    ok(&run(&sandbox, &["fork", "try"]));
    first.cancel().await.unwrap();

    let mut events: Vec<Json> = (0..6)
        .map(|_| serde_json::from_str(&next(&lines)).unwrap())
        .collect();
    let summary: Vec<String> = events
        .iter_mut()
        .map(|e| {
            format!(
                "{} {} {}",
                e["kind"].as_str().unwrap(),
                e["client"].as_str().unwrap(),
                e["operation"].as_str().unwrap_or("-")
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            "connected Claude Code -",
            "operation Claude Code handoff",
            "connected Codex CLI -",
            "operation Codex CLI resume",
            "operation memfork-cli fork",
            "disconnected Claude Code -",
        ],
        "{events:#?}"
    );
    assert_eq!(events[1]["key"], "shop:handoff:00000001");
    assert_eq!(events[1]["namespace"], "shop");
    assert_eq!(events[4]["branch"], "try");
    assert!(events[0]["time"].as_str().unwrap().ends_with('Z'));

    // --count ends it.
    let status = watcher.wait().unwrap();
    assert!(status.success());
    second.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_in_words_says_connected_never_live() {
    let sandbox = Sandbox::new();
    ok(&run(&sandbox, &["put", "warm", "up"]));
    let listening = client(&sandbox, "claude-code").await;
    call(&listening, "memfork_branches", json!({})).await;

    let (mut watcher, lines) = watch(&sandbox, &["--count", "1"]);
    let header = next(&lines);
    let clients = next(&lines);
    assert!(header.contains("daemon connected"), "{header}");
    assert!(
        clients.contains("clients connected: Claude Code (shop)"),
        "{clients}"
    );
    ok(&run(&sandbox, &["put", "k", "v"]));
    let line = next(&lines);
    assert!(
        line.contains("memfork-cli") && line.contains("put") && line.contains("k"),
        "{line}"
    );
    for l in [&header, &clients, &line] {
        assert!(
            !l.to_ascii_lowercase()
                .split(|c: char| !c.is_ascii_alphabetic())
                .any(|w| w == "live"),
            "{l}"
        );
        assert!(!l.contains('\x1b'), "not a terminal: plain, {l:?}");
    }
    assert!(watcher.wait().unwrap().success());
    listening.cancel().await.unwrap();
}

#[test]
fn watch_waits_for_a_daemon_rather_than_starting_one() {
    let sandbox = Sandbox::new();
    let mut watcher = sandbox
        .command()
        .args(["watch", "--count", "1"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(800));
    assert!(sandbox.owner().is_none(), "watch started a daemon");
    let _ = watcher.kill();
    let out = watcher.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("waiting for one to start"), "{err}");
}

// ---- a forgotten session is replaced, not reported ---------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_client_idle_past_the_session_timeout_keeps_working() {
    let mut sandbox = Sandbox::new();
    sandbox.spawn(&[
        "serve",
        "--port",
        "0",
        "--idle-timeout",
        "60",
        "--session-timeout",
        "1",
    ]);
    sandbox
        .wait_for_daemon(Duration::from_secs(90))
        .expect("the daemon did not start");
    let c = client(&sandbox, "quiet-client").await;
    call(&c, "memfork_put", json!({ "key": "k", "value": "1" })).await;
    // Longer than the daemon keeps a quiet session.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let result = c
        .call_tool(
            CallToolRequestParams::new("memfork_get".to_owned())
                .with_arguments(json!({ "key": "k" }).as_object().unwrap().clone()),
        )
        .await
        .expect("the call after the session timed out failed");
    assert_eq!(result.structured_content.unwrap()["value"], "1");
    c.cancel().await.unwrap();
}

// ---- words -------------------------------------------------------------------

#[test]
fn the_status_word_is_connected_everywhere() {
    // Output and documentation say a daemon or client is "connected". The
    // other word is not used at all, so it cannot creep back in as a status.
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let mut files = vec![
        root.join("README.md"),
        root.join("CHANGELOG.md"),
        root.join("docs").join("DESIGN.md"),
        root.join("crates").join("memfork-py").join("README.md"),
    ];
    let mut stack = vec![
        root.join("crates").join("memfork").join("src"),
        root.join("crates").join("memfork-core").join("src"),
    ];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    let banned = ["l", "ive"].concat();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        for (n, line) in text.lines().enumerate() {
            let words = line
                .split(|c: char| !c.is_ascii_alphabetic())
                .map(str::to_ascii_lowercase);
            for word in words {
                assert_ne!(word, banned, "{}:{}: {line}", file.display(), n + 1);
            }
        }
    }
}
