//! Reading the event stream: what every client is doing, as it happens, as
//! lines of JSON another program can ingest (`docs/EVENTS.md`).
//!
//! ```sh
//! cargo build -p memfork
//! cargo run -p memfork --example event_stream -- target/debug/memfork
//! ```
//!
//! The argument is a `memfork` binary to start a daemon with; it defaults to
//! `memfork` on `PATH`. The daemon serves a temporary data directory, so
//! nothing here touches your own memory, and it is stopped at the end.

use std::path::PathBuf;
use std::time::Duration;

use memfork::client::Daemon;
use memfork::launch::Launch;
use memfork::{daemon, serve};
use serde_json::{json, Value as Json};

fn main() -> Result<(), String> {
    let binary = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "memfork".to_owned());
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let data = dir.path().join("data");

    // Start (or find) a daemon for this directory. Idle for 30 seconds and
    // it exits on its own; we stop it sooner below.
    let endpoint = daemon::ensure(&data, &Launch::program(PathBuf::from(&binary)), 30)
        .map_err(|e| e.to_string())?;
    let port = endpoint.port.ok_or("the daemon has no port")?;
    println!("daemon connected on 127.0.0.1:{port}");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let reader = Daemon::new(&endpoint)?;
        let writer = Daemon::new(&endpoint)?;

        // A few operations through the daemon's command-line endpoint, the
        // same one `memfork put` uses, spaced out so the stream is readable.
        let writes = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            for (command, key) in [
                ("put", "shop:decision:payments"),
                ("put", "shop:decision:emails"),
                ("fork", "attempt"),
            ] {
                let request = match command {
                    "put" => json!({
                        "branch": "main",
                        "command": { "Put": { "key": key, "value": "{}", "meta": [], "sources": [] } },
                        "namespace": "shop",
                    }),
                    _ => json!({
                        "branch": "main",
                        "command": { "Fork": { "name": key } },
                        "namespace": "shop",
                    }),
                };
                let _ = writer.post(serve::CLI_PATH, &request).await;
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });

        // Read: the hello line, then events, until three operations have
        // gone by.
        let mut seen = 0;
        reader
            .stream_lines(serve::EVENTS_PATH, |line| {
                let Ok(v) = serde_json::from_str::<Json>(line) else {
                    return true;
                };
                match v["kind"].as_str() {
                    Some("hello") => println!(
                        "hello: schema {}, version {}, {} client(s) connected",
                        v["schema"], v["version"], v["clients"].as_array().map_or(0, Vec::len)
                    ),
                    Some(kind) => {
                        println!(
                            "{} {kind:<12} {:<14} {:<8} {}",
                            v["time"].as_str().unwrap_or(""),
                            v["client"].as_str().unwrap_or(""),
                            v["operation"].as_str().unwrap_or("-"),
                            v["key"].as_str().or(v["branch"].as_str()).unwrap_or("")
                        );
                        if kind == "operation" {
                            seen += 1;
                        }
                    }
                    None => {}
                }
                seen < 3
            })
            .await?;
        let _ = writes.await;
        Ok::<(), String>(())
    })?;

    println!("{}", daemon::stop(&data)?);
    Ok(())
}
