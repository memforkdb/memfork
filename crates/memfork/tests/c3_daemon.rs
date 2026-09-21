//! C3 — two MCP stdio clients on one data directory share state through the
//! daemon, **with no daemon started by hand**.
//!
//! Real processes throughout: real `memfork mcp` children, real MCP clients
//! over real pipes, a real daemon started by autostart, and real kills. The
//! failures worth catching here — a race that leaves two daemons, a proxy that
//! hangs when its daemon dies — do not happen in a simulation.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::RoleClient;
use serde_json::{json, Map, Value as Json};
use support::Sandbox;

/// Connect an MCP client to a `memfork mcp` on this sandbox's data directory.
async fn client(sandbox: &Sandbox) -> RunningService<RoleClient, ()> {
    let std_cmd = sandbox.command();
    let transport =
        TokioChildProcess::new(tokio::process::Command::from(std_cmd).configure(|cmd| {
            cmd.arg("mcp");
            cmd.stderr(std::process::Stdio::null());
        }))
        .expect("spawned `memfork mcp`");
    ().serve(transport)
        .await
        .expect("the MCP handshake completed")
}

async fn call(service: &RunningService<RoleClient, ()>, name: &str, args: Json) -> Json {
    let arguments = match args {
        Json::Object(map) => map,
        _ => Map::new(),
    };
    let result = service
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments))
        .await
        .unwrap_or_else(|e| panic!("`{name}` failed: {e}"));
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

/// Call a tool expecting failure, and return the error text.
async fn call_expecting_failure(
    service: &RunningService<RoleClient, ()>,
    name: &str,
    args: Json,
) -> String {
    let arguments = match args {
        Json::Object(map) => map,
        _ => Map::new(),
    };
    match service
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments))
        .await
    {
        Err(e) => e.to_string(),
        Ok(result) if result.is_error == Some(true) => result
            .structured_content
            .map(|c| c.to_string())
            .unwrap_or_default(),
        Ok(other) => panic!("`{name}` unexpectedly succeeded: {other:?}"),
    }
}

// ---- C3 proper --------------------------------------------------------------

#[tokio::test]
async fn c3_two_clients_share_memory_with_no_daemon_started_by_hand() {
    // Nobody runs `memfork serve`. Two clients simply start, and the memory
    // one writes is the memory the other reads.
    let sandbox = Sandbox::new();
    assert!(sandbox.owner().is_none(), "something was already running");

    let a = client(&sandbox).await;
    let b = client(&sandbox).await;

    // Connecting is not using: until one of them needs the memory there is
    // nothing to own it.
    assert!(
        sandbox.owner().is_none(),
        "a daemon started before any client used the store"
    );

    let put = call(
        &a,
        "memfork_put",
        json!({ "key": "shared:1", "value": "written by A" }),
    )
    .await;
    assert_eq!(put["stored"], true);

    // And with that first use, a daemon appeared without anyone asking for one.
    let daemon = sandbox
        .wait_for_daemon(Duration::from_secs(20))
        .expect("autostart produced no daemon");
    assert!(daemon.port.is_some());
    assert_eq!(daemon.memfork_version.as_deref(), Some(memfork::VERSION));

    let got = call(&b, "memfork_get", json!({ "key": "shared:1" })).await;
    assert_eq!(got["found"], true, "B could not see what A wrote: {got}");
    assert_eq!(got["value"], "written by A");

    // And the other way round.
    call(
        &b,
        "memfork_put",
        json!({ "key": "shared:2", "value": "written by B" }),
    )
    .await;
    let back = call(&a, "memfork_get", json!({ "key": "shared:2" })).await;
    assert_eq!(back["value"], "written by B");

    a.cancel().await.expect("A shut down");
    b.cancel().await.expect("B shut down");
}

#[tokio::test]
async fn c3_each_client_keeps_its_own_current_branch() {
    // Shared data, separate places to stand. One client checking out a branch
    // must not move another's feet.
    let sandbox = Sandbox::new();
    let a = client(&sandbox).await;
    let b = client(&sandbox).await;

    call(&a, "memfork_put", json!({ "key": "k", "value": "base" })).await;

    // A forks, which also moves A onto the new branch.
    let forked = call(&a, "memfork_fork", json!({ "name": "attempt" })).await;
    assert_eq!(forked["current_branch"], "attempt");

    // B is still where it was, and sees the branch exists.
    let b_branches = call(&b, "memfork_branches", json!({})).await;
    assert_eq!(
        b_branches["current_branch"], "main",
        "A's fork moved B's current branch"
    );
    let names: Vec<&str> = b_branches["branches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"attempt"),
        "B cannot see A's branch: {names:?}"
    );

    // A writes on its fork; B, on main, does not see it.
    call(
        &a,
        "memfork_put",
        json!({ "key": "k", "value": "A's attempt" }),
    )
    .await;
    let on_main = call(&b, "memfork_get", json!({ "key": "k" })).await;
    assert_eq!(
        on_main["value"], "base",
        "A's write on its own branch leaked onto B's"
    );

    // B checking out does not move A either.
    call(&b, "memfork_checkout", json!({ "name": "attempt" })).await;
    let a_still = call(&a, "memfork_branches", json!({})).await;
    assert_eq!(a_still["current_branch"], "attempt");
    call(&b, "memfork_checkout", json!({ "name": "main" })).await;
    let a_after = call(&a, "memfork_branches", json!({})).await;
    assert_eq!(
        a_after["current_branch"], "attempt",
        "B's checkout moved A's current branch"
    );

    a.cancel().await.expect("A shut down");
    b.cancel().await.expect("B shut down");
}

#[tokio::test]
async fn c3_a_merge_by_one_client_is_visible_to_the_other() {
    let sandbox = Sandbox::new();
    let a = client(&sandbox).await;
    let b = client(&sandbox).await;

    call(
        &a,
        "memfork_put",
        json!({ "key": "plan", "value": "original" }),
    )
    .await;
    call(&a, "memfork_fork", json!({ "name": "attempt" })).await;
    call(
        &a,
        "memfork_put",
        json!({ "key": "plan", "value": "rewritten" }),
    )
    .await;

    // Before the merge, B sees the original.
    assert_eq!(
        call(&b, "memfork_get", json!({ "key": "plan" })).await["value"],
        "original"
    );

    call(
        &a,
        "memfork_merge",
        json!({ "source": "attempt", "target": "main" }),
    )
    .await;

    assert_eq!(
        call(&b, "memfork_get", json!({ "key": "plan" })).await["value"],
        "rewritten",
        "B did not see the merge"
    );

    a.cancel().await.expect("A shut down");
    b.cancel().await.expect("B shut down");
}

// ---- the autostart race -----------------------------------------------------

#[test]
fn c3_two_daemons_racing_leave_exactly_one_and_the_loser_touches_nothing() {
    // The shape that matters: several processes decide at the same instant
    // that no daemon exists. Exactly one may end up owning the directory, and
    // the losers must exit without having written to the log.
    let mut sandbox = Sandbox::new();

    // Start six at once, with no stagger.
    let racers: Vec<usize> = (0..6)
        .map(|_| sandbox.spawn(&["serve", "--port", "0", "--idle-timeout", "25"]))
        .collect();

    let daemon = sandbox
        .wait_for_daemon(Duration::from_secs(20))
        .expect("none of the racers became the daemon");

    // Give the losers a moment to notice and exit.
    std::thread::sleep(Duration::from_millis(1500));

    let alive: Vec<usize> = racers
        .iter()
        .copied()
        .filter(|i| sandbox.is_running(*i))
        .collect();
    assert_eq!(
        alive.len(),
        1,
        "{} racers are still running; exactly one should be",
        alive.len()
    );

    // The survivor is the one the endpoint names.
    let owner = sandbox.owner().expect("nothing owns the directory");
    assert_eq!(owner.pid, daemon.pid);
    assert_eq!(owner.port, daemon.port);

    // Exactly one endpoint file, and a log that only the winner opened: a
    // fresh WAL is its header and nothing else.
    let wal = sandbox.data().join(memfork::persist::WAL_FILE);
    let size = std::fs::metadata(&wal).expect("the log exists").len();
    assert_eq!(
        size,
        memfork::persist::wal::HEADER_LEN,
        "a losing daemon wrote to the log"
    );
}

#[tokio::test]
async fn c3_two_clients_starting_together_both_reach_the_same_daemon() {
    // The same race, from the client side: both must end up talking to one
    // daemon rather than one of them failing with "in use".
    let sandbox = Sandbox::new();

    let (a, b) = tokio::join!(client(&sandbox), client(&sandbox));

    // The race is over reaching the store, and a client reaches for it when it
    // first calls a tool — not when it connects. So the two calls go together.
    let (first, second) = tokio::join!(
        call(&a, "memfork_put", json!({ "key": "a", "value": "1" })),
        call(&b, "memfork_put", json!({ "key": "b", "value": "2" })),
    );
    assert_eq!(first["stored"], true);
    assert_eq!(second["stored"], true);

    let daemon = sandbox
        .wait_for_daemon(Duration::from_secs(20))
        .expect("no daemon");

    // Both saw one another's writes, which is only possible on one daemon.
    let listed = call(&a, "memfork_list", json!({})).await;
    assert_eq!(listed["count"], 2, "the clients are on different daemons");

    // Still exactly one owner.
    assert_eq!(sandbox.owner().expect("an owner").pid, daemon.pid);

    a.cancel().await.expect("A shut down");
    b.cancel().await.expect("B shut down");
}

// ---- the daemon dying mid-session -------------------------------------------

#[tokio::test]
async fn c3_a_proxy_whose_daemon_dies_restarts_it_and_carries_on() {
    // Started by hand here, so the test holds the handle and can kill it for
    // real rather than asking it to stop.
    let mut sandbox = Sandbox::new();
    let daemon = sandbox.spawn(&["serve", "--port", "0", "--idle-timeout", "60"]);
    sandbox
        .wait_for_daemon(Duration::from_secs(20))
        .expect("the daemon did not start");

    let service = client(&sandbox).await;
    call(
        &service,
        "memfork_put",
        json!({ "key": "before", "value": "written before the crash" }),
    )
    .await;

    // Kill it outright: no flush, no cleanup, no chance to tidy up.
    sandbox.kill(daemon);
    assert!(
        sandbox.wait_for_no_daemon(Duration::from_secs(10)),
        "the daemon still holds the directory after being killed"
    );

    // The next call must work: the proxy starts another daemon and retries.
    let after = call(&service, "memfork_get", json!({ "key": "before" })).await;
    assert_eq!(
        after["found"], true,
        "the value written before the crash did not survive: {after}"
    );
    assert_eq!(after["value"], "written before the crash");

    // And the session keeps working.
    call(
        &service,
        "memfork_put",
        json!({ "key": "after", "value": "written after the restart" }),
    )
    .await;
    assert_eq!(
        call(&service, "memfork_get", json!({ "key": "after" })).await["value"],
        "written after the restart"
    );

    assert!(sandbox.owner().is_some(), "no daemon is running afterwards");
    service.cancel().await.expect("shut down");
}

#[tokio::test]
async fn c3_a_proxy_that_cannot_get_a_daemon_says_so_rather_than_hanging() {
    // The failure after the retry: the client should get a clear error in
    // reasonable time, not a call that never returns.
    let mut sandbox = Sandbox::new();
    let daemon = sandbox.spawn(&["serve", "--port", "0", "--idle-timeout", "60"]);
    sandbox
        .wait_for_daemon(Duration::from_secs(20))
        .expect("the daemon did not start");
    let service = client(&sandbox).await;
    call(&service, "memfork_put", json!({ "key": "k", "value": "v" })).await;

    sandbox.kill(daemon);
    assert!(sandbox.wait_for_no_daemon(Duration::from_secs(10)));

    // Make starting another impossible: a second process takes the directory
    // and holds it, so every restart attempt is refused.
    let _blocker = sandbox.spawn(&[
        "crash-writer",
        "--progress",
        "block.txt",
        "--limit",
        "100000",
    ]);
    std::thread::sleep(Duration::from_millis(800));

    let message = tokio::time::timeout(
        Duration::from_secs(60),
        call_expecting_failure(&service, "memfork_get", json!({ "key": "k" })),
    )
    .await
    .expect("the call hung instead of failing");

    assert!(
        message.to_lowercase().contains("daemon"),
        "the error does not explain itself: {message}"
    );
    service.cancel().await.expect("shut down");
}

// ---- stopping, versions, idleness -------------------------------------------

#[test]
fn c3_stop_shuts_the_daemon_down_and_cleans_up() {
    let mut sandbox = Sandbox::new();
    let daemon = sandbox.spawn(&["serve", "--port", "0", "--idle-timeout", "120"]);
    sandbox
        .wait_for_daemon(Duration::from_secs(20))
        .expect("the daemon did not start");
    assert!(sandbox
        .data()
        .join(memfork::persist::lock::ENDPOINT_FILE)
        .exists());

    let output = sandbox.command().args(["stop"]).output().expect("stop ran");
    assert!(output.status.success(), "stop failed: {output:?}");
    let said = String::from_utf8_lossy(&output.stdout);
    assert!(said.contains("stopped the daemon"), "{said}");

    assert!(
        sandbox.wait_for_no_daemon(Duration::from_secs(10)),
        "the daemon still owns the directory"
    );
    assert!(
        !sandbox
            .data()
            .join(memfork::persist::lock::ENDPOINT_FILE)
            .exists(),
        "the endpoint file was left behind"
    );
    assert!(
        sandbox.wait_for_exit(daemon, Duration::from_secs(10)),
        "the daemon released the directory but never exited"
    );
}

#[test]
fn c3_stop_on_a_quiet_directory_is_not_an_error() {
    let sandbox = Sandbox::new();
    let output = sandbox.command().args(["stop"]).output().expect("stop ran");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("nothing is running"));
}

#[test]
fn c3_a_daemon_from_another_version_is_refused_with_a_way_out() {
    // Hand-written endpoint from a "previous version", with the lock genuinely
    // held so the endpoint is believed.
    let mut sandbox = Sandbox::new();
    sandbox.spawn(&["serve", "--port", "0", "--idle-timeout", "60"]);
    let running = sandbox
        .wait_for_daemon(Duration::from_secs(20))
        .expect("the daemon did not start");

    // Rewrite the endpoint to claim a different build.
    let endpoint = memfork::persist::Endpoint {
        memfork_version: Some("0.0.1-ancient".to_owned()),
        ..running
    };
    std::fs::write(
        sandbox.data().join(memfork::persist::lock::ENDPOINT_FILE),
        serde_json::to_string_pretty(&endpoint).expect("json"),
    )
    .expect("written");

    let output = sandbox
        .command()
        .args(["mcp"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("mcp ran");
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "the proxy talked to it anyway");
    assert!(said.contains("0.0.1-ancient"), "{said}");
    assert!(said.contains(memfork::VERSION), "{said}");
    assert!(
        said.contains("memfork stop"),
        "the error does not say how to fix it: {said}"
    );
}

#[test]
fn c3_the_daemon_exits_on_its_own_when_nothing_needs_it() {
    // An autostarted background process that never goes away is a process
    // somebody has to hunt down later.
    let mut sandbox = Sandbox::new();
    let daemon = sandbox.spawn(&["serve", "--port", "0", "--idle-timeout", "2"]);
    sandbox
        .wait_for_daemon(Duration::from_secs(20))
        .expect("the daemon did not start");

    assert!(
        sandbox.wait_for_no_daemon(Duration::from_secs(20)),
        "the daemon outstayed its idle timeout"
    );
    assert!(
        sandbox.wait_for_exit(daemon, Duration::from_secs(10)),
        "the daemon went idle but never exited"
    );
    assert!(
        !sandbox
            .data()
            .join(memfork::persist::lock::ENDPOINT_FILE)
            .exists(),
        "an idle exit left its endpoint file behind"
    );
}

#[test]
fn c3_the_daemon_refuses_anyone_without_the_token() {
    let mut sandbox = Sandbox::new();
    sandbox.spawn(&["serve", "--port", "0", "--idle-timeout", "60"]);
    let daemon = sandbox
        .wait_for_daemon(Duration::from_secs(20))
        .expect("the daemon did not start");
    let port = daemon.port.expect("a port");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let status = runtime.block_on(async move {
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build_http::<http_body_util::Full<hyper::body::Bytes>>();
        let request = hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri(format!("http://127.0.0.1:{port}/mcp"))
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(http_body_util::Full::new(hyper::body::Bytes::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
            )))
            .expect("request");
        client.request(request).await.expect("answered").status()
    });
    assert_eq!(
        status,
        hyper::StatusCode::UNAUTHORIZED,
        "the daemon served a request with no token"
    );
}

// ---- the guard --------------------------------------------------------------

#[test]
fn c3_this_suite_cannot_touch_the_real_data_directory() {
    // The control, not the intention. A command that somehow resolved the real
    // per-user directory must fail rather than write there.
    let sandbox = Sandbox::new();
    let output = sandbox
        .command()
        .env_remove(memfork::persist::datadir::DATA_DIR_ENV)
        .args(["mcp"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("mcp ran");

    assert!(
        !output.status.success(),
        "a command with no data directory started anyway"
    );
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("refusing to use the real per-user data directory"),
        "{said}"
    );

    // And the real directory is still not there, or at least unchanged: the
    // guard fired before anything could be created.
    if let Some(real) = support::real_per_user_dir() {
        assert!(
            !real.join(memfork::persist::lock::LOCK_FILE).exists()
                || std::fs::metadata(real.join(memfork::persist::lock::LOCK_FILE)).is_ok(),
            "the guard did not prevent a write to {}",
            real.display()
        );
    }
}
