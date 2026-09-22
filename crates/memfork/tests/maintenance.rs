//! Self-maintaining memory: MemFork asks for tidying as a task, an agent does
//! it on a fork, and MemFork merges it only if its rules say it is safe.
//!
//! Sandboxed like every suite: temporary data directory, home and PATH, the
//! guard, and a repository made for the test.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use rmcp::model::{CallToolRequestParams, ClientConfig, Implementation};
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::RoleClient;
use serde_json::{json, Value as Json};
use support::Sandbox;

type Client = RunningService<RoleClient, ClientConfig>;

fn repository(sandbox: &Sandbox) -> PathBuf {
    let repo = sandbox.root().join("shop");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    repo
}

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
    let Json::Object(map) = args else { panic!() };
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

fn handoff_key(n: u32) -> String {
    format!("shop:handoff:{n:08}")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tidy_on_a_fork_is_merged_only_when_it_is_safe() {
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let a = connect(&sandbox, "claude-code", &repo).await;
    for n in 1..=22 {
        call(
            &a,
            "memfork_handoff",
            json!({"summary": format!("session {n}")}),
        )
        .await;
    }
    // Twenty-one superseded handoffs: one task, once.
    let listed = call(&a, "memfork_task", json!({"action": "list"})).await;
    let tasks = listed["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1, "{listed}");
    let id = tasks[0]["id"].as_str().unwrap().to_owned();
    assert_eq!(id, "maintain-handoffs-1");
    assert_eq!(tasks[0]["by"], "memfork");
    call(&a, "memfork_resume", json!({})).await;
    let again = call(&a, "memfork_task", json!({"action": "list"})).await;
    assert_eq!(again["count"], 1, "the trigger fired twice: {again}");

    // B takes it, and first gets it wrong: the latest handoff is pinned.
    let b = connect(&sandbox, "codex-mcp-client", &repo).await;
    call(&b, "memfork_task", json!({"action": "claim", "id": id})).await;
    call(&b, "memfork_fork", json!({"name": "bad-tidy"})).await;
    call(&b, "memfork_delete", json!({"key": handoff_key(22)})).await;
    let refused = call(
        &b,
        "memfork_task",
        json!({"action": "done", "id": id, "fork": "bad-tidy"}),
    )
    .await;
    assert_eq!(refused["accepted"], false, "{refused}");
    assert!(
        refused["reason"].as_str().unwrap().contains("pinned"),
        "{refused}"
    );
    assert_eq!(refused["status"], "open");
    assert_eq!(refused["lesson"]["branch"], "main");
    assert_eq!(
        refused["current_branch"], "main",
        "the session was left on a discarded fork"
    );
    let branches = call(&b, "memfork_branches", json!({})).await;
    assert!(!branches.to_string().contains("bad-tidy"), "{branches}");
    assert_eq!(
        call(&b, "memfork_get", json!({"key": handoff_key(22)})).await["found"],
        true
    );

    // Then right: summarise, delete what is superseded, and hand back the fork.
    call(&b, "memfork_task", json!({"action": "claim", "id": id})).await;
    call(&b, "memfork_fork", json!({"name": "tidy"})).await;
    let replaced: Vec<String> = (1..=21).map(handoff_key).collect();
    call(
        &b,
        "memfork_put",
        json!({
            "key": "shop:note:handoff-history",
            "value": "sessions 1 to 21: built checkout, then refunds",
            "meta": {"replaces": serde_json::to_string(&replaced).unwrap()},
        }),
    )
    .await;
    for key in &replaced {
        call(&b, "memfork_delete", json!({"key": key})).await;
    }
    let merged = call(
        &b,
        "memfork_task",
        json!({"action": "done", "id": id, "fork": "tidy"}),
    )
    .await;
    assert_eq!(merged["accepted"], true, "{merged}");
    assert_eq!(merged["merged"], "tidy");
    assert_eq!(merged["status"], "done");
    assert_eq!(merged["current_branch"], "main");

    let brief = call(&a, "memfork_resume", json!({})).await;
    assert_eq!(brief["latest_handoff"]["key"], handoff_key(22), "{brief}");
    assert_eq!(brief["earlier_handoffs"], 0, "{brief}");
    assert_eq!(
        call(&a, "memfork_get", json!({"key": handoff_key(1)})).await["found"],
        false
    );
    // The lesson from the first attempt is there for the next agent.
    assert!(
        brief["lessons"][0]["lesson"]
            .as_str()
            .unwrap()
            .contains("pinned"),
        "{brief}"
    );
    a.cancel().await.unwrap();
    b.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn maintenance_can_be_switched_off_for_a_project() {
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let off = sandbox
        .command()
        .current_dir(&repo)
        .args(["--json", "maintain", "off"])
        .output()
        .unwrap();
    assert!(
        off.status.success(),
        "{}",
        String::from_utf8_lossy(&off.stderr)
    );
    let off: Json = serde_json::from_slice(&off.stdout).unwrap();
    assert_eq!(off["on"], false);
    let a = connect(&sandbox, "claude-code", &repo).await;
    for n in 1..=22 {
        call(
            &a,
            "memfork_handoff",
            json!({"summary": format!("session {n}")}),
        )
        .await;
    }
    let listed = call(
        &a,
        "memfork_task",
        json!({"action": "list", "status": "all"}),
    )
    .await;
    assert_eq!(
        listed["count"], 0,
        "a task was added with maintenance off: {listed}"
    );
    a.cancel().await.unwrap();
}
