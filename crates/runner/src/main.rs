//! The `sm` runner: host modes (local, sandbox), tunnel client, TUI, pairing, server mode.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use protocol::tunnel::Capabilities;
use runner::login::flow::normalize_hub;
use runner::login::{CredentialStore, KeychainCredentials, login, real_sleeper};
use runner::sleep;
use runner::tunnel::{Timing, TunnelConfig, tunnel};
use sidecar::store::Store;

/// Crate name, used by the skeleton smoke test.
pub const CRATE: &str = env!("CARGO_PKG_NAME");

const USAGE: &str = "usage: sm login --hub <url>";
const RUN_USAGE: &str = "usage: sm run --hub <url>";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("login") => run_login(&args[1..]).await,
        Some("run") => run_tunnel(&args[1..]).await,
        Some(other) => bail!("unknown command {other:?}\n{USAGE}"),
        None => {
            println!("{CRATE} {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

/// `sm login --hub <url>`: pair this machine with a hub by device code.
async fn run_login(args: &[String]) -> Result<()> {
    let hub = match args {
        [flag, url] if flag == "--hub" => url.as_str(),
        _ => bail!("{USAGE}"),
    };
    let store = CredentialStore::from_env()?;
    login(hub, &store, &mut std::io::stdout(), &real_sleeper()).await?;
    Ok(())
}

/// `sm run --hub <url>`: keeps this machine's tunnel up until Ctrl-C, with the OS sleep hook.
/// The hook starts before the tunnel runs, so a sleep that comes early is still hinted.
async fn run_tunnel(args: &[String]) -> Result<()> {
    let hub = match args {
        [flag, url] if flag == "--hub" => url.as_str(),
        _ => bail!("{RUN_USAGE}"),
    };
    let store = CredentialStore::from_env()?;
    let login = store
        .load()?
        .ok_or_else(|| anyhow!("not logged in: run `sm login --hub {hub}`"))?;
    if normalize_hub(&login.hub)? != normalize_hub(hub)? {
        bail!("this machine is logged in to {}, not {hub}", login.hub);
    }
    let sidecar = Arc::new(Store::open(&state_dir()?).context("open the sidecar store")?);
    let config = TunnelConfig {
        hub_ws_base: login.hub.clone(),
        machine_id: login.machine_id.clone(),
        runner_version: env!("CARGO_PKG_VERSION").to_owned(),
        // No harness or spawn mode is advertised yet: the start path only keeps the tunnel up.
        capabilities: Capabilities {
            host_mode: "local".to_owned(),
            harnesses: Vec::new(),
            max_sessions: 1,
            spawn: Vec::new(),
            features: Vec::new(),
        },
        timing: Timing::default(),
    };
    let (client, _sink, _commands) = tunnel(config, sidecar, KeychainCredentials::new(store));
    let _sleep_hook = sleep::spawn(client.sleep_hint());
    client
        .run(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// Where the sidecar keeps its state: `SM_STATE_DIR`, else `$XDG_STATE_HOME/sm`, else
/// `~/.local/state/sm`.
fn state_dir() -> Result<PathBuf> {
    let non_empty = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    if let Some(dir) = non_empty("SM_STATE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = non_empty("XDG_STATE_HOME") {
        return Ok(PathBuf::from(dir).join("sm"));
    }
    let home = non_empty("HOME").ok_or_else(|| anyhow!("no home directory: set SM_STATE_DIR"))?;
    Ok(PathBuf::from(home).join(".local/state/sm"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE, "runner");
    }
}
