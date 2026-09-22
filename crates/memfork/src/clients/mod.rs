//! The client adapter registry (DESIGN §6.1).
//!
//! One data file describes every MCP client MemFork knows how to register
//! with. Adding a client is an entry in `clients.toml` plus a fixture test; no
//! code here knows any client's name.
//!
//! Two ways in, in order of preference:
//!
//! 1. **The client's own `mcp add` command.** Then the client writes its own
//!    config, in its own format, and MemFork never parses a file it does not
//!    own. This is what a client that ships a CLI wants us to do.
//! 2. **Editing the config file**, for clients that ship no CLI, or whose CLI
//!    cannot express the requested scope. See [`edit`], which changes only the
//!    MemFork entry and keeps a timestamped backup.

pub mod edit;
pub mod hooks;

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::launch::Launch;

/// The registry, compiled into the binary.
const REGISTRY: &str = include_str!("clients.toml");

/// The name MemFork registers itself under, in every client.
pub const SERVER_NAME: &str = "memfork";

/// The arguments the registered command is given.
pub const SERVER_ARGS: &[&str] = &["mcp"];

/// Which configuration a registration goes into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Available in every project, for this user.
    User,
    /// Available in the current directory's project only.
    Project,
}

impl Scope {
    /// Parse a `--scope` value.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "user" => Some(Scope::User),
            "project" => Some(Scope::Project),
            _ => None,
        }
    }

    /// The name this scope parses from.
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
        }
    }
}

/// An operating system, named explicitly so one machine can resolve and test
/// every platform's paths, from whichever one is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    /// Linux and other Unixes.
    Linux,
    /// macOS.
    MacOs,
    /// Windows.
    Windows,
}

impl Os {
    /// The OS this binary is running on.
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Linux
        }
    }

    /// The path separator this OS writes.
    pub fn separator(self) -> char {
        match self {
            Os::Windows => '\\',
            _ => '/',
        }
    }

    /// The key suffix used for per-OS overrides in `clients.toml`.
    fn suffix(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::MacOs => "macos",
            Os::Windows => "windows",
        }
    }

    /// Every OS, for fixture tests.
    pub fn all() -> [Os; 3] {
        [Os::Linux, Os::MacOs, Os::Windows]
    }
}

/// The parsed registry.
#[derive(Debug, Deserialize)]
struct Registry {
    client: Vec<Client>,
}

/// One MCP client.
#[derive(Debug, Clone, Deserialize)]
pub struct Client {
    /// Stable identifier, used by `--client`.
    pub id: String,
    /// Human-readable name.
    pub display: String,
    /// Where this entry's facts came from.
    pub docs: String,
    /// The date `docs` was last checked.
    pub verified: String,
    /// How to register through the client's own command, if it has one.
    pub cli: Option<ClientCli>,
    /// How to register by editing the client's config file.
    pub file: Option<ClientFile>,
    /// Directories the client creates for itself, used to tell "installed
    /// but never configured" apart from "not installed". Any one existing is
    /// enough; a client with a different home on each OS lists them all.
    #[serde(default, alias = "detect_dir")]
    pub detect_dirs: OneOrMany,
    /// The names this client gives in MCP `initialize`, so what it writes can
    /// be shown under `display`. A trailing `*` matches a prefix.
    #[serde(default)]
    pub mcp_names: Vec<String>,
    /// Where this client reads a repository's instructions from.
    #[serde(default)]
    pub instructions: Option<ClientInstructions>,
    /// The client's hook system, for autopilot's automatic forks. Absent
    /// for a client whose hooks are unverified or that has none: autopilot
    /// then never runs anything through it.
    #[serde(default)]
    pub hooks: Option<ClientHooks>,
    /// What could not be confirmed against `docs` on `verified`, in a
    /// sentence: a client name never seen in a shipped build, a path the
    /// documentation does not give for one OS. Shown by `memfork doctor`
    /// and never silently assumed.
    #[serde(default)]
    pub unverified: Option<String>,
}

/// One string or a list of them, so a registry key can grow from one value
/// to several without every entry changing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    /// Nothing.
    #[default]
    None,
    /// One.
    One(String),
    /// Several.
    Many(Vec<String>),
}

impl OneOrMany {
    /// As a slice, however it was written.
    pub fn as_slice(&self) -> &[String] {
        match self {
            OneOrMany::None => &[],
            OneOrMany::One(one) => std::slice::from_ref(one),
            OneOrMany::Many(many) => many,
        }
    }
}

/// The files a client reads project instructions from.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientInstructions {
    /// Paths relative to the repository root, all of which this client reads
    /// unconditionally.
    pub reads: Vec<String>,
    /// Where these facts came from.
    pub docs: String,
    /// The date `docs` was last checked.
    pub verified: String,
}

/// A client's hook system, as far as autopilot uses it.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientHooks {
    /// The shape its settings take; [`hooks`] writes it.
    pub format: hooks::Shape,
    /// The per-project settings file that is the person's own and not
    /// committed, relative to the repository root with `/` separators.
    pub project_local: String,
    /// Where these facts came from.
    pub docs: String,
    /// The date `docs` was last checked.
    pub verified: String,
}

/// The client's own `mcp add` command.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientCli {
    /// Executable to look for on `PATH`.
    pub binary: String,
    /// Argument template for adding a server.
    pub add: Vec<String>,
    /// Argument template for asking whether a server is registered. Absent
    /// for a client whose command can add a server but not say, outside an
    /// interactive wizard, whether one is there; registration is then read
    /// from its file.
    #[serde(default)]
    pub status: Option<Vec<String>>,
    /// Argument template for removing a registration, if the client documents
    /// one. Absent means MemFork will not try: it says what to run instead.
    #[serde(default)]
    pub remove: Option<Vec<String>>,
    /// Flags that select user scope. Empty means the CLI needs none.
    #[serde(default)]
    pub scope_user: Option<Vec<String>>,
    /// Flags that select project scope. Absent means the CLI cannot do it.
    #[serde(default)]
    pub scope_project: Option<Vec<String>>,
}

impl ClientCli {
    /// The flags for a scope, or `None` if this CLI cannot express it.
    pub fn scope_flags(&self, scope: Scope) -> Option<&[String]> {
        match scope {
            Scope::User => self.scope_user.as_deref(),
            Scope::Project => self.scope_project.as_deref(),
        }
    }

    /// Expand the `status` template into a concrete argument list, if this
    /// client has one.
    pub fn status_args(&self) -> Option<Vec<String>> {
        // No `{command}` or `{args}` appears in a status template: asking
        // about a server takes its name, not the command behind it.
        self.status
            .as_ref()
            .map(|status| self.expand(status, &[], &Launch::program("")))
    }

    /// Expand the `add` template into a concrete argument list.
    pub fn add_args(&self, scope: Scope, launch: &Launch) -> Option<Vec<String>> {
        let scope_flags = self.scope_flags(scope)?;
        Some(self.expand(&self.add, scope_flags, launch))
    }

    /// Expand the `remove` template, if this client documents one.
    pub fn remove_args(&self, scope: Scope) -> Option<Vec<String>> {
        let template = self.remove.as_ref()?;
        // A client whose remove takes no scope has no flags to give it; one
        // that does gets the same flags its add would.
        let scope_flags = self.scope_flags(scope).unwrap_or(&[]);
        Some(self.expand(template, scope_flags, &Launch::program("")))
    }

    fn expand(&self, template: &[String], scope_flags: &[String], launch: &Launch) -> Vec<String> {
        let mut out = Vec::new();
        for token in template {
            match token.as_str() {
                "{scope}" => out.extend(scope_flags.iter().cloned()),
                "{name}" => out.push(SERVER_NAME.to_owned()),
                "{command}" => out.push(launch.program.display().to_string()),
                // Whatever has to come before the subcommand comes first: a
                // wheel install is launched as `python -m memfork mcp`, not
                // as `mcp` on its own.
                "{args}" => {
                    out.extend(launch.args.iter().cloned());
                    out.extend(SERVER_ARGS.iter().map(|a| (*a).to_owned()));
                }
                literal => out.push(literal.to_owned()),
            }
        }
        out
    }
}

/// The client's configuration file.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientFile {
    /// `json`, `jsonc` or `toml`.
    pub format: Format,
    /// Key path to the table of servers, e.g. `["mcpServers"]`.
    pub server_map: Vec<String>,
    /// The value of the entry's `type` key for a local server, if this
    /// client wants one: `stdio` for most, `local` for some. Absent means no
    /// `type` key is written.
    #[serde(default)]
    pub stdio_type: Option<String>,
    /// How the command is written: `command` and `args` apart, or one
    /// `command` list with the program first.
    #[serde(default)]
    pub command_style: CommandStyle,
    /// Keys the client requires on every entry besides the command, written
    /// as they stand: `tools = ["*"]` for one that lists allowed tools.
    #[serde(default)]
    pub extra: Option<toml::Table>,
    /// Which key a remote server's URL goes under. Encoded here
    /// because the clients disagree (DESIGN §6.1).
    pub http_url_key: String,
    /// User-scope path template, with `$HOME` and the other variables
    /// [`Vars`] expands.
    ///
    /// Absent when MemFork will not go near this client's user configuration —
    /// because the file holds more than MCP servers, and the client's own
    /// command is the supported way in.
    #[serde(default)]
    pub user: Option<String>,
    /// Project-scope path template, relative to the working directory.
    /// Absent for a client that reads no configuration from a project.
    #[serde(default)]
    pub project: Option<String>,
    /// Per-OS overrides, keyed `user_windows`, `project_macos` and so on.
    #[serde(flatten)]
    overrides: BTreeMap<String, toml::Value>,
}

/// Which syntax a client's config file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// JSON. Edited by splicing one entry into the text, so everything else —
    /// whitespace, key order, and any comments the client tolerates — stays
    /// byte for byte.
    Json,
    /// JSON with comments and trailing commas, as several editors write it.
    /// Edited exactly as `json`; named apart so the registry says which
    /// clients need the tolerance.
    Jsonc,
    /// TOML, edited in place with comments and formatting preserved.
    Toml,
}

/// How a client's entry names the command to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommandStyle {
    /// `"command": "memfork", "args": ["mcp"]`.
    #[default]
    Split,
    /// `"command": ["memfork", "mcp"]`.
    Array,
}

/// The variables a path template may use, resolved for one OS.
///
/// Every one is derived from the home directory when it is not given, and
/// when `MEMFORK_HOME` redirects the home directory they are all derived from
/// it whatever the real environment says. That is what keeps a test, or a
/// `memfork init` run against a stand-in home, from reaching the real
/// `%APPDATA%\\Code\\User` on the machine it runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vars {
    /// `$HOME`.
    pub home: String,
    /// `$APPDATA`: Windows roaming application data.
    pub appdata: String,
    /// `$LOCALAPPDATA`: Windows local application data.
    pub localappdata: String,
    /// `$XDG_CONFIG_HOME`.
    pub xdg_config_home: String,
    /// `$XDG_DATA_HOME`.
    pub xdg_data_home: String,
}

impl Vars {
    /// Every variable derived from the home directory, with the OS's own
    /// defaults.
    pub fn for_home(home: &str, os: Os) -> Self {
        let home = home.trim_end_matches(['/', '\\']).to_owned();
        let join = |parts: &[&str]| {
            let mut s = home.clone();
            for p in parts {
                s.push(os.separator());
                s.push_str(p);
            }
            s
        };
        Vars {
            appdata: join(&["AppData", "Roaming"]),
            localappdata: join(&["AppData", "Local"]),
            xdg_config_home: join(&[".config"]),
            xdg_data_home: join(&[".local", "share"]),
            home,
        }
    }

    /// The variables for this machine: from the real environment, unless
    /// `MEMFORK_HOME` stands in for the home directory, in which case every
    /// one is derived from that and the real environment is not consulted.
    pub fn here(home: &str) -> Self {
        let os = Os::current();
        let mut vars = Vars::for_home(home, os);
        if std::env::var_os("MEMFORK_HOME").is_some() {
            return vars;
        }
        let real = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        if let Some(v) = real("APPDATA") {
            vars.appdata = v;
        }
        if let Some(v) = real("LOCALAPPDATA") {
            vars.localappdata = v;
        }
        if let Some(v) = real("XDG_CONFIG_HOME") {
            vars.xdg_config_home = v;
        }
        if let Some(v) = real("XDG_DATA_HOME") {
            vars.xdg_data_home = v;
        }
        vars
    }

    /// Expand a template, normalising separators to the OS's own.
    pub fn expand(&self, template: &str, os: Os) -> String {
        // Longest names first, so `$XDG_CONFIG_HOME` is not read as `$XDG`
        // followed by text, and `$LOCALAPPDATA` is not read as `$LOCAL`.
        let expanded = template
            .replace("$XDG_CONFIG_HOME", &self.xdg_config_home)
            .replace("$XDG_DATA_HOME", &self.xdg_data_home)
            .replace("$LOCALAPPDATA", &self.localappdata)
            .replace("$APPDATA", &self.appdata)
            .replace("$HOME", &self.home);
        let sep = os.separator();
        expanded
            .split(['/', '\\'])
            .collect::<Vec<_>>()
            .join(&sep.to_string())
    }
}

impl ClientFile {
    /// The path template for a scope on an OS, honouring any per-OS override.
    ///
    /// `None` for a scope this client has no path for, which is how the
    /// registry says "MemFork does not touch this file".
    fn template(&self, scope: Scope, os: Os) -> Option<&str> {
        let key = format!("{}_{}", scope.as_str(), os.suffix());
        if let Some(over) = self.overrides.get(&key).and_then(toml::Value::as_str) {
            return Some(over);
        }
        match scope {
            Scope::User => self.user.as_deref(),
            Scope::Project => self.project.as_deref(),
        }
    }

    /// Resolve the config path for a scope and an OS from a set of variables.
    ///
    /// Returns a string rather than a `PathBuf` so that one machine can render
    /// another platform's paths, which is what makes the three-OS fixtures
    /// testable from anywhere.
    pub fn path_for(&self, scope: Scope, os: Os, vars: &Vars) -> Option<String> {
        let template = self.template(scope, os)?;
        Some(vars.expand(template, os))
    }

    /// Resolve the config path on this machine.
    pub fn path_here(&self, scope: Scope, home: &str) -> Option<PathBuf> {
        self.path_for(scope, Os::current(), &Vars::here(home))
            .map(PathBuf::from)
    }
}

impl Client {
    /// The client's own directories on this machine, as the registry names
    /// them.
    pub fn detect_dirs_here(&self, home: &str) -> Vec<PathBuf> {
        let vars = Vars::here(home);
        self.detect_dirs
            .as_slice()
            .iter()
            .map(|t| PathBuf::from(vars.expand(t, Os::current())))
            .collect()
    }

    /// Whether MemFork may write this client's file for a scope.
    ///
    /// A missing path is the prohibition: the registry simply does not carry
    /// the location, so no code path can reach it.
    pub fn file_writable(&self, scope: Scope) -> bool {
        match (&self.file, scope) {
            (None, _) => false,
            (Some(f), Scope::User) => f.user.is_some(),
            (Some(f), Scope::Project) => f.project.is_some(),
        }
    }

    /// The command that asks this client whether MemFork is registered, if it
    /// has one, it can answer, and it is installed.
    pub fn status_command(&self) -> Option<(String, PathBuf, Vec<String>)> {
        let cli = self.cli.as_ref()?;
        let args = cli.status_args()?;
        let resolved = crate::init::which(&cli.binary)?;
        Some((cli.binary.clone(), resolved, args))
    }
}

/// Whether a client has MemFork registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Registration {
    /// It does, pointing at this MemFork.
    Yes,
    /// It has a MemFork registered, but not this one.
    ///
    /// Which happens the moment anybody installs MemFork somewhere else — and
    /// the installer does exactly that, moving the binary out of wherever it
    /// was built or `cargo install`ed. The client goes on launching a path
    /// that is stale or gone, and the tools quietly stop appearing.
    Stale {
        /// What it points at instead, when the client said.
        found: Option<String>,
    },
    /// It does not.
    No,
    /// It could not be determined, and why.
    Unknown(String),
}

impl Registration {
    /// As an optional boolean, for JSON output.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Registration::Yes => Some(true),
            // Registered at the wrong path is not registered: the tools do
            // not appear, so reporting `true` would be a lie told in JSON.
            Registration::Stale { .. } | Registration::No => Some(false),
            Registration::Unknown(_) => None,
        }
    }

    /// Where a stale registration points, if the client said.
    pub fn points_at(&self) -> Option<&str> {
        match self {
            Registration::Stale { found } => found.as_deref(),
            _ => None,
        }
    }
}

/// How a registration was established, for the report.
#[derive(Debug, Clone)]
pub enum Checked {
    /// By running the client's own command.
    Command(String),
    /// By reading a config file MemFork owns the format of.
    File(PathBuf),
    /// Not checked at all.
    Nothing,
}

/// Ask a client whether MemFork is registered.
///
/// A client that ships a command is asked with it, always, in preference to
/// reading a file. The client owns its configuration: it may keep servers
/// somewhere the registry does not model, or rewrite what it was given, and a
/// file MemFork read would then disagree with the client itself. That
/// disagreement is exactly what made `memfork doctor` report a registered
/// server as missing.
pub fn registration(
    client: &Client,
    scope: Scope,
    home: &str,
    launch: &Launch,
) -> (Registration, Checked) {
    if let Some((binary, resolved, args)) = client.status_command() {
        let printed = format!("{binary} {}", args.join(" "));
        // Asking a client can take seconds: some start the server to check it.
        let spinner = crate::style::Spinner::quiet(&format!(
            "asking {} whether MemFork is registered",
            client.display
        ));
        let output = std::process::Command::new(&resolved).args(&args).output();
        drop(spinner);
        return match output {
            Ok(output) => {
                let text = format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                // A `get` says yes by exiting zero; a `list` exits zero either
                // way, so the name has to appear as well.
                let registered = output.status.success() && text.contains(SERVER_NAME);
                let answer = match registered {
                    false => Registration::No,
                    // Registered — but at which MemFork? Every one of these
                    // clients prints the command it would run, so the honest
                    // check is whether that command is this one.
                    true if launch.is_named_in(&text) => Registration::Yes,
                    true => Registration::Stale {
                        found: memfork_path_in(&text),
                    },
                };
                (answer, Checked::Command(printed))
            }
            Err(e) => (
                Registration::Unknown(format!("`{printed}` could not be run: {e}")),
                Checked::Command(printed),
            ),
        };
    }

    let Some(file) = client.file.as_ref() else {
        return (
            Registration::Unknown("this client has no command and no config file".to_owned()),
            Checked::Nothing,
        );
    };
    let Some(path) = file.path_here(scope, home) else {
        return (
            Registration::Unknown(
                "MemFork does not read this client's user configuration; install its \
                 command to have it answer for itself"
                    .to_owned(),
            ),
            Checked::Nothing,
        );
    };
    if !path.exists() {
        return (Registration::No, Checked::File(path));
    }
    match edit::plan(file, &path, launch) {
        Ok(e) if e.change == edit::Change::Unchanged => (Registration::Yes, Checked::File(path)),
        // An entry that exists and differs is the stale case, and the file
        // says exactly what it points at.
        Ok(e) if e.change == edit::Change::Update => (
            Registration::Stale {
                found: edit::registered_command(file, &path),
            },
            Checked::File(path),
        ),
        Ok(_) => (Registration::No, Checked::File(path)),
        Err(reason) => (Registration::Unknown(reason), Checked::File(path)),
    }
}

/// Pick the MemFork command out of whatever a client printed.
///
/// Every client in the registry prints a line naming the command; none of them
/// promise a format. So this looks for the one thing that cannot be mistaken —
/// a token that names a path and mentions MemFork — and returns nothing rather
/// than a guess.
fn memfork_path_in(text: &str) -> Option<String> {
    let trim: &[char] = &['"', ',', '\''];
    text.split_whitespace()
        .map(|token| token.trim_matches(trim))
        .find(|token| {
            token.to_ascii_lowercase().contains(SERVER_NAME)
                && token.chars().any(|c| c == '/' || c == '\\')
        })
        .map(str::to_owned)
}

/// Every client in the registry, in file order.
///
/// The registry is compiled in and covered by tests, so a parse failure here
/// is a build-time mistake rather than a user-visible one; it surfaces as an
/// empty registry with the reason available from [`load`].
pub fn all() -> Vec<Client> {
    cached().to_vec()
}

/// The registry, parsed once per process. It is compiled in, so parsing it
/// again for every writer's name would cost a hundred thousand parses on
/// a large store for the same answer.
fn cached() -> &'static [Client] {
    static PARSED: std::sync::OnceLock<Vec<Client>> = std::sync::OnceLock::new();
    PARSED.get_or_init(|| load().unwrap_or_default())
}

/// Parse the registry, reporting why if it cannot be read.
pub fn load() -> Result<Vec<Client>, String> {
    toml::from_str::<Registry>(REGISTRY)
        .map(|r| r.client)
        .map_err(|e| format!("the compiled-in client registry is malformed: {e}"))
}

/// The display name for a writer recorded from MCP `initialize`, if the
/// registry knows that client; otherwise the name as it was given.
pub fn display_for_writer(name: &str) -> String {
    cached()
        .iter()
        .find(|c| {
            c.mcp_names
                .iter()
                .any(|pattern| match pattern.strip_suffix('*') {
                    Some(prefix) => name.starts_with(prefix),
                    None => name == pattern,
                })
        })
        .map(|c| c.display.clone())
        .unwrap_or_else(|| name.to_owned())
}

/// The fewest instruction files that reach every one of `clients`.
///
/// Returns each file with the clients it serves, in a stable order. Clients
/// that read the same file get it once. Greedy: the file that reaches the
/// most clients not yet reached is taken first, ties going to the file listed
/// first in registry order, so the same set of clients always gives the same
/// files. A client that reads nothing is left out.
pub fn instruction_files(clients: &[Client]) -> Vec<(String, Vec<String>)> {
    let mut remaining: Vec<&Client> = clients
        .iter()
        .filter(|c| c.instructions.as_ref().is_some_and(|i| !i.reads.is_empty()))
        .collect();
    // Registry order, whatever order they were asked for in.
    let order = ids();
    remaining.sort_by_key(|c| {
        order
            .iter()
            .position(|id| *id == c.id)
            .unwrap_or(usize::MAX)
    });
    let mut chosen: Vec<(String, Vec<String>)> = Vec::new();
    while !remaining.is_empty() {
        // The file read by the most remaining clients; ties go to the file
        // that comes first in registry order.
        let mut candidates: Vec<&String> = Vec::new();
        for c in &remaining {
            for f in c.instructions.iter().flat_map(|i| &i.reads) {
                if !candidates.contains(&f) {
                    candidates.push(f);
                }
            }
        }
        let reach = |f: &String| {
            remaining
                .iter()
                .filter(|c| c.instructions.iter().any(|i| i.reads.contains(f)))
                .count()
        };
        let Some(best) = candidates
            .iter()
            .copied()
            .enumerate()
            .max_by(|(ia, a), (ib, b)| reach(a).cmp(&reach(b)).then(ib.cmp(ia)))
            .map(|(_, f)| f.clone())
        else {
            break;
        };
        let served: Vec<String> = remaining
            .iter()
            .filter(|c| c.instructions.iter().any(|i| i.reads.contains(&best)))
            .map(|c| c.id.clone())
            .collect();
        remaining.retain(|c| !served.contains(&c.id));
        chosen.push((best, served));
    }
    chosen
}

/// Find one client by id.
pub fn find(id: &str) -> Option<Client> {
    all().into_iter().find(|c| c.id == id)
}

/// Every client id, for help text and error messages.
pub fn ids() -> Vec<String> {
    all().into_iter().map(|c| c.id).collect()
}

/// The user's home directory.
///
/// `MEMFORK_HOME` overrides it, which is how the fixture tests point a whole
/// registration run at a temporary directory without touching a real config.
pub fn home_dir() -> Option<String> {
    if let Some(over) = std::env::var_os("MEMFORK_HOME") {
        return Some(over.to_string_lossy().into_owned());
    }
    #[allow(
        deprecated,
        reason = "un-deprecated in Rust 1.86 with correct Windows behaviour"
    )]
    std::env::home_dir().map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writers_are_shown_by_the_name_a_person_knows() {
        assert_eq!(display_for_writer("claude-code"), "Claude Code");
        assert_eq!(display_for_writer("codex-mcp-client"), "Codex CLI");
        assert_eq!(display_for_writer("gemini-cli-mcp-client"), "Gemini CLI");
        // Prefix match: this client's name includes the server's name.
        assert_eq!(display_for_writer("grok-shell-memfork"), "Grok Build");
        // Unknown clients keep the name they gave.
        assert_eq!(display_for_writer("some-new-client"), "some-new-client");
    }

    #[test]
    fn every_client_says_where_it_reads_instructions() {
        for c in all() {
            let i = c
                .instructions
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no instructions entry", c.id));
            assert!(!i.reads.is_empty(), "{}", c.id);
            assert!(i.docs.starts_with("https://"), "{}", c.id);
            assert!(i.verified.len() == 10, "{}", c.id);
            for f in &i.reads {
                assert!(
                    !f.starts_with('/') && !f.contains('\\') && !f.contains(".."),
                    "{}: `{f}` must be a plain path inside the repository",
                    c.id
                );
            }
        }
    }

    #[test]
    fn clients_that_share_a_file_get_it_once() {
        let pick = |ids: &[&str]| {
            let clients: Vec<Client> = ids.iter().map(|id| find(id).unwrap()).collect();
            instruction_files(&clients)
        };
        let agents = crate::init::project::AGENTS_FILE;
        let shared = pick(&["codex", "cursor", "grok"]);
        assert_eq!(shared.len(), 1, "{shared:?}");
        assert_eq!(shared[0].0, agents);
        assert_eq!(shared[0].1, ["cursor", "codex", "grok"]);

        // Every client, and each file it needs, with no file twice.
        let all_ids = ids();
        let every = pick(&all_ids.iter().map(String::as_str).collect::<Vec<_>>());
        let files: Vec<&str> = every.iter().map(|(f, _)| f.as_str()).collect();
        let mut unique = files.clone();
        unique.dedup();
        assert_eq!(files, unique);
        for id in &all_ids {
            assert!(
                every
                    .iter()
                    .any(|(_, served)| served.iter().any(|s| s == id)),
                "{id} is not reached by {every:?}"
            );
        }
        // Three files reach everyone: AGENTS.md for most, and the two clients
        // with a file of their own.
        assert_eq!(files.len(), 3, "{every:?}");

        // Stable: the same clients in another order give the same files.
        let mut backwards = all_ids.clone();
        backwards.reverse();
        let reversed = pick(&backwards.iter().map(String::as_str).collect::<Vec<_>>());
        let names =
            |v: &[(String, Vec<String>)]| v.iter().map(|(f, _)| f.clone()).collect::<Vec<_>>();
        assert_eq!(names(&every), names(&reversed));
    }

    #[test]
    fn the_registry_parses_and_is_complete() {
        let clients = load().expect("the compiled-in registry parses");
        assert_eq!(
            ids(),
            vec![
                "claude-code",
                "cursor",
                "codex",
                "gemini-cli",
                "grok",
                "cline",
                "opencode",
                "qwen-code",
                "kiro",
                "copilot-cli",
                "devin",
                "windsurf",
                "zed",
                "vscode",
                "factory-droid",
                "openhands",
            ],
            "the registry is the list of supported clients"
        );
        for c in &clients {
            assert!(!c.display.is_empty(), "{} has no display name", c.id);
            assert!(c.docs.starts_with("https://"), "{} has no docs link", c.id);
            assert!(
                c.verified.len() == 10 && c.verified.starts_with("20"),
                "{} has no verification date",
                c.id
            );
            assert!(
                c.cli.is_some() || c.file.is_some(),
                "{} has no way to register at all",
                c.id
            );
            assert!(
                !c.detect_dirs.as_slice().is_empty(),
                "{} has no way to be detected",
                c.id
            );
            if let Some(why) = &c.unverified {
                assert!(why.len() > 20 && !why.ends_with('.'), "{}: {why}", c.id);
            }
            for name in &c.mcp_names {
                assert!(!name.is_empty(), "{}", c.id);
            }
        }
    }

    #[test]
    fn what_could_not_be_verified_is_said_not_assumed() {
        // A closed-source client whose initialize name nobody has seen in its
        // source or a build has no `mcp_names`, and says so.
        for id in ["cursor", "kiro", "devin", "windsurf", "vscode", "openhands"] {
            let c = find(id).expect(id);
            assert!(c.mcp_names.is_empty(), "{id} guesses a client name");
            let why = c.unverified.as_deref().unwrap_or_default();
            assert!(why.contains("MCP initialize"), "{id}: {why}");
        }
        // Copilot's name comes from logs in its own issue tracker: recorded,
        // and marked as such.
        let copilot = find("copilot-cli").expect("copilot");
        assert_eq!(copilot.mcp_names, ["copilot-cli"]);
        assert!(copilot
            .unverified
            .as_deref()
            .is_some_and(|w| w.contains("issue tracker")));
    }

    fn vars(os: Os) -> Vars {
        Vars::for_home(
            match os {
                Os::Linux => "/home/ada",
                Os::MacOs => "/Users/ada",
                Os::Windows => "C:\\Users\\ada",
            },
            os,
        )
    }

    fn user_path(id: &str, os: Os) -> String {
        find(id)
            .and_then(|c| c.file)
            .unwrap_or_else(|| panic!("{id} registers by file"))
            .path_for(Scope::User, os, &vars(os))
            .unwrap_or_else(|| panic!("{id} has no user path on {os:?}"))
    }

    fn project_path(id: &str, os: Os) -> Option<String> {
        find(id)
            .and_then(|c| c.file)
            .unwrap_or_else(|| panic!("{id} registers by file"))
            .path_for(Scope::Project, os, &vars(os))
    }

    #[test]
    fn paths_resolve_for_every_os() {
        assert_eq!(user_path("cursor", Os::Linux), "/home/ada/.cursor/mcp.json");
        assert_eq!(
            user_path("cursor", Os::MacOs),
            "/Users/ada/.cursor/mcp.json"
        );
        assert_eq!(
            user_path("cursor", Os::Windows),
            "C:\\Users\\ada\\.cursor\\mcp.json"
        );
        assert_eq!(
            project_path("cursor", Os::Windows),
            Some(".cursor\\mcp.json".to_owned())
        );
    }

    #[test]
    fn a_trailing_separator_on_home_does_not_double_up() {
        let cursor = find("cursor").and_then(|c| c.file).expect("cursor");
        assert_eq!(
            cursor.path_for(
                Scope::User,
                Os::Linux,
                &Vars::for_home("/home/ada/", Os::Linux)
            ),
            Some("/home/ada/.cursor/mcp.json".to_owned())
        );
    }

    #[test]
    fn the_new_clients_paths_follow_each_os_convention() {
        // VS Code and its extension keep user data in the roaming profile on
        // Windows and Application Support on macOS.
        assert_eq!(
            user_path("vscode", Os::Windows),
            "C:\\Users\\ada\\AppData\\Roaming\\Code\\User\\mcp.json"
        );
        assert_eq!(
            user_path("vscode", Os::MacOs),
            "/Users/ada/Library/Application Support/Code/User/mcp.json"
        );
        assert_eq!(
            user_path("vscode", Os::Linux),
            "/home/ada/.config/Code/User/mcp.json"
        );
        assert_eq!(
            user_path("cline", Os::Windows),
            "C:\\Users\\ada\\AppData\\Roaming\\Code\\User\\globalStorage\\saoudrizwan.claude-dev\\settings\\cline_mcp_settings.json"
        );
        assert_eq!(
            user_path("cline", Os::MacOs),
            "/Users/ada/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json"
        );
        // Zed: XDG on Linux and macOS, the roaming profile on Windows.
        assert_eq!(
            user_path("zed", Os::Linux),
            "/home/ada/.config/zed/settings.json"
        );
        assert_eq!(
            user_path("zed", Os::MacOs),
            "/Users/ada/.config/zed/settings.json"
        );
        assert_eq!(
            user_path("zed", Os::Windows),
            "C:\\Users\\ada\\AppData\\Roaming\\Zed\\settings.json"
        );
        // OpenCode uses the XDG layout everywhere, Windows included.
        assert_eq!(
            user_path("opencode", Os::Windows),
            "C:\\Users\\ada\\.config\\opencode\\opencode.json"
        );
        // Devin: XDG on Unix, the roaming profile on Windows.
        assert_eq!(
            user_path("devin", Os::Linux),
            "/home/ada/.config/devin/mcp_config.json"
        );
        assert_eq!(
            user_path("devin", Os::Windows),
            "C:\\Users\\ada\\AppData\\Roaming\\devin\\mcp_config.json"
        );
        // Plain dot-directories under home for the rest.
        assert_eq!(
            user_path("qwen-code", Os::Linux),
            "/home/ada/.qwen/settings.json"
        );
        assert_eq!(
            user_path("kiro", Os::Linux),
            "/home/ada/.kiro/settings/mcp.json"
        );
        assert_eq!(
            user_path("copilot-cli", Os::Windows),
            "C:\\Users\\ada\\.copilot\\mcp-config.json"
        );
        assert_eq!(
            user_path("windsurf", Os::MacOs),
            "/Users/ada/.codeium/windsurf/mcp_config.json"
        );
        assert_eq!(
            user_path("factory-droid", Os::Linux),
            "/home/ada/.factory/mcp.json"
        );
        assert_eq!(
            user_path("openhands", Os::Linux),
            "/home/ada/.openhands/mcp.json"
        );
        // Some read no project file at all, and the registry says so.
        for id in ["cline", "windsurf", "openhands"] {
            assert_eq!(project_path(id, Os::Linux), None, "{id}");
            assert!(!find(id).unwrap().file_writable(Scope::Project), "{id}");
        }
        assert_eq!(
            project_path("copilot-cli", Os::Linux),
            Some(".github/mcp.json".to_owned())
        );
    }

    #[test]
    fn a_stand_in_home_keeps_every_variable_under_it() {
        // MEMFORK_HOME is how the tests keep away from a real machine's
        // configs; the Windows and XDG variables must follow it, not the
        // real environment.
        let v = Vars::for_home("/tmp/home", Os::Linux);
        assert_eq!(v.xdg_config_home, "/tmp/home/.config");
        assert_eq!(v.xdg_data_home, "/tmp/home/.local/share");
        let w = Vars::for_home("C:\\tmp\\home", Os::Windows);
        assert_eq!(w.appdata, "C:\\tmp\\home\\AppData\\Roaming");
        assert_eq!(w.localappdata, "C:\\tmp\\home\\AppData\\Local");
        // And the longer names are not mistaken for the shorter ones.
        assert_eq!(
            w.expand("$LOCALAPPDATA/x/$APPDATA/y", Os::Windows),
            "C:\\tmp\\home\\AppData\\Local\\x\\C:\\tmp\\home\\AppData\\Roaming\\y"
        );
    }

    #[test]
    fn the_add_command_is_built_from_the_template() {
        let claude = find("claude-code").and_then(|c| c.cli).expect("claude cli");
        assert_eq!(
            claude
                .add_args(Scope::User, &Launch::program("memfork"))
                .unwrap(),
            vec!["mcp", "add", "--scope", "user", "memfork", "--", "memfork", "mcp"]
        );

        // Gemini takes the command positionally, with no `--` separator.
        let gemini = find("gemini-cli").and_then(|c| c.cli).expect("gemini cli");
        assert_eq!(
            gemini
                .add_args(Scope::Project, &Launch::program("/opt/memfork"))
                .unwrap(),
            vec![
                "mcp",
                "add",
                "-s",
                "project",
                "memfork",
                "/opt/memfork",
                "mcp"
            ]
        );

        // Grok's user scope is the default and contributes no flags.
        let grok = find("grok").and_then(|c| c.cli).expect("grok cli");
        assert_eq!(
            grok.add_args(Scope::User, &Launch::program("memfork"))
                .unwrap(),
            vec!["mcp", "add", "memfork", "--", "memfork", "mcp"]
        );

        // The new commands, as each documents them.
        let args = |id: &str, scope| {
            find(id)
                .and_then(|c| c.cli)
                .unwrap_or_else(|| panic!("{id} has a cli"))
                .add_args(scope, &Launch::program("memfork"))
        };
        assert_eq!(
            args("cline", Scope::User).unwrap(),
            vec!["mcp", "add", "memfork", "--yes", "--", "memfork", "mcp"]
        );
        assert_eq!(
            args("opencode", Scope::User).unwrap(),
            vec!["mcp", "add", "memfork", "--", "memfork", "mcp"]
        );
        assert_eq!(
            args("qwen-code", Scope::Project).unwrap(),
            vec!["mcp", "add", "-s", "project", "memfork", "memfork", "mcp"]
        );
        assert_eq!(
            args("copilot-cli", Scope::User).unwrap(),
            vec!["mcp", "add", "memfork", "--", "memfork", "mcp"]
        );
        assert_eq!(
            args("devin", Scope::User).unwrap(),
            vec!["mcp", "add", "-s", "user", "memfork", "--", "memfork", "mcp"]
        );
        assert_eq!(
            args("openhands", Scope::User).unwrap(),
            vec![
                "mcp",
                "add",
                "memfork",
                "--transport",
                "stdio",
                "memfork",
                "--",
                "mcp"
            ]
        );
        // Commands with no project scope say so, and the file is used.
        for id in ["cline", "opencode", "copilot-cli", "openhands"] {
            assert!(args(id, Scope::Project).is_none(), "{id}");
        }
    }

    #[test]
    fn a_cli_that_cannot_express_a_scope_says_so() {
        // `codex mcp add` documents no scope flag, so project scope has to go
        // through the config file instead.
        let codex = find("codex").and_then(|c| c.cli).expect("codex cli");
        assert!(codex
            .add_args(Scope::User, &Launch::program("memfork"))
            .is_some());
        assert!(codex
            .add_args(Scope::Project, &Launch::program("memfork"))
            .is_none());
    }

    #[test]
    fn a_cli_that_cannot_answer_has_no_status_command() {
        // Cline's command adds but cannot list outside its wizard; OpenCode
        // adds and lists but cannot remove.
        let cline = find("cline").and_then(|c| c.cli).expect("cline cli");
        assert!(cline.status_args().is_none());
        assert!(cline.remove_args(Scope::User).is_some());
        let opencode = find("opencode").and_then(|c| c.cli).expect("opencode cli");
        assert!(opencode.status_args().is_some());
        assert!(opencode.remove_args(Scope::User).is_none());
    }

    #[test]
    fn claude_codes_user_file_is_not_writable() {
        // It holds the OAuth session; MemFork goes through the CLI or not at all.
        let claude = find("claude-code").expect("claude-code");
        assert!(!claude.file_writable(Scope::User));
        assert!(claude.file_writable(Scope::Project));

        // Every other client's user file is ordinary and may be written.
        for id in ids().iter().filter(|id| *id != "claude-code") {
            assert!(
                find(id).expect(id).file_writable(Scope::User),
                "{id} should be writable"
            );
        }
    }

    #[test]
    fn the_http_url_quirks_are_recorded() {
        // DESIGN §6.1 calls this out by name: it must be data, not code.
        for (id, key) in [
            ("gemini-cli", "httpUrl"),
            ("qwen-code", "httpUrl"),
            ("windsurf", "serverUrl"),
        ] {
            let f = find(id).and_then(|c| c.file).expect(id);
            assert_eq!(f.http_url_key, key, "{id}");
        }
        for id in [
            "cursor",
            "codex",
            "grok",
            "claude-code",
            "vscode",
            "zed",
            "opencode",
        ] {
            let f = find(id).and_then(|c| c.file).expect(id);
            assert_eq!(f.http_url_key, "url", "{id}");
        }
    }

    #[test]
    fn the_entry_shapes_are_data() {
        let shape = |id: &str| find(id).and_then(|c| c.file).expect(id);
        assert_eq!(shape("claude-code").stdio_type.as_deref(), Some("stdio"));
        assert_eq!(shape("vscode").stdio_type.as_deref(), Some("stdio"));
        assert_eq!(shape("opencode").stdio_type.as_deref(), Some("local"));
        assert_eq!(shape("copilot-cli").stdio_type.as_deref(), Some("local"));
        assert_eq!(shape("cursor").stdio_type, None);
        assert_eq!(shape("opencode").command_style, CommandStyle::Array);
        assert_eq!(shape("vscode").command_style, CommandStyle::Split);
        assert!(shape("copilot-cli")
            .extra
            .is_some_and(|e| e.contains_key("tools")));
        assert_eq!(shape("zed").format, Format::Jsonc);
        assert_eq!(shape("zed").server_map, ["context_servers"]);
        assert_eq!(shape("vscode").server_map, ["servers"]);
        assert_eq!(shape("opencode").server_map, ["mcp"]);
    }
}
