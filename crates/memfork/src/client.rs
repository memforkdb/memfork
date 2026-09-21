//! Talking to the daemon from the command line: operations, and the activity
//! stream for `memfork watch`.
//!
//! The same loopback listener and token as the MCP endpoint. Plain JSON posts
//! and one long-lived newline-delimited stream, so there is nothing here a
//! person could not follow with `curl`.

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::Value as Json;

use crate::persist::Endpoint;

/// How long one operation may take before the command line gives up.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A connection to a running daemon.
#[derive(Debug)]
pub struct Daemon {
    base: String,
    token: String,
    http: Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>>,
    /// The port, for messages.
    pub port: u16,
}

impl Daemon {
    /// The daemon an endpoint file describes.
    pub fn new(endpoint: &Endpoint) -> Result<Self, String> {
        let port = endpoint
            .port
            .ok_or_else(|| "the endpoint file names no port".to_owned())?;
        let token = endpoint
            .token
            .clone()
            .ok_or_else(|| "the endpoint file carries no token".to_owned())?;
        Ok(Daemon {
            base: format!("http://127.0.0.1:{port}"),
            token,
            http: Client::builder(TokioExecutor::new()).build_http(),
            port,
        })
    }

    fn request(
        &self,
        method: hyper::Method,
        path: &str,
        body: String,
    ) -> Result<hyper::Request<Full<Bytes>>, String> {
        hyper::Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base))
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .header(
                hyper::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| format!("cannot build the request: {e}"))
    }

    /// Post JSON and read JSON back, with the status.
    pub async fn post(&self, path: &str, body: &Json) -> Result<(u16, Json), String> {
        let request = self.request(hyper::Method::POST, path, body.to_string())?;
        let response = tokio::time::timeout(REQUEST_TIMEOUT, self.http.request(request))
            .await
            .map_err(|_| "the daemon did not answer in time".to_owned())?
            .map_err(|e| format!("cannot reach the daemon on port {}: {e}", self.port))?;
        let status = response.status().as_u16();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| format!("the answer could not be read: {e}"))?
            .to_bytes();
        if status == 401 {
            return Err("the daemon rejected the token; it may have been restarted".to_owned());
        }
        let json = serde_json::from_slice(&bytes).map_err(|e| {
            format!(
                "the daemon answered HTTP {status} with something that is not JSON ({e}): {}",
                String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )
        })?;
        Ok((status, json))
    }

    /// Read a newline-delimited stream, handing each line to `each` until it
    /// returns `false` or the daemon ends the stream.
    pub async fn stream_lines(
        &self,
        path: &str,
        mut each: impl FnMut(&str) -> bool,
    ) -> Result<(), String> {
        let request = self.request(hyper::Method::GET, path, String::new())?;
        let response = self
            .http
            .request(request)
            .await
            .map_err(|e| format!("cannot reach the daemon on port {}: {e}", self.port))?;
        if !response.status().is_success() {
            return Err(format!("the daemon answered HTTP {}", response.status()));
        }
        let mut body = response.into_body();
        let mut pending: Vec<u8> = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| format!("the stream broke: {e}"))?;
            let Ok(data) = frame.into_data() else {
                continue;
            };
            pending.extend_from_slice(&data);
            while let Some(at) = pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = pending.drain(..=at).collect();
                let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
                if !text.is_empty() && !each(&text) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}
