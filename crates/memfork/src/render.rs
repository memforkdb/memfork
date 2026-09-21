//! Turning a command's result into lines for a person (DESIGN §5.2).
//!
//! A command's result is data — the same JSON `--json` prints — computed
//! wherever the store is. What a person sees is drawn from it here, in the
//! process attached to the terminal, so `memfork branches` looks the same
//! whether the daemon ran it or `--ephemeral` did, and colour follows the
//! terminal in front of the person rather than the daemon's.

use serde_json::Value as Json;

use crate::cli::Command;
use crate::events::Event;
use crate::exec::Outcome;
use crate::style::{palette, Glyph, Style};

/// The lines to print for a command's outcome.
pub fn lines(command: &Command, outcome: &Outcome, style: &Style) -> Vec<String> {
    match command {
        Command::Log { graph: true, .. } => crate::history::render(&outcome.json["graph"], style),
        Command::Branches => branches(&outcome.json, style),
        Command::Diff { .. } => diff(&outcome.json, style),
        Command::Fork { .. } => lead(&outcome.text, |w| style.accent(w)),
        Command::Merge { .. } => lead(&outcome.text, |w| style.accent(w)),
        Command::Discard { .. } => lead(&outcome.text, |w| style.warn(w)),
        _ => outcome.text.clone(),
    }
}

/// The lines, with the first word of the first line styled.
fn lead(text: &[String], paint: impl Fn(&str) -> String) -> Vec<String> {
    text.iter()
        .enumerate()
        .map(|(i, line)| match (i, line.split_once(' ')) {
            (0, Some((word, rest))) => format!("{} {rest}", paint(word)),
            _ => line.clone(),
        })
        .collect()
}

/// `memfork branches`: one line each, saying where it stands against the
/// default branch in words.
pub fn branches(json: &Json, style: &Style) -> Vec<String> {
    let all = json["branches"].as_array().cloned().unwrap_or_default();
    let width = all
        .iter()
        .filter_map(|b| b["name"].as_str())
        .map(str::len)
        .max()
        .unwrap_or(0);
    let default_name = all
        .iter()
        .find(|b| b["is_default"] == true)
        .and_then(|b| b["name"].as_str())
        .unwrap_or("main")
        .to_owned();
    all.iter()
        .map(|b| {
            let name = b["name"].as_str().unwrap_or_default();
            let head = b["head"].as_str().unwrap_or_default();
            let keys = b["key_count"].as_u64().unwrap_or(0);
            let mut parts = vec![
                format!(
                    "{}{}",
                    style.strong(palette::PRIMARY, name),
                    " ".repeat(width - name.len())
                ),
                style.dim(&head[..head.len().min(12)]),
                style.dim(&format!("seq {}", b["seq"].as_u64().unwrap_or(0))),
                format!("{keys} key{}", if keys == 1 { "" } else { "s" }),
            ];
            if b["is_default"] == true {
                parts.push(style.badge(palette::NAVY, "default"));
            } else {
                let ahead = b["ahead"].as_u64().unwrap_or(0);
                let behind = b["behind"].as_u64().unwrap_or(0);
                let standing = match (ahead, behind) {
                    (0, 0) => style.dim(&format!("same as {default_name}")),
                    (a, 0) => style.accent(&format!("{a} ahead of {default_name}")),
                    (0, b) => style.warn(&format!("{b} behind {default_name}")),
                    (a, b) => style.warn(&format!("{a} ahead, {b} behind {default_name}")),
                };
                parts.push(standing);
                if let Some(seq) = b["forked_at_seq"].as_u64() {
                    parts.push(style.dim(&format!("forked at seq {seq}")));
                }
            }
            if let Some(by) = b["written_by"].as_str() {
                parts.push(style.dim(&format!(
                    "last written by {}",
                    crate::clients::display_for_writer(by)
                )));
            }
            parts.join("  ")
        })
        .collect()
}

/// `memfork diff`: one line per key, marked, coloured and named.
pub fn diff(json: &Json, style: &Style) -> Vec<String> {
    let changes = json["changes"].as_array().cloned().unwrap_or_default();
    if changes.is_empty() {
        return vec![style.dim(&format!(
            "no differences between {} and {}",
            json["a"].as_str().unwrap_or("?"),
            json["b"].as_str().unwrap_or("?")
        ))];
    }
    changes
        .iter()
        .map(|c| {
            let key = c["key"].as_str().unwrap_or_default();
            let kind = c["kind"].as_str().unwrap_or_default();
            let (glyph, paint): (Glyph, fn(&Style, &str) -> String) = match kind {
                "added" => (Glyph::Added, Style::accent),
                "removed" => (Glyph::Removed, Style::warn),
                _ => (Glyph::Modified, Style::primary),
            };
            format!(
                "{} {}  {}",
                paint(style, style.glyph(glyph)),
                paint(style, key),
                style.dim(kind)
            )
        })
        .collect()
}

/// One line of `memfork watch`.
pub fn event(e: &Event, style: &Style) -> String {
    let clock = style.dim(&crate::events::local_clock(&e.time));
    match e.kind.as_str() {
        "connected" => format!(
            "{clock}  {} {} connected{}",
            style.accent(style.glyph(Glyph::Connected)),
            style.strong(palette::PRIMARY, &e.client),
            e.namespace
                .as_deref()
                .map(|n| style.dim(&format!("  project {n}")))
                .unwrap_or_default()
        ),
        "disconnected" => format!(
            "{clock}  {} {} disconnected",
            style.dim(style.glyph(Glyph::Waiting)),
            style.primary(&e.client),
        ),
        _ => {
            let op = e.operation.as_deref().unwrap_or("?");
            let painted_op = match op {
                "fork" | "merge" | "handoff" | "resume" => style.accent(op),
                "discard" | "delete" | "del" => style.warn(op),
                _ => style.primary(op),
            };
            let mut parts = vec![
                clock,
                format!(
                    "{}{}",
                    style.primary(&e.client),
                    " ".repeat(14usize.saturating_sub(e.client.len()))
                ),
                format!(
                    "{painted_op}{}",
                    " ".repeat(9usize.saturating_sub(op.len()))
                ),
            ];
            if let Some(key) = &e.key {
                parts.push(style.primary(key));
            }
            if let Some(branch) = &e.branch {
                parts.push(style.dim(&format!("on {branch}")));
            }
            if !e.ok {
                parts.push(style.warn(&format!(
                    "failed: {}",
                    e.error.as_deref().unwrap_or("unknown error")
                )));
            }
            parts.join("  ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn branches_say_where_they_stand_in_words() {
        let json = json!({ "branches": [
            { "name": "main", "head": "a".repeat(64), "seq": 9, "key_count": 3,
              "is_default": true, "ahead": 0, "behind": 0 },
            { "name": "try-it", "head": "b".repeat(64), "seq": 11, "key_count": 1,
              "is_default": false, "ahead": 2, "behind": 1, "forked_at_seq": 7,
              "written_by": "claude-code" },
        ]});
        let lines = branches(&json, &Style::PLAIN);
        assert!(lines[0].starts_with("main  "), "{lines:?}");
        assert!(lines[0].contains("[default]"), "{lines:?}");
        assert!(lines[1].contains("2 ahead, 1 behind main"), "{lines:?}");
        assert!(lines[1].contains("forked at seq 7"), "{lines:?}");
        assert!(
            lines[1].contains("last written by Claude Code"),
            "{lines:?}"
        );
    }

    #[test]
    fn diff_lines_carry_a_mark_and_a_word() {
        let json = json!({ "a": "main", "b": "x", "changes": [
            { "key": "k1", "kind": "added" },
            { "key": "k2", "kind": "removed" },
            { "key": "k3", "kind": "modified" },
        ]});
        assert_eq!(
            diff(&json, &Style::PLAIN),
            vec!["+ k1  added", "- k2  removed", "~ k3  modified"]
        );
        let coloured = diff(
            &json,
            &Style {
                colour: true,
                unicode: false,
            },
        );
        assert!(
            coloured[0].contains("\x1b[38;2;61;220;151m"),
            "added is the accent"
        );
        assert!(
            coloured[1].contains("\x1b[38;2;232;163;61m"),
            "removed is amber"
        );
        let none = diff(
            &json!({"a": "main", "b": "x", "changes": []}),
            &Style::PLAIN,
        );
        assert_eq!(none, vec!["no differences between main and x"]);
    }

    #[test]
    fn a_watch_line_names_the_client_the_operation_and_the_key() {
        let e = Event {
            operation: Some("handoff".to_owned()),
            key: Some("shop:handoff:00000002".to_owned()),
            branch: Some("main".to_owned()),
            ..Event::about("claude-code", Some("shop"))
        };
        let line = event(&e, &Style::PLAIN);
        assert!(line.contains("Claude Code"), "{line}");
        assert!(line.contains("handoff"), "{line}");
        assert!(line.contains("shop:handoff:00000002"), "{line}");
        assert!(line.contains("on main"), "{line}");
        let joined = Event {
            kind: "connected".to_owned(),
            ..Event::about("codex-mcp-client", Some("shop"))
        };
        let line = event(&joined, &Style::PLAIN);
        assert!(line.contains("* Codex CLI connected"), "{line}");
        assert!(line.contains("connected"), "{line}");
    }
}
