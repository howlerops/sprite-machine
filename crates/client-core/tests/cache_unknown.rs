//! The cache keeps the wire text of every envelope (turn-event.md §2): an event this build does
//! not know is stored as its original JSON and comes back byte-identical after a restart, so a
//! newer client can interpret it. Known envelopes round-trip too.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use client_core::ThreadSync;
use client_core::cache::{Cache, CacheError};
use protocol::turn_event::{Envelope, Event};
use serde_json::{Value, json};

const RUN_STARTED_EXAMPLE: &str =
    include_str!("../../protocol/schemas/turn-event/examples/RunStarted.json");

const THREAD: &str = "th_1";

/// A known envelope (RunStarted) at seq 1, as compact wire text.
fn known_text() -> (String, Value) {
    let mut value: Value = serde_json::from_str(RUN_STARTED_EXAMPLE).expect("example parses");
    value["seq"] = json!(1);
    (serde_json::to_string(&value).expect("serializes"), value)
}

/// An envelope of a type this build does not know, at `seq`. The spacing and key order are
/// deliberately not the serializer's, so only a verbatim copy can match it.
fn unknown_text(seq: u64) -> String {
    format!(
        "{{ \"v\": 1,  \"seq\": {seq}, \"thread_id\": \"{THREAD}\", \"task_id\": null, \
         \"run_id\": null, \"ts\": \"2026-10-27T22:00:04.000Z\", \
         \"event\": {{ \"type\": \"SomethingFromTheFuture\", \"x\": [1, 2.50, \"y\"] }} }}"
    )
}

fn temp_db(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "client-core-cache-unknown-{tag}-{}-{nanos}.sqlite",
        std::process::id()
    ))
}

#[test]
fn unknown_event_is_cached_as_its_original_json_and_survives_a_restart() {
    let path = temp_db("restart");
    let (known, _) = known_text();
    let unknown = unknown_text(2);
    {
        let mut cache = Cache::open(&path).expect("cache opens");
        cache
            .append_raw(THREAD, &[known.as_str(), unknown.as_str()])
            .expect("persist");
    }

    // A restart: the file is reopened and nothing is held in memory.
    let cache = Cache::open(&path).expect("cache reopens");
    let stored = cache.raw_events(THREAD).expect("raw events");
    assert_eq!(stored, vec![known.clone(), unknown.clone()]);
    assert_eq!(cache.last_seq(THREAD).expect("last seq"), 2);

    // The typed view still ignores the unknown event but keeps its seq, so the resume point
    // and the restored view are right.
    let envelopes = cache.events(THREAD).expect("events");
    assert_eq!(envelopes[1].event, Event::Unknown);
    let restored = ThreadSync::restore(THREAD, envelopes).expect("contiguous");
    assert_eq!(restored.last_seq(), 2);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn known_envelopes_round_trip_through_the_raw_path() {
    let (known, original) = known_text();
    let mut cache = Cache::open_in_memory().expect("cache");
    cache
        .append_raw(THREAD, &[known.as_str()])
        .expect("persist");

    let stored = cache.raw_events(THREAD).expect("raw events");
    assert_eq!(stored, vec![known.clone()], "stored text is the input text");

    let typed: Envelope = serde_json::from_str(&stored[0]).expect("parses");
    assert!(!matches!(typed.event, Event::Unknown));
    assert_eq!(serde_json::to_value(&typed).expect("serializes"), original);
    assert_eq!(cache.events(THREAD).expect("events"), vec![typed]);
}

#[test]
fn a_repeated_unknown_event_keeps_the_first_bytes_and_bad_input_stores_nothing() {
    let (known, _) = known_text();
    let unknown = unknown_text(2);
    let respaced = unknown.replace(' ', "  ");
    let mut cache = Cache::open_in_memory().expect("cache");
    cache
        .append_raw(THREAD, &[known.as_str(), unknown.as_str()])
        .expect("persist");

    // A duplicate at or below the stored seq is skipped: the first copy's bytes stay.
    let last = cache
        .append_raw(THREAD, &[respaced.as_str()])
        .expect("duplicate is not an error");
    assert_eq!(last, 2);
    assert_eq!(cache.raw_events(THREAD).expect("raw"), vec![known, unknown]);

    // A gap is refused and nothing from the batch is stored.
    let gap = unknown_text(5);
    assert!(matches!(
        cache.append_raw(THREAD, &[gap.as_str()]),
        Err(CacheError::Gap {
            last: 2,
            got: 5,
            ..
        })
    ));
    // Malformed JSON is refused before anything is written.
    let good = unknown_text(3);
    assert!(matches!(
        cache.append_raw(THREAD, &[good.as_str(), "{\"seq\": 4"]),
        Err(CacheError::Json(_))
    ));
    assert_eq!(cache.last_seq(THREAD).expect("last seq"), 2);
}
