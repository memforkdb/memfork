//! Duplicates and contradictions are flagged where agents and people will see
//! them, and never resolved by MemFork.
//!
//! Sandboxed like every suite: temporary data directory, home and PATH, the
//! guard, and a repository made for the test.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

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
async fn duplicates_and_contradictions_are_flagged_everywhere_and_fixed_nowhere() {
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let a = connect(&sandbox, "claude-code", &repo).await;
    let b = connect(&sandbox, "codex-mcp-client", &repo).await;
    call(
        &a,
        "memfork_put",
        json!({"key": "shop:decision:db", "value": "postgres"}),
    )
    .await;

    let mut watcher = sandbox
        .command()
        .args(["watch", "--json"])
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

    // B decides the other way on a fork: told at once, in the answer.
    call(&b, "memfork_fork", json!({"name": "try-sqlite"})).await;
    let put = call(
        &b,
        "memfork_put",
        json!({"key": "shop:decision:db", "value": "sqlite"}),
    )
    .await;
    assert_eq!(put["conflicts"][0]["branch"], "main", "{put}");
    assert_eq!(put["conflicts"][0]["by"], "claude-code");

    // The same value under a near-identical key.
    call(
        &a,
        "memfork_put",
        json!({"key": "shop:note:deploy-steps", "value": "make deploy"}),
    )
    .await;
    let dup = call(
        &a,
        "memfork_put",
        json!({"key": "shop:note:deploy_step", "value": "make deploy"}),
    )
    .await;
    assert_eq!(dup["similar"], json!(["shop:note:deploy-steps"]), "{dup}");

    // Two facts from the same file that disagree.
    std::fs::write(repo.join("src/auth.rs"), "fn login() {}").unwrap();
    call(&a, "memfork_put", json!({"key": "shop:fact:login", "value": "login is in auth.rs", "sources": ["src/auth.rs"]})).await;
    call(&a, "memfork_put", json!({"key": "shop:fact:login-old", "value": "login is in session.rs", "sources": ["src/auth.rs"]})).await;

    let mut flagged = Vec::new();
    while flagged.len() < 2 {
        let line = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the feed went quiet");
        let e: Json = serde_json::from_str(&line).unwrap();
        if e["operation"] == "flag" {
            flagged.push(e["detail"].as_str().unwrap_or("").to_owned());
        }
    }
    let _ = watcher.kill();
    let _ = watcher.wait();
    assert_eq!(
        flagged,
        [
            "decided differently on main",
            "same value as shop:note:deploy-steps"
        ]
    );

    // The briefing lists them for anybody resuming.
    let brief = call(&a, "memfork_resume", json!({})).await;
    let kinds: Vec<&str> = brief["flags"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["conflicting_decisions", "duplicate", "facts_disagree"],
        "{brief}"
    );

    // The command line and doctor say the same.
    let flags = sandbox
        .command()
        .current_dir(&repo)
        .args(["--json", "flags"])
        .output()
        .unwrap();
    let flags: Json = serde_json::from_slice(&flags.stdout).unwrap();
    assert_eq!(flags["flags"].as_array().unwrap().len(), 3, "{flags}");
    let text = sandbox
        .command()
        .current_dir(&repo)
        .args(["flags"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&text.stdout).into_owned();
    assert!(text.contains("shop:decision:db is decided differently on main (by claude-code) and try-sqlite (by codex-mcp-client)"), "{text}");
    let doctor = sandbox
        .command()
        .current_dir(&repo)
        .args(["--json", "doctor"])
        .output()
        .unwrap();
    let doctor: Json = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(
        doctor["flags"],
        json!({"namespace": "shop", "count": 3}),
        "{doctor}"
    );

    // And nothing was resolved.
    assert_eq!(
        call(&a, "memfork_get", json!({"key": "shop:decision:db"})).await["value"],
        "postgres"
    );
    assert_eq!(
        call(&b, "memfork_get", json!({"key": "shop:decision:db"})).await["value"],
        "sqlite"
    );
    assert_eq!(
        call(&a, "memfork_get", json!({"key": "shop:note:deploy_step"})).await["found"],
        true
    );
    a.cancel().await.unwrap();
    b.cancel().await.unwrap();
}
