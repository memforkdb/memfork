<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/memfork-logo-dark.svg">
  <img src="docs/assets/memfork-logo.svg" alt="MemFork" width="420">
</picture>

# MemFork

**The agentic in-memory database**

[![CI](https://github.com/memforkdb/memfork/actions/workflows/ci.yml/badge.svg)](https://github.com/memforkdb/memfork/actions/workflows/ci.yml)
[![Licence](https://img.shields.io/badge/licence-Apache--2.0-blue.svg)](LICENSE)
[![crates.io](https://img.shields.io/crates/v/memfork.svg)](https://crates.io/crates/memfork)
[![docs.rs](https://img.shields.io/docsrs/memfork-core)](https://docs.rs/memfork-core)
[![PyPI](https://img.shields.io/pypi/v/memfork.svg)](https://pypi.org/project/memfork/)

</div>

Agents work by trying things. MemFork is memory that works the same way: your
agent can **fork** everything it remembers before a risky step, **merge** the
fork if the attempt worked, **discard** it if it did not, and **rewind** to how
things were at any earlier point. Forking costs the same whether memory holds
ten things or ten million, so an agent can branch freely instead of being
careful. It runs in your own process or as a small local server, keeps what it
stores, and is Apache-2.0.

## Install

**macOS and Linux**

```sh
curl -fsSL https://github.com/memforkdb/memfork/releases/latest/download/install.sh | sh
```

**Windows**

```powershell
irm https://github.com/memforkdb/memfork/releases/latest/download/install.ps1 | iex
```

**With Python**

```sh
pip install memfork      # or: uv tool install memfork
```

One file goes into a directory you own — `~/.memfork/bin` or
`%LOCALAPPDATA%\Programs\memfork\bin` — and nothing asks for administrator.
The installer checks the download against a published checksum, tells you how
to uninstall, and if you are upgrading it stops the old version first.

## Connect it to your tools

```sh
memfork init
```

That finds the MCP clients you have and registers MemFork with each one,
preferring the client's own command so it writes its own configuration. Restart
the client and the tools appear.

```text
$ memfork init
  Claude Code    ran                claude mcp add --scope user memfork -- /home/ada/.memfork/bin/memfork mcp
  Cursor         added              /home/ada/.cursor/mcp.json
  Codex CLI      ran                codex mcp add memfork -- /home/ada/.memfork/bin/memfork mcp

Registered with 3 client(s). Restart them to pick up the tools.
```

If something is not working, `memfork doctor` says which MemFork is running,
where your memory is kept, and what each client thinks is registered.

## Try it

Ask your agent to remember something, branch, change it on the branch, and
throw the branch away. From the command line, the same thing:

```sh
memfork run - <<'SCRIPT'
put plan:1 "ship on Friday"          # remember something

fork attempt                          # branch the whole of memory
put plan:1 "ship on Monday" --branch attempt
get plan:1                            # still "ship on Friday" here

discard attempt                       # the attempt never happened
get plan:1                            # "ship on Friday"
SCRIPT
```

Swap `discard` for `merge attempt` and the change comes back to the main line
instead. Nothing is copied either way: a branch shares structure with the
branch it came from until one of them changes.

## What your agent gets

Fifteen tools, in four groups.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/icons/package-dark.svg"><img src="docs/assets/icons/package-light.svg" alt="" width="16"></picture>
**Remember and recall** — store a value under a key, read it back, list keys by
prefix, delete, and search by meaning when you supply a vector.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/icons/git-branch-dark.svg"><img src="docs/assets/icons/git-branch-light.svg" alt="" width="16"></picture>
**Branch** — fork memory, compare two branches, merge one into another, discard
one entirely, or switch which branch you are working on.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/icons/rotate-ccw-dark.svg"><img src="docs/assets/icons/rotate-ccw-light.svg" alt="" width="16"></picture>
**Go back** — read memory as it was at any earlier point, or look through the
history of what changed.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/icons/arrow-right-left-dark.svg"><img src="docs/assets/icons/arrow-right-left-light.svg" alt="" width="16"></picture>
**Hand over** — pick up a project where the last agent left it, and leave a
note for the next one when you stop.

Full descriptions are in [the tool reference](#mcp-tools) below.

## Supported clients

`memfork init` knows each of these and uses the client's own command where it
has one. No client is special.

| Client | How MemFork registers |
|---|---|
| Claude Code | `claude mcp add` |
| Cursor | its `mcp.json` |
| Codex CLI | `codex mcp add` |
| Gemini CLI | `gemini mcp add` |
| Grok | `grok mcp add` |

Anything else that speaks MCP over stdio works too — point it at `memfork mcp`.

## Several clients, one memory

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/icons/plug-dark.svg"><img src="docs/assets/icons/plug-light.svg" alt="" width="16"></picture>
Point as many clients at MemFork as you like. The first one that actually uses
memory quietly starts a small server that owns the store; the rest connect to
it. You never have to start it yourself, and nothing starts until a tool is
called — a client that only checks that MemFork works leaves nothing running.

```sh
memfork doctor   # is it running, on what port, which version
memfork stop     # shut it down now rather than waiting for it to go idle
```

It listens on `127.0.0.1` only and needs a token it writes to a file only you
can read, so nothing off your machine can reach your memory. It exits by itself
once nothing has needed it for ten minutes.

Clients share the data but not their place in it: each keeps its own current
branch, so one client forking or switching never moves another.

**Upgrading:** the installer stops it for you. If you are replacing the binary
by hand, run `memfork stop` first — a server from a different version will not
talk to a client from this one, and says so rather than guessing.

## Handing work between agents

One agent can stop in the middle of a task and another, from any vendor,
can pick it up: what was decided and why, what is done, and what comes next.

**Each project has a namespace.** When a client starts `memfork mcp`, MemFork
takes the name of the repository it was started in (or of the working
directory, outside a repository), lowercased, and tells the agent that name
when it connects. Everything about the project goes under it, with colons
between the parts:

| Key | What it holds |
|---|---|
| `shop:decision:<topic>` | a decision and the reason for it |
| `shop:task:<id>` | an open task; `{"status":"done"}` closes it |
| `shop:handoff:<n>` | handoff notes, numbered, the newest last |

Set it yourself with `memfork mcp --namespace <name>` in the client's
configuration, or with `MEMFORK_NAMESPACE`. The tools that take a key take it
literally: nothing is prefixed for you, and keys written before namespaces
existed are exactly where they were.

**Two tools do the handing over.** `memfork_resume` returns one short briefing:
the latest handoff, the most recent decisions and the open tasks. An agent
calls it when it starts. `memfork_handoff` records where things stand, and an
agent calls it before it stops or before you switch to another tool.

A worked example, in a repository called `shop`:

```text
In Claude Code:
  you    Add hosted checkout. We're not storing card data, so use the
         provider's hosted page.
  agent  memfork_put shop:decision:payments "Hosted checkout: no card data
         on our servers."  ...builds it...
  you    I'm out of time; hand this over.
  agent  memfork_handoff  summary "Checkout works; refunds not started"
                          next ["refunds", "EU tax"]
                          blockers ["need a sandbox account for refunds"]

Later, in Codex:
  you    Carry on with the shop.
  agent  memfork_resume
         -> latest handoff by claude-code: "Checkout works; refunds not
            started", next: refunds, EU tax; decision: hosted checkout,
            no card data on our servers
  agent  Starting on refunds. You'll need a sandbox account first...
```

**Who wrote what is kept.** Everything a client stores records the name the
client gave when it connected, so a briefing says which agent decided or handed
off what.

**Tell every agent the routine.** Run this inside the repository:

```sh
memfork init --project            # the clients installed here
memfork init --project --all      # every client, for a repository your team shares
memfork init --project --client codex --client gemini-cli
```

It writes one short block — resume when you start, record decisions with their
reasons, hand off before you stop, fork before anything risky — into the
instruction file each client reads (`AGENTS.md`, `CLAUDE.md`, `GEMINI.md`),
once per file however many clients share it. The block sits between two marker
comments and nothing outside them is touched: running it again updates only
the block, `--remove` takes only the block out, `--dry-run` prints the exact
diff and writes nothing. It never runs git; committing the files is up to you.
Plain `memfork init` never edits files in your project.

**What is not shared.** Agents share what they write down, not their
conversation. A handoff carries only what the agent put into it, and a decision
that was discussed but never stored is not there for the next one. The block
above exists to make writing it down the habit.

## Why not Redis, or a vector database?

| | MemFork | Redis | Vector DBs |
|---|---|---|---|
| Fork state before a risky step | O(1), any size | — | — |
| Merge two lines of work | three-way, key level | — | — |
| Throw an attempt away completely | one call | — | — |
| Read the state as of ten steps ago | one call | — | — |
| Multi-key transaction with real rollback | yes | `MULTI`, no rollback on error | — |
| Runs in your process | yes | network hop | network hop |
| Licence | Apache-2.0 | RSAL / SSPL | varies |

The thing MemFork is for is the shape of agent work: try something, then either
keep it or pretend it never happened. That is a branch, and branching state is
what MemFork does that a key-value store does not.

## What it does not do

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/icons/shield-check-dark.svg"><img src="docs/assets/icons/shield-check-light.svg" alt="" width="16"></picture>
Said plainly, because finding out later is worse.

- **Search is exact, not approximate.** Every entry with a vector is compared,
  so a million of them is a million comparisons. Fine for tens of thousands;
  slow beyond that. An index that understands branches is still to come.
- **No embedding model.** You supply the vectors; MemFork stores and compares
  them.
- **One machine.** Not distributed, not replicated. The server is local only.
- **Your memory is a file you own.** It is not encrypted, and anything running
  as you can read it. Do not put secrets in it.
- **The Python library is separate from the shared store.** `import memfork`
  gives you an engine in your own process; it does not see what an MCP client
  wrote. Connecting the two is on the list.

## MCP tools

| Tool | What it does |
|---|---|
| `memfork_put` | Store a value under a key, with an optional vector and importance |
| `memfork_get` | Read a key back |
| `memfork_list` | List keys under a prefix |
| `memfork_delete` | Forget a key |
| `memfork_search` | Find the entries nearest a vector |
| `memfork_fork` | Branch the whole of memory |
| `memfork_merge` | Merge one branch into another |
| `memfork_discard` | Throw a branch away |
| `memfork_diff` | What differs between two branches |
| `memfork_branches` | List branches |
| `memfork_checkout` | Switch this client's current branch |
| `memfork_at` | Read a key as it was at an earlier point |
| `memfork_log` | The history of a branch |
| `memfork_resume` | A short briefing on a project: latest handoff, recent decisions, open tasks |
| `memfork_handoff` | Leave a note on where the work stands, for whoever picks it up |

---

# For developers

## Rust

```toml
[dependencies]
memfork-core = "0.1"
```

```rust
use memfork_core::{Db, Value};

let db = Db::new();
db.put("main", "plan:1", Value::new("ship on Friday"))?;

db.fork("main", "attempt")?;
db.put("attempt", "plan:1", Value::new("ship on Monday"))?;

// The parent is untouched until you merge.
assert_eq!(db.get("main", "plan:1")?.unwrap().value, "ship on Friday");

db.discard("attempt")?;
# Ok::<(), memfork_core::Error>(())
```

`memfork-core` is the engine on its own: no I/O, no async, no network. It is
what the binary and the Python package are both built on.

## Python

```sh
pip install memfork
```

```python
import memfork

db = memfork.Database()
db.put("plan:1", b"ship on Friday")

db.fork("attempt")
db.put("plan:1", b"ship on Monday", branch="attempt")
db.get("plan:1").value        # b'ship on Friday' — the fork is invisible here

db.merge("attempt")           # or db.discard("attempt")
```

One wheel carries both this and the `memfork` command, for Python 3.9 and
newer, on Linux (glibc and musl), macOS and Windows, x86-64 and arm64 — so
installing never needs a compiler.

`memfork.Database` is in memory only: it does not write to disk and does not
see what an MCP client wrote. The durable store shared between clients belongs
to the server.

## Command line

| Command | |
|---|---|
| `put`, `get`, `list`, `delete`, `search` | one operation against a database |
| `fork`, `merge`, `discard`, `diff`, `branches`, `at`, `log` | branching and history |
| `run <file\|->` | a script of the above against one database |
| `mcp` | serve MCP over stdio — what clients run |
| `serve` | run the shared server (started for you when needed) |
| `stop` | shut it down |
| `init`, `doctor` | register with clients; report what is going on |
| `init --project` | write MemFork's instruction block into this repository's client instruction files |
| `tools --format openai\|anthropic\|gemini` | the tool schemas in a vendor's format |

`--json` on any command prints machine-readable output. `--ephemeral` keeps
nothing and shares nothing. `memfork mcp --namespace <name>` (or
`MEMFORK_NAMESPACE`) names the project a session works in.

## How it works

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/icons/zap-dark.svg"><img src="docs/assets/icons/zap-light.svg" alt="" width="16"></picture>
Branches are cheap because the data structures are persistent: a fork copies a
pointer, and the two branches share everything they have in common until one of
them changes. Every change is a commit whose identity is the hash of its
contents, so the same sequence of operations produces the same ids on every
machine — which is what makes history comparable and recovery verifiable.

Writes go to a checksummed log before they are visible. A snapshot is taken at
the far end of the retained history rather than at the present, so restarting
never shortens how far back you can look.

The whole design is in **[docs/DESIGN.md](docs/DESIGN.md)**.

## Building

```sh
git clone https://github.com/memforkdb/memfork
cd memfork
cargo build --release      # the binary lands in target/release
```

Rust 1.89 or newer for the binary; `memfork-core` alone builds on 1.85.

Before sending a change, everything in
**[CONTRIBUTING.md](CONTRIBUTING.md)** has to pass — on all three operating
systems, which CI checks for you.

## Licence

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
