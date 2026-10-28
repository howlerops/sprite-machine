//! [`KeychainCredentials`]: the tunnel's `CredentialSource` over the credential store.

use std::sync::Arc;

use anyhow::{Result, anyhow};

use super::store::CredentialStore;
use crate::tunnel::CredentialSource;

/// Reads the machine credential from the keychain (or its `0600` file) on every connect, so a
/// re-login is picked up on the next reconnect, and wipes it when the hub revokes the machine.
#[derive(Clone)]
pub struct KeychainCredentials {
    store: Arc<CredentialStore>,
}

impl KeychainCredentials {
    pub fn new(store: CredentialStore) -> Self {
        Self {
            store: Arc::new(store),
        }
    }

    /// The default store (`SM_CREDENTIAL_FILE`, else keychain, else file).
    pub fn from_env() -> Result<Self> {
        Ok(Self::new(CredentialStore::from_env()?))
    }
}

impl std::fmt::Debug for KeychainCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeychainCredentials(..)")
    }
}

impl CredentialSource for KeychainCredentials {
    async fn credential(&self) -> Result<String> {
        let store = Arc::clone(&self.store);
        // Keychain calls block (D-Bus, syscalls): keep them off the runtime threads.
        let login = tokio::task::spawn_blocking(move || store.load())
            .await
            .map_err(|error| anyhow!("credential read task failed: {error}"))??;
        login
            .map(|login| login.credential)
            .ok_or_else(|| anyhow!("not logged in: run `sm login --hub <url>`"))
    }

    fn wipe(&self) {
        if let Err(error) = self.store.wipe() {
            tracing::warn!("could not wipe the revoked credential: {error}");
        }
    }
}
