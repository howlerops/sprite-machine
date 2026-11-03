//! Dispatch (M2B-013, plan §4.10): checks on `POST /v1/threads` before a task is queued for a
//! chosen machine.
//!
//! The create path itself already existed (M2-008 / M2B-002: Thread DO create, the machine as
//! host, the tunnel `dispatch` command queued on the Machine DO, delivered after `welcome`, and a
//! runner `command_result{capacity}` failing the task). This module adds what was missing: the
//! `workspace.spawn` mode is validated (`same_dir` or `worktree`), and refused when the machine
//! said in its `hello` that it does not support it.
//!
//! Capacity is the runner's call (tunnel.md §3.6): it refuses a dispatch beyond its
//! `max_sessions` with `command_result{ok:false, code:"capacity"}`, and the hub then ends the
//! task with `RunError{code:"capacity"}` (the thread shows it failed). The hub does not count
//! sessions itself.

use serde_json::Value;

use crate::db::{Db, opt_text};
use crate::threads::ApiError;

/// The spawn modes of `dispatch` (tunnel.md §3.5): the folder itself, or a new git worktree.
pub const SPAWN_MODES: [&str; 2] = ["same_dir", "worktree"];

/// Checks `workspace.spawn` of a create body against what the machine advertised.
///
/// `capabilities` is the machine's last `hello` capabilities as stored (JSON text), or `None`
/// before its first hello. A missing `spawn` is fine (the runner's default applies). An unknown
/// mode is `400 invalid_request`; a known mode the machine does not list in `spawn` is too.
pub fn check_spawn(body: &Value, capabilities: Option<&str>) -> Result<(), ApiError> {
    let Some(spawn) = body.pointer("/workspace/spawn") else {
        return Ok(());
    };
    if spawn.is_null() {
        return Ok(());
    }
    let Some(spawn) = spawn.as_str().filter(|mode| SPAWN_MODES.contains(mode)) else {
        return Err(ApiError::invalid_request(
            "workspace.spawn must be \"same_dir\" or \"worktree\"",
        ));
    };
    let advertised = capabilities
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|caps| caps.get("spawn").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    if !advertised.is_empty() && !advertised.iter().any(|mode| mode.as_str() == Some(spawn)) {
        return Err(ApiError::invalid_request(format!(
            "the machine does not support spawn mode {spawn}"
        )));
    }
    Ok(())
}

/// The stored `hello` capabilities of a machine of `org_id` (`None` before its first hello).
pub async fn machine_capabilities(
    db: &impl Db,
    machine_id: &str,
    org_id: &str,
) -> Result<Option<String>, ApiError> {
    let rows = db
        .query(
            "SELECT capabilities FROM machines WHERE id = ? AND org_id = ?",
            &[machine_id.into(), org_id.into()],
        )
        .await
        .map_err(|error| ApiError::internal(format!("dispatch: {error}")))?;
    match rows.first() {
        Some(row) => opt_text(row, "capabilities")
            .map_err(|error| ApiError::internal(format!("dispatch: {error}"))),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const BOTH: &str = r#"{"spawn":["same_dir","worktree"],"max_sessions":4}"#;
    const SAME_ONLY: &str = r#"{"spawn":["same_dir"]}"#;

    fn body(spawn: Value) -> Value {
        json!({ "machine_id": "mch_1", "workspace": { "path": "/w", "spawn": spawn } })
    }

    #[test]
    fn both_modes_pass_when_the_machine_lists_them() {
        for mode in SPAWN_MODES {
            check_spawn(&body(json!(mode)), Some(BOTH)).expect(mode);
        }
    }

    #[test]
    fn a_missing_or_null_spawn_passes() {
        check_spawn(&json!({ "machine_id": "m" }), Some(SAME_ONLY)).expect("no workspace");
        check_spawn(&body(Value::Null), Some(SAME_ONLY)).expect("null spawn");
    }

    #[test]
    fn an_unknown_mode_is_invalid_even_before_the_first_hello() {
        for bad in [json!("clone"), json!(3), json!("")] {
            let error = check_spawn(&body(bad), None).expect_err("unknown mode");
            assert_eq!((error.status, error.code), (400, "invalid_request"));
        }
    }

    #[test]
    fn a_mode_the_machine_did_not_advertise_is_refused() {
        let error = check_spawn(&body(json!("worktree")), Some(SAME_ONLY)).expect_err("refused");
        assert_eq!(error.status, 400);
        assert!(error.message.contains("worktree"), "{}", error.message);
    }

    #[test]
    fn without_an_advertised_list_any_known_mode_passes() {
        check_spawn(&body(json!("worktree")), None).expect("before hello");
        check_spawn(&body(json!("worktree")), Some(r#"{"spawn":[]}"#)).expect("empty list");
    }
}
