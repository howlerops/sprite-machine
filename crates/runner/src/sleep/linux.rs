//! systemd-logind on the system D-Bus (`org.freedesktop.login1.Manager`): `PrepareForSleep`
//! for the events, and `Inhibit("sleep", ..., "delay")` for the delay. logind holds the sleep
//! for up to `InhibitDelayMaxSec` (5 s by default) while the inhibitor fd is open.

use std::os::fd::OwnedFd;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use zbus::{Connection, connection, proxy};

use super::{PowerEvent, SleepWatcher};

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Manager {
    /// Emitted with `start` true just before the system sleeps, and false after it resumes.
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;

    /// Returns an fd. The lock holds while the fd is open; closing it lets the sleep go on.
    fn inhibit(
        &self,
        what: &str,
        who: &str,
        why: &str,
        mode: &str,
    ) -> zbus::Result<zbus::zvariant::OwnedFd>;
}

/// Waits for logind's `PrepareForSleep` signals and holds a `delay` inhibitor between them.
pub struct LogindSleepWatcher {
    // Keeps the bus connection, and so the signal subscription and the inhibitor, alive.
    connection: Connection,
    signals: PrepareForSleepStream,
    // The inhibitor fd. `Some` while the delay is held.
    delay: Option<OwnedFd>,
}

impl LogindSleepWatcher {
    /// Subscribes on the system bus. Fails when there is no system bus or no logind.
    pub async fn connect() -> Result<Self> {
        let connection = Connection::system()
            .await
            .context("cannot reach the system D-Bus")?;
        Self::subscribe(connection).await
    }

    /// As [`LogindSleepWatcher::connect`], on the bus at `address` (a D-Bus address string).
    pub async fn connect_to(address: &str) -> Result<Self> {
        let connection = connection::Builder::address(address)
            .context("invalid D-Bus address")?
            .build()
            .await
            .context("cannot reach the D-Bus")?;
        Self::subscribe(connection).await
    }

    async fn subscribe(connection: Connection) -> Result<Self> {
        let manager = ManagerProxy::new(&connection)
            .await
            .context("logind Manager proxy")?;
        let signals = manager
            .receive_prepare_for_sleep()
            .await
            .context("subscribe to PrepareForSleep")?;
        Ok(Self {
            connection,
            signals,
            delay: None,
        })
    }
}

impl SleepWatcher for LogindSleepWatcher {
    async fn next_event(&mut self) -> Result<PowerEvent> {
        let signal = self
            .signals
            .next()
            .await
            .context("PrepareForSleep stream ended")?;
        let args = signal.args().context("PrepareForSleep arguments")?;
        Ok(if *args.start() {
            PowerEvent::WillSleep
        } else {
            PowerEvent::Resumed
        })
    }

    async fn take_delay(&mut self) -> Result<()> {
        let manager = ManagerProxy::new(&self.connection)
            .await
            .context("logind Manager proxy")?;
        let fd = manager
            .inhibit(
                "sleep",
                "sm",
                "runner tells the hub presence asleep before sleep",
                "delay",
            )
            .await
            .context("logind Inhibit(sleep, delay)")?;
        // Replaces (and so closes) any inhibitor still held.
        self.delay = Some(fd.into());
        Ok(())
    }

    fn release_delay(&mut self) -> Result<()> {
        // Closing the fd is the release.
        self.delay = None;
        Ok(())
    }
}
