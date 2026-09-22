#!/bin/sh
# The benchmark loop of docs/BENCHMARK.md, as a skeleton.
#
# It prepares a fresh repository copy and a fresh MemFork data directory for
# every run, alternates the two conditions, times each session, runs the
# task's check and writes one CSV row per session. Starting an agent and
# reading its usage report differ for every agent, so those are the two
# functions below, which stop with a message until they are written for the
# agent under test.
#
#   TASK_DIR=path/to/task RUNS=10 AGENT=name sh scripts/benchmark/run.sh
#
# TASK_DIR holds repo/, sessions/1.txt, sessions/2.txt, ... and check.sh.
# Results go to OUT_DIR (default: benchmark-results/ in the working directory).

set -eu

: "${TASK_DIR:?set TASK_DIR to the task directory}"
: "${AGENT:?set AGENT to a name for the agent under test}"
RUNS=${RUNS:-10}
MODEL=${MODEL:-unknown}
OUT_DIR=${OUT_DIR:-benchmark-results}

# Start the agent in the current directory with the prompt in file "$1",
# as a new process with no conversation carried over, and save its usage
# report to file "$2". With MEMFORK=1 in the environment the agent must have
# `memfork mcp` configured; with MEMFORK=0 it must not.
start_agent() {
    echo "start_agent is not written for '$AGENT' yet: see docs/BENCHMARK.md" >&2
    return 1
}

# Print "input_tokens,cached_input_tokens,output_tokens,tool_calls" read from
# the usage report in file "$1". Leave a field empty if the agent does not
# report it; never estimate one.
read_usage() {
    echo "read_usage is not written for '$AGENT' yet: see docs/BENCHMARK.md" >&2
    return 1
}

[ -d "$TASK_DIR/repo" ] || { echo "no repo/ in $TASK_DIR" >&2; exit 2; }
[ -d "$TASK_DIR/sessions" ] || { echo "no sessions/ in $TASK_DIR" >&2; exit 2; }
[ -f "$TASK_DIR/check.sh" ] || { echo "no check.sh in $TASK_DIR" >&2; exit 2; }

mkdir -p "$OUT_DIR"
csv="$OUT_DIR/results.csv"
if [ ! -f "$csv" ]; then
    echo "agent,model,condition,run,session,input_tokens,cached_input_tokens,output_tokens,tool_calls,wall_seconds,succeeded,briefing_bytes" >"$csv"
fi

run=1
while [ "$run" -le "$RUNS" ]; do
    # Alternate, so drift in the service affects both conditions alike.
    for memfork in 1 0; do
        if [ "$memfork" = 1 ]; then condition=with; else condition=without; fi
        work="$OUT_DIR/$condition-$run"
        rm -rf "$work"
        mkdir -p "$work"
        cp -R "$TASK_DIR/repo" "$work/repo"
        MEMFORK_DATA_DIR="$(cd "$work" && pwd)/memfork-data"
        export MEMFORK_DATA_DIR
        MEMFORK=$memfork
        export MEMFORK

        rows="$work/rows.csv"
        : >"$rows"
        session=1
        while [ -f "$TASK_DIR/sessions/$session.txt" ]; do
            prompt="$(cd "$TASK_DIR/sessions" && pwd)/$session.txt"
            report="$(cd "$work" && pwd)/usage-$session.txt"
            started=$(date +%s)
            (cd "$work/repo" && start_agent "$prompt" "$report")
            ended=$(date +%s)
            usage=$(read_usage "$report")
            echo "$AGENT,$MODEL,$condition,$run,$session,$usage,$((ended - started))" >>"$rows"
            session=$((session + 1))
        done

        if (cd "$work/repo" && sh "$TASK_DIR/check.sh") >"$work/check.log" 2>&1; then
            succeeded=1
        else
            succeeded=0
        fi

        briefing_bytes=
        if [ "$memfork" = 1 ]; then
            memfork --json stats >"$work/stats.json" || true
            memfork stop >/dev/null 2>&1 || true
            # Total briefing bytes across projects and clients; filled in by
            # whoever publishes, from stats.json, until this reads it itself.
        fi

        while IFS= read -r row; do
            echo "$row,$succeeded,$briefing_bytes" >>"$csv"
        done <"$rows"
    done
    run=$((run + 1))
done

echo "wrote $csv"
