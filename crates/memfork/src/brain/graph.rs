//! The memory graph: entries and agents as nodes, the relations the engine
//! already knows as edges, and a layout that is a pure function of the
//! store.
//!
//! Nothing here is inferred. A fact names its source files; a decision
//! cites a fact by key; a lesson names the task it came from and any
//! decision it mentions by key; a task names what it depends on; a handoff's
//! `next` list names tasks; a briefing carried entries and went to an agent;
//! an agent holds a task and wrote entries. That is the whole list.
//!
//! Layout is decided here, not on the page, so the same store gives the same
//! picture on every machine and a test can say so without a browser. Columns
//! run left to right in a fixed order. Within a column a node's height is a
//! hash of its id, so adding a node never moves another; a column with few
//! nodes is spaced evenly instead, in key order, which is the same picture
//! everywhere too.

use std::collections::{BTreeMap, BTreeSet};

use memfork_core::{Db, Entry, WRITTEN_BY};
use serde_json::{json, Value as Json};

use crate::events::Connected;
use crate::shared::Shared;

/// The columns, left to right.
pub const COLUMNS: [&str; 7] = [
    "agents",
    "briefings",
    "decisions · handoffs",
    "lessons",
    "plan",
    "facts",
    "files",
];

/// A column holding this many nodes or fewer spaces them evenly, in id
/// order. Above it, every node's height is a hash of its id.
pub const FEW: usize = 12;

/// The height of a column, in the units a node's `y` is given in.
pub const HEIGHT: u32 = 65_535;

/// Longest label drawn on the canvas.
const MAX_LABEL: usize = 60;

/// The kinds of node, in the order their counts are reported.
pub const KINDS: [&str; 9] = [
    "agent", "brief", "decision", "handoff", "lesson", "task", "fact", "entry", "file",
];

/// Which column a kind lives in.
pub fn column(kind: &str) -> u8 {
    match kind {
        "agent" => 0,
        "brief" => 1,
        "decision" | "handoff" => 2,
        "lesson" => 3,
        "task" => 4,
        "file" => 6,
        _ => 5,
    }
}

/// One node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// The entry's key, `agent:<name>`, `file:<path>` or `brief:<order>`.
    pub id: String,
    /// One of [`KINDS`].
    pub kind: &'static str,
    /// What is drawn beside it.
    pub label: String,
    /// The branch sequence number it has existed since; zero when unknown.
    pub seq: u64,
    /// A word for its state: `stale` for a fact, `open`, `claimed` or `done`
    /// for a task, `connected` or `away` for an agent, `waiting` or `picked
    /// up` for a handoff.
    pub state: Option<&'static str>,
    /// Who wrote it, as a person knows the client.
    pub who: Option<String>,
}

/// One relation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edge {
    /// Index into the graph's nodes.
    pub from: usize,
    /// Index into the graph's nodes.
    pub to: usize,
    /// `source of`, `cited by`, `about`, `for`, `depends on`, `next`, `in`,
    /// `served to`, `holds` or `wrote`.
    pub kind: &'static str,
}

/// The graph, laid out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Graph {
    /// The branch's sequence number the graph was built from.
    pub seq: u64,
    /// The past point it shows, if it is not the present.
    pub at: Option<u64>,
    /// Nodes, in a fixed order: by column, then by id.
    pub nodes: Vec<Node>,
    /// Each node's column and height.
    pub positions: Vec<(u8, u16)>,
    /// Edges, sorted, without duplicates.
    pub edges: Vec<Edge>,
}

/// The 64-bit FNV-1a hash of a string: small, deterministic, and enough to
/// scatter ids over a column.
pub fn fnv1a64(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Where every node goes: the column its kind lives in, and a height that
/// depends on nothing but the ids in that column.
pub fn layout(nodes: &[Node]) -> Vec<(u8, u16)> {
    let mut per_column: BTreeMap<u8, Vec<usize>> = BTreeMap::new();
    for (i, node) in nodes.iter().enumerate() {
        per_column.entry(column(node.kind)).or_default().push(i);
    }
    let mut out = vec![(0u8, 0u16); nodes.len()];
    for (col, members) in per_column {
        let mut ordered = members.clone();
        ordered.sort_by(|a, b| nodes[*a].id.cmp(&nodes[*b].id));
        let n = ordered.len();
        for (rank, i) in ordered.into_iter().enumerate() {
            let y = if n <= FEW {
                // Evenly spaced, centred in the column: rank + 1/2 out of n.
                ((2 * rank as u64 + 1) * u64::from(HEIGHT) / (2 * n as u64)) as u16
            } else {
                (fnv1a64(&nodes[i].id) >> 48) as u16
            };
            out[i] = (col, y);
        }
    }
    out
}

/// Cut a label to what fits beside a node.
fn label(text: &str) -> String {
    let text = text.trim().lines().next().unwrap_or_default().trim();
    let mut out: String = text.chars().take(MAX_LABEL).collect();
    if text.chars().count() > MAX_LABEL {
        out.push('…');
    }
    out
}

/// The text of an entry, whatever it holds.
fn text_of(entry: &Entry) -> String {
    String::from_utf8_lossy(&entry.value).into_owned()
}

/// The key family and the rest of a key under `ns`.
fn split<'a>(key: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    key.strip_prefix(prefix)?.split_once(':')
}

/// The keys of `family` that `text` refers to: written in full, or as
/// `<family>:<rest>`. Found by scanning for the family's name, so the cost
/// is the text's length and not the number of keys.
fn references(text: &str, family: &str, ns_prefix: &str, known: &BTreeSet<String>) -> Vec<String> {
    let marker = format!("{family}:");
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find(&marker) {
        let start = from + at + marker.len();
        let rest: String = text[start..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
            .collect();
        let rest = rest.trim_end_matches(':');
        if !rest.is_empty() {
            let key = format!("{ns_prefix}{marker}{rest}");
            if known.contains(&key) && !found.contains(&key) {
                found.push(key);
            }
        }
        from = start;
    }
    found
}

/// Build the graph of `ns` on `branch`, now or as it was at `at`.
pub fn build(
    db: &Db,
    branch: &str,
    ns: &str,
    at: Option<u64>,
    side: &Shared,
    connected: &[Connected],
) -> Result<Graph, memfork_core::Error> {
    let view = match at {
        Some(seq) => db.at(branch, seq)?,
        None => db.read(branch)?,
    };
    let prefix = format!("{ns}{}", crate::namespace::SEPARATOR);
    let entries = view.list(&prefix, None);
    let present = at.is_none();

    let mut nodes: Vec<Node> = Vec::with_capacity(entries.len());
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    let mut edges: Vec<(String, String, &'static str)> = Vec::new();
    let mut agents: BTreeMap<String, u64> = BTreeMap::new();

    let add = |nodes: &mut Vec<Node>, index: &mut BTreeMap<String, usize>, node: Node| {
        if let Some(&i) = index.get(&node.id) {
            if node.seq < nodes[i].seq {
                nodes[i].seq = node.seq;
            }
            return i;
        }
        index.insert(node.id.clone(), nodes.len());
        nodes.push(node);
        nodes.len() - 1
    };

    let stale: BTreeSet<String> = side.sidecar.stale_facts(ns).into_iter().collect();
    let known: BTreeSet<String> = entries.iter().map(|(k, _)| k.clone()).collect();
    let task_prefix = crate::namespace::prefix(ns, "task");

    // Pass one: every entry is a node.
    for (key, entry) in &entries {
        let Some((family, rest)) = split(key, &prefix) else {
            continue;
        };
        let text = text_of(entry);
        let parsed: Json = serde_json::from_str(&text).unwrap_or(Json::Null);
        let who = entry
            .meta
            .get(WRITTEN_BY)
            .map(|w| crate::clients::display_for_writer(w));
        let is_fact = crate::facts::sources_of(&entry.meta).is_some();
        let (kind, label, state): (&'static str, String, Option<&'static str>) = match family {
            "decision" => ("decision", label(rest), None),
            "handoff" => (
                "handoff",
                format!("handoff {}", rest.trim_start_matches('0')),
                None,
            ),
            "lesson" => (
                "lesson",
                label(parsed["lesson"].as_str().unwrap_or(&text)),
                None,
            ),
            "task" => {
                let holder = present.then(|| side.board.holder(key)).flatten();
                let status = parsed["status"].as_str().unwrap_or("open");
                let state = if status == "done" {
                    "done"
                } else if holder.is_some() {
                    "claimed"
                } else {
                    "open"
                };
                if let Some((client, _)) = holder {
                    let agent = format!("agent:{}", crate::clients::display_for_writer(&client));
                    edges.push((agent, key.clone(), "holds"));
                }
                (
                    "task",
                    label(parsed["title"].as_str().unwrap_or(rest)),
                    Some(state),
                )
            }
            _ if is_fact => ("fact", label(rest), stale.contains(key).then_some("stale")),
            _ => ("entry", label(rest), None),
        };
        let seq = entry.created_seq;
        if let Some(w) = &who {
            let id = format!("agent:{w}");
            let first = agents.entry(id.clone()).or_insert(seq);
            *first = (*first).min(seq);
            edges.push((id, key.clone(), "wrote"));
        }
        add(
            &mut nodes,
            &mut index,
            Node {
                id: key.clone(),
                kind,
                label,
                seq,
                state,
                who,
            },
        );
    }

    // Pass two: what the entries say about each other.
    for (key, entry) in &entries {
        let Some((family, _)) = split(key, &prefix) else {
            continue;
        };
        let text = text_of(entry);
        let parsed: Json = serde_json::from_str(&text).unwrap_or(Json::Null);
        let seq = entry.created_seq;
        if let Some(sources) = crate::facts::sources_of(&entry.meta) {
            for path in sources {
                let id = format!("file:{path}");
                add(
                    &mut nodes,
                    &mut index,
                    Node {
                        id: id.clone(),
                        kind: "file",
                        label: path.clone(),
                        seq,
                        state: None,
                        who: None,
                    },
                );
                edges.push((id, key.clone(), "source of"));
            }
        }
        match family {
            "decision" => {
                let meta: String = entry.meta.values().cloned().collect::<Vec<_>>().join(" ");
                for fact in references(&format!("{text} {meta}"), "fact", &prefix, &known) {
                    edges.push((fact, key.clone(), "cited by"));
                }
            }
            "lesson" => {
                for decision in references(&text, "decision", &prefix, &known) {
                    edges.push((key.clone(), decision, "about"));
                }
                if let Some(task) = parsed["task"].as_str() {
                    let task_key = crate::board::task_key(ns, task);
                    if known.contains(&task_key) {
                        edges.push((key.clone(), task_key, "for"));
                    }
                }
            }
            "task" => {
                for dep in parsed["depends_on"].as_array().into_iter().flatten() {
                    if let Some(dep) = dep.as_str() {
                        let dep_key = crate::board::task_key(ns, dep);
                        if known.contains(&dep_key) {
                            edges.push((dep_key, key.clone(), "depends on"));
                        }
                    }
                }
            }
            "handoff" => {
                let next: Vec<String> = parsed["next"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Json::as_str)
                    .map(str::to_lowercase)
                    .collect();
                if !next.is_empty() {
                    for (task_key, task) in
                        entries.iter().filter(|(k, _)| k.starts_with(&task_prefix))
                    {
                        let id = task_key[task_prefix.len()..].to_lowercase();
                        let title = serde_json::from_slice::<Json>(&task.value)
                            .ok()
                            .and_then(|t| t["title"].as_str().map(str::to_lowercase))
                            .unwrap_or_default();
                        if next
                            .iter()
                            .any(|n| n.contains(&id) || (!title.is_empty() && n.contains(&title)))
                        {
                            edges.push((key.clone(), task_key.clone(), "next"));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Briefings served, from beside the store.
    for brief in side.sidecar.briefings(ns) {
        if brief.branch != branch || at.is_some_and(|seq| brief.seq > seq) {
            continue;
        }
        let id = format!("brief:{:012}", brief.order);
        let to = format!("agent:{}", crate::clients::display_for_writer(&brief.to));
        agents.entry(to.clone()).or_insert(brief.seq);
        add(
            &mut nodes,
            &mut index,
            Node {
                id: id.clone(),
                kind: "brief",
                label: format!(
                    "briefing · {} B · to {}",
                    brief.bytes,
                    crate::clients::display_for_writer(&brief.to)
                ),
                seq: brief.seq,
                state: None,
                who: Some("MemFork".to_owned()),
            },
        );
        for key in &brief.keys {
            if known.contains(key) {
                edges.push((key.clone(), id.clone(), "in"));
            }
        }
        edges.push((id, to, "served to"));
    }

    // Agents: everyone who wrote, held or was served, and everyone here now.
    if present {
        for c in connected {
            let id = format!("agent:{}", crate::clients::display_for_writer(&c.client));
            agents.entry(id).or_insert(0);
        }
    }
    let here: BTreeSet<String> = connected
        .iter()
        .map(|c| format!("agent:{}", crate::clients::display_for_writer(&c.client)))
        .collect();
    for (id, seq) in agents {
        let name = id.trim_start_matches("agent:").to_owned();
        let state = if present && here.contains(&id) {
            "connected"
        } else {
            "away"
        };
        add(
            &mut nodes,
            &mut index,
            Node {
                id,
                kind: "agent",
                label: name,
                seq,
                state: Some(state),
                who: None,
            },
        );
    }
    for (from, key, kind) in &edges {
        if kind == &"holds" || kind == &"wrote" {
            if let Some(&i) = index.get(from) {
                if let Some(&j) = index.get(key) {
                    nodes[i].seq = nodes[i].seq.min(nodes[j].seq);
                }
            }
        }
    }

    // A fixed order: by column, then by id; the same store, the same list.
    let mut order: Vec<usize> = (0..nodes.len()).collect();
    order.sort_by(|a, b| {
        column(nodes[*a].kind)
            .cmp(&column(nodes[*b].kind))
            .then_with(|| nodes[*a].id.cmp(&nodes[*b].id))
    });
    let mut renumber = vec![0usize; nodes.len()];
    for (new, old) in order.iter().enumerate() {
        renumber[*old] = new;
    }
    let nodes: Vec<Node> = order.iter().map(|i| nodes[*i].clone()).collect();
    let mut resolved: Vec<Edge> = edges
        .iter()
        .filter_map(|(from, to, kind)| {
            Some(Edge {
                from: renumber[*index.get(from)?],
                to: renumber[*index.get(to)?],
                kind,
            })
        })
        .collect();
    resolved.sort();
    resolved.dedup();
    let positions = layout(&nodes);

    Ok(Graph {
        seq: view.seq(),
        at,
        nodes,
        positions,
        edges: resolved,
    })
}

/// The graph as the page receives it: arrays rather than objects, since a
/// large store has a hundred thousand nodes and every byte is parsed.
pub fn to_json(graph: &Graph) -> Json {
    let mut counts: BTreeMap<&str, usize> = KINDS.iter().map(|k| (*k, 0)).collect();
    for node in &graph.nodes {
        *counts.entry(node.kind).or_insert(0) += 1;
    }
    json!({
        "seq": graph.seq,
        "at": graph.at,
        "columns": COLUMNS,
        "few": FEW,
        "height": HEIGHT,
        "nodes": graph.nodes.iter().zip(&graph.positions).map(|(n, (col, y))| json!([
            n.id, n.kind, n.label, col, y, n.seq, n.state, n.who,
        ])).collect::<Vec<_>>(),
        "edges": graph.edges.iter().map(|e| json!([e.from, e.to, e.kind])).collect::<Vec<_>>(),
        "counts": counts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use memfork_core::Value;

    fn shop() -> Db {
        let db = Db::new();
        let put = |key: &str, value: Value| {
            db.put("main", key, value).unwrap();
        };
        put(
            "shop:fact:auth-entry",
            Value::new("auth lives in src/auth, entry login.rs")
                .with_meta(
                    crate::facts::SOURCES_META,
                    crate::facts::sources_meta(&["src/auth/login.rs".to_owned()]),
                )
                .with_meta(WRITTEN_BY, "claude-code"),
        );
        put(
            "shop:decision:payments",
            Value::new("hosted checkout, see fact:auth-entry").with_meta(WRITTEN_BY, "claude-code"),
        );
        put(
            "shop:task:schema",
            Value::new(r#"{"title":"design the order schema","status":"done"}"#),
        );
        put(
            "shop:task:checkout",
            Value::new(r#"{"title":"checkout page","status":"open","depends_on":["schema"]}"#),
        );
        put(
            "shop:lesson:00000001",
            Value::new(r#"{"lesson":"cards break decision:payments","branch":"try-refunds"}"#)
                .with_meta(WRITTEN_BY, "codex-mcp-client"),
        );
        put(
            "shop:handoff:00000001",
            Value::new(r#"{"summary":"checkout works","next":["do the checkout page"]}"#)
                .with_meta(WRITTEN_BY, "claude-code"),
        );
        db
    }

    fn edge_kinds(graph: &Graph) -> Vec<(String, String, &'static str)> {
        graph
            .edges
            .iter()
            .map(|e| {
                (
                    graph.nodes[e.from].id.clone(),
                    graph.nodes[e.to].id.clone(),
                    e.kind,
                )
            })
            .collect()
    }

    #[test]
    fn the_relations_are_the_ones_the_engine_knows() {
        let db = shop();
        let side = Shared::in_memory();
        let graph = build(&db, "main", "shop", None, &side, &[]).unwrap();
        let edges = edge_kinds(&graph);
        let has =
            |a: &str, b: &str, k: &str| edges.iter().any(|(x, y, z)| x == a && y == b && *z == k);
        assert!(has(
            "file:src/auth/login.rs",
            "shop:fact:auth-entry",
            "source of"
        ));
        assert!(has(
            "shop:fact:auth-entry",
            "shop:decision:payments",
            "cited by"
        ));
        assert!(has(
            "shop:lesson:00000001",
            "shop:decision:payments",
            "about"
        ));
        assert!(has("shop:task:schema", "shop:task:checkout", "depends on"));
        assert!(has("shop:handoff:00000001", "shop:task:checkout", "next"));
        assert!(has("agent:Claude Code", "shop:decision:payments", "wrote"));
        assert!(has("agent:Codex CLI", "shop:lesson:00000001", "wrote"));
        let kinds: Vec<&str> = graph.nodes.iter().map(|n| n.kind).collect();
        assert!(kinds.contains(&"agent") && kinds.contains(&"file") && kinds.contains(&"task"));
        // Columns are in order along the node list.
        let cols: Vec<u8> = graph.positions.iter().map(|(c, _)| *c).collect();
        assert!(cols.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn the_same_store_gives_the_same_graph() {
        let side = Shared::in_memory();
        let a = build(&shop(), "main", "shop", None, &side, &[]).unwrap();
        let b = build(&shop(), "main", "shop", None, &side, &[]).unwrap();
        assert_eq!(a, b);
        assert_eq!(to_json(&a), to_json(&b));
    }

    #[test]
    fn few_nodes_are_spaced_evenly_and_many_are_hashed_and_stable() {
        let mut nodes: Vec<Node> = (0..3)
            .map(|i| Node {
                id: format!("shop:decision:d{i}"),
                kind: "decision",
                label: String::new(),
                seq: 0,
                state: None,
                who: None,
            })
            .collect();
        let few = layout(&nodes);
        assert_eq!(
            few.iter().map(|(_, y)| *y).collect::<Vec<_>>(),
            vec![10922, 32767, 54612]
        );

        for i in 3..40 {
            nodes.push(Node {
                id: format!("shop:decision:d{i}"),
                kind: "decision",
                label: String::new(),
                seq: 0,
                state: None,
                who: None,
            });
        }
        let many = layout(&nodes);
        nodes.push(Node {
            id: "shop:decision:one-more".to_owned(),
            kind: "decision",
            label: String::new(),
            seq: 0,
            state: None,
            who: None,
        });
        let more = layout(&nodes);
        assert_eq!(
            &more[..many.len()],
            &many[..],
            "adding a node moved another"
        );
        assert_eq!(
            more[many.len()].1,
            (fnv1a64("shop:decision:one-more") >> 48) as u16
        );
        assert_eq!(fnv1a64(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64("a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn a_past_view_shows_what_was_there_then() {
        let db = shop();
        let side = Shared::in_memory();
        let now = build(&db, "main", "shop", None, &side, &[]).unwrap();
        let then = build(&db, "main", "shop", Some(2), &side, &[]).unwrap();
        assert!(then.nodes.len() < now.nodes.len());
        assert_eq!(then.at, Some(2));
        assert!(then.nodes.iter().all(|n| n.seq <= 2));
    }

    #[test]
    fn references_are_found_by_key_or_shorthand() {
        let known: BTreeSet<String> = ["shop:fact:a-b", "shop:fact:c"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let found = references(
            "see shop:fact:a-b and fact:c, not fact:zzz.",
            "fact",
            "shop:",
            &known,
        );
        assert_eq!(
            found,
            vec!["shop:fact:a-b".to_owned(), "shop:fact:c".to_owned()]
        );
    }
}
