//! The machine-wide policy: an administrator's file that user settings never
//! override, shown by `memfork doctor`, and enforced on every path it
//! reaches — the data directory, maintenance tasks, secret overrides.
//!
//! Every test applies its policy through `MEMFORK_POLICY_FILE`, which reads
//! beneath the machine file and can only add restrictions: nothing here
//! touches a system directory, and a real machine file, if the machine
//! running the tests has one, only makes these stricter.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::path::{Path, PathBuf};
use std::process::Output;

use memfork::policy::EXTRA_ENV;
use serde_json::Value as Json;
use support::Sandbox;

fn policy(sandbox: &Sandbox, text: &str) -> PathBuf {
    let file = sandbox.root().join("policy.toml");
    std::fs::write(&file, text).unwrap();
    file
}

fn run(sandbox: &Sandbox, file: &Path, args: &[&str]) -> Output {
    let mut cmd = sandbox.command();
    cmd.env(EXTRA_ENV, file).args(args);
    cmd.output().unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn fake_token() -> String {
    format!("ghp_{}", "aB3dE6gH9j".repeat(4))
}

#[test]
fn doctor_shows_the_policy_in_force_and_where_a_machine_file_would_go() {
    let sandbox = Sandbox::new();
    let file = policy(
        &sandbox,
        "maintenance_tasks = false\nsecret_overrides = false\nrace = false\n",
    );

    let short = run(&sandbox, &file, &["doctor"]);
    assert!(short.status.success(), "{}", stderr(&short));
    let text = stdout(&short);
    assert!(text.contains("policy        "), "{text}");
    assert!(text.contains("maintenance tasks off"), "{text}");
    assert!(text.contains("secret overrides off"), "{text}");
    assert!(text.contains("race off"), "{text}");
    // The short report is short: no persistence note, no per-client detail.
    assert!(!text.contains("survives restarts"), "{text}");
    assert!(!text.contains("    checked"), "{text}");
    assert!(text.contains("--verbose"), "{text}");

    let full = stdout(&run(&sandbox, &file, &["doctor", "--verbose"]));
    assert!(full.contains("Policy\n"), "{full}");
    assert!(full.contains("machine file"), "{full}");
    assert!(full.contains("extra file"), "{full}");
    assert!(full.contains("the machine file wins"), "{full}");
    assert!(full.contains("maintenance_tasks off"), "{full}");
    assert!(full.contains("dashboard     allowed"), "{full}");
    assert!(full.contains("survives restarts"), "{full}");
    // Each OS's machine location is a real path, named so an administrator
    // knows where to put the file.
    let expected = if cfg!(windows) {
        r"\memfork\policy.toml"
    } else if cfg!(target_os = "macos") {
        "/Library/Application Support/memfork/policy.toml"
    } else {
        "/etc/memfork/policy.toml"
    };
    assert!(full.contains(expected), "{full}");

    let doc: Json =
        serde_json::from_str(&stdout(&run(&sandbox, &file, &["--json", "doctor"]))).unwrap();
    assert_eq!(doc["policy"]["in_force"], true);
    assert_eq!(doc["policy"]["allows"]["maintenance_tasks"], false);
    assert_eq!(doc["policy"]["allows"]["secret_overrides"], false);
    assert_eq!(doc["policy"]["allows"]["race"], false);
    assert_eq!(doc["policy"]["allows"]["dashboard"], true);
    assert_eq!(doc["policy"]["machine_file_present"], false);
    assert!(
        doc["policy"]["machine_file"]
            .as_str()
            .unwrap()
            .ends_with(expected.trim_start_matches('\\'))
            || cfg!(windows)
    );
}

#[test]
fn without_a_policy_doctor_says_so_and_names_the_place() {
    let sandbox = Sandbox::new();
    let out = sandbox.command().arg("doctor").output().unwrap();
    let text = stdout(&out);
    assert!(text.contains("policy        none"), "{text}");
    assert!(text.contains("policy.toml"), "{text}");
}

#[test]
fn maintenance_tasks_switched_off_by_policy_cannot_be_switched_on() {
    let sandbox = Sandbox::new();
    let file = policy(&sandbox, "maintenance_tasks = false\n");

    let on = run(&sandbox, &file, &["maintain", "on"]);
    assert!(!on.status.success(), "{}", stdout(&on));
    let why = stderr(&on);
    assert!(
        why.contains("Maintenance tasks are switched off by the machine policy"),
        "{why}"
    );
    assert!(why.contains("policy.toml"), "{why}");
    assert!(why.contains("administrator"), "{why}");

    let status = run(&sandbox, &file, &["maintain", "status"]);
    assert!(status.status.success(), "{}", stderr(&status));
    let text = stdout(&status);
    assert!(text.contains("off"), "{text}");
    assert!(text.contains("machine policy"), "{text}");

    let json: Json = serde_json::from_str(&stdout(&run(
        &sandbox,
        &file,
        &["--json", "maintain", "status"],
    )))
    .unwrap();
    assert_eq!(json["on"], false);
    assert_eq!(json["policy_allows"], false);
}

#[test]
fn secret_overrides_switched_off_by_policy_are_refused_on_every_write_path() {
    let sandbox = Sandbox::new();
    let file = policy(&sandbox, "secret_overrides = false\n");
    let token = fake_token();

    // The refusal without an override is the ordinary one, and stores nothing.
    let plain = run(&sandbox, &file, &["put", "shop:note:1", &token]);
    assert!(!plain.status.success());
    assert!(
        stderr(&plain).contains("github-token"),
        "{}",
        stderr(&plain)
    );

    // With an override, the policy answers first.
    let forced = run(
        &sandbox,
        &file,
        &[
            "put",
            "shop:note:1",
            &token,
            "--allow-secret",
            "github-token",
        ],
    );
    assert!(!forced.status.success());
    let why = stderr(&forced);
    assert!(
        why.contains("Secret overrides are switched off by the machine policy"),
        "{why}"
    );
    assert!(
        !why.contains(&token),
        "the refusal repeated the secret: {why}"
    );

    let read = run(&sandbox, &file, &["get", "shop:note:1"]);
    assert!(
        !stdout(&read).contains(&token),
        "the value was stored anyway"
    );

    // The same through the ephemeral path, which runs the check locally.
    let local = run(
        &sandbox,
        &file,
        &[
            "--ephemeral",
            "put",
            "shop:note:1",
            &token,
            "--allow-secret",
            "github-token",
        ],
    );
    assert!(!local.status.success());
    assert!(
        stderr(&local).contains("switched off by the machine policy"),
        "{}",
        stderr(&local)
    );
}

#[test]
fn a_pinned_data_directory_beats_the_flag_and_the_environment() {
    let sandbox = Sandbox::new();
    let pinned = sandbox.root().join("pinned");
    let file = policy(
        &sandbox,
        &format!(
            "data_dir = {}\n",
            serde_json::to_string(&pinned.display().to_string()).unwrap()
        ),
    );

    // The sandbox sets MEMFORK_DATA_DIR to its own directory; the policy wins.
    let out = run(&sandbox, &file, &["put", "shop:note:1", "hello"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        pinned.join("memfork.endpoint").exists() || pinned.join("memfork.lock").exists(),
        "the daemon did not use the pinned directory; contents: {:?}",
        std::fs::read_dir(&pinned).map(|d| d.count())
    );
    let doctor = stdout(&run(&sandbox, &file, &["doctor"]));
    assert!(doctor.contains("pinned by the machine policy"), "{doctor}");

    // A flag asking for somewhere else is refused, with the reason.
    let other = sandbox.root().join("elsewhere");
    let refused = run(
        &sandbox,
        &file,
        &["--data-dir", &other.display().to_string(), "branches"],
    );
    assert!(!refused.status.success());
    let why = stderr(&refused);
    assert!(why.contains("pins the data directory"), "{why}");
    assert!(why.contains("administrator"), "{why}");
    assert!(!other.exists(), "the refused directory was created anyway");

    // Stopping goes to the pinned directory too.
    let stop = run(&sandbox, &file, &["stop"]);
    assert!(stop.status.success(), "{}", stderr(&stop));
}

#[test]
fn a_policy_that_cannot_be_read_stops_everything_but_doctor() {
    let sandbox = Sandbox::new();
    let file = policy(&sandbox, "maintenance_tasks = \"sometimes\"\n");

    for args in [
        vec!["branches"],
        vec!["--ephemeral", "branches"],
        vec!["put", "k", "v"],
        vec!["init", "--dry-run"],
        vec!["stop"],
    ] {
        let out = run(&sandbox, &file, &args);
        assert!(
            !out.status.success(),
            "{args:?} ran under an unreadable policy"
        );
        let why = stderr(&out);
        assert!(why.contains("could not be read"), "{args:?}: {why}");
        assert!(why.contains("policy.toml"), "{args:?}: {why}");
        assert!(why.contains("administrator"), "{args:?}: {why}");
    }
    let json = run(&sandbox, &file, &["--json", "branches"]);
    let err: Json = serde_json::from_str(stderr(&json).trim()).unwrap();
    assert!(err["error"].as_str().unwrap().contains("could not be read"));

    let doctor = run(&sandbox, &file, &["doctor"]);
    assert!(doctor.status.success(), "{}", stderr(&doctor));
    let text = stdout(&doctor);
    assert!(text.contains("UNREADABLE"), "{text}");
    let full = stdout(&run(&sandbox, &file, &["doctor", "--verbose"]));
    assert!(full.contains("UNREADABLE"), "{full}");
    assert!(full.contains("Every command but this one stops"), "{full}");

    // An unknown key is the same: a typo is not a policy.
    let typo = policy(&sandbox, "dashbord = false\n");
    let out = run(&sandbox, &typo, &["branches"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("dashbord"), "{}", stderr(&out));
}

#[test]
fn the_extra_file_cannot_weaken_a_stricter_one() {
    // Modelled in the library, since a test may not write the machine file:
    // the combination rule is what makes the extra file safe to honour.
    use memfork::policy::{Feature, Origin};
    let dir = tempfile::tempdir().unwrap();
    let extra = dir.path().join("extra.toml");
    std::fs::write(&extra, "race = true\nmaintenance_tasks = false\n").unwrap();
    let env = memfork::persist::datadir::MapEnv::from(&[
        (EXTRA_ENV, extra.to_str().unwrap()),
        ("ProgramData", dir.path().join("none").to_str().unwrap()),
    ]);
    let loaded = memfork::policy::load(memfork::clients::Os::Linux, &env).unwrap();
    assert!(
        loaded.allows(Feature::Race),
        "the extra file allowed what nothing forbade"
    );
    assert_eq!(
        loaded.forbidden_by(Feature::MaintenanceTasks),
        Some(Origin::Extra)
    );
}
