//! Every file the code compiles in is in the published package.
//!
//! The crate lists what it publishes in `include`. A data file read with
//! `include_str!` that the list leaves out builds here and fails for anybody
//! installing from crates.io, so each one is checked against the list.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Whether `path` (relative, `/`-separated) matches an `include` pattern:
/// an exact path, `dir/*.ext`, or `dir/**/*.ext`.
fn matches(pattern: &str, path: &str) -> bool {
    if let Some((dir, ext)) = pattern.split_once("/**/*") {
        return path.starts_with(&format!("{dir}/")) && path.ends_with(ext);
    }
    if let Some((dir, ext)) = pattern.split_once("/*") {
        return path
            .strip_prefix(&format!("{dir}/"))
            .is_some_and(|rest| !rest.contains('/') && rest.ends_with(ext));
    }
    pattern == path
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn every_compiled_in_file_is_published() {
    let manifest: toml::Value =
        toml::from_str(&std::fs::read_to_string(crate_dir().join("Cargo.toml")).unwrap()).unwrap();
    let include: Vec<String> = manifest["package"]["include"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    let mut files = Vec::new();
    sources(&crate_dir().join("src"), &mut files);
    let mut checked = 0;
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for piece in text.split("include_str!(\"").skip(1) {
            let target = piece.split('"').next().unwrap();
            let full = file.parent().unwrap().join(target);
            let relative = full
                .strip_prefix(crate_dir())
                .unwrap()
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            assert!(
                include.iter().any(|p| matches(p, &relative)),
                "{relative} is compiled in by {} but not in the package's `include`",
                file.display()
            );
            checked += 1;
        }
    }
    assert!(checked >= 7, "found only {checked} compiled-in files");
}
