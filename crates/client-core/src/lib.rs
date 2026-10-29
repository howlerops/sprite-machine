//! Client core: TurnEvent subscription, view models, cache, outbound queue.
//!
//! No UI toolkit lives here (plan §4.4): the desktop views and the mobile page render the
//! models this crate builds.
//!
//! - [`sync`]: the thread subscription (`hello` / `snapshot` / `event` / `live`), dedupe by
//!   `seq`, gap detection, and resume with `?after=` (never a re-POST).
//! - [`view`]: pure view models: the inbox, a task transcript, machines and presence.
//! - [`patch`]: RFC 6902 JSON Patch for state deltas.
//! - [`cache`]: SQLite (bundled): the stored log per thread and the persisted outbox.
//! - [`outbox`]: the outbound queue with idempotency keys, backoff with jitter, and 409
//!   conflicts surfaced with the resolver.
//! - [`connectivity`]: online / connecting / offline, and [`Connection`], the owner of the
//!   subscription and the outbox that publishes the one banner state.
//! - [`tracer`]: the MVP-000 headless client, as an adapter over the above.

pub mod cache;
pub mod connectivity;
pub mod outbox;
pub mod patch;
pub mod sync;
pub mod threads;
pub mod tracer;
pub mod view;

pub use cache::{Cache, CacheError};
pub use connectivity::{Connection, Connectivity, banner_state};
pub use outbox::{
    Clock, FlushReport, HttpTransport, Intent, IntentState, ManualClock, Method, Outbox,
    OutboxError, PostRequest, PostResponse, QueuedIntent, Random, RetryPolicy, SessionConfig,
    StartedRemoval, SystemClock, SystemRandom, Transport, TransportError,
};
pub use sync::{Applied, Subscription, SubscriptionError, SyncError, ThreadSync};
pub use threads::{CreateThread, CreateThreadError, FirstPrompt, ThreadCreated, create_thread};
pub use view::inbox::InboxStatus;
pub use view::machines::MachineRow;
pub use view::{TaskStatus, ThreadView};

/// Crate name, used by the skeleton smoke test.
pub const CRATE: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE, "client-core");
    }
}
