//! Executing a parsed command against an engine, and rendering the result.

use memfork_core::{Db, MergePolicy, Value};
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
                v = v.with_meta(k, val);
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
            let text = entries
                .iter()
                .map(|(k, e)| format!("{k}\t{}", String::from_utf8_lossy(&e.value)))
                .collect();
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
                "branches": branches.iter().map(|b| json!({
                    "name": b.name,
                    "head": b.head.to_hex(),
                    "seq": b.seq,
                    "key_count": b.key_count,
                    "is_default": b.is_default,
                })).collect::<Vec<_>>(),
            });
            Ok(Outcome::new(text, json))
        }

        Command::Log { limit } => {
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
                    let text = entries
                        .iter()
                        .map(|(k, e)| format!("{k}\t{}", String::from_utf8_lossy(&e.value)))
                        .collect();
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
        | Command::Stop { .. }
        | Command::Doctor => Err(ExecError::Usage(format!(
            "`memfork {}` is not an operation on a database",
            command.name()
        ))),
    }
}
