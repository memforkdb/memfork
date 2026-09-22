//! Plans: tasks with dependencies and acceptance commands (DESIGN §6.11).
//!
//! A plan is a set of ordinary tasks on the board, each of which may name the
//! tasks it depends on and a command that proves it is done. A task is ready
//! when everything it depends on is done. Nothing here schedules anything:
//! whichever agents connect pull ready tasks from the same board, so the
//! pipeline is data, not a program.
//!
//! **Where acceptance runs.** A command has to run in the project, which the
//! daemon cannot see, so the proxy (or `--ephemeral`, or the command line)
//! runs it when an agent marks the task done, and sends the result along.
//!
//! **Which commands run.** A command stored in shared memory would otherwise
//! run for whichever agent marks the task done, outside that client's own
//! approval of commands. So a command runs only if the repository's plan file
//! on disk holds the same command for the same task: the file is the trust
//! anchor, and changing it is an edit to the repository like any other. An
//! empty `accept` is no command at all.
//!
//! **Bounded.** Each run has a timeout (600 seconds unless the task says, at
//! most 3600), is killed with everything it started when the time is up, and
//! keeps the last 4 KiB of what it printed.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The plan file `memfork plan` reads and writes unless told another.
pub const DEFAULT_FILE: &str = "memfork-plan.toml";

/// Seconds an acceptance command may take unless its task says otherwise.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 600;

/// The longest an acceptance command may be given.
pub const MAX_TIMEOUT_SECONDS: u64 = 3600;

/// How much of an acceptance command's output is kept: its last bytes.
pub const OUTPUT_TAIL_BYTES: usize = 4096;

/// Most tasks one plan may hold.
pub const MAX_PLAN_TASKS: usize = 200;

/// Most dependencies one task may name.
pub const MAX_DEPENDENCIES: usize = 32;

/// Longest acceptance command, in characters.
pub const MAX_ACCEPT_CHARS: usize = 1000;

/// One task as a plan file or the `plan` action gives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanTask {
    /// Its id within the project.
    pub id: String,
    /// What it is, in a line.
    pub title: String,
    /// Anything more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Ids of the tasks that must be done first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// A command that exits 0 in the project when the task is done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accept: Option<String>,
    /// How long the command may take.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanFile {
    /// What the plan is for, in a line; templates have one.
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    task: Vec<PlanTask>,
}

/// The templates `memfork plan new` ships with, as (name, file).
const BUILT_IN: &[(&str, &str)] = &[
    ("feature", include_str!("plan_templates/feature.toml")),
    ("bugfix", include_str!("plan_templates/bugfix.toml")),
    ("refactor", include_str!("plan_templates/refactor.toml")),
    ("upgrade", include_str!("plan_templates/upgrade.toml")),
    ("tests", include_str!("plan_templates/tests.toml")),
];

/// The directory in the data directory where a person's own templates go.
pub const TEMPLATES_DIR: &str = "plans";

/// A plan to start from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// What `--template` takes.
    pub name: String,
    /// What it is for, in a line.
    pub description: String,
    /// The plan file, exactly as it will be written.
    pub text: String,
    /// Where it came from: `built-in`, or the file it was read from.
    pub source: String,
    /// Why it cannot be used, for a person's own file that does not parse.
    pub problem: Option<String>,
}

/// Every template: the built-in ones, then a person's own from
/// `<data dir>/plans/*.toml` in name order. One of theirs with a built-in
/// name takes its place.
pub fn templates(data_dir: Option<&Path>) -> Vec<Template> {
    let describe = |text: &str| -> (String, Option<String>) {
        match toml::from_str::<PlanFile>(text) {
            Ok(file) => {
                let checked =
                    check_shape(&file.task).and_then(|()| check_alone(&file.task).map(|_| ()));
                (file.description.unwrap_or_default(), checked.err())
            }
            Err(e) => (String::new(), Some(format!("does not parse: {e}"))),
        }
    };
    let mut out: Vec<Template> = BUILT_IN
        .iter()
        .map(|(name, text)| {
            let (description, problem) = describe(text);
            Template {
                name: (*name).to_owned(),
                description,
                text: (*text).to_owned(),
                source: "built-in".to_owned(),
                problem,
            }
        })
        .collect();
    let Some(dir) = data_dir.map(|d| d.join(TEMPLATES_DIR)) else {
        return out;
    };
    let mut own: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "toml"))
                .collect()
        })
        .unwrap_or_default();
    own.sort();
    for path in own {
        let Some(name) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        let Ok(text) = read_file(&path) else {
            continue;
        };
        let (description, problem) = describe(&text);
        let template = Template {
            name: name.clone(),
            description,
            text,
            source: path.display().to_string(),
            problem,
        };
        match out.iter_mut().find(|t| t.name == name) {
            Some(existing) => *existing = template,
            None => out.push(template),
        }
    }
    out
}

/// An acceptance command as a task holds it: trimmed, and `None` when empty,
/// since an empty or placeholder command is never run.
pub fn normalise_accept(accept: Option<&str>) -> Option<String> {
    accept
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_owned)
}

/// Read a plan file's text. Checks its shape; the graph is checked against
/// the board when the plan is written, or on its own by [`check_alone`].
///
/// ```
/// let plan = r#"
/// [[task]]
/// id = "schema"
/// title = "add the refunds table"
///
/// [[task]]
/// id = "api"
/// title = "refunds endpoint"
/// depends_on = ["schema"]
/// accept = "cargo test -p api"
/// "#;
/// let tasks = memfork::plans::parse(plan).unwrap();
/// assert_eq!(tasks.len(), 2);
/// assert_eq!(tasks[1].depends_on, ["schema"]);
/// // Nothing outside the plan is depended on, and there is no cycle.
/// assert!(memfork::plans::check_alone(&tasks).unwrap().is_empty());
/// ```
pub fn parse(text: &str) -> Result<Vec<PlanTask>, String> {
    let file: PlanFile =
        toml::from_str(text).map_err(|e| format!("the plan does not parse: {e}"))?;
    let mut tasks = file.task;
    for task in &mut tasks {
        task.accept = normalise_accept(task.accept.as_deref());
    }
    check_shape(&tasks)?;
    Ok(tasks)
}

/// Check a plan without the board: no cycle among its own tasks. Returns the
/// dependencies it names but does not hold, which must already be tasks on
/// the board when it is written.
pub fn check_alone(tasks: &[PlanTask]) -> Result<Vec<String>, String> {
    let ids: BTreeSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    let outside: BTreeSet<String> = tasks
        .iter()
        .flat_map(|t| t.depends_on.iter())
        .filter(|d| !ids.contains(d.as_str()))
        .cloned()
        .collect();
    let others = outside.iter().map(|d| (d.clone(), Vec::new())).collect();
    check_graph(tasks, &others)?;
    Ok(outside.into_iter().collect())
}

/// Sizes and ids, before anything is looked up.
pub fn check_shape(tasks: &[PlanTask]) -> Result<(), String> {
    if tasks.is_empty() {
        return Err(
            "the plan has no tasks; each one is a `[[task]]` with an `id` and a `title`".to_owned(),
        );
    }
    if tasks.len() > MAX_PLAN_TASKS {
        return Err(format!(
            "a plan may hold at most {MAX_PLAN_TASKS} tasks; this one has {}",
            tasks.len()
        ));
    }
    let mut seen = BTreeSet::new();
    for task in tasks {
        if !seen.insert(task.id.as_str()) {
            return Err(format!("the plan names task `{}` twice", task.id));
        }
        if task.title.trim().is_empty() {
            return Err(format!("task `{}` has no `title`", task.id));
        }
        if task.depends_on.len() > MAX_DEPENDENCIES {
            return Err(format!(
                "task `{}` depends on {} tasks; at most {MAX_DEPENDENCIES}",
                task.id,
                task.depends_on.len()
            ));
        }
        for dep in &task.depends_on {
            if dep.contains(':') {
                return Err(format!(
                    "task `{}` depends on `{dep}`, which names another project; a plan's \
                     dependencies are tasks in its own project",
                    task.id
                ));
            }
        }
        if task.depends_on.contains(&task.id) {
            return Err(format!("task `{}` depends on itself", task.id));
        }
        if task
            .accept
            .as_ref()
            .is_some_and(|a| a.chars().count() > MAX_ACCEPT_CHARS)
        {
            return Err(format!(
                "task `{}` has an acceptance command over {MAX_ACCEPT_CHARS} characters",
                task.id
            ));
        }
        if let Some(t) = task.timeout_seconds {
            if t == 0 || t > MAX_TIMEOUT_SECONDS {
                return Err(format!(
                    "task `{}`: `timeout_seconds` must be 1 to {MAX_TIMEOUT_SECONDS}",
                    task.id
                ));
            }
        }
    }
    Ok(())
}

/// Every dependency exists — in the plan or among `others`, the project's
/// tasks the plan does not replace, with their own dependencies — and there
/// is no cycle. The error names the cycle.
pub fn check_graph(
    tasks: &[PlanTask],
    others: &BTreeMap<String, Vec<String>>,
) -> Result<(), String> {
    let mut graph: BTreeMap<&str, Vec<&str>> = others
        .iter()
        .map(|(id, deps)| (id.as_str(), deps.iter().map(String::as_str).collect()))
        .collect();
    for task in tasks {
        graph.insert(
            task.id.as_str(),
            task.depends_on.iter().map(String::as_str).collect(),
        );
    }
    for task in tasks {
        for dep in &task.depends_on {
            if !graph.contains_key(dep.as_str()) {
                return Err(format!(
                    "task `{}` depends on `{dep}`, which is not a task in this project or \
                     this plan",
                    task.id
                ));
            }
        }
    }
    // Depth-first, in key order, so the cycle named is the same every time.
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }
    fn visit<'a>(
        id: &'a str,
        graph: &BTreeMap<&'a str, Vec<&'a str>>,
        marks: &mut BTreeMap<&'a str, Mark>,
        path: &mut Vec<&'a str>,
    ) -> Result<(), String> {
        match marks.get(id) {
            Some(Mark::Done) => return Ok(()),
            Some(Mark::Visiting) => {
                let start = path.iter().position(|p| *p == id).unwrap_or(0);
                let mut cycle: Vec<&str> = path[start..].to_vec();
                cycle.push(id);
                return Err(format!(
                    "the plan has a cycle, so none of it could ever be ready: {}",
                    cycle.join(" -> ")
                ));
            }
            None => {}
        }
        marks.insert(id, Mark::Visiting);
        path.push(id);
        for dep in graph.get(id).into_iter().flatten() {
            visit(dep, graph, marks, path)?;
        }
        path.pop();
        marks.insert(id, Mark::Done);
        Ok(())
    }
    let mut marks = BTreeMap::new();
    for id in graph.keys() {
        visit(id, &graph, &mut marks, &mut Vec::new())?;
    }
    Ok(())
}

/// What running an acceptance command found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acceptance {
    /// The command that ran.
    pub command: String,
    /// Its exit code; `None` if it could not start, was killed, or ran out
    /// of time.
    pub exit_code: Option<i32>,
    /// Whether it ran out of time.
    #[serde(default)]
    pub timed_out: bool,
    /// Whole seconds it took, for people; never part of an id.
    #[serde(default)]
    pub seconds: u64,
    /// The last [`OUTPUT_TAIL_BYTES`] of what it printed, both streams.
    #[serde(default)]
    pub output: String,
}

impl Acceptance {
    /// Whether it proves the task done.
    pub fn passed(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out
    }

    /// The last line it printed that is not blank, for a lesson.
    pub fn last_line(&self) -> Option<&str> {
        self.output
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty())
    }
}

/// Check that `command` is what the repository's plan file says for task
/// `id`. `plan_file` is the path the plan was written from, relative to the
/// project; [`DEFAULT_FILE`] when the task did not come from a file.
pub fn trusted(
    root: &Path,
    plan_file: Option<&str>,
    id: &str,
    command: &str,
) -> Result<(), String> {
    let relative = plan_file.unwrap_or(DEFAULT_FILE);
    let parts = crate::facts::normalise(&[relative.to_owned()])
        .map_err(|e| format!("the task's plan file cannot be used: {e}"))?;
    let shown = parts
        .first()
        .cloned()
        .unwrap_or_else(|| relative.to_owned());
    let path = shown
        .split('/')
        .fold(root.to_path_buf(), |p, part| p.join(part));
    let refuse = |why: String| {
        format!(
            "task `{id}` has the acceptance command `{command}`, but {why}. An acceptance \
             command runs only when the repository's plan file holds the same command for \
             the same task, so a command written into shared memory cannot run on its own. \
             Add it to `{shown}` (then `memfork plan write {shown}`), or remove it from the task."
        )
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|_| refuse(format!("there is no readable `{shown}` in this project")))?;
    let tasks = parse(&text).map_err(|e| refuse(format!("`{shown}` could not be read: {e}")))?;
    match tasks.iter().find(|t| t.id == id) {
        Some(t) if t.accept.as_deref() == Some(command) => Ok(()),
        Some(t) => Err(refuse(format!(
            "`{shown}` gives task `{id}` {}",
            t.accept
                .as_deref()
                .map_or_else(|| "no command".to_owned(), |a| format!("the command `{a}`"))
        ))),
        None => Err(refuse(format!("`{shown}` has no task `{id}`"))),
    }
}

/// The acceptance to send with `done` for a task, as the task entry's JSON
/// holds it: `None` if it has no command, the run if it has a trusted one,
/// and an error saying why if its command is not trusted.
pub fn prepare_done(
    root: &Path,
    id: &str,
    task: &serde_json::Value,
) -> Result<Option<Acceptance>, String> {
    let Some(command) = normalise_accept(task.get("accept").and_then(serde_json::Value::as_str))
    else {
        return Ok(None);
    };
    trusted(
        root,
        task.get("plan_file").and_then(serde_json::Value::as_str),
        id,
        &command,
    )?;
    let timeout = task
        .get("timeout_seconds")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
        .clamp(1, MAX_TIMEOUT_SECONDS);
    Ok(Some(run(root, &command, Duration::from_secs(timeout))))
}

/// The platform's shell, run on a command line.
fn shell(command: &str) -> Command {
    if cfg!(windows) {
        let comspec = std::env::var_os("ComSpec").unwrap_or_else(|| {
            let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
            Path::new(&root)
                .join("System32")
                .join("cmd.exe")
                .into_os_string()
        });
        let mut cmd = Command::new(comspec);
        cmd.arg("/D").arg("/C").arg(command);
        cmd
    } else {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(command);
        cmd
    }
}

/// Start a process group of its own on Unix, so a timeout can stop all of it.
#[cfg(unix)]
fn own_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn own_group(_cmd: &mut Command) {}

/// Stop a process and everything it started.
fn kill_tree(child: &mut std::process::Child) {
    let pid = child.id().to_string();
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let taskkill = Path::new(&root).join("System32").join("taskkill.exe");
        let _ = Command::new(taskkill)
            .args(["/T", "/F", "/PID", &pid])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    } else {
        // The group this child leads: `kill` takes a negative id for that,
        // after `--` so it is not read as a signal.
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// How long to wait for a stopped command's output to drain: something it
/// started and that outlived it may hold the pipe open for good.
const DRAIN: Duration = Duration::from_secs(2);

/// Keep reading `from` on a thread, holding only its last bytes; they arrive
/// on the channel when the stream ends.
fn tail_of(mut from: impl std::io::Read + Send + 'static) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut kept: Vec<u8> = Vec::new();
        let mut buffer = [0u8; 8192];
        while let Ok(n) = from.read(&mut buffer) {
            if n == 0 {
                break;
            }
            kept.extend_from_slice(&buffer[..n]);
            if kept.len() > OUTPUT_TAIL_BYTES * 2 {
                kept.drain(..kept.len() - OUTPUT_TAIL_BYTES);
            }
        }
        let _ = tx.send(kept);
    });
    rx
}

/// Run an acceptance command in `root`, within `timeout`.
pub fn run(root: &Path, command: &str, timeout: Duration) -> Acceptance {
    let started = Instant::now();
    let mut cmd = shell(command);
    cmd.current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_group(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return Acceptance {
                command: command.to_owned(),
                exit_code: None,
                timed_out: false,
                seconds: 0,
                output: format!("the command could not be started: {e}"),
            }
        }
    };
    let out = child.stdout.take().map(tail_of);
    let err = child.stderr.take().map(tail_of);
    let deadline = started + timeout;
    let (exit_code, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.code(), false),
            Ok(None) if Instant::now() >= deadline => {
                kill_tree(&mut child);
                break (None, true);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => {
                kill_tree(&mut child);
                break (None, false);
            }
        }
    };
    // A command that finished has closed its streams; one that was stopped
    // may have left something holding them, so that wait is bounded.
    let wait = |rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>| -> Vec<u8> {
        rx.and_then(|rx| {
            if timed_out {
                rx.recv_timeout(DRAIN).ok()
            } else {
                rx.recv().ok()
            }
        })
        .unwrap_or_default()
    };
    let mut bytes = wait(out);
    bytes.extend(wait(err));
    if bytes.len() > OUTPUT_TAIL_BYTES {
        bytes.drain(..bytes.len() - OUTPUT_TAIL_BYTES);
    }
    let mut output = String::from_utf8_lossy(&bytes).into_owned();
    if timed_out {
        output.push_str(&format!("\n(stopped after {} seconds)", timeout.as_secs()));
    }
    Acceptance {
        command: command.to_owned(),
        exit_code,
        timed_out,
        seconds: started.elapsed().as_secs(),
        output,
    }
}

/// Read what a plan file holds, for `memfork plan` to report on.
pub fn read_file(path: &Path) -> Result<String, String> {
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|mut f| f.read_to_string(&mut text))
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, deps: &[&str]) -> PlanTask {
        PlanTask {
            id: id.to_owned(),
            title: format!("do {id}"),
            detail: None,
            depends_on: deps.iter().map(|d| (*d).to_owned()).collect(),
            accept: None,
            timeout_seconds: None,
        }
    }

    #[test]
    fn every_built_in_template_is_a_short_valid_plan_with_a_command_to_fill_in() {
        let all = templates(None);
        assert_eq!(all.len(), 5);
        for t in &all {
            assert_eq!(t.problem, None, "{}", t.name);
            assert!(!t.description.is_empty(), "{} has no description", t.name);
            let tasks = parse(&t.text).expect("parses");
            assert!(
                (4..=6).contains(&tasks.len()),
                "{} has {} tasks",
                t.name,
                tasks.len()
            );
            assert!(
                check_alone(&tasks).expect("no cycle").is_empty(),
                "{}",
                t.name
            );
            assert!(
                t.text.contains("accept = \"\""),
                "{} has nothing to fill in",
                t.name
            );
            // An empty command is never run.
            assert!(tasks.iter().all(|task| task.accept.is_none()), "{}", t.name);
            crate::secrets::check(&t.name, &t.text, &crate::secrets::Allow::default())
                .expect("no secret-shaped text");
        }
    }

    #[test]
    fn a_persons_own_template_is_listed_and_can_replace_a_built_in_one() {
        let data = tempfile::tempdir().expect("tempdir");
        let dir = data.path().join(TEMPLATES_DIR);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            dir.join("release.toml"),
            "description = \"cut a release\"\n[[task]]\nid = \"tag\"\ntitle = \"tag it\"\n",
        )
        .expect("w");
        std::fs::write(
            dir.join("bugfix.toml"),
            "description = \"ours\"\n[[task]]\nid = \"a\"\ntitle = \"a\"\n",
        )
        .expect("w");
        std::fs::write(dir.join("broken.toml"), "[[task]\n").expect("w");
        let all = templates(Some(data.path()));
        let names: Vec<&str> = all.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            ["feature", "bugfix", "refactor", "upgrade", "tests", "broken", "release"]
        );
        assert_eq!(all[1].description, "ours");
        assert!(all[5].problem.is_some());
    }

    #[test]
    fn a_cycle_is_named_and_refused() {
        let err = check_graph(
            &[task("a", &["c"]), task("b", &["a"]), task("c", &["b"])],
            &BTreeMap::new(),
        )
        .expect_err("a cycle");
        assert!(err.contains("a -> c -> b -> a"), "{err}");
        assert!(check_graph(&[task("a", &[]), task("b", &["a"])], &BTreeMap::new()).is_ok());
    }

    #[test]
    fn dependencies_must_exist_and_stay_in_the_project() {
        let err = check_graph(&[task("b", &["a"])], &BTreeMap::new()).expect_err("missing");
        assert!(err.contains("not a task"), "{err}");
        let mut others = BTreeMap::new();
        others.insert("a".to_owned(), Vec::new());
        assert!(check_graph(&[task("b", &["a"])], &others).is_ok());
        let err = check_shape(&[task("b", &["other:a"])]).expect_err("another project");
        assert!(err.contains("another project"), "{err}");
    }

    #[test]
    fn an_empty_accept_is_no_command() {
        let tasks = parse(
            "[[task]]\nid = \"a\"\ntitle = \"t\"\naccept = \"\"\n\n[[task]]\nid = \"b\"\ntitle = \"u\"\naccept = \"   \"\n",
        )
        .expect("parsed");
        assert!(tasks.iter().all(|t| t.accept.is_none()));
        let root = tempfile::tempdir().expect("tempdir");
        let answer = prepare_done(root.path(), "a", &serde_json::json!({"accept": ""}));
        assert_eq!(answer, Ok(None));
    }

    #[test]
    fn only_a_command_the_plan_file_holds_runs() {
        let root = tempfile::tempdir().expect("tempdir");
        let task = serde_json::json!({"accept": "exit 0"});
        let err = prepare_done(root.path(), "a", &task).expect_err("no file");
        assert!(err.contains("memfork-plan.toml"), "{err}");
        std::fs::write(
            root.path().join(DEFAULT_FILE),
            "[[task]]\nid = \"a\"\ntitle = \"t\"\naccept = \"exit 1\"\n",
        )
        .expect("written");
        let err = prepare_done(root.path(), "a", &task).expect_err("different");
        assert!(err.contains("`exit 1`"), "{err}");
        std::fs::write(
            root.path().join(DEFAULT_FILE),
            "[[task]]\nid = \"a\"\ntitle = \"t\"\naccept = \"exit 0\"\n",
        )
        .expect("written");
        let ran = prepare_done(root.path(), "a", &task)
            .expect("trusted")
            .expect("ran");
        assert!(ran.passed(), "{ran:?}");
    }

    #[test]
    fn a_run_reports_its_exit_its_output_and_a_timeout() {
        let root = tempfile::tempdir().expect("tempdir");
        let failed = run(
            root.path(),
            "echo first && echo second && exit 3",
            Duration::from_secs(60),
        );
        assert_eq!(failed.exit_code, Some(3), "{failed:?}");
        assert_eq!(failed.last_line(), Some("second"));
        assert!(!failed.passed());
        let slow = if cfg!(windows) {
            "ping -n 30 127.0.0.1 > NUL"
        } else {
            "sleep 30"
        };
        let started = Instant::now();
        let stopped = run(root.path(), slow, Duration::from_secs(1));
        assert!(stopped.timed_out && !stopped.passed(), "{stopped:?}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the timeout did not stop it"
        );
    }

    #[test]
    fn output_is_bounded() {
        let root = tempfile::tempdir().expect("tempdir");
        let noisy = if cfg!(windows) {
            "for /L %i in (1,1,3000) do @echo line %i"
        } else {
            "i=1; while [ $i -le 3000 ]; do echo line $i; i=$((i+1)); done"
        };
        let ran = run(root.path(), noisy, Duration::from_secs(60));
        assert!(ran.passed(), "{ran:?}");
        assert!(
            ran.output.len() <= OUTPUT_TAIL_BYTES + 64,
            "{}",
            ran.output.len()
        );
        assert_eq!(ran.last_line(), Some("line 3000"));
    }
}
