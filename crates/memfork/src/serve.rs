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

use crate::cli::Command;
use crate::events::{Event, Events};
use crate::mcp::MemforkServer;
use crate::persist::{Endpoint, Store};
use crate::tools::dispatch::Session;

/// The path the MCP endpoint is served at.
pub const MCP_PATH: &str = "/mcp";

/// The path `memfork stop` posts to.
pub const SHUTDOWN_PATH: &str = "/shutdown";

/// The path `memfork watch` reads the activity stream from.
pub const EVENTS_PATH: &str = "/events";

/// The path the command line posts its operations to.
pub const CLI_PATH: &str = "/cli";

/// The path a proxy reports facts it has checked to.
pub const REPORT_PATH: &str = "/report";

/// How often the side structure is written while it changes.
const SIDECAR_FLUSH: Duration = Duration::from_secs(5);

/// The name the command line's writes are recorded under.
pub const CLI_WRITER: &str = "memfork-cli";

/// How long a client session may go without a request before the daemon
/// forgets it. A proxy ends its session when its client goes away, so this
/// only matters for one that was killed; and a proxy whose session was
/// forgotten simply starts another.
pub const DEFAULT_SESSION_SECONDS: u64 = 1800;

/// How long the daemon waits with nothing to do before exiting.
pub const DEFAULT_IDLE_SECONDS: u64 = 600;

/// How to run the daemon.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// Port to bind on loopback. Zero asks the operating system to pick.
    pub port: u16,
    /// Exit after this long with no requests. Zero means never.
    pub idle_seconds: u64,
    /// Forget a client session after this long with no requests from it.
    pub session_seconds: u64,
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
    let events = Arc::new(Events::default());
    let (sidecar, note) = crate::sidecar::Sidecar::open(store.dir());
    if let Some(note) = note {
        eprintln!("memfork: {note}");
    }
    let side = Arc::new(crate::shared::Shared {
        sidecar,
        ..crate::shared::Shared::default()
    });
    let shared = db.clone();
    let hub = Arc::clone(&events);
    let side_for_sessions = Arc::clone(&side);
    let mut sessions = LocalSessionManager::default();
    sessions.session_config.keep_alive = Some(Duration::from_secs(options.session_seconds.max(1)));
    let service = StreamableHttpService::new(
        // Each session starts in the fallback namespace with no writer; its
        // `initialize` says which project and client it is (see
        // `crate::mcp::adopt`), and from then on it reports what it does.
        move || {
            Ok(MemforkServer::new(Arc::new(
                Session::new(shared.clone())
                    .reporting_to(Arc::clone(&hub))
                    .sharing(Arc::clone(&side_for_sessions)),
            )))
        },
        Arc::new(sessions),
        // Our tools are request-and-response, so the server can answer in
        // plain JSON and the proxy needs no event-stream parsing. The default
        // allowed-hosts list is loopback only, which is left alone: it is a
        // second line against DNS rebinding on top of binding 127.0.0.1.
        rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default()
            .with_json_response(true)
            .with_legacy_session_mode(true),
    );

    // Statistics and fact hashes to disk, now and then while they change.
    let flusher = {
        let side = Arc::clone(&side);
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(SIDECAR_FLUSH) => {
                        if let Err(e) = side.sidecar.flush() {
                            eprintln!("memfork: {e}");
                        }
                    }
                }
            }
        })
    };

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
        let side = Arc::clone(&side);
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
                let events = Arc::clone(&events);
                let db = db.clone();
                let side = Arc::clone(&side);
                tokio::spawn(async move {
                    let guard = Guard {
                        inner: service,
                        token,
                        clock,
                        shutdown,
                        events,
                        db,
                        port,
                        side,
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
    flusher.abort();
    if let Err(e) = side.sidecar.flush() {
        eprintln!("memfork: {e}");
    }
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
    events: Arc<Events>,
    db: memfork_core::Db,
    port: u16,
    side: Arc<crate::shared::Shared>,
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
        // Watching is not using: a `memfork watch` left open must not keep
        // an otherwise idle daemon alive.
        if request.uri().path() != EVENTS_PATH {
            self.clock.touch();
        }

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

        if request.uri().path() == EVENTS_PATH {
            return Box::pin(std::future::ready(Ok(event_stream(
                &self.events,
                self.port,
                self.shutdown.clone(),
            ))));
        }

        if request.uri().path() == CLI_PATH {
            let db = self.db.clone();
            let events = Arc::clone(&self.events);
            let side = Arc::clone(&self.side);
            return Box::pin(async move { Ok(run_cli(request, db, events, side).await) });
        }

        if request.uri().path() == REPORT_PATH {
            let events = Arc::clone(&self.events);
            let side = Arc::clone(&self.side);
            return Box::pin(async move { Ok(take_report(request, events, side).await) });
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

fn json_response(status: StatusCode, value: &serde_json::Value) -> Response<BoxBody> {
    let body = Full::new(Bytes::from(value.to_string()))
        .map_err(|never| match never {})
        .boxed();
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(body)
        .unwrap_or_else(|_| Response::new(BoxBody::default()))
}

/// The activity stream: one JSON object per line, starting with who is
/// connected, ending when the daemon stops.
fn event_stream(events: &Events, port: u16, shutdown: CancellationToken) -> Response<BoxBody> {
    let hello = crate::events::Hello {
        schema: crate::events::SCHEMA,
        kind: "hello".to_owned(),
        version: crate::VERSION.to_owned(),
        port,
        clients: events.connected(),
    };
    let first = serde_json::to_string(&hello).unwrap_or_default();
    let receiver = events.subscribe();
    let stream = futures::stream::unfold(
        (Some(first), receiver, shutdown),
        |(first, mut receiver, shutdown)| async move {
            if let Some(line) = first {
                return Some((line, (None, receiver, shutdown)));
            }
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return None,
                    got = receiver.recv() => match got {
                        Ok(event) => {
                            let line = serde_json::to_string(&event).unwrap_or_default();
                            return Some((line, (None, receiver, shutdown)));
                        }
                        // A watcher that fell behind misses what it missed,
                        // and carries on.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                    },
                }
            }
        },
    );
    let frames = futures::StreamExt::map(stream, |line| {
        Ok::<_, std::io::Error>(hyper::body::Frame::data(Bytes::from(line + "\n")))
    });
    let body = BodyExt::boxed(http_body_util::StreamBody::new(frames));
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/x-ndjson")
        .header("cache-control", "no-store")
        .body(body)
        .unwrap_or_else(|_| Response::new(BoxBody::default()))
}

/// One command-line operation, carried out on the shared store exactly as
/// `--ephemeral` would carry it out in memory, and reported to watchers.
/// Facts a proxy or the command line checked, to count and to show in the
/// feed: `{"client", "namespace", "facts": [[key, state], ...]}`.
async fn take_report(
    request: Request<Incoming>,
    events: Arc<Events>,
    side: Arc<crate::shared::Shared>,
) -> Response<BoxBody> {
    let Ok(collected) = request.into_body().collect().await else {
        return text(StatusCode::BAD_REQUEST, "the report could not be read\n");
    };
    let Ok(report) = serde_json::from_slice::<serde_json::Value>(&collected.to_bytes()) else {
        return text(StatusCode::BAD_REQUEST, "the report did not parse\n");
    };
    let client = report["client"].as_str().unwrap_or("unknown client");
    let ns = report["namespace"]
        .as_str()
        .filter(|n| crate::namespace::validate(n).is_ok())
        .unwrap_or(crate::namespace::FALLBACK);
    let mut checked = crate::facts::Checked::default();
    for pair in report["facts"].as_array().into_iter().flatten().take(1000) {
        let (Some(key), Some(state)) = (pair[0].as_str(), pair[1].as_str()) else {
            continue;
        };
        match state {
            "fresh" => checked.fresh += 1,
            "stale" => checked.stale += 1,
            "unverified" => checked.unverified += 1,
            _ => continue,
        }
        checked.facts.push((key.to_owned(), state.to_owned()));
    }
    crate::tools::dispatch::record_checked(&side, Some(&events), client, ns, &checked);
    json_response(
        StatusCode::OK,
        &serde_json::json!({ "recorded": checked.facts.len() }),
    )
}

async fn run_cli(
    request: Request<Incoming>,
    db: memfork_core::Db,
    events: Arc<Events>,
    side: Arc<crate::shared::Shared>,
) -> Response<BoxBody> {
    let bad = |why: String| {
        json_response(
            StatusCode::BAD_REQUEST,
            &serde_json::json!({ "error": why }),
        )
    };
    let bytes = match request.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => return bad(format!("the request could not be read: {e}")),
    };
    #[derive(serde::Deserialize)]
    struct Call {
        branch: String,
        command: Command,
        namespace: Option<String>,
    }
    let call: Call = match serde_json::from_slice(&bytes) {
        Ok(call) => call,
        Err(e) => return bad(format!("the request did not parse: {e}")),
    };
    if !call.command.allowed_in_script() {
        return bad(format!(
            "`memfork {}` is not an operation on the store",
            call.command.name()
        ));
    }
    let Call {
        branch,
        command,
        namespace,
    } = call;
    let (key, target) = crate::exec::target(&command, &branch);
    let name = command.name();
    let ctx = crate::exec::Context {
        writer: Some(CLI_WRITER.to_owned()),
        namespace: namespace
            .clone()
            .filter(|n| crate::namespace::validate(n).is_ok())
            .unwrap_or_else(|| crate::namespace::FALLBACK.to_owned()),
        shared: side,
        root: None,
    };
    let outcome =
        tokio::task::spawn_blocking(move || crate::exec::execute_in(&db, &branch, &command, &ctx))
            .await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(e) => Err(crate::exec::ExecError::Usage(format!(
            "the operation did not finish: {e}"
        ))),
    };
    events.publish(Event {
        operation: Some(name.to_owned()),
        key,
        branch: target,
        ok: outcome.is_ok(),
        error: outcome.as_ref().err().map(ToString::to_string),
        ..Event::about(CLI_WRITER, namespace.as_deref())
    });
    match outcome {
        Ok(outcome) => json_response(
            StatusCode::OK,
            &serde_json::json!({ "text": outcome.text, "json": outcome.json }),
        ),
        Err(e) => json_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            &serde_json::json!({ "error": e.to_string() }),
        ),
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
