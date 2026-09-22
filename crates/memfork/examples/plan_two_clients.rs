//! A plan worked by two scripted clients: tasks with dependencies on the
//! board, each client claiming what is ready and marking it done, with no
//! orchestrator between them.
//!
//! ```sh
//! cargo run -p memfork --example plan_two_clients
//! ```
//!
//! Both sessions share one board (leases, statistics) the way two proxies to
//! one daemon do, so a task one of them holds is one the other is told about.

use std::sync::Arc;

use memfork::shared::Shared;
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

/// Take the first ready task, do it, and mark it done. Returns what was done.
fn work_one(who: &str, session: &Session) -> Option<String> {
    let ready = call(
        session,
        "memfork_task",
        json!({ "action": "list", "status": "ready" }),
    );
    let id = ready["tasks"]
        .as_array()?
        .first()?
        .get("id")?
        .as_str()?
        .to_owned();
    let claimed = call(
        session,
        "memfork_task",
        json!({ "action": "claim", "id": id }),
    );
    if claimed["claimed"] == json!(false) {
        println!("{who}: {id} is held by {}", claimed["held_by"]);
        return None;
    }
    // ... the work itself would happen here ...
    call(
        session,
        "memfork_task",
        json!({ "action": "done", "id": id }),
    );
    println!("{who}: did {id}");
    Some(id)
}

fn main() {
    let db = Db::new();
    let shared: Arc<Shared> = Shared::in_memory();

    let a = Session::new(db.clone()).sharing(shared.clone());
    a.set_writer("client-a");
    let b = Session::new(db).sharing(shared);
    b.set_writer("client-b");

    // One client writes the plan: what depends on what.
    let planned = call(
        &a,
        "memfork_task",
        json!({ "action": "plan", "tasks": [
            { "id": "schema", "title": "add the refunds table" },
            { "id": "api", "title": "refunds endpoint", "depends_on": ["schema"] },
            { "id": "docs", "title": "document refunds", "depends_on": ["api"] },
            { "id": "tests", "title": "tests for refunds", "depends_on": ["api"] },
        ]}),
    );
    println!("plan written; ready now: {}", planned["ready"]);

    // Both pull work from the same board until nothing is left. Whoever asks
    // first gets a task; the other is told who holds it and moves on.
    for round in 1..=6 {
        let did_a = work_one("client-a", &a);
        let did_b = work_one("client-b", &b);
        let left = call(
            &a,
            "memfork_task",
            json!({ "action": "list", "status": "unfinished" }),
        );
        let unfinished = left["tasks"].as_array().map_or(0, Vec::len);
        println!("round {round}: {unfinished} unfinished");
        if unfinished == 0 {
            break;
        }
        if did_a.is_none() && did_b.is_none() {
            println!("nothing ready this round");
        }
    }

    let done = call(
        &a,
        "memfork_task",
        json!({ "action": "list", "status": "done" }),
    );
    let ids: Vec<&str> = done["tasks"]
        .as_array()
        .map(|t| t.iter().filter_map(|x| x["id"].as_str()).collect())
        .unwrap_or_default();
    println!("done, in order of the plan's dependencies: {ids:?}");
}
