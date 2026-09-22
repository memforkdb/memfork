//! The current view as one file: the page, its style and its script inlined,
//! with the summary, the graph and every entry of the project embedded as
//! data, so it opens from disk with no daemon and reaches out to nothing.
//!
//! Memory is shared and shown, so an export is checked the way a write is:
//! every value goes through the secret detector, and one that looks like a
//! credential is replaced with a marker rather than carried. The page shows
//! a preview first, and the file is made by the browser's own download;
//! nothing is uploaded, by this or by anything else here.

use std::collections::BTreeSet;

use hyper::StatusCode;
use serde_json::{json, Value as Json};

use super::{graph, summary, Context, Failure, Query};
use crate::secrets::{self, Allow, Refused};

/// What replaces a value that looked like a credential.
pub const WITHHELD: &str = "[withheld: looked like a credential]";

/// Longest value carried for one entry.
const VALUE_CHARS: usize = 20_000;

/// Fields whose strings are names and ids, never values, so they are not
/// checked and never replaced: replacing an id would break the graph.
const NAMED: &[&str] = &[
    "key",
    "keys",
    "id",
    "branch",
    "branches",
    "a",
    "b",
    "namespace",
    "namespaces",
    "to",
    "by",
    "who",
    "kind",
    "state",
    "status",
    "held_by",
    "done_by",
    "path",
    "sources",
    "columns",
    "policy",
    "version",
    "client",
    "client_id",
    "task",
    "depends_on",
    "blocked_by",
    "what",
    "commit",
    "message",
];

/// An export, made once for both the preview and the file.
#[derive(Debug, Clone)]
pub struct Export {
    /// The file.
    pub html: String,
    /// What the page shows before the download: size, counts, what was
    /// withheld.
    pub preview: Json,
}

/// Build the export of a project on a branch, now or at a past point.
pub fn build(context: &Context, query: &Query) -> Result<Export, Failure> {
    let branch = summary::branch_of(context, query)?;
    let ns = summary::namespace_of(context, query, &branch)?;
    let at = query.number("at")?;
    let db = &context.db;

    let mut summary = summary::summary(context, query)?;
    let g = graph::build(
        db,
        &branch,
        &ns,
        at,
        &context.side,
        &context.events.connected(),
    )
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let mut graph_json = graph::to_json(&g);

    // Every entry, for the side sheet: what `entry` would answer, minus the
    // history, which is not part of an export.
    let view = match at {
        Some(seq) => db.at(&branch, seq),
        None => db.read(&branch),
    }
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let prefix = format!("{ns}{}", crate::namespace::SEPARATOR);
    let stale: BTreeSet<String> = context.side.sidecar.stale_facts(&ns).into_iter().collect();
    let fresh: BTreeSet<String> = context.side.sidecar.fresh_facts(&ns).into_iter().collect();
    let mut entries = serde_json::Map::new();
    for (key, entry) in view.list(&prefix, None) {
        let state = if stale.contains(&key) {
            "stale"
        } else if fresh.contains(&key) {
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
        let value: String = String::from_utf8_lossy(&entry.value)
            .chars()
            .take(VALUE_CHARS)
            .collect();
        let meta: serde_json::Map<String, Json> = entry
            .meta
            .iter()
            .filter(|(k, _)| k.as_str() != crate::facts::SOURCES_META)
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        entries.insert(
            key.clone(),
            json!({
                "key": key,
                "branch": branch,
                "at": at,
                "value": value,
                "bytes": entry.value.len(),
                "by": entry.meta.get(memfork_core::WRITTEN_BY).map(|w| crate::clients::display_for_writer(w)),
                "importance": entry.importance,
                "created_seq": entry.created_seq,
                "last_seq": entry.last_access_seq,
                "meta": meta,
                "sources": sources,
                "history": [],
                "history_truncated": false,
                "exported": true,
            }),
        );
    }
    let mut entries = Json::Object(entries);

    // The check a write gets, applied to everything that could carry a
    // value. Ids and names are left alone; a rule set that cannot be read
    // stops the export rather than let something through.
    let mut withheld = Vec::new();
    scrub(&mut summary, "summary", &mut withheld)?;
    scrub(&mut entries, "entry", &mut withheld)?;
    if let Some(nodes) = graph_json["nodes"].as_array_mut() {
        for node in nodes {
            let at = format!("graph:{}", node[0].as_str().unwrap_or(""));
            if let Some(label) = node.get_mut(2) {
                scrub_string(label, &at, &mut withheld)?;
            }
        }
    }

    let data = json!({
        "version": crate::VERSION,
        "namespace": ns,
        "branch": branch,
        "at": at,
        "seq": view.seq(),
        "summary": summary,
        "graph": graph_json,
        "entries": entries,
        "withheld": withheld.len(),
    });
    let html = assemble(&data);
    let preview = json!({
        "namespace": ns,
        "branch": branch,
        "at": at,
        "seq": view.seq(),
        "entries": data["entries"].as_object().map_or(0, serde_json::Map::len),
        "nodes": g.nodes.len(),
        "bytes": html.len(),
        "withheld": withheld.iter().map(|(at, rule)| json!({ "where": at, "rule": rule })).collect::<Vec<_>>(),
        "file": format!("memfork-brain-{ns}-{branch}-seq{}.html", at.unwrap_or(view.seq())),
    });
    Ok(Export { html, preview })
}

/// Walk a JSON value and check every string that could be a value.
fn scrub(value: &mut Json, at: &str, withheld: &mut Vec<(String, String)>) -> Result<(), Failure> {
    match value {
        Json::Object(map) => {
            for (name, inner) in map.iter_mut() {
                if NAMED.contains(&name.as_str()) {
                    continue;
                }
                scrub(inner, &format!("{at}.{name}"), withheld)?;
            }
        }
        Json::Array(items) => {
            for (i, inner) in items.iter_mut().enumerate() {
                scrub(inner, &format!("{at}[{i}]"), withheld)?;
            }
        }
        Json::String(_) => scrub_string(value, at, withheld)?,
        _ => {}
    }
    Ok(())
}

fn scrub_string(
    value: &mut Json,
    at: &str,
    withheld: &mut Vec<(String, String)>,
) -> Result<(), Failure> {
    let Some(text) = value.as_str() else {
        return Ok(());
    };
    match secrets::check("value", text, &Allow::default()) {
        Ok(()) => Ok(()),
        Err(Refused::Secret(found)) => {
            withheld.push((at.to_owned(), found.rule));
            *value = json!(WITHHELD);
            Ok(())
        }
        Err(other) => Err((StatusCode::INTERNAL_SERVER_ERROR, other.to_string())),
    }
}

/// The page with its style and script inlined and the data embedded. The
/// data is JSON inside a script element, so `</` and `<!--` are escaped
/// the way JSON allows, and nothing in it can end the element early.
fn assemble(data: &Json) -> String {
    let embedded = data
        .to_string()
        .replace("</", "<\\/")
        .replace("<!--", "<\\u0021--");
    super::PAGE_HTML
        .replace(
            "<link rel=\"stylesheet\" href=\"/brain/app.css\">",
            &format!("<style>\n{}\n</style>", super::PAGE_CSS),
        )
        .replace(
            "<script src=\"/brain/app.js\"></script>",
            &format!(
                "<script>window.MEMFORK_EXPORT = {embedded};</script>\n<script>\n{}\n</script>",
                super::PAGE_JS
            ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_that_look_like_credentials_are_withheld_and_ids_are_not() {
        // Built at run time so this file never holds anything shaped like a key.
        let key = format!("AKIA{}", "B".repeat(16));
        let mut value = json!({
            "key": format!("shop:note:{key}"),
            "value": format!("the id is {key}"),
            "tasks": [{ "id": "t1", "title": format!("rotate {key}") }],
            "by": "someone",
        });
        let mut withheld = Vec::new();
        scrub(&mut value, "x", &mut withheld).unwrap();
        assert_eq!(value["value"], WITHHELD);
        assert_eq!(value["tasks"][0]["title"], WITHHELD);
        assert_eq!(value["key"], format!("shop:note:{key}"));
        assert_eq!(withheld.len(), 2);
        assert_eq!(withheld[0].1, "aws-access-key");
    }

    #[test]
    fn the_embedded_data_cannot_end_the_script_element() {
        let html = assemble(&json!({ "value": "</script><!-- x" }));
        assert!(!html.contains("</script><!--"));
        assert!(html.contains("<\\/script><\\u0021--"));
        assert!(html.contains("window.MEMFORK_EXPORT"));
        assert!(!html.contains("href=\"/brain/app.css\""));
        assert!(!html.contains("src=\"/brain/app.js\""));
    }
}
