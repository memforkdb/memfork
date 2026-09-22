# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Agents working together: a task board with claims, plans agents work in
order, lessons from abandoned attempts, facts that know when they are stale,
briefings that fit a budget and say what changed since you last looked,
duplicates and contradictions flagged, memory that asks to be tidied, and
credentials kept out. Stores written by 0.1.x and 0.2.x open unchanged.

### Added

- **`memfork_task`, a task board.** Add a task, claim it, mark it done, release
  it, list the board. Of two agents claiming one task, one gets it and the
  other is told who has it, even when both are sessions of the same tool. A
  claim lasts five minutes by default, up to an hour; `memfork mcp` renews its
  session's claims for as long as it runs, and a crashed session's claims run
  out within one lease. Claims are kept by the running server, outside memory's
  history, so they never change a commit id. This is the sixteenth tool, and the
  last: MemFork keeps to at most sixteen.
- **Text search.** `memfork_search` takes `text` as well as `embedding`, with
  integer scoring that ranks the same on every OS, at most 50 hits, and a
  snippet for each. `memfork find <text>` on the command line.
- **Lessons.** `memfork_discard` with a `lesson` keeps one line of what the
  attempt taught on the branch it came from, as `<project>:lesson:<n>`; the
  next agent's briefing includes it and `memfork log --graph` shows it beside
  the discarded branch. `memfork discard --lesson` and `memfork lessons`.
- **Facts with sources.** `memfork_put` with `sources`, a list of files in the
  repository, makes the entry a fact. Reads say `fact: fresh` while those files
  are unchanged and `fact: stale`, with the files, once they change. Files are
  hashed where they are, by `memfork mcp` or the command line; only the paths
  enter memory's history, so the same fact has the same id everywhere.
  `memfork put --source` and `memfork facts`.
- **Briefings by budget.** `memfork_resume` takes `task`, to put what matches
  it first, and `budget`, the most bytes it may take (1024 to 65536). It never
  goes over, and reports its exact size in bytes and an approximate token
  count, with the formula. It now includes recent lessons and facts.
- **`memfork stats`**: briefings and their bytes against the memory they
  summarised, lessons kept and served, facts fresh and stale, claims won and
  lost, per project and tool. Kept in `memfork-sidecar.json`, outside memory's
  history.
- `memfork watch` shows claims, releases, finished tasks, lessons and fact
  checks. Renewals are not shown.
- `docs/BENCHMARK.md`: a protocol for measuring MemFork against no MemFork on
  the same task, with a runner skeleton in `scripts/benchmark/`. No results yet.

- **Plans.** Tasks may name the tasks they depend on and an acceptance
  command; a task is ready when everything it depends on is done. `memfork_task`
  takes `plan` to write several tasks at once, refusing cycles, and lists
  `ready` and `blocked` tasks. Marking a task done runs its acceptance command
  in the project, with a time limit: passing closes it, failing reopens it with
  a lesson. A command runs only if the repository's plan file holds it, so
  nothing written into shared memory can run a command on its own. `memfork
  plan write|check|show`, and `memfork plan new --template` with five
  templates: feature, bugfix, refactor, upgrade, tests.
- **What changed since you last looked.** Every briefing starts with
  `since_last`: decisions, lessons, handoffs by others, tasks that changed
  hands or finished, and facts gone stale since this tool last looked.
  `since_last_only` returns just that.
- **Flags.** Decisions made differently on different branches by different
  tools, the same value under near-identical keys, and facts from the same
  files that disagree are reported in the write's answer, in briefings, in
  watch, in `memfork flags` and in `memfork doctor`, and never resolved by
  MemFork.
- **Maintenance tasks.** When a project's memory grows large, handoffs pile
  up, facts go stale or flags accumulate, MemFork adds one task saying what to
  tidy. An agent does it on a fork; MemFork checks the result by fixed rules
  and merges it, or discards it with a lesson. `memfork maintain off` switches
  it off for a project.
- **Credentials are refused.** Writes through the tools, the command line and
  plan files are checked for private keys, well-known token shapes and
  passwords. A match stores nothing and says which rule matched and where,
  never what; `allow_secret` (`--allow-secret`) writes a false positive by
  naming its rule.
- **MCP prompts** for the routines: resume, handoff, review-decisions,
  tidy-memory, next-task.
- **The event stream is a documented contract.** Every line `memfork watch
  --json` prints carries `schema: 1`; the fields, the operation names and the
  rule for changing them are in `docs/EVENTS.md`, held by a test.

### Fixed

- **A daemon could fail to start while something asked whether one was
  running.** Asking takes the directory lock for a moment, and a daemon trying
  for it at that moment exited with "in use"; a stale endpoint could also be
  removed just after a new daemon published it. Taking the lock now waits out
  a question, and a question removes a stale endpoint only while it holds the
  lock.

### Changed

- The instruction block `memfork init --project` writes now also says: name
  the task when resuming, search with `text`, claim tasks, store findings with
  their sources, and leave a lesson when discarding.
- MCP sampling is not used: the specification has deprecated it.

## [0.2.1] - 2026-09-21

A robustness release: nothing new, and less for a first user to trip over.

### Fixed

- **The daemon may take a minute to start.** The wait was a fixed 20 seconds,
  which a slow machine — an antivirus scanning a new executable, say — could
  run out on a first start. It is now 60 seconds, and `MEMFORK_START_TIMEOUT`
  sets another. A start that takes more than a few seconds says so once.
- **A daemon that cannot start says why.** Its own error output now goes to
  `memfork-daemon.log` in the data directory instead of being lost. If it exits
  without serving, the command fails within seconds rather than waiting out the
  timeout, and the message names the command that was tried, quotes the end of
  that log, and says what to do next.
- **`memfork ls` fits the terminal.** On a terminal each value is shown on one
  line and cut to the width, with a marker and a note; `--full` shows them
  whole. Into a pipe or a file every value is printed whole, exactly as before,
  and `--json` is unchanged. The same goes for `memfork at` listing a branch.
- **`memfork init` with no clients says what to do** instead of "Nothing to
  change", and `memfork doctor` reports a client that is not there as not
  installed rather than unknown.
- **`install.sh` explains a failed unpack**: `.tar.xz` needs xz support, which
  slim Linux images often lack.
- The README's "Try it" example has a PowerShell version, says what to do if
  `memfork` is not found after installing, and says what `cargo install` needs.

## [0.2.0] - 2026-09-21

Agent handoff becomes a first-class workflow, and you can see it happen. One
agent can stop mid-task and leave a handoff; another, from any vendor, resumes
from it. Stores written by 0.1.x open unchanged.

### Added

- **Handing work between agents.** Two new tools: `memfork_resume` gives an
  agent a short, bounded briefing on a project when it starts — the latest
  handoff, recent decisions, open tasks — and `memfork_handoff` records where
  the work stands before it stops. Any client can resume what any other handed
  off.
- **Project namespaces.** Each session works in a namespace taken from the
  repository it was started in, and is told it when it connects. Set it with
  `memfork mcp --namespace` or `MEMFORK_NAMESPACE`. Keys are still literal;
  nothing is prefixed for you.
- **Who wrote what.** Every entry a client stores records the client's name,
  and briefings, `branches` and `log --graph` show it.
- **`memfork init --project`** writes one managed block into each client's own
  instruction file in a repository — resume when you start, record decisions,
  hand off before you stop, fork before anything risky — with `--client`
  (repeatable), `--all`, `--remove` and `--dry-run`. Only the block is ever
  touched, and it never runs git.
- **`memfork watch`** shows what every client is doing as it happens: time,
  client, operation, key or branch, including handoffs and resumes. `--json`
  prints one object per line.
- **`memfork log --graph`** draws every branch as a tree, with forks, merges and
  discarded attempts.
- **Colour**, from one palette, with a word beside every state and ASCII
  stand-ins for every glyph. `--color auto|always|never`; `--json` and
  `memfork mcp` are never coloured.
- **Download progress** in both installers, when a person is watching.

### Changed

- **The command line works on the shared store.** `put`, `get`, `ls` and the
  other operations, and `memfork call`, now act on the same store your MCP
  clients use, through the local server, which they start if needed.
  `--ephemeral` runs one against a fresh in-memory database, exactly as they
  all did before. `--ephemeral` and `--data-dir` are now accepted before or
  after any subcommand.
- `memfork branches` says how far each branch is ahead of or behind the default
  branch, where it forked and who wrote to it last; `memfork diff` marks and
  colours each change.
- `memfork ls` and `memfork at` line values up in one column, padded to the
  widest key, instead of separating them with a tab. `--json` is unchanged.
- Tool descriptions teach one key convention, `<project>:<kind>:<id>`.
- `memfork init --client` may be given more than once.
- The README lists `cargo install memfork` beside `pip install memfork`.

### Fixed

- A client that had been quiet for longer than the server keeps a session got
  an error on its next tool call. The session is now replaced and the call
  goes through.
- The README listed `list` and `delete`; the commands are `ls` and `del`.

## [0.1.1] - 2026-09-21

Fixes to the installers, found on the first real upgrade on Windows. The
engine, the binary and the Python package are unchanged apart from the version.

### Fixed

- **install.ps1 no longer closes your terminal.** Run as `irm ... | iex`, it
  executes inside your PowerShell session, and a failure used to call `exit`,
  which ended that session. It now prints the error, says MemFork was not
  installed, and returns. It also no longer leaves its strict mode, error
  preference, functions or variables behind in your session; the only change
  it makes to your session is the PATH entry it reports.
- **A blocked upgrade on Windows now says what is blocking it.** When the
  installed binary is still in use, the installer names each application
  running MemFork and what to do, for example "Claude Code (pid 30748) is
  using MemFork. Close it, then run this installer again." The daemon is still
  stopped automatically; client applications are never closed by the
  installer.
- **install.sh cannot change or close a shell that sources it.** The documented
  way to run it is `curl ... | sh`, which was already safe; its body now runs
  in a subshell, so sourcing it by mistake cannot leave `set -eu` on or exit
  the shell.

## [0.1.0] - 2026-09-21

The first release.

### Added

- **Branchable memory.** Fork a branch in constant time whatever it holds,
  merge it three-way at key level, or discard it and leave the parent exactly
  as it was. Branches share structure rather than copying, so forking a
  million-key branch takes under a millisecond and allocates under a kilobyte.
- **Time travel.** Read any branch as it was at any earlier point, and fork
  from there.
- **Transactions** over many keys, with real rollback.
- **Search** by vector similarity, exact and deterministic: the same query
  ranks the same way on every machine.
- **Semantic eviction.** Entries are kept or dropped by importance and
  recency together, not by insertion order, with a hook that reports what went.
- **Durable storage.** A write-ahead log with per-record checksums, snapshots
  taken at the retention horizon so a restart never shortens what time travel
  can reach, and recovery that reproduces identical commit ids.
- **An MCP server** with thirteen tools, over stdio, for any client that
  speaks the protocol.
- **A shared daemon.** Several clients use one store at once and see each
  other's writes, while each keeps its own current branch. It starts when
  something first needs it and exits when nothing has for a while.
- **`memfork init`** registers the server with the MCP clients you have,
  preferring each client's own command and editing a configuration file only
  when a client ships none — changing that client's MemFork entry and nothing
  else, with a backup beside it.
- **`memfork doctor`** reports what is installed, what is talking to it, and
  where memory is kept.
- **Prebuilt binaries** for Linux, macOS and Windows on x86-64 and arm64, with
  installers that need no administrator and stop a running daemon before
  replacing it.
- **`pip install memfork`**, one wheel carrying both the `memfork` command and
  the engine as a Python library, for Python 3.9 and newer.

### Known limits

- The Python library is in-process memory only. It does not see the store an
  MCP client uses; that one belongs to the daemon.
- Search is exact rather than approximate, so it is linear in the number of
  entries with a vector.
- The daemon is local only: `127.0.0.1`, token-protected, never a network
  interface.
- File permissions on the daemon's endpoint file differ between Unix and
  Windows. See [SECURITY.md](SECURITY.md).

[Unreleased]: https://github.com/memforkdb/memfork/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/memforkdb/memfork/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/memforkdb/memfork/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/memforkdb/memfork/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/memforkdb/memfork/releases/tag/v0.1.0
