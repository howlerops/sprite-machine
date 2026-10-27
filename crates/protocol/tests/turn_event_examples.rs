//! `protocol::turn_event` against the SPEC-001 examples (`crates/protocol/schemas/turn-event/`).
//!
//! Every example must deserialize into [`Envelope`] and serialize back to an identical
//! `serde_json::Value` (turn-event.md §8), and every schema in `events/` must have an example.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use protocol::turn_event::{
    Envelope, Event, RunFinished, RunOutcome, RunReason, RunStarted, StampError, ThreadSeq,
    format_timestamp, is_valid_id,
};
use serde_json::{Value, json};

fn schema_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas/turn-event")
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    files
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

#[test]
fn every_example_round_trips_to_an_identical_value() {
    let examples = json_files(&schema_dir().join("examples"));
    assert!(
        examples.len() >= 27,
        "expected ≥ 27 examples, found {}",
        examples.len()
    );
    for path in examples {
        let original = read_json(&path);
        let typed: Envelope = serde_json::from_value(original.clone())
            .unwrap_or_else(|e| panic!("{} does not deserialize: {e}", path.display()));
        assert!(
            !matches!(typed.event, Event::Unknown),
            "{} parsed as an unknown event",
            path.display()
        );
        let back = serde_json::to_value(&typed).expect("envelope serializes");
        assert_eq!(back, original, "{} changed on round trip", path.display());
    }
}

#[test]
fn every_variant_has_a_schema_and_an_example() {
    let schemas: BTreeSet<String> = json_files(&schema_dir().join("events"))
        .iter()
        .map(|path| {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .expect("file name");
            name.trim_end_matches(".schema.json").to_owned()
        })
        .collect();
    let variants: BTreeSet<String> = Event::TYPE_NAMES.iter().map(|s| (*s).to_owned()).collect();
    assert_eq!(
        schemas, variants,
        "Event variants differ from events/*.schema.json"
    );

    let mut seen = BTreeSet::new();
    for path in json_files(&schema_dir().join("examples")) {
        let typed: Envelope = serde_json::from_value(read_json(&path)).expect("example parses");
        seen.insert(typed.event.type_name().to_owned());
    }
    assert_eq!(seen, variants, "some variants have no example");
}

#[test]
fn unknown_event_type_is_ignored_not_an_error() {
    let value = json!({
        "v": 1, "seq": 3, "thread_id": "th_1", "task_id": null, "run_id": null,
        "ts": "2026-10-27T22:00:03.000Z",
        "event": { "type": "SomethingFromTheFuture", "x": 1 }
    });
    let typed: Envelope = serde_json::from_value(value).expect("unknown type still parses");
    assert_eq!(typed.event, Event::Unknown);
}

#[test]
fn other_envelope_versions_are_rejected() {
    let mut value = read_json(&schema_dir().join("examples/RunStarted.json"));
    value["v"] = json!(2);
    assert!(serde_json::from_value::<Envelope>(value).is_err());
}

#[test]
fn null_task_and_run_ids_are_serialized() {
    let envelope = Envelope::new(
        "th_1",
        None,
        None,
        "2026-10-27T22:00:03.000Z",
        RunFinished {
            outcome: RunOutcome::Success { stop_reason: None },
        },
    );
    let value = serde_json::to_value(&envelope).expect("serializes");
    assert_eq!(value["task_id"], Value::Null);
    assert_eq!(value["run_id"], Value::Null);
    assert_eq!(value["seq"], json!(0));
    assert!(value.get("raw").is_none());
}

#[test]
fn thread_seq_stamps_gap_free_from_one() {
    let mut seq = ThreadSeq::new();
    let make = || {
        Envelope::new(
            "th_1",
            Some("tk_1".into()),
            Some("run_1".into()),
            format_timestamp(0),
            RunStarted {
                reason: RunReason::Prompt,
                parent_run_id: None,
                agent_id: None,
                machine_id: None,
            },
        )
    };
    let stamped: Vec<u64> = (0..5)
        .map(|_| seq.stamp(make()).expect("producer envelope").seq)
        .collect();
    assert_eq!(stamped, vec![1, 2, 3, 4, 5]);
    assert_eq!(seq.last(), 5);

    // Producers send seq 0; an already-stamped envelope is refused and does not consume a seq.
    let mut already = make();
    already.seq = 9;
    assert_eq!(
        seq.stamp(already),
        Err(StampError::AlreadyStamped { seq: 9 })
    );
    assert_eq!(seq.next_seq(), 6);

    let mut resumed = ThreadSeq::resume(41);
    assert_eq!(resumed.stamp(make()).expect("stamps").seq, 42);
}

#[test]
fn ids_follow_the_spec_pattern() {
    assert!(is_valid_id("th_01JBW52B6Z"));
    assert!(is_valid_id("tc_1:src.main-rs"));
    assert!(!is_valid_id(""));
    assert!(!is_valid_id("has space"));
    assert!(!is_valid_id(&"x".repeat(129)));
}
