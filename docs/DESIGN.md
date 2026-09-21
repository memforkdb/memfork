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
- `Key`: UTF-8 string, max 1024 bytes. Convention `namespace:id`.
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
  - `meta: BTreeMap<String,String>`
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
- `memfork doctor` — print version, data dir, lock status, detected tools, and whether
  each tool's config contains the MemFork entry.
- `memfork put|get|del|ls|search|fork|merge|discard|branches|log|at|diff` — thin CLI
  over the core for humans and scripts. **[v0.3]** `search` is included so the CLI
  can exercise §4.2's search; every command takes `--branch` (default `main`) and
  `--json`.
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

## 6. MCP tools
All tools take an optional `branch` (default: the session's current branch).
```
memfork_put       key, value, importance?, embedding?, ttl_commits?, meta?
memfork_get       key
memfork_delete    key
memfork_list      prefix?, limit?
memfork_search    embedding, k?, prefix?
memfork_fork      name, from?, at_seq?
memfork_checkout  name                 # sets the session's current branch
memfork_merge     source, target?, policy?   # fail | ours | theirs
memfork_discard   name
memfork_branches
memfork_log       limit?
memfork_at        seq, key? | prefix?   # time-travel read
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
- **[v0.8] Nothing irreversible before the repository is public.** The pipeline is proved with a
  prerelease tag, which builds every binary and wheel, publishes a GitHub prerelease,
  and publishes to neither crates.io nor PyPI. The publish jobs skip prerelease tags,
  and a job that runs on a prerelease says so, so the skip is observed rather than
  hoped for. The history rewrite that precedes going public would otherwise be
  impossible: publishing puts a version on a registry permanently, and a
  crates.io version can never be reused.

**[v0.8] Release runbook.** In this order. Only the first step happens before
the repository is made public:

1. `git tag v0.1.0-rc.1 && git push origin v0.1.0-rc.1` — proves the pipeline:
   six binaries, eight wheels, both installers, a GitHub prerelease, nothing
   published. Delete the tag and its release afterwards, because the history
   rewrite below changes the commit it points at.
2. The repository is made ready to be public: history rewritten, build-process
   references removed, the files a contributor expects added.
3. `cargo publish --workspace --dry-run`, then `cargo publish -p memfork-core`,
   then `cargo publish -p memfork` once the index has it. Order matters: the
   binary depends on the library.
4. PyPI: a trusted publisher for `memforkdb/memfork`, workflow `wheels.yml`,
   environment `pypi`. No token is stored anywhere.
5. `git tag v0.1.0 && git push origin v0.1.0` — the real release. The publish
   jobs run this time.

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

**The repository itself.**

MemFork was built with help from an AI coding assistant. That is a fact about
how it was made rather than about what it is, and the repository is written to
read that way: the engineering rules live in `CONTRIBUTING.md`, addressed to
any contributor; `AGENTS.md` points there and adds nothing of its own; and
source comments state a rule rather than citing one by number, since a number
means nothing to somebody reading the code.

Which clients MemFork *supports* is a separate matter, and they are named
wherever they belong — the adapter registry, `memfork init`, `memfork doctor`,
the README. Vendor neutrality (§6.1) means no client is privileged, not that
none is named.

- **E1** no tracked file names the assistant's instruction files or carries a
  co-author trailer. Checked by CI, with the patterns assembled at runtime so
  that the check does not match its own source.
- **E2** the history carries no co-author trailer either.
- **E3** every document renders in light and dark on GitHub, and uses icons
  rather than emoji. Checked by CI: pictographs and emoji variation selectors
  fail, while typography — an arrow, an em dash, a section sign — does not.
- **E4** somebody who has never read this document can install MemFork,
  register it with a client and run the fork, try, discard loop from the README
  alone.

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
7. **Nothing irreversible before going public** (§8). A prerelease tag proves the
   pipeline; crates.io and PyPI come after the history rewrite, because neither
   can be undone.
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
1. **A round of polish** (§9) now sits between the working software and the
   first public release, and what used to follow it became "still to do".
   The work is separating the build process from the product, the repository
   files a developer expects, user-facing wording, icons in place of emoji, and
   a logo. None of it changes behaviour, and all of it is the difference between
   a project that works and one someone else can pick up.
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
