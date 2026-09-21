//! C11 and C12 — when a daemon starts, and when a proxy stops.
//!
//! Two things that are invisible until they go wrong, and then look like the
//! program leaking processes.
//!
//! **Nothing starts a daemon until a tool is called.** Clients launch
//! `memfork mcp` just to ask what it is — `claude mcp get` health-checks a
//! server by running it and shaking hands — so a proxy that reached for the
//! daemon during `initialize` would leave one running behind every probe.
//! `memfork stop; memfork doctor` did exactly that: doctor asked the client,
//! the client launched the server, and the server restarted the daemon that
//! had just been stopped.
//!
//! **A proxy exits with its client.** Otherwise every connection leaves a
//! process behind.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::io::Write as _;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use rmcp::model::CallToolRequestParams;
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::RoleClient;
use serde_json::Map;
use support::Sandbox;

async fn client(sandbox: &Sandbox) -> RunningService<RoleClient, ()> {
    let std_cmd = sandbox.command();
    let transport =
        TokioChildProcess::new(tokio::process::Command::from(std_cmd).configure(|cmd| {
            cmd.arg("mcp");
            cmd.stderr(Stdio::null());
        }))
        .expect("spawned `memfork mcp`");
    ().serve(transport)
        .await
        .expect("the MCP handshake completed")
}

async fn call_tool(service: &RunningService<RoleClient, ()>, name: &str) {
    service
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(Map::new()))
        .await
        .unwrap_or_else(|e| panic!("`{name}` failed: {e}"));
}

/// Wait for a child to exit, killing it if it will not.
fn exits_within(child: &mut Child, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// A `memfork mcp` with its pipes held open, and the handshake already sent.
fn start_proxy(sandbox: &Sandbox, then_call_a_tool: bool) -> Child {
    let mut child = sandbox
        .command()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawned");

    let stdin = child.stdin.as_mut().expect("stdin");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"p","version":"1"}}}}}}"#
    )
    .expect("written");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .expect("written");
    if then_call_a_tool {
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"memfork_branches","arguments":{{}}}}}}"#
        )
        .expect("written");
    }
    stdin.flush().expect("flushed");
    child
}

// ---- nothing starts a daemon until a tool is called -------------------------

#[tokio::test]
async fn c11_a_handshake_and_a_tool_list_start_no_daemon() {
    let sandbox = Sandbox::new();
    let before = sandbox.files();

    let service = client(&sandbox).await;
    let tools = service.list_all_tools().await.expect("tools listed");
    assert_eq!(tools.len(), 13, "the proxy listed the wrong tool surface");

    assert!(
        sandbox.owner().is_none(),
        "a handshake started a daemon: {:?}",
        sandbox.owner()
    );
    assert_eq!(
        sandbox.files(),
        before,
        "a handshake wrote files into the data directory"
    );
    assert!(!sandbox.data().join(memfork::persist::WAL_FILE).exists());

    service.cancel().await.expect("shut down");

    // And disconnecting leaves nothing either.
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        sandbox.owner().is_none(),
        "a daemon appeared after the client disconnected"
    );
    assert_eq!(sandbox.files(), before);
}

#[tokio::test]
async fn c11_the_daemon_starts_on_the_first_tool_call_and_not_before() {
    let sandbox = Sandbox::new();
    let service = client(&sandbox).await;

    service.list_all_tools().await.expect("tools listed");
    assert!(sandbox.owner().is_none(), "listing tools started a daemon");

    // The first call is the first thing that needs the store.
    call_tool(&service, "memfork_branches").await;
    let daemon = sandbox
        .wait_for_daemon(Duration::from_secs(20))
        .expect("the first tool call did not start a daemon");
    assert!(daemon.port.is_some());

    service.cancel().await.expect("shut down");
}

#[tokio::test]
async fn c11_a_proxy_lists_exactly_what_a_daemon_would() {
    // The proxy answers `tools/list` from its own copy of the registry rather
    // than asking the daemon, which is only safe while the two cannot differ.
    // So ask both and compare: the proxy over stdio, the daemon over its own
    // HTTP endpoint.
    let mut sandbox = Sandbox::new();

    fn fingerprint(tools: &[rmcp::model::Tool]) -> Vec<String> {
        let mut out: Vec<String> = tools
            .iter()
            .map(|t| {
                format!(
                    "{}|{}|{}",
                    t.name,
                    t.description.as_deref().unwrap_or(""),
                    serde_json::to_string(t.input_schema.as_ref()).unwrap_or_default()
                )
            })
            .collect();
        out.sort();
        out
    }

    let proxy = client(&sandbox).await;
    let from_proxy = fingerprint(&proxy.list_all_tools().await.expect("listed"));
    assert!(
        sandbox.owner().is_none(),
        "listing tools through the proxy started a daemon"
    );

    sandbox.spawn(&["serve", "--port", "0", "--idle-timeout", "60"]);
    // Longer than the other waits here on purpose: this one starts a daemon
    // by hand from a cold debug binary, and a loaded CI runner has taken more
    // than twenty seconds to get it listening.
    let endpoint = sandbox
        .wait_for_daemon(Duration::from_secs(90))
        .expect("the daemon did not start");
    let upstream = memfork::proxy::Upstream::connect(&endpoint)
        .await
        .expect("connected to the daemon");
    let from_daemon = fingerprint(&upstream.list_tools().await.expect("the daemon listed"));

    assert_eq!(
        from_proxy, from_daemon,
        "the tool list a proxy serves has drifted from the daemon's"
    );

    proxy.cancel().await.expect("shut down");
}

#[test]
fn c11_doctor_leaves_no_daemon_behind_when_a_client_probes_the_server() {
    // The whole sequence that produced the bug: doctor asks the client whether
    // MemFork is registered, the client answers by launching `memfork mcp` and
    // shaking hands, and that must not leave a daemon running.
    //
    // The client is a shim doing exactly what a health check does, so the test
    // does not depend on a real one being installed.
    let sandbox = Sandbox::new();

    let handshake = sandbox.root().join("handshake.jsonl");
    std::fs::write(
        &handshake,
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"1"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
            "\n",
        ),
    )
    .expect("handshake written");

    sandbox.add_probing_shim("claude", &handshake);

    let output = sandbox
        .command()
        .arg("doctor")
        .output()
        .expect("doctor ran");
    assert!(output.status.success(), "doctor failed: {output:?}");

    assert!(
        sandbox.shim_ran("claude"),
        "the shim was never invoked, so this proved nothing"
    );
    assert!(
        sandbox.owner().is_none(),
        "`memfork doctor` left a daemon running: {:?}",
        sandbox.owner()
    );
    assert!(
        !sandbox.data().join(memfork::persist::WAL_FILE).exists(),
        "`memfork doctor` created a store just by reporting on one"
    );
}

#[test]
fn c11_stop_then_doctor_leaves_nothing_running() {
    // The exact pair of commands that showed the bug.
    let mut sandbox = Sandbox::new();
    let handshake = sandbox.root().join("handshake.jsonl");
    std::fs::write(
        &handshake,
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"1"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
            "\n",
        ),
    )
    .expect("written");
    sandbox.add_probing_shim("claude", &handshake);

    // Something was running, as it would be in real use.
    sandbox.spawn(&["serve", "--port", "0", "--idle-timeout", "120"]);
    sandbox
        .wait_for_daemon(Duration::from_secs(20))
        .expect("the daemon did not start");

    sandbox.command().arg("stop").output().expect("stop ran");
    assert!(
        sandbox.wait_for_no_daemon(Duration::from_secs(10)),
        "stop did not stop it"
    );

    sandbox
        .command()
        .arg("doctor")
        .output()
        .expect("doctor ran");
    assert!(
        sandbox.owner().is_none(),
        "`memfork stop` then `memfork doctor` left a daemon running again"
    );
}

// ---- proxies do not accumulate ---------------------------------------------

#[test]
fn c12_a_proxy_exits_when_its_client_closes_stdin() {
    // Checked both before and after a daemon connection, because the two take
    // different paths out: one has an upstream to drop, the other does not.
    for (label, connected) in [("before connecting", false), ("while connected", true)] {
        let sandbox = Sandbox::new();
        let mut child = start_proxy(&sandbox, connected);

        if connected {
            sandbox
                .wait_for_daemon(Duration::from_secs(20))
                .expect("the tool call did not start a daemon");
        }

        // Closing stdin is how a client says it is finished.
        drop(child.stdin.take());
        assert!(
            exits_within(&mut child, Duration::from_secs(15)),
            "the proxy did not exit when its client closed stdin ({label})"
        );
    }
}

#[test]
fn c12_a_proxy_exits_when_its_client_dies_outright() {
    // Not a polite close: every pipe the client held goes at once, which is
    // what a killed client leaves behind.
    let sandbox = Sandbox::new();
    let mut child = start_proxy(&sandbox, false);

    drop(child.stdin.take());
    drop(child.stdout.take());

    assert!(
        exits_within(&mut child, Duration::from_secs(15)),
        "the proxy outlived a client that died"
    );
}

#[test]
fn c12_a_client_reading_to_the_end_is_not_left_waiting() {
    // Exiting is not enough: the client's pipe has to close with the client.
    //
    // On Windows a new process inherits every inheritable handle its parent
    // holds, so the daemon inherited the client's stdout and held it open for
    // its whole life. `memfork mcp` exited, and a client reading to the end of
    // the pipe waited anyway — for ten minutes, in the suite that found this.
    // Unix never had the problem, which is exactly why it needed a test.
    let sandbox = Sandbox::new();

    let mut child = sandbox
        .command()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawned");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"p","version":"1"}}}}}}"#
        )
        .expect("written");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
        )
        .expect("written");
        // A tool call, so a daemon really is started.
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"memfork_branches","arguments":{{}}}}}}"#
        )
        .expect("written");
        stdin.flush().expect("flushed");
    }
    drop(child.stdin.take());

    sandbox
        .wait_for_daemon(Duration::from_secs(20))
        .expect("the tool call did not start a daemon");

    // Read the whole of stdout, from another thread so the test can give up
    // rather than hang the suite if the pipe is held open again.
    let mut stdout = child.stdout.take().expect("stdout");
    let (done, waiting) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut said = String::new();
        let outcome = std::io::Read::read_to_string(&mut stdout, &mut said);
        let _ = done.send(outcome.map(|_| said));
    });

    let said = waiting
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| {
            let _ = child.kill();
            panic!("the client's pipe never closed: something the proxy started is holding it open")
        })
        .expect("stdout was readable");

    assert!(
        said.contains("branches"),
        "the tool call did not answer: {said}"
    );
    let _ = child.wait();
}

#[test]
fn c12_many_short_sessions_leave_nothing_running() {
    // The accumulation this guards against only shows up over several
    // connections, which is how a client that health-checks on every start
    // would use it.
    let sandbox = Sandbox::new();
    let before = sandbox.files();

    for round in 0..5 {
        let mut child = start_proxy(&sandbox, false);
        drop(child.stdin.take());
        assert!(
            exits_within(&mut child, Duration::from_secs(15)),
            "round {round}: the proxy did not exit"
        );
    }

    assert!(
        sandbox.owner().is_none(),
        "five handshakes left a daemon running"
    );
    assert_eq!(
        sandbox.files(),
        before,
        "five handshakes wrote something to the data directory"
    );
}
