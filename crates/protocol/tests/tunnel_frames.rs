//! Round-trip tests for tunnel frames (docs/specs/tunnel.md §3). Each JSON example is copied
//! verbatim from the spec. Deserializing it and serializing it back must give an equal value.

use protocol::tunnel::{
    Actor, ActorKind, CloseCode, Command, CommandErrorCode, ContentPart, Frame, NackCode, Outcome,
    PresenceState,
};
use serde_json::Value;

/// Parses `example` as a frame, re-serializes it, and asserts the value is unchanged.
fn roundtrip_frame(example: &str) -> Frame {
    let original: Value = serde_json::from_str(example).expect("example is JSON");
    let typed: Frame = serde_json::from_value(original.clone()).expect("example deserializes");
    assert_eq!(
        serde_json::to_value(&typed).expect("frame serializes"),
        original
    );
    typed
}

/// Same as [`roundtrip_frame`], for a bare `command` object (§3.5).
fn roundtrip_command(example: &str) -> Command {
    let original: Value = serde_json::from_str(example).expect("example is JSON");
    let typed: Command = serde_json::from_value(original.clone()).expect("example deserializes");
    assert_eq!(
        serde_json::to_value(&typed).expect("command serializes"),
        original
    );
    typed
}

#[test]
fn hello_example_roundtrips() {
    let frame = roundtrip_frame(
        r#"{
  "t": "hello",
  "proto": 1,
  "machine_id": "mch_01JBW3K8Q2",
  "runner_version": "0.3.0",
  "resume_token": "rt_01JBW4ZK1Y.9f2c",
  "last_down_seq": 41,
  "capabilities": {
    "host_mode": "local",
    "harnesses": ["claude", "opencode"],
    "max_sessions": 32,
    "spawn": ["same_dir", "worktree"],
    "features": ["checkpoints.shadow_git", "terminals", "attachments"]
  }
}"#,
    );
    match frame {
        Frame::Hello {
            proto,
            resume_token,
            last_down_seq,
            capabilities,
            ..
        } => {
            assert_eq!(proto, 1);
            assert_eq!(resume_token.as_deref(), Some("rt_01JBW4ZK1Y.9f2c"));
            assert_eq!(last_down_seq, 41);
            assert_eq!(capabilities.harnesses, ["claude", "opencode"]);
        }
        other => panic!("expected hello, got {other:?}"),
    }
}

#[test]
fn hello_with_null_resume_token_roundtrips() {
    // §3.1: `resume_token` is `null` on first connect or after the runner lost its state.
    let frame = roundtrip_frame(
        r#"{
  "t": "hello",
  "proto": 1,
  "machine_id": "mch_01JBW3K8Q2",
  "runner_version": "0.3.0",
  "resume_token": null,
  "last_down_seq": 0,
  "capabilities": {
    "host_mode": "sandbox",
    "harnesses": [],
    "max_sessions": 1,
    "spawn": ["same_dir"],
    "features": []
  }
}"#,
    );
    assert!(matches!(
        frame,
        Frame::Hello {
            resume_token: None,
            ..
        }
    ));
}

#[test]
fn welcome_example_roundtrips() {
    let frame = roundtrip_frame(
        r#"{
  "t": "welcome",
  "proto": 1,
  "resumed": true,
  "resume_token": "rt_01JBW51M3C.a7d0",
  "last_up_seq": 118,
  "heartbeat_ms": 20000,
  "max_inflight": 256
}"#,
    );
    assert!(matches!(
        frame,
        Frame::Welcome {
            resumed: true,
            last_up_seq: 118,
            heartbeat_ms: 20000,
            max_inflight: 256,
            ..
        }
    ));
}

#[test]
fn event_example_roundtrips() {
    let frame = roundtrip_frame(
        r#"{
  "t": "event",
  "seq": 119,
  "event_id": "evt_01JBW52A0F",
  "envelope": {
    "v": 1,
    "seq": 0,
    "thread_id": "th_1",
    "task_id": "tk_1",
    "run_id": "run_1",
    "ts": "2026-10-27T22:00:03.000Z",
    "event": {
      "type": "TextMessageContent",
      "message_id": "msg_a1",
      "delta": "Adding the route now."
    }
  }
}"#,
    );
    match frame {
        Frame::Event {
            seq,
            event_id,
            envelope,
        } => {
            assert_eq!(seq, 119);
            assert_eq!(event_id, "evt_01JBW52A0F");
            assert_eq!(envelope["event"]["type"], "TextMessageContent");
        }
        other => panic!("expected event, got {other:?}"),
    }
}

#[test]
fn thread_open_example_roundtrips() {
    let frame = roundtrip_frame(
        r#"{
  "t": "thread_open",
  "seq": 120,
  "event_id": "evt_01JBW52B7H",
  "thread_id": "th_01JBW52B6Z",
  "title": "fix flaky test",
  "agent": "claude",
  "workspace": { "path": "/Users/me/src/app", "branch": "main" }
}"#,
    );
    match frame {
        Frame::ThreadOpen { workspace, .. } => {
            assert_eq!(workspace.path, "/Users/me/src/app");
            assert_eq!(workspace.branch.as_deref(), Some("main"));
        }
        other => panic!("expected thread_open, got {other:?}"),
    }
}

#[test]
fn command_frame_with_prompt_roundtrips() {
    // §3.5 example with the `"...": "per kind, below"` placeholder replaced by the real
    // `prompt` command from §3.5 (the `prompt` example, verbatim).
    let frame = roundtrip_frame(
        r#"{
  "t": "command",
  "seq": 42,
  "idempotency_key": "idk_01JBW5A1QX",
  "issued_at": "2026-10-27T22:01:00.000Z",
  "command": {
    "kind": "prompt",
    "thread_id": "th_1",
    "task_id": "tk_2",
    "message_id": "msg_u2",
    "parts": [
      { "type": "text", "text": "Now add a test" },
      {
        "type": "file",
        "name": "log.txt",
        "mime_type": "text/plain",
        "url": "https://hub.example/att/a2?sig=x",
        "size": 812
      }
    ],
    "author": { "kind": "user", "id": "usr_1", "device_id": "dev_phone" }
  }
}"#,
    );
    match frame {
        Frame::Command {
            seq,
            idempotency_key,
            command: Command::Prompt { parts, author, .. },
            ..
        } => {
            assert_eq!(seq, 42);
            assert_eq!(idempotency_key, "idk_01JBW5A1QX");
            assert_eq!(parts.len(), 2);
            assert_eq!(author.kind, ActorKind::User);
        }
        other => panic!("expected command(prompt), got {other:?}"),
    }
}

#[test]
fn prompt_command_roundtrips() {
    let command = roundtrip_command(
        r#"{
  "kind": "prompt",
  "thread_id": "th_1",
  "task_id": "tk_2",
  "message_id": "msg_u2",
  "parts": [
    { "type": "text", "text": "Now add a test" },
    {
      "type": "file",
      "name": "log.txt",
      "mime_type": "text/plain",
      "url": "https://hub.example/att/a2?sig=x",
      "size": 812
    }
  ],
  "author": { "kind": "user", "id": "usr_1", "device_id": "dev_phone" }
}"#,
    );
    assert!(matches!(command, Command::Prompt { .. }));
}

#[test]
fn approval_command_roundtrips() {
    let command = roundtrip_command(
        r#"{
  "kind": "approval",
  "thread_id": "th_1",
  "task_id": "tk_1",
  "interrupt_id": "int_1",
  "outcome": { "type": "selected", "option_id": "allow" },
  "resolved_by": { "kind": "user", "id": "usr_1", "device_id": "dev_phone" }
}"#,
    );
    match command {
        Command::Approval { outcome, .. } => {
            assert_eq!(
                outcome,
                Outcome::Selected {
                    option_id: "allow".to_owned()
                }
            );
        }
        other => panic!("expected approval, got {other:?}"),
    }
}

#[test]
fn dispatch_command_roundtrips() {
    let command = roundtrip_command(
        r#"{
  "kind": "dispatch",
  "thread_id": "th_01JBW5C0D4",
  "task_id": "tk_01JBW5C0D5",
  "agent": "claude",
  "workspace": { "path": "/Users/me/src/app", "spawn": "worktree" },
  "permission_mode": "default",
  "message_id": "msg_u9",
  "parts": [{ "type": "text", "text": "Upgrade axum to 0.8" }],
  "author": { "kind": "user", "id": "usr_1", "device_id": "dev_phone" }
}"#,
    );
    match command {
        Command::Dispatch {
            workspace, parts, ..
        } => {
            assert_eq!(workspace.spawn.as_deref(), Some("worktree"));
            assert_eq!(
                parts,
                [ContentPart::Text {
                    text: "Upgrade axum to 0.8".to_owned()
                }]
            );
        }
        other => panic!("expected dispatch, got {other:?}"),
    }
}

#[test]
fn cancel_command_roundtrips() {
    let command =
        roundtrip_command(r#"{ "kind": "cancel", "thread_id": "th_1", "task_id": "tk_1" }"#);
    assert_eq!(
        command,
        Command::Cancel {
            thread_id: "th_1".to_owned(),
            task_id: "tk_1".to_owned()
        }
    );
}

#[test]
fn session_config_example_roundtrips() {
    // §3.5 `session_config`, both fields set (the spec example).
    let command = roundtrip_command(
        r#"{
  "kind": "session_config",
  "thread_id": "th_1",
  "mode_id": "plan",
  "config": { "id": "model", "value": "opus" }
}"#,
    );
    match command {
        Command::SessionConfig {
            thread_id,
            mode_id,
            config,
        } => {
            assert_eq!(thread_id, "th_1");
            assert_eq!(mode_id.as_deref(), Some("plan"));
            let config = config.expect("a config change");
            assert_eq!(config.id, "model");
            assert_eq!(config.value, Value::String("opus".to_owned()));
        }
        other => panic!("expected session_config, got {other:?}"),
    }
}

#[test]
fn session_config_with_only_a_mode_or_only_a_boolean_roundtrips() {
    let mode_only = roundtrip_command(
        r#"{ "kind": "session_config", "thread_id": "th_1", "mode_id": "code" }"#,
    );
    assert!(matches!(
        mode_only,
        Command::SessionConfig {
            mode_id: Some(_),
            config: None,
            ..
        }
    ));
    let boolean = roundtrip_command(
        r#"{ "kind": "session_config", "thread_id": "th_1", "config": { "id": "auto_fix", "value": true } }"#,
    );
    assert!(matches!(
        boolean,
        Command::SessionConfig {
            mode_id: None,
            config: Some(_),
            ..
        }
    ));
}

#[test]
fn session_config_is_a_known_kind_and_rejects_unknown_fields() {
    assert!(protocol::tunnel::KNOWN_COMMAND_KINDS.contains(&"session_config"));
    let extra = r#"{ "kind": "session_config", "thread_id": "th_1", "mode_id": "x", "extra": 1 }"#;
    assert!(serde_json::from_str::<Command>(extra).is_err());
    let extra_in_config = r#"{ "kind": "session_config", "thread_id": "th_1",
        "config": { "id": "model", "value": "opus", "extra": 1 } }"#;
    assert!(serde_json::from_str::<Command>(extra_in_config).is_err());
}

#[test]
fn unknown_command_kind_fails_to_parse() {
    // §3.5: the runner must answer an unknown kind with command_result{unsupported}; the
    // typed parse must not silently accept it.
    let raw = r#"{ "kind": "resize", "thread_id": "th_1" }"#;
    assert!(serde_json::from_str::<Command>(raw).is_err());
}

#[test]
fn command_result_example_roundtrips() {
    let frame = roundtrip_frame(
        r#"{
  "t": "command_result",
  "seq": 121,
  "event_id": "evt_01JBW5D2N1",
  "idempotency_key": "idk_01JBW5A1QX",
  "ok": false,
  "code": "capacity",
  "message": "32 sessions already running"
}"#,
    );
    assert!(matches!(
        frame,
        Frame::CommandResult {
            ok: false,
            code: CommandErrorCode::Capacity,
            ..
        }
    ));
}

#[test]
fn ack_example_roundtrips() {
    let frame = roundtrip_frame(r#"{ "t": "ack", "seq": 119 }"#);
    assert_eq!(frame, Frame::Ack { seq: 119 });
}

#[test]
fn nack_example_roundtrips() {
    let frame = roundtrip_frame(
        r#"{
  "t": "nack",
  "seq": 119,
  "code": "invalid_sequence",
  "message": "TextMessageContent before TextMessageStart for msg_a1"
}"#,
    );
    assert!(matches!(
        frame,
        Frame::Nack {
            seq: 119,
            code: NackCode::InvalidSequence,
            ..
        }
    ));
}

#[test]
fn presence_example_roundtrips() {
    let frame = roundtrip_frame(r#"{ "t": "presence", "state": "asleep", "reason": "os_sleep" }"#);
    assert_eq!(
        frame,
        Frame::Presence {
            state: PresenceState::Asleep,
            reason: "os_sleep".to_owned()
        }
    );
}

#[test]
fn ping_and_pong_serialize_to_the_literal_strings() {
    // §3.7: exact literal strings, so the hub can auto-respond while hibernated.
    assert_eq!(
        serde_json::to_string(&Frame::Ping).unwrap(),
        r#"{"t":"ping"}"#
    );
    assert_eq!(
        serde_json::to_string(&Frame::Pong).unwrap(),
        r#"{"t":"pong"}"#
    );
    assert_eq!(
        serde_json::from_str::<Frame>(r#"{"t":"ping"}"#).unwrap(),
        Frame::Ping
    );
    assert_eq!(
        serde_json::from_str::<Frame>(r#"{"t":"pong"}"#).unwrap(),
        Frame::Pong
    );
}

#[test]
fn close_example_roundtrips() {
    let frame = roundtrip_frame(
        r#"{
  "t": "close",
  "code": "superseded",
  "message": "a newer connection for this machine was accepted"
}"#,
    );
    assert_eq!(
        frame,
        Frame::Close {
            code: CloseCode::Superseded,
            message: "a newer connection for this machine was accepted".to_owned()
        }
    );
}

#[test]
fn close_codes_match_spec_table() {
    // §3.8 table: (code, WS close code, runner reconnects?). `None` = runner → hub only.
    let table = [
        (CloseCode::Normal, "normal", 4000, Some(false)),
        (CloseCode::Superseded, "superseded", 4001, Some(false)),
        (CloseCode::AuthInvalid, "auth_invalid", 4002, Some(false)),
        (CloseCode::AuthRevoked, "auth_revoked", 4003, Some(false)),
        (
            CloseCode::VersionUnsupported,
            "version_unsupported",
            4004,
            Some(false),
        ),
        (CloseCode::ProtocolError, "protocol_error", 4005, Some(true)),
        (
            CloseCode::HeartbeatTimeout,
            "heartbeat_timeout",
            4006,
            Some(true),
        ),
        (CloseCode::ServerRestart, "server_restart", 4007, Some(true)),
        (CloseCode::GoingToSleep, "going_to_sleep", 4008, None),
    ];
    assert_eq!(CloseCode::ALL.len(), table.len());
    for (code, name, ws, reconnects) in table {
        assert_eq!(code.ws_code(), ws, "{name} ws code");
        assert_eq!(
            CloseCode::from_ws_code(ws),
            Some(code),
            "{name} from ws code"
        );
        assert_eq!(code.reconnects(), reconnects, "{name} reconnect policy");
        let json = serde_json::to_value(code).unwrap();
        assert_eq!(json, Value::String(name.to_owned()), "{name} wire name");
    }
    assert_eq!(CloseCode::from_ws_code(3999), None);
    assert_eq!(CloseCode::from_ws_code(4009), None);
}

#[test]
fn actor_round_trips_with_optional_fields_absent() {
    // turn-event.md §4.1 actor shape: `name` and `device_id` are optional.
    let actor: Actor = serde_json::from_str(r#"{ "kind": "agent", "id": "agt_1" }"#).unwrap();
    assert_eq!(actor.kind, ActorKind::Agent);
    assert_eq!(
        serde_json::to_value(&actor).unwrap(),
        serde_json::json!({ "kind": "agent", "id": "agt_1" })
    );
}

#[test]
fn unknown_frame_tag_fails_to_parse() {
    assert!(serde_json::from_str::<Frame>(r#"{ "t": "resize", "seq": 1 }"#).is_err());
}
