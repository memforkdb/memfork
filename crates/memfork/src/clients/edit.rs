//! Adding one entry to a config file MemFork does not own.
//!
//! The rule this module exists to keep: **nothing in the file changes except
//! the MemFork entry.** Not a comment, not a key order, not an unrelated
//! value, not a setting MemFork has never heard of.
//!
//! - **TOML** is edited with `toml_edit`, which keeps the document's comments,
//!   key order and whitespace exactly as they were.
//! - **JSON** is spliced: the text is scanned for the servers object and the
//!   MemFork member, and only that stretch is replaced or inserted. Whitespace,
//!   key order, comments and trailing commas — which several editors keep in
//!   their settings — survive byte for byte.
//!
//! Every write is preceded by a timestamped backup beside the original.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value as Json};

use super::{ClientFile, CommandStyle, Format, SERVER_ARGS, SERVER_NAME};
use crate::launch::Launch;

/// What editing a config file would do, or did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// The file does not have a MemFork entry yet.
    Add,
    /// The file has one, and it differs from what MemFork would write.
    Update,
    /// The file already says exactly this. Nothing to do.
    Unchanged,
}

/// A prepared edit: the new file contents, and what changed.
#[derive(Debug, Clone)]
pub struct Edit {
    /// The file this applies to.
    pub path: PathBuf,
    /// Whether it adds, updates or does nothing.
    pub change: Change,
    /// The file as it is now, or empty if it does not exist.
    pub before: String,
    /// The file as it would be.
    pub after: String,
    /// Whether the file had to be created.
    pub creates_file: bool,
}

impl Edit {
    /// A unified-ish diff of just the lines that differ, for `--dry-run`.
    pub fn diff(&self) -> String {
        let before: Vec<&str> = self.before.lines().collect();
        let after: Vec<&str> = self.after.lines().collect();
        let mut out = String::new();
        // A line-level diff is enough here: the edit touches one contiguous
        // region, and the point is to let a person see exactly what changes.
        let common_prefix = before
            .iter()
            .zip(&after)
            .take_while(|(a, b)| a == b)
            .count();
        let common_suffix = before[common_prefix..]
            .iter()
            .rev()
            .zip(after[common_prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        for line in &before[common_prefix..before.len() - common_suffix] {
            out.push_str(&format!("      - {line}\n"));
        }
        for line in &after[common_prefix..after.len() - common_suffix] {
            out.push_str(&format!("      + {line}\n"));
        }
        if out.is_empty() {
            out.push_str("      (no change)\n");
        }
        out
    }
}

/// Work out what registering MemFork in this file would change.
pub fn plan(file: &ClientFile, path: &Path, launch: &Launch) -> Result<Edit, String> {
    let before = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let creates_file = before.is_empty() && !path.exists();

    let (after, change) = match file.format {
        Format::Json | Format::Jsonc => edit_json(file, &before, launch)?,
        Format::Toml => edit_toml(file, &before, launch)?,
    };

    Ok(Edit {
        path: path.to_path_buf(),
        change,
        before,
        after,
        creates_file,
    })
}

/// Write a planned edit, keeping a timestamped backup of the original.
pub fn apply(edit: &Edit) -> Result<Option<PathBuf>, String> {
    if edit.change == Change::Unchanged {
        return Ok(None);
    }
    if let Some(parent) = edit.path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
    }

    // Back up whatever is there before replacing it. A file that did not exist
    // has nothing to lose, so no backup is made for a creation.
    let backup = if edit.creates_file {
        None
    } else {
        let backup = backup_path(&edit.path);
        std::fs::write(&backup, &edit.before)
            .map_err(|e| format!("cannot write the backup {}: {e}", backup.display()))?;
        Some(backup)
    };

    std::fs::write(&edit.path, &edit.after)
        .map_err(|e| format!("cannot write {}: {e}", edit.path.display()))?;
    Ok(backup)
}

/// `<file>.memfork-backup-<seconds since the epoch>`.
fn backup_path(path: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".memfork-backup-{stamp}"));
    PathBuf::from(name)
}

// ---- the entry ---------------------------------------------------------------

/// The MemFork entry as data, in the shape this client's file wants: a
/// `type` where the client needs one, the command split or as one list, and
/// whatever keys the client requires on every entry.
fn entry_value(file: &ClientFile, launch: &Launch) -> Json {
    let mut entry = Map::new();
    if let Some(kind) = &file.stdio_type {
        entry.insert("type".to_owned(), Json::String(kind.clone()));
    }
    let program = launch.program.display().to_string();
    let args: Vec<Json> = launch
        .args
        .iter()
        .cloned()
        .chain(SERVER_ARGS.iter().map(|a| (*a).to_owned()))
        .map(Json::String)
        .collect();
    match file.command_style {
        CommandStyle::Split => {
            entry.insert("command".to_owned(), Json::String(program));
            entry.insert("args".to_owned(), Json::Array(args));
        }
        CommandStyle::Array => {
            let mut command = vec![Json::String(program)];
            command.extend(args);
            entry.insert("command".to_owned(), Json::Array(command));
        }
    }
    if let Some(extra) = &file.extra {
        for (key, value) in extra {
            entry.insert(key.clone(), toml_to_json(value));
        }
    }
    Json::Object(entry)
}

fn toml_to_json(value: &toml::Value) -> Json {
    match value {
        toml::Value::String(s) => Json::String(s.clone()),
        toml::Value::Integer(i) => Json::from(*i),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number),
        toml::Value::Boolean(b) => Json::Bool(*b),
        toml::Value::Datetime(d) => Json::String(d.to_string()),
        toml::Value::Array(items) => Json::Array(items.iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => Json::Object(
            table
                .iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect(),
        ),
    }
}

// ---- JSON ------------------------------------------------------------------
//
// A splice editor. The file is scanned for the one object that holds the
// servers and the one member that is MemFork's, and only that stretch of text
// is replaced or inserted. Nothing is parsed into a tree and written back out,
// so the rest of the file — its whitespace, its key order, and the comments
// and trailing commas some editors keep in their settings — is untouched byte
// for byte. Comments and trailing commas are tolerated everywhere; a client
// that forbids them never had any for MemFork to preserve.

/// One `"key": value` inside an object, by position.
struct Member {
    key: String,
    /// Where the key's opening quote is.
    key_start: usize,
    /// The value's first byte, and one past its last.
    value_start: usize,
    value_end: usize,
}

/// An object, by position.
struct Object {
    /// The `{`.
    open: usize,
    /// The `}`.
    close: usize,
    members: Vec<Member>,
    /// A comma after the last member with nothing but space and comments
    /// before the `}`, as some editors leave.
    trailing_comma: Option<usize>,
}

struct Scanner<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Scanner<'a> {
    fn new(text: &'a str) -> Self {
        Scanner {
            s: text.as_bytes(),
            i: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn fail(&self, what: &str) -> String {
        format!(
            "this file is not valid JSON, so MemFork will not touch it: {what} at byte {}",
            self.i
        )
    }

    /// Skip whitespace, `//` comments and `/* */` comments.
    fn skip(&mut self) -> Result<(), String> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r') => self.i += 1,
                Some(b'/') => match self.s.get(self.i + 1) {
                    Some(b'/') => {
                        while let Some(c) = self.peek() {
                            if c == b'\n' {
                                break;
                            }
                            self.i += 1;
                        }
                    }
                    Some(b'*') => {
                        self.i += 2;
                        loop {
                            match self.peek() {
                                None => return Err(self.fail("an unclosed comment")),
                                Some(b'*') if self.s.get(self.i + 1) == Some(&b'/') => {
                                    self.i += 2;
                                    break;
                                }
                                Some(_) => self.i += 1,
                            }
                        }
                    }
                    _ => return Err(self.fail("a stray `/`")),
                },
                _ => return Ok(()),
            }
        }
    }

    /// Scan a string at `"`, returning its decoded text.
    fn string(&mut self) -> Result<String, String> {
        let start = self.i;
        if self.peek() != Some(b'"') {
            return Err(self.fail("expected a string"));
        }
        self.i += 1;
        loop {
            match self.peek() {
                None => return Err(self.fail("an unterminated string")),
                Some(b'\\') => self.i += 2,
                Some(b'"') => {
                    self.i += 1;
                    break;
                }
                Some(_) => self.i += 1,
            }
        }
        let raw =
            std::str::from_utf8(&self.s[start..self.i]).map_err(|e| self.fail(&e.to_string()))?;
        serde_json::from_str::<String>(raw).map_err(|e| self.fail(&e.to_string()))
    }

    /// Scan any value, returning its span.
    fn value(&mut self) -> Result<(usize, usize), String> {
        let start = self.i;
        match self.peek() {
            Some(b'{') => {
                let object = self.object()?;
                Ok((start, object.close + 1))
            }
            Some(b'[') => {
                self.i += 1;
                self.skip()?;
                if self.peek() == Some(b']') {
                    self.i += 1;
                    return Ok((start, self.i));
                }
                loop {
                    self.value()?;
                    self.skip()?;
                    match self.peek() {
                        Some(b',') => {
                            self.i += 1;
                            self.skip()?;
                            if self.peek() == Some(b']') {
                                self.i += 1;
                                return Ok((start, self.i));
                            }
                        }
                        Some(b']') => {
                            self.i += 1;
                            return Ok((start, self.i));
                        }
                        _ => return Err(self.fail("expected `,` or `]`")),
                    }
                }
            }
            Some(b'"') => {
                self.string()?;
                Ok((start, self.i))
            }
            Some(c) if c == b'-' || c.is_ascii_alphanumeric() => {
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'+' | b'.') {
                        self.i += 1;
                    } else {
                        break;
                    }
                }
                Ok((start, self.i))
            }
            _ => Err(self.fail("expected a value")),
        }
    }

    /// Scan an object at `{`.
    fn object(&mut self) -> Result<Object, String> {
        let open = self.i;
        if self.peek() != Some(b'{') {
            return Err(self.fail("expected `{`"));
        }
        self.i += 1;
        let mut members = Vec::new();
        let mut trailing_comma = None;
        loop {
            self.skip()?;
            match self.peek() {
                Some(b'}') => {
                    let close = self.i;
                    self.i += 1;
                    return Ok(Object {
                        open,
                        close,
                        members,
                        trailing_comma,
                    });
                }
                Some(b'"') => {
                    let key_start = self.i;
                    let key = self.string()?;
                    self.skip()?;
                    if self.peek() != Some(b':') {
                        return Err(self.fail("expected `:`"));
                    }
                    self.i += 1;
                    self.skip()?;
                    let (value_start, value_end) = self.value()?;
                    members.push(Member {
                        key,
                        key_start,
                        value_start,
                        value_end,
                    });
                    trailing_comma = None;
                    self.skip()?;
                    match self.peek() {
                        Some(b',') => {
                            trailing_comma = Some(self.i);
                            self.i += 1;
                        }
                        Some(b'}') => {}
                        _ => return Err(self.fail("expected `,` or `}`")),
                    }
                }
                None => return Err(self.fail("an unclosed object")),
                _ => return Err(self.fail("expected a key")),
            }
        }
    }
}

/// JSON with its comments and trailing commas taken out, for `serde_json`.
fn strip_jsonc(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut scanner = Scanner::new(text);
    // Copy every token; skipping is what drops comments. Trailing commas are
    // dropped by looking past them.
    loop {
        scanner.skip()?;
        let Some(c) = scanner.peek() else { break };
        match c {
            b'"' => {
                let start = scanner.i;
                scanner.string()?;
                out.push_str(&text[start..scanner.i]);
            }
            b',' => {
                scanner.i += 1;
                let save = scanner.i;
                scanner.skip()?;
                if !matches!(scanner.peek(), Some(b'}' | b']')) {
                    out.push(',');
                }
                scanner.i = save;
            }
            _ => {
                out.push(c as char);
                scanner.i += 1;
            }
        }
    }
    Ok(out)
}

/// The indentation the file already uses, so a rewrite matches its house style.
fn detect_indent(source: &str) -> String {
    for line in source.lines() {
        let spaces = line.len() - line.trim_start_matches(' ').len();
        if spaces > 0 {
            return " ".repeat(spaces);
        }
        if line.starts_with('\t') {
            return "\t".to_owned();
        }
    }
    "  ".to_owned()
}

/// The whitespace at the start of the line `at` is on.
fn line_indent(text: &str, at: usize) -> String {
    let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
    text[line_start..at]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

/// A value as pretty JSON with `unit` indentation, every line after the
/// first prefixed by `indent`, so it sits inside an object at that depth.
fn pretty_at(value: &Json, unit: &str, indent: &str) -> Result<String, String> {
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(unit.as_bytes());
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    serde::Serialize::serialize(value, &mut ser)
        .map_err(|e| format!("cannot write the updated JSON: {e}"))?;
    let text = String::from_utf8(buf).map_err(|e| format!("the updated JSON is not UTF-8: {e}"))?;
    Ok(text
        .lines()
        .enumerate()
        .map(|(n, line)| {
            if n == 0 {
                line.to_owned()
            } else {
                format!("{indent}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn edit_json(file: &ClientFile, before: &str, launch: &Launch) -> Result<(String, Change), String> {
    let entry = entry_value(file, launch);
    let unit = detect_indent(before);

    if before.trim().is_empty() {
        // Nothing to preserve: write the whole shape.
        let mut root = Json::Object(Map::new());
        let mut cursor = &mut root;
        for key in &file.server_map {
            cursor = cursor
                .as_object_mut()
                .ok_or_else(|| "the server map is not an object".to_owned())?
                .entry(key.clone())
                .or_insert_with(|| Json::Object(Map::new()));
        }
        cursor
            .as_object_mut()
            .ok_or_else(|| "the server map is not an object".to_owned())?
            .insert(SERVER_NAME.to_owned(), entry);
        let mut after = pretty_at(&root, &unit, "")?;
        after.push('\n');
        return Ok((after, Change::Add));
    }

    let mut scanner = Scanner::new(before);
    scanner.skip()?;
    if scanner.peek() != Some(b'{') {
        return Err("this file's top level is not a JSON object".to_owned());
    }
    let mut object = scanner.object()?;
    scanner.skip()?;
    if scanner.peek().is_some() {
        return Err(scanner.fail("text after the top-level object"));
    }

    // Walk to the server map through what exists; whatever is missing from
    // the path is created inside the deepest object that is there.
    let mut missing: &[String] = &file.server_map;
    for (n, key) in file.server_map.iter().enumerate() {
        let Some(member) = object.members.iter().find(|m| &m.key == key) else {
            missing = &file.server_map[n..];
            break;
        };
        if before.as_bytes()[member.value_start] != b'{' {
            return Err(format!("`{key}` exists but is not an object"));
        }
        let mut inner = Scanner::new(before);
        inner.i = member.value_start;
        object = inner.object()?;
        missing = &file.server_map[n + 1..];
    }

    // The member text to put in `object`: MemFork's entry, wrapped in any
    // containers still missing.
    let member_indent = match object.members.first() {
        Some(first) => line_indent(before, first.key_start),
        None => format!("{}{unit}", line_indent(before, object.open)),
    };
    let (member_key, member_value) = match missing.split_first() {
        None => (SERVER_NAME, entry.clone()),
        Some((first, rest)) => {
            let mut value = Json::Object(Map::from_iter([(SERVER_NAME.to_owned(), entry.clone())]));
            for key in rest.iter().rev() {
                value = Json::Object(Map::from_iter([(key.clone(), value)]));
            }
            (first.as_str(), value)
        }
    };
    let member_text = format!(
        "{}: {}",
        serde_json::to_string(member_key).map_err(|e| e.to_string())?,
        pretty_at(&member_value, &unit, &member_indent)?
    );

    if missing.is_empty() {
        if let Some(existing) = object.members.iter().find(|m| m.key == SERVER_NAME) {
            let current = strip_jsonc(&before[existing.value_start..existing.value_end])
                .and_then(|t| serde_json::from_str::<Json>(&t).map_err(|e| e.to_string()));
            if current.as_ref().ok() == Some(&entry) {
                return Ok((before.to_owned(), Change::Unchanged));
            }
            let mut after = String::with_capacity(before.len() + 128);
            after.push_str(&before[..existing.value_start]);
            after.push_str(&pretty_at(&entry, &unit, &member_indent)?);
            after.push_str(&before[existing.value_end..]);
            return Ok((after, Change::Update));
        }
    }

    // Insert a new member into `object`.
    let mut after = String::with_capacity(before.len() + member_text.len() + 16);
    match (object.members.last(), object.trailing_comma) {
        (Some(_), Some(comma)) => {
            // Keep the file's trailing-comma style: the new member takes the
            // comma with it.
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
            // `{}` with nothing but space or comments inside.
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
    Ok((after, Change::Add))
}

// ---- TOML ------------------------------------------------------------------

fn edit_toml(file: &ClientFile, before: &str, launch: &Launch) -> Result<(String, Change), String> {
    let mut doc: toml_edit::DocumentMut = before
        .parse()
        .map_err(|e| format!("this file is not valid TOML, so MemFork will not touch it: {e}"))?;

    // Walk to the server table, creating only what is missing. Tables created
    // here are implicit, so they do not print an empty `[mcp_servers]` header
    // of their own.
    let mut cursor = doc.as_table_mut();
    for key in &file.server_map {
        if !cursor.contains_key(key) {
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            cursor[key] = toml_edit::Item::Table(table);
        }
        cursor = cursor[key]
            .as_table_mut()
            .ok_or_else(|| format!("`{key}` exists but is not a table"))?;
    }

    let wanted = toml_entry(file, launch)?;
    let change = match cursor.get(SERVER_NAME) {
        Some(existing) => {
            if existing.to_string().trim()
                == toml_edit::Item::Table(wanted.clone()).to_string().trim()
            {
                Change::Unchanged
            } else {
                Change::Update
            }
        }
        None => Change::Add,
    };
    if change != Change::Unchanged {
        cursor[SERVER_NAME] = toml_edit::Item::Table(wanted);
    }

    Ok((doc.to_string(), change))
}

/// The entry as a TOML table: the same data as the JSON one, in this syntax.
fn toml_entry(file: &ClientFile, launch: &Launch) -> Result<toml_edit::Table, String> {
    let Json::Object(entry) = entry_value(file, launch) else {
        return Err("the entry is not an object".to_owned());
    };
    let mut table = toml_edit::Table::new();
    for (key, value) in &entry {
        table[key] = toml_edit::Item::Value(json_to_toml(value)?);
    }
    Ok(table)
}

fn json_to_toml(value: &Json) -> Result<toml_edit::Value, String> {
    Ok(match value {
        Json::String(s) => toml_edit::Value::from(s.as_str()),
        Json::Bool(b) => toml_edit::Value::from(*b),
        Json::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => toml_edit::Value::from(i),
            (None, Some(f)) => toml_edit::Value::from(f),
            _ => return Err(format!("`{n}` cannot be written as TOML")),
        },
        Json::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(json_to_toml(item)?);
            }
            toml_edit::Value::Array(array)
        }
        Json::Object(map) => {
            let mut inline = toml_edit::InlineTable::new();
            for (k, v) in map {
                inline.insert(k, json_to_toml(v)?);
            }
            toml_edit::Value::InlineTable(inline)
        }
        Json::Null => return Err("null cannot be written as TOML".to_owned()),
    })
}

/// What command a client's config already registers for MemFork, if any.
///
/// Read rather than inferred: when a registration turns out to point at
/// another MemFork, the useful thing to tell someone is which one.
pub fn registered_command(file: &ClientFile, path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let command_of = |node: &Json| -> Option<String> {
        match node.get("command")? {
            Json::String(s) => Some(s.clone()),
            Json::Array(items) => items.first().and_then(Json::as_str).map(str::to_owned),
            _ => None,
        }
    };
    match file.format {
        Format::Json | Format::Jsonc => {
            let doc: Json = serde_json::from_str(&strip_jsonc(&text).ok()?).ok()?;
            let mut node = &doc;
            for key in &file.server_map {
                node = node.get(key)?;
            }
            command_of(node.get(SERVER_NAME)?)
        }
        Format::Toml => {
            let doc: toml_edit::DocumentMut = text.parse().ok()?;
            let mut node = doc.as_item();
            for key in &file.server_map {
                node = node.get(key)?;
            }
            let command = node.get(SERVER_NAME)?.get("command")?;
            command
                .as_str()
                .map(str::to_owned)
                .or_else(|| command.as_array()?.get(0)?.as_str().map(str::to_owned))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients;

    fn file_for(id: &str) -> ClientFile {
        clients::find(id)
            .and_then(|c| c.file)
            .unwrap_or_else(|| panic!("{id} registers by file"))
    }

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("fixture written");
        path
    }

    #[test]
    fn json_adds_the_entry_and_leaves_everything_else_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = r#"{
  "someUnrelatedSetting": true,
  "mcpServers": {
    "other-server": {
      "command": "other",
      "args": ["serve"],
      "env": { "KEY": "value" }
    }
  },
  "zzzLastKey": [1, 2, 3]
}
"#;
        let path = write(dir.path(), "mcp.json", original);
        let edit = plan(&file_for("cursor"), &path, &Launch::program("memfork")).expect("planned");
        assert_eq!(edit.change, Change::Add);
        apply(&edit).expect("applied");

        let after: Json = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let before: Json = serde_json::from_str(original).unwrap();

        // The unrelated server is untouched, byte for byte.
        assert_eq!(
            after["mcpServers"]["other-server"],
            before["mcpServers"]["other-server"]
        );
        // So is every unrelated top-level key.
        assert_eq!(
            after["someUnrelatedSetting"],
            before["someUnrelatedSetting"]
        );
        assert_eq!(after["zzzLastKey"], before["zzzLastKey"]);
        // And key order is preserved, not alphabetised.
        let keys: Vec<&String> = after.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec!["someUnrelatedSetting", "mcpServers", "zzzLastKey"]
        );
        // The entry itself is what we meant to write.
        assert_eq!(after["mcpServers"]["memfork"]["command"], "memfork");
        assert_eq!(after["mcpServers"]["memfork"]["args"][0], "mcp");
    }

    #[test]
    fn json_writes_the_type_field_only_where_the_registry_says_to() {
        let dir = tempfile::tempdir().expect("tempdir");

        let path = write(dir.path(), "a.json", "{}\n");
        let edit =
            plan(&file_for("claude-code"), &path, &Launch::program("memfork")).expect("planned");
        let v: Json = serde_json::from_str(&edit.after).unwrap();
        assert_eq!(v["mcpServers"]["memfork"]["type"], "stdio");

        let path = write(dir.path(), "b.json", "{}\n");
        let edit = plan(&file_for("cursor"), &path, &Launch::program("memfork")).expect("planned");
        let v: Json = serde_json::from_str(&edit.after).unwrap();
        assert!(v["mcpServers"]["memfork"].get("type").is_none());
    }

    #[test]
    fn json_keeps_the_files_own_indentation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "wide.json", "{\n    \"a\": 1\n}\n");
        let edit = plan(&file_for("cursor"), &path, &Launch::program("memfork")).expect("planned");
        assert!(
            edit.after.contains("\n    \"mcpServers\""),
            "four-space indentation was not kept:\n{}",
            edit.after
        );
    }

    #[test]
    fn toml_preserves_comments_and_formatting_exactly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = r#"# My Codex configuration.
model = "something"      # trailing comment

[mcp_servers.other]
command = "other"
args = ["serve"]

# A comment about the next section.
[ui]
theme   = "dark"
"#;
        let path = write(dir.path(), "config.toml", original);
        let edit = plan(&file_for("codex"), &path, &Launch::program("memfork")).expect("planned");
        assert_eq!(edit.change, Change::Add);

        // Every original line survives, including comments and odd spacing.
        for line in original.lines() {
            assert!(
                edit.after.contains(line),
                "TOML edit lost the line `{line}`:\n{}",
                edit.after
            );
        }
        assert!(edit.after.contains("[mcp_servers.memfork]"));
        assert!(edit.after.contains(r#"command = "memfork""#));
    }

    #[test]
    fn a_second_run_changes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (id, name, seed) in [
            ("cursor", "mcp.json", "{}\n"),
            ("codex", "config.toml", "model = \"x\"\n"),
        ] {
            let path = write(dir.path(), name, seed);
            let file = file_for(id);

            let first = plan(&file, &path, &Launch::program("memfork")).expect("planned");
            assert_eq!(first.change, Change::Add, "{id}");
            apply(&first).expect("applied");
            let after_first = std::fs::read_to_string(&path).unwrap();

            let second = plan(&file, &path, &Launch::program("memfork")).expect("planned again");
            assert_eq!(second.change, Change::Unchanged, "{id} was not idempotent");
            assert!(apply(&second).expect("no-op").is_none());
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                after_first,
                "{id} rewrote the file on a second run"
            );
        }
    }

    #[test]
    fn a_changed_command_is_an_update_not_a_duplicate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "mcp.json", "{}\n");
        let file = file_for("cursor");

        apply(&plan(&file, &path, &Launch::program("/old/memfork")).expect("planned"))
            .expect("applied");
        let edit = plan(&file, &path, &Launch::program("/new/memfork")).expect("planned");
        assert_eq!(edit.change, Change::Update);
        apply(&edit).expect("applied");

        let v: Json = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["memfork"]["command"], "/new/memfork");
        assert_eq!(
            v["mcpServers"].as_object().unwrap().len(),
            1,
            "the update duplicated the entry instead of replacing it"
        );
    }

    #[test]
    fn a_backup_is_kept_before_any_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = "{\n  \"keep\": \"me\"\n}\n";
        let path = write(dir.path(), "mcp.json", original);
        let backup =
            apply(&plan(&file_for("cursor"), &path, &Launch::program("memfork")).expect("planned"))
                .expect("applied")
                .expect("a backup was made");

        assert!(backup.to_string_lossy().contains(".memfork-backup-"));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);
    }

    #[test]
    fn creating_a_new_file_needs_no_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("mcp.json");
        let edit = plan(&file_for("cursor"), &path, &Launch::program("memfork")).expect("planned");
        assert!(edit.creates_file);
        assert!(apply(&edit).expect("applied").is_none());
        assert!(path.exists());
    }

    #[test]
    fn a_malformed_file_is_refused_rather_than_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let broken = "{ this is not json";
        let path = write(dir.path(), "mcp.json", broken);
        let err =
            plan(&file_for("cursor"), &path, &Launch::program("memfork")).expect_err("refused");
        assert!(err.contains("not valid JSON"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);

        let path = write(dir.path(), "config.toml", "[[[nope");
        let err =
            plan(&file_for("codex"), &path, &Launch::program("memfork")).expect_err("refused");
        assert!(err.contains("not valid TOML"), "{err}");
    }

    #[test]
    fn the_diff_shows_only_what_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "config.toml", "model = \"x\"\n");
        let edit = plan(&file_for("codex"), &path, &Launch::program("memfork")).expect("planned");
        let diff = edit.diff();
        assert!(diff.contains("+ [mcp_servers.memfork]"), "{diff}");
        assert!(
            !diff.contains("- model"),
            "unrelated lines appeared:\n{diff}"
        );
    }

    #[test]
    fn jsonc_comments_and_trailing_commas_survive_byte_for_byte() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Zed's settings: comments, a trailing comma, and unrelated keys.
        let original = "// Zed settings\n{\n  \"theme\": \"One Dark\", // mine\n  \"context_servers\": {\n    /* another server */\n    \"other\": {\n      \"command\": \"other\",\n      \"args\": [\"serve\",],\n    },\n  },\n  \"vim_mode\": true,\n}\n";
        let path = write(dir.path(), "settings.json", original);
        let edit = plan(&file_for("zed"), &path, &Launch::program("memfork")).expect("planned");
        assert_eq!(edit.change, Change::Add);
        // Every original byte is still there, in order: the edit is an insertion.
        let (before, after) = (edit.before.as_str(), edit.after.as_str());
        let inserted_at = after
            .char_indices()
            .zip(before.chars())
            .find(|((_, a), b)| a != b)
            .map_or(after.len(), |((i, _), _)| i);
        let tail = &after[after.len() - (before.len() - inserted_at)..];
        assert_eq!(
            tail,
            &before[inserted_at..],
            "text after the insertion changed"
        );
        assert!(after.contains("// Zed settings"), "{after}");
        assert!(after.contains("/* another server */"), "{after}");
        assert!(after.contains("\"vim_mode\": true,\n}"), "{after}");
        // The new member took the trailing comma with it, so the file's style holds.
        assert!(after.contains("\"memfork\": {\n      \"command\": \"memfork\",\n      \"args\": [\n        \"mcp\"\n      ]\n    },\n  },"), "{after}");
        // And it parses, comments and all, to what was meant.
        let stripped = strip_jsonc(after).unwrap();
        let v: Json = serde_json::from_str(&stripped).unwrap();
        assert_eq!(v["context_servers"]["memfork"]["command"], "memfork");
        assert_eq!(v["context_servers"]["other"]["args"][0], "serve");

        // A second run finds it and changes nothing.
        apply(&edit).expect("applied");
        let again = plan(&file_for("zed"), &path, &Launch::program("memfork")).expect("planned");
        assert_eq!(again.change, Change::Unchanged);
    }

    #[test]
    fn the_command_can_be_one_list_with_a_type_and_the_client_can_require_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        // OpenCode: `"type": "local"` and one command list.
        let path = write(
            dir.path(),
            "opencode.json",
            "{\n  \"$schema\": \"https://opencode.ai/config.json\"\n}\n",
        );
        let edit = plan(
            &file_for("opencode"),
            &path,
            &Launch::program("/opt/memfork"),
        )
        .expect("planned");
        let v: Json = serde_json::from_str(&edit.after).unwrap();
        assert_eq!(v["mcp"]["memfork"]["type"], "local");
        assert_eq!(
            v["mcp"]["memfork"]["command"],
            serde_json::json!(["/opt/memfork", "mcp"])
        );
        assert!(v["mcp"]["memfork"].get("args").is_none());
        assert_eq!(v["$schema"], "https://opencode.ai/config.json");

        // Copilot CLI: `tools` on every entry, as its documentation requires.
        let path = write(dir.path(), "mcp-config.json", "{}\n");
        let edit =
            plan(&file_for("copilot-cli"), &path, &Launch::program("memfork")).expect("planned");
        let v: Json = serde_json::from_str(&edit.after).unwrap();
        assert_eq!(v["mcpServers"]["memfork"]["type"], "local");
        assert_eq!(
            v["mcpServers"]["memfork"]["tools"],
            serde_json::json!(["*"])
        );
        assert_eq!(v["mcpServers"]["memfork"]["command"], "memfork");
    }

    #[test]
    fn missing_containers_are_created_inside_what_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A VS Code mcp.json with inputs but no servers yet.
        let original = "{\n    \"inputs\": []\n}\n";
        let path = write(dir.path(), "mcp.json", original);
        let edit = plan(&file_for("vscode"), &path, &Launch::program("memfork")).expect("planned");
        assert_eq!(
            edit.after,
            "{\n    \"inputs\": [],\n    \"servers\": {\n        \"memfork\": {\n            \"type\": \"stdio\",\n            \"command\": \"memfork\",\n            \"args\": [\n                \"mcp\"\n            ]\n        }\n    }\n}\n"
        );
        // An empty servers object gets the member on its own lines.
        let path = write(dir.path(), "empty.json", "{\n  \"servers\": {}\n}\n");
        let edit = plan(&file_for("vscode"), &path, &Launch::program("memfork")).expect("planned");
        assert!(
            edit.after
                .starts_with("{\n  \"servers\": {\n    \"memfork\": {\n      \"type\": \"stdio\","),
            "{}",
            edit.after
        );
        assert!(edit.after.ends_with("    }\n  }\n}\n"), "{}", edit.after);
    }

    #[test]
    fn a_registered_command_is_read_whatever_shape_it_has() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "opencode.json", "{}\n");
        apply(
            &plan(
                &file_for("opencode"),
                &path,
                &Launch::program("/old/memfork"),
            )
            .expect("planned"),
        )
        .expect("applied");
        assert_eq!(
            registered_command(&file_for("opencode"), &path).as_deref(),
            Some("/old/memfork")
        );
        let path = write(dir.path(), "settings.json", "// comment\n{\n}\n");
        apply(&plan(&file_for("zed"), &path, &Launch::program("/old/memfork")).expect("planned"))
            .expect("applied");
        assert_eq!(
            registered_command(&file_for("zed"), &path).as_deref(),
            Some("/old/memfork")
        );
    }

    #[test]
    fn toml_carries_a_type_and_extra_keys_too() {
        // The same entry data, in TOML syntax, for a client that wanted them.
        let mut file = file_for("codex");
        file.stdio_type = Some("stdio".to_owned());
        file.extra = Some(toml::toml! { enabled = true }.clone());
        let table = toml_entry(&file, &Launch::program("memfork")).expect("entry");
        let text = toml_edit::Item::Table(table).to_string();
        assert!(text.contains("type = \"stdio\""), "{text}");
        assert!(text.contains("enabled = true"), "{text}");
        assert!(text.contains("command = \"memfork\""), "{text}");
    }
}
