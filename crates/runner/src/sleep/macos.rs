//! macOS sleep notifications.
//!
//! TODO: not implemented. The planned source is IOKit `IORegisterForSystemPower` (on a run loop
//! thread; `kIOMessageSystemWillSleep` must be acknowledged with `IOAllowPowerChange`) or the
//! `NSWorkspaceWillSleepNotification` observer. This file was written on a Linux host and has
//! not been compiled or run on macOS. Until it is implemented, `connect` fails and the runner
//! runs without the hint.

use anyhow::{Result, bail};

use super::SleepWatcher;

/// Placeholder for the IOKit / NSWorkspace watcher.
pub struct MacSleepWatcher(());

impl MacSleepWatcher {
    pub async fn connect() -> Result<Self> {
        bail!("macOS sleep notifications are not implemented yet (TODO in sleep/macos.rs)")
    }
}

impl SleepWatcher for MacSleepWatcher {
    async fn pre_sleep(&mut self) -> Result<()> {
        bail!("macOS sleep notifications are not implemented yet (TODO in sleep/macos.rs)")
    }
}
