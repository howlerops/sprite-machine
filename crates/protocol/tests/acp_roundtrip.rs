//! Round-trip tests for ACP v1 payloads: each fixture is deserialized into the
//! schema type and re-serialized; the result must equal the original JSON value.

use protocol::acp::{CreateElicitationRequest, RequestPermissionRequest, SessionNotification};

fn assert_roundtrip<T>(fixture: &str)
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let original: serde_json::Value = serde_json::from_str(fixture).expect("fixture is JSON");
    let typed: T = serde_json::from_value(original.clone()).expect("fixture deserializes");
    let back = serde_json::to_value(&typed).expect("type serializes");
    assert_eq!(back, original);
}

#[test]
fn session_update_agent_message_chunk() {
    assert_roundtrip::<SessionNotification>(include_str!(
        "fixtures/acp/session_update_agent_message_chunk.json"
    ));
}

#[test]
fn session_update_agent_thought_chunk() {
    assert_roundtrip::<SessionNotification>(include_str!(
        "fixtures/acp/session_update_agent_thought_chunk.json"
    ));
}

#[test]
fn session_update_tool_call() {
    assert_roundtrip::<SessionNotification>(include_str!(
        "fixtures/acp/session_update_tool_call.json"
    ));
}

#[test]
fn session_update_tool_call_update() {
    assert_roundtrip::<SessionNotification>(include_str!(
        "fixtures/acp/session_update_tool_call_update.json"
    ));
}

#[test]
fn session_update_plan() {
    assert_roundtrip::<SessionNotification>(include_str!("fixtures/acp/session_update_plan.json"));
}

#[test]
fn session_update_usage_update() {
    assert_roundtrip::<SessionNotification>(include_str!(
        "fixtures/acp/session_update_usage_update.json"
    ));
}

#[test]
fn session_update_available_commands_update() {
    assert_roundtrip::<SessionNotification>(include_str!(
        "fixtures/acp/session_update_available_commands_update.json"
    ));
}

#[test]
fn session_update_current_mode_update() {
    assert_roundtrip::<SessionNotification>(include_str!(
        "fixtures/acp/session_update_current_mode_update.json"
    ));
}

#[test]
fn request_permission() {
    assert_roundtrip::<RequestPermissionRequest>(include_str!(
        "fixtures/acp/request_permission.json"
    ));
}

#[test]
fn elicitation_create() {
    assert_roundtrip::<CreateElicitationRequest>(include_str!(
        "fixtures/acp/elicitation_create.json"
    ));
}
