//! `memfork autopilot hook`: what a client's hook runs (DESIGN §5.6).
//!
//! A client that has a hook system runs this command before and after a
//! tool call and when the agent stops, with the event as JSON on stdin. It
//! reads the project's autopilot file, decides by the rules whether the
//! action is risky, and tells the daemon to fork, or to settle a fork by the
//! outcome; when a check command is configured it runs that here, in the
//! project, and reports what it found.
//!
//! **Fail open, always.** No daemon running, no endpoint file, the daemon
//! unreachable or of another version, the policy off, the project file
//! absent or broken, stdin unreadable: it exits 0 with nothing on stdout or
//! stderr, and it never starts a daemon. A client's hook error, even a
//! non-blocking one, would put a notice in front of the agent, and autopilot
//! is not allowed to do that.
//!
//! The event's shape is Claude Code's, verified against its documentation
//! on the date the registry records. Another client's shape would be a
//! second parser chosen by the registry's `format`, not a change here.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value as Json};

use super::config::{self, Config};
use super::rules::RuleSet;

/// The most stdin a hook reads.
pub const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;

/// How long one request to the daemon may take. A hook that runs before a
/// tool call holds the agent for that long at most.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// The events this command understands.
pub const EVENTS: &[&str] = &["PreToolUse", "PostToolUse", "PostToolUseFailure", "Stop"];

/// Tools that run a shell command.
pub const COMMAND_TOOLS: &[&str] = &["Bash", "PowerShell"];

/// Tools that edit one file.
pub const EDIT_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// What one hook run decided to do, for tests and `--json`; the command
/// itself prints nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    /// One line saying what happened, or why nothing did.
    pub did: String,
}

/// What a hook event carries, as far as autopilot needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    /// `PreToolUse`, `PostToolUse`, `PostToolUseFailure` or `Stop`.
    pub event: String,
    /// The working directory the client reports.
    pub cwd: Option<PathBuf>,
    /// The tool, for tool events.
    pub tool: Option<String>,
    /// The tool use id, for tool events.
    pub tool_use_id: Option<String>,
    /// The command, for a command tool.
    pub command: Option<String>,
    /// The file, for an edit tool.
    pub file: Option<String>,
    /// The failure text, for a failed tool.
    pub error: Option<String>,
    /// What the tool printed, for a command tool that finished: stdout
    /// then stderr, as the client reports them.
    pub output: Option<String>,
    /// The exit status the client reports for a command that finished.
    pub exit_status: Option<i64>,
}

impl Input {
    /// Read the client's JSON.
    pub fn parse(text: &str) -> Option<Input> {
        let json: Json = serde_json::from_str(text).ok()?;
        let event = json["hook_event_name"].as_str()?.to_owned();
        let tool_input = &json["tool_input"];
        Some(Input {
            event,
            cwd: json["cwd"].as_str().map(PathBuf::from),
            tool: json["tool_name"].as_str().map(str::to_owned),
            tool_use_id: json["tool_use_id"].as_str().map(str::to_owned),
            command: tool_input["command"].as_str().map(str::to_owned),
            file: tool_input["file_path"]
                .as_str()
                .or_else(|| tool_input["notebook_path"].as_str())
                .map(str::to_owned),
            error: json["error"].as_str().map(str::to_owned),
            output: {
                let response = &json["tool_response"];
                let stdout = response["stdout"].as_str().unwrap_or("");
                let stderr = response["stderr"].as_str().unwrap_or("");
                response
                    .is_object()
                    .then(|| match (stdout.is_empty(), stderr.is_empty()) {
                        (_, true) => stdout.to_owned(),
                        (true, false) => stderr.to_owned(),
                        (false, false) => format!("{stdout}\n{stderr}"),
                    })
            },
            exit_status: json["tool_response"]["exit_code"].as_i64(),
        })
    }

    fn is_command_tool(&self) -> bool {
        self.tool
            .as_deref()
            .is_some_and(|t| COMMAND_TOOLS.contains(&t))
    }

    fn is_edit_tool(&self) -> bool {
        self.tool
            .as_deref()
            .is_some_and(|t| EDIT_TOOLS.contains(&t))
    }
}

/// The exit code in a failed command's text, whose first line is
/// `Exit code N` when the command ran.
pub fn exit_code_of(error: &str) -> Option<i64> {
    error
        .lines()
        .next()?
        .trim()
        .strip_prefix("Exit code ")?
        .trim()
        .parse()
        .ok()
}

/// The exit code a wrapped command printed as its last line: Claude Code
/// runs every shell command as `<cmd> 2>&1; echo "exit: $?"`, so the
/// shell's own status is always 0 and the command's is this line.
pub fn wrapped_exit_of(output: &str) -> Option<i64> {
    last_line(output)?
        .strip_prefix("exit:")?
        .trim()
        .parse()
        .ok()
}

/// The last line of some text that is not blank.
fn last_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_owned)
}

/// The last line that is not blank and not the wrapper's `exit: N`.
fn last_line_before_exit(output: &str) -> Option<String> {
    let mut lines: Vec<&str> = output.lines().map(str::trim).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if lines.last().is_some_and(|l| l.starts_with("exit:")) {
        lines.pop();
    }
    lines
        .into_iter()
        .rev()
        .find(|l| !l.is_empty())
        .map(str::to_owned)
}

/// A file as the sweep counts it: relative to the project with `/`
/// separators when it is inside, the path as given otherwise.
pub fn file_key(root: &Path, file: &str) -> String {
    let normalised = file.replace('\\', "/");
    let root_text = root.display().to_string().replace('\\', "/");
    let stripped = normalised
        .strip_prefix(&root_text)
        .map(|rest| rest.trim_start_matches('/'))
        .filter(|rest| !rest.is_empty())
        .map(str::to_owned);
    stripped.unwrap_or(normalised)
}

/// What a check, or the action's own outcome, decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// `passed`, `failed` or `none`.
    pub outcome: &'static str,
    /// The check command, if one ran.
    pub check: Option<String>,
    /// Its exit code, or the action's.
    pub exit_code: Option<i64>,
    /// Whether the check ran out of time.
    pub timed_out: bool,
    /// Seconds it was given.
    pub timeout_seconds: Option<u64>,
    /// The last line printed.
    pub last_line: Option<String>,
}

impl Verdict {
    fn none() -> Verdict {
        Verdict {
            outcome: "none",
            check: None,
            exit_code: None,
            timed_out: false,
            timeout_seconds: None,
            last_line: None,
        }
    }

    /// Run the project's check command, if it names one.
    fn by_check(root: &Path, config: &Config) -> Option<Verdict> {
        let check = config.check.as_deref()?;
        let ran = crate::plans::run(root, check, config.timeout());
        Some(Verdict {
            outcome: if ran.passed() { "passed" } else { "failed" },
            check: Some(check.to_owned()),
            exit_code: ran.exit_code.map(i64::from),
            timed_out: ran.timed_out,
            timeout_seconds: Some(config.timeout().as_secs()),
            last_line: ran.last_line().map(str::to_owned),
        })
    }

    /// The action's own outcome, by the best signal there is, in order: a
    /// tool the client says failed is a failure; a trailing `exit: N` line
    /// in what the tool printed is N, since Claude Code wraps every shell
    /// command as `<cmd> 2>&1; echo "exit: $?"` and the shell's own status
    /// is then always 0; only then the exit status the client reports; and
    /// a tool that finished with none of these passed.
    fn by_action(input: &Input) -> Verdict {
        let own = |exit_code: i64, last_line: Option<String>| Verdict {
            outcome: if exit_code == 0 { "passed" } else { "failed" },
            check: None,
            exit_code: Some(exit_code),
            timed_out: false,
            timeout_seconds: None,
            last_line: if exit_code == 0 { None } else { last_line },
        };
        if let Some(error) = &input.error {
            return Verdict {
                outcome: "failed",
                check: None,
                exit_code: exit_code_of(error),
                timed_out: false,
                timeout_seconds: None,
                last_line: last_line(error),
            };
        }
        let output = input.output.as_deref().unwrap_or("");
        if let Some(code) = wrapped_exit_of(output) {
            return own(code, last_line_before_exit(output));
        }
        if let Some(code) = input.exit_status {
            return own(code, last_line(output));
        }
        own(0, None)
    }

    fn fields(&self, body: &mut Json) {
        body["outcome"] = json!(self.outcome);
        body["check"] = json!(self.check);
        body["exit_code"] = json!(self.exit_code);
        body["timed_out"] = json!(self.timed_out);
        body["timeout_seconds"] = json!(self.timeout_seconds);
        body["last_line"] = json!(self.last_line);
    }
}

/// Run the hook for `client` on `text`, the event JSON. Never fails: the
/// report says what happened, and the command prints none of it.
pub fn run(client: &str, text: &str) -> Report {
    let did = |s: &str| Report { did: s.to_owned() };
    let Some(input) = Input::parse(text) else {
        return did("the event could not be read");
    };
    if !EVENTS.contains(&input.event.as_str()) {
        return did("not an event autopilot acts on");
    }
    let cwd = input
        .cwd
        .clone()
        .or_else(|| std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok());
    let Some(cwd) = cwd else {
        return did("no working directory");
    };
    let Some(root) = crate::namespace::repository_root(&cwd) else {
        return did("not in a repository");
    };
    let Some(config) = config::read(&root).config().cloned() else {
        return did("autopilot is not switched on here");
    };
    if !config.forks() {
        return did("automatic forks are off here");
    }
    if !crate::policy::allows(crate::policy::Feature::Autopilot) {
        return did("the machine policy switches autopilot off");
    }
    let env_ns = std::env::var(crate::namespace::NAMESPACE_ENV).ok();
    let Ok(namespace) = crate::namespace::resolve(None, env_ns.as_deref(), &cwd) else {
        return did("no usable namespace");
    };
    let Ok(dir) = crate::persist::datadir::here() else {
        return did("no data directory");
    };
    let Ok(Some(endpoint)) = crate::daemon::usable(&dir.path) else {
        return did("no daemon is running");
    };
    if endpoint.port.is_none() {
        return did("no daemon is running");
    }
    let Ok(daemon) = crate::client::Daemon::new(&endpoint) else {
        return did("the endpoint file is unusable");
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return did("no runtime");
    };
    let post = |body: Json| -> Option<Json> {
        let (status, answer) = runtime
            .block_on(daemon.post_within(super::PATH, &body, REQUEST_TIMEOUT))
            .ok()?;
        (status == 200).then_some(answer)
    };
    let base = |action: &str| {
        json!({
            "action": action,
            "client": client,
            "namespace": namespace.name,
            "tool_use_id": input.tool_use_id,
        })
    };

    match input.event.as_str() {
        "PreToolUse" if input.is_command_tool() => {
            let Some(command) = input.command.as_deref() else {
                return did("no command");
            };
            let Ok(rules) = RuleSet::for_project(&config.extra_rules, &config.ignore_rules) else {
                return did("the project's rules do not compile");
            };
            let Some(rule) = rules.matches(command) else {
                return did("not risky");
            };
            let mut body = base("fork");
            body["rule"] = json!(rule.name);
            body["command"] = json!(command);
            match post(body) {
                Some(answer) => did(&format!("forked {}", answer["forked"])),
                None => did("the daemon could not be told"),
            }
        }
        "PreToolUse" if input.is_edit_tool() => {
            let Some(file) = input.file.as_deref() else {
                return did("no file");
            };
            let mut body = base("edit");
            body["file"] = json!(file_key(&root, file));
            body["max_files"] = json!(config.max_files);
            match post(body) {
                Some(answer) => did(&format!("edit noted; forked {}", answer["forked"])),
                None => did("the daemon could not be told"),
            }
        }
        "PreToolUse" => did("not a tool autopilot watches"),
        "PostToolUse" | "PostToolUseFailure" => {
            if !input.is_command_tool() {
                return did("not a tool autopilot settles by");
            }
            let open = post(base("open"));
            let Some(open) = open.filter(|o| o["open"].as_array().is_some_and(|a| !a.is_empty()))
            else {
                return did("no fork is open for this action");
            };
            let verdict =
                Verdict::by_check(&root, &config).unwrap_or_else(|| Verdict::by_action(&input));
            let mut body = base("settle");
            verdict.fields(&mut body);
            match post(body) {
                Some(answer) => did(&format!(
                    "settled {} as {}: {}",
                    open["open"].as_array().map_or(0, Vec::len),
                    verdict.outcome,
                    answer["settled"]
                )),
                None => did("the daemon could not be told"),
            }
        }
        _ => {
            // Stop: whatever is open settles by the check, or is kept.
            let mut ask = base("open");
            ask["tool_use_id"] = Json::Null;
            let open = post(ask);
            let Some(_) = open.filter(|o| o["open"].as_array().is_some_and(|a| !a.is_empty()))
            else {
                return did("nothing is open");
            };
            let verdict = Verdict::by_check(&root, &config).unwrap_or_else(Verdict::none);
            let mut body = base("settle");
            body["tool_use_id"] = Json::Null;
            verdict.fields(&mut body);
            match post(body) {
                Some(answer) => did(&format!(
                    "settled at stop as {}: {}",
                    verdict.outcome, answer["settled"]
                )),
                None => did("the daemon could not be told"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_events_shape_is_read_as_documented() {
        let pre = Input::parse(
            r#"{"session_id":"abc","cwd":"C:\\p","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"npm test","description":"x"},"tool_use_id":"toolu_01"}"#,
        )
        .unwrap();
        assert_eq!(pre.event, "PreToolUse");
        assert_eq!(pre.tool.as_deref(), Some("Bash"));
        assert_eq!(pre.command.as_deref(), Some("npm test"));
        assert_eq!(pre.tool_use_id.as_deref(), Some("toolu_01"));
        assert_eq!(pre.cwd.as_deref(), Some(Path::new("C:\\p")));
        assert!(pre.is_command_tool() && !pre.is_edit_tool());

        let edit = Input::parse(
            r#"{"hook_event_name":"PreToolUse","tool_name":"Write","tool_input":{"file_path":"C:\\project\\src\\index.ts","content":"..."}}"#,
        )
        .unwrap();
        assert!(edit.is_edit_tool());
        assert_eq!(edit.file.as_deref(), Some("C:\\project\\src\\index.ts"));

        let failed = Input::parse(
            r#"{"hook_event_name":"PostToolUseFailure","tool_name":"Bash","tool_input":{"command":"npm test"},"tool_use_id":"t","error":"Exit code 1\nError: Cannot find module 'express'","is_interrupt":false}"#,
        )
        .unwrap();
        assert_eq!(exit_code_of(failed.error.as_deref().unwrap()), Some(1));
        let v = Verdict::by_action(&failed);
        assert_eq!(v.outcome, "failed");
        assert_eq!(v.exit_code, Some(1));
        assert_eq!(
            v.last_line.as_deref(),
            Some("Error: Cannot find module 'express'")
        );
        let ok = Verdict::by_action(&Input::parse(r#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"x"}}"#).unwrap());
        assert_eq!((ok.outcome, ok.exit_code), ("passed", Some(0)));
        // A failure that never ran the shell has no exit code line.
        assert_eq!(exit_code_of("spawn failed"), None);

        assert!(Input::parse("not json").is_none());
        assert!(Input::parse(r#"{"cwd":"x"}"#).is_none());
    }

    /// Claude Code runs every shell command as `<cmd> 2>&1; echo "exit: $?"`,
    /// so the status it reports is the wrapper's, always 0, and the
    /// command's own is the last line printed. The signals are judged in
    /// order: the client's failure event, the `exit: N` line, the status.
    #[test]
    fn a_wrapped_command_is_judged_by_its_exit_line_not_the_wrappers_status() {
        let wrapped = "npm install left-pad 2>&1; echo \"exit: $?\"";
        let post = |response: Json| {
            let mut event = json!({
                "hook_event_name": "PostToolUse", "tool_name": "Bash",
                "tool_input": { "command": wrapped }, "tool_use_id": "t",
            });
            event["tool_response"] = response;
            Input::parse(&event.to_string()).unwrap()
        };
        let stdout = "npm ERR! code E404\nnpm ERR! 404 Not Found - GET https://registry.npmjs.org/left-pad\n\nexit: 1\n";
        let failed =
            post(json!({ "stdout": stdout, "stderr": "", "exit_code": 0, "interrupted": false }));
        assert_eq!(failed.exit_status, Some(0));
        let v = Verdict::by_action(&failed);
        assert_eq!((v.outcome, v.exit_code), ("failed", Some(1)));
        assert_eq!(
            v.last_line.as_deref(),
            Some("npm ERR! 404 Not Found - GET https://registry.npmjs.org/left-pad")
        );
        // An `exit: 0` line passes, with no last line to report.
        let passed = post(
            json!({ "stdout": "added 1 package\nexit: 0", "stderr": "", "exit_code": 0, "interrupted": false }),
        );
        let v = Verdict::by_action(&passed);
        assert_eq!(
            (v.outcome, v.exit_code, v.last_line),
            ("passed", Some(0), None)
        );
        // Without the wrapper's line, the status the client reports decides.
        let status =
            post(json!({ "stdout": "boom", "stderr": "", "exit_code": 2, "interrupted": false }));
        let v = Verdict::by_action(&status);
        assert_eq!(
            (v.outcome, v.exit_code, v.last_line.as_deref()),
            ("failed", Some(2), Some("boom"))
        );
        // What went to stderr counts as output too, after stdout.
        let stderr =
            post(json!({ "stdout": "", "stderr": "fatal\nexit: 128", "interrupted": false }));
        let v = Verdict::by_action(&stderr);
        assert_eq!(
            (v.outcome, v.exit_code, v.last_line.as_deref()),
            ("failed", Some(128), Some("fatal"))
        );
        // A failure event wins over everything, even an `exit: 0` line.
        let mut event = json!({
            "hook_event_name": "PostToolUseFailure", "tool_name": "Bash",
            "tool_input": { "command": wrapped }, "tool_use_id": "t",
            "error": "Command timed out after 2m 0.0s\nexit: 0",
        });
        event["tool_response"] = json!({ "stdout": "exit: 0", "exit_code": 0 });
        let v = Verdict::by_action(&Input::parse(&event.to_string()).unwrap());
        assert_eq!((v.outcome, v.exit_code), ("failed", None));
        assert_eq!(wrapped_exit_of("exit: 7"), Some(7));
        assert_eq!(wrapped_exit_of("  exit:  7  \n\n"), Some(7));
        assert_eq!(wrapped_exit_of("exit: seven"), None);
        assert_eq!(wrapped_exit_of("done"), None);
        assert_eq!(wrapped_exit_of(""), None);
    }

    #[test]
    fn a_file_is_counted_relative_to_the_project_with_forward_slashes() {
        let root = Path::new("C:\\project");
        assert_eq!(file_key(root, "C:\\project\\src\\index.ts"), "src/index.ts");
        assert_eq!(
            file_key(Path::new("/home/a/p"), "/home/a/p/src/x.rs"),
            "src/x.rs"
        );
        assert_eq!(file_key(Path::new("/home/a/p"), "/etc/hosts"), "/etc/hosts");
    }

    #[test]
    fn without_a_daemon_or_a_repository_the_hook_does_nothing_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let event = format!(
            r#"{{"hook_event_name":"PreToolUse","cwd":{},"tool_name":"Bash","tool_input":{{"command":"rm -rf x"}}}}"#,
            serde_json::to_string(&dir.path().display().to_string()).unwrap()
        );
        assert_eq!(run("claude-code", &event).did, "not in a repository");
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        assert_eq!(
            run("claude-code", &event).did,
            "autopilot is not switched on here"
        );
        std::fs::write(dir.path().join(config::FILE), config::template(None)).unwrap();
        // Switched on, but no daemon: nothing, and nothing started.
        let report = run("claude-code", &event);
        assert!(
            report.did == "no daemon is running" || report.did == "no data directory",
            "{}",
            report.did
        );
        assert_eq!(
            run("claude-code", "garbage").did,
            "the event could not be read"
        );
        assert_eq!(
            run("claude-code", r#"{"hook_event_name":"SessionStart"}"#).did,
            "not an event autopilot acts on"
        );
    }
}
