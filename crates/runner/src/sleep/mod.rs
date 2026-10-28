//! The OS sleep hook (tunnel.md §3.7): before the machine sleeps, the runner tells the hub
//! `presence{asleep}` through [`SleepHint::asleep`].
//!
//! A [`SleepWatcher`] reports each pre-sleep notification from the OS. [`forward`] calls
//! [`SleepHint::asleep`] for each one. [`spawn`] starts the platform watcher; when it cannot be
//! started (no system bus, unsupported OS), it logs a warning and ends, and the runner keeps
//! running without the hint.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use std::future::Future;

use anyhow::Result;
use tokio::task::JoinHandle;

use crate::tunnel::SleepHint;

#[cfg(target_os = "linux")]
pub use linux::LogindSleepWatcher;
#[cfg(target_os = "macos")]
pub use macos::MacSleepWatcher;

/// A source of pre-sleep notifications.
pub trait SleepWatcher: Send + 'static {
    /// Resolves at each notification that the machine is about to sleep. Returns an error when
    /// the source is gone and no further notification can come.
    fn pre_sleep(&mut self) -> impl Future<Output = Result<()>> + Send;
}

/// Calls [`SleepHint::asleep`] for each pre-sleep notification until the watcher fails.
pub async fn forward<W: SleepWatcher>(mut watcher: W, hint: SleepHint) -> Result<()> {
    loop {
        watcher.pre_sleep().await?;
        if !hint.asleep().await {
            tracing::debug!("sleep hint not written (no live connection, or the client stopped)");
        }
    }
}

/// Starts the platform watcher and forwards its notifications to `hint` on a task. If the
/// watcher cannot start or stops, the task logs a warning and ends.
pub fn spawn(hint: SleepHint) -> JoinHandle<()> {
    tokio::spawn(async move {
        match platform_watcher().await {
            Ok(watcher) => {
                if let Err(error) = forward(watcher, hint).await {
                    tracing::warn!("OS sleep hook stopped: {error:#}");
                }
            }
            Err(error) => tracing::warn!("OS sleep hook disabled: {error:#}"),
        }
    })
}

#[cfg(target_os = "linux")]
async fn platform_watcher() -> Result<LogindSleepWatcher> {
    LogindSleepWatcher::connect().await
}

#[cfg(target_os = "macos")]
async fn platform_watcher() -> Result<MacSleepWatcher> {
    MacSleepWatcher::connect().await
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn platform_watcher() -> Result<NoWatcher> {
    anyhow::bail!("no OS sleep notifications on this platform")
}

/// Stands in for a platform watcher where none exists.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub struct NoWatcher;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl SleepWatcher for NoWatcher {
    async fn pre_sleep(&mut self) -> Result<()> {
        anyhow::bail!("no OS sleep notifications on this platform")
    }
}
