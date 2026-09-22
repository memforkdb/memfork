//! Self-maintaining memory, using the agents' own intelligence
//! (DESIGN §6.13).
//!
//! MemFork does not think. When a project's memory needs tidying, it says so
//! as a task on the board, with exactly what to do, and a connected agent
//! claims it like any other task, does the work on a fork, and marks it done
//! naming the fork. MemFork then checks the result by rules alone and merges
//! it, or discards it with a lesson saying which rule it broke.
//!
//! **Triggers**, checked when an agent of the project resumes or hands off,
//! so only ever while one is connected:
//!
//! * `size`: the project's memory is over [`MAX_PROJECT_BYTES`];
//! * `handoffs`: more than [`MAX_SUPERSEDED_HANDOFFS`] handoffs are
//!   superseded by later ones;
//! * `stale_facts`: more than [`MAX_STALE_FACTS`] facts were last found
//!   stale;
//! * `flags`: more than [`MAX_OPEN_FLAGS`] duplicates and contradictions are
//!   flagged.
//!
//! Each adds one task, and adds it again only after that task is done and
//! the trigger has cleared. A project can have maintenance switched off.
//!
//! **The check** before a merge: the fork came from the task's branch;
//! nothing pinned was removed (importance 1, the latest handoff, open or
//! claimed tasks); nothing written in the last [`RECENT_COMMITS`] commits
//! was removed, unless the task names it; every removed entry is named by a
//! replacement's `replaces` metadata or by the task; no key left the
//! project's families; and the project got smaller — or, for the triggers
//! whose job is to correct rather than to shrink, no bigger.

use std::collections::{BTreeMap, BTreeSet};

use memfork_core::{Db, Entry, MergePolicy, Value, WRITTEN_BY};
use serde_json::{json, Map, Value as Json};

use crate::namespace;
use crate::shared::Shared;

/// Bytes of keys and values in one project past which it is worth tidying.
pub const MAX_PROJECT_BYTES: u64 = 256 * 1024;

/// Superseded handoffs past which they are worth summarising.
pub const MAX_SUPERSEDED_HANDOFFS: usize = 20;

/// Stale facts past which they are worth rechecking.
pub const MAX_STALE_FACTS: usize = 10;

/// Open flags past which they are worth resolving.
pub const MAX_OPEN_FLAGS: usize = 5;

/// How recent a write is too recent to remove without being named.
pub const RECENT_COMMITS: u64 = 50;

/// Most keys a maintenance task names.
const MAX_TARGETS: usize = 50;

/// Who maintenance tasks are written by.
pub const WRITER: &str = "memfork";

/// The families a project's keys may be in, beside any it already has.
const FAMILIES: &[&str] = &["decision", "fact", "handoff", "lesson", "note", "task"];

/// A trigger that holds now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Firing {
    /// Which trigger.
    pub trigger: &'static str,
    /// What to do, in words an agent can act on.
    pub detail: String,
    /// The keys the task is about, which it may remove.
    pub targets: Vec<String>,
}

fn bytes_of(entries: &[(String, std::sync::Arc<Entry>)]) -> u64 {
    entries
        .iter()
        .map(|(k, e)| (k.len() + e.value.len()) as u64)
        .sum()
}

const HOW: &str = "Fork memory first and do the work on the fork. Give an entry that replaces \
                   others a `replaces` meta listing their keys (a JSON list or comma-separated). \
                   Leave the latest handoff, open tasks and anything with importance 1 alone. \
                   Then mark this task done with `fork` naming your fork: MemFork checks the \
                   result and merges it, or discards it with a lesson saying why.";

/// The triggers that hold for `ns` on `branch` now.
pub fn firing(
    db: &Db,
    branch: &str,
    ns: &str,
    shared: &Shared,
) -> Result<Vec<Firing>, memfork_core::Error> {
    let mut out = Vec::new();
    let entries = db.list(branch, &format!("{ns}{}", namespace::SEPARATOR), None)?;
    let size = bytes_of(&entries);
    if size > MAX_PROJECT_BYTES {
        out.push(Firing {
            trigger: "size",
            detail: format!(
                "This project's memory is {size} bytes, over {MAX_PROJECT_BYTES}. Merge entries \
                 that say the same thing into one, and delete what is no longer true. {HOW}"
            ),
            targets: Vec::new(),
        });
    }
    let handoff = namespace::prefix(ns, "handoff");
    let mut handoffs: Vec<&String> = entries
        .iter()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with(&handoff))
        .collect();
    handoffs.sort();
    handoffs.pop(); // the latest stays
    if handoffs.len() > MAX_SUPERSEDED_HANDOFFS {
        let targets: Vec<String> = handoffs
            .iter()
            .take(MAX_TARGETS)
            .map(|k| (*k).clone())
            .collect();
        out.push(Firing {
            trigger: "handoffs",
            detail: format!(
                "{} handoffs are superseded by later ones. Summarise what they still say that \
                 matters into one `{ns}:note:handoff-history` entry that replaces them, and \
                 delete them. {HOW}",
                handoffs.len()
            ),
            targets,
        });
    }
    let stale: Vec<String> = shared
        .sidecar
        .stale_facts(ns)
        .into_iter()
        .filter(|k| entries.iter().any(|(e, _)| e == k))
        .collect();
    if stale.len() > MAX_STALE_FACTS {
        out.push(Firing {
            trigger: "stale_facts",
            detail: format!(
                "{} facts were last found stale: their files changed. Check each against its \
                 files and put it again with its sources if it still holds, corrected if it \
                 does not, or delete it. {HOW}",
                stale.len()
            ),
            targets: stale.into_iter().take(MAX_TARGETS).collect(),
        });
    }
    let flags = crate::flags::scan(db, branch, ns)?;
    if flags.len() > MAX_OPEN_FLAGS {
        let mut targets = BTreeSet::new();
        for f in &flags {
            if let Some(k) = f["key"].as_str() {
                targets.insert(k.to_owned());
            }
            for k in f["keys"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Json::as_str)
            {
                targets.insert(k.to_owned());
            }
        }
        out.push(Firing {
            trigger: "flags",
            detail: format!(
                "{} duplicates and contradictions are flagged (`memfork flags` lists them). For \
                 each: keep one of two duplicates; for decisions made differently, keep one and \
                 record why; for facts that disagree, keep the one the files bear out. {HOW}",
                flags.len()
            ),
            targets: targets.into_iter().take(MAX_TARGETS).collect(),
        });
    }
    Ok(out)
}

/// Add a task for each trigger that holds and has no outstanding task, and
/// let a trigger fire again once its task is done and it has cleared.
/// Returns the tasks added.
pub fn tend(
    db: &Db,
    branch: &str,
    ns: &str,
    shared: &Shared,
) -> Result<Vec<Json>, memfork_core::Error> {
    if !shared.sidecar.maintenance_on(ns)
        || !crate::policy::allows(crate::policy::Feature::MaintenanceTasks)
    {
        return Ok(Vec::new());
    }
    let holding = firing(db, branch, ns, shared)?;
    // A trigger whose task is done and that no longer holds may fire again.
    for (trigger, key) in shared.sidecar.fired_all(ns) {
        let done = db
            .get(branch, &key)?
            .and_then(|e| serde_json::from_slice::<Json>(&e.value).ok())
            .is_none_or(|t| t["status"] == "done");
        if done && !holding.iter().any(|f| f.trigger == trigger) {
            shared.sidecar.rearm(ns, &trigger);
        }
    }
    let mut added = Vec::new();
    for fire in holding {
        if shared.sidecar.fired(ns, fire.trigger).is_some() {
            continue;
        }
        let key = shared.sidecar.fire(ns, fire.trigger, |n| {
            crate::board::task_key(ns, &format!("maintain-{}-{n}", fire.trigger))
        });
        let mut task = Map::new();
        task.insert("title".to_owned(), json!(title(fire.trigger)));
        task.insert("detail".to_owned(), json!(fire.detail));
        task.insert("status".to_owned(), json!("open"));
        task.insert("holder".to_owned(), Json::Null);
        task.insert("claims".to_owned(), json!(0));
        task.insert("maintenance".to_owned(), json!(fire.trigger));
        if !fire.targets.is_empty() {
            task.insert("targets".to_owned(), json!(fire.targets));
        }
        let value =
            Value::new(Json::Object(task.clone()).to_string()).with_meta(WRITTEN_BY, WRITER);
        let commit = db.put(branch, &key, value)?;
        added.push(json!({
            "key": key,
            "trigger": fire.trigger,
            "task": Json::Object(task),
            "commit": commit.to_hex(),
        }));
    }
    Ok(added)
}

fn title(trigger: &str) -> &'static str {
    match trigger {
        "size" => "Tidy this project's memory: it has grown large",
        "handoffs" => "Summarise the superseded handoffs",
        "stale_facts" => "Recheck the facts that went stale",
        _ => "Resolve the flagged duplicates and contradictions",
    }
}

/// The keys an entry's `replaces` metadata names.
fn replaces(entry: &Entry) -> Vec<String> {
    let Some(raw) = entry.meta.get("replaces") else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_else(|_| {
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    })
}

/// Check the work done on `fork` for the maintenance task `task` on
/// `parent`. `Ok` means it is safe to merge; `Err` says which rule it broke.
pub fn check(
    db: &Db,
    parent: &str,
    fork: &str,
    ns: &str,
    key: &str,
    task: &Json,
) -> Result<(), String> {
    if fork == parent || !db.has_branch(fork) {
        return Err(format!(
            "`{fork}` is not a fork to check; do the work on a fork of `{parent}`"
        ));
    }
    if crate::lessons::parent_of(db, fork) != parent {
        return Err(format!(
            "`{fork}` was not forked from `{parent}`, where the task is"
        ));
    }
    let prefix = format!("{ns}{}", namespace::SEPARATOR);
    let list = |b: &str| db.list(b, &prefix, None).map_err(|e| e.to_string());
    let before: BTreeMap<String, std::sync::Arc<Entry>> = list(parent)?.into_iter().collect();
    let after: BTreeMap<String, std::sync::Arc<Entry>> = list(fork)?.into_iter().collect();
    let targets: BTreeSet<String> = task["targets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Json::as_str)
        .map(str::to_owned)
        .collect();
    let named: BTreeSet<String> = after
        .iter()
        .filter(|(k, e)| {
            before
                .get(*k)
                .is_none_or(|old| old.value != e.value || old.meta != e.meta)
        })
        .flat_map(|(_, e)| replaces(e))
        .collect();
    let head_seq = db
        .head(parent)
        .and_then(|h| db.commit(h))
        .map(|c| c.seq)
        .map_err(|e| e.to_string())?;
    let handoff = namespace::prefix(ns, "handoff");
    let latest_handoff = before
        .keys()
        .filter(|k| k.starts_with(&handoff))
        .max()
        .cloned();
    let task_prefix = namespace::prefix(ns, "task");
    for (k, e) in &before {
        if after.contains_key(k) {
            continue;
        }
        let pinned = e.importance >= 1.0
            || Some(k) == latest_handoff.as_ref()
            || k == key
            || (k.starts_with(&task_prefix)
                && serde_json::from_slice::<Json>(&e.value)
                    .map_or(true, |t| t["status"] != "done"));
        if pinned {
            return Err(format!("`{k}` was removed, and it is pinned: the latest handoff, an unfinished task, or importance 1"));
        }
        let targeted = targets.contains(k);
        if !targeted && e.last_access_seq + RECENT_COMMITS > head_seq {
            return Err(format!(
                "`{k}` was removed, and it was written in the last {RECENT_COMMITS} commits"
            ));
        }
        if !targeted && !named.contains(k) {
            return Err(format!(
                "`{k}` was removed with nothing replacing it: name it in a replacement's \
                 `replaces` meta, or keep it"
            ));
        }
    }
    let families: BTreeSet<String> = before
        .keys()
        .filter_map(|k| k.strip_prefix(&prefix))
        .filter_map(|rest| rest.split(namespace::SEPARATOR).next())
        .map(str::to_owned)
        .chain(FAMILIES.iter().map(|f| (*f).to_owned()))
        .collect();
    for k in after.keys().filter(|k| !before.contains_key(*k)) {
        let family = k
            .strip_prefix(&prefix)
            .and_then(|rest| rest.split(namespace::SEPARATOR).next())
            .unwrap_or("");
        if !families.contains(family) || !k[prefix.len()..].contains(namespace::SEPARATOR) {
            return Err(format!(
                "`{k}` is not in one of the project's families of keys"
            ));
        }
    }
    let size = |m: &BTreeMap<String, std::sync::Arc<Entry>>| -> u64 {
        m.iter()
            .map(|(k, e)| (k.len() + e.value.len()) as u64)
            .sum()
    };
    let (was, now) = (size(&before), size(&after));
    let shrink = matches!(task["maintenance"].as_str(), Some("size" | "handoffs"));
    if shrink && now >= was {
        return Err(format!(
            "the project did not get smaller: {was} bytes before, {now} after"
        ));
    }
    if !shrink && now > was {
        return Err(format!(
            "the project got bigger: {was} bytes before, {now} after"
        ));
    }
    Ok(())
}

/// Merge a fork that passed, then remove it.
pub fn merge(db: &Db, parent: &str, fork: &str) -> Result<(), String> {
    db.merge(fork, parent, MergePolicy::Fail).map_err(|e| {
        format!("it passed the checks, but merging it failed ({e}); `{parent}` changed under it")
    })?;
    db.discard(fork).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(db: &Db, branch: &str, key: &str, value: &str) {
        db.put(branch, key, Value::new(value.to_owned()))
            .expect("put");
    }

    fn task(trigger: &str, targets: &[&str]) -> Json {
        json!({"maintenance": trigger, "targets": targets, "status": "claimed"})
    }

    /// A project old enough that its early writes are not recent.
    fn project() -> Db {
        let db = Db::new();
        put(&db, "main", "p:note:a", &"a".repeat(200));
        put(&db, "main", "p:note:b", &"b".repeat(200));
        for i in 0..RECENT_COMMITS {
            put(&db, "main", "p:note:counter", &i.to_string());
        }
        db
    }

    #[test]
    fn a_tidy_that_replaces_what_it_removes_and_shrinks_passes() {
        let db = project();
        db.fork("main", "tidy").expect("fork");
        db.delete("tidy", "p:note:a").expect("delete");
        db.delete("tidy", "p:note:b").expect("delete");
        db.put(
            "tidy",
            "p:note:ab",
            Value::new("a and b".to_owned()).with_meta("replaces", "[\"p:note:a\",\"p:note:b\"]"),
        )
        .expect("put");
        assert_eq!(
            check(&db, "main", "tidy", "p", "p:task:m", &task("size", &[])),
            Ok(())
        );
        merge(&db, "main", "tidy").expect("merged");
        assert!(db.get("main", "p:note:ab").expect("get").is_some());
        assert!(!db.has_branch("tidy"));
    }

    #[test]
    fn each_rule_refuses_what_it_should() {
        let db = project();
        put(&db, "main", "p:handoff:00000001", "latest");
        let refused = |setup: &dyn Fn(&Db), trigger: &str, targets: &[&str], why: &str| {
            let _ = db.discard("tidy");
            db.fork("main", "tidy").expect("fork");
            setup(&db);
            let err = check(
                &db,
                "main",
                "tidy",
                "p",
                "p:task:m",
                &task(trigger, targets),
            )
            .expect_err(why);
            assert!(err.contains(why), "{err}");
        };
        refused(
            &|db| {
                db.delete("tidy", "p:note:a").expect("d");
            },
            "size",
            &[],
            "nothing replacing it",
        );
        refused(
            &|db| {
                db.delete("tidy", "p:handoff:00000001").expect("d");
            },
            "size",
            &[],
            "pinned",
        );
        refused(
            &|db| {
                db.delete("tidy", "p:note:counter").expect("d");
            },
            "size",
            &[],
            "last 50 commits",
        );
        refused(
            &|db| {
                put(db, "tidy", "p:note:c", "more");
            },
            "size",
            &[],
            "did not get smaller",
        );
        refused(
            &|db| {
                put(db, "tidy", "p:elsewhere", "x");
                db.delete("tidy", "p:note:a").expect("d");
            },
            "size",
            &["p:note:a"],
            "families",
        );
        refused(
            &|db| {
                put(db, "tidy", "p:note:a", &"a".repeat(300));
            },
            "stale_facts",
            &[],
            "got bigger",
        );
        // A target may go without a replacement, and a correcting task may
        // stay the same size.
        let _ = db.discard("tidy");
        db.fork("main", "tidy").expect("fork");
        db.delete("tidy", "p:note:a").expect("d");
        assert_eq!(
            check(
                &db,
                "main",
                "tidy",
                "p",
                "p:task:m",
                &task("stale_facts", &["p:note:a"])
            ),
            Ok(())
        );
    }

    #[test]
    fn a_trigger_adds_one_task_until_it_is_done_and_clears() {
        let db = Db::new();
        let shared = Shared::default();
        for i in 0..=MAX_SUPERSEDED_HANDOFFS + 1 {
            put(&db, "main", &format!("p:handoff:{:08}", i + 1), "note");
        }
        let added = tend(&db, "main", "p", &shared).expect("tend");
        assert_eq!(added.len(), 1);
        assert_eq!(added[0]["trigger"], "handoffs");
        let key = added[0]["key"].as_str().expect("key").to_owned();
        assert!(
            tend(&db, "main", "p", &shared).expect("again").is_empty(),
            "added twice"
        );
        // Done, but the handoffs are still there: it does not fire again yet.
        let mut t: Json =
            serde_json::from_slice(&db.get("main", &key).expect("g").expect("t").value)
                .expect("json");
        t["status"] = json!("done");
        put(&db, "main", &key, &t.to_string());
        assert!(tend(&db, "main", "p", &shared).expect("held").is_empty());
        shared.sidecar.set_maintenance("p", false);
        assert!(!shared.sidecar.maintenance_on("p"));
    }
}
