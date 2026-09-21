"""MemFork: branchable memory for agents.

Fork the whole of memory before a risky step, merge it if the attempt worked,
discard it if it did not. Forking costs the same whatever memory holds.

    >>> import memfork
    >>> db = memfork.Database()
    >>> db.put("plan:1", b"the original plan")
    >>> db.fork("attempt")
    >>> db.put("plan:1", b"a risky rewrite", branch="attempt")
    >>> db.get("plan:1").value
    b'the original plan'
    >>> db.discard("attempt")

What this module holds is *in memory*, and it is not the memory an MCP client
sees. The durable, shared store belongs to the MemFork daemon; this package
installs that too, as the ``memfork`` command::

    memfork init        # register the server with your clients
    memfork doctor      # what is installed, and what is talking to it

Binding the durable store is later work. Until then, the two are separate on
purpose rather than by accident.
"""

from ._memfork import Commit, Database, Entry, Hit, Merge, __version__, version

__all__ = [
    "Commit",
    "Database",
    "Entry",
    "Hit",
    "Merge",
    "__version__",
    "version",
]
