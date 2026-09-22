//! MemFork itself sends nothing anywhere. This is the part of that promise a
//! test can hold without a network: the code that can open a socket is
//! confined to a handful of named modules, each of which speaks only to
//! loopback, and nothing else in either crate can reach the network at all.
//!
//! It is a scan of the source, in the same spirit as the guard that keeps
//! tests out of the real data directory: a rule that is checked rather than
//! remembered. The other half — running the whole command surface with
//! outbound traffic blocked at the operating system — is the `no-network`
//! workflow, which needs a firewall and so lives in CI.
//!
//! The allow-list here is the one SECURITY.md names. Adding a module to it is
//! a change to what MemFork can do on a network, and is reviewed as such.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

/// Modules that may open a socket, and what each is for. Every one of them
/// speaks to `127.0.0.1` and nowhere else.
const MAY_USE_SOCKETS: &[(&str, &str)] = &[
    (
        "crates/memfork/src/serve.rs",
        "the daemon's loopback listener",
    ),
    (
        "crates/memfork/src/client.rs",
        "the command line's connection to the daemon",
    ),
    (
        "crates/memfork/src/proxy.rs",
        "a `memfork mcp` proxy's connection to the daemon",
    ),
    (
        "crates/memfork/src/daemon.rs",
        "the probe that asks a daemon to stop",
    ),
];

/// Symbols that open, bind or resolve a socket, or pull in a client that
/// does. Any of these outside the allow-list is a network capability nobody
/// reviewed.
const SOCKET_SYMBOLS: &[&str] = &[
    "TcpStream",
    "TcpListener",
    "UdpSocket",
    "UnixStream",
    "UnixListener",
    "ToSocketAddrs",
    "lookup_host",
    "hyper_util::client",
    "reqwest",
    "ureq",
    "curl::",
    "getaddrinfo",
    "std::net::",
    "tokio::net::",
];

/// Ways of naming a host that is not this machine. Even the allow-listed
/// modules may not use them: they connect by address, to loopback.
const REMOTE_NAMING: &[&str] = &[
    "ToSocketAddrs",
    "lookup_host",
    "dns",
    "\"localhost",
    "0.0.0.0",
    "Ipv4Addr::UNSPECIFIED",
    "Ipv6Addr::UNSPECIFIED",
    "in_addr_any",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the workspace root")
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("readable").flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The source with comments and string literals blanked, so a word in a
/// comment or a message is not mistaken for a use of it.
fn code_only(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '"' => {
                let mut prev = '"';
                for c in chars.by_ref() {
                    if c == '"' && prev != '\\' {
                        break;
                    }
                    prev = if prev == '\\' && c == '\\' { ' ' } else { c };
                }
                out.push_str("\"\"");
            }
            other => out.push(other),
        }
    }
    out
}

#[test]
fn only_the_named_modules_can_open_a_socket_and_only_to_loopback() {
    let root = workspace_root();
    let mut sources = Vec::new();
    rust_sources(
        &root.join("crates").join("memfork").join("src"),
        &mut sources,
    );
    rust_sources(
        &root.join("crates").join("memfork-core").join("src"),
        &mut sources,
    );
    rust_sources(
        &root.join("crates").join("memfork-py").join("src"),
        &mut sources,
    );
    assert!(sources.len() > 30, "found only {} sources", sources.len());

    let mut offences = Vec::new();
    let mut seen_allowed = Vec::new();
    for path in &sources {
        let rel = relative(&root, path);
        let code = code_only(&std::fs::read_to_string(path).unwrap());
        let allowed = MAY_USE_SOCKETS.iter().any(|(m, _)| *m == rel);
        for symbol in SOCKET_SYMBOLS {
            if code.contains(symbol) {
                if allowed {
                    seen_allowed.push(rel.clone());
                } else {
                    offences.push(format!(
                        "{rel} uses `{symbol}` and is not in the allow-list"
                    ));
                }
            }
        }
        if allowed {
            for naming in REMOTE_NAMING {
                if code.contains(naming) {
                    offences.push(format!(
                        "{rel} names a host with `{naming}`; the allow-list is loopback only"
                    ));
                }
            }
            let source = std::fs::read_to_string(path).unwrap();
            assert!(
                source.contains("127.0.0.1") || source.contains("Ipv4Addr::LOCALHOST"),
                "{rel} is allowed a socket but does not name loopback"
            );
        }
    }
    assert!(
        offences.is_empty(),
        "network capability outside the allow-list:\n  {}",
        offences.join("\n  ")
    );
    // Every allow-listed module is still one that needs to be: an entry that
    // no longer opens a socket should be removed, not carried.
    for (module, _) in MAY_USE_SOCKETS {
        assert!(
            seen_allowed.iter().any(|s| s == module),
            "{module} is allow-listed but opens no socket; take it off the list"
        );
    }
}

#[test]
fn no_http_client_or_tls_stack_is_a_dependency() {
    // The daemon speaks plain HTTP on loopback. A TLS stack or a general HTTP
    // client in the tree would be a capability to reach out with, and there
    // is no reason for one.
    let root = workspace_root();
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    for crate_name in [
        "reqwest",
        "ureq",
        "curl",
        "native-tls",
        "rustls",
        "openssl",
        "hyper-tls",
        "hyper-rustls",
        "trust-dns",
        "hickory",
    ] {
        assert!(
            !lock.contains(&format!("name = \"{crate_name}\"")),
            "`{crate_name}` is in Cargo.lock; MemFork has no use for it"
        );
    }
}

#[test]
fn security_md_names_the_same_modules() {
    // The allow-list is a promise to people, so the document they read has
    // to carry the same list, and change when it changes.
    let root = workspace_root();
    let security = std::fs::read_to_string(root.join("SECURITY.md")).unwrap();
    for (module, _) in MAY_USE_SOCKETS {
        let leaf = module.rsplit('/').next().unwrap();
        assert!(
            security.contains(&format!("`{leaf}`")),
            "SECURITY.md does not name `{leaf}` among the modules that open sockets"
        );
    }
    assert!(
        security.to_ascii_lowercase().contains("no telemetry"),
        "SECURITY.md must say plainly that there is no telemetry"
    );
}
