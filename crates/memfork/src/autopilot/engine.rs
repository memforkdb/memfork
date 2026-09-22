//! The daemon's side of autopilot: acting on sessions (DESIGN §5.6).
//!
//! A proxy observes the repository and a hook observes the agent; neither
//! owns the store or a session's current branch. Both post what they saw to
//! the daemon's `/autopilot` route, behind the daemon's own token, and the
//! code here does the rest against the connected sessions: switch, fork, merge,
//! discard with a lesson, and say so — in the feed, in the journal beside
//! the store, and as a note on each affected session's next tool result.
//!
//! A proxy's request names its own session. A hook's request can only name
//! the client and the project, because a client gives its hooks a session
//! id it never gives its MCP servers; so a hook acts on every session of
//! that client in that project, which is one session in the intended use,
//! and every session is told when it is more.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, Weak};

use memfork_core::{Db, MergeKind, MergePolicy};
use serde_json::{json, Value as Json};

use super::{clip_action, is_fork, parent_of_fork, ForkKind, JournalEntry, OpenFork, WRITER};
use crate::events::{Event, Events};
use crate::shared::Shared;
use crate::tools::dispatch::Session;

/// The connected sessions of one process, so a request that names a client or a
/// session can reach the object that holds its current branch. Weak: a
/// session that has gone is pruned at the next look.
#[derive(Debug, Default)]
pub struct Directory {
    sessions: Mutex<Vec<Weak<Session>>>,
}

impl Directory {
    /// Add a session.
    pub fn register(&self, session: &Arc<Session>) {
        let mut all = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        all.retain(|w| w.strong_count() > 0);
        all.push(Arc::downgrade(session));
    }

    /// Every connected session.
    pub fn all(&self) -> Vec<Arc<Session>> {
        let mut all = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        all.retain(|w| w.strong_count() > 0);
        all.iter().filter_map(Weak::upgrade).collect()
    }

    /// The session a proxy named.
    pub fn by_id(&self, id: &str) -> Option<Arc<Session>> {
        self.all().into_iter().find(|s| s.who().session == id)
    }

    /// Every session of `client` (a registry id or the name it gave) in
    /// `namespace`, in the order they connected.
    pub fn of(&self, client: &str, namespace: &str) -> Vec<Arc<Session>> {
        let wanted = crate::clients::find(client)
            .map(|c| c.display)
            .unwrap_or_else(|| crate::clients::display_for_writer(client));
        self.all()
            .into_iter()
            .filter(|s| s.namespace() == namespace)
            .filter(|s| {
                s.writer()
                    .is_some_and(|w| crate::clients::display_for_writer(&w) == wanted)
            })
            .collect()
    }
}

/// Where a request is carried out.
#[derive(Clone)]
pub struct Context {
    /// The store.
    pub db: Db,
    /// The side structure and the session directory.
    pub shared: Arc<Shared>,
    /// The feed, if this process has one.
    pub events: Option<Arc<Events>>,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context").finish_non_exhaustive()
    }
}

/// A refusal, with the status a route answers.
pub type Failure = (u16, String);

fn bad(why: impl Into<String>) -> Failure {
    (400, why.into())
}

impl Context {
    fn publish(&self, event: Event) {
        if let Some(events) = &self.events {
            events.publish(event);
        }
    }

    fn event(
        &self,
        ns: &str,
        operation: &str,
        branch: Option<String>,
        detail: Option<String>,
        ok: bool,
    ) {
        self.publish(Event {
            operation: Some(operation.to_owned()),
            branch,
            detail,
            ok,
            ..Event::about(WRITER, Some(ns))
        });
    }

    fn journal(&self, ns: &str, kind: &str, branch: Option<&str>, detail: String) {
        self.shared.sidecar.journal(
            ns,
            JournalEntry {
                order: 0,
                time: crate::events::now(),
                kind: kind.to_owned(),
                branch: branch.map(str::to_owned),
                detail,
            },
        );
    }

    /// Carry out one request.
    pub fn handle(&self, request: &Json) -> Result<Json, Failure> {
        let action = request["action"]
            .as_str()
            .ok_or_else(|| bad("the request names no action"))?;
        match action {
            "cursor" => self.cursor(request),
            "follow" => self.follow(request),
            "fork" => self.fork(request),
            "edit" => self.edit(request),
            "open" => self.open(request),
            "settle" => self.settle(request),
            "status" => Ok(self.status(request)),
            other => Err(bad(format!("`{other}` is not an autopilot action"))),
        }
    }

    // ---- follow --------------------------------------------------------------

    fn cursor(&self, request: &Json) -> Result<Json, Failure> {
        let worktree = text(request, "worktree")?;
        Ok(json!({ "cursor": self.shared.sidecar.reflog_cursor(worktree) }))
    }

    fn follow(&self, request: &Json) -> Result<Json, Failure> {
        let ns = text(request, "namespace")?;
        let session_id = text(request, "session")?;
        let repo = text(request, "repo")?;
        let worktree = text(request, "worktree")?;
        let Some(session) = self.shared.sessions.by_id(session_id) else {
            return Err((404, "no such session".to_owned()));
        };
        let mut notes = Vec::new();

        match request["head"].as_str().unwrap_or("unknown") {
            "branch" => {
                let branch = text(request, "branch")?;
                session.autopilot().detached_noted = false;
                if let Some(note) =
                    self.follow_branch(&session, ns, repo, branch, request["from"].as_str())
                {
                    notes.push(note);
                }
            }
            "detached" => {
                let mut state = session.autopilot();
                if !state.detached_noted {
                    state.detached_noted = true;
                    drop(state);
                    let on = session.branch();
                    let detail = format!("git HEAD is detached; memory stays on `{on}`");
                    self.event(
                        ns,
                        "autopilot",
                        Some(on.clone()),
                        Some("detached HEAD".to_owned()),
                        true,
                    );
                    self.journal(ns, "detached", Some(&on), detail.clone());
                    notes.push(json!({ "kind": "detached", "branch": on, "note": detail }));
                }
            }
            _ => {}
        }

        for merge in request["merges"].as_array().into_iter().flatten() {
            let (Some(source), Some(target)) = (merge["source"].as_str(), merge["target"].as_str())
            else {
                continue;
            };
            if let Some(note) = self.follow_merge(ns, source, target) {
                notes.push(note);
            }
        }

        if let Some(cursor) = request["cursor"].as_u64() {
            self.shared.sidecar.set_reflog_cursor(worktree, cursor);
        }

        self.note_orphans(ns, repo, request);

        for note in &notes {
            session.autopilot().notes.push(note.clone());
        }
        Ok(json!({ "notes": notes }))
    }

    /// The session's git branch is `branch`: land memory on the branch of
    /// that name, making it if need be.
    fn follow_branch(
        &self,
        session: &Arc<Session>,
        ns: &str,
        repo: &str,
        branch: &str,
        from: Option<&str>,
    ) -> Option<Json> {
        if is_fork(branch) || memfork_core::validate_branch_name(branch).is_err() {
            return None;
        }
        let current = session.branch();
        if current == branch {
            self.shared.sidecar.note_followed(repo, branch);
            return None;
        }
        if self.db.has_branch(branch) {
            session.switch_to(branch.to_owned());
            self.shared.sidecar.note_followed(repo, branch);
            let detail = format!("memory switched to `{branch}` with git");
            self.event(
                ns,
                "follow",
                Some(branch.to_owned()),
                Some("switched".to_owned()),
                true,
            );
            self.journal(ns, "follow", Some(branch), detail.clone());
            return Some(json!({ "kind": "follow", "branch": branch, "note": detail }));
        }
        // New to memory: fork it from the branch git came from, when that
        // branch has memory, else from where this session was.
        let source = match from {
            Some(f) if self.db.has_branch(f) && !is_fork(f) => f.to_owned(),
            _ if is_fork(&current) => parent_of_fork(&current)
                .filter(|p| self.db.has_branch(p))
                .map_or_else(|| current.clone(), str::to_owned),
            _ => current.clone(),
        };
        match self.db.fork(&source, branch) {
            Ok(_) => {}
            Err(e) => {
                self.event(
                    ns,
                    "follow",
                    Some(branch.to_owned()),
                    Some(format!("could not fork from {source}")),
                    false,
                );
                return Some(json!({
                    "kind": "follow",
                    "branch": branch,
                    "note": format!("memory could not follow git to `{branch}`: {e}"),
                }));
            }
        }
        session.switch_to(branch.to_owned());
        self.shared.sidecar.note_followed(repo, branch);
        let detail = format!("memory forked `{branch}` from `{source}` with git");
        self.event(
            ns,
            "follow",
            Some(branch.to_owned()),
            Some(format!("forked from {source}")),
            true,
        );
        self.journal(ns, "follow", Some(branch), detail.clone());
        Some(json!({ "kind": "follow", "branch": branch, "from": source, "note": detail }))
    }

    /// Git merged `source` into `target`: do the same in memory, never
    /// forcing a conflict.
    fn follow_merge(&self, ns: &str, source: &str, target: &str) -> Option<Json> {
        if !self.db.has_branch(source) || !self.db.has_branch(target) || source == target {
            return None;
        }
        let pair = format!("{source} -> {target}");
        match self.db.merge(source, target, MergePolicy::Fail) {
            Ok(outcome) => {
                let how = match outcome.kind {
                    MergeKind::UpToDate => "already up to date",
                    MergeKind::FastForward => "fast-forward",
                    MergeKind::Merged => "merged",
                };
                let detail = format!("memory merged `{source}` into `{target}` with git ({how})");
                self.event(
                    ns,
                    "merge",
                    Some(pair),
                    Some(format!("git merge {source}")),
                    true,
                );
                self.journal(ns, "merge", Some(target), detail.clone());
                Some(
                    json!({ "kind": "merge", "source": source, "target": target, "result": how, "note": detail }),
                )
            }
            Err(memfork_core::Error::MergeConflict { keys }) => {
                let detail = format!(
                    "git merged `{source}` into `{target}`, but memory conflicts on {}: nothing changed",
                    keys.iter().take(5).map(|k| format!("`{k}`")).collect::<Vec<_>>().join(", ")
                );
                self.event(
                    ns,
                    "autopilot",
                    Some(pair),
                    Some(format!("conflict: {source} into {target}")),
                    false,
                );
                self.journal(ns, "conflict", Some(target), detail.clone());
                Some(json!({
                    "kind": "conflict",
                    "source": source,
                    "target": target,
                    "conflicts": keys,
                    "note": detail,
                    "hint": format!(
                        "Look at the keys, then call memfork_merge with source `{source}`, target `{target}` and policy `ours` or `theirs`."
                    ),
                }))
            }
            Err(e) => {
                self.event(
                    ns,
                    "merge",
                    Some(pair),
                    Some(format!("git merge {source}")),
                    false,
                );
                Some(
                    json!({ "kind": "merge", "source": source, "target": target, "note": format!("memory could not merge `{source}` into `{target}`: {e}") }),
                )
            }
        }
    }

    // ---- forks ---------------------------------------------------------------

    /// The name of the next fork of `parent` that does not exist.
    fn fork_name(&self, parent: &str) -> String {
        let base = parent_of_fork(parent).unwrap_or(parent);
        (1..)
            .map(|n| format!("{}{base}/{n}", super::FORK_PREFIX))
            .find(|name| !self.db.has_branch(name))
            .unwrap_or_else(|| format!("{}{base}/1", super::FORK_PREFIX))
    }

    fn sessions_of(&self, request: &Json) -> Result<(String, String, Vec<Arc<Session>>), Failure> {
        let client = text(request, "client")?;
        let ns = text(request, "namespace")?;
        Ok((
            client.to_owned(),
            ns.to_owned(),
            self.shared.sessions.of(client, ns),
        ))
    }

    fn fork(&self, request: &Json) -> Result<Json, Failure> {
        let (_, ns, sessions) = self.sessions_of(request)?;
        let rule = text(request, "rule")?;
        let action = clip_action(text(request, "command")?);
        let origin = request["tool_use_id"].as_str().map(str::to_owned);
        let mut forked = Vec::new();
        for session in &sessions {
            if let Some(name) = self.fork_session(
                session,
                &ns,
                rule,
                &action,
                origin.clone(),
                ForkKind::Command,
            ) {
                forked.push(name);
            }
        }
        self.say_shared(&sessions, &ns);
        Ok(json!({ "sessions": sessions.len(), "forked": forked }))
    }

    fn edit(&self, request: &Json) -> Result<Json, Failure> {
        let (_, ns, sessions) = self.sessions_of(request)?;
        let file = text(request, "file")?;
        let max_files = request["max_files"]
            .as_u64()
            .unwrap_or(super::config::DEFAULT_MAX_FILES)
            .max(1);
        let origin = request["tool_use_id"].as_str().map(str::to_owned);
        let mut forked = Vec::new();
        for session in &sessions {
            let count = {
                let mut state = session.autopilot();
                state.edited.insert(file.to_owned());
                if state.open.is_some() {
                    continue;
                }
                state.edited.len() as u64
            };
            if count > max_files {
                let action = format!("edits to {count} files, past the limit of {max_files}");
                if let Some(name) = self.fork_session(
                    session,
                    &ns,
                    "edits",
                    &action,
                    origin.clone(),
                    ForkKind::Edits,
                ) {
                    forked.push(name);
                }
            }
        }
        if !forked.is_empty() {
            self.say_shared(&sessions, &ns);
        }
        Ok(json!({ "sessions": sessions.len(), "forked": forked }))
    }

    fn fork_session(
        &self,
        session: &Arc<Session>,
        ns: &str,
        rule: &str,
        action: &str,
        origin: Option<String>,
        kind: ForkKind,
    ) -> Option<String> {
        {
            let mut state = session.autopilot();
            let existing = state.open.as_ref().map(|open| open.name.clone());
            if let Some(existing) = existing {
                if self.db.has_branch(&existing) {
                    let note =
                        format!("already protected by fork `{existing}`; `{action}` runs on it");
                    state
                        .notes
                        .push(json!({ "kind": "protected", "fork": existing, "note": note }));
                    return None;
                }
                // The agent removed it itself; start afresh.
                state.open = None;
            }
        }
        let parent = session.branch();
        let name = self.fork_name(&parent);
        if let Err(e) = self.db.fork(&parent, &name) {
            self.event(
                ns,
                "fork",
                Some(name.clone()),
                Some(format!("{rule}: {action}")),
                false,
            );
            session.autopilot().notes.push(json!({ "kind": "fork", "note": format!("autopilot could not fork memory before `{action}`: {e}") }));
            return None;
        }
        session.switch_to(name.clone());
        let open = OpenFork {
            name: name.clone(),
            parent: parent.clone(),
            rule: rule.to_owned(),
            action: action.to_owned(),
            origin,
            kind,
        };
        let detail = format!("memory forked to `{name}` before `{action}` (rule: {rule})");
        self.event(
            ns,
            "fork",
            Some(name.clone()),
            Some(format!("{rule}: {action}")),
            true,
        );
        self.journal(ns, "fork", Some(&name), detail.clone());
        let mut state = session.autopilot();
        state.notes.push(
            json!({ "kind": "fork", "fork": name, "parent": parent, "rule": rule, "note": detail }),
        );
        state.open = Some(open);
        Some(name)
    }

    /// When a hook reached more than one session, each is told.
    fn say_shared(&self, sessions: &[Arc<Session>], ns: &str) {
        if sessions.len() < 2 {
            return;
        }
        let n = sessions.len();
        let note = format!(
            "{n} sessions of this client in this project share this fork and settle together; one session per project is the intended use"
        );
        for session in sessions {
            session
                .autopilot()
                .notes
                .push(json!({ "kind": "shared", "sessions": n, "note": note }));
        }
        self.event(
            ns,
            "autopilot",
            None,
            Some(format!("{n} sessions share a fork")),
            true,
        );
    }

    fn open(&self, request: &Json) -> Result<Json, Failure> {
        let (_, _, sessions) = self.sessions_of(request)?;
        let origin = request["tool_use_id"].as_str();
        let open: Vec<Json> = sessions
            .iter()
            .filter_map(|s| {
                let state = s.autopilot();
                let fork = state.open.as_ref()?;
                if origin.is_some() && fork.origin.as_deref() != origin {
                    return None;
                }
                let mut view = fork.to_json();
                view["session"] = json!(s.who().session);
                Some(view)
            })
            .collect();
        Ok(json!({ "open": open }))
    }

    fn settle(&self, request: &Json) -> Result<Json, Failure> {
        let (_, ns, sessions) = self.sessions_of(request)?;
        let outcome = text(request, "outcome")?;
        if !matches!(outcome, "passed" | "failed" | "none") {
            return Err(bad("`outcome` must be passed, failed or none"));
        }
        let origin = request["tool_use_id"].as_str();
        let mut results = Vec::new();
        for session in &sessions {
            let open = {
                let state = session.autopilot();
                match &state.open {
                    Some(fork) if origin.is_none() || fork.origin.as_deref() == origin => {
                        fork.clone()
                    }
                    _ => continue,
                }
            };
            let result = self.settle_one(session, &ns, &open, outcome, request);
            let mut state = session.autopilot();
            state.open = None;
            state.edited.clear();
            results.push(result);
        }
        Ok(json!({ "settled": results }))
    }

    fn settle_one(
        &self,
        session: &Arc<Session>,
        ns: &str,
        open: &OpenFork,
        outcome: &str,
        request: &Json,
    ) -> Json {
        let fork = open.name.as_str();
        let parent = if self.db.has_branch(&open.parent) {
            open.parent.clone()
        } else {
            self.db.default_branch().to_owned()
        };
        if !self.db.has_branch(fork) {
            let note = format!("fork `{fork}` is already gone; nothing to settle");
            session
                .autopilot()
                .notes
                .push(json!({ "kind": "settled", "fork": fork, "result": "gone", "note": note }));
            if session.branch() == fork {
                session.switch_to(parent);
            }
            return json!({ "fork": fork, "result": "gone" });
        }
        let pair = format!("{fork} -> {parent}");
        match outcome {
            "passed" => match self.db.merge(fork, &parent, MergePolicy::Fail) {
                Ok(_) => {
                    let _ = self.db.discard(fork);
                    self.move_off(fork, &parent);
                    let how = request["check"].as_str().map_or_else(
                        || "exit 0".to_owned(),
                        |c| format!("`{}` passed", clip_action(c)),
                    );
                    let detail = format!(
                        "`{}` worked ({how}): memory merged `{fork}` into `{parent}`",
                        open.action
                    );
                    self.event(ns, "merge", Some(pair), Some(how), true);
                    self.journal(ns, "merged", Some(&parent), detail.clone());
                    session.autopilot().notes.push(json!({ "kind": "settled", "fork": fork, "result": "merged", "into": parent, "note": detail }));
                    json!({ "fork": fork, "result": "merged", "into": parent })
                }
                Err(memfork_core::Error::MergeConflict { keys }) => {
                    let detail = format!(
                        "`{}` worked, but memory on `{fork}` conflicts with `{parent}` on {}: the fork is kept, nothing changed",
                        open.action,
                        keys.iter().take(5).map(|k| format!("`{k}`")).collect::<Vec<_>>().join(", ")
                    );
                    self.event(
                        ns,
                        "autopilot",
                        Some(pair),
                        Some(format!("conflict: {fork} into {parent}")),
                        false,
                    );
                    self.journal(ns, "conflict", Some(fork), detail.clone());
                    session.autopilot().notes.push(json!({
                        "kind": "settled", "fork": fork, "result": "conflict", "conflicts": keys, "note": detail,
                        "hint": format!("Look at the keys, then call memfork_merge with source `{fork}`, target `{parent}` and policy `ours` or `theirs`, and memfork_discard the fork."),
                    }));
                    json!({ "fork": fork, "result": "conflict", "conflicts": keys })
                }
                Err(e) => {
                    self.event(ns, "merge", Some(pair), None, false);
                    session.autopilot().notes.push(json!({ "kind": "settled", "fork": fork, "result": "error", "note": format!("memory could not merge `{fork}` into `{parent}`: {e}") }));
                    json!({ "fork": fork, "result": "error", "error": e.to_string() })
                }
            },
            "failed" => {
                let line = lesson_line(open, request);
                let lesson = crate::lessons::tidy(&line).unwrap_or_else(|_| line.clone());
                let recorded = crate::lessons::record(&self.db, fork, &lesson, ns, Some(WRITER));
                match &recorded {
                    Ok(rec) => {
                        self.shared
                            .sidecar
                            .count(ns, WRITER, |c| c.lessons_recorded += 1);
                        self.publish(Event {
                            operation: Some("lesson".to_owned()),
                            key: Some(rec.key.clone()),
                            branch: Some(rec.branch.clone()),
                            ..Event::about(WRITER, Some(ns))
                        });
                    }
                    Err(e) => {
                        self.event(
                            ns,
                            "lesson",
                            Some(parent.clone()),
                            Some(format!("could not be written: {e}")),
                            false,
                        );
                    }
                }
                let discarded = self.db.discard(fork);
                self.move_off(fork, &parent);
                let detail = match &discarded {
                    Ok(()) => format!(
                        "`{}` failed: memory fork `{fork}` discarded, lesson kept on `{parent}`",
                        open.action
                    ),
                    Err(e) => format!(
                        "`{}` failed, but fork `{fork}` could not be discarded: {e}",
                        open.action
                    ),
                };
                self.event(
                    ns,
                    "discard",
                    Some(fork.to_owned()),
                    Some(format!("{}: {}", open.rule, open.action)),
                    discarded.is_ok(),
                );
                self.journal(ns, "discarded", Some(fork), detail.clone());
                session.autopilot().notes.push(json!({
                    "kind": "settled", "fork": fork, "result": "discarded", "lesson": lesson,
                    "lesson_key": recorded.as_ref().ok().map(|r| r.key.clone()), "note": detail,
                }));
                json!({ "fork": fork, "result": "discarded", "lesson": lesson })
            }
            _ => {
                let detail = format!(
                    "fork `{fork}` kept: no check is configured to judge `{}`; merge it with memfork_merge into `{parent}`, or discard it with a lesson",
                    open.action
                );
                self.event(
                    ns,
                    "autopilot",
                    Some(fork.to_owned()),
                    Some("kept: no check configured".to_owned()),
                    true,
                );
                self.journal(ns, "kept", Some(fork), detail.clone());
                session.autopilot().notes.push(json!({ "kind": "settled", "fork": fork, "result": "kept", "parent": parent, "note": detail }));
                json!({ "fork": fork, "result": "kept" })
            }
        }
    }

    /// Every session on `fork` goes back to `parent`.
    fn move_off(&self, fork: &str, parent: &str) {
        for session in self.shared.sessions.all() {
            if session.branch() == fork {
                session.switch_to(parent.to_owned());
            }
        }
    }

    // ---- status --------------------------------------------------------------

    /// Compare the branches git has now, when the request carries them,
    /// with the memory branches that followed git in `repo`: what followed
    /// and still exists in memory but not in git is an orphan. Remembered
    /// beside the store for the Brain, which cannot see the repository.
    fn note_orphans(&self, ns: &str, repo: &str, request: &Json) {
        let Some(list) = request["git_branches"].as_array() else {
            return;
        };
        let git: BTreeSet<String> = list
            .iter()
            .filter_map(Json::as_str)
            .map(str::to_owned)
            .collect();
        let memory: BTreeSet<String> = self.db.branches().into_iter().map(|b| b.name).collect();
        let followed = self.shared.sidecar.followed(repo);
        let orphans: Vec<String> = followed
            .iter()
            .filter(|b| memory.contains(*b) && !git.contains(*b))
            .cloned()
            .collect();
        self.shared.sidecar.set_orphans(ns, orphans);
    }

    /// A status request from a command that runs in the repository carries
    /// the branches git has now, so the orphans it reports are current
    /// rather than as of the last switch.
    fn status(&self, request: &Json) -> Json {
        let ns = request["namespace"]
            .as_str()
            .unwrap_or(crate::namespace::FALLBACK);
        if let Some(repo) = request["repo"].as_str() {
            self.note_orphans(ns, repo, request);
        }
        self.status_of(ns)
    }

    /// What autopilot holds for a project: forks open on connected sessions,
    /// forks kept for a person to settle, memory branches whose git branch
    /// is gone, and the journal. For the status command, doctor and the
    /// Brain.
    pub fn status_of(&self, ns: &str) -> Json {
        let open: Vec<Json> = self
            .shared
            .sessions
            .all()
            .into_iter()
            .filter(|s| s.namespace() == ns)
            .filter_map(|s| {
                let state = s.autopilot();
                let fork = state.open.as_ref()?;
                let mut view = fork.to_json();
                view["client"] = json!(s.writer().map(|w| crate::clients::display_for_writer(&w)));
                Some(view)
            })
            .collect();
        let memory: BTreeSet<String> = self.db.branches().into_iter().map(|b| b.name).collect();
        let orphans: Vec<String> = self
            .shared
            .sidecar
            .orphans(ns)
            .into_iter()
            .filter(|b| memory.contains(b))
            .collect();
        let kept: Vec<String> = memory.iter().filter(|b| is_fork(b)).cloned().collect();
        json!({
            "namespace": ns,
            "open": open,
            "kept_forks": kept,
            "orphans": orphans.iter().map(|b| orphan_view(b)).collect::<Vec<_>>(),
            "journal": self.shared.sidecar.journal_entries(ns),
        })
    }
}

/// A memory branch whose git branch is gone, with why it might be and the
/// two ways out. Never discarded by MemFork.
pub fn orphan_view(branch: &str) -> Json {
    let target = "main";
    json!({
        "branch": branch,
        "why": "git branch deleted, or squash-merged: memory was not merged",
        "merge_then_discard": [
            format!("memfork merge {branch} --branch {target}"),
            format!("memfork discard {branch} --lesson \"<what it taught>\""),
        ],
        "discard": format!("memfork discard {branch} --lesson \"<what it taught>\""),
    })
}

/// The lesson a failed action leaves, from data alone.
pub fn lesson_line(open: &OpenFork, request: &Json) -> String {
    let mut line = format!("autopilot: `{}` (rule: {}) failed", open.action, open.rule);
    if let Some(check) = request["check"].as_str() {
        line.push_str(&format!(
            " {}",
            clip_action(check).chars().take(60).collect::<String>()
        ));
    }
    if request["timed_out"].as_bool() == Some(true) {
        line.push_str(", timed out");
        if let Some(seconds) = request["timeout_seconds"].as_u64() {
            line.push_str(&format!(" after {seconds} s"));
        }
    } else if let Some(code) = request["exit_code"].as_i64() {
        line.push_str(&format!(", exit {code}"));
    }
    if let Some(last) = request["last_line"]
        .as_str()
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
        let last: String = last.split_whitespace().collect::<Vec<_>>().join(" ");
        line.push_str(&format!(": {}", last.chars().take(100).collect::<String>()));
    }
    line
}

fn text<'a>(request: &'a Json, field: &str) -> Result<&'a str, Failure> {
    request[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad(format!("the request needs `{field}`")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> Context {
        Context {
            db: Db::new(),
            shared: Shared::in_memory(),
            events: None,
        }
    }

    fn session(ctx: &Context, client: &str, ns: &str, id: &str) -> Arc<Session> {
        let session =
            Arc::new(Session::in_namespace(ctx.db.clone(), ns).sharing(Arc::clone(&ctx.shared)));
        session.set_writer(client);
        session.set_session_id(id);
        ctx.shared.sessions.register(&session);
        session
    }

    #[test]
    fn the_lesson_is_composed_from_data_alone() {
        let open = OpenFork {
            name: "autopilot/main/1".to_owned(),
            parent: "main".to_owned(),
            rule: "migration".to_owned(),
            action: "npx prisma migrate dev".to_owned(),
            origin: None,
            kind: ForkKind::Command,
        };
        let line = lesson_line(
            &open,
            &json!({ "check": "cargo test", "exit_code": 101, "last_line": "  test payments::refund ... FAILED  " }),
        );
        assert_eq!(
            line,
            "autopilot: `npx prisma migrate dev` (rule: migration) failed cargo test, exit 101: test payments::refund ... FAILED"
        );
        let timed = lesson_line(
            &open,
            &json!({ "check": "cargo test", "timed_out": true, "timeout_seconds": 300 }),
        );
        assert_eq!(timed, "autopilot: `npx prisma migrate dev` (rule: migration) failed cargo test, timed out after 300 s");
        let own = lesson_line(
            &open,
            &json!({ "exit_code": 1, "last_line": "Error: Cannot find module 'express'" }),
        );
        assert_eq!(own, "autopilot: `npx prisma migrate dev` (rule: migration) failed, exit 1: Error: Cannot find module 'express'");
        assert!(
            crate::lessons::tidy(&line).unwrap().chars().count()
                <= crate::lessons::MAX_LESSON_CHARS
        );
    }

    #[test]
    fn a_request_that_lacks_what_it_needs_is_refused_with_the_field_named() {
        let ctx = context();
        let err = ctx
            .handle(&json!({ "action": "fork", "client": "claude-code", "namespace": "shop", "rule": "migration" }))
            .unwrap_err();
        assert_eq!(err.0, 400);
        assert!(err.1.contains("`command`"), "{err:?}");
        let err = ctx.handle(&json!({ "action": "dance" })).unwrap_err();
        assert!(err.1.contains("not an autopilot action"));
        let err = ctx
            .handle(&json!({ "action": "settle", "client": "claude-code", "namespace": "shop", "outcome": "maybe" }))
            .unwrap_err();
        assert!(err.1.contains("passed, failed or none"));
        // No session of that client: nothing forked, nothing broken.
        let none = ctx
            .handle(&json!({ "action": "fork", "client": "claude-code", "namespace": "shop", "rule": "migration", "command": "x" }))
            .unwrap();
        assert_eq!(none["sessions"], 0);
    }

    #[test]
    fn fork_settle_merge_and_discard_with_a_lesson() {
        let ctx = context();
        let a = session(&ctx, "claude-code", "shop", "a");
        let b = session(&ctx, "claude-code", "shop", "b");
        let _other = session(&ctx, "codex-mcp-client", "shop", "c");
        let _elsewhere = session(&ctx, "claude-code", "other", "d");
        a.call(
            "memfork_put",
            &args(json!({ "key": "shop:decision:x", "value": "one" })),
        )
        .unwrap();

        let forked = ctx
            .handle(&json!({ "action": "fork", "client": "claude-code", "namespace": "shop", "rule": "migration", "command": "npx prisma migrate dev", "tool_use_id": "t1" }))
            .unwrap();
        assert_eq!(forked["sessions"], 2);
        assert_eq!(
            forked["forked"],
            json!(["autopilot/main/1", "autopilot/main/2"])
        );
        assert_eq!(a.branch(), "autopilot/main/1");
        assert_eq!(b.branch(), "autopilot/main/2");
        // Both are told they share.
        let note = a
            .call("memfork_get", &args(json!({ "key": "shop:decision:x" })))
            .unwrap();
        let kinds: Vec<&str> = note["autopilot"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["fork", "shared"]);
        assert!(note["autopilot"][1]["note"]
            .as_str()
            .unwrap()
            .contains("2 sessions"));

        // A second risky action does not nest.
        let again = ctx
            .handle(&json!({ "action": "fork", "client": "claude-code", "namespace": "shop", "rule": "recursive-delete", "command": "rm -rf x", "tool_use_id": "t2" }))
            .unwrap();
        assert_eq!(again["forked"], json!([]));

        // Work on the fork.
        a.call(
            "memfork_put",
            &args(json!({ "key": "shop:decision:y", "value": "on the fork" })),
        )
        .unwrap();

        // Only the origin settles it.
        let wrong = ctx
            .handle(&json!({ "action": "settle", "client": "claude-code", "namespace": "shop", "tool_use_id": "t2", "outcome": "passed" }))
            .unwrap();
        assert_eq!(wrong["settled"], json!([]));

        let passed = ctx
            .handle(&json!({ "action": "settle", "client": "claude-code", "namespace": "shop", "tool_use_id": "t1", "outcome": "passed", "check": "cargo test" }))
            .unwrap();
        assert_eq!(passed["settled"][0]["result"], "merged");
        assert_eq!(a.branch(), "main");
        assert_eq!(b.branch(), "main");
        assert!(!ctx.db.has_branch("autopilot/main/1"));
        let merged = ctx.db.read("main").unwrap();
        assert!(merged.get("shop:decision:y").is_some());

        // Failing: a lesson on the parent, the fork gone.
        ctx.handle(&json!({ "action": "fork", "client": "claude-code", "namespace": "shop", "rule": "migration", "command": "npx prisma migrate dev", "tool_use_id": "t3" })).unwrap();
        a.call(
            "memfork_put",
            &args(json!({ "key": "shop:decision:z", "value": "doomed" })),
        )
        .unwrap();
        let failed = ctx
            .handle(&json!({ "action": "settle", "client": "claude-code", "namespace": "shop", "tool_use_id": "t3", "outcome": "failed", "check": "cargo test", "exit_code": 101, "last_line": "FAILED" }))
            .unwrap();
        assert_eq!(failed["settled"][0]["result"], "discarded");
        assert_eq!(a.branch(), "main");
        let main = ctx.db.read("main").unwrap();
        assert!(main.get("shop:decision:z").is_none());
        let lessons = crate::lessons::recent(&ctx.db, "main", "shop", 5).unwrap();
        assert_eq!(lessons[0]["by"], WRITER);
        assert!(lessons[0]["lesson"].as_str().unwrap().starts_with("autopilot: `npx prisma migrate dev` (rule: migration) failed cargo test, exit 101: FAILED"));

        // No check: kept, and said.
        ctx.handle(&json!({ "action": "fork", "client": "claude-code", "namespace": "shop", "rule": "migration", "command": "x", "tool_use_id": "t4" })).unwrap();
        let kept = ctx
            .handle(&json!({ "action": "settle", "client": "claude-code", "namespace": "shop", "outcome": "none" }))
            .unwrap();
        assert_eq!(kept["settled"][0]["result"], "kept");
        assert!(a.branch().starts_with("autopilot/main/"));
        let status = ctx
            .handle(&json!({ "action": "status", "namespace": "shop" }))
            .unwrap();
        assert_eq!(status["open"], json!([]));
        assert_eq!(status["kept_forks"].as_array().unwrap().len(), 2);
        assert!(status["journal"].as_array().unwrap().len() >= 5);
    }

    #[test]
    fn an_edit_sweep_forks_on_the_file_past_the_limit_and_stop_settles_it() {
        let ctx = context();
        let a = session(&ctx, "claude-code", "shop", "a");
        for file in ["a.rs", "b.rs", "a.rs"] {
            let r = ctx.handle(&json!({ "action": "edit", "client": "claude-code", "namespace": "shop", "file": file, "max_files": 2 })).unwrap();
            assert_eq!(r["forked"], json!([]), "{file}");
        }
        let r = ctx.handle(&json!({ "action": "edit", "client": "claude-code", "namespace": "shop", "file": "c.rs", "max_files": 2 })).unwrap();
        assert_eq!(r["forked"], json!(["autopilot/main/1"]));
        let open = ctx
            .handle(&json!({ "action": "open", "client": "claude-code", "namespace": "shop" }))
            .unwrap();
        assert_eq!(open["open"][0]["kind"], "edits");
        // A command's outcome does not settle an edit fork.
        let none = ctx.handle(&json!({ "action": "open", "client": "claude-code", "namespace": "shop", "tool_use_id": "t9" })).unwrap();
        assert_eq!(none["open"], json!([]));
        let settled = ctx.handle(&json!({ "action": "settle", "client": "claude-code", "namespace": "shop", "outcome": "failed", "check": "npm test", "exit_code": 1 })).unwrap();
        assert_eq!(settled["settled"][0]["result"], "discarded");
        assert_eq!(a.branch(), "main");
        assert!(a.autopilot().edited.is_empty());
    }

    #[test]
    fn a_conflict_keeps_the_fork_and_says_which_keys() {
        let ctx = context();
        let a = session(&ctx, "claude-code", "shop", "a");
        let other = session(&ctx, "codex-mcp-client", "shop", "c");
        a.call(
            "memfork_put",
            &args(json!({ "key": "shop:decision:x", "value": "one" })),
        )
        .unwrap();
        ctx.handle(&json!({ "action": "fork", "client": "claude-code", "namespace": "shop", "rule": "migration", "command": "m", "tool_use_id": "t1" })).unwrap();
        a.call(
            "memfork_put",
            &args(json!({ "key": "shop:decision:x", "value": "fork" })),
        )
        .unwrap();
        other
            .call(
                "memfork_put",
                &args(json!({ "key": "shop:decision:x", "value": "parent" })),
            )
            .unwrap();
        let settled = ctx.handle(&json!({ "action": "settle", "client": "claude-code", "namespace": "shop", "tool_use_id": "t1", "outcome": "passed" })).unwrap();
        assert_eq!(settled["settled"][0]["result"], "conflict");
        assert_eq!(
            settled["settled"][0]["conflicts"],
            json!(["shop:decision:x"])
        );
        assert_eq!(a.branch(), "autopilot/main/1");
        assert_eq!(
            ctx.db
                .read("main")
                .unwrap()
                .get("shop:decision:x")
                .unwrap()
                .value
                .as_ref(),
            b"parent"
        );
    }

    #[test]
    fn memory_follows_a_switch_a_first_switch_forks_and_a_git_merge_merges() {
        let ctx = context();
        let a = session(&ctx, "claude-code", "shop", "a");
        a.call(
            "memfork_put",
            &args(json!({ "key": "shop:decision:x", "value": "one" })),
        )
        .unwrap();
        let follow = |head: &str, branch: &str, from: Option<&str>, merges: Json| {
            ctx.handle(&json!({
                "action": "follow", "session": "a", "namespace": "shop", "repo": "r", "worktree": "w",
                "head": head, "branch": branch, "from": from, "merges": merges, "cursor": 10,
                "git_branches": ["main", "feature/x"],
            }))
            .unwrap()
        };
        // On main already: nothing.
        assert_eq!(
            follow("branch", "main", None, json!([]))["notes"],
            json!([])
        );
        // To a branch memory does not have: forked from where git came from.
        let r = follow("branch", "feature/x", Some("main"), json!([]));
        assert_eq!(r["notes"][0]["kind"], "follow");
        assert_eq!(r["notes"][0]["from"], "main");
        assert_eq!(a.branch(), "feature/x");
        a.call(
            "memfork_put",
            &args(json!({ "key": "shop:decision:y", "value": "on x" })),
        )
        .unwrap();
        // Back: a plain switch.
        let r = follow("branch", "main", Some("feature/x"), json!([]));
        assert_eq!(r["notes"][0]["note"], "memory switched to `main` with git");
        assert_eq!(a.branch(), "main");
        // Detached: said once, memory stays.
        let r = follow("detached", "", None, json!([]));
        assert_eq!(r["notes"][0]["kind"], "detached");
        assert_eq!(follow("detached", "", None, json!([]))["notes"], json!([]));
        assert_eq!(a.branch(), "main");
        // A git merge merges memory.
        let r = follow(
            "branch",
            "main",
            None,
            json!([{ "source": "feature/x", "target": "main" }]),
        );
        assert_eq!(r["notes"][0]["kind"], "merge");
        assert!(ctx
            .db
            .read("main")
            .unwrap()
            .get("shop:decision:y")
            .is_some());
        // The cursor is remembered, and no orphans yet.
        assert_eq!(
            ctx.handle(&json!({ "action": "cursor", "worktree": "w" }))
                .unwrap()["cursor"],
            10
        );
        let status = ctx
            .handle(&json!({ "action": "status", "namespace": "shop" }))
            .unwrap();
        assert_eq!(status["orphans"], json!([]));
        // Git deletes the branch: memory keeps it, and the status lists it
        // with why and the two ways out.
        ctx.handle(&json!({
            "action": "follow", "session": "a", "namespace": "shop", "repo": "r", "worktree": "w",
            "head": "branch", "branch": "main", "merges": [], "cursor": 12, "git_branches": ["main"],
        }))
        .unwrap();
        let status = ctx
            .handle(&json!({ "action": "status", "namespace": "shop" }))
            .unwrap();
        assert_eq!(status["orphans"][0]["branch"], "feature/x");
        assert!(status["orphans"][0]["why"]
            .as_str()
            .unwrap()
            .contains("squash-merged"));
        assert!(status["orphans"][0]["merge_then_discard"][0]
            .as_str()
            .unwrap()
            .starts_with("memfork merge feature/x"));
        assert!(ctx.db.has_branch("feature/x"), "discarded silently");
        // A session nobody knows is refused, not guessed at.
        let err = ctx.handle(&json!({ "action": "follow", "session": "zz", "namespace": "shop", "repo": "r", "worktree": "w", "head": "branch", "branch": "main" })).unwrap_err();
        assert_eq!(err.0, 404);
    }

    fn args(value: Json) -> crate::tools::schema::JsonObject {
        match value {
            Json::Object(map) => map,
            _ => unreachable!(),
        }
    }
}
