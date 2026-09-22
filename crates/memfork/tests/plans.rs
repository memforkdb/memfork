//! Plans: tasks with dependencies, worked in order by whichever agents are
//! connected, with acceptance commands that run only from the repository's
//! own plan file.
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

static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// `(refused, body)`: a refusal is a tool error, or for arguments the tool
/// could not accept, an MCP invalid-params error, whose message is the body.
async fn call(client: &Client, tool: &str, args: Json) -> (bool, Json) {
    let Json::Object(map) = args else { panic!() };
    match client
        .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(map))
        .await
    {
        Ok(result) => (
            result.is_error == Some(true),
            result.structured_content.unwrap_or(Json::Null),
        ),
        Err(e) => (true, json!({ "error": e.to_string() })),
    }
}

async fn ok(client: &Client, tool: &str, args: Json) -> Json {
    let (is_error, body) = call(client, tool, args).await;
    assert!(!is_error, "`{tool}` returned an error: {body}");
    body
}

fn ready_ids(list: &Json) -> Vec<String> {
    list["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_work_a_three_task_chain_in_order() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let a = connect(&sandbox, "claude-code", &repo).await;
    let b = connect(&sandbox, "codex-mcp-client", &repo).await;

    let planned = ok(
        &a,
        "memfork_task",
        json!({"action": "plan", "tasks": [
            {"id": "schema", "title": "add the refunds table"},
            {"id": "api", "title": "refunds endpoint", "depends_on": ["schema"]},
            {"id": "docs", "title": "document refunds", "depends_on": ["api"]},
        ]}),
    )
    .await;
    assert_eq!(planned["ready"], json!(["schema"]), "{planned}");

    // B asks for work and gets the only thing that can be done.
    let ready = ok(
        &b,
        "memfork_task",
        json!({"action": "list", "status": "ready"}),
    )
    .await;
    assert_eq!(ready_ids(&ready), ["schema"]);
    let blocked = ok(
        &b,
        "memfork_task",
        json!({"action": "list", "status": "blocked"}),
    )
    .await;
    assert_eq!(ready_ids(&blocked), ["api", "docs"]);
    assert_eq!(blocked["tasks"][0]["blocked_by"], json!(["schema"]));

    // Claiming what is not ready yet is allowed, but the board says it waits.
    let brief = ok(&a, "memfork_resume", json!({})).await;
    let docs = brief["open_tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == "docs")
        .cloned()
        .unwrap();
    assert_eq!(docs["ready"], false, "{brief}");
    assert_eq!(docs["blocked_by"], json!(["api"]));

    assert_eq!(
        ok(
            &b,
            "memfork_task",
            json!({"action": "claim", "id": "schema"})
        )
        .await["claimed"],
        true
    );
    let done = ok(
        &b,
        "memfork_task",
        json!({"action": "done", "id": "schema"}),
    )
    .await;
    assert_eq!(done["now_ready"], json!(["api"]), "{done}");

    let ready = ok(
        &a,
        "memfork_task",
        json!({"action": "list", "status": "ready"}),
    )
    .await;
    assert_eq!(ready_ids(&ready), ["api"]);
    ok(&a, "memfork_task", json!({"action": "claim", "id": "api"})).await;
    let done = ok(&a, "memfork_task", json!({"action": "done", "id": "api"})).await;
    assert_eq!(done["now_ready"], json!(["docs"]));
    ok(&b, "memfork_task", json!({"action": "claim", "id": "docs"})).await;
    let done = ok(&b, "memfork_task", json!({"action": "done", "id": "docs"})).await;
    assert_eq!(done["now_ready"], json!([]));

    let all = ok(
        &a,
        "memfork_task",
        json!({"action": "list", "status": "done"}),
    )
    .await;
    assert_eq!(all["count"], 3);
    a.cancel().await.unwrap();
    b.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_acceptance_reopens_the_task_with_a_lesson_and_a_passing_one_closes_it() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    // `exit N` means the same to `sh -c` and `cmd /C`.
    std::fs::write(
        repo.join("memfork-plan.toml"),
        "[[task]]\nid = \"tests\"\ntitle = \"make the tests pass\"\naccept = \"exit 3\"\n",
    )
    .unwrap();
    let wrote = sandbox
        .command()
        .current_dir(&repo)
        .args(["plan", "write"])
        .output()
        .unwrap();
    assert!(
        wrote.status.success(),
        "{}",
        String::from_utf8_lossy(&wrote.stderr)
    );
    assert!(String::from_utf8_lossy(&wrote.stdout).contains("wrote 1 task"));

    let a = connect(&sandbox, "claude-code", &repo).await;
    ok(
        &a,
        "memfork_task",
        json!({"action": "claim", "id": "tests"}),
    )
    .await;
    let failed = ok(&a, "memfork_task", json!({"action": "done", "id": "tests"})).await;
    assert_eq!(failed["accepted"], false, "{failed}");
    assert_eq!(failed["exit_code"], 3);
    assert_eq!(failed["status"], "open");
    let lesson_key = failed["lesson"]["key"].as_str().unwrap().to_owned();
    assert!(
        failed["lesson"]["lesson"]
            .as_str()
            .unwrap()
            .contains("`exit 3` exited 3"),
        "{failed}"
    );

    // Reopened and unclaimed: anybody may take it; the lesson is in the briefing.
    let b = connect(&sandbox, "codex-mcp-client", &repo).await;
    let brief = ok(&b, "memfork_resume", json!({})).await;
    assert_eq!(brief["lessons"][0]["key"], lesson_key, "{brief}");
    assert_eq!(brief["lessons"][0]["task"], "tests");
    assert_eq!(
        ok(
            &b,
            "memfork_task",
            json!({"action": "claim", "id": "tests"})
        )
        .await["claimed"],
        true
    );

    // The fix lands in the plan file; writing it again replaces the open task.
    ok(
        &b,
        "memfork_task",
        json!({"action": "release", "id": "tests"}),
    )
    .await;
    std::fs::write(
        repo.join("memfork-plan.toml"),
        "[[task]]\nid = \"tests\"\ntitle = \"make the tests pass\"\naccept = \"exit 0\"\n",
    )
    .unwrap();
    let rewrote = sandbox
        .command()
        .current_dir(&repo)
        .args(["plan", "write"])
        .output()
        .unwrap();
    assert!(
        rewrote.status.success(),
        "{}",
        String::from_utf8_lossy(&rewrote.stderr)
    );
    ok(
        &b,
        "memfork_task",
        json!({"action": "claim", "id": "tests"}),
    )
    .await;
    let passed = ok(&b, "memfork_task", json!({"action": "done", "id": "tests"})).await;
    assert_eq!(passed["accepted"], true, "{passed}");
    assert_eq!(passed["status"], "done");
    a.cancel().await.unwrap();
    b.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_that_is_not_in_the_plan_file_never_runs() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let a = connect(&sandbox, "claude-code", &repo).await;
    // An agent writes a command into shared memory. It must not run for
    // whoever marks the task done.
    ok(
        &a,
        "memfork_task",
        json!({"action": "add", "id": "sneaky", "title": "t", "accept": "echo ran > ran.txt"}),
    )
    .await;
    let b = connect(&sandbox, "codex-mcp-client", &repo).await;
    ok(
        &b,
        "memfork_task",
        json!({"action": "claim", "id": "sneaky"}),
    )
    .await;
    let (is_error, body) = call(
        &b,
        "memfork_task",
        json!({"action": "done", "id": "sneaky"}),
    )
    .await;
    assert!(is_error, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("memfork-plan.toml"),
        "{body}"
    );
    assert!(!repo.join("ran.txt").exists(), "the command ran");
    // And the daemon will not take `done` without a result.
    let still = ok(
        &b,
        "memfork_task",
        json!({"action": "list", "status": "claimed"}),
    )
    .await;
    assert_eq!(ready_ids(&still), ["sneaky"]);

    // An empty command is no command: done needs nothing.
    std::fs::write(
        repo.join("memfork-plan.toml"),
        "[[task]]\nid = \"plain\"\ntitle = \"t\"\naccept = \"\"\n",
    )
    .unwrap();
    let wrote = sandbox
        .command()
        .current_dir(&repo)
        .args(["plan", "write"])
        .output()
        .unwrap();
    assert!(
        wrote.status.success(),
        "{}",
        String::from_utf8_lossy(&wrote.stderr)
    );
    ok(
        &b,
        "memfork_task",
        json!({"action": "claim", "id": "plain"}),
    )
    .await;
    let done = ok(&b, "memfork_task", json!({"action": "done", "id": "plain"})).await;
    assert_eq!(done["status"], "done", "{done}");
    assert!(done.get("accepted").is_none());
    a.cancel().await.unwrap();
    b.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cycles_and_other_projects_are_refused_when_the_plan_is_written() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let a = connect(&sandbox, "claude-code", &repo).await;
    let (is_error, body) = call(
        &a,
        "memfork_task",
        json!({"action": "plan", "tasks": [
            {"id": "a", "title": "a", "depends_on": ["c"]},
            {"id": "b", "title": "b", "depends_on": ["a"]},
            {"id": "c", "title": "c", "depends_on": ["b"]},
        ]}),
    )
    .await;
    assert!(is_error);
    assert!(body.to_string().contains("a -> c -> b -> a"), "{body}");
    let (is_error, body) = call(
        &a,
        "memfork_task",
        json!({"action": "plan", "tasks": [{"id": "a", "title": "a", "depends_on": ["web:login"]}]}),
    )
    .await;
    assert!(is_error);
    assert!(body.to_string().contains("another project"), "{body}");
    let listed = ok(
        &a,
        "memfork_task",
        json!({"action": "list", "status": "all"}),
    )
    .await;
    assert_eq!(
        listed["count"], 0,
        "a refused plan wrote something: {listed}"
    );
    a.cancel().await.unwrap();

    // `plan check` says the same without touching the board.
    std::fs::write(
        repo.join("memfork-plan.toml"),
        "[[task]]\nid = \"x\"\ntitle = \"x\"\ndepends_on = [\"y\"]\n\n[[task]]\nid = \"y\"\ntitle = \"y\"\ndepends_on = [\"x\"]\n",
    )
    .unwrap();
    let checked = sandbox
        .command()
        .current_dir(&repo)
        .args(["plan", "check"])
        .output()
        .unwrap();
    assert!(!checked.status.success());
    assert!(String::from_utf8_lossy(&checked.stderr).contains("cycle"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_holding_a_secret_is_refused_by_the_command_line_and_the_tool() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let token = format!("ghp_{}", "aB3dE6gH9j".repeat(4));
    std::fs::write(
        repo.join("memfork-plan.toml"),
        format!("[[task]]\nid = \"deploy\"\ntitle = \"deploy\"\ndetail = \"use {token}\"\n"),
    )
    .unwrap();
    for args in [vec!["plan", "write"], vec!["plan", "check"]] {
        let out = sandbox
            .command()
            .current_dir(&repo)
            .args(&args)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{args:?} accepted it");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("plan file memfork-plan.toml"), "{stderr}");
        assert!(
            stderr.contains("github-token") && stderr.contains("line 4"),
            "{stderr}"
        );
        assert!(!stderr.contains("aB3dE6"), "{stderr}");
    }
    let a = connect(&sandbox, "claude-code", &repo).await;
    let (is_error, body) = call(
        &a,
        "memfork_task",
        json!({"action": "plan", "tasks": [{"id": "deploy", "title": "deploy", "detail": format!("use {token}")}]}),
    )
    .await;
    assert!(is_error, "{body}");
    assert_eq!(body["secret"]["field"], "tasks[0].detail", "{body}");
    assert!(!body.to_string().contains("aB3dE6"));
    let listed = ok(
        &a,
        "memfork_task",
        json!({"action": "list", "status": "all"}),
    )
    .await;
    assert_eq!(listed["count"], 0);
    a.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_shows_a_task_becoming_ready_and_plan_show_groups_the_board() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let sandbox = Sandbox::new();
    let repo = repository(&sandbox);
    let a = connect(&sandbox, "claude-code", &repo).await;
    ok(
        &a,
        "memfork_task",
        json!({"action": "plan", "tasks": [
            {"id": "one", "title": "first"},
            {"id": "two", "title": "second", "depends_on": ["one"]},
        ]}),
    )
    .await;
    let shown = sandbox
        .command()
        .current_dir(&repo)
        .args(["plan", "show"])
        .output()
        .unwrap();
    let shown = String::from_utf8_lossy(&shown.stdout).into_owned();
    assert!(
        shown.contains("ready (1)") && shown.contains("blocked (1)"),
        "{shown}"
    );
    assert!(shown.contains("(waiting for one)"), "{shown}");

    let mut watcher = sandbox
        .command()
        .args(["watch", "--json", "--count", "4"])
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
    ok(&a, "memfork_task", json!({"action": "claim", "id": "one"})).await;
    ok(&a, "memfork_task", json!({"action": "done", "id": "one"})).await;
    let mut seen = Vec::new();
    // claim, the proxy reading the task for its acceptance command, done, ready
    for _ in 0..4 {
        let e: Json =
            serde_json::from_str(&rx.recv_timeout(Duration::from_secs(30)).unwrap()).unwrap();
        seen.push(format!(
            "{} {}",
            e["operation"].as_str().unwrap_or(""),
            e["key"].as_str().unwrap_or("")
        ));
    }
    let _ = watcher.wait();
    assert!(seen.contains(&"ready shop:task:two".to_owned()), "{seen:?}");
    a.cancel().await.unwrap();
}
