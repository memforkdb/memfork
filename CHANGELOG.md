# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Nothing yet.

## [0.1.0] - UNRELEASED

<!-- Release day: set the date above to the day the tag is pushed, in
     YYYY-MM-DD. Nothing else in this file needs changing. -->

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

[Unreleased]: https://github.com/memforkdb/memfork/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/memforkdb/memfork/releases/tag/v0.1.0
