//! The OS sleep hook (tunnel.md §3.7): before the machine sleeps, the runner tells the hub
//! `presence{asleep}` through [`SleepHint::asleep`].
//!
//! A [`SleepWatcher`] reports power events from the OS and holds a delay on sleep: the OS waits
//! for the delay to be released before it sleeps. [`forward`] writes the hint for each
//! `WillSleep` and only then releases the delay, so the hint is on the socket before the machine
//! goes to sleep. [`spawn`] starts the platform watcher; when it cannot be started (no system
//! bus, unsupported OS), it logs a warning and ends, and the runner keeps running without the
//! hint.

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

/// A power event the runner acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerEvent {
    /// The machine is about to sleep and waits for the delay to be released.
    WillSleep,
    /// The machine resumed. The delay is taken again.
    Resumed,
}

/// A source of power events that can hold a delay on sleep.
///
/// The forwarder calls [`SleepWatcher::take_delay`] at startup and after each
/// [`PowerEvent::Resumed`], and [`SleepWatcher::release_delay`] after the hint for each
/// [`PowerEvent::WillSleep`]. Linux holds a logind `Inhibit("sleep", "delay")` fd. macOS holds
/// nothing: it acknowledges the pending `WillSleep` with `IOAllowPowerChange`.
pub trait SleepWatcher: Send + 'static {
    /// Resolves at each power event. Returns an error when the source is gone.
    fn next_event(&mut self) -> impl Future<Output = Result<PowerEvent>> + Send;
    /// Takes the delay on sleep. An error leaves the machine free to sleep without the hint.
    fn take_delay(&mut self) -> impl Future<Output = Result<()>> + Send;
    /// Releases the delay so the pending sleep can proceed.
    fn release_delay(&mut self) -> Result<()>;
}

/// Writes the sleep hint. [`SleepHint`] is the real one; tests substitute a recorder.
pub trait Hint: Send + 'static {
    /// `true` once the hint was written to the socket.
    fn send_asleep(&self) -> impl Future<Output = bool> + Send;
}

impl Hint for SleepHint {
    async fn send_asleep(&self) -> bool {
        self.asleep().await
    }
}

/// What the macOS watcher does with one IOKit power message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerMessage {
    /// `kIOMessageCanSystemSleep`: the OS asks whether it may sleep. Nothing to hint.
    CanSystemSleep,
    /// `kIOMessageSystemWillSleep`: the machine is going to sleep.
    SystemWillSleep,
    /// Any other message (wake, power source changes, ...).
    Other,
}

/// The action for one [`PowerMessage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    /// Allow the power change at once. No hint.
    AllowNow,
    /// Pass `WillSleep` to the forwarder: write the hint, then allow the sleep.
    HintThenAllow,
    /// Not for the runner.
    Ignore,
}

/// Which action a power message gets. Platform-independent, so the decision is tested on every
/// host.
pub fn power_action(message: PowerMessage) -> PowerAction {
    match message {
        PowerMessage::CanSystemSleep => PowerAction::AllowNow,
        PowerMessage::SystemWillSleep => PowerAction::HintThenAllow,
        PowerMessage::Other => PowerAction::Ignore,
    }
}

/// Takes the delay, then for each power event: `WillSleep` writes the hint and releases the
/// delay; `Resumed` takes the delay again. Ends with the watcher's error when the source is gone.
/// A failed `take_delay` is logged and the loop goes on, so the hint is still written.
pub async fn forward<W: SleepWatcher, H: Hint>(mut watcher: W, hint: H) -> Result<()> {
    take_delay_logged(&mut watcher).await;
    loop {
        match watcher.next_event().await? {
            PowerEvent::WillSleep => {
                if !hint.send_asleep().await {
                    tracing::debug!(
                        "sleep hint not written (no live connection, or the client stopped)"
                    );
                }
                if let Err(error) = watcher.release_delay() {
                    tracing::warn!("could not release the sleep delay: {error:#}");
                }
            }
            PowerEvent::Resumed => take_delay_logged(&mut watcher).await,
        }
    }
}

async fn take_delay_logged<W: SleepWatcher>(watcher: &mut W) {
    if let Err(error) = watcher.take_delay().await {
        tracing::warn!("no sleep delay held, the hint may not arrive before sleep: {error:#}");
    }
}

/// Starts the platform watcher and forwards its events to `hint` on a task. If the watcher
/// cannot start or stops, the task logs a warning and ends.
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
    async fn next_event(&mut self) -> Result<PowerEvent> {
        anyhow::bail!("no OS sleep notifications on this platform")
    }

    async fn take_delay(&mut self) -> Result<()> {
        anyhow::bail!("no OS sleep notifications on this platform")
    }

    fn release_delay(&mut self) -> Result<()> {
        anyhow::bail!("no OS sleep notifications on this platform")
    }
}
