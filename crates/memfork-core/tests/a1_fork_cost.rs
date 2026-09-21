//! A1 — forking a 1,000,000-key branch takes under 1 ms and allocates under 1 KB.
//!
//! This is the claim the whole design rests on: branching is structural
//! sharing, not copying. The test measures both halves of it — wall-clock time
//! and bytes allocated — and also checks the structural fact underneath, that
//! the two branches point at literally the same root allocation.
//!
//! The allocation counter is a dev-dependency (`cap`), so no `unsafe` appears
//! in MemFork's own source.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::alloc::System;
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use cap::Cap;
use memfork_core::{Db, Store, Value};

#[global_allocator]
static ALLOC: Cap<System> = Cap::new(System, usize::MAX);

/// The allocation counter is process-wide, so two tests measuring it at once
/// would each see the other's allocations. Every test in this file takes this
/// lock for its whole body, which makes the measurement the only thing
/// allocating.
static MEASURING: Mutex<()> = Mutex::new(());

fn measuring() -> MutexGuard<'static, ()> {
    // A panic in one test must not stop the other from running.
    MEASURING.lock().unwrap_or_else(|e| e.into_inner())
}

/// A million keys in debug is slow but not unreasonable; in release it is a
/// couple of seconds. The number is fixed by the acceptance criterion.
const KEYS: usize = 1_000_000;

#[test]
fn a1_fork_of_a_million_keys_is_constant_time_and_constant_space() {
    let _guard = measuring();
    let db = Db::new();

    // Build the branch in batches so the transaction's staging area stays
    // small; the resulting branch is one million keys either way.
    let batch = 50_000;
    let mut written = 0usize;
    while written < KEYS {
        let mut txn = db.begin("main").unwrap();
        for i in written..(written + batch).min(KEYS) {
            txn.put(&format!("k:{i:07}"), Value::new(format!("v{i}")))
                .unwrap();
        }
        txn.commit(Some("bulk load".to_owned())).unwrap();
        written += batch;
    }
    assert_eq!(db.read("main").unwrap().len(), KEYS);

    // Measure only the fork itself.
    let before_bytes = ALLOC.allocated();
    let start = Instant::now();
    db.fork("main", "attempt").unwrap();
    let elapsed = start.elapsed();
    let after_bytes = ALLOC.allocated();

    let allocated = after_bytes.saturating_sub(before_bytes);

    assert!(
        elapsed.as_micros() < 1_000,
        "fork of {KEYS} keys took {elapsed:?}, expected under 1 ms"
    );
    assert!(
        allocated < 1024,
        "fork of {KEYS} keys allocated {allocated} bytes, expected under 1 KiB"
    );

    // The reason it is cheap: both branches are the same commit, and their
    // roots are one allocation, not two.
    assert_eq!(db.head("main").unwrap(), db.head("attempt").unwrap());
    let parent = db.read("main").unwrap();
    let child = db.read("attempt").unwrap();
    assert!(
        parent
            .commit()
            .root
            .shares_allocation_with(&child.commit().root),
        "fork copied the root instead of sharing it"
    );
    assert_eq!(child.len(), KEYS);
}

#[test]
fn a1_fork_cost_does_not_grow_with_the_database() {
    let _guard = measuring();
    // The same fork, on a database of a hundred keys and on one of a hundred
    // thousand, must cost the same. This catches an implementation that is
    // merely fast rather than actually constant.
    fn fork_bytes(keys: usize, branch: &str) -> usize {
        let db = Db::new();
        let mut txn = db.begin("main").unwrap();
        for i in 0..keys {
            txn.put(&format!("k:{i:07}"), Value::new(format!("v{i}")))
                .unwrap();
        }
        txn.commit(None).unwrap();
        let before = ALLOC.allocated();
        db.fork("main", branch).unwrap();
        ALLOC.allocated().saturating_sub(before)
    }

    let small = fork_bytes(100, "small-fork");
    let large = fork_bytes(100_000, "large-fork");
    assert!(
        large <= small + 64,
        "forking a large branch allocated {large} bytes against {small} for a small one"
    );
}
