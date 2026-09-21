//! Executing a parsed command against an engine, and rendering the result.

use std::collections::{BTreeMap, BTreeSet};

use memfork_core::{CommitId, Db, MergePolicy, Op, Value, WRITTEN_BY};
use serde_json::{json, Value as Json};

use crate::cli::{parse_meta, parse_vector, Command};

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

/// Run one command against `db` on `branch`.
pub fn execute(db: &Db, branch: &str, command: &Command) -> Result<Outcome, ExecError> {
    execute_as(db, branch, command, None)
}

/// [`execute`], recording `writer` against anything written — which is how
/// the daemon records that a write came from the command line.
pub fn execute_as(
    db: &Db,
    branch: &str,
    command: &Command,
    writer: Option<&str>,
) -> Result<Outcome, ExecError> {
    match command {
        Command::Put {
            key,
            value,
            importance,
            embedding,
            ttl_commits,
            meta,
        } => {
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

        Command::Ls { prefix, limit } => {
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

        Command::Discard { name } => {
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

        Command::At { seq, key, prefix } => {
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
        Command::Fork { name, .. } | Command::Discard { name } => (None, Some(name.clone())),
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
        "discarded": db.discarded().iter().map(|d| json!({
            "name": d.name,
            "forked_at": d.forked_at.to_hex(),
            "commits": d.commits,
        })).collect::<Vec<_>>(),
    })
}
