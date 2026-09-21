"""The ``memfork`` command, as installed by the wheel.

The work happens in Rust; what this file adds is the one thing Rust cannot
work out for itself. MemFork starts a daemon, and tells MCP clients what to
run, and both need a command that starts MemFork again later. Inside a wheel
install there is no such thing as "this executable": the running program is a
Python interpreter, and handing a client that path would register something
that does nothing.

So the launch command is worked out here, where ``sys.argv[0]`` and
``sys.executable`` are known, and passed through ``MEMFORK_LAUNCH``.
"""

from __future__ import annotations

import json
import os
import sys

from ._memfork import run_cli


def launch_command() -> list[str]:
    """The command that starts MemFork again, from wherever this is installed.

    The console script if there is one — ``pip``, ``pipx`` and ``uv`` all
    install one, and it works without knowing which interpreter to use.
    Otherwise this interpreter and this module, which is what ``python -m
    memfork`` leaves behind.
    """
    script = sys.argv[0] if sys.argv else ""
    if script:
        name = os.path.basename(script).lower()
        # `python -m memfork` sets argv[0] to the module's path, which is not
        # something to run; a console script is.
        if name.startswith("memfork") and not name.endswith(".py"):
            resolved = os.path.abspath(script)
            for candidate in _with_windows_suffix(resolved):
                if os.path.isfile(candidate):
                    return [candidate]
    return [sys.executable, "-m", "memfork"]


def _with_windows_suffix(path: str) -> list[str]:
    """`path`, and what Windows actually called the file.

    The console-script launcher on Windows reports ``argv[0]`` with the
    extension stripped — ``...\Scripts\memfork`` for a file named
    ``memfork.exe`` — so taking it at its word finds nothing and MemFork
    silently falls back to launching the interpreter. Checked on Windows
    rather than assumed.
    """
    if os.name != "nt" or os.path.splitext(path)[1]:
        return [path]
    return [path + ".exe", path]


def main() -> int:
    """Run the command line, and return its exit status."""
    # Only set when nobody else has: a parent that already knows how MemFork
    # was launched — the daemon's parent, say — knows better than this guess.
    os.environ.setdefault("MEMFORK_LAUNCH", json.dumps(launch_command()))
    # And how to start one without handing it the client's pipes. See
    # `memfork._spawn` for why Python has to be the one to do it.
    os.environ.setdefault(
        "MEMFORK_SPAWN_VIA", json.dumps([sys.executable, "-m", "memfork._spawn"])
    )
    argv = ["memfork", *sys.argv[1:]]
    return run_cli(argv)


if __name__ == "__main__":
    sys.exit(main())
