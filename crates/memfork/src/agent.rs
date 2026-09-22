//! A scripted agent: a client of the daemon that does exactly what it is
//! told, with no model behind it.
//!
//! It connects the way a real client's proxy does, over the same MCP
//! transport, saying the name of the client it stands in for, so the daemon
//! attributes its writes and the Brain shows them as it would show the real
//! thing. It hashes a fact's sources and checks facts in answers against a
//! project directory, as the proxy does, so freshness works too.
//!
//! `memfork demo` drives two of these. Nothing here spends anything or
//! touches a real store: the caller says which daemon and which directory.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value as Json};

use crate::persist::Endpoint;
use crate::proxy::{Hello, Upstream};

/// One scripted agent, connected.
#[derive(Debug)]
pub struct FakeAgent {
    /// The name a person knows the client by.
    pub display: String,
    /// The project it works in.
    pub namespace: String,
    root: PathBuf,
    upstream: Upstream,
    hasher: crate::facts::Hasher,
}

/// The MCP client name for a display name, from the registry: what that
/// client sends in `initialize`. A name the registry does not know is used
/// as it is.
pub fn mcp_name_for(display: &str) -> String {
    crate::clients::all()
        .into_iter()
        .find(|c| c.display.eq_ignore_ascii_case(display))
        .and_then(|c| {
            c.mcp_names
                .first()
                .map(|n| n.trim_end_matches('*').to_owned())
        })
        .unwrap_or_else(|| display.to_owned())
}

/// Two clients to stand in for, from the registry: the first two whose
/// MCP name is confirmed. Data, not a choice made in code.
pub fn default_pair() -> (String, String) {
    let mut names = crate::clients::all()
        .into_iter()
        .filter(|c| c.unverified.is_none() && !c.mcp_names.is_empty())
        .map(|c| c.display);
    let a = names.next().unwrap_or_else(|| "Agent A".to_owned());
    let b = names.next().unwrap_or_else(|| "Agent B".to_owned());
    (a, b)
}

impl FakeAgent {
    /// Connect to the daemon an endpoint file describes, as `display`, in
    /// `namespace`, with `root` as the project directory.
    pub async fn connect(
        endpoint: &Endpoint,
        display: &str,
        namespace: &str,
        root: &Path,
    ) -> Result<Self, String> {
        let hello = Hello {
            namespace: namespace.to_owned(),
            client: Some(mcp_name_for(display)),
            session: Some(format!(
                "fake-{}-{}",
                mcp_name_for(display),
                std::process::id()
            )),
        };
        let upstream = Upstream::connect(endpoint, &hello)
            .await
            .map_err(|e| e.to_string())?;
        Ok(FakeAgent {
            display: display.to_owned(),
            namespace: namespace.to_owned(),
            root: root.to_path_buf(),
            upstream,
            hasher: crate::facts::Hasher::default(),
        })
    }

    /// Call one tool. A `memfork_put` with `sources` has them hashed here,
    /// where the files are; facts in any answer are checked against the
    /// files and the verdicts reported, as a proxy would.
    pub async fn call(&self, tool: &str, args: Json) -> Result<Json, String> {
        let mut arguments: Map<String, Json> = match args {
            Json::Object(map) => map,
            _ => Map::new(),
        };
        if tool == "memfork_put" {
            let sources: Option<Vec<String>> = arguments
                .get("sources")
                .and_then(|s| serde_json::from_value(s.clone()).ok());
            if let Some(Ok(sources)) = sources.map(|s| crate::facts::normalise(&s)) {
                let hashes = self.hasher.record(&self.root, &sources);
                arguments.insert("source_hashes".to_owned(), json!(hashes));
            }
        }
        let params =
            rmcp::model::CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments);
        let result = self
            .upstream
            .call_tool(&params)
            .await
            .map_err(|e| e.to_string())?;
        if result.get("isError") == Some(&json!(true)) {
            return Err(format!(
                "`{tool}` failed: {}",
                result["content"][0]["text"]
                    .as_str()
                    .unwrap_or("no reason given")
            ));
        }
        let mut content = result
            .get("structuredContent")
            .cloned()
            .unwrap_or(Json::Null);
        let checked = crate::facts::check(&mut content, &self.root, &self.hasher);
        if !checked.is_empty() {
            self.upstream
                .report(&json!({
                    "client": mcp_name_for(&self.display),
                    "namespace": self.namespace,
                    "facts": checked.facts.iter().map(|(k, s)| json!([k, s])).collect::<Vec<_>>(),
                }))
                .await;
        }
        Ok(content)
    }

    /// End the session, so the daemon stops listing this client.
    pub async fn disconnect(self) {
        self.upstream.close().await;
    }
}
