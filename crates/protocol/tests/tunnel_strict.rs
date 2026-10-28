//! Strict parsing for tunnel frames and commands (docs/specs/tunnel.md §3, §3.5, §6 step 1).
//!
//! Every frame and command type rejects unknown fields and unknown `t` / `kind` values. An
//! unknown `command.kind` is reported by `parse_command_frame` so the runner can answer it with
//! `command_result{ok:false, code:"unsupported"}`. `envelope` in `event` frames is not checked here.

use protocol::tunnel::{Frame, ParsedCommand, parse_command_frame, parse_frame};
use serde_json::{Value, json};

/// Strictly parses `v` as a frame (via [`parse_frame`]). Used to check acceptance and rejection.
fn frame_accepts(v: &Value) -> bool {
    parse_frame(&v.to_string()).is_ok()
}

/// A copy of `v` with an extra field `"extra": 1` inserted at the object found by `pointer`.
fn with_extra_at(mut v: Value, pointer: &str) -> Value {
    v.pointer_mut(pointer)
        .and_then(Value::as_object_mut)
        .expect("object at pointer")
        .insert("extra".to_owned(), json!(1));
    v
}

/// One valid example per frame type, built from the spec's §3 shapes.
fn frame_fixtures() -> Vec<(&'static str, Value)> {
    vec![
        (
            "hello",
            json!({
                "t": "hello", "proto": 1, "machine_id": "mch_1", "runner_version": "0.3.0",
                "resume_token": null, "last_down_seq": 0,
                "capabilities": {
                    "host_mode": "local", "harnesses": ["claude"], "max_sessions": 4,
                    "spawn": ["same_dir"], "features": []
                }
            }),
        ),
        (
            "welcome",
            json!({
                "t": "welcome", "proto": 1, "resumed": true, "resume_token": "rt_1",
                "last_up_seq": 3, "heartbeat_ms": 20000, "max_inflight": 256
            }),
        ),
        (
            "event",
            json!({ "t": "event", "seq": 1, "event_id": "evt_1", "envelope": { "v": 1 } }),
        ),
        (
            "thread_open",
            json!({
                "t": "thread_open", "seq": 2, "event_id": "evt_2", "thread_id": "th_1",
                "title": "t", "agent": "claude",
                "workspace": { "path": "/p", "branch": "main" }
            }),
        ),
        (
            "command_result",
            json!({
                "t": "command_result", "seq": 3, "event_id": "evt_3", "idempotency_key": "idk_1",
                "ok": false, "code": "capacity", "message": "full"
            }),
        ),
        (
            "command",
            json!({
                "t": "command", "seq": 4, "idempotency_key": "idk_2",
                "issued_at": "2026-10-27T22:01:00.000Z",
                "command": { "kind": "cancel", "thread_id": "th_1", "task_id": "tk_1" }
            }),
        ),
        ("ack", json!({ "t": "ack", "seq": 5 })),
        (
            "nack",
            json!({ "t": "nack", "seq": 5, "code": "forbidden", "message": "no" }),
        ),
        (
            "presence",
            json!({ "t": "presence", "state": "busy", "reason": "full" }),
        ),
        ("ping", json!({ "t": "ping" })),
        ("pong", json!({ "t": "pong" })),
        (
            "close",
            json!({ "t": "close", "code": "protocol_error", "message": "bad" }),
        ),
    ]
}

#[test]
fn extra_top_level_field_is_rejected_on_every_frame_type() {
    for (name, valid) in frame_fixtures() {
        assert!(frame_accepts(&valid), "{name} fixture must be valid");
        let extra = with_extra_at(valid, "");
        assert!(
            !frame_accepts(&extra),
            "{name} with an extra field must fail"
        );
    }
}

#[test]
fn unknown_frame_tag_and_tag_case_are_rejected() {
    assert!(!frame_accepts(&json!({ "t": "resize", "seq": 1 })));
    assert!(!frame_accepts(&json!({ "t": "Ping" })));
    assert!(!frame_accepts(&json!({ "seq": 1 })));
    // Unit frames take no fields at all.
    assert!(!frame_accepts(&json!({ "t": "ping", "extra": 1 })));
    assert!(!frame_accepts(&json!({ "t": "pong", "seq": 1 })));
}

#[test]
fn extra_field_in_nested_types_is_rejected() {
    // (fixture, pointers to the nested objects that must reject an extra field)
    let cases: Vec<(Value, Vec<&str>)> = vec![
        (
            // Capabilities (§3.1)
            json!({
                "t": "hello", "proto": 1, "machine_id": "mch_1", "runner_version": "0.3.0",
                "resume_token": null, "last_down_seq": 0,
                "capabilities": {
                    "host_mode": "local", "harnesses": [], "max_sessions": 1,
                    "spawn": [], "features": []
                }
            }),
            vec!["/capabilities"],
        ),
        (
            // Workspace (§3.4)
            json!({
                "t": "thread_open", "seq": 2, "event_id": "evt_2", "thread_id": "th_1",
                "title": "t", "agent": "claude", "workspace": { "path": "/p" }
            }),
            vec!["/workspace"],
        ),
        (
            // prompt: Actor, ContentPart::Text and ContentPart::File (§3.5)
            json!({
                "t": "command", "seq": 4, "idempotency_key": "idk_2",
                "issued_at": "2026-10-27T22:01:00.000Z",
                "command": {
                    "kind": "prompt", "thread_id": "th_1", "task_id": "tk_2",
                    "message_id": "msg_1",
                    "parts": [
                        { "type": "text", "text": "hi" },
                        { "type": "file", "name": "a", "mime_type": "text/plain",
                          "url": "https://hub.example/a", "size": 1 }
                    ],
                    "author": { "kind": "user", "id": "usr_1" }
                }
            }),
            vec![
                "/command",
                "/command/parts/0",
                "/command/parts/1",
                "/command/author",
            ],
        ),
        (
            // approval: Outcome and resolved_by (§3.5)
            json!({
                "t": "command", "seq": 4, "idempotency_key": "idk_2",
                "issued_at": "2026-10-27T22:01:00.000Z",
                "command": {
                    "kind": "approval", "thread_id": "th_1", "task_id": "tk_1",
                    "interrupt_id": "int_1",
                    "outcome": { "type": "selected", "option_id": "allow" },
                    "resolved_by": { "kind": "user", "id": "usr_1" }
                }
            }),
            vec!["/command/outcome", "/command/resolved_by"],
        ),
        (
            // dispatch: Workspace and parts (§3.5)
            json!({
                "t": "command", "seq": 4, "idempotency_key": "idk_2",
                "issued_at": "2026-10-27T22:01:00.000Z",
                "command": {
                    "kind": "dispatch", "thread_id": "th_2", "task_id": "tk_3",
                    "agent": "claude", "workspace": { "path": "/p", "spawn": "worktree" },
                    "permission_mode": "default", "message_id": "msg_9",
                    "parts": [{ "type": "text", "text": "x" }],
                    "author": { "kind": "user", "id": "usr_1" }
                }
            }),
            vec!["/command/workspace", "/command/parts/0"],
        ),
        (
            // cancel (§3.5)
            json!({
                "t": "command", "seq": 4, "idempotency_key": "idk_2",
                "issued_at": "2026-10-27T22:01:00.000Z",
                "command": { "kind": "cancel", "thread_id": "th_1", "task_id": "tk_1" }
            }),
            vec!["/command"],
        ),
    ];

    for (valid, pointers) in cases {
        assert!(frame_accepts(&valid), "fixture must be valid: {valid}");
        for pointer in pointers {
            let extra = with_extra_at(valid.clone(), pointer);
            assert!(
                !frame_accepts(&extra),
                "extra field at {pointer} must fail: {extra}"
            );
        }
    }
}

#[test]
fn event_envelope_is_not_checked_here() {
    // §6 step 3: the envelope is validated by the TurnEvent layer, not by the frame parser.
    let frame = json!({
        "t": "event", "seq": 1, "event_id": "evt_1",
        "envelope": { "v": 1, "anything": "goes", "extra": [1, 2] }
    });
    assert!(frame_accepts(&frame));
}

#[test]
fn parse_command_frame_returns_typed_for_a_known_kind() {
    let raw = json!({
        "t": "command", "seq": 42, "idempotency_key": "idk_01JBW5A1QX",
        "issued_at": "2026-10-27T22:01:00.000Z",
        "command": { "kind": "cancel", "thread_id": "th_1", "task_id": "tk_1" }
    })
    .to_string();
    match parse_command_frame(&raw).expect("known kind parses") {
        ParsedCommand::Typed(frame) => match *frame {
            Frame::Command {
                seq: 42,
                ref idempotency_key,
                ..
            } => assert_eq!(idempotency_key, "idk_01JBW5A1QX"),
            other => panic!("expected a command frame, got {other:?}"),
        },
        other => panic!("expected typed command, got {other:?}"),
    }
}

#[test]
fn unknown_kind_is_reported_and_answered_as_unsupported() {
    let raw = json!({
        "t": "command", "seq": 7, "idempotency_key": "idk_7",
        "issued_at": "2026-10-27T22:01:00.000Z",
        "command": { "kind": "resize", "thread_id": "th_1", "cols": 80 }
    })
    .to_string();
    let parsed = parse_command_frame(&raw).expect("unknown kind is not a protocol error");
    assert_eq!(
        parsed,
        ParsedCommand::Unsupported {
            seq: 7,
            idempotency_key: "idk_7".to_owned(),
            issued_at: "2026-10-27T22:01:00.000Z".to_owned(),
            kind: "resize".to_owned(),
        }
    );

    // The runner's reply: command_result{ok:false, code:"unsupported"} (§3.5, §3.6).
    let reply = parsed
        .unsupported_result(9, "evt_9")
        .expect("unsupported commands have a reply");
    let reply_json = serde_json::to_value(&reply).expect("reply serializes");
    assert_eq!(
        reply_json,
        json!({
            "t": "command_result", "seq": 9, "event_id": "evt_9",
            "idempotency_key": "idk_7", "ok": false, "code": "unsupported",
            "message": "unsupported command kind `resize`"
        })
    );
    assert!(frame_accepts(&reply_json), "the reply is a valid frame");
}

#[test]
fn typed_command_has_no_unsupported_reply() {
    let raw = json!({
        "t": "command", "seq": 1, "idempotency_key": "idk_1",
        "issued_at": "2026-10-27T22:01:00.000Z",
        "command": { "kind": "cancel", "thread_id": "th_1", "task_id": "tk_1" }
    })
    .to_string();
    let parsed = parse_command_frame(&raw).expect("parses");
    assert!(parsed.unsupported_result(2, "evt_2").is_none());
}

#[test]
fn unknown_kind_with_bad_envelope_is_a_protocol_error() {
    let base = json!({
        "t": "command", "seq": 7, "idempotency_key": "idk_7",
        "issued_at": "2026-10-27T22:01:00.000Z",
        "command": { "kind": "resize" }
    });
    // Extra top-level field.
    let extra = with_extra_at(base.clone(), "");
    assert!(parse_command_frame(&extra.to_string()).is_err());
    // Missing idempotency_key.
    let mut missing = base.clone();
    missing
        .as_object_mut()
        .expect("object")
        .remove("idempotency_key");
    assert!(parse_command_frame(&missing.to_string()).is_err());
    // `command` is not an object.
    let mut not_object = base;
    not_object["command"] = json!("resize");
    assert!(parse_command_frame(&not_object.to_string()).is_err());
}

#[test]
fn known_kind_with_extra_field_is_a_protocol_error() {
    let raw = json!({
        "t": "command", "seq": 1, "idempotency_key": "idk_1",
        "issued_at": "2026-10-27T22:01:00.000Z",
        "command": { "kind": "cancel", "thread_id": "th_1", "task_id": "tk_1", "extra": 1 }
    })
    .to_string();
    assert!(parse_command_frame(&raw).is_err());
}

#[test]
fn input_that_is_not_a_command_frame_is_an_error() {
    assert!(parse_command_frame(r#"{ "t": "ack", "seq": 1 }"#).is_err());
    assert!(
        parse_command_frame(
            r#"{ "t": "command", "seq": 1, "idempotency_key": "k", "issued_at": "t" }"#
        )
        .is_err()
    );
    assert!(parse_command_frame(r#"{ "t": "command", "seq": 1, "idempotency_key": "k", "issued_at": "t", "command": { "kind": 3 } }"#).is_err());
    assert!(parse_command_frame("not json").is_err());
}
