//! The `sandbox` host mode: a Cloudflare Sandbox started by the Workspace DO (PLAN-v2 §4.1, §4.3).

use std::sync::Arc;

use super::{HostError, HostKind, HostMode};
use crate::checkpoint::{Checkpointer, DirectoryBackupCheckpointer};
use crate::config::{Config, MachineCredential};
use crate::keepalive::{
    DEFAULT_KEEPALIVE_INTERVAL, KeepAlive, KeepAliveClient, UnwiredKeepAliveClient,
    WorkspaceKeepAlive,
};
use crate::preview::{PreviewClient, PreviewProvider, UnwiredPreviewClient, WorkspacePreview};

/// `sandbox` mode. The machine credential comes from `SM_MACHINE_CREDENTIAL`, already read
/// into memory by [`Config`]. The Workspace DO keeps the sandbox awake, backs it up, and
/// exposes its ports.
pub struct SandboxHost {
    hub_url: String,
    credential: MachineCredential,
    keep_alive: WorkspaceKeepAlive,
    checkpointer: DirectoryBackupCheckpointer,
    preview: WorkspacePreview,
}

impl SandboxHost {
    /// Builds the sandbox mode with the Workspace DO clients not wired yet. Keep-alive pings
    /// and previews return `NotImplemented` until M1A-010 and M1A-011 supply real clients.
    pub fn new(config: &Config) -> Result<Self, HostError> {
        Self::with_clients(
            config,
            Arc::new(UnwiredKeepAliveClient),
            Arc::new(UnwiredPreviewClient),
        )
    }

    /// Builds the sandbox mode with injected Workspace DO clients.
    ///
    /// Fails with [`HostError::MissingHubUrl`] or [`HostError::MissingCredential`] when the
    /// config lacks either one.
    pub fn with_clients(
        config: &Config,
        keep_alive_client: Arc<dyn KeepAliveClient>,
        preview_client: Arc<dyn PreviewClient>,
    ) -> Result<Self, HostError> {
        let hub_url = config.hub_url.clone().ok_or(HostError::MissingHubUrl)?;
        let credential = config
            .machine_credential
            .clone()
            .ok_or(HostError::MissingCredential)?;
        Ok(Self {
            hub_url,
            credential,
            keep_alive: WorkspaceKeepAlive::new(keep_alive_client, DEFAULT_KEEPALIVE_INTERVAL),
            checkpointer: DirectoryBackupCheckpointer,
            preview: WorkspacePreview::new(preview_client),
        })
    }
}

impl HostMode for SandboxHost {
    fn kind(&self) -> HostKind {
        HostKind::Sandbox
    }

    fn hub_url(&self) -> Option<&str> {
        Some(self.hub_url.as_str())
    }

    fn credential(&self) -> Result<Option<MachineCredential>, HostError> {
        Ok(Some(self.credential.clone()))
    }

    fn keep_alive(&self) -> &dyn KeepAlive {
        &self.keep_alive
    }

    fn checkpointer(&self) -> &dyn Checkpointer {
        &self.checkpointer
    }

    fn preview(&self) -> &dyn PreviewProvider {
        &self.preview
    }
}
