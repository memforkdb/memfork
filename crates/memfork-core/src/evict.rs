//! Semantic eviction (DESIGN §4.4).
//!
//! When a branch outgrows its memory budget, the entries that go are the ones
//! worth least: low importance, long unread. Not the oldest, which is what an
//! LRU would take — an agent's most important memory is often one it wrote
//! early and has not needed since.
//!
//! Eviction is itself a commit, carrying [`Op::Evict`] rather than
//! [`Op::Delete`], so it appears in the log as what it was and time travel can
//! still reach the evicted value for as long as history is retained.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::entry::Entry;

/// When to evict, and what to keep.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EvictionConfig {
    /// Approximate ceiling on a branch's entries, in bytes. Accounting is an
    /// estimate: it counts the data, not the allocator's overhead.
    pub budget_bytes: usize,
    /// How many commits it takes for an unread entry's score to halve.
    ///
    /// Small values forget quickly; large values keep things around on the
    /// strength of one early read. Measured in commits rather than seconds,
    /// so a database that sits idle forgets nothing.
    pub half_life_commits: f64,
}

impl Default for EvictionConfig {
    fn default() -> Self {
        EvictionConfig {
            // Enough for a large working set, small enough to bound a
            // long-running agent. Callers with a real figure should set one.
            budget_bytes: 64 * 1024 * 1024,
            half_life_commits: 1000.0,
        }
    }
}

impl EvictionConfig {
    /// A budget in bytes, with the default half-life.
    pub fn with_budget(budget_bytes: usize) -> Self {
        EvictionConfig {
            budget_bytes,
            ..Self::default()
        }
    }

    /// Set the half-life.
    #[must_use]
    pub fn half_life(mut self, commits: f64) -> Self {
        self.half_life_commits = commits;
        self
    }
}

/// What an entry is worth keeping, per DESIGN §4.4:
/// `importance * 0.5^((head_seq - last_access_seq) / half_life)`.
///
/// Deterministic: the same inputs give the same score on every platform, so
/// two databases fed the same operations evict the same entries.
pub fn score(importance: f32, head_seq: u64, last_access_seq: u64, half_life: f64) -> f64 {
    // An entry cannot have been read in the future; if the clock says
    // otherwise, treat it as read now rather than scoring it above 1.
    let age = head_seq.saturating_sub(last_access_seq) as f64;
    if half_life <= 0.0 {
        // No decay at all: importance alone decides.
        return f64::from(importance);
    }
    f64::from(importance) * 0.5_f64.powf(age / half_life)
}

/// Approximate stored size of one entry, in bytes.
///
/// Counts the data the caller gave us plus a fixed allowance for the
/// bookkeeping around it. It is an estimate, and DESIGN §4.4 says so: the point
/// is a stable ordering and a bound that tracks reality, not an exact figure.
pub fn entry_size(key: &str, entry: &Entry) -> usize {
    /// `Arc` header, sequence numbers, importance, the map node holding it.
    const OVERHEAD: usize = 128;
    let embedding = entry.embedding.as_ref().map_or(0, |v| v.len() * 4);
    let meta: usize = entry.meta.iter().map(|(k, v)| k.len() + v.len() + 48).sum();
    OVERHEAD + key.len() + entry.value.len() + embedding + meta
}

/// A key chosen for eviction, and why.
#[derive(Debug, Clone)]
pub struct Evicted {
    /// The key that was evicted.
    pub key: String,
    /// The entry as it was.
    pub entry: Arc<Entry>,
    /// The score it was evicted at.
    pub score: f64,
    /// How many bytes it was holding.
    pub bytes: usize,
}

/// Choose what to evict from a branch to get back under budget.
///
/// Lowest score first; ties broken by key ascending, so the choice is
/// reproducible rather than dependent on iteration order.
pub fn choose<'a, I>(
    entries: I,
    head_seq: u64,
    access: &BTreeMap<String, u64>,
    config: &EvictionConfig,
) -> Vec<Evicted>
where
    I: Iterator<Item = (&'a String, &'a Arc<Entry>)>,
{
    let mut scored: Vec<Evicted> = entries
        .map(|(key, entry)| {
            // An entry never read since it was written falls back to when it
            // was written, which is what the entry itself records.
            let last = access
                .get(key)
                .copied()
                .unwrap_or(entry.last_access_seq)
                .max(entry.last_access_seq);
            Evicted {
                score: score(entry.importance, head_seq, last, config.half_life_commits),
                bytes: entry_size(key, entry),
                key: key.clone(),
                entry: Arc::clone(entry),
            }
        })
        .collect();

    let total: usize = scored.iter().map(|e| e.bytes).sum();
    if total <= config.budget_bytes {
        return Vec::new();
    }

    // `total_cmp` is a total order over every f64, so the comparator can never
    // be inconsistent, and the key tie-break makes the result reproducible.
    scored.sort_by(|a, b| a.score.total_cmp(&b.score).then_with(|| a.key.cmp(&b.key)));

    let mut freed = 0usize;
    let mut chosen = Vec::new();
    for candidate in scored {
        if total - freed <= config.budget_bytes {
            break;
        }
        freed += candidate.bytes;
        chosen.push(candidate);
    }
    chosen
}

/// Called with the entries about to be evicted, before they go.
///
/// DESIGN §4.4: the hook receives them first, so a caller can write them
/// somewhere durable — a summary, a vector store, a file — rather than lose
/// them. Eviction proceeds whatever the hook does; it is a notification, not a
/// veto, because a hook that could block eviction could also exhaust memory.
pub type OnEvict = Arc<dyn Fn(&[Evicted]) + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;

    fn entry(importance: f32, last_access: u64, bytes: usize) -> Arc<Entry> {
        let mut e = Value::new("x".repeat(bytes)).with_importance(importance);
        e.importance = importance;
        Arc::new(e.into_entry(last_access, last_access))
    }

    #[test]
    fn the_score_is_the_formula_in_the_spec() {
        // Unread for exactly one half-life: half the importance.
        assert!((score(1.0, 100, 0, 100.0) - 0.5).abs() < 1e-12);
        assert!((score(1.0, 200, 0, 100.0) - 0.25).abs() < 1e-12);
        // Just written: importance, undecayed. Compared against the f32 value
        // widened to f64, because 0.8 is not exactly representable in f32 and
        // the score carries that through rather than hiding it.
        assert!((score(0.8, 100, 100, 100.0) - f64::from(0.8_f32)).abs() < 1e-12);
        // Importance scales it linearly, and 0.5 is exact in both widths.
        assert!((score(0.5, 100, 0, 100.0) - 0.25).abs() < 1e-12);
    }

    #[test]
    fn importance_beats_recency_when_it_should() {
        // A important entry unread for one half-life still outranks a
        // worthless one written moments ago. That is the whole point of
        // scoring rather than using an LRU.
        let important_but_old = score(1.0, 100, 0, 100.0);
        let worthless_but_new = score(0.1, 100, 100, 100.0);
        assert!(important_but_old > worthless_but_new);
    }

    #[test]
    fn nothing_is_evicted_under_budget() {
        let entries = [("a".to_owned(), entry(0.5, 0, 10))];
        let refs: Vec<(&String, &Arc<Entry>)> = entries.iter().map(|(k, v)| (k, v)).collect();
        let chosen = choose(
            refs.into_iter(),
            0,
            &BTreeMap::new(),
            &EvictionConfig::with_budget(1024 * 1024),
        );
        assert!(chosen.is_empty());
    }

    #[test]
    fn the_lowest_scoring_entries_go_first() {
        let entries = [
            ("keep".to_owned(), entry(1.0, 100, 100)),
            ("drop".to_owned(), entry(0.01, 0, 100)),
            ("middle".to_owned(), entry(0.5, 50, 100)),
        ];
        let refs: Vec<(&String, &Arc<Entry>)> = entries.iter().map(|(k, v)| (k, v)).collect();
        let total: usize = entries.iter().map(|(k, e)| entry_size(k, e)).sum::<usize>();

        // A budget that forces exactly one eviction.
        let chosen = choose(
            refs.into_iter(),
            100,
            &BTreeMap::new(),
            &EvictionConfig::with_budget(total - 1).half_life(50.0),
        );
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].key, "drop");
    }

    #[test]
    fn ties_break_on_the_key_so_the_choice_is_reproducible() {
        let entries: Vec<(String, Arc<Entry>)> = ["c", "a", "b"]
            .iter()
            .map(|k| ((*k).to_owned(), entry(0.5, 0, 100)))
            .collect();
        let run = || {
            let refs: Vec<(&String, &Arc<Entry>)> = entries.iter().map(|(k, v)| (k, v)).collect();
            choose(
                refs.into_iter(),
                0,
                &BTreeMap::new(),
                &EvictionConfig::with_budget(entry_size("a", &entries[0].1) * 2),
            )
            .into_iter()
            .map(|e| e.key)
            .collect::<Vec<_>>()
        };
        assert_eq!(run(), vec!["a".to_owned()]);
        assert_eq!(run(), run());
    }

    #[test]
    fn a_recorded_read_protects_an_entry() {
        // The side structure is what makes reading count: without it, an entry
        // written long ago and read constantly would look stale.
        let entries = [
            ("read".to_owned(), entry(0.5, 0, 100)),
            ("unread".to_owned(), entry(0.5, 0, 100)),
        ];
        let refs: Vec<(&String, &Arc<Entry>)> = entries.iter().map(|(k, v)| (k, v)).collect();
        let mut access = BTreeMap::new();
        access.insert("read".to_owned(), 100u64);

        let chosen = choose(
            refs.into_iter(),
            100,
            &access,
            &EvictionConfig::with_budget(entry_size("read", &entries[0].1) + 1).half_life(10.0),
        );
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].key, "unread");
    }
}
