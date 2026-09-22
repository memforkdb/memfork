# Contributing to MemFork

Thank you for looking. This document is the whole agreement: what MemFork is,
the rules the code lives by, and how to get a change accepted.

MemFork is an embedded in-memory database for agent state, with Git semantics —
fork, merge, discard, rewind. It runs identically on Windows, macOS and Linux,
and it is Apache-2.0.

The design is in [`docs/DESIGN.md`](docs/DESIGN.md). Read it before changing
the engine. If the design and your instinct disagree, follow the design or open
an issue — please don't quietly redesign it in a pull request.

## The rules

These are not style preferences. Each one is here because breaking it would
break a promise MemFork makes to the people using it.

**Branching is structural sharing, never an operating-system trick.** No
`fork()`, no copy-on-write pages, no filesystem snapshots. Forking is O(1)
because the data structures are persistent, and that is the only reason it is
allowed to be. Anything else would behave differently on three platforms.

**Every feature behaves identically on Windows, macOS and Linux.** No
`#[cfg(unix)]` without a matching Windows implementation and a test for both.
Where the platforms genuinely differ — file locking, handle inheritance — say
so in the documentation rather than implying a parity that is not there.

**Determinism.** The same sequence of operations produces the same commit ids
and the same search results on every machine, every time. Wall-clock time,
randomness and hash-map iteration order may not influence an id, an ordering or
a result. The golden-file test exists to catch exactly this.

**Permissive licences only.** Dependencies must be MIT, Apache-2.0, BSD, ISC,
Zlib or Unicode. No GPL, AGPL, LGPL, SSPL or MPL: MemFork is Apache-2.0, and a
copyleft dependency would make that choice for everyone downstream. `cargo deny
check` enforces this, and a new licence needs a line in `deny.toml` saying why
it belongs.

**No `unsafe`, with one exception.** The exception is documented in
[`SECURITY.md`](SECURITY.md) and is three FFI calls on Windows. Adding a second
needs a reason in the pull request that survives review.

**No `unwrap()` or `expect()` outside tests.** A database that panics on a
caller's mistake is a database that takes the caller's process with it.

**Vendor neutral.** No code path, tool description or document may assume a
particular model or client. Which clients are supported is data in the adapter
registry, never code. Naming a client as *supported* is fine; privileging one
is not.

**The README is a contract.** Anything a user needs that is not in the README
is a bug, and its examples are compiled as doctests so they cannot rot.

## Getting set up

```sh
git clone https://github.com/memforkdb/memfork
cd memfork
cargo build --workspace
```

Rust 1.89 or newer for the binary; the `memfork-core` library alone builds on
1.85. Nothing else is required — no C toolchain beyond what your platform's
default already provides.

For the Python package you also need [maturin](https://www.maturin.rs):

```sh
pip install maturin
maturin develop --manifest-path crates/memfork-py/Cargo.toml
```

## What has to pass

All of it, before every commit:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo deny --workspace --all-features check
```

And, when you have touched the Python package or the installers:

```sh
python -m pytest crates/memfork-py/tests     # needs the wheel installed
bash installers/test-installer.sh            # runs against a release on disk
```

And when you have touched anything that could open a socket, or the
installers, run the command surface with no network:

```sh
cargo build --release -p memfork
bash scripts/no-network.sh                   # the last check fails on a networked machine; CI blocks the network first
```

`crates/memfork/tests/no_network.rs` runs with the rest and holds the
allow-list of modules that may open a socket; SECURITY.md names the same
list, and the test checks that it does.

CI runs the same things on all three operating systems. A change is not
finished until it passes on all of them, not just on yours.

The Hygiene workflow also checks the repository itself:

- No unfilled template placeholders and no tool-generated commit trailers.
- No personal assistant or prompt files committed, at any depth. Keep
  those in `.git/info/exclude`.
- The one client instruction file whose name is also a personal
  assistant file may be *named* only where the product needs it: the client
  registry (`clients.toml`), the tests of `memfork init --project`, the
  README, `docs/DESIGN.md` and the changelog.
- No build-phase numbers or acceptance-test ids in comments or docs, outside
  the revision record in `docs/DESIGN.md`.
- No emoji in Markdown.

## Tests

Tests are the specification in executable form. The ones the design document
names — fork cost, isolation, determinism, crash recovery, the shared daemon —
are what say whether a change broke a promise.

Two habits matter here:

**Test isolation is structural, not careful.** Every test and every process a
test starts runs with an explicit temporary data directory, an empty `PATH`,
and a temporary home. `MEMFORK_FORBID_PER_USER_DATA_DIR` makes falling back to
the real per-user directory a hard error, and `tests/guard.rs` fails the build
if a test builds a command outside that harness. This exists because tests
twice wrote into a developer's own store. Being careful was not a control.

**A test that can only fail on one platform still belongs.** Several real bugs
here were invisible on the machine they were written on: a daemon inheriting a
pipe on Windows, a shell function with no local variables on Linux, a linker
that will not leave symbols undefined on macOS.

**A registry entry says what it could not confirm.** Adding an MCP client to
`clients.toml` means checking its documentation on that day and recording the
date. Anything the documentation does not say — the name a closed-source client
sends in `initialize`, a Windows path given only as `~/…` — goes in that
entry's `unverified` line, which `memfork doctor --verbose` shows. A guess
that happens to be right today is a wrong answer given quietly later.

## Commits and pull requests

Conventional messages — `feat:`, `fix:`, `test:`, `ci:`, `docs:` — and small
commits that do one thing. Explain *why* in the body; the diff already says
what.

In a pull request, say what you changed, how you tested it, and anything you
decided not to do. If you found a real problem with the task as specified, say
so and then finish the work under a stated assumption rather than stopping.

## Layout

```
memfork/
  crates/memfork-core/      the engine: no I/O, no async
  crates/memfork/           the `memfork` binary: mcp | serve | init | doctor | CLI
  crates/memfork-py/        the Python package, built with maturin
  installers/               install.sh, install.ps1 and their tests
  docs/DESIGN.md            how it works and why
  docs/RELEASING.md         how a release is cut
  .github/workflows/        CI, release, wheels, installers, hygiene
  deny.toml                 the dependency policy
```

## Code of conduct

By taking part you agree to the [Code of Conduct](CODE_OF_CONDUCT.md).

## Security

Please do not open a public issue for a vulnerability. See
[`SECURITY.md`](SECURITY.md).

## Licence

Contributions are accepted under Apache-2.0, the licence of the project. You
keep the copyright in what you write.
