"""D2: what `pip install memfork` gives you, checked against a real install.

These run against the *installed* package, not the source tree, because the
things most likely to break are the things installing changes: whether a wheel
needs a compiler, whether the console script exists, and — the one that has
bitten already — whether MemFork can work out how to launch itself again when
the running program is a Python interpreter rather than the MemFork binary.

Run them the way CI does::

    pip install --only-binary :all: --find-links dist memfork
    pytest crates/memfork-py/tests

Every MemFork these start is confined to a temporary data directory, and runs
with the guard that makes falling back to the real one a hard error.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path

import pytest

import memfork

#: How long to wait for a daemon that starts in the background.
DAEMON_TIMEOUT = 30.0


@pytest.fixture
def store(tmp_path: Path) -> dict[str, str]:
    """An environment with a MemFork that can reach nothing of the machine's.

    The data directory, the home directory and `PATH` are all temporary. The
    last one is not caution for its own sake: `memfork doctor` asks each
    installed client whether MemFork is registered, and a real client answers
    by launching the registered server — so a test run with the developer's
    own `PATH` starts a MemFork from outside this install and waits on it.
    """
    data = tmp_path / "data"
    data.mkdir()
    home = tmp_path / "home"
    home.mkdir()
    empty_bin = tmp_path / "bin"
    empty_bin.mkdir()

    env = dict(os.environ)
    env["MEMFORK_DATA_DIR"] = str(data)
    # Whatever else happens, no test may reach the developer's own store.
    env["MEMFORK_FORBID_PER_USER_DATA_DIR"] = "1"
    env["MEMFORK_HOME"] = str(home)
    env["PATH"] = os.pathsep.join(_confined_path(empty_bin))
    return env


def _confined_path(empty_bin: Path) -> list[str]:
    """One empty directory, plus what Windows needs to start a process."""
    entries = [str(empty_bin)]
    if os.name == "nt":
        root = os.environ.get("SystemRoot")
        if root:
            entries += [os.path.join(root, "System32"), root]
    return entries


def command() -> list[str]:
    """The `memfork` this interpreter installed.

    Beside the interpreter, not on `PATH`. Asking `PATH` finds whatever
    MemFork the developer happens to have installed — which is how this test
    spent ten minutes exercising a build from months ago and reporting its
    missing fields as failures.
    """
    beside = Path(sys.executable).parent
    # A virtualenv puts the interpreter and the scripts in one directory; a
    # system Python keeps the scripts in `Scripts` beside it. Both are real
    # installs and the console script is the more realistic way in, so look in
    # both before falling back to the module.
    names = ("memfork.exe", "memfork") if os.name == "nt" else ("memfork",)
    for directory in (beside, beside / "Scripts", beside / "bin"):
        for name in names:
            candidate = directory / name
            if candidate.is_file():
                return [str(candidate)]
    # A wheel always installs the script, but running these against a source
    # checkout should not be a hard failure.
    return [sys.executable, "-m", "memfork"]


def run(args: list[str], env: dict[str, str], **kwargs: object) -> subprocess.CompletedProcess:
    return subprocess.run(
        command() + args,
        env=env,
        capture_output=True,
        text=True,
        timeout=120,
        **kwargs,  # type: ignore[arg-type]
    )


def mcp_session(calls: list[dict], env: dict[str, str]) -> str:
    """Drive `memfork mcp` over stdio and return everything it said."""
    lines = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "d2", "version": "1"},
            },
        },
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        *calls,
    ]
    payload = "".join(json.dumps(line) + "\n" for line in lines)
    finished = subprocess.run(
        command() + ["mcp"],
        input=payload,
        env=env,
        capture_output=True,
        text=True,
        timeout=120,
    )
    return finished.stdout


def wait_for_daemon(env: dict[str, str]) -> dict:
    """Wait for something to own the data directory, and say what."""
    endpoint = Path(env["MEMFORK_DATA_DIR"]) / "memfork.endpoint"
    deadline = time.monotonic() + DAEMON_TIMEOUT
    while time.monotonic() < deadline:
        if endpoint.is_file():
            try:
                return json.loads(endpoint.read_text(encoding="utf-8"))
            except json.JSONDecodeError:
                pass  # Being written as we looked.
        time.sleep(0.05)
    raise AssertionError("no daemon started")


# ---- the engine ------------------------------------------------------------


def test_fork_try_merge_round_trip():
    db = memfork.Database()
    db.put("plan:1", b"the original plan")

    db.fork("attempt")
    db.put("plan:1", b"a risky rewrite", branch="attempt")
    assert db.get("plan:1").value == b"the original plan", "the fork leaked"

    merge = db.merge("attempt")
    assert merge.kind in {"fast-forward", "merged"}
    assert db.get("plan:1").value == b"a risky rewrite"


def test_discard_leaves_the_parent_alone():
    db = memfork.Database()
    db.put("plan:1", b"the original plan")
    db.fork("attempt")
    db.put("plan:1", b"a risky rewrite", branch="attempt")

    db.discard("attempt")
    assert db.get("plan:1").value == b"the original plan"
    assert [name for name, _ in db.branches()] == ["main"]


def test_time_travel_reads_an_earlier_commit():
    db = memfork.Database()
    db.put("k", b"first")
    db.put("k", b"second")
    assert db.at("k", 1).value == b"first"


def test_a_value_comes_back_as_bytes_not_a_list_of_numbers():
    db = memfork.Database()
    db.put("k", b"\x00\xff")
    assert db.get("k").value == b"\x00\xff"


def test_a_missing_branch_raises_key_error():
    db = memfork.Database()
    with pytest.raises(KeyError):
        db.get("k", branch="nope")


# ---- the command -----------------------------------------------------------


def test_the_console_script_runs_and_agrees_about_the_version(store):
    finished = run(["--version"], store)
    assert finished.returncode == 0, finished.stderr
    assert memfork.__version__ in finished.stdout


def test_memfork_knows_how_to_launch_itself_from_a_wheel(store):
    # The whole difficulty of a wheel install. `current_exe()` here is a Python
    # interpreter, so MemFork has to be told what to run instead — and whatever
    # it reports has to be something that exists and works.
    finished = run(["--json", "doctor"], store)
    assert finished.returncode == 0, finished.stderr
    report = json.loads(finished.stdout)

    binary = Path(report["binary"])
    assert binary.is_file(), f"doctor named a launch command that is not there: {binary}"

    launch = report["launch_command"]
    assert launch.endswith("mcp"), launch
    assert "memfork" in launch.lower()


def test_a_tool_call_starts_a_daemon_and_another_client_sees_the_write(store):
    # Autostart has to work from a wheel too: the daemon is spawned by
    # MemFork, and spawning the interpreter instead would start nothing.
    said = mcp_session(
        [
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "memfork_put",
                    "arguments": {"key": "wheel:1", "value": "written from a wheel"},
                },
            }
        ],
        store,
    )
    assert "wheel:1" in said, said

    endpoint = wait_for_daemon(store)
    assert endpoint["port"], endpoint
    assert endpoint["memfork_version"] == memfork.__version__

    try:
        back = mcp_session(
            [
                {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {"name": "memfork_get", "arguments": {"key": "wheel:1"}},
                }
            ],
            store,
        )
        assert "written from a wheel" in back, back
    finally:
        run(["stop"], store)


def test_the_daemon_holds_none_of_the_clients_pipes(store):
    """D6, the wheel half.

    A wheel install is not one process: the console-script launcher and the
    virtualenv redirector each duplicate the client's pipes into what they
    start, so the daemon could inherit a copy of the client's stdout and hold
    it open for its whole life. What that looks like is `memfork mcp` exiting
    and the client still waiting — for ten minutes, in the run that found it.

    So this reads to the end of the pipe, which is the thing that hung.
    """
    import threading

    payload = "".join(
        json.dumps(line) + "\n"
        for line in [
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "d6", "version": "1"},
                },
            },
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {"name": "memfork_branches", "arguments": {}},
            },
        ]
    )

    proxy = subprocess.Popen(
        command() + ["mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        env=store,
    )
    said: list[str] = []

    def read_to_the_end():
        assert proxy.stdout is not None
        said.append(proxy.stdout.read())

    reader = threading.Thread(target=read_to_the_end, daemon=True)
    reader.start()
    assert proxy.stdin is not None
    proxy.stdin.write(payload)
    proxy.stdin.close()

    try:
        reader.join(timeout=60)
        assert not reader.is_alive(), (
            "the pipe never closed: something the proxy started is holding "
            "the client's stdout open"
        )
        assert "branches" in said[0], said
    finally:
        proxy.kill()
        run(["stop"], store)


def test_a_handshake_alone_starts_nothing(store):
    mcp_session([{"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}], store)
    assert not (Path(store["MEMFORK_DATA_DIR"]) / "memfork.lock").exists()


def test_init_registers_a_command_that_exists(tmp_path, store):
    # A registration is only worth anything if the client can run it later.
    # What init would write is checked here without writing it anywhere.
    finished = run(["--json", "init", "--dry-run"], store)
    assert finished.returncode == 0, finished.stderr
    plan = json.loads(finished.stdout)

    assert Path(plan["command"]).is_file(), plan["command"]
    assert plan["command_line"].endswith("mcp"), plan["command_line"]

    # And it really starts MemFork: run the argv it would register. Taken as
    # an array, not parsed back out of the printed line — `python -m memfork`
    # is three arguments, and splitting a command line to rediscover that is
    # how this test failed on Windows while passing everywhere else.
    argv = plan["command_argv"]
    assert argv[-1] == "mcp", argv
    finished = subprocess.run(
        [*argv[:-1], "--version"], env=store, capture_output=True, text=True, timeout=120
    )
    assert finished.returncode == 0, finished.stderr
    assert memfork.__version__ in finished.stdout
