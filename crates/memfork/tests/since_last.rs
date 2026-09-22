//! What changed since you last looked: a returning agent's briefing starts
//! with what others did while it was away, and can be only that.
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

static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn repository(sandbox: &Sandbox) -> PathBuf {
    let repo = sandbox.root().join("shop");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
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

#[tokio::test(flavor = "multi_thread")]
async fn a_returning_agent_sees_what_others_did_while_it_was_away() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    std::fs::write(repo.join("src/pay.rs"), "fn pay() {}").unwrap();

    let a = connect(&sandbox, "claude-code", &repo).await;
    call(
        &a,
        "memfork_put",
        json!({"key": "shop:decision:db", "value": "postgres"}),
    )
    .await;
    call(&a, "memfork_put", json!({"key": "shop:fact:pay", "value": "payments in src/pay.rs", "sources": ["src/pay.rs"]})).await;
    call(
        &a,
        "memfork_task",
        json!({"action": "add", "id": "refunds", "title": "refunds"}),
    )
    .await;
    call(&a, "memfork_handoff", json!({"summary": "my own note"})).await;
    let first = call(&a, "memfork_resume", json!({})).await;
    assert_eq!(first["since_last"]["available"], true, "{first}");
    assert_eq!(
        first["since_last"]["nothing_new"], true,
        "its own work is not news: {first}"
    );

    // While A is away, B works.
    let b = connect(&sandbox, "codex-mcp-client", &repo).await;
    call(
        &b,
        "memfork_put",
        json!({"key": "shop:decision:cache", "value": "no cache for carts"}),
    )
    .await;
    call(
        &b,
        "memfork_put",
        json!({"key": "shop:decision:db", "value": "postgres 16"}),
    )
    .await;
    call(
        &b,
        "memfork_task",
        json!({"action": "claim", "id": "refunds"}),
    )
    .await;
    call(
        &b,
        "memfork_task",
        json!({"action": "done", "id": "refunds"}),
    )
    .await;
    call(&b, "memfork_handoff", json!({"summary": "refunds shipped"})).await;
    b.cancel().await.unwrap();
    // And a file a fact rests on changes, with no commit at all.
    std::fs::write(repo.join("src/pay.rs"), "fn pay(amount: u64) {}").unwrap();

    let back = call(&a, "memfork_resume", json!({})).await;
    let since = &back["since_last"];
    assert_eq!(since["available"], true, "{back}");
    assert_eq!(since["nothing_new"], false);
    assert!(since["commits"].as_u64().unwrap() >= 5, "{since}");
    let decisions: Vec<(&str, &str)> = since["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| (d["key"].as_str().unwrap(), d["change"].as_str().unwrap()))
        .collect();
    assert_eq!(
        decisions,
        [
            ("shop:decision:cache", "new"),
            ("shop:decision:db", "changed")
        ]
    );
    assert_eq!(since["decisions"][0]["by"], "codex-mcp-client");
    assert_eq!(
        since["tasks"][0]["change"], "finished by codex-mcp-client",
        "{since}"
    );
    assert_eq!(since["handoffs_by_others"][0]["summary"], "refunds shipped");
    assert_eq!(
        since["handoffs_by_others"].as_array().unwrap().len(),
        1,
        "{since}"
    );
    assert_eq!(since["facts"][0]["fact"], "stale", "{since}");

    // Only the news, which is smaller than the whole briefing.
    call(
        &a,
        "memfork_put",
        json!({"key": "shop:note:x", "value": "a", "branch": "main"}),
    )
    .await;
    let c = connect(&sandbox, "gemini-cli-mcp-client", &repo).await;
    call(
        &c,
        "memfork_put",
        json!({"key": "shop:decision:queue", "value": "one queue"}),
    )
    .await;
    c.cancel().await.unwrap();
    let whole = call(&a, "memfork_resume", json!({})).await;
    call(
        &a,
        "memfork_put",
        json!({"key": "shop:note:y", "value": "b"}),
    )
    .await;
    let d = connect(&sandbox, "gemini-cli-mcp-client", &repo).await;
    call(
        &d,
        "memfork_put",
        json!({"key": "shop:decision:queue", "value": "two queues"}),
    )
    .await;
    d.cancel().await.unwrap();
    let news = call(&a, "memfork_resume", json!({"since_last_only": true})).await;
    assert!(news.get("latest_handoff").is_none(), "{news}");
    assert!(news.get("recent_decisions").is_none(), "{news}");
    assert_eq!(
        news["since_last"]["decisions"][0]["value"], "two queues",
        "{news}"
    );
    assert!(
        news["budget"]["bytes"].as_u64().unwrap() < whole["budget"]["bytes"].as_u64().unwrap(),
        "only the news was not smaller: {} vs {}",
        news["budget"]["bytes"],
        whole["budget"]["bytes"]
    );
    assert_eq!(news["budget"]["bytes"], news.to_string().len() as u64);

    // Looking again at once: nothing new.
    let again = call(&a, "memfork_resume", json!({"since_last_only": true})).await;
    assert_eq!(again["since_last"]["nothing_new"], true, "{again}");
    assert_eq!(again["since_last"]["commits"], 0);
    a.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_record_the_whole_briefing_comes_back() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let a = connect(&sandbox, "claude-code", &repo).await;
    call(
        &a,
        "memfork_put",
        json!({"key": "shop:decision:db", "value": "postgres"}),
    )
    .await;
    a.cancel().await.unwrap();

    let newcomer = connect(&sandbox, "codex-mcp-client", &repo).await;
    let brief = call(
        &newcomer,
        "memfork_resume",
        json!({"since_last_only": true}),
    )
    .await;
    assert_eq!(brief["since_last"]["available"], false, "{brief}");
    assert!(brief["since_last"]["reason"]
        .as_str()
        .unwrap()
        .contains("whole briefing"));
    assert_eq!(
        brief["recent_decisions"][0]["key"], "shop:decision:db",
        "{brief}"
    );

    // A branch discarded and made again is not the history it saw.
    call(&newcomer, "memfork_fork", json!({"name": "try"})).await;
    call(
        &newcomer,
        "memfork_put",
        json!({"key": "shop:decision:x", "value": "1"}),
    )
    .await;
    call(&newcomer, "memfork_resume", json!({})).await;
    call(&newcomer, "memfork_checkout", json!({"name": "main"})).await;
    call(&newcomer, "memfork_discard", json!({"name": "try"})).await;
    call(&newcomer, "memfork_fork", json!({"name": "try"})).await;
    let fresh = call(
        &newcomer,
        "memfork_resume",
        json!({"since_last_only": true}),
    )
    .await;
    assert_eq!(fresh["since_last"]["available"], false, "{fresh}");
    newcomer.cancel().await.unwrap();
}
