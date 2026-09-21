//! B1 — an MCP client test harness can call every tool, and round-trip
//! fork → write → merge and fork → write → discard.
//!
//! The harness is a real MCP client from the same SDK the server uses,
//! speaking real MCP over real pipes to a real `memfork mcp` child process.
//! Nothing here is mocked: if the wire format, the schemas or the session
//! handling were wrong, this would fail the way a client would.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use rmcp::model::CallToolRequestParams;
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::RoleClient;
use serde_json::{json, Map, Value as Json};

mod support;

/// Spawn `memfork mcp` and connect an MCP client to it.
async fn connect() -> RunningService<RoleClient, ()> {
    let std_cmd = support::memfork();
    let transport =
        TokioChildProcess::new(tokio::process::Command::from(std_cmd).configure(|cmd| {
            // `--ephemeral` because these test the protocol, not durability: each
            // case wants a database of its own and none of them should touch the
            // real data directory. Without it they would queue behind each other
            // on the data directory lock, which is the right behaviour and the
            // wrong test.
            cmd.arg("mcp").arg("--ephemeral");
            // Keep the child's stderr out of the test output unless it matters.
            cmd.stderr(std::process::Stdio::null());
        }))
        .expect("spawned `memfork mcp`");
    ().serve(transport)
        .await
        .expect("the MCP handshake completed")
}

/// Call a tool and return its structured result, failing the test on a
/// tool-level error.
async fn call(client: &RunningService<RoleClient, ()>, name: &str, args: Json) -> Json {
    let arguments = match args {
        Json::Object(map) => map,
        Json::Null => Map::new(),
        other => panic!("arguments must be an object, got {other}"),
    };
    let result = client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments))
        .await
        .unwrap_or_else(|e| panic!("`{name}` failed at the protocol level: {e}"));

    let structured = result
        .structured_content
        .clone()
        .unwrap_or_else(|| panic!("`{name}` returned no structured content"));
    assert_ne!(
        result.is_error,
        Some(true),
        "`{name}` returned a tool error: {structured}"
    );
    structured
}

/// Call a tool expecting it to fail at the tool level, and return the error.
async fn call_expecting_error(
    client: &RunningService<RoleClient, ()>,
    name: &str,
    args: Json,
) -> Json {
    let arguments = match args {
        Json::Object(map) => map,
        _ => Map::new(),
    };
    let result = client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments))
        .await
        .unwrap_or_else(|e| panic!("`{name}` failed at the protocol level: {e}"));
    assert_eq!(
        result.is_error,
        Some(true),
        "`{name}` unexpectedly succeeded"
    );
    result.structured_content.clone().expect("structured error")
}

#[tokio::test]
async fn b1_the_server_lists_every_tool_with_a_usable_schema() {
    let client = connect().await;

    let info = client.peer_info().expect("the server sent its info");
    let server_info = info
        .server_info
        .clone()
        .expect("the server identified itself");
    assert_eq!(server_info.name, "memfork");
    let instructions = info.instructions.as_deref().unwrap_or_default();
    assert!(
        instructions.to_ascii_lowercase().contains("fork"),
        "the server does not tell the client what it is for"
    );
    assert!(
        instructions.to_ascii_lowercase().contains("kept on disk"),
        "the server does not say that memory survives a restart"
    );

    let tools = client.list_all_tools().await.expect("tools listed");
    let names: BTreeSet<String> = tools.iter().map(|t| t.name.to_string()).collect();
    let expected: BTreeSet<String> = [
        "memfork_put",
        "memfork_get",
        "memfork_delete",
        "memfork_list",
        "memfork_search",
        "memfork_fork",
        "memfork_checkout",
        "memfork_merge",
        "memfork_discard",
        "memfork_branches",
        "memfork_log",
        "memfork_at",
        "memfork_resume",
        "memfork_handoff",
        "memfork_diff",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    assert_eq!(names, expected, "DESIGN §6 lists exactly these tools");

    for tool in &tools {
        let schema = tool.input_schema.as_ref();
        assert_eq!(
            schema.get("type").and_then(Json::as_str),
            Some("object"),
            "{}'s schema is not an object",
            tool.name
        );
        assert!(
            tool.description.as_ref().is_some_and(|d| d.len() > 80),
            "{} has no usable description",
            tool.name
        );
    }

    client.cancel().await.expect("clean shutdown");
}

#[tokio::test]
async fn b1_every_tool_can_be_called_over_mcp() {
    let client = connect().await;
    let mut called: BTreeSet<&str> = BTreeSet::new();

    // Seeded so the read tools have something to find.
    call(
        &client,
        "memfork_put",
        json!({ "key": "note:1", "value": "first", "importance": 0.8,
                "meta": { "source": "b1" } }),
    )
    .await;
    called.insert("memfork_put");

    call(
        &client,
        "memfork_put",
        json!({ "key": "vec:1", "value": "vectored", "embedding": [1.0, 0.0, 0.0] }),
    )
    .await;

    let got = call(&client, "memfork_get", json!({ "key": "note:1" })).await;
    assert_eq!(got["found"], true);
    assert_eq!(got["value"], "first");
    assert_eq!(got["meta"]["source"], "b1");
    called.insert("memfork_get");

    let listed = call(&client, "memfork_list", json!({ "prefix": "note:" })).await;
    assert_eq!(listed["count"], 1);
    called.insert("memfork_list");

    let found = call(
        &client,
        "memfork_search",
        json!({ "embedding": [1.0, 0.0, 0.0], "k": 1 }),
    )
    .await;
    assert_eq!(found["hits"][0]["key"], "vec:1");
    called.insert("memfork_search");

    let branches = call(&client, "memfork_branches", json!({})).await;
    assert_eq!(branches["current_branch"], "main");
    called.insert("memfork_branches");

    let log = call(&client, "memfork_log", json!({ "limit": 2 })).await;
    assert_eq!(log["entries"].as_array().map(Vec::len), Some(2));
    called.insert("memfork_log");

    let past = call(&client, "memfork_at", json!({ "seq": 1, "key": "note:1" })).await;
    assert_eq!(past["value"], "first");
    called.insert("memfork_at");

    call(&client, "memfork_fork", json!({ "name": "scratch" })).await;
    called.insert("memfork_fork");

    call(&client, "memfork_checkout", json!({ "name": "main" })).await;
    called.insert("memfork_checkout");

    let diff = call(
        &client,
        "memfork_diff",
        json!({ "a": "main", "b": "scratch" }),
    )
    .await;
    assert_eq!(diff["count"], 0);
    called.insert("memfork_diff");

    let deleted = call(&client, "memfork_delete", json!({ "key": "note:1" })).await;
    assert_eq!(deleted["deleted"], true);
    called.insert("memfork_delete");

    let merged = call(&client, "memfork_merge", json!({ "source": "scratch" })).await;
    assert_eq!(merged["result"], "up_to_date");
    called.insert("memfork_merge");

    let discarded = call(&client, "memfork_discard", json!({ "name": "scratch" })).await;
    assert_eq!(discarded["discarded"], true);
    called.insert("memfork_discard");

    let empty = call(&client, "memfork_resume", json!({ "namespace": "b1" })).await;
    assert_eq!(empty["empty"], true);
    let handed = call(
        &client,
        "memfork_handoff",
        json!({ "namespace": "b1", "summary": "exercised", "next": ["ship"] }),
    )
    .await;
    assert_eq!(handed["key"], "b1:handoff:00000001");
    called.insert("memfork_handoff");
    let resumed = call(&client, "memfork_resume", json!({ "namespace": "b1" })).await;
    assert_eq!(resumed["latest_handoff"]["next"][0], "ship");
    called.insert("memfork_resume");

    assert_eq!(
        called.len(),
        memfork::tools::names().len(),
        "not every tool was exercised: {called:?}"
    );
    client.cancel().await.expect("clean shutdown");
}

#[tokio::test]
async fn b1_fork_write_merge_round_trips() {
    let client = connect().await;

    call(
        &client,
        "memfork_put",
        json!({ "key": "plan:1", "value": "the original plan" }),
    )
    .await;

    // Fork, and land on the new branch.
    let forked = call(&client, "memfork_fork", json!({ "name": "attempt" })).await;
    assert_eq!(forked["current_branch"], "attempt");

    call(
        &client,
        "memfork_put",
        json!({ "key": "plan:1", "value": "a rewrite that worked" }),
    )
    .await;
    call(
        &client,
        "memfork_put",
        json!({ "key": "plan:2", "value": "and something new" }),
    )
    .await;

    // The parent cannot see any of it yet.
    let on_main = call(
        &client,
        "memfork_get",
        json!({ "key": "plan:1", "branch": "main" }),
    )
    .await;
    assert_eq!(on_main["value"], "the original plan");
    let new_on_main = call(
        &client,
        "memfork_get",
        json!({ "key": "plan:2", "branch": "main" }),
    )
    .await;
    assert_eq!(new_on_main["found"], false);

    // Merge it in.
    let merged = call(
        &client,
        "memfork_merge",
        json!({ "source": "attempt", "target": "main" }),
    )
    .await;
    assert_eq!(merged["result"], "fast_forward");
    assert!(merged["conflicts"].as_array().unwrap().is_empty());

    let after = call(
        &client,
        "memfork_get",
        json!({ "key": "plan:1", "branch": "main" }),
    )
    .await;
    assert_eq!(after["value"], "a rewrite that worked");
    let added = call(
        &client,
        "memfork_get",
        json!({ "key": "plan:2", "branch": "main" }),
    )
    .await;
    assert_eq!(added["value"], "and something new");

    client.cancel().await.expect("clean shutdown");
}

#[tokio::test]
async fn b1_fork_write_discard_round_trips() {
    let client = connect().await;

    call(
        &client,
        "memfork_put",
        json!({ "key": "plan:1", "value": "the original plan" }),
    )
    .await;
    let before = call(&client, "memfork_branches", json!({})).await;
    let main_before = before["branches"][0]["commit"].clone();

    call(&client, "memfork_fork", json!({ "name": "attempt" })).await;
    call(
        &client,
        "memfork_put",
        json!({ "key": "plan:1", "value": "a rewrite that did not work" }),
    )
    .await;
    call(
        &client,
        "memfork_put",
        json!({ "key": "junk:1", "value": "and some debris" }),
    )
    .await;

    // Throw it away. The session must not be stranded on a dead branch.
    let discarded = call(&client, "memfork_discard", json!({ "name": "attempt" })).await;
    assert_eq!(discarded["discarded"], true);
    assert_eq!(discarded["current_branch"], "main");
    assert_eq!(discarded["switched_branch"], true);

    // `main` is exactly as it was, down to the commit id.
    let after = call(&client, "memfork_branches", json!({})).await;
    assert_eq!(after["branches"].as_array().map(Vec::len), Some(1));
    assert_eq!(after["branches"][0]["commit"], main_before);

    let plan = call(&client, "memfork_get", json!({ "key": "plan:1" })).await;
    assert_eq!(plan["value"], "the original plan");
    let junk = call(&client, "memfork_get", json!({ "key": "junk:1" })).await;
    assert_eq!(junk["found"], false);

    client.cancel().await.expect("clean shutdown");
}

#[tokio::test]
async fn b1_a_conflicting_merge_is_reported_not_applied() {
    let client = connect().await;

    call(
        &client,
        "memfork_put",
        json!({ "key": "k", "value": "base" }),
    )
    .await;
    call(&client, "memfork_fork", json!({ "name": "side" })).await;
    call(
        &client,
        "memfork_put",
        json!({ "key": "k", "value": "theirs" }),
    )
    .await;
    call(
        &client,
        "memfork_put",
        json!({ "key": "k", "value": "ours", "branch": "main" }),
    )
    .await;

    // The default policy changes nothing and hands back the conflicting keys.
    let conflict = call(
        &client,
        "memfork_merge",
        json!({ "source": "side", "target": "main" }),
    )
    .await;
    assert_eq!(conflict["result"], "conflict");
    assert_eq!(conflict["conflicts"][0], "k");
    assert_eq!(conflict["nothing_changed"], true);
    let unchanged = call(
        &client,
        "memfork_get",
        json!({ "key": "k", "branch": "main" }),
    )
    .await;
    assert_eq!(unchanged["value"], "ours");

    // Naming a side resolves it.
    let resolved = call(
        &client,
        "memfork_merge",
        json!({ "source": "side", "target": "main", "policy": "theirs" }),
    )
    .await;
    assert_eq!(resolved["result"], "merged");
    let after = call(
        &client,
        "memfork_get",
        json!({ "key": "k", "branch": "main" }),
    )
    .await;
    assert_eq!(after["value"], "theirs");

    client.cancel().await.expect("clean shutdown");
}

#[tokio::test]
async fn b1_engine_refusals_come_back_as_tool_errors_the_model_can_act_on() {
    let client = connect().await;

    let err = call_expecting_error(
        &client,
        "memfork_get",
        json!({ "key": "k", "branch": "no-such-branch" }),
    )
    .await;
    assert!(
        err["error"].as_str().unwrap().contains("no such branch"),
        "{err}"
    );

    let err = call_expecting_error(&client, "memfork_discard", json!({ "name": "main" })).await;
    assert!(
        err["error"]
            .as_str()
            .unwrap()
            .contains("cannot be discarded"),
        "{err}"
    );

    client.cancel().await.expect("clean shutdown");
}

#[tokio::test]
async fn b1_a_malformed_call_is_a_protocol_error_not_a_silent_success() {
    let client = connect().await;

    // A missing required argument is the caller's mistake, so MCP should see a
    // protocol error rather than a result that looks like it worked.
    let result = client
        .call_tool(CallToolRequestParams::new("memfork_put").with_arguments(Map::new()))
        .await;
    assert!(result.is_err(), "an argument-less put was accepted");

    let result = client
        .call_tool(CallToolRequestParams::new("memfork_not_a_tool").with_arguments(Map::new()))
        .await;
    assert!(result.is_err(), "an unknown tool was accepted");

    client.cancel().await.expect("clean shutdown");
}

#[tokio::test]
async fn b1_a_result_never_leaves_the_caller_needing_another_call_to_orient() {
    // A model that cannot tell which branch it is on spends a call finding
    // out, and one whose write is not confirmed reads the key back. Both were
    // seen in practice. Every result carries the current branch, and a write
    // reports what it stored.
    let client = connect().await;

    for (tool, args) in [
        ("memfork_put", json!({ "key": "k", "value": "v" })),
        ("memfork_get", json!({ "key": "k" })),
        ("memfork_delete", json!({ "key": "gone" })),
        ("memfork_list", json!({})),
        ("memfork_search", json!({ "embedding": [1.0] })),
        ("memfork_branches", json!({})),
        ("memfork_log", json!({})),
        ("memfork_at", json!({ "seq": 0 })),
        ("memfork_diff", json!({ "a": "main", "b": "main" })),
        ("memfork_fork", json!({ "name": "side" })),
        ("memfork_checkout", json!({ "name": "main" })),
        ("memfork_merge", json!({ "source": "side" })),
        ("memfork_discard", json!({ "name": "side" })),
    ] {
        let result = call(&client, tool, args).await;
        assert!(
            result["current_branch"].is_string(),
            "`{tool}` does not say which branch the caller is on: {result}"
        );
    }

    // A write is self-evidencing: the value comes back, so there is nothing to
    // verify with a second call.
    let put = call(
        &client,
        "memfork_put",
        json!({ "key": "note:1", "value": "the first thing" }),
    )
    .await;
    assert_eq!(put["stored"], true);
    assert_eq!(put["value"], "the first thing");
    assert_eq!(put["replaced"], false);

    let again = call(
        &client,
        "memfork_put",
        json!({ "key": "note:1", "value": "the second thing" }),
    )
    .await;
    assert_eq!(again["value"], "the second thing");
    assert_eq!(
        again["replaced"], true,
        "an overwrite was not reported as one"
    );

    client.cancel().await.expect("clean shutdown");
}

#[tokio::test]
async fn b1_forking_needs_no_checkout_and_says_so() {
    // The fork/write/merge loop should cost one call per step. A fork that did
    // not switch, or did not say it had, would cost two.
    let client = connect().await;

    let forked = call(&client, "memfork_fork", json!({ "name": "attempt" })).await;
    assert_eq!(forked["current_branch"], "attempt");

    // The next write lands on the fork without naming it.
    let put = call(&client, "memfork_put", json!({ "key": "k", "value": "v" })).await;
    assert_eq!(put["branch"], "attempt");
    assert_eq!(put["current_branch"], "attempt");

    // And discarding moves the caller somewhere valid, and says where.
    let discarded = call(&client, "memfork_discard", json!({ "name": "attempt" })).await;
    assert_eq!(discarded["current_branch"], "main");
    assert_eq!(discarded["switched_branch"], true);

    client.cancel().await.expect("clean shutdown");
}
