# Adopting MemFork in an organisation

What MemFork is, what it touches on a machine, how to roll it out, how to
switch parts of it off, and how to audit it. Written for the person who has to
approve it, in plain terms; the precise version of every claim here is in
[SECURITY.md](../SECURITY.md) and [DESIGN.md](DESIGN.md).

## What it is

MemFork is a local program that gives AI coding tools a shared, branchable
memory. Each tool talks to it over MCP, the protocol those tools already use
for their own tools. An agent stores what it decided and why, hands work to
the next session or the next tool, forks its memory before a risky step and
merges or discards the result. Several tools on one machine share one memory.

It is one executable, Apache-2.0, with no service behind it. It never calls a
model, holds no API key, and costs nothing to run.

## What it touches

**Network.** Nothing off the machine. The daemon listens on `127.0.0.1` on a
port the operating system assigns, behind a token in a file only the user can
read. There is no telemetry, no update check, no account. The code that can
open a socket is four named modules, listed in SECURITY.md and held by a test;
a CI job runs the whole command surface with outbound traffic blocked at the
firewall. The installers are the exception: they download a release from
GitHub, or from a mirror you name.

**Disk.** One data directory per user (`%LOCALAPPDATA%\memfork`,
`~/Library/Application Support/memfork`, `~/.local/share/memfork`), or the
directory the machine policy pins. It holds memory itself, a small side file of
counters and file hashes, the daemon's endpoint file and its log. Memory is
plain, unencrypted, and readable by anything running as that user — like any
other file in their home directory. Credentials are refused on write by a
deterministic detector; it is a safety net, not a guarantee.

**Your tools' configuration.** `memfork init` registers MemFork with the MCP
clients on the machine, through each client's own command where it has one,
otherwise by editing the client's configuration file — one entry, a backup
kept, everything else untouched byte for byte. `--dry-run` shows the change
first. `memfork init --project` writes one marked block into a repository's
instruction files and can take it out again.

**Your repositories.** MemFork never runs `git`. It reads the files a stored
fact names, inside the project, to tell whether the fact is still fresh. A
plan's acceptance command runs on the machine when an agent marks a task done,
and only if the repository's own plan file holds the same command — an agent
cannot write a command into shared memory and have another tool run it.

**Nothing else.** No hooks, no services, no scheduled tasks, no changes to
`PATH` beyond the directory the installer puts the binary in (and says so).

**The Brain**, `memfork brain`, is a page the daemon serves on the same
loopback listener, read only: every route it can reach answers `GET`, and the
token it holds is refused on every route that writes. It makes no request that
leaves the machine, and its export is a file the browser saves, with anything
shaped like a credential withheld. `brain = false` in the machine policy
switches it and `memfork demo` off.

## How to roll it out

1. **Install** with the script for the platform, `pip`, `cargo`, or from a
   mirror (below). The install scripts put one file into a directory the user
   owns and need no administrator.
2. **Place the machine policy** if you want anything switched off or the data
   directory pinned; see the next section. Do this before users run
   `memfork init`, so the first run already obeys it.
3. **Register the tools**: each user runs `memfork init`, or a repository's
   maintainer runs `memfork init --project --all` once and commits the
   instruction files, so every tool used on that repository is told the same
   routine.
4. **Check**: `memfork doctor` on a user's machine shows the version, where
   memory is kept, whether the daemon is running, the policy in force and one
   line per client. `--verbose` has the whole report.

### Installing from a mirror, or with no internet at all

The installers take a download base, so a release can be served from a share
or an internal web server and the install never reaches GitHub:

1. From a machine that can reach GitHub, fetch the release assets you need:
   the archive for each platform (`memfork-<target>.tar.xz` or `.zip`), its
   `.sha256` file, and `install.sh` / `install.ps1`. Verify them first
   (`gh attestation verify <file> --repo memforkdb/memfork`).
2. Put them in one directory served over HTTP inside your network, or on a
   share reachable as a path.
3. On each machine, run the installer with the base pointing there and the
   version pinned, so nothing needs resolving:

   ```sh
   MEMFORK_DOWNLOAD_BASE=https://mirror.example/memfork/v0.3.0 \
   MEMFORK_VERSION=v0.3.0 sh install.sh
   ```

   ```powershell
   $env:MEMFORK_DOWNLOAD_BASE = 'https://mirror.example/memfork/v0.3.0'
   $env:MEMFORK_VERSION = 'v0.3.0'
   .\install.ps1
   ```

   The installer verifies the archive against the `.sha256` file from the same
   base and refuses anything that does not match. With `MEMFORK_VERSION`
   unset and no download base, it asks GitHub which release is the latest
   exactly once and downloads that version by name, so a stale cache cannot
   mix versions. `MEMFORK_GITHUB_BASE` points that question at a GitHub
   Enterprise host instead.

This is exactly what the installer tests do in CI, on all three operating
systems, against a release served from disk with the network blocked
(`installers/test-installer.sh`, run by the `no-network` workflow).

## How to switch features off

One file, in a location only an administrator can write, that no user setting
overrides:

| OS | Policy file |
|---|---|
| Windows | `%ProgramData%\memfork\policy.toml` |
| macOS | `/Library/Application Support/memfork/policy.toml` |
| Linux | `/etc/memfork/policy.toml` |

```toml
brain = false              # memfork brain and memfork demo, the read-only page
race = false               # memfork race, which runs agents unattended
autopilot = false          # memory following the git branch, automatic forks
maintenance_tasks = false  # tasks MemFork adds to tidy a project's memory
sampling = false           # asking a client's model for a summary
secret_overrides = false   # allow_secret / --allow-secret
data_dir = "/srv/memfork"  # where memory is kept, for every user
```

Every key is optional and each feature defaults to allowed. A file that cannot
be read — malformed, or with a key this version does not know — stops every
command except `memfork doctor`, which says what is wrong, rather than run with
a rule it does not understand. `memfork doctor --verbose` shows each feature's
answer and which file said so. `MEMFORK_POLICY_FILE` names a second file for
trying a policy out; where both set a key the machine file wins, so it can only
add restrictions.

## How to audit it

- **What happened.** `memfork watch --json` is a versioned, documented stream
  of every operation: who did what, to which key or branch, when, and whether
  it succeeded ([EVENTS.md](EVENTS.md)). Any log shipper that reads a line of
  JSON can take it. `memfork log --graph` shows a project's history as a tree
  of forks, merges and discarded attempts.
- **What is stored.** `memfork ls`, `memfork get` and `memfork at <seq>` read
  memory as it is and as it was; `memfork facts`, `memfork lessons`,
  `memfork flags` and `memfork stats` each answer one question. Everything has
  `--json`.
- **What is in force.** `memfork doctor --verbose`: the binary, the data
  directory and why it was chosen, the daemon, the policy and each file it came
  from, and each client's registration and where its registry entry was
  verified — with anything that could not be verified marked as such.
- **What you installed.** Each release carries build provenance attestations
  and a software bill of materials; `gh attestation verify` checks a download
  against the workflow that built it ([RELEASING.md](RELEASING.md)).

## What it cannot do for you

MemFork cannot control what an agent does with what it reads: memory an agent
reads becomes part of that agent's prompt and goes wherever that agent already
sends your code. It cannot encrypt memory at rest, or stop a process running
as the user from reading it. It does not sync between machines or users, and
has no accounts or roles: one machine, one memory per user, or one pinned by
policy. The executables are not signed by a paid certificate; the attestation
is what stands in its place.
