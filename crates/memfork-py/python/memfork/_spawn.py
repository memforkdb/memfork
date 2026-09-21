"""Start the MemFork daemon without handing it anything of the client's.

This exists for one Windows behaviour. A new process there inherits every
*inheritable* handle its parent holds, and a wheel install has handles nobody
asked for: the console-script launcher and the virtualenv redirector each
duplicate the client's pipes into the process they start, so by the time
MemFork is running there are copies of the client's stdout that nothing can
name. Clearing the flag on the standard handles — which is what the binary
does, and which is enough for the binary — does not reach them.

A daemon that inherits one keeps the client's stdout open for as long as it
runs, so a client that reads to the end of the pipe waits long after
`memfork mcp` has exited.

Python can say what Rust's standard library cannot: `close_fds=True` means
`bInheritHandles = FALSE`, and the daemon then starts with nothing inherited
at all. So MemFork, when it is running inside Python, starts its daemon
through this module: one short-lived process that inherits the mess, hands
none of it on, and exits.

Not a public interface. `memfork` sets `MEMFORK_SPAWN_VIA` to point here.
"""

from __future__ import annotations

import json
import subprocess
import sys

#: Windows creation flags: a new process group so a Ctrl-C in the client's
#: terminal does not take the daemon with it, and no console of its own.
_DETACHED_PROCESS = 0x00000008
_CREATE_NEW_PROCESS_GROUP = 0x00000200


def main(argv: list[str]) -> int:
    """Spawn the command given as one JSON array argument, and return."""
    if len(argv) != 2:
        print("usage: python -m memfork._spawn '[\"program\", \"args\"]'", file=sys.stderr)
        return 2

    try:
        command = json.loads(argv[1])
    except json.JSONDecodeError as e:
        print(f"memfork: the command to spawn is not valid JSON: {e}", file=sys.stderr)
        return 2
    if not isinstance(command, list) or not command or not all(
        isinstance(part, str) for part in command
    ):
        print("memfork: the command to spawn must be a non-empty array of strings", file=sys.stderr)
        return 2

    kwargs: dict[str, object] = {
        "stdin": subprocess.DEVNULL,
        "stdout": subprocess.DEVNULL,
        "stderr": subprocess.DEVNULL,
        # The point of this module.
        "close_fds": True,
    }
    if sys.platform == "win32":
        kwargs["creationflags"] = _DETACHED_PROCESS | _CREATE_NEW_PROCESS_GROUP
    else:
        # Its own process group, for the same reason as on Windows.
        kwargs["start_new_session"] = True

    try:
        subprocess.Popen(command, **kwargs)  # type: ignore[arg-type]
    except OSError as e:
        print(f"memfork: cannot start the daemon: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
