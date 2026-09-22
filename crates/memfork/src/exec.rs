//! Executing a parsed command against an engine, and rendering the result.

use std::collections::{BTreeMap, BTreeSet};

use memfork_core::{CommitId, Db, MergePolicy, Op, Value, WRITTEN_BY};
use serde_json::{json, Value as Json};

use crate::cli::{parse_meta, parse_vector, Command, PlanAction, TaskAction};
use crate::shared::Shared;
use crate::tools::dispatch::Session;

/// Where a command runs: who it is recorded as, which project it is in, the
/// state shared with the rest of the process, and — when this process can
/// see it — the project's directory, for checking facts.
#[derive(Debug, Clone)]
pub struct Context {
    /// Who writes are recorded as.
    pub writer: Option<String>,
    /// The project namespace.
    pub namespace: String,
    /// Leases, statistics and fact hashes.
    pub shared: std::sync::Arc<Shared>,
    /// The project's directory, if this process can see it.
    pub root: Option<std::path::PathBuf>,
}

impl Context {
    /// A context of its own, in memory, recorded as nobody.
    pub fn alone() -> Self {
        Context {
            writer: None,
            namespace: crate::namespace::FALLBACK.to_owned(),
            shared: Shared::in_memory(),
            root: None,
        }
    }

    /// A session to run a tool through, as this context's writer. Claims
    /// made from the command line all belong to one session, "cli", so a
    /// person can claim in one command and finish in the next.
    fn session(&self, db: &Db) -> Session {
        let mut session = Session::in_namespace(db.clone(), self.namespace.clone())
            .sharing(std::sync::Arc::clone(&self.shared));
        if let Some(root) = &self.root {
            session = session.in_project(root.clone());
        }
        session.set_writer(self.writer.as_deref().unwrap_or(crate::serve::CLI_WRITER));
        session.set_session_id("cli");
        session
    }

    fn tool(&self, db: &Db, branch: &str, name: &str, mut args: Json) -> Result<Json, ExecError> {
        if let Json::Object(map) = &mut args {
            map.insert("branch".to_owned(), json!(branch));
        }
        let Json::Object(map) = args else {
            return Err(ExecError::Usage(
                "internal: tool arguments must be an object".to_owned(),
            ));
        };
        self.session(db).call(name, &map).map_err(|e| match e {
            crate::tools::dispatch::ToolError::Engine(e) => ExecError::Engine(e),
            other => ExecError::Usage(other.to_string()),
        })
    }
}

/// What a command produced: the same information twice, once for a person and
/// once for a program.
#[derive(Debug)]
pub struct Outcome {
    /// Lines to print in text mode.
    pub text: Vec<String>,
    /// The same result as JSON.
    pub json: Json,
}

impl Outcome {
    fn new(text: Vec<String>, json: Json) -> Self {
        Outcome { text, json }
    }

    fn line(text: impl Into<String>, json: Json) -> Self {
        Outcome::new(vec![text.into()], json)
    }
}

/// Anything that can go wrong running a command.
#[derive(Debug)]
pub enum ExecError {
    /// The engine rejected the operation.
    Engine(memfork_core::Error),
    /// The arguments did not make sense.
    Usage(String),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::Engine(e) => write!(f, "{e}"),
            ExecError::Usage(m) => write!(f, "{m}"),
        }
    }
}

impl From<memfork_core::Error> for ExecError {
    fn from(e: memfork_core::Error) -> Self {
        ExecError::Engine(e)
    }
}

fn short(id: memfork_core::CommitId) -> String {
    id.to_hex()[..12].to_owned()
}

/// Run one command against `db` on `branch`, alone and in memory.
pub fn execute(db: &Db, branch: &str, command: &Command) -> Result<Outcome, ExecError> {
    execute_in(db, branch, command, &Context::alone())
}

/// Run one command in a context: the daemon's for the command line on the
/// shared store, a local one for `--ephemeral` and scripts.
pub fn execute_in(
    db: &Db,
    branch: &str,
    command: &Command,
    ctx: &Context,
) -> Result<Outcome, ExecError> {
    let writer = ctx.writer.as_deref();
    match command {
        // A fact goes through the tool, which records its sources' hashes
        // beside the store.
        Command::Put {
            key,
            value,
            importance,
            embedding,
            ttl_commits,
            meta,
            sources,
            source_hashes,
            allow_secret,
        } if !sources.is_empty() => {
            let mut args = json!({ "key": key, "value": value, "sources": sources,
                                   "allow_secret": allow_secret });
            if let Some(i) = importance {
                args["importance"] = json!(i);
            }
            if let Some(e) = embedding {
                args["embedding"] = json!(parse_vector(e).map_err(ExecError::Usage)?);
            }
            if let Some(t) = ttl_commits {
                args["ttl_commits"] = json!(t);
            }
            if !meta.is_empty() {
                let mut pairs = serde_json::Map::new();
                for pair in meta {
                    let (k, v) = parse_meta(pair).map_err(ExecError::Usage)?;
                    pairs.insert(k, json!(v));
                }
                args["meta"] = Json::Object(pairs);
            }
            if let Some(hashes) = source_hashes {
                args["source_hashes"] = json!(hashes);
            }
            let result = ctx.tool(db, branch, "memfork_put", args)?;
            let commit = result["commit"].as_str().unwrap_or("");
            Ok(Outcome::line(
                format!(
                    "put {key} on {branch} @ {} (a fact with {} source{})",
                    &commit[..commit.len().min(12)],
                    sources.len(),
                    if sources.len() == 1 { "" } else { "s" }
                ),
                json!({"op": "put", "branch": branch, "key": key, "commit": commit,
                       "sources": result["sources"]}),
            ))
        }

        Command::Discard {
            name,
            lesson: Some(lesson),
            allow_secret,
        } => {
            let result = ctx.tool(
                db,
                branch,
                "memfork_discard",
                json!({ "name": name, "lesson": lesson, "allow_secret": allow_secret }),
            )?;
            let key = result["lesson"]["key"].as_str().unwrap_or("");
            let parent = result["lesson"]["branch"].as_str().unwrap_or("");
            Ok(Outcome::new(
                vec![
                    format!("discarded {name}"),
                    format!("  lesson kept on {parent} as {key}"),
                ],
                json!({"op": "discard", "name": name, "lesson": result["lesson"]}),
            ))
        }

        Command::Find { text, k, prefix } => {
            let mut args = json!({ "text": text, "k": k });
            if let Some(p) = prefix {
                args["prefix"] = json!(p);
            }
            let result = ctx.tool(db, branch, "memfork_search", args)?;
            let hits = result["hits"].as_array().cloned().unwrap_or_default();
            let width = hits
                .iter()
                .filter_map(|h| h["key"].as_str())
                .map(|k| k.chars().count())
                .max()
                .unwrap_or(0);
            let text = hits
                .iter()
                .map(|h| {
                    let key = h["key"].as_str().unwrap_or_default();
                    let pad = width - key.chars().count();
                    format!(
                        "{key}{}  {}",
                        " ".repeat(pad),
                        h["snippet"].as_str().unwrap_or("")
                    )
                })
                .collect();
            Ok(Outcome::new(
                text,
                json!({"op": "find", "branch": branch, "result": result}),
            ))
        }

        Command::Task {
            action,
            namespace,
            allow_secret,
        } => {
            let mut args = match action {
                TaskAction::Add {
                    title,
                    id,
                    detail,
                    depends_on,
                    accept,
                    timeout_seconds,
                } => json!({
                    "action": "add", "title": title, "id": id, "detail": detail,
                    "depends_on": depends_on, "accept": accept,
                    "timeout_seconds": timeout_seconds,
                }),
                TaskAction::Claim { id, lease } => {
                    json!({"action": "claim", "id": id, "lease_seconds": lease})
                }
                TaskAction::Renew { id } => json!({"action": "renew", "id": id}),
                TaskAction::Release { id } => json!({"action": "release", "id": id}),
                TaskAction::Done { id, acceptance } => {
                    let mut args = json!({"action": "done", "id": id});
                    if let Some(ran) = acceptance {
                        args["acceptance"] = json!(ran);
                    }
                    args
                }
                TaskAction::List { status } => json!({"action": "list", "status": status}),
            };
            if let Some(ns) = namespace {
                args["namespace"] = json!(ns);
            }
            if let Some(allow) = allow_secret {
                args["allow_secret"] = json!(allow);
            }
            let result = ctx.tool(db, branch, "memfork_task", args)?;
            Ok(Outcome::new(
                task_lines(&result),
                json!({"op": "task", "result": result}),
            ))
        }

        Command::Plan {
            action,
            namespace,
            allow_secret,
        } => {
            let mut args = match action {
                PlanAction::Write {
                    tasks: Some(tasks),
                    plan_file,
                    ..
                } => json!({"action": "plan", "tasks": tasks, "plan_file": plan_file}),
                PlanAction::Write { .. }
                | PlanAction::Check { .. }
                | PlanAction::New { .. }
                | PlanAction::Templates => {
                    return Err(ExecError::Usage(
                        "a plan file is read where it is, before the command is sent; \
                         run `memfork plan` from the project"
                            .to_owned(),
                    ))
                }
                PlanAction::Show => json!({"action": "list", "status": "all"}),
            };
            if let Some(ns) = namespace {
                args["namespace"] = json!(ns);
            }
            if let Some(allow) = allow_secret {
                args["allow_secret"] = json!(allow);
            }
            let result = ctx.tool(db, branch, "memfork_task", args)?;
            let lines = match action {
                PlanAction::Show => plan_lines(&result),
                _ => task_lines(&result),
            };
            Ok(Outcome::new(lines, json!({"op": "plan", "result": result})))
        }

        Command::Facts { prefix, namespace } => {
            let ns = namespace.clone().unwrap_or_else(|| ctx.namespace.clone());
            let prefix = prefix
                .clone()
                .unwrap_or_else(|| format!("{ns}{}", crate::namespace::SEPARATOR));
            let facts: Vec<Json> = db
                .list(branch, &prefix, None)?
                .iter()
                .filter(|(_, e)| crate::facts::sources_of(&e.meta).is_some())
                .map(|(k, e)| {
                    let mut fact = json!({
                        "key": k,
                        "value": String::from_utf8_lossy(&e.value),
                        "by": e.meta.get(WRITTEN_BY),
                    });
                    crate::tools::handoff::with_fact_fields(&mut fact, k, e, &ctx.shared.sidecar);
                    fact
                })
                .collect();
            let mut json =
                json!({"op": "facts", "branch": branch, "prefix": prefix, "facts": facts});
            if let Some(root) = &ctx.root {
                crate::facts::check(&mut json, root, &ctx.shared.hasher);
            }
            Ok(Outcome::new(Vec::new(), json))
        }

        Command::Lessons { namespace } => {
            let ns = namespace.clone().unwrap_or_else(|| ctx.namespace.clone());
            let all = crate::lessons::recent(db, branch, &ns, crate::lessons::MAX_LESSONS)?;
            let text = if all.is_empty() {
                vec![format!(
                    "no lessons in `{ns}`; leave one with `memfork discard <branch> --lesson <text>`"
                )]
            } else {
                all.iter()
                    .map(|l| {
                        format!(
                            "{}  {}  (from {}, by {})",
                            l["key"].as_str().unwrap_or_default(),
                            l["lesson"].as_str().unwrap_or_default(),
                            l["branch"].as_str().unwrap_or("?"),
                            l["by"].as_str().unwrap_or("unknown"),
                        )
                    })
                    .collect()
            };
            Ok(Outcome::new(
                text,
                json!({"op": "lessons", "branch": branch, "namespace": ns, "lessons": all}),
            ))
        }

        Command::Stats { project } => Ok(Outcome::new(
            Vec::new(),
            json!({"op": "stats", "stats": ctx.shared.sidecar.stats(project.as_deref())}),
        )),

        Command::Put {
            key,
            value,
            importance,
            embedding,
            ttl_commits,
            meta,
            allow_secret,
            ..
        } => {
            let pairs = meta
                .iter()
                .map(|pair| parse_meta(pair).map_err(ExecError::Usage))
                .collect::<Result<Vec<_>, _>>()?;
            let allow = crate::secrets::Allow::parse(allow_secret.as_deref())
                .map_err(|r| ExecError::Usage(r.to_string()))?;
            crate::secrets::check_all(
                [
                    ("key".to_owned(), key.as_str()),
                    ("value".to_owned(), value.as_str()),
                ]
                .into_iter()
                .chain(pairs.iter().map(|(k, v)| (format!("meta.{k}"), v.as_str()))),
                &allow,
            )
            .map_err(|r| ExecError::Usage(r.to_string()))?;
            let mut v = Value::new(value.clone());
            if let Some(i) = importance {
                v = v.with_importance(*i);
            }
            if let Some(e) = embedding {
                v = v.with_embedding(parse_vector(e).map_err(ExecError::Usage)?);
            }
            if let Some(t) = ttl_commits {
                v = v.with_ttl_commits(*t);
            }
            for pair in meta {
                let (k, val) = parse_meta(pair).map_err(ExecError::Usage)?;
                if k.starts_with("memfork.") {
                    return Err(ExecError::Usage(format!(
                        "`--meta {k}` is reserved: keys starting with `memfork.` are set \
                         by MemFork itself"
                    )));
                }
                v = v.with_meta(k, val);
            }
            if let Some(writer) = writer {
                v = v.with_meta(WRITTEN_BY, writer);
            }
            let id = db.put(branch, key, v)?;
            Ok(Outcome::line(
                format!("put {key} on {branch} @ {}", short(id)),
                json!({"op": "put", "branch": branch, "key": key, "commit": id.to_hex()}),
            ))
        }

        Command::Get { key } => match db.get(branch, key)? {
            Some(entry) => {
                let value = String::from_utf8_lossy(&entry.value).into_owned();
                Ok(Outcome::line(
                    value.clone(),
                    json!({
                        "op": "get",
                        "branch": branch,
                        "key": key,
                        "found": true,
                        "value": value,
                        "importance": entry.importance,
                        "created_seq": entry.created_seq,
                        "last_access_seq": entry.last_access_seq,
                        "ttl_commits": entry.ttl_commits,
                        "embedding_dim": entry.embedding.as_ref().map(Vec::len),
                        "meta": entry.meta,
                    }),
                ))
            }
            None => Ok(Outcome::new(
                Vec::new(),
                json!({"op": "get", "branch": branch, "key": key, "found": false}),
            )),
        },

        Command::Del { key } => {
            let id = db.delete(branch, key)?;
            Ok(Outcome::line(
                format!("deleted {key} on {branch} @ {}", short(id)),
                json!({"op": "del", "branch": branch, "key": key, "commit": id.to_hex()}),
            ))
        }

        Command::Ls { prefix, limit, .. } => {
            let entries = db.list(branch, prefix, *limit)?;
            let text = key_value_lines(&entries);
            let json = json!({
                "op": "ls",
                "branch": branch,
                "prefix": prefix,
                "keys": entries.iter().map(|(k, e)| json!({
                    "key": k,
                    "value": String::from_utf8_lossy(&e.value),
                })).collect::<Vec<_>>(),
            });
            Ok(Outcome::new(text, json))
        }

        Command::Search {
            embedding,
            k,
            prefix,
        } => {
            let query = parse_vector(embedding).map_err(ExecError::Usage)?;
            let hits = db.search(branch, &query, *k, prefix.as_deref())?;
            let text = hits
                .iter()
                .map(|h| format!("{:.6}\t{}", h.score, h.key))
                .collect();
            let json = json!({
                "op": "search",
                "branch": branch,
                "hits": hits.iter().map(|h| json!({
                    "key": h.key,
                    "score": h.score,
                    "value": String::from_utf8_lossy(&h.entry.value),
                })).collect::<Vec<_>>(),
            });
            Ok(Outcome::new(text, json))
        }

        Command::Fork { name, from, at_seq } => {
            let from = from.as_deref().unwrap_or(branch);
            let head = match at_seq {
                Some(seq) => db.fork_at(from, *seq, name)?,
                None => db.fork(from, name)?,
            };
            Ok(Outcome::line(
                format!("forked {name} from {from} @ {}", short(head)),
                json!({
                    "op": "fork",
                    "name": name,
                    "from": from,
                    "at_seq": at_seq,
                    "commit": head.to_hex(),
                }),
            ))
        }

        Command::Merge {
            source,
            target,
            policy,
        } => {
            let target = target.as_deref().unwrap_or(branch);
            let policy = MergePolicy::parse(policy)
                .ok_or_else(|| ExecError::Usage(format!("unknown merge policy `{policy}`")))?;
            let outcome = db.merge(source, target, policy)?;
            let kind = outcome.kind.as_str();
            let mut text = vec![format!(
                "{kind}: {source} into {target} @ {} ({} key(s) changed)",
                short(outcome.head),
                outcome.changed.len()
            )];
            for key in &outcome.conflicts {
                text.push(format!("  resolved conflict: {key}"));
            }
            Ok(Outcome::new(
                text,
                json!({
                    "op": "merge",
                    "source": source,
                    "target": target,
                    "policy": policy.as_str(),
                    "kind": kind,
                    "commit": outcome.head.to_hex(),
                    "base": outcome.base.to_hex(),
                    "changed": outcome.changed,
                    "conflicts": outcome.conflicts,
                }),
            ))
        }

        Command::Discard { name, .. } => {
            db.discard(name)?;
            Ok(Outcome::line(
                format!("discarded {name}"),
                json!({"op": "discard", "name": name}),
            ))
        }

        Command::Branches => {
            let branches = db.branches();
            let graph = Ancestry::of(db);
            let default_head = branches.iter().find(|b| b.is_default).map(|b| b.head);
            let text = branches
                .iter()
                .map(|b| {
                    format!(
                        "{}{}\t{}\tseq {}\t{} key(s)",
                        if b.is_default { "* " } else { "  " },
                        b.name,
                        short(b.head),
                        b.seq,
                        b.key_count
                    )
                })
                .collect();
            let json = json!({
                "op": "branches",
                "branches": branches.iter().map(|b| {
                    let (ahead, behind) = match default_head {
                        Some(main) if !b.is_default => graph.ahead_behind(b.head, main),
                        _ => (0, 0),
                    };
                    json!({
                        "name": b.name,
                        "head": b.head.to_hex(),
                        "seq": b.seq,
                        "key_count": b.key_count,
                        "is_default": b.is_default,
                        "forked_at": b.forked_at.to_hex(),
                        "forked_at_seq": graph.seq(b.forked_at),
                        "ahead": ahead,
                        "behind": behind,
                        "written_by": graph.writer(b.head),
                    })
                }).collect::<Vec<_>>(),
            });
            Ok(Outcome::new(text, json))
        }

        Command::Log { limit, graph: true } => Ok(Outcome::new(
            Vec::new(),
            json!({ "op": "log", "graph": history(db, *limit) }),
        )),

        Command::Log {
            limit,
            graph: false,
        } => {
            let entries = db.log(branch, *limit)?;
            let text = entries
                .iter()
                .map(|e| {
                    format!(
                        "{}\tseq {}\t{} op(s)\t{}",
                        short(e.id),
                        e.seq,
                        e.op_count,
                        e.message.as_deref().unwrap_or("")
                    )
                })
                .collect();
            let json = json!({
                "op": "log",
                "branch": branch,
                "commits": entries.iter().map(|e| json!({
                    "id": e.id.to_hex(),
                    "seq": e.seq,
                    "parents": e.parents.iter().map(|p| p.to_hex()).collect::<Vec<_>>(),
                    "message": e.message,
                    "op_count": e.op_count,
                    "key_count": e.key_count,
                })).collect::<Vec<_>>(),
            });
            Ok(Outcome::new(text, json))
        }

        Command::At {
            seq, key, prefix, ..
        } => {
            let view = db.at(branch, *seq)?;
            match key {
                Some(key) => match view.get(key) {
                    Some(entry) => {
                        let value = String::from_utf8_lossy(&entry.value).into_owned();
                        Ok(Outcome::line(
                            value.clone(),
                            json!({
                                "op": "at",
                                "branch": branch,
                                "seq": seq,
                                "key": key,
                                "found": true,
                                "value": value,
                            }),
                        ))
                    }
                    None => Ok(Outcome::new(
                        Vec::new(),
                        json!({
                            "op": "at", "branch": branch, "seq": seq,
                            "key": key, "found": false,
                        }),
                    )),
                },
                None => {
                    let entries = view.list(prefix.as_deref().unwrap_or(""), None);
                    let text = key_value_lines(&entries);
                    let json = json!({
                        "op": "at",
                        "branch": branch,
                        "seq": seq,
                        "commit": view.commit_id().to_hex(),
                        "keys": entries.iter().map(|(k, e)| json!({
                            "key": k,
                            "value": String::from_utf8_lossy(&e.value),
                        })).collect::<Vec<_>>(),
                    });
                    Ok(Outcome::new(text, json))
                }
            }
        }

        Command::Diff { a, b } => {
            let changes = db.diff(a, b)?;
            let text = changes
                .iter()
                .map(|c| format!("{} {}", c.kind.marker(), c.key))
                .collect();
            let json = json!({
                "op": "diff",
                "a": a,
                "b": b,
                "changes": changes.iter().map(|c| json!({
                    "key": c.key,
                    "kind": match c.kind {
                        memfork_core::ChangeKind::Added => "added",
                        memfork_core::ChangeKind::Removed => "removed",
                        memfork_core::ChangeKind::Modified => "modified",
                    },
                })).collect::<Vec<_>>(),
            });
            Ok(Outcome::new(text, json))
        }

        // `run` is handled by the caller, which owns the script source.
        Command::Run { .. } => Err(ExecError::Usage(
            "`run` cannot be executed from inside a script".to_owned(),
        )),

        // These are handled by `main`, which owns the pieces they need: an
        // async runtime, the client registry, the filesystem. Reaching here
        // means a script line slipped past `allowed_in_script`.
        Command::Mcp { .. }
        | Command::CrashWriter { .. }
        | Command::Tools { .. }
        | Command::Call { .. }
        | Command::Init { .. }
        | Command::Serve { .. }
        | Command::Stop
        | Command::Watch { .. }
        | Command::Doctor => Err(ExecError::Usage(format!(
            "`memfork {}` is not an operation on a database",
            command.name()
        ))),
    }
}

/// One line per key, the values starting in one column: each key padded to
/// the widest, then two spaces. A tab would leave rows out of line whenever
/// two keys fall either side of a tab stop.
fn key_value_lines<E: std::ops::Deref<Target = memfork_core::Entry>>(
    entries: &[(String, E)],
) -> Vec<String> {
    let width = entries
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    entries
        .iter()
        .map(|(k, e)| {
            let pad = width - k.chars().count();
            format!(
                "{k}{}  {}",
                " ".repeat(pad),
                String::from_utf8_lossy(&e.value)
            )
        })
        .collect()
}

/// What an operation was done to, for the activity feed: the key, if any, and
/// the branch as a person would want to read it.
pub fn target(command: &Command, branch: &str) -> (Option<String>, Option<String>) {
    let on = Some(branch.to_owned());
    match command {
        Command::Put { key, .. } | Command::Get { key } | Command::Del { key } => {
            (Some(key.clone()), on)
        }
        Command::Ls { prefix, .. } => ((!prefix.is_empty()).then(|| prefix.clone()), on),
        Command::Search { prefix, .. } => (prefix.clone(), on),
        Command::At { key, prefix, .. } => (key.clone().or_else(|| prefix.clone()), on),
        Command::Fork { name, .. } | Command::Discard { name, .. } => (None, Some(name.clone())),
        Command::Find { prefix, .. } => (prefix.clone(), on),
        Command::Task {
            action, namespace, ..
        } => {
            let ns = namespace.as_deref().unwrap_or("");
            let id = match action {
                TaskAction::Claim { id, .. }
                | TaskAction::Renew { id }
                | TaskAction::Release { id }
                | TaskAction::Done { id, .. } => Some(id.clone()),
                TaskAction::Add { id, .. } => id.clone(),
                TaskAction::List { .. } => None,
            };
            (
                id.map(|id| {
                    if ns.is_empty() {
                        id
                    } else {
                        crate::board::task_key(ns, &id)
                    }
                }),
                on,
            )
        }
        Command::Merge { source, target, .. } => (
            None,
            Some(format!(
                "{source} -> {}",
                target.as_deref().unwrap_or(branch)
            )),
        ),
        Command::Diff { a, b } => (None, Some(format!("{a} .. {b}"))),
        _ => (None, on),
    }
}

/// The commit graph, for questions about ancestry.
struct Ancestry {
    commits: BTreeMap<CommitId, std::sync::Arc<memfork_core::Commit>>,
}

impl Ancestry {
    fn of(db: &Db) -> Self {
        Ancestry {
            commits: db.all_commits().into_iter().map(|c| (c.id, c)).collect(),
        }
    }

    fn ancestors(&self, from: CommitId) -> BTreeSet<CommitId> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![from];
        while let Some(id) = stack.pop() {
            if seen.insert(id) {
                if let Some(c) = self.commits.get(&id) {
                    stack.extend(c.parents.iter().copied());
                }
            }
        }
        seen
    }

    /// Commits `branch` has that `base` lacks, and the other way round.
    fn ahead_behind(&self, branch: CommitId, base: CommitId) -> (usize, usize) {
        let mine = self.ancestors(branch);
        let theirs = self.ancestors(base);
        (
            mine.difference(&theirs).count(),
            theirs.difference(&mine).count(),
        )
    }

    fn seq(&self, id: CommitId) -> Option<u64> {
        self.commits.get(&id).map(|c| c.seq)
    }

    /// Who wrote a commit, as far as its puts say.
    fn writer(&self, id: CommitId) -> Option<String> {
        self.commits.get(&id).and_then(|c| writer_of(c))
    }
}

fn writer_of(commit: &memfork_core::Commit) -> Option<String> {
    commit.ops.iter().find_map(|op| match op {
        Op::Put { value, .. } => value.meta.get(WRITTEN_BY).cloned(),
        _ => None,
    })
}

/// The newest lesson left about each discarded branch, from every existing
/// branch, by the discarded branch's name.
fn lessons_by_branch(db: &Db) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for b in db.branches() {
        let Ok(entries) = db.list(&b.name, "", None) else {
            continue;
        };
        for (key, entry) in entries {
            if !key.contains(":lesson:") {
                continue;
            }
            let view = crate::lessons::view(&key, &entry);
            if let (Some(branch), Some(lesson)) = (view["branch"].as_str(), view["lesson"].as_str())
            {
                out.insert(branch.to_owned(), lesson.to_owned());
            }
        }
    }
    out
}

/// Everything `memfork log --graph` draws: every commit newest first, the
/// branch heads, and the discarded branches the graph no longer holds.
///
/// "Newest first" is a topological order — a commit always comes before its
/// parents — with ties broken by sequence number and then id, so the same
/// history always draws the same way.
pub fn history(db: &Db, limit: Option<usize>) -> Json {
    let commits: BTreeMap<CommitId, std::sync::Arc<memfork_core::Commit>> =
        db.all_commits().into_iter().map(|c| (c.id, c)).collect();
    let mut children: BTreeMap<CommitId, usize> = commits.keys().map(|id| (*id, 0)).collect();
    for c in commits.values() {
        for p in &c.parents {
            if let Some(n) = children.get_mut(p) {
                *n += 1;
            }
        }
    }
    let forks: BTreeSet<CommitId> = children
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(id, _)| *id)
        .collect();

    let mut waiting = children.clone();
    let mut ready: std::collections::BinaryHeap<(u64, std::cmp::Reverse<CommitId>)> = waiting
        .iter()
        .filter(|(_, n)| **n == 0)
        .filter_map(|(id, _)| commits.get(id).map(|c| (c.seq, std::cmp::Reverse(*id))))
        .collect();
    let mut order = Vec::with_capacity(commits.len());
    while let Some((_, std::cmp::Reverse(id))) = ready.pop() {
        let Some(c) = commits.get(&id) else { continue };
        order.push(std::sync::Arc::clone(c));
        for p in &c.parents {
            if let Some(n) = waiting.get_mut(p) {
                *n -= 1;
                if *n == 0 {
                    if let Some(pc) = commits.get(p) {
                        ready.push((pc.seq, std::cmp::Reverse(*p)));
                    }
                }
            }
        }
    }
    let total = order.len();
    let shown = limit.unwrap_or(40).min(total);
    let gone = db.discarded();
    let lessons = if gone.is_empty() {
        BTreeMap::new()
    } else {
        lessons_by_branch(db)
    };
    let discarded: Vec<Json> = gone
        .iter()
        .map(|d| {
            json!({
                "name": d.name,
                "forked_at": d.forked_at.to_hex(),
                "commits": d.commits,
                "lesson": lessons.get(&d.name),
            })
        })
        .collect();

    json!({
        "commits": order.iter().take(shown).map(|c| json!({
            "commit": c.id.to_hex(),
            "parents": c.parents.iter().map(|p| p.to_hex()).collect::<Vec<_>>(),
            "seq": c.seq,
            "message": c.message,
            "changes": c.ops.len(),
            "by": writer_of(c),
            "fork_point": forks.contains(&c.id),
        })).collect::<Vec<_>>(),
        "omitted": total - shown,
        "branches": db.branches().iter().map(|b| json!({
            "name": b.name,
            "head": b.head.to_hex(),
            "is_default": b.is_default,
        })).collect::<Vec<_>>(),
        "discarded": discarded,
    })
}

/// A task board answer, for a person.
/// `memfork plan show`: the board grouped as a plan.
fn plan_lines(result: &Json) -> Vec<String> {
    let tasks = result["tasks"].as_array().cloned().unwrap_or_default();
    if tasks.is_empty() {
        return vec!["no tasks; write a plan with `memfork plan write`".to_owned()];
    }
    let title = |t: &Json| {
        format!(
            "{}  {}",
            t["id"].as_str().unwrap_or(""),
            t["title"].as_str().unwrap_or("")
        )
    };
    let mut ready = Vec::new();
    let mut blocked = Vec::new();
    let mut claimed = Vec::new();
    let mut done = Vec::new();
    for t in &tasks {
        match t["status"].as_str().unwrap_or("open") {
            "done" => done.push(format!("  {}", title(t))),
            "claimed" => claimed.push(format!(
                "  {}  (held by {})",
                title(t),
                t["held_by"].as_str().unwrap_or("someone")
            )),
            _ if t["ready"] == false => blocked.push(format!(
                "  {}  (waiting for {})",
                title(t),
                t["blocked_by"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Json::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            _ => ready.push(format!("  {}", title(t))),
        }
    }
    let mut lines = Vec::new();
    for (name, group) in [
        ("ready", ready),
        ("claimed", claimed),
        ("blocked", blocked),
        ("done", done),
    ] {
        if !group.is_empty() {
            lines.push(format!("{name} ({})", group.len()));
            lines.extend(group);
        }
    }
    lines
}

fn task_lines(result: &Json) -> Vec<String> {
    let key = result["key"].as_str().unwrap_or("");
    match result["action"].as_str().unwrap_or("") {
        "add" => vec![format!(
            "added {key}: {}",
            result["task"]["title"].as_str().unwrap_or("")
        )],
        "claim" if result["claimed"] == true => vec![format!(
            "claimed {key} for {}s",
            result["lease_seconds"].as_u64().unwrap_or(0)
        )],
        "claim" if result["status"] == "done" => vec![format!("{key} is already done")],
        "claim" => vec![format!(
            "{key} is held by {} ({}s left); pick another task",
            result["held_by"].as_str().unwrap_or("someone"),
            result["seconds_left"].as_u64().unwrap_or(0)
        )],
        "renew" if result["renewed"] == true => vec![format!("renewed {key}")],
        "renew" => vec![format!("{key} is not yours any more; claim it again")],
        "release" | "done" if result["changed"] == false => vec![format!(
            "{key} is held by {}; only the holder can change it",
            result["held_by"].as_str().unwrap_or("someone")
        )],
        "release" => vec![format!("released {key}; it is open again")],
        "done" if result["accepted"] == false => {
            let mut lines = vec![format!(
                "{key} is not done: its acceptance command {}; it is open again",
                match (result["timed_out"] == true, result["exit_code"].as_i64()) {
                    (true, _) => "ran out of time".to_owned(),
                    (false, Some(code)) => format!("exited {code}"),
                    (false, None) => "did not finish".to_owned(),
                }
            )];
            if let Some(lesson) = result["lesson"]["key"].as_str() {
                lines.push(format!("  lesson kept as {lesson}"));
            }
            lines
        }
        "done" => {
            let mut lines = vec![format!("{key} is done")];
            let ready: Vec<&str> = result["now_ready"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Json::as_str)
                .collect();
            if !ready.is_empty() {
                lines.push(format!("  now ready: {}", ready.join(", ")));
            }
            lines
        }
        "plan" => {
            let written = result["written"].as_array().map_or(0, Vec::len);
            let ready: Vec<&str> = result["ready"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Json::as_str)
                .collect();
            vec![
                format!(
                    "wrote {written} task{} to the board",
                    if written == 1 { "" } else { "s" }
                ),
                format!(
                    "  ready now: {}",
                    if ready.is_empty() {
                        "none".to_owned()
                    } else {
                        ready.join(", ")
                    }
                ),
            ]
        }
        "list" => {
            let tasks = result["tasks"].as_array().cloned().unwrap_or_default();
            let width = tasks
                .iter()
                .filter_map(|t| t["id"].as_str())
                .map(|i| i.chars().count())
                .max()
                .unwrap_or(0);
            let mut lines: Vec<String> = tasks
                .iter()
                .map(|t| {
                    let id = t["id"].as_str().unwrap_or("");
                    let status = t["status"].as_str().unwrap_or("");
                    let holder = t["held_by"]
                        .as_str()
                        .map(|h| format!(" by {h}"))
                        .unwrap_or_default();
                    format!(
                        "{id}{}  {status:<7}{holder}  {}",
                        " ".repeat(width - id.chars().count()),
                        t["title"].as_str().unwrap_or("")
                    )
                })
                .collect();
            if lines.is_empty() {
                lines.push("no tasks".to_owned());
            }
            lines
        }
        _ => Vec::new(),
    }
}
