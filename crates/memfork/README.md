# memfork

The `memfork` binary: an MCP server that gives any MCP client branchable agent
memory, a registration command for the clients you already have, and a
command-line interface over the [MemFork][repo] engine.

```sh
memfork init     # register the MCP server with your MCP clients
memfork doctor   # what this install is, and what it is talking to
```

What it stores is kept: memory written in one session is there in the next,
and several clients can share one store at the same time.

```sh
memfork run - <<'SCRIPT'
put plan:1 "the original plan"
fork attempt
put plan:1 "a risky rewrite" --branch attempt
merge attempt
SCRIPT
```

See the [project README][repo] for installation and the full command
reference.

[repo]: https://github.com/memforkdb/memfork

Apache-2.0.
