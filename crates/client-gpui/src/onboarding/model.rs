//! What onboarding shows and when. No GPUI here.

/// Whether the hub has an API to store a model key for cloud workspaces (M2-002 P4).
pub const MODEL_KEY_API_AVAILABLE: bool = false;
/// Whether the hub has an API to register a bring-your-own A2A URL (M2-004 P5).
pub const BYO_URL_API_AVAILABLE: bool = false;

/// The command that starts remote control; the page shows exactly this text.
pub const RC_COMMAND: &str = "sm rc";

/// The command that signs a computer in to this hub (`sm rc` needs it once).
pub fn login_command(hub: &str) -> String {
    format!("sm login --hub {}", hub.trim_end_matches('/'))
}

/// The three ways to a first agent, in the order the page lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathId {
    ModelKey,
    ByoUrl,
    RunRc,
}

/// One way, with whether it can be used now and, if not, the Fluent id of the reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathInfo {
    pub id: PathId,
    pub disabled_reason: Option<&'static str>,
}

impl PathInfo {
    pub fn enabled(&self) -> bool {
        self.disabled_reason.is_none()
    }
}

pub fn paths() -> [PathInfo; 3] {
    [
        PathInfo {
            id: PathId::ModelKey,
            disabled_reason: (!MODEL_KEY_API_AVAILABLE).then_some("onboarding-model-key-disabled"),
        },
        PathInfo {
            id: PathId::ByoUrl,
            disabled_reason: (!BYO_URL_API_AVAILABLE).then_some("onboarding-byo-disabled"),
        },
        PathInfo {
            id: PathId::RunRc,
            disabled_reason: None,
        },
    ]
}

/// What the shell knows when it decides whether to show onboarding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeedState {
    /// The machines feed has answered for this session.
    pub loaded: bool,
    /// The last machines fetch failed (an unreachable hub is not "no machines").
    pub failed: bool,
    pub machines: usize,
    /// The inbox has loaded a list and it is empty.
    pub inbox_empty: bool,
    /// A machine has been seen in this session: onboarding is done for good.
    pub completed: bool,
    /// The user chose Skip.
    pub skipped: bool,
    /// The user asked for the steps (the empty agents table or composer).
    pub requested: bool,
}

/// Whether the onboarding page replaces the inbox: a signed-in session whose machines feed is
/// loaded and empty, with nothing in the inbox to look at, unless the user skipped or a machine
/// was already seen. A request for the steps shows them while there is no machine.
pub fn needed(state: FeedState) -> bool {
    if state.completed || state.machines > 0 || state.failed || !state.loaded {
        return false;
    }
    state.requested || (state.inbox_empty && !state.skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> FeedState {
        FeedState {
            loaded: true,
            inbox_empty: true,
            ..FeedState::default()
        }
    }

    #[test]
    fn needed_only_for_a_loaded_empty_feed() {
        assert!(needed(fresh()));
        assert!(!needed(FeedState {
            loaded: false,
            ..fresh()
        }));
        assert!(!needed(FeedState {
            machines: 1,
            ..fresh()
        }));
        assert!(!needed(FeedState {
            failed: true,
            ..fresh()
        }));
        assert!(!needed(FeedState {
            inbox_empty: false,
            ..fresh()
        }));
    }

    #[test]
    fn skip_and_completion_end_it_but_a_request_brings_the_steps_back() {
        let skipped = FeedState {
            skipped: true,
            ..fresh()
        };
        assert!(!needed(skipped));
        assert!(needed(FeedState {
            requested: true,
            ..skipped
        }));
        assert!(!needed(FeedState {
            completed: true,
            requested: true,
            ..fresh()
        }));
    }

    #[test]
    fn the_commands_are_exact() {
        assert_eq!(RC_COMMAND, "sm rc");
        assert_eq!(
            login_command("https://hub.example/"),
            "sm login --hub https://hub.example"
        );
    }

    #[test]
    fn only_the_rc_path_is_enabled_and_the_others_say_why() {
        let [key, byo, rc] = paths();
        assert!(!key.enabled() && !byo.enabled() && rc.enabled());
        assert_eq!(key.disabled_reason, Some("onboarding-model-key-disabled"));
        assert_eq!(byo.disabled_reason, Some("onboarding-byo-disabled"));
    }
}
