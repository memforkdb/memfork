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
    /// A directory the client creates for itself, used to tell "installed but
    /// never configured" apart from "not installed".
    #[serde(default)]
    pub detect_dir: Option<String>,
}

/// The client's own `mcp add` command.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientCli {
    /// Executable to look for on `PATH`.
    pub binary: String,
    /// Argument template for adding a server.
    pub add: Vec<String>,
    /// Argument template for asking whether a server is registered.
    pub status: Vec<String>,
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

    /// Expand the `status` template into a concrete argument list.
    pub fn status_args(&self) -> Vec<String> {
        // No `{command}` or `{args}` appears in a status template: asking
        // about a server takes its name, not the command behind it.
        self.expand(&self.status, &[], &Launch::program(""))
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
    /// `json` or `toml`.
    pub format: Format,
    /// Key path to the table of servers, e.g. `["mcpServers"]`.
    pub server_map: Vec<String>,
    /// Whether the entry carries `"type": "stdio"`.
    #[serde(default)]
    pub emit_type_stdio: bool,
    /// Which key a remote server's URL goes under. Encoded here
    /// because the clients disagree (DESIGN §6.1).
    pub http_url_key: String,
    /// User-scope path template, with `$HOME`.
    ///
    /// Absent when MemFork will not go near this client's user configuration —
    /// because the file holds more than MCP servers, and the client's own
    /// command is the supported way in.
    #[serde(default)]
    pub user: Option<String>,
    /// Project-scope path template, relative to the working directory.
    pub project: String,
    /// Per-OS overrides, keyed `user_windows`, `project_macos` and so on.
    #[serde(flatten)]
    overrides: BTreeMap<String, toml::Value>,
}

/// Which syntax a client's config file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// JSON, edited with key order preserved.
    Json,
    /// TOML, edited in place with comments and formatting preserved.
    Toml,
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
            Scope::Project => Some(&self.project),
        }
    }

    /// Resolve the config path for a scope, an OS and a home directory.
    ///
    /// Returns a string rather than a `PathBuf` so that one machine can render
    /// another platform's paths, which is what makes the three-OS fixtures
    /// testable from anywhere.
    pub fn path_for(&self, scope: Scope, os: Os, home: &str) -> Option<String> {
        let template = self.template(scope, os)?;
        let expanded = template.replace("$HOME", home.trim_end_matches(['/', '\\']));
        let sep = os.separator();
        Some(
            expanded
                .split(['/', '\\'])
                .collect::<Vec<_>>()
                .join(&sep.to_string()),
        )
    }

    /// Resolve the config path on this machine.
    pub fn path_here(&self, scope: Scope, home: &str) -> Option<PathBuf> {
        self.path_for(scope, Os::current(), home).map(PathBuf::from)
    }
}

impl Client {
    /// The client's own directory on this machine, if the registry names one.
    pub fn detect_dir_here(&self, home: &str) -> Option<PathBuf> {
        let template = self.detect_dir.as_ref()?;
        let expanded = template.replace("$HOME", home.trim_end_matches(['/', '\\']));
        Some(PathBuf::from(expanded))
    }

    /// Whether MemFork may write this client's file for a scope.
    ///
    /// A missing path is the prohibition: the registry simply does not carry
    /// the location, so no code path can reach it.
    pub fn file_writable(&self, scope: Scope) -> bool {
        match (&self.file, scope) {
            (None, _) => false,
            (Some(f), Scope::User) => f.user.is_some(),
            (Some(_), Scope::Project) => true,
        }
    }

    /// The command that asks this client whether MemFork is registered, if it
    /// has one and it is installed.
    pub fn status_command(&self) -> Option<(String, PathBuf, Vec<String>)> {
        let cli = self.cli.as_ref()?;
        let resolved = crate::init::which(&cli.binary)?;
        Some((cli.binary.clone(), resolved, cli.status_args()))
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
        return match std::process::Command::new(&resolved).args(&args).output() {
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
    load().unwrap_or_default()
}

/// Parse the registry, reporting why if it cannot be read.
pub fn load() -> Result<Vec<Client>, String> {
    toml::from_str::<Registry>(REGISTRY)
        .map(|r| r.client)
        .map_err(|e| format!("the compiled-in client registry is malformed: {e}"))
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
    fn the_registry_parses_and_is_complete() {
        let clients = load().expect("the compiled-in registry parses");
        assert_eq!(
            ids(),
            vec!["claude-code", "cursor", "codex", "gemini-cli", "grok"],
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
        }
    }

    #[test]
    fn paths_resolve_for_every_os() {
        let cursor = find("cursor").expect("cursor is in the registry");
        let file = cursor.file.expect("cursor registers by file");
        assert_eq!(
            file.path_for(Scope::User, Os::Linux, "/home/ada"),
            Some("/home/ada/.cursor/mcp.json".to_owned())
        );
        assert_eq!(
            file.path_for(Scope::User, Os::MacOs, "/Users/ada"),
            Some("/Users/ada/.cursor/mcp.json".to_owned())
        );
        assert_eq!(
            file.path_for(Scope::User, Os::Windows, "C:\\Users\\ada"),
            Some("C:\\Users\\ada\\.cursor\\mcp.json".to_owned())
        );
        assert_eq!(
            file.path_for(Scope::Project, Os::Windows, "C:\\Users\\ada"),
            Some(".cursor\\mcp.json".to_owned())
        );
    }

    #[test]
    fn a_trailing_separator_on_home_does_not_double_up() {
        let cursor = find("cursor").and_then(|c| c.file).expect("cursor");
        assert_eq!(
            cursor.path_for(Scope::User, Os::Linux, "/home/ada/"),
            Some("/home/ada/.cursor/mcp.json".to_owned())
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
    fn claude_codes_user_file_is_not_writable() {
        // It holds the OAuth session; MemFork goes through the CLI or not at all.
        let claude = find("claude-code").expect("claude-code");
        assert!(!claude.file_writable(Scope::User));
        assert!(claude.file_writable(Scope::Project));

        // Every other client's user file is ordinary and may be written.
        for id in ["cursor", "codex", "gemini-cli", "grok"] {
            assert!(
                find(id).expect(id).file_writable(Scope::User),
                "{id} should be writable"
            );
        }
    }

    #[test]
    fn the_gemini_http_url_quirk_is_recorded() {
        // DESIGN §6.1 calls this out by name: it must be data, not code.
        let gemini = find("gemini-cli")
            .and_then(|c| c.file)
            .expect("gemini file");
        assert_eq!(gemini.http_url_key, "httpUrl");
        for id in ["cursor", "codex", "grok", "claude-code"] {
            let f = find(id).and_then(|c| c.file).expect(id);
            assert_eq!(f.http_url_key, "url", "{id}");
        }
    }
}
