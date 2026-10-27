//! Runner configuration, read from `SM_*` environment variables.
//!
//! | Variable                | Default                                                                          | Meaning |
//! | ----------------------- | -------------------------------------------------------------------------------- | ------- |
//! | `SM_HUB_URL`            | none (`hub_url` is `None`)                                                       | Hub base URL, e.g. `wss://hub.example.com`. |
//! | `SM_MACHINE_CREDENTIAL` | none (`machine_credential` is `None`)                                            | Machine credential for the tunnel. Held in memory only; its `Debug` prints `<redacted>`. Empty counts as unset. |
//! | `SM_STATE_DIR`          | `$XDG_DATA_HOME/sm` if `XDG_DATA_HOME` is absolute, else `$HOME/.local/share/sm` | Durable state directory. |
//! | `SM_LOG`                | `info`                                                                           | `tracing` filter directive, e.g. `info,sidecar=debug`. |
//! | `SM_HEALTHZ_ADDR`       | `127.0.0.1:0` (ephemeral port)                                                   | Socket address for the `/healthz` server. |
//!
//! Parsing is a pure function over a map ([`Config::from_map`]), so tests never touch the
//! process environment. [`Config::from_env`] collects the map from the process.

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context as _;

/// Default `/healthz` bind address: loopback, ephemeral port.
pub const DEFAULT_HEALTHZ_ADDR: &str = "127.0.0.1:0";
/// Default tracing filter.
pub const DEFAULT_LOG_FILTER: &str = "info";

/// Runner configuration. Build it with [`Config::from_env`] or [`Config::from_map`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Hub base URL (`SM_HUB_URL`), if configured.
    pub hub_url: Option<String>,
    /// Machine credential (`SM_MACHINE_CREDENTIAL`), if configured.
    pub machine_credential: Option<MachineCredential>,
    /// Durable state directory (`SM_STATE_DIR`).
    pub state_dir: PathBuf,
    /// Tracing filter directive (`SM_LOG`).
    pub log_filter: String,
    /// Bind address for the `/healthz` server (`SM_HEALTHZ_ADDR`).
    pub healthz_addr: SocketAddr,
}

/// A machine credential. It is never printed: `Debug` writes `<redacted>`.
#[derive(Clone, PartialEq, Eq)]
pub struct MachineCredential(String);

impl MachineCredential {
    /// Wraps a credential string.
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// Returns the secret for use in an `Authorization` header. Do not log the result.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for MachineCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Configuration errors. Messages never contain the machine credential.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// `SM_HEALTHZ_ADDR` is not a socket address.
    #[error("invalid SM_HEALTHZ_ADDR {value:?}: {reason}")]
    InvalidHealthzAddr { value: String, reason: String },
    /// Neither `SM_STATE_DIR` nor `HOME` is set, so no default state dir exists.
    #[error("cannot pick a default state dir: set SM_STATE_DIR or HOME")]
    NoStateDir,
}

impl Config {
    /// Reads the configuration from the process environment.
    ///
    /// Non-UTF-8 variables are ignored.
    pub fn from_env() -> Result<Self, ConfigError> {
        let vars: HashMap<String, String> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect();
        Self::from_map(&vars)
    }

    /// Parses the configuration from a map of variable names to values.
    ///
    /// Empty values count as unset. See the module docs for defaults.
    pub fn from_map(vars: &HashMap<String, String>) -> Result<Self, ConfigError> {
        let get = |key: &str| vars.get(key).map(String::as_str).filter(|v| !v.is_empty());

        let state_dir = if let Some(dir) = get("SM_STATE_DIR") {
            PathBuf::from(dir)
        } else if let Some(xdg) = get("XDG_DATA_HOME").filter(|v| v.starts_with('/')) {
            PathBuf::from(xdg).join("sm")
        } else if let Some(home) = get("HOME") {
            PathBuf::from(home).join(".local").join("share").join("sm")
        } else {
            return Err(ConfigError::NoStateDir);
        };

        let healthz_raw = get("SM_HEALTHZ_ADDR").unwrap_or(DEFAULT_HEALTHZ_ADDR);
        let healthz_addr =
            healthz_raw
                .parse::<SocketAddr>()
                .map_err(|e| ConfigError::InvalidHealthzAddr {
                    value: healthz_raw.to_owned(),
                    reason: e.to_string(),
                })?;

        Ok(Self {
            hub_url: get("SM_HUB_URL").map(str::to_owned),
            machine_credential: get("SM_MACHINE_CREDENTIAL").map(MachineCredential::new),
            state_dir,
            log_filter: get("SM_LOG").unwrap_or(DEFAULT_LOG_FILTER).to_owned(),
            healthz_addr,
        })
    }
}

/// Installs the global `tracing` subscriber (stderr, filtered by `config.log_filter`).
///
/// Safe to call more than once: later calls do nothing and return `Ok`. An invalid filter
/// is an error on every call. If another subscriber was installed first, it stays in place.
pub fn init_tracing(config: &Config) -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_new(&config.log_filter)
        .with_context(|| format!("invalid SM_LOG filter {:?}", config.log_filter))?;
    if tracing::dispatcher::has_been_set() {
        return Ok(());
    }
    // `try_init` only fails when a global subscriber already exists, which is the outcome we want.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
    Ok(())
}
