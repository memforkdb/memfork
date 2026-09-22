//! The command line (DESIGN §5).
//!
//! These exercise the batch mode, which is how a
//! sequence of operations shares one in-memory database today.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write;

use predicates::prelude::*;

mod support;

use support::memfork_assert as memfork;

/// Run a script through standard input and return stdout.
fn run(script: &str) -> String {
    let out = memfork()
        .arg("run")
        .arg("-")
        .write_stdin(script)
        .assert()
        .success();
    String::from_utf8(out.get_output().stdout.clone()).expect("stdout is UTF-8")
}

fn run_json(script: &str) -> serde_json::Value {
    let out = memfork()
        .args(["--json", "run", "-"])
        .write_stdin(script)
        .assert()
        .success();
    serde_json::from_slice(&out.get_output().stdout).expect("stdout is JSON")
}

#[test]
fn the_fork_try_merge_flow_works_end_to_end() {
    let out = run(r#"
        put plan:1 "the original plan"
        fork attempt
        put plan:1 "a rewrite that worked" --branch attempt
        merge attempt
        get plan:1
        "#);
    assert!(out.contains("a rewrite that worked"), "{out}");
}

#[test]
fn the_fork_try_discard_flow_leaves_the_parent_alone() {
    let out = run(r#"
        put plan:1 "the original plan"
        fork attempt
        put plan:1 "a rewrite that did not work" --branch attempt
        discard attempt
        get plan:1
        branches
        "#);
    assert!(out.contains("the original plan"), "{out}");
    assert!(!out.contains("did not work"), "{out}");
    assert!(
        !out.contains("attempt\t"),
        "the discarded branch is still listed: {out}"
    );
}

#[test]
fn every_phase_one_subcommand_runs() {
    let results = run_json(
        r#"
        put a:1 first
        put a:2 second --importance 0.9 --ttl-commits 5 --meta source=test
        put v:1 vectored --embedding 1,0,0
        put v:2 orthogonal --embedding 0,1,0
        get a:1
        ls a:
        search 1,0,0 --k 1
        fork side
        put a:1 changed --branch side
        diff main side
        branches
        log --limit 2
        at 1
        del a:2
        merge side --policy theirs
        discard side
        "#,
    );
    let ops: Vec<&str> = results
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["op"].as_str().unwrap())
        .collect();
    assert_eq!(
        ops,
        vec![
            "put", "put", "put", "put", "get", "ls", "search", "fork", "put", "diff", "branches",
            "log", "at", "del", "merge", "discard",
        ]
    );

    // Spot-check the semantics, not just that each command ran.
    let by_op = |name: &str| {
        results
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["op"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(by_op("get")["value"], "first");
    assert_eq!(by_op("search")["hits"][0]["key"], "v:1");
    assert_eq!(by_op("ls")["keys"].as_array().unwrap().len(), 2);
    assert_eq!(by_op("diff")["changes"][0]["kind"], "modified");
    assert_eq!(by_op("merge")["policy"], "theirs");
}

#[test]
fn time_travel_reads_a_past_commit() {
    let out = run(r#"
        put k v1
        put k v2
        put k v3
        at 1 --key k
        at 2 --key k
        at 3 --key k
        "#);
    assert_eq!(
        out.lines().skip(3).collect::<Vec<_>>(),
        vec!["v1", "v2", "v3"]
    );
}

#[test]
fn a_failing_merge_stops_the_script_and_reports_the_line() {
    let assert = memfork()
        .args(["run", "-"])
        .write_stdin(
            r#"
put k base
fork side
put k ours
put k theirs --branch side
merge side
"#,
        )
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("line 6"), "{stderr}");
    assert!(stderr.contains("conflict"), "{stderr}");
}

#[test]
fn quoting_and_comments_are_handled() {
    let out = run(r#"
        # a comment line
        put k "a value with spaces and a # inside"   # trailing comment
        put empty ""
        get k
        "#);
    assert!(out.contains("a value with spaces and a # inside"), "{out}");
}

#[test]
fn a_script_may_not_start_a_server_or_recurse() {
    for bad in [
        "mcp",
        "serve",
        "init",
        "doctor",
        "run -",
        "tools --format openai",
        "call memfork_get",
    ] {
        let assert = memfork()
            .args(["run", "-"])
            .write_stdin(format!("{bad}\n"))
            .assert()
            .failure();
        let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
        assert!(
            stderr.contains("cannot be used inside a script"),
            "`{bad}` was not refused: {stderr}"
        );
    }
}

#[test]
fn serve_refuses_to_be_a_daemon_with_nothing_to_share() {
    // Everything the earlier phases stubbed out now works, so the only thing
    // left to check here is the one combination that makes no sense: a shared
    // daemon over a store that keeps nothing.
    memfork()
        .args(["serve", "--ephemeral"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("nothing to share"));
}

#[test]
fn the_phase_two_subcommands_work() {
    // Covered in depth by the B-series tests; this asserts they are wired to
    // the command line at all.
    memfork()
        .args(["tools", "--format", "anthropic"])
        .assert()
        .success()
        .stdout(predicate::str::contains("memfork_fork"));
    memfork()
        .args(["--ephemeral", "call", "memfork_branches"])
        .assert()
        .success()
        .stdout(predicate::str::contains("main"));
    memfork()
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("data dir"));
    memfork()
        .args(["doctor", "--verbose"])
        .assert()
        .success()
        .stdout(predicate::str::contains("survives restarts"));
}

#[test]
fn a_script_can_be_read_from_a_file_with_spaces_and_non_ascii_in_its_path() {
    // DESIGN §7: paths with spaces and non-ASCII must work on every OS.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scripts for the agent Ω 🔱");
    std::fs::create_dir_all(&path).unwrap();
    let file = path.join("démo script.mf");
    let mut f = std::fs::File::create(&file).unwrap();
    writeln!(f, "put ключ значение\nget ключ").unwrap();
    drop(f);

    memfork()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("значение"));
}

#[test]
fn errors_are_reported_and_exit_non_zero() {
    memfork()
        .args(["--ephemeral", "get", "k", "--branch", "no-such-branch"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no such branch"));

    memfork()
        .args(["run", "-"])
        .write_stdin("discard main\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be discarded"));

    memfork()
        .args(["run", "-"])
        .write_stdin("put k v --importance 3\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("importance"));

    memfork()
        .args(["run", "-"])
        .write_stdin("not-a-command\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("line 1"));
}

#[test]
fn json_errors_are_json() {
    let assert = memfork()
        .args(["--ephemeral", "--json", "get", "k", "--branch", "nope"])
        .assert()
        .failure();
    let parsed: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stderr).expect("stderr is JSON");
    assert!(parsed["error"].as_str().unwrap().contains("no such branch"));
}

#[test]
fn version_and_help_work() {
    memfork()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
    memfork()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("fork"));
}

#[test]
fn the_same_script_gives_the_same_commit_ids_every_run() {
    // The CLI must not introduce any non-determinism of its own.
    let script = "put a 1\nput b 2\nfork side\nput c 3 --branch side\nmerge side\nlog\n";
    assert_eq!(run_json(script), run_json(script));
}

#[test]
fn listed_values_line_up_whatever_the_key_length() {
    // Tabs put `project:codename` and `project:owner` on different tab
    // stops; padding to the widest key keeps every value in one column.
    let text =
        run("put project:codename \"Blue Heron\"\nput project:owner ada\nput a 1\nls\nat 3\n");
    let listed: Vec<&str> = text.lines().filter(|l| !l.starts_with("put ")).collect();
    assert_eq!(listed.len(), 6, "{text}");
    for line in &listed {
        assert!(!line.contains('\t'), "a tab in {line:?}");
    }
    for rows in listed.chunks(3) {
        let columns: Vec<usize> = rows
            .iter()
            .map(|l| l.len() - l.split_once("  ").map_or("", |(_, v)| v.trim_start()).len())
            .collect();
        assert!(columns.windows(2).all(|w| w[0] == w[1]), "{rows:?}");
    }
    assert!(text.contains("project:codename  Blue Heron"), "{text}");
    assert!(text.contains("project:owner     ada"), "{text}");

    // The JSON is unchanged: keys and values as fields, no padding.
    let doc = run_json("put project:owner ada\nls\n");
    assert_eq!(doc[1]["keys"][0]["key"], "project:owner");
    assert_eq!(doc[1]["keys"][0]["value"], "ada");
}

#[test]
fn into_a_pipe_listings_print_whole_values_with_or_without_full() {
    // Scripts read `ls` and `at` through a pipe; they get every byte, as
    // before, and `--full` changes nothing there.
    let long = "x".repeat(500);
    let script = format!("put k '{long}'\nls\nls --full\nat 1\nat 1 --full\n");
    let text = run(&script);
    let listed: Vec<&str> = text.lines().filter(|l| l.starts_with("k  ")).collect();
    assert_eq!(listed.len(), 4, "{text}");
    for line in listed {
        assert_eq!(line, format!("k  {long}"));
    }
    let doc = run_json(&format!("put k '{long}'\nls --full\n"));
    assert_eq!(doc[1]["keys"][0]["value"], long.as_str());
}

#[test]
fn completions_come_out_of_the_same_definition_for_every_shell() {
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let out = memfork().args(["completions", shell]).assert().success();
        let script = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
        assert!(script.contains("memfork"), "{shell}: {script}");
        // Every subcommand, including the ones added since, is in the script.
        for sub in ["doctor", "completions", "watch", "plan", "task"] {
            assert!(
                script.contains(sub),
                "{shell} completion knows nothing of `{sub}`"
            );
        }
        assert!(
            !script.contains("\u{1b}["),
            "{shell}: a completion script carried colour"
        );
    }
    memfork()
        .args(["completions", "nushell"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "bash, zsh, fish, powershell, elvish",
        ));
}
