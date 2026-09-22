# Examples

Small programs that each show one thing MemFork does, runnable from a clone
of the repository with nothing installed but Rust.

| Example | What it shows | Run |
|---|---|---|
| `memory_loop` | One agent's loop: remember, fork before a risky step, discard or merge, read the past. The engine alone, in memory. | `cargo run -p memfork --example memory_loop` |
| `handoff` | Two sessions, one memory: the first calls `memfork_handoff`, the second `memfork_resume`, through the same tools an MCP client uses. | `cargo run -p memfork --example handoff` |
| `plan_two_clients` | A plan of dependent tasks on the board, worked by two scripted clients that claim what is ready and mark it done, with no orchestrator. | `cargo run -p memfork --example plan_two_clients` |
| `event_stream` | Starts a daemon on a temporary directory, makes a few changes through it, and prints the event stream (`docs/EVENTS.md`), then stops it. | `cargo build -p memfork && cargo run -p memfork --example event_stream -- target/debug/memfork` |

The sources are in [`crates/memfork/examples/`](../crates/memfork/examples/).
Each is compiled by CI, and the three that need no daemon are run there too.

The Python package has the same in-process engine:

```python
import memfork

db = memfork.Database()
db.put("plan:1", b"the original plan")
db.fork("attempt")
db.put("plan:1", b"a risky rewrite", branch="attempt")
assert db.get("plan:1").value == b"the original plan"
db.discard("attempt")
```

What `import memfork` holds is in memory and separate from the shared store an
MCP client writes; the daemon owns that, and the command line and the MCP tools
are the ways to it. The README's *Rust* and *Python* sections say what each
surface covers.
