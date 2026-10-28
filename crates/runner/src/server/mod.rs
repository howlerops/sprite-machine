//! `sm rc`: the runner's server mode (PLAN-v2-ade.md §4.10). The machine connects to the hub,
//! advertises the harnesses it found, and runs the agent sessions that phones and desktops
//! dispatch to it.
//!
//! - One tunnel ([`crate::tunnel`]) for the machine; its sleep watcher runs beside it.
//! - One session per thread: its own [`sidecar::bridge::Bridge`] over its own harness process
//!   (`Bridge::spawn_with`, so a crashed agent is started again and resumed on the next prompt)
//!   and its own sidecar store under `<state dir>/sessions/<thread id>`. A shared store would let
//!   one bridge's start-up end the interrupts of another session.
//! - [`router`] reads the tunnel's commands and hands each to its session, or answers it itself
//!   with `command_result` (`capacity`, `harness_missing`, `unknown_thread`, `invalid`).
//! - `--spawn worktree` runs each session in its own `git worktree` under
//!   `<state dir>/worktrees`, removed when the session ends (kept when it has uncommitted
//!   changes).
//! - The spawn mode is saved per project in `<state dir>/projects.json` with the last session of
//!   the directory, which `-c` / `--continue` picks up again and registers with the hub
//!   (`thread_open`) as a session started on this runner.
//!
//! The runner's `--permission-mode` is the mode every session gets, whatever the dispatch
//! asked for: whoever sits at the machine decides how far an agent may go. The bridge applies it
//! (`Config::initial_mode`, an ACP `session/set_mode`) right after the session is attached and
//! before the first prompt; a mode the agent refuses fails the task.
//!
//! A session ends when the hub cancels its idle thread, or after `--idle-timeout` (default 2 h)
//! without a task; its clean worktree is removed and its slot is free. At start, worktrees under
//! `<state dir>/worktrees` that no session owns are removed when clean and listed in one
//! warning when they have changes.

mod names;
mod project;
mod router;
mod session;
mod worktree;

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sidecar::store::Store;

use crate::harness;
use crate::login::{CredentialStore, KeychainCredentials};
use crate::sleep;
use crate::tunnel::{Timing, TunnelConfig, tunnel};

pub use self::names::{host_prefix, session_name};
pub use self::project::{LastSession, Projects};
pub use self::worktree::Worktree;

/// Default `--capacity`.
pub const DEFAULT_CAPACITY: u32 = 32;

/// Default `--idle-timeout`: 2 hours.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

/// Where a session's files live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnMode {
    /// In the directory the session was asked for.
    SameDir,
    /// In a new `git worktree` of it.
    Worktree,
}

impl SpawnMode {
    /// `same-dir` / `same_dir` / `worktree`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "same-dir" | "same_dir" => Some(Self::SameDir),
            "worktree" => Some(Self::Worktree),
            _ => None,
        }
    }

    /// The tunnel's name for the mode (tunnel.md §3.1).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SameDir => "same_dir",
            Self::Worktree => "worktree",
        }
    }
}

/// How far an agent may go without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    BypassPermissions,
}

impl PermissionMode {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "default" => Some(Self::Default),
            "acceptEdits" => Some(Self::AcceptEdits),
            "plan" => Some(Self::Plan),
            "bypassPermissions" => Some(Self::BypassPermissions),
            _ => None,
        }
    }

    /// The ACP session mode id.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
        }
    }
}

/// Everything `sm rc` needs. [`RcArgs::parse`] fills the flags; `sm` fills the environment.
pub struct Options {
    /// The stored machine login (hub, machine id, credential).
    pub credentials: CredentialStore,
    /// The runner's state directory (tunnel outbox, sessions, worktrees, `projects.json`).
    pub state_dir: PathBuf,
    /// The current directory: the project `--spawn` is saved for and `-c` looks up.
    pub cwd: PathBuf,
    /// The home directory (local MCP servers, `~/.local/bin` for harness discovery).
    pub home: PathBuf,
    /// `PATH`, where harnesses are looked for.
    pub path_env: OsString,
    /// `--spawn`; `None`: the project's saved mode, else same-dir.
    pub spawn: Option<SpawnMode>,
    /// `--capacity`: most sessions with a running task at once.
    pub capacity: u32,
    pub permission_mode: PermissionMode,
    /// `--name-prefix`.
    pub name_prefix: String,
    /// `-c` / `--continue`.
    pub continue_last: bool,
    /// `--idle-timeout`: a session without a task for this long ends.
    pub idle_timeout: Duration,
    pub timing: Timing,
}

/// The flags of `sm rc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RcArgs {
    pub hub: Option<String>,
    pub spawn: Option<SpawnMode>,
    pub capacity: u32,
    pub permission_mode: PermissionMode,
    pub name_prefix: Option<String>,
    pub continue_last: bool,
    pub idle_timeout: Duration,
}

/// `90`, `90s`, `15m` or `2h`.
fn parse_duration(text: &str) -> Option<Duration> {
    let (digits, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((at, _)) => text.split_at(at),
        None => (text, "s"),
    };
    let n: u64 = digits.parse().ok().filter(|n| *n > 0)?;
    let factor = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return None,
    };
    n.checked_mul(factor).map(Duration::from_secs)
}

pub const RC_USAGE: &str = "usage: sm rc [--hub <url>] [--spawn same-dir|worktree] [--capacity <n>] \
[--permission-mode default|acceptEdits|plan|bypassPermissions] [--name-prefix <prefix>] \
[--idle-timeout <n>[s|m|h]] [-c|--continue]";

impl RcArgs {
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut parsed = Self {
            hub: None,
            spawn: None,
            capacity: DEFAULT_CAPACITY,
            permission_mode: PermissionMode::Default,
            name_prefix: None,
            continue_last: false,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        };
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let mut value = |name: &str| -> Result<&String> {
                args.next()
                    .ok_or_else(|| anyhow!("{name} needs a value\n{RC_USAGE}"))
            };
            match flag.as_str() {
                "--hub" => parsed.hub = Some(value("--hub")?.clone()),
                "--spawn" => {
                    let text = value("--spawn")?;
                    parsed.spawn = Some(SpawnMode::parse(text).ok_or_else(|| {
                        anyhow!("--spawn takes same-dir or worktree, not {text:?}")
                    })?);
                }
                "--capacity" => {
                    let text = value("--capacity")?;
                    parsed.capacity =
                        text.parse::<u32>().ok().filter(|n| *n > 0).ok_or_else(|| {
                            anyhow!("--capacity takes a number above 0, not {text:?}")
                        })?;
                }
                "--permission-mode" => {
                    let text = value("--permission-mode")?;
                    parsed.permission_mode = PermissionMode::parse(text).ok_or_else(|| {
                        anyhow!(
                            "--permission-mode takes default, acceptEdits, plan or \
                             bypassPermissions, not {text:?}"
                        )
                    })?;
                }
                "--name-prefix" => {
                    let text = value("--name-prefix")?;
                    parsed.name_prefix = Some(names::sanitize(text).ok_or_else(|| {
                        anyhow!("--name-prefix needs letters, digits or '-', not {text:?}")
                    })?);
                }
                "--idle-timeout" => {
                    let text = value("--idle-timeout")?;
                    parsed.idle_timeout = parse_duration(text).ok_or_else(|| {
                        anyhow!("--idle-timeout takes a number with s, m or h (2h), not {text:?}")
                    })?;
                }
                "-c" | "--continue" => parsed.continue_last = true,
                other => bail!("unknown flag {other:?}\n{RC_USAGE}"),
            }
        }
        Ok(parsed)
    }
}

/// Runs the server until `shutdown` completes (or the tunnel stops for good).
pub async fn run(options: Options, shutdown: impl Future<Output = ()>) -> Result<()> {
    let login = options
        .credentials
        .load()?
        .ok_or_else(|| anyhow!("not logged in: run `sm login --hub <url>`"))?;
    let found = harness::discover(&options.path_env, &options.home);
    let projects = Projects::new(&options.state_dir);
    std::fs::create_dir_all(&options.state_dir)
        .with_context(|| format!("create {}", options.state_dir.display()))?;

    let spawn = match options.spawn {
        Some(mode) => {
            projects.set_spawn_mode(&options.cwd, mode)?;
            mode
        }
        None => projects
            .spawn_mode(&options.cwd)
            .unwrap_or(SpawnMode::SameDir),
    };

    // Before any session exists, so every worktree here is a leftover; `-c` still needs the
    // worktree of the last session.
    let keep: Vec<PathBuf> = options
        .continue_last
        .then(|| projects.last_session(&options.cwd))
        .flatten()
        .map(|last| vec![last.cwd])
        .unwrap_or_default();
    worktree::sweep(&options.state_dir.join("worktrees"), &keep).await;

    let mut capabilities = harness::capabilities(&found, options.capacity);
    // The saved / chosen mode first: it is the one a dispatch without a preference gets.
    capabilities
        .spawn
        .sort_by_key(|mode| mode != spawn.as_str());
    let config = TunnelConfig {
        hub_ws_base: login.hub.clone(),
        machine_id: login.machine_id.clone(),
        runner_version: env!("CARGO_PKG_VERSION").to_owned(),
        capabilities,
        timing: options.timing,
    };
    let tunnel_store = Arc::new(Store::open(&options.state_dir).context("open the sidecar store")?);
    let (client, sink, commands) = tunnel(
        config,
        tunnel_store,
        KeychainCredentials::new(options.credentials),
    );
    // The hook starts before the tunnel runs, so a sleep that comes early is still hinted.
    let sleep_hook = sleep::spawn(client.sleep_hint());
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let client_task = tokio::spawn(client.run(async {
        let _ = stop_rx.await;
    }));

    let result = router::Router::new(
        router::Context {
            machine_id: login.machine_id,
            state_dir: options.state_dir,
            cwd: options.cwd,
            home: options.home,
            found,
            spawn,
            capacity: usize::try_from(options.capacity).unwrap_or(usize::MAX),
            permission_mode: options.permission_mode,
            name_prefix: options.name_prefix,
            idle_timeout: options.idle_timeout,
        },
        sink,
        commands,
        projects,
    )
    .run(options.continue_last, shutdown, client_task, stop_tx)
    .await;
    sleep_hook.abort();
    result
}

/// `state_dir/sessions/<thread id>`: the sidecar store of one session.
pub(crate) fn session_dir(state_dir: &Path, thread_id: &str) -> PathBuf {
    state_dir.join("sessions").join(thread_id)
}
