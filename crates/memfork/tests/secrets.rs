//! Credentials stay out of shared memory: every write path refuses text shaped
//! like a secret, says which rule and where, never repeats the text, stores
//! nothing, and takes an override that names the rule.
//!
//! Every fake credential here is assembled at run time, so the repository
//! never holds anything shaped like one.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::process::Stdio;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, ClientConfig, Implementation};
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::RoleClient;
use serde_json::{json, Value as Json};
use support::Sandbox;

type Client = RunningService<RoleClient, ClientConfig>;

static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn fake_token() -> String {
    format!("ghp_{}", "aB3dE6gH9j".repeat(4))
}

async fn connect(sandbox: &Sandbox) -> Client {
    let repo = sandbox.root().join("shop");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let mut std_cmd = sandbox.command();
    std_cmd.current_dir(&repo);
    let transport =
        TokioChildProcess::new(tokio::process::Command::from(std_cmd).configure(|cmd| {
            cmd.arg("mcp");
            cmd.stderr(Stdio::null());
        }))
        .expect("spawned `memfork mcp`");
    ClientConfig::new(
        Default::default(),
        Implementation::new("claude-code", "1.0"),
    )
    .serve(transport)
    .await
    .expect("the MCP handshake completed")
}

/// The raw result: a refusal is a tool-level error, not a protocol one.
async fn call(client: &Client, tool: &str, args: Json) -> (bool, Json) {
    let Json::Object(map) = args else { panic!() };
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(map))
        .await
        .unwrap_or_else(|e| panic!("`{tool}` failed at the protocol level: {e}"));
    (
        result.is_error == Some(true),
        result.structured_content.unwrap_or(Json::Null),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn every_write_tool_refuses_a_credential_and_stores_nothing() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let a = connect(&sandbox).await;
    let token = fake_token();
    let pasted = format!("deploy with {token}");

    let attempts = [
        (
            "memfork_put",
            json!({"key": "shop:note:deploy", "value": pasted}),
            "value",
        ),
        (
            "memfork_put",
            json!({"key": "shop:note:m", "value": "fine", "meta": {"how": pasted}}),
            "meta.how",
        ),
        (
            "memfork_handoff",
            json!({"summary": "deploy works", "next": ["ship", pasted]}),
            "next[1]",
        ),
        (
            "memfork_task",
            json!({"action": "add", "title": "deploy", "detail": pasted}),
            "detail",
        ),
    ];
    for (tool, args, field) in attempts {
        let (is_error, body) = call(&a, tool, args).await;
        assert!(is_error, "{tool} stored it: {body}");
        let text = body.to_string();
        assert!(
            !text.contains(&token) && !text.contains("aB3dE6"),
            "{tool} repeated it: {text}"
        );
        assert_eq!(body["secret"]["rule"], "github-token", "{body}");
        assert_eq!(body["secret"]["field"], field, "{body}");
        assert!(text.contains("allow_secret"), "{text}");
    }

    call(&a, "memfork_fork", json!({"name": "try"})).await;
    let (is_error, body) = call(
        &a,
        "memfork_discard",
        json!({"name": "try", "lesson": pasted}),
    )
    .await;
    assert!(is_error, "{body}");
    assert_eq!(body["secret"]["field"], "lesson");
    let (_, branches) = call(&a, "memfork_branches", json!({})).await;
    assert!(
        branches.to_string().contains("try"),
        "a refused discard discarded anyway"
    );

    // Nothing of any of it was stored.
    let (_, listed) = call(&a, "memfork_list", json!({"prefix": ""})).await;
    assert!(!listed.to_string().contains("aB3dE6"), "{listed}");
    assert_eq!(listed["count"], 0, "{listed}");

    // Naming the rule writes it; naming another does not; a typo is refused.
    let (is_error, _) = call(
        &a,
        "memfork_put",
        json!({"key": "shop:note:deploy", "value": pasted, "allow_secret": "jwt"}),
    )
    .await;
    assert!(is_error);
    let (is_error, body) = call(
        &a,
        "memfork_put",
        json!({"key": "shop:note:deploy", "value": pasted, "allow_secret": "github-tokn"}),
    )
    .await;
    assert!(is_error);
    assert!(body.to_string().contains("not a secret rule"), "{body}");
    let (is_error, body) = call(
        &a,
        "memfork_put",
        json!({"key": "shop:note:deploy", "value": pasted, "allow_secret": "github-token"}),
    )
    .await;
    assert!(!is_error, "{body}");
    a.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_feed_reports_a_refusal_without_the_key_or_the_text() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let a = connect(&sandbox).await;
    call(&a, "memfork_branches", json!({})).await;
    let mut watcher = sandbox
        .command()
        .args(["watch", "--json", "--count", "1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = watcher.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            let _ = tx.send(line);
        }
    });
    rx.recv_timeout(Duration::from_secs(30)).unwrap(); // hello
    let token = fake_token();
    // The secret is in the key this time.
    call(
        &a,
        "memfork_put",
        json!({"key": format!("shop:{token}"), "value": "v"}),
    )
    .await;
    let event = rx.recv_timeout(Duration::from_secs(30)).unwrap();
    let _ = watcher.wait();
    assert!(!event.contains("aB3dE6"), "{event}");
    let event: Json = serde_json::from_str(&event).unwrap();
    assert_eq!(event["ok"], false);
    assert_eq!(event["detail"], "secret refused: github-token");
    assert!(event.get("key").is_none_or(Json::is_null), "{event}");
    a.cancel().await.unwrap();
}

#[test]
fn the_command_line_refuses_and_accepts_the_override() {
    let _turn = ONE_AT_A_TIME.blocking_lock();
    let sandbox = Sandbox::new();
    let token = fake_token();
    let out = sandbox
        .command()
        .args(["put", "note", &format!("key is {token}")])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("github-token") && stderr.contains("--allow-secret"),
        "{stderr}"
    );
    assert!(!stderr.contains("aB3dE6"), "{stderr}");
    let got = sandbox
        .command()
        .args(["--json", "get", "note"])
        .output()
        .unwrap();
    let got: Json = serde_json::from_slice(&got.stdout).unwrap();
    assert_eq!(got["found"], false, "the refused value was stored");

    for args in [
        vec!["task", "add", "rotate", "--detail", &token],
        vec!["put", "k", "v", "--meta", &format!("m={token}")],
    ] {
        let out = sandbox.command().args(&args).output().unwrap();
        assert!(!out.status.success(), "{args:?} was accepted");
    }

    let out = sandbox
        .command()
        .args([
            "put",
            "note",
            &format!("key is {token}"),
            "--allow-secret",
            "github-token",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
