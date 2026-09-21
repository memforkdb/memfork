//! `memfork serve` — the local daemon (DESIGN §5).
//!
//! One process owns the data directory and the write-ahead log; every
//! `memfork mcp` is a proxy to it. That is what lets two clients share one
//! memory, which is the normal case rather than the exotic one.
//!
//! Loopback only, always. The daemon binds `127.0.0.1` and nothing else, and
//! every request must carry the bearer token from the endpoint file, so a
//! process that cannot read that file cannot reach the memory. rmcp's own
//! `Host` validation stays on as a second line against DNS rebinding.
//!
//! The daemon exits on its own after an idle period. Nobody wants a background
//! process they did not start living forever, and an autostarted daemon that
//! never stops is exactly that.

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::StreamableHttpService;
use tokio_util::sync::CancellationToken;

use crate::mcp::MemforkServer;
use crate::persist::{Endpoint, Store};
use crate::tools::dispatch::Session;

/// The path the MCP endpoint is served at.
pub const MCP_PATH: &str = "/mcp";

/// The path `memfork stop` posts to.
pub const SHUTDOWN_PATH: &str = "/shutdown";

/// How long the daemon waits with nothing to do before exiting.
pub const DEFAULT_IDLE_SECONDS: u64 = 600;

/// How to run the daemon.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// Port to bind on loopback. Zero asks the operating system to pick.
    pub port: u16,
    /// Exit after this long with no requests. Zero means never.
    pub idle_seconds: u64,
}

/// Why the daemon stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// Nothing asked anything of it for the idle period.
    Idle,
    /// `memfork stop`, or something else that posted to the shutdown path.
    Asked,
    /// The listener failed.
    Failed,
}

/// Shared between the request handler and the idle watcher.
#[derive(Debug)]
struct Clock {
    /// Milliseconds since the daemon started, at the last request.
    last_seen: AtomicU64,
    started: Instant,
}

impl Clock {
    fn new() -> Self {
        Clock {
            last_seen: AtomicU64::new(0),
            started: Instant::now(),
        }
    }

    fn touch(&self) {
        let elapsed = self.started.elapsed().as_millis() as u64;
        self.last_seen.store(elapsed, Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        let now = self.started.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.last_seen.load(Ordering::Relaxed)))
    }
}

/// Run the daemon until it is asked to stop or goes idle.
///
/// `store` is the open data directory: holding it is what makes this process
/// the single writer, and dropping it releases the lock and removes the
/// endpoint file.
pub async fn run(
    db: memfork_core::Db,
    store: Arc<Store>,
    options: ServeOptions,
) -> Result<Stopped, String> {
    let token = crate::persist::lock::new_token().map_err(|e| e.to_string())?;

    let listener =
        tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, options.port)))
            .await
            .map_err(|e| format!("cannot listen on 127.0.0.1:{}: {e}", options.port))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("cannot read the port that was bound: {e}"))?
        .port();

    // Publish only once the socket is listening, so anything that reads the
    // endpoint file can connect immediately rather than racing the bind.
    let mut endpoint = Endpoint::for_this_process();
    endpoint.port = Some(port);
    endpoint.token = Some(token.clone());
    store.publish(&endpoint).map_err(|e| e.to_string())?;

    let shutdown = CancellationToken::new();
    let clock = Arc::new(Clock::new());
    clock.touch();

    // Every HTTP session gets its own MCP session over the one database, so
    // two clients share the data and keep their own current branch.
    let shared = db.clone();
    let service = StreamableHttpService::new(
        // Each session starts in the fallback namespace with no writer; its
        // `initialize` says which project and client it is (see
        // `crate::mcp::adopt`).
        move || Ok(MemforkServer::new(Arc::new(Session::new(shared.clone())))),
        Arc::new(LocalSessionManager::default()),
        // Our tools are request-and-response, so the server can answer in
        // plain JSON and the proxy needs no event-stream parsing. The default
        // allowed-hosts list is loopback only, which is left alone: it is a
        // second line against DNS rebinding on top of binding 127.0.0.1.
        rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default()
            .with_json_response(true)
            .with_legacy_session_mode(true),
    );

    let idle = {
        let clock = Arc::clone(&clock);
        let shutdown = shutdown.clone();
        let limit = options.idle_seconds;
        tokio::spawn(async move {
            if limit == 0 {
                return false;
            }
            let limit = Duration::from_secs(limit);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return false,
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {
                        if clock.idle_for() >= limit {
                            shutdown.cancel();
                            return true;
                        }
                    }
                }
            }
        })
    };

    let accept = {
        let shutdown = shutdown.clone();
        let clock = Arc::clone(&clock);
        let token = token.clone();
        async move {
            loop {
                let stream = tokio::select! {
                    _ = shutdown.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => stream,
                        // One bad connection is not a reason to stop serving
                        // the others.
                        Err(_) => continue,
                    },
                };
                let service = service.clone();
                let shutdown = shutdown.clone();
                let clock = Arc::clone(&clock);
                let token = token.clone();
                tokio::spawn(async move {
                    let guard = Guard {
                        inner: service,
                        token,
                        clock,
                        shutdown,
                    };
                    let io = hyper_util::rt::TokioIo::new(stream);
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, hyper_util::service::TowerToHyperService::new(guard))
                        .await;
                });
            }
        }
    };

    accept.await;
    let stopped = if idle.await.unwrap_or(false) {
        Stopped::Idle
    } else {
        Stopped::Asked
    };

    // Flush before the lock goes, so a looser fsync policy does not lose the
    // last commits to a shutdown the daemon chose itself.
    store.flush().map_err(|e| e.to_string())?;
    Ok(stopped)
}

/// Checks the token, notices activity, and answers the shutdown path itself.
#[derive(Clone)]
struct Guard {
    inner: StreamableHttpService<MemforkServer, LocalSessionManager>,
    token: String,
    clock: Arc<Clock>,
    shutdown: CancellationToken,
}

type BoxBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

fn text(status: StatusCode, message: &str) -> Response<BoxBody> {
    let body = Full::new(Bytes::from(message.to_owned()))
        .map_err(|never| match never {})
        .boxed();
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(body)
        .unwrap_or_else(|_| Response::new(BoxBody::default()))
}

impl tower_service::Service<Request<Incoming>> for Guard {
    type Response = Response<BoxBody>;
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<
        Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<Incoming>) -> Self::Future {
        self.clock.touch();

        let presented = request
            .headers()
            .get(hyper::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default()
            .to_owned();
        // Compared in full rather than short-circuiting on the first differing
        // byte. The daemon is loopback-only and the token is 32 random bytes,
        // so this is belt and braces, but it costs nothing.
        let authorized = presented.len() == self.token.len()
            && presented
                .bytes()
                .zip(self.token.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0;

        if !authorized {
            return Box::pin(async {
                Ok(text(
                    StatusCode::UNAUTHORIZED,
                    "this MemFork daemon needs the token from its endpoint file\n",
                ))
            });
        }

        if request.uri().path() == SHUTDOWN_PATH {
            self.shutdown.cancel();
            return Box::pin(async { Ok(text(StatusCode::OK, "stopping\n")) });
        }

        let mut inner = self.inner.clone();
        Box::pin(async move {
            match tower_service::Service::call(&mut inner, request).await {
                Ok(response) => Ok(response.map(|b| b.map_err(std::io::Error::other).boxed())),
                Err(never) => match never {},
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_idle_clock_measures_from_the_last_request() {
        let clock = Clock::new();
        clock.touch();
        assert!(clock.idle_for() < Duration::from_millis(100));
    }

    /// Short enough that a forgotten daemon goes away, long enough that a
    /// client which pauses to think does not lose its server. Checked at
    /// compile time, since both sides are constants.
    const _: () = {
        assert!(DEFAULT_IDLE_SECONDS >= 300);
        assert!(DEFAULT_IDLE_SECONDS <= 3600);
    };
}
