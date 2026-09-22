//! The task board: tasks, and claims on them that expire (DESIGN §6.5).
//!
//! A task is an ordinary entry under `<project>:task:<id>`, a JSON object
//! with a title, a status (`open`, `claimed` or `done`) and, while it is
//! claimed, the name of the client holding it. That committed entry is the
//! record a briefing, a merge and time travel see.
//!
//! **Who holds a task right now is not in it.** A claim is a lease that
//! expires, and expiry needs a clock; a clock reading in a commit would make
//! the same operations produce different ids at different times. So the
//! lease — which session holds it and until when — lives here, in memory,
//! beside the store and outside its history:
//!
//! * claiming checks the lease and, if the task is free, writes `claimed` in
//!   an optimistic transaction and then takes the lease, all under one lock,
//!   so two claims can never both succeed;
//! * renewing only moves the expiry, and commits nothing;
//! * releasing and finishing commit the new status and drop the lease;
//! * a lease that has run out is as good as none: a dead agent never blocks
//!   a task, and a restarted daemon starts with every task free.
//!
//! Leases belong to the work, not to a memory branch, so they are keyed by
//! the task's key alone: a task held on `main` is held for an agent working
//! on a fork too. The committed entries fork, merge and discard like any
//! other key.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use memfork_core::{Db, Entry, Value, WRITTEN_BY};
use serde_json::{json, Map, Value as Json};

use crate::namespace;

/// How long a claim lasts unless the claimer asks for something else.
pub const DEFAULT_LEASE_SECONDS: u64 = 300;

/// The shortest lease a claim may ask for.
pub const MIN_LEASE_SECONDS: u64 = 1;

/// The longest lease a claim may ask for. Longer work renews.
pub const MAX_LEASE_SECONDS: u64 = 3600;

/// Longest task title, in characters.
const MAX_TITLE_CHARS: usize = 200;

/// Longest task detail, in characters.
const MAX_DETAIL_CHARS: usize = 2000;

/// Most tasks a listing returns.
const MAX_LISTED: usize = 200;

/// How many times a claim retries a transaction that lost a race.
const RETRIES: usize = 16;

/// A task to add to the board.
#[derive(Debug, Clone, Copy, Default)]
pub struct NewTask<'a> {
    /// Its id; the next free number if `None`.
    pub id: Option<&'a str>,
    /// What it is, in a line.
    pub title: &'a str,
    /// Anything more.
    pub detail: Option<&'a str>,
    /// Ids of tasks in the same project that must be done first.
    pub depends_on: &'a [String],
    /// A command that exits 0 in the project when the task is done.
    pub accept: Option<&'a str>,
    /// How long that command may take.
    pub timeout_seconds: Option<u64>,
}

/// Whether each unfinished task in a project is ready, and if not, which of
/// the tasks it depends on are not done yet. Keyed by task id.
pub type Readiness = BTreeMap<String, Vec<String>>;

/// What time it is, for leases. Tests use one they can move.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Now.
    fn now(&self) -> SystemTime;
}

/// The system clock.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A clock that moves only when told to.
#[derive(Debug)]
pub struct ManualClock(Mutex<SystemTime>);

impl Default for ManualClock {
    fn default() -> Self {
        ManualClock(Mutex::new(
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
        ))
    }
}

impl ManualClock {
    /// Move the clock forward.
    pub fn advance(&self, by: Duration) {
        let mut now = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *now += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Who is asking: the client's name, which goes into committed data, and the
/// session, which never does.
///
/// Two sessions of one tool send the same client name, so ownership of a
/// claim is by session. The session id is random per proxy and lives only
/// here, so it can never reach a commit id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Who {
    /// The client's name, as it gave it.
    pub client: String,
    /// This session.
    pub session: String,
}

#[derive(Debug, Clone)]
struct Lease {
    who: Who,
    until: SystemTime,
    seconds: u64,
}

/// Why a task operation could not be done.
#[derive(Debug)]
pub enum BoardError {
    /// The request was malformed.
    Bad(String),
    /// The store refused.
    Engine(memfork_core::Error),
}

impl std::fmt::Display for BoardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoardError::Bad(m) => write!(f, "{m}"),
            BoardError::Engine(e) => write!(f, "{e}"),
        }
    }
}

impl From<memfork_core::Error> for BoardError {
    fn from(e: memfork_core::Error) -> Self {
        BoardError::Engine(e)
    }
}

/// What a claim attempt found, for the caller's statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The caller now holds it.
    Claimed,
    /// Somebody else holds it; nothing changed.
    Held,
    /// It is done; nothing changed.
    Done,
}

/// The leases, and the clock they are measured by.
#[derive(Debug)]
pub struct Board {
    clock: Arc<dyn Clock>,
    leases: Mutex<BTreeMap<String, Lease>>,
}

impl Default for Board {
    fn default() -> Self {
        Board::with_clock(Arc::new(SystemClock))
    }
}

/// A task's key, from a namespace and an id.
pub fn task_key(ns: &str, id: &str) -> String {
    format!("{}{id}", namespace::prefix(ns, "task"))
}

/// A task id is a short name or a number: letters, digits, `.`, `_`, `-`.
fn validate_id(id: &str) -> Result<(), BoardError> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(BoardError::Bad(format!(
            "`{id}` is not a usable task id: up to 64 letters, digits, `.`, `_` or `-`"
        )));
    }
    Ok(())
}

/// The task an entry holds, whatever shape it was written in: JSON from this
/// board, or plain text from before it, which is a task that is open.
fn read_task(entry: &Entry) -> Map<String, Json> {
    let text = String::from_utf8_lossy(&entry.value);
    match serde_json::from_str::<Json>(&text) {
        Ok(Json::Object(map)) => map,
        _ => {
            let mut map = Map::new();
            map.insert("title".to_owned(), json!(text));
            map.insert("status".to_owned(), json!("open"));
            map
        }
    }
}

fn status_of(task: &Map<String, Json>) -> String {
    task.get("status")
        .and_then(Json::as_str)
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "open".to_owned())
}

fn clip(s: &str, max: usize) -> String {
    s.trim().chars().take(max).collect()
}

impl Board {
    /// A board measuring leases by `clock`.
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Board {
            clock,
            leases: Mutex::new(BTreeMap::new()),
        }
    }

    fn leases(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Lease>> {
        self.leases.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The unexpired lease on `key`, if any.
    fn current<'a>(&self, leases: &'a BTreeMap<String, Lease>, key: &str) -> Option<&'a Lease> {
        let now = self.clock.now();
        leases.get(key).filter(|l| l.until > now)
    }

    fn seconds_left(&self, lease: &Lease) -> u64 {
        lease
            .until
            .duration_since(self.clock.now())
            .map(|d| d.as_secs() + u64::from(d.subsec_nanos() > 0))
            .unwrap_or(0)
    }

    /// Who holds `key` right now, for listings and briefings: the client and
    /// the seconds left, or `None`.
    pub fn holder(&self, key: &str) -> Option<(String, u64)> {
        let leases = self.leases();
        self.current(&leases, key)
            .map(|l| (l.who.client.clone(), self.seconds_left(l)))
    }

    /// Add a task. With no id, the next free number.
    pub fn add(
        &self,
        db: &Db,
        branch: &str,
        ns: &str,
        task: NewTask<'_>,
        by: Option<&str>,
    ) -> Result<Json, BoardError> {
        let NewTask {
            id,
            title,
            detail,
            depends_on,
            accept,
            timeout_seconds,
        } = task;
        let title = clip(title, MAX_TITLE_CHARS);
        if title.is_empty() {
            return Err(BoardError::Bad("a task needs a `title`".to_owned()));
        }
        let prefix = namespace::prefix(ns, "task");
        let id = match id {
            Some(id) => {
                validate_id(id)?;
                id.to_owned()
            }
            None => {
                let next = db
                    .list(branch, &prefix, None)?
                    .iter()
                    .filter_map(|(k, _)| k.strip_prefix(&prefix)?.parse::<u64>().ok())
                    .max()
                    .unwrap_or(0)
                    + 1;
                next.to_string()
            }
        };
        let key = format!("{prefix}{id}");
        if db.get(branch, &key)?.is_some() {
            return Err(BoardError::Bad(format!(
                "a task `{id}` already exists in `{ns}`; choose another id or leave it out"
            )));
        }
        // A new task cannot close a cycle: nothing can depend on it yet. Its
        // own dependencies must exist, in this project.
        let planned = crate::plans::PlanTask {
            id: id.clone(),
            title: title.clone(),
            detail: None,
            depends_on: depends_on.to_vec(),
            accept: crate::plans::normalise_accept(accept),
            timeout_seconds,
        };
        crate::plans::check_shape(std::slice::from_ref(&planned)).map_err(BoardError::Bad)?;
        crate::plans::check_graph(
            std::slice::from_ref(&planned),
            &self.graph(db, branch, &prefix, &[])?,
        )
        .map_err(BoardError::Bad)?;
        let mut task = Map::new();
        task.insert("title".to_owned(), json!(title));
        if let Some(detail) = detail
            .map(|d| clip(d, MAX_DETAIL_CHARS))
            .filter(|d| !d.is_empty())
        {
            task.insert("detail".to_owned(), json!(detail));
        }
        plan_fields(&mut task, &planned, None);
        task.insert("status".to_owned(), json!("open"));
        task.insert("holder".to_owned(), Json::Null);
        task.insert("claims".to_owned(), json!(0));
        let commit = db.put(branch, &key, value(&task, by))?;
        Ok(json!({
            "action": "add",
            "id": id,
            "key": key,
            "task": Json::Object(task),
            "commit": commit.to_hex(),
        }))
    }

    /// Claim a task for `who` for `lease_seconds`.
    pub fn claim(
        &self,
        db: &Db,
        branch: &str,
        key: &str,
        who: &Who,
        lease_seconds: u64,
    ) -> Result<(Json, ClaimOutcome), BoardError> {
        let seconds = lease_seconds.clamp(MIN_LEASE_SECONDS, MAX_LEASE_SECONDS);
        // One lock across the check, the commit and the lease: the daemon is
        // one process, so this is what makes two claims unable to both win.
        let mut leases = self.leases();
        if let Some(lease) = self.current(&leases, key) {
            if lease.who != *who {
                return Ok((
                    json!({
                        "action": "claim",
                        "key": key,
                        "claimed": false,
                        "held_by": lease.who.client,
                        "same_client": lease.who.client == who.client,
                        "seconds_left": self.seconds_left(lease),
                        "hint": "Somebody else is working on this. Pick another task, \
                                 or wait for the claim to be released or to run out.",
                    }),
                    ClaimOutcome::Held,
                ));
            }
        }
        let mut committed = None;
        let mut done = false;
        let mut last_err = None;
        for _ in 0..RETRIES {
            let mut txn = db.begin(branch)?;
            let Some(entry) = txn.get(key) else {
                return Err(BoardError::Bad(format!(
                    "there is no task `{key}` on `{branch}`; add it first"
                )));
            };
            let mut task = read_task(&entry);
            if status_of(&task) == "done" {
                done = true;
                break;
            }
            let claims = task.get("claims").and_then(Json::as_u64).unwrap_or(0);
            task.insert("status".to_owned(), json!("claimed"));
            task.insert("holder".to_owned(), json!(who.client));
            task.insert("claims".to_owned(), json!(claims + 1));
            txn.put(key, value(&task, Some(&who.client)))?;
            match txn.commit(Some(format!("claim {key}"))) {
                Ok(id) => {
                    committed = Some(id);
                    break;
                }
                Err(e @ memfork_core::Error::Conflict { .. }) => last_err = Some(e),
                Err(e) => return Err(e.into()),
            }
        }
        if done {
            return Ok((
                json!({
                    "action": "claim",
                    "key": key,
                    "claimed": false,
                    "status": "done",
                    "hint": "This task is already done.",
                }),
                ClaimOutcome::Done,
            ));
        }
        let Some(commit) = committed else {
            return Err(last_err.map_or_else(
                || BoardError::Bad("the claim could not be committed".to_owned()),
                BoardError::Engine,
            ));
        };
        let until = self.clock.now() + Duration::from_secs(seconds);
        leases.insert(
            key.to_owned(),
            Lease {
                who: who.clone(),
                until,
                seconds,
            },
        );
        Ok((
            json!({
                "action": "claim",
                "key": key,
                "claimed": true,
                "lease_seconds": seconds,
                "commit": commit.to_hex(),
            }),
            ClaimOutcome::Claimed,
        ))
    }

    /// Extend a claim `who` holds by its own lease period. Commits nothing.
    pub fn renew(&self, key: &str, who: &Who) -> Json {
        let mut leases = self.leases();
        let now = self.clock.now();
        match leases.get_mut(key) {
            // Still ours even if it ran out, as long as nobody has claimed it
            // since: taking it back is renewing it.
            Some(lease) if lease.who == *who => {
                lease.until = now + Duration::from_secs(lease.seconds);
                json!({ "action": "renew", "key": key, "renewed": true,
                        "lease_seconds": lease.seconds })
            }
            _ => json!({
                "action": "renew",
                "key": key,
                "renewed": false,
                "hint": "You do not hold this task any more. Claim it again before \
                         carrying on with it.",
            }),
        }
    }

    /// Release a claim, leaving the task open.
    pub fn release(&self, db: &Db, branch: &str, key: &str, who: &Who) -> Result<Json, BoardError> {
        self.finish(db, branch, key, who, "open")
    }

    /// Open a task again and drop its claim: a check it had to pass failed.
    pub fn reopen(&self, db: &Db, branch: &str, key: &str, who: &Who) -> Result<Json, BoardError> {
        self.finish(db, branch, key, who, "reopen")
    }

    /// The client holding `key`, if it is somebody other than `who`.
    pub fn held_elsewhere(&self, key: &str, who: &Who) -> Option<String> {
        let leases = self.leases();
        self.current(&leases, key)
            .filter(|l| l.who != *who)
            .map(|l| l.who.client.clone())
    }

    /// Mark a task done. A task with an acceptance command needs the result
    /// of running it, from where the project is: done if it passed, and
    /// reopened, with the claim dropped, if it did not.
    pub fn done(
        &self,
        db: &Db,
        branch: &str,
        key: &str,
        who: &Who,
        acceptance: Option<&crate::plans::Acceptance>,
    ) -> Result<Json, BoardError> {
        let entry = db
            .get(branch, key)?
            .ok_or_else(|| BoardError::Bad(format!("there is no task `{key}` on `{branch}`")))?;
        let task = read_task(&entry);
        let Some(command) =
            crate::plans::normalise_accept(task.get("accept").and_then(Json::as_str))
        else {
            return self.finish(db, branch, key, who, "done");
        };
        let Some(ran) = acceptance else {
            return Err(BoardError::Bad(format!(
                "`{key}` is done only when its acceptance command (`{command}`) exits 0 in \
                 the project, and it runs where the project is: mark it done through \
                 `memfork mcp` or `memfork task done`, which run it and send the result"
            )));
        };
        if ran.command != command {
            return Err(BoardError::Bad(format!(
                "the acceptance result sent was for `{}`, but `{key}` now says `{command}`; \
                 mark it done again",
                ran.command
            )));
        }
        if ran.passed() {
            let mut answer = self.finish(db, branch, key, who, "done")?;
            answer["accepted"] = json!(true);
            return Ok(answer);
        }
        let mut answer = self.finish(db, branch, key, who, "reopen")?;
        answer["accepted"] = json!(false);
        answer["exit_code"] = json!(ran.exit_code);
        answer["timed_out"] = json!(ran.timed_out);
        answer["output"] = json!(ran.output);
        Ok(answer)
    }

    fn finish(
        &self,
        db: &Db,
        branch: &str,
        key: &str,
        who: &Who,
        to: &str,
    ) -> Result<Json, BoardError> {
        let action = match to {
            "done" => "done",
            "reopen" => "reopen",
            _ => "release",
        };
        // A failed acceptance reopens the task: open again, and unclaimed.
        let to = if to == "reopen" { "open" } else { to };
        let mut leases = self.leases();
        if let Some(lease) = self.current(&leases, key) {
            if lease.who != *who {
                return Ok(json!({
                    "action": action,
                    "key": key,
                    "changed": false,
                    "held_by": lease.who.client,
                    "hint": "Somebody else holds this task; only the holder can release \
                             it or mark it done while the claim lasts.",
                }));
            }
        }
        let mut committed = None;
        let mut last_err = None;
        for _ in 0..RETRIES {
            let mut txn = db.begin(branch)?;
            let Some(entry) = txn.get(key) else {
                return Err(BoardError::Bad(format!(
                    "there is no task `{key}` on `{branch}`"
                )));
            };
            let mut task = read_task(&entry);
            task.insert("status".to_owned(), json!(to));
            task.insert("holder".to_owned(), Json::Null);
            if to == "done" {
                task.insert("done_by".to_owned(), json!(who.client));
            }
            txn.put(key, value(&task, Some(&who.client)))?;
            match txn.commit(Some(format!("{action} {key}"))) {
                Ok(id) => {
                    committed = Some(id);
                    break;
                }
                Err(e @ memfork_core::Error::Conflict { .. }) => last_err = Some(e),
                Err(e) => return Err(e.into()),
            }
        }
        let Some(commit) = committed else {
            return Err(last_err.map_or_else(
                || BoardError::Bad("the change could not be committed".to_owned()),
                BoardError::Engine,
            ));
        };
        leases.remove(key);
        Ok(json!({
            "action": action,
            "key": key,
            "changed": true,
            "status": to,
            "commit": commit.to_hex(),
        }))
    }

    /// Each task's dependencies, by id, leaving out the ids in `except`.
    fn graph(
        &self,
        db: &Db,
        branch: &str,
        prefix: &str,
        except: &[&str],
    ) -> Result<BTreeMap<String, Vec<String>>, BoardError> {
        Ok(db
            .list(branch, prefix, None)?
            .iter()
            .filter_map(|(k, e)| {
                let id = k.strip_prefix(prefix)?;
                (!except.contains(&id)).then(|| (id.to_owned(), depends_on(&read_task(e))))
            })
            .collect())
    }

    /// Whether any task in `ns` depends on another, which is when saying
    /// which tasks are ready tells anybody anything.
    pub fn uses_plans(&self, db: &Db, branch: &str, ns: &str) -> bool {
        let prefix = namespace::prefix(ns, "task");
        db.list(branch, &prefix, None)
            .map(|entries| {
                entries
                    .iter()
                    .any(|(_, e)| !depends_on(&read_task(e)).is_empty())
            })
            .unwrap_or(false)
    }

    /// For every unfinished task in `ns`: the tasks it waits on that are not
    /// done. An empty list means it is ready.
    pub fn readiness(&self, db: &Db, branch: &str, ns: &str) -> Result<Readiness, BoardError> {
        let prefix = namespace::prefix(ns, "task");
        let entries = db.list(branch, &prefix, None)?;
        let status: BTreeMap<&str, String> = entries
            .iter()
            .filter_map(|(k, e)| Some((k.strip_prefix(&prefix)?, status_of(&read_task(e)))))
            .collect();
        Ok(entries
            .iter()
            .filter_map(|(k, e)| {
                let id = k.strip_prefix(&prefix)?;
                let task = read_task(e);
                (status_of(&task) != "done").then(|| {
                    let waiting = depends_on(&task)
                        .into_iter()
                        .filter(|d| status.get(d.as_str()).map(String::as_str) != Some("done"))
                        .collect();
                    (id.to_owned(), waiting)
                })
            })
            .collect())
    }

    /// Write a whole plan in one commit. A task the plan names that already
    /// exists is replaced only while it is open and unclaimed; dependencies
    /// must exist in the plan or the project, and a cycle is refused.
    pub fn plan(
        &self,
        db: &Db,
        branch: &str,
        ns: &str,
        tasks: &[crate::plans::PlanTask],
        plan_file: Option<&str>,
        by: Option<&str>,
    ) -> Result<Json, BoardError> {
        let mut tasks = tasks.to_vec();
        for task in &mut tasks {
            validate_id(&task.id)?;
            task.title = clip(&task.title, MAX_TITLE_CHARS);
            task.detail = task
                .detail
                .as_deref()
                .map(|d| clip(d, MAX_DETAIL_CHARS))
                .filter(|d| !d.is_empty());
            task.accept = crate::plans::normalise_accept(task.accept.as_deref());
        }
        crate::plans::check_shape(&tasks).map_err(BoardError::Bad)?;
        let prefix = namespace::prefix(ns, "task");
        let leases = self.leases();
        let ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let mut last_err = None;
        for _ in 0..RETRIES {
            let others = self.graph(db, branch, &prefix, &ids)?;
            crate::plans::check_graph(&tasks, &others).map_err(BoardError::Bad)?;
            let mut txn = db.begin(branch)?;
            let mut replaced = Vec::new();
            for task in &tasks {
                let key = format!("{prefix}{}", task.id);
                let mut fields = Map::new();
                if let Some(old) = txn.get(&key) {
                    let old = read_task(&old);
                    if status_of(&old) != "open" || self.current(&leases, &key).is_some() {
                        return Err(BoardError::Bad(format!(
                            "task `{}` is already {}; a plan can replace only tasks that are \
                             open and unclaimed, so give this one another id",
                            task.id,
                            if status_of(&old) == "done" {
                                "done"
                            } else {
                                "claimed"
                            }
                        )));
                    }
                    replaced.push(task.id.clone());
                    if let Some(claims) = old.get("claims") {
                        fields.insert("claims".to_owned(), claims.clone());
                    }
                }
                fields.insert("title".to_owned(), json!(task.title));
                if let Some(detail) = &task.detail {
                    fields.insert("detail".to_owned(), json!(detail));
                }
                plan_fields(&mut fields, task, plan_file);
                fields.insert("status".to_owned(), json!("open"));
                fields.insert("holder".to_owned(), Json::Null);
                fields.entry("claims".to_owned()).or_insert(json!(0));
                txn.put(&key, value(&fields, by))?;
            }
            match txn.commit(Some(format!("plan: {} tasks in {ns}", tasks.len()))) {
                Ok(commit) => {
                    drop(leases);
                    let ready: Vec<String> = self
                        .readiness(db, branch, ns)?
                        .into_iter()
                        .filter(|(id, waiting)| waiting.is_empty() && ids.contains(&id.as_str()))
                        .map(|(id, _)| id)
                        .collect();
                    return Ok(json!({
                        "action": "plan",
                        "namespace": ns,
                        "written": ids,
                        "replaced": replaced,
                        "ready": ready,
                        "plan_file": plan_file,
                        "commit": commit.to_hex(),
                    }));
                }
                Err(e @ memfork_core::Error::Conflict { .. }) => last_err = Some(e),
                Err(e) => return Err(e.into()),
            }
        }
        Err(last_err.map_or_else(
            || BoardError::Bad("the plan could not be committed".to_owned()),
            BoardError::Engine,
        ))
    }

    /// The tasks in `ns`, with who holds each right now. `status` filters by
    /// the status that is true now: `open`, `claimed`, `done`, `unfinished`,
    /// `all`, or `ready` and `blocked` among the open ones.
    pub fn list(&self, db: &Db, branch: &str, ns: &str, status: &str) -> Result<Json, BoardError> {
        if !matches!(
            status,
            "open" | "claimed" | "done" | "all" | "unfinished" | "ready" | "blocked"
        ) {
            return Err(BoardError::Bad(format!(
                "`status` must be open, claimed, done, unfinished, ready, blocked or all; \
                 got `{status}`"
            )));
        }
        let prefix = namespace::prefix(ns, "task");
        let entries = db.list(branch, &prefix, None)?;
        let readiness = self.readiness(db, branch, ns)?;
        let mut tasks = Vec::new();
        let mut total = 0usize;
        for (key, entry) in &entries {
            let mut view = self.view(key, entry, &prefix);
            annotate(&mut view, &readiness);
            let now = view["status"].as_str().unwrap_or("open").to_owned();
            let ready = view["ready"] == json!(true);
            let wanted = match status {
                "all" => true,
                "unfinished" => now != "done",
                "ready" => now == "open" && ready,
                "blocked" => now == "open" && !ready,
                other => now == other,
            };
            if wanted {
                total += 1;
                if tasks.len() < MAX_LISTED {
                    tasks.push(view);
                }
            }
        }
        Ok(json!({
            "action": "list",
            "namespace": ns,
            "branch": branch,
            "status": status,
            "count": total,
            "tasks": tasks,
            "omitted": total.saturating_sub(tasks.len()),
        }))
    }

    /// A task as a listing shows it: its committed fields, and the status
    /// that is true now, which a lapsed claim turns back into `open`.
    pub fn view(&self, key: &str, entry: &Entry, prefix: &str) -> Json {
        let task = read_task(entry);
        let committed = status_of(&task);
        let held = self.holder(key);
        let status = match (committed.as_str(), &held) {
            ("done", _) => "done",
            (_, Some(_)) => "claimed",
            _ => "open",
        };
        let mut out = Map::new();
        out.insert(
            "id".to_owned(),
            json!(key.strip_prefix(prefix).unwrap_or(key)),
        );
        out.insert("key".to_owned(), json!(key));
        out.insert(
            "title".to_owned(),
            task.get("title").cloned().unwrap_or(Json::Null),
        );
        if let Some(detail) = task.get("detail") {
            out.insert("detail".to_owned(), detail.clone());
        }
        out.insert("status".to_owned(), json!(status));
        if let Some((client, left)) = held {
            out.insert("held_by".to_owned(), json!(client));
            out.insert("seconds_left".to_owned(), json!(left));
        }
        if let Some(by) = task.get("done_by") {
            out.insert("done_by".to_owned(), by.clone());
        }
        let deps = depends_on(&task);
        if !deps.is_empty() {
            out.insert("depends_on".to_owned(), json!(deps));
        }
        if let Some(accept) = task.get("accept") {
            out.insert("accept".to_owned(), accept.clone());
        }
        out.insert("by".to_owned(), json!(entry.meta.get(WRITTEN_BY)));
        Json::Object(out)
    }

    /// Every lease `who` holds, for a proxy keeping them alive.
    pub fn held_by(&self, who: &Who) -> Vec<(String, u64)> {
        self.leases()
            .iter()
            .filter(|(_, l)| l.who == *who)
            .map(|(k, l)| (k.clone(), l.seconds))
            .collect()
    }
}

/// The ids a task depends on, as it was written.
fn depends_on(task: &Map<String, Json>) -> Vec<String> {
    task.get("depends_on")
        .and_then(Json::as_array)
        .map(|deps| {
            deps.iter()
                .filter_map(Json::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A plan's fields on a task entry. Written only when present, so a task
/// with none of them is stored exactly as a plain one is.
fn plan_fields(task: &mut Map<String, Json>, planned: &crate::plans::PlanTask, file: Option<&str>) {
    if !planned.depends_on.is_empty() {
        task.insert("depends_on".to_owned(), json!(planned.depends_on));
    }
    if let Some(accept) = &planned.accept {
        task.insert("accept".to_owned(), json!(accept));
        if let Some(t) = planned.timeout_seconds {
            task.insert("timeout_seconds".to_owned(), json!(t));
        }
        if let Some(file) = file {
            task.insert("plan_file".to_owned(), json!(file));
        }
    }
}

/// Mark a task view ready, or blocked and by what. Done tasks are neither.
pub fn annotate(view: &mut Json, readiness: &Readiness) {
    let Some(id) = view["id"].as_str().map(str::to_owned) else {
        return;
    };
    if let Some(waiting) = readiness.get(&id) {
        view["ready"] = json!(waiting.is_empty());
        if !waiting.is_empty() {
            view["blocked_by"] = json!(waiting);
        }
    }
}

fn value(task: &Map<String, Json>, by: Option<&str>) -> Value {
    let mut value = Value::new(Json::Object(task.clone()).to_string());
    if let Some(by) = by {
        value = value.with_meta(WRITTEN_BY, by);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn who(client: &str, session: &str) -> Who {
        Who {
            client: client.to_owned(),
            session: session.to_owned(),
        }
    }

    fn board() -> (Board, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::default());
        (Board::with_clock(clock.clone()), clock)
    }

    #[test]
    fn one_claim_wins_and_a_lapsed_one_frees_the_task() {
        let (board, clock) = board();
        let db = Db::new();
        let added = board
            .add(
                &db,
                "main",
                "shop",
                NewTask {
                    id: None,
                    title: "refunds",
                    ..NewTask::default()
                },
                Some("a"),
            )
            .unwrap();
        let key = added["key"].as_str().unwrap().to_owned();
        assert_eq!(key, "shop:task:1");

        let a = who("claude-code", "s1");
        let b = who("claude-code", "s2");
        let (first, outcome) = board.claim(&db, "main", &key, &a, 60).unwrap();
        assert_eq!(outcome, ClaimOutcome::Claimed, "{first}");
        let (second, outcome) = board.claim(&db, "main", &key, &b, 60).unwrap();
        assert_eq!(outcome, ClaimOutcome::Held);
        assert_eq!(second["held_by"], "claude-code");
        assert_eq!(second["same_client"], true);

        clock.advance(Duration::from_secs(61));
        let (third, outcome) = board.claim(&db, "main", &key, &b, 60).unwrap();
        assert_eq!(outcome, ClaimOutcome::Claimed, "{third}");
        // And the one whose lease lapsed has lost it.
        assert_eq!(board.renew(&key, &a)["renewed"], false);
    }

    #[test]
    fn renewing_commits_nothing_and_keeps_the_claim() {
        let (board, clock) = board();
        let db = Db::new();
        board
            .add(
                &db,
                "main",
                "p",
                NewTask {
                    id: Some("x"),
                    title: "t",
                    ..NewTask::default()
                },
                None,
            )
            .unwrap();
        let a = who("c", "s1");
        board.claim(&db, "main", "p:task:x", &a, 10).unwrap();
        let head = db.head("main").unwrap();
        for _ in 0..5 {
            clock.advance(Duration::from_secs(8));
            assert_eq!(board.renew("p:task:x", &a)["renewed"], true);
        }
        assert_eq!(
            db.head("main").unwrap(),
            head,
            "a renewal committed something"
        );
        assert_eq!(board.holder("p:task:x").unwrap().0, "c");
    }

    #[test]
    fn only_the_holder_finishes_while_the_claim_lasts() {
        let (board, _) = board();
        let db = Db::new();
        board
            .add(
                &db,
                "main",
                "p",
                NewTask {
                    id: Some("x"),
                    title: "t",
                    ..NewTask::default()
                },
                None,
            )
            .unwrap();
        let (a, b) = (who("c1", "s1"), who("c2", "s2"));
        board.claim(&db, "main", "p:task:x", &a, 60).unwrap();
        assert_eq!(
            board.done(&db, "main", "p:task:x", &b, None).unwrap()["changed"],
            false
        );
        assert_eq!(
            board.done(&db, "main", "p:task:x", &a, None).unwrap()["changed"],
            true
        );
        let (again, outcome) = board.claim(&db, "main", "p:task:x", &b, 60).unwrap();
        assert_eq!(outcome, ClaimOutcome::Done, "{again}");
        let listed = board.list(&db, "main", "p", "done").unwrap();
        assert_eq!(listed["tasks"][0]["done_by"], "c1");
    }

    #[test]
    fn the_committed_record_holds_no_clock_reading_or_session() {
        // The same operations at different times, from different sessions,
        // give the same commit ids.
        let run = |advance: u64, session: &str| {
            let (board, clock) = board();
            clock.advance(Duration::from_secs(advance));
            let db = Db::new();
            board
                .add(
                    &db,
                    "main",
                    "p",
                    NewTask {
                        id: Some("x"),
                        title: "t",
                        ..NewTask::default()
                    },
                    Some("c"),
                )
                .unwrap();
            board
                .claim(&db, "main", "p:task:x", &who("c", session), 30)
                .unwrap();
            board
                .done(&db, "main", "p:task:x", &who("c", session), None)
                .unwrap();
            db.head("main").unwrap()
        };
        assert_eq!(run(0, "one"), run(99_999, "two"));
    }

    #[test]
    fn a_plain_text_task_from_before_the_board_is_open() {
        let (board, _) = board();
        let db = Db::new();
        db.put("main", "p:task:old", Value::new("wire up refunds"))
            .unwrap();
        let listed = board.list(&db, "main", "p", "open").unwrap();
        assert_eq!(listed["tasks"][0]["title"], "wire up refunds");
        let (_, outcome) = board
            .claim(&db, "main", "p:task:old", &who("c", "s"), 60)
            .unwrap();
        assert_eq!(outcome, ClaimOutcome::Claimed);
    }

    #[test]
    fn a_lease_follows_the_work_across_branches() {
        let (board, _) = board();
        let db = Db::new();
        board
            .add(
                &db,
                "main",
                "p",
                NewTask {
                    id: Some("x"),
                    title: "t",
                    ..NewTask::default()
                },
                None,
            )
            .unwrap();
        board
            .claim(&db, "main", "p:task:x", &who("c1", "s1"), 60)
            .unwrap();
        db.fork("main", "attempt").unwrap();
        let (_, outcome) = board
            .claim(&db, "attempt", "p:task:x", &who("c2", "s2"), 60)
            .unwrap();
        assert_eq!(
            outcome,
            ClaimOutcome::Held,
            "a fork is not a way round a claim"
        );
    }

    #[test]
    fn ids_and_statuses_are_checked() {
        let (board, _) = board();
        let db = Db::new();
        assert!(board
            .add(
                &db,
                "main",
                "p",
                NewTask {
                    id: Some("a:b"),
                    title: "t",
                    ..NewTask::default()
                },
                None
            )
            .is_err());
        assert!(board
            .add(
                &db,
                "main",
                "p",
                NewTask {
                    id: None,
                    title: "  ",
                    ..NewTask::default()
                },
                None
            )
            .is_err());
        board
            .add(
                &db,
                "main",
                "p",
                NewTask {
                    id: Some("x"),
                    title: "t",
                    ..NewTask::default()
                },
                None,
            )
            .unwrap();
        assert!(board
            .add(
                &db,
                "main",
                "p",
                NewTask {
                    id: Some("x"),
                    title: "t",
                    ..NewTask::default()
                },
                None
            )
            .is_err());
        assert!(board.list(&db, "main", "p", "maybe").is_err());
        assert!(board
            .claim(&db, "main", "p:task:none", &who("c", "s"), 60)
            .is_err());
    }
}
