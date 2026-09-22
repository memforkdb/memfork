//! `memfork doctor` — what this install is, and what it is talking to.
//!
//! The report answers the questions someone asks when the tools are not
//! showing up: which binary is running, how each client was asked, and whether
//! it says MemFork is registered. It also states plainly that nothing is
//! persisted yet, because "my memory is empty again" is the other question
//! this command exists to answer.
//!
//! Registration comes from [`clients::registration`], which asks a client's
//! own command whenever it has one rather than reading its config file. A
//! client owns its configuration; reading around it is how this command used
//! to report a registered server as missing.

use serde_json::{json, Value as Json};

use crate::clients::{self, Checked, Registration, Scope};
use crate::init;
use crate::launch;
use crate::tools;

/// One client's entry in the report.
#[derive(Debug)]
struct ClientReport {
    id: String,
    display: String,
    docs: String,
    verified: String,
    /// The client's own command, and whether it is on `PATH`.
    cli: Option<(String, bool)>,
    /// Whether MemFork is registered.
    registration: Registration,
    /// How that was established.
    checked: Checked,
    /// Whether the client appears to be installed at all.
    installed: bool,
    /// The user-scope config path, when the registry carries one. `None` for
    /// a client whose user configuration MemFork deliberately knows nothing
    /// about.
    user_path: Option<String>,
    /// The project-scope config path, which MemFork always knows.
    project_path: Option<String>,
    /// What about this registry entry could not be confirmed against the
    /// client's documentation, when something could not.
    unverified: Option<String>,
}

impl ClientReport {
    /// Whether this client is installed and `memfork init` would help it —
    /// the only case where suggesting it is useful.
    ///
    /// A registration pointing at another MemFork counts. It is the case most
    /// worth catching: everything looks configured, and no tools appear.
    fn needs_init(&self) -> bool {
        self.installed
            && matches!(
                self.registration,
                Registration::No | Registration::Stale { .. }
            )
    }
}

/// What durability means here, said once, in one place.
pub const DURABILITY_NOTE: &str = "\
Memory is written to the data directory below and survives restarts. Several \
clients can share it: the first one that needs the store starts a daemon that \
owns it, and the rest connect to that. The daemon exits once nothing has \
needed it for a while, and `memfork stop` ends it now. A client run with \
--ephemeral keeps nothing and shares nothing.";

/// Build the report.
fn gather(home: Option<&str>) -> Vec<ClientReport> {
    let launch = launch::resolve();
    clients::all()
        .into_iter()
        .map(|c| {
            let cli = c
                .cli
                .as_ref()
                .map(|cli| (cli.binary.clone(), init::which(&cli.binary).is_some()));
            let path_for = |scope| {
                home.and_then(|h| c.file.as_ref().and_then(|f| f.path_here(scope, h)))
                    .map(|p| p.display().to_string())
            };
            let user_path = path_for(Scope::User);
            let project_path = path_for(Scope::Project);

            let (registration, checked) = match home {
                Some(h) => clients::registration(&c, Scope::User, h, &launch),
                None => (
                    Registration::Unknown("no home directory".to_owned()),
                    Checked::Nothing,
                ),
            };

            ClientReport {
                installed: home.is_some_and(|h| init::is_installed(&c, h)),
                id: c.id.clone(),
                display: c.display.clone(),
                docs: c.docs.clone(),
                verified: c.verified.clone(),
                cli,
                registration,
                checked,
                user_path,
                project_path,
                unverified: c.unverified.clone(),
            }
        })
        .collect()
}

/// Where data is kept, and whether anything has it open.
#[derive(Debug)]
struct Storage {
    path: Option<std::path::PathBuf>,
    source: Option<crate::persist::Source>,
    /// Why there is no path, when there is none.
    why: Option<String>,
    owner: Option<crate::persist::Endpoint>,
}

impl Storage {
    fn describe_dir(&self) -> String {
        match (&self.path, self.source) {
            (Some(path), Some(source)) => {
                let why = match source {
                    crate::persist::Source::Policy => "pinned by the machine policy",
                    crate::persist::Source::Environment => "from MEMFORK_DATA_DIR",
                    crate::persist::Source::Project => "this project's own",
                    crate::persist::Source::PerUser => "per-user",
                };
                format!("{} ({why})", crate::style::path(path))
            }
            _ => match &self.why {
                Some(why) => format!("(none: {})", why.lines().next().unwrap_or(why)),
                None => "(cannot be determined)".to_owned(),
            },
        }
    }

    fn describe_lock(&self) -> String {
        match &self.owner {
            Some(owner) => format!("held by process {}", owner.pid),
            None => "free".to_owned(),
        }
    }

    /// What is serving this data directory, if anything.
    fn describe_daemon(&self) -> String {
        match &self.owner {
            None => "not running (one starts when a client needs it)".to_owned(),
            Some(owner) => match owner.port {
                None => format!(
                    "not running; process {} holds the directory without serving",
                    owner.pid
                ),
                Some(port) => {
                    let version = owner.memfork_version.as_deref().unwrap_or("unknown");
                    let mine = if version == crate::VERSION {
                        ""
                    } else {
                        "  <- a different version from this one; run `memfork stop`"
                    };
                    format!(
                        "127.0.0.1:{port}, process {}, version {version}{mine}",
                        owner.pid
                    )
                }
            },
        }
    }

    /// Where the Brain is, or how to get it.
    fn describe_brain(&self) -> String {
        if !cfg!(feature = "brain") {
            return "not in this build".to_owned();
        }
        if !crate::policy::allows(crate::policy::Feature::Brain) {
            return "switched off by the machine policy".to_owned();
        }
        match self.owner.as_ref().and_then(|o| o.port) {
            Some(port) => format!(
                "http://127.0.0.1:{port}/brain (`memfork brain` opens it with the read token)"
            ),
            None => "`memfork brain` starts the daemon and opens it".to_owned(),
        }
    }

    fn source_name(&self) -> Option<&'static str> {
        self.source.map(|s| match s {
            crate::persist::Source::Policy => "policy",
            crate::persist::Source::Environment => "environment",
            crate::persist::Source::Project => "project",
            crate::persist::Source::PerUser => "per-user",
        })
    }
}

fn storage() -> Storage {
    match crate::persist::datadir::here() {
        Ok(dir) => {
            let owner = crate::persist::lock::owner(&dir.path);
            Storage {
                path: Some(dir.path),
                source: Some(dir.source),
                why: None,
                owner,
            }
        }
        Err(why) => Storage {
            path: None,
            source: None,
            why: Some(why.to_string()),
            owner: None,
        },
    }
}

fn describe(checked: &Checked) -> String {
    match checked {
        Checked::Command(cmd) => format!("asked `{cmd}`"),
        Checked::File(path) => format!("read {}", path.display()),
        Checked::Nothing => "not checked".to_owned(),
    }
}

/// The status word for a client, the same in both reports.
fn status_word(r: &ClientReport) -> &'static str {
    match &r.registration {
        Registration::Yes => "registered",
        // Not a detail: a client in this state shows no MemFork tools at
        // all, and calling it "registered" is how somebody spends an hour
        // wondering why.
        Registration::Stale { .. } => "registered, but not to this MemFork",
        Registration::No if r.installed => "not registered",
        Registration::No => "not installed",
        // A client that is not here at all is simply not installed; why
        // MemFork could not ask it is beside the point.
        Registration::Unknown(_) if !r.installed => "not installed",
        Registration::Unknown(_) => "unknown",
    }
}

/// Autopilot for the repository this command runs in, if it runs in one.
/// Asks the daemon when one is running; never starts one.
fn autopilot() -> Option<crate::autopilot::status::Report> {
    let cwd = std::env::current_dir().ok()?;
    let root = crate::namespace::repository_root(&cwd)?;
    Some(crate::autopilot::status::report(&root, None))
}

/// The autopilot section: what is open, kept or orphaned, with the way out
/// for each. Nothing here is ever done by MemFork on its own.
fn autopilot_section(report: &crate::autopilot::status::Report) -> String {
    let mut out = String::from("Autopilot\n");
    out.push_str(&format!("  state         {}\n", report.summary()));
    let Some(daemon) = &report.daemon else {
        out.push_str("  daemon        not running; forks and orphans are listed while one is\n");
        return out;
    };
    for fork in daemon["open"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  open fork     {} before `{}` (rule: {})\n",
            fork["fork"].as_str().unwrap_or("?"),
            fork["action"].as_str().unwrap_or("?"),
            fork["rule"].as_str().unwrap_or("?")
        ));
    }
    let kept: Vec<&str> = daemon["kept_forks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Json::as_str)
        .collect();
    if !kept.is_empty() {
        out.push_str(&format!(
            "  kept forks    {}: `memfork merge <fork>` or `memfork discard <fork> --lesson \"...\"`\n",
            kept.join(", ")
        ));
    }
    let orphans = report.orphans();
    if orphans.is_empty() {
        out.push_str("  orphans       none\n");
    }
    for orphan in orphans {
        out.push_str(&format!(
            "  orphan        {} ({})\n",
            orphan["branch"].as_str().unwrap_or("?"),
            orphan["why"].as_str().unwrap_or("")
        ));
        for command in orphan["merge_then_discard"]
            .as_array()
            .into_iter()
            .flatten()
        {
            out.push_str(&format!(
                "                  {}\n",
                command.as_str().unwrap_or("")
            ));
        }
        out.push_str(&format!(
            "                or {}\n",
            orphan["discard"].as_str().unwrap_or("")
        ));
    }
    out
}

/// The lines every report starts with: what this is, where memory is, and
/// what is serving it.
fn header(
    home: Option<&str>,
    storage: &Storage,
    autopilot: Option<&crate::autopilot::status::Report>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("memfork {}\n", env!("CARGO_PKG_VERSION")));
    // The path on its own. Printing the whole command line here made the
    // binary look as though it were called `memfork.exe mcp`.
    let launch = launch::resolve();
    out.push_str(&format!(
        "  binary        {}\n",
        crate::style::path(&launch.program)
    ));
    // And the command, when the command is more than that path: a wheel
    // installed without a console script is launched as `python -m memfork`,
    // and a client has to be told so. Appending the subcommand is not a
    // difference worth a line of its own.
    if !launch.args.is_empty() {
        out.push_str(&format!(
            "  launches as   {}\n",
            launch.display(clients::SERVER_ARGS)
        ));
    }
    out.push_str(&format!(
        "  home          {}\n",
        home.map_or_else(
            || "(not found)".to_owned(),
            |h| crate::style::path(std::path::Path::new(h))
        )
    ));
    out.push_str(&format!("  data dir      {}\n", storage.describe_dir()));
    out.push_str(&format!("  daemon        {}\n", storage.describe_daemon()));
    out.push_str(&format!("  brain         {}\n", storage.describe_brain()));
    out.push_str(&format!(
        "  autopilot     {}\n",
        autopilot.map_or_else(
            || "not in a repository; autopilot is per repository".to_owned(),
            |r| r.summary()
        )
    ));
    out.push_str(&format!("  policy        {}\n", policy_line()));
    out
}

/// The policy in one line: what is in force, or where a file would go.
fn policy_line() -> String {
    match crate::policy::current() {
        Ok(p) if p.in_force() => p.summary(),
        Ok(p) => format!(
            "none (an administrator may place one at {})",
            p.machine_path
                .as_deref()
                .map_or_else(|| "the machine location".to_owned(), crate::style::path)
        ),
        Err(e) => format!("UNREADABLE: {}", e.why),
    }
}

/// The policy section of the full report.
fn policy_section() -> String {
    let mut out = String::from("Policy\n");
    match crate::policy::current() {
        Ok(p) => {
            out.push_str(&format!(
                "  machine file  {} ({})\n",
                p.machine_path
                    .as_deref()
                    .map_or_else(|| "(no machine location)".to_owned(), crate::style::path),
                if p.machine_present {
                    "present, in force"
                } else {
                    "not present"
                }
            ));
            match &p.extra_path {
                Some(extra) => out.push_str(&format!(
                    "  extra file    {} (from {}; the machine file wins where both speak)\n",
                    crate::style::path(extra),
                    crate::policy::EXTRA_ENV
                )),
                None => out.push_str(&format!(
                    "  extra file    none ({} is not set)\n",
                    crate::policy::EXTRA_ENV
                )),
            }
            for feature in crate::policy::Feature::ALL {
                out.push_str(&format!(
                    "  {:<13} {}\n",
                    feature.key(),
                    if p.allows(feature) { "allowed" } else { "off" }
                ));
            }
            out.push_str(&format!(
                "  data_dir      {}\n",
                p.data_dir()
                    .map_or_else(|| "not pinned".to_owned(), crate::style::path)
            ));
        }
        Err(e) => {
            out.push_str(&format!("  file          {}\n", e.path));
            out.push_str(&format!("  UNREADABLE    {}\n", e.why));
            out.push_str("  Every command but this one stops until an administrator fixes it.\n");
        }
    }
    out
}

/// The closing line: what, if anything, to do.
fn closing(reports: &[ClientReport]) -> String {
    // Only suggest `memfork init` when there is something for it to do.
    // Printing it unconditionally told people to fix what was not broken.
    let pending: Vec<&str> = reports
        .iter()
        .filter(|r| r.needs_init())
        .map(|r| r.display.as_str())
        .collect();
    if !pending.is_empty() {
        format!(
            "Run `memfork init` to register with: {}.\n",
            pending.join(", ")
        )
    } else if reports.iter().any(|r| r.registration == Registration::Yes) {
        "Every detected client has MemFork registered.\n".to_owned()
    } else {
        "No MCP clients were detected here.\n".to_owned()
    }
}

/// The short report: the header, one line per client, and what to do.
pub fn short() -> String {
    let home = clients::home_dir();
    let storage = storage();
    let autopilot = autopilot();
    let mut out = header(home.as_deref(), &storage, autopilot.as_ref());
    out.push('\n');
    // Forks left open and branches git has dropped are worth a person's
    // look even in the short report; the rest of the section is verbose.
    if let Some(report) = &autopilot {
        let daemon_has_something = report.daemon.as_ref().is_some_and(|d| {
            !report.orphans().is_empty()
                || d["open"].as_array().is_some_and(|a| !a.is_empty())
                || d["kept_forks"].as_array().is_some_and(|a| !a.is_empty())
        });
        if daemon_has_something {
            out.push_str(&autopilot_section(report));
            out.push('\n');
        }
    }
    let reports = gather(home.as_deref());
    out.push_str("Clients\n");
    let width = reports.iter().map(|r| r.display.len()).max().unwrap_or(0);
    for r in &reports {
        let mut line = format!("  {:<width$}  {}", r.display, status_word(r));
        if let Registration::Stale { found } = &r.registration {
            line.push_str(&format!(
                " (points at {})",
                found.as_deref().unwrap_or("another MemFork")
            ));
        }
        if r.unverified.is_some() {
            line.push_str("  (registry entry unverified)");
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&closing(&reports));
    out
}

/// The report as text.
pub fn text() -> String {
    let home = clients::home_dir();
    let storage = storage();
    let autopilot = autopilot();
    let mut out = header(home.as_deref(), &storage, autopilot.as_ref());
    out.push_str(&format!(
        "  engine        memfork-core {}\n",
        memfork_core::VERSION
    ));
    out.push_str(&format!("  mcp tools     {}\n", tools::all().len()));
    out.push_str(&format!("  lock          {}\n", storage.describe_lock()));
    out.push('\n');

    out.push_str("Persistence\n");
    for line in wrap(DURABILITY_NOTE, 74) {
        out.push_str(&format!("  {line}\n"));
    }
    out.push('\n');

    out.push_str(&policy_section());
    out.push('\n');

    if let Some(report) = &autopilot {
        out.push_str(&autopilot_section(report));
        out.push('\n');
    }

    let reports = gather(home.as_deref());
    out.push_str("Clients\n");
    for r in &reports {
        let status = status_word(r);
        out.push_str(&format!("  {} ({})  [{status}]\n", r.display, r.id));

        match &r.cli {
            Some((binary, true)) => {
                out.push_str(&format!("    command     {binary} (found on PATH)\n"));
            }
            Some((binary, false)) => {
                out.push_str(&format!("    command     {binary} (not on PATH)\n"));
            }
            None => out.push_str("    command     (this client ships none)\n"),
        }
        out.push_str(&format!("    checked     {}\n", describe(&r.checked)));
        if let Registration::Unknown(why) = &r.registration {
            if r.installed {
                out.push_str(&format!("    why         {why}\n"));
            }
        }
        if let Registration::Stale { found } = &r.registration {
            out.push_str(&format!(
                "    points at   {}\n",
                found.as_deref().unwrap_or("another MemFork")
            ));
            out.push_str("    fix         run `memfork init` to point it here\n");
        }
        match &r.user_path {
            Some(path) => out.push_str(&format!("    user        {path}\n")),
            None => out.push_str(
                "    user        (kept by the client; MemFork neither reads nor writes it)\n",
            ),
        }
        if let Some(path) = &r.project_path {
            out.push_str(&format!("    project     {path}\n"));
        }
        out.push_str(&format!("    verified    {} — {}\n", r.verified, r.docs));
        if let Some(why) = &r.unverified {
            out.push_str(&format!("    unverified  {why}\n"));
        }
    }

    out.push('\n');
    out.push_str(&closing(&reports));
    out
}

/// The report as JSON.
pub fn json() -> Json {
    let home = clients::home_dir();
    let reports = gather(home.as_deref());
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "engine": memfork_core::VERSION,
        "binary": launch::resolve().program.display().to_string(),
        "launch_command": init::command_line(),
        "home": home,
        "mcp_tools": tools::all().len(),
        "data_dir": storage().path.as_ref().map(|p| p.display().to_string()),
        "data_dir_source": storage().source_name(),
        "lock": storage().owner.as_ref().map(|o| json!({ "pid": o.pid, "port": o.port })),
        "daemon": storage().owner.as_ref().and_then(|o| o.port.map(|port| json!({
            "port": port,
            "pid": o.pid,
            "version": o.memfork_version,
            "version_matches": o.memfork_version.as_deref() == Some(crate::VERSION),
        }))),
        "brain": {
            "built": cfg!(feature = "brain"),
            "allowed": crate::policy::allows(crate::policy::Feature::Brain),
            "url": storage().owner.as_ref().and_then(|o| o.port.map(|port| format!("http://127.0.0.1:{port}/brain"))),
        },
        "autopilot": autopilot().map(|r| r.to_json()),
        "persistence": {
            "enabled": true,
            "note": DURABILITY_NOTE,
        },
        "policy": match crate::policy::current() {
            Ok(p) => p.to_json(),
            Err(e) => json!({ "error": e.to_string(), "file": e.path }),
        },
        "needs_init": reports.iter().any(ClientReport::needs_init),
        "clients": reports.iter().map(|r| json!({
            "id": r.id,
            "display": r.display,
            "command": r.cli.as_ref().map(|(b, _)| b.clone()),
            "command_on_path": r.cli.as_ref().map(|(_, found)| *found),
            "installed": r.installed,
            "registered": r.registration.as_bool(),
            "points_at": match r.registration.points_at() {
                Some(path) => json!(path),
                None => Json::Null,
            },
            "unknown_because": match &r.registration {
                Registration::Unknown(why) => json!(why),
                _ => Json::Null,
            },
            "checked": match &r.checked {
                Checked::Command(cmd) => json!({ "how": "command", "detail": cmd }),
                Checked::File(path) => {
                    json!({ "how": "file", "detail": path.display().to_string() })
                }
                Checked::Nothing => json!({ "how": "nothing", "detail": Json::Null }),
            },
            "user_config": r.user_path,
            "project_config": r.project_path,
            "verified": r.verified,
            "unverified": r.unverified,
            "docs": r.docs,
        })).collect::<Vec<_>>(),
    })
}

/// Wrap text to a width, for the fixed-width report.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.len() + 1 + word.len() > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_says_where_memory_is_kept() {
        // What someone asks this command when their memory seems wrong: where
        // is it, and has something else got hold of it.
        let text = text();
        assert!(text.contains("survives restarts"), "{text}");
        // This paragraph described the earlier one-process-at-a-time behaviour
        // for a whole phase after the daemon replaced it.
        assert!(text.contains("Several clients can share it"), "{text}");
        assert!(
            !text.contains("is refused, and told which process"),
            "the persistence note still describes the behaviour the daemon replaced"
        );
        // The binary is a path. It briefly carried the whole command line,
        // subcommand and all, which read as though the program were named
        // `memfork.exe mcp`.
        let binary_line = text
            .lines()
            .find(|l| l.trim_start().starts_with("binary "))
            .unwrap_or_default();
        assert!(
            !binary_line.trim_end().ends_with(" mcp"),
            "the binary line is a command line, not a path: {binary_line}"
        );
        assert!(
            !text.contains("launches as"),
            "this build is its own binary, so there is nothing extra to say: {text}"
        );

        assert!(text.contains("data dir      "), "{text}");
        assert!(text.contains("lock          "), "{text}");
        assert!(text.contains("daemon        "), "{text}");

        let json = json();
        assert_eq!(json["persistence"]["enabled"], true);
        assert!(json["persistence"]["note"].is_string());
    }

    #[test]
    fn the_report_covers_every_client_in_the_registry() {
        let text = text();
        for client in clients::all() {
            assert!(text.contains(&client.display), "{} is missing", client.id);
            assert!(
                text.contains(&client.docs),
                "{} has no docs link",
                client.id
            );
        }
        assert_eq!(
            json()["clients"].as_array().map(Vec::len),
            Some(clients::all().len())
        );
    }

    #[test]
    fn the_report_states_the_version_and_tool_count() {
        let text = text();
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
        assert!(text.contains(&tools::all().len().to_string()));
    }

    #[test]
    fn the_report_says_how_it_asked() {
        // Whether registration was read off a command or a file is part of the
        // answer: it is what tells someone where to look next.
        let text = text();
        assert!(text.contains("    checked     "), "{text}");
    }

    #[test]
    fn wrapping_never_loses_or_splits_a_word() {
        let words: Vec<&str> = DURABILITY_NOTE.split_whitespace().collect();
        let wrapped = wrap(DURABILITY_NOTE, 40);
        let rejoined: Vec<&str> = wrapped.iter().flat_map(|l| l.split_whitespace()).collect();
        assert_eq!(words, rejoined);
        assert!(wrapped.iter().all(|l| l.len() <= 40 || !l.contains(' ')));
    }
}
