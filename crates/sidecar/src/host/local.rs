//! The `local` host mode: the user's machine (PLAN-v2 §4.1, §4.10).

use std::sync::Arc;

use super::identity::{CREDENTIAL_SERVICE, MACHINE_CREDENTIAL_ACCOUNT};
use super::{CredentialStore, HostError, HostKind, HostMode};
use crate::checkpoint::Checkpointer;
use crate::checkpoint::ShadowGitCheckpointer;
use crate::config::{Config, MachineCredential};
use crate::keepalive::KeepAlive;
use crate::keepalive::NoKeepAlive;
use crate::preview::NoPreview;
use crate::preview::PreviewProvider;

/// `local` mode. Identity comes from a [`CredentialStore`] (the OS keychain in production).
/// Nothing keeps the machine awake, checkpoints are shadow-git commits, and there are no
/// previews.
pub struct LocalHost {
    hub_url: Option<String>,
    store: Arc<dyn CredentialStore>,
    keep_alive: NoKeepAlive,
    checkpointer: ShadowGitCheckpointer,
    preview: NoPreview,
}

impl LocalHost {
    /// Builds the local mode from `config`, with `store` as the identity source.
    pub fn new(config: &Config, store: Arc<dyn CredentialStore>) -> Self {
        Self {
            hub_url: config.hub_url.clone(),
            store,
            keep_alive: NoKeepAlive,
            checkpointer: ShadowGitCheckpointer,
            preview: NoPreview,
        }
    }
}

impl HostMode for LocalHost {
    fn kind(&self) -> HostKind {
        HostKind::Local
    }

    fn hub_url(&self) -> Option<&str> {
        self.hub_url.as_deref()
    }

    fn credential(&self) -> Result<Option<MachineCredential>, HostError> {
        Ok(self
            .store
            .load(CREDENTIAL_SERVICE, MACHINE_CREDENTIAL_ACCOUNT)?)
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
