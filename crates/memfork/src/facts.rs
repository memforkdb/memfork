//! Verified facts: findings that know when they have gone stale (DESIGN §6.7).
//!
//! An agent that explores a repository stores what it found — "auth lives in
//! src/auth/, entry login.rs" — with the files it came from. What the files
//! said at that moment is recorded as a content hash of each; whenever the
//! fact is read back, the files are hashed again and the fact is marked fresh
//! if they match, stale if any changed or is gone.
//!
//! **Only the paths are part of the memory.** The committed entry carries the
//! list of source paths and nothing else about them. The hashes are kept beside
//! the store, outside its history (see [`crate::sidecar`]): a hash in a commit
//! would make the same fact produce a different id on every machine whose copy
//! of the file differs, and would fatten every commit that records a fact.
//!
//! **Hashing needs a working directory, which only the proxy has.** Paths are
//! relative to the project — the repository's top level, else the directory
//! the client was started in — and the proxy, or an `--ephemeral` server, or
//! the command line, does the hashing on the way in and the checking on the
//! way out. The daemon only stores and returns what they give it. Nothing here
//! runs git.
//!
//! **Bounded.** At most [`MAX_SOURCES`] files a fact; a file larger than
//! [`MAX_HASHED_BYTES`] is hashed as its first that-many bytes plus its size;
//! at most [`MAX_BYTES_PER_CHECK`] read to check one answer, after which the
//! rest are reported as not checked; and a cache keyed by path, size and
//! modification time means an unchanged file is not read twice.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde_json::{json, Value as Json};

/// The metadata key a fact's source paths are committed under.
pub const SOURCES_META: &str = "memfork.sources";

/// Most source files one fact may name.
pub const MAX_SOURCES: usize = 32;

/// Longest source path, in bytes.
const MAX_PATH_BYTES: usize = 1024;

/// A file is hashed as at most this many bytes, plus its length.
pub const MAX_HASHED_BYTES: u64 = 8 * 1024 * 1024;

/// Most bytes read from disk to check one answer.
pub const MAX_BYTES_PER_CHECK: u64 = 64 * 1024 * 1024;

/// Make a list of source paths canonical: relative to the project, `/`
/// separators, no `.` parts, nothing that climbs out with `..` or starts from
/// a root, no duplicates, and in the order given.
///
/// ```
/// let paths = ["src\\auth\\login.rs".to_owned(), "./src/auth/mod.rs".to_owned(), "src/auth/login.rs".to_owned()];
/// assert_eq!(memfork::facts::normalise(&paths).unwrap(), ["src/auth/login.rs", "src/auth/mod.rs"]);
/// assert!(memfork::facts::normalise(&["../secret".to_owned()]).is_err());
/// assert!(memfork::facts::normalise(&["/etc/passwd".to_owned()]).is_err());
/// ```
pub fn normalise(paths: &[String]) -> Result<Vec<String>, String> {
    if paths.len() > MAX_SOURCES {
        return Err(format!(
            "a fact may name at most {MAX_SOURCES} source files; this names {}",
            paths.len()
        ));
    }
    let mut out: Vec<String> = Vec::new();
    for raw in paths {
        let unified = raw.trim().replace('\\', "/");
        if unified.is_empty() || unified.len() > MAX_PATH_BYTES {
            return Err(format!("`{raw}` is not a usable source path"));
        }
        let absolute = unified.starts_with('/')
            || unified.as_bytes().get(1) == Some(&b':')
            || unified.starts_with("//");
        let mut parts = Vec::new();
        for part in unified.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    return Err(format!(
                        "`{raw}` climbs out of the project with `..`; name files inside it"
                    ))
                }
                other => parts.push(other),
            }
        }
        if absolute || parts.is_empty() {
            return Err(format!(
                "`{raw}` must be a path relative to the project, such as `src/auth/login.rs`"
            ));
        }
        let path = parts.join("/");
        if !out.contains(&path) {
            out.push(path);
        }
    }
    Ok(out)
}

/// The source paths an entry's metadata records, if it is a fact.
pub fn sources_of(meta: &BTreeMap<String, String>) -> Option<Vec<String>> {
    meta.get(SOURCES_META)
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
}

/// The metadata value for a list of paths.
pub fn sources_meta(paths: &[String]) -> String {
    Json::from(paths.to_vec()).to_string()
}

/// Which recorded hashes go with which fact: a digest of the key, the value
/// and the source paths. The same fact on two branches, or forked from one to
/// the other, shares its record; a rewrite with different words or sources
/// gets its own.
///
/// ```
/// use memfork::facts::record_id;
/// let sources = ["src/auth/login.rs".to_owned()];
/// let a = record_id("shop:fact:auth", b"auth lives in src/auth", &sources);
/// assert_eq!(a, record_id("shop:fact:auth", b"auth lives in src/auth", &sources));
/// assert_ne!(a, record_id("shop:fact:auth", b"auth moved", &sources));
/// assert_eq!(a.len(), 64);
/// ```
pub fn record_id(key: &str, value: &[u8], sources: &[String]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(key.len() as u64).to_le_bytes());
    hasher.update(key.as_bytes());
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
    for s in sources {
        hasher.update(&(s.len() as u64).to_le_bytes());
        hasher.update(s.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// Recorded hashes: path to content hash, `None` for a file that was missing.
pub type Hashes = BTreeMap<String, Option<String>>;

/// A file as it was when last hashed.
#[derive(Debug)]
struct Seen {
    size: u64,
    modified: Option<SystemTime>,
    hash: String,
}

/// Hashes files under a project, remembering what it has read.
#[derive(Debug, Default)]
pub struct Hasher {
    cache: Mutex<BTreeMap<PathBuf, Seen>>,
}

impl Hasher {
    /// The hash of `root/relative`, `None` if it is not a readable file.
    /// Adds the bytes read to `spent`; a cached answer costs nothing.
    pub fn hash(&self, root: &Path, relative: &str, spent: &mut u64) -> Option<String> {
        let path = relative
            .split('/')
            .fold(root.to_path_buf(), |p, part| p.join(part));
        let meta = std::fs::metadata(&path).ok().filter(|m| m.is_file())?;
        let size = meta.len();
        let modified = meta.modified().ok();
        if let Some(seen) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&path)
        {
            if seen.size == size && seen.modified == modified && modified.is_some() {
                return Some(seen.hash.clone());
            }
        }
        let mut file = std::fs::File::open(&path).ok()?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = vec![0u8; 64 * 1024];
        let mut read_total = 0u64;
        while read_total < MAX_HASHED_BYTES {
            let want = buffer
                .len()
                .min(usize::try_from(MAX_HASHED_BYTES - read_total).unwrap_or(usize::MAX));
            let n = file.read(&mut buffer[..want]).ok()?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            read_total += n as u64;
        }
        // The length goes in too, so a large file that changes only past the
        // hashed part still changes its hash when its length does.
        hasher.update(&size.to_le_bytes());
        *spent += read_total;
        let hash = hasher.finalize().to_hex().to_string();
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).insert(
            path,
            Seen {
                size,
                modified,
                hash: hash.clone(),
            },
        );
        Some(hash)
    }

    /// Hash every source, for recording at write time.
    pub fn record(&self, root: &Path, sources: &[String]) -> Hashes {
        let mut spent = 0u64;
        sources
            .iter()
            .map(|s| (s.clone(), self.hash(root, s, &mut spent)))
            .collect()
    }
}

/// What checking an answer found, for statistics and the activity feed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Checked {
    /// Facts whose sources are unchanged.
    pub fresh: u64,
    /// Facts with a changed or missing source.
    pub stale: u64,
    /// Facts with nothing recorded to compare with, or not checked in time.
    pub unverified: u64,
    /// Each fact checked, by key, with `fresh`, `stale` or `unverified`.
    pub facts: Vec<(String, String)>,
}

impl Checked {
    /// Whether anything was checked.
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }
}

/// Mark every fact in an answer fresh, stale or unverified.
///
/// A fact in an answer is an object with a `sources` list and a `recorded`
/// field — the hashes the daemon kept, or `null` if it kept none. `recorded`
/// is replaced by `fact` (the state) and, when stale, `stale_sources`.
pub fn check(answer: &mut Json, root: &Path, hasher: &Hasher) -> Checked {
    let mut checked = Checked::default();
    let mut spent = 0u64;
    walk(answer, root, hasher, &mut spent, &mut checked);
    checked
}

fn walk(value: &mut Json, root: &Path, hasher: &Hasher, spent: &mut u64, out: &mut Checked) {
    match value {
        Json::Object(map) => {
            if map.contains_key("recorded") && map.get("sources").is_some_and(Json::is_array) {
                let recorded = map.remove("recorded").unwrap_or(Json::Null);
                let key = map
                    .get("key")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_owned();
                let (state, stale) = judge(&recorded, root, hasher, spent);
                map.insert("fact".to_owned(), json!(state));
                if !stale.is_empty() {
                    map.insert("stale_sources".to_owned(), json!(stale));
                }
                match state {
                    "fresh" => out.fresh += 1,
                    "stale" => out.stale += 1,
                    _ => out.unverified += 1,
                }
                out.facts.push((key, state.to_owned()));
            }
            for child in map.values_mut() {
                walk(child, root, hasher, spent, out);
            }
        }
        Json::Array(items) => {
            for item in items {
                walk(item, root, hasher, spent, out);
            }
        }
        _ => {}
    }
    if !out.facts.is_empty() {
        crate::tools::handoff::restate_size(value);
    }
}

fn judge(
    recorded: &Json,
    root: &Path,
    hasher: &Hasher,
    spent: &mut u64,
) -> (&'static str, Vec<String>) {
    let Some(recorded) = recorded.as_object() else {
        return ("unverified", Vec::new());
    };
    let mut stale = Vec::new();
    for (path, then) in recorded {
        if *spent > MAX_BYTES_PER_CHECK {
            return ("unverified", Vec::new());
        }
        let now = hasher.hash(root, path, spent);
        // A file missing then and now is still missing: stale either way.
        if now.is_none() || now.as_deref() != then.as_str() {
            stale.push(path.clone());
        }
    }
    if stale.is_empty() {
        ("fresh", stale)
    } else {
        ("stale", stale)
    }
}

/// Where a session's project is, for hashing: the repository's top level, or
/// the working directory outside a repository.
pub fn project_root(cwd: &Path) -> PathBuf {
    crate::namespace::repository_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_paths_are_made_canonical_and_kept_inside_the_project() {
        let ok = normalise(&[
            "src\\auth\\login.rs".to_owned(),
            "./src/auth/".to_owned(),
            "src/auth/login.rs".to_owned(),
        ])
        .unwrap();
        assert_eq!(ok, ["src/auth/login.rs", "src/auth"]);
        for bad in [
            "/etc/passwd",
            "C:/Windows",
            "../other/x",
            "src/../../x",
            "",
            ".",
        ] {
            assert!(normalise(&[bad.to_owned()]).is_err(), "{bad} was accepted");
        }
        let many: Vec<String> = (0..=MAX_SOURCES).map(|i| format!("f{i}")).collect();
        assert!(normalise(&many).is_err());
    }

    #[test]
    fn a_fact_is_fresh_then_stale_when_its_file_changes_and_stale_when_it_goes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/a.rs"), "fn a() {}").unwrap();
        let hasher = Hasher::default();
        let recorded = hasher.record(tmp.path(), &["src/a.rs".to_owned()]);

        let answer = |recorded: &Hashes| json!({"key": "p:fact:a", "sources": ["src/a.rs"], "recorded": recorded});
        let mut fresh = answer(&recorded);
        let checked = check(&mut fresh, tmp.path(), &hasher);
        assert_eq!(fresh["fact"], "fresh");
        assert!(fresh.get("recorded").is_none());
        assert_eq!(checked.fresh, 1);

        // A new length and a new time: the cache cannot hide the change.
        std::fs::write(tmp.path().join("src/a.rs"), "fn a() { changed() }").unwrap();
        let mut stale = answer(&recorded);
        check(&mut stale, tmp.path(), &hasher);
        assert_eq!(stale["fact"], "stale");
        assert_eq!(stale["stale_sources"], json!(["src/a.rs"]));

        std::fs::remove_file(tmp.path().join("src/a.rs")).unwrap();
        let mut gone = answer(&recorded);
        check(&mut gone, tmp.path(), &hasher);
        assert_eq!(gone["fact"], "stale");
    }

    #[test]
    fn nothing_recorded_is_unverified_and_nested_facts_are_found() {
        let tmp = tempfile::tempdir().unwrap();
        let hasher = Hasher::default();
        let mut answer = json!({"results": [
            {"key": "a", "sources": ["x"], "recorded": null},
            {"key": "b", "value": "not a fact"},
        ]});
        let checked = check(&mut answer, tmp.path(), &hasher);
        assert_eq!(answer["results"][0]["fact"], "unverified");
        assert!(answer["results"][1].get("fact").is_none());
        assert_eq!(
            checked.facts,
            vec![("a".to_owned(), "unverified".to_owned())]
        );
    }

    #[test]
    fn a_record_id_depends_on_key_words_and_sources_only() {
        let a = record_id("k", b"v", &["x".to_owned()]);
        assert_eq!(a, record_id("k", b"v", &["x".to_owned()]));
        assert_ne!(a, record_id("k", b"w", &["x".to_owned()]));
        assert_ne!(a, record_id("k", b"v", &["y".to_owned()]));
        assert_ne!(record_id("ab", b"c", &[]), record_id("a", b"bc", &[]));
    }
}
