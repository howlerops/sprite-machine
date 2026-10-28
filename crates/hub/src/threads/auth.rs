//! Auth for the thread routes moved to `crate::auth` (M2-003a): real session tokens, roles and
//! audit. What stays here are the dev-token helpers the Thread DO and the tests use.

use protocol::tunnel::{Actor, ActorKind};

pub use crate::auth::dev_fixtures_enabled;

/// The actor behind `Bearer dev` (`DEV_FIXTURES=1` only): `{kind:"user", id:"usr_dev",
/// device_id:"dev_dev"}`, an owner of the dev org.
pub fn dev_actor() -> Actor {
    Actor {
        kind: ActorKind::User,
        id: crate::auth::DEV_USER_ID.to_owned(),
        name: None,
        device_id: Some(crate::auth::DEV_DEVICE_ID.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_fixtures_var_must_be_one() {
        assert!(dev_fixtures_enabled(Some("1")));
        assert!(dev_fixtures_enabled(Some(" 1 ")));
        assert!(!dev_fixtures_enabled(Some("0")));
        assert!(!dev_fixtures_enabled(None));
    }

    #[test]
    fn dev_actor_matches_the_spec() {
        let actor = dev_actor();
        assert_eq!(actor.id, "usr_dev");
        assert_eq!(actor.device_id.as_deref(), Some("dev_dev"));
        assert_eq!(actor.name, None);
    }
}
