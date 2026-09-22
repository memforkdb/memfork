//! What the page shows beside the graph: the headline, the panels, the
//! footer, one entry in full, a ranked search, and the keys two branches
//! disagree on. All of it read from the store and the side structure, none
//! of it changing either.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use hyper::StatusCode;
use memfork_core::{Entry, Op, WRITTEN_BY};
use serde_json::{json, Value as Json};

use super::{namespaces, Context, Failure, Query};
use crate::namespace;

/// Most hits a search returns.
pub const MAX_HITS: usize = 50;

/// Most commits walked for one entry's history.
pub const MAX_HISTORY_COMMITS: usize = 10_000;

/// Most lines of history returned for one entry.
pub const MAX_HISTORY: usize = 50;

/// Most keys a branch comparison lists.
pub const MAX_DIFF: usize = 500;

/// Longest value shown in a panel row.
const ROW_CHARS: usize = 300;

/// Longest value shown in the side sheet.
const SHEET_CHARS: usize = 20_000;

fn clip(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

fn text_of(entry: &Entry) -> String {
    String::from_utf8_lossy(&entry.value).into_owned()
}

fn who(entry: &Entry) -> Option<String> {
    entry
        .meta
        .get(WRITTEN_BY)
        .map(|w| crate::clients::display_for_writer(w))
}

fn display(name: &str) -> String {
    crate::clients::display_for_writer(name)
}

/// The branch a query names, or the default one; refused if it does not
/// exist.
pub fn branch_of(context: &Context, query: &Query) -> Result<String, Failure> {
    let branch = query
        .get("branch")
        .unwrap_or_else(|| context.db.default_branch())
        .to_owned();
    if !context.db.has_branch(&branch) {
        return Err((
            StatusCode::NOT_FOUND,
            format!("there is no branch `{branch}`"),
        ));
    }
    Ok(branch)
}

/// The project a query names, or the one to show first: what a connected
/// client is working in, else the first project on the branch, else the
/// fallback.
pub fn namespace_of(context: &Context, query: &Query, branch: &str) -> Result<String, Failure> {
    if let Some(ns) = query.get("ns") {
        namespace::validate(ns).map_err(|why| (StatusCode::BAD_REQUEST, format!("`ns`: {why}")))?;
        return Ok(ns.to_owned());
    }
    if let Some(connected) = context.events.connected().first() {
        return Ok(connected.namespace.clone());
    }
    Ok(namespaces(&context.db, branch)?
        .into_iter()
        .next()
        .unwrap_or_else(|| namespace::FALLBACK.to_owned()))
}

/// The headline, the panels and the footer for a project on a branch.
pub fn summary(context: &Context, query: &Query) -> Result<Json, Failure> {
    let branch = branch_of(context, query)?;
    let ns = namespace_of(context, query, &branch)?;
    let db = &context.db;
    let side = &context.side;
    let view = db
        .read(&branch)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let prefix = format!("{ns}{}", namespace::SEPARATOR);
    let entries = view.list(&prefix, None);
    let connected = context.events.connected();

    // The headline: real counts, from the side file.
    let stats = side.sidecar.stats(Some(&ns));
    let total = &stats["total"];
    let count = |name: &str| total[name].as_u64().unwrap_or(0);
    let counts = json!({
        "briefings": count("briefings"),
        "handoffs_picked_up": count("handoffs_picked_up"),
        "dead_ends_not_repeated": count("lessons_served"),
        "stale_facts_caught": count("facts_stale"),
        "claim_conflicts_avoided": count("claim_conflicts"),
        "briefing_bytes": count("briefing_bytes"),
        "memory_bytes": count("memory_bytes"),
    });
    let twice = count("lessons_served")
        + count("facts_stale")
        + count("claim_conflicts")
        + count("handoffs_picked_up");

    let briefings = side.sidecar.briefings(&ns);
    let served_count = |key: &str| {
        briefings
            .iter()
            .filter(|b| b.keys.iter().any(|k| k == key))
            .count()
    };

    // Handoffs: who left each, who picked it up, and what they took next.
    let handoff_prefix = namespace::prefix(&ns, "handoff");
    let task_prefix = namespace::prefix(&ns, "task");
    let mut handoffs: Vec<Json> = entries
        .iter()
        .filter(|(k, _)| k.starts_with(&handoff_prefix))
        .map(|(key, entry)| {
            let parsed: Json = serde_json::from_slice(&entry.value).unwrap_or(Json::Null);
            let by = who(entry);
            let pickup = briefings
                .iter()
                .filter(|b| b.handoff.as_deref() == Some(key.as_str()))
                .find(|b| Some(display(&b.to)) != by);
            let picked = pickup.map(|b| {
                let to = display(&b.to);
                // What that client took on after the briefing: a task it holds
                // now, or one it finished since.
                let next: Vec<String> = entries
                    .iter()
                    .filter(|(k, _)| k.starts_with(&task_prefix))
                    .filter(|(k, e)| {
                        let held = side.board.holder(k).is_some_and(|(c, _)| display(&c) == to);
                        let task: Json = serde_json::from_slice(&e.value).unwrap_or(Json::Null);
                        let done_by = task["done_by"].as_str().map(display);
                        held || (done_by.as_deref() == Some(to.as_str())
                            && e.last_access_seq > b.seq)
                    })
                    .map(|(k, e)| {
                        let task: Json = serde_json::from_slice(&e.value).unwrap_or(Json::Null);
                        task["title"]
                            .as_str()
                            .map_or_else(|| k[task_prefix.len()..].to_owned(), str::to_owned)
                    })
                    .collect();
                json!({
                    "to": to,
                    "bytes": b.bytes,
                    "approx_tokens": b.bytes.div_ceil(4),
                    "seq": b.seq,
                    "next": next,
                })
            });
            json!({
                "key": key,
                "number": key[handoff_prefix.len()..].trim_start_matches('0'),
                "by": by,
                "seq": entry.created_seq,
                "summary": clip(parsed["summary"].as_str().unwrap_or(&text_of(entry)), ROW_CHARS),
                "next": parsed["next"],
                "blockers": parsed["blockers"],
                "questions": parsed["questions"],
                "picked_up": picked,
            })
        })
        .collect();
    handoffs.reverse();

    // Coordination: the board as it stands.
    let tasks = side
        .board
        .list(db, &branch, &ns, "all")
        .map(|l| l["tasks"].clone())
        .unwrap_or_else(|_| json!([]));

    // Freshness: every fact, with what the last check found.
    let stale: BTreeSet<String> = side.sidecar.stale_facts(&ns).into_iter().collect();
    let fresh: BTreeSet<String> = side.sidecar.fresh_facts(&ns).into_iter().collect();
    let fact_state = |key: &str| {
        if stale.contains(key) {
            "stale"
        } else if fresh.contains(key) {
            "fresh"
        } else {
            "unverified"
        }
    };
    let facts: Vec<Json> = entries
        .iter()
        .filter_map(|(key, entry)| {
            let sources = crate::facts::sources_of(&entry.meta)?;
            Some(json!({
                "key": key,
                "value": clip(&text_of(entry), ROW_CHARS),
                "sources": sources,
                "state": fact_state(key),
                "by": who(entry),
                "seq": entry.created_seq,
            }))
        })
        .collect();

    // Dead ends: lessons, where they came from, how often they were served.
    let lesson_prefix = namespace::prefix(&ns, "lesson");
    let mut lessons: Vec<Json> = entries
        .iter()
        .filter(|(k, _)| k.starts_with(&lesson_prefix))
        .map(|(key, entry)| {
            let mut view = crate::lessons::view(key, entry);
            view["by"] = json!(who(entry));
            view["served"] = json!(served_count(key));
            view["seq"] = json!(entry.created_seq);
            view
        })
        .collect();
    lessons.reverse();

    // Briefings served, newest first.
    let mut served: Vec<Json> = briefings
        .iter()
        .filter(|b| b.branch == branch)
        .map(|b| {
            let mut families: BTreeMap<&str, usize> = BTreeMap::new();
            for key in &b.keys {
                let family = key
                    .strip_prefix(&prefix)
                    .and_then(|rest| rest.split_once(':'))
                    .map_or("entry", |(f, _)| f);
                *families.entry(family).or_insert(0) += 1;
            }
            json!({
                "order": b.order,
                "to": display(&b.to),
                "bytes": b.bytes,
                "approx_tokens": b.bytes.div_ceil(4),
                "seq": b.seq,
                "carried": families,
                "keys": b.keys,
                "omitted": b.omitted,
                "since_commits": b.since_commits,
                "task": b.task,
            })
        })
        .collect();
    served.reverse();

    // The footer.
    let store_bytes: usize = view
        .list("", None)
        .iter()
        .map(|(k, e)| k.len() + e.value.len())
        .sum();
    let log = db.log(&branch, None).unwrap_or_default();
    let horizon = log.last().map_or(0, |c| c.seq);
    let policy = match crate::policy::current() {
        Ok(p) => p.summary(),
        Err(e) => format!("unreadable: {}", e.why),
    };
    let branches: Vec<String> = db.branches().into_iter().map(|b| b.name).collect();

    // Autopilot: what it did, and what it left for a person.
    let autopilot = crate::autopilot::engine::Context {
        db: db.clone(),
        shared: Arc::clone(side),
        events: Some(Arc::clone(&context.events)),
    }
    .status_of(&ns);

    Ok(json!({
        "version": crate::VERSION,
        "port": context.port,
        "read_only": true,
        "namespace": ns,
        "branch": branch,
        "seq": view.seq(),
        "entries": entries.len(),
        "branches": branches,
        "namespaces": namespaces(db, &branch)?,
        "clients": connected,
        "policy": policy,
        "headline": { "twice": twice, "counts": counts },
        "handoffs": handoffs,
        "tasks": tasks,
        "facts": facts,
        "lessons": lessons,
        "briefings": served,
        "autopilot": autopilot,
        "footer": {
            "store_bytes": store_bytes,
            "commits_retained": db.commit_count(),
            "history_from_seq": horizon,
            "branch_entries": view.len(),
        },
    }))
}

/// What needs a look: contradictions, near-duplicates, stale facts,
/// maintenance suggestions and claims whose holder is gone. Flagged, never
/// fixed. Its own route, fetched after the first paint: scanning a large
/// project for duplicates is the one slow thing here, and the headline and
/// the panels need not wait for it.
pub fn attention(context: &Context, query: &Query) -> Result<Json, Failure> {
    let branch = branch_of(context, query)?;
    let ns = namespace_of(context, query, &branch)?;
    let db = &context.db;
    let side = &context.side;
    let view = db
        .read(&branch)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let prefix = format!("{ns}{}", namespace::SEPARATOR);
    let entries = view.list(&prefix, None);
    let task_prefix = namespace::prefix(&ns, "task");
    let here: BTreeSet<String> = context
        .events
        .connected()
        .iter()
        .map(|c| display(&c.client))
        .collect();
    let stale: BTreeSet<String> = side.sidecar.stale_facts(&ns).into_iter().collect();
    // Attention: flagged, never fixed.
    let mut attention: Vec<Json> = Vec::new();
    for flag in crate::flags::scan(db, &branch, &ns).unwrap_or_default() {
        let (title, detail) = match flag["kind"].as_str().unwrap_or("") {
            "conflicting_decisions" => (
                format!(
                    "Two decisions on {}",
                    short(&ns, flag["key"].as_str().unwrap_or(""))
                ),
                flag["branches"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|b| {
                        format!(
                            "{} on {}",
                            b["by"]
                                .as_str()
                                .map_or_else(|| "someone".to_owned(), display),
                            b["branch"].as_str().unwrap_or("?")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            "duplicate" => (
                "Near-duplicate keys".to_owned(),
                flag["keys"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Json::as_str)
                    .collect::<Vec<_>>()
                    .join(" and "),
            ),
            "facts_disagree" => (
                "Facts from the same files disagree".to_owned(),
                flag["keys"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Json::as_str)
                    .collect::<Vec<_>>()
                    .join(" and "),
            ),
            other => (
                other.to_owned(),
                flag["look"].as_str().unwrap_or("").to_owned(),
            ),
        };
        attention.push(json!({
            "kind": flag["kind"],
            "title": title,
            "detail": detail,
            "keys": flag["keys"].as_array().cloned().or_else(|| flag["key"].as_str().map(|k| vec![json!(k)])),
        }));
    }
    for key in &stale {
        if entries.iter().any(|(k, _)| k == key) {
            attention.push(json!({
                "kind": "stale_fact",
                "title": "Fact went stale",
                "detail": format!("{} · re-check before trusting", short(&ns, key)),
                "keys": [key],
            }));
        }
    }
    for (trigger, key) in side.sidecar.fired_all(&ns) {
        attention.push(json!({
            "kind": "maintenance",
            "title": "Memory could use tidying",
            "detail": format!("{} · task {} is on the board", trigger.replace('_', " "), short(&ns, &key)),
            "keys": [key],
        }));
    }
    for (key, _) in entries.iter().filter(|(k, _)| k.starts_with(&task_prefix)) {
        if let Some((client, left)) = side.board.holder(key) {
            let holder = display(&client);
            if !here.contains(&holder) {
                attention.push(json!({
                    "kind": "stuck_claim",
                    "title": "A claim whose holder is not connected",
                    "detail": format!("{} held by {holder} · lease ends in {left} s", short(&ns, key)),
                    "keys": [key],
                }));
            }
        }
    }

    // Autopilot: a fork it kept for a person, and memory whose git branch is
    // gone. Listed with the way out; never done by MemFork.
    let autopilot = crate::autopilot::engine::Context {
        db: db.clone(),
        shared: Arc::clone(side),
        events: Some(Arc::clone(&context.events)),
    }
    .status_of(&ns);
    for fork in autopilot["kept_forks"].as_array().into_iter().flatten() {
        let name = fork.as_str().unwrap_or("?");
        attention.push(json!({
            "kind": "autopilot_fork_kept",
            "title": "A fork autopilot kept",
            "detail": format!("{name} · no check judged it: merge it, or discard it with a lesson"),
            "keys": [],
        }));
    }
    for orphan in autopilot["orphans"].as_array().into_iter().flatten() {
        let name = orphan["branch"].as_str().unwrap_or("?");
        attention.push(json!({
            "kind": "orphan_branch",
            "title": "Memory branch with no git branch",
            "detail": format!(
                "{name} · {} · {}, then {}",
                orphan["why"].as_str().unwrap_or(""),
                orphan["merge_then_discard"][0].as_str().unwrap_or(""),
                orphan["merge_then_discard"][1].as_str().unwrap_or("")
            ),
            "keys": [],
        }));
    }

    Ok(json!({
        "namespace": ns,
        "branch": branch,
        "attention": attention,
    }))
}

/// A key without its project, for a sentence.
fn short(ns: &str, key: &str) -> String {
    key.strip_prefix(&format!("{ns}{}", namespace::SEPARATOR))
        .unwrap_or(key)
        .to_owned()
}

/// One entry in full: its value, its metadata, its sources and what the last
/// check found for each, and its history across commits.
pub fn entry(context: &Context, query: &Query) -> Result<Json, Failure> {
    let branch = branch_of(context, query)?;
    let key = query
        .get("key")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "`key` is needed".to_owned()))?;
    let at = query.number("at")?;
    let db = &context.db;
    let view = match at {
        Some(seq) => db.at(&branch, seq),
        None => db.read(&branch),
    }
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let Some(entry) = view.get(key) else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("there is no `{key}` on {branch}"),
        ));
    };
    let ns = key.split_once(':').map_or("", |(ns, _)| ns);
    let stale: BTreeSet<String> = context.side.sidecar.stale_facts(ns).into_iter().collect();
    let fresh: BTreeSet<String> = context.side.sidecar.fresh_facts(ns).into_iter().collect();
    let state = if stale.contains(key) {
        "stale"
    } else if fresh.contains(key) {
        "fresh"
    } else {
        "unverified"
    };
    let sources = crate::facts::sources_of(&entry.meta).map(|paths| {
        paths
            .into_iter()
            .map(|p| json!({ "path": p, "state": state }))
            .collect::<Vec<_>>()
    });

    // History: every commit on the branch that touched this key, newest
    // first, within the retention window.
    let mut history = Vec::new();
    let mut cursor = db.head(&branch).ok();
    let mut walked = 0;
    while let Some(id) = cursor {
        if walked >= MAX_HISTORY_COMMITS || history.len() >= MAX_HISTORY {
            break;
        }
        let Ok(commit) = db.commit(id) else { break };
        walked += 1;
        for op in &commit.ops {
            if op.key() != key {
                continue;
            }
            let (what, by) = match op {
                Op::Put { value, .. } => {
                    ("written", value.meta.get(WRITTEN_BY).map(|w| display(w)))
                }
                Op::Delete { .. } => ("deleted", None),
                Op::Evict { .. } => ("evicted to stay in budget", None),
            };
            history.push(json!({
                "seq": commit.seq,
                "commit": commit.id.to_hex(),
                "what": what,
                "by": by,
                "message": commit.message,
            }));
        }
        cursor = commit.first_parent();
    }

    let meta: BTreeMap<&String, &String> = entry
        .meta
        .iter()
        .filter(|(k, _)| k.as_str() != crate::facts::SOURCES_META)
        .collect();
    Ok(json!({
        "key": key,
        "branch": branch,
        "at": at,
        "value": clip(&text_of(&entry), SHEET_CHARS),
        "bytes": entry.value.len(),
        "by": who(&entry),
        "importance": entry.importance,
        "created_seq": entry.created_seq,
        "last_seq": entry.last_access_seq,
        "ttl_commits": entry.ttl_commits,
        "meta": meta,
        "sources": sources,
        "history": history,
        "history_truncated": history.len() >= MAX_HISTORY,
    }))
}

/// The engine's ranked text search over a project on a branch.
pub fn search(context: &Context, query: &Query) -> Result<Json, Failure> {
    let branch = branch_of(context, query)?;
    let ns = namespace_of(context, query, &branch)?;
    let q = query.get("q").unwrap_or_default();
    let view = context
        .db
        .read(&branch)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let entries = view.list(&format!("{ns}{}", namespace::SEPARATOR), None);
    let texts: Vec<(&str, String)> = entries
        .iter()
        .map(|(k, e)| (k.as_str(), text_of(e)))
        .collect();
    let docs: Vec<crate::find::Doc<'_>> = texts
        .iter()
        .map(|(k, t)| crate::find::Doc { key: k, text: t })
        .collect();
    let hits: Vec<Json> = if crate::find::words(q).is_empty() {
        Vec::new()
    } else {
        crate::find::rank(&docs, q, MAX_HITS)
            .into_iter()
            .map(|h| {
                let text = texts
                    .iter()
                    .find(|(k, _)| *k == h.key)
                    .map_or("", |(_, t)| t.as_str());
                json!({
                    "key": h.key,
                    "score": h.score,
                    "snippet": crate::find::snippet(text, h.first_match),
                })
            })
            .collect()
    };
    Ok(json!({
        "namespace": ns,
        "branch": branch,
        "query": q,
        "hits": hits,
        "limit": MAX_HITS,
    }))
}

/// The keys that differ between two branches: what a merge would face.
pub fn diff(context: &Context, query: &Query) -> Result<Json, Failure> {
    let a = query
        .get("a")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "`a` is needed".to_owned()))?;
    let b = query
        .get("b")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "`b` is needed".to_owned()))?;
    for name in [a, b] {
        if !context.db.has_branch(name) {
            return Err((
                StatusCode::NOT_FOUND,
                format!("there is no branch `{name}`"),
            ));
        }
    }
    let ns = query
        .get("ns")
        .map(|ns| format!("{ns}{}", namespace::SEPARATOR));
    let changes = context
        .db
        .diff(a, b)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let va = context
        .db
        .read(a)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let vb = context
        .db
        .read(b)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let wanted: Vec<_> = changes
        .iter()
        .filter(|c| ns.as_ref().is_none_or(|p| c.key.starts_with(p)))
        .collect();
    let total = wanted.len();
    let listed: Vec<Json> = wanted
        .iter()
        .take(MAX_DIFF)
        .map(|c| {
            let value = |v: Option<std::sync::Arc<Entry>>| v.map(|e| clip(&text_of(&e), ROW_CHARS));
            json!({
                "key": c.key,
                "kind": match c.kind {
                    memfork_core::ChangeKind::Added => "added",
                    memfork_core::ChangeKind::Removed => "removed",
                    memfork_core::ChangeKind::Modified => "modified",
                },
                "a": value(va.get(&c.key)),
                "b": value(vb.get(&c.key)),
            })
        })
        .collect();
    Ok(json!({
        "a": a,
        "b": b,
        "namespace": query.get("ns"),
        "count": total,
        "changes": listed,
        "omitted": total.saturating_sub(MAX_DIFF),
    }))
}
