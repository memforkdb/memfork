# The event stream

`memfork watch --json` prints what the daemon does as it happens, one JSON
object per line, for other tools to ingest. This page is the contract for
those lines. The shape has a version, `schema`, and the rule for changing it
is at the end.

```sh
memfork watch --json            # every line, until interrupted
memfork watch --json --count 50 # then exit
```

The same lines are what the daemon serves at `/events` on its loopback
listener, behind the bearer token from the endpoint file; `memfork watch` is
a reader of that endpoint and nothing more. Watching does not count as
activity for the daemon's idle timeout, and `watch` waits for a daemon rather
than starting one.

## The first line: `hello`

Sent once per connection, before any event.

| Field | Type | Meaning |
|---|---|---|
| `schema` | integer | The version of every line in this stream. Currently `1`. |
| `kind` | string | Always `hello`. |
| `version` | string | The daemon's MemFork version. |
| `port` | integer | The loopback port it listens on. |
| `clients` | array | Every client session connected right now, in the order they connected. Each has `client` (the name a person knows) and `namespace` (its project). |

```json
{"schema":1,"kind":"hello","version":"X.Y.Z","port":51823,"clients":[{"client":"Claude Code","namespace":"shop"}]}
```

## Every line after it: an event

| Field | Type | Present | Meaning |
|---|---|---|---|
| `schema` | integer | always | The version of this line's shape. Currently `1`. |
| `time` | string | always | When, as RFC 3339 in UTC to the millisecond, for example `2026-09-22T14:03:07.412Z`. |
| `kind` | string | always | `connected`, `disconnected` or `operation`. |
| `client` | string | always | Who did it, as a person knows the client: `Claude Code`, `Codex CLI`, `memfork-cli` for the command line, `memfork-autopilot` for what autopilot did on its own. A client the registry does not know keeps the name it gave. |
| `client_id` | string | when it differs from `client` | The name the client gave in MCP `initialize`. |
| `namespace` | string | when known | The project the session works in. |
| `operation` | string | `kind` is `operation` | What was done; see below. |
| `key` | string | when one key was involved | The key it was done to. Absent from a write refused for holding a secret, which may be where the secret was. |
| `branch` | string | when a branch was involved | The branch it was done on, or the branch it created or removed. |
| `detail` | string | when the operation needs it | A word or two more; see below. |
| `ok` | boolean | always | Whether it succeeded. A claim that lost, or a `done` whose acceptance failed, is `false`. |
| `error` | string | when `ok` is `false` and there was a message | Why not. Never contains a value that looked like a secret. |

Absent fields are left out of the line rather than set to `null`.

```json
{"schema":1,"time":"2026-09-22T14:03:07.412Z","kind":"connected","client":"Claude Code","client_id":"claude-code","namespace":"shop","ok":true}
{"schema":1,"time":"2026-09-22T14:03:09.001Z","kind":"operation","client":"Claude Code","client_id":"claude-code","namespace":"shop","operation":"put","key":"shop:decision:payments","branch":"main","ok":true}
{"schema":1,"time":"2026-09-22T14:03:12.770Z","kind":"operation","client":"Codex CLI","client_id":"codex-mcp-client","namespace":"shop","operation":"claim","key":"shop:task:3","branch":"main","detail":"held by Claude Code","ok":false}
{"schema":1,"time":"2026-09-22T14:04:00.115Z","kind":"disconnected","client":"Claude Code","namespace":"shop","ok":true}
```

### `operation`

An MCP tool call is reported under the tool's name without its `memfork_`
prefix: `put`, `get`, `delete`, `list`, `search`, `fork`, `checkout`,
`merge`, `discard`, `branches`, `log`, `at`, `diff`, `resume`, `handoff`.
`memfork_task` is reported under its action instead: `add`, `claim`,
`release`, `done`, `list`, `plan`. A renewal of a claim is not reported.

A command-line operation is reported under the subcommand's name (`put`,
`fork`, `task`, `plan`, and so on), with `client` set to `memfork-cli`.

Some operations produce a second line of their own:

| `operation` | When | `key` / `branch` | `detail` |
|---|---|---|---|
| `lesson` | a discard left a lesson | the lesson's key; the branch it was written to | |
| `fact` | a fact was checked against its source files | the fact's key | `fresh`, `stale` or `unverified` |
| `flag` | a write looked like a duplicate or a contradiction | the key written | `same value as <key>` or `decided differently on <branch>` |
| `ready` | finishing a task made another ready | the task now ready | |
| `maintain` | MemFork added a maintenance task | the task's key | the trigger: `size`, `handoffs`, `stale_facts` or `flags` |

Autopilot reports what it did with `client` set to `memfork-autopilot`:

| `operation` | When | `key` / `branch` | `detail` |
|---|---|---|---|
| `follow` | memory followed a git switch | the branch memory is now on | `switched`, or `forked from <branch>` |
| `merge` | memory merged because git did, or an autopilot fork was merged after its action worked | `<source> -> <target>` | `git merge <branch>`, `exit 0`, or `` `<check>` passed `` |
| `fork` | memory was forked before a risky action | the fork | `<rule>: <command>`, or `edits: ...` |
| `discard` | an autopilot fork was discarded after its action failed; a `lesson` line follows | the fork | `<rule>: <command>` |
| `autopilot` | anything else it has to say | the branch or fork involved | `detached HEAD`, `kept: no check configured`, `conflict: <source> into <target>` (with `ok` false), `<n> sessions share a fork` |

### `detail`

Besides the values above: `held by <client>` on a claim that lost,
`acceptance failed; reopened` or `not merged; reopened` on a `done` that did
not close the task, and `secret refused: <rule>` on a write that was refused.

## Reading it

Each line is complete on its own, so a reader may start anywhere and skip
lines it does not understand. Read `kind` first, then `operation`. Ignore
fields you do not know: a later `1.x` line may carry more of them.

The daemon keeps a bounded backlog per reader; a reader that falls far behind
misses events rather than stalling the daemon, and a reader that reconnects
gets a new `hello` and only what happens from then on.

## The rule for changing it

Within one `schema` value a field may be **added**, and a new `operation` or
`detail` value may appear. A field is never removed, renamed or given a new
meaning, and no existing value changes meaning. Any of those bumps `schema`,
and the old shape is described here alongside the new one for at least one
release.

A test in `crates/memfork/src/events.rs` holds the field set of version 1
and fails when it drifts, and another fails if a field is not described on
this page.

There is no OpenTelemetry exporter. The Rust SDK is not a light dependency,
and this stream is the integration point: anything that can read a line of
JSON can forward it.
