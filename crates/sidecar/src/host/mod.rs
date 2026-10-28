//! Host modes: what differs between `local` and `sandbox` (PLAN-v2 §4.1).
//!
//! [`HostMode`] swaps only the concerns that differ: keep-alive ([`crate::keepalive`]),
//! checkpoints ([`crate::checkpoint`]), identity ([`identity`]) and preview ([`crate::preview`]).
//! Transport is shared: both modes return the same [`TunnelConfig`] (hub URL + credential).
//!
//! [`from_config`] picks the mode from `SM_HOST`. Unset or `local` selects [`HostKind::Local`],
//! the default. `sandbox` selects [`HostKind::Sandbox`].

pub mod identity;
mod local;
mod sandbox;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::checkpoint::Checkpointer;
use crate::config::{Config, MachineCredential};
use crate::keepalive::KeepAlive;
use crate::preview::PreviewProvider;

pub use self::identity::{
    CredentialError, CredentialStore, MACHINE_CREDENTIAL_ENV, MemoryCredentialStore, harness_env,
};
pub use self::local::LocalHost;
pub use self::sandbox::SandboxHost;

/// Env var that selects the host mode.
pub const HOST_ENV: &str = "SM_HOST";

/// A boxed, sendable future. Used by the object-safe traits in this crate.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The host modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostKind {
    /// The user's machine: outbound tunnel, keychain identity, shadow-git checkpoints.
    #[default]
    Local,
    /// A Cloudflare Sandbox started by the Workspace DO.
    Sandbox,
}

impl HostKind {
    /// Parses an `SM_HOST` value. `None` and the empty string select [`HostKind::Local`].
    pub fn from_value(value: Option<&str>) -> Result<Self, HostError> {
        match value {
            None | Some("") | Some("local") => Ok(Self::Local),
            Some("sandbox") => Ok(Self::Sandbox),
            Some(other) => Err(HostError::UnknownKind {
                value: other.to_owned(),
            }),
        }
    }

    /// Reads `SM_HOST` from the process environment.
    pub fn from_env() -> Result<Self, HostError> {
        match std::env::var_os(HOST_ENV) {
            None => Self::from_value(None),
            Some(raw) => match raw.into_string() {
                Ok(value) => Self::from_value(Some(&value)),
                Err(raw) => Err(HostError::UnknownKind {
                    value: raw.to_string_lossy().into_owned(),
                }),
            },
        }
    }
}

/// Host configuration errors. Messages never contain the machine credential.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    /// `SM_HOST` names no known mode.
    #[error("unknown SM_HOST {value:?}: expected `local` or `sandbox`")]
    UnknownKind { value: String },
    /// Sandbox mode needs `SM_HUB_URL` to dial the hub.
    #[error("sandbox host needs SM_HUB_URL")]
    MissingHubUrl,
    /// Sandbox mode needs `SM_MACHINE_CREDENTIAL`, passed in by the Workspace DO.
    #[error("sandbox host needs SM_MACHINE_CREDENTIAL")]
    MissingCredential,
    /// The credential store could not return the local credential.
    #[error(transparent)]
    Credential(#[from] CredentialError),
}

/// The outbound tunnel's connection details. Both modes build it the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelConfig {
    /// Hub base URL, if configured.
    pub hub_url: Option<String>,
    /// Machine credential for the tunnel. Its `Debug` prints `<redacted>`.
    pub credential: Option<MachineCredential>,
}

/// One host mode: the concerns that differ between `local` and `sandbox`.
pub trait HostMode: Send + Sync {
    /// Which mode this is.
    fn kind(&self) -> HostKind;
    /// Hub base URL (`SM_HUB_URL`), if configured.
    fn hub_url(&self) -> Option<&str>;
    /// The machine credential for the tunnel, from this mode's identity source.
    fn credential(&self) -> Result<Option<MachineCredential>, HostError>;
    /// Keep-alive while a turn or approval is pending.
    fn keep_alive(&self) -> &dyn KeepAlive;
    /// Per-turn checkpoints and restore.
    fn checkpointer(&self) -> &dyn Checkpointer;
    /// Exposing workspace ports.
    fn preview(&self) -> &dyn PreviewProvider;

    /// The tunnel configuration. Shared by both modes, so it is not overridden.
    fn tunnel(&self) -> Result<TunnelConfig, HostError> {
        Ok(TunnelConfig {
            hub_url: self.hub_url().map(str::to_owned),
            credential: self.credential()?,
        })
    }

    /// Filters the environment passed to harness child processes. The machine credential is
    /// removed, so the agent and its tools never see it.
    fn harness_env(&self, env: Vec<(String, String)>) -> Vec<(String, String)> {
        identity::harness_env(env)
    }
}

/// Builds the host mode selected by `SM_HOST` from `config`.
///
/// The local mode starts with an empty in-memory credential store. The OS keychain store
/// is not wired yet; construct [`LocalHost::new`] with a real store to inject one.
pub fn from_config(config: &Config) -> Result<Box<dyn HostMode>, HostError> {
    from_kind(HostKind::from_env()?, config)
}

/// Builds the host mode `kind` from `config`. See [`from_config`].
pub fn from_kind(kind: HostKind, config: &Config) -> Result<Box<dyn HostMode>, HostError> {
    Ok(match kind {
        HostKind::Local => Box::new(LocalHost::new(
            config,
            Arc::new(MemoryCredentialStore::new()),
        )),
        HostKind::Sandbox => Box::new(SandboxHost::new(config)?),
    })
}
