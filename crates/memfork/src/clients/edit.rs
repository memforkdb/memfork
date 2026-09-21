//! Adding one entry to a config file MemFork does not own.
//!
//! The rule this module exists to keep: **nothing in the file changes except
//! the MemFork entry.** Not a comment, not a key order, not an unrelated
//! value, not a setting MemFork has never heard of.
//!
//! - **TOML** is edited with `toml_edit`, which keeps the document's comments,
//!   key order and whitespace exactly as they were.
//! - **JSON** is parsed with key order preserved, changed at one key, and
//!   re-serialized with the indentation detected in the original. Content and
//!   ordering survive exactly; whitespace is reproduced rather than preserved,
//!   which is the one thing JSON cannot promise the way TOML can.
//!
//! Every write is preceded by a timestamped backup beside the original.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value as Json};

use super::{ClientFile, Format, SERVER_ARGS, SERVER_NAME};
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
        Format::Json => edit_json(file, &before, launch)?,
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

// ---- JSON ------------------------------------------------------------------

fn edit_json(file: &ClientFile, before: &str, launch: &Launch) -> Result<(String, Change), String> {
    let indent = detect_indent(before);
    let trailing_newline = before.is_empty() || before.ends_with('\n');

    let mut root: Json = if before.trim().is_empty() {
        Json::Object(Map::new())
    } else {
        serde_json::from_str(before).map_err(|e| {
            format!("this file is not valid JSON, so MemFork will not touch it: {e}")
        })?
    };
    if !root.is_object() {
        return Err("this file's top level is not a JSON object".to_owned());
    }

    let entry = json_entry(file, launch);

    // Walk to the server map, creating only the containers on the way.
    let mut cursor = &mut root;
    for key in &file.server_map {
        let obj = cursor
            .as_object_mut()
            .ok_or_else(|| format!("`{key}` sits under a value that is not an object"))?;
        cursor = obj
            .entry(key.clone())
            .or_insert_with(|| Json::Object(Map::new()));
        if !cursor.is_object() {
            return Err(format!("`{key}` exists but is not an object"));
        }
    }
    let servers = cursor
        .as_object_mut()
        .ok_or_else(|| "the server map is not an object".to_owned())?;

    let change = match servers.get(SERVER_NAME) {
        Some(existing) if *existing == entry => Change::Unchanged,
        Some(_) => Change::Update,
        None => Change::Add,
    };
    if change != Change::Unchanged {
        servers.insert(SERVER_NAME.to_owned(), entry);
    }

    let mut after = serialize_json(&root, &indent)?;
    if trailing_newline {
        after.push('\n');
    }
    Ok((after, change))
}

fn json_entry(file: &ClientFile, launch: &Launch) -> Json {
    let mut entry = Map::new();
    if file.emit_type_stdio {
        entry.insert("type".to_owned(), Json::String("stdio".to_owned()));
    }
    entry.insert(
        "command".to_owned(),
        Json::String(launch.program.display().to_string()),
    );
    entry.insert(
        "args".to_owned(),
        Json::Array(
            launch
                .args
                .iter()
                .cloned()
                .chain(SERVER_ARGS.iter().map(|a| (*a).to_owned()))
                .map(Json::String)
                .collect(),
        ),
    );
    Json::Object(entry)
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

fn serialize_json(value: &Json, indent: &str) -> Result<String, String> {
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    serde::Serialize::serialize(value, &mut ser)
        .map_err(|e| format!("cannot write the updated JSON: {e}"))?;
    String::from_utf8(buf).map_err(|e| format!("the updated JSON is not UTF-8: {e}"))
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

    let wanted = toml_entry(launch);
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

fn toml_entry(launch: &Launch) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    table["command"] = toml_edit::value(launch.program.display().to_string());
    let mut args = toml_edit::Array::new();
    for a in launch
        .args
        .iter()
        .map(String::as_str)
        .chain(SERVER_ARGS.iter().copied())
    {
        args.push(a);
    }
    table["args"] = toml_edit::value(args);
    table
}

/// What command a client's config already registers for MemFork, if any.
///
/// Read rather than inferred: when a registration turns out to point at
/// another MemFork, the useful thing to tell someone is which one.
pub fn registered_command(file: &ClientFile, path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    match file.format {
        Format::Json => {
            let doc: Json = serde_json::from_str(&text).ok()?;
            let mut node = &doc;
            for key in &file.server_map {
                node = node.get(key)?;
            }
            node.get(SERVER_NAME)?
                .get("command")?
                .as_str()
                .map(str::to_owned)
        }
        Format::Toml => {
            let doc: toml_edit::DocumentMut = text.parse().ok()?;
            let mut node = doc.as_item();
            for key in &file.server_map {
                node = node.get(key)?;
            }
            node.get(SERVER_NAME)?
                .get("command")?
                .as_str()
                .map(str::to_owned)
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
}
