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

/// Most "last seen" records kept, across every project, client and branch.
pub const MAX_SEEN_RECORDS: usize = 10_000;

/// Most stale facts remembered per project.
pub const MAX_STALE_KEPT: usize = 1000;

/// Most briefings remembered per project, newest kept.
pub const MAX_BRIEFINGS_KEPT: usize = 100;

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
    /// Handoffs left by another session and carried to this client in a
    /// briefing for the first time: work picked up rather than re-explained.
    pub handoffs_picked_up: u64,
}

/// One briefing that was served, as the Brain shows it: to whom, how big,
/// and what it carried. Kept beside the store, never in it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Briefing {
    /// Its place among every record, for order.
    pub order: u64,
    /// The branch's sequence number when it was served.
    pub seq: u64,
    /// The branch it described.
    pub branch: String,
    /// The client it was served to.
    pub to: String,
    /// Its size in bytes.
    pub bytes: u64,
    /// The keys it carried: the handoff, lessons, decisions, facts, tasks.
    pub keys: Vec<String>,
    /// The handoff it carried, if one.
    pub handoff: Option<String>,
    /// How many commits the since-last-look part covered, if it was there.
    pub since_commits: Option<u64>,
    /// What was left out to fit, by list.
    pub omitted: BTreeMap<String, u64>,
    /// The task it was ranked for, if one.
    pub task: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FactRecord {
    order: u64,
    hashes: Hashes,
}

/// Where a client last was on a branch.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SeenRecord {
    commit: String,
    seq: u64,
    order: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Data {
    version: u32,
    /// project -> client -> counters
    stats: BTreeMap<String, BTreeMap<String, Counters>>,
    facts: BTreeMap<String, FactRecord>,
    next_order: u64,
    /// project -> client -> branch -> the head that client last saw there
    seen: BTreeMap<String, BTreeMap<String, BTreeMap<String, SeenRecord>>>,
    /// project -> maintenance: switched off, and the triggers that have fired
    maintenance: BTreeMap<String, Maintenance>,
    /// project -> the facts last found stale, by key
    stale: BTreeMap<String, std::collections::BTreeSet<String>>,
    /// project -> the briefings served, oldest first
    briefings: BTreeMap<String, Vec<Briefing>>,
}

/// A project's self-maintenance, as far as the side file keeps it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Maintenance {
    off: bool,
    /// trigger -> the task it added, while that is outstanding
    fired: BTreeMap<String, String>,
    /// The number the next maintenance task takes.
    next: u64,
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

    /// Remember that `client` has seen `branch` of `project` as of `commit`,
    /// number `seq`. Keeps the newest [`MAX_SEEN_RECORDS`].
    pub fn saw(&self, project: &str, client: &str, branch: &str, commit: &str, seq: u64) {
        let mut data = self.data();
        let unchanged = data
            .seen
            .get(project)
            .and_then(|c| c.get(client))
            .and_then(|b| b.get(branch))
            .is_some_and(|r| r.commit == commit);
        if unchanged {
            return;
        }
        let order = data.next_order;
        data.next_order += 1;
        data.seen
            .entry(project.to_owned())
            .or_default()
            .entry(client.to_owned())
            .or_default()
            .insert(
                branch.to_owned(),
                SeenRecord {
                    commit: commit.to_owned(),
                    seq,
                    order,
                },
            );
        let total: usize = data
            .seen
            .values()
            .flat_map(BTreeMap::values)
            .map(BTreeMap::len)
            .sum();
        if total > MAX_SEEN_RECORDS {
            let oldest = data
                .seen
                .iter()
                .flat_map(|(p, clients)| {
                    clients.iter().flat_map(move |(c, branches)| {
                        branches
                            .iter()
                            .map(move |(b, r)| (r.order, p.clone(), c.clone(), b.clone()))
                    })
                })
                .min();
            if let Some((_, p, c, b)) = oldest {
                if let Some(branches) = data.seen.get_mut(&p).and_then(|cs| cs.get_mut(&c)) {
                    branches.remove(&b);
                }
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// The head `client` last saw on `branch` of `project`, and its number.
    pub fn last_seen(&self, project: &str, client: &str, branch: &str) -> Option<(String, u64)> {
        self.data()
            .seen
            .get(project)
            .and_then(|c| c.get(client))
            .and_then(|b| b.get(branch))
            .map(|r| (r.commit.clone(), r.seq))
    }

    /// Remember the verdicts checking facts found, so the project's stale
    /// facts can be counted. At most [`MAX_STALE_KEPT`] per project.
    pub fn note_facts(&self, project: &str, facts: &[(String, String)]) {
        if facts.is_empty() {
            return;
        }
        let mut data = self.data();
        let set = data.stale.entry(project.to_owned()).or_default();
        for (key, state) in facts {
            match state.as_str() {
                "stale" if set.len() < MAX_STALE_KEPT => {
                    set.insert(key.clone());
                }
                "fresh" => {
                    set.remove(key);
                }
                _ => {}
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Remember a briefing that was served. Keeps the newest
    /// [`MAX_BRIEFINGS_KEPT`] per project. Returns whether the handoff it
    /// carried, if any, was new to that client: written by somebody else and
    /// in none of the briefings this client was served before.
    pub fn note_briefing(&self, project: &str, mut briefing: Briefing) -> bool {
        let mut data = self.data();
        briefing.order = data.next_order;
        data.next_order += 1;
        let list = data.briefings.entry(project.to_owned()).or_default();
        let picked_up = briefing.handoff.as_ref().is_some_and(|handoff| {
            !list
                .iter()
                .any(|b| b.to == briefing.to && b.handoff.as_ref() == Some(handoff))
        });
        list.push(briefing);
        if list.len() > MAX_BRIEFINGS_KEPT {
            let excess = list.len() - MAX_BRIEFINGS_KEPT;
            list.drain(..excess);
        }
        self.dirty.store(true, Ordering::Relaxed);
        picked_up
    }

    /// The briefings served in `project`, oldest first.
    pub fn briefings(&self, project: &str) -> Vec<Briefing> {
        self.data()
            .briefings
            .get(project)
            .cloned()
            .unwrap_or_default()
    }

    /// The facts in `project` last found stale.
    pub fn stale_facts(&self, project: &str) -> Vec<String> {
        self.data()
            .stale
            .get(project)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Whether MemFork may add maintenance tasks to `project`.
    pub fn maintenance_on(&self, project: &str) -> bool {
        !self.data().maintenance.get(project).is_some_and(|m| m.off)
    }

    /// Switch maintenance tasks on or off for `project`.
    pub fn set_maintenance(&self, project: &str, on: bool) {
        self.data()
            .maintenance
            .entry(project.to_owned())
            .or_default()
            .off = !on;
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// The maintenance task a trigger added and that is still outstanding.
    pub fn fired(&self, project: &str, trigger: &str) -> Option<String> {
        self.data()
            .maintenance
            .get(project)
            .and_then(|m| m.fired.get(trigger).cloned())
    }

    /// Every trigger that has fired in `project`, with its task.
    pub fn fired_all(&self, project: &str) -> BTreeMap<String, String> {
        self.data()
            .maintenance
            .get(project)
            .map(|m| m.fired.clone())
            .unwrap_or_default()
    }

    /// Note that `trigger` fired in `project`: the task takes the next number,
    /// and `key_for` turns it into the task's key, which is returned.
    pub fn fire(
        &self,
        project: &str,
        trigger: &str,
        key_for: impl FnOnce(u64) -> String,
    ) -> String {
        let mut data = self.data();
        let m = data.maintenance.entry(project.to_owned()).or_default();
        m.next += 1;
        let key = key_for(m.next);
        m.fired.insert(trigger.to_owned(), key.clone());
        self.dirty.store(true, Ordering::Relaxed);
        key
    }

    /// Let `trigger` fire again in `project`.
    pub fn rearm(&self, project: &str, trigger: &str) {
        if let Some(m) = self.data().maintenance.get_mut(project) {
            m.fired.remove(trigger);
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
    #[test]
    fn briefings_are_kept_bounded_and_a_handoff_is_picked_up_once_per_client() {
        let side = Sidecar::in_memory();
        let brief = |to: &str, handoff: Option<&str>| Briefing {
            to: to.to_owned(),
            branch: "main".to_owned(),
            handoff: handoff.map(str::to_owned),
            keys: handoff.map(str::to_owned).into_iter().collect(),
            ..Briefing::default()
        };
        assert!(!side.note_briefing("shop", brief("codex", None)));
        assert!(side.note_briefing("shop", brief("codex", Some("shop:handoff:00000001"))));
        assert!(!side.note_briefing("shop", brief("codex", Some("shop:handoff:00000001"))));
        assert!(side.note_briefing("shop", brief("claude", Some("shop:handoff:00000001"))));
        assert!(side.note_briefing("shop", brief("codex", Some("shop:handoff:00000002"))));
        let kept = side.briefings("shop");
        assert_eq!(kept.len(), 5);
        assert!(kept.windows(2).all(|w| w[0].order < w[1].order));
        assert!(side.briefings("other").is_empty());

        for i in 0..(MAX_BRIEFINGS_KEPT + 7) {
            side.note_briefing("big", brief(&format!("c{i}"), None));
        }
        let big = side.briefings("big");
        assert_eq!(big.len(), MAX_BRIEFINGS_KEPT);
        assert_eq!(big[0].to, "c7", "the oldest were not the ones dropped");
    }
}
