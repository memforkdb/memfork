//! The MemFork extension module: `import memfork`.
//!
//! Two things ship in one wheel, because a Python user should not have to pick
//! which MemFork they installed:
//!
//! - [`Database`], the engine in this process. Fork memory before a risky
//!   step, merge it if the attempt worked, discard it if it did not.
//! - `run_cli`, which is the whole `memfork` command — so `pip install
//!   memfork` also puts the MCP server on the path, and `uvx memfork mcp`
//!   works without a separate install. (A `#[pyfunction]`, so it is reachable
//!   from Python rather than from Rust.)
//!
//! **What this does not do, in 0.1.** The engine here is in memory only. The
//! durable, shared store is the daemon's, and `memfork-core` has no I/O by
//! design (DESIGN §4.5), so nothing in this module writes to disk or sees what
//! an MCP client wrote. A Python program that wants the shared memory talks to
//! the server — the same one this package installs. Binding the persistence
//! layer is later work, and saying so plainly beats a surprise.

use std::collections::BTreeMap;
use std::sync::Arc;

use memfork_core::{Db, Error as CoreError, MergePolicy, Value};
use pyo3::exceptions::{PyKeyError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

/// Turn an engine error into the Python exception that fits it.
fn to_py(error: CoreError) -> PyErr {
    match error {
        // A missing key or branch is a lookup failure, and Python programs
        // catch `KeyError` for those.
        CoreError::NoSuchBranch(_) | CoreError::NoSuchCommit(_) => {
            PyKeyError::new_err(error.to_string())
        }
        other => PyValueError::new_err(other.to_string()),
    }
}

/// One entry, as Python sees it.
#[pyclass(module = "memfork", frozen, skip_from_py_object)]
#[derive(Debug, Clone)]
pub struct Entry {
    /// The stored bytes. Held as bytes rather than as a Python object so the
    /// entry can be cloned and moved without a Python token in hand.
    value: Vec<u8>,
    /// Its vector, if it has one.
    #[pyo3(get)]
    embedding: Option<Vec<f32>>,
    /// Retention weight in `[0, 1]`.
    #[pyo3(get)]
    importance: f32,
    /// The branch sequence at which the key was first written.
    #[pyo3(get)]
    created_seq: u64,
    /// The branch sequence at which it was last written.
    #[pyo3(get)]
    last_access_seq: u64,
    /// Caller metadata.
    #[pyo3(get)]
    meta: BTreeMap<String, String>,
}

#[pymethods]
impl Entry {
    /// The stored value, as `bytes`.
    ///
    /// Written by hand rather than derived: a `Vec<u8>` would come back as a
    /// list of integers, and what was stored was bytes.
    #[getter]
    fn value<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.value)
    }

    fn __repr__(&self) -> String {
        format!(
            "Entry(value={} bytes, importance={})",
            self.value.len(),
            self.importance
        )
    }
}

impl Entry {
    fn from_core(entry: &memfork_core::Entry) -> Self {
        Entry {
            value: entry.value.to_vec(),
            embedding: entry.embedding.clone(),
            importance: entry.importance,
            created_seq: entry.created_seq,
            last_access_seq: entry.last_access_seq,
            meta: entry.meta.clone(),
        }
    }
}

/// One search result.
#[pyclass(module = "memfork", frozen, get_all, skip_from_py_object)]
#[derive(Debug, Clone)]
pub struct Hit {
    /// The key that matched.
    key: String,
    /// Cosine similarity, in `[-1, 1]`.
    score: f32,
}

#[pymethods]
impl Hit {
    fn __repr__(&self) -> String {
        format!("Hit(key={:?}, score={:.4})", self.key, self.score)
    }
}

/// One commit in a branch's history.
#[pyclass(module = "memfork", frozen, get_all, skip_from_py_object)]
#[derive(Debug, Clone)]
pub struct Commit {
    /// The content-addressed id, as hex.
    id: String,
    /// Its position on the branch.
    seq: u64,
    /// The message, if the commit carries one.
    message: Option<String>,
}

#[pymethods]
impl Commit {
    fn __repr__(&self) -> String {
        format!("Commit(seq={}, id={:.12})", self.seq, self.id)
    }
}

/// What a merge did.
#[pyclass(module = "memfork", frozen, get_all, skip_from_py_object)]
#[derive(Debug, Clone)]
pub struct Merge {
    /// `up-to-date`, `fast-forward` or `merged`.
    kind: String,
    /// Keys the merge changed on the target.
    changed: Vec<String>,
    /// Keys that conflicted, when the policy was `fail`.
    conflicts: Vec<String>,
    /// The target's head afterwards, as hex.
    head: String,
}

#[pymethods]
impl Merge {
    fn __repr__(&self) -> String {
        format!(
            "Merge(kind={:?}, changed={}, conflicts={})",
            self.kind,
            self.changed.len(),
            self.conflicts.len()
        )
    }
}

/// Branchable memory, in this process.
#[pyclass(module = "memfork")]
#[derive(Debug)]
pub struct Database {
    db: Arc<Db>,
}

#[pymethods]
impl Database {
    /// A new database with one branch.
    #[new]
    #[pyo3(signature = (branch = "main"))]
    fn new(branch: &str) -> PyResult<Self> {
        Ok(Database {
            db: Arc::new(Db::with_default_branch(branch).map_err(to_py)?),
        })
    }

    /// The branch used when a call does not name one.
    #[getter]
    fn default_branch(&self) -> String {
        self.db.default_branch().to_owned()
    }

    /// Store a value, returning the new commit id.
    #[pyo3(signature = (key, value, *, branch = None, embedding = None, importance = None, ttl_commits = None, meta = None))]
    #[allow(clippy::too_many_arguments)]
    fn put(
        &self,
        key: &str,
        value: &[u8],
        branch: Option<&str>,
        embedding: Option<Vec<f32>>,
        importance: Option<f32>,
        ttl_commits: Option<u64>,
        meta: Option<BTreeMap<String, String>>,
    ) -> PyResult<String> {
        let mut v = Value::new(value.to_vec());
        if let Some(e) = embedding {
            v = v.with_embedding(e);
        }
        if let Some(i) = importance {
            v = v.with_importance(i);
        }
        if let Some(t) = ttl_commits {
            v = v.with_ttl_commits(t);
        }
        for (k, value) in meta.unwrap_or_default() {
            v = v.with_meta(k, value);
        }
        let id = self.db.put(self.branch(branch), key, v).map_err(to_py)?;
        Ok(id.to_hex())
    }

    /// Read a value back, or `None` if the key is not there.
    #[pyo3(signature = (key, *, branch = None))]
    fn get(&self, key: &str, branch: Option<&str>) -> PyResult<Option<Entry>> {
        Ok(self
            .db
            .get(self.branch(branch), key)
            .map_err(to_py)?
            .map(|e| Entry::from_core(&e)))
    }

    /// Remove a key, returning the new commit id.
    #[pyo3(signature = (key, *, branch = None))]
    fn delete(&self, key: &str, branch: Option<&str>) -> PyResult<String> {
        Ok(self
            .db
            .delete(self.branch(branch), key)
            .map_err(to_py)?
            .to_hex())
    }

    /// The keys starting with `prefix`, in order.
    #[pyo3(signature = (prefix = "", *, branch = None, limit = None))]
    fn list(
        &self,
        prefix: &str,
        branch: Option<&str>,
        limit: Option<usize>,
    ) -> PyResult<Vec<(String, Entry)>> {
        Ok(self
            .db
            .list(self.branch(branch), prefix, limit)
            .map_err(to_py)?
            .into_iter()
            .map(|(k, e)| (k, Entry::from_core(&e)))
            .collect())
    }

    /// The `k` nearest entries to `query`, by cosine similarity.
    #[pyo3(signature = (query, k = 10, *, branch = None, prefix = None))]
    fn search(
        &self,
        query: Vec<f32>,
        k: usize,
        branch: Option<&str>,
        prefix: Option<&str>,
    ) -> PyResult<Vec<Hit>> {
        Ok(self
            .db
            .search(self.branch(branch), &query, k, prefix)
            .map_err(to_py)?
            .into_iter()
            .map(|h| Hit {
                key: h.key,
                score: h.score,
            })
            .collect())
    }

    /// Branch memory. Cheap whatever it holds, so branch freely.
    #[pyo3(signature = (name, *, from_branch = None, at_seq = None))]
    fn fork(&self, name: &str, from_branch: Option<&str>, at_seq: Option<u64>) -> PyResult<String> {
        let from = self.branch(from_branch);
        let id = match at_seq {
            Some(seq) => self.db.fork_at(from, seq, name),
            None => self.db.fork(from, name),
        }
        .map_err(to_py)?;
        Ok(id.to_hex())
    }

    /// Merge `source` into `target`.
    ///
    /// `policy` is `fail` (the default), `ours` or `theirs`. With `fail`, a
    /// conflict changes nothing and comes back in `conflicts`.
    #[pyo3(signature = (source, target = None, *, policy = "fail"))]
    fn merge(&self, source: &str, target: Option<&str>, policy: &str) -> PyResult<Merge> {
        let policy = MergePolicy::parse(policy)
            .ok_or_else(|| PyValueError::new_err(format!("unknown merge policy `{policy}`")))?;
        let outcome = self
            .db
            .merge(source, self.branch(target), policy)
            .map_err(to_py)?;
        Ok(Merge {
            kind: outcome.kind.as_str().to_owned(),
            changed: outcome.changed,
            conflicts: outcome.conflicts,
            head: outcome.head.to_hex(),
        })
    }

    /// Throw a branch away. The branch it came from is untouched.
    fn discard(&self, branch: &str) -> PyResult<()> {
        self.db.discard(branch).map_err(to_py)
    }

    /// Every branch, with its head sequence.
    fn branches(&self) -> Vec<(String, u64)> {
        self.db
            .branches()
            .into_iter()
            .map(|b| (b.name, b.seq))
            .collect()
    }

    /// A branch's history, newest first.
    #[pyo3(signature = (*, branch = None, limit = None))]
    fn log(&self, branch: Option<&str>, limit: Option<usize>) -> PyResult<Vec<Commit>> {
        Ok(self
            .db
            .log(self.branch(branch), limit)
            .map_err(to_py)?
            .into_iter()
            .map(|e| Commit {
                id: e.id.to_hex(),
                seq: e.seq,
                message: e.message,
            })
            .collect())
    }

    /// Read a key as it was at an earlier point on the branch.
    #[pyo3(signature = (key, seq, *, branch = None))]
    fn at(&self, key: &str, seq: u64, branch: Option<&str>) -> PyResult<Option<Entry>> {
        let view = self.db.at(self.branch(branch), seq).map_err(to_py)?;
        Ok(view.get(key).map(|e| Entry::from_core(&e)))
    }

    /// What differs between two branches: `(key, change)` pairs.
    fn diff(&self, a: &str, b: &str) -> PyResult<Vec<(String, String)>> {
        Ok(self
            .db
            .diff(a, b)
            .map_err(to_py)?
            .into_iter()
            .map(|c| (c.key, c.kind.marker().to_string()))
            .collect())
    }

    fn __repr__(&self) -> String {
        format!(
            "Database(default_branch={:?}, branches={})",
            self.db.default_branch(),
            self.db.branches().len()
        )
    }
}

impl Database {
    /// The branch a call names, or the default.
    fn branch<'a>(&'a self, given: Option<&'a str>) -> &'a str {
        given.unwrap_or_else(|| self.db.default_branch())
    }
}

/// Run the `memfork` command, and return its exit status.
///
/// This is the command the wheel puts on the path. It is the binary's code,
/// not a reimplementation of it: one command surface, however MemFork was
/// installed.
#[pyfunction]
#[pyo3(signature = (argv))]
fn run_cli(py: Python<'_>, argv: Vec<String>) -> PyResult<i32> {
    // The command blocks — `memfork mcp` serves until its client goes away —
    // so the GIL has to be released or nothing else in this interpreter can
    // run, including the signal handlers that stop it.
    let code = py.detach(|| memfork::run::from_argv(argv));
    Ok(code)
}

/// The version of the MemFork this wheel carries.
#[pyfunction]
fn version() -> &'static str {
    memfork::VERSION
}

#[pymodule]
fn _memfork(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<Database>()?;
    module.add_class::<Entry>()?;
    module.add_class::<Hit>()?;
    module.add_class::<Commit>()?;
    module.add_class::<Merge>()?;
    module.add_function(wrap_pyfunction!(run_cli, module)?)?;
    module.add_function(wrap_pyfunction!(version, module)?)?;
    module.add("__version__", memfork::VERSION)?;
    Ok(())
}
