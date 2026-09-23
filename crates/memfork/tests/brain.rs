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
    let endpoint = started(&sandbox);
    let (port, full, read) = tokens(&endpoint);

    for (path, kind, source) in [
        ("/brain", "text/html", memfork::brain::PAGE_HTML),
        ("/brain/app.js", "text/javascript", memfork::brain::PAGE_JS),
        ("/brain/app.css", "text/css", memfork::brain::PAGE_CSS),
    ] {
        let a = ask(port, Method::GET, path, None, None);
        assert_eq!(a.status, StatusCode::OK, "{path}");
        assert!(
            a.header("content-type").unwrap().starts_with(kind),
            "{path}"
        );
        // What is served is the source, byte for byte, and says so: the
        // entity tag is the file's SHA-256, which `sha256sum` on the source
        // file prints too.
        assert_eq!(a.body, source, "{path} is not the file in the binary");
        assert_eq!(
            a.header("etag"),
            Some(format!("\"{}\"", memfork::brain::digest(source)).as_str()),
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
    // The summary and the endpoint file both name the page's build, which
    // is this binary's: the daemon was started from it.
    assert_eq!(json["page_build"], memfork::brain::page_build());
    assert_eq!(
        endpoint.page_build.as_deref(),
        Some(memfork::brain::page_build())
    );
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

// ---- the panels, one entry, search, and a branch comparison ------------------

#[test]
fn the_summary_carries_the_panels_and_the_headline_counts_real_things() {
    let sandbox = Sandbox::new();
    started(&sandbox);
    furnish(&sandbox);
    let (port, _, read) = tokens(&sandbox.wait_for_daemon(Duration::from_secs(20)).unwrap());
    let a = ask(
        port,
        Method::GET,
        "/brain/summary?ns=shop",
        None,
        Some(&read),
    );
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let s = a.json();
    assert_eq!(s["headline"]["twice"], 0, "nothing has been served yet");
    assert_eq!(s["tasks"].as_array().unwrap().len(), 2);
    let schema = s["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == "schema")
        .unwrap();
    assert_eq!(schema["status"], "open");
    let checkout = s["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == "checkout")
        .unwrap();
    assert_eq!(checkout["ready"], false, "{checkout}");
    assert_eq!(s["facts"].as_array().unwrap().len(), 1);
    assert_eq!(s["facts"][0]["key"], "shop:fact:auth-entry");
    assert_eq!(s["facts"][0]["state"], "unverified", "{}", s["facts"][0]);
    assert_eq!(s["lessons"].as_array().unwrap().len(), 1);
    assert_eq!(s["lessons"][0]["branch"], "try-refunds");
    assert_eq!(s["lessons"][0]["served"], 0);
    assert!(s["handoffs"].as_array().unwrap().is_empty());
    assert!(s["briefings"].as_array().unwrap().is_empty());
    assert!(s["footer"]["store_bytes"].as_u64().unwrap() > 0);
    assert!(s["footer"]["commits_retained"].as_u64().unwrap() > 0);

    // A handoff, a resume by another client, and a stale fact: the panels
    // and the headline follow.
    let run = |args: &[&str]| {
        let output = sandbox
            .command()
            .env(memfork::namespace::NAMESPACE_ENV, "shop")
            .args(args)
            .output()
            .expect("ran");
        assert!(
            output.status.success(),
            "`memfork {}`: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    run(&[
        "put",
        "shop:handoff:00000001",
        r#"{"summary":"schema next","next":["design the schema"]}"#,
    ]);
    std::fs::write(
        sandbox.root().join("src").join("auth").join("login.rs"),
        "fn login() { changed }\n",
    )
    .unwrap();
    // A resume through the tools, as another client, sees the handoff and
    // the stale fact.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let transport = rmcp::transport::TokioChildProcess::new(
            rmcp::transport::ConfigureCommandExt::configure(
                tokio::process::Command::from({
                    let mut c = sandbox.command();
                    c.env(memfork::namespace::NAMESPACE_ENV, "shop");
                    c
                }),
                |cmd| {
                    cmd.arg("mcp");
                    cmd.stderr(std::process::Stdio::null());
                },
            ),
        )
        .expect("spawned");
        let client = rmcp::ServiceExt::serve((), transport)
            .await
            .expect("handshake");
        let result = client
            .call_tool(rmcp::model::CallToolRequestParams::new(
                "memfork_resume".to_owned(),
            ))
            .await
            .expect("resumed");
        let brief = result.structured_content.expect("structured");
        assert_eq!(
            brief["latest_handoff"]["key"], "shop:handoff:00000001",
            "{brief}"
        );
        client.cancel().await.expect("closed");
    });

    let s = ask(
        port,
        Method::GET,
        "/brain/summary?ns=shop",
        None,
        Some(&read),
    )
    .json();
    assert_eq!(
        s["handoffs"].as_array().unwrap().len(),
        1,
        "{}",
        s["handoffs"]
    );
    assert_eq!(s["handoffs"][0]["number"], "1");
    assert!(
        s["handoffs"][0]["picked_up"].is_object(),
        "{}",
        s["handoffs"][0]
    );
    assert_eq!(
        s["briefings"].as_array().unwrap().len(),
        1,
        "{}",
        s["briefings"]
    );
    assert!(s["briefings"][0]["bytes"].as_u64().unwrap() > 0);
    assert_eq!(s["facts"][0]["state"], "stale", "{}", s["facts"][0]);
    let attention = ask(
        port,
        Method::GET,
        "/brain/attention?ns=shop",
        None,
        Some(&read),
    )
    .json();
    assert!(
        attention["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "stale_fact"),
        "{}",
        attention["attention"]
    );
    assert_eq!(s["headline"]["counts"]["briefings"], 1);
    assert_eq!(s["headline"]["counts"]["handoffs_picked_up"], 1);
    assert!(
        s["headline"]["twice"].as_u64().unwrap() >= 2,
        "{}",
        s["headline"]
    );
    assert_eq!(s["lessons"][0]["served"], 1);
}

#[test]
fn one_entry_comes_with_its_history_and_sources_and_a_script_tag_stays_text() {
    let sandbox = Sandbox::new();
    started(&sandbox);
    furnish(&sandbox);
    let sneaky = "<script>alert(1)</script><img src=x onerror=alert(2)>";
    let output = sandbox
        .command()
        .args(["put", "shop:decision:payments", sneaky])
        .output()
        .unwrap();
    assert!(output.status.success());
    let (port, _, read) = tokens(&sandbox.wait_for_daemon(Duration::from_secs(20)).unwrap());
    let a = ask(
        port,
        Method::GET,
        "/brain/entry?key=shop%3Adecision%3Apayments",
        None,
        Some(&read),
    );
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let e = a.json();
    // The value travels as JSON text, character for character, and the page
    // renders it as text; nothing here turns it into markup.
    assert_eq!(e["value"], sneaky);
    assert_eq!(a.header("content-type"), Some("application/json"));
    assert_eq!(e["by"], "memfork-cli");
    let history = e["history"].as_array().unwrap();
    assert_eq!(history.len(), 2, "{history:?}");
    assert!(history[0]["seq"].as_u64() > history[1]["seq"].as_u64());
    assert_eq!(history[0]["what"], "written");
    assert!(e["sources"].is_null());

    let fact = ask(
        port,
        Method::GET,
        "/brain/entry?key=shop%3Afact%3Aauth-entry",
        None,
        Some(&read),
    )
    .json();
    assert_eq!(fact["sources"][0]["path"], "src/auth/login.rs");
    assert_eq!(fact["sources"][0]["state"], "unverified");

    let missing = ask(
        port,
        Method::GET,
        "/brain/entry?key=shop%3Anothing",
        None,
        Some(&read),
    );
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let none = ask(port, Method::GET, "/brain/entry", None, Some(&read));
    assert_eq!(none.status, StatusCode::BAD_REQUEST);
}

#[test]
fn search_is_the_engines_ranked_search_and_a_diff_is_what_a_merge_would_face() {
    let sandbox = Sandbox::new();
    started(&sandbox);
    furnish(&sandbox);
    let (port, _, read) = tokens(&sandbox.wait_for_daemon(Duration::from_secs(20)).unwrap());
    let a = ask(
        port,
        Method::GET,
        "/brain/search?ns=shop&q=checkout",
        None,
        Some(&read),
    );
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let hits = a.json()["hits"].as_array().unwrap().clone();
    assert!(!hits.is_empty());
    assert!(
        hits.iter().any(|h| h["key"] == "shop:decision:payments"),
        "{hits:?}"
    );
    let scores: Vec<u64> = hits.iter().map(|h| h["score"].as_u64().unwrap()).collect();
    assert!(scores.windows(2).all(|w| w[0] >= w[1]));
    let again = ask(
        port,
        Method::GET,
        "/brain/search?ns=shop&q=checkout",
        None,
        Some(&read),
    );
    assert_eq!(
        again.json()["hits"],
        serde_json::json!(hits),
        "search is not deterministic"
    );
    let empty = ask(
        port,
        Method::GET,
        "/brain/search?ns=shop",
        None,
        Some(&read),
    )
    .json();
    assert!(empty["hits"].as_array().unwrap().is_empty());

    let run = |args: &[&str]| {
        let output = sandbox.command().args(args).output().expect("ran");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&["fork", "try-eu"]);
    run(&[
        "put",
        "--branch",
        "try-eu",
        "shop:decision:tax",
        "EU VAT at checkout",
    ]);
    run(&[
        "put",
        "--branch",
        "try-eu",
        "shop:decision:payments",
        "another provider",
    ]);
    let d = ask(
        port,
        Method::GET,
        "/brain/diff?a=main&b=try-eu&ns=shop",
        None,
        Some(&read),
    );
    assert_eq!(d.status, StatusCode::OK, "{}", d.body);
    let d = d.json();
    assert_eq!(d["count"], 2, "{d}");
    let kinds: Vec<(String, String)> = d["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["key"].as_str().unwrap().to_owned(),
                c["kind"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(
        kinds.contains(&("shop:decision:tax".to_owned(), "added".to_owned())),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&("shop:decision:payments".to_owned(), "modified".to_owned())),
        "{kinds:?}"
    );
    let bad = ask(
        port,
        Method::GET,
        "/brain/diff?a=main&b=nope",
        None,
        Some(&read),
    );
    assert_eq!(bad.status, StatusCode::NOT_FOUND);
}

// ---- `memfork brain` ---------------------------------------------------------

#[test]
fn memfork_brain_starts_the_daemon_and_prints_a_link_with_the_read_token_in_the_fragment() {
    let sandbox = Sandbox::new();
    assert!(sandbox.owner().is_none(), "a daemon was already running");
    let output = sandbox
        .command()
        .args(["brain", "--no-open"])
        .output()
        .expect("ran");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let endpoint = sandbox
        .wait_for_daemon(Duration::from_secs(20))
        .expect("`memfork brain` started no daemon");
    let (port, full, read) = tokens(&endpoint);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let url = stdout.lines().next().unwrap_or_default();
    assert_eq!(url, format!("http://127.0.0.1:{port}/brain#t={read}"));
    assert!(
        !url.contains('?'),
        "the token travels in a query string: {url}"
    );
    assert!(
        !stdout.contains(&full),
        "the daemon's own token was printed"
    );
    assert!(stdout.contains("read token"), "{stdout}");

    let json = sandbox
        .command()
        .args(["--json", "brain", "--no-open"])
        .output()
        .expect("ran");
    assert!(json.status.success());
    let doc: Json = serde_json::from_slice(&json.stdout).expect("json");
    assert_eq!(doc["url"], url);
    assert_eq!(doc["port"], port);
    assert_eq!(doc["opened"], false);
    assert!(!String::from_utf8_lossy(&json.stdout).contains(&full));

    // Doctor points at the page without the token; the link is the command's.
    let doctor = sandbox.command().args(["doctor"]).output().expect("ran");
    let text = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        text.contains(&format!("brain         http://127.0.0.1:{port}/brain")),
        "{text}"
    );
    assert!(!text.contains(&read) && !text.contains(&full), "{text}");
    let doc: Json = serde_json::from_slice(
        &sandbox
            .command()
            .args(["--json", "doctor"])
            .output()
            .expect("ran")
            .stdout,
    )
    .expect("json");
    assert_eq!(
        doc["brain"]["url"],
        format!("http://127.0.0.1:{port}/brain")
    );
    assert_eq!(doc["brain"]["allowed"], true);
    assert_eq!(doc["brain"]["built"], true);
}

#[test]
fn memfork_brain_opens_the_browser_it_is_told_to_with_the_address_as_one_argument() {
    let sandbox = Sandbox::new();
    // The "browser" is another MemFork, storing whatever it was handed in a
    // second data directory, which is how the test reads the address back
    // on every operating system without a script.
    let other = sandbox.root().join("other");
    std::fs::create_dir_all(&other).unwrap();
    // Single quotes: the path has backslashes on Windows, which double quotes
    // would read as escapes.
    let browser = format!(
        "'{}' --data-dir '{}' put brain:opened",
        support::memfork_binary().display(),
        other.display()
    );
    let output = sandbox
        .command()
        .env(memfork::brain::BROWSER_ENV, &browser)
        .args(["brain"])
        .output()
        .expect("ran");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("cannot open"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let endpoint = sandbox
        .wait_for_daemon(Duration::from_secs(20))
        .expect("a daemon");
    let (port, _, read) = tokens(&endpoint);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut opened = String::new();
    while std::time::Instant::now() < deadline {
        let got = sandbox
            .command()
            .args([
                "--data-dir",
                &other.display().to_string(),
                "get",
                "brain:opened",
            ])
            .output()
            .expect("ran");
        // A key that is not there yet is answered with nothing, not a failure.
        let text = String::from_utf8_lossy(&got.stdout).trim().to_owned();
        if got.status.success() && !text.is_empty() {
            opened = text;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        opened.contains(&format!("http://127.0.0.1:{port}/brain#t={read}")),
        "the browser was handed: {opened:?}"
    );
    let _ = memfork::daemon::stop(&other);

    // `none` opens nothing and still prints the address.
    let none = sandbox
        .command()
        .env(memfork::brain::BROWSER_ENV, "none")
        .args(["brain"])
        .output()
        .expect("ran");
    assert!(none.status.success());
    assert!(String::from_utf8_lossy(&none.stdout).contains("/brain#t="));
}

#[test]
fn the_machine_policy_can_switch_the_brain_off() {
    let sandbox = Sandbox::new();
    let policy = sandbox.root().join("policy.toml");
    std::fs::write(&policy, "brain = false\n").unwrap();
    let output = sandbox
        .command()
        .env(memfork::policy::EXTRA_ENV, &policy)
        .args(["brain", "--no-open"])
        .output()
        .expect("ran");
    assert!(
        !output.status.success(),
        "the Brain opened under a policy that forbids it"
    );
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("The Brain is switched off by the machine policy"),
        "{said}"
    );
    assert!(
        sandbox.owner().is_none(),
        "a refused `memfork brain` started a daemon"
    );

    let doctor = sandbox
        .command()
        .env(memfork::policy::EXTRA_ENV, &policy)
        .args(["doctor"])
        .output()
        .expect("ran");
    let text = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        text.contains("brain         switched off by the machine policy"),
        "{text}"
    );
}

// ---- export -----------------------------------------------------------------

#[test]
fn an_export_is_one_self_contained_file_with_credentials_withheld_and_no_token() {
    let sandbox = Sandbox::new();
    started(&sandbox);
    furnish(&sandbox);
    // Built at run time so this file never holds anything shaped like a key.
    let key = format!("AKIA{}", "B".repeat(16));
    let output = sandbox
        .command()
        .args([
            "put",
            "shop:note:cloud",
            &format!("the access key is {key}"),
            "--allow-secret",
            "aws-access-key",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let (port, full, read) = tokens(&sandbox.wait_for_daemon(Duration::from_secs(20)).unwrap());

    let preview = ask(
        port,
        Method::GET,
        "/brain/export?ns=shop&preview=1",
        None,
        Some(&read),
    );
    assert_eq!(preview.status, StatusCode::OK, "{}", preview.body);
    let p = preview.json();
    assert_eq!(p["namespace"], "shop");
    assert!(p["entries"].as_u64().unwrap() >= 6, "{p}");
    assert!(p["bytes"].as_u64().unwrap() > 10_000);
    let withheld = p["withheld"].as_array().unwrap();
    assert!(
        withheld.iter().any(|w| w["rule"] == "aws-access-key"),
        "{withheld:?}"
    );
    assert!(p["file"].as_str().unwrap().ends_with(".html"));
    assert!(
        !preview.body.contains(&key),
        "the preview carries the credential"
    );

    let file = ask(
        port,
        Method::GET,
        "/brain/export?ns=shop",
        None,
        Some(&read),
    );
    assert_eq!(file.status, StatusCode::OK);
    assert!(file
        .header("content-type")
        .unwrap()
        .starts_with("text/html"));
    assert_eq!(file.header("content-disposition"), Some("attachment"));
    let html = &file.body;
    assert_eq!(html.len() as u64, p["bytes"].as_u64().unwrap());
    assert!(html.contains("window.MEMFORK_EXPORT"));
    assert!(html.contains(memfork::brain::export::WITHHELD));
    assert!(!html.contains(&key), "the export carries the credential");
    assert!(
        !html.contains(&read) && !html.contains(&full),
        "the export carries a token"
    );
    // Self-contained: nothing loaded from anywhere, the page's own files included.
    assert!(
        !html.contains("href=\"/brain/"),
        "the export links to the daemon"
    );
    assert!(
        !html.contains("src=\"/brain/"),
        "the export loads from the daemon"
    );
    assert!(
        !html.contains("http://") && !html.contains("https://"),
        "the export names an address"
    );
    assert!(html.contains("shop:decision:payments"));
    assert!(html.contains("shop:fact:auth-entry"));
    // The past, too.
    let past = ask(
        port,
        Method::GET,
        "/brain/export?ns=shop&at=2&preview=1",
        None,
        Some(&read),
    );
    assert_eq!(past.status, StatusCode::OK, "{}", past.body);
    assert_eq!(past.json()["at"], 2);
    assert!(past.json()["entries"].as_u64().unwrap() < p["entries"].as_u64().unwrap());
}

// ---- the recorded store the page is booted against under Node ----------------

/// Where the page tests keep a summary and a graph recorded from a real
/// store, so the whole page can be booted under Node against them.
fn fixtures_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("brain")
        .join("page")
        .join("tests")
        .join("fixtures")
}

fn keys_of(v: &Json) -> Vec<String> {
    v.as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

/// The page test under Node boots the real page against a summary and a
/// graph recorded from a real store: a decision, a fact, two tasks, a
/// lesson, five handoffs and one briefing served to another client. Set
/// `MEMFORK_RECORD_PAGE_FIXTURES=1` to record them again; otherwise this
/// asserts the recording still has the shape a live store answers with, so
/// the page test cannot pass against answers the daemon no longer gives.
#[test]
fn the_page_fixtures_come_from_a_real_store_and_keep_its_shape() {
    let sandbox = Sandbox::new();
    started(&sandbox);
    furnish(&sandbox);
    let run = |args: &[&str]| {
        let output = sandbox
            .command()
            .env(memfork::namespace::NAMESPACE_ENV, "shop")
            .args(args)
            .output()
            .expect("ran");
        assert!(
            output.status.success(),
            "`memfork {}`: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let summaries = [
        "schema next",
        "schema drafted; checkout page next",
        "checkout page half done, blocked on the payment provider's sandbox",
        "sandbox access granted; wiring the callback",
        "callback wired and tested; release after review",
    ];
    for (n, summary) in summaries.iter().enumerate() {
        run(&[
            "put",
            &format!("shop:handoff:{:08}", n + 1),
            &format!(r#"{{"summary":"{summary}","next":["design the schema"]}}"#),
        ]);
    }
    // One resume by another client: a briefing served, a handoff picked up.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let transport = rmcp::transport::TokioChildProcess::new(
            rmcp::transport::ConfigureCommandExt::configure(
                tokio::process::Command::from({
                    let mut c = sandbox.command();
                    c.env(memfork::namespace::NAMESPACE_ENV, "shop");
                    c
                }),
                |cmd| {
                    cmd.arg("mcp");
                    cmd.stderr(std::process::Stdio::null());
                },
            ),
        )
        .expect("spawned");
        let client = rmcp::ServiceExt::serve((), transport)
            .await
            .expect("handshake");
        client
            .call_tool(rmcp::model::CallToolRequestParams::new(
                "memfork_resume".to_owned(),
            ))
            .await
            .expect("resumed");
        client.cancel().await.expect("closed");
    });

    let (port, _, read) = tokens(&sandbox.wait_for_daemon(Duration::from_secs(20)).unwrap());
    let live_summary = ask(
        port,
        Method::GET,
        "/brain/summary?ns=shop",
        None,
        Some(&read),
    );
    assert_eq!(live_summary.status, StatusCode::OK, "{}", live_summary.body);
    let live_summary = live_summary.json();
    let live_graph = graph_of(&sandbox, "?ns=shop");
    assert_eq!(live_summary["handoffs"].as_array().unwrap().len(), 5);
    assert!(
        live_graph["nodes"].as_array().unwrap().len() >= 10,
        "{live_graph}"
    );

    let dir = fixtures_dir();
    if std::env::var_os("MEMFORK_RECORD_PAGE_FIXTURES").is_some() {
        std::fs::create_dir_all(&dir).unwrap();
        for (name, json) in [("summary", &live_summary), ("graph", &live_graph)] {
            let mut text = serde_json::to_string_pretty(json).unwrap();
            text.push('\n');
            std::fs::write(dir.join(format!("{name}.json")), text).unwrap();
        }
        return;
    }
    let recorded = |name: &str| -> Json {
        let path = dir.join(format!("{name}.json"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e}; record with MEMFORK_RECORD_PAGE_FIXTURES=1",
                path.display()
            )
        });
        serde_json::from_str(&text).unwrap()
    };
    let summary = recorded("summary");
    let graph = recorded("graph");
    let same = |what: &str, a: &Json, b: &Json| {
        assert_eq!(
            keys_of(a),
            keys_of(b),
            "{what}: the recording no longer has the live shape; record again with MEMFORK_RECORD_PAGE_FIXTURES=1"
        );
    };
    same("summary", &summary, &live_summary);
    same(
        "summary.headline",
        &summary["headline"],
        &live_summary["headline"],
    );
    same(
        "summary.headline.counts",
        &summary["headline"]["counts"],
        &live_summary["headline"]["counts"],
    );
    same(
        "summary.footer",
        &summary["footer"],
        &live_summary["footer"],
    );
    same(
        "summary.autopilot",
        &summary["autopilot"],
        &live_summary["autopilot"],
    );
    for list in ["handoffs", "briefings", "facts", "lessons", "tasks"] {
        same(
            &format!("summary.{list}[0]"),
            &summary[list][0],
            &live_summary[list][0],
        );
    }
    same("graph", &graph, &live_graph);
    assert_eq!(graph["kinds"], live_graph["kinds"]);
    assert_eq!(
        graph["nodes"][0].as_array().unwrap().len(),
        live_graph["nodes"][0].as_array().unwrap().len(),
        "a graph node's tuple changed"
    );
    assert_eq!(summary["handoffs"].as_array().unwrap().len(), 5);
    assert_eq!(summary["namespace"], "shop");
    assert!(!graph["nodes"].as_array().unwrap().is_empty());
}
