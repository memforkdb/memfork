//! `memfork demo` plays the whole session on a store of its own, touches
//! nothing else, and removes what it made.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::time::Duration;

use support::Sandbox;

#[test]
fn the_demo_plays_to_the_end_on_its_own_store_and_cleans_up() {
    let sandbox = Sandbox::new();
    let before = sandbox.files();
    let output = sandbox
        .command()
        .args(["demo", "--no-open", "--fast", "--exit"])
        .output()
        .expect("ran");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // The whole script, narrated.
    for step in [
        "1/9",
        "Agents are nodes too",
        "A decision, and the fact it rests on",
        "Coordination without an orchestrator",
        "A dead end becomes a lesson",
        "The handoff",
        "Another client picks it up",
        "The engine notices",
        "A second session of the same client",
        "9/9",
        "nothing was kept",
    ] {
        assert!(stdout.contains(step), "no `{step}` in:\n{stdout}");
    }
    // The page's address, with the read token where a browser keeps it.
    let url = stdout
        .lines()
        .find(|l| l.starts_with("http://127.0.0.1:"))
        .expect("no address printed");
    assert!(url.contains("/brain#t="), "{url}");
    assert!(!url.contains('?'), "{url}");

    // Its own store, gone afterwards; the sandbox's, untouched.
    let dir = stdout
        .lines()
        .find(|l| l.contains("a throwaway store in "))
        .and_then(|l| l.split("a throwaway store in ").nth(1))
        .map(|rest| rest.split(';').next().unwrap_or(rest).trim().to_owned())
        .expect("the demo did not say where its store is");
    assert!(
        !std::path::Path::new(&dir).exists(),
        "the demo's directory {dir} is still there"
    );
    assert!(
        sandbox.owner().is_none(),
        "the demo started a daemon on the sandbox's store"
    );
    assert_eq!(
        sandbox.files(),
        before,
        "the demo wrote into the sandbox's store"
    );
    assert!(
        sandbox.wait_for_no_daemon(Duration::from_secs(5)),
        "a daemon owns the sandbox's store after the demo"
    );
}

#[test]
fn the_demo_names_the_agents_it_is_told_to_and_refuses_one_name() {
    let sandbox = Sandbox::new();
    let output = sandbox
        .command()
        .args([
            "demo",
            "--no-open",
            "--fast",
            "--exit",
            "--agents",
            "Ada, Grace",
        ])
        .output()
        .expect("ran");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let one = sandbox
        .command()
        .args(["demo", "--no-open", "--fast", "--exit", "--agents", "Ada"])
        .output()
        .expect("ran");
    assert!(!one.status.success());
    assert!(
        String::from_utf8_lossy(&one.stderr).contains("two names"),
        "{}",
        String::from_utf8_lossy(&one.stderr)
    );
}

#[test]
fn the_machine_policy_can_switch_the_demo_off_with_the_brain() {
    let sandbox = Sandbox::new();
    let policy = sandbox.root().join("policy.toml");
    std::fs::write(&policy, "brain = false\n").unwrap();
    let output = sandbox
        .command()
        .env(memfork::policy::EXTRA_ENV, &policy)
        .args(["demo", "--no-open", "--fast", "--exit"])
        .output()
        .expect("ran");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("switched off by the machine policy"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// What the demo's store holds once the script has played: the page's own
/// summary, read with the token the demo printed.
#[test]
fn the_demo_leaves_the_engine_with_everything_the_page_shows() {
    use http_body_util::BodyExt;

    let sandbox = Sandbox::new();
    let log = sandbox.root().join("demo.out");
    let child = sandbox
        .command()
        .args(["demo", "--no-open", "--fast"])
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(&log).unwrap())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawned");
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Kill(child);

    // Wait for the script to reach its end and the page to be open.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut text = String::new();
    while std::time::Instant::now() < deadline {
        text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains("press Ctrl+C") {
            break;
        }
        if let Ok(Some(status)) = child.0.try_wait() {
            panic!("the demo ended early ({status}):\n{text}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        text.contains("press Ctrl+C"),
        "the demo did not finish its script:\n{text}"
    );
    let url = text
        .lines()
        .find(|l| l.starts_with("http://127.0.0.1:"))
        .expect("no address");
    let port: u16 = url
        .trim_start_matches("http://127.0.0.1:")
        .split('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let token = url.split("#t=").nth(1).unwrap().to_owned();
    let store = text
        .lines()
        .find(|l| l.contains("a throwaway store in "))
        .and_then(|l| l.split("a throwaway store in ").nth(1))
        .map(|rest| rest.split(';').next().unwrap_or(rest).trim().to_owned())
        .map(|dir| std::path::PathBuf::from(dir).join("store"))
        .expect("no store named");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let fetch = |route: &str| -> serde_json::Value {
        runtime.block_on(async {
            let client =
                hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                    .build_http::<http_body_util::Full<hyper::body::Bytes>>();
            let request = hyper::Request::builder()
                .uri(format!("http://127.0.0.1:{port}/brain/{route}?ns=shop"))
                .header(hyper::header::AUTHORIZATION, format!("Bearer {token}"))
                .body(http_body_util::Full::new(hyper::body::Bytes::new()))
                .unwrap();
            let response = client.request(request).await.expect("answered");
            assert_eq!(response.status(), 200);
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            serde_json::from_slice(&bytes).unwrap()
        })
    };
    let summary = fetch("summary");
    let attention = fetch("attention");

    // The whole story is in the engine, not in the narration.
    let s = &summary;
    assert_eq!(
        s["handoffs"].as_array().unwrap().len(),
        1,
        "{}",
        s["handoffs"]
    );
    assert!(
        s["handoffs"][0]["picked_up"].is_object(),
        "{}",
        s["handoffs"][0]
    );
    assert_eq!(
        s["briefings"].as_array().unwrap().len(),
        3,
        "{}",
        s["briefings"]
    );
    assert_eq!(s["facts"][0]["state"], "stale", "{}", s["facts"]);
    assert_eq!(s["lessons"].as_array().unwrap().len(), 1);
    assert!(
        s["lessons"][0]["served"].as_u64().unwrap() >= 1,
        "{}",
        s["lessons"]
    );
    let tasks = s["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 6);
    let status = |id: &str| tasks.iter().find(|t| t["id"] == id).unwrap()["status"].clone();
    assert_eq!(status("schema"), "done");
    assert_eq!(status("checkout"), "done");
    assert_eq!(status("refunds"), "claimed");
    let kinds: Vec<&str> = attention["attention"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"stale_fact"), "{kinds:?}");
    assert!(kinds.contains(&"conflicting_decisions"), "{kinds:?}");
    assert!(
        s["headline"]["twice"].as_u64().unwrap() >= 3,
        "{}",
        s["headline"]
    );
    assert!(s["branches"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b == "try-eu"));

    // The demo is killed rather than interrupted, so its own cleanup does
    // not run; the daemon is stopped and the directory removed here.
    drop(child);
    let _ = memfork::daemon::stop(&store);
    if let Some(parent) = store.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}
