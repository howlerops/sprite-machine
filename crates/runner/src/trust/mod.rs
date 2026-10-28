//! Workspace trust and the remote-terminal opt-in (PLAN-v2-ade.md §4.10).
//!
//! - A directory is trusted the first time it is used. With a terminal to ask, the runner asks
//!   `Trust <dir>? [y/N]` and stores a yes ([`TrustStore`]). Without one (a dispatch from the hub),
//!   an untrusted directory is refused.
//! - The home directory and `/` are asked every time and never stored.
//! - Trust decisions are recorded in the local audit log ([`Audit`]).
//! - Remote terminal is off unless `--allow-remote-terminal` is given or `<state dir>/settings.json`
//!   says `"allow_remote_terminal": true` ([`remote_terminal_allowed`]).

mod audit;
mod store;

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};

pub use audit::Audit;
pub use store::TrustStore;

/// Answers the trust question for a directory: `true` is yes. Shared with the router's worker
/// threads, so it may run off the async runtime.
pub type Asker = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

/// What the trust check decided for a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Stored as trusted.
    Trusted,
    /// Answered yes and stored now.
    Granted,
    /// Answered yes for the home directory or `/`: this use only, not stored.
    Once,
    /// Not trusted, and no answer was given (no terminal, or a no).
    Refused,
}

/// The trust check before a session starts in `dir` (canonicalized first). `asker` is `None`
/// when there is no terminal to ask at.
pub fn decide(
    store: &TrustStore,
    audit: &Audit,
    dir: &Path,
    asker: Option<&Asker>,
) -> io::Result<Verdict> {
    let dir = TrustStore::canonical(dir)?;
    if store.is_trusted(&dir)? {
        return Ok(Verdict::Trusted);
    }
    let Some(ask) = asker else {
        return Ok(Verdict::Refused);
    };
    let ask: &(dyn Fn(&Path) -> bool + Send + Sync) = &**ask;
    if !ask(&dir) {
        audit.record("trust_declined", json!({ "dir": dir, "by": "prompt" }));
        return Ok(Verdict::Refused);
    }
    if store.persistable(&dir) {
        let dir = store.trust(&dir)?;
        audit.record(
            "trust_granted",
            json!({ "dir": dir, "by": "prompt", "persisted": true }),
        );
        Ok(Verdict::Granted)
    } else {
        audit.record(
            "trust_granted",
            json!({ "dir": dir, "by": "prompt", "persisted": false }),
        );
        Ok(Verdict::Once)
    }
}

/// `sm trust <dir>`: stores `dir` as trusted. Returns its canonical form. `by` names the caller
/// in the audit record.
pub fn grant(
    store: &TrustStore,
    audit: &Audit,
    dir: &Path,
    by: &str,
) -> io::Result<std::path::PathBuf> {
    let dir = store.trust(dir)?;
    audit.record(
        "trust_granted",
        json!({ "dir": dir, "by": by, "persisted": true }),
    );
    Ok(dir)
}

/// `sm untrust <dir>`: forgets `dir`. `false` when it was not trusted.
pub fn revoke(store: &TrustStore, audit: &Audit, dir: &Path, by: &str) -> io::Result<bool> {
    let removed = store.untrust(dir)?;
    if removed {
        audit.record("trust_removed", json!({ "dir": dir, "by": by }));
    }
    Ok(removed)
}

/// The question and the answer. `y` or `yes` (any case) is yes; anything else, and a closed
/// input, is no.
pub fn ask_on(dir: &Path, input: &mut impl BufRead, out: &mut impl Write) -> io::Result<bool> {
    write!(out, "Trust {}? [y/N] ", dir.display())?;
    out.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// The asker of a machine with a terminal: the question goes to stderr (stdout carries the
/// sign-in code) and the answer is read from stdin.
pub fn terminal_asker() -> Asker {
    Arc::new(|dir: &Path| {
        let mut input = io::stdin().lock();
        ask_on(dir, &mut input, &mut io::stderr()).unwrap_or(false)
    })
}

/// Whether a remote-dispatched session may open a terminal on this machine: the flag, or
/// `"allow_remote_terminal": true` in `<state dir>/settings.json`. Anything else, including a
/// settings file that does not parse, means no.
pub fn remote_terminal_allowed(state_dir: &Path, flag: bool) -> bool {
    flag || fs::read(state_dir.join("settings.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.get("allow_remote_terminal")?.as_bool())
        .unwrap_or(false)
}
