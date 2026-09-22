//! Command-line surface over the engine (DESIGN §5).

use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};

/// Git for agent memory, shared by every AI tool on your machine.
#[derive(Debug, Parser)]
#[command(
    name = "memfork",
    version,
    about = "Git for agent memory, shared by every AI tool on your machine: fork, merge, rewind, and hand work from one agent to another.",
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

    /// Print machine-readable JSON instead of text. Never coloured.
    #[arg(long, global = true)]
    pub json: bool,

    /// When to use colour: `auto` for a terminal that wants it, `always` even
    /// when piped, in CI or with NO_COLOR set, `never`. `--json` and
    /// `memfork mcp` are never coloured.
    #[arg(long, global = true, value_name = "WHEN", default_value = "auto",
          value_parser = ["auto", "always", "never"])]
    pub color: String,

    /// Keep nothing and share nothing: work on a fresh in-memory database, with
    /// no daemon and no data directory. Without it, `put`, `get` and the other
    /// operations work on the shared store through the daemon, starting it if
    /// need be.
    #[arg(long, global = true)]
    pub ephemeral: bool,

    /// Where the data is kept. Defaults to a per-user directory, or to
    /// `./.memfork` if that directory already exists.
    #[arg(long, global = true, value_name = "PATH")]
    pub data_dir: Option<String>,
}

/// Where a persistent command keeps its data, and how carefully.
#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct PersistArgs {
    /// Keep nothing: run entirely in memory and forget it all on exit.
    ///
    /// The opposite of the library default. `memfork-core` is in-memory
    /// unless a caller asks for durability; the binary persists unless told
    /// not to, because an agent's memory that empties on restart is not
    /// memory. Filled from the global `--ephemeral`.
    #[arg(skip)]
    pub ephemeral: bool,

    /// Where to keep the data. Filled from the global `--data-dir`.
    #[arg(skip)]
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
#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
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
        /// A file this finding came from, relative to the project; repeat for
        /// several. Makes the entry a fact that says when those files change.
        #[arg(long = "source", value_name = "PATH")]
        sources: Vec<String>,
        /// The sources' hashes, worked out by the command line before the
        /// command is sent: the daemon has no working directory to read them
        /// from.
        #[arg(skip)]
        #[serde(default)]
        source_hashes: Option<std::collections::BTreeMap<String, Option<String>>>,
        /// Write it even though it looks like a credential: the rule id the
        /// refusal named. Only for something that is not a secret.
        #[arg(long = "allow-secret", value_name = "RULE")]
        allow_secret: Option<String>,
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
        /// On a terminal, print every value whole rather than cut to the
        /// terminal's width. Output to a pipe or a file is always whole.
        #[arg(long)]
        full: bool,
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
        /// One line on what was learned, kept on the branch it was forked
        /// from after everything else on this one is gone.
        #[arg(long)]
        lesson: Option<String>,
        /// Write it even though it looks like a credential: the rule id the
        /// refusal named. Only for something that is not a secret.
        #[arg(long = "allow-secret", value_name = "RULE")]
        allow_secret: Option<String>,
    },

    /// Find entries by words in their keys and values, best match first.
    Find {
        /// The words to look for.
        text: String,
        /// How many results, at most 50.
        #[arg(long, default_value_t = 10)]
        k: usize,
        /// Only look at keys starting with this.
        #[arg(long)]
        prefix: Option<String>,
    },

    /// Share out work: add, claim, release and finish tasks.
    Task {
        /// What to do.
        #[command(subcommand)]
        action: TaskAction,
        /// The project. Defaults to the one this directory belongs to.
        #[arg(long, global = true, value_name = "NAME")]
        namespace: Option<String>,
        /// Write it even though it looks like a credential: the rule id the
        /// refusal named. Only for something that is not a secret.
        #[arg(long = "allow-secret", global = true, value_name = "RULE")]
        allow_secret: Option<String>,
    },

    /// Plans: tasks with dependencies and acceptance commands, from a file.
    Plan {
        /// What to do.
        #[command(subcommand)]
        action: PlanAction,
        /// The project. Defaults to the one this directory belongs to.
        #[arg(long, global = true, value_name = "NAME")]
        namespace: Option<String>,
        /// Write it even though something in it looks like a credential: the
        /// rule id the refusal named. Only for something that is not a secret.
        #[arg(long = "allow-secret", global = true, value_name = "RULE")]
        allow_secret: Option<String>,
    },

    /// List the facts in a project, and whether their source files have
    /// changed since each was written.
    Facts {
        /// Only keys starting with this. Defaults to the project's own.
        prefix: Option<String>,
        /// The project. Defaults to the one this directory belongs to.
        #[arg(long, value_name = "NAME")]
        namespace: Option<String>,
    },

    /// Maintenance tasks: whether MemFork may add them to a project when its
    /// memory needs tidying.
    Maintain {
        /// on, off, or status.
        #[arg(value_parser = ["on", "off", "status"])]
        setting: String,
        /// The project. Defaults to the one this directory belongs to.
        #[arg(long, value_name = "NAME")]
        namespace: Option<String>,
    },

    /// Duplicates and contradictions in a project worth a look: decisions
    /// made differently on different branches, the same value under
    /// near-identical keys, facts from the same files that disagree. MemFork
    /// never fixes these itself.
    Flags {
        /// The project. Defaults to the one this directory belongs to.
        #[arg(long, value_name = "NAME")]
        namespace: Option<String>,
    },

    /// The lessons left by discarded attempts in a project, newest first.
    Lessons {
        /// The project. Defaults to the one this directory belongs to.
        #[arg(long, value_name = "NAME")]
        namespace: Option<String>,
    },

    /// What MemFork saved and served: briefings and their size, lessons,
    /// facts found fresh or stale, claims. Bytes and approximate tokens only.
    Stats {
        /// Only this project.
        #[arg(long, value_name = "NAME")]
        project: Option<String>,
    },

    /// List branches.
    Branches,

    /// Show a branch's history, newest first.
    Log {
        /// Stop after this many commits.
        #[arg(long)]
        limit: Option<usize>,
        /// Draw every branch as a tree: forks, merges and discarded
        /// attempts, newest first.
        #[arg(long)]
        graph: bool,
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
        /// On a terminal, print every listed value whole rather than cut to
        /// the terminal's width. Output to a pipe or a file is always whole.
        #[arg(long)]
        full: bool,
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

        /// The project namespace this session works in. Defaults to
        /// MEMFORK_NAMESPACE, else the repository's top-level directory name,
        /// else the working directory's name. Lowercase letters, digits, `.`,
        /// `_` and `-`.
        #[arg(long, value_name = "NAME")]
        namespace: Option<String>,
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
        /// Only configure this client, by registry id. Repeat for several.
        #[arg(long, value_name = "ID")]
        client: Vec<String>,
        /// Register for this user everywhere, or only for this project.
        #[arg(long, default_value = "user", value_parser = ["user", "project"])]
        scope: String,
        /// Instead of registering, write MemFork's instruction block into
        /// the instruction files of this repository's clients. Run inside a
        /// repository. Only the block is ever touched.
        #[arg(long, conflicts_with = "scope")]
        project: bool,
        /// With --project: every client in the registry, installed or not,
        /// for a repository shared by people using different tools.
        #[arg(long, requires = "project", conflicts_with = "client")]
        all: bool,
        /// With --project: take the block out again, and nothing else.
        #[arg(long, requires = "project")]
        remove: bool,
    },

    /// Report version, paths, the daemon, the policy in force and which
    /// clients know about MemFork. Short by default; `--verbose` has it all.
    Doctor {
        /// The full report: how each client was asked, every config path,
        /// where each registry entry was verified, and the persistence note.
        #[arg(long)]
        verbose: bool,
    },

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
        /// Forget a client session after this many seconds without a
        /// request. For tests; the default suits everything else.
        #[arg(long, hide = true, value_name = "SECONDS",
              default_value_t = crate::serve::DEFAULT_SESSION_SECONDS)]
        session_timeout: u64,
    },

    /// Stop the daemon for a data directory, gracefully.
    ///
    /// It flushes, releases the directory and removes its endpoint file. Use
    /// this before upgrading, so the new version starts its own daemon rather
    /// than refusing to talk to the old one.
    Stop,

    /// Print a completion script for a shell, to source or install.
    ///
    /// For example `memfork completions bash > ~/.local/share/bash-completion/completions/memfork`,
    /// `memfork completions zsh > "${fpath[1]}/_memfork"`,
    /// `memfork completions fish > ~/.config/fish/completions/memfork.fish`,
    /// or in PowerShell `memfork completions powershell | Out-String | Invoke-Expression`.
    Completions {
        /// The shell: bash, zsh, fish, powershell or elvish.
        #[arg(value_parser = ["bash", "zsh", "fish", "powershell", "elvish"])]
        shell: String,
    },

    /// Show what the daemon is doing as it happens: which client did what,
    /// to which key or branch, including handoffs and resumes.
    ///
    /// Runs until interrupted, waiting for a daemon if none is running.
    /// `--json` prints one JSON object per line.
    Watch {
        /// Stop after this many events.
        #[arg(long, value_name = "N")]
        count: Option<usize>,
    },

    /// Open the Brain: a read-only page, served by the daemon on this
    /// machine only, that shows the memory graph and what the engine did
    /// with it. Starts the daemon if none is running.
    ///
    /// The address it prints carries the daemon's read token after the `#`,
    /// so the page can read and nothing more. The token dies with the
    /// daemon; run this again for a fresh link.
    Brain {
        /// Print the address and do not open a browser.
        #[arg(long)]
        no_open: bool,
    },
}

/// What `memfork plan` does.
#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
pub enum PlanAction {
    /// Put a plan file's tasks on the board, in one step. A task already
    /// there is replaced only while it is open and unclaimed.
    Write {
        /// The plan file. Defaults to memfork-plan.toml at the top of the
        /// project.
        file: Option<String>,
        /// The file's tasks, read here before the command is sent.
        #[arg(skip)]
        #[serde(default)]
        tasks: Option<Vec<crate::plans::PlanTask>>,
        /// Where the file is, relative to the project.
        #[arg(skip)]
        #[serde(default)]
        plan_file: Option<String>,
    },
    /// Check a plan file without writing anything: its shape, its ids, and
    /// that it has no cycle.
    Check {
        /// The plan file. Defaults to memfork-plan.toml at the top of the
        /// project.
        file: Option<String>,
    },
    /// Show the board as a plan: what is ready, what is blocked and by what,
    /// who holds what, and what is done.
    Show,
    /// Start a plan file from a template, to fill in and then write.
    New {
        /// Which template; `memfork plan templates` lists them.
        #[arg(long)]
        template: String,
        /// Where to write it. Defaults to memfork-plan.toml at the top of
        /// the project.
        file: Option<String>,
        /// Replace a file that is already there.
        #[arg(long)]
        force: bool,
    },
    /// List the templates `memfork plan new` can start from: the built-in
    /// ones and any of your own in the data directory's `plans` folder.
    Templates,
}

/// What `memfork task` does.
#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
pub enum TaskAction {
    /// Add a task.
    Add {
        /// What the task is, in a line.
        title: String,
        /// Its id. Leave it out for the next number.
        #[arg(long)]
        id: Option<String>,
        /// Anything more it needs.
        #[arg(long)]
        detail: Option<String>,
        /// A task that must be done first; repeat for several.
        #[arg(long = "depends-on", value_name = "ID")]
        depends_on: Vec<String>,
        /// A command that exits 0 in the project when the task is done. It
        /// runs only if the repository's plan file holds the same command.
        #[arg(long, value_name = "COMMAND")]
        accept: Option<String>,
        /// How long that command may take, in seconds (at most 3600).
        #[arg(long = "timeout", value_name = "SECONDS")]
        timeout_seconds: Option<u64>,
    },
    /// Claim a task, so nobody else starts it.
    Claim {
        /// The task's id.
        id: String,
        /// How long the claim lasts, in seconds.
        #[arg(long, default_value_t = crate::board::DEFAULT_LEASE_SECONDS)]
        lease: u64,
    },
    /// Keep a claim alive.
    Renew {
        /// The task's id.
        id: String,
    },
    /// Give a claimed task back, still open.
    Release {
        /// The task's id.
        id: String,
    },
    /// Mark a task done. A task with an acceptance command is done only
    /// when that command exits 0 here; if it fails the task is reopened and
    /// a lesson recorded.
    Done {
        /// The task's id.
        id: String,
        /// For a maintenance task: the fork the work was done on, to be
        /// checked and merged.
        #[arg(long, value_name = "BRANCH")]
        fork: Option<String>,
        /// The acceptance command's result, worked out here before the
        /// command is sent, since the daemon cannot see the project.
        #[arg(skip)]
        #[serde(default)]
        acceptance: Option<crate::plans::Acceptance>,
    },
    /// List tasks.
    List {
        /// Which: ready, blocked, open, claimed, done, unfinished or all.
        #[arg(long, default_value = "unfinished",
              value_parser = ["ready", "blocked", "open", "claimed", "done", "unfinished", "all"])]
        status: String,
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
                | Command::Stop
                | Command::Watch { .. }
                | Command::Brain { .. }
                | Command::Init { .. }
                | Command::Doctor { .. }
                | Command::Completions { .. }
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
            Command::Find { .. } => "find",
            Command::Task { .. } => "task",
            Command::Plan { .. } => "plan",
            Command::Facts { .. } => "facts",
            Command::Lessons { .. } => "lessons",
            Command::Flags { .. } => "flags",
            Command::Maintain { .. } => "maintain",
            Command::Stats { .. } => "stats",
            Command::Run { .. } => "run",
            Command::Mcp { .. } => "mcp",
            Command::Tools { .. } => "tools",
            Command::Call { .. } => "call",
            Command::Serve { .. } => "serve",
            Command::Init { .. } => "init",
            Command::Doctor { .. } => "doctor",
            Command::Completions { .. } => "completions",
            Command::CrashWriter { .. } => "crash-writer",
            Command::Stop => "stop",
            Command::Watch { .. } => "watch",
            Command::Brain { .. } => "brain",
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
