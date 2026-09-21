//! `memfork mcp` — the MCP server over stdio (DESIGN §6).
//!
//! The tool surface comes from [`crate::tools`], so the MCP server, the vendor
//! function-calling formats and `memfork call` are three views of one registry.
//! Schemas are served exactly as written there, in the conservative subset of
//! DESIGN §6.1, rather than being derived from Rust types — a derive would emit
//! `$schema`, `format` and `anyOf`, which some clients sanitise and some
//! reject.
//!
//! **State is durable by default** (DESIGN §4.5). The binary persists unless
//! `--ephemeral` says otherwise, which is the opposite of the library default:
//! a library should not write to someone's disk unasked, and an agent memory
//! that empties on restart is not memory. This server is the one the daemon
//! runs, and the one an `--ephemeral` session runs directly; every other
//! client reaches it through [`crate::proxy`].

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation,
    InitializeRequestParams, InitializeResult, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::json;

use crate::namespace;
use crate::tools::{self, dispatch::Session, dispatch::ToolError};

/// What the client is told about this server when it connects.
///
/// Shared with the proxy, so a client sees the same thing whether it reached
/// a direct server or one forwarding to the daemon.
pub const INSTRUCTIONS: &str = "\
MemFork gives you branchable memory: store and recall things, and — the part \
that matters — fork the whole of memory before a risky step, then merge it if \
the attempt worked or discard it if it did not. Forking costs the same whatever \
memory holds, so fork freely: it is cheaper than being careful. Writes on a \
fork are invisible to the branch you came from until you merge, and discarding \
leaves that branch exactly as it was. You can also read memory as it was at any \
earlier point.

What you store is kept on disk and is still there next time, so memory you \
write in one session is memory you can recall in the next. Other clients can \
be using the same memory at the same time, and will see what you write; each \
of you keeps your own current branch, so switching branches never moves \
anybody else.";

/// The experimental capability a proxy uses to tell the daemon who it is
/// speaking for: `{"namespace": …, "client": …}`.
///
/// A proxy is the daemon's client, so the daemon's own `clientInfo` would only
/// ever say "memfork-proxy". The proxy knows the real client's name, from that
/// client's `initialize`, and the project, from its working directory, and
/// passes both on here — an ordinary MCP capability field, so any other
/// client simply does not send it.
pub const SESSION_CAPABILITY: &str = "memfork/session";

/// What the client is told when it connects: [`INSTRUCTIONS`], then which
/// project this session is in and the conventions that go with it.
///
/// Built per session, because the namespace is per session. This is how every
/// agent learns its project's name and the handoff routine without anyone
/// editing a file.
pub fn instructions(ns: &str) -> String {
    let decision = namespace::prefix(ns, "decision");
    let task = namespace::prefix(ns, "task");
    format!(
        "{INSTRUCTIONS}

This session's project namespace is `{ns}`. Keep what you record about this \
project under it, with colons between the parts: each decision and its reason \
under `{decision}<topic>`, open tasks under `{task}<id>`. Call memfork_resume \
when you start, to pick up what earlier agents decided and did, and \
memfork_handoff before you stop, so the next one can continue from you. Fork \
before anything risky, merge if it worked, discard if it did not. Other \
projects share this store under their own names, and keys without a project \
in front are still there for the tools that take a key."
    )
}

/// Take what a client said in `initialize` into its session: who it is, for
/// attribution, and — from a proxy — which project it is working in.
pub fn adopt(session: &Session, request: &InitializeRequestParams) {
    let hello = request
        .capabilities
        .experimental
        .as_ref()
        .and_then(|e| e.get(SESSION_CAPABILITY));
    let named = hello
        .and_then(|h| h.get("client"))
        .and_then(|c| c.as_str())
        .filter(|c| !c.trim().is_empty());
    session.set_writer(named.unwrap_or(&request.client_info.name));
    // A namespace that is not valid is ignored rather than refused: the
    // session keeps the one it had, and the handshake still succeeds.
    if let Some(ns) = hello
        .and_then(|h| h.get("namespace"))
        .and_then(|n| n.as_str())
        .filter(|n| namespace::validate(n).is_ok())
    {
        session.set_namespace(ns.to_owned());
    }
    session.announce();
}

/// The tool list, built from the registry.
///
/// Shared with the proxy, which answers `tools/list` from here rather than
/// asking the daemon: the surface is static, so going upstream for it would
/// mean starting a daemon to answer a question about MemFork itself.
pub fn tool_list() -> Vec<Tool> {
    tools::all()
        .into_iter()
        .map(|t| Tool::new(t.name, t.description, Arc::new(t.schema)).with_title(t.title))
        .collect()
}

/// The MCP server: one session over one in-memory database.
#[derive(Debug, Clone)]
pub struct MemforkServer {
    session: Arc<Session>,
}

impl MemforkServer {
    /// A server over the given session.
    pub fn new(session: Arc<Session>) -> Self {
        MemforkServer { session }
    }
}

impl ServerHandler for MemforkServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("memfork", env!("CARGO_PKG_VERSION"))
                    .with_title("MemFork — branchable agent memory"),
            )
            .with_instructions(instructions(&self.session.namespace()))
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        adopt(&self.session, &request);
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // Fifteen tools fit in one page comfortably, and DESIGN §6.1 caps the
        // count at sixteen, so there is nothing to paginate.
        Ok(ListToolsResult::with_all_items(tool_list()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        match self.session.call(&request.name, &args) {
            Ok(value) => Ok(CallToolResult::structured(value).into()),

            // The caller's request could not be routed or understood. MCP
            // renders these as protocol errors, which is right: the fault is in
            // the request, not in what the tool found.
            Err(e @ (ToolError::UnknownTool(_) | ToolError::BadArguments(_))) => {
                Err(ErrorData::invalid_params(e.to_string(), None))
            }

            // The tool ran and the engine said no — an unknown branch, a bad
            // sequence number. The model should see this and correct itself, so
            // it comes back as a tool-level error with the reason in it.
            Err(e @ ToolError::Engine(_)) => Ok(CallToolResult::structured_error(json!({
                "error": e.to_string(),
                "tool": request.name,
            }))
            .into()),
        }
    }
}

/// Serve MCP over stdio until the client disconnects.
///
/// Anything written to stdout would corrupt the protocol stream, so every
/// diagnostic in this path goes to stderr.
pub async fn serve_stdio(session: Arc<Session>) -> Result<(), String> {
    let service = MemforkServer::new(session)
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| format!("cannot start the MCP server: {e}"))?;
    service
        .waiting()
        .await
        .map_err(|e| format!("the MCP server stopped with an error: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registry_tool_is_offered_with_its_schema() {
        let listed = tool_list();
        let registry = tools::all();
        assert_eq!(listed.len(), registry.len());
        for (tool, def) in listed.iter().zip(&registry) {
            assert_eq!(tool.name, def.name);
            assert_eq!(tool.description.as_deref(), Some(def.description.as_str()));
            assert_eq!(
                tool.input_schema.as_ref(),
                &def.schema,
                "the server altered the schema of {}",
                def.name
            );
        }
    }

    #[test]
    fn the_instructions_describe_what_this_build_actually_does() {
        // These have been wrong twice, both times because a phase changed the
        // behaviour and left the words behind. So each claim that has been
        // wrong before gets an assertion.
        let lower = INSTRUCTIONS.to_ascii_lowercase();
        assert!(lower.contains("kept on disk"), "{INSTRUCTIONS}");
        assert!(lower.contains("still there next time"), "{INSTRUCTIONS}");
        assert!(
            !lower.contains("nothing is written to disk"),
            "the instructions still claim nothing is persisted"
        );
        assert!(
            lower.contains("other clients can be using the same memory"),
            "the instructions do not mention sharing: {INSTRUCTIONS}"
        );
        assert!(
            !lower.contains("start a second copy") && !lower.contains("one process owns"),
            "the instructions still describe the single-process behaviour that \
             the daemon replaced"
        );
    }

    #[test]
    fn each_session_is_told_its_project_and_the_routine() {
        let text = instructions("shop-front");
        assert!(text.starts_with(INSTRUCTIONS));
        assert!(text.contains("project namespace is `shop-front`"), "{text}");
        assert!(text.contains("`shop-front:decision:<topic>`"), "{text}");
        assert!(text.contains("`shop-front:task:<id>`"), "{text}");
        for step in [
            "memfork_resume",
            "memfork_handoff",
            "fork",
            "merge",
            "discard",
        ] {
            assert!(text.contains(step), "no `{step}` in: {text}");
        }
    }

    fn hello(client: &str, experimental: Option<serde_json::Value>) -> InitializeRequestParams {
        let mut caps = rmcp::model::ClientCapabilities::default();
        if let Some(serde_json::Value::Object(obj)) = experimental {
            let mut map = std::collections::BTreeMap::new();
            map.insert(SESSION_CAPABILITY.to_owned(), obj);
            caps.experimental = Some(map);
        }
        InitializeRequestParams::new(caps, Implementation::new(client, "1.0"))
    }

    #[test]
    fn a_direct_client_is_recorded_by_the_name_it_gives() {
        let session = Session::in_namespace(memfork_core::Db::new(), "here");
        adopt(&session, &hello("some-client", None));
        assert_eq!(session.writer().as_deref(), Some("some-client"));
        assert_eq!(session.namespace(), "here");
    }

    #[test]
    fn a_proxy_speaks_for_its_client_and_project() {
        let session = Session::new(memfork_core::Db::new());
        adopt(
            &session,
            &hello(
                "memfork-proxy",
                Some(json!({"client": "real-client", "namespace": "shop"})),
            ),
        );
        assert_eq!(session.writer().as_deref(), Some("real-client"));
        assert_eq!(session.namespace(), "shop");
        assert!(instructions(&session.namespace()).contains("`shop`"));
    }

    #[test]
    fn an_unusable_namespace_in_a_hello_is_ignored() {
        let session = Session::in_namespace(memfork_core::Db::new(), "kept");
        adopt(
            &session,
            &hello("memfork-proxy", Some(json!({"namespace": "Not Valid:"}))),
        );
        assert_eq!(session.namespace(), "kept");
        assert_eq!(session.writer().as_deref(), Some("memfork-proxy"));
    }

    #[test]
    fn the_instructions_name_no_vendor() {
        const VENDORS: &[&str] = &[
            "claude",
            "anthropic",
            "openai",
            "gpt",
            "gemini",
            "grok",
            "cursor",
            "codex",
        ];
        let lower = instructions("project").to_ascii_lowercase();
        for v in VENDORS {
            assert!(!lower.contains(v), "instructions mention `{v}`");
        }
    }
}
