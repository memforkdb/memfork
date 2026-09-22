//! The machine-wide policy an administrator can place (DESIGN §5.4).
//!
//! One file, in a location only an administrator can write, switches features
//! off for every user of the machine and can pin where memory is kept. Nothing
//! a user sets — a flag, an environment variable, a project's own `.memfork`
//! directory, `memfork maintain on` — overrides it. `memfork doctor` shows the
//! policy in force and where it came from.
//!
//! The file is TOML, and every key is optional:
//!
//! ```toml
//! # Features. Each defaults to allowed; `false` switches it off machine-wide.
//! dashboard = false
//! race = false
//! autopilot = false
//! maintenance_tasks = false
//! sampling = false
//! # Whether a write that looks like a credential may be forced through with
//! # `allow_secret` (`--allow-secret`). Defaults to allowed.
//! secret_overrides = false
//! # Where memory is kept, for every user and every project. Unset, each
//! # user's own data directory is used as usual.
//! data_dir = "/srv/memfork"
//! ```
//!
//! Where it lives, per operating system:
//!
//! | OS | Path |
//! |---|---|
//! | Windows | `%ProgramData%\memfork\policy.toml` |
//! | macOS | `/Library/Application Support/memfork/policy.toml` |
//! | Linux | `/etc/memfork/policy.toml` |
//!
//! A second file may be named in `MEMFORK_POLICY_FILE`. It exists so the
//! tests, and a person trying a policy out, can apply one without writing to
//! a system directory; it cannot weaken the machine file, because wherever
//! both set a key the machine file wins. A user can therefore only ever add
//! restrictions through it, never remove one.
//!
//! A policy file that cannot be read — malformed, or naming a key this
//! version does not know — stops every command except `memfork doctor`, which
//! reports it. A typo in a machine-wide rule should be found by the
//! administrator who made it, not enforced as nothing.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::{json, Value as Json};

use crate::clients::Os;
use crate::persist::datadir::{Env, RealEnv};

/// The file's name, in whichever directory the OS puts it.
pub const FILE_NAME: &str = "policy.toml";

/// The directory under the system-wide location.
const DIR_NAME: &str = "memfork";

/// Environment variable naming a second policy file, read beneath the
/// machine one.
pub const EXTRA_ENV: &str = "MEMFORK_POLICY_FILE";

/// A feature the policy can switch off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    /// `memfork ui`, the local dashboard.
    Dashboard,
    /// `memfork race`, which runs agents unattended.
    Race,
    /// Autopilot: memory following the git branch, and automatic forks.
    Autopilot,
    /// Maintenance tasks MemFork adds to a project's board.
    MaintenanceTasks,
    /// Asking a client's model for a summary through MCP sampling.
    Sampling,
    /// `allow_secret`, which writes something the secret detector refused.
    SecretOverrides,
}

impl Feature {
    /// Every feature, in the order the file and the report list them.
    pub const ALL: [Feature; 6] = [
        Feature::Dashboard,
        Feature::Race,
        Feature::Autopilot,
        Feature::MaintenanceTasks,
        Feature::Sampling,
        Feature::SecretOverrides,
    ];

    /// The key in the file.
    pub fn key(self) -> &'static str {
        match self {
            Feature::Dashboard => "dashboard",
            Feature::Race => "race",
            Feature::Autopilot => "autopilot",
            Feature::MaintenanceTasks => "maintenance_tasks",
            Feature::Sampling => "sampling",
            Feature::SecretOverrides => "secret_overrides",
        }
    }

    /// The words for it in a sentence.
    pub fn describe(self) -> &'static str {
        match self {
            Feature::Dashboard => "the dashboard",
            Feature::Race => "race",
            Feature::Autopilot => "autopilot",
            Feature::MaintenanceTasks => "maintenance tasks",
            Feature::Sampling => "sampling",
            Feature::SecretOverrides => "secret overrides",
        }
    }
}

/// The file as written. Every key optional; an unknown key is an error.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct File {
    dashboard: Option<bool>,
    race: Option<bool>,
    autopilot: Option<bool>,
    maintenance_tasks: Option<bool>,
    sampling: Option<bool>,
    secret_overrides: Option<bool>,
    data_dir: Option<String>,
}

impl File {
    fn get(&self, feature: Feature) -> Option<bool> {
        match feature {
            Feature::Dashboard => self.dashboard,
            Feature::Race => self.race,
            Feature::Autopilot => self.autopilot,
            Feature::MaintenanceTasks => self.maintenance_tasks,
            Feature::Sampling => self.sampling,
            Feature::SecretOverrides => self.secret_overrides,
        }
    }
}

/// Where a setting came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The machine file.
    Machine,
    /// The file `MEMFORK_POLICY_FILE` names.
    Extra,
}

/// The policy in force.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Where the machine file is, or would be, on this OS. `None` when the
    /// location cannot be worked out, which happens only on Windows with no
    /// `ProgramData` at all.
    pub machine_path: Option<PathBuf>,
    /// Whether the machine file is present and was read.
    pub machine_present: bool,
    /// The extra file, if `MEMFORK_POLICY_FILE` named one that was read.
    pub extra_path: Option<PathBuf>,
    /// Each feature switched off, and by which file.
    forbidden: Vec<(Feature, Origin)>,
    /// The pinned data directory, and by which file.
    data_dir: Option<(PathBuf, Origin)>,
}

impl Policy {
    /// No file anywhere: everything allowed.
    pub fn permissive() -> Self {
        Policy {
            machine_path: None,
            machine_present: false,
            extra_path: None,
            forbidden: Vec::new(),
            data_dir: None,
        }
    }

    /// Whether a feature may be used.
    pub fn allows(&self, feature: Feature) -> bool {
        !self.forbidden.iter().any(|(f, _)| *f == feature)
    }

    /// Which file switched a feature off, if one did.
    pub fn forbidden_by(&self, feature: Feature) -> Option<Origin> {
        self.forbidden
            .iter()
            .find(|(f, _)| *f == feature)
            .map(|(_, o)| *o)
    }

    /// The data directory the policy pins, if it pins one.
    pub fn data_dir(&self) -> Option<&Path> {
        self.data_dir.as_ref().map(|(p, _)| p.as_path())
    }

    /// Whether any file was read at all.
    pub fn in_force(&self) -> bool {
        self.machine_present || self.extra_path.is_some()
    }

    /// The path of the file an origin names, for messages.
    pub fn path_of(&self, origin: Origin) -> String {
        let path = match origin {
            Origin::Machine => self.machine_path.as_deref(),
            Origin::Extra => self.extra_path.as_deref(),
        };
        path.map_or_else(|| "the policy".to_owned(), crate::style::path)
    }

    /// What to tell someone whose request a feature switch-off refused:
    /// what happened, and that only an administrator can change it.
    pub fn refusal(&self, feature: Feature) -> String {
        let origin = self.forbidden_by(feature).unwrap_or(Origin::Machine);
        format!(
            "{} {} switched off by the machine policy at {}. Nothing on the command \
             line, in the environment or in a project can turn it back on; an \
             administrator would change that file.",
            first_upper(feature.describe()),
            if feature.describe().ends_with('s') {
                "are"
            } else {
                "is"
            },
            self.path_of(origin)
        )
    }

    /// One line for `memfork doctor`: what is in force.
    pub fn summary(&self) -> String {
        if !self.in_force() {
            return "none".to_owned();
        }
        let mut parts: Vec<String> = self
            .forbidden
            .iter()
            .map(|(f, _)| format!("{} off", f.describe()))
            .collect();
        if let Some((dir, _)) = &self.data_dir {
            parts.push(format!(
                "data directory pinned to {}",
                crate::style::path(dir)
            ));
        }
        if parts.is_empty() {
            "a policy file is present and switches nothing off".to_owned()
        } else {
            parts.join(", ")
        }
    }

    /// The policy as data, for `memfork doctor --json`.
    pub fn to_json(&self) -> Json {
        let mut features = serde_json::Map::new();
        for f in Feature::ALL {
            features.insert(f.key().to_owned(), json!(self.allows(f)));
        }
        json!({
            "machine_file": self.machine_path.as_deref().map(crate::style::path),
            "machine_file_present": self.machine_present,
            "extra_file": self.extra_path.as_deref().map(crate::style::path),
            "in_force": self.in_force(),
            "allows": features,
            "data_dir": self.data_dir.as_ref().map(|(p, _)| crate::style::path(p)),
        })
    }
}

fn first_upper(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Why the policy could not be read. Every command but `doctor` stops on it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "the policy file {path} could not be read: {why}\n\
     MemFork stops rather than run with a machine-wide rule it cannot \
     understand. An administrator would fix that file; `memfork doctor` \
     reports it in the meantime."
)]
pub struct Error {
    /// The file.
    pub path: String,
    /// What was wrong with it.
    pub why: String,
}

/// Where the machine file is on an OS.
///
/// Written against an explicit OS and environment, like the data directory,
/// so all three answers are tested wherever the tests run.
pub fn machine_path(os: Os, env: &impl Env) -> Option<PathBuf> {
    let base = match os {
        Os::Windows => PathBuf::from(
            env.get("ProgramData")
                .or_else(|| env.get("PROGRAMDATA"))
                .or_else(|| env.get("ALLUSERSPROFILE"))?,
        ),
        Os::MacOs => PathBuf::from("/Library/Application Support"),
        Os::Linux => PathBuf::from("/etc"),
    };
    Some(base.join(DIR_NAME).join(FILE_NAME))
}

/// Read one file's text, `None` if it does not exist.
fn read(path: &Path) -> Result<Option<String>, Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error {
            path: crate::style::path(path),
            why: e.to_string(),
        }),
    }
}

fn parse(path: &Path, text: &str) -> Result<File, Error> {
    toml::from_str(text).map_err(|e| Error {
        path: crate::style::path(path),
        why: e.message().to_owned(),
    })
}

/// Combine what the files say. The machine file is applied last, so wherever
/// both speak it wins.
fn combine(
    machine_path: Option<PathBuf>,
    machine: Option<&File>,
    extra: Option<(&Path, &File)>,
) -> Policy {
    let mut policy = Policy {
        machine_path,
        machine_present: machine.is_some(),
        extra_path: extra.map(|(p, _)| p.to_path_buf()),
        forbidden: Vec::new(),
        data_dir: None,
    };
    let layers = [
        extra.map(|(_, f)| (f, Origin::Extra)),
        machine.map(|f| (f, Origin::Machine)),
    ];
    for (file, origin) in layers.into_iter().flatten() {
        for feature in Feature::ALL {
            if let Some(allowed) = file.get(feature) {
                policy.forbidden.retain(|(f, _)| *f != feature);
                if !allowed {
                    policy.forbidden.push((feature, origin));
                }
            }
        }
        if let Some(dir) = &file.data_dir {
            policy.data_dir = Some((PathBuf::from(dir), origin));
        }
    }
    policy
        .forbidden
        .sort_by_key(|(f, _)| Feature::ALL.iter().position(|x| x == f));
    policy
}

/// Read the policy for an OS and an environment. The files themselves are
/// read from disk at the paths worked out.
pub fn load(os: Os, env: &impl Env) -> Result<Policy, Error> {
    let machine_path = machine_path(os, env);
    let machine = match &machine_path {
        Some(p) => read(p)?.map(|text| parse(p, &text)).transpose()?,
        None => None,
    };
    let extra_path = env.get(EXTRA_ENV).map(PathBuf::from);
    let extra = match &extra_path {
        Some(p) => read(p)?.map(|text| parse(p, &text)).transpose()?,
        None => None,
    };
    Ok(combine(
        machine_path,
        machine.as_ref(),
        extra_path.as_deref().zip(extra.as_ref()),
    ))
}

/// The policy this process runs under, read once.
pub fn current() -> Result<&'static Policy, Error> {
    static POLICY: OnceLock<Result<Policy, Error>> = OnceLock::new();
    POLICY
        .get_or_init(|| load(Os::current(), &RealEnv))
        .as_ref()
        .map_err(Clone::clone)
}

/// Whether the policy this process runs under allows a feature. A policy
/// that could not be read allows nothing it could have switched off: the
/// refusal that stops every command says why, and this is the answer for
/// the daemon's own checks in the meantime.
pub fn allows(feature: Feature) -> bool {
    current().is_ok_and(|p| p.allows(feature))
}

/// The refusal for a feature under this process's policy.
pub fn refusal(feature: Feature) -> String {
    match current() {
        Ok(p) => p.refusal(feature),
        Err(e) => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::datadir::MapEnv;

    fn file(text: &str) -> File {
        parse(Path::new("t.toml"), text).expect("parses")
    }

    #[test]
    fn the_machine_file_has_one_place_per_os() {
        let win = MapEnv::from(&[("ProgramData", r"C:\ProgramData")]);
        assert_eq!(
            machine_path(Os::Windows, &win),
            Some(PathBuf::from(r"C:\ProgramData\memfork\policy.toml"))
        );
        assert_eq!(
            machine_path(Os::MacOs, &MapEnv::default()),
            Some(PathBuf::from(
                "/Library/Application Support/memfork/policy.toml"
            ))
        );
        assert_eq!(
            machine_path(Os::Linux, &MapEnv::default()),
            Some(PathBuf::from("/etc/memfork/policy.toml"))
        );
        // Windows with no ProgramData at all: no guess.
        assert_eq!(machine_path(Os::Windows, &MapEnv::default()), None);
    }

    #[test]
    fn nothing_written_means_everything_allowed() {
        let p = combine(Some(PathBuf::from("/etc/memfork/policy.toml")), None, None);
        for f in Feature::ALL {
            assert!(p.allows(f), "{f:?}");
        }
        assert!(p.data_dir().is_none());
        assert!(!p.in_force());
        assert_eq!(p.summary(), "none");
    }

    #[test]
    fn the_machine_file_wins_wherever_both_speak() {
        let machine = file("maintenance_tasks = false\nsampling = true\n");
        let extra =
            file("maintenance_tasks = true\nsampling = false\nrace = false\ndata_dir = \"/x\"\n");
        let p = combine(
            Some(PathBuf::from("/etc/memfork/policy.toml")),
            Some(&machine),
            Some((Path::new("/home/ada/policy.toml"), &extra)),
        );
        // Set by both: the machine file's answer.
        assert!(!p.allows(Feature::MaintenanceTasks));
        assert_eq!(
            p.forbidden_by(Feature::MaintenanceTasks),
            Some(Origin::Machine)
        );
        assert!(
            p.allows(Feature::Sampling),
            "the extra file cannot switch off what the machine file allows"
        );
        // Set only by the extra file: it may add a restriction.
        assert!(!p.allows(Feature::Race));
        assert_eq!(p.forbidden_by(Feature::Race), Some(Origin::Extra));
        assert_eq!(p.data_dir(), Some(Path::new("/x")));
        assert!(p.in_force());
    }

    #[test]
    fn a_pinned_data_dir_in_the_machine_file_beats_the_extra_one() {
        let machine = file("data_dir = \"/srv/memfork\"\n");
        let extra = file("data_dir = \"/tmp/mine\"\n");
        let p = combine(
            Some(PathBuf::from("/etc/memfork/policy.toml")),
            Some(&machine),
            Some((Path::new("/e.toml"), &extra)),
        );
        assert_eq!(p.data_dir(), Some(Path::new("/srv/memfork")));
    }

    #[test]
    fn an_unknown_key_is_an_error_not_a_silence() {
        let err = parse(Path::new("/etc/memfork/policy.toml"), "dashbord = false\n")
            .expect_err("refused");
        assert!(err.why.contains("dashbord"), "{err}");
        assert!(err.to_string().contains("administrator"), "{err}");
        let err = parse(Path::new("p"), "dashboard = \"no\"\n").expect_err("refused");
        assert!(
            err.why.contains("bool") || err.why.contains("boolean"),
            "{err}"
        );
    }

    #[test]
    fn a_refusal_says_what_and_who_can_change_it() {
        let machine = file("secret_overrides = false\n");
        let p = combine(
            Some(PathBuf::from("/etc/memfork/policy.toml")),
            Some(&machine),
            None,
        );
        let why = p.refusal(Feature::SecretOverrides);
        assert!(
            why.starts_with("Secret overrides are switched off"),
            "{why}"
        );
        assert!(
            why.contains(&crate::style::path(Path::new("/etc/memfork/policy.toml"))),
            "{why}"
        );
        assert!(why.contains("administrator"), "{why}");
        let why = p.refusal(Feature::Dashboard);
        assert!(why.starts_with("The dashboard is"), "{why}");
    }

    #[test]
    fn the_summary_and_json_say_what_is_in_force() {
        let machine = file("race = false\nmaintenance_tasks = false\ndata_dir = \"/srv/m\"\n");
        let p = combine(
            Some(PathBuf::from("/etc/memfork/policy.toml")),
            Some(&machine),
            None,
        );
        let s = p.summary();
        assert!(s.contains("race off"), "{s}");
        assert!(s.contains("maintenance tasks off"), "{s}");
        assert!(s.contains("pinned to"), "{s}");
        let j = p.to_json();
        assert_eq!(j["allows"]["race"], false);
        assert_eq!(j["allows"]["dashboard"], true);
        assert_eq!(j["machine_file_present"], true);
        assert_eq!(j["in_force"], true);
    }

    #[test]
    fn a_file_that_is_present_but_empty_is_in_force_and_forbids_nothing() {
        let p = combine(
            Some(PathBuf::from("/etc/memfork/policy.toml")),
            Some(&file("")),
            None,
        );
        assert!(p.in_force());
        assert!(Feature::ALL.iter().all(|f| p.allows(*f)));
        assert!(p.summary().contains("switches nothing off"));
    }

    #[test]
    fn loading_reads_the_extra_file_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let extra = dir.path().join("policy.toml");
        std::fs::write(&extra, "autopilot = false\n").expect("written");
        // A machine location that does not exist, so only the extra file is
        // read, on every OS.
        let env = MapEnv::from(&[
            (EXTRA_ENV, &extra.display().to_string()),
            (
                "ProgramData",
                &dir.path().join("no-such").display().to_string(),
            ),
        ]);
        for os in Os::all() {
            let p = load(os, &env).expect("loaded");
            assert!(!p.allows(Feature::Autopilot), "{os:?}");
            assert!(!p.machine_present);
            assert_eq!(p.extra_path.as_deref(), Some(extra.as_path()));
        }
        // And a broken one stops the load with its path.
        std::fs::write(&extra, "not = = toml").expect("written");
        let err = load(Os::Linux, &env).expect_err("refused");
        assert!(err.path.contains("policy.toml"), "{err}");
    }
}
