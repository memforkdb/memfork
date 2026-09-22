# Measuring MemFork

MemFork claims one thing about cost: what it hands an agent is measured in
bytes, and `memfork stats` reports those bytes and an approximate token count
(bytes divided by 4, rounded up). Whether that saves an agent anything overall
is a question for a measurement, not a claim. This is how to make one that
somebody else can repeat.

No results are published here yet. When there are some, they go in the table at
the end, raw, with the versions and the date they were measured on.

## What is compared

The same task, done by the same agent with the same model, in two conditions:

- **Without MemFork**: the agent has no MemFork server configured.
- **With MemFork**: the agent has `memfork mcp` configured and the repository
  has MemFork's instruction block (`memfork init --project`).

Everything else is identical: the repository at the same commit, the same
prompts, the same model and settings, the same machine.

The task is split into **sessions**, because carrying work from one session to
the next is where memory can matter. Each session is a fresh agent process with
no conversation carried over. A task has at least two sessions: the first is
told to do part of the work and stop; each later one is told to continue. The
prompts are fixed in advance and are the same in both conditions. Neither
condition's prompts mention MemFork.

## What is measured

For every session of every run:

| Field | Where it comes from |
|---|---|
| `input_tokens`, `output_tokens` | the agent's own usage report for that session, as the agent prints it |
| `cached_input_tokens` | the same report, if the agent reports it; blank otherwise |
| `tool_calls` | the agent's own report or transcript |
| `wall_seconds` | the runner's clock |
| `succeeded` | the task's own check (a test command) after the last session |
| `briefing_bytes` | `memfork stats --json` after the run, with MemFork only |

Token counts are read from the agent, never estimated. Tokenisers differ between
models, so a count from one agent says nothing about another; compare
conditions only within one agent and model.

## Protocol

1. Choose a task with a mechanical success check: a repository at a fixed
   commit, a list of session prompts, and a command that exits 0 when the task
   is done.
2. Choose N, at least 10 runs per condition. Agents are not deterministic, so
   one run of each proves nothing.
3. For each run, in an order that alternates the conditions (with, without,
   with, ...) so that drift in the service affects both alike:
   1. Copy the repository into a fresh directory.
   2. Give MemFork a fresh, empty data directory (`MEMFORK_DATA_DIR`), so no
      run sees another's memory. Without MemFork, remove it from the agent's
      configuration.
   3. Run each session prompt in turn as a new agent process, saving the
      agent's usage report for each.
   4. Run the success check.
   5. With MemFork, save `memfork stats --json`, then stop the daemon.
4. Report every run, including failures. Do not drop outliers.
5. Report, per condition: the median and the range of total input tokens, total
   output tokens and wall time, and the success rate. Report the difference
   between the medians; do not turn it into money, energy or anything else.

## Publishing

Publish the raw table below, the task (repository, commit, prompts and check),
the agent and model with their versions, the MemFork version, the operating
system, and the date. Anyone should be able to run it again.

## Runner

`scripts/benchmark/run.sh` is a skeleton of the loop above. It prepares the
directories, alternates the conditions, times each session and writes one CSV
row per session. It does not know how to start any particular agent or read
its usage report: those are two small functions to fill in for the agent under
test, since every agent does it differently. It is POSIX sh, and runs on
Windows in Git Bash.

```sh
TASK_DIR=path/to/task RUNS=10 AGENT=my-agent sh scripts/benchmark/run.sh
```

A task directory holds `repo/` (the repository at its commit), `sessions/`
(`1.txt`, `2.txt`, ... in order) and `check.sh` (exits 0 when the task is done).

## Results

None yet.

| agent | model | condition | run | session | input_tokens | cached_input_tokens | output_tokens | tool_calls | wall_seconds | succeeded | briefing_bytes |
|---|---|---|---|---|---|---|---|---|---|---|---|
