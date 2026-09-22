//! Duplicates and contradictions, flagged and never fixed (DESIGN §6.12).
//!
//! Three deterministic checks over a project's memory, each a thing an agent
//! or a person should look at. MemFork reports them — in briefings, on the
//! write that caused one, in `memfork watch`, `memfork flags` and `memfork
//! doctor` — and never resolves one itself: which of two decisions is right
//! is a judgement, and judgements belong to the agents and the people.
//!
//! * **Conflicting decisions**: one `<project>:decision:<topic>` key holding
//!   different values on different branches, written by different clients.
//! * **Duplicates**: the same value under two near-identical keys in one
//!   family — equal once case, `-`, `_`, `.`, spaces and a trailing `s` are
//!   set aside, or at most two edits apart for keys of eight characters or
//!   more.
//! * **Facts that disagree**: two facts resting on exactly the same source
//!   files that say different things. Facts that merely share a file are
//!   normal — several findings come from one file — so only identical source
//!   sets count.
//!
//! Bounded: a family with more than [`MAX_FAMILY`] entries is not compared
//! pair by pair, and at most [`MAX_FLAGS`] flags are returned.

use std::collections::BTreeMap;

use memfork_core::{Db, Entry, WRITTEN_BY};
use serde_json::{json, Value as Json};

use crate::namespace;

/// Most entries in one family that are compared pair by pair.
pub const MAX_FAMILY: usize = 2000;

/// Most flags one scan returns.
pub const MAX_FLAGS: usize = 50;

/// Longest value shown in a flag, in characters.
const SHOWN_CHARS: usize = 120;

fn shown(entry: &Entry) -> String {
    let text = String::from_utf8_lossy(&entry.value);
    let mut out: String = text.chars().take(SHOWN_CHARS).collect();
    if text.chars().count() > SHOWN_CHARS {
        out.push('…');
    }
    out
}

/// A key with the differences that do not matter set aside.
fn normal(key: &str) -> String {
    let mut out: String = key
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | '.' | ' '))
        .flat_map(char::to_lowercase)
        .collect();
    if out.ends_with('s') {
        out.pop();
    }
    out
}

/// Edit distance, giving up once it is over `limit`.
fn within(a: &str, b: &str, limit: usize) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > limit {
        return false;
    }
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        let mut best = row[0];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let next = (row[j + 1] + 1).min(row[j] + 1).min(prev + cost);
            prev = row[j + 1];
            row[j + 1] = next;
            best = best.min(next);
        }
        if best > limit {
            return false;
        }
    }
    row[b.len()] <= limit
}

/// Whether two keys in one family are near enough to be the same thing,
/// judged by their last parts: the topics.
pub fn near(a: &str, b: &str) -> bool {
    if a == b {
        return false;
    }
    let topic = |k: &str| {
        k.rfind(namespace::SEPARATOR)
            .map_or(k, |i| &k[i + namespace::SEPARATOR.len_utf8()..])
            .to_owned()
    };
    let (ta, tb) = (topic(a), topic(b));
    // Numbered entries, such as handoffs, lessons and tasks, differ by their
    // numbers on purpose.
    let numbered = |t: &str| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit());
    if numbered(&ta) && numbered(&tb) {
        return false;
    }
    let (na, nb) = (normal(&ta), normal(&tb));
    na == nb || (ta.chars().count() >= 8 && tb.chars().count() >= 8 && within(&na, &nb, 2))
}

/// The family a key belongs to: everything up to its last `:`.
fn family(key: &str) -> &str {
    key.rfind(namespace::SEPARATOR).map_or("", |i| &key[..i])
}

/// Other branches' different values for a decision key, from other clients.
pub fn conflicts_for(db: &Db, key: &str, branch: &str, by: Option<&str>) -> Vec<Json> {
    let mut out = Vec::new();
    let here = db.get(branch, key).ok().flatten();
    for other in db.branches() {
        if other.name == branch {
            continue;
        }
        let Ok(Some(there)) = db.get(&other.name, key) else {
            continue;
        };
        let differs = here.as_ref().is_none_or(|h| h.value != there.value);
        let writer = there.meta.get(WRITTEN_BY).map(String::as_str);
        if differs && writer.is_some() && writer != by {
            out.push(json!({
                "branch": other.name,
                "by": writer,
                "value": shown(&there),
            }));
        }
    }
    out
}

/// Keys in the same family as `key` on `branch` that hold the same value
/// under a near-identical key.
pub fn similar_to(db: &Db, branch: &str, key: &str, value: &[u8]) -> Vec<String> {
    let fam = family(key);
    let Ok(entries) = db.list(
        branch,
        &format!("{fam}{}", namespace::SEPARATOR),
        Some(MAX_FAMILY + 1),
    ) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|(k, e)| family(k) == fam && e.value.as_ref() == value && near(k, key))
        .map(|(k, _)| k.clone())
        .collect()
}

/// Every flag in `ns`, looking at `branch` and, for decisions, every branch.
pub fn scan(db: &Db, branch: &str, ns: &str) -> Result<Vec<Json>, memfork_core::Error> {
    let mut flags = Vec::new();
    let prefix = format!("{ns}{}", namespace::SEPARATOR);
    let entries = db.list(branch, &prefix, None)?;

    // Conflicting decisions, across branches.
    let decision = namespace::prefix(ns, "decision");
    let mut seen: BTreeMap<String, BTreeMap<String, (String, Option<String>)>> = BTreeMap::new();
    for b in db.branches() {
        for (key, entry) in db.list(&b.name, &decision, None)? {
            seen.entry(key).or_default().insert(
                b.name.clone(),
                (shown(&entry), entry.meta.get(WRITTEN_BY).cloned()),
            );
        }
    }
    for (key, by_branch) in &seen {
        let values: std::collections::BTreeSet<&String> =
            by_branch.values().map(|(v, _)| v).collect();
        let writers: std::collections::BTreeSet<&Option<String>> =
            by_branch.values().map(|(_, w)| w).collect();
        if values.len() > 1 && writers.len() > 1 {
            flags.push(json!({
                "kind": "conflicting_decisions",
                "key": key,
                "branches": by_branch.iter().map(|(b, (v, w))| json!({
                    "branch": b, "by": w, "value": v,
                })).collect::<Vec<_>>(),
                "look": "Two clients decided differently on different branches; merge one way, or record why both stand.",
            }));
        }
    }

    // Duplicates, family by family.
    let mut families: BTreeMap<&str, Vec<&(String, std::sync::Arc<Entry>)>> = BTreeMap::new();
    for pair in &entries {
        families.entry(family(&pair.0)).or_default().push(pair);
    }
    for members in families.values() {
        if members.len() > MAX_FAMILY {
            continue;
        }
        for (i, (a, ea)) in members.iter().map(|p| (&p.0, &p.1)).enumerate() {
            for (b, eb) in members.iter().skip(i + 1).map(|p| (&p.0, &p.1)) {
                if ea.value == eb.value && near(a, b) {
                    flags.push(json!({
                        "kind": "duplicate",
                        "keys": [a, b],
                        "value": shown(ea),
                        "look": "The same value under two near-identical keys; keep one and delete the other.",
                    }));
                }
            }
        }
    }

    // Facts on the same files that say different things.
    let mut by_sources: BTreeMap<Vec<String>, Vec<&(String, std::sync::Arc<Entry>)>> =
        BTreeMap::new();
    for pair in &entries {
        if let Some(mut sources) = crate::facts::sources_of(&pair.1.meta) {
            sources.sort();
            by_sources.entry(sources).or_default().push(pair);
        }
    }
    for (sources, facts) in &by_sources {
        let values: std::collections::BTreeSet<&[u8]> =
            facts.iter().map(|p| p.1.value.as_ref()).collect();
        if facts.len() > 1 && values.len() > 1 {
            flags.push(json!({
                "kind": "facts_disagree",
                "keys": facts.iter().map(|p| p.0.clone()).collect::<Vec<_>>(),
                "sources": sources,
                "look": "Facts from exactly the same files say different things; check the files and keep the one that is true.",
            }));
        }
    }
    flags.truncate(MAX_FLAGS);
    Ok(flags)
}

#[cfg(test)]
mod tests {
    use super::*;
    use memfork_core::Value;

    fn put(db: &Db, branch: &str, key: &str, value: &str, by: &str) {
        db.put(
            branch,
            key,
            Value::new(value.to_owned()).with_meta(WRITTEN_BY, by),
        )
        .expect("put");
    }

    #[test]
    fn near_keys_are_the_same_thing_and_different_ones_are_not() {
        assert!(near(
            "p:decision:payment-provider",
            "p:decision:payment_providers"
        ));
        assert!(near(
            "p:decision:paymentprovider",
            "p:decision:paymentprovidr"
        ));
        assert!(!near("p:decision:db", "p:decision:dbs2"));
        assert!(!near("p:decision:auth", "p:decision:cache"));
        assert!(!near("p:decision:x", "p:decision:x"));
        assert!(!near("p:handoff:00000001", "p:handoff:00000002"));
    }

    #[test]
    fn each_kind_is_found_and_nothing_else_is() {
        let db = Db::new();
        put(&db, "main", "p:decision:db", "postgres", "claude-code");
        db.fork("main", "try").expect("fork");
        put(&db, "try", "p:decision:db", "sqlite", "codex-mcp-client");
        put(&db, "main", "p:note:deploy-steps", "run make deploy", "a");
        put(&db, "main", "p:note:deploy_step", "run make deploy", "b");
        put(&db, "main", "p:note:other", "run make deploy", "b");
        let fact = |key: &str, value: &str, sources: &[&str]| {
            let paths: Vec<String> = sources.iter().map(|s| (*s).to_owned()).collect();
            db.put(
                "main",
                key,
                Value::new(value.to_owned()).with_meta(
                    crate::facts::SOURCES_META,
                    crate::facts::sources_meta(&paths),
                ),
            )
            .expect("fact");
        };
        fact("p:fact:a", "auth is in login.rs", &["src/login.rs"]);
        fact("p:fact:b", "auth is in session.rs", &["src/login.rs"]);
        fact(
            "p:fact:c",
            "login.rs also logs",
            &["src/login.rs", "src/log.rs"],
        );

        let flags = scan(&db, "main", "p").expect("scan");
        let kinds: Vec<&str> = flags
            .iter()
            .map(|f| f["kind"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            kinds,
            ["conflicting_decisions", "duplicate", "facts_disagree"],
            "{flags:?}"
        );
        assert_eq!(
            flags[1]["keys"],
            json!(["p:note:deploy-steps", "p:note:deploy_step"])
        );
        assert_eq!(flags[2]["keys"], json!(["p:fact:a", "p:fact:b"]));
        assert_eq!(
            scan(&db, "main", "p").expect("again"),
            flags,
            "not deterministic"
        );
    }

    #[test]
    fn the_same_client_changing_its_mind_on_a_fork_is_not_a_conflict() {
        let db = Db::new();
        put(&db, "main", "p:decision:db", "postgres", "claude-code");
        db.fork("main", "try").expect("fork");
        put(&db, "try", "p:decision:db", "sqlite", "claude-code");
        assert!(scan(&db, "main", "p").expect("scan").is_empty());
        assert!(conflicts_for(&db, "p:decision:db", "try", Some("claude-code")).is_empty());
        assert_eq!(
            conflicts_for(&db, "p:decision:db", "try", Some("codex")).len(),
            1
        );
    }
}
