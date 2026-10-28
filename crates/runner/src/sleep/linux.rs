//! systemd-logind `PrepareForSleep` on the system D-Bus (`org.freedesktop.login1.Manager`).

use anyhow::{Context, Result};
use futures_util::StreamExt;
use zbus::{Connection, connection, proxy};

use super::SleepWatcher;

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Manager {
    /// Emitted with `start` true just before the system sleeps, and false after it resumes.
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

/// Waits for logind's `PrepareForSleep(true)` signals.
pub struct LogindSleepWatcher {
    // Keeps the bus connection, and so the signal subscription, alive.
    _connection: Connection,
    signals: PrepareForSleepStream,
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
            _connection: connection,
            signals,
        })
    }
}

impl SleepWatcher for LogindSleepWatcher {
    async fn pre_sleep(&mut self) -> Result<()> {
        loop {
            let signal = self
                .signals
                .next()
                .await
                .context("PrepareForSleep stream ended")?;
            let args = signal.args().context("PrepareForSleep arguments")?;
            if *args.start() {
                return Ok(());
            }
            // The resume notification (`start` false) is not a pre-sleep event.
        }
    }
}
