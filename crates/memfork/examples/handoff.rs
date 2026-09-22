//! Two sessions, one memory: the first hands work off, the second resumes
//! from it — through the same tools an MCP client calls, in one process.
//!
//! ```sh
//! cargo run -p memfork --example handoff
//! ```
//!
//! The two sessions here stand for two tool sessions — the same tool on two
//! days, or two different tools — sharing one store. In production they
//! would be two `memfork mcp` proxies to one daemon; the tool surface is
//! identical.

use memfork::tools::dispatch::Session;
use memfork_core::Db;
use serde_json::{json, Value as Json};

fn call(session: &Session, tool: &str, args: Json) -> Json {
    let Json::Object(args) = args else {
        return Json::Null;
    };
    match session.call(tool, &args) {
        Ok(answer) => answer,
        Err(e) => json!({ "error": e.to_string() }),
    }
}

fn main() {
    let db = Db::new();

    // Session one: an agent works, records what it decided, and stops.
    let first = Session::new(db.clone());
    first.set_writer("first-tool");
    let _ = call(&first, "memfork_checkout", json!({ "name": "main" }));
    call(
        &first,
        "memfork_put",
        json!({
            "key": "shop:decision:payments",
            "value": r#"{"choice":"hosted checkout","why":"no card data on our servers"}"#,
        }),
    );
    let handoff = call(
        &first,
        "memfork_handoff",
        json!({
            "summary": "Checkout works end to end; refunds are not started.",
            "done": ["hosted checkout", "order emails"],
            "next": ["refunds", "tax for EU orders"],
            "blockers": ["need a sandbox account for refunds"],
        }),
    );
    println!("first session handed off as {}", handoff["key"]);

    // Session two: another agent, another day, another vendor. It resumes
    // and gets the briefing: the latest handoff first, then decisions.
    let second = Session::new(db);
    second.set_writer("second-tool");
    let briefing = call(&second, "memfork_resume", json!({ "task": "refunds" }));
    let latest = &briefing["latest_handoff"];
    println!("resumed from a handoff by {}", latest["by"]);
    println!("  summary: {}", latest["summary"]);
    println!("  next:    {}", latest["next"]);
    println!("  blocked: {}", latest["blockers"]);
    let budget = &briefing["budget"];
    println!(
        "briefing size: {} bytes, about {} tokens ({})",
        budget["bytes"], budget["approx_tokens"], budget["estimate"]
    );
}
