//! Lessons from discarded forks (DESIGN §6.9).
//!
//! Discarding an attempt leaves no trace of it, which is the point — and also
//! means the next agent may try the same dead end. So a discard may carry one
//! line saying what was learned. The line is written to the branch the fork
//! came from, under `<project>:lesson:<n>`, naming the discarded branch and
//! the client that wrote it. Everything else on the fork still vanishes.
//!
//! **Which branch the fork came from is worked out, not remembered.** The log
//! records the commit a branch was created at, not the name of the branch it
//! was taken from. The parent is therefore the surviving branch whose history
//! holds that commit and has moved least since it; ties go to the default
//! branch, then to name order. If none holds it, the default branch. The same
//! store always gives the same answer, before a restart and after.
//!
//! **Bounded.** At most [`MAX_LESSONS`] current lessons per project: writing one
//! more deletes the oldest in the same commit. Deleted lessons are still in
//! history, where time travel reaches them.

use std::collections::{BTreeMap, BTreeSet};

use memfork_core::{CommitId, Db, Value, WRITTEN_BY};
use serde_json::{json, Value as Json};

use crate::namespace;

/// Most current lessons a project keeps.
pub const MAX_LESSONS: usize = 200;

/// Longest lesson, in characters.
pub const MAX_LESSON_CHARS: usize = 300;

/// Digits in a lesson number, so key order is number order.
const DIGITS: usize = 8;

static NUMBERING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A lesson made ready to store: one line, bounded.
///
/// ```
/// assert_eq!(
///     memfork::lessons::tidy("  the migration needs\n  the table first ").unwrap(),
///     "the migration needs the table first"
/// );
/// assert!(memfork::lessons::tidy("   ").is_err());
/// // A long one is cut to the limit rather than refused.
/// let long = memfork::lessons::tidy(&"x".repeat(400)).unwrap();
/// assert_eq!(long.chars().count(), memfork::lessons::MAX_LESSON_CHARS);
/// ```
pub fn tidy(raw: &str) -> Result<String, String> {
    let line: String = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_LESSON_CHARS)
        .collect();
    if line.is_empty() {
        return Err("a lesson cannot be empty; leave it out to discard without one".to_owned());
    }
    Ok(line)
}

/// The branch `branch` was forked from, by the rule above.
pub fn parent_of(db: &Db, branch: &str) -> String {
    let branches = db.branches();
    let Some(forked_at) = branches
        .iter()
        .find(|b| b.name == branch)
        .map(|b| b.forked_at)
    else {
        return db.default_branch().to_owned();
    };
    let parents: BTreeMap<CommitId, Vec<CommitId>> = db
        .all_commits()
        .into_iter()
        .map(|c| (c.id, c.parents.to_vec()))
        .collect();
    let ancestors = |from: CommitId| {
        let mut seen = BTreeSet::new();
        let mut stack = vec![from];
        while let Some(id) = stack.pop() {
            if seen.insert(id) {
                stack.extend(parents.get(&id).into_iter().flatten().copied());
            }
        }
        seen
    };
    let base = ancestors(forked_at);
    branches
        .iter()
        .filter(|b| b.name != branch)
        .filter_map(|b| {
            let mine = ancestors(b.head);
            mine.contains(&forked_at).then(|| {
                (
                    mine.difference(&base).count(),
                    !b.is_default,
                    b.name.clone(),
                )
            })
        })
        .min()
        .map(|(_, _, name)| name)
        .unwrap_or_else(|| db.default_branch().to_owned())
}

/// What recording a lesson did.
#[derive(Debug, Clone)]
pub struct Recorded {
    /// The key it was stored under.
    pub key: String,
    /// The branch it was stored on.
    pub branch: String,
    /// The commit that stored it.
    pub commit: CommitId,
}

/// Write `lesson` about `discarded` to its parent branch, before the discard.
pub fn record(
    db: &Db,
    discarded: &str,
    lesson: &str,
    ns: &str,
    by: Option<&str>,
) -> Result<Recorded, memfork_core::Error> {
    let parent = parent_of(db, discarded);
    let commits = db
        .branches()
        .iter()
        .find(|b| b.name == discarded)
        .map(|b| {
            let base = db.commit(b.forked_at).map(|c| c.seq).unwrap_or(0);
            b.seq.saturating_sub(base)
        })
        .unwrap_or(0);
    let body = json!({
        "lesson": lesson,
        "branch": discarded,
        "commits": commits,
    });
    write(
        db,
        &parent,
        body,
        ns,
        by,
        &format!("lesson from {discarded}"),
    )
}

/// Write a lesson about a task whose acceptance command failed, on the
/// branch the task is on.
pub fn record_about_task(
    db: &Db,
    branch: &str,
    lesson: &str,
    task: &str,
    ns: &str,
    by: Option<&str>,
) -> Result<Recorded, memfork_core::Error> {
    let body = json!({ "lesson": lesson, "task": task });
    write(
        db,
        branch,
        body,
        ns,
        by,
        &format!("lesson from task {task}"),
    )
}

/// Store the next numbered lesson on `on`, keeping the newest [`MAX_LESSONS`].
fn write(
    db: &Db,
    on: &str,
    body: Json,
    ns: &str,
    by: Option<&str>,
    message: &str,
) -> Result<Recorded, memfork_core::Error> {
    let prefix = namespace::prefix(ns, "lesson");
    let mut value = Value::new(body.to_string()).with_importance(0.8);
    if let Some(by) = by {
        value = value.with_meta(WRITTEN_BY, by);
    }

    // Numbering is serialised across sessions, as handoffs' is: two lessons
    // written at once must not both take the same number.
    let _numbering = NUMBERING.lock().unwrap_or_else(|e| e.into_inner());
    let mut last = None;
    for _ in 0..16 {
        let existing: Vec<(u64, String)> = db
            .list(on, &prefix, None)?
            .into_iter()
            .filter_map(|(k, _)| Some((k.strip_prefix(&prefix)?.parse::<u64>().ok()?, k)))
            .collect();
        let mut txn = db.begin(on)?;
        let next = existing.iter().map(|(n, _)| *n).max().unwrap_or(0) + 1;
        let key = format!("{prefix}{next:0width$}", width = DIGITS);
        txn.put(&key, value.clone())?;
        // Keep the newest; the oldest leave current memory, not history.
        let mut numbers: Vec<&(u64, String)> = existing.iter().collect();
        numbers.sort();
        let over = (numbers.len() + 1).saturating_sub(MAX_LESSONS);
        for (_, old) in numbers.into_iter().take(over) {
            txn.delete(old)?;
        }
        match txn.commit(Some(message.to_owned())) {
            Ok(commit) => {
                return Ok(Recorded {
                    key,
                    branch: on.to_owned(),
                    commit,
                })
            }
            Err(e @ memfork_core::Error::Conflict { .. }) => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| memfork_core::Error::NoSuchBranch(on.to_owned())))
}

/// The newest `limit` lessons in `ns`, newest first, as a briefing shows them.
pub fn recent(
    db: &Db,
    branch: &str,
    ns: &str,
    limit: usize,
) -> Result<Vec<Json>, memfork_core::Error> {
    let prefix = namespace::prefix(ns, "lesson");
    let mut all = db.list(branch, &prefix, None)?;
    all.reverse();
    Ok(all
        .into_iter()
        .take(limit)
        .map(|(key, entry)| view(&key, &entry))
        .collect())
}

/// A lesson as answers show it.
pub fn view(key: &str, entry: &memfork_core::Entry) -> Json {
    let parsed: Json = serde_json::from_slice(&entry.value).unwrap_or(Json::Null);
    let mut view = json!({
        "key": key,
        "lesson": parsed.get("lesson").cloned()
            .unwrap_or_else(|| json!(String::from_utf8_lossy(&entry.value))),
        "branch": parsed.get("branch").cloned().unwrap_or(Json::Null),
        "by": entry.meta.get(WRITTEN_BY),
    });
    // A lesson from a failed acceptance names its task.
    if let Some(task) = parsed.get("task") {
        view["task"] = task.clone();
    }
    view
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parent_is_the_branch_that_moved_least_since_the_fork() {
        let db = Db::new();
        db.put("main", "a", Value::new("1")).unwrap();
        db.fork("main", "feature").unwrap();
        db.put("feature", "f", Value::new("1")).unwrap();
        db.fork("feature", "attempt").unwrap();
        db.put("attempt", "x", Value::new("1")).unwrap();
        // Only feature's history holds the commit attempt was forked at.
        assert_eq!(parent_of(&db, "attempt"), "feature");

        // A fork straight off main, with main unmoved and a sibling that has
        // moved on from the same point: main wins as the default.
        db.fork("main", "side").unwrap();
        db.fork("main", "try").unwrap();
        assert_eq!(parent_of(&db, "try"), "main");
    }

    #[test]
    fn a_lesson_lands_on_the_parent_and_nothing_else_survives_the_discard() {
        let db = Db::new();
        db.put("main", "shop:decision:db", Value::new("postgres"))
            .unwrap();
        db.fork("main", "attempt").unwrap();
        db.put("attempt", "shop:decision:db", Value::new("sqlite"))
            .unwrap();
        db.put("attempt", "shop:note:x", Value::new("scratch"))
            .unwrap();
        let rec = record(
            &db,
            "attempt",
            "sqlite locks under concurrent writes",
            "shop",
            Some("c"),
        )
        .unwrap();
        db.discard("attempt").unwrap();
        assert_eq!(rec.branch, "main");
        assert_eq!(rec.key, "shop:lesson:00000001");
        let lesson = view(&rec.key, &db.get("main", &rec.key).unwrap().unwrap());
        assert_eq!(lesson["lesson"], "sqlite locks under concurrent writes");
        assert_eq!(lesson["branch"], "attempt");
        assert_eq!(lesson["by"], "c");
        assert_eq!(
            db.get("main", "shop:decision:db")
                .unwrap()
                .unwrap()
                .value
                .as_ref(),
            b"postgres"
        );
        assert!(db.get("main", "shop:note:x").unwrap().is_none());
    }

    #[test]
    fn lessons_are_one_line_and_bounded_in_number() {
        assert_eq!(tidy("  two\nlines  ").unwrap(), "two lines");
        assert!(tidy("   ").is_err());
        assert_eq!(
            tidy(&"x".repeat(1000)).unwrap().chars().count(),
            MAX_LESSON_CHARS
        );

        let db = Db::new();
        for i in 0..(MAX_LESSONS + 3) {
            let name = format!("b{i}");
            db.fork("main", &name).unwrap();
            record(&db, &name, &format!("lesson {i}"), "p", None).unwrap();
            db.discard(&name).unwrap();
        }
        let kept = db.list("main", "p:lesson:", None).unwrap();
        assert_eq!(kept.len(), MAX_LESSONS);
        assert_eq!(kept[0].0, "p:lesson:00000004", "the oldest went first");
        assert_eq!(
            recent(&db, "main", "p", 1).unwrap()[0]["lesson"],
            format!("lesson {}", MAX_LESSONS + 2)
        );
    }
}
