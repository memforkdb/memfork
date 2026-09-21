//! Handing work from one agent to another (DESIGN §6.3).
//!
//! `memfork_handoff` records where things stand when an agent stops;
//! `memfork_resume` gives the next agent — the same client tomorrow, or a
//! different one from a different vendor — one compact briefing to start from.
//! Both work in the session's project namespace.
//!
//! Everything lives in ordinary keys under that namespace, so nothing here is
//! hidden from the raw tools:
//!
//! | key                         | what                                   |
//! |-----------------------------|----------------------------------------|
//! | `<ns>:handoff:<8 digits>`   | one handoff note, numbered from 1      |
//! | `<ns>:decision:<topic>`     | a decision and its reason              |
//! | `<ns>:task:<id>`            | an open task; `"status":"done"` closes it |
//!
//! A briefing is bounded: it is read at the start of every piece of work, so
//! it has to stay cheap however much a project has accumulated. What does not
//! fit is counted, not silently lost, and the caller is told where to look.

use std::sync::Mutex;

use memfork_core::{Db, Entry, Value, WRITTEN_BY};
use serde_json::{json, Map, Value as Json};

use crate::namespace;

/// Most bytes of JSON a briefing may take.
pub const MAX_BRIEFING_BYTES: usize = 6 * 1024;

/// Longest single piece of text in a briefing, in characters.
pub const MAX_TEXT_CHARS: usize = 300;

/// How many recent decisions a briefing includes at most.
pub const MAX_DECISIONS: usize = 10;

/// How many open tasks a briefing includes at most.
pub const MAX_TASKS: usize = 20;

/// How many items of each list in a handoff a briefing includes at most.
pub const MAX_LIST_ITEMS: usize = 10;

/// Handoffs keep the eviction policy's attention: the latest one is the most
/// useful thing a project has.
const HANDOFF_IMPORTANCE: f32 = 0.9;

/// Digits in a handoff number, so key order is number order.
const HANDOFF_DIGITS: usize = 8;

/// Serialises numbering. Every session in the daemon shares one database, and
/// two agents handing off at the same moment must not both take number 7.
static NUMBERING: Mutex<()> = Mutex::new(());

/// A handoff as the tool receives it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Handoff {
    /// Where things stand, in a sentence or two.
    pub summary: String,
    /// What was finished.
    pub done: Vec<String>,
    /// What should happen next, most important first.
    pub next: Vec<String>,
    /// What is in the way.
    pub blockers: Vec<String>,
    /// What needs an answer from someone.
    pub questions: Vec<String>,
}

impl Handoff {
    fn to_json(&self) -> Json {
        json!({
            "summary": self.summary,
            "done": self.done,
            "next": self.next,
            "blockers": self.blockers,
            "questions": self.questions,
        })
    }
}

/// What writing a handoff produced.
#[derive(Debug, Clone)]
pub struct Written {
    /// The key it was stored under.
    pub key: String,
    /// Its number in the namespace, from 1.
    pub number: u64,
    /// The commit that stored it.
    pub commit: memfork_core::CommitId,
}

/// Store a handoff as the next one in `ns`.
pub fn write(
    db: &Db,
    branch: &str,
    ns: &str,
    note: &Handoff,
    written_by: Option<&str>,
) -> Result<Written, memfork_core::Error> {
    let _numbering = NUMBERING.lock().unwrap_or_else(|e| e.into_inner());
    let prefix = namespace::prefix(ns, "handoff");
    let number = numbered(
        db.list(branch, &prefix, None)?
            .iter()
            .map(|(k, _)| k.as_str()),
        &prefix,
    )
    .map(|(n, _)| n)
    .max()
    .unwrap_or(0)
        + 1;
    let key = format!("{prefix}{number:0width$}", width = HANDOFF_DIGITS);
    let mut value = Value::new(note.to_json().to_string()).with_importance(HANDOFF_IMPORTANCE);
    if let Some(by) = written_by {
        value = value.with_meta(WRITTEN_BY, by);
    }
    let commit = db.put(branch, &key, value)?;
    Ok(Written {
        key,
        number,
        commit,
    })
}

/// Handoff keys under `prefix`, with their numbers. Anything under the prefix
/// that is not a number — written by hand with the raw tools — is ignored
/// rather than trusted.
fn numbered<'a>(
    keys: impl Iterator<Item = &'a str> + 'a,
    prefix: &'a str,
) -> impl Iterator<Item = (u64, &'a str)> + 'a {
    keys.filter_map(move |k| {
        let rest = k.strip_prefix(prefix)?;
        (!rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
            .then(|| rest.parse::<u64>().ok().map(|n| (n, k)))
            .flatten()
    })
}

/// Build the briefing for `ns` on `branch`.
pub fn briefing(db: &Db, branch: &str, ns: &str) -> Result<Json, memfork_core::Error> {
    let handoff_prefix = namespace::prefix(ns, "handoff");
    let handoffs = db.list(branch, &handoff_prefix, None)?;
    let latest = numbered(handoffs.iter().map(|(k, _)| k.as_str()), &handoff_prefix)
        .max_by_key(|(n, _)| *n)
        .and_then(|(n, key)| {
            handoffs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(k, e)| (n, k.clone(), e.clone()))
        });
    let handoff_count = numbered(handoffs.iter().map(|(k, _)| k.as_str()), &handoff_prefix).count();

    // Newest write first; key order breaks ties, so the same store always
    // gives the same briefing.
    let mut decisions = db.list(branch, &namespace::prefix(ns, "decision"), None)?;
    decisions.sort_by(|(ka, a), (kb, b)| {
        b.last_access_seq
            .cmp(&a.last_access_seq)
            .then_with(|| ka.cmp(kb))
    });

    let tasks: Vec<_> = db
        .list(branch, &namespace::prefix(ns, "task"), None)?
        .into_iter()
        .filter(|(_, e)| !is_done(e))
        .collect();

    if latest.is_none() && decisions.is_empty() && tasks.is_empty() {
        return Ok(json!({
            "namespace": ns,
            "branch": branch,
            "empty": true,
            "hint": format!(
                "Nothing is recorded for `{ns}` yet, so there is no earlier work to \
                 pick up. As you work, store each decision and its reason with \
                 memfork_put under `{ns}:decision:<topic>`, open tasks under \
                 `{ns}:task:<id>`, and call memfork_handoff before you stop."
            ),
        }));
    }

    let mut truncated = false;
    let mut brief = Brief {
        ns: ns.to_owned(),
        branch: branch.to_owned(),
        handoff: latest
            .as_ref()
            .map(|(number, key, entry)| HandoffView::read(*number, key, entry, &mut truncated)),
        earlier_handoffs: handoff_count.saturating_sub(usize::from(latest.is_some())),
        decisions: decisions
            .iter()
            .take(MAX_DECISIONS)
            .map(|(k, e)| item_json(k, e, &mut truncated))
            .collect(),
        tasks: tasks
            .iter()
            .take(MAX_TASKS)
            .map(|(k, e)| item_json(k, e, &mut truncated))
            .collect(),
        omitted_decisions: decisions.len().saturating_sub(MAX_DECISIONS),
        omitted_tasks: tasks.len().saturating_sub(MAX_TASKS),
        truncated,
    };

    // Over budget: give things up one at a time, least useful to the next
    // agent first, until it fits. What was done matters less than what is
    // next, and older decisions less than the handoff that followed them.
    while brief.render().to_string().len() > MAX_BRIEFING_BYTES && brief.give_up_one() {}
    Ok(brief.render())
}

/// A briefing being assembled, kept in parts so it can be trimmed to fit.
struct Brief {
    ns: String,
    branch: String,
    handoff: Option<HandoffView>,
    earlier_handoffs: usize,
    decisions: Vec<Json>,
    tasks: Vec<Json>,
    omitted_decisions: usize,
    omitted_tasks: usize,
    truncated: bool,
}

impl Brief {
    /// Drop the least useful remaining item. `false` when nothing is left to
    /// drop.
    fn give_up_one(&mut self) -> bool {
        self.truncated = true;
        if let Some(h) = &mut self.handoff {
            for list in [&mut h.done, &mut h.questions] {
                if list.items.pop().is_some() {
                    list.omitted += 1;
                    return true;
                }
            }
        }
        if self.decisions.pop().is_some() {
            self.omitted_decisions += 1;
            return true;
        }
        if self.tasks.pop().is_some() {
            self.omitted_tasks += 1;
            return true;
        }
        if let Some(h) = &mut self.handoff {
            for list in [&mut h.blockers, &mut h.next] {
                if list.items.pop().is_some() {
                    list.omitted += 1;
                    return true;
                }
            }
        }
        false
    }

    fn render(&self) -> Json {
        let ns = &self.ns;
        let mut map = Map::new();
        map.insert("namespace".to_owned(), json!(ns));
        map.insert("branch".to_owned(), json!(self.branch));
        map.insert("empty".to_owned(), json!(false));
        map.insert(
            "latest_handoff".to_owned(),
            self.handoff
                .as_ref()
                .map_or(Json::Null, HandoffView::render),
        );
        map.insert("earlier_handoffs".to_owned(), json!(self.earlier_handoffs));
        map.insert("recent_decisions".to_owned(), json!(self.decisions));
        map.insert("open_tasks".to_owned(), json!(self.tasks));
        if self.omitted_decisions > 0 || self.omitted_tasks > 0 {
            map.insert(
                "omitted".to_owned(),
                json!({
                    "decisions": self.omitted_decisions,
                    "tasks": self.omitted_tasks,
                    "hint": format!(
                        "More is stored than fits in a briefing. memfork_list with \
                         prefix `{ns}:decision:` or `{ns}:task:` shows all of it."
                    ),
                }),
            );
        }
        let handoff_cut = self.handoff.as_ref().is_some_and(HandoffView::cut);
        map.insert(
            "truncated".to_owned(),
            json!(
                self.truncated
                    || handoff_cut
                    || self.omitted_decisions > 0
                    || self.omitted_tasks > 0
            ),
        );
        Json::Object(map)
    }
}

/// One list from a handoff, and how many of its items did not fit.
#[derive(Default)]
struct Items {
    items: Vec<String>,
    omitted: usize,
}

/// The latest handoff, as a briefing shows it.
struct HandoffView {
    key: String,
    number: u64,
    by: Option<String>,
    summary: Option<String>,
    next: Items,
    blockers: Items,
    questions: Items,
    done: Items,
}

impl HandoffView {
    fn read(number: u64, key: &str, entry: &Entry, truncated: &mut bool) -> Self {
        let raw = String::from_utf8_lossy(&entry.value);
        let parsed: Json = serde_json::from_str(&raw).unwrap_or(Json::Null);
        let list = |field: &str, truncated: &mut bool| -> Items {
            let all: Vec<&str> = parsed
                .get(field)
                .and_then(Json::as_array)
                .map(|a| a.iter().filter_map(Json::as_str).collect())
                .unwrap_or_default();
            Items {
                items: all
                    .iter()
                    .take(MAX_LIST_ITEMS)
                    .map(|s| clip(s, truncated))
                    .collect(),
                omitted: all.len().saturating_sub(MAX_LIST_ITEMS),
            }
        };
        let (summary, next, blockers, questions, done) = if parsed.is_object() {
            (
                parsed
                    .get("summary")
                    .and_then(Json::as_str)
                    .map(|s| clip(s, truncated)),
                list("next", truncated),
                list("blockers", truncated),
                list("questions", truncated),
                list("done", truncated),
            )
        } else {
            // Written by hand under a handoff key, not by memfork_handoff:
            // show it as text rather than pretend it has the structure.
            (
                Some(clip(&raw, truncated)),
                Items::default(),
                Items::default(),
                Items::default(),
                Items::default(),
            )
        };
        HandoffView {
            key: key.to_owned(),
            number,
            by: entry.meta.get(WRITTEN_BY).cloned(),
            summary,
            next,
            blockers,
            questions,
            done,
        }
    }

    /// Whether any list lost items.
    fn cut(&self) -> bool {
        [&self.next, &self.blockers, &self.questions, &self.done]
            .iter()
            .any(|l| l.omitted > 0)
    }

    fn render(&self) -> Json {
        let mut map = Map::new();
        map.insert("key".to_owned(), json!(self.key));
        map.insert("number".to_owned(), json!(self.number));
        map.insert("by".to_owned(), json!(self.by));
        map.insert("summary".to_owned(), json!(self.summary));
        let mut omitted = Map::new();
        for (name, list) in [
            ("next", &self.next),
            ("blockers", &self.blockers),
            ("questions", &self.questions),
            ("done", &self.done),
        ] {
            map.insert(name.to_owned(), json!(list.items));
            if list.omitted > 0 {
                omitted.insert(name.to_owned(), json!(list.omitted));
            }
        }
        if !omitted.is_empty() {
            // Counted, so the reader knows there was more; the full note is
            // under `key`, for memfork_get.
            map.insert("omitted_items".to_owned(), Json::Object(omitted));
        }
        Json::Object(map)
    }
}

fn item_json(key: &str, entry: &Entry, truncated: &mut bool) -> Json {
    json!({
        "key": key,
        "value": clip(&String::from_utf8_lossy(&entry.value), truncated),
        "by": entry.meta.get(WRITTEN_BY),
    })
}

/// A task is closed when its value is a JSON object whose `status` is `done`.
/// Anything else — plain text included — is open.
fn is_done(entry: &Entry) -> bool {
    serde_json::from_slice::<Json>(&entry.value)
        .ok()
        .and_then(|v| {
            v.get("status")
                .and_then(Json::as_str)
                .map(|s| s.eq_ignore_ascii_case("done"))
        })
        .unwrap_or(false)
}

/// Cut text to [`MAX_TEXT_CHARS`] characters, marking that it was cut.
fn clip(s: &str, truncated: &mut bool) -> String {
    if s.chars().count() <= MAX_TEXT_CHARS {
        return s.to_owned();
    }
    *truncated = true;
    let mut out: String = s.chars().take(MAX_TEXT_CHARS - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(summary: &str) -> Handoff {
        Handoff {
            summary: summary.to_owned(),
            done: vec!["one".to_owned()],
            next: vec!["two".to_owned()],
            ..Handoff::default()
        }
    }

    #[test]
    fn an_empty_project_says_so_and_how_to_start() {
        let db = Db::new();
        let b = briefing(&db, "main", "shop").unwrap();
        assert_eq!(b["empty"], true);
        let hint = b["hint"].as_str().unwrap();
        assert!(hint.contains("shop:decision:"), "{hint}");
        assert!(hint.contains("memfork_handoff"), "{hint}");
    }

    #[test]
    fn handoffs_are_numbered_and_the_newest_wins() {
        let db = Db::new();
        let first = write(&db, "main", "shop", &note("first"), Some("a")).unwrap();
        let second = write(&db, "main", "shop", &note("second"), Some("b")).unwrap();
        assert_eq!(first.key, "shop:handoff:00000001");
        assert_eq!(second.number, 2);

        let b = briefing(&db, "main", "shop").unwrap();
        assert_eq!(b["latest_handoff"]["summary"], "second");
        assert_eq!(b["latest_handoff"]["by"], "b");
        assert_eq!(b["earlier_handoffs"], 1);
        // The first is still there, as history.
        assert!(db.get("main", &first.key).unwrap().is_some());
    }

    #[test]
    fn namespaces_do_not_see_each_other() {
        let db = Db::new();
        write(&db, "main", "shop", &note("shop work"), None).unwrap();
        db.put("main", "shop:decision:db", Value::new("postgres"))
            .unwrap();
        assert_eq!(briefing(&db, "main", "blog").unwrap()["empty"], true);
        // Nor does a namespace that is a prefix of another's name.
        db.put("main", "shopfront:decision:x", Value::new("y"))
            .unwrap();
        let b = briefing(&db, "main", "shop").unwrap();
        assert_eq!(b["recent_decisions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn decisions_are_newest_first_with_key_order_breaking_ties() {
        let db = Db::new();
        db.put("main", "p:decision:b", Value::new("1")).unwrap();
        db.put("main", "p:decision:a", Value::new("2")).unwrap();
        memfork_core::with_txn(&db, "main", "two at once", 1, |t| {
            t.put("p:decision:z", Value::new("3"))?;
            t.put("p:decision:y", Value::new("4"))?;
            Ok(())
        })
        .unwrap();
        let b = briefing(&db, "main", "p").unwrap();
        let keys: Vec<&str> = b["recent_decisions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["key"].as_str().unwrap())
            .collect();
        assert_eq!(
            keys,
            [
                "p:decision:y",
                "p:decision:z",
                "p:decision:a",
                "p:decision:b"
            ]
        );
    }

    #[test]
    fn done_tasks_are_left_out() {
        let db = Db::new();
        db.put("main", "p:task:1", Value::new("plain text is open"))
            .unwrap();
        db.put("main", "p:task:2", Value::new(r#"{"status":"done"}"#))
            .unwrap();
        db.put(
            "main",
            "p:task:3",
            Value::new(r#"{"status":"open","what":"x"}"#),
        )
        .unwrap();
        let b = briefing(&db, "main", "p").unwrap();
        let keys: Vec<&str> = b["open_tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, ["p:task:1", "p:task:3"]);
    }

    #[test]
    fn a_briefing_stays_small_however_much_is_stored() {
        let db = Db::new();
        let long = "x".repeat(5_000);
        let big = Handoff {
            summary: long.clone(),
            done: vec![long.clone(); 50],
            next: vec![long.clone(); 50],
            blockers: vec![long.clone(); 50],
            questions: vec![long.clone(); 50],
        };
        write(&db, "main", "p", &big, None).unwrap();
        for i in 0..200 {
            db.put(
                "main",
                &format!("p:decision:{i:03}"),
                Value::new(long.clone()),
            )
            .unwrap();
            db.put("main", &format!("p:task:{i:03}"), Value::new(long.clone()))
                .unwrap();
        }
        let b = briefing(&db, "main", "p").unwrap();
        assert!(
            b.to_string().len() <= MAX_BRIEFING_BYTES,
            "{}",
            b.to_string().len()
        );
        assert_eq!(b["truncated"], true);
        assert!(b["omitted"]["decisions"].as_u64().unwrap() >= 190);
        assert!(b["omitted"]["hint"]
            .as_str()
            .unwrap()
            .contains("p:decision:"));
        // What was done went first; what comes next survived.
        let handoff = &b["latest_handoff"];
        assert!(!handoff["next"].as_array().unwrap().is_empty(), "{handoff}");
        assert!(handoff["done"].as_array().unwrap().is_empty(), "{handoff}");
        assert!(handoff["omitted_items"]["done"].as_u64().unwrap() >= 10);
    }

    #[test]
    fn the_same_store_gives_the_same_briefing() {
        let build = || {
            let db = Db::new();
            write(&db, "main", "p", &note("n"), Some("a")).unwrap();
            for i in 0..30 {
                db.put(
                    "main",
                    &format!("p:decision:{i}"),
                    Value::new(format!("{i}")),
                )
                .unwrap();
            }
            briefing(&db, "main", "p").unwrap().to_string()
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn a_hand_written_handoff_key_is_shown_as_text() {
        let db = Db::new();
        db.put("main", "p:handoff:00000001", Value::new("just words"))
            .unwrap();
        db.put(
            "main",
            "p:handoff:notes",
            Value::new("not numbered, ignored"),
        )
        .unwrap();
        let b = briefing(&db, "main", "p").unwrap();
        assert_eq!(b["latest_handoff"]["summary"], "just words");
        assert_eq!(b["earlier_handoffs"], 0);
        let next = write(&db, "main", "p", &note("n"), None).unwrap();
        assert_eq!(next.number, 2);
    }
}
