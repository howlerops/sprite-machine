//! The `sm` runner: host modes (local, sandbox), tunnel client, TUI, pairing, server mode.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use runner::login::flow::normalize_hub;
use runner::login::{
    Confirm, CredentialStore, login, logout, real_sleeper, status, terminal_prompter,
};
use runner::qr;
use runner::server::{self, Options, RcArgs};
use runner::trust::{self, Audit, TrustStore};
use runner::tunnel::Timing;

/// Crate name, used by the skeleton smoke test.
pub const CRATE: &str = env!("CARGO_PKG_NAME");

const USAGE: &str = "usage: sm | sm --rc [name] | sm login --hub <url> | sm logout | sm status | sm rc [flags] | \
sm rc close <thread id> | sm trust <dir> | sm untrust <dir> | sm mobile [--hub <url>]";
const MOBILE_USAGE: &str = "usage: sm mobile [--hub <url>]";
const LOGIN_USAGE: &str = "usage: sm login --hub <url> [--yes]";
const LOGOUT_USAGE: &str = "usage: sm logout";
const STATUS_USAGE: &str = "usage: sm status";
const CLOSE_USAGE: &str = "usage: sm rc close <thread id>";
const TRUST_USAGE: &str = "usage: sm trust <dir> | sm untrust <dir>";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("login") => run_login(&args[1..]).await,
        Some("logout") => run_logout(&args[1..]),
        Some("status") => run_status(&args[1..]),
        Some("rc") if args.get(1).map(String::as_str) == Some("close") => {
            run_close(&args[2..]).await
        }
        Some("rc" | "run") => run_rc(&args[1..]).await,
        Some("trust") => run_trust(&args[1..], true),
        Some("untrust") => run_trust(&args[1..], false),
        Some("mobile") => run_mobile(&args[1..]),
        Some("--rc") if args.len() <= 2 => run_tui(true, args.get(1).cloned()).await,
        Some("--version" | "-V") => {
            println!("{CRATE} {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => bail!("unknown command {other:?}\n{USAGE}"),
        None => run_tui(false, None).await,
    }
}

/// `sm` / `sm --rc [name]`: the interactive session (see `runner::tui`). `--rc` shares it with
/// the hub from the start; without it the `autoConnect` setting decides, and `/rc` shares later.
async fn run_tui(rc: bool, name: Option<String>) -> Result<()> {
    let state_dir = state_dir()?;
    runner::tui::host::run(runner::tui::host::Options {
        credentials: CredentialStore::from_env()?,
        settings: runner::settings::load(&state_dir),
        state_dir,
        cwd: std::env::current_dir().context("current directory")?,
        home: std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default(),
        path_env: std::env::var_os("PATH").unwrap_or_default(),
        permission_mode: server::PermissionMode::Default,
        rc,
        name,
    })
    .await
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

/// `sm logout`: forgets the stored credential (keychain and file) on this machine.
fn run_logout(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        bail!("{LOGOUT_USAGE}");
    }
    logout(&CredentialStore::from_env()?, &mut std::io::stdout())
}

/// `sm status`: the stored hub, machine id and whether a credential is present. Never the
/// credential.
fn run_status(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        bail!("{STATUS_USAGE}");
    }
    status(&CredentialStore::from_env()?, &mut std::io::stdout())
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
    let state_dir = state_dir()?;
    // The trust question needs a terminal to ask at; a machine run without one refuses an
    // untrusted directory.
    let terminal = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let prompt = qr::Prompt::default();
    let asker = terminal.then(|| trust::terminal_asker(prompt.clone()));
    // On a terminal the spacebar shows the QR of the latest session. The raw mode lasts until this
    // returns, errors included.
    let _raw = qr::keys::RawMode::enable(terminal)?;
    let latest = qr::Latest::default();
    if _raw.is_some() {
        let latest = latest.clone();
        // Detached: a read blocked on stdin does not hold the process up at exit.
        std::thread::spawn(move || {
            let _ = qr::keys::pump(
                std::io::stdin().lock(),
                &latest,
                &prompt,
                &mut std::io::stdout(),
            );
        });
    }
    let session_latest = latest;
    let session_started: server::Sink = std::sync::Arc::new(move |url: &str| {
        use std::io::Write;
        session_latest.set(url);
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "session: {url}");
        let _ = out.flush();
    });
    let options = Options {
        credentials,
        allow_remote_terminal: trust::remote_terminal_allowed(&state_dir, rc.allow_remote_terminal),
        state_dir,
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
        // The terminal gets the QR code and the URL; nothing else ever sees the link code.
        signin: Some(std::sync::Arc::new(|block: &str| {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(block.as_bytes());
            let _ = out.flush();
        })),
        asker,
    };
    server::run_announcing(options, session_started, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

/// `sm mobile [--hub <url>]`: a QR code for the phone, which opens the hub's mobile web page (and
/// installs the app once it ships). The hub is the stored login's, unless `--hub` names one.
fn run_mobile(args: &[String]) -> Result<()> {
    let hub = match args {
        [] => CredentialStore::from_env()?
            .load()?
            .map(|login| login.hub)
            .ok_or_else(|| anyhow!("not logged in: run `sm login --hub <url>`"))?,
        [flag, url] if flag == "--hub" => url.clone(),
        _ => bail!("{MOBILE_USAGE}"),
    };
    print!("{}", qr::block(&qr::mobile_url(&hub)?));
    Ok(())
}

/// `sm trust <dir>` / `sm untrust <dir>`: stores or forgets a trusted directory of this machine.
fn run_trust(args: &[String], trusting: bool) -> Result<()> {
    let [dir] = args else {
        bail!("{TRUST_USAGE}");
    };
    let state = state_dir()?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let store = TrustStore::new(&state, &home);
    let audit = Audit::new(&state);
    let dir = PathBuf::from(dir);
    if trusting {
        let trusted = trust::grant(&store, &audit, &dir, "sm trust")
            .with_context(|| format!("trust {}", dir.display()))?;
        println!("trusted {}", trusted.display());
    } else if trust::revoke(&store, &audit, &dir, "sm untrust")
        .with_context(|| format!("untrust {}", dir.display()))?
    {
        println!("untrusted {}", dir.display());
    } else {
        println!("{} was not trusted", dir.display());
    }
    Ok(())
}

/// `sm rc close <thread id>`: asks the `sm rc` running on this machine to end the session of an
/// idle thread (the thread stays; its next prompt starts the session again).
async fn run_close(args: &[String]) -> Result<()> {
    let [thread_id] = args else {
        bail!("{CLOSE_USAGE}");
    };
    server::close_session(&state_dir()?, thread_id).await?;
    println!("closed {thread_id}");
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
