//! B2 — `memfork init` registers the server for every supported client,
//! re-running changes nothing, and `--dry-run` writes nothing.
//!
//! Two things make this testable without installing five clients:
//!
//! - `MEMFORK_HOME` redirects every user-scope path at a temporary directory,
//!   so nothing here can touch a real config.
//! - A **shim** on `PATH` stands in for a client's own `mcp add` command and
//!   records the arguments it was handed, so the exact command line is
//!   asserted rather than assumed.
//!
//! The three-OS requirement is met by resolving paths for a named OS rather
//! than the running one, so all three are checked from whichever one runs the
//! suite.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use serde_json::Value as Json;

mod support;

use assert_cmd::Command;
use support::memfork_assert as memfork;

/// A temporary world: a home directory, a working directory, and a `PATH`
/// holding only the shims this test asked for.
struct World {
    dir: tempfile::TempDir,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("home")).expect("home");
        std::fs::create_dir_all(dir.path().join("work")).expect("work");
        std::fs::create_dir_all(dir.path().join("bin")).expect("bin");
        World { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn work(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    fn bin(&self) -> PathBuf {
        self.dir.path().join("bin")
    }

    /// The MemFork a command from this world resolves to.
    fn memfork_path(&self) -> PathBuf {
        support::memfork_binary()
    }

    fn shim_log(&self) -> PathBuf {
        self.dir.path().join("shim.log")
    }

    /// Make a client look installed the way a real one does: by having created
    /// its own directory under the home directory, even with no config in it.
    fn mark_installed(&self, dir: &str) {
        std::fs::create_dir_all(self.home().join(dir)).expect("client dir");
    }

    /// Put a fake client command on `PATH` standing in for a real one.
    ///
    /// It behaves the way the real commands do, which is what makes the
    /// registration check testable: `mcp add` records what it was handed and
    /// remembers that the server now exists; `mcp get` and `mcp list` answer
    /// from that memory, failing when nothing has been added; `mcp remove`
    /// forgets it. A shim that always said yes, or always said no, would let
    /// a broken check pass.
    ///
    /// What it remembers is the whole command line it was given, and what it
    /// prints back is that line. Both real clients checked here do the same —
    /// `claude mcp get` and `codex mcp get` print the command they would run —
    /// and that is the only thing that tells a registration pointing at this
    /// MemFork from one pointing at a copy somewhere else.
    fn add_shim(&self, name: &str) {
        let log = self.shim_log();
        let marker = self.dir.path().join(format!("{name}.registered"));
        if cfg!(target_os = "windows") {
            let path = self.bin().join(format!("{name}.cmd"));
            std::fs::write(
                &path,
                format!(
                    "@echo off\r\n\
                     if \"%2\"==\"add\" (\r\n\
                     echo {name} %*>>\"{log}\"\r\n\
                     echo %*>\"{marker}\"\r\n\
                     exit /b 0\r\n\
                     )\r\n\
                     if \"%2\"==\"remove\" (\r\n\
                     echo {name} %*>>\"{log}\"\r\n\
                     del \"{marker}\" >nul 2>&1\r\n\
                     exit /b 0\r\n\
                     )\r\n\
                     if exist \"{marker}\" (\r\n\
                     echo memfork\r\n\
                     type \"{marker}\"\r\n\
                     exit /b 0\r\n\
                     )\r\n\
                     exit /b 1\r\n",
                    log = log.display(),
                    marker = marker.display()
                ),
            )
            .expect("shim written");
        } else {
            let path = self.bin().join(name);
            std::fs::write(
                &path,
                // Shell builtins only — no `cat`, no `rm`. These tests give
                // every process a `PATH` holding one directory, which is what
                // stops `memfork doctor` reaching a client the developer
                // really has installed. A shim that shelled out to `cat`
                // therefore printed nothing on Linux and macOS while working
                // on Windows, where `type` is built into cmd. Removal
                // truncates the file rather than deleting it, because `rm` is
                // a command too.
                format!(
                    "#!/bin/sh\n\
                     if [ \"$2\" = add ]; then\n\
                     \tprintf '%s %s\\n' '{name}' \"$*\" >> '{log}'\n\
                     \tprintf '%s\\n' \"$*\" > '{marker}'\n\
                     \texit 0\n\
                     fi\n\
                     if [ \"$2\" = remove ]; then\n\
                     \tprintf '%s %s\\n' '{name}' \"$*\" >> '{log}'\n\
                     \t: > '{marker}'\n\
                     \texit 0\n\
                     fi\n\
                     registered=''\n\
                     if [ -f '{marker}' ]; then\n\
                     \tread -r registered < '{marker}' || registered=''\n\
                     fi\n\
                     if [ -n \"$registered\" ]; then\n\
                     \techo memfork\n\
                     \techo \"$registered\"\n\
                     \texit 0\n\
                     fi\n\
                     exit 1\n",
                    log = log.display(),
                    marker = marker.display()
                ),
            )
            .expect("shim written");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("shim made executable");
            }
        }
    }

    /// The `mcp add` and `mcp remove` invocations the shims recorded, in
    /// order. Status queries are not logged, so this stays a record of what
    /// was *changed*.
    fn shim_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.shim_log())
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim().to_owned())
            .filter(|l| !l.is_empty())
            .collect()
    }

    /// A `memfork` command pointed at this world.
    fn memfork(&self) -> Command {
        let mut cmd = memfork();
        cmd.env("MEMFORK_HOME", self.home())
            .current_dir(self.work())
            .env("PATH", self.path_value());
        cmd
    }

    /// `PATH` with only the shim directory, plus whatever Windows needs to
    /// start a process at all.
    fn path_value(&self) -> String {
        let mut entries = vec![self.bin().display().to_string()];
        if cfg!(target_os = "windows") {
            if let Ok(root) = std::env::var("SystemRoot") {
                entries.push(format!("{root}\\System32"));
            }
        }
        entries.join(if cfg!(target_os = "windows") {
            ";"
        } else {
            ":"
        })
    }

    /// Every file under the temporary world, for "nothing was written" checks.
    fn files(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk(self.dir.path(), &mut out);
        out.sort();
        out
    }
}

fn run_json(cmd: &mut Command) -> Json {
    let out = cmd.assert().success();
    serde_json::from_slice(&out.get_output().stdout).expect("valid JSON")
}

/// The record for one client in `memfork init --json` output.
fn client<'a>(doc: &'a Json, id: &str) -> &'a Json {
    doc["clients"]
        .as_array()
        .expect("clients")
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no record for {id}"))
}

// ---- paths, for all three operating systems --------------------------------

#[test]
fn b2_config_paths_are_right_on_every_os() {
    // Resolved from a table rather than from the running platform, so Linux,
    // macOS and Windows are all checked wherever this runs. The registry is
    // the thing under test; `memfork doctor --json` exposes the resolution for
    // the running OS, and the unit tests in `clients` cover the other two.
    let world = World::new();
    let doc = run_json(world.memfork().args(["--json", "doctor"]));

    let home = world.home().display().to_string();
    // A `None` user path means the registry deliberately carries no location
    // for it, which is what stops any code path from reaching the file.
    let expected: &[(&str, Option<&str>, &str)] = &[
        ("claude-code", None, ".mcp.json"),
        ("cursor", Some(".cursor/mcp.json"), ".cursor/mcp.json"),
        ("codex", Some(".codex/config.toml"), ".codex/config.toml"),
        (
            "gemini-cli",
            Some(".gemini/settings.json"),
            ".gemini/settings.json",
        ),
        ("grok", Some(".grok/config.toml"), ".grok/config.toml"),
    ];
    let normalize = |p: &str| p.replace('\\', "/");

    for (id, user_tail, project_tail) in expected {
        let record = client(&doc, id);
        let project = record["project_config"].as_str().expect("a project path");
        assert_eq!(
            normalize(project),
            normalize(project_tail),
            "{id}: project config path"
        );

        match user_tail {
            None => assert!(
                record["user_config"].is_null(),
                "{id} should carry no user path at all: {record}"
            ),
            Some(tail) => {
                let user = record["user_config"].as_str().expect("a user path");
                assert!(
                    user.starts_with(&home),
                    "{id}: user config {user} is not under the home directory"
                );
                assert!(
                    normalize(user).ends_with(tail),
                    "{id}: user config {user} does not end with {tail}"
                );
            }
        }
    }
}

// ---- the client's own command is preferred ---------------------------------

#[test]
fn b2_a_clients_own_command_is_used_and_given_the_documented_arguments() {
    let world = World::new();
    for shim in ["claude", "codex", "gemini", "grok"] {
        world.add_shim(shim);
    }

    world.mark_installed(".cursor");

    let doc = run_json(world.memfork().args(["--json", "init"]));
    let exe = doc["command"]
        .as_str()
        .expect("the memfork path")
        .to_owned();

    // Every client that ships a command goes through it, not through its file.
    for id in ["claude-code", "codex", "gemini-cli", "grok"] {
        assert_eq!(
            client(&doc, id)["method"],
            "client command",
            "{id} did not use its own command"
        );
        assert_eq!(client(&doc, id)["action"], "run", "{id}");
    }
    // Cursor ships none, so it is the one that gets its file written.
    assert_eq!(client(&doc, "cursor")["method"], "config file");

    // The exact command lines, as each vendor documents them.
    let calls = world.shim_calls();
    let has = |needle: &str| calls.iter().any(|c| c.contains(needle));
    assert!(
        has(&format!("claude mcp add --scope user memfork -- {exe} mcp")),
        "claude was not called as documented: {calls:?}"
    );
    // Codex documents no scope flag.
    assert!(
        has(&format!("codex mcp add memfork -- {exe} mcp")),
        "codex was not called as documented: {calls:?}"
    );
    // Gemini takes the command positionally, with no `--` separator.
    assert!(
        has(&format!("gemini mcp add -s user memfork {exe} mcp")),
        "gemini was not called as documented: {calls:?}"
    );
    // Grok's user scope is the default and contributes no flag.
    assert!(
        has(&format!("grok mcp add memfork -- {exe} mcp")),
        "grok was not called as documented: {calls:?}"
    );
}

#[test]
fn b2_project_scope_uses_each_clients_project_flag() {
    let world = World::new();
    for shim in ["claude", "gemini", "grok"] {
        world.add_shim(shim);
    }
    let doc = run_json(
        world
            .memfork()
            .args(["--json", "init", "--scope", "project"]),
    );
    let exe = doc["command"]
        .as_str()
        .expect("the memfork path")
        .to_owned();
    let calls = world.shim_calls();

    assert!(
        calls.iter().any(|c| c.contains(&format!(
            "claude mcp add --scope project memfork -- {exe} mcp"
        ))),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains(&format!("gemini mcp add -s project memfork {exe} mcp"))),
        "{calls:?}"
    );
    assert!(
        calls.iter().any(|c| c.contains(&format!(
            "grok mcp add --scope project memfork -- {exe} mcp"
        ))),
        "{calls:?}"
    );
}

#[test]
fn b2_a_command_that_cannot_express_a_scope_falls_back_to_the_file() {
    // `codex mcp add` documents no scope flag, so project scope has to be
    // written to `.codex/config.toml` — and init has to say why.
    let world = World::new();
    world.add_shim("codex");

    let doc = run_json(
        world
            .memfork()
            .args(["--json", "init", "--scope", "project"]),
    );
    let codex = client(&doc, "codex");
    assert_eq!(codex["method"], "config file");
    assert!(
        codex["note"]
            .as_str()
            .is_some_and(|n| n.contains("no project scope")),
        "init did not explain the fallback: {codex}"
    );

    let written = world.work().join(".codex").join("config.toml");
    assert!(written.exists(), "the project config was not written");
    let text = std::fs::read_to_string(&written).unwrap();
    assert!(text.contains("[mcp_servers.memfork]"), "{text}");
}

// ---- Claude Code never has its user config edited --------------------------

#[test]
fn b2_claude_codes_user_config_is_never_edited() {
    // `~/.claude.json` holds the OAuth session. With no `claude` command
    // available, init must register for the project instead and say so —
    // never open that file for writing.
    let world = World::new();
    world.mark_installed(".claude");
    let user_config = world.home().join(".claude.json");
    let original = r#"{"oauthAccount":{"secret":"do-not-touch"},"mcpServers":{}}"#;
    std::fs::write(&user_config, original).expect("seeded");

    let doc = run_json(
        world
            .memfork()
            .args(["--json", "init", "--client", "claude-code"]),
    );
    let record = client(&doc, "claude-code");

    assert_eq!(record["scope"], "project", "{record}");
    assert_eq!(record["method"], "config file");
    let note = record["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("session credentials") && note.contains("not on PATH"),
        "init did not explain itself: {record}"
    );

    // The user config is byte-for-byte what it was.
    assert_eq!(std::fs::read_to_string(&user_config).unwrap(), original);
    // And no backup of it was made, because it was never opened for writing.
    assert!(
        !world
            .files()
            .iter()
            .any(|p| p.to_string_lossy().contains(".claude.json.memfork-backup")),
        "a backup of the user config was created, so it was about to be written"
    );

    // The project file carries the registration instead.
    let project = world.work().join(".mcp.json");
    let doc: Json = serde_json::from_str(&std::fs::read_to_string(&project).unwrap()).unwrap();
    assert_eq!(doc["mcpServers"]["memfork"]["type"], "stdio");
    assert_eq!(doc["mcpServers"]["memfork"]["args"][0], "mcp");
}

// ---- idempotence and dry runs ----------------------------------------------

#[test]
fn b2_re_running_changes_nothing() {
    let world = World::new();
    // Cursor is the file-based client, so a second run has something to be
    // idempotent about.
    world.mark_installed(".cursor");
    world.memfork().arg("init").assert().success();

    let config = world.home().join(".cursor").join("mcp.json");
    assert!(config.exists(), "the first run wrote nothing");
    let after_first = std::fs::read_to_string(&config).unwrap();
    let files_after_first = world.files();

    let doc = run_json(world.memfork().args(["--json", "init"]));
    assert_eq!(
        client(&doc, "cursor")["action"],
        "already registered",
        "the second run did not recognise its own work"
    );

    assert_eq!(std::fs::read_to_string(&config).unwrap(), after_first);
    assert_eq!(
        world.files(),
        files_after_first,
        "the second run created or removed files"
    );
}

#[test]
fn b2_a_dry_run_writes_nothing_and_runs_nothing() {
    let world = World::new();
    for shim in ["claude", "codex", "gemini", "grok"] {
        world.add_shim(shim);
    }
    world.mark_installed(".cursor");
    let before = world.files();

    let output = world
        .memfork()
        .args(["init", "--dry-run"])
        .assert()
        .success();
    let text = String::from_utf8_lossy(&output.get_output().stdout).into_owned();

    // It says exactly what it would do.
    assert!(text.contains("would run"), "{text}");
    assert!(text.contains("mcp add"), "{text}");
    assert!(text.contains("would add"), "{text}");
    assert!(text.contains("this was a dry run"), "{text}");
    // Including the diff for a file it would write.
    assert!(text.contains("+ "), "no diff was shown:\n{text}");

    assert_eq!(world.files(), before, "a dry run wrote files");
    assert!(
        world.shim_calls().is_empty(),
        "a dry run executed a client command: {:?}",
        world.shim_calls()
    );
}

#[test]
fn b2_a_dry_run_shows_the_exact_command_it_would_execute() {
    let world = World::new();
    world.add_shim("claude");
    let doc = run_json(world.memfork().args(["--json", "init", "--dry-run"]));
    let exe = doc["command"].as_str().expect("the memfork path");
    let record = client(&doc, "claude-code");
    assert_eq!(record["action"], "would run");
    assert_eq!(
        record["detail"],
        format!("claude mcp add --scope user memfork -- {exe} mcp")
    );
    assert!(world.shim_calls().is_empty());
}

// ---- selection and failure handling ----------------------------------------

#[test]
fn b2_a_single_client_can_be_selected() {
    let world = World::new();
    world.mark_installed(".cursor");
    let doc = run_json(
        world
            .memfork()
            .args(["--json", "init", "--client", "cursor"]),
    );
    assert_eq!(doc["clients"].as_array().map(Vec::len), Some(1));
    assert_eq!(doc["clients"][0]["id"], "cursor");
    assert!(world.home().join(".cursor").join("mcp.json").exists());
    assert!(!world.home().join(".codex").exists());
}

#[test]
fn b2_an_unknown_client_is_refused_with_the_list_of_known_ones() {
    let world = World::new();
    world
        .memfork()
        .args(["init", "--client", "emacs"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("cursor"));
}

#[test]
fn b2_an_existing_config_keeps_everything_it_already_had() {
    let world = World::new();
    let dir = world.home().join(".cursor");
    std::fs::create_dir_all(&dir).expect("dir");
    let config = dir.join("mcp.json");
    std::fs::write(
        &config,
        r#"{
  "mcpServers": {
    "someone-elses-server": {
      "command": "other",
      "args": ["serve", "--flag"],
      "env": { "TOKEN": "keep-me" }
    }
  }
}
"#,
    )
    .expect("seeded");

    world
        .memfork()
        .args(["init", "--client", "cursor"])
        .assert()
        .success();

    let doc: Json = serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(
        doc["mcpServers"]["someone-elses-server"]["command"],
        "other"
    );
    assert_eq!(
        doc["mcpServers"]["someone-elses-server"]["args"][1],
        "--flag"
    );
    assert_eq!(
        doc["mcpServers"]["someone-elses-server"]["env"]["TOKEN"],
        "keep-me"
    );
    assert!(doc["mcpServers"]["memfork"]["command"].is_string());

    // And the original was backed up before being touched.
    assert!(
        world
            .files()
            .iter()
            .any(|p| p.to_string_lossy().contains("mcp.json.memfork-backup")),
        "no backup was kept"
    );
}

#[test]
fn b2_a_failing_client_command_is_reported_rather_than_swallowed() {
    let world = World::new();
    // A shim that always fails, standing in for a client that refuses.
    let path = if cfg!(target_os = "windows") {
        let p = world.bin().join("claude.cmd");
        std::fs::write(&p, "@echo off\r\necho nope 1>&2\r\nexit /b 1\r\n").expect("shim");
        p
    } else {
        let p = world.bin().join("claude");
        std::fs::write(&p, "#!/bin/sh\necho nope >&2\nexit 1\n").expect("shim");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        p
    };
    assert!(path.exists());

    world
        .memfork()
        .args(["init", "--client", "claude-code"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Claude Code"));
}

#[test]
fn b2_doctor_reports_what_init_did() {
    let world = World::new();
    world.mark_installed(".cursor");
    world
        .memfork()
        .args(["init", "--client", "cursor"])
        .assert()
        .success();

    let doc = run_json(world.memfork().args(["--json", "doctor"]));
    assert_eq!(client(&doc, "cursor")["registered"], true);
    assert_eq!(client(&doc, "codex")["registered"], false);

    // And doctor says, in both renderings, where memory is kept.
    assert_eq!(doc["persistence"]["enabled"], true);
    let text = world
        .memfork()
        .args(["doctor", "--verbose"])
        .assert()
        .success();
    let text = String::from_utf8_lossy(&text.get_output().stdout).into_owned();
    assert!(text.contains("survives restarts"), "{text}");
}

#[test]
fn b2_a_client_installed_but_never_configured_is_still_found() {
    // A client that ships no command and has never had an MCP server added has
    // no config file yet — only its own directory. Treating that as "not
    // installed" would silently skip exactly the person `memfork init` is for.
    let world = World::new();
    let doc = run_json(world.memfork().args(["--json", "init", "--dry-run"]));
    assert_eq!(
        client(&doc, "cursor")["action"],
        "not installed",
        "nothing on disk should mean nothing to configure"
    );

    world.mark_installed(".cursor");
    let doc = run_json(world.memfork().args(["--json", "init", "--dry-run"]));
    assert_eq!(client(&doc, "cursor")["action"], "would add");
}

#[test]
fn b2_a_windows_style_command_shim_is_executed_by_its_resolved_path() {
    // On Windows these commands are usually `.cmd` shims, and a bare name
    // cannot be spawned: `CreateProcess` will not run a batch file it looked up
    // itself. Registration has to use the path `PATH` resolved to. This test is
    // meaningful on every OS — it just exercises the shim — but it is the
    // Windows leg of CI that would have caught the bug.
    let world = World::new();
    world.add_shim("claude");
    world
        .memfork()
        .args(["init", "--client", "claude-code"])
        .assert()
        .success();
    assert_eq!(
        world.shim_calls().len(),
        1,
        "the client command was not executed: {:?}",
        world.shim_calls()
    );
}

// ---- registration is read from the client, not from its files --------------

#[test]
fn b2_doctor_asks_a_clients_own_command_rather_than_reading_its_files() {
    // The bug this replaces: doctor read `~/.claude.json`, did not find an
    // entry matching byte for byte what it would have written, and reported a
    // server that `claude mcp get memfork` showed as registered and connected
    // as missing. A client owns its configuration; only it can answer.
    let world = World::new();
    world.add_shim("claude");

    // Before registering, the client says no.
    let doc = run_json(world.memfork().args(["--json", "doctor"]));
    let record = client(&doc, "claude-code");
    assert_eq!(record["registered"], false);
    assert_eq!(record["checked"]["how"], "command");
    assert_eq!(record["checked"]["detail"], "claude mcp get memfork");

    // Register through the command, then ask again.
    world
        .memfork()
        .args(["init", "--client", "claude-code"])
        .assert()
        .success();

    let doc = run_json(world.memfork().args(["--json", "doctor"]));
    let record = client(&doc, "claude-code");
    assert_eq!(
        record["registered"], true,
        "doctor did not believe the client that said it was registered"
    );
    assert_eq!(record["checked"]["how"], "command");
}

#[test]
fn b2_the_claude_user_config_is_never_read_either() {
    // Not writing it is not enough: the registry carries no user path for this
    // client at all, so no code path can reach the file. A poisoned file that
    // would break any reader proves it.
    let world = World::new();
    world.add_shim("claude");
    let user_config = world.home().join(".claude.json");
    std::fs::write(&user_config, "{ this is not json and never was").expect("seeded");

    // Both commands work, and neither complains about the file.
    let doc = run_json(world.memfork().args(["--json", "doctor"]));
    let record = client(&doc, "claude-code");
    assert_eq!(record["checked"]["how"], "command");
    assert!(
        !record.to_string().contains(".claude.json"),
        "doctor named the user config: {record}"
    );
    assert!(
        record["project_config"]
            .as_str()
            .is_some_and(|p| p.ends_with(".mcp.json")),
        "{record}"
    );

    world
        .memfork()
        .args(["init", "--client", "claude-code"])
        .assert()
        .success();

    // The file is exactly as it was, and no backup of it exists.
    assert_eq!(
        std::fs::read_to_string(&user_config).unwrap(),
        "{ this is not json and never was"
    );
    assert!(!world
        .files()
        .iter()
        .any(|p| p.to_string_lossy().contains(".claude.json.memfork-backup")));
}

#[test]
fn b2_a_client_whose_command_is_missing_says_it_could_not_ask() {
    // With no `claude` on PATH there is no way to know, and MemFork will not
    // guess by reading the file. Saying "unknown" and why is the honest answer.
    let world = World::new();
    world.mark_installed(".claude");

    let doc = run_json(world.memfork().args(["--json", "doctor"]));
    let record = client(&doc, "claude-code");
    assert!(record["registered"].is_null(), "{record}");
    assert!(
        record["unknown_because"]
            .as_str()
            .is_some_and(|w| w.contains("does not read")),
        "{record}"
    );
}

// ---- registrations that point at another MemFork ---------------------------

/// Register a client against some other MemFork, the way an earlier install
/// would have left it.
fn register_elsewhere(world: &World, shim: &str, path: &str) {
    let status = std::process::Command::new(world.bin().join(if cfg!(target_os = "windows") {
        format!("{shim}.cmd")
    } else {
        shim.to_owned()
    }))
    .args(["mcp", "add", "memfork", "--", path, "mcp"])
    .status()
    .expect("the shim ran");
    assert!(status.success());
}

/// Where an earlier install would plausibly have put it.
const OLD_INSTALL: &str = if cfg!(target_os = "windows") {
    r"C:\Users\somebody\.cargo\bin\memfork.exe"
} else {
    "/home/somebody/.cargo/bin/memfork"
};

#[test]
fn b2_a_backup_is_only_mentioned_when_a_file_was_backed_up() {
    // Registering through a client's own command writes no file, so there is
    // nothing to back up. The line said `backup:` and then repeated the
    // command, which promised a file that does not exist.
    let world = World::new();
    world.add_shim("claude");

    // A config that already exists, because a backup is a copy of something:
    // creating a file leaves nothing to keep, so a fresh install would make
    // this test pass by finding no `backup:` line at all.
    let cursor = world.home().join(".cursor");
    std::fs::create_dir_all(&cursor).expect("cursor dir");
    std::fs::write(
        cursor.join("mcp.json"),
        r#"{"mcpServers":{"someone-elses-server":{"command":"other"}}}"#,
    )
    .expect("an existing config");

    let output = world
        .memfork()
        .arg("init")
        .assert()
        .success()
        .get_output()
        .clone();
    let text = String::from_utf8_lossy(&output.stdout).into_owned();

    for line in text.lines() {
        let Some((_, backup)) = line.split_once("backup:") else {
            continue;
        };
        let backup = backup.trim();
        assert!(
            !backup.contains("mcp add"),
            "a command was reported as a backup file: {line}"
        );
        assert!(
            Path::new(backup).exists(),
            "`backup:` named something that is not a file: {backup}"
        );
    }

    // Cursor has no command of its own, so its file *is* written, and that one
    // does leave a backup — otherwise this test would pass by finding nothing.
    assert!(
        text.contains("backup:"),
        "no backup was reported at all, so this proved nothing:\n{text}"
    );
}

#[test]
fn b2_the_shim_remembers_the_command_it_was_given() {
    // Not a test of MemFork: a test of the stand-in, so that when the tests
    // above fail it is clear which side is wrong. Everything about stale
    // registrations rests on a client being able to say *what* it registered,
    // and this is the one place that behaviour is asserted directly.
    let world = World::new();
    world.add_shim("claude");
    register_elsewhere(&world, "claude", OLD_INSTALL);

    let shim = world.bin().join(if cfg!(target_os = "windows") {
        "claude.cmd"
    } else {
        "claude"
    });
    let output = std::process::Command::new(&shim)
        .args(["mcp", "get", "memfork"])
        .output()
        .expect("the shim ran");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        output.status.success(),
        "the shim says nothing is registered after an add: {said}"
    );
    assert!(
        said.contains("memfork"),
        "the shim did not name the server: {said}"
    );
    assert!(
        said.contains(OLD_INSTALL),
        "the shim did not report the command it was given, so nothing can \
         tell one MemFork from another. It said: {said}"
    );
}

#[test]
fn b2_a_registration_pointing_at_another_memfork_is_repointed() {
    // The case every install creates: the binary moves, and the client goes on
    // launching the path it was given. Nothing errors — the tools just stop
    // appearing — so `memfork init` has to notice and put it right.
    let world = World::new();
    world.add_shim("claude");
    register_elsewhere(&world, "claude", OLD_INSTALL);

    let doc = run_json(
        world
            .memfork()
            .args(["--json", "init", "--client", "claude-code"]),
    );
    let record = client(&doc, "claude-code");
    assert_eq!(record["action"], "repoint", "{record}");
    assert_eq!(record["method"], "client command", "{record}");
    assert!(
        record["detail"]
            .as_str()
            .is_some_and(|d| d.contains(OLD_INSTALL)),
        "init did not say what it was replacing: {record}"
    );

    // Removed, then added: `mcp add` over a name that already exists either
    // fails or duplicates, depending on the client, and neither is a fix.
    let calls = world.shim_calls();
    let removed = calls.iter().position(|c| c.contains("mcp remove"));
    let added = calls.iter().rposition(|c| c.contains("mcp add"));
    assert!(
        removed.is_some() && added > removed,
        "expected a remove followed by an add: {calls:?}"
    );
    assert!(
        calls
            .last()
            .is_some_and(|c| c.contains(world.memfork_path().to_str().expect("a printable path"))),
        "the new registration does not point here: {calls:?}"
    );

    // And now it is registered, so running again changes nothing.
    let again = run_json(
        world
            .memfork()
            .args(["--json", "init", "--client", "claude-code"]),
    );
    assert_eq!(
        client(&again, "claude-code")["action"],
        "already registered",
        "repointing was not durable"
    );
}

#[test]
fn b2_doctor_flags_a_registration_that_points_somewhere_else() {
    // Doctor is where someone looks when the tools are missing. Reporting this
    // as "registered" is how they spend an hour not finding the reason.
    let world = World::new();
    world.add_shim("claude");
    register_elsewhere(&world, "claude", OLD_INSTALL);

    let doc = run_json(world.memfork().args(["--json", "doctor"]));
    let record = client(&doc, "claude-code");
    assert_eq!(
        record["registered"], false,
        "a registration pointing elsewhere was reported as registered: {record}"
    );
    assert_eq!(record["points_at"], OLD_INSTALL, "{record}");
    assert_eq!(doc["needs_init"], true, "doctor did not suggest a fix");

    let text = world.memfork().arg("doctor").assert().success();
    let text = String::from_utf8_lossy(&text.get_output().stdout).into_owned();
    assert!(
        text.contains("registered, but not to this MemFork"),
        "the report does not name the problem:\n{text}"
    );
    assert!(text.contains(OLD_INSTALL), "{text}");
    assert!(
        text.contains("Run `memfork init`"),
        "the report does not say how to fix it:\n{text}"
    );
}

#[test]
fn b2_a_dry_run_does_not_touch_a_stale_registration() {
    // A dry run has to be safe on exactly the machine where the fix is
    // needed, which is the one whose configuration is already wrong.
    let world = World::new();
    world.add_shim("claude");
    register_elsewhere(&world, "claude", OLD_INSTALL);

    let doc =
        run_json(
            world
                .memfork()
                .args(["--json", "init", "--client", "claude-code", "--dry-run"]),
        );
    // Claude Code documents `mcp remove`, so this one would be repointed.
    // What matters here is that the plan is visible before anything runs.
    assert_eq!(client(&doc, "claude-code")["action"], "would repoint");
    assert!(
        world.shim_calls().iter().all(|c| !c.contains("mcp remove")),
        "a dry run removed a registration: {:?}",
        world.shim_calls()
    );
}

#[test]
fn b2_init_is_idempotent_through_a_clients_own_command() {
    // The file-based path is covered above; this is the command path, where
    // idempotence depends on believing the client's own answer.
    let world = World::new();
    world.add_shim("claude");

    world
        .memfork()
        .args(["init", "--client", "claude-code"])
        .assert()
        .success();
    assert_eq!(world.shim_calls().len(), 1);

    let doc = run_json(
        world
            .memfork()
            .args(["--json", "init", "--client", "claude-code"]),
    );
    assert_eq!(client(&doc, "claude-code")["action"], "already registered");
    assert_eq!(
        world.shim_calls().len(),
        1,
        "the second run registered again: {:?}",
        world.shim_calls()
    );
}

// ---- the closing hint ------------------------------------------------------

#[test]
fn b2_doctor_suggests_init_only_when_something_needs_it() {
    let world = World::new();
    world.add_shim("claude");

    // An installed, unregistered client: say so, and name it.
    let text = world.memfork().arg("doctor").assert().success();
    let text = String::from_utf8_lossy(&text.get_output().stdout).into_owned();
    assert!(text.contains("Run `memfork init`"), "{text}");
    assert!(text.contains("Claude Code"), "{text}");

    // Once registered, stop telling people to fix what is not broken.
    world
        .memfork()
        .args(["init", "--client", "claude-code"])
        .assert()
        .success();
    let text = world.memfork().arg("doctor").assert().success();
    let text = String::from_utf8_lossy(&text.get_output().stdout).into_owned();
    assert!(
        !text.contains("Run `memfork init`"),
        "doctor still suggested init with everything registered:\n{text}"
    );
    assert!(
        text.contains("Every detected client has MemFork registered"),
        "{text}"
    );
}

#[test]
fn b2_doctor_says_when_no_client_was_detected_at_all() {
    let world = World::new();
    let text = world.memfork().arg("doctor").assert().success();
    let text = String::from_utf8_lossy(&text.get_output().stdout).into_owned();
    assert!(text.contains("No MCP clients were detected"), "{text}");
    assert!(!text.contains("Run `memfork init`"), "{text}");

    let doc = run_json(world.memfork().args(["--json", "doctor"]));
    assert_eq!(doc["needs_init"], false);
}

// ---- the eleven clients after the first five ------------------------------------

#[test]
fn b2_a_new_clients_command_is_called_as_its_documentation_says() {
    let world = World::new();
    for shim in ["qwen", "copilot", "devin", "opencode"] {
        world.add_shim(shim);
    }
    let doc = run_json(world.memfork().args(["--json", "init"]));
    let exe = doc["command"]
        .as_str()
        .expect("the memfork path")
        .to_owned();
    let calls = world.shim_calls();
    let has = |needle: &str| calls.iter().any(|c| c.contains(needle));
    assert!(
        has(&format!("qwen mcp add -s user memfork {exe} mcp")),
        "{calls:?}"
    );
    assert!(
        has(&format!("copilot mcp add memfork -- {exe} mcp")),
        "{calls:?}"
    );
    assert!(
        has(&format!("devin mcp add -s user memfork -- {exe} mcp")),
        "{calls:?}"
    );
    assert!(
        has(&format!("opencode mcp add memfork -- {exe} mcp")),
        "{calls:?}"
    );
    for id in ["qwen-code", "copilot-cli", "devin", "opencode"] {
        assert_eq!(client(&doc, id)["method"], "client command", "{id}");
    }
    // Re-running asks each command and changes nothing.
    let again = run_json(world.memfork().args(["--json", "init"]));
    for id in ["qwen-code", "copilot-cli", "devin", "opencode"] {
        assert_eq!(client(&again, id)["action"], "already registered", "{id}");
    }
    assert_eq!(world.shim_calls().len(), calls.len());
}

#[test]
fn b2_a_settings_file_with_comments_keeps_them() {
    // Zed's settings.json: comments, a trailing comma, keys MemFork knows
    // nothing about. Only the one member is added.
    let world = World::new();
    // Where Zed keeps its settings on this OS, as the registry resolves it
    // for a stand-in home: %APPDATA%\Zed on Windows, ~/.config/zed elsewhere.
    let home = world.home().display().to_string();
    let config = PathBuf::from(
        memfork::clients::find("zed")
            .and_then(|c| c.file)
            .expect("zed registers by file")
            .path_for(
                memfork::clients::Scope::User,
                memfork::clients::Os::current(),
                &memfork::clients::Vars::for_home(&home, memfork::clients::Os::current()),
            )
            .expect("a user path"),
    );
    std::fs::create_dir_all(config.parent().unwrap()).expect("dir");
    let original = "// Zed settings\n{\n  \"theme\": \"One Dark\", // mine\n  \"context_servers\": {\n    \"other\": {\n      \"command\": \"other\",\n      \"args\": [\"serve\"],\n    },\n  },\n}\n";
    std::fs::write(&config, original).expect("seeded");

    let doc = run_json(world.memfork().args(["--json", "init", "--client", "zed"]));
    assert_eq!(client(&doc, "zed")["method"], "config file");
    assert_eq!(client(&doc, "zed")["action"], "add", "{doc}");
    let after = std::fs::read_to_string(&config).unwrap();
    assert!(
        after.starts_with("// Zed settings\n{\n  \"theme\": \"One Dark\", // mine\n"),
        "{after}"
    );
    assert!(
        after.contains(
            "\"other\": {\n      \"command\": \"other\",\n      \"args\": [\"serve\"],\n    },\n"
        ),
        "{after}"
    );
    assert!(after.contains("\"memfork\": {"), "{after}");
    assert!(after.ends_with("  },\n}\n"), "{after}");

    let again = run_json(world.memfork().args(["--json", "init", "--client", "zed"]));
    assert_eq!(client(&again, "zed")["action"], "already registered");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), after);

    // Doctor reads the same file and says it is registered.
    let doc = run_json(world.memfork().args(["--json", "doctor"]));
    assert_eq!(client(&doc, "zed")["registered"], true);
}

#[test]
fn b2_doctor_says_what_a_registry_entry_could_not_confirm() {
    let world = World::new();
    let doc = run_json(world.memfork().args(["--json", "doctor"]));
    let cursor = client(&doc, "cursor");
    assert!(
        cursor["unverified"]
            .as_str()
            .is_some_and(|w| w.contains("MCP initialize")),
        "{cursor}"
    );
    assert!(client(&doc, "codex")["unverified"].is_null());
    let text = world
        .memfork()
        .args(["doctor", "--verbose"])
        .assert()
        .success();
    let text = String::from_utf8_lossy(&text.get_output().stdout).into_owned();
    assert!(text.contains("    unverified  "), "{text}");
}
