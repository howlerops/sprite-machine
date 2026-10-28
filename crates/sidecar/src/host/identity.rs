//! Identity: where the machine credential comes from in each host mode (PLAN-v2 §4.1).
//!
//! - `local`: the trusted-device credential lives in the OS keychain, behind [`CredentialStore`].
//! - `sandbox`: `SM_MACHINE_CREDENTIAL`, read into memory by [`Config`](crate::config::Config),
//!   is removed from the environment of harness child processes by [`harness_env`].

use std::collections::HashMap;
use std::sync::Mutex;

use crate::config::MachineCredential;

/// Keychain service name for the runner's secrets.
pub const CREDENTIAL_SERVICE: &str = "sm";
/// Keychain account for the trusted-device machine credential.
pub const MACHINE_CREDENTIAL_ACCOUNT: &str = "machine-credential";
/// Env var that holds the sandbox machine credential. Never passed to harness processes.
pub const MACHINE_CREDENTIAL_ENV: &str = "SM_MACHINE_CREDENTIAL";

/// Credential store errors. Messages never contain a credential.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    /// The store cannot be reached at all (for example, no keychain service).
    #[error("credential store unavailable: {0}")]
    Unavailable(String),
    /// The store failed to read or write.
    #[error("credential store failed: {0}")]
    Store(String),
}

/// Keeps credentials, keyed by service and account. The OS keychain implements this in
/// production. [`MemoryCredentialStore`] implements it in memory.
pub trait CredentialStore: Send + Sync {
    /// Returns the credential for `service` and `account`, or `None` if none is stored.
    fn load(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<MachineCredential>, CredentialError>;

    /// Stores `credential` for `service` and `account`, replacing any previous value.
    fn save(
        &self,
        service: &str,
        account: &str,
        credential: MachineCredential,
    ) -> Result<(), CredentialError>;
}

/// An in-memory [`CredentialStore`]. Nothing is persisted.
#[derive(Debug, Default)]
pub struct MemoryCredentialStore {
    entries: Mutex<HashMap<(String, String), MachineCredential>>,
}

impl MemoryCredentialStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn load(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<MachineCredential>, CredentialError> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| CredentialError::Store("lock poisoned".to_owned()))?;
        Ok(entries
            .get(&(service.to_owned(), account.to_owned()))
            .cloned())
    }

    fn save(
        &self,
        service: &str,
        account: &str,
        credential: MachineCredential,
    ) -> Result<(), CredentialError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| CredentialError::Store("lock poisoned".to_owned()))?;
        entries.insert((service.to_owned(), account.to_owned()), credential);
        Ok(())
    }
}

/// Returns `env` without `SM_MACHINE_CREDENTIAL`. Harness child processes get this list, so
/// the machine credential never reaches the agent or its tools. Every other entry is kept in
/// order.
pub fn harness_env<I>(env: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    env.into_iter()
        .filter(|(key, _)| key != MACHINE_CREDENTIAL_ENV)
        .collect()
}
