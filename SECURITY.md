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
| 0.1.x | Yes |
| anything older | No |

## What MemFork does with your data

Memory is stored in a per-user data directory, in plain form. It is not
encrypted, and it is readable by anything running as you. Treat it as you would
any other file in your home directory: do not put secrets in it that you would
not put in a text file there.

The daemon that several clients share listens on `127.0.0.1` only, never on a
network interface, and requires a token that it writes to a file beside the
lock. A client that cannot read that file cannot talk to the daemon.

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
`MEMFORK_DATA_DIR`.

## Scope

In scope: anything that lets one user read or change another user's memory,
anything that lets a remote party reach the daemon, and any way to make MemFork
execute code it was not asked to.

Out of scope: an attacker who already runs as you, and MemFork's own memory
being readable by you.
