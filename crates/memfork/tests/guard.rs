//! The control on test isolation.
//!
//! A test that starts `memfork` without saying where its data goes will write
//! into whoever is running it. That happened once: persistence became the
//! default, and tests that had been harmless began
//! writing to the developer's own store. It happened a second time when a
//! stale test ran `memfork serve` with no data directory.
//!
//! Being careful is not a control. These two tests are:
//!
//! - the binary refuses the real per-user directory when the guard is set, and
//! - no test builds a `memfork` command except through the sandbox, which sets
//!   that guard on every process it starts.
//!
//! The second is what catches the next mistake, because it fails when the test
//! is written rather than when it runs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::path::Path;

/// Every test source file in this crate.
fn test_sources() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut out = Vec::new();
    let mut stack = vec![dir];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .expect("the tests directory is readable")
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let name = path
                    .strip_prefix(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests"))
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                let text = std::fs::read_to_string(&path).expect("readable");
                out.push((name, text));
            }
        }
    }
    assert!(out.len() > 4, "the test sources could not be found");
    out
}

#[test]
fn no_test_builds_a_memfork_command_outside_the_sandbox() {
    // `support` is the one place allowed to name the binary, because it is the
    // one place that sets the data directory and the guard.
    let offenders: Vec<String> = test_sources()
        .into_iter()
        .filter(|(name, _)| !name.replace('\\', "/").starts_with("support/"))
        .filter(|(_, text)| text.contains("cargo_bin(\"memfork\")"))
        .map(|(name, _)| name)
        .collect();

    assert!(
        offenders.is_empty(),
        "these tests build a `memfork` command directly instead of going \
         through `support::memfork()` or `Sandbox::command()`, so nothing \
         stops them writing to the real data directory:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn the_binary_refuses_the_real_data_directory_when_the_guard_is_set() {
    // The runtime half. A command that resolves the per-user directory must
    // fail loudly rather than create it.
    let output = support::memfork()
        .env_remove(memfork::persist::datadir::DATA_DIR_ENV)
        .arg("doctor")
        .output()
        .expect("doctor ran");
    let said = String::from_utf8_lossy(&output.stdout);
    assert!(
        said.contains("refusing to use the real per-user data directory"),
        "doctor resolved the real data directory under the guard:\n{said}"
    );
}

#[test]
fn a_persistent_command_without_a_data_directory_refuses_to_start() {
    for args in [
        vec!["mcp"],
        vec!["serve", "--port", "0", "--idle-timeout", "1"],
    ] {
        let output = support::memfork()
            .env_remove(memfork::persist::datadir::DATA_DIR_ENV)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("ran");
        assert!(
            !output.status.success(),
            "`memfork {}` started with no data directory under the guard",
            args.join(" ")
        );
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(
            said.contains("refusing to use the real per-user data directory"),
            "`memfork {}` failed for the wrong reason: {said}",
            args.join(" ")
        );
    }
}

#[test]
fn the_real_data_directory_is_untouched_by_this_suite() {
    // Whatever else happens, this asserts the outcome the guard exists for.
    //
    // It used to assert that the directory held no MemFork files at all, which
    // was the same thing only while nobody had MemFork installed. Once the
    // developer runs it for real, that directory is *supposed* to have a store
    // in it, and a test that failed on their own data would be measuring the
    // wrong thing. So this watches for change rather than for existence: take
    // the directory as it is, try the commands that could write to it, and
    // assert that not one byte moved.
    let Some(real) = support::real_per_user_dir() else {
        return;
    };

    fn snapshot(dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out: Vec<String> = entries
            .flatten()
            .map(|e| {
                let meta = e.metadata().ok();
                format!(
                    "{}|{}|{:?}",
                    e.file_name().to_string_lossy(),
                    meta.as_ref().map(|m| m.len()).unwrap_or_default(),
                    meta.and_then(|m| m.modified().ok())
                )
            })
            .collect();
        out.sort();
        out
    }

    let before = snapshot(&real);

    // Every way a test can start a persistent process, with nothing telling it
    // where to put its data. Under the guard each of these must refuse.
    for args in [
        vec!["doctor"],
        vec!["mcp"],
        vec!["serve", "--port", "0", "--idle-timeout", "1"],
        vec!["put", "k", "v"],
    ] {
        let _ = support::memfork()
            .env_remove(memfork::persist::datadir::DATA_DIR_ENV)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output();
    }

    if snapshot(&real) == before {
        return;
    }

    // Something changed. Before calling it a failure, ask whether it could
    // have been us: a developer running this suite may also be *using*
    // MemFork, and their own daemon writing to their own store while the
    // suite happens to be looking is not this suite touching it. Nothing here
    // can hold that directory — every command the suite builds is refused —
    // so an owner is proof the change came from outside.
    if let Some(owner) = memfork::persist::lock::owner(&real) {
        eprintln!(
            "note: {} changed while this test ran, but process {} owns it — \
             something outside this suite is using MemFork, so this check \
             cannot attribute the change and is not asserting on it.",
            real.display(),
            owner.pid
        );
        return;
    }

    panic!(
        "this test suite changed the real data directory {}, and nothing \
         outside it holds the directory. Something started a persistent \
         command without a data directory.",
        real.display()
    );
}
