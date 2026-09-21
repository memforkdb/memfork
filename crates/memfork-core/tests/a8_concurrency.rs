//! A8 — 8 reader threads plus 1 writer per branch, across 4 branches, for
//! 10 seconds: no panics and no torn reads.
//!
//! "No torn read" is checked with an invariant a half-applied commit would
//! break. Each writer commits a whole round in one transaction: every one of
//! its keys is set to the same round number, and a witness key records it. A
//! reader that ever saw part of a commit would find keys from two different
//! rounds in one view.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use memfork_core::{Db, Value};

const BRANCHES: usize = 4;
const READERS_PER_BRANCH: usize = 8;
const KEYS_PER_ROUND: usize = 16;
const DURATION: Duration = Duration::from_secs(10);

fn branch_name(i: usize) -> String {
    format!("worker-{i}")
}

#[test]
fn a8_readers_and_writers_never_tear_a_commit() {
    let db = Db::new();
    for i in 0..BRANCHES {
        db.fork("main", &branch_name(i)).unwrap();
    }

    let stop = Arc::new(AtomicBool::new(false));
    let commits = Arc::new(AtomicU64::new(0));
    let reads = Arc::new(AtomicU64::new(0));
    // Every thread starts together, so the run is genuinely concurrent rather
    // than a series of thread startups.
    let barrier = Arc::new(Barrier::new(BRANCHES * (READERS_PER_BRANCH + 1)));

    let mut handles = Vec::new();

    for b in 0..BRANCHES {
        let branch = branch_name(b);

        // One writer per branch.
        {
            let db = db.clone();
            let branch = branch.clone();
            let stop = Arc::clone(&stop);
            let commits = Arc::clone(&commits);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let mut round: u64 = 0;
                while !stop.load(Ordering::Relaxed) {
                    round += 1;
                    let mut txn = db.begin(&branch).unwrap();
                    for k in 0..KEYS_PER_ROUND {
                        txn.put(&format!("k:{k:02}"), Value::new(round.to_string()))
                            .unwrap();
                    }
                    txn.put("witness", Value::new(round.to_string())).unwrap();
                    txn.commit(Some(format!("round {round}"))).unwrap();
                    commits.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        // Eight readers per branch.
        for _ in 0..READERS_PER_BRANCH {
            let db = db.clone();
            let branch = branch.clone();
            let stop = Arc::clone(&stop);
            let reads = Arc::clone(&reads);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let mut last_seq = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let view = db.read(&branch).unwrap();

                    // A branch head only ever moves forward.
                    assert!(
                        view.seq() >= last_seq,
                        "branch {branch} went backwards: {} after {last_seq}",
                        view.seq()
                    );
                    last_seq = view.seq();

                    if view.is_empty() {
                        continue; // before the writer's first round
                    }

                    // Every key of a round carries that round's number, and
                    // the witness agrees. A torn read would break this.
                    let witness = view
                        .get("witness")
                        .expect("a committed round always has its witness");
                    let round = String::from_utf8_lossy(&witness.value).into_owned();
                    assert_eq!(
                        view.len(),
                        KEYS_PER_ROUND + 1,
                        "branch {branch} was seen with a partial key set"
                    );
                    for k in 0..KEYS_PER_ROUND {
                        let entry = view
                            .get(&format!("k:{k:02}"))
                            .expect("a committed round always has all its keys");
                        assert_eq!(
                            String::from_utf8_lossy(&entry.value),
                            round,
                            "branch {branch} showed key k:{k:02} from a different round \
                             than the witness: a torn read"
                        );
                    }

                    // And the same must hold for a past commit read concurrently.
                    if view.seq() > 1 {
                        let past = db.at(&branch, view.seq() - 1).unwrap();
                        let past_round = past
                            .get("witness")
                            .map(|e| String::from_utf8_lossy(&e.value).into_owned());
                        if let Some(past_round) = past_round {
                            for k in 0..KEYS_PER_ROUND {
                                let entry = past.get(&format!("k:{k:02}")).unwrap();
                                assert_eq!(String::from_utf8_lossy(&entry.value), past_round);
                            }
                        }
                    }

                    reads.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
    }

    let start = Instant::now();
    while start.elapsed() < DURATION {
        std::thread::sleep(Duration::from_millis(50));
    }
    stop.store(true, Ordering::Relaxed);

    for h in handles {
        h.join().expect("a worker thread panicked");
    }

    let commits = commits.load(Ordering::Relaxed);
    let reads = reads.load(Ordering::Relaxed);
    assert!(commits > 0, "no writer made progress");
    assert!(reads > 0, "no reader made progress");
    eprintln!("A8: {commits} commits and {reads} reads in {DURATION:?}");

    // Each branch ends on a whole, consistent round, and `main` never moved.
    for b in 0..BRANCHES {
        let view = db.read(&branch_name(b)).unwrap();
        assert_eq!(view.len(), KEYS_PER_ROUND + 1);
    }
    assert!(db.read("main").unwrap().is_empty());
    assert_eq!(db.read("main").unwrap().seq(), 0);
}

#[test]
fn a8_concurrent_writers_on_one_branch_never_lose_a_commit() {
    // DESIGN §4.3 allows one writer per branch, but a caller that does
    // otherwise must still get correct behaviour: the loser retries rather
    // than silently overwriting, and no commit is lost.
    //
    // Deliberately oversubscribed — more writer threads than the machine has
    // cores — because that is the shape that catches a retry budget running
    // out under contention rather than a writer actually failing.
    let db = Db::new();
    let cores = std::thread::available_parallelism().map_or(4, NonZeroUsize::get);
    let threads = (cores * 4).clamp(8, 64);
    let per_thread = 40;
    let barrier = Arc::new(Barrier::new(threads));

    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let db = db.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for i in 0..per_thread {
                    db.put("main", &format!("t{t}:{i:03}"), Value::new("v"))
                        .unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("a writer thread panicked");
    }

    let view = db.read("main").unwrap();
    assert_eq!(view.len(), threads * per_thread);
    assert_eq!(
        view.seq() as usize,
        threads * per_thread,
        "every successful put must be exactly one commit"
    );
}

#[test]
fn a8_writers_on_different_branches_do_not_block_each_other() {
    let db = Db::new();
    for i in 0..BRANCHES {
        db.fork("main", &branch_name(i)).unwrap();
    }
    let barrier = Arc::new(Barrier::new(BRANCHES));
    let handles: Vec<_> = (0..BRANCHES)
        .map(|b| {
            let db = db.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                // A long-running transaction on one branch must not stop
                // another branch committing.
                let mut txn = db.begin(&branch_name(b)).unwrap();
                for i in 0..500 {
                    txn.put(&format!("k{i}"), Value::new("v")).unwrap();
                }
                txn.commit(None).unwrap()
            })
        })
        .collect();
    let heads: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().expect("a writer thread panicked"))
        .collect();

    // Same operations on every branch, so identical content addresses.
    assert!(heads.windows(2).all(|w| w[0] == w[1]));
}

#[test]
fn a8_an_explicit_transaction_racing_auto_commits_either_wins_or_says_so() {
    // The engine's own operations take turns, but an explicit transaction is
    // optimistic as DESIGN §4.3 describes: it may lose. What it must
    // never do is half-apply, or report success while another writer's commit
    // is what actually landed.
    let db = Db::new();
    db.put("main", "seed", Value::new("0")).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let noisy = {
        let db = db.clone();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                i += 1;
                db.put("main", "noise", Value::new(i.to_string())).unwrap();
                // Leave the explicit transaction a window to win in, so the
                // test measures both outcomes rather than only starvation.
                std::thread::yield_now();
            }
        })
    };

    let mut won = 0;
    let mut lost = 0;
    for i in 0..200u64 {
        let mut txn = db.begin("main").unwrap();
        txn.put("txn:a", Value::new(i.to_string())).unwrap();
        txn.put("txn:b", Value::new(i.to_string())).unwrap();
        let base = txn.base();
        match txn.commit(Some(format!("attempt {i}"))) {
            Ok(head) => {
                won += 1;
                // Whatever else happened, this commit's two keys agree: the
                // transaction was applied whole.
                let view = db.commit(head).unwrap();
                let a =
                    String::from_utf8_lossy(&view.root.get("txn:a").unwrap().value).into_owned();
                let b =
                    String::from_utf8_lossy(&view.root.get("txn:b").unwrap().value).into_owned();
                assert_eq!(a, b);
                assert_eq!(a, i.to_string());
            }
            Err(memfork_core::Error::Conflict {
                expected, actual, ..
            }) => {
                lost += 1;
                assert_eq!(expected, base, "a conflict must name the base it lost from");
                assert_ne!(actual, base);
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    stop.store(true, Ordering::Relaxed);
    noisy.join().expect("the auto-commit thread panicked");

    assert_eq!(won + lost, 200);
    assert!(won > 0, "the explicit transaction never won a single race");

    // And the branch is intact: every commit in its history is reachable.
    let log = db.log("main", None).unwrap();
    assert_eq!(log.len() as u64, db.read("main").unwrap().seq() + 1);
}
