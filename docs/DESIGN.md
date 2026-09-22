# MemFork — design

How MemFork works and why it works that way. Written for someone changing the
engine; if you only want to *use* MemFork, the [README](../README.md) is the
whole story.

This document is the agreement the code keeps. Where it and an implementation
disagree, one of them is a bug — say which in an issue rather than deciding
quietly in a pull request. Revisions are listed at the end, with what changed
and why; markers like **[v0.8]** in the text say when a decision was taken.

## 1. What it is
An embedded in-memory database for agent state. Agents can store and recall
memory, fork it before a risky step, merge on success, discard on failure, and
rewind to any earlier point. It ships as a Rust library, a single `memfork`
binary (MCP server, daemon and command line in one), and a Python package.

## 2. What it is not
- Not distributed, not replicated, no clustering.
- No SQL, no query language.
- No built-in embedding model: callers supply vectors.
- No auth beyond a localhost token. Never bind to a non-loopback address by default.
- Not a Redis protocol clone.

## 3. What every design choice has to protect
1. Branching state: O(1) fork, three-way merge, discard.
2. Time travel: read any branch as of any past commit.
3. Atomic multi-key transactions with real rollback.
4. Embedded: in-process library, zero network hop.
5. Semantic eviction: importance + recency decay, not LRU.
6. Deterministic: reproducible IDs and search ordering.
7. Apache 2.0, identical behaviour on Windows / macOS / Linux.

## 4. Core model (`memfork-core`)

### 4.1 Types
- `Key`: UTF-8 string, max 1024 bytes. Convention `namespace:id`. **[v0.9]**
  With projects (§6.3) the convention is `<project>:<kind>:<id>` — one
  separator, the colon, everywhere — so a prefix such as `shop:decision:`
  lists exactly one kind of thing in one project. The engine itself attaches no
  meaning to any of it.
- `Entry`:
  - `value: Bytes` (opaque; JSON by convention)
  - `embedding: Option<Vec<f32>>` (fixed dimension per database, set on first insert)
  - `importance: f32` in [0,1], default 0.5
  - `created_seq: u64`, `last_access_seq: u64` (logical clock, not wall time).
    **[v0.3]** Both are set on write only. A read never updates
    `last_access_seq`: doing so would make every read a write, which would
    break "readers never block" (§4.3) and would put reads into the event log.
    Recency for eviction therefore comes from a side structure, see §4.4.
  - `ttl_commits: Option<u64>` (expiry measured in commits on that branch)
  - `meta: BTreeMap<String,String>`. **[v0.9]** The key `memfork.by` records
    who wrote the entry (§6.2). It is ordinary metadata, so it is part of the
    operation and of the commit id: the same write by the same writer is the
    same commit everywhere, and a store written before it existed is read
    unchanged. It is not part of the entry's *content*: `content_eq`, and so
    merge and diff, ignore it, because two writers storing the same value have
    not disagreed.
- `Root`: persistent ordered map `Key -> Arc<Entry>`, behind a `trait Store` so the
  structure can be replaced later. **[v0.3]** The implementation is
  `rpds::RedBlackTreeMapSync`, not `imbl::OrdMap`: `imbl` is MPL-2.0, which the
  licence policy does not permit, and `rpds` is MIT. It gives the same
  properties the design depends on — O(log n) reads and writes, O(1) cloning,
  structural sharing, ascending-order iteration, and `Send + Sync` roots.
- `Commit`:
  - `id: CommitId` = **[v0.3]** blake3(parent ids ‖ message ‖ canonical-encoded ops).
    Content-addressed. The message is encoded as an optional length-prefixed
    string, with `None` distinct from `Some("")`, so a commit with no message,
    one with an empty message and one with a real message are three different
    commits. `seq` takes no part, and neither does wall-clock time.
    Without the message in the address, two commits a reader can plainly tell
    apart would share an id.
  - `parents: SmallVec<[CommitId;2]>` (2 for merges)
  - `root: Root`
  - `seq: u64` (per-branch monotonic)
  - `message: Option<String>`
  - `ops: Vec<Op>` (the change set — this IS the append-only event log)
- `Branch`: `name -> head CommitId`. Default branch `main`.

### 4.2 Operations
- `put / get / delete / list(prefix)` on a branch.
- `begin(branch) -> Txn`; `txn.put/delete…`; `txn.commit(msg)` atomically swaps the
  branch head; dropping a `Txn` without commit is a rollback and leaves no trace.
  Single-op calls are auto-commit transactions.
- `fork(from_branch, new_branch)`: new branch points at the same commit. O(1).
- `fork_at(from_branch, seq, new_branch)`: fork from history.
- `merge(source, target, policy)`: three-way merge at key level against the common
  ancestor. `policy`: `Fail` (return conflict list, change nothing), `Ours`, `Theirs`.
  A key is in conflict only if both sides changed it to different values.
  **[v0.3]** Three outcomes, reported as `MergeKind`:
  - `UpToDate` — the source is already an ancestor of the target, or the merge
    plan is empty because both sides made the same change or the policy
    resolved every difference to the target. Nothing changes and no commit is
    made; an empty commit would be history that says nothing happened.
  - `FastForward` — the target has not moved since the fork, so its head simply
    advances to the source's. No merge commit, no recomputation, O(1) whatever
    the branches hold.
  - `Merged` — a commit with both heads as parents, its ops sorted by key.
- `discard(branch)`: delete the branch; unreachable commits are freed (Arc refcount).
  `main` cannot be discarded.
- `at(branch, seq) -> ReadView`: read-only view of any past commit.
- `log(branch, limit)`, `diff(a, b)` where a/b are branch names or commit ids.
- `search(branch, query_vec, k, filter_prefix?)`: exact cosine similarity, brute force,
  ties broken by key ascending. Deterministic. (HNSW is explicitly deferred — a
  branch-aware approximate index is a research problem, and is still to do.)

### 4.3 Concurrency
- Many concurrent readers, one writer per branch. Readers never block: they hold an
  `Arc` to an immutable root.
- Writers to different branches do not contend. They briefly share one lock when
  inserting into the commit index, held only for that insert.
- Optimistic commit: a `Txn` records its base head; commit fails with `Conflict` if the
  head moved, caller retries. No deadlocks possible.
- **[v0.3]** Each branch carries a writer lock in addition to its head pointer.
  The read-modify-write operations the engine drives end to end — auto-commits
  and merges — take a turn on it rather than racing each other; an explicit
  `Txn` does not take it and stays purely optimistic as above, so it may lose
  and must say so. Without this, a caller writing single keys from several
  threads could exhaust a retry budget and see `Conflict` for a write that
  should have succeeded, which is not a contract worth having. The head lock is
  still held only for the instant of the compare-and-swap, so readers never
  wait behind a writer that is still building a commit. A merge takes only the
  target's writer lock, so two merges in opposite directions cannot deadlock.

### 4.4 Eviction
- Configurable memory budget (bytes, approximate accounting).
- Score = `importance * 0.5^((head_seq - last_access) / half_life)`, where
  `last_access` is the reading below.
- **[v0.3]** Access tracking lives in a side structure, not in a field the commit
  chain carries. Reads do not update `last_access_seq` in the entry (§4.1), so
  recency lives in a per-branch map `Key -> seq` held beside the commit graph,
  updated on read and on write, and consulted only when eviction runs. It must
  stay outside the commit chain for three reasons: a read must not become a
  write, an access must not become an event in the append-only log, and — most
  importantly — commit ids must not depend on who read what, or the same
  operations would stop producing the same ids (§3.6), which a golden file
  pins on every platform.
  Consequences to design for: the side structure is not versioned, so time
  travel and `fork_at` do not restore past access times; a fork inherits a
  snapshot of its parent's readings at the moment of the fork; and a
  discarded branch's readings are dropped with it.
- When over budget on commit: evict lowest-scoring entries from the branch head until
  under budget. Eviction is itself a commit (`Op::Evict`), so it is visible in the log
  and reversible via time travel while history is retained.
- `on_evict` hook receives evicted entries first (for consolidation to a durable store).
- History retention: keep last N commits per branch (default 10,000); older commits are
  squashed.
- **[v0.5] The snapshot sits at the retention horizon, not at the present.**
  The snapshot is the state at the oldest commit still retained, and the WAL
  holds every commit after it. Recovery replays all of them, so `at`, `log` and
  `fork_at` answer identically before and after a restart, back to the
  retention limit. Snapshotting the present would be simpler and would quietly
  shorten time travel to "since the last snapshot", destroying one of the four
  things §3 says MemFork is for.

### 4.5 Durability

**[v0.5] The default differs between the library and the binary, on purpose.**

- `memfork-core` is **in-memory unless the caller attaches a journal**. A
  library should not start writing to someone's disk because it was linked.
- The `memfork` binary **persists unless told not to**, with `--ephemeral` to
  opt out. An agent memory that empties on every restart is not memory, and a
  user who installs a memory server does not expect to have to ask for it.

**Data directory.** `./.memfork/` if it already exists in the working directory
— never created implicitly, so it only applies when someone made it on purpose
— else `MEMFORK_DATA_DIR`, else the platform's per-user directory:
`%LOCALAPPDATA%\memfork`, `~/Library/Application Support/memfork`, or
`$XDG_DATA_HOME/memfork` falling back to `~/.local/share/memfork`.

**[v0.5] Resolved by hand, with no crate.** v0.4 left this open. `dirs` pulls in
`option-ext`, which is MPL-2.0 and outside the licence policy; `etcetera` is
permissively licensed but buys about forty lines of `match`. Neither is worth a
dependency. Resolution takes the operating system and the environment as
parameters rather than reading the machine it runs on, so all three platforms
are tested from whichever one runs the suite.

**[v0.5] Write-ahead log format.**

```text
header   magic "MEMFWAL\0" (8) | version u32le | reserved u32le
record   len u32le | checksum u64le | payload[len]
         checksum = blake3(len_le ‖ payload)[..8]
```

- Magic and version so a future format change is *detectable* rather than
  silently misread. A file without the magic is refused by name; a version this
  build does not know is refused rather than guessed at.
- The checksum covers the length as well as the payload. A checksum over the
  payload alone leaves a corrupted length undetectable, and the length is the
  dangerous field: it decides how much is read and how much is allocated. The
  length is capped at 16 MiB besides.
- BLAKE3 rather than a CRC, because BLAKE3 is already a dependency and a
  checksum crate would be a new one for no gain at these sizes. Eight bytes is
  far beyond what accidental corruption defeats. It is not a defence against a
  deliberate attacker: anyone who can write the log can write a checksum.
- **A torn tail is normal, not corruption.** A process killed mid-append leaves
  a partial record, which says nothing about the records before it. Reading
  stops at the last record that checks out and the file is truncated there.
- **Commit ids are never written down.** A record carries parents, message and
  operations; replay recomputes the id exactly as the original commit did. That
  is what makes "recovery reproduces identical commit ids" a property worth
  testing rather than a statement about copying bytes.

**[v0.5] fsync policy.** `--fsync always|interval|never`, default `always`.

| | costs | risks |
|---|---|---|
| `always` | one flush per commit: roughly 0.1–2 ms on an SSD, so hundreds to thousands of commits a second against the tens of thousands the in-memory engine manages | nothing: an acknowledged change has reached the disk |
| `interval` | batches flushes across commits | the last window, to a power cut or kernel panic — not to a process crash |
| `never` | nothing | everything since the last snapshot, if the machine stops — survives a process crash, since the operating system still holds the bytes |

**[v0.5] Single-writer enforcement: two files, not one.**

- `memfork.lock` is **empty and stays empty**, held under an advisory exclusive
  lock for as long as a process owns the directory.
- `memfork.endpoint` is ordinary JSON — file-format version, pid, port, token,
  start time and **[v0.6]** the MemFork version that wrote it — written
  tmp + fsync + rename, and **never locked**.

They are separate because on Windows an exclusive `LockFileEx` range blocks
*reads* as well as writes. Verified, not assumed: reading a locked file there
fails with "another process has locked a portion of the file" (os error 33). A
token inside the locked file would be unreadable by the very client that needs
it. Locking uses `std::fs::File::try_lock`, stable since Rust 1.89, so there is
no locking crate either.

**Staleness needs no heuristics.** Every operating system releases an advisory
lock when the holding process dies, however it dies. Acquiring the lock *is*
proof the previous owner is gone — no pid liveness guessing, no start-time
comparison, the same answer on all three platforms. An endpoint file left by a
dead owner is then deleted. A reader trusts the endpoint file only while the
lock is held.

**[v0.13] Asking is taking, for a moment.** Whether anybody owns the directory
is found by trying the lock and letting go at once, so a process that asks
while a daemon starts can hold the lock at the instant the daemon tries for
it. Taking the lock therefore keeps trying for up to a second while no
endpoint file is published — a question holds the lock for microseconds and
never publishes one, an owner holds it for good and does — and gives up at
once when one appears, so a process that lost a race never takes over when
the winner stops. A question removes a stale endpoint file only while it
still holds the lock. Before this, three daemon starts in thirty exited with
"in use" while a test asked about the directory as fast as it could.

**[v0.5] File permissions are not equal across platforms, and this says so.**
The endpoint file is created `0600` on Unix. On Windows an explicit ACL needs
`unsafe` Win32 calls, and the workspace allows exactly one of those
(§5), so the file relies on
living inside `%LOCALAPPDATA%`, which is already user-scoped by default. That
is weaker: an administrator, or anything running as the user, can read it.
Documented rather than implied to be equivalent.

**Recovery** = snapshot + WAL replay, and must reproduce identical commit ids —
checked against the same golden file that pins ids across the three operating
systems.

**[v0.6] Version skew is refused, not guessed at.** A client that finds a
daemon reporting a different `memfork_version` must say so and stop, naming
both versions and `memfork stop`. Two builds sharing one store is how a store
gets corrupted by formats that were never meant to meet, and an endpoint file
with no version at all is treated as a mismatch rather than assumed compatible.

**[v0.6] Tests may not fall back to the per-user directory.** Setting
`MEMFORK_FORBID_PER_USER_DATA_DIR` makes resolving it a hard error. Every test
sets it, and every process a test starts inherits it, so a test that forgets to
choose a directory fails instead of writing into the developer's own store —
which happened twice before the guard existed.

## 5. The `memfork` binary
Subcommands:
- `memfork mcp` — MCP server over stdio. **[v0.5]** Persists to the data dir by
  default; `--ephemeral`, `--data-dir`, `--fsync`, `--retention`,
  `--memory-budget` and `--half-life` adjust that.
- `memfork serve` — local daemon, streamable HTTP MCP on `127.0.0.1:<port>`.
  **[v0.6]** The token lives in the endpoint file, not the lock file (§4.5).
  The port is chosen by the system unless `--port` says otherwise, and the
  daemon exits after `--idle-timeout` seconds with no requests (default 600),
  flushing first: a background process nobody started should not outlive its
  usefulness.
- **[v0.6]** `memfork stop` — shut the daemon down gracefully: flush, release
  the directory, remove the endpoint file. Use it before upgrading.
- `memfork init` — detect installed clients and register the MCP server with each.
  Driven by the client adapter registry (§6.1), never by hard-coded per-client logic.
  Idempotent. Prints exactly what it changed. `--dry-run` and `--client <name>` supported.
  **[v0.9]** `--client` may be repeated. `memfork init --project` is a separate
  job entirely — the instruction files of §6.4 — and plain `init` never edits a
  file in the project.
- **[v0.9]** `memfork mcp --namespace <name>` names the project a session works
  in (§6.3); otherwise `MEMFORK_NAMESPACE`, otherwise the repository.
- `memfork doctor` — print version, data dir, lock status, detected tools, and whether
  each tool's config contains the MemFork entry.
- `memfork put|get|del|ls|search|fork|merge|discard|branches|log|at|diff` — thin CLI
  over the core for humans and scripts. **[v0.3]** `search` is included so the CLI
  can exercise §4.2's search; every command takes `--branch` (default `main`) and
  `--json`. **[v0.10]** Each works on the shared store: the command is sent to
  the daemon, which starts if it is not running, and carried out there by the
  same code `--ephemeral` runs in memory, so the two cannot differ. Writes are
  attributed to `memfork-cli`. `--ephemeral`, `--data-dir` and `--color` are
  global. `memfork call` goes through the daemon the same way, as an MCP session
  of its own. `memfork log --graph` draws every branch as a tree (§5.2).
- **[v0.10]** `memfork watch` — the daemon's activity as it happens (§5.3).
- **[v0.12]** `memfork task add|claim|renew|release|done|list`,
  `memfork find <text>`, `memfork facts [prefix]`, `memfork lessons` and
  `memfork stats` — the board (§6.5), text search (§6.6), every fact with its
  verdict (§6.7), the project's lessons (§6.9) and the counters (§6.8), each
  with `--json`. `put --source <path>` (repeatable)
  records a fact and `discard --lesson <text>` leaves a lesson (§6.9). The
  command line checks facts itself, since it runs where the files are.
- **[v0.13]** `memfork plan write|check|show|new|templates` (§6.11), `memfork
  flags` (§6.12), `memfork maintain on|off|status` (§6.13); `task add` takes
  `--depends-on`, `--accept` and `--timeout`, `task done` runs an acceptance
  command and takes `--fork`, and every write takes `--allow-secret` (§6.10).
- **[v0.12]** The daemon's `/report` endpoint, behind the same token, takes the
  fact verdicts a proxy or the command line worked out, for the feed and the
  counters. It changes no memory.
- **[v0.3]** `memfork run <script|->` — run a batch of the above subcommands against
  one in-memory database, one command per line, `#` starting a comment, shell-style
  quoting, and an optional per-line `--branch`. Until durability lands (§4.5) a
  process does not outlive its state, so this is how a sequence of operations shares
  a database. It stops at the first failing line and names the line number.
  `run`, `mcp`, `serve`, `init` and `doctor` may not appear inside a script.

**[v0.6] Transport rule, restated.** A persistent `memfork mcp` is *always* a
proxy. **[v0.7]** It starts the daemon on the first `tools/call` and not
before: `initialize` and `tools/list` are answered by the proxy itself, from
the same static registry the daemon would serve. If no daemon holds the data
directory it starts one — detached, with safe standard-library calls only: a
new process group on Unix, `CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS` on
Windows, null standard streams on both — waits for the endpoint file, and
proxies to it. Only the daemon ever opens the write-ahead log.

**[v0.11] Starting is allowed to be slow, and failing says why.** The wait
for the endpoint is sixty seconds by default, `MEMFORK_START_TIMEOUT` seconds
if set: a first start on a machine scanning a new executable is the slowest
there is and the worst time to fail. Past three seconds one plain line says
the start is still under way. The daemon's stderr goes to
`memfork-daemon.log` in the data directory, rewritten by each start, rather
than to nowhere; if the process exits without serving, the wait stops two
seconds later instead of running out the clock, and a failure names the
command that was tried, quotes the end of that log, and says what to do next.

Nobody should have to run `memfork serve` by hand. Two clients at once is the
normal case, and the second failing with "in use by process N" would be a bug
report rather than a feature.

**[v0.7] A handshake is not a reason to start a daemon.** Clients launch
`memfork mcp` just to ask what it is: `claude mcp get` health-checks a server
by running it and shaking hands. A proxy that reached for the daemon during
`initialize` therefore left one running behind every such probe — including
behind `memfork doctor`, which asks each client whether MemFork is registered.
`memfork stop; memfork doctor` restarted what had just been stopped. The tool
surface does not depend on the data, so nothing about it needs the store.

**[v0.8] MemFork resolves how to launch itself; it does not assume it is a
binary.** Two things need to start MemFork again later — the proxy, which
spawns the daemon, and `memfork init`, which tells a client what to run — and
both used `current_exe()`. That is right exactly when MemFork *is* its own
executable. Installed from a wheel it is not: the running program is a Python
interpreter, so spawning it would start Python rather than a daemon, and
registering it would hand a client a command that does nothing.

A launch is therefore a program *and* the arguments that come before the
subcommand. The Python front door computes it — the console script if there is
one, otherwise `<interpreter> -m memfork` — and passes it in `MEMFORK_LAUNCH`
as a JSON array. Everything else falls back to this executable. A malformed
value is ignored rather than obeyed.

**[v0.8] A registration that points at another MemFork is not "registered".**
Installing moves the binary, and the client goes on launching the path it was
given: nothing errors, the tools simply stop appearing. `memfork doctor`
reports it as *registered, but not to this MemFork*, names the path it points
at, and counts it as needing `memfork init`. `memfork init` removes the stale
registration and adds the current one, saying what it changed. Where a client
documents no way to remove a registration, MemFork prints the command to run
rather than guessing at one that edits somebody's configuration.

**[v0.8] A spawned daemon inherits nothing of the client's.** On Windows a new
process inherits every inheritable handle its parent holds, and the proxy holds
the client's pipes; a daemon that inherited them kept the client's stdout open
for its whole life, so a client reading to the end of the pipe waited for ever
after `memfork mcp` had already exited. Unix never had this, which is precisely
why it needed a test.

It takes two answers, because a wheel install is not one process. The binary
clears `HANDLE_FLAG_INHERIT` on its own standard handles before spawning —
the one place MemFork calls the platform API directly, and the only `unsafe`
in the workspace. That is not enough inside Python: the console-script
launcher and the virtualenv redirector each duplicate the client's pipes into
the process they start, and nothing in the running process can name those
copies to clear them. `bInheritHandles = FALSE` would settle it and the Rust
standard library cannot ask for it, but Python can — `close_fds=True` means
exactly that. So a MemFork running inside Python starts its daemon through
`memfork._spawn`, one short-lived process that inherits the mess, hands none
of it on, and exits. `MEMFORK_SPAWN_VIA` is how the front door offers it.

**[v0.7] A proxy exits with its client.** Closing stdin, or dying outright,
ends the session and the process, so proxies never accumulate. This is not
implicit: it is the difference between a client that reconnects often and a
machine filling up with processes nobody asked for.

Racing is expected and harmless: two clients starting at the same instant both
find no daemon and both spawn one. Exactly one wins the directory lock; the
losers exit without touching the log, and both clients then find the winner.

If the daemon dies mid-session, the proxy starts another **once** and retries
the call. Once, not in a loop: a daemon that dies immediately will not be fixed
by trying again, and a proxy that keeps trying turns a clear failure into a
hang. If the retry fails the client gets an MCP error saying so.

`--ephemeral` is entirely private: no daemon, no data directory, nothing shared
and nothing kept.

**[v0.10] A forgotten session is replaced, not reported.** The daemon forgets a
client session after a period with no requests from it (thirty minutes). Before,
a proxy kept presenting the forgotten session and its client got an error on
its next call; now the daemon's "no such session" is treated like a daemon that
went away, and the proxy opens a new session and retries. A proxy also ends its
session when its client goes away, so the daemon's list of connected clients
is accurate rather than a list of everything that ever connected.

### 5.2 Colour, glyphs and motion
**[v0.10]** The palette is defined once (`style.rs`): Deep Sea blues as the base
— `#9EB3C2` for primary text, `#1C7293` for secondary text and lines — with
`#065A82` and `#21295C` used only as backgrounds for badges and bars carrying
light text, because as text they vanish on a dark terminal; one bright accent,
`#3DDC97`, for forks and success; amber, `#E8A33D`, for discards and warnings. A
test fails if either dark blue is ever used as a text colour.

There is no colour or motion from `memfork mcp` (its stdout is a protocol) or
under `--json` (its stdout is data), whatever `--color` says. Otherwise `auto`
colours only a terminal, outside CI, without `NO_COLOR`, and not a dumb one;
`--color always` beats all four, because a flag on the command line is the more
specific instruction; `--color never` never colours. A spinner is drawn only on
a terminal stderr, and only where there is real waiting: starting the daemon,
which replays the log, and asking or registering with a client through its own
command. Operations that take microseconds get none. When a spinner cannot be
drawn, starting the daemon is still said in one plain line.

Colour is never the only signal: every coloured state also has a word — `[main]`,
`merge`, `fork point`, `discarded`, `ahead`, `behind`, `connected` — and every
glyph has an ASCII stand-in, used on a legacy console, without a UTF-8 locale, or
when stdout is not a terminal. No emoji.

Output is drawn from the command's JSON result in the process attached to the
terminal, never in the daemon, so it follows the terminal in front of the
person.

`log --graph` draws commits newest first in a topological order that breaks ties
by sequence number and then id, so the same history always draws the same way.
Discarding frees a branch's commits (§4.2), so the engine keeps a short record of
the most recent discards — name, fork point, commits made — rebuilt from the log
on replay and taking no part in any id; the graph draws each as a stub off its
fork point.

### 5.3 The activity feed
**[v0.10]** The daemon publishes every tool call and every command-line
operation as an event — time, client, project, operation, key or branch,
success — and tracks each client session from its `initialize` until it ends.
`memfork watch` reads them from `/events` on the daemon's loopback listener,
with the same token, as one JSON object per line, starting with the daemon's
version and the clients connected. The status word is "connected", everywhere,
and a test keeps the other word out of the code and the docs. Watching does not
count as activity for the daemon's idle timeout, and `watch` waits for a daemon
rather than starting one. Wall-clock time appears in events and nowhere else in
the daemon. **[v0.12]** Also in the feed: `claim` (with who holds it when a
claim loses), `release`, `done`, `lesson`, and `fact` with `fresh`, `stale` or
`unverified`. A renewal is not shown: a proxy renews on its own, and the feed
would fill with it. (Lease expiry, the one other clock reading, is kept in
memory and never reaches an id.)

**[v0.14] The feed is a contract.** Every line, the `hello` included, carries
`schema`, the version of its shape (`events::SCHEMA`, currently 1). Within a
version a field may be added and never removed, renamed or given a new
meaning; the field set is held by a test and every field is described in
[`docs/EVENTS.md`](EVENTS.md), which a second test checks. There is no
OpenTelemetry exporter: the Rust SDK is not a light dependency, and the
versioned stream is the integration point.

## 6. MCP tools
All tools take an optional `branch` (default: the session's current branch).
```
memfork_put       key, value, importance?, embedding?, ttl_commits?, meta?, sources?
memfork_get       key
memfork_delete    key
memfork_list      prefix?, limit?
memfork_search    text | embedding, k?, prefix?      # [v0.12] text
memfork_fork      name, from?, at_seq?
memfork_checkout  name                 # sets the session's current branch
memfork_merge     source, target?, policy?   # fail | ours | theirs
memfork_discard   name, lesson?                      # [v0.12] lesson
memfork_branches
memfork_log       limit?
memfork_at        seq, key? | prefix?   # time-travel read
memfork_resume    namespace?, task?, budget?, since_last_only?   # [v0.9]; [v0.12] task, budget; [v0.13] since_last_only
memfork_task      action, id?, title?, detail?, depends_on?, accept?, timeout_seconds?,
                  tasks?, fork?, lease_seconds?, status?, namespace?   # [v0.12]; [v0.13] plans, fork
memfork_handoff   summary, done?, next?, blockers?, questions?, namespace?   # [v0.9]
# [v0.13] memfork_put, memfork_handoff, memfork_task and memfork_discard take allow_secret?
memfork_diff      a, b
```
Tool descriptions must tell the model WHEN to use each tool (e.g. "fork before any
risky or exploratory step; merge if it worked, discard if it did not").

### 6.1 Vendor neutrality
MemFork must work with any model vendor. Nothing in core, tool names, tool descriptions
or docs may assume a specific model or client. Three access tiers:

1. **Local MCP clients (stdio)** — Claude Code, Claude Desktop, Cursor, Codex CLI,
   Gemini CLI, Grok Build, VS Code / Copilot, Windsurf, Zed, Cline, and any future
   MCP client.
2. **Remote MCP (streamable HTTP)** — hosted model APIs that call MCP servers by URL
   (e.g. xAI remote MCP tools, OpenAI Responses API, Anthropic MCP connector). These
   cannot reach localhost: document tunnelling / self-hosting `memfork serve` behind
   TLS + bearer token. Never enable non-loopback binding without an explicit flag AND
   a token.
3. **No MCP at all** — `memfork tools --format openai|anthropic|gemini` prints the tool
   definitions as that vendor's function-calling JSON, and `memfork call <tool> <json>`
   executes one call. Plus the Rust/Python SDK. This covers any model, including local
   ones (Ollama, llama.cpp, vLLM).

**Client adapter registry.** One data file (`clients.toml`, compiled in) with one entry
per client: name, detection rule, config path per OS, config format (json | toml),
the key path of the server map, and per-client field quirks. Adding a client = adding
an entry + a fixture test, no new code. Known quirk to encode: Gemini CLI uses
`httpUrl` for streamable HTTP and `url` for SSE, whereas most other clients use `url`
for streamable HTTP. Verify every entry against the client's current docs.

**Schema compatibility.** Tool input schemas use the conservative JSON Schema subset
every vendor accepts: `type`, `properties`, `required`, `description`, `enum`, `items`.
No `$ref`, `oneOf`/`anyOf`/`allOf`, `format`, `pattern`, or nested unions — some
clients sanitise or reject them. Keep tool count ≤ 16 and names ≤ 48 chars,
`[a-z0-9_]` only. A CI test validates every schema against this subset.
**[v0.9]** Fifteen tools. **[v0.12]** Sixteen, with `memfork_task`: the limit
is reached, so anything added later is a command, or an argument to an existing
tool, not a new tool.

### 6.2 Who wrote it
**[v0.9]** A client names itself in MCP `initialize`, and the session records
that name against every entry it writes, under `memfork.by` (§4.1). A caller
cannot set that key itself — any `meta` key starting `memfork.` is refused —
so one client cannot write in another's name.

A proxy is the daemon's client, so on its own the daemon would only ever hear
"memfork-proxy". The proxy therefore passes on what it knows in the standard
`capabilities.experimental` field of its own `initialize`, under
`memfork/session`: the real client's name and the project namespace. Any other
client simply does not send it, and is recorded by its own `clientInfo.name`.
**[v0.12]** It also sends a session id, random per proxy, that keys the claims
the session holds (§6.5). It is never written to the store, so it cannot reach
an id.

Deletes, forks, merges and discards are not attributed in the store: an entry
that no longer exists has nowhere to carry a name, and a branch operation has
no entry at all. Adding a writer to commit messages would change every such
commit's id by who made it, for a record nobody reads there.

The registry's `mcp_names` turn the raw name into the one a person knows, for
display. None of these clients documents its name; each entry says where it was
found.

### 6.3 Projects, handoff and resume
**[v0.9]** One store serves every project on the machine. Each session has a
namespace: `--namespace`, else `MEMFORK_NAMESPACE`, else the name of the
repository's top-level directory — found by walking up to the first `.git`,
which is a directory in a clone and a file in a worktree, and never by running
git — else the working directory's name. Derived names are lowercased and
reduced to `[a-z0-9._-]`, at most 64 characters; a name someone typed is
refused with a suggestion rather than silently altered. The proxy knows its
working directory and passes the namespace to the daemon (§6.2). The MCP
`instructions` a client receives name the session's namespace and the routine,
so every agent learns both with no file edited.

The tools that take a key take it literally. Nothing is prefixed behind the
caller's back, and everything stored before namespaces existed is where it was.
Only the two tools below use the namespace, and both accept another one
explicitly.

| Key | Holds |
|---|---|
| `<ns>:handoff:<8 digits>` | one handoff, numbered from 1; numbering is serialised across sessions |
| `<ns>:decision:<topic>` | a decision and its reason, written with `memfork_put` |
| `<ns>:task:<id>` | an open task; a JSON value with `"status":"done"` closes it; see §6.5 |
| `<ns>:lesson:<8 digits>` | **[v0.12]** what a discarded attempt taught; see §6.9 |
| any other `<ns>:` key with `sources` | **[v0.12]** a fact; see §6.7 |

`memfork_handoff` stores `summary`, `done`, `next`, `blockers` and `questions`
as the next numbered note; earlier notes stay as history. `memfork_resume`
returns one briefing: the latest handoff and a count of earlier ones, the ten
most recently written decisions (newest first, key order breaking ties) and up
to twenty open tasks. It is bounded — 300 characters per text, ten items per
list, 6 KB of JSON in all — because it is read at the start of every piece of
work. Over budget, it gives up the least useful thing first: what was done,
then open questions, then the oldest decisions, then tasks, then blockers, and
the next steps last. What it leaves out is counted, with the prefix to list
for the rest. Nothing in it depends on wall-clock time or map order, so the
same store gives the same briefing. A project with nothing stored returns
`empty: true` and says how to start.

**[v0.12] Briefings by budget.** `memfork_resume` also returns up to five
recent lessons (§6.9) and up to ten facts (§6.7), and takes two arguments.
`task` says what the caller is about to do: lessons, decisions, facts and tasks
are then ranked by the text scoring of §6.6 against it before the caps apply,
and anything left out goes in order of that rank. `budget` is the most bytes of
JSON the briefing may take, 6 KB if not given, raised to 1024 and lowered to
64 KiB. The briefing ends with what it cost:

```
"budget": { "limit_bytes": 4096, "bytes": 1873, "approx_tokens": 469,
            "estimate": "approx_tokens = bytes / 4, rounded up" }
```

Bytes are exact: the briefing's own JSON, `current_branch` included. Tokens
are an estimate, one per four bytes rounded up, because tokenisers differ by
model and MemFork does not call one; the formula is in the answer so nobody
takes it for more. The briefing is sized for the larger of how it leaves the
daemon and how it reaches the agent after its facts are checked (§6.7), and
whoever checks them restates `bytes`. Over budget it gives things up in the
order above, then the caller's own `task` echo, then halves the handoff
summary, so it never exceeds its budget. Nothing in it depends on the clock or
map order: the same store, task and budget give the same bytes on every OS,
which a test pins.

**[v0.13] What changed since you last looked.** The side file (§6.8) keeps, for
each project, client and branch, the head that client last saw: it is noted
after every successful call, outside the commit chain, at most 10,000 records.
A briefing asked for by a session starts with `since_last`, the difference
between the project's keys at that commit and now: new and changed decisions,
new lessons, handoffs by other clients, tasks added, claimed, released or
finished, and the project's facts with their verdict now, so one gone stale
since is marked stale. Each list holds ten at most and gives way to the budget
before the handoff's next steps do. `since_last_only` returns only that, which
for a returning agent is far smaller than the whole briefing. With no record,
or when the commit last seen is no longer in the branch's history (the branch
was discarded and made again), the whole briefing comes back with the reason.
The record is per client name, so two sessions of one tool share it. When a
project uses dependencies (§6.11), open tasks say whether they are ready and,
if not, what they wait for; and flags (§6.12) appear when there are any.

### 6.4 Project instructions
**[v0.9]** `memfork init --project`, run inside a repository, writes one short
block into the instruction file each chosen client reads, so every agent is
told the same routine: resume when you start, record decisions with their
reasons, hand off before you stop, fork before anything risky.

Which files is data. Each registry entry lists the files the client always
reads (`[client.instructions].reads`), and the fewest files that reach every
chosen client are written, once each, however many clients share one. The
chosen clients are the ones installed, or those named with `--client`, or
every client with `--all`, for a repository shared by people using different
tools. One client reads two of the files, so when both are needed it sees the
block twice; that is harmless and documented rather than worked around.

The block is managed: it sits between marker comments and nothing outside them
is ever touched. A file without it gets it appended after a blank line; a file
with it gets only the block replaced, recognised by the begin marker's prefix
so an older wording is found; `--remove` takes out only the block and the blank
line an append added, so adding and removing gives back the original bytes; a
missing file is created holding only the block; line endings and a byte-order
mark are kept; a file with broken or repeated markers is refused rather than
guessed at. `--dry-run` prints the exact unified diff and writes nothing. It
never runs git.

**[v0.12]** The block adds: say the task when resuming, search memory with
`text` before asking, claim a task before starting it, store findings with
their `sources`, and leave a `lesson` when discarding.

### 6.5 The task board
**[v0.12]** `memfork_task` keeps a project's tasks, so two agents do not do
the same work. `add` stores `<ns>:task:<id>` — the next free number unless an
id is given — as `{title, detail?, status: "open", holder: null, claims: 0}`.
`claim` makes the caller its holder, `renew` extends the claim, `release`
gives it back, `done` closes it, and `list` shows the tasks with a status
filter (`open`, `claimed`, `done`, `unfinished`, `all`).

A claim is a lease: `lease_seconds`, 300 by default, 1 to 3600. The lease
itself — which session holds the task and until when — is kept in memory in
the daemon, not in the store. A clock reading in a commit would make the same
operations produce different ids at different times, so the committed entry
carries only the status, the holder's client name and a count of claims, and a
restarted daemon starts with every task free. Claiming checks the lease,
commits `claimed` and takes the lease under one lock, so of two racing claims
exactly one wins; the loser is told who holds it, for how many more seconds,
and whether the holder is the same tool in another session
(`same_client: true`), since two windows of one client are two agents. Renewing
commits nothing and does not appear in `memfork watch`. Releasing and finishing
commit, and only the holder may do either while its lease runs.

A proxy renews each claim its session holds at half the lease period for as
long as it runs, so an agent in a long build keeps its task between calls.
When the proxy goes, renewals stop and the task is free within one lease
period. An expired claim reads as `open` wherever a task is shown.

Leases belong to the work, not to a memory branch: they are keyed by the task
key alone, so a task claimed on `main` is claimed for an agent on a fork too.
The committed entries fork, merge and discard like any other key.

### 6.6 Finding by text
**[v0.12]** `memfork_search` takes `text` as well as `embedding` — one of the
two. Text search needs no model: queries and entries are split into lowercase
words of letters and digits, and each entry under `prefix` (the whole store if
none) is scored with integers only, so the order is the same on every OS:

* a word's weight is `1 + floor(log2(N / df))`, where `N` is the number of
  entries searched and `df` the number containing the word;
* a word in the key scores `weight × 4 × 2` for a whole-word match, or
  `weight × 4` for a key word it begins (three letters or more);
* a word in the value scores `weight × min(occurrences, 6)`;
* a query of two or more words found together, in order, adds
  `2 × 8 × the heaviest weight`.

Ties go to the key in byte order. Each hit has its key, score, the writer, and
a snippet of at most 200 characters around the first match. `k` is 10 by
default and at most 50.

It is a scan: time grows with the entries searched. Measured by
`tests/find_scale.rs` in a release build on a Windows 11 laptop, over entries
of about 60 bytes: 10,000 entries take about 25 ms a query and 100,000 about
250 ms, whatever the query. A prefix narrows the scan. An index is in §10.

### 6.7 Facts that know when they are stale
**[v0.12]** A finding about the code — where something lives, how a function
behaves — goes stale when the code changes. `memfork_put` takes `sources`, a
list of at most 32 paths relative to the project: `\` becomes `/`, `.` parts
go, and `..`, absolute paths and empty ones are refused. An entry with sources
is a fact.

The committed entry carries the paths, under `memfork.sources`, and nothing
about the files. File hashes in a commit would give the same put a different
id on every machine and every edit, and make every commit larger. The hashes
the files had are kept beside the store instead (§6.8), keyed by the blake3 of
the fact's key, value and paths, so the same fact on any branch finds them.

Hashing happens where the files are. The daemon may serve projects it cannot
see, so the proxy (or `--ephemeral` server, or the command line) hashes the
named files with blake3 as it sends the put, and sends the hashes along. When
an answer names a fact, the daemon adds the recorded hashes, and the proxy
hashes the files again and replaces them with a verdict before the agent sees
it:

* `fact: "fresh"` — every file is as it was;
* `fact: "stale"` with `stale_sources` — one or more changed or went missing;
* `fact: "unverified"` — nothing was recorded, or checking would cost too much.

The cost is bounded. Of a file over 8 MiB only the first 8 MiB and its length
are hashed, so a change past that point is seen only if the length changes; a
single answer reads at most 64 MiB, and facts past that are `unverified`; and a
file whose size and modification time are unchanged is not read again. The proxy reports each verdict to the daemon (§5, `/report`)
for `memfork watch` and `memfork stats`.

A fact is refreshed by putting it again, which records the files as they are
now. The same put with the same sources gives the same commit id on every OS,
whatever the files contain; a test pins it.

### 6.8 Stats and fact hashes, beside the store
**[v0.12]** `memfork-sidecar.json`, in the data directory, holds what must
survive a restart but must not change an id: the recorded fact hashes, at most
50,000 (the oldest go first), and counters per project and client — briefings
and their bytes, the bytes of memory they summarised, lessons recorded and
served, fact verdicts, claims and lost claims, text searches. It is written
every five seconds when something changed and when the daemon stops, by
writing a new file and renaming it over the old one. A file that does not
parse is renamed to `memfork-sidecar.json.unreadable` and a fresh one started,
with a note on stderr; losing it loses counts and freshness, never memory.
Leases are not in it (§6.5). `memfork stats` shows it; it is never part of a
commit, and a store opened read-only never creates it.

### 6.9 Lessons from discarded attempts
**[v0.12]** A discard throws the attempt away; `lesson` keeps what it taught.
`memfork_discard` with a one-line `lesson` (at most 300 characters, whitespace
collapsed) first writes `<ns>:lesson:<8 digits>` on the branch the attempt was
forked from, as `{lesson, branch, commits}` with importance 0.8, then discards.
Nothing else from the fork survives.

The parent is found from history, not remembered: of the other branches whose
history contains the fork point, the one with the fewest commits since it; a
tie goes to the default branch, then to the name; with none, the default
branch. Numbering is serialised across sessions. A project keeps its 200 newest
lessons; writing the 201st deletes the oldest from current memory, not from
history. `memfork_resume` includes the newest five (or the five that best
match `task`), and `memfork log --graph` shows the lesson beside the discarded
branch's stub.

### 6.10 Secrets stay out of shared memory
**[v0.13]** Memory is read by every tool on the machine and shown to people, so
a credential pasted into it has leaked. Every write is checked against rules
kept as data in `secret_rules.toml`: private key blocks; the shapes of AWS,
GitHub, GitLab, Slack, Stripe, Google, npm and PyPI tokens, JSON web tokens and
`sk-` API keys; and a random-looking value given to something named like a
key, token, secret or password — at least 16 characters, of at least three
kinds, at least ten different, integer tests only so every OS agrees. Checked:
`memfork_put` keys, values and metadata; every field of `memfork_handoff`; task
titles, details and acceptance commands; lessons; plan files, as a whole, so a
refusal names the line in the file; and the same through the command line.

A match refuses the write and stores nothing. The refusal names the rule, the
field, and the line and column — never the text — as a tool error the agent
can act on, and the watch feed reports it without the key, which may be where
the secret was. A false positive is written by naming its rule in
`allow_secret` (`--allow-secret`); an id that is not a rule is refused rather
than ignored. The machine-wide policy file that can forbid overrides is part
of a later step; the check has the one place it will plug in.

### 6.11 Plans: a pipeline kept as data
**[v0.13]** A task may name the tasks it depends on (`depends_on`, ids in the
same project) and an acceptance command (`accept`). A task is ready when
everything it depends on is done. `list` takes `ready` and `blocked`; watch
shows a task becoming ready when the last thing in its way is done. The `plan`
action writes several tasks in one commit; it replaces a task only while it is
open and unclaimed, and refuses an unknown dependency, one naming another
project, and a cycle, naming the cycle. Nothing schedules anything: whichever
agents connect pull ready tasks from the same board, so the pipeline is data,
not a program.

An acceptance command runs where the project is — the proxy, `--ephemeral`,
or the command line — when an agent marks its task done, with a timeout (600
seconds unless the task says, at most 3600), the whole process tree killed if
it runs over (a process group on macOS and Linux, `taskkill /T` on Windows),
and the last 4 KiB of its output kept. It runs under the platform's shell, `sh
-c` or `cmd /C`, so a plan meant for every OS uses commands that mean the same
in both. Passing closes the task. Failing reopens it, drops the claim, and
records a lesson naming the task, the command, how it ended and its last line
of output, unless that line looks like a credential. The daemon will not
close such a task without a result.

**Which commands run.** A command stored in shared memory would otherwise run
for whichever agent marks the task done, outside that client's own approval of
commands, so any agent could plant one for another tool to run. A command runs
only if the repository's plan file on disk — `memfork-plan.toml` at the top of
the project, or the file the plan was written from — holds the same command for
the same task. The file is the trust anchor: changing it is an edit to the
repository, seen by the client's own approval and by version control. An empty
`accept` is no command at all, and a task with one needs no result.

`memfork plan write [file]` puts a plan file on the board, `memfork plan check`
validates one without writing, and `memfork plan show` groups the board into
ready, claimed, blocked (and by what) and done. `memfork plan new --template
<name>` starts a file from one of five built-in templates — feature, bugfix,
refactor, upgrade, tests — each four or five tasks with an empty acceptance
command to fill in once; `memfork plan templates` lists them, and a person's
own, as `.toml` files in the data directory's `plans` folder, sit beside them.

### 6.12 Duplicates and contradictions, flagged
**[v0.13]** Three deterministic checks over a project, never resolved by
MemFork:

* a decision key holding different values on different branches, written by
  different clients (one client changing its mind on a fork is not flagged);
* the same value under near-identical keys in one family — equal once case,
  `-`, `_`, `.`, spaces and a trailing `s` are set aside, or at most two edits
  apart for topics of eight characters or more; numbered keys never match;
* facts resting on exactly the same source files that say different things.
  Facts that merely share a file are normal and not flagged.

A write that creates one says so in its answer (`similar`, `conflicts`) and as
a `flag` event; briefings carry up to five when there are any; `memfork flags`
lists them all; `memfork doctor` counts them for its directory's project when
a daemon is already running. Bounded: a family over 2,000 entries is not
compared pair by pair, and a scan returns at most 50.

### 6.13 Memory that keeps itself
**[v0.13]** MemFork does not think; when a project's memory needs tidying it
says so as a task. Four triggers, checked when an agent of the project resumes
or hands off, so only ever while one is connected: memory over 256 KiB; more
than 20 handoffs superseded by later ones; more than 10 facts last found stale,
from the verdicts proxies report; more than 5 open flags. Each adds one task,
written by `memfork`, with exactly what to do and the keys it is about, and
adds it again only after that task is done and the trigger has cleared.
`memfork maintain off` switches it off for a project, in the side file.

The agent claims the task, does the work on a fork, and marks it done naming
the fork in `fork`. MemFork checks the fork by rules alone: it came from the
task's branch; nothing pinned was removed (importance 1, the latest handoff,
unfinished tasks); nothing written in the last 50 commits was removed unless
the task names it; every removed entry is named by a replacement's `replaces`
metadata or by the task; every new key is in one of the project's families
(its own, or decision, fact, handoff, lesson, note, task); and the project got
smaller — or, for stale facts and flags, whose job is to correct rather than
shrink, no bigger. Pass: merged, and the fork removed. Fail: the fork is
discarded with a lesson naming the rule, and the task reopened for another
try. A session left on the fork moves back to the branch.

### 6.14 Prompts, and why not sampling
**[v0.13]** Five MCP prompts, as data in `prompts.toml` — resume, handoff,
review-decisions, tidy-memory, next-task — a few imperative lines each, naming
no model or client, with the session's project filled in and at most one
optional argument. The proxy answers `prompts/list` and `prompts/get` itself,
as it does `tools/list`, so offering them starts no daemon. Checked against
each client's documentation on 2026-09-22: Claude Code, Gemini CLI, VS Code
and Cline show them as slash commands; Claude Desktop, Cursor, Windsurf and
Zed say they support prompts without saying how they appear; Codex CLI and Grok
say nothing.

MCP sampling — asking the client's own model for a summary — is left out. The
specification deprecated it on 2026-07-28 ("new implementations SHOULD NOT
adopt it"), the Rust SDK marks it deprecated, and of the ten clients checked
only VS Code documents supporting it. Nothing in MemFork needed it: the
thinking is done by agents, as tasks.

## 7. Cross-platform requirements
- Targets: x86_64 + aarch64 for linux-musl, apple-darwin, pc-windows-msvc.
- No Unix-only APIs. Daemon uses localhost TCP on all OSes.
- All paths via `std::path` + `dirs`. Tests must pass with spaces and non-ASCII in paths.
- Text files written with `\n`; `.gitattributes` enforces `* text=auto eol=lf`.

## 8. CI and release
- `ci.yml`: matrix `ubuntu-latest`, `macos-latest`, `windows-latest`; steps: fmt, clippy
  (-D warnings), test, `cargo deny check`.
- Release with `dist` (cargo-dist): on tag `v*`, build all six targets and publish a
  GitHub Release. **[v0.8]** Pinned to dist 0.33; every action pinned to a commit
  through `github-action-commits`, because a tag is a name somebody else can move.
  arm64 targets build on native runners rather than being cross-compiled.
- **[v0.8] The installers are written by hand**, not generated. dist's own installers
  "cannot run any kind of custom install logic", and an upgrade needs exactly that:
  stopping the daemon an older MemFork is running before replacing its binary, which
  Windows will not do while it is running. `installers/install.sh` and
  `installers/install.ps1` ship as release assets, so the one-liner in the README
  fetches the script that matches the release rather than whatever is on the default
  branch. They install to a per-user directory, never need administrator, verify a
  SHA-256 and fail closed when it is missing or wrong, honour `MEMFORK_VERSION`, and
  print how to uninstall. `install.ps1` works on Windows PowerShell 5.1 and
  PowerShell 7, on x64 and arm64.
- **[v0.8] Python wheels via `maturin`: eight, not six.** The six targets above are
  *binary* targets, where musl is right because one binary then runs on any
  distribution. A wheel is a different question: a musllinux wheel installs on Alpine
  and nowhere else, so an Ubuntu user would be sent to a compiler. Wheels are built
  for manylinux and musllinux on x86-64 and arm64, macOS on both, and Windows on both,
  plus an sdist. One abi3 wheel per platform covers Python 3.9 and up.
- **[v0.8] One wheel carries both** the `memfork` command and `import memfork`, as
  maturin recommends: a console script entry point into the extension module, rather
  than a second binary in the same wheel. The Python API in 0.1 is the in-memory
  engine only — `memfork-core` has no I/O by design (§4.5) — so it neither writes to
  disk nor sees what an MCP client wrote. Said plainly in the README and the module
  docstring rather than left to be discovered.
- **[v0.8] A release can be rehearsed.** A prerelease tag — `vX.Y.Z-rc.N` —
  builds every binary and wheel and publishes a GitHub prerelease, and
  publishes to neither crates.io nor PyPI. The publish job skips prerelease
  tags, and a job that runs only on a prerelease says so, so the skip is
  observed rather than hoped for. Rehearsal exists because the two registries
  are the only irreversible steps: a crates.io version can never be reused, and
  a PyPI file can never be replaced.

**[v0.8] Release sequence.** The full guide, with commands, is
[`docs/RELEASING.md`](RELEASING.md). In outline:

1. Set the version in the workspace manifest and its internal dependency pins,
   and date the changelog entry. `dist plan --tag vX.Y.Z` confirms the tag and
   the version agree.
2. Optionally, rehearse with a release candidate as above, and delete it once it
   is green.
3. `cargo publish --workspace --dry-run`, then `cargo publish -p memfork-core`,
   then `cargo publish -p memfork` once the index has it. Order matters: the
   binary depends on the library.
4. Push the tag. The binaries, installers and GitHub Release come from
   `release.yml`; the wheels, and the upload to PyPI through a trusted
   publisher, from `wheels.yml`. No token is stored anywhere.
5. Install from the published artefacts on a clean machine and check that every
   route reports the new version.

## 9. What the tests guarantee

The tests are this document in executable form. Each one exists
because breaking it would break a promise, and each is named in the test files
so that a failure says which promise. They are grouped here by what they are
about rather than by when they were written.

**The engine.**

**The engine.**

- **A1** fork is O(1): forking a 1,000,000-key branch takes < 1 ms and allocates < 1 KB.
- **A2** isolation: writes on a fork are invisible on the parent until merge.
- **A3** discard leaves the parent byte-identical (same head CommitId).
- **A4** merge: non-conflicting changes merge cleanly; conflicting keys reported under
  `Fail` with nothing changed; `Ours`/`Theirs` resolve as named.
- **A5** rollback: a dropped `Txn` changes nothing, including seq counters.
- **A6** time travel: `at(branch, n)` returns exactly the state after commit n, for every n
  in a 1,000-commit randomized history (property test, `proptest`).
- **A7** determinism: the same op script yields identical CommitIds on all three OSes
  (golden file checked in CI).
- **A8** concurrency: 8 reader threads + 1 writer per branch across 4 branches, 10 s, no
  panics, no torn reads (every read sees a complete commit).
- **A9** search: exact cosine top-k matches a naive reference implementation; tie order stable.
- **A10** CI green on all three OSes; `cargo deny check` passes.

**The MCP server and client registration.**

- **B1** an MCP client test harness can call every tool and round-trip a fork → write →
  merge and a fork → write → discard flow.
- **B2** `memfork init` registers the server for Claude Code, Cursor, Codex CLI, Gemini CLI
  and Grok Build on the current OS (fixture-tested for all three OSes); re-running
  changes nothing; `--dry-run` writes nothing.
- **B4** `memfork tools --format openai|anthropic|gemini` output validates against each
  vendor's function-calling schema; `memfork call` round-trips every tool.
- **B5** schema-subset CI test passes for every tool.
- **B3** manual: in Claude Code on Windows, the tools appear and the fork/discard demo works.

**Eviction, durability and the shared daemon.**

- **C1** eviction respects budget and score order; evicted entries reach the
  `on_evict` hook.
- **C2** crash recovery: an actual kill (`SIGKILL`, and `TerminateProcess` on
  Windows) mid-write; restart yields a valid state with identical commit ids up
  to the last durable commit. Run on all three OSes, killing a real child
  process rather than simulating one.
- **C4** restart fidelity: write N commits, restart, and `at(branch, n)`
  answers identically for every retained n — not just at the head.
- **C5** the golden commit ids of A7 survive a round trip through the
  log, tying recovery to ids already pinned on three operating systems.
- **C3** two MCP stdio clients on one data dir share state through the daemon on
  all 3 OSes, **including with no daemon started by hand**.
- **C6** two clients starting at the same instant end with exactly one
  daemon and both connected; the losing daemon exits without touching the log.
- **C7** a daemon killed mid-session is restarted once by the proxy and
  the call retried; when that cannot work, a clear MCP error rather than a
  hang. Tested by killing the daemon for real.
- **C8** each connected client keeps its own current branch over the
  shared data: one client's checkout never moves another's.
- **C9** a daemon of a different version is refused by name, pointing at
  `memfork stop`; `memfork doctor` reports the daemon's version.
- **C10** no test can write to the real per-user data directory: the
  binary refuses it under the guard, and a test asserts that no test source
  builds a `memfork` command outside the sandbox that sets it.
  **[v0.7]** Test processes are confined on `PATH` and `MEMFORK_HOME` as well
  as on the data directory, because `memfork doctor` asks each installed
  client whether MemFork is registered and a real client answers by launching
  the registered server — so an unconfined test started a MemFork from outside
  the build. The outcome test watches the real directory for *change* rather
  than for emptiness, since a developer who uses MemFork is supposed to have a
  store there.
- **C11** a session that does `initialize` and `tools/list` and then
  disconnects leaves no daemon, no lock and no new file; the daemon appears on
  the first `tools/call` and not before; a proxy lists exactly what a daemon
  lists; and `memfork doctor` leaves nothing running even when the client it
  asks health-checks the server by launching it.
- **C12** `memfork mcp` exits promptly when its client closes stdin or
  dies, both before and after it has reached the daemon, on all three OSes.

**Distribution.**

- **D1** on a clean machine per OS, following ONLY the README gets a working install and
  MCP registration in under 2 minutes. *(Manual.)*
- **D2** `pip install memfork` works with no compiler on all six targets. 
  Checked by installing each built wheel with `--only-binary :all:` on a runner of
  that architecture and running the Python suite against what was installed. macOS on
  Intel has no hosted runner any more, so that one wheel is checked by its tag and
  contents and the report says so rather than claiming a run that did not happen.
- **D3** the installers install, refuse a download whose checksum does not
  match, and stop a running daemon before replacing its binary — tested on all three
  OSes against a release served from disk, so no published release is needed and no
  test reaches the network.
- **D4** a MemFork installed from a wheel resolves how to launch itself: a
  tool call autostarts a daemon, a second client sees what the first wrote, and the
  command `memfork init` would register is a file that exists and runs.
- **D5** `memfork init` repoints a registration that names another MemFork,
  removing the old one first and saying what it changed; `memfork doctor` reports
  such a registration as not registered and names what it points at.
- **D6** a client that reads `memfork mcp`'s output to the end is not left
  waiting: nothing the proxy starts holds the client's pipe open.
- **D7** `cargo publish --workspace --dry-run` is clean, and the publish jobs
  skip prerelease tags — tested by running the release pipeline on one.

**Handing work between agents.**

- **F1** namespaces: derived from the repository's top level from any depth,
  including a worktree's `.git` file, without running git; sanitised;
  overridden by flag or environment, and an explicit name that is not valid is
  refused before anything starts. Every session's instructions name it.
- **F2** two real MCP clients with different names, started from the same
  repository, share one daemon: the first records decisions and hands off, the
  second resumes, sees what the first wrote and who wrote it, and its own
  handoff is numbered next with the first kept. Two projects in one store do
  not see each other; raw keys are never prefixed.
- **F3** a briefing is deterministic, bounded to 6 KB however much is stored,
  gives up what was done before what comes next, and says clearly when a
  project is empty.
- **F4** attribution: the same write by the same writer is the same commit on
  every OS (a pinned id); a different writer is a different commit; the same
  value written by two writers is not a merge conflict and does not show in a
  diff; callers cannot set `memfork.by`.
- **F5** a data directory written by the 0.1.1 release's own code — a snapshot,
  a log tail, three branches, a merge, a discard — opens with every branch,
  entry and commit id exactly as 0.1.1 read them, is not rewritten by being
  opened, and takes new attributed writes with reproducible, pinned ids.
- **F6** `memfork init --project` writes the block for installed, named or
  all clients, once per shared file; changes only the block; is idempotent;
  gives back the original bytes on `--remove`; keeps CRLF and a byte-order
  mark; refuses outside a repository and on broken markers; never runs git;
  and plain `memfork init` edits no project file.
- **F7** the version appears in exactly the places `docs/RELEASING.md` lists.

**Seeing what happens.**

- **G1** separate commands share the store through the daemon, which the first
  one starts; command-line writes are attributed to `memfork-cli`; a refusal
  is an error; `--ephemeral` runs one command in memory, starting and writing
  nothing; `memfork call` uses the shared store too.
- **G2** `memfork watch` reports connections, disconnections and operations —
  including handoff and resume, with their keys — from two MCP clients and the
  command line, in order, as JSON lines; in words it says "connected"; it waits
  for a daemon rather than starting one.
- **G3** colour: none into a pipe, with `NO_COLOR`, or with `--color never`;
  colour with `--color always` over a pipe, CI and `NO_COLOR`; none under
  `--json` or from `memfork mcp` whatever is asked; no spinner when stderr is
  not a terminal. The decision rules are also tested with a terminal
  simulated.
- **G4** `log --graph` draws forks, merges and discarded attempts, with words,
  identically from the daemon and in memory; the discard record survives a
  replay of the log.
- **G5** a client idle past the daemon's session timeout keeps working.
- **G6** the installers download, check and install the same with progress
  on, and draw no progress bar into a log.
- **G7** a daemon that dies on arrival is reported within seconds, with the
  command tried, the end of its own log and what to do; one that never serves
  is waited for exactly `MEMFORK_START_TIMEOUT` seconds with one progress line;
  a timeout that is not a number is refused by name.
- **G8** a command that has to start the daemon, run with its stdout and
  stderr captured, returns promptly while the daemon runs on: the daemon holds
  neither.
- **G9** on a terminal, `ls` and `at` put each value on one line, cut to the
  width with a marker and a note; `--full` prints them whole; into a pipe every
  value is whole, byte for byte, with or without `--full`; `--json` is
  unchanged.

**Agents working together.** **[v0.12]**

- **H1** of two clients claiming one task, exactly one wins and the other is
  told who holds it; a second session of the same client is refused too and
  told so; a claim outlives its lease while its proxy runs, with no calls, and
  is free within one lease period after the proxy dies. Lease expiry is also
  tested with a clock the test moves.
- **H2** a lesson left on discard reaches the next agent's briefing on the
  parent branch, and nothing else from the fork does.
- **H3** a fact is fresh, stale once its file changes or goes, and fresh again
  once put again; the verdicts are counted.
- **H4** the same put with the same sources has the same commit id whatever
  the files contain, pinned so every OS agrees; the same file named with `\`
  or `/` is one source.
- **H5** a budgeted briefing never exceeds its budget, reports its exact size
  and a token estimate by the stated formula, is byte-identical across runs,
  and is pinned so every OS agrees; checking its facts cannot push it over.
- **H6** text search returns at most `k` hits, ranked, the same every time,
  at 10,000 entries in every run and 100,000 on request.
- **H7** `watch` shows claim, release, lesson and fact events and no renewals;
  `log --graph` shows a lesson beside its discard; `task`, `facts`, `find`,
  `lessons` and `stats` work from the command line with `--json`.
- **H8** stores written by 0.1.x and 0.2.x open unchanged, read back exactly,
  and gain no lesson, no new key and no side file by being read.

**The repository itself.**

The engineering rules are kept in `CONTRIBUTING.md`, addressed to any contributor,
and `AGENTS.md` points there rather than keeping rules of its own. Source
comments state a rule rather than citing one by number, since a number means
nothing to somebody reading the code, and they describe what the code does and
guarantees rather than when it was written.

Which clients MemFork supports is named wherever it belongs — the adapter
registry, `memfork init`, `memfork doctor`, the README. Vendor neutrality
(§6.1) means no client is privileged, not that none is named.

- **E1** no tracked file carries an unfilled template placeholder or a stray
  tooling file. Checked by CI, with the patterns assembled at runtime so that
  the check does not match its own source. **[v0.9]** One client's instruction
  file shares its name with an assistant's personal file. That file is never
  tracked, at any depth; its *name* may appear only in the client registry, the
  tests of `memfork init --project`, the README, this document and the
  changelog, because the product writes into it for that client.
- **E2** comments and documentation contain no internal schedule language.
  Checked by CI, reading this file only as far as the revision record.
- **E3** every document renders in light and dark on GitHub, and uses icons
  rather than emoji. Checked by CI: pictographs and emoji variation selectors
  fail, while typography — an arrow, an em dash, a section sign — does not.
- **E4** somebody who has never read this document can install MemFork,
  register it with a client and run the fork, try, discard loop from the README
  alone.

**Plans, maintenance and secrets.** **[v0.13]**

- **I1** asking who owns a data directory, as fast as possible, never stops a
  daemon taking it or removes the endpoint it publishes; a process that loses
  to a published owner gives up at once.
- **I2** every write tool, and the command line, refuses a credential with the
  rule, field, line and column and never the text, stores nothing, reports it
  in watch without the key, and takes an override naming the rule.
- **I3** two clients work a three-task chain in order; a failed acceptance
  reopens the task with a lesson and a passing one closes it; a command not in
  the plan file never runs; an empty `accept` needs nothing; a cycle and
  another project's task are refused; a plan file holding a secret is refused
  by `plan write`, `plan check` and the `plan` action; watch shows a task
  becoming ready.
- **I4** every built-in template is a valid plan of four to six tasks with a
  command to fill in; one of a person's own is listed and can replace a
  built-in; `plan new` will not overwrite without `--force`.
- **I5** a returning agent's briefing shows what others did while it was away
  and a fact gone stale; `since_last_only` is smaller than the whole; with no
  record, or a rewritten branch, the whole briefing comes back.
- **I6** each kind of flag is found, reported in the answer, the feed, the
  briefing, `memfork flags` and `doctor`, and nothing is resolved.
- **I7** a maintenance trigger adds one task once; a fork that removes a pinned
  entry is discarded with a lesson and the task reopened; a proper tidy is
  merged; switched off, nothing is added. Each rule of the check refuses what
  it should.
- **I8** the proxy offers five prompts without starting a daemon, filled in for
  the project.

Everything above runs on Windows, macOS and Linux in CI, and a change is not
finished until it passes on all three.

## 10. Still to do

Not promises, and not in any order:

- A branch-aware approximate index, so search stops being linear in the number
  of entries with a vector.
- Binding the durable store to Python, so `import memfork` can see what an MCP
  client wrote.
- A TypeScript binding.
- Published benchmarks against the alternatives, measured rather than claimed.

## 11. Revisions

### v0.13 — plans, memory that keeps itself, secrets
1. **A lock race fixed** (§4.5): asking whether a directory is owned could stop
   a daemon taking it.
2. **Secrets stay out** (§6.10), on every write path, with overrides by rule.
3. **Plans** (§6.11): dependencies, readiness, acceptance commands trusted only
   from the repository's plan file, and templates.
4. **What changed since you last looked** (§6.3).
5. **Flags** for duplicates and contradictions (§6.12).
6. **Maintenance tasks**, checked by rules before merging (§6.13).
7. **Prompts**, and sampling left out with the reason (§6.14).

### v0.12 — agents working together
1. **A task board with claims that expire** (§6.5): leases in memory, beside
   the store, so ids never depend on the clock; a proxy keeps its session's
   claims alive; `memfork_task` is the sixteenth and last tool (§6.1).
2. **Search by text** (§6.6), integer-scored, bounded, with measured timings.
3. **Lessons from discarded attempts** (§6.9), written to the parent branch.
4. **Facts that know when they are stale** (§6.7): paths in the commit, hashes
   beside it, checked where the files are.
5. **Briefings by budget** (§6.3): `task` and `budget`, exact bytes and an
   estimated token count with its formula.
6. **Stats beside the store** (§6.8).

### v0.11 — nothing to trip over
1. **The daemon may take a minute to start** (§5), overridable, with one line
   of progress, its own output kept in a log, and a failure that says what
   was tried and what to do.
2. **`ls` and `at` fit a terminal** (§5.2): values on one line each, cut to
   the width with a marker and a note, whole with `--full`, and always whole
   into a pipe.
3. **Acceptance tests G7–G9** (§9): the start timeout and its message, a
   captured command that starts a daemon returning promptly, and listings on
   a terminal, with `--full`, and into a pipe.

### v0.10 — seeing it happen
1. **The command line works on the shared store** (§5), through the daemon and
   the same executor `--ephemeral` uses in memory.
2. **`memfork watch`** (§5.3), from an activity feed the daemon publishes.
3. **History as a tree, clearer branches, coloured diffs** (§5.2), drawn in the
   terminal's own process; discards kept as a short record since their commits
   are freed.
4. **One palette and one set of rules** for colour and motion (§5.2), with
   `--color always` beating the environment and never `--json` or `mcp`.
5. **A forgotten session is replaced** (§5), fixing calls that failed after a
   client had been quiet for longer than the daemon keeps a session; and a
   proxy ends its session when its client goes.
6. **Installer download progress**, only for a person watching (§8).
7. **Acceptance tests G1–G6** (§9).

### v0.9 — handing work between agents
1. **Projects have namespaces** (§6.3), worked out from the repository without
   running git and announced in each session's instructions, so no file has to
   be edited for an agent to learn its project's name. Raw keys stay literal.
2. **Handoff and resume** (§6.3): two tools, fifteen in all, with a bounded,
   deterministic briefing.
3. **Who wrote what** (§6.2, §4.1) is ordinary entry metadata, so neither the
   log format nor the id format changes and a 0.1.x store opens unchanged; it is
   excluded from content equality so writers do not conflict over identical
   values. Branch operations and deletes are not attributed in the store.
4. **One key separator**, the colon, which the tool descriptions already
   taught, rather than introducing a slash for projects.
5. **Project instructions** (§6.4): a managed block in each client's own
   instruction file, the files chosen from registry data.
6. **The hygiene rule on one file name is split** (§9, E1): never tracked as a
   file, nameable only where the product needs it.
7. **The version lives in three places and a test says so** (§9, F7); the doc
   URL that was a fourth was removed rather than documented.
8. **Acceptance tests F1–F7** (§9).

### v0.8 — distribution
1. **Launching is resolved, not assumed** (§5). `current_exe()` is a Python
   interpreter in a wheel install, so the daemon and every client registration
   would have pointed at the wrong program. `MEMFORK_LAUNCH` carries the answer.
2. **A registration pointing at another MemFork is detected and repaired**
   (§5, D5). Installing moves the binary; without this the tools silently stop
   appearing and everything still looks configured.
3. **The installers are hand-written** (§8, D3), because dist's cannot stop a
   running daemon and Windows cannot replace a running executable.
4. **Eight wheels, not six** (§8, D2). musl is right for a binary and wrong for
   a wheel, and "no compiler" has to be true for the Linux most people run.
5. **One wheel carries the command and the library** (§8), with the Python
   API's limits — in memory, not shared with MCP clients — stated rather than
   discovered.
6. **A spawned daemon inherits nothing of the client's** (§5, D6). Two
   answers, because a wheel install is not one process: the binary clears its
   own handles — the only `unsafe` in the workspace, and the reason the lint
   is now `deny` rather than `forbid` — and inside Python the daemon is
   started through `memfork._spawn`, which can refuse to pass anything on.
7. **Releases can be rehearsed** (§8). A prerelease tag proves the whole
   pipeline without touching crates.io or PyPI, the two steps that cannot be
   undone.
8. **Acceptance tests D3–D7** (§9) for the installers, wheel-mode launching,
   stale registrations, the inherited pipe and the publish skip.

### v0.7 — when a daemon starts, and when a proxy stops
1. **The daemon starts on the first tool call, not on the handshake** (§5).
   Clients start `memfork mcp` to health-check it, so a daemon started to
   answer `initialize` was left behind by every probe — including by
   `memfork doctor`, which made `memfork stop; memfork doctor` restart what it
   had just stopped. The proxy answers `initialize` and `tools/list` from the
   same static registry the daemon serves.
2. **A proxy exits with its client** (§5), stated rather than assumed, so
   proxies cannot accumulate.
3. **Acceptance tests C11 and C12** (§9) for both, with real processes.
4. **Tests are confined on `PATH` and the home directory too** (§9, C10). A
   test that ran `memfork doctor` with the developer's own `PATH` reached the
   real client on the machine, which health-checked MemFork by launching the
   *installed* binary against the test's data directory — and the suite hung
   waiting on it. Isolation has to cover every direction a command can reach
   out in, not just where its data goes.
5. **Doctor's persistence note describes the daemon** rather than the earlier single-process
   one-process-at-a-time behaviour, as do the MCP server instructions. Both
   were stale for a whole revision; both now have a test that fails on the old wording,
   because prose that only a human reads is prose that drifts.

### v0.6 — sharing one store
1. **Every persistent `memfork mcp` is a proxy** (§5), and starts the daemon
   itself if there is none. Requiring a user to run `memfork serve` by hand
   would make the ordinary case — two clients — look like a failure.
2. **The daemon exits when idle** (§5, default 600s), because an autostarted
   background process that never stops is one somebody has to hunt down.
3. **`memfork stop`** (§5) shuts it down gracefully, and is the answer given
   whenever a version mismatch is reported.
4. **The endpoint file carries the MemFork version** (§4.5), and a client that
   finds a different one refuses to talk to it. An endpoint with no version is
   a mismatch, not an assumption of compatibility.
5. **A guard makes test isolation structural** (§4.5, C10). Twice, a change of
   default turned harmless tests into ones that wrote into the developer's own
   store. Being careful was not a control; `MEMFORK_FORBID_PER_USER_DATA_DIR`
   and a test that greps the test sources are.
6. **Acceptance tests C6–C10** (§9) for the race, the daemon dying, per-client
   branches, version skew and the isolation guard.

### v0.5 — durability
1. **Durability is on by default in the binary, off by default in the library**
   (§4.5). Two different callers with two different expectations: a library
   should not write to a disk it was not asked to, and a memory server that
   forgets on restart is not doing its job.
2. **The data-directory question from v0.4 is settled** (§4.5): resolved by
   hand, no crate, no MPL. Locking likewise needs no crate, since
   `std::fs::File::try_lock` is stable.
3. **The WAL format is specified** (§4.5): magic, version, per-record checksum
   over length and payload, capped lengths, and a torn tail treated as normal
   rather than as corruption.
4. **The fsync policy is configurable and its default is justified** (§4.5),
   with what it costs and what it risks written down rather than left to be
   discovered.
5. **Single-writer enforcement uses two files** (§4.5). The lock file is empty
   and the endpoint file is never locked, because on Windows an exclusive lock
   blocks reads — so a token in the locked file would be unreadable by the
   client that needs it.
6. **Snapshots sit at the retention horizon** (§4.4), so a restart does not
   shorten what time travel can reach.
7. **Durability and the daemon are settled separately** (§9), with two acceptance tests added
   for restart fidelity and the golden-file round trip.

### v0.4 — after the MCP server
1. **Repository standards** (§9): the files a contributor expects, wording
   aimed at users rather than at the people who built it, icons rather than
   emoji, and a logo. None of it changes behaviour, and all of it is the
   difference between a project that works and one someone else can pick up.
2. **The data-directory dependency is an open decision** (§4.5). v0.2 named
   `dirs`; `dirs` pulls in `option-ext`, which is MPL-2.0 and outside the
   licence policy. The MCP server sidestepped it by needing only the home
   directory; durable storage needs a real per-user data directory and has to
   choose.

### v0.3 — after the engine
The engine is built and accepted; this revision records where the implementation
and v0.2 disagreed, and why the implementation is right.

1. **`rpds` replaces `imbl`** (§4.1). `imbl` is MPL-2.0, outside the licence
   policy in the project rules. `rpds` is MIT and gives the same guarantees.
   The `trait Store` seam v0.2 already called for is what made the swap a
   one-file change.
2. **The commit id covers the message** (§4.1):
   `blake3(parents ‖ message ‖ ops)`, with `None` distinct from `Some("")`.
   Under v0.2's definition two commits with the same parents and ops but
   different messages collided, so a message was content a reader could see and
   the content address could not.
3. **Reads do not touch `last_access_seq`** (§4.1), and access tracking becomes
   a side structure outside the commit chain (§4.4). Updating it on read
   would turn every read into a write, break "readers never block", and make
   commit ids depend on read traffic.
4. **`memfork run` and `memfork search` are part of the CLI** (§5). `run` exists
   because durability came later; `search` because the CLI could not otherwise
   exercise §4.2.
5. **Merge has three outcomes** (§4.2). Fast-forward when the target has not
   moved, and no commit at all when the plan is empty, rather than recording
   history that says nothing happened.
6. **A per-branch writer lock** (§4.3) for the read-modify-write operations the
   engine drives — auto-commits and merges. Explicit transactions stay
   optimistic exactly as v0.2 describes. Found by CI: under more writer threads
   than cores, an auto-commit could exhaust its retry budget and report
   `Conflict` for a write that should have succeeded.

Unchanged and still deferred at that point: eviction (§4.4) and durability
(§4.5), and the branch-aware approximate index, which is still to do.
