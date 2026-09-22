//! The Brain's HTTP surface, against a real daemon: the port is the system's
//! choice, every data route needs a token, the read token reads and nothing
//! else, a `Host` that is not this listener is refused, no route changes
//! memory, and the page's files reach nowhere but this daemon.
//!
//! The page itself runs in a browser this suite does not have. What it does
//! with these answers is tested under Node in `src/brain/page/tests`, and
//! what only a browser can show is in the manual script in the README.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::time::Duration;

use http_body_util::BodyExt;
use hyper::body::Bytes;
use hyper::{HeaderMap, Method, StatusCode};
use memfork::persist::Endpoint;
use serde_json::Value as Json;
use support::Sandbox;

/// One answer from the daemon.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Answer {
    fn json(&self) -> Json {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

/// A request to the daemon, with whatever `Host` and token the test wants.
fn ask(port: u16, method: Method, path: &str, host: Option<&str>, token: Option<&str>) -> Answer {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move {
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build_http::<http_body_util::Full<Bytes>>();
        let mut request = hyper::Request::builder()
            .method(method)
            .uri(format!("http://127.0.0.1:{port}{path}"));
        if let Some(host) = host {
            request = request.header(hyper::header::HOST, host);
        }
        if let Some(token) = token {
            request = request.header(hyper::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let request = request
            .body(http_body_util::Full::new(Bytes::from("{}")))
            .expect("request");
        let response = tokio::time::timeout(Duration::from_secs(10), client.request(request))
            .await
            .expect("answered in time")
            .expect("answered");
        let status = response.status();
        let headers = response.headers().clone();
        // The event stream never ends on its own; one frame is enough.
        let body = if path == memfork::serve::EVENTS_PATH {
            let mut body = response.into_body();
            let frame = tokio::time::timeout(Duration::from_secs(10), body.frame())
                .await
                .expect("a first line in time")
                .expect("a frame")
                .expect("readable");
            String::from_utf8_lossy(frame.data_ref().expect("data")).into_owned()
        } else {
            let bytes = response
                .into_body()
                .collect()
                .await
                .expect("body")
                .to_bytes();
            String::from_utf8_lossy(&bytes).into_owned()
        };
        Answer {
            status,
            headers,
            body,
        }
    })
}

/// Start a daemon the way a client does, through a first operation, so its
/// output goes where a real daemon's goes.
fn started(sandbox: &Sandbox) -> Endpoint {
    let output = sandbox
        .command()
        .args(["put", "shop:decision:first", "the first thing"])
        .output()
        .expect("ran");
    assert!(
        output.status.success(),
        "the first put failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let endpoint = sandbox
        .wait_for_daemon(Duration::from_secs(20))
        .expect("a daemon");
    assert!(endpoint.read_token.is_some(), "no read token was published");
    endpoint
}

fn tokens(endpoint: &Endpoint) -> (u16, String, String) {
    (
        endpoint.port.expect("port"),
        endpoint.token.clone().expect("token"),
        endpoint.read_token.clone().expect("read token"),
    )
}

fn stop(sandbox: &Sandbox) {
    let output = sandbox.command().arg("stop").output().expect("ran");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(sandbox.wait_for_no_daemon(Duration::from_secs(10)));
}

#[test]
fn the_port_is_the_systems_choice_and_changes_across_starts() {
    let sandbox = Sandbox::new();
    let mut ports = Vec::new();
    for _ in 0..3 {
        let endpoint = started(&sandbox);
        ports.push(endpoint.port.expect("port"));
        stop(&sandbox);
    }
    assert!(
        ports.iter().any(|p| *p != ports[0]),
        "three starts bound the same port {ports:?}; the port is not the system's choice"
    );
    assert!(ports.iter().all(|p| *p != 0));
}

#[test]
fn the_page_files_need_no_token_and_every_data_route_does() {
    let sandbox = Sandbox::new();
    let (port, full, read) = tokens(&started(&sandbox));

    for (path, kind) in [
        ("/brain", "text/html"),
        ("/brain/app.js", "text/javascript"),
        ("/brain/app.css", "text/css"),
    ] {
        let a = ask(port, Method::GET, path, None, None);
        assert_eq!(a.status, StatusCode::OK, "{path}");
        assert!(
            a.header("content-type").unwrap().starts_with(kind),
            "{path}"
        );
        assert_eq!(
            a.header("content-security-policy"),
            Some(memfork::brain::CONTENT_SECURITY_POLICY),
            "{path}"
        );
        assert!(
            !a.body.contains(&full) && !a.body.contains(&read),
            "{path} carries a token"
        );
    }
    assert_eq!(
        ask(port, Method::POST, "/brain", None, None).status,
        StatusCode::METHOD_NOT_ALLOWED
    );

    for route in memfork::brain::ROUTES {
        let path = format!("/brain/{route}");
        assert_eq!(
            ask(port, Method::GET, &path, None, None).status,
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
        assert_eq!(
            ask(port, Method::GET, &path, None, Some("not-a-token")).status,
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
        let with_read = ask(port, Method::GET, &path, None, Some(&read));
        assert_ne!(
            with_read.status,
            StatusCode::UNAUTHORIZED,
            "{path} refused the read token"
        );
        assert_ne!(
            with_read.status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{path} refused GET"
        );
        let with_full = ask(port, Method::GET, &path, None, Some(&full));
        assert_ne!(
            with_full.status,
            StatusCode::UNAUTHORIZED,
            "{path} refused the daemon's token"
        );
    }
    let summary = ask(port, Method::GET, "/brain/summary", None, Some(&read));
    assert_eq!(summary.status, StatusCode::OK);
    let json = summary.json();
    assert_eq!(json["port"], port);
    assert_eq!(json["read_only"], true);
    assert_eq!(json["namespace"], "shop");
    assert_eq!(json["branch"], "main");
    assert_eq!(json["namespaces"], serde_json::json!(["shop"]));

    // An unknown route and a bad branch are answered, never served.
    assert_eq!(
        ask(port, Method::GET, "/brain/nothing", None, Some(&read)).status,
        StatusCode::NOT_FOUND
    );
    let bad = ask(
        port,
        Method::GET,
        "/brain/summary?branch=nope",
        None,
        Some(&read),
    );
    assert_eq!(bad.status, StatusCode::NOT_FOUND);
    assert!(bad.json()["error"].as_str().unwrap().contains("nope"));
}

#[test]
fn the_read_token_reads_and_nothing_else() {
    let sandbox = Sandbox::new();
    let (port, full, read) = tokens(&started(&sandbox));

    // The routes that write, or stop: refused to the read token, one by one.
    for path in [
        memfork::serve::CLI_PATH,
        memfork::serve::MCP_PATH,
        memfork::serve::REPORT_PATH,
        memfork::serve::SHUTDOWN_PATH,
    ] {
        let a = ask(port, Method::POST, path, None, Some(&read));
        assert_eq!(
            a.status,
            StatusCode::UNAUTHORIZED,
            "{path} accepted the read token"
        );
        assert!(a.body.contains("only reads"), "{path}: {}", a.body);
    }
    assert!(
        sandbox.owner().is_some_and(|o| o.port == Some(port)),
        "a shutdown with the read token stopped the daemon"
    );

    // What reads, it may read.
    let events = ask(
        port,
        Method::GET,
        memfork::serve::EVENTS_PATH,
        None,
        Some(&read),
    );
    assert_eq!(events.status, StatusCode::OK);
    let hello = events.json();
    assert_eq!(hello["kind"], "hello");
    assert_eq!(hello["port"], port);

    // And no Brain route answers anything but GET, with either token.
    for route in memfork::brain::ROUTES {
        for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            for token in [&full, &read] {
                let a = ask(
                    port,
                    method.clone(),
                    &format!("/brain/{route}"),
                    None,
                    Some(token),
                );
                assert_eq!(
                    a.status,
                    StatusCode::METHOD_NOT_ALLOWED,
                    "{method} /brain/{route}"
                );
            }
        }
    }
}

#[test]
fn a_host_that_is_not_this_listener_is_refused_on_every_route() {
    let sandbox = Sandbox::new();
    let (port, full, _) = tokens(&started(&sandbox));
    for path in [
        "/brain/summary",
        "/brain",
        memfork::serve::CLI_PATH,
        memfork::serve::EVENTS_PATH,
    ] {
        for host in [
            "example.com",
            &format!("example.com:{port}"),
            "127.0.0.1:1",
            "localhost",
        ] {
            let a = ask(port, Method::GET, path, Some(host), Some(&full));
            assert_eq!(
                a.status,
                StatusCode::FORBIDDEN,
                "{path} answered Host {host}"
            );
            assert!(a.body.contains("127.0.0.1"), "{}", a.body);
        }
    }
    for host in [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("LOCALHOST:{port}"),
    ] {
        let a = ask(
            port,
            Method::GET,
            "/brain/summary",
            Some(&host),
            Some(&full),
        );
        assert_eq!(a.status, StatusCode::OK, "Host {host} was refused");
    }
}

#[test]
fn nothing_the_page_can_reach_changes_memory() {
    let sandbox = Sandbox::new();
    let (port, _, read) = tokens(&started(&sandbox));
    let log_before = sandbox
        .command()
        .args(["--json", "log"])
        .output()
        .expect("ran")
        .stdout;
    let branches_before = sandbox
        .command()
        .args(["--json", "branches"])
        .output()
        .expect("ran")
        .stdout;

    for route in memfork::brain::ROUTES {
        for method in [
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
        ] {
            for query in [
                "",
                "?branch=main&ns=shop",
                "?branch=x",
                "?ns=../etc",
                "?at=999999",
            ] {
                let _ = ask(
                    port,
                    method.clone(),
                    &format!("/brain/{route}{query}"),
                    None,
                    Some(&read),
                );
            }
        }
    }
    let _ = ask(port, Method::GET, "/brain", None, None);

    let log_after = sandbox
        .command()
        .args(["--json", "log"])
        .output()
        .expect("ran")
        .stdout;
    let branches_after = sandbox
        .command()
        .args(["--json", "branches"])
        .output()
        .expect("ran")
        .stdout;
    assert_eq!(
        String::from_utf8_lossy(&log_before),
        String::from_utf8_lossy(&log_after),
        "a Brain route changed the log"
    );
    assert_eq!(
        branches_before, branches_after,
        "a Brain route changed the branches"
    );
    assert!(
        sandbox.owner().is_some_and(|o| o.port == Some(port)),
        "the daemon is gone"
    );
}

#[test]
fn the_headers_keep_the_browser_at_home_and_the_tokens_stay_out_of_sight() {
    let sandbox = Sandbox::new();
    let endpoint = started(&sandbox);
    let (port, full, read) = tokens(&endpoint);
    for (path, token) in [
        ("/brain", None),
        ("/brain/app.js", None),
        ("/brain/app.css", None),
        ("/brain/summary", Some(read.as_str())),
        ("/brain/summary?branch=none", Some(read.as_str())),
        ("/brain/none", Some(read.as_str())),
        ("/brain/summary", None),
    ] {
        let a = ask(port, Method::GET, path, None, token);
        assert!(
            a.header("access-control-allow-origin").is_none(),
            "{path} has a CORS header"
        );
        assert!(a.header("set-cookie").is_none(), "{path} sets a cookie");
        assert!(
            !a.body.contains(&full) && !a.body.contains(&read),
            "{path} shows a token"
        );
        if a.status != StatusCode::UNAUTHORIZED {
            assert_eq!(
                a.header("x-content-type-options"),
                Some("nosniff"),
                "{path}"
            );
            assert_eq!(a.header("referrer-policy"), Some("no-referrer"), "{path}");
            assert_eq!(a.header("cache-control"), Some("no-store"), "{path}");
        }
    }
    // The daemon's own output, where a real daemon's goes.
    let log =
        std::fs::read_to_string(sandbox.data().join("memfork-daemon.log")).unwrap_or_default();
    assert!(
        !log.contains(&full) && !log.contains(&read),
        "the daemon logged a token"
    );
    // The endpoint file is the one place the tokens are, and it is the
    // daemon's, not the page's.
    let file = std::fs::read_to_string(sandbox.data().join(memfork::persist::lock::ENDPOINT_FILE))
        .unwrap();
    assert!(file.contains(&read));
}

#[test]
fn the_page_files_reach_nowhere_but_this_daemon() {
    let files = [
        ("brain.html", memfork::brain::PAGE_HTML),
        ("brain.css", memfork::brain::PAGE_CSS),
        ("brain.js", memfork::brain::PAGE_JS),
    ];
    // Anything that would fetch, run or store outside the page's own files.
    let forbidden = [
        "http://",
        "https://",
        "url(",
        "@import",
        "javascript:",
        "EventSource",
        "localStorage",
        "sessionStorage",
        "indexedDB",
        "document.cookie",
        "importScripts",
        "new Worker",
        "eval(",
        "new Function",
        "innerHTML =",
        "outerHTML",
        "document.write",
        "srcdoc",
        "<iframe",
        "<object",
        "<embed",
        "<form",
        "<base",
        "<meta http-equiv",
    ];
    for (name, text) in files {
        for word in forbidden {
            assert!(!text.contains(word), "{name} contains `{word}`");
        }
        for (number, line) in text.lines().enumerate() {
            let line_number = number + 1;
            // An inline handler or an inline style attribute would be refused
            // by the policy anyway; there must be none to refuse.
            assert!(
                !line.contains(" on") || !regex_like_handler(line),
                "{name}:{line_number} has an inline handler"
            );
            assert!(
                !line.contains(" style=\""),
                "{name}:{line_number} has an inline style"
            );
        }
    }
    // What the markup loads, it loads from here.
    for (attr, expected) in [("href=\"", "/brain/app.css"), ("src=\"", "/brain/app.js")] {
        for piece in memfork::brain::PAGE_HTML.split(attr).skip(1) {
            let target = piece.split('"').next().unwrap();
            assert!(
                target == expected || target.starts_with('#'),
                "the page loads {target}"
            );
        }
    }
    assert!(
        !memfork::brain::PAGE_HTML.contains("<script>"),
        "the page has an inline script"
    );
    assert!(
        !memfork::brain::PAGE_HTML.contains("<style"),
        "the page has an inline style sheet"
    );
    // The script is a stranger to every place the token could leak to: it
    // reads the fragment, sends a header, and that is all.
    assert!(memfork::brain::PAGE_JS.contains("location.hash"));
    assert!(memfork::brain::PAGE_JS.contains("authorization: `Bearer"));
    assert!(!memfork::brain::PAGE_JS.contains("?token="));
    assert!(!memfork::brain::PAGE_JS.contains("&token="));
}

/// Whether a line has an `onsomething="` attribute.
fn regex_like_handler(line: &str) -> bool {
    line.split(" on").skip(1).any(|rest| {
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        !name.is_empty() && rest[name.len()..].starts_with('=')
    })
}

// ---- the graph ---------------------------------------------------------------

/// A store with one of everything, written the way agents write it.
fn furnish(sandbox: &Sandbox) {
    let script = [
        vec![
            "put",
            "shop:fact:auth-entry",
            "auth lives in src/auth",
            "--source",
            "src/auth/login.rs",
        ],
        vec![
            "put",
            "shop:decision:payments",
            "hosted checkout; rests on fact:auth-entry",
        ],
        vec!["task", "add", "design the schema", "--id", "schema"],
        vec![
            "task",
            "add",
            "checkout page",
            "--id",
            "checkout",
            "--depends-on",
            "schema",
        ],
        vec!["fork", "try-refunds"],
        vec![
            "put",
            "--branch",
            "try-refunds",
            "shop:note:cards",
            "store the cards",
        ],
        vec![
            "discard",
            "try-refunds",
            "--lesson",
            "storing cards breaks decision:payments",
        ],
    ];
    std::fs::create_dir_all(sandbox.root().join("src").join("auth")).unwrap();
    std::fs::write(
        sandbox.root().join("src").join("auth").join("login.rs"),
        "fn login() {}\n",
    )
    .unwrap();
    for line in script {
        let output = sandbox
            .command()
            .env(memfork::namespace::NAMESPACE_ENV, "shop")
            .args(&line)
            .output()
            .expect("ran");
        assert!(
            output.status.success(),
            "`memfork {}` failed: {}",
            line.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn graph_of(sandbox: &Sandbox, query: &str) -> Json {
    let (port, _, read) = tokens(
        &sandbox
            .wait_for_daemon(Duration::from_secs(20))
            .expect("a daemon"),
    );
    let a = ask(
        port,
        Method::GET,
        &format!("/brain/graph{query}"),
        None,
        Some(&read),
    );
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    a.json()
}

#[test]
fn the_graph_holds_the_relations_the_engine_knows_and_is_the_same_after_a_restart() {
    let sandbox = Sandbox::new();
    started(&sandbox);
    furnish(&sandbox);
    let first = graph_of(&sandbox, "?ns=shop");
    assert_eq!(first["columns"].as_array().unwrap().len(), 7);
    let ids: Vec<&str> = first["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n[0].as_str().unwrap())
        .collect();
    for expected in [
        "shop:fact:auth-entry",
        "shop:decision:payments",
        "shop:task:schema",
        "shop:task:checkout",
        "shop:lesson:00000001",
        "file:src/auth/login.rs",
        "agent:memfork-cli",
    ] {
        assert!(ids.contains(&expected), "no node {expected} in {ids:?}");
    }
    let edges: Vec<(String, String, String)> = first["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let n = |i: &Json| {
                first["nodes"][i.as_u64().unwrap() as usize][0]
                    .as_str()
                    .unwrap()
                    .to_owned()
            };
            (n(&e[0]), n(&e[1]), e[2].as_str().unwrap().to_owned())
        })
        .collect();
    let has = |a: &str, b: &str, k: &str| edges.iter().any(|(x, y, z)| x == a && y == b && z == k);
    assert!(
        has(
            "file:src/auth/login.rs",
            "shop:fact:auth-entry",
            "source of"
        ),
        "{edges:?}"
    );
    assert!(
        has("shop:fact:auth-entry", "shop:decision:payments", "cited by"),
        "{edges:?}"
    );
    assert!(
        has("shop:task:schema", "shop:task:checkout", "depends on"),
        "{edges:?}"
    );
    assert!(
        has("shop:lesson:00000001", "shop:decision:payments", "about"),
        "{edges:?}"
    );
    // Every node has a column and a height, in the column order promised.
    let columns: Vec<u64> = first["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n[3].as_u64().unwrap())
        .collect();
    assert!(columns.windows(2).all(|w| w[0] <= w[1]), "{columns:?}");

    // The same store, another daemon: the same picture, coordinate for
    // coordinate.
    stop(&sandbox);
    started_again(&sandbox);
    let second = graph_of(&sandbox, "?ns=shop");
    assert_eq!(first["nodes"], second["nodes"]);
    assert_eq!(first["edges"], second["edges"]);

    // The past: fewer nodes, none from after the point asked for.
    let past = graph_of(&sandbox, "?ns=shop&at=2");
    assert_eq!(past["at"], 2);
    assert!(past["nodes"].as_array().unwrap().len() < first["nodes"].as_array().unwrap().len());
    assert!(past["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|n| n[5].as_u64().unwrap() <= 2));
}

/// Start a daemon for a sandbox that already has a store, without writing.
fn started_again(sandbox: &Sandbox) -> Endpoint {
    let output = sandbox.command().args(["branches"]).output().expect("ran");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    sandbox
        .wait_for_daemon(Duration::from_secs(20))
        .expect("a daemon")
}

#[test]
fn stores_written_by_earlier_versions_draw_as_graphs() {
    // Every fixture store there is, whichever versions wrote them.
    let fixtures = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let mut versions: Vec<String> = std::fs::read_dir(&fixtures)
        .unwrap()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix("store-")
                .map(str::to_owned)
        })
        .collect();
    versions.sort();
    assert!(
        versions.len() >= 2,
        "expected the fixture stores, found {versions:?}"
    );
    for version in &versions {
        let sandbox = Sandbox::new();
        let fixture = fixtures.join(format!("store-{version}")).join("data");
        for name in ["memfork.snapshot", "memfork.wal"] {
            std::fs::copy(fixture.join(name), sandbox.data().join(name)).unwrap();
        }
        started_again(&sandbox);
        let (port, _, read) = tokens(&sandbox.wait_for_daemon(Duration::from_secs(20)).unwrap());
        let summary = ask(port, Method::GET, "/brain/summary", None, Some(&read));
        assert_eq!(
            summary.status,
            StatusCode::OK,
            "{version}: {}",
            summary.body
        );
        let ns = summary.json()["namespace"].as_str().unwrap().to_owned();
        let graph = ask(
            port,
            Method::GET,
            &format!("/brain/graph?ns={ns}"),
            None,
            Some(&read),
        );
        assert_eq!(graph.status, StatusCode::OK, "{version}: {}", graph.body);
        let graph = graph.json();
        assert!(
            !graph["nodes"].as_array().unwrap().is_empty(),
            "{version}: an old store drew no nodes for {ns}"
        );
        assert_eq!(graph["columns"].as_array().unwrap().len(), 7);
    }
}
