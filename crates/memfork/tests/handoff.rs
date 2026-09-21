//! Handing work between two clients through real MCP sessions.
//!
//! Two `memfork mcp` processes, started from the same repository by two
//! differently named clients, share one daemon: the first records decisions
//! and leaves a handoff, the second resumes from it and sees who wrote what.
//! Everything runs in a temporary data directory with the usual guards.

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

/// A repository inside the sandbox, found by its `.git` directory.
fn repository(sandbox: &Sandbox, name: &str) -> PathBuf {
    let repo = sandbox.root().join(name);
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    repo
}

/// A client named `name`, running `memfork mcp` from `cwd` with `args`.
async fn connect(sandbox: &Sandbox, name: &str, cwd: &Path, args: &[&str]) -> Client {
    let mut std_cmd = sandbox.command();
    std_cmd.current_dir(cwd);
    let transport =
        TokioChildProcess::new(tokio::process::Command::from(std_cmd).configure(|cmd| {
            cmd.arg("mcp");
            cmd.args(args);
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
    result
        .structured_content
        .unwrap_or_else(|| panic!("`{tool}` returned no structured content"))
}

fn instructions(client: &Client) -> String {
    client
        .peer_info()
        .and_then(|i| i.instructions.clone())
        .expect("the server sent instructions")
}

#[tokio::test(flavor = "multi_thread")]
async fn one_client_hands_off_and_another_resumes() {
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox, "Shop Front");

    // The first agent, started deep inside the repository.
    let first = connect(&sandbox, "claude-code", &repo.join("src"), &[]).await;
    assert!(
        instructions(&first).contains("project namespace is `shop-front`"),
        "{}",
        instructions(&first)
    );

    let empty = call(&first, "memfork_resume", json!({})).await;
    assert_eq!(empty["empty"], true, "{empty}");
    assert_eq!(empty["namespace"], "shop-front");

    call(
        &first,
        "memfork_put",
        json!({ "key": "shop-front:decision:payments",
                "value": "Use the hosted checkout: no card data touches our servers." }),
    )
    .await;
    call(
        &first,
        "memfork_put",
        json!({ "key": "shop-front:task:refunds", "value": "Wire up refunds." }),
    )
    .await;
    let handed = call(
        &first,
        "memfork_handoff",
        json!({
            "summary": "Checkout works end to end; refunds are not started.",
            "done": ["hosted checkout", "order emails"],
            "next": ["refunds", "tax for EU orders"],
            "blockers": ["need a sandbox account for refunds"],
        }),
    )
    .await;
    assert_eq!(handed["key"], "shop-front:handoff:00000001");
    first.cancel().await.expect("first client shut down");

    // A different client, later, from the repository root.
    let second = connect(&sandbox, "codex-mcp-client", &repo, &[]).await;
    let brief = call(&second, "memfork_resume", json!({})).await;
    assert_eq!(brief["empty"], false, "{brief}");
    assert_eq!(brief["namespace"], "shop-front");
    let handoff = &brief["latest_handoff"];
    assert_eq!(
        handoff["summary"],
        "Checkout works end to end; refunds are not started."
    );
    assert_eq!(handoff["next"], json!(["refunds", "tax for EU orders"]));
    assert_eq!(handoff["by"], "claude-code", "who handed off is recorded");
    assert_eq!(
        brief["recent_decisions"][0]["key"],
        "shop-front:decision:payments"
    );
    assert_eq!(brief["recent_decisions"][0]["by"], "claude-code");
    assert_eq!(brief["open_tasks"][0]["key"], "shop-front:task:refunds");

    // The second client's own writes carry its name, not the first's.
    call(
        &second,
        "memfork_put",
        json!({ "key": "shop-front:decision:tax", "value": "Use the provider's tax API." }),
    )
    .await;
    let got = call(
        &second,
        "memfork_get",
        json!({ "key": "shop-front:decision:tax" }),
    )
    .await;
    assert_eq!(got["written_by"], "codex-mcp-client");
    let earlier = call(
        &second,
        "memfork_get",
        json!({ "key": "shop-front:decision:payments" }),
    )
    .await;
    assert_eq!(earlier["written_by"], "claude-code");

    // And its handoff is the next one; the first is kept.
    let next = call(
        &second,
        "memfork_handoff",
        json!({ "summary": "tax decided" }),
    )
    .await;
    assert_eq!(next["number"], 2);
    let brief = call(&second, "memfork_resume", json!({})).await;
    assert_eq!(brief["latest_handoff"]["by"], "codex-mcp-client");
    assert_eq!(brief["earlier_handoffs"], 1);
    second.cancel().await.expect("second client shut down");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_projects_share_the_store_without_seeing_each_other() {
    let sandbox = Sandbox::new();
    let shop = repository(&sandbox, "shop");
    let blog = repository(&sandbox, "blog");

    let a = connect(&sandbox, "client-a", &shop, &[]).await;
    let b = connect(&sandbox, "client-b", &blog, &[]).await;
    call(&a, "memfork_handoff", json!({ "summary": "shop work" })).await;

    assert_eq!(call(&b, "memfork_resume", json!({})).await["empty"], true);
    // A project can still look at another on purpose.
    let other = call(&b, "memfork_resume", json!({ "namespace": "shop" })).await;
    assert_eq!(other["latest_handoff"]["summary"], "shop work");

    // Raw keys are literal: nothing was prefixed behind anyone's back.
    call(&a, "memfork_put", json!({ "key": "plain", "value": "v" })).await;
    let got = call(&b, "memfork_get", json!({ "key": "plain" })).await;
    assert_eq!(got["found"], true);

    a.cancel().await.unwrap();
    b.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_namespace_can_be_named_by_flag_or_environment() {
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox, "whatever");

    let flagged = connect(&sandbox, "c", &repo, &["--namespace", "team-api"]).await;
    assert!(instructions(&flagged).contains("project namespace is `team-api`"));
    call(
        &flagged,
        "memfork_handoff",
        json!({ "summary": "from the flag" }),
    )
    .await;
    flagged.cancel().await.unwrap();

    let mut std_cmd = sandbox.command();
    std_cmd
        .current_dir(&repo)
        .env(memfork::namespace::NAMESPACE_ENV, "team-api");
    let transport =
        TokioChildProcess::new(tokio::process::Command::from(std_cmd).configure(|cmd| {
            cmd.arg("mcp");
            cmd.stderr(Stdio::null());
        }))
        .unwrap();
    let from_env = ClientConfig::new(Default::default(), Implementation::new("c", "1"))
        .serve(transport)
        .await
        .unwrap();
    let brief = call(&from_env, "memfork_resume", json!({})).await;
    assert_eq!(brief["latest_handoff"]["summary"], "from the flag");
    from_env.cancel().await.unwrap();
}

#[test]
fn an_unusable_namespace_flag_is_refused_before_anything_starts() {
    let sandbox = Sandbox::new();
    let out = sandbox
        .command()
        .args(["mcp", "--namespace", "Team API"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not a usable namespace"), "{err}");
    assert!(err.contains("`team-api`"), "{err}");
    assert!(sandbox.owner().is_none(), "a daemon was started anyway");
}

#[test]
fn a_client_cannot_write_in_another_clients_name() {
    let sandbox = Sandbox::new();
    let out = sandbox
        .command()
        .args([
            "call",
            "memfork_put",
            r#"{"key":"k","value":"v","meta":{"memfork.by":"someone-else"}}"#,
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("reserved"), "{err}");
}
