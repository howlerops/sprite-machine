//! The runner's tunnel client (tunnel.md): one outbound WebSocket to the hub's Machine DO with
//! reconnect, exponential backoff with full jitter, resume tokens and a durable outbox.
//!
//! - [`TunnelSink`] implements the bridge's `EventSink`: every envelope is written to the
//!   [`sidecar::store::Store`] outbox first (tunnel.md §4.2 step 1) and sent from there, so a
//!   crash or a dead socket loses nothing. Un-acked frames are re-sent after a reconnect and
//!   pruned on ack.
//! - [`TunnelCommands`] implements the bridge's `CommandSource`: down `command` frames pass the
//!   `Receiver` rules and the idempotency-key dedupe, reach the bridge, and are acked
//!   cumulatively once the bridge reports them processed.
//! - [`TunnelClient::run`] owns the connection: `hello` first, `welcome`, heartbeats, wake
//!   detection, close codes (which ones stop reconnecting) and the resume / fresh-stream rules.
//!
//! The protocol crate stays pure; connection timers and backoff live here.

mod backoff;
mod client;
mod io;
mod tls;

pub use backoff::{Backoff, Jitter, RandomJitter};
pub use client::{CredentialSource, Exit, SleepHint, StaticCredential, TunnelClient};
pub use io::{AnswerReply, TunnelCommands, TunnelSink};

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use protocol::tunnel::Capabilities;
use sidecar::store::Store;

/// Tunnel client settings.
#[derive(Debug, Clone)]
pub struct TunnelConfig {
    /// `ws://host:port` or `wss://host` (no path).
    pub hub_ws_base: String,
    pub machine_id: String,
    pub runner_version: String,
    /// Reported in `hello`.
    pub capabilities: Capabilities,
    pub timing: Timing,
}

/// Connection timers. The defaults are the spec's (tunnel.md §3.1, §3.8); tests shorten them.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// First backoff ceiling (500 ms).
    pub backoff_base: Duration,
    /// Backoff cap (30 s).
    pub backoff_cap: Duration,
    /// A connection that stayed up this long resets the backoff (60 s).
    pub stable_after: Duration,
    /// How long to wait for `welcome` after `hello` (10 s).
    pub welcome_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            backoff_base: Duration::from_millis(500),
            backoff_cap: Duration::from_secs(30),
            stable_after: Duration::from_secs(60),
            welcome_timeout: Duration::from_secs(10),
        }
    }
}

/// The wall clock, injectable so tests can simulate an OS sleep (the wall clock jumps while the
/// monotonic clock does not).
pub trait WallClock: Send + Sync + 'static {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

/// [`WallClock`] over `SystemTime`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemWallClock;

impl WallClock for SystemWallClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

/// Builds a client, its event sink and its command source over `store`.
///
/// The three share the store's outbox. Spawn [`TunnelClient::run`]; hand the sink and the
/// command source to the bridge.
pub fn tunnel<C: CredentialSource>(
    config: TunnelConfig,
    store: Arc<Store>,
    credentials: C,
) -> (TunnelClient<C>, TunnelSink, TunnelCommands) {
    client::build(config, store, credentials)
}
