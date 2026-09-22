//! Executing a tool call against the engine.
//!
//! One entry point, [`Session::call`], shared by the MCP server and by
//! `memfork call`, so the two can never diverge in behaviour.

use std::sync::{Arc, Mutex};

use memfork_core::{Db, MergeKind, MergePolicy, Value, WRITTEN_BY};
use serde_json::{json, Value as Json};

use super::handoff;
use super::schema::JsonObject;
use crate::board::{ClaimOutcome, Who};
use crate::events::{Event, Events};
use crate::shared::Shared;
use crate::{facts, find, lessons, namespace};

/// Longest writer name recorded, in characters.
const MAX_WRITER_CHARS: usize = 128;

/// Metadata keys under this prefix belong to MemFork, and callers may not set
/// them: a model that could write `memfork.by` could put words in another
/// client's mouth.
const RESERVED_META_PREFIX: &str = "memfork.";

/// Why a tool call could not be carried out.
#[derive(Debug)]
pub enum ToolError {
    /// No tool by that name.
    UnknownTool(String),
    /// The arguments were missing, of the wrong type or out of range.
    BadArguments(String),
    /// The engine refused the operation.
    Engine(memfork_core::Error),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::UnknownTool(name) => {
                write!(
                    f,
                    "no tool named `{name}`; available tools: {}",
                    super::names().join(", ")
                )
            }
            ToolError::BadArguments(m) => write!(f, "{m}"),
            ToolError::Engine(e) => write!(f, "{e}"),
        }
    }
}

impl From<memfork_core::Error> for ToolError {
    fn from(e: memfork_core::Error) -> Self {
        ToolError::Engine(e)
    }
}

/// One caller's view of a database: the engine, the branch they are on, the
/// project they are working in, and who they are.
///
/// The current branch is per-session state, not database state, so two clients
/// can sit on different branches of the same database. The same goes for the
/// namespace and the writer: two clients in two projects share one store.
#[derive(Debug)]
pub struct Session {
    db: Db,
    branch: Mutex<String>,
    namespace: Mutex<String>,
    writer: Mutex<Option<String>>,
    /// Where this session's activity is reported, in the daemon, and its
    /// place in the list of connected clients once it has said who it is.
    events: Option<Arc<Events>>,
    joined: Mutex<Option<u64>>,
    /// Leases, statistics and fact hashes, shared by every session in the
    /// process.
    shared: Arc<Shared>,
    /// This session, for owning claims: random, and never committed.
    session_id: Mutex<String>,
    /// The project's directory, when this process can see it: an
    /// `--ephemeral` server or the command line. The daemon has none; its
    /// proxies check facts instead.
    root: Option<std::path::PathBuf>,
}

/// Record what checking facts found, wherever it was checked: counted for
/// `client` in `ns`, and one `fact` event each for `memfork watch`.
pub fn record_checked(
    shared: &Shared,
    events: Option<&Events>,
    client: &str,
    ns: &str,
    checked: &facts::Checked,
) {
    if checked.is_empty() {
        return;
    }
    shared.sidecar.count(ns, client, |c| {
        c.facts_fresh += checked.fresh;
        c.facts_stale += checked.stale;
        c.facts_unverified += checked.unverified;
    });
    if let Some(events) = events {
        for (key, state) in &checked.facts {
            events.publish(Event {
                operation: Some("fact".to_owned()),
                key: Some(key.clone()),
                detail: Some(state.clone()),
                ..Event::about(client, Some(ns))
            });
        }
    }
}

/// A random id for a session: sixteen random bytes as hex. Only ever held in
/// memory, beside the store, so it never reaches a commit id.
pub fn new_session_id() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // No randomness to be had: fall back to something that is at least
        // distinct within this process.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return format!("{:016x}{:016x}", std::process::id(), n);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Session {
    /// Start a session on the database's default branch, in the fallback
    /// namespace, with no writer recorded.
    pub fn new(db: Db) -> Self {
        let branch = db.default_branch().to_owned();
        Session {
            db,
            branch: Mutex::new(branch),
            namespace: Mutex::new(namespace::FALLBACK.to_owned()),
            writer: Mutex::new(None),
            events: None,
            joined: Mutex::new(None),
            shared: Shared::in_memory(),
            session_id: Mutex::new(new_session_id()),
            root: None,
        }
    }

    /// Share leases, statistics and fact hashes with every other session in
    /// this process.
    #[must_use]
    pub fn sharing(mut self, shared: Arc<Shared>) -> Self {
        self.shared = shared;
        self
    }

    /// Check facts against the files under `root`, in this process.
    #[must_use]
    pub fn in_project(mut self, root: std::path::PathBuf) -> Self {
        self.root = Some(root);
        self
    }

    /// What this session shares with the rest of the process.
    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Take the session id a proxy chose, so claims survive its reconnects.
    pub fn set_session_id(&self, id: &str) {
        let id: String = id.trim().chars().take(64).collect();
        if !id.is_empty() {
            *self.session_id.lock().unwrap_or_else(|e| e.into_inner()) = id;
        }
    }

    /// Who this session is, for claims.
    pub fn who(&self) -> Who {
        Who {
            client: self.writer().unwrap_or_else(|| "unknown client".to_owned()),
            session: self
                .session_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    /// Count something for this session's client in this session's project.
    fn count(&self, change: impl FnOnce(&mut crate::sidecar::Counters)) {
        let client = self.writer().unwrap_or_else(|| "unknown client".to_owned());
        self.shared
            .sidecar
            .count(&self.namespace(), &client, change);
    }

    /// Record what checking facts found: statistics, and one event per fact.
    pub fn facts_checked(&self, checked: &facts::Checked) {
        let who = self.writer().unwrap_or_else(|| "unknown client".to_owned());
        record_checked(
            &self.shared,
            self.events.as_deref(),
            &who,
            &self.namespace(),
            checked,
        );
    }

    /// Report this session's activity to `events`, for `memfork watch`.
    #[must_use]
    pub fn reporting_to(mut self, events: Arc<Events>) -> Self {
        self.events = Some(events);
        self
    }

    /// The client has said who it is: list it as connected. Once only.
    pub fn announce(&self) {
        let Some(events) = &self.events else {
            return;
        };
        let mut joined = self.joined.lock().unwrap_or_else(|e| e.into_inner());
        if joined.is_none() {
            let who = self.writer().unwrap_or_else(|| "unknown client".to_owned());
            *joined = Some(events.joined(&who, &self.namespace()));
        }
    }

    /// Start a session in a given namespace.
    pub fn in_namespace(db: Db, namespace: impl Into<String>) -> Self {
        let session = Session::new(db);
        session.set_namespace(namespace.into());
        session
    }

    /// The project namespace the handoff and resume tools work in.
    pub fn namespace(&self) -> String {
        self.namespace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Move the session to another namespace. The caller has validated it.
    pub fn set_namespace(&self, name: String) {
        *self.namespace.lock().unwrap_or_else(|e| e.into_inner()) = name;
    }

    /// Who this session's writes are recorded as, if anyone.
    pub fn writer(&self) -> Option<String> {
        self.writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Record this session's writes as coming from `name` — for the MCP
    /// server, the name the client gave when it connected. Blank names are
    /// ignored, and long ones cut, since this is whatever a client sent.
    pub fn set_writer(&self, name: &str) {
        let name: String = name.trim().chars().take(MAX_WRITER_CHARS).collect();
        if !name.is_empty() {
            *self.writer.lock().unwrap_or_else(|e| e.into_inner()) = Some(name);
        }
    }

    /// The database this session reads and writes.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// The branch this session is currently on.
    pub fn branch(&self) -> String {
        // A poisoned lock means another thread panicked while holding it; the
        // branch name behind it is still a valid `String`, so recovering is
        // better than propagating a panic into an MCP response.
        self.branch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_branch(&self, name: String) {
        *self.branch.lock().unwrap_or_else(|e| e.into_inner()) = name;
    }

    /// Run one tool call and return its result as JSON.
    ///
    /// Every result carries `current_branch`, added here rather than by each
    /// tool so it cannot be forgotten. Without it a caller that is unsure
    /// which branch it is on has to spend a call finding out, and one that has
    /// just forked has to confirm the switch — two of the redundant calls seen
    /// in practice.
    pub fn call(&self, name: &str, args: &JsonObject) -> Result<Json, ToolError> {
        let before = self.branch();
        let outcome = self.dispatch(name, args);
        self.report(name, args, &before, &outcome);
        let mut result = outcome?;
        // Facts are checked where the files are. Here only if this process
        // can see them; otherwise the proxy does it on the way out.
        if let Some(root) = &self.root {
            let checked = facts::check(&mut result, root, &self.shared.hasher);
            self.facts_checked(&checked);
        }
        if let Json::Object(map) = &mut result {
            map.insert("current_branch".to_owned(), json!(self.branch()));
        }
        Ok(result)
    }

    /// Tell `memfork watch` what just happened, if anyone is listening.
    fn report(
        &self,
        name: &str,
        args: &JsonObject,
        before: &str,
        outcome: &Result<Json, ToolError>,
    ) {
        let Some(events) = &self.events else {
            return;
        };
        let text = |field: &str| args.get(field).and_then(Json::as_str).map(str::to_owned);
        // The branch a person would want to see: the one created, removed or
        // merged for branch operations, the one acted on for everything else.
        let branch = match name {
            "memfork_fork" | "memfork_checkout" | "memfork_discard" => text("name"),
            "memfork_merge" => text("source").map(|s| {
                format!(
                    "{s} -> {}",
                    text("target").unwrap_or_else(|| before.to_owned())
                )
            }),
            _ => Some(text("branch").unwrap_or_else(|| before.to_owned())),
        };
        // A handoff chooses its own key, so that one comes from the result.
        let key = text("key").or_else(|| text("prefix")).or_else(|| {
            outcome
                .as_ref()
                .ok()
                .and_then(|r| r.get("key"))
                .and_then(Json::as_str)
                .map(str::to_owned)
        });
        let namespace = match name {
            "memfork_handoff" | "memfork_resume" | "memfork_task" => {
                text("namespace").unwrap_or_else(|| self.namespace())
            }
            _ => self.namespace(),
        };
        let who = self.writer().unwrap_or_else(|| "unknown client".to_owned());
        let result = outcome.as_ref().ok();
        // The task board speaks in its actions; a renewal is housekeeping and
        // stays out of the feed.
        let operation = match name {
            "memfork_task" => match text("action").as_deref() {
                Some("renew") => return,
                Some(action) => action.to_owned(),
                None => "task".to_owned(),
            },
            other => other.trim_start_matches("memfork_").to_owned(),
        };
        let detail = result.and_then(|r| {
            r.get("held_by")
                .and_then(Json::as_str)
                .map(|h| format!("held by {h}"))
        });
        let ok =
            outcome.is_ok() && !result.is_some_and(|r| r.get("claimed") == Some(&json!(false)));
        events.publish(Event {
            operation: Some(operation),
            key,
            branch,
            detail,
            ok,
            error: outcome.as_ref().err().map(ToString::to_string),
            ..Event::about(&who, Some(&namespace))
        });
        // A discard that left a lesson says so as a second event.
        if let Some(lesson) = result.and_then(|r| r.get("lesson")) {
            events.publish(Event {
                operation: Some("lesson".to_owned()),
                key: lesson.get("key").and_then(Json::as_str).map(str::to_owned),
                branch: lesson
                    .get("branch")
                    .and_then(Json::as_str)
                    .map(str::to_owned),
                ..Event::about(&who, Some(&namespace))
            });
        }
    }

    fn dispatch(&self, name: &str, args: &JsonObject) -> Result<Json, ToolError> {
        if super::find(name).is_none() {
            return Err(ToolError::UnknownTool(name.to_owned()));
        }
        let branch = match opt_str(args, "branch")? {
            Some(b) => b.to_owned(),
            None => self.branch(),
        };

        match name {
            "memfork_put" => {
                let key = req_str(args, "key")?;
                let mut value = Value::new(req_str(args, "value")?.to_owned());
                if let Some(i) = opt_f32(args, "importance")? {
                    value = value.with_importance(i);
                }
                if let Some(e) = opt_f32_array(args, "embedding")? {
                    value = value.with_embedding(e);
                }
                if let Some(t) = opt_u64(args, "ttl_commits")? {
                    value = value.with_ttl_commits(t);
                }
                for (k, v) in opt_meta(args, "meta")? {
                    if k.starts_with(RESERVED_META_PREFIX) {
                        return Err(ToolError::BadArguments(format!(
                            "`meta.{k}` is reserved: keys starting with \
                             `{RESERVED_META_PREFIX}` are set by MemFork itself"
                        )));
                    }
                    value = value.with_meta(k, v);
                }
                if let Some(writer) = self.writer() {
                    value = value.with_meta(WRITTEN_BY, writer);
                }
                // A fact: the paths are committed; the hashes they had are
                // kept beside the store, never in it.
                let sources = match opt_str_list(args, "sources")? {
                    Some(raw) if !raw.is_empty() => {
                        Some(facts::normalise(&raw).map_err(ToolError::BadArguments)?)
                    }
                    _ => None,
                };
                if let Some(sources) = &sources {
                    value = value.with_meta(facts::SOURCES_META, facts::sources_meta(sources));
                }
                // Read first so the result can say whether this replaced
                // something. It is one in-memory lookup, and it saves the
                // caller a round trip to find out.
                let replaced = self.db.get(&branch, key)?.is_some();
                let stored = text(&value.value);
                let recorded = match &sources {
                    Some(sources) => {
                        let hashes = match args.get("source_hashes") {
                            Some(Json::Object(given)) => Some(
                                sources
                                    .iter()
                                    .map(|s| {
                                        let hash = given.get(s).and_then(Json::as_str);
                                        (s.clone(), hash.map(str::to_owned))
                                    })
                                    .collect(),
                            ),
                            _ => self
                                .root
                                .as_ref()
                                .map(|root| self.shared.hasher.record(root, sources)),
                        };
                        if let Some(hashes) = hashes {
                            let id = facts::record_id(key, &value.value, sources);
                            self.shared.sidecar.record_fact(&id, hashes);
                            true
                        } else {
                            false
                        }
                    }
                    None => false,
                };
                let commit = self.db.put(&branch, key, value)?;
                Ok(json!({
                    "branch": branch,
                    "key": key,
                    "stored": true,
                    // Echoed so the write is self-evidencing: a caller that
                    // would otherwise read the key back to check can see it
                    // here instead.
                    "value": stored,
                    "replaced": replaced,
                    "commit": commit.to_hex(),
                    "sources": sources,
                    "sources_recorded": sources.is_some().then_some(recorded),
                }))
            }

            "memfork_get" => {
                let key = req_str(args, "key")?;
                match self.db.get(&branch, key)? {
                    Some(entry) => Ok(json!({
                        "branch": branch,
                        "key": key,
                        "found": true,
                        "value": text(&entry.value),
                        "importance": entry.importance,
                        "created_seq": entry.created_seq,
                        "ttl_commits": entry.ttl_commits,
                        "has_embedding": entry.embedding.is_some(),
                        "written_by": entry.meta.get(WRITTEN_BY),
                        "meta": entry.meta,
                    }))
                    .map(|mut got| {
                        handoff::with_fact_fields(&mut got, key, &entry, &self.shared.sidecar);
                        got
                    }),
                    None => Ok(json!({ "branch": branch, "key": key, "found": false })),
                }
            }

            "memfork_delete" => {
                let key = req_str(args, "key")?;
                let existed = self.db.get(&branch, key)?.is_some();
                let commit = self.db.delete(&branch, key)?;
                Ok(json!({
                    "branch": branch,
                    "key": key,
                    "deleted": existed,
                    "commit": commit.to_hex(),
                }))
            }

            "memfork_list" => {
                let prefix = opt_str(args, "prefix")?.unwrap_or("");
                let limit = opt_usize(args, "limit")?;
                let entries = self.db.list(&branch, prefix, limit)?;
                Ok(json!({
                    "branch": branch,
                    "prefix": prefix,
                    "count": entries.len(),
                    "entries": entries.iter().map(|(k, e)| json!({
                        "key": k,
                        "value": text(&e.value),
                        "importance": e.importance,
                    })).collect::<Vec<_>>(),
                }))
            }

            "memfork_search" if args.get("text").is_some_and(|t| !t.is_null()) => {
                if args.get("embedding").is_some_and(|e| !e.is_null()) {
                    return Err(ToolError::BadArguments(
                        "give either `text` or `embedding`, not both".to_owned(),
                    ));
                }
                let query = req_str(args, "text")?;
                let k = opt_usize(args, "k")?
                    .unwrap_or(find::DEFAULT_RESULTS)
                    .clamp(1, find::MAX_RESULTS);
                let prefix = opt_str(args, "prefix")?.unwrap_or("");
                let entries = self.db.list(&branch, prefix, None)?;
                let texts: Vec<String> = entries.iter().map(|(_, e)| text(&e.value)).collect();
                let docs: Vec<find::Doc<'_>> = entries
                    .iter()
                    .zip(&texts)
                    .map(|((k, _), t)| find::Doc { key: k, text: t })
                    .collect();
                let hits = find::rank(&docs, query, k);
                self.count(|c| c.finds += 1);
                let by_key: std::collections::BTreeMap<
                    &str,
                    (&std::sync::Arc<memfork_core::Entry>, &String),
                > = entries
                    .iter()
                    .zip(&texts)
                    .map(|((k, e), t)| (k.as_str(), (e, t)))
                    .collect();
                Ok(json!({
                    "branch": branch,
                    "text": query,
                    "count": hits.len(),
                    "searched": entries.len(),
                    "hits": hits.iter().filter_map(|h| {
                        let (entry, value) = by_key.get(h.key.as_str())?;
                        let mut hit = json!({
                            "key": h.key,
                            "score": h.score,
                            "snippet": find::snippet(value, h.first_match),
                            "by": entry.meta.get(WRITTEN_BY),
                        });
                        handoff::with_fact_fields(&mut hit, &h.key, entry, &self.shared.sidecar);
                        Some(hit)
                    }).collect::<Vec<_>>(),
                }))
            }

            "memfork_search" => {
                let query = match opt_f32_array(args, "embedding")? {
                    Some(q) => q,
                    None => {
                        return Err(ToolError::BadArguments(
                            "give `text` to search by words, or `embedding` to search by a \
                             vector"
                                .to_owned(),
                        ))
                    }
                };
                let k = opt_usize(args, "k")?.unwrap_or(10);
                let prefix = opt_str(args, "prefix")?;
                let hits = self.db.search(&branch, &query, k, prefix)?;
                Ok(json!({
                    "branch": branch,
                    "count": hits.len(),
                    "hits": hits.iter().map(|h| json!({
                        "key": h.key,
                        "score": h.score,
                        "value": text(&h.entry.value),
                    })).collect::<Vec<_>>(),
                }))
            }

            "memfork_fork" => {
                let new_branch = req_str(args, "name")?;
                let from = opt_str(args, "from")?.unwrap_or(&branch).to_owned();
                let head = match opt_u64(args, "at_seq")? {
                    Some(seq) => self.db.fork_at(&from, seq, new_branch)?,
                    None => self.db.fork(&from, new_branch)?,
                };
                // Forking is what you do before trying something, so land on
                // the new branch rather than making the caller switch.
                self.set_branch(new_branch.to_owned());
                Ok(json!({
                    "name": new_branch,
                    "from": from,
                    "commit": head.to_hex(),
                }))
            }

            "memfork_checkout" => {
                let name = req_str(args, "name")?;
                if !self.db.has_branch(name) {
                    return Err(ToolError::Engine(memfork_core::Error::NoSuchBranch(
                        name.to_owned(),
                    )));
                }
                self.set_branch(name.to_owned());
                let view = self.db.read(name)?;
                Ok(json!({
                    "seq": view.seq(),
                    "count": view.len(),
                }))
            }

            "memfork_merge" => {
                let source = req_str(args, "source")?;
                let target = opt_str(args, "target")?.unwrap_or(&branch).to_owned();
                let policy = match opt_str(args, "policy")? {
                    None => MergePolicy::Fail,
                    Some(p) => MergePolicy::parse(p).ok_or_else(|| {
                        ToolError::BadArguments(format!(
                            "`policy` must be one of fail, ours or theirs; got `{p}`"
                        ))
                    })?,
                };
                match self.db.merge(source, &target, policy) {
                    Ok(outcome) => Ok(json!({
                        "source": source,
                        "target": target,
                        "policy": policy.as_str(),
                        "result": match outcome.kind {
                            MergeKind::UpToDate => "up_to_date",
                            MergeKind::FastForward => "fast_forward",
                            MergeKind::Merged => "merged",
                        },
                        "commit": outcome.head.to_hex(),
                        "changed": outcome.changed,
                        "conflicts": outcome.conflicts,
                    })),
                    // A conflict is an answer, not a failure: report the keys so
                    // the caller can look at them and pick a policy.
                    Err(memfork_core::Error::MergeConflict { keys }) => Ok(json!({
                        "source": source,
                        "target": target,
                        "policy": policy.as_str(),
                        "result": "conflict",
                        "conflicts": keys,
                        "nothing_changed": true,
                        "hint": "Inspect the conflicting keys, then merge again with \
                                 policy `ours` to keep the target's values or `theirs` \
                                 to take the source's.",
                    })),
                    Err(e) => Err(ToolError::Engine(e)),
                }
            }

            "memfork_discard" => {
                let name = req_str(args, "name")?;
                // The lesson goes to the parent first: if the discard then
                // fails, the lesson is still true, and nothing is lost.
                let lesson = match opt_str(args, "lesson")? {
                    Some(raw) => {
                        if !self.db.has_branch(name) {
                            return Err(ToolError::Engine(memfork_core::Error::NoSuchBranch(
                                name.to_owned(),
                            )));
                        }
                        let line = lessons::tidy(raw).map_err(ToolError::BadArguments)?;
                        let rec = lessons::record(
                            &self.db,
                            name,
                            &line,
                            &self.namespace(),
                            self.writer().as_deref(),
                        )?;
                        self.count(|c| c.lessons_recorded += 1);
                        Some(json!({
                            "key": rec.key,
                            "branch": rec.branch,
                            "lesson": line,
                            "commit": rec.commit.to_hex(),
                        }))
                    }
                    None => None,
                };
                self.db.discard(name)?;
                // Do not strand the session on a branch that no longer exists.
                let moved = if self.branch() == name {
                    self.set_branch(self.db.default_branch().to_owned());
                    true
                } else {
                    false
                };
                Ok(json!({
                    "name": name,
                    "discarded": true,
                    "switched_branch": moved,
                    "lesson": lesson,
                }))
            }

            "memfork_branches" => {
                let current = self.branch();
                Ok(json!({
                    "branches": self.db.branches().iter().map(|b| json!({
                        "name": b.name,
                        "commit": b.head.to_hex(),
                        "seq": b.seq,
                        "count": b.key_count,
                        "is_default": b.is_default,
                        "is_current": b.name == current,
                    })).collect::<Vec<_>>(),
                }))
            }

            "memfork_log" => {
                let limit = opt_usize(args, "limit")?;
                let entries = self.db.log(&branch, limit)?;
                Ok(json!({
                    "branch": branch,
                    "entries": entries.iter().map(|e| json!({
                        "commit": e.id.to_hex(),
                        "seq": e.seq,
                        "message": e.message,
                        "changes": e.op_count,
                        "count": e.key_count,
                        "is_merge": e.parents.len() > 1,
                    })).collect::<Vec<_>>(),
                }))
            }

            "memfork_at" => {
                let seq = req_u64(args, "seq")?;
                let view = self.db.at(&branch, seq)?;
                match opt_str(args, "key")? {
                    Some(key) => Ok(json!({
                        "branch": branch,
                        "seq": seq,
                        "key": key,
                        "found": view.get(key).is_some(),
                        "value": view.get(key).map(|e| text(&e.value)),
                    })),
                    None => {
                        let prefix = opt_str(args, "prefix")?.unwrap_or("");
                        let entries = view.list(prefix, None);
                        Ok(json!({
                            "branch": branch,
                            "seq": seq,
                            "commit": view.commit_id().to_hex(),
                            "count": entries.len(),
                            "entries": entries.iter().map(|(k, e)| json!({
                                "key": k,
                                "value": text(&e.value),
                            })).collect::<Vec<_>>(),
                        }))
                    }
                }
            }

            "memfork_diff" => {
                let a = req_str(args, "a")?;
                let b = req_str(args, "b")?;
                let changes = self.db.diff(a, b)?;
                Ok(json!({
                    "a": a,
                    "b": b,
                    "count": changes.len(),
                    "changes": changes.iter().map(|c| json!({
                        "key": c.key,
                        "change": match c.kind {
                            memfork_core::ChangeKind::Added => "added",
                            memfork_core::ChangeKind::Removed => "removed",
                            memfork_core::ChangeKind::Modified => "modified",
                        },
                    })).collect::<Vec<_>>(),
                }))
            }

            "memfork_handoff" => {
                let ns = self.namespace_arg(args)?;
                let note = handoff::Handoff {
                    summary: req_str(args, "summary")?.to_owned(),
                    done: opt_str_array(args, "done")?,
                    next: opt_str_array(args, "next")?,
                    blockers: opt_str_array(args, "blockers")?,
                    questions: opt_str_array(args, "questions")?,
                };
                let written =
                    handoff::write(&self.db, &branch, &ns, &note, self.writer().as_deref())?;
                Ok(json!({
                    "namespace": ns,
                    "branch": branch,
                    "key": written.key,
                    "number": written.number,
                    "stored": true,
                    "commit": written.commit.to_hex(),
                }))
            }

            "memfork_resume" => {
                let ns = self.namespace_arg(args)?;
                let ask = handoff::Ask {
                    task: opt_str(args, "task")?.map(str::to_owned),
                    budget: opt_usize(args, "budget")?,
                    current_branch: Some(self.branch()),
                };
                let brief =
                    handoff::briefing_with(&self.db, &branch, &ns, &ask, Some(&self.shared))?;
                let size = brief.to_string().len() as u64;
                let memory: u64 = self
                    .db
                    .list(&branch, &format!("{ns}{}", namespace::SEPARATOR), None)?
                    .iter()
                    .map(|(k, e)| (k.len() + e.value.len()) as u64)
                    .sum();
                let lessons_served = brief["lessons"].as_array().map_or(0, Vec::len) as u64;
                self.count(|c| {
                    c.briefings += 1;
                    c.briefing_bytes += size;
                    c.memory_bytes += memory;
                    c.lessons_served += lessons_served;
                });
                Ok(brief)
            }

            "memfork_task" => self.task(&branch, args),

            // `find` above already rejected anything not in the registry, so a
            // name reaching here means the registry and this match disagree.
            other => Err(ToolError::UnknownTool(other.to_owned())),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let (Some(events), Some(handle)) = (
            &self.events,
            *self.joined.lock().unwrap_or_else(|e| e.into_inner()),
        ) {
            events.left(handle);
        }
    }
}

impl Session {
    /// `memfork_task`: the task board.
    fn task(&self, branch: &str, args: &JsonObject) -> Result<Json, ToolError> {
        let action = req_str(args, "action")?;
        let ns = self.namespace_arg(args)?;
        let board = &self.shared.board;
        let bad = |e: crate::board::BoardError| match e {
            crate::board::BoardError::Bad(m) => ToolError::BadArguments(m),
            crate::board::BoardError::Engine(e) => ToolError::Engine(e),
        };
        let key = || -> Result<String, ToolError> {
            let id = req_str(args, "id")?;
            Ok(crate::board::task_key(&ns, id))
        };
        let who = self.who();
        match action {
            "add" => board
                .add(
                    &self.db,
                    branch,
                    &ns,
                    crate::board::NewTask {
                        id: opt_str(args, "id")?,
                        title: opt_str(args, "title")?.unwrap_or(""),
                        detail: opt_str(args, "detail")?,
                    },
                    self.writer().as_deref(),
                )
                .map_err(bad),
            "claim" => {
                let seconds =
                    opt_u64(args, "lease_seconds")?.unwrap_or(crate::board::DEFAULT_LEASE_SECONDS);
                let (result, outcome) = board
                    .claim(&self.db, branch, &key()?, &who, seconds)
                    .map_err(bad)?;
                match outcome {
                    ClaimOutcome::Claimed => self.count(|c| c.claims += 1),
                    ClaimOutcome::Held => self.count(|c| c.claim_conflicts += 1),
                    ClaimOutcome::Done => {}
                }
                Ok(result)
            }
            "renew" => Ok(board.renew(&key()?, &who)),
            "release" => board.release(&self.db, branch, &key()?, &who).map_err(bad),
            "done" => board.done(&self.db, branch, &key()?, &who).map_err(bad),
            "list" => board
                .list(
                    &self.db,
                    branch,
                    &ns,
                    opt_str(args, "status")?.unwrap_or("unfinished"),
                )
                .map_err(bad),
            other => Err(ToolError::BadArguments(format!(
                "`action` must be add, claim, renew, release, done or list; got `{other}`"
            ))),
        }
    }

    /// The namespace a handoff or resume call works in: the one it names, if
    /// valid, else the session's.
    fn namespace_arg(&self, args: &JsonObject) -> Result<String, ToolError> {
        match opt_str(args, "namespace")? {
            None => Ok(self.namespace()),
            Some(given) => {
                namespace::validate(given).map_err(|why| {
                    ToolError::BadArguments(format!("`namespace` `{given}` is not usable: {why}"))
                })?;
                Ok(given.to_owned())
            }
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn missing(field: &str, wanted: &str) -> ToolError {
    ToolError::BadArguments(format!("`{field}` is required and must be {wanted}"))
}

fn wrong(field: &str, wanted: &str, got: &Json) -> ToolError {
    ToolError::BadArguments(format!("`{field}` must be {wanted}, got {got}"))
}

fn req_str<'a>(args: &'a JsonObject, field: &str) -> Result<&'a str, ToolError> {
    match args.get(field) {
        Some(Json::String(s)) => Ok(s),
        Some(other) => Err(wrong(field, "a string", other)),
        None => Err(missing(field, "a string")),
    }
}

fn opt_str<'a>(args: &'a JsonObject, field: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(field) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) => Ok(Some(s)),
        Some(other) => Err(wrong(field, "a string", other)),
    }
}

fn req_u64(args: &JsonObject, field: &str) -> Result<u64, ToolError> {
    match args.get(field) {
        Some(v) => as_u64(field, v),
        None => Err(missing(field, "a whole number")),
    }
}

fn opt_u64(args: &JsonObject, field: &str) -> Result<Option<u64>, ToolError> {
    match args.get(field) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => as_u64(field, v).map(Some),
    }
}

fn as_u64(field: &str, v: &Json) -> Result<u64, ToolError> {
    // Some clients send whole numbers as JSON floats, so 3.0 is accepted and
    // 3.5 is not.
    match v {
        Json::Number(n) => match n.as_u64() {
            Some(u) => Ok(u),
            None => match n.as_f64() {
                Some(f) if f >= 0.0 && f.fract() == 0.0 && f <= u64::MAX as f64 => Ok(f as u64),
                _ => Err(wrong(field, "a whole number of zero or more", v)),
            },
        },
        other => Err(wrong(field, "a whole number of zero or more", other)),
    }
}

fn opt_usize(args: &JsonObject, field: &str) -> Result<Option<usize>, ToolError> {
    Ok(opt_u64(args, field)?.map(|v| usize::try_from(v).unwrap_or(usize::MAX)))
}

fn opt_f32(args: &JsonObject, field: &str) -> Result<Option<f32>, ToolError> {
    match args.get(field) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Number(n)) => match n.as_f64() {
            Some(f) => Ok(Some(f as f32)),
            None => Err(wrong(field, "a number", &args[field])),
        },
        Some(other) => Err(wrong(field, "a number", other)),
    }
}

fn opt_f32_array(args: &JsonObject, field: &str) -> Result<Option<Vec<f32>>, ToolError> {
    match args.get(field) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                match item.as_f64() {
                    Some(f) => out.push(f as f32),
                    None => {
                        return Err(ToolError::BadArguments(format!(
                            "`{field}` must contain only numbers; element {i} is {item}"
                        )))
                    }
                }
            }
            Ok(Some(out))
        }
        Some(other) => Err(wrong(field, "an array of numbers", other)),
    }
}

fn opt_str_list(args: &JsonObject, field: &str) -> Result<Option<Vec<String>>, ToolError> {
    match args.get(field) {
        None | Some(Json::Null) => Ok(None),
        _ => opt_str_array(args, field).map(Some),
    }
}

fn opt_str_array(args: &JsonObject, field: &str) -> Result<Vec<String>, ToolError> {
    match args.get(field) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, item)| match item {
                Json::String(s) => Ok(s.clone()),
                other => Err(ToolError::BadArguments(format!(
                    "`{field}` must contain only strings; element {i} is {other}"
                ))),
            })
            .collect(),
        // A model with one item sometimes sends it bare; it meant a list of one.
        Some(Json::String(s)) => Ok(vec![s.clone()]),
        Some(other) => Err(wrong(field, "an array of strings", other)),
    }
}

fn opt_meta(args: &JsonObject, field: &str) -> Result<Vec<(String, String)>, ToolError> {
    match args.get(field) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Object(map)) => map
            .iter()
            .map(|(k, v)| match v {
                Json::String(s) => Ok((k.clone(), s.clone())),
                // Anything scalar is rendered rather than refused; a model that
                // puts a number in metadata meant the number.
                Json::Number(_) | Json::Bool(_) => Ok((k.clone(), v.to_string())),
                other => Err(ToolError::BadArguments(format!(
                    "`{field}.{k}` must be a string, got {other}"
                ))),
            })
            .collect(),
        Some(other) => Err(wrong(field, "an object of string values", other)),
    }
}
