# A data directory written by MemFork 0.1.1

`data/` was written by the 0.1.1 release's own code, not by this checkout, and
`expected.json` is what 0.1.1 read back when it opened that directory again:
every branch, its head, every entry with its metadata, and every commit id in
its log. The compatibility test copies `data/` somewhere temporary, opens it
with the current code, and requires exactly the same answers. So a change to
the log or snapshot format, or to how commit ids are computed, fails that test
rather than stranding the memory people already have.

It holds a snapshot and a log tail after it (the store was compacted with a
retention of 4 and then written to again), three branches, a merge, a
discarded branch, a delete, an embedding, metadata, and non-ASCII text.

The files are marked binary in `.gitattributes`, so no platform rewrites them.

SHA-256, to tell at a glance that they have not changed:

```
bdd81ce0e6da1c365dbdef6d2c8142017ce031619510f613bab60cdf43f08556  data/memfork.snapshot
88bf4384698ddac000b2d2a5ab87b3f7bc8aa74ab197e9c653bb8ed77235c59c  data/memfork.wal
```

## How it was made

From a worktree of the `v0.1.1` tag, with the program below saved as
`crates/memfork/examples/fixture.rs` there (it is not part of any release):

```
git worktree add --detach ../memfork-0.1.1 v0.1.1
cd ../memfork-0.1.1
cargo run --release -p memfork --example fixture -- <output directory>
```

It writes `<output directory>/data` and `<output directory>/expected.json`;
only `memfork.snapshot` and `memfork.wal` are kept from `data`, since the lock
file is empty and belongs to whichever process holds the directory.

```rust
//! Write a data directory with MemFork 0.1.1's own code, and record what it
//! holds, for the compatibility test in later versions. Not committed.
use std::path::PathBuf;
use memfork::persist::{Options, Store};
use memfork_core::{MergePolicy, Value};
use serde_json::json;

fn main() {
    let out = PathBuf::from(std::env::args().nth(1).expect("output dir"));
    let dir = out.join("data");
    std::fs::create_dir_all(&dir).unwrap();
    let options = Options { retention: 4, ..Options::default() };
    let (db, store, _) = Store::open(&dir, options).unwrap();

    db.put("main", "project:decision:wal",
        Value::new("append-only log, checksummed").with_importance(0.9).with_meta("source", "design")).unwrap();
    db.put("main", "project:task:1", Value::new("write the fixture")).unwrap();
    db.put("main", "note:unicode", Value::new("h\u{e9}llo \u{2014} \u{2713}")).unwrap();
    db.put("main", "vec:a", Value::new("near").with_embedding(vec![1.0, 0.0, 0.5])).unwrap();
    db.fork("main", "experiment").unwrap();
    db.put("experiment", "project:idea", Value::new("try a trie")).unwrap();
    db.put("experiment", "project:decision:wal", Value::new("append-only log, checksummed").with_importance(0.9).with_meta("source", "design")).unwrap();
    db.put("main", "project:task:2", Value::new("review")).unwrap();
    db.merge("experiment", "main", MergePolicy::Fail).unwrap();
    db.fork("main", "abandoned").unwrap();
    db.put("abandoned", "project:bad-idea", Value::new("rewrite everything")).unwrap();
    db.discard("abandoned").unwrap();
    db.delete("main", "project:task:1").unwrap();
    for i in 0..6 {
        db.put("main", &format!("counter:{i}"), Value::new(format!("{i}"))).unwrap();
    }
    assert!(store.compact(&db).unwrap(), "compaction ran");
    db.put("main", "after:snapshot", Value::new("in the log tail")).unwrap();
    db.fork("main", "open-branch").unwrap();
    db.put("open-branch", "project:wip", Value::new("unfinished")).unwrap();

    drop(db);
    drop(store);
    // What 0.1.1 itself sees when it opens this directory again.
    let (db, store, recovery) = Store::open(&dir, Options { retention: 4, ..Options::default() }).unwrap();
    assert!(recovery.from_snapshot > 0 && recovery.from_wal > 0, "{recovery:?}");
    let mut branches = Vec::new();
    for b in db.branches() {
        let entries: Vec<_> = db.list(&b.name, "", None).unwrap().iter().map(|(k, e)| json!({
            "key": k,
            "value": String::from_utf8_lossy(&e.value),
            "importance": e.importance,
            "meta": e.meta,
            "embedding": e.embedding,
        })).collect();
        let log: Vec<_> = db.log(&b.name, None).unwrap().iter().map(|l| json!({
            "commit": l.id.to_hex(), "seq": l.seq, "parents": l.parents.iter().map(|p| p.to_hex()).collect::<Vec<_>>(),
        })).collect();
        branches.push(json!({
            "name": b.name, "head": b.head.to_hex(), "seq": b.seq, "entries": entries, "log": log,
        }));
    }
    let expected = json!({ "written_by": "memfork 0.1.1", "branches": branches });
    std::fs::write(out.join("expected.json"), serde_json::to_string_pretty(&expected).unwrap() + "\n").unwrap();
    drop(db);
    drop(store);
}
```
