//! Everything `memfork` the command does (DESIGN §5).
//!
//! This lives in the library rather than in `main.rs` because there are two
//! front doors. One is the binary. The other is the Python wheel, where
//! `pip install memfork` puts a `memfork` command on the path that calls
//! [`from_env`] through the extension module — the same code, the same
//! behaviour, no second implementation to keep in step.

use crate::style::{Channel, ColorChoice, Spinner, Style};
use crate::{
    client, clients, daemon, doctor, init, launch, mcp, persist, proxy, render, serve, tools,
};

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use memfork_core::Db;
use serde_json::json;

use crate::cli::{
    split_line, Cli, Command, GlobalArgs, PersistArgs, PlanAction, ScriptLine, TaskAction,
};
use crate::exec::{execute_in, ExecError, Outcome};

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
    let choice = ColorChoice::parse(&cli.global.color).unwrap_or_default();
    let channel = if cli.global.json {
        Channel::Json
    } else {
        Channel::Human
    };
    let recorded = match &cli.command {
        Command::Mcp { .. } | Command::Serve { .. } | Command::CrashWriter { .. } => {
            Channel::Protocol
        }
        _ => channel,
    };
    crate::style::set_preferences(choice, recorded);
    let persist = |p: &PersistArgs| PersistArgs {
        ephemeral: cli.global.ephemeral,
        data_dir: cli.global.data_dir.clone(),
        ..p.clone()
    };
    let result = match &cli.command {
        Command::Mcp {
            persist: p,
            namespace,
        } => run_mcp(&persist(p), namespace.as_deref()),
        // Beside `mcp` rather than inside the block below, for the same
        // reason: it holds a data directory for a long time and writes
        // nothing to stdout, so it has no business holding stdout's lock.
        Command::CrashWriter {
            persist: p,
            progress,
            limit,
        } => run_crash_writer(&persist(p), progress, *limit),
        Command::Serve {
            persist: p,
            port,
            session_timeout,
        } => run_serve(&persist(p), *port, *session_timeout),
        Command::Stop => run_stop(cli.global.data_dir.as_deref()),
        command => {
            let mut stdout = std::io::stdout().lock();
            let style = Style::for_stdout(choice, channel);
            match command {
                Command::Run { script } => run_script(
                    &mut stdout,
                    script,
                    &cli.global.branch,
                    cli.global.json,
                    &style,
                ),
                Command::Watch { count } => run_watch(&mut stdout, &cli.global, *count, &style),
                Command::Tools { format } => run_tools(&mut stdout, format),
                Command::Call { tool, arguments } => {
                    run_call(&mut stdout, tool, arguments, &cli.global, choice)
                }
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
                Command::Plan {
                    action: PlanAction::Templates,
                    ..
                } => run_plan_templates(&mut stdout, &cli.global),
                Command::Plan {
                    action:
                        PlanAction::New {
                            template,
                            file,
                            force,
                        },
                    ..
                } => run_plan_new(&mut stdout, &cli.global, template, file.as_deref(), *force),
                Command::Plan {
                    action: PlanAction::Check { file },
                    allow_secret,
                    ..
                } => run_plan_check(
                    &mut stdout,
                    file.as_deref(),
                    allow_secret.as_deref(),
                    cli.global.json,
                ),
                // One operation. In memory, alone, with --ephemeral; on the
                // shared store through the daemon otherwise.
                command if cli.global.ephemeral => {
                    let db = Db::new();
                    let mut command = command.clone();
                    prepare_here(&mut command).and_then(|()| {
                        execute_in(&db, &cli.global.branch, &command, &local_context()).and_then(
                            |outcome| {
                                emit(&mut stdout, &command, &outcome, cli.global.json, &style)
                                    .map_err(io_err)
                            },
                        )
                    })
                }
                command => run_op(&mut stdout, command, &cli.global, choice, &style),
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

fn emit(
    out: &mut impl Write,
    command: &Command,
    outcome: &Outcome,
    as_json: bool,
    style: &Style,
) -> std::io::Result<()> {
    if as_json {
        writeln!(out, "{}", outcome.json)
    } else {
        for line in render::lines(command, outcome, style) {
            writeln!(out, "{line}")?;
        }
        Ok(())
    }
}

/// The data directory the global `--data-dir` names, or the usual one.
fn data_dir(global: &GlobalArgs) -> Result<std::path::PathBuf, ExecError> {
    match &global.data_dir {
        Some(path) => Ok(std::path::PathBuf::from(path)),
        None => persist::datadir::here()
            .map(|d| d.path)
            .map_err(|e| ExecError::Usage(e.to_string())),
    }
}

/// The daemon for `dir`, starting it — which replays the log, so it can take a
/// moment — if none is running.
fn daemon_for(dir: &std::path::Path, choice: ColorChoice) -> Result<persist::Endpoint, ExecError> {
    if let Some(running) = daemon::usable(dir).map_err(|e| ExecError::Usage(e.to_string()))? {
        return Ok(running);
    }
    let spinner = Spinner::start(
        choice,
        Channel::Human,
        "starting the MemFork daemon and reading the store",
    );
    daemon::ensure_reporting(
        dir,
        &launch::resolve(),
        serve::DEFAULT_IDLE_SECONDS,
        &|waited| spinner.update(&daemon::still_starting(waited)),
    )
    .map_err(|e| ExecError::Usage(e.to_string()))
}

fn runtime() -> Result<tokio::runtime::Runtime, ExecError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| ExecError::Usage(format!("cannot start the async runtime: {e}")))
}

/// One operation on the shared store, carried out by the daemon.
fn run_op(
    out: &mut impl Write,
    command: &Command,
    global: &GlobalArgs,
    choice: ColorChoice,
    style: &Style,
) -> Result<(), ExecError> {
    let dir = data_dir(global)?;
    let endpoint = daemon_for(&dir, choice)?;
    let daemon = client::Daemon::new(&endpoint).map_err(ExecError::Usage)?;
    let namespace = session_namespace(None).ok().map(|n| n.name);
    let root = project_root();
    let hasher = crate::facts::Hasher::default();
    // A fact's sources are hashed here, where the files are; the daemon has no
    // working directory to read them from.
    let mut command = command.clone();
    if let Command::Put {
        sources,
        source_hashes,
        ..
    } = &mut command
    {
        if !sources.is_empty() {
            let clean = crate::facts::normalise(sources).map_err(ExecError::Usage)?;
            *source_hashes = Some(hasher.record(&root, &clean));
        }
    }
    prepare_here(&mut command)?;
    // A task with an acceptance command is done only if that command passes,
    // and it runs here, where the project is.
    if let Command::Task {
        action: TaskAction::Done { id, acceptance },
        namespace: named,
        ..
    } = &mut command
    {
        let ns = named
            .clone()
            .or_else(|| namespace.clone())
            .unwrap_or_else(|| crate::namespace::FALLBACK.to_owned());
        let get = json!({
            "branch": global.branch,
            "command": Command::Get { key: crate::board::task_key(&ns, id) },
            "namespace": namespace,
        });
        let (status, answer) = runtime()?
            .block_on(daemon.post(serve::CLI_PATH, &get))
            .map_err(ExecError::Usage)?;
        let task = (status == 200)
            .then(|| answer["json"]["value"].as_str())
            .flatten()
            .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok());
        if let Some(task) = task {
            *acceptance = crate::plans::prepare_done(&root, id, &task).map_err(ExecError::Usage)?;
        }
    }
    let request = json!({
        "branch": global.branch,
        "command": command,
        "namespace": namespace,
    });
    let (status, answer) = runtime()?
        .block_on(daemon.post(serve::CLI_PATH, &request))
        .map_err(ExecError::Usage)?;
    if status != 200 {
        return Err(ExecError::Usage(
            answer["error"]
                .as_str()
                .unwrap_or("the daemon refused the operation")
                .to_owned(),
        ));
    }
    let outcome = Outcome {
        text: answer["text"]
            .as_array()
            .map(|lines| {
                lines
                    .iter()
                    .filter_map(|l| l.as_str())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        json: answer["json"].clone(),
    };
    let mut outcome = outcome;
    let checked = crate::facts::check(&mut outcome.json, &root, &hasher);
    if !checked.is_empty() {
        let body = json!({
            "client": serve::CLI_WRITER,
            "namespace": namespace,
            "facts": checked.facts.iter().map(|(k, s)| json!([k, s])).collect::<Vec<_>>(),
        });
        let _ = runtime()?.block_on(daemon.post(serve::REPORT_PATH, &body));
    }
    emit(out, &command, &outcome, global.json, style).map_err(io_err)
}

/// What a command needs from the files here before it goes anywhere: a plan
/// file, read and checked where it is.
fn prepare_here(command: &mut Command) -> Result<(), ExecError> {
    if let Command::Plan {
        action:
            PlanAction::Write {
                file,
                tasks,
                plan_file,
            },
        allow_secret,
        ..
    } = command
    {
        let read = read_plan(file.as_deref(), allow_secret.as_deref())?;
        *tasks = Some(read.tasks);
        *plan_file = read.relative;
    }
    Ok(())
}

/// A plan file, as `memfork plan` reads it.
struct ReadPlan {
    tasks: Vec<crate::plans::PlanTask>,
    /// Where it is, relative to the project, if it is inside it.
    relative: Option<String>,
    /// How to name it to a person.
    shown: String,
}

/// Read a plan file: checked for credentials as a whole, so a refusal names
/// the line in the file, then parsed. A plan with acceptance commands must be
/// inside the project, since the file is what makes those commands trusted.
fn read_plan(file: Option<&str>, allow: Option<&str>) -> Result<ReadPlan, ExecError> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let root = project_root();
    let path = match file {
        Some(f) => lexical(&cwd.join(f)),
        None => root.join(crate::plans::DEFAULT_FILE),
    };
    let relative = path.strip_prefix(&root).ok().map(|rest| {
        rest.components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/")
    });
    let shown = relative
        .clone()
        .unwrap_or_else(|| path.display().to_string());
    let text = crate::plans::read_file(&path).map_err(|e| {
        ExecError::Usage(match file {
            None => format!(
                "{e}; write one, or start from a template with `memfork plan new --template <name>`"
            ),
            Some(_) => e,
        })
    })?;
    let allow = crate::secrets::Allow::parse(allow).map_err(|r| ExecError::Usage(r.to_string()))?;
    crate::secrets::check(&format!("plan file {shown}"), &text, &allow)
        .map_err(|r| ExecError::Usage(r.to_string()))?;
    let tasks =
        crate::plans::parse(&text).map_err(|e| ExecError::Usage(format!("{shown}: {e}")))?;
    if relative.is_none() && tasks.iter().any(|t| t.accept.is_some()) {
        return Err(ExecError::Usage(format!(
            "{shown} is outside this project ({}), and it has acceptance commands, which run \
             only from a plan file in the project; move it inside",
            root.display()
        )));
    }
    Ok(ReadPlan {
        tasks,
        relative,
        shown,
    })
}

/// A path with its `.` and `..` parts worked out, without touching the disk.
fn lexical(path: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `memfork plan templates`.
fn run_plan_templates(out: &mut impl Write, global: &GlobalArgs) -> Result<(), ExecError> {
    let all = crate::plans::templates(data_dir(global).ok().as_deref());
    if global.json {
        let list: Vec<serde_json::Value> = all
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "source": t.source,
                    "problem": t.problem,
                })
            })
            .collect();
        writeln!(
            out,
            "{}",
            json!({"op": "plan templates", "templates": list})
        )
        .map_err(io_err)?;
        return Ok(());
    }
    let width = all
        .iter()
        .map(|t| t.name.chars().count())
        .max()
        .unwrap_or(0);
    for t in &all {
        let pad = " ".repeat(width - t.name.chars().count());
        let line = match (&t.problem, t.source.as_str()) {
            (Some(problem), source) => {
                format!("{}{pad}  cannot be used: {problem} ({source})", t.name)
            }
            (None, "built-in") => format!("{}{pad}  {}", t.name, t.description),
            (None, source) => format!("{}{pad}  {}  ({source})", t.name, t.description),
        };
        writeln!(out, "{line}").map_err(io_err)?;
    }
    writeln!(
        out,
        "start one with `memfork plan new --template <name>`; add your own as .toml files in {}",
        data_dir(global)
            .map(|d| d.join(crate::plans::TEMPLATES_DIR).display().to_string())
            .unwrap_or_else(|_| "the data directory's plans folder".to_owned())
    )
    .map_err(io_err)
}

/// `memfork plan new`: a template, written as a plan file to fill in.
fn run_plan_new(
    out: &mut impl Write,
    global: &GlobalArgs,
    name: &str,
    file: Option<&str>,
    force: bool,
) -> Result<(), ExecError> {
    let all = crate::plans::templates(data_dir(global).ok().as_deref());
    let Some(template) = all.iter().find(|t| t.name == name) else {
        return Err(ExecError::Usage(format!(
            "there is no template `{name}`; the templates are: {}",
            all.iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    };
    if let Some(problem) = &template.problem {
        return Err(ExecError::Usage(format!(
            "the template `{name}` cannot be used: {problem} ({})",
            template.source
        )));
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let path = match file {
        Some(f) => lexical(&cwd.join(f)),
        None => project_root().join(crate::plans::DEFAULT_FILE),
    };
    if path.exists() && !force {
        return Err(ExecError::Usage(format!(
            "{} is already there; write elsewhere, or pass --force to replace it",
            path.display()
        )));
    }
    std::fs::write(&path, &template.text)
        .map_err(|e| ExecError::Usage(format!("cannot write {}: {e}", path.display())))?;
    let tasks = crate::plans::parse(&template.text)
        .map(|t| t.len())
        .unwrap_or(0);
    if global.json {
        writeln!(
            out,
            "{}",
            json!({"op": "plan new", "template": name, "file": path.display().to_string(), "tasks": tasks})
        )
        .map_err(io_err)?;
        return Ok(());
    }
    writeln!(
        out,
        "wrote {} from the `{name}` template: {tasks} tasks\n  fill in each empty `accept` with a command that exits 0 when that task is done, then run `memfork plan write`",
        path.display()
    )
    .map_err(io_err)
}

/// `memfork plan check`: a plan file on its own, written nowhere.
fn run_plan_check(
    out: &mut impl Write,
    file: Option<&str>,
    allow: Option<&str>,
    json_out: bool,
) -> Result<(), ExecError> {
    let read = read_plan(file, allow)?;
    let outside = crate::plans::check_alone(&read.tasks)
        .map_err(|e| ExecError::Usage(format!("{}: {e}", read.shown)))?;
    let first: Vec<&str> = read
        .tasks
        .iter()
        .filter(|t| t.depends_on.is_empty())
        .map(|t| t.id.as_str())
        .collect();
    let with_accept = read.tasks.iter().filter(|t| t.accept.is_some()).count();
    if json_out {
        let answer = json!({
            "op": "plan check",
            "file": read.shown,
            "ok": true,
            "tasks": read.tasks.len(),
            "ready_first": first,
            "with_acceptance": with_accept,
            "outside": outside,
        });
        writeln!(out, "{answer}").map_err(io_err)?;
        return Ok(());
    }
    let n = read.tasks.len();
    let mut lines = vec![
        format!(
            "{}: {n} task{}, no cycle, {with_accept} with an acceptance command",
            read.shown,
            if n == 1 { "" } else { "s" }
        ),
        format!(
            "  ready first: {}",
            if first.is_empty() {
                "none".to_owned()
            } else {
                first.join(", ")
            }
        ),
    ];
    if !outside.is_empty() {
        lines.push(format!(
            "  depends on tasks that must already be on the board: {}",
            outside.join(", ")
        ));
    }
    for line in lines {
        writeln!(out, "{line}").map_err(io_err)?;
    }
    Ok(())
}

/// Where this command's project is: the repository's top level, or the
/// working directory outside one. Facts' source paths are relative to it.
fn project_root() -> std::path::PathBuf {
    crate::facts::project_root(&std::env::current_dir().unwrap_or_default())
}

/// A context of this process's own, for `--ephemeral` and scripts: in memory,
/// in this directory's project, checking facts against its files.
fn local_context() -> crate::exec::Context {
    crate::exec::Context {
        writer: None,
        namespace: session_namespace(None)
            .map(|n| n.name)
            .unwrap_or_else(|_| crate::namespace::FALLBACK.to_owned()),
        shared: crate::shared::Shared::in_memory(),
        root: Some(project_root()),
    }
}

/// `memfork watch`: the daemon's activity, as it happens.
fn run_watch(
    out: &mut impl Write,
    global: &GlobalArgs,
    count: Option<usize>,
    style: &Style,
) -> Result<(), ExecError> {
    let dir = data_dir(global)?;
    let rt = runtime()?;
    let mut seen = 0usize;
    let mut said_waiting = false;
    loop {
        let endpoint = match daemon::usable(&dir).map_err(|e| ExecError::Usage(e.to_string()))? {
            Some(endpoint) => endpoint,
            None => {
                if !said_waiting {
                    eprintln!(
                        "memfork: no daemon is running for {}; waiting for one to start \
                         (a client's first tool call starts it)",
                        dir.display()
                    );
                    said_waiting = true;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
                continue;
            }
        };
        said_waiting = false;
        let daemon = client::Daemon::new(&endpoint).map_err(ExecError::Usage)?;
        let mut failed: Option<std::io::Error> = None;
        let streamed = rt.block_on(daemon.stream_lines(serve::EVENTS_PATH, |line| {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                return true;
            };
            let hello = value["kind"] == "hello";
            let written = if global.json {
                writeln!(out, "{line}")
            } else if hello {
                watch_header(out, &value, &dir, style)
            } else {
                match serde_json::from_value::<crate::events::Event>(value) {
                    Ok(event) => writeln!(out, "{}", render::event(&event, style)),
                    Err(_) => Ok(()),
                }
            };
            if let Err(e) = written.and_then(|()| out.flush()) {
                failed = Some(e);
                return false;
            }
            if !hello {
                seen += 1;
            }
            count.is_none_or(|n| seen < n)
        }));
        if let Some(e) = failed {
            // The reader went away, as `watch | head` does. Nothing to report.
            return if e.kind() == std::io::ErrorKind::BrokenPipe {
                Ok(())
            } else {
                Err(io_err(e))
            };
        }
        if count.is_some_and(|n| seen >= n) {
            return Ok(());
        }
        if let Err(why) = streamed {
            eprintln!("memfork: lost the daemon ({why}); waiting for it to come back");
        } else {
            eprintln!("memfork: the daemon stopped; waiting for another to start");
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

fn watch_header(
    out: &mut impl Write,
    hello: &serde_json::Value,
    dir: &std::path::Path,
    style: &Style,
) -> std::io::Result<()> {
    use crate::style::Glyph;
    writeln!(
        out,
        "{} {} {}",
        style.accent(style.glyph(Glyph::Connected)),
        style.strong(crate::style::palette::PRIMARY, "daemon connected"),
        style.dim(&format!(
            "127.0.0.1:{}, version {}, store {}",
            hello["port"],
            hello["version"].as_str().unwrap_or("?"),
            dir.display()
        ))
    )?;
    let clients: Vec<String> = hello["clients"]
        .as_array()
        .map(|all| {
            all.iter()
                .map(|c| {
                    format!(
                        "{} ({})",
                        c["client"].as_str().unwrap_or("?"),
                        c["namespace"].as_str().unwrap_or("?")
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    if clients.is_empty() {
        writeln!(out, "  {}", style.dim("no clients connected yet"))
    } else {
        writeln!(out, "  clients connected: {}", clients.join(", "))
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
    style: &Style,
) -> Result<(), ExecError> {
    let source = read_script(script)?;
    let db = Db::new();
    let ctx = local_context();
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
        let outcome = execute_in(&db, branch, &parsed.command, &ctx)
            .map_err(|e| ExecError::Usage(format!("line {line_no}: {e}")))?;

        if as_json {
            results.push(outcome.json);
        } else {
            for l in render::lines(&parsed.command, &outcome, style) {
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
fn run_serve(args: &PersistArgs, port: u16, session_seconds: u64) -> Result<(), ExecError> {
    if args.ephemeral {
        return Err(ExecError::Usage(
            "`memfork serve --ephemeral` would be a daemon with nothing to share. \
             Drop --ephemeral, or use `memfork mcp --ephemeral` for a private, \
             in-memory session."
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
                session_seconds,
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
        let session = Arc::new(
            tools::dispatch::Session::in_namespace(db, namespace.name).in_project(project_root()),
        );
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
fn run_call(
    out: &mut impl Write,
    tool: &str,
    arguments: &str,
    global: &GlobalArgs,
    choice: ColorChoice,
) -> Result<(), ExecError> {
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
    let namespace = session_namespace(None)?.name;
    let result = if global.ephemeral {
        let session = tools::dispatch::Session::in_namespace(Db::new(), namespace);
        session
            .call(tool, &args)
            .map_err(|e| ExecError::Usage(e.to_string()))?
    } else {
        call_through_daemon(tool, args, namespace, global, choice)?
    };
    let text = serde_json::to_string_pretty(&result)
        .map_err(|e| ExecError::Usage(format!("cannot render the result: {e}")))?;
    // Always JSON: a tool result is JSON by definition, so `--json` adds nothing.
    writeln!(out, "{text}").map_err(io_err)
}

/// Register the MCP server with the clients installed here.
/// One tool call on the shared store, as an MCP session of its own, recorded
/// as coming from the command line.
fn call_through_daemon(
    tool: &str,
    args: serde_json::Map<String, serde_json::Value>,
    namespace: String,
    global: &GlobalArgs,
    choice: ColorChoice,
) -> Result<serde_json::Value, ExecError> {
    let dir = data_dir(global)?;
    let endpoint = daemon_for(&dir, choice)?;
    let root = project_root();
    let hasher = crate::facts::Hasher::default();
    let mut args = args;
    // As a proxy would: hash a fact's sources here, where the files are.
    if tool == "memfork_put" {
        let sources: Option<Vec<String>> = args
            .get("sources")
            .and_then(|s| serde_json::from_value(s.clone()).ok());
        if let Some(Ok(sources)) = sources.map(|s| crate::facts::normalise(&s)) {
            args.insert(
                "source_hashes".to_owned(),
                json!(hasher.record(&root, &sources)),
            );
        }
    }
    let hello = proxy::Hello {
        namespace: namespace.clone(),
        client: Some(serve::CLI_WRITER.to_owned()),
        session: Some("cli".to_owned()),
    };
    runtime()?.block_on(async {
        let upstream = proxy::Upstream::connect(&endpoint, &hello)
            .await
            .map_err(|e| ExecError::Usage(e.to_string()))?;
        // As a proxy would: a task's acceptance command runs here.
        let wants_acceptance = tool == "memfork_task"
            && args.get("action") == Some(&json!("done"))
            && !args.contains_key("acceptance");
        if let (true, Some(id)) = (wants_acceptance, args.get("id").and_then(|v| v.as_str())) {
            let id = id.to_owned();
            let ns = args
                .get("namespace")
                .and_then(|v| v.as_str())
                .unwrap_or(&namespace)
                .to_owned();
            let get = rmcp::model::CallToolRequestParams::new("memfork_get").with_arguments(
                json!({"key": crate::board::task_key(&ns, &id)})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            );
            let task = upstream
                .call_tool(&get)
                .await
                .ok()
                .and_then(|raw| {
                    raw["structuredContent"]["value"]
                        .as_str()
                        .map(str::to_owned)
                })
                .and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok());
            if let Some(task) = task {
                match crate::plans::prepare_done(&root, &id, &task) {
                    Ok(Some(ran)) => {
                        args.insert("acceptance".to_owned(), json!(ran));
                    }
                    Ok(None) => {}
                    Err(why) => {
                        upstream.close().await;
                        return Err(ExecError::Usage(why));
                    }
                }
            }
        }
        let params = rmcp::model::CallToolRequestParams::new(tool.to_owned()).with_arguments(args);
        let answer = upstream.call_tool(&params).await;
        let raw = match answer {
            Ok(raw) => raw,
            Err(e) => {
                upstream.close().await;
                return Err(ExecError::Usage(e.to_string()));
            }
        };
        let mut content = raw
            .get("structuredContent")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        // And check any facts in the answer against the files, as a proxy would.
        let checked = crate::facts::check(&mut content, &root, &hasher);
        if !checked.is_empty() {
            upstream
                .report(&json!({
                    "client": serve::CLI_WRITER,
                    "namespace": namespace,
                    "facts": checked.facts.iter().map(|(k, s)| json!([k, s])).collect::<Vec<_>>(),
                }))
                .await;
        }
        upstream.close().await;
        if raw.get("isError").and_then(serde_json::Value::as_bool) == Some(true) {
            return Err(ExecError::Usage(
                content["error"]
                    .as_str()
                    .unwrap_or("the tool reported an error")
                    .to_owned(),
            ));
        }
        Ok(content)
    })
}

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
    } else if plans
        .iter()
        .all(|p| matches!(p.action, init::Action::NotInstalled))
    {
        // The first thing a new user may see; "nothing to change" would not
        // tell them what to do next.
        writeln!(
            out,
            "\nNo MCP clients were found on this machine, so nothing was registered.\n\
             Install one of the clients above and run `memfork init` again. Any other \
             client that speaks MCP works too: configure it to run `memfork mcp`."
        )
        .map_err(io_err)?;
    } else if changed == 0 {
        writeln!(
            out,
            "\nNothing to change: every client found already has this MemFork."
        )
        .map_err(io_err)?;
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
