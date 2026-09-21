//! What is remembered about a discarded branch once its commits are gone.
//!
//! Discarding frees every commit only that branch could reach, so the graph
//! keeps no trace of it. A history view still wants to say "an attempt forked
//! here and was thrown away", so the engine keeps a short list of discards —
//! rebuilt identically by replaying the log.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use memfork_core::{Db, Journal, JournalError, Record, Value};

#[derive(Debug, Default)]
struct Recorder(Mutex<Vec<Record>>);

impl Journal for Recorder {
    fn append(&self, record: &Record) -> Result<(), JournalError> {
        self.0.lock().unwrap().push(record.clone());
        Ok(())
    }
}

#[test]
fn a_discard_is_remembered_by_name_fork_point_and_size() {
    let db = Db::new();
    db.put("main", "a", Value::new("1")).unwrap();
    let fork_point = db.head("main").unwrap();
    db.fork("main", "attempt").unwrap();
    db.put("attempt", "b", Value::new("2")).unwrap();
    db.put("attempt", "c", Value::new("3")).unwrap();
    let before = db.commit_count();
    db.discard("attempt").unwrap();

    // The commits really are gone...
    assert!(db.commit_count() < before);
    // ...and what is left to say about them is kept.
    let gone = db.discarded();
    assert_eq!(gone.len(), 1);
    assert_eq!(gone[0].name, "attempt");
    assert_eq!(gone[0].forked_at, fork_point);
    assert_eq!(gone[0].commits, 2);
    // The fork point is still in the graph, so a view can attach it there.
    assert!(db.commit(fork_point).is_ok());
}

#[test]
fn replaying_the_log_rebuilds_the_same_record() {
    let recorder = Arc::new(Recorder::default());
    let db = Db::new();
    db.set_journal(Some(recorder.clone() as Arc<dyn Journal>));
    db.put("main", "a", Value::new("1")).unwrap();
    db.fork("main", "one").unwrap();
    db.put("one", "x", Value::new("x")).unwrap();
    db.discard("one").unwrap();
    db.fork("main", "two").unwrap();
    db.discard("two").unwrap();

    let replayed = Db::new();
    for record in recorder.0.lock().unwrap().iter() {
        replayed.apply_record(record).unwrap();
    }
    assert_eq!(replayed.discarded(), db.discarded());
    assert_eq!(replayed.discarded().len(), 2);
}

#[test]
fn only_the_most_recent_discards_are_kept() {
    let db = Db::new();
    for i in 0..(memfork_core::DISCARDS_REMEMBERED + 5) {
        let name = format!("b{i}");
        db.fork("main", &name).unwrap();
        db.discard(&name).unwrap();
    }
    let gone = db.discarded();
    assert_eq!(gone.len(), memfork_core::DISCARDS_REMEMBERED);
    assert_eq!(gone[0].name, "b5");
}

#[test]
fn branches_say_where_they_forked() {
    let db = Db::new();
    db.put("main", "a", Value::new("1")).unwrap();
    let at = db.head("main").unwrap();
    db.fork("main", "side").unwrap();
    db.put("main", "b", Value::new("2")).unwrap();
    let side = db
        .branches()
        .into_iter()
        .find(|b| b.name == "side")
        .unwrap();
    assert_eq!(side.forked_at, at);
}
