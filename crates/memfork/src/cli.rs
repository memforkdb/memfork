//! Command-line surface over the engine (DESIGN §5).

use clap::{Args, Parser, Subcommand};

/// Git for agent state, in process.
#[derive(Debug, Parser)]
#[command(
    name = "memfork",
    version,
    about = "Git for agent state, in process: fork, merge, discard and rewind an agent's memory.",
    long_about = None,
    propagate_version = true
)]
pub struct Cli {
    /// Options every subcommand understands.
    #[command(flatten)]
    pub global: GlobalArgs,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Options every subcommand understands.
#[derive(Debug, Clone, Args)]
pub struct GlobalArgs {
    /// Branch to operate on.
    #[arg(long, short, global = true, default_value = "main")]
    pub branch: String,

    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    pub json: bool,
}

/// Where a persistent command keeps its data, and how carefully.
#[derive(Debug, Clone, Args)]
pub struct PersistArgs {
    /// Keep nothing: run entirely in memory and forget it all on exit.
    ///
    /// The opposite of the library default. `memfork-core` is in-memory
    /// unless a caller asks for durability; the binary persists unless told
    /// not to, because an agent's memory that empties on restart is not
    /// memory.
    #[arg(long)]
    pub ephemeral: bool,

    /// Where to keep the data. Defaults to a per-user directory, or to
    /// `./.memfork` if that directory already exists.
    #[arg(long, value_name = "PATH")]
    pub data_dir: Option<String>,

    /// When to flush the log to disk.
    ///
    /// `always` (the default) means an acknowledged change has reached the
    /// disk. `interval` batches flushes and risks the last window to a power
    /// cut. `never` risks everything since the last snapshot, and survives a
    /// process crash but not a machine one.
    #[arg(long, default_value = "always", value_parser = ["always", "interval", "never"])]
    pub fsync: String,

    /// How many commits per branch to keep before folding them into the
    /// snapshot. Time travel reaches back this far across a restart.
    #[arg(long, value_name = "COMMITS", default_value_t = 10_000)]
    pub retention: u64,

    /// Approximate memory ceiling per branch, in bytes. Without one, nothing
    /// is ever evicted.
    #[arg(long, value_name = "BYTES")]
    pub memory_budget: Option<usize>,

    /// How many commits it takes for an unread entry's eviction score to
    /// halve. Only meaningful alongside a memory budget.
    #[arg(long, value_name = "COMMITS", default_value_t = 1000.0)]
    pub half_life: f64,

    /// How long a daemon started for this session stays alive with nothing to
    /// do. Zero means never exit.
    #[arg(long, value_name = "SECONDS", default_value_t = 600)]
    pub idle_timeout: u64,
}

/// One line of a batch script: a subcommand with no program name in front.
#[derive(Debug, Parser)]
#[command(name = "", no_binary_name = true, disable_help_flag = false)]
pub struct ScriptLine {
    /// The options this line may set for itself.
    #[command(flatten)]
    pub global: ScriptGlobalArgs,

    /// The operation this line performs.
    #[command(subcommand)]
    pub command: Command,
}

/// The per-line options of a batch script. `--json` is decided once for the
/// whole run, so a script line may only choose its branch.
#[derive(Debug, Clone, Args)]
pub struct ScriptGlobalArgs {
    /// Branch to operate on.
    #[arg(long, short, global = true)]
    pub branch: Option<String>,
}

/// Everything `memfork` can be asked to do.
///
/// The doc comment on each variant is also its `--help` text, so it is written
/// for the person reading the terminal.
#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Write a key.
    Put {
        /// The key to write.
        key: String,
        /// The value to store. Opaque bytes; JSON by convention.
        value: String,
        /// Retention weight in [0, 1].
        #[arg(long)]
        importance: Option<f32>,
        /// Comma-separated vector, e.g. `0.1,0.2,0.3`.
        #[arg(long)]
        embedding: Option<String>,
        /// Expire this entry after this many commits on its branch.
        #[arg(long)]
        ttl_commits: Option<u64>,
        /// Metadata pair, repeatable: `--meta source=notes`.
        #[arg(long, value_name = "KEY=VALUE")]
        meta: Vec<String>,
    },

    /// Read a key.
    Get {
        /// The key to read.
        key: String,
    },

    /// Remove a key.
    Del {
        /// The key to remove.
        key: String,
    },

    /// List keys, in ascending order.
    Ls {
        /// Only keys starting with this prefix.
        #[arg(default_value = "")]
        prefix: String,
        /// Stop after this many keys.
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Search by cosine similarity against a query vector.
    Search {
        /// Comma-separated query vector, e.g. `0.1,0.2,0.3`.
        embedding: String,
        /// How many results to return.
        #[arg(long, default_value_t = 10)]
        k: usize,
        /// Only consider keys starting with this prefix.
        #[arg(long)]
        prefix: Option<String>,
    },

    /// Create a branch pointing at another branch, optionally at a past commit.
    ///
    /// Fork before any risky or exploratory step: it costs the same whether
    /// the branch holds ten keys or ten million.
    Fork {
        /// Name of the new branch.
        name: String,
        /// Branch to fork from. Defaults to `--branch`.
        #[arg(long)]
        from: Option<String>,
        /// Fork from this sequence number instead of the head.
        #[arg(long)]
        at_seq: Option<u64>,
    },

    /// Merge one branch into another.
    Merge {
        /// Branch to merge from.
        source: String,
        /// Branch to merge into. Defaults to `--branch`.
        #[arg(long)]
        target: Option<String>,
        /// What to do with keys both sides changed.
        #[arg(long, default_value = "fail", value_parser = ["fail", "ours", "theirs"])]
        policy: String,
    },

    /// Delete a branch and everything only it could reach.
    Discard {
        /// Name of the branch to delete.
        name: String,
    },

    /// List branches.
    Branches,

    /// Show a branch's history, newest first.
    Log {
        /// Stop after this many commits.
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Read a branch as it was after a past commit.
    At {
        /// The sequence number to read at. 0 is the empty genesis state.
        seq: u64,
        /// Read one key instead of listing.
        #[arg(long)]
        key: Option<String>,
        /// Only list keys starting with this prefix.
        #[arg(long)]
        prefix: Option<String>,
    },

    /// Show key-level differences between two branches or commit ids.
    Diff {
        /// The left side.
        a: String,
        /// The right side.
        b: String,
    },

    /// Run a batch of commands against one in-memory database.
    ///
    /// Each line is a subcommand exactly as it would be typed, `#` starts a
    /// comment, and blank lines are ignored. Against an in-memory database
    /// this is the way to run more than one operation against the same state.
    Run {
        /// Script file to run, or `-` to read standard input.
        script: String,
    },

    /// Serve the MCP tools over stdio.
    ///
    /// This is what an MCP client runs. State is kept in the data directory
    /// and survives restarts, unless `--ephemeral` says otherwise.
    Mcp {
        /// Where to keep the data, and how carefully.
        #[command(flatten)]
        persist: PersistArgs,
    },

    /// Print the tool definitions in a vendor's function-calling format.
    ///
    /// For models that do not speak MCP at all. Pair it with `memfork call`.
    Tools {
        /// Which vendor's format to print.
        #[arg(long, value_parser = ["openai", "anthropic", "gemini"])]
        format: String,
    },

    /// Execute one tool call and print the result.
    ///
    /// Single-shot, like the other one-off subcommands: the database is
    /// created for this call and discarded after it. Since
    /// durability, use `memfork run` to put several operations on one state.
    Call {
        /// Tool name, for example `memfork_put`.
        tool: String,
        /// Arguments as a JSON object. Defaults to `{}`.
        #[arg(default_value = "{}")]
        arguments: String,
    },

    /// Register the MCP server with the MCP clients installed here.
    ///
    /// Prefers each client's own `mcp add` command, and falls back to editing
    /// its config file — changing only the MemFork entry, and keeping a
    /// timestamped backup. Re-running changes nothing.
    Init {
        /// Print what would change without running or writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Only configure this client, by registry id.
        #[arg(long)]
        client: Option<String>,
        /// Register for this user everywhere, or only for this project.
        #[arg(long, default_value = "user", value_parser = ["user", "project"])]
        scope: String,
    },

    /// Report version, paths, persistence and which clients know about MemFork.
    Doctor,

    /// Commit in a loop until killed, for the crash-recovery tests.
    ///
    /// Hidden because it is a test fixture, not a feature: it exists so the
    /// crash tests can kill a real process mid-write rather than simulate one.
    #[command(hide = true)]
    CrashWriter {
        /// Where to write.
        #[command(flatten)]
        persist: PersistArgs,
        /// Where to append the id of every commit that is durable.
        #[arg(long, value_name = "PATH")]
        progress: String,
        /// Stop after this many commits, in case nothing kills it.
        #[arg(long, default_value_t = 100_000)]
        limit: u64,
    },

    /// Run the shared daemon that several clients can use at once.
    ///
    /// Started for you when a client needs one, so there is rarely a reason to
    /// run it by hand. Binds loopback only and needs the token from its own
    /// endpoint file, so nothing off this machine can reach it.
    Serve {
        /// Where to keep the data, and how carefully.
        #[command(flatten)]
        persist: PersistArgs,
        /// Port to listen on, on 127.0.0.1 only. Zero lets the system choose.
        ///
        /// The chosen port goes in the endpoint file, so clients find it
        /// without being told.
        #[arg(long, default_value_t = 0)]
        port: u16,
    },

    /// Stop the daemon for a data directory, gracefully.
    ///
    /// It flushes, releases the directory and removes its endpoint file. Use
    /// this before upgrading, so the new version starts its own daemon rather
    /// than refusing to talk to the old one.
    Stop {
        /// Which data directory's daemon. Defaults to the usual one.
        #[arg(long, value_name = "PATH")]
        data_dir: Option<String>,
    },
}

impl Command {
    /// Whether this subcommand may appear inside a batch script.
    ///
    /// Scripts run against one in-process database, so anything that starts a
    /// server, writes to another program's config or recurses is refused.
    pub fn allowed_in_script(&self) -> bool {
        !matches!(
            self,
            Command::Run { .. }
                | Command::Mcp { .. }
                | Command::CrashWriter { .. }
                | Command::Tools { .. }
                | Command::Call { .. }
                | Command::Serve { .. }
                | Command::Stop { .. }
                | Command::Init { .. }
                | Command::Doctor
        )
    }

    /// The name a script line would use, for error messages.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Put { .. } => "put",
            Command::Get { .. } => "get",
            Command::Del { .. } => "del",
            Command::Ls { .. } => "ls",
            Command::Search { .. } => "search",
            Command::Fork { .. } => "fork",
            Command::Merge { .. } => "merge",
            Command::Discard { .. } => "discard",
            Command::Branches => "branches",
            Command::Log { .. } => "log",
            Command::At { .. } => "at",
            Command::Diff { .. } => "diff",
            Command::Run { .. } => "run",
            Command::Mcp { .. } => "mcp",
            Command::Tools { .. } => "tools",
            Command::Call { .. } => "call",
            Command::Serve { .. } => "serve",
            Command::Init { .. } => "init",
            Command::Doctor => "doctor",
            Command::CrashWriter { .. } => "crash-writer",
            Command::Stop { .. } => "stop",
        }
    }
}

/// Split a script line into arguments, honouring single and double quotes and
/// backslash escapes, so values may contain spaces.
pub fn split_line(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has_token = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();

    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), ch) if ch == q => quote = None,
            (Some('\''), ch) => cur.push(ch),
            (Some('"'), '\\') => match chars.next() {
                Some(escaped) => cur.push(escaped),
                None => return Err("line ends with a trailing backslash".to_owned()),
            },
            (Some(_), ch) => cur.push(ch),
            (None, '\'') | (None, '"') => {
                quote = Some(c);
                has_token = true;
            }
            (None, '\\') => match chars.next() {
                Some(escaped) => {
                    cur.push(escaped);
                    has_token = true;
                }
                None => return Err("line ends with a trailing backslash".to_owned()),
            },
            (None, ch) if ch.is_whitespace() => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            (None, ch) => {
                cur.push(ch);
                has_token = true;
            }
        }
    }
    if quote.is_some() {
        return Err("line ends inside an unclosed quote".to_owned());
    }
    if has_token {
        out.push(cur);
    }
    Ok(out)
}

/// Parse a comma-separated vector.
pub fn parse_vector(s: &str) -> Result<Vec<f32>, String> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            p.parse::<f32>()
                .map_err(|_| format!("`{p}` is not a number"))
        })
        .collect()
}

/// Parse a `key=value` metadata pair.
pub fn parse_meta(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_owned(), v.to_owned())),
        _ => Err(format!("`{s}` is not a KEY=VALUE pair")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_plain_words() {
        assert_eq!(split_line("put a b").unwrap(), vec!["put", "a", "b"]);
        assert_eq!(split_line("   ").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn honours_quotes_and_escapes() {
        assert_eq!(
            split_line(r#"put key "a value with spaces""#).unwrap(),
            vec!["put", "key", "a value with spaces"]
        );
        assert_eq!(
            split_line(r#"put key 'single "quoted"'"#).unwrap(),
            vec!["put", "key", r#"single "quoted""#]
        );
        assert_eq!(
            split_line(r#"put key "escaped \" quote""#).unwrap(),
            vec!["put", "key", r#"escaped " quote"#]
        );
        // An empty quoted string is a real, empty argument.
        assert_eq!(split_line(r#"put key """#).unwrap(), vec!["put", "key", ""]);
    }

    #[test]
    fn rejects_unbalanced_quotes() {
        assert!(split_line(r#"put key "unclosed"#).is_err());
        assert!(split_line("put key trailing\\").is_err());
    }

    #[test]
    fn parses_vectors_and_meta() {
        assert_eq!(parse_vector("0.5,-1,2").unwrap(), vec![0.5, -1.0, 2.0]);
        assert!(parse_vector("0.5,x").is_err());
        assert_eq!(
            parse_meta("source=notes").unwrap(),
            ("source".to_owned(), "notes".to_owned())
        );
        assert_eq!(
            parse_meta("empty=").unwrap(),
            ("empty".to_owned(), String::new())
        );
        assert!(parse_meta("novalue").is_err());
    }

    #[test]
    fn the_command_line_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        ScriptLine::command().debug_assert();
    }
}
