//! Agents working together through memory: claims on tasks, lessons from
//! abandoned attempts, facts that know when they are stale — through real MCP
//! sessions sharing one daemon, and through the command line.
//!
//! Everything runs in a sandbox: a temporary data directory, home and PATH,
//! the guard against the real data directory, and a repository made for the
//! test. Every daemon a test starts stops with its sandbox.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use rmcp::model::{CallToolRequestParams, ClientConfig, Implementation};
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::RoleClient;
use serde_json::{json, Value as Json};
use support::Sandbox;

type Client = RunningService<RoleClient, ClientConfig>;

/// One test at a time starts daemons in this file; see `handoff.rs`.
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

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_race_for_a_task_and_a_dead_ones_claim_runs_out() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let a = connect(&sandbox, "claude-code", &repo).await;
    let b = connect(&sandbox, "codex-mcp-client", &repo).await;
    // A second session of the same tool: same client name, different session.
    let a2 = connect(&sandbox, "claude-code", &repo).await;

    let added = call(
        &a,
        "memfork_task",
        json!({"action": "add", "title": "refunds"}),
    )
    .await;
    assert_eq!(added["key"], "shop:task:1");

    let won = call(
        &a,
        "memfork_task",
        json!({"action": "claim", "id": "1", "lease_seconds": 2}),
    )
    .await;
    assert_eq!(won["claimed"], true, "{won}");
    let lost = call(
        &b,
        "memfork_task",
        json!({"action": "claim", "id": "1", "lease_seconds": 2}),
    )
    .await;
    assert_eq!(lost["claimed"], false);
    assert_eq!(lost["held_by"], "claude-code");
    let same_tool = call(&a2, "memfork_task", json!({"action": "claim", "id": "1"})).await;
    assert_eq!(
        same_tool["claimed"], false,
        "another session of the same tool got in"
    );
    assert_eq!(same_tool["same_client"], true);

    // No tool calls for well over a lease period: the proxy keeps it alive.
    tokio::time::sleep(Duration::from_millis(4500)).await;
    let still = call(&b, "memfork_task", json!({"action": "claim", "id": "1"})).await;
    assert_eq!(
        still["claimed"], false,
        "the claim lapsed while its proxy was up: {still}"
    );

    // The holder goes away; within one lease period the task is free.
    a.cancel().await.unwrap();
    let gone = Instant::now();
    let mut freed = None;
    while gone.elapsed() < Duration::from_secs(10) {
        let try_it = call(
            &b,
            "memfork_task",
            json!({"action": "claim", "id": "1", "lease_seconds": 30}),
        )
        .await;
        if try_it["claimed"] == true {
            freed = Some(gone.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let freed = freed.expect("the dead client's claim never ran out");
    // At most one lease period after its last renewal, which was at most half
    // a period before it died; with a margin for a loaded machine.
    assert!(
        freed <= Duration::from_millis(2000 + 1500),
        "freed after {freed:?}"
    );

    let listed = call(
        &b,
        "memfork_task",
        json!({"action": "list", "status": "claimed"}),
    )
    .await;
    assert_eq!(listed["tasks"][0]["held_by"], "codex-mcp-client");
    let done = call(&b, "memfork_task", json!({"action": "done", "id": "1"})).await;
    assert_eq!(done["changed"], true);
    let brief = call(&a2, "memfork_resume", json!({})).await;
    let open = brief.get("open_tasks").and_then(Json::as_array);
    assert!(
        open.is_none_or(Vec::is_empty),
        "a done task is still open: {brief}"
    );

    b.cancel().await.unwrap();
    a2.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lesson_reaches_the_next_agent_and_nothing_else_of_the_attempt_does() {
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
    call(&a, "memfork_fork", json!({"name": "try-sqlite"})).await;
    call(
        &a,
        "memfork_put",
        json!({"key": "shop:decision:db", "value": "sqlite"}),
    )
    .await;
    call(
        &a,
        "memfork_put",
        json!({"key": "shop:note:scratch", "value": "half-done migration"}),
    )
    .await;
    let gone = call(
        &a,
        "memfork_discard",
        json!({"name": "try-sqlite", "lesson": "sqlite locks under our concurrent writers"}),
    )
    .await;
    assert_eq!(gone["lesson"]["branch"], "main");
    a.cancel().await.unwrap();

    let b = connect(&sandbox, "codex-mcp-client", &repo).await;
    let brief = call(&b, "memfork_resume", json!({})).await;
    let lessons = brief["lessons"].as_array().unwrap();
    assert_eq!(lessons.len(), 1, "{brief}");
    assert_eq!(
        lessons[0]["lesson"],
        "sqlite locks under our concurrent writers"
    );
    assert_eq!(lessons[0]["branch"], "try-sqlite");
    assert_eq!(lessons[0]["by"], "claude-code");
    let text = brief.to_string();
    assert!(
        !text.contains("half-done migration"),
        "the fork leaked: {text}"
    );
    assert!(
        !text.contains("\"sqlite\""),
        "the fork's decision leaked: {text}"
    );
    let db = call(&b, "memfork_get", json!({"key": "shop:decision:db"})).await;
    assert_eq!(db["value"], "postgres");
    let found = call(&b, "memfork_search", json!({"text": "sqlite"})).await;
    assert_eq!(found["hits"][0]["key"], "shop:lesson:00000001", "{found}");
    b.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fact_is_fresh_then_stale_when_its_file_changes_then_fresh_when_updated() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    std::fs::write(repo.join("src/login.rs"), "pub fn login() {}\n").unwrap();
    let a = connect(&sandbox, "claude-code", &repo).await;

    let fact = json!({
        "key": "shop:fact:auth",
        "value": "auth lives in src/, entry login.rs",
        "sources": ["src/login.rs"],
    });
    let put = call(&a, "memfork_put", fact.clone()).await;
    assert_eq!(put["sources_recorded"], true, "{put}");
    let got = call(&a, "memfork_get", json!({"key": "shop:fact:auth"})).await;
    assert_eq!(got["fact"], "fresh", "{got}");
    assert!(
        got.get("recorded").is_none(),
        "hashes leaked to the client: {got}"
    );

    std::fs::write(repo.join("src/login.rs"), "pub fn login(user: &str) {}\n").unwrap();
    let got = call(&a, "memfork_get", json!({"key": "shop:fact:auth"})).await;
    assert_eq!(got["fact"], "stale", "{got}");
    assert_eq!(got["stale_sources"], json!(["src/login.rs"]));
    // The briefing says so too.
    let brief = call(&a, "memfork_resume", json!({})).await;
    assert_eq!(brief["facts"][0]["fact"], "stale", "{brief}");

    // The agent re-checks and writes it again: fresh.
    call(
        &a,
        "memfork_put",
        json!({
            "key": "shop:fact:auth",
            "value": "auth lives in src/, entry login.rs, which takes a user",
            "sources": ["src/login.rs"],
        }),
    )
    .await;
    let got = call(&a, "memfork_get", json!({"key": "shop:fact:auth"})).await;
    assert_eq!(got["fact"], "fresh", "{got}");

    // A missing file is stale.
    std::fs::remove_file(repo.join("src/login.rs")).unwrap();
    let got = call(&a, "memfork_get", json!({"key": "shop:fact:auth"})).await;
    assert_eq!(got["fact"], "stale");
    a.cancel().await.unwrap();

    // Freshness was counted.
    let out = sandbox
        .command()
        .current_dir(&repo)
        .args(["--json", "stats"])
        .output()
        .unwrap();
    let stats: Json = serde_json::from_slice(&out.stdout).unwrap();
    let c = &stats["stats"]["projects"]["shop"]["claude-code"];
    assert!(c["facts_fresh"].as_u64().unwrap() >= 2, "{stats}");
    assert!(c["facts_stale"].as_u64().unwrap() >= 2, "{stats}");
}

#[test]
fn the_command_line_works_the_board_finds_and_lists_facts() {
    let _turn = ONE_AT_A_TIME.blocking_lock();
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    std::fs::write(repo.join("src/cart.rs"), "fn total() {}").unwrap();
    let run = |args: &[&str]| {
        let out = sandbox
            .command()
            .current_dir(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert!(run(&["task", "add", "write the refunds endpoint"]).contains("added shop:task:1"));
    assert!(run(&["task", "claim", "1"]).contains("claimed shop:task:1"));
    let listed: Json = serde_json::from_str(&run(&["--json", "task", "list"])).unwrap();
    assert_eq!(listed["result"]["tasks"][0]["held_by"], "memfork-cli");
    assert!(run(&["task", "done", "1"]).contains("is done"));

    run(&[
        "put",
        "shop:fact:cart",
        "the cart total is in src/cart.rs",
        "--source",
        "src/cart.rs",
    ]);
    let facts = run(&["facts"]);
    assert!(
        facts.contains("shop:fact:cart") && facts.contains("fresh"),
        "{facts}"
    );
    std::fs::write(repo.join("src/cart.rs"), "fn total() -> u64 { 0 }").unwrap();
    let facts = run(&["facts"]);
    assert!(
        facts.contains("stale") && facts.contains("src/cart.rs"),
        "{facts}"
    );

    let found = run(&["find", "cart total"]);
    assert!(found.starts_with("shop:fact:cart"), "{found}");

    run(&["fork", "attempt"]);
    run(&["put", "x", "1", "--branch", "attempt"]);
    let gone = run(&[
        "discard",
        "attempt",
        "--lesson",
        "the cart cannot be cached",
    ]);
    assert!(gone.contains("lesson kept on main"), "{gone}");
    let graph = run(&["log", "--graph"]);
    assert!(
        graph.contains("lesson: the cart cannot be cached"),
        "{graph}"
    );
    let lessons = run(&["lessons"]);
    assert!(
        lessons.contains("the cart cannot be cached  (from attempt, by memfork-cli)"),
        "{lessons}"
    );
    let lessons: Json = serde_json::from_str(&run(&["--json", "lessons"])).unwrap();
    assert_eq!(
        lessons["lessons"][0]["key"], "shop:lesson:00000001",
        "{lessons}"
    );

    let stats: Json = serde_json::from_str(&run(&["--json", "stats"])).unwrap();
    let c = &stats["stats"]["projects"]["shop"]["memfork-cli"];
    assert_eq!(c["claims"], 1, "{stats}");
    assert_eq!(c["lessons_recorded"], 1, "{stats}");
    let text = run(&["stats"]);
    assert!(text.contains("tokens are an estimate"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_shows_claims_lessons_and_facts() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    std::fs::write(repo.join("src/a.rs"), "a").unwrap();
    let a = connect(&sandbox, "claude-code", &repo).await;
    call(
        &a,
        "memfork_task",
        json!({"action": "add", "id": "x", "title": "t"}),
    )
    .await;

    let mut watcher = sandbox
        .command()
        .args(["watch", "--json", "--count", "8"])
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
    let hello: Json =
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(30)).unwrap()).unwrap();
    assert_eq!(hello["kind"], "hello");

    call(&a, "memfork_task", json!({"action": "claim", "id": "x"})).await;
    call(&a, "memfork_task", json!({"action": "release", "id": "x"})).await;
    call(
        &a,
        "memfork_put",
        json!({"key": "shop:fact:a", "value": "v", "sources": ["src/a.rs"]}),
    )
    .await;
    call(&a, "memfork_get", json!({"key": "shop:fact:a"})).await;
    call(&a, "memfork_fork", json!({"name": "t"})).await;
    call(&a, "memfork_discard", json!({"name": "t", "lesson": "no"})).await;

    let mut seen = Vec::new();
    for _ in 0..8 {
        let e: Json =
            serde_json::from_str(&rx.recv_timeout(Duration::from_secs(30)).unwrap()).unwrap();
        seen.push(format!(
            "{} {}",
            e["operation"].as_str().unwrap_or("-"),
            e["detail"].as_str().unwrap_or("-")
        ));
    }
    let _ = watcher.wait();
    for wanted in [
        "claim -",
        "release -",
        "put -",
        "get -",
        "fact fresh",
        "fork -",
        "discard -",
        "lesson -",
    ] {
        assert!(
            seen.iter().any(|s| s == wanted),
            "no `{wanted}` in {seen:?}"
        );
    }
    assert!(
        !seen.iter().any(|s| s.starts_with("renew")),
        "a renewal reached the feed: {seen:?}"
    );
    a.cancel().await.unwrap();
}

// ---- determinism, in process --------------------------------------------------

fn args(v: Json) -> serde_json::Map<String, Json> {
    v.as_object().unwrap().clone()
}

fn session_in(root: &Path) -> memfork::tools::dispatch::Session {
    let session = memfork::tools::dispatch::Session::in_namespace(memfork_core::Db::new(), "shop")
        .in_project(root.to_path_buf());
    session.set_writer("claude-code");
    session
}

#[test]
fn the_same_fact_gets_the_same_commit_id_whatever_its_files_hold() {
    // The committed entry names its sources and nothing about them, so the id
    // depends on the key, the value and the paths only: not on the files, the
    // machine, or whether the files exist at all.
    let mut ids = Vec::new();
    for contents in [
        Some("pub fn login() {}\n"),
        Some("something else entirely"),
        None,
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        if let Some(text) = contents {
            std::fs::write(dir.path().join("src").join("login.rs"), text).unwrap();
        }
        let session = session_in(dir.path());
        let put = session
            .call(
                "memfork_put",
                &args(json!({
                    "key": "shop:fact:auth",
                    "value": "auth lives in src/login.rs",
                    "sources": ["src/login.rs", "src\\login.rs"],
                })),
            )
            .map(|_| ())
            .err();
        // The same file named two ways is one source, on every OS alike.
        assert!(put.is_none(), "{put:?}");
        let put = session
            .call(
                "memfork_put",
                &args(json!({
                    "key": "shop:fact:auth",
                    "value": "auth lives in src/login.rs",
                    "sources": ["src\\login.rs"],
                })),
            )
            .unwrap();
        assert_eq!(put["sources"], json!(["src/login.rs"]));
        ids.push(put["commit"].as_str().unwrap().to_owned());
    }
    assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
    // Pinned, so a change to how facts are committed is seen, on every OS.
    assert_eq!(ids[0], PINNED_FACT_COMMIT);
}

const PINNED_FACT_COMMIT: &str = "6489a99c31aff71d2a9a175ad260388c036a51a8719d5837b1f30f0a1fe1806e";

fn busy_session() -> memfork::tools::dispatch::Session {
    let dir = std::env::temp_dir();
    let session = session_in(&dir);
    for i in 0..40 {
        session
            .call(
                "memfork_put",
                &args(json!({
                    "key": format!("shop:decision:topic-{i:02}"),
                    "value": format!("decision {i}: use approach {} because of reason {}; {}", i % 7, i % 5, "detail ".repeat(i % 9)),
                })),
            )
            .unwrap();
    }
    for i in 0..12 {
        session
            .call(
                "memfork_task",
                &args(json!({"action": "add", "title": format!("task {i} for refunds and carts")})),
            )
            .unwrap();
    }
    session
        .call(
            "memfork_handoff",
            &args(json!({"summary": "half the refunds endpoint", "next": ["finish refunds", "tests"], "blockers": ["payment sandbox down"]})),
        )
        .unwrap();
    session
        .call("memfork_fork", &args(json!({"name": "attempt"})))
        .unwrap();
    session
        .call(
            "memfork_discard",
            &args(json!({"name": "attempt", "lesson": "refunds cannot share the cart lock"})),
        )
        .unwrap();
    session
}

#[test]
fn a_budgeted_briefing_never_exceeds_its_budget_and_is_the_same_every_time() {
    let mut digests = Vec::new();
    // 512 is below the smallest budget and is raised to it.
    for (budget, limit) in [
        (512_u64, 1024_u64),
        (1024, 1024),
        (1500, 1500),
        (4096, 4096),
        (8192, 8192),
    ] {
        let mut runs = Vec::new();
        for _ in 0..2 {
            let session = busy_session();
            let brief = session
                .call(
                    "memfork_resume",
                    &args(json!({"task": "refunds", "budget": budget})),
                )
                .unwrap();
            let text = brief.to_string();
            assert!(
                text.len() as u64 <= limit,
                "{} bytes over a budget of {limit}: {text}",
                text.len()
            );
            assert_eq!(brief["budget"]["limit_bytes"], limit);
            assert_eq!(
                brief["budget"]["bytes"],
                text.len() as u64,
                "the briefing misreports its size"
            );
            assert_eq!(
                brief["budget"]["approx_tokens"],
                (text.len() as u64).div_ceil(4)
            );
            // What matters survives even the smallest budget.
            assert_eq!(
                brief["latest_handoff"]["next"][0], "finish refunds",
                "{text}"
            );
            runs.push(text);
        }
        assert_eq!(
            runs[0], runs[1],
            "two runs gave different briefings at {budget}"
        );
        digests.push(blake3::hash(runs[0].as_bytes()).to_hex().to_string());
    }
    // Pinned, so the same operations give the same briefing on every OS.
    assert_eq!(
        blake3::hash(digests.concat().as_bytes()).to_hex().as_str(),
        PINNED_BRIEFINGS
    );
}

const PINNED_BRIEFINGS: &str = "86a7c1cb39bcde95e214d3f075f5208659792a45642af65a23e2839da8dede92";

#[test]
fn checking_facts_never_pushes_a_briefing_over_its_budget() {
    // Facts go out with their recorded hashes and come back with a verdict;
    // the briefing is sized for whichever is larger, and says its new size.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let session = session_in(dir.path());
    for i in 0..30 {
        let file = format!("src/module_with_a_long_name_{i:02}.rs");
        if i % 3 != 0 {
            std::fs::write(dir.path().join(&file), format!("fn f{i}() {{}}")).unwrap();
        }
        session
            .call(
                "memfork_put",
                &args(
                    json!({"key": format!("shop:fact:m{i:02}"), "value": "x", "sources": [file]}),
                ),
            )
            .unwrap();
        // Every other one goes stale.
        if i % 2 == 0 {
            std::fs::write(dir.path().join(&file), "changed").unwrap();
        }
    }
    session
        .call(
            "memfork_handoff",
            &args(json!({"summary": "s".repeat(4000), "next": ["n".repeat(900)]})),
        )
        .unwrap();
    for budget in [1024_u64, 1300, 2000, 3000] {
        let brief = session
            .call(
                "memfork_resume",
                &args(json!({"task": "t".repeat(3000), "budget": budget})),
            )
            .unwrap();
        let text = brief.to_string();
        assert!(
            text.len() as u64 <= budget,
            "{} over {budget}: {text}",
            text.len()
        );
        assert_eq!(brief["budget"]["bytes"], text.len() as u64);
    }
}
