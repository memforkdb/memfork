//! Forwarding a stdio MCP session to the daemon.
//!
//! Every persistent `memfork mcp` is a proxy: the daemon owns the data
//! directory and the write-ahead log, and the proxies own nothing. That is
//! what lets two clients share one memory without two processes writing the
//! same log.
//!
//! The daemon speaks ordinary streamable HTTP MCP, so a remote client could
//! use it too (DESIGN §6.1, tier 2). The proxy uses the smallest subset of that
//! which does the job — `initialize`, `tools/list`, `tools/call` over plain
//! JSON POSTs — rather than a full client transport. The daemon is configured
//! to answer in JSON, so there is no event stream to parse, and writing a few
//! hundred lines of SSE handling to talk to our own process would be work
//! spent on the wrong problem.
//!
//! **Nothing starts a daemon until a tool is actually called.** `initialize`
//! and `tools/list` are answered here, from the same static registry the
//! daemon would serve, because the tool surface does not depend on the data.
//!
//! That is not an optimisation. Clients start `memfork mcp` to ask what it is
//! — `claude mcp get` health-checks a server by launching it and shaking hands
//! — and a proxy that started a daemon to answer `initialize` would leave one
//! running behind every such probe. `memfork stop; memfork doctor` did exactly
//! that: doctor asked the client, the client launched the server, and the
//! server started the daemon that had just been stopped.

use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation,
    InitializeRequestParams, InitializeResult, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{json, Value as Json};
use tokio::sync::Mutex;

use crate::persist::Endpoint;

/// The protocol version the proxy asks the daemon for.
///
/// A version with the classic `initialize` handshake, so the exchange is the
/// well-trodden one rather than the newest one.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// How long any single request to the daemon may take.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Why talking to the daemon failed.
#[derive(Debug, Clone, thiserror::Error)]
pub enum UpstreamError {
    /// The daemon could not be reached at all.
    #[error("cannot reach the MemFork daemon at {url}: {reason}")]
    Unreachable {
        /// Where it was expected.
        url: String,
        /// What went wrong.
        reason: String,
    },
    /// The daemon answered, but not with something usable.
    #[error("the MemFork daemon answered unexpectedly: {0}")]
    Unusable(String),
    /// The daemon no longer knows this session: it was idle past the
    /// daemon's session timeout. A new session is the fix.
    #[error("the MemFork daemon has forgotten this session")]
    SessionGone,
    /// The daemon returned a JSON-RPC error.
    #[error("{message}")]
    Rpc {
        /// The JSON-RPC code.
        code: i64,
        /// The message.
        message: String,
    },
}

impl UpstreamError {
    /// Whether this looks like the daemon having gone away, rather than a
    /// request it refused.
    pub fn is_disconnect(&self) -> bool {
        matches!(
            self,
            UpstreamError::Unreachable { .. } | UpstreamError::SessionGone
        )
    }
}

/// Who a proxy is speaking for, told to the daemon when it connects.
///
/// See [`crate::mcp::SESSION_CAPABILITY`]. The daemon records the client's
/// name against what the session writes, and works in the namespace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hello {
    /// The project namespace, already valid.
    pub namespace: String,
    /// The name the real client gave in its own `initialize`, if it has
    /// shaken hands yet.
    pub client: Option<String>,
}

impl Hello {
    fn capabilities(&self) -> Json {
        let mut session = serde_json::Map::new();
        session.insert("namespace".to_owned(), json!(self.namespace));
        if let Some(client) = &self.client {
            session.insert("client".to_owned(), json!(client));
        }
        json!({ "experimental": { crate::mcp::SESSION_CAPABILITY: session } })
    }
}

/// A connection to a running daemon.
#[derive(Debug)]
pub struct Upstream {
    url: String,
    token: String,
    http: Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>>,
    session: Mutex<Option<String>>,
    next_id: std::sync::atomic::AtomicI64,
}

impl Upstream {
    /// Connect to the daemon an endpoint file describes, and shake hands,
    /// saying who this connection speaks for.
    pub async fn connect(endpoint: &Endpoint, hello: &Hello) -> Result<Self, UpstreamError> {
        let port = endpoint
            .port
            .ok_or_else(|| UpstreamError::Unusable("the endpoint file names no port".to_owned()))?;
        let token = endpoint.token.clone().ok_or_else(|| {
            UpstreamError::Unusable("the endpoint file carries no token".to_owned())
        })?;

        let upstream = Upstream {
            url: format!("http://127.0.0.1:{port}{}", crate::serve::MCP_PATH),
            token,
            http: Client::builder(TokioExecutor::new()).build_http(),
            session: Mutex::new(None),
            next_id: std::sync::atomic::AtomicI64::new(1),
        };
        upstream.initialize(hello).await?;
        Ok(upstream)
    }

    async fn initialize(&self, hello: &Hello) -> Result<(), UpstreamError> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": hello.capabilities(),
                    "clientInfo": { "name": "memfork-proxy", "version": crate::VERSION },
                }),
            )
            .await?;
        let _ = result;
        self.notify("notifications/initialized", json!({})).await
    }

    /// Ask the daemon for the tool list.
    pub async fn list_tools(&self) -> Result<Vec<Tool>, UpstreamError> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .cloned()
            .ok_or_else(|| UpstreamError::Unusable("no tools in the answer".to_owned()))?;
        serde_json::from_value(tools)
            .map_err(|e| UpstreamError::Unusable(format!("the tool list did not parse: {e}")))
    }

    /// End this session, so the daemon stops listing its client as
    /// connected. Best effort: a daemon that has gone needs no telling.
    pub async fn close(&self) {
        let Some(session) = self.session.lock().await.clone() else {
            return;
        };
        let Ok(request) = hyper::Request::builder()
            .method(hyper::Method::DELETE)
            .uri(&self.url)
            .header(
                hyper::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .header("Mcp-Session-Id", session)
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .body(Full::new(Bytes::new()))
        else {
            return;
        };
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.http.request(request),
        )
        .await;
    }

    /// Forward one tool call.
    pub async fn call_tool(&self, params: &CallToolRequestParams) -> Result<Json, UpstreamError> {
        let payload = serde_json::to_value(params)
            .map_err(|e| UpstreamError::Unusable(format!("cannot encode the call: {e}")))?;
        self.request("tools/call", payload).await
    }

    async fn notify(&self, method: &str, params: Json) -> Result<(), UpstreamError> {
        let body = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        // A notification has no id and no reply; anything but a transport
        // failure is success.
        self.post(&body).await.map(|_| ())
    }

    async fn request(&self, method: &str, params: Json) -> Result<Json, UpstreamError> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let text = self.post(&body).await?;
        let message = parse_message(&text)?;

        if let Some(error) = message.get("error") {
            return Err(UpstreamError::Rpc {
                code: error.get("code").and_then(Json::as_i64).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(Json::as_str)
                    .unwrap_or("the daemon reported an error")
                    .to_owned(),
            });
        }
        message
            .get("result")
            .cloned()
            .ok_or_else(|| UpstreamError::Unusable(format!("no result in {text}")))
    }

    async fn post(&self, body: &Json) -> Result<String, UpstreamError> {
        let unreachable = |reason: String| UpstreamError::Unreachable {
            url: self.url.clone(),
            reason,
        };

        let mut builder = hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri(&self.url)
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .header(hyper::header::ACCEPT, "application/json, text/event-stream")
            .header(
                hyper::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .header("MCP-Protocol-Version", PROTOCOL_VERSION);
        if let Some(session) = self.session.lock().await.as_ref() {
            builder = builder.header("Mcp-Session-Id", session);
        }

        let request = builder
            .body(Full::new(Bytes::from(body.to_string())))
            .map_err(|e| unreachable(format!("cannot build the request: {e}")))?;

        let response = tokio::time::timeout(REQUEST_TIMEOUT, self.http.request(request))
            .await
            .map_err(|_| unreachable("it did not answer in time".to_owned()))?
            .map_err(|e| unreachable(e.to_string()))?;

        // The daemon hands out a session on the first exchange; carry it.
        if let Some(session) = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
        {
            *self.session.lock().await = Some(session);
        }

        let status = response.status();
        let collected = response
            .into_body()
            .collect()
            .await
            .map_err(|e| unreachable(format!("the answer could not be read: {e}")))?;
        let text = String::from_utf8_lossy(&collected.to_bytes()).into_owned();

        // A session the daemon has let go of. Only once there is one to lose:
        // before the handshake a 404 means something else entirely.
        if status == hyper::StatusCode::NOT_FOUND && self.session.lock().await.is_some() {
            return Err(UpstreamError::SessionGone);
        }
        if status == hyper::StatusCode::UNAUTHORIZED {
            return Err(UpstreamError::Unusable(
                "the daemon rejected our token; it may have been restarted".to_owned(),
            ));
        }
        if !status.is_success() {
            return Err(UpstreamError::Unusable(format!("HTTP {status}: {text}")));
        }
        Ok(text)
    }
}

/// Pull the JSON-RPC message out of a response body.
///
/// Plain JSON in the ordinary case, because the daemon is configured to answer
/// that way. An event stream is still handled, in case a future daemon falls
/// back to one: the message is the first `data:` payload.
fn parse_message(text: &str) -> Result<Json, UpstreamError> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed)
            .map_err(|e| UpstreamError::Unusable(format!("the answer did not parse: {e}")));
    }
    for line in text.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            let payload = payload.trim();
            if payload.is_empty() {
                continue;
            }
            return serde_json::from_str(payload).map_err(|e| {
                UpstreamError::Unusable(format!("an event-stream payload did not parse: {e}"))
            });
        }
    }
    Err(UpstreamError::Unusable(format!(
        "the answer was neither JSON nor an event stream: {}",
        text.chars().take(200).collect::<String>()
    )))
}

/// How a proxy gets hold of a daemon, including starting one, introducing
/// itself with the given [`Hello`].
pub type Reconnect = Arc<
    dyn Fn(Hello) -> futures::future::BoxFuture<'static, Result<Upstream, String>> + Send + Sync,
>;

/// An MCP server that forwards everything to the daemon.
#[derive(Clone)]
pub struct Proxy {
    /// `None` until a tool call needs the daemon. Handshakes never fill it.
    upstream: Arc<tokio::sync::RwLock<Option<Arc<Upstream>>>>,
    connect: Reconnect,
    /// The project this session works in, from where the proxy was started.
    namespace: String,
    /// The real client's name, from its `initialize`.
    client: Arc<std::sync::Mutex<Option<String>>>,
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proxy").finish_non_exhaustive()
    }
}

impl Proxy {
    /// A proxy that will reach the daemon when it first needs to.
    ///
    /// `connect` both starts the daemon, if none is running, and connects to
    /// it. It is not called here: see the note at the top of this module about
    /// what a handshake must not cost.
    pub fn new(connect: Reconnect, namespace: String) -> Self {
        Proxy {
            upstream: Arc::new(tokio::sync::RwLock::new(None)),
            connect,
            namespace,
            client: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Who this proxy speaks for, as it stands.
    fn hello(&self) -> Hello {
        Hello {
            namespace: self.namespace.clone(),
            client: self
                .client
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    /// The daemon, connecting — and starting it — if this is the first need.
    async fn current(&self) -> Result<Arc<Upstream>, String> {
        if let Some(existing) = self.upstream.read().await.clone() {
            return Ok(existing);
        }
        let mut slot = self.upstream.write().await;
        // Another task may have connected while this one waited for the lock.
        if let Some(existing) = slot.clone() {
            return Ok(existing);
        }
        let fresh = Arc::new((self.connect)(self.hello()).await?);
        *slot = Some(Arc::clone(&fresh));
        Ok(fresh)
    }

    /// Try again after the daemon went away: start a new one and reconnect.
    ///
    /// Once, not in a loop. A daemon that dies immediately after starting is a
    /// problem retrying will not fix, and a proxy that keeps trying turns a
    /// clear failure into a hang.
    async fn reconnect(&self) -> Result<Arc<Upstream>, String> {
        let mut slot = self.upstream.write().await;
        let fresh = Arc::new((self.connect)(self.hello()).await?);
        *slot = Some(Arc::clone(&fresh));
        Ok(fresh)
    }

    /// Run an operation against the daemon, restarting it once if it has gone.
    async fn with_retry<T, F>(&self, operation: F) -> Result<T, ErrorData>
    where
        F: for<'a> Fn(&'a Upstream) -> futures::future::BoxFuture<'a, Result<T, UpstreamError>>,
    {
        let first = self.current().await.map_err(|why| {
            ErrorData::internal_error(
                format!(
                    "the MemFork daemon could not be started: {why}. \
                     Run `memfork doctor` to see the state of the data directory."
                ),
                None,
            )
        })?;
        match operation(&first).await {
            Ok(value) => return Ok(value),
            Err(e) if !e.is_disconnect() => return Err(as_mcp_error(&e)),
            Err(_) => {}
        }

        // It went away mid-session. Start another and try once more.
        let fresh = self.reconnect().await.map_err(|why| {
            ErrorData::internal_error(
                format!(
                    "the MemFork daemon stopped and could not be restarted: {why}. \
                     Run `memfork doctor` to see the state of the data directory."
                ),
                None,
            )
        })?;
        operation(&fresh).await.map_err(|e| {
            ErrorData::internal_error(
                format!(
                    "the MemFork daemon stopped, was restarted, and still could not \
                     be reached: {e}"
                ),
                None,
            )
        })
    }
}

fn as_mcp_error(e: &UpstreamError) -> ErrorData {
    match e {
        UpstreamError::Rpc { code, message } => {
            // Pass the daemon's own refusal through, rather than wrapping it
            // in one of ours: the client should see what the tool said.
            ErrorData::new(rmcp::model::ErrorCode(*code as i32), message.clone(), None)
        }
        other => ErrorData::internal_error(other.to_string(), None),
    }
}

impl ServerHandler for Proxy {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("memfork", crate::VERSION)
                    .with_title("MemFork — branchable agent memory"),
            )
            .with_instructions(crate::mcp::instructions(&self.namespace))
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        // Remembered for the daemon, which only ever hears from this proxy.
        // Still nothing is started: see the note at the top of this module.
        let name = request.client_info.name.trim();
        if !name.is_empty() {
            *self.client.lock().unwrap_or_else(|e| e.into_inner()) = Some(name.to_owned());
        }
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // Answered here, not upstream. The tool surface is static — it does
        // not depend on what is in the store — so asking the daemon would
        // mean starting one, and a client that only wants to know what tools
        // exist would leave a daemon behind every time it asked.
        //
        // The two lists cannot drift: both are built from `crate::tools`, and
        // a test compares what a proxy lists with what a daemon lists.
        Ok(ListToolsResult::with_all_items(crate::mcp::tool_list()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let params = request.clone();
        let raw = self
            .with_retry(move |up| {
                let params = params.clone();
                Box::pin(async move { up.call_tool(&params).await })
            })
            .await?;

        // The daemon already produced a proper tool result; pass it through
        // rather than rebuilding it, so nothing is lost in translation.
        match serde_json::from_value::<CallToolResult>(raw.clone()) {
            Ok(result) => Ok(result.into()),
            Err(_) => Ok(CallToolResult::structured(raw).into()),
        }
    }
}

/// Serve a proxied MCP session over stdio.
///
/// Returns when the client goes away: closing stdin, or dying, ends the
/// session and this process with it. A proxy that outlived its client would
/// accumulate one per connection.
pub async fn serve_stdio(proxy: Proxy) -> Result<(), String> {
    let upstream = Arc::clone(&proxy.upstream);
    let service = proxy
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| format!("cannot start the MCP proxy: {e}"))?;
    let waited = service
        .waiting()
        .await
        .map_err(|e| format!("the MCP proxy stopped with an error: {e}"));
    // The client has gone; say so to the daemon rather than leaving it to
    // notice after its session timeout.
    let connected = upstream.read().await.clone();
    if let Some(connected) = connected {
        connected.close().await;
    }
    waited.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hello_carries_the_project_and_the_client() {
        let hello = Hello {
            namespace: "shop".to_owned(),
            client: Some("real-client".to_owned()),
        };
        let caps = hello.capabilities();
        let session = &caps["experimental"][crate::mcp::SESSION_CAPABILITY];
        assert_eq!(session["namespace"], "shop");
        assert_eq!(session["client"], "real-client");

        // Before the client has shaken hands there is no name to pass on.
        let early = Hello {
            namespace: "shop".to_owned(),
            client: None,
        };
        assert!(
            early.capabilities()["experimental"][crate::mcp::SESSION_CAPABILITY]
                .get("client")
                .is_none()
        );
    }

    #[test]
    fn a_plain_json_answer_parses() {
        let message =
            parse_message(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#).expect("parsed");
        assert_eq!(message["result"]["ok"], true);
    }

    #[test]
    fn an_event_stream_answer_parses() {
        // Only reachable if a future daemon stops answering in JSON, but
        // silently failing then would be a bad way to find out.
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let message = parse_message(body).expect("parsed");
        assert_eq!(message["result"]["ok"], true);
    }

    #[test]
    fn something_that_is_neither_is_reported_rather_than_guessed_at() {
        let err = parse_message("<html>not this</html>").expect_err("refused");
        assert!(err.to_string().contains("neither JSON nor an event stream"));
    }

    #[test]
    fn only_a_transport_failure_counts_as_the_daemon_going_away() {
        // A tool that refuses is not a disconnect, and retrying it by
        // restarting the daemon would be both useless and confusing.
        assert!(UpstreamError::Unreachable {
            url: "http://127.0.0.1:1/mcp".to_owned(),
            reason: "refused".to_owned(),
        }
        .is_disconnect());
        assert!(!UpstreamError::Rpc {
            code: -32602,
            message: "bad arguments".to_owned(),
        }
        .is_disconnect());
        assert!(!UpstreamError::Unusable("odd".to_owned()).is_disconnect());
        // A forgotten session is fixed the same way: by starting another.
        assert!(UpstreamError::SessionGone.is_disconnect());
    }
}
