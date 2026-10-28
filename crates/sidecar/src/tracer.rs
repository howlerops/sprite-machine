//! The tracer bullet's runner loop, as a thin adapter over the real [`crate::bridge`].
//!
//! The tracer wires the bridge to plain channels: `Command`s in, `Envelope`s out, no tunnel
//! framing. This adapter gives each command a synthetic seq and idempotency key, keeps the
//! store in a throwaway directory. `UserMessage` is the hub's (client-api.md §6), so the
//! bridge does not emit it for these commands.
//!
//! Failures the bridge reports as `command_result` are logged, since the tracer has no
//! up-stream frame for them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use protocol::tunnel::Command;
use protocol::turn_event::Envelope;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::bridge::{
    Bridge, CommandFailure, CommandSource, Config, EventSink, IncomingCommand, SinkError, UlidIds,
};
use crate::store::Store;

/// Default approval TTL (plan §4.1): 10 minutes.
pub const APPROVAL_TTL_MS: u64 = 10 * 60 * 1000;

/// How the bridge identifies itself in `RunStarted`.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub machine_id: String,
    pub agent_id: String,
    /// Absolute working directory for `session/new`.
    pub cwd: PathBuf,
}

/// Runs the bridge until `commands` closes (the tunnel client shut down). Envelopes go to
/// `events` in production order.
pub async fn run<R, W>(
    config: BridgeConfig,
    acp_reader: R,
    acp_writer: W,
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Envelope>,
) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let state_dir = scratch_dir();
    let store = Arc::new(Store::open(&state_dir).context("open the bridge store")?);
    let mut bridge_config = Config::new(config.machine_id, config.agent_id, config.cwd);
    bridge_config.approval_ttl = std::time::Duration::from_millis(APPROVAL_TTL_MS);
    let bridge = Bridge::connect(
        bridge_config,
        acp_reader,
        acp_writer,
        store,
        Arc::new(UlidIds::new()),
        ChannelSink { events },
        ChannelCommands { commands, next: 0 },
    );
    let result = bridge.run().await.context("tracer bridge");
    remove_scratch(&state_dir);
    result
}

/// Milliseconds since the Unix epoch (0 if the clock is before 1970).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn scratch_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "sm-tracer-bridge-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ))
}

fn remove_scratch(dir: &Path) {
    if let Err(error) = std::fs::remove_dir_all(dir) {
        tracing::warn!(%error, dir = %dir.display(), "could not remove the tracer bridge store");
    }
}

struct ChannelSink {
    events: mpsc::Sender<Envelope>,
}

impl EventSink for ChannelSink {
    async fn send(&self, envelopes: Vec<Envelope>) -> Result<(), SinkError> {
        for envelope in envelopes {
            self.events
                .send(envelope)
                .await
                .map_err(|_| SinkError("the tunnel client stopped taking events".to_owned()))?;
        }
        Ok(())
    }

    async fn command_result(&self, failure: CommandFailure) -> Result<(), SinkError> {
        tracing::warn!(?failure, "tracer command failed");
        Ok(())
    }
}

struct ChannelCommands {
    commands: mpsc::Receiver<Command>,
    next: u64,
}

impl CommandSource for ChannelCommands {
    async fn next(&mut self) -> Option<IncomingCommand> {
        let command = self.commands.recv().await?;
        self.next += 1;
        Some(IncomingCommand {
            seq: self.next,
            idempotency_key: format!("tracer-{}", self.next),
            command,
        })
    }
}
