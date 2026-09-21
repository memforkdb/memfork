# MemFork — instructions for coding agents

Same rules as for everyone else, written down in one place:

- **[CONTRIBUTING.md](CONTRIBUTING.md)** — the rules the code lives by, how to
  build it, and what has to pass before a commit.
- **[docs/DESIGN.md](docs/DESIGN.md)** — how MemFork works and why. Read it
  before changing the engine. If the design and your instinct disagree, follow
  the design or open an issue; please do not quietly redesign it.

Three things worth saying twice, because they are the ones most often got
wrong by anybody working quickly:

1. **Every test runs in its own sandbox.** Temporary data directory, empty
   `PATH`, temporary home. Never build a `memfork` command outside the helpers
   in `crates/memfork/tests/support`; a check fails the build if you do.
2. **All three operating systems, every time.** A change that passes on yours
   is not finished. Several real bugs here were invisible on the machine they
   were written on.
3. **Stop and ask rather than widen the work.** Finish what was asked, say
   what you left out and why.
