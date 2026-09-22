//! A client's hooks for autopilot, in its own settings file (DESIGN §5.6).
//!
//! A client with a hook system runs `memfork autopilot hook` before and
//! after a tool call and when the agent stops. Which client, which file and
//! which shape is registry data (`[client.hooks]`); this module writes the
//! shape the registry names, and there is one so far.
//!
//! **Managed** is the same contract as the instruction block: MemFork's
//! entries are recognised by their own arguments, `autopilot hook`, and
//! nothing else in the file is touched. Install splices one entry into each
//! event's list, or adds the list, or adds the `hooks` object, by byte
//! position; removal takes out only MemFork's entries and any list or
//! object that held nothing else. The file's indentation, key order,
//! comments and trailing commas stay as they were, so adding and removing
//! gives back the original bytes.

use std::path::Path;

use serde_json::{json, Value as Json};

use super::edit::{detect_indent, line_indent, pretty_at, strip_jsonc, Object, Scanner};
use crate::init::project::{Change, FilePlan};
use crate::launch::Launch;

/// The arguments that make a hook entry MemFork's.
pub const HOOK_ARGS: [&str; 2] = ["autopilot", "hook"];

/// Seconds a hook that runs before an action may take; the daemon answers
/// in milliseconds, and the client kills anything slower without blocking
/// the action.
pub const PRE_TIMEOUT_SECONDS: u64 = 5;

/// The hook shape one client's registry entry names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub enum Shape {
    /// `hooks.<Event>[].hooks[]` with `matcher`, `type`, `command`,
    /// `args`, `timeout` and `async`.
    #[serde(rename = "claude-code")]
    ClaudeCode,
}

/// One event MemFork hooks, and how.
struct Spec {
    event: &'static str,
    matcher: Option<&'static str>,
    background: bool,
}

/// Before an action: synchronous and quick, so the fork exists when the
/// action runs. After it, and at a stop: in the background, since a check
/// may run a test suite and must not hold the agent.
const SPECS: [Spec; 4] = [
    Spec {
        event: "PreToolUse",
        matcher: Some("Bash|PowerShell|Edit|Write|MultiEdit|NotebookEdit"),
        background: false,
    },
    Spec {
        event: "PostToolUse",
        matcher: Some("Bash|PowerShell"),
        background: true,
    },
    Spec {
        event: "PostToolUseFailure",
        matcher: Some("Bash|PowerShell"),
        background: true,
    },
    Spec {
        event: "Stop",
        matcher: None,
        background: true,
    },
];

/// The events MemFork writes hooks for, for the docs and the tests.
pub fn events() -> Vec<&'static str> {
    SPECS.iter().map(|s| s.event).collect()
}

fn hook_value(spec: &Spec, launch: &Launch, client: &str) -> Json {
    let mut args: Vec<String> = launch.args.clone();
    args.extend(HOOK_ARGS.iter().map(|a| (*a).to_owned()));
    args.push("--client".to_owned());
    args.push(client.to_owned());
    let mut hook = json!({
        "type": "command",
        "command": launch.program.display().to_string(),
        "args": args,
    });
    if spec.background {
        hook["async"] = json!(true);
    } else {
        hook["timeout"] = json!(PRE_TIMEOUT_SECONDS);
    }
    hook
}

fn group_value(spec: &Spec, launch: &Launch, client: &str) -> Json {
    let mut group = serde_json::Map::new();
    if let Some(matcher) = spec.matcher {
        group.insert("matcher".to_owned(), json!(matcher));
    }
    group.insert(
        "hooks".to_owned(),
        json!([hook_value(spec, launch, client)]),
    );
    Json::Object(group)
}

/// The whole `hooks` object as MemFork writes it into an empty file.
fn hooks_value(launch: &Launch, client: &str) -> Json {
    let mut hooks = serde_json::Map::new();
    for spec in &SPECS {
        hooks.insert(
            spec.event.to_owned(),
            json!([group_value(spec, launch, client)]),
        );
    }
    Json::Object(hooks)
}

/// Whether a hook entry is MemFork's: its arguments carry `autopilot hook`.
fn hook_is_ours(hook: &Json) -> bool {
    hook["args"].as_array().is_some_and(|args| {
        args.windows(2)
            .any(|w| w[0] == HOOK_ARGS[0] && w[1] == HOOK_ARGS[1])
    })
}

/// Whether a matcher group holds nothing but MemFork's hooks.
fn group_is_ours(group: &Json) -> bool {
    group["hooks"]
        .as_array()
        .is_some_and(|hooks| !hooks.is_empty() && hooks.iter().all(hook_is_ours))
}

/// Whether the file holds MemFork's hooks for every event, some, or none.
pub fn installed(path: &Path) -> Installed {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Installed::None;
    };
    let Ok(doc) = strip_jsonc(&text)
        .and_then(|t| serde_json::from_str::<Json>(&t).map_err(|e| e.to_string()))
    else {
        return Installed::None;
    };
    let present = SPECS
        .iter()
        .filter(|spec| {
            doc["hooks"][spec.event]
                .as_array()
                .is_some_and(|groups| groups.iter().any(group_is_ours))
        })
        .count();
    match present {
        0 => Installed::None,
        n if n == SPECS.len() => Installed::All,
        _ => Installed::Some,
    }
}

/// How much of MemFork's hooks a file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    /// Every event.
    All,
    /// Some events, not all: edited by hand, or an older install.
    Some,
    /// Nothing of MemFork's.
    None,
}

/// What to do to the client's hooks file at `relative` under `root`.
pub fn plan(
    root: &Path,
    relative: &str,
    display: &str,
    launch: &Launch,
    client: &str,
    remove: bool,
) -> Result<FilePlan, String> {
    let path = relative
        .split('/')
        .fold(root.to_path_buf(), |p, part| p.join(part));
    let before =
        match std::fs::read(&path) {
            Ok(bytes) => Some(String::from_utf8(bytes).map_err(|_| {
                format!("{relative} is not UTF-8 text, so MemFork will not edit it")
            })?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("cannot read {relative}: {e}")),
        };
    let (change, after) = match &before {
        None if remove => (Change::Absent, String::new()),
        None => {
            let (_, text) =
                edit("", launch, client, false).map_err(|why| format!("{relative}: {why}"))?;
            (Change::Create, text)
        }
        Some(text) => {
            edit(text, launch, client, remove).map_err(|why| format!("{relative}: {why}"))?
        }
    };
    Ok(FilePlan {
        relative: relative.to_owned(),
        path,
        clients: vec![display.to_owned()],
        change,
        before,
        after,
    })
}

/// The new text of a file, and what changed.
pub fn edit(
    before: &str,
    launch: &Launch,
    client: &str,
    remove: bool,
) -> Result<(Change, String), String> {
    if remove {
        return take_out(before);
    }
    put_in(before, launch, client)
}

// ---- install -----------------------------------------------------------------

fn put_in(before: &str, launch: &Launch, client: &str) -> Result<(Change, String), String> {
    let unit = detect_indent(before);
    if before.trim().is_empty() {
        let mut after = pretty_at(&json!({ "hooks": hooks_value(launch, client) }), &unit, "")?;
        after.push('\n');
        return Ok((Change::Create, after));
    }
    let root = root_object(before)?;
    let Some(hooks) = root.members.iter().find(|m| m.key == "hooks") else {
        let after = insert_member(before, &root, "hooks", &hooks_value(launch, client), &unit)?;
        return Ok((Change::Append, after));
    };
    if before.as_bytes()[hooks.value_start] != b'{' {
        return Err("`hooks` exists but is not an object".to_owned());
    }
    let mut text = before.to_owned();
    let mut changed = false;
    for spec in &SPECS {
        let wanted = group_value(spec, launch, client);
        let root = root_object(&text)?;
        let hooks = root
            .members
            .iter()
            .find(|m| m.key == "hooks")
            .ok_or_else(|| "`hooks` vanished".to_owned())?;
        let hooks_object = object_at(&text, hooks.value_start)?;
        let Some(member) = hooks_object.members.iter().find(|m| m.key == spec.event) else {
            text = insert_member(&text, &hooks_object, spec.event, &json!([wanted]), &unit)?;
            changed = true;
            continue;
        };
        if text.as_bytes()[member.value_start] != b'[' {
            return Err(format!("`hooks.{}` exists but is not a list", spec.event));
        }
        let array = array_at(&text, member.value_start)?;
        let ours: Vec<(usize, usize)> = array
            .elements
            .iter()
            .copied()
            .filter(|(start, end)| parsed(&text[*start..*end]).is_some_and(|g| group_is_ours(&g)))
            .collect();
        match ours.first() {
            Some((start, end)) => {
                if parsed(&text[*start..*end]).as_ref() == Some(&wanted) {
                    continue;
                }
                let indent = line_indent(&text, *start);
                let mut after = String::with_capacity(text.len() + 128);
                after.push_str(&text[..*start]);
                after.push_str(&pretty_at(&wanted, &unit, &indent)?);
                after.push_str(&text[*end..]);
                text = after;
                changed = true;
            }
            None => {
                text = append_element(&text, &array, &wanted, &unit)?;
                changed = true;
            }
        }
    }
    Ok(if changed {
        (Change::Update, text)
    } else {
        (Change::Unchanged, text)
    })
}

// ---- removal -----------------------------------------------------------------

fn take_out(before: &str) -> Result<(Change, String), String> {
    if before.trim().is_empty() {
        return Ok((Change::Absent, before.to_owned()));
    }
    let root = root_object(before)?;
    let Some(hooks) = root.members.iter().find(|m| m.key == "hooks") else {
        return Ok((Change::Absent, before.to_owned()));
    };
    if before.as_bytes()[hooks.value_start] != b'{' {
        return Ok((Change::Absent, before.to_owned()));
    }
    let mut text = before.to_owned();
    let mut removed = false;
    // Every event, not only the ones MemFork writes: an entry moved by hand
    // is still MemFork's.
    let events: Vec<String> = object_at(&text, hooks.value_start)?
        .members
        .iter()
        .map(|m| m.key.clone())
        .collect();
    for event in events {
        loop {
            let root = root_object(&text)?;
            let Some(hooks) = root.members.iter().find(|m| m.key == "hooks") else {
                break;
            };
            let hooks_object = object_at(&text, hooks.value_start)?;
            let Some((index, member)) = hooks_object
                .members
                .iter()
                .enumerate()
                .find(|(_, m)| m.key == event)
            else {
                break;
            };
            if text.as_bytes()[member.value_start] != b'[' {
                break;
            }
            let array = array_at(&text, member.value_start)?;
            let Some(position) = array.elements.iter().position(|(start, end)| {
                parsed(&text[*start..*end]).is_some_and(|g| group_is_ours(&g))
            }) else {
                break;
            };
            removed = true;
            if array.elements.len() == 1 {
                // It held nothing but MemFork's: the list goes with it.
                text = remove_member(&text, &hooks_object, index);
            } else {
                text = remove_element(&text, &array, position);
            }
        }
    }
    if removed {
        let root = root_object(&text)?;
        if let Some((index, hooks)) = root
            .members
            .iter()
            .enumerate()
            .find(|(_, m)| m.key == "hooks")
        {
            if object_at(&text, hooks.value_start)?.members.is_empty() {
                text = remove_member(&text, &root, index);
            }
        }
    }
    Ok(if removed {
        (Change::Remove, text)
    } else {
        (Change::Absent, text)
    })
}

// ---- the splice --------------------------------------------------------------

/// A list, by position.
struct Array {
    open: usize,
    close: usize,
    elements: Vec<(usize, usize)>,
    trailing_comma: Option<usize>,
}

fn root_object(text: &str) -> Result<Object, String> {
    let mut scanner = Scanner::new(text);
    scanner.skip()?;
    if scanner.peek() != Some(b'{') {
        return Err("this file's top level is not a JSON object".to_owned());
    }
    let object = scanner.object()?;
    scanner.skip()?;
    if scanner.peek().is_some() {
        return Err("this file has text after its top-level object".to_owned());
    }
    Ok(object)
}

fn object_at(text: &str, at: usize) -> Result<Object, String> {
    let mut scanner = Scanner::new(text);
    scanner.i = at;
    scanner.object()
}

fn array_at(text: &str, at: usize) -> Result<Array, String> {
    let mut scanner = Scanner::new(text);
    scanner.i = at;
    if scanner.peek() != Some(b'[') {
        return Err("expected a list".to_owned());
    }
    let open = at;
    scanner.i += 1;
    let mut elements = Vec::new();
    let mut trailing_comma = None;
    loop {
        scanner.skip()?;
        match scanner.peek() {
            Some(b']') => {
                return Ok(Array {
                    open,
                    close: scanner.i,
                    elements,
                    trailing_comma,
                });
            }
            Some(_) => {
                let span = scanner.value()?;
                elements.push(span);
                trailing_comma = None;
                scanner.skip()?;
                match scanner.peek() {
                    Some(b',') => {
                        trailing_comma = Some(scanner.i);
                        scanner.i += 1;
                    }
                    Some(b']') => {}
                    _ => return Err("expected `,` or `]` in a list".to_owned()),
                }
            }
            None => return Err("an unclosed list".to_owned()),
        }
    }
}

fn parsed(text: &str) -> Option<Json> {
    strip_jsonc(text)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

/// `object` with `"key": value` added after its last member.
fn insert_member(
    before: &str,
    object: &Object,
    key: &str,
    value: &Json,
    unit: &str,
) -> Result<String, String> {
    let member_indent = match object.members.first() {
        Some(first) => line_indent(before, first.key_start),
        None => format!("{}{unit}", line_indent(before, object.open)),
    };
    let member_text = format!(
        "{}: {}",
        serde_json::to_string(key).map_err(|e| e.to_string())?,
        pretty_at(value, unit, &member_indent)?
    );
    let mut after = String::with_capacity(before.len() + member_text.len() + 16);
    match (object.members.last(), object.trailing_comma) {
        (Some(_), Some(comma)) => {
            after.push_str(&before[..=comma]);
            after.push('\n');
            after.push_str(&member_indent);
            after.push_str(&member_text);
            after.push(',');
            after.push_str(&before[comma + 1..]);
        }
        (Some(last), None) => {
            after.push_str(&before[..last.value_end]);
            after.push(',');
            after.push('\n');
            after.push_str(&member_indent);
            after.push_str(&member_text);
            after.push_str(&before[last.value_end..]);
        }
        (None, _) => {
            let outer = line_indent(before, object.open);
            after.push_str(&before[..=object.open]);
            after.push('\n');
            after.push_str(&member_indent);
            after.push_str(&member_text);
            after.push('\n');
            after.push_str(&outer);
            after.push_str(&before[object.close..]);
        }
    }
    Ok(after)
}

/// `array` with `value` added after its last element.
fn append_element(before: &str, array: &Array, value: &Json, unit: &str) -> Result<String, String> {
    let indent = match array.elements.first() {
        Some((start, _)) => line_indent(before, *start),
        None => format!("{}{unit}", line_indent(before, array.open)),
    };
    let element = pretty_at(value, unit, &indent)?;
    let mut after = String::with_capacity(before.len() + element.len() + 16);
    match (array.elements.last(), array.trailing_comma) {
        (Some(_), Some(comma)) => {
            after.push_str(&before[..=comma]);
            after.push('\n');
            after.push_str(&indent);
            after.push_str(&element);
            after.push(',');
            after.push_str(&before[comma + 1..]);
        }
        (Some((_, end)), None) => {
            after.push_str(&before[..*end]);
            after.push(',');
            after.push('\n');
            after.push_str(&indent);
            after.push_str(&element);
            after.push_str(&before[*end..]);
        }
        (None, _) => {
            let outer = line_indent(before, array.open);
            after.push_str(&before[..=array.open]);
            after.push('\n');
            after.push_str(&indent);
            after.push_str(&element);
            after.push('\n');
            after.push_str(&outer);
            after.push_str(&before[array.close..]);
        }
    }
    Ok(after)
}

/// `object` without its member at `index`, and without the separator that
/// joined it: the exact reverse of an insertion.
fn remove_member(before: &str, object: &Object, index: usize) -> String {
    let member = &object.members[index];
    let (start, end) = if object.members.len() == 1 {
        (object.open + 1, object.close)
    } else if index > 0 {
        (object.members[index - 1].value_end, member.value_end)
    } else {
        (member.key_start, object.members[1].key_start)
    };
    format!("{}{}", &before[..start], &before[end..])
}

/// `array` without its element at `index`, the same way.
fn remove_element(before: &str, array: &Array, index: usize) -> String {
    let (element_start, element_end) = array.elements[index];
    let (start, end) = if array.elements.len() == 1 {
        (array.open + 1, array.close)
    } else if index > 0 {
        (array.elements[index - 1].1, element_end)
    } else {
        (element_start, array.elements[1].0)
    };
    format!("{}{}", &before[..start], &before[end..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch() -> Launch {
        Launch::program("/opt/memfork/bin/memfork")
    }

    fn install(before: &str) -> (Change, String) {
        edit(before, &launch(), "claude-code", false).unwrap()
    }

    fn remove(before: &str) -> (Change, String) {
        edit(before, &launch(), "claude-code", true).unwrap()
    }

    fn parse(text: &str) -> Json {
        serde_json::from_str(&strip_jsonc(text).unwrap()).unwrap()
    }

    #[test]
    fn an_empty_file_gets_the_whole_shape_and_removal_leaves_an_empty_object() {
        let (change, text) = install("");
        assert_eq!(change, Change::Create);
        let doc = parse(&text);
        for event in events() {
            let group = &doc["hooks"][event][0];
            assert!(group_is_ours(group), "{event}");
            let hook = &group["hooks"][0];
            assert_eq!(hook["type"], "command");
            assert_eq!(hook["command"], "/opt/memfork/bin/memfork");
            assert_eq!(
                hook["args"],
                json!(["autopilot", "hook", "--client", "claude-code"])
            );
        }
        assert_eq!(
            doc["hooks"]["PreToolUse"][0]["matcher"],
            "Bash|PowerShell|Edit|Write|MultiEdit|NotebookEdit"
        );
        assert_eq!(
            doc["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"],
            PRE_TIMEOUT_SECONDS
        );
        assert!(doc["hooks"]["PreToolUse"][0]["hooks"][0]
            .get("async")
            .is_none());
        assert_eq!(doc["hooks"]["PostToolUse"][0]["hooks"][0]["async"], true);
        assert_eq!(doc["hooks"]["Stop"][0].get("matcher"), None);
        // Again: nothing.
        assert_eq!(install(&text).0, Change::Unchanged);
        let (change, after) = remove(&text);
        assert_eq!(change, Change::Remove);
        assert_eq!(after.trim(), "{}");
    }

    #[test]
    fn a_file_without_hooks_gets_them_and_gives_back_its_bytes() {
        let before = "{\n  \"permissions\": {\n    \"allow\": [\"Bash(npm test)\"]\n  }\n}\n";
        let (change, text) = install(before);
        assert_eq!(change, Change::Append);
        assert!(text.starts_with(
            "{\n  \"permissions\": {\n    \"allow\": [\"Bash(npm test)\"]\n  },\n  \"hooks\": {"
        ));
        assert_eq!(parse(&text)["permissions"]["allow"][0], "Bash(npm test)");
        assert_eq!(installed_text(&text), Installed::All);
        let (change, after) = remove(&text);
        assert_eq!(change, Change::Remove);
        assert_eq!(after, before);
    }

    #[test]
    fn other_hooks_in_the_same_events_survive_byte_for_byte_with_comments_and_trailing_commas() {
        let before = "{\n\t// mine\n\t\"hooks\": {\n\t\t\"PreToolUse\": [\n\t\t\t{\n\t\t\t\t\"matcher\": \"Bash\",\n\t\t\t\t\"hooks\": [{\"type\": \"command\", \"command\": \"./lint.sh\"},],\n\t\t\t},\n\t\t],\n\t\t\"Stop\": [\n\t\t\t{\"hooks\": [{\"type\": \"command\", \"command\": \"say done\"}]}\n\t\t],\n\t\t\"SessionStart\": [],\n\t},\n\t\"other\": true,\n}\n";
        let (change, text) = install(before);
        assert_eq!(change, Change::Update);
        let doc = parse(&text);
        assert_eq!(doc["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(doc["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert!(group_is_ours(&doc["hooks"]["PreToolUse"][1]));
        assert_eq!(doc["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(doc["hooks"]["PostToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(doc["hooks"]["SessionStart"], json!([]));
        assert_eq!(doc["other"], true);
        // The comment and the tabs are still there.
        assert!(text.contains("\t// mine\n"));
        assert!(text.contains("\"hooks\": [{\"type\": \"command\", \"command\": \"./lint.sh\"},],"));
        assert_eq!(install(&text).0, Change::Unchanged);

        let (change, after) = remove(&text);
        assert_eq!(change, Change::Remove);
        assert_eq!(after, before);
    }

    #[test]
    fn an_older_entry_pointing_at_another_memfork_is_replaced_not_doubled() {
        let (_, text) = edit("", &Launch::program("/old/memfork"), "claude-code", false).unwrap();
        let (change, updated) = install(&text);
        assert_eq!(change, Change::Update);
        let doc = parse(&updated);
        assert_eq!(doc["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(
            doc["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "/opt/memfork/bin/memfork"
        );
        assert!(!updated.contains("/old/memfork"));
    }

    #[test]
    fn an_empty_hooks_object_is_the_one_stated_edge() {
        let before = "{\n  \"hooks\": {}\n}\n";
        let (change, text) = install(before);
        assert_eq!(change, Change::Update);
        assert_eq!(installed_text(&text), Installed::All);
        let (change, after) = remove(&text);
        assert_eq!(change, Change::Remove);
        // The key MemFork emptied goes too: the file comes back without it.
        assert_eq!(after, "{}\n");
    }

    #[test]
    fn a_file_that_is_not_json_or_whose_hooks_are_not_an_object_is_refused() {
        assert!(edit("[1, 2]", &launch(), "claude-code", false).is_err());
        assert!(edit("{\"hooks\": 3}", &launch(), "claude-code", false).is_err());
        assert!(edit(
            "{\"hooks\": {\"Stop\": {}}}",
            &launch(),
            "claude-code",
            false
        )
        .is_err());
        assert!(edit("{ oops", &launch(), "claude-code", false).is_err());
        // Removal from a file with no hooks changes nothing.
        assert_eq!(remove("{\"a\": 1}\n").0, Change::Absent);
        assert_eq!(remove("").0, Change::Absent);
    }

    #[test]
    fn a_hand_moved_entry_is_still_ours_and_is_taken_out() {
        let before = "{\n  \"hooks\": {\n    \"SessionStart\": [\n      {\"hooks\": [{\"type\": \"command\", \"command\": \"x\", \"args\": [\"autopilot\", \"hook\"]}]}\n    ]\n  }\n}\n";
        let (change, after) = remove(before);
        assert_eq!(change, Change::Remove);
        assert_eq!(after, "{}\n");
    }

    fn installed_text(text: &str) -> Installed {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.local.json");
        std::fs::write(&path, text).unwrap();
        installed(&path)
    }

    #[test]
    fn the_plan_reads_and_creates_under_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(
            dir.path(),
            ".claude/settings.local.json",
            "Claude Code",
            &launch(),
            "claude-code",
            false,
        )
        .unwrap();
        assert_eq!(plan.change, Change::Create);
        assert!(plan.before.is_none());
        assert_eq!(
            plan.path,
            dir.path().join(".claude").join("settings.local.json")
        );
        crate::init::project::apply(&plan).unwrap();
        assert_eq!(installed(&plan.path), Installed::All);
        let again = super::plan(
            dir.path(),
            ".claude/settings.local.json",
            "Claude Code",
            &launch(),
            "claude-code",
            false,
        )
        .unwrap();
        assert_eq!(again.change, Change::Unchanged);
        let gone = super::plan(
            dir.path(),
            ".claude/settings.local.json",
            "Claude Code",
            &launch(),
            "claude-code",
            true,
        )
        .unwrap();
        assert_eq!(gone.change, Change::Remove);
        assert!(gone.left_empty() || gone.after.trim() == "{}");
    }
}
