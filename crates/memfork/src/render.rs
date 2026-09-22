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
        Command::Ls { full, .. } => listing(outcome, *full, style),
        Command::Facts { .. } => facts(&outcome.json, style),
        Command::Stats { .. } => stats(&outcome.json["stats"], style),
        Command::At {
            key: None, full, ..
        } => listing(outcome, *full, style),
        Command::Fork { .. } => lead(&outcome.text, |w| style.accent(w)),
        Command::Merge { .. } => lead(&outcome.text, |w| style.accent(w)),
        Command::Discard { .. } => lead(&outcome.text, |w| style.warn(w)),
        _ => outcome.text.clone(),
    }
}

/// `memfork facts`: each fact, whether its sources have changed, and what it
/// says.
pub fn facts(json: &Json, style: &Style) -> Vec<String> {
    let all = json["facts"].as_array().cloned().unwrap_or_default();
    if all.is_empty() {
        return vec![style.dim(&format!(
            "no facts under `{}`; record one with `memfork put <key> <value> --source <file>`",
            json["prefix"].as_str().unwrap_or("")
        ))];
    }
    let width = all
        .iter()
        .filter_map(|f| f["key"].as_str())
        .map(|k| k.chars().count())
        .max()
        .unwrap_or(0);
    all.iter()
        .map(|f| {
            let key = f["key"].as_str().unwrap_or_default();
            let state = f["fact"].as_str().unwrap_or("unverified");
            let word = match state {
                "fresh" => style.accent("fresh"),
                "stale" => style.warn("stale"),
                other => style.dim(other),
            };
            let changed = f["stale_sources"]
                .as_array()
                .map(|s| {
                    let names: Vec<&str> = s.iter().filter_map(Json::as_str).collect();
                    format!("  changed: {}", names.join(", "))
                })
                .unwrap_or_default();
            let value: String = f["value"]
                .as_str()
                .unwrap_or("")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(80)
                .collect();
            format!(
                "{}{}  {word:<5}  {}{}",
                style.primary(key),
                " ".repeat(width - key.chars().count()),
                value,
                style.warn(&changed)
            )
        })
        .collect()
}

/// `memfork stats`: what was served and saved, in bytes and approximate
/// tokens — never anything more precise than it is.
pub fn stats(json: &Json, style: &Style) -> Vec<String> {
    let block = |c: &Json, indent: &str| -> Vec<String> {
        let n = |f: &str| c[f].as_u64().unwrap_or(0);
        let mut lines = vec![format!(
            "{indent}briefings  {} served, {} bytes (about {} tokens), against {} bytes of memory",
            n("briefings"),
            n("briefing_bytes"),
            n("briefing_bytes").div_ceil(4),
            n("memory_bytes")
        )];
        lines.push(format!(
            "{indent}lessons    {} recorded, {} served in briefings",
            n("lessons_recorded"),
            n("lessons_served")
        ));
        lines.push(format!(
            "{indent}facts      {} fresh, {} stale, {} unverified when served",
            n("facts_fresh"),
            n("facts_stale"),
            n("facts_unverified")
        ));
        lines.push(format!(
            "{indent}tasks      {} claimed, {} claims refused because another had it",
            n("claims"),
            n("claim_conflicts")
        ));
        lines.push(format!("{indent}finds      {}", n("finds")));
        lines
    };
    let mut out = vec![style.strong(crate::style::palette::PRIMARY, "all projects")];
    out.extend(block(&json["total"], "  "));
    if let Some(projects) = json["projects"].as_object() {
        for (project, clients) in projects {
            for (client, counters) in clients.as_object().into_iter().flatten() {
                out.push(String::new());
                out.push(format!(
                    "{}  {}",
                    style.strong(crate::style::palette::PRIMARY, project),
                    style.dim(&crate::clients::display_for_writer(client))
                ));
                out.extend(block(counters, "  "));
            }
        }
    }
    out.push(String::new());
    out.push(style.dim("tokens are an estimate: bytes divided by 4, rounded up"));
    out
}

/// `memfork ls`, and `memfork at` listing a branch.
///
/// Into a pipe or a file, exactly the lines the command produced: whole
/// values, whatever they hold, so a script sees every byte. On a terminal,
/// each value is put on one line and cut to fit the width, with a marker
/// where it was cut and a note saying `--full` shows it whole — three stored
/// decisions should not fill a screen with JSON. An empty listing says so on
/// a terminal, rather than printing nothing.
pub fn listing(outcome: &Outcome, full: bool, style: &Style) -> Vec<String> {
    let Some(columns) = style.columns else {
        return outcome.text.clone();
    };
    let keys = outcome.json["keys"].as_array().cloned().unwrap_or_default();
    if keys.is_empty() {
        let branch = outcome.json["branch"].as_str().unwrap_or("main");
        let within = outcome.json["prefix"]
            .as_str()
            .filter(|p| !p.is_empty())
            .map(|p| format!(" starting with `{p}`"))
            .unwrap_or_default();
        return vec![style.dim(&format!("no keys{within} on {branch}"))];
    }
    if full {
        return outcome.text.clone();
    }
    let width = keys
        .iter()
        .filter_map(|k| k["key"].as_str())
        .map(|k| k.chars().count())
        .max()
        .unwrap_or(0);
    // Whatever room the keys leave, but never so little that nothing of the
    // value shows; a very narrow terminal wraps rather than hides.
    let room = columns.saturating_sub(width + 2).max(16);
    let (marker, marker_len) = if style.unicode {
        ("…", 1)
    } else {
        ("...", 3)
    };
    let mut cut = 0usize;
    let mut lines: Vec<String> = keys
        .iter()
        .map(|k| {
            let key = k["key"].as_str().unwrap_or_default();
            let value = k["value"].as_str().unwrap_or_default();
            // One line per key, however many the value has.
            let flat = value.split_whitespace().collect::<Vec<_>>().join(" ");
            let shown = if flat.chars().count() > room {
                cut += 1;
                let kept: String = flat.chars().take(room - marker_len).collect();
                format!("{kept}{}", style.dim(marker))
            } else {
                flat
            };
            let pad = width - key.chars().count();
            format!("{}{}  {shown}", style.primary(key), " ".repeat(pad))
        })
        .collect();
    if cut > 0 {
        lines.push(style.dim(&format!(
            "{cut} value{} cut to fit the terminal; --full shows them whole",
            if cut == 1 { "" } else { "s" }
        )));
    }
    lines
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
                "fork" | "merge" | "handoff" | "resume" | "claim" | "done" | "lesson" => {
                    style.accent(op)
                }
                "discard" | "delete" | "del" | "release" => style.warn(op),
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
            if let Some(detail) = &e.detail {
                parts.push(match detail.as_str() {
                    "fresh" => style.accent(detail),
                    "stale" => style.warn(detail),
                    _ => style.dim(detail),
                });
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
                columns: None,
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

    fn outcome(entries: &[(&str, &str)]) -> Outcome {
        let db = memfork_core::Db::new();
        for (k, v) in entries {
            db.put("main", k, memfork_core::Value::new(v.to_string()))
                .unwrap();
        }
        crate::exec::execute(
            &db,
            "main",
            &Command::Ls {
                prefix: String::new(),
                limit: None,
                full: false,
            },
        )
        .unwrap()
    }

    fn terminal(columns: usize) -> Style {
        Style {
            colour: false,
            unicode: true,
            columns: Some(columns),
        }
    }

    const DECISION: &str = "{\n  \"decision\": \"store orders in one table per tenant\",\n  \"reason\": \"tenants never share rows, and a per-tenant table makes deleting one a single statement\"\n}";

    #[test]
    fn on_a_terminal_values_are_cut_to_fit_and_say_so() {
        let out = outcome(&[("shop:decision:orders", DECISION), ("shop:owner", "ada")]);
        let lines = listing(&out, false, &terminal(60));
        assert_eq!(lines.len(), 3, "{lines:#?}");
        for line in &lines[..2] {
            assert!(
                line.chars().count() <= 60,
                "{line:?} is wider than the terminal"
            );
            assert!(!line.contains('\n'));
        }
        assert!(
            lines[0].starts_with("shop:decision:orders  {"),
            "{lines:#?}"
        );
        assert!(lines[0].ends_with('…'), "{lines:#?}");
        assert_eq!(lines[1], "shop:owner            ada");
        assert!(lines[2].contains("1 value cut to fit the terminal; --full shows them whole"));

        let ascii = listing(
            &out,
            false,
            &Style {
                unicode: false,
                ..terminal(60)
            },
        );
        assert!(ascii[0].ends_with("..."), "{ascii:#?}");
    }

    #[test]
    fn full_prints_every_value_whole_even_on_a_terminal() {
        let out = outcome(&[("shop:decision:orders", DECISION)]);
        assert_eq!(listing(&out, true, &terminal(60)), out.text);
        assert!(out.text[0].contains("per-tenant table makes deleting one a single statement"));
    }

    #[test]
    fn into_a_pipe_values_are_whole_exactly_as_the_command_produced() {
        let out = outcome(&[("shop:decision:orders", DECISION), ("shop:owner", "ada")]);
        assert_eq!(listing(&out, false, &Style::PLAIN), out.text);
        // Newlines and all.
        assert!(out.text[0].contains('\n'));
    }

    #[test]
    fn an_empty_listing_says_so_on_a_terminal_and_prints_nothing_into_a_pipe() {
        let out = outcome(&[]);
        assert_eq!(listing(&out, false, &terminal(80)), vec!["no keys on main"]);
        assert!(listing(&out, false, &Style::PLAIN).is_empty());
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
