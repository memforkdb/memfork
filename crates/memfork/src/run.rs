//! Everything `memfork` the command does (DESIGN §5).
//!
//! This lives in the library rather than in `main.rs` because there are two
//! front doors. One is the binary. The other is the Python wheel, where
//! `pip install memfork` puts a `memfork` command on the path that calls
//! [`from_env`] through the extension module — the same code, the same
//! behaviour, no second implementation to keep in step.

use crate::{clients, daemon, doctor, init, launch, mcp, persist, proxy, serve, tools};

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use memfork_core::Db;
use serde_json::json;

use crate::cli::{split_line, Cli, Command, PersistArgs, ScriptLine};
use crate::exec::{execute, ExecError, Outcome};

/// Run the command line given to this process.
///
/// The binary calls this, and so does the Python entry point in the wheel:
/// one command surface, whichever way MemFork was installed.
pub fn from_env() -> ExitCode {
    run(Cli::parse())
}

/// Run a command line given as arguments, and return an exit status.
///
/// For the Python front door, which has an `argv` in hand and wants a number
/// back rather than a process that exits: raising `SystemExit` is the
/// interpreter's job, not this one's.
pub fn from_argv(argv: Vec<String>) -> i32 {
    match Cli::try_parse_from(argv) {
        Ok(cli) => {
            if run(cli) == ExitCode::SUCCESS {
                0
            } else {
                1
            }
        }
        // clap prints `--help` and `--version` to stdout and a usage error to
        // stderr, and each carries its own exit status. Both are ordinary
        // outcomes of a command line, not failures of this function.
        Err(e) => {
            let _ = e.print();
            i32::from(e.use_stderr())
        }
    }
}

/// Run an already-parsed command line.
pub fn run(cli: Cli) -> ExitCode {
    // `memfork mcp` must not hold a `StdoutLock`. Tokio's async stdout writes
    // from a blocking task that takes `std::io::stdout().lock()` itself, so a
    // lock held here would deadlock the server on its first response — the
    // client would see a connection that opens and then never answers. Every
    // other subcommand locks stdout for the duration of its own output.
    let result = match &cli.command {
        Command::Mcp { persist, namespace } => run_mcp(persist, namespace.as_deref()),
        // Beside `mcp` rather than inside the block below, for the same
        // reason: it holds a data directory for a long time and writes
        // nothing to stdout, so it has no business holding stdout's lock.
        Command::CrashWriter {
            persist,
            progress,
            limit,
        } => run_crash_writer(persist, progress, *limit),
        Command::Serve { persist, port } => run_serve(persist, *port),
        Command::Stop { data_dir } => run_stop(data_dir.as_deref()),
        command => {
            let mut stdout = std::io::stdout().lock();
            match command {
                Command::Run { script } => {
                    run_script(&mut stdout, script, &cli.global.branch, cli.global.json)
                }
                Command::Tools { format } => run_tools(&mut stdout, format),
                Command::Call { tool, arguments } => run_call(&mut stdout, tool, arguments),
                Command::Init {
                    dry_run,
                    client,
                    project: true,
                    all,
                    remove,
                    ..
                } => run_init_project(
                    &mut stdout,
                    ProjectInit {
                        dry_run: *dry_run,
                        clients: client,
                        all: *all,
                        remove: *remove,
                    },
                    cli.global.json,
                ),
                Command::Init {
                    dry_run,
                    client,
                    scope,
                    ..
                } => run_init(&mut stdout, *dry_run, client, scope, cli.global.json),
                Command::Doctor => run_doctor(&mut stdout, cli.global.json),
                command => {
                    let db = Db::new();
                    execute(&db, &cli.global.branch, command).and_then(|outcome| {
                        emit(&mut stdout, &outcome, cli.global.json).map_err(io_err)
                    })
                }
            }
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let mut stderr = std::io::stderr().lock();
            if cli.global.json {
                let _ = writeln!(stderr, "{}", json!({"error": e.to_string()}));
            } else {
                let _ = writeln!(stderr, "memfork: {e}");
            }
            ExitCode::FAILURE
        }
    }
}

fn io_err(e: std::io::Error) -> ExecError {
    ExecError::Usage(format!("cannot write output: {e}"))
}

fn emit(out: &mut impl Write, outcome: &Outcome, as_json: bool) -> std::io::Result<()> {
    if as_json {
        writeln!(out, "{}", outcome.json)
    } else {
        for line in &outcome.text {
            writeln!(out, "{line}")?;
        }
        Ok(())
    }
}

/// Run a batch script against one in-memory database.
///
/// Stops at the first failing line and reports which line it was, so a script
/// never half-runs without saying so.
fn run_script(
    out: &mut impl Write,
    script: &str,
    default_branch: &str,
    as_json: bool,
) -> Result<(), ExecError> {
    let source = read_script(script)?;
    let db = Db::new();
    let mut results = Vec::new();

    for (n, raw) in source.lines().enumerate() {
        let line_no = n + 1;
        let line = strip_comment(raw);
        if line.trim().is_empty() {
            continue;
        }
        let args =
            split_line(line).map_err(|e| ExecError::Usage(format!("line {line_no}: {e}")))?;
        if args.is_empty() {
            continue;
        }
        let parsed = ScriptLine::try_parse_from(&args).map_err(|e| {
            ExecError::Usage(format!("line {line_no}: {}", first_line(&e.to_string())))
        })?;
        if !parsed.command.allowed_in_script() {
            return Err(ExecError::Usage(format!(
                "line {line_no}: `{}` cannot be used inside a script",
                parsed.command.name()
            )));
        }
        let branch = parsed.global.branch.as_deref().unwrap_or(default_branch);
        let outcome = execute(&db, branch, &parsed.command)
            .map_err(|e| ExecError::Usage(format!("line {line_no}: {e}")))?;

        if as_json {
            results.push(outcome.json);
        } else {
            for l in &outcome.text {
                writeln!(out, "{l}").map_err(io_err)?;
            }
        }
    }

    if as_json {
        writeln!(out, "{}", json!(results)).map_err(io_err)?;
    }
    Ok(())
}

/// Serve MCP over stdio.
///
/// rmcp is async, so the runtime is built here rather than putting
/// `#[tokio::main]` on `main`: every other subcommand is synchronous and has
/// no use for one.
/// Open the database a persistent command should use.
///
/// Returns the database, whatever keeps it durable, and a line describing what
/// happened — printed to stderr, never stdout, because stdout may be carrying
/// the MCP protocol.
fn open_database(
    args: &PersistArgs,
) -> Result<(Db, Option<Arc<persist::Store>>, String), ExecError> {
    if args.ephemeral {
        let db = Db::new();
        apply_eviction(&db, args);
        return Ok((db, None, "in memory only; nothing will be kept".to_owned()));
    }

    let dir = match &args.data_dir {
        Some(path) => persist::DataDir {
            path: std::path::PathBuf::from(path),
            source: persist::Source::Environment,
        },
        None => persist::datadir::here().map_err(|e| ExecError::Usage(e.to_string()))?,
    };

    let fsync = persist::FsyncPolicy::parse(&args.fsync)
        .ok_or_else(|| ExecError::Usage(format!("unknown fsync policy `{}`", args.fsync)))?;
    let options = persist::Options {
        fsync,
        retention: args.retention,
    };

    let (db, store, recovery) =
        persist::Store::open(&dir.path, options).map_err(|e| ExecError::Usage(e.to_string()))?;
    apply_eviction(&db, args);

    let mut note = format!("data directory {}", dir.path.display());
    if !recovery.is_empty() {
        note.push_str(&format!(
            "; recovered {} change(s) from the snapshot and {} from the log",
            recovery.from_snapshot, recovery.from_wal
        ));
    }
    if let Some(reason) = &recovery.torn_tail {
        // Expected after a crash, and not an error: the records before the
        // torn one are whole, and the partial one never happened.
        note.push_str(&format!(
            "; discarded an incomplete final record ({reason})"
        ));
    }
    Ok((db, Some(store), note))
}

fn apply_eviction(db: &Db, args: &PersistArgs) {
    if let Some(budget) = args.memory_budget {
        db.set_eviction(Some(
            memfork_core::EvictionConfig::with_budget(budget).half_life(args.half_life),
        ));
    }
}

/// Commit in a loop until killed, for the crash-recovery tests.
///
/// Every id is appended to the progress file only *after* the commit is
/// durable, so the test can assert that everything it was told about survived.
fn run_crash_writer(args: &PersistArgs, progress: &str, limit: u64) -> Result<(), ExecError> {
    use std::io::Write as _;

    let (db, store, _) = open_database(args)?;
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(progress)
        .map_err(|e| ExecError::Usage(format!("cannot open {progress}: {e}")))?;

    for i in 0..limit {
        let id = db
            .put(
                "main",
                &format!("key:{i:08}"),
                memfork_core::Value::new(format!("value {i}")),
            )
            .map_err(|e| ExecError::Usage(e.to_string()))?;
        if let Some(store) = &store {
            store.flush().map_err(|e| ExecError::Usage(e.to_string()))?;
        }
        // Only now is the commit durable, so only now is it promised.
        writeln!(log, "{id}")
            .map_err(|e| ExecError::Usage(format!("cannot write progress: {e}")))?;
        log.flush()
            .map_err(|e| ExecError::Usage(format!("cannot flush progress: {e}")))?;
    }
    Ok(())
}

/// Resolve the data directory a persistent command should use.
fn resolve_dir(args: &PersistArgs) -> Result<std::path::PathBuf, ExecError> {
    match &args.data_dir {
        Some(path) => Ok(std::path::PathBuf::from(path)),
        None => persist::datadir::here()
            .map(|d| d.path)
            .map_err(|e| ExecError::Usage(e.to_string())),
    }
}

/// Run the shared daemon.
fn run_serve(args: &PersistArgs, port: u16) -> Result<(), ExecError> {
    if args.ephemeral {
        return Err(ExecError::Usage(
            "`memfork serve --ephemeral` would be a daemon with nothing to share.              Drop --ephemeral, or use `memfork mcp --ephemeral` for a private,              in-memory session."
                .to_owned(),
        ));
    }
    let (db, store, note) = open_database(args)?;
    let Some(store) = store else {
        return Err(ExecError::Usage(
            "the daemon needs a data directory".to_owned(),
        ));
    };
    eprintln!("memfork: {note}");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| ExecError::Usage(format!("cannot start the async runtime: {e}")))?;
    let stopped = runtime
        .block_on(serve::run(
            db,
            store,
            serve::ServeOptions {
                port,
                idle_seconds: args.idle_timeout,
            },
        ))
        .map_err(ExecError::Usage)?;
    eprintln!("memfork: daemon stopped ({stopped:?})");
    Ok(())
}

/// Stop the daemon for a data directory.
fn run_stop(data_dir: Option<&str>) -> Result<(), ExecError> {
    let dir = match data_dir {
        Some(path) => std::path::PathBuf::from(path),
        None => persist::datadir::here()
            .map(|d| d.path)
            .map_err(|e| ExecError::Usage(e.to_string()))?,
    };
    let said = daemon::stop(&dir).map_err(ExecError::Usage)?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{said}").map_err(io_err)
}

/// The namespace for a session started here, from the flag, the environment
/// or the working directory, in that order.
fn session_namespace(flag: Option<&str>) -> Result<crate::namespace::Namespace, ExecError> {
    let env = std::env::var(crate::namespace::NAMESPACE_ENV).ok();
    let cwd = std::env::current_dir()
        .map_err(|e| ExecError::Usage(format!("cannot read the working directory: {e}")))?;
    crate::namespace::resolve(flag, env.as_deref(), &cwd).map_err(ExecError::Usage)
}

fn describe_namespace(ns: &crate::namespace::Namespace) -> String {
    use crate::namespace::Source;
    let from = match ns.source {
        Source::Flag => "from --namespace",
        Source::Environment => "from MEMFORK_NAMESPACE",
        Source::Repository => "from the repository's directory name",
        Source::WorkingDirectory => "from the working directory's name",
        Source::Fallback => "nothing better could be worked out",
    };
    format!("project namespace `{}` ({from})", ns.name)
}

fn run_mcp(args: &PersistArgs, namespace_flag: Option<&str>) -> Result<(), ExecError> {
    // Diagnostics go to stderr: stdout carries the protocol, and a stray byte
    // there would corrupt it. Off unless RUST_LOG asks for it.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off")),
        )
        .try_init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| ExecError::Usage(format!("cannot start the async runtime: {e}")))?;

    // Settled first, so a bad --namespace is reported before anything starts.
    let namespace = session_namespace(namespace_flag)?;

    // Ephemeral sessions are entirely private: no daemon, no data directory,
    // nothing shared and nothing kept.
    if args.ephemeral {
        let (db, _, note) = open_database(args)?;
        eprintln!("memfork: {note}; {}", describe_namespace(&namespace));
        let session = Arc::new(tools::dispatch::Session::in_namespace(db, namespace.name));
        return runtime
            .block_on(mcp::serve_stdio(session))
            .map_err(ExecError::Usage);
    }

    // Otherwise this is a proxy. The daemon owns the data directory and the
    // log; every client is a proxy, so two clients share one memory instead of
    // the second being turned away.
    let dir = resolve_dir(args)?;
    // How to start the daemon. Not `current_exe`, which is a Python
    // interpreter when MemFork was installed from a wheel.
    let launch = launch::resolve();
    let idle = args.idle_timeout;

    let connect = {
        let dir = dir.clone();
        let launch = launch.clone();
        move |hello: proxy::Hello| {
            let dir = dir.clone();
            let launch = launch.clone();
            Box::pin(async move {
                let endpoint = tokio::task::spawn_blocking(move || {
                    daemon::ensure(&dir, &launch, idle).map_err(|e| e.to_string())
                })
                .await
                .map_err(|e| format!("the daemon lookup task failed: {e}"))??;
                proxy::Upstream::connect(&endpoint, &hello)
                    .await
                    .map_err(|e| e.to_string())
            }) as futures::future::BoxFuture<'static, Result<proxy::Upstream, String>>
        }
    };

    // One thing is worth settling before serving anything: if a daemon of
    // another version already holds this directory, say so now, on stderr,
    // where a person will read it — rather than later, as an error handed to
    // whatever model happened to call the first tool. Looking at the endpoint
    // file starts nothing.
    daemon::usable(&dir).map_err(|e| ExecError::Usage(e.to_string()))?;

    // Nothing is connected and no daemon is started here. A client that only
    // wants to shake hands and list the tools — which is how several of them
    // health-check a server — gets its answer without a daemon being started
    // on its behalf and left behind.
    eprintln!(
        "memfork: sharing the memory in {}; the daemon starts when a tool is called; {}",
        dir.display(),
        describe_namespace(&namespace)
    );

    runtime
        .block_on(proxy::serve_stdio(proxy::Proxy::new(
            Arc::new(connect),
            namespace.name,
        )))
        .map_err(ExecError::Usage)
}

/// Print the tool definitions in a vendor's function-calling format.
fn run_tools(out: &mut impl Write, format: &str) -> Result<(), ExecError> {
    let format = tools::vendor::Format::parse(format).ok_or_else(|| {
        ExecError::Usage(format!(
            "unknown format `{format}`; expected openai, anthropic or gemini"
        ))
    })?;
    let rendered = tools::vendor::render(&tools::all(), format);
    let text = serde_json::to_string_pretty(&rendered)
        .map_err(|e| ExecError::Usage(format!("cannot render the tool definitions: {e}")))?;
    writeln!(out, "{text}").map_err(io_err)
}

/// Execute one tool call against a fresh database.
///
/// Single-shot, like every other one-off subcommand: the database is created
/// for this call and discarded after it. `memfork run` is what puts several
/// operations against one in-memory database.
fn run_call(out: &mut impl Write, tool: &str, arguments: &str) -> Result<(), ExecError> {
    let parsed: serde_json::Value = serde_json::from_str(arguments)
        .map_err(|e| ExecError::Usage(format!("`arguments` is not valid JSON: {e}")))?;
    let args = match parsed {
        serde_json::Value::Object(map) => map,
        other => {
            return Err(ExecError::Usage(format!(
                "`arguments` must be a JSON object, got {other}"
            )))
        }
    };
    let session = tools::dispatch::Session::in_namespace(Db::new(), session_namespace(None)?.name);
    let result = session
        .call(tool, &args)
        .map_err(|e| ExecError::Usage(e.to_string()))?;
    let text = serde_json::to_string_pretty(&result)
        .map_err(|e| ExecError::Usage(format!("cannot render the result: {e}")))?;
    // Always JSON: a tool result is JSON by definition, so `--json` adds nothing.
    writeln!(out, "{text}").map_err(io_err)
}

/// Register the MCP server with the clients installed here.
/// What `memfork init --project` was asked to do.
struct ProjectInit<'a> {
    dry_run: bool,
    clients: &'a [String],
    all: bool,
    remove: bool,
}

/// Write, update or remove MemFork's block in this repository's instruction
/// files.
fn run_init_project(
    out: &mut impl Write,
    ask: ProjectInit<'_>,
    as_json: bool,
) -> Result<(), ExecError> {
    let cwd = std::env::current_dir()
        .map_err(|e| ExecError::Usage(format!("cannot read the working directory: {e}")))?;
    let root = crate::namespace::repository_root(&cwd).ok_or_else(|| {
        ExecError::Usage(format!(
            "{} is not inside a repository (no `.git` above it). Run this from \
             the repository whose instruction files should carry the block.",
            cwd.display()
        ))
    })?;

    // Which clients: the ones named, every one, or the ones installed here.
    let registry = init::select(ask.clients).map_err(ExecError::Usage)?;
    let chosen: Vec<clients::Client> = if ask.all || !ask.clients.is_empty() {
        registry
    } else {
        let home = clients::home_dir();
        registry
            .into_iter()
            .filter(|c| home.as_deref().is_some_and(|h| init::is_installed(c, h)))
            .collect()
    };
    let how = if ask.all {
        "every client in the registry"
    } else if !ask.clients.is_empty() {
        "the clients named"
    } else {
        "the clients installed on this machine"
    };
    if chosen.is_empty() {
        return Err(ExecError::Usage(format!(
            "no clients to write for: none of the registry's clients is installed \
             here. Name them with --client ({}), or use --all.",
            clients::ids().join(", ")
        )));
    }

    let display = |id: &str| {
        chosen
            .iter()
            .find(|c| c.id == id)
            .map_or_else(|| id.to_owned(), |c| c.display.clone())
    };
    let files: Vec<(String, Vec<String>)> = clients::instruction_files(&chosen)
        .into_iter()
        .map(|(file, ids)| (file, ids.iter().map(|id| display(id)).collect()))
        .collect();
    let plans = init::project::plan(&root, &files, ask.remove).map_err(ExecError::Usage)?;

    let mut failures = Vec::new();
    let mut records = Vec::new();
    if !as_json {
        writeln!(
            out,
            "memfork init --project{}: {} in {}, for {how}",
            if ask.dry_run { " --dry-run" } else { "" },
            if ask.remove {
                "removing the MemFork block"
            } else {
                "the MemFork block"
            },
            root.display()
        )
        .map_err(io_err)?;
    }
    for plan in &plans {
        let mut error = None;
        if !ask.dry_run {
            if let Err(e) = init::project::apply(plan) {
                failures.push(format!("{}: {e}", plan.relative));
                error = Some(e);
            }
        }
        let word = if ask.dry_run || !plan.change.writes() {
            plan.change.verb()
        } else if error.is_some() {
            "failed"
        } else {
            plan.change.done()
        };
        if as_json {
            records.push(json!({
                "file": plan.relative,
                "clients": plan.clients,
                "change": word,
                "diff": if ask.dry_run { Some(plan.diff()) } else { None },
                "left_empty": plan.left_empty(),
                "error": error,
            }));
        } else {
            writeln!(
                out,
                "  {:<12} {:<14} read by {}",
                plan.relative,
                word,
                plan.clients.join(", ")
            )
            .map_err(io_err)?;
            if ask.dry_run && plan.change.writes() {
                for line in plan.diff().lines() {
                    writeln!(out, "      {line}").map_err(io_err)?;
                }
            }
            if plan.left_empty() && !ask.dry_run {
                writeln!(
                    out,
                    "      note: {} now holds nothing else; delete it if you do not need it",
                    plan.relative
                )
                .map_err(io_err)?;
            }
            if let Some(e) = &error {
                writeln!(out, "      error: {e}").map_err(io_err)?;
            }
        }
    }
    if as_json {
        writeln!(
            out,
            "{}",
            json!({
                "root": root.display().to_string(),
                "dry_run": ask.dry_run,
                "remove": ask.remove,
                "chosen": chosen.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
                "files": records,
            })
        )
        .map_err(io_err)?;
    } else if ask.dry_run {
        writeln!(out, "Nothing was written (--dry-run).").map_err(io_err)?;
    } else {
        writeln!(
            out,
            "Only the lines between the MemFork markers were touched. Nothing was \
             committed; review and commit the files as you would any other change."
        )
        .map_err(io_err)?;
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ExecError::Usage(failures.join("; ")))
    }
}

fn run_init(
    out: &mut impl Write,
    dry_run: bool,
    client: &[String],
    scope: &str,
    as_json: bool,
) -> Result<(), ExecError> {
    let scope = clients::Scope::parse(scope)
        .ok_or_else(|| ExecError::Usage(format!("unknown scope `{scope}`")))?;
    let launch = launch::resolve();
    let plans = init::plan(scope, client, None, &launch).map_err(ExecError::Usage)?;

    let mut changed = 0usize;
    let mut failures = Vec::new();
    let mut records = Vec::new();

    for plan in &plans {
        let (verb, detail) = describe(&plan.action, dry_run);
        let mut applied_error = None;
        let mut backup = None;

        if !dry_run && plan.action.changes_anything() {
            match init::apply(plan) {
                Ok(b) => {
                    changed += 1;
                    backup = b;
                }
                Err(e) => {
                    failures.push(format!("{}: {e}", plan.display));
                    applied_error = Some(e);
                }
            }
        }

        if as_json {
            records.push(json!({
                "id": plan.id,
                "client": plan.display,
                "scope": plan.scope.as_str(),
                "method": method_name(&plan.action),
                "action": verb,
                "detail": detail,
                "note": plan.note,
                "backup": backup,
                "error": applied_error,
            }));
        } else {
            let done = if dry_run || !plan.action.changes_anything() {
                verb
            } else if applied_error.is_some() {
                "failed"
            } else {
                past_tense(verb)
            };
            writeln!(out, "  {:<14} {:<18} {detail}", plan.display, done).map_err(io_err)?;
            if let Some(note) = &plan.note {
                writeln!(out, "      note: {note}").map_err(io_err)?;
            }
            if let Some(e) = &applied_error {
                writeln!(out, "      error: {e}").map_err(io_err)?;
            }
            if let Some(b) = &backup {
                writeln!(out, "      backup: {b}").map_err(io_err)?;
            }
            if dry_run {
                if let init::Action::WriteFile(edit) = &plan.action {
                    write!(out, "{}", edit.diff()).map_err(io_err)?;
                }
            }
        }
    }

    if as_json {
        let doc = json!({
            "scope": scope.as_str(),
            "dry_run": dry_run,
            "command": launch.program.display().to_string(),
            // What a client is actually told to run, which in a wheel install
            // is an interpreter and a module rather than a path on its own.
            // Both forms: one to read, one to run. Quoting a command line and
            // taking it apart again is a way to get it wrong.
            "command_line": launch.display(clients::SERVER_ARGS),
            "command_argv": launch.argv(clients::SERVER_ARGS),
            "clients": records,
        });
        writeln!(out, "{doc}").map_err(io_err)?;
    } else if dry_run {
        writeln!(out, "\nNothing was run or written: this was a dry run.").map_err(io_err)?;
    } else if changed == 0 {
        writeln!(out, "\nNothing to change.").map_err(io_err)?;
    } else {
        writeln!(
            out,
            "\nRegistered with {changed} client(s). Restart them to pick up the tools."
        )
        .map_err(io_err)?;
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(ExecError::Usage(failures.join("; ")))
    }
}

/// Which of the two registration methods an action uses, for the report.
fn method_name(action: &init::Action) -> &'static str {
    match action {
        init::Action::RunCommand { .. } | init::Action::ReplaceCommand { .. } => "client command",
        init::Action::WriteFile(_) | init::Action::AlreadyRegistered { .. } => "config file",
        init::Action::StaleElsewhere { .. }
        | init::Action::NotInstalled
        | init::Action::Refused(_) => "none",
    }
}

/// A short verb for what will happen, and the detail line beside it.
fn describe(action: &init::Action, dry_run: bool) -> (&'static str, String) {
    match action {
        init::Action::RunCommand { binary, args, .. } => (
            if dry_run { "would run" } else { "run" },
            format!("{binary} {}", args.join(" ")),
        ),
        init::Action::ReplaceCommand {
            binary, add, was, ..
        } => (
            if dry_run { "would repoint" } else { "repoint" },
            format!(
                "was {}; now {binary} {}",
                was.as_deref().unwrap_or("another MemFork"),
                add.join(" ")
            ),
        ),
        init::Action::StaleElsewhere { was, fix } => (
            "points elsewhere",
            format!("at {}; {fix}", was.as_deref().unwrap_or("another MemFork")),
        ),
        init::Action::WriteFile(edit) => (
            match (dry_run, edit.change) {
                (true, clients::edit::Change::Update) => "would update",
                (true, _) => "would add",
                (false, clients::edit::Change::Update) => "update",
                (false, _) => "add",
            },
            edit.path.display().to_string(),
        ),
        init::Action::AlreadyRegistered { checked } => (
            "already registered",
            match checked {
                clients::Checked::Command(cmd) => format!("`{cmd}` says so"),
                clients::Checked::File(path) => path.display().to_string(),
                clients::Checked::Nothing => String::new(),
            },
        ),
        init::Action::NotInstalled => ("not installed", String::new()),
        init::Action::Refused(reason) => ("skipped", reason.clone()),
    }
}

fn past_tense(verb: &str) -> &'static str {
    match verb {
        "run" => "ran",
        "add" => "added",
        "update" => "updated",
        "repoint" => "repointed",
        _ => "done",
    }
}

/// Report what this install is and what it is talking to.
fn run_doctor(out: &mut impl Write, as_json: bool) -> Result<(), ExecError> {
    if as_json {
        writeln!(out, "{}", doctor::json()).map_err(io_err)
    } else {
        write!(out, "{}", doctor::text()).map_err(io_err)
    }
}

fn read_script(script: &str) -> Result<String, ExecError> {
    if script == "-" {
        if std::io::stdin().is_terminal() {
            return Err(ExecError::Usage(
                "reading a script from a terminal: pipe one in, or pass a file path".to_owned(),
            ));
        }
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| ExecError::Usage(format!("cannot read standard input: {e}")))?;
        Ok(buf)
    } else {
        std::fs::read_to_string(script)
            .map_err(|e| ExecError::Usage(format!("cannot read `{script}`: {e}")))
    }
}

/// Strip a `#` comment, unless the `#` is inside a quoted string.
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, c) {
            (Some('"'), '\\') => escaped = true,
            (Some(q), ch) if ch == q => quote = None,
            (Some(_), _) => {}
            (None, '\'') | (None, '"') => quote = Some(c),
            (None, '\\') => escaped = true,
            (None, '#') => return &line[..i],
            (None, _) => {}
        }
    }
    line
}

/// clap renders multi-line errors; a script line wants the first line of it.
fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_are_stripped_outside_quotes() {
        assert_eq!(strip_comment("put a b # trailing"), "put a b ");
        assert_eq!(strip_comment("# whole line"), "");
        assert_eq!(
            strip_comment(r#"put a "not # a comment""#),
            r#"put a "not # a comment""#
        );
        assert_eq!(strip_comment("put a b"), "put a b");
    }

    #[test]
    fn first_line_of_a_clap_error() {
        assert_eq!(first_line("error: bad\n\nUsage: x"), "error: bad");
    }
}
