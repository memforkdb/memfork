//! Drawing `memfork log --graph`: every branch as a tree, newest first.
//!
//! The data comes from [`crate::exec::history`], computed wherever the store
//! is — in the daemon, or in memory under `--ephemeral` — and the drawing
//! happens here, in the process attached to the terminal, so colour and glyphs
//! follow the terminal actually in front of the person.
//!
//! Each commit gets a lane. A merge opens a lane for its second parent; where
//! two lanes arrive at the same commit — a fork point — they join. Discarded
//! branches no longer have commits, so each is drawn as a stub off the commit
//! it forked from. Every mark has a word beside it: `[main]`, `merge`,
//! `fork point`, `discarded`.

use serde_json::Value as Json;

use crate::style::{palette, Glyph, Style};

struct Node {
    id: String,
    parents: Vec<String>,
    seq: u64,
    message: String,
    by: Option<String>,
    fork_point: bool,
}

fn nodes(graph: &Json) -> Vec<Node> {
    graph["commits"]
        .as_array()
        .map(|all| {
            all.iter()
                .map(|c| Node {
                    id: c["commit"].as_str().unwrap_or_default().to_owned(),
                    parents: c["parents"]
                        .as_array()
                        .map(|p| {
                            p.iter()
                                .filter_map(Json::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default(),
                    seq: c["seq"].as_u64().unwrap_or(0),
                    message: c["message"].as_str().unwrap_or("").to_owned(),
                    by: c["by"].as_str().map(str::to_owned),
                    fork_point: c["fork_point"].as_bool().unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// How a junction line joins lanes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Junction {
    /// Lanes to the right arrive at `col`.
    Join,
    /// `col` opens lanes to the right.
    Split,
}

/// The first free lane, or a new one.
fn free(lanes: &mut Vec<Option<String>>, after: Option<usize>) -> usize {
    let start = after.map_or(0, |c| c + 1);
    match (start..lanes.len()).find(|&i| lanes[i].is_none()) {
        Some(i) => i,
        None => {
            lanes.push(None);
            lanes.len() - 1
        }
    }
}

fn cell_lane(lanes: &[Option<String>], i: usize, style: &Style) -> String {
    if lanes.get(i).is_some_and(Option::is_some) {
        format!("{} ", style.dim(style.glyph(Glyph::Lane)))
    } else {
        "  ".to_owned()
    }
}

/// A line of lanes with a horizontal junction from `col` to the lanes in
/// `ends`.
fn junction(
    lanes: &[Option<String>],
    col: usize,
    ends: &[usize],
    kind: Junction,
    style: &Style,
) -> String {
    let far = ends.iter().copied().max().unwrap_or(col);
    let width = lanes.len().max(far + 1);
    let across = style.glyph(Glyph::Across);
    let end = match kind {
        Junction::Join => style.glyph(Glyph::JoinRight),
        Junction::Split => style.glyph(Glyph::SplitRight),
    };
    let mut out = String::new();
    for i in 0..width {
        if i < col || i > far {
            out.push_str(&cell_lane(lanes, i, style));
        } else if i == col {
            out.push_str(&style.dim(&format!("{}{}", style.glyph(Glyph::Tee), across)));
        } else if i == far {
            out.push_str(&style.dim(&format!("{end} ")));
        } else if ends.contains(&i) {
            out.push_str(&style.dim(&format!("{end}{across}")));
        } else if lanes[i].is_some() {
            out.push_str(&style.dim(&format!("{}{across}", style.glyph(Glyph::Cross))));
        } else {
            out.push_str(&style.dim(&format!("{across}{across}")));
        }
    }
    out.trim_end().to_owned()
}

/// Draw the graph `crate::exec::history` produced.
pub fn render(graph: &Json, style: &Style) -> Vec<String> {
    let nodes = nodes(graph);
    let heads: Vec<(String, String, bool)> = graph["branches"]
        .as_array()
        .map(|b| {
            b.iter()
                .map(|b| {
                    (
                        b["head"].as_str().unwrap_or_default().to_owned(),
                        b["name"].as_str().unwrap_or_default().to_owned(),
                        b["is_default"].as_bool().unwrap_or(false),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let discarded: Vec<(String, String, u64, Option<String>)> = graph["discarded"]
        .as_array()
        .map(|d| {
            d.iter()
                .map(|d| {
                    (
                        d["forked_at"].as_str().unwrap_or_default().to_owned(),
                        d["name"].as_str().unwrap_or_default().to_owned(),
                        d["commits"].as_u64().unwrap_or(0),
                        d["lesson"].as_str().map(str::to_owned),
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    let mut out: Vec<Row> = Vec::new();
    let mut lanes: Vec<Option<String>> = Vec::new();
    for node in &nodes {
        let mut hits: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, l)| l.as_deref() == Some(node.id.as_str()))
            .map(|(i, _)| i)
            .collect();
        let col = if hits.is_empty() {
            let c = free(&mut lanes, None);
            lanes[c] = Some(node.id.clone());
            c
        } else {
            hits.remove(0)
        };
        if !hits.is_empty() {
            out.push(Row::Text(junction(
                &lanes,
                col,
                &hits,
                Junction::Join,
                style,
            )));
            for h in &hits {
                lanes[*h] = None;
            }
            while lanes.last().is_some_and(Option::is_none) {
                lanes.pop();
            }
        }

        // Discarded attempts that forked here, as stubs above the commit,
        // run out past every other lane so none is cut.
        for (_, name, commits, lesson) in discarded.iter().filter(|(at, _, _, _)| *at == node.id) {
            let across = style.glyph(Glyph::Across);
            let mut line = String::new();
            for i in 0..lanes.len() {
                if i < col {
                    line.push_str(&cell_lane(&lanes, i, style));
                } else if i == col {
                    line.push_str(&style.dim(&format!("{}{across}", style.glyph(Glyph::Tee))));
                } else if lanes[i].is_some() {
                    line.push_str(&style.dim(&format!("{}{across}", style.glyph(Glyph::Cross))));
                } else {
                    line.push_str(&style.dim(&format!("{across}{across}")));
                }
            }
            line.push_str(&style.warn(style.glyph(Glyph::Discarded)));
            line.push(' ');
            line.push_str(&style.warn(&format!(
                "discarded {name} ({commits} commit{})",
                if *commits == 1 { "" } else { "s" }
            )));
            if let Some(lesson) = lesson {
                line.push_str(&style.dim(&format!("  lesson: {lesson}")));
            }
            out.push(Row::Text(line));
        }

        // The commit itself: lanes and labels, then — padded later, once
        // every row's width is known — its message.
        let mut head = String::new();
        for i in 0..lanes.len() {
            if i == col {
                let dot = style.glyph(Glyph::Commit);
                head.push_str(&if node.fork_point || node.parents.len() > 1 {
                    style.accent(dot)
                } else {
                    style.primary(dot)
                });
                head.push(' ');
            } else {
                head.push_str(&cell_lane(&lanes, i, style));
            }
        }
        // Every lane cell is two columns, whatever it holds.
        let lane_width = 2 * lanes.len();
        let (labels, labels_width, tail) = label(node, &heads, style);
        head.push_str(&labels);
        out.push(Row::Commit {
            head,
            width: lane_width + labels_width,
            tail,
        });

        // Its parents: the first continues this lane, any others open lanes.
        lanes[col] = node.parents.first().cloned();
        if node.parents.len() > 1 {
            let mut opened = Vec::new();
            for p in &node.parents[1..] {
                let k = free(&mut lanes, Some(col));
                lanes[k] = Some(p.clone());
                opened.push(k);
            }
            out.push(Row::Text(junction(
                &lanes,
                col,
                &opened,
                Junction::Split,
                style,
            )));
        }
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
    }

    // One column for every message, so a row with a branch badge or a
    // `merge` does not push its message out of line with the rest.
    let column = out
        .iter()
        .filter_map(|r| match r {
            Row::Commit { width, .. } => Some(*width),
            Row::Text(_) => None,
        })
        .max()
        .unwrap_or(0);
    let mut out: Vec<String> = out
        .into_iter()
        .map(|r| match r {
            Row::Text(line) => line,
            Row::Commit { head, width, tail } if tail.is_empty() => {
                let _ = width;
                head.trim_end().to_owned()
            }
            Row::Commit { head, width, tail } => {
                format!("{head}{}  {tail}", " ".repeat(column - width))
            }
        })
        .collect();

    let omitted = graph["omitted"].as_u64().unwrap_or(0);
    if omitted > 0 {
        out.push(style.dim(&format!(
            "{} {omitted} older commit{} not shown; --limit shows more",
            if style.unicode { "…" } else { "..." },
            if omitted == 1 { "" } else { "s" }
        )));
    }
    out
}

/// A line of the drawing, before messages are lined up.
enum Row {
    /// Drawn as it is: a junction or a discarded stub.
    Text(String),
    /// A commit: its lanes and labels, how many columns those take, and its
    /// message and author.
    Commit {
        head: String,
        width: usize,
        tail: String,
    },
}

/// A commit's labels — id, sequence number, branch badges, `merge`, `fork
/// point` — with the columns they take, and separately its message and
/// author, which are lined up across rows.
///
/// The width is counted on the plain text: a badge is its name in brackets
/// without colour and its name padded by a space either side with it, the
/// same width either way.
fn label(node: &Node, heads: &[(String, String, bool)], style: &Style) -> (String, usize, String) {
    let id = &node.id[..node.id.len().min(12)];
    let seq = format!("seq {}", node.seq);
    let mut styled = vec![style.dim(id), style.dim(&seq)];
    let mut plain = vec![id.to_owned(), seq];
    for (_, name, is_default) in heads.iter().filter(|(h, _, _)| *h == node.id) {
        let background = if *is_default {
            palette::NAVY
        } else {
            palette::DEEP
        };
        styled.push(style.badge(background, name));
        plain.push(format!("[{name}]"));
    }
    for (flag, word) in [
        (node.parents.len() > 1, "merge"),
        (node.fork_point, "fork point"),
    ] {
        if flag {
            styled.push(style.accent(word));
            plain.push(word.to_owned());
        }
    }
    let mut tail = Vec::new();
    if !node.message.is_empty() {
        tail.push(style.primary(&node.message));
    }
    if let Some(by) = &node.by {
        tail.push(style.dim(&format!("by {}", crate::clients::display_for_writer(by))));
    }
    let width = plain.join("  ").chars().count();
    (styled.join("  "), width, tail.join("  "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use memfork_core::{Db, MergePolicy, Value};

    /// Replace each 64-character id with a short name, so a drawing can be
    /// compared as text.
    fn anonymise(lines: &[String], names: &[(String, &str)]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                let mut l = l.clone();
                for (id, name) in names {
                    // Padded to an id's width, so columns stay where they were.
                    l = l.replace(&id[..12], &format!("{name:<12}"));
                }
                l
            })
            .collect()
    }

    #[test]
    fn forks_merges_and_discards_are_drawn_with_words() {
        let db = Db::new();
        db.put("main", "a", Value::new("1")).unwrap();
        let base = db.head("main").unwrap();
        db.fork("main", "feature").unwrap();
        db.put("feature", "f", Value::new("2")).unwrap();
        let feature = db.head("feature").unwrap();
        db.put("main", "m", Value::new("3")).unwrap();
        let main_side = db.head("main").unwrap();
        db.fork("main", "attempt").unwrap();
        db.put("attempt", "x", Value::new("4")).unwrap();
        db.discard("attempt").unwrap();
        db.merge("feature", "main", MergePolicy::Fail).unwrap();
        let merge = db.head("main").unwrap();

        let graph = crate::exec::history(&db, None);
        let genesis = db.log("main", None).unwrap().last().unwrap().id;
        let names = [
            (merge.to_hex(), "MERGE"),
            (feature.to_hex(), "FEAT"),
            (main_side.to_hex(), "MAINSIDE"),
            (base.to_hex(), "BASE"),
            (genesis.to_hex(), "GENESIS"),
        ];
        let drawn = anonymise(&render(&graph, &Style::PLAIN), &names);
        let text = drawn.join("\n");
        assert_eq!(
            drawn,
            vec![
                "* MERGE         seq 3  [main]  merge  merge feature into main (fail)".to_owned(),
                "|-\\".to_owned(),
                // The attempt forked after `put m`, and was thrown away.
                "|-+-x discarded attempt (1 commit)".to_owned(),
                "* | MAINSIDE      seq 2               put m".to_owned(),
                "| * FEAT          seq 2  [feature]    put f".to_owned(),
                "|-/".to_owned(),
                "* BASE          seq 1  fork point     put a".to_owned(),
                "* GENESIS       seq 0                 genesis".to_owned(),
            ],
            "\n{text}"
        );
    }

    #[test]
    fn every_message_starts_in_the_same_column() {
        // Badges, `merge`, `fork point`, extra lanes and a longer sequence
        // number all widen a row's labels; none may move its message.
        let db = Db::new();
        for i in 0..12 {
            db.put("main", &format!("k{i}"), Value::new("v")).unwrap();
        }
        db.fork("main", "a-rather-long-branch-name").unwrap();
        db.put("a-rather-long-branch-name", "x", Value::new("1"))
            .unwrap();
        db.put("main", "y", Value::new("2")).unwrap();
        db.merge("a-rather-long-branch-name", "main", MergePolicy::Fail)
            .unwrap();
        let drawn = render(&crate::exec::history(&db, None), &Style::PLAIN);
        let columns: Vec<usize> = drawn
            .iter()
            .filter_map(|line| {
                ["put ", "merge ", "genesis"]
                    .iter()
                    .filter_map(|m| line.rfind(m))
                    .max()
            })
            .collect();
        assert!(columns.len() >= 14, "{drawn:#?}");
        assert!(
            columns.windows(2).all(|w| w[0] == w[1]),
            "messages start at {columns:?}\n{}",
            drawn.join("\n")
        );
    }

    #[test]
    fn a_long_history_says_how_much_it_left_out() {
        let db = Db::new();
        for i in 0..10 {
            db.put("main", &format!("k{i}"), Value::new("v")).unwrap();
        }
        let drawn = render(&crate::exec::history(&db, Some(3)), &Style::PLAIN);
        assert_eq!(drawn.len(), 4);
        assert!(drawn[3].contains("8 older commits not shown"), "{drawn:?}");
    }

    #[test]
    fn plain_drawing_is_ascii_and_uncoloured() {
        let db = Db::new();
        db.put("main", "a", Value::new("1")).unwrap();
        db.fork("main", "b").unwrap();
        db.put("b", "x", Value::new("1")).unwrap();
        for line in render(&crate::exec::history(&db, None), &Style::PLAIN) {
            assert!(line.is_ascii(), "{line}");
            assert!(!line.contains('\x1b'), "{line}");
        }
    }
}
