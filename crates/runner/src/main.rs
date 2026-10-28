//! The `sm` runner: host modes (local, sandbox), tunnel client, TUI, pairing, server mode.

use anyhow::{Result, bail};
use runner::login::{CredentialStore, login, real_sleeper};

/// Crate name, used by the skeleton smoke test.
pub const CRATE: &str = env!("CARGO_PKG_NAME");

const USAGE: &str = "usage: sm login --hub <url>";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("login") => run_login(&args[1..]).await,
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

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE, "runner");
    }
}
