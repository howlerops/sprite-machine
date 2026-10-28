//! AG-UI / TurnEvent fixture server for client development.
//!
//! A synthetic thread that speaks the subscription protocol of `docs/specs/turn-event.md` §6:
//! `GET /threads/{thread_id}/subscribe?after=<seq>` (WebSocket) and
//! `POST /threads/{thread_id}/interrupts/{interrupt_id}` (client-api.md §5.4).
//!
//! Each [`Scenario`] is a pre-generated script of TurnEvent envelopes built from the
//! `protocol::turn_event` serde types. The server appends the script to one thread log at
//! `rate` events per second, assigns `seq` with [`protocol::turn_event::ThreadSeq`], and
//! replays the log to subscribers with the catch-up algorithm of §6.

mod patch;
mod scenarios;
mod script;
mod server;

pub use script::{Scenario, Script, Step, complete_log, default_actor};
pub use server::{DEFAULT_THREAD_ID, FixtureConfig, RunningFixture, start};

/// Crate name, used by the skeleton smoke test.
pub const CRATE: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE, "agui-fixture");
    }
}
