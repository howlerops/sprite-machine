//! The `sm` runner: host modes (local, sandbox), tunnel client, TUI, pairing, server mode.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use runner::login::flow::normalize_hub;
use runner::login::{Confirm, CredentialStore, login, real_sleeper, terminal_prompter};
use runner::server::{self, Options, RcArgs};
use runner::tunnel::Timing;

/// Crate name, used by the skeleton smoke test.
pub const CRATE: &str = env!("CARGO_PKG_NAME");

const USAGE: &str = "usage: sm login --hub <url> | sm rc [flags]";
const LOGIN_USAGE: &str = "usage: sm login --hub <url> [--yes]";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("login") => run_login(&args[1..]).await,
        Some("rc" | "run") => run_rc(&args[1..]).await,
        Some(other) => bail!("unknown command {other:?}\n{USAGE}"),
        None => {
            println!("{CRATE} {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

/// `sm login --hub <url>`: pair this machine with a hub by device code.
async fn run_login(args: &[String]) -> Result<()> {
    let is_yes = |arg: &String| arg == "--yes" || arg == "-y";
    let (hub, yes) = match args {
        [flag, url] if flag == "--hub" => (url.as_str(), false),
        [flag, url, yes] if flag == "--hub" && is_yes(yes) => (url.as_str(), true),
        [yes, flag, url] if flag == "--hub" && is_yes(yes) => (url.as_str(), true),
        _ => bail!("{LOGIN_USAGE}"),
    };
    let confirm = if yes {
        Confirm::Yes
    } else {
        Confirm::Ask(terminal_prompter())
    };
    let store = CredentialStore::from_env()?;
    login(
        hub,
        &store,
        &mut std::io::stdout(),
        &real_sleeper(),
        &confirm,
    )
    .await?;
    Ok(())
}

/// `sm rc [flags]` (`sm run` is the same command): the server mode (see `runner::server`). Keeps
/// this machine's tunnel up and runs the sessions the hub dispatches, until Ctrl-C.
async fn run_rc(args: &[String]) -> Result<()> {
    let rc = RcArgs::parse(args)?;
    let credentials = CredentialStore::from_env()?;
    let login = credentials
        .load()?
        .ok_or_else(|| anyhow!("not logged in: run `sm login --hub <url>`"))?;
    if let Some(hub) = &rc.hub
        && normalize_hub(&login.hub)? != normalize_hub(hub)?
    {
        bail!("this machine is logged in to {}, not {hub}", login.hub);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let options = Options {
        credentials,
        state_dir: state_dir()?,
        cwd: std::env::current_dir().context("current directory")?,
        home,
        path_env: std::env::var_os("PATH").unwrap_or_default(),
        spawn: rc.spawn,
        capacity: rc.capacity,
        permission_mode: rc.permission_mode,
        name_prefix: rc.name_prefix.unwrap_or_else(server::host_prefix),
        continue_last: rc.continue_last,
        idle_timeout: rc.idle_timeout,
        timing: Timing::default(),
    };
    server::run(options, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
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
