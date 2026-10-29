//! Test-only fixture routes for the mobile web page's browser tests (WEB-001, wasm32 only).
//!
//! | Route                                                  | Effect                                |
//! | ------------------------------------------------------ | ------------------------------------- |
//! | `POST /v1/dev/fixtures/machines/{machine_id}/presence` | `{state}`: set a machine's presence in D1 |
//! | `POST /v1/dev/fixtures/threads`                        | `{machine_id, title, status?, thread_id?, init?}`: upsert a thread index row; `init: true` also hosts it in the caller's org and runs the Thread DO init hook (WEB-002) |
//!
//! Both answer `404 not_found` unless the Worker var `DEV_FIXTURES` is `1`, and otherwise need an
//! admin session of the org they change (the org comes from the session, never from the body).
//! They do what a runner's tunnel and a Thread DO do for real; the page only ever reads D1.

use crate::threads::ApiError;

#[cfg(target_arch = "wasm32")]
pub mod routes;

/// `404` unless the `DEV_FIXTURES` var is on: with it off these routes do not exist.
pub fn gate(enabled: bool) -> Result<(), ApiError> {
    if enabled {
        Ok(())
    } else {
        Err(ApiError::not_found("no such route"))
    }
}

/// The stored presence for a requested one.
pub fn presence_state(name: &str) -> Option<&'static str> {
    match name {
        "online" => Some("online"),
        "asleep" => Some("asleep"),
        "offline" => Some("offline"),
        _ => None,
    }
}

/// The stored task status for a client status name (client-api.md §5.2).
pub fn stored_status(name: &str) -> Option<&'static str> {
    match name {
        "idle" => Some("open"),
        "running" => Some("running"),
        "needs_you" => Some("waiting"),
        "done" => Some("done"),
        "failed" => Some("failed"),
        "cancelled" => Some("cancelled"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::dev_fixtures_enabled;

    #[test]
    fn the_routes_do_not_exist_without_dev_fixtures() {
        let off = gate(false).expect_err("off");
        assert_eq!(off.status, 404);
        assert_eq!(off.code, "not_found");
        assert!(gate(true).is_ok());
        // The var is exactly "1": anything else is off.
        assert!(!dev_fixtures_enabled(None));
        assert!(!dev_fixtures_enabled(Some("true")));
        assert!(dev_fixtures_enabled(Some("1")));
    }

    #[test]
    fn presence_and_status_names_map_to_stored_values() {
        assert_eq!(presence_state("asleep"), Some("asleep"));
        assert_eq!(presence_state("away"), None);
        assert_eq!(stored_status("needs_you"), Some("waiting"));
        assert_eq!(stored_status("idle"), Some("open"));
        assert_eq!(stored_status("failed"), Some("failed"));
        assert_eq!(stored_status("cancelled"), Some("cancelled"));
        assert_eq!(stored_status("weird"), None);
    }
}
