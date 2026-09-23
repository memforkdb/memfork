//! The Brain: a read-only page, served by the daemon on its loopback
//! listener, that shows what the engine knows and what it decided.
//!
//! It is a view of a database engine, not a control panel. Nothing on the
//! page changes memory: every route here answers `GET` and nothing else, the
//! routes read the store and the side structure and write neither, and the
//! token the page holds is the read token, which the daemon refuses on every
//! route that writes. Time travel, the page's one control, is a request for
//! a past view.
//!
//! The page is three files compiled into the binary — the markup, the style
//! and the script — and needs nothing from anywhere else: no font, no icon
//! set, no library, no request that leaves the machine. Memory contents are
//! untrusted text and reach the page as JSON, never as markup; the script
//! renders them as text. The response headers say the same to the browser:
//! a content security policy that allows nothing but the page's own files.
//!
//! `memfork brain` prints the page's address with the read token in the URL
//! fragment, which a browser never sends to the server, and opens it.

pub mod export;
pub mod graph;
pub mod summary;

use std::collections::BTreeMap;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{json, Value as Json};

use crate::events::Events;
use crate::serve::{json_response, text, BoxBody, BRAIN_PATH, BRAIN_PREFIX};
use crate::shared::Shared;

/// The page.
pub const PAGE_HTML: &str = include_str!("page/brain.html");
/// Its style.
pub const PAGE_CSS: &str = include_str!("page/brain.css");
/// Its script.
pub const PAGE_JS: &str = include_str!("page/brain.js");

/// What the browser may load and connect to while showing the page: the
/// page's own files, and nothing else. Inline script and style are refused
/// as well, so a value that somehow reached the markup could not run.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; \
     style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; \
     form-action 'none'; frame-ancestors 'none'";

/// The three files the page is made of, at the paths the page loads them
/// from. They carry no data and need no token.
pub const STATIC: &[(&str, &str, &str)] = &[
    (BRAIN_PATH, "text/html; charset=utf-8", PAGE_HTML),
    ("/brain/app.css", "text/css; charset=utf-8", PAGE_CSS),
    ("/brain/app.js", "text/javascript; charset=utf-8", PAGE_JS),
];

/// Every route that answers with data, by the name after `/brain/`. Each
/// answers `GET` only, needs a token, and reads; there is no other kind, and
/// a test walks this list to prove it.
pub const ROUTES: &[&str] = &[
    "summary",
    "attention",
    "graph",
    "entry",
    "search",
    "diff",
    "export",
];

/// The SHA-256 of one of the page's files, in hex: what `sha256sum` prints
/// for the same file in the source tree, so a file the daemon serves can be
/// held against the source it was meant to be built from.
pub fn digest(body: &str) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(body.as_bytes());
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// The build of the page: the first twelve hex digits of the SHA-256 over
/// its three files in [`STATIC`] order, markup then style then script. Two
/// binaries with the same version and a different page, the ordinary state
/// of a working tree between commits, have different builds; the page's
/// footer, the summary and `memfork doctor` show it, the daemon writes it
/// into its endpoint file, and `memfork brain` refuses a daemon whose build
/// is not its own rather than open a page that is not the one just built.
pub fn page_build() -> &'static str {
    static BUILD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BUILD.get_or_init(|| {
        let mut all = String::new();
        for (_, _, body) in STATIC {
            all.push_str(body);
        }
        let mut hex = digest(&all);
        hex.truncate(12);
        hex
    })
}

/// Why a daemon cannot show this command's page, if it cannot: it was
/// started from another build of the same version, so the page it serves is
/// not the one this binary carries. `None` when the builds agree.
pub fn build_mismatch(endpoint: &crate::persist::Endpoint) -> Option<String> {
    let ours = page_build();
    match endpoint.page_build.as_deref() {
        Some(theirs) if theirs == ours => None,
        Some(theirs) => Some(format!(
            "the running daemon (process {}) serves page build {theirs}, and this command \
             carries page build {ours}: it was started from another build of MemFork \
             {}, probably before this binary was rebuilt. Run `memfork stop`, then try \
             again",
            endpoint.pid,
            crate::VERSION
        )),
        None => Some(format!(
            "the running daemon (process {}) published no page build, so it is older than \
             this command; run `memfork stop`, then try again",
            endpoint.pid
        )),
    }
}

/// The families of key the page knows, as `<project>:<family>:<rest>`.
pub const FAMILIES: &[&str] = &["decision", "fact", "task", "lesson", "handoff", "note"];

/// Environment variable naming the command that opens the page instead of
/// the system's default browser: a program and its leading arguments, the
/// address appended. `none` opens nothing. For tests, and for anyone whose
/// default browser is not the one they want this in.
pub const BROWSER_ENV: &str = "MEMFORK_BROWSER";

/// The page's address for a daemon, with its read token in the fragment,
/// which a browser keeps to itself. `None` for a daemon that published no
/// read token, which is one older than this build.
pub fn url(endpoint: &crate::persist::Endpoint) -> Option<String> {
    let port = endpoint.port?;
    let token = endpoint.read_token.as_deref()?;
    Some(format!("http://127.0.0.1:{port}{BRAIN_PATH}#t={token}"))
}

/// Open `url` in the default browser, or in what [`BROWSER_ENV`] names.
///
/// Nothing is waited for: the opener returns at once on every platform, and
/// a browser that takes a while is not this command's business. The address
/// goes to the opener as one argument and nowhere else: not to a shell.
pub fn open_in_browser(url: &str) -> Result<(), String> {
    let mut command = match std::env::var(BROWSER_ENV) {
        Ok(custom) if !custom.trim().is_empty() => {
            if custom.trim() == "none" {
                return Ok(());
            }
            let parts = crate::cli::split_line(&custom)
                .map_err(|e| format!("{BROWSER_ENV} could not be parsed: {e}"))?;
            let (program, args) = parts
                .split_first()
                .ok_or_else(|| format!("{BROWSER_ENV} names no program"))?;
            let mut command = std::process::Command::new(program);
            command.args(args);
            command
        }
        _ => default_opener(),
    };
    command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        // No console window flashing up for the `cmd` that runs `start`, and
        // no pipe of this process kept open by whatever the browser becomes:
        // a script reading this command's output would wait for that.
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
        crate::daemon::stop_inheriting_std_handles();
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("cannot open a browser ({e}); open the address above yourself"))
}

/// Each platform's own way of opening an address in the default browser.
fn default_opener() -> std::process::Command {
    if cfg!(target_os = "windows") {
        let mut command = std::process::Command::new("cmd");
        // `start` takes a window title first; an empty one keeps the address
        // from being read as the title.
        command.args(["/c", "start", ""]);
        command
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else {
        std::process::Command::new("xdg-open")
    }
}

/// What a route needs from the daemon.
#[derive(Debug, Clone)]
pub struct Context {
    /// The store.
    pub db: memfork_core::Db,
    /// Who is connected, and the activity feed.
    pub events: Arc<Events>,
    /// Leases, statistics and fact records.
    pub side: Arc<Shared>,
    /// The listener's port, which the page checks against its own address.
    pub port: u16,
}

/// The page's own files, if `path` is one of them: answered without a
/// token, with the headers that keep the browser to this listener.
pub fn static_asset(path: &str, method: &Method) -> Option<Response<BoxBody>> {
    let (_, content_type, body) = STATIC.iter().find(|(p, _, _)| *p == path)?;
    if method != Method::GET {
        return Some(text(
            StatusCode::METHOD_NOT_ALLOWED,
            "the Brain only answers GET\n",
        ));
    }
    // The file's own hash, quoted as an entity tag: `curl -i` on the address
    // shows it, and `sha256sum` on the source file prints the same, or does
    // not, which is the whole question when a page looks stale.
    let etag = format!("\"{}\"", digest(body));
    let body = http_body_util::Full::new(hyper::body::Bytes::from_static(body.as_bytes()));
    let response = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", *content_type)
        .header("etag", etag)
        .body(http_body_util::BodyExt::boxed(
            http_body_util::BodyExt::map_err(body, |never| match never {}),
        ))
        .unwrap_or_else(|_| Response::new(BoxBody::default()));
    Some(with_page_headers(response))
}

/// Answer a data route. The token has been checked by the caller.
pub async fn handle(request: Request<Incoming>, context: Context) -> Response<BoxBody> {
    let path = request.uri().path().to_owned();
    let Some(name) = path.strip_prefix(BRAIN_PREFIX).map(str::to_owned) else {
        return with_page_headers(text(StatusCode::NOT_FOUND, "no such page\n"));
    };
    if !ROUTES.contains(&name.as_str()) {
        return with_page_headers(text(StatusCode::NOT_FOUND, "no such route\n"));
    }
    if request.method() != Method::GET {
        return with_page_headers(text(
            StatusCode::METHOD_NOT_ALLOWED,
            "the Brain only answers GET: nothing here changes memory\n",
        ));
    }
    let query = Query::parse(request.uri().query());
    let answer = tokio::task::spawn_blocking(move || match name.as_str() {
        "summary" => summary::summary(&context, &query).map(Answer::Json),
        "attention" => summary::attention(&context, &query).map(Answer::Json),
        "graph" => graph_route(&context, &query).map(Answer::Json),
        "entry" => summary::entry(&context, &query).map(Answer::Json),
        "search" => summary::search(&context, &query).map(Answer::Json),
        "diff" => summary::diff(&context, &query).map(Answer::Json),
        "export" => export::build(&context, &query).map(|e| {
            if query.get("preview").is_some() {
                Answer::Json(e.preview)
            } else {
                Answer::Html(e.html)
            }
        }),
        _ => Err((StatusCode::NOT_FOUND, "no such route".to_owned())),
    })
    .await;
    let response = match answer {
        Ok(Ok(Answer::Json(value))) => json_response(StatusCode::OK, &value),
        Ok(Ok(Answer::Html(html))) => html_response(&html),
        Ok(Err((status, why))) => json_response(status, &json!({ "error": why })),
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({ "error": format!("the route did not finish: {e}") }),
        ),
    };
    with_page_headers(response)
}

/// What a route answers with.
enum Answer {
    /// Data for the page.
    Json(Json),
    /// A whole page, for the export.
    Html(String),
}

/// A page to download: marked as a file, so the browser saves rather than
/// shows it, and never cached.
fn html_response(html: &str) -> Response<BoxBody> {
    let body = http_body_util::Full::new(hyper::body::Bytes::from(html.to_owned()));
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .header("content-disposition", "attachment")
        .body(http_body_util::BodyExt::boxed(
            http_body_util::BodyExt::map_err(body, |never| match never {}),
        ))
        .unwrap_or_else(|_| Response::new(BoxBody::default()))
}

/// The headers every Brain answer carries: the content security policy, no
/// sniffing, no referrer, no caching. No CORS header of any kind: a page
/// from another origin gets nothing.
fn with_page_headers(mut response: Response<BoxBody>) -> Response<BoxBody> {
    let headers = response.headers_mut();
    let set = |headers: &mut hyper::HeaderMap, name: &'static str, value: &str| {
        if let Ok(value) = hyper::header::HeaderValue::from_str(value) {
            headers.insert(name, value);
        }
    };
    set(headers, "content-security-policy", CONTENT_SECURITY_POLICY);
    set(headers, "x-content-type-options", "nosniff");
    set(headers, "referrer-policy", "no-referrer");
    set(headers, "cache-control", "no-store");
    response
}

/// A route's failure: the status and a sentence.
pub type Failure = (StatusCode, String);

/// The query string, decoded.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Query(BTreeMap<String, String>);

impl Query {
    /// Parse `a=1&b=two%20words`. A key given twice keeps the last value.
    pub fn parse(raw: Option<&str>) -> Self {
        let mut map = BTreeMap::new();
        for pair in raw.unwrap_or_default().split('&') {
            if pair.is_empty() {
                continue;
            }
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            map.insert(decode(key), decode(value));
        }
        Query(map)
    }

    /// One parameter, if it was given and is not empty.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    /// A whole number parameter.
    pub fn number(&self, key: &str) -> Result<Option<u64>, Failure> {
        match self.get(key) {
            None => Ok(None),
            Some(raw) => raw.parse().map(Some).map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("`{key}` must be a whole number, got `{raw}`"),
                )
            }),
        }
    }
}

/// Percent-decoding, with `+` as a space. A byte sequence that is not UTF-8
/// is replaced rather than refused.
fn decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(byte) => {
                        out.push(byte);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every project that has keys on a branch, in key order.
pub fn namespaces(db: &memfork_core::Db, branch: &str) -> Result<Vec<String>, Failure> {
    let view = db
        .read(branch)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let mut found: Vec<String> = Vec::new();
    for key in view.keys() {
        let Some((project, rest)) = key.split_once(':') else {
            continue;
        };
        let family = rest.split_once(':').map_or(rest, |(f, _)| f);
        if !FAMILIES.contains(&family) || project.is_empty() {
            continue;
        }
        if !found.iter().any(|p| p == project) {
            found.push(project.to_owned());
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

/// The memory graph of a project on a branch, now or at a past point, laid
/// out.
fn graph_route(context: &Context, query: &Query) -> Result<Json, Failure> {
    let branch = summary::branch_of(context, query)?;
    let ns = summary::namespace_of(context, query, &branch)?;
    let at = query.number("at")?;
    let graph = graph::build(
        &context.db,
        &branch,
        &ns,
        at,
        &context.side,
        &context.events.connected(),
    )
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    Ok(graph::to_json(&graph))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_string_is_decoded() {
        let q = Query::parse(Some("ns=shop&q=two+words%20more&at=12&empty="));
        assert_eq!(q.get("ns"), Some("shop"));
        assert_eq!(q.get("q"), Some("two words more"));
        assert_eq!(q.number("at").unwrap_or(None), Some(12));
        assert_eq!(q.get("empty"), None);
        assert_eq!(q.get("missing"), None);
        assert!(Query::parse(Some("at=twelve")).number("at").is_err());
        assert_eq!(decode("a%zz"), "a%zz");
        assert_eq!(decode("%E2%9C%93"), "\u{2713}");
    }

    #[test]
    fn every_data_route_is_named_once_and_the_static_files_are_three() {
        let mut names = ROUTES.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ROUTES.len());
        assert_eq!(STATIC.len(), 3);
        assert!(STATIC.iter().all(|(p, _, _)| p.starts_with(BRAIN_PATH)));
    }

    #[test]
    fn projects_are_found_from_their_keys() {
        let db = memfork_core::Db::new();
        db.put("main", "shop:decision:a", memfork_core::Value::new("1"))
            .unwrap_or_else(|e| panic!("{e}"));
        db.put("main", "api:task:t1", memfork_core::Value::new("1"))
            .unwrap_or_else(|e| panic!("{e}"));
        db.put("main", "plain-key", memfork_core::Value::new("1"))
            .unwrap_or_else(|e| panic!("{e}"));
        db.put("main", "shop:other:x", memfork_core::Value::new("1"))
            .unwrap_or_else(|e| panic!("{e}"));
        let found = namespaces(&db, "main").unwrap_or_default();
        assert_eq!(found, vec!["api".to_owned(), "shop".to_owned()]);
    }

    #[test]
    fn a_digest_is_the_sha256_that_sha256sum_prints() {
        // `printf 'memfork' | sha256sum`, and the empty file.
        assert_eq!(
            digest("memfork"),
            "b2f07274951c8b97cb1223ad66990be5fa795ced78fe5bbce571a3f83a744afc"
        );
        assert_eq!(
            digest(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_ne!(digest("a"), digest("b"));
    }

    #[test]
    fn the_page_build_is_twelve_hex_digits_of_the_three_files_together() {
        let build = page_build();
        assert_eq!(build.len(), 12);
        assert!(build.bytes().all(|b| b.is_ascii_hexdigit()));
        let all = format!("{PAGE_HTML}{PAGE_CSS}{PAGE_JS}");
        assert_eq!(build, &digest(&all)[..12]);
    }

    #[test]
    fn a_daemon_from_another_build_of_this_version_is_refused_by_name() {
        let mut endpoint = crate::persist::Endpoint::for_this_process();
        endpoint.pid = 4242;
        assert_eq!(endpoint.page_build.as_deref(), Some(page_build()));
        assert_eq!(build_mismatch(&endpoint), None);

        endpoint.page_build = Some("000000000000".to_owned());
        let why = build_mismatch(&endpoint).expect("refused");
        assert!(why.contains("process 4242"), "{why}");
        assert!(why.contains("000000000000"), "{why}");
        assert!(why.contains(page_build()), "{why}");
        assert!(why.contains(crate::VERSION), "{why}");
        assert!(why.contains("`memfork stop`"), "{why}");

        endpoint.page_build = None;
        let why = build_mismatch(&endpoint).expect("refused");
        assert!(why.contains("no page build"), "{why}");
        assert!(why.contains("`memfork stop`"), "{why}");
    }
}
