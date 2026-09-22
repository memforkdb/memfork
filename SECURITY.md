# Security

## Reporting a vulnerability

Please report privately, through GitHub, rather than in a public issue:

1. Go to the [Security tab](https://github.com/memforkdb/memfork/security) of
   this repository.
2. Choose **Report a vulnerability**.

That opens a private advisory visible only to the maintainers. Include what you
did, what happened, and the platform you saw it on — MemFork behaves the same
on Windows, macOS and Linux by design, so a difference between them is itself
worth reporting.

You should get a first reply within a week. If a report is confirmed, a fix and
an advisory go out together, and you are credited unless you would rather not
be.

## Supported versions

MemFork is pre-1.0. The latest released version is the supported one: fixes go
into a new patch release rather than back into older ones.

| Version | Supported |
|---|---|
| the latest 0.x | Yes |
| anything older | No |

## Threat model

Written for the person who has to say yes or no to installing MemFork on a
machine they are responsible for. Each section says what MemFork does, what it
touches, and what it cannot promise.

### What runs, and what listens where

MemFork is one executable. It runs in three shapes:

- **`memfork mcp`**, started by an AI tool as its MCP server, one per tool
  session. It speaks MCP over the tool's own pipes. It is a proxy: on the first
  tool call it connects to the daemon below, starting one if none is running.
  It exits when the tool closes the pipe.
- **The daemon** (`memfork serve`, started for you). One per data directory.
  It owns the store, listens on **`127.0.0.1` only**, on a port the operating
  system assigns at each start, and requires a bearer token on every request.
  The port and token are in `memfork.endpoint` in the data directory; a process
  that cannot read that file cannot talk to the daemon. It exits after ten
  minutes with nothing to do.
- **The command line** (`memfork put`, `memfork watch`, and so on), which
  connects to the daemon the same way, or with `--ephemeral` runs alone in
  memory and touches no file.

Nothing binds any other address, and there is no flag to make it. The listener
rejects a `Host` header that is not loopback, as a second line against DNS
rebinding.

### Network: MemFork itself sends nothing anywhere

**No telemetry, no update check, no account, no cloud.** The only network
connections MemFork makes are to its own daemon on `127.0.0.1`. The only code
in the workspace that can open a socket is in four modules of the `memfork`
crate, each named here because a test (`crates/memfork/tests/no_network.rs`)
fails if any other file gains that ability or if one of these names a host
that is not loopback:

| Module | What it connects to |
|---|---|
| `serve.rs` | binds the daemon's listener on `127.0.0.1` |
| `client.rs` | the command line's connection to that listener |
| `proxy.rs` | a `memfork mcp` proxy's connection to that listener |
| `daemon.rs` | the probe that asks a running daemon to stop |

`memfork-core`, the engine, has no I/O at all. The Python package binds the
engine only. There is no HTTP client and no TLS stack in the dependency tree,
and the same test fails if one appears.

The exceptions are the ones you would expect, and they are not MemFork
running: the **installers** (`install.sh`, `install.ps1`) download a release
from GitHub, or from the mirror `MEMFORK_DOWNLOAD_BASE` names; `pip` and
`cargo` fetch what they install. A CI workflow (`no-network.yml`) runs the
whole command surface — the daemon, `memfork mcp`, the command line, and the
installers pointed at a release served from disk — on each operating system
with outbound traffic blocked at the firewall, and fails if anything needed
the outside. Features that do not exist yet are covered the same way as they
land.

What an *agent* does with what it reads is another matter, and the README says
so plainly: memory an agent reads becomes part of that agent's prompt and goes
wherever that agent already sends your code. MemFork does not change that in
either direction.

### What is stored, and where

Memory is stored in a per-user data directory, in plain form:

| OS | Default data directory |
|---|---|
| Windows | `%LOCALAPPDATA%\memfork` |
| macOS | `~/Library/Application Support/memfork` |
| Linux | `$XDG_DATA_HOME/memfork`, else `~/.local/share/memfork` |

`MEMFORK_DATA_DIR` or a project's own `.memfork` directory moves it; the
machine policy (below) can pin it for everyone. It is not encrypted, and it is
readable by anything running as you. Treat it as you would any other file in
your home directory.

In it: the write-ahead log and snapshots (memory itself, every branch and
every retained commit); `memfork-sidecar.json`, which holds usage counts and
the blake3 hashes of files that facts were recorded from, with their paths
relative to the project and no file contents; `memfork.endpoint`, with the
daemon's port and token, and `memfork.lock`; and `memfork-daemon.log`, the
daemon's own diagnostics, rewritten by each start. Claims on tasks are kept
only in the running daemon's memory.

`memfork doctor` prints the directory in use and why it was chosen.

### What each feature can touch

- **Reading files.** To check whether a fact is still true, `memfork mcp` and
  the command line read the files a fact names, inside the project, to hash
  them. A path that is absolute or climbs out with `..` is refused when the
  fact is stored, at most the first 8 MiB of a file is read, and one answer
  reads at most 64 MiB. Nothing is read unless a stored fact names it. The
  daemon has no working directory and reads no project file; its `/report`
  endpoint, behind the same token, takes only the verdicts (fresh, stale,
  unverified) to count.
- **Running commands.** A plan's task may carry an acceptance command. It runs
  on your machine, in the project, when an agent marks the task done, through
  `memfork mcp` or the command line — never in the daemon — with a time limit,
  and everything it started is stopped if it runs over. Because tasks are kept
  in memory every tool shares, a command runs only if the repository's own plan
  file (`memfork-plan.toml`, or the file the plan was written from) holds the
  same command for the same task. An agent that writes a command into memory
  cannot make another tool run it; changing the plan file is an edit to your
  repository like any other. A plan file outside the project may not carry
  commands. The command runs with the permissions and environment of the
  client that started `memfork mcp`. Nothing else in MemFork executes anything
  it was not given on its own command line.
- **Writing your tools' configuration.** `memfork init` registers MemFork with
  the MCP clients on the machine: through each client's own `mcp add` command
  where it has one, otherwise by editing the client's configuration file,
  changing only the MemFork entry, keeping a timestamped backup, and never
  touching a file the registry does not name. Claude Code's user file, which
  holds its session, is never read or written. `memfork init --project` writes
  one marked block into the instruction files of a repository, and `--remove`
  takes only that block out. `--dry-run` shows every change first. Nothing
  else MemFork does writes outside the data directory.
- **Git.** MemFork never runs `git`. It finds a repository by looking for
  `.git` and reads nothing inside it.
- **Credentials.** Every write is checked against rules for private keys,
  well-known token shapes and passwords, and a match is refused. The refusal,
  the watch feed and every log name the rule and the place, never the matched
  text. It is a safety net for mistakes, not a guarantee: a secret in a shape
  no rule knows is stored like anything else. The rules are in
  `crates/memfork/src/secret_rules.toml`; `allow_secret` writes a false
  positive by naming its rule, and the machine policy can forbid that.

### The machine policy

An administrator can place one file that every MemFork on the machine obeys
and no user setting overrides: `%ProgramData%\memfork\policy.toml`,
`/Library/Application Support/memfork/policy.toml` or
`/etc/memfork/policy.toml`. It can switch off the dashboard, race, autopilot,
maintenance tasks, sampling and secret overrides, and pin the data directory.
A file that cannot be read stops every command except `memfork doctor`, which
reports it, rather than run with a rule it does not understand. The README
documents the keys; `memfork doctor --verbose` shows what is in force and
which file said so.

### Not in this version

The dashboard (`memfork ui`), autopilot and `memfork race` are not in this
release. The policy already knows their names so that a policy written today
holds when they arrive, and this document gains a section for each as it
lands, saying exactly what it touches and what it cannot contain.

## Two things stated plainly

**There is one `unsafe` block.** It is in `crates/memfork/src/daemon.rs`, in
`windows_handles`, and it is three calls to the Windows API:
`GetStdHandle` and `SetHandleInformation` for each of the three standard
handles. It exists because a new process on Windows inherits every inheritable
handle its parent holds, so a daemon started from a client's session would
otherwise hold that client's pipes open for as long as it ran. The standard
library offers no safe way to say "not this one". Everything else in the
workspace is `#![deny(unsafe_code)]`.

**File permissions are not the same on Windows.** On Unix the endpoint file
that carries the daemon's port and token is created with mode `0600`, so only
your account can read it. Windows has no equivalent one-line guarantee:
the file inherits the permissions of the directory it is created in, which for
a per-user data directory under `%LOCALAPPDATA%` means your account and
administrators. MemFork does not set an explicit ACL, and this document says so
rather than implying a parity that does not exist. If that difference matters
to your threat model, put the data directory somewhere you control with
`MEMFORK_DATA_DIR`, or pin it with the machine policy.

## The release, and how to check what you downloaded

Every release is built by GitHub Actions from a tag, and each archive,
installer and checksum file carries a build provenance attestation signed
through GitHub's Sigstore instance, so a download can be traced to the
workflow run and the commit that produced it. A second workflow attaches a
CycloneDX software bill of materials for every crate to each release, once the
release exists. To verify a download:

```sh
gh attestation verify memfork-x86_64-unknown-linux-musl.tar.xz --repo memforkdb/memfork
```

`docs/RELEASING.md` has the offline form and the wheel's equivalent. There is
no paid code-signing certificate: on Windows and macOS the executable is not
signed by an identity the operating system recognises, so first runs may be
held for a scan or a confirmation, and the attestation above is what stands in
its place.

## Scope

In scope: anything that lets one user read or change another user's memory,
anything that lets a remote party reach the daemon, any way to make MemFork
execute code it was not asked to, and any connection MemFork makes that this
document does not describe.

Out of scope: an attacker who already runs as you, and MemFork's own memory
being readable by you.
