//! Per-thread state the mapper tracks so it can emit `StateSnapshot` before the first
//! `StateDelta` of a scope (§4.6, §5 rule 8) and diff later updates into RFC 6902 patches.

use serde_json::{Map, Value, json};

use crate::turn_event::{PatchOp, PatchOpKind, StateScope};

/// What to emit for a state change.
#[derive(Debug, Clone, PartialEq)]
pub enum StateChange {
    /// First update of the scope (or an explicit reset): the full state.
    Snapshot(Map<String, Value>),
    /// A later update: `replace` operations against the last state.
    Delta(Vec<PatchOp>),
    /// Nothing changed.
    Unchanged,
}

#[derive(Debug, Clone, Default)]
pub struct StateTracker {
    session: Option<Map<String, Value>>,
    usage: Option<Map<String, Value>>,
    thread: Option<Map<String, Value>>,
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// The `session` state before anything is known (§4.6 shape).
fn session_seed() -> Map<String, Value> {
    object(json!({
        "available_commands": [],
        "current_mode_id": null,
        "modes": [],
        "config_options": []
    }))
}

/// The `thread` state before anything is known (§4.6 shape).
fn thread_seed() -> Map<String, Value> {
    object(json!({"title": null, "pending_tasks": []}))
}

impl StateTracker {
    fn slot(&mut self, scope: StateScope) -> &mut Option<Map<String, Value>> {
        match scope {
            StateScope::Session => &mut self.session,
            StateScope::Usage => &mut self.usage,
            // `flow` state belongs to the flow engine; the ACP mapper never touches it.
            StateScope::Thread | StateScope::Flow => &mut self.thread,
        }
    }

    /// Applies `changes` (top-level keys) to `scope`.
    pub fn apply(&mut self, scope: StateScope, changes: Vec<(&str, Value)>) -> StateChange {
        let seed = match scope {
            StateScope::Session => session_seed(),
            StateScope::Thread | StateScope::Flow => thread_seed(),
            StateScope::Usage => Map::new(),
        };
        let slot = self.slot(scope);
        match slot {
            None => {
                let mut state = seed;
                for (key, value) in changes {
                    state.insert(key.to_owned(), value);
                }
                *slot = Some(state.clone());
                StateChange::Snapshot(state)
            }
            Some(state) => {
                let mut patch = Vec::new();
                for (key, value) in changes {
                    if state.get(key) != Some(&value) {
                        state.insert(key.to_owned(), value.clone());
                        patch.push(PatchOp {
                            op: PatchOpKind::Replace,
                            path: format!("/{key}"),
                            from: None,
                            value: Some(value),
                        });
                    }
                }
                if patch.is_empty() {
                    StateChange::Unchanged
                } else {
                    StateChange::Delta(patch)
                }
            }
        }
    }

    /// Replaces the whole `session` state (`session/new`): always a snapshot.
    pub fn reset_session(&mut self, changes: Vec<(&str, Value)>) -> Map<String, Value> {
        let mut state = session_seed();
        for (key, value) in changes {
            state.insert(key.to_owned(), value);
        }
        self.session = Some(state.clone());
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_change_is_a_snapshot_then_deltas_only_for_what_changed() {
        let mut tracker = StateTracker::default();
        let first = tracker.apply(
            StateScope::Session,
            vec![("current_mode_id", json!("code"))],
        );
        let StateChange::Snapshot(state) = first else {
            panic!("expected a snapshot");
        };
        assert_eq!(state["current_mode_id"], json!("code"));
        assert_eq!(state["available_commands"], json!([]));

        assert_eq!(
            tracker.apply(
                StateScope::Session,
                vec![("current_mode_id", json!("code"))]
            ),
            StateChange::Unchanged
        );
        let StateChange::Delta(patch) = tracker.apply(
            StateScope::Session,
            vec![("current_mode_id", json!("plan"))],
        ) else {
            panic!("expected a delta");
        };
        assert_eq!(patch.len(), 1);
        assert_eq!(patch[0].path, "/current_mode_id");
    }
}
