//! What the daemon keeps beside the store, outside its history (DESIGN §6.8).
//!
//! Two things belong to no commit: usage statistics, and the content hashes a
//! verified fact was recorded with (see [`crate::facts`]). Both are about this
//! machine and this moment, not about what memory says, so putting either in
//! a commit would make ids depend on the machine. They are kept here instead, and
//! are written to one file in the data directory so they survive a restart:
//! at most every few seconds while they change, and when the daemon stops.
//!
//! Losing the file loses statistics and marks older facts unverified until
//! they are rewritten; it never loses memory. A file that cannot be read is
//! set aside rather than trusted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use crate::facts::Hashes;

/// The file in the data directory.
pub const FILE: &str = "memfork-sidecar.json";

/// The layout version written, so a later one can tell.
const VERSION: u32 = 1;

/// Most fact records kept; the oldest go first.
pub const MAX_FACT_RECORDS: usize = 50_000;

/// Counters for one client in one project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Counters {
    /// Briefings served by `memfork_resume`.
    pub briefings: u64,
    /// Their total size in bytes.
    pub briefing_bytes: u64,
    /// The size of the project's memory each time one was served, summed, so
    /// the two can be compared.
    pub memory_bytes: u64,
    /// Lessons written.
    pub lessons_recorded: u64,
    /// Lessons included in briefings.
    pub lessons_served: u64,
    /// Facts returned with unchanged sources.
    pub facts_fresh: u64,
    /// Facts returned with a changed or missing source.
    pub facts_stale: u64,
    /// Facts returned with nothing to compare with.
    pub facts_unverified: u64,
    /// Tasks claimed.
    pub claims: u64,
    /// Claims refused because somebody else held the task: work not done
    /// twice.
    pub claim_conflicts: u64,
    /// Text searches run.
    pub finds: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FactRecord {
    order: u64,
    hashes: Hashes,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Data {
    version: u32,
    /// project -> client -> counters
    stats: BTreeMap<String, BTreeMap<String, Counters>>,
    facts: BTreeMap<String, FactRecord>,
    next_order: u64,
}

/// The side structure, and where it is kept.
#[derive(Debug)]
pub struct Sidecar {
    path: Option<PathBuf>,
    data: Mutex<Data>,
    dirty: AtomicBool,
}

impl Default for Sidecar {
    fn default() -> Self {
        Sidecar::in_memory()
    }
}

impl Sidecar {
    /// One that is never written anywhere: `--ephemeral`, and tests.
    pub fn in_memory() -> Self {
        Sidecar {
            path: None,
            data: Mutex::new(Data {
                version: VERSION,
                ..Data::default()
            }),
            dirty: AtomicBool::new(false),
        }
    }

    /// The one kept in `dir`, read back if it is there. Returns a note when
    /// an existing file could not be read and was set aside.
    pub fn open(dir: &Path) -> (Self, Option<String>) {
        let path = dir.join(FILE);
        let mut note = None;
        let data = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Data>(&bytes) {
                Ok(data) => data,
                Err(e) => {
                    let aside = dir.join(format!("{FILE}.unreadable"));
                    let _ = std::fs::rename(&path, &aside);
                    note = Some(format!(
                        "{} could not be read ({e}) and was set aside as {}; statistics \
                         start again and older facts are unverified until rewritten",
                        path.display(),
                        aside.display()
                    ));
                    Data::default()
                }
            },
            Err(_) => Data::default(),
        };
        (
            Sidecar {
                path: Some(path),
                data: Mutex::new(Data {
                    version: VERSION,
                    ..data
                }),
                dirty: AtomicBool::new(false),
            },
            note,
        )
    }

    fn data(&self) -> std::sync::MutexGuard<'_, Data> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Change the counters for `client` in `project`.
    pub fn count(&self, project: &str, client: &str, change: impl FnOnce(&mut Counters)) {
        let mut data = self.data();
        change(
            data.stats
                .entry(project.to_owned())
                .or_default()
                .entry(client.to_owned())
                .or_default(),
        );
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Keep the hashes a fact was written with.
    pub fn record_fact(&self, id: &str, hashes: Hashes) {
        let mut data = self.data();
        let order = data.next_order;
        data.next_order += 1;
        data.facts
            .insert(id.to_owned(), FactRecord { order, hashes });
        if data.facts.len() > MAX_FACT_RECORDS {
            if let Some(oldest) = data
                .facts
                .iter()
                .min_by_key(|(_, r)| r.order)
                .map(|(k, _)| k.clone())
            {
                data.facts.remove(&oldest);
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// The hashes a fact was written with, if they were kept.
    pub fn fact(&self, id: &str) -> Option<Hashes> {
        self.data().facts.get(id).map(|r| r.hashes.clone())
    }

    /// Every counter, by project and client, with totals.
    pub fn stats(&self, project: Option<&str>) -> Json {
        let data = self.data();
        let mut projects = serde_json::Map::new();
        let mut total = Counters::default();
        for (name, clients) in &data.stats {
            if project.is_some_and(|p| p != name) {
                continue;
            }
            let mut by_client = serde_json::Map::new();
            for (client, c) in clients {
                add(&mut total, c);
                by_client.insert(client.clone(), json!(c));
            }
            projects.insert(name.clone(), Json::Object(by_client));
        }
        json!({ "projects": projects, "total": total })
    }

    /// Write the file if anything changed since it was last written.
    pub fn flush(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return Ok(());
        }
        let bytes = serde_json::to_vec(&*self.data())
            .map_err(|e| format!("cannot encode {}: {e}", path.display()))?;
        // Beside it and renamed over it: a crash leaves the old file or the
        // new one, never half of either.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)
            .and_then(|()| std::fs::rename(&tmp, path))
            .map_err(|e| {
                self.dirty.store(true, Ordering::Relaxed);
                format!("cannot write {}: {e}", path.display())
            })
    }
}

fn add(total: &mut Counters, c: &Counters) {
    total.briefings += c.briefings;
    total.briefing_bytes += c.briefing_bytes;
    total.memory_bytes += c.memory_bytes;
    total.lessons_recorded += c.lessons_recorded;
    total.lessons_served += c.lessons_served;
    total.facts_fresh += c.facts_fresh;
    total.facts_stale += c.facts_stale;
    total.facts_unverified += c.facts_unverified;
    total.claims += c.claims;
    total.claim_conflicts += c.claim_conflicts;
    total.finds += c.finds;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_survives_a_restart_and_sets_a_bad_file_aside() {
        let tmp = tempfile::tempdir().unwrap();
        let (side, note) = Sidecar::open(tmp.path());
        assert!(note.is_none());
        side.count("shop", "claude-code", |c| c.claims += 2);
        let mut hashes = Hashes::new();
        hashes.insert("src/a.rs".to_owned(), Some("abc".to_owned()));
        side.record_fact("f1", hashes.clone());
        side.flush().unwrap();

        let (again, _) = Sidecar::open(tmp.path());
        assert_eq!(again.stats(None)["total"]["claims"], 2);
        assert_eq!(again.fact("f1"), Some(hashes));

        std::fs::write(tmp.path().join(FILE), "not json").unwrap();
        let (fresh, note) = Sidecar::open(tmp.path());
        assert!(note.unwrap().contains("set aside"));
        assert_eq!(fresh.stats(None)["total"]["claims"], 0);
        assert!(tmp.path().join(format!("{FILE}.unreadable")).exists());
    }

    #[test]
    fn fact_records_are_bounded_oldest_first() {
        let side = Sidecar::in_memory();
        for i in 0..(MAX_FACT_RECORDS + 3) {
            side.record_fact(&format!("f{i}"), Hashes::new());
        }
        assert!(side.fact("f0").is_none());
        assert!(side.fact("f2").is_none());
        assert!(side.fact("f3").is_some());
    }

    #[test]
    fn stats_are_per_project_and_client_with_totals() {
        let side = Sidecar::in_memory();
        side.count("a", "x", |c| c.briefings += 1);
        side.count("a", "y", |c| c.briefings += 2);
        side.count("b", "x", |c| c.briefings += 4);
        let all = side.stats(None);
        assert_eq!(all["total"]["briefings"], 7);
        assert_eq!(all["projects"]["a"]["y"]["briefings"], 2);
        assert_eq!(side.stats(Some("b"))["total"]["briefings"], 4);
    }
}
