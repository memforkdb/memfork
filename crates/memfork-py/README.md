# MemFork

Git for agent memory, shared by every AI tool on your machine. Fork, merge,
discard and rewind what an agent remembers — forking costs the same whatever
memory holds, so an agent can branch before a risky step and throw the branch
away if it goes wrong — and hand work from one agent to another through a
store every MCP client shares.

```sh
pip install memfork
```

One wheel, two things.

## The engine, in your process

```python
import memfork

db = memfork.Database()
db.put("plan:1", b"the original plan")

db.fork("attempt")
db.put("plan:1", b"a risky rewrite", branch="attempt")

db.get("plan:1").value           # b'the original plan' — the fork is invisible here
db.merge("attempt")              # or db.discard("attempt")
```

Branches are cheap because they share structure rather than copying. You can
also read memory as it was earlier: `db.at("plan:1", seq)`.

## The MCP server, for your agent clients

The same install puts the `memfork` command on your path:

```sh
memfork init      # register the server with the MCP clients you have
memfork doctor    # what is installed, and what is talking to it
```

Clients then get fifteen tools — store, recall, search, fork, merge, discard,
time travel, and handing work from one agent to another — over one store they
share. With a single client that still pays: its next session resumes where the
last one stopped.

## What is in memory and what is on disk

`memfork.Database` is **in memory only**. It does not write to disk, and it
does not see what an MCP client wrote: the durable, shared store belongs to the
MemFork daemon, which the `memfork` command starts when something needs it.

That split is deliberate — the engine is I/O-free by design — but it does mean
a Python program that wants the shared memory talks to the server rather than
importing it. Binding the durable store is later work.

## More

Full documentation, the CLI reference and the design spec:
<https://github.com/memforkdb/memfork>

Apache-2.0.
