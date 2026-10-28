//! Keep-alive: what stops the host sleeping mid-turn (PLAN-v2 §4.1, §4.3).
//!
//! - `local`: [`NoKeepAlive`]. The machine's own power settings apply.
//! - `sandbox`: [`WorkspaceKeepAlive`]. The bridge holds a [`KeepAliveGuard`] while a turn runs
//!   or an approval is pending. It calls [`KeepAlive::tick`] every [`KeepAlive::interval`], and
//!   each tick pings the Workspace DO through an injected [`KeepAliveClient`]. Nothing is sent
//!   while no hold is active.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::host::BoxFuture;

/// Ping interval for the Workspace DO. Its idle timeout is 10 min (PLAN-v2 §4.3), so a minute
/// leaves room for slow pings.
pub const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(60);

/// Why the host must stay awake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// A turn is running.
    Turn,
    /// An approval is pending.
    Approval,
}

#[derive(Debug, Default)]
struct Holds {
    turns: AtomicUsize,
    approvals: AtomicUsize,
}

impl Holds {
    fn counter(&self, hold: Hold) -> &AtomicUsize {
        match hold {
            Hold::Turn => &self.turns,
            Hold::Approval => &self.approvals,
        }
    }

    fn active(&self) -> bool {
        self.turns.load(Ordering::Relaxed) > 0 || self.approvals.load(Ordering::Relaxed) > 0
    }
}

/// Holds the host awake until dropped. [`KeepAliveGuard::noop`] holds nothing.
#[must_use = "dropping the guard releases the hold"]
#[derive(Debug)]
pub struct KeepAliveGuard {
    held: Option<(Arc<Holds>, Hold)>,
}

impl KeepAliveGuard {
    /// A guard that holds nothing. Used by `local`.
    pub fn noop() -> Self {
        Self { held: None }
    }

    fn new(holds: Arc<Holds>, hold: Hold) -> Self {
        holds.counter(hold).fetch_add(1, Ordering::Relaxed);
        Self {
            held: Some((holds, hold)),
        }
    }
}

impl Drop for KeepAliveGuard {
    fn drop(&mut self) {
        if let Some((holds, hold)) = self.held.take() {
            holds.counter(hold).fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Keep-alive errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeepAliveError {
    /// The Workspace DO rejected or failed the ping.
    #[error("keep-alive ping failed: {0}")]
    Ping(String),
    /// The client is not wired yet.
    #[error("{op} is not implemented yet: {item} fills it")]
    NotImplemented {
        op: &'static str,
        item: &'static str,
    },
}

/// Keeps the host running while work is pending.
pub trait KeepAlive: Send + Sync {
    /// Starts a hold for `hold`. The hold lasts until the returned guard is dropped.
    fn hold(&self, hold: Hold) -> KeepAliveGuard;

    /// How often [`KeepAlive::tick`] should run, or `None` if no pings are needed.
    fn interval(&self) -> Option<Duration>;

    /// Pings if a hold is active. Resolves to `true` when a ping was sent.
    fn tick(&self) -> BoxFuture<'_, Result<bool, KeepAliveError>>;
}

/// The Workspace DO's keep-alive endpoint. Injected into [`WorkspaceKeepAlive`].
pub trait KeepAliveClient: Send + Sync {
    /// Asks the Workspace DO to keep this sandbox running.
    fn ping(&self) -> BoxFuture<'_, Result<(), KeepAliveError>>;
}

/// The `local` keep-alive. It holds nothing and never pings.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoKeepAlive;

impl KeepAlive for NoKeepAlive {
    fn hold(&self, _hold: Hold) -> KeepAliveGuard {
        KeepAliveGuard::noop()
    }

    fn interval(&self) -> Option<Duration> {
        None
    }

    fn tick(&self) -> BoxFuture<'_, Result<bool, KeepAliveError>> {
        Box::pin(std::future::ready(Ok(false)))
    }
}

/// The `sandbox` keep-alive: pings through a [`KeepAliveClient`] while a hold is active.
pub struct WorkspaceKeepAlive {
    client: Arc<dyn KeepAliveClient>,
    interval: Duration,
    holds: Arc<Holds>,
}

impl WorkspaceKeepAlive {
    /// Builds the keep-alive with `client` and a ping `interval`.
    pub fn new(client: Arc<dyn KeepAliveClient>, interval: Duration) -> Self {
        Self {
            client,
            interval,
            holds: Arc::new(Holds::default()),
        }
    }
}

impl KeepAlive for WorkspaceKeepAlive {
    fn hold(&self, hold: Hold) -> KeepAliveGuard {
        KeepAliveGuard::new(Arc::clone(&self.holds), hold)
    }

    fn interval(&self) -> Option<Duration> {
        Some(self.interval)
    }

    fn tick(&self) -> BoxFuture<'_, Result<bool, KeepAliveError>> {
        Box::pin(async move {
            if !self.holds.active() {
                return Ok(false);
            }
            self.client.ping().await?;
            Ok(true)
        })
    }
}

/// The keep-alive client before M1A-010 wires the Workspace DO. Every ping returns
/// `NotImplemented`.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnwiredKeepAliveClient;

impl KeepAliveClient for UnwiredKeepAliveClient {
    fn ping(&self) -> BoxFuture<'_, Result<(), KeepAliveError>> {
        Box::pin(std::future::ready(Err(KeepAliveError::NotImplemented {
            op: "workspace keep-alive ping",
            item: "M1A-010",
        })))
    }
}
