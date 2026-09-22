//! The Brain's performance budgets, on a store of a hundred thousand
//! entries: what the daemon's side of each promise costs. Asserted in a
//! release build, which is how MemFork ships and how CI runs this suite a
//! second time; a debug build prints the figures and asserts nothing, since
//! they would say nothing about the product.
//!
//! The page's side — first paint as the browser measures it, an event
//! applied within a frame, sixty frames a second — is measured by the page
//! itself and, for the event, by the tests under Node in
//! `src/brain/page/tests`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use memfork::brain::{graph, summary, Context, Query};
use memfork::events::Events;
use memfork::shared::Shared;
use memfork_core::{Db, Value, WRITTEN_BY};

const ENTRIES: usize = 100_000;

/// The daemon's side of each promise, in a release build.
const SUMMARY_BUDGET: Duration = Duration::from_millis(200);
const ATTENTION_BUDGET: Duration = Duration::from_millis(400);
/// The graph arrives after the first paint. Building, laying out and
/// encoding a hundred thousand nodes and their edges takes about half a
/// second on a laptop; the budget leaves room for a slower runner.
const GRAPH_BUDGET: Duration = Duration::from_millis(1000);
/// The engine's ranked search scores every entry of the project for each
/// query, so its cost grows with the store: the promise of fifty
/// milliseconds holds at ten thousand entries, and a hundred thousand take
/// several times that. Both are measured and both are held.
const SEARCH_BUDGET_10K: Duration = Duration::from_millis(50);
const SEARCH_BUDGET: Duration = Duration::from_millis(800);
const LAYOUT_BUDGET: Duration = Duration::from_millis(50);

const WORDS: &[&str] = &[
    "refund",
    "cart",
    "checkout",
    "payment",
    "invoice",
    "login",
    "session",
    "token",
    "cache",
    "queue",
    "retry",
    "timeout",
    "schema",
    "migration",
    "index",
    "shard",
    "lock",
    "webhook",
];

/// A project of a hundred thousand entries: mostly decisions, with facts
/// that cite files, tasks in a plan, lessons and handoffs, as a long project
/// would have.
fn filled() -> Db {
    filled_to(ENTRIES)
}

fn filled_to(entries: usize) -> Db {
    let db = Db::new();
    let mut i = 0;
    while i < entries {
        let mut txn = db.begin("main").unwrap();
        for j in i..(i + 1000).min(entries) {
            let word = |n: usize| WORDS[n % WORDS.len()];
            let by = if j % 3 == 0 {
                "claude-code"
            } else {
                "codex-mcp-client"
            };
            let (key, value) = match j % 20 {
                0 => (
                    format!("shop:fact:f{j:06}"),
                    Value::new(format!("the {} path is in src/{}.rs", word(j), word(j / 7)))
                        .with_meta(
                            memfork::facts::SOURCES_META,
                            memfork::facts::sources_meta(&[format!("src/{}.rs", word(j / 7))]),
                        ),
                ),
                1 => (
                    format!("shop:task:t{j:06}"),
                    Value::new(format!(
                        r#"{{"title":"{} the {}","status":"open","depends_on":["t{:06}"]}}"#,
                        word(j),
                        word(j / 3),
                        (j + 19) % ENTRIES
                    )),
                ),
                2 => (
                    format!("shop:lesson:{j:08}"),
                    Value::new(format!(
                        r#"{{"lesson":"{} breaks decision:d{:06}","branch":"try-{}"}}"#,
                        word(j),
                        (j + 1) % ENTRIES,
                        word(j)
                    )),
                ),
                3 => (
                    format!("shop:handoff:{j:08}"),
                    Value::new(format!(
                        r#"{{"summary":"{} done, {} next","next":["{}"]}}"#,
                        word(j),
                        word(j + 1),
                        word(j + 1)
                    )),
                ),
                _ => (
                    format!("shop:decision:d{j:06}"),
                    Value::new(format!(
                        "decision {j}: the {} path uses {} with {} because {}; see fact:f{:06}",
                        word(j),
                        word(j / 7),
                        word(j / 49),
                        word(j * 13),
                        (j / 20) * 20
                    )),
                ),
            };
            txn.put(&key, value.with_meta(WRITTEN_BY, by)).unwrap();
        }
        txn.commit(Some(format!("batch {i}"))).unwrap();
        i += 1000;
    }
    db
}

fn context(db: Db) -> Context {
    Context {
        db,
        events: Arc::new(Events::default()),
        side: Shared::in_memory(),
        port: 0,
    }
}

/// Every figure is printed, then every one over its budget fails the test
/// together, so one slow route does not hide the others' numbers.
fn check(over: &mut Vec<String>, name: &str, took: Duration, budget: Duration) {
    println!("brain: {name} on {ENTRIES} entries: {took:?} (budget {budget:?})");
    if !cfg!(debug_assertions) && took > budget {
        over.push(format!(
            "{name} took {took:?}, over its budget of {budget:?}"
        ));
    }
}

#[test]
fn the_daemons_side_of_the_budgets_holds_on_a_hundred_thousand_entries() {
    let built = Instant::now();
    let context = context(filled());
    println!("brain: building the store took {:?}", built.elapsed());
    let query = Query::parse(Some("ns=shop"));
    let mut over = Vec::new();

    // What the first paint waits for: the summary.
    let start = Instant::now();
    let s = summary::summary(&context, &query).unwrap();
    check(&mut over, "summary", start.elapsed(), SUMMARY_BUDGET);
    assert_eq!(s["entries"], ENTRIES);

    // What needs attention, fetched after the first paint.
    let start = Instant::now();
    let a = summary::attention(&context, &query).unwrap();
    check(&mut over, "attention", start.elapsed(), ATTENTION_BUDGET);
    assert!(a["attention"].is_array());

    // The graph, built, laid out and encoded for the page.
    let start = Instant::now();
    let g = graph::build(&context.db, "main", "shop", None, &context.side, &[]).unwrap();
    let json = graph::to_json(&g).to_string();
    check(&mut over, "graph", start.elapsed(), GRAPH_BUDGET);
    assert!(g.nodes.len() >= ENTRIES);
    println!(
        "brain: graph has {} nodes, {} edges, {} bytes as JSON",
        g.nodes.len(),
        g.edges.len(),
        json.len()
    );

    // Layout alone, and the same twice.
    let start = Instant::now();
    let positions = graph::layout(&g.nodes);
    check(&mut over, "layout", start.elapsed(), LAYOUT_BUDGET);
    assert_eq!(positions, g.positions, "layout is not deterministic");

    // The engine's ranked search.
    let query = Query::parse(Some("ns=shop&q=webhook+retry"));
    let start = Instant::now();
    let hits = summary::search(&context, &query).unwrap();
    check(&mut over, "search", start.elapsed(), SEARCH_BUDGET);
    assert!(!hits["hits"].as_array().unwrap().is_empty());

    let small = self::context(filled_to(10_000));
    let start = Instant::now();
    let hits = summary::search(&small, &query).unwrap();
    let took = start.elapsed();
    println!("brain: search on 10000 entries: {took:?} (budget {SEARCH_BUDGET_10K:?})");
    if !cfg!(debug_assertions) && took > SEARCH_BUDGET_10K {
        over.push(format!("search on 10000 entries took {took:?}"));
    }
    assert!(!hits["hits"].as_array().unwrap().is_empty());

    assert!(
        over.is_empty(),
        "over budget:
  {}",
        over.join(
            "
  "
        )
    );
}
