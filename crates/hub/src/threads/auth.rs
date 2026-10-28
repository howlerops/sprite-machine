//! The auth placeholder of client-api.md §2, until M2-003a lands: only the literal bearer token
//! `dev`, and only when the dev var `DEV_FIXTURES=1` is set. The actor comes from the token,
//! never from a request body.

use protocol::tunnel::{Actor, ActorKind};

use super::error::ApiError;

/// The actor behind the dev token: `{kind:"user", id:"usr_dev", device_id:"dev_dev"}`.
pub fn dev_actor() -> Actor {
    Actor {
        kind: ActorKind::User,
        id: "usr_dev".to_owned(),
        name: None,
        device_id: Some("dev_dev".to_owned()),
    }
}

/// `true` when the `DEV_FIXTURES` var value turns on the dev fixtures.
pub fn dev_fixtures_enabled(var: Option<&str>) -> bool {
    var.is_some_and(|value| value.trim() == "1")
}

/// Derives the caller from an `Authorization` header value. Any missing or unknown token is
/// `401 unauthorized`.
pub fn authenticate(authorization: Option<&str>, dev_fixtures: bool) -> Result<Actor, ApiError> {
    let token = authorization
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    match token {
        Some("dev") if dev_fixtures => Ok(dev_actor()),
        _ => Err(ApiError::unauthorized("missing or invalid access token")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_dev_token_with_dev_fixtures_is_accepted() {
        assert_eq!(authenticate(Some("Bearer dev"), true), Ok(dev_actor()));
        for (header, fixtures) in [
            (Some("Bearer dev"), false),
            (Some("Bearer other"), true),
            (Some("dev"), true),
            (Some("Basic dev"), true),
            (None, true),
        ] {
            let error = authenticate(header, fixtures).expect_err("rejected");
            assert_eq!(error.status, 401);
            assert_eq!(error.code, "unauthorized");
        }
    }

    #[test]
    fn dev_fixtures_var_must_be_one() {
        assert!(dev_fixtures_enabled(Some("1")));
        assert!(!dev_fixtures_enabled(Some("0")));
        assert!(!dev_fixtures_enabled(None));
    }
}
