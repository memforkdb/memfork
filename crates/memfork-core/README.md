# memfork-core

The MemFork engine: an embedded, in-memory database for AI agent state with Git
semantics — fork, merge, discard, rewind. No I/O, no async.

See the [project README](https://github.com/memforkdb/memfork) for the full
picture, and `docs/DESIGN.md` for the design.

```rust
use memfork_core::{Db, Error, MergePolicy, Value};

fn main() -> Result<(), Error> {
    let db = Db::new();
    db.put("main", "plan:1", Value::new("the original plan"))?;

    // Fork before a risky step. Constant time, whatever the branch holds.
    db.fork("main", "attempt")?;
    db.put("attempt", "plan:1", Value::new("a risky rewrite"))?;

    // The parent cannot see the attempt until it is merged.
    assert_eq!(
        db.get("main", "plan:1")?.map(|e| e.value.clone()),
        Some("the original plan".into())
    );

    db.merge("attempt", "main", MergePolicy::Fail)?; // it worked
    // db.discard("attempt")?;                       // or it did not
    Ok(())
}
```

Apache-2.0.
