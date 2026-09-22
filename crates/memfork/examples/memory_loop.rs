//! A single agent's memory loop, in one process: remember, fork before a
//! risky step, keep or throw the attempt away, and read the past.
//!
//! ```sh
//! cargo run -p memfork --example memory_loop
//! ```
//!
//! This is the engine alone — the same `memfork-core` the daemon runs on, with
//! nothing written to disk. What an MCP client stores goes through the daemon
//! instead and is kept; see `examples/README.md`.

use memfork_core::{Db, MergePolicy, Value};

fn main() -> Result<(), memfork_core::Error> {
    let db = Db::new();

    // Remember things as you go: a decision and its reason.
    db.put(
        "main",
        "shop:decision:payments",
        Value::new(r#"{"choice":"hosted checkout","why":"no card data on our servers"}"#),
    )?;

    // A risky step: try a rewrite on a fork, where nothing can be seen from
    // `main` until it is merged.
    db.fork("main", "attempt")?;
    db.put(
        "attempt",
        "shop:decision:payments",
        Value::new(r#"{"choice":"own forms"}"#),
    )?;
    db.put(
        "attempt",
        "shop:note:1",
        Value::new("halfway through the rewrite"),
    )?;

    println!(
        "main still says:  {}",
        read(&db, "main", "shop:decision:payments")?
    );
    println!(
        "the attempt says: {}",
        read(&db, "attempt", "shop:decision:payments")?
    );

    // It did not work out. Discarding leaves `main` exactly as it was.
    db.discard("attempt")?;
    let names: Vec<String> = db.branches().into_iter().map(|b| b.name).collect();
    println!("branches after the discard: {names:?}");

    // Try again, and this time keep it.
    db.fork("main", "second")?;
    db.put(
        "second",
        "shop:decision:emails",
        Value::new(r#"{"choice":"queue them"}"#),
    )?;
    let merged = db.merge("second", "main", MergePolicy::Fail)?;
    println!("merged into main: {:?}", merged.changed);

    // Read the past: main as it was one commit ago, before the merge.
    let head = db
        .branches()
        .into_iter()
        .find(|b| b.name == "main")
        .map_or(0, |b| b.seq);
    let before = db.at("main", head - 1)?;
    println!("keys on main before the merge: {:?}", before.keys());
    let now: Vec<String> = db
        .list("main", "shop:", None)?
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    println!("keys on main now:              {now:?}");
    Ok(())
}

fn read(db: &Db, branch: &str, key: &str) -> Result<String, memfork_core::Error> {
    Ok(db
        .get(branch, key)?
        .map(|e| String::from_utf8_lossy(&e.value).into_owned())
        .unwrap_or_else(|| "(nothing)".to_owned()))
}
