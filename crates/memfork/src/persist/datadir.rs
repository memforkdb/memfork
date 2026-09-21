//! Where MemFork keeps its data (DESIGN §4.5).
//!
//! Resolved by hand rather than by a crate. The obvious choice, `dirs`, pulls
//! in `option-ext`, which is MPL-2.0 and outside the licence policy; the next
//! choice, `etcetera`, is permissively licensed but buys about forty lines of
//! `match`. DESIGN §4.5 records the decision.
//!
//! Resolution is written against an explicit operating system and an explicit
//! set of environment variables rather than against the machine it runs on, so
//! all three platforms' answers are checked wherever the tests happen to run.

use std::path::PathBuf;

use crate::clients::Os;

/// The directory name used inside a project, and the leaf name used inside a
/// per-user data directory.
pub const DIR_NAME: &str = ".memfork";

/// The leaf directory inside the per-user data directory.
const APP_NAME: &str = "memfork";

/// Environment variable that overrides everything else.
pub const DATA_DIR_ENV: &str = "MEMFORK_DATA_DIR";

/// Environment variable that forbids falling back to the real per-user
/// directory.
///
/// Set by every test, and inherited by every process a test spawns. A test
/// that forgets to say where its data goes then fails loudly instead of
/// quietly writing into the developer's own store — which is exactly what
/// happened once, and is the sort of thing that should be impossible rather
/// than merely fixed.
pub const FORBID_PER_USER_ENV: &str = "MEMFORK_FORBID_PER_USER_DATA_DIR";

/// Why the data directory is where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `MEMFORK_DATA_DIR` said so.
    Environment,
    /// A `.memfork` directory already exists beside the working directory.
    Project,
    /// The platform's per-user data directory.
    PerUser,
}

/// Where the data lives, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir {
    /// The directory itself.
    pub path: PathBuf,
    /// How it was chosen.
    pub source: Source,
}

/// Why there is no data directory to use.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NoDataDir {
    /// Nothing in the environment says where a per-user directory would be.
    #[error("cannot work out where to keep data; set {DATA_DIR_ENV}")]
    Unknown,
    /// The per-user directory was resolved, and this environment forbids it.
    #[error(
        "refusing to use the real per-user data directory {path}.
         {FORBID_PER_USER_ENV} is set, which tests do so that a test which          forgets to choose a directory fails here instead of writing into          somebody's own store. Set {DATA_DIR_ENV} to a temporary directory."
    )]
    Forbidden {
        /// The directory that was refused.
        path: PathBuf,
    },
}

/// Look up an environment variable.
///
/// Taken as a parameter so the resolution can be tested for every platform
/// from any one of them.
pub trait Env {
    /// The value of a variable, if it is set and non-empty.
    fn get(&self, key: &str) -> Option<String>;
}

/// The real environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealEnv;

impl Env for RealEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.is_empty())
    }
}

/// A fixed set of variables, for tests.
#[derive(Debug, Clone, Default)]
pub struct MapEnv(pub std::collections::BTreeMap<String, String>);

impl MapEnv {
    /// Build one from pairs.
    pub fn from(pairs: &[(&str, &str)]) -> Self {
        MapEnv(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }
}

impl Env for MapEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned().filter(|v| !v.is_empty())
    }
}

/// Resolve the data directory for a given platform.
///
/// In order:
/// 1. `MEMFORK_DATA_DIR`, for anyone who wants to decide for themselves;
/// 2. `<working dir>/.memfork`, if it already exists — a project that has one
///    keeps its memory beside its code, and MemFork never creates it
///    implicitly, so this only applies when someone made it on purpose;
/// 3. the platform's per-user data directory.
pub fn resolve(os: Os, env: &impl Env, working_dir: &std::path::Path) -> Option<DataDir> {
    if let Some(explicit) = env.get(DATA_DIR_ENV) {
        return Some(DataDir {
            path: PathBuf::from(explicit),
            source: Source::Environment,
        });
    }

    let project = working_dir.join(DIR_NAME);
    if project.is_dir() {
        return Some(DataDir {
            path: project,
            source: Source::Project,
        });
    }

    per_user(os, env).map(|path| DataDir {
        path,
        source: Source::PerUser,
    })
}

/// The platform's per-user data directory, with `memfork` under it.
///
/// Windows: `%LOCALAPPDATA%`, which is per-user and roams nowhere — the right
/// place for a cache-like store that should not follow a user between
/// machines. macOS: `~/Library/Application Support`, as Apple specifies.
/// Everywhere else: `$XDG_DATA_HOME`, or the `~/.local/share` the XDG
/// specification names as its default.
pub fn per_user(os: Os, env: &impl Env) -> Option<PathBuf> {
    let home = || env.get("HOME").or_else(|| env.get("USERPROFILE"));
    let base = match os {
        Os::Windows => env
            .get("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| home().map(|h| PathBuf::from(h).join("AppData").join("Local")))?,
        Os::MacOs => PathBuf::from(home()?)
            .join("Library")
            .join("Application Support"),
        Os::Linux => match env.get("XDG_DATA_HOME") {
            Some(xdg) => PathBuf::from(xdg),
            None => PathBuf::from(home()?).join(".local").join("share"),
        },
    };
    Some(base.join(APP_NAME))
}

/// Resolve the data directory on this machine.
///
/// Refuses the per-user directory when [`FORBID_PER_USER_ENV`] is set, or when
/// this is a test build of the library. Explicit choices — `MEMFORK_DATA_DIR`,
/// or a project's own `.memfork` — are always honoured, so a test that says
/// where its data goes is unaffected.
pub fn here() -> Result<DataDir, NoDataDir> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let resolved = resolve(Os::current(), &RealEnv, &cwd).ok_or(NoDataDir::Unknown)?;
    if resolved.source == Source::PerUser && per_user_is_forbidden() {
        return Err(NoDataDir::Forbidden {
            path: resolved.path,
        });
    }
    Ok(resolved)
}

/// Whether this process may fall back to the real per-user directory.
fn per_user_is_forbidden() -> bool {
    cfg!(test) || std::env::var_os(FORBID_PER_USER_ENV).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nowhere() -> PathBuf {
        // A working directory with no `.memfork` in it.
        PathBuf::from(if cfg!(windows) {
            r"C:\no\such\place"
        } else {
            "/no/such/place"
        })
    }

    #[test]
    fn the_environment_wins() {
        let env = MapEnv::from(&[(DATA_DIR_ENV, "/somewhere/else"), ("HOME", "/home/ada")]);
        for os in Os::all() {
            let got = resolve(os, &env, &nowhere()).expect("resolved");
            assert_eq!(got.source, Source::Environment);
            assert_eq!(got.path, PathBuf::from("/somewhere/else"));
        }
    }

    #[test]
    fn an_empty_variable_is_the_same_as_an_unset_one() {
        // A shell that exports an empty value should not send data to "".
        let env = MapEnv::from(&[(DATA_DIR_ENV, ""), ("HOME", "/home/ada")]);
        let got = resolve(Os::Linux, &env, &nowhere()).expect("resolved");
        assert_eq!(got.source, Source::PerUser);
    }

    #[test]
    fn a_project_directory_is_used_when_it_already_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = MapEnv::from(&[("HOME", "/home/ada")]);

        // Not there: fall through to the per-user directory.
        assert_eq!(
            resolve(Os::Linux, &env, dir.path())
                .expect("resolved")
                .source,
            Source::PerUser
        );

        // Created on purpose: use it.
        std::fs::create_dir(dir.path().join(DIR_NAME)).expect("created");
        let got = resolve(Os::Linux, &env, dir.path()).expect("resolved");
        assert_eq!(got.source, Source::Project);
        assert_eq!(got.path, dir.path().join(DIR_NAME));
    }

    #[test]
    fn a_file_named_like_the_project_directory_is_not_mistaken_for_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(DIR_NAME), "not a directory").expect("written");
        let env = MapEnv::from(&[("HOME", "/home/ada")]);
        assert_eq!(
            resolve(Os::Linux, &env, dir.path())
                .expect("resolved")
                .source,
            Source::PerUser
        );
    }

    #[test]
    fn each_platform_uses_its_own_convention() {
        let windows = MapEnv::from(&[("LOCALAPPDATA", r"C:\Users\ada\AppData\Local")]);
        assert_eq!(
            per_user(Os::Windows, &windows).expect("resolved"),
            PathBuf::from(r"C:\Users\ada\AppData\Local").join("memfork")
        );

        let mac = MapEnv::from(&[("HOME", "/Users/ada")]);
        assert_eq!(
            per_user(Os::MacOs, &mac).expect("resolved"),
            PathBuf::from("/Users/ada/Library/Application Support/memfork")
        );

        let xdg = MapEnv::from(&[("HOME", "/home/ada"), ("XDG_DATA_HOME", "/home/ada/.data")]);
        assert_eq!(
            per_user(Os::Linux, &xdg).expect("resolved"),
            PathBuf::from("/home/ada/.data/memfork")
        );

        let plain = MapEnv::from(&[("HOME", "/home/ada")]);
        assert_eq!(
            per_user(Os::Linux, &plain).expect("resolved"),
            PathBuf::from("/home/ada/.local/share/memfork")
        );
    }

    #[test]
    fn windows_falls_back_to_the_profile_when_localappdata_is_missing() {
        let env = MapEnv::from(&[("USERPROFILE", r"C:\Users\ada")]);
        assert_eq!(
            per_user(Os::Windows, &env).expect("resolved"),
            PathBuf::from(r"C:\Users\ada")
                .join("AppData")
                .join("Local")
                .join("memfork")
        );
    }

    #[test]
    fn a_test_build_refuses_the_real_per_user_directory() {
        // The guard, checked from inside a test build, where `cfg!(test)`
        // makes it active without touching the environment.
        match here() {
            Err(NoDataDir::Forbidden { path }) => {
                assert!(path.ends_with("memfork"), "{path:?}");
            }
            Err(NoDataDir::Unknown) => {}
            Ok(resolved) => assert_ne!(
                resolved.source,
                Source::PerUser,
                "a test build resolved the real per-user data directory"
            ),
        }
    }

    #[test]
    fn an_explicit_choice_is_still_honoured_under_the_guard() {
        // The guard must not get in the way of a test that does say where its
        // data goes, or tests would have to work around it.
        let env = MapEnv::from(&[(DATA_DIR_ENV, "/tmp/somewhere"), ("HOME", "/home/ada")]);
        let got = resolve(Os::Linux, &env, &nowhere()).expect("resolved");
        assert_eq!(got.source, Source::Environment);
    }

    #[test]
    fn with_nothing_at_all_there_is_no_answer_rather_than_a_guess() {
        let env = MapEnv::default();
        for os in Os::all() {
            assert!(
                per_user(os, &env).is_none(),
                "{os:?} invented a data directory out of nothing"
            );
        }
    }
}
