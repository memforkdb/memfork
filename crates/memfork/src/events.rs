//! What the daemon is doing, as it happens, for `memfork watch` (DESIGN §5.3).
//!
//! Every tool call and every command-line operation the daemon carries out is
//! published here as an [`Event`], and every client session is tracked from
//! the moment it says who it is until it goes away. `memfork watch` reads the
//! stream over the daemon's loopback listener, with the same token as
//! everything else.
//!
//! Wall-clock time appears here and nowhere else in the daemon: an event is a
//! report for a person, not part of the store. Nothing here feeds back into a
//! commit id, a result or its order.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// How many events a slow watcher may fall behind before it misses some.
const BACKLOG: usize = 1024;

/// One thing that happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// When, as RFC 3339 in UTC.
    pub time: String,
    /// What kind of thing: `operation`, `connected` or `disconnected`.
    pub kind: String,
    /// The client's name as a person knows it.
    pub client: String,
    /// The name the client gave in MCP `initialize`, if it differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// The session's project namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// What was done: `put`, `handoff`, `resume`, `fork` and so on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// The key it was done to, if one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The branch it was done on, or the branch it created or removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// A word or two more, where the operation needs it: `fresh` or `stale`
    /// for a fact, who holds a task a claim could not take.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Whether it succeeded.
    pub ok: bool,
    /// Why not, when it did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A connected client, as `memfork watch` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connected {
    /// The client's name as a person knows it.
    pub client: String,
    /// Its project namespace.
    pub namespace: String,
}

/// The first thing a watcher is sent: the daemon, and who is connected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Always `hello`.
    pub kind: String,
    /// The daemon's version.
    pub version: String,
    /// The port it listens on.
    pub port: u16,
    /// Every connected client session, in the order they connected.
    pub clients: Vec<Connected>,
}

/// The hub: a broadcast of events, and the sessions currently connected.
#[derive(Debug)]
pub struct Events {
    sender: broadcast::Sender<Event>,
    sessions: Mutex<BTreeMap<u64, Connected>>,
    next: AtomicU64,
}

impl Default for Events {
    fn default() -> Self {
        Events {
            sender: broadcast::channel(BACKLOG).0,
            sessions: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(1),
        }
    }
}

impl Events {
    /// Publish an event. Nobody watching is not an error.
    pub fn publish(&self, event: Event) {
        let _ = self.sender.send(event);
    }

    /// Start receiving events.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }

    /// Who is connected right now.
    pub fn connected(&self) -> Vec<Connected> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// A session has said who it is. Returns the handle to pass to
    /// [`Events::left`].
    pub fn joined(&self, client_id: &str, namespace: &str) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let entry = Connected {
            client: crate::clients::display_for_writer(client_id),
            namespace: namespace.to_owned(),
        };
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, entry.clone());
        self.publish(Event {
            kind: "connected".to_owned(),
            ..Event::about(client_id, Some(namespace))
        });
        id
    }

    /// A session has gone.
    pub fn left(&self, handle: u64) {
        let gone = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&handle);
        if let Some(gone) = gone {
            self.publish(Event {
                kind: "disconnected".to_owned(),
                client: gone.client,
                namespace: Some(gone.namespace),
                ..Event::about("", None)
            });
        }
    }
}

impl Event {
    /// An event about `client_id`, stamped now, with everything else empty.
    pub fn about(client_id: &str, namespace: Option<&str>) -> Event {
        let client = crate::clients::display_for_writer(client_id);
        Event {
            time: now(),
            kind: "operation".to_owned(),
            client_id: (!client_id.is_empty() && client != client_id).then(|| client_id.to_owned()),
            client,
            namespace: namespace.map(str::to_owned),
            operation: None,
            key: None,
            branch: None,
            detail: None,
            ok: true,
            error: None,
        }
    }
}

/// The current time as RFC 3339 in UTC, to the millisecond.
pub fn now() -> String {
    jiff::Timestamp::now()
        .round(jiff::Unit::Millisecond)
        .map_or_else(|_| jiff::Timestamp::now().to_string(), |t| t.to_string())
}

/// An RFC 3339 time as local wall-clock time, `HH:MM:SS`, for a person.
pub fn local_clock(rfc3339: &str) -> String {
    rfc3339
        .parse::<jiff::Timestamp>()
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| rfc3339.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_come_and_go_and_say_so() {
        let hub = Events::default();
        let mut rx = hub.subscribe();
        let a = hub.joined("claude-code", "shop");
        let _b = hub.joined("some-client", "blog");
        assert_eq!(
            hub.connected(),
            vec![
                Connected {
                    client: "Claude Code".to_owned(),
                    namespace: "shop".to_owned()
                },
                Connected {
                    client: "some-client".to_owned(),
                    namespace: "blog".to_owned()
                },
            ]
        );
        hub.left(a);
        assert_eq!(hub.connected().len(), 1);

        let first = rx.try_recv().unwrap();
        assert_eq!(first.kind, "connected");
        assert_eq!(first.client, "Claude Code");
        assert_eq!(first.client_id.as_deref(), Some("claude-code"));
        let _ = rx.try_recv().unwrap();
        let gone = rx.try_recv().unwrap();
        assert_eq!(gone.kind, "disconnected");
        assert_eq!(gone.client, "Claude Code");
    }

    #[test]
    fn times_are_utc_and_read_back_as_a_clock() {
        let t = now();
        assert!(t.ends_with('Z'), "{t}");
        let clock = local_clock(&t);
        assert_eq!(clock.len(), 8, "{clock}");
        assert_eq!(clock.matches(':').count(), 2);
    }

    #[test]
    fn an_event_serialises_without_empty_fields() {
        let e = Event {
            operation: Some("put".to_owned()),
            key: Some("shop:decision:x".to_owned()),
            ..Event::about("cli", Some("shop"))
        };
        let json = serde_json::to_value(&e).unwrap();
        assert!(json.get("error").is_none());
        assert_eq!(json["operation"], "put");
        assert_eq!(json["kind"], "operation");
    }
}
