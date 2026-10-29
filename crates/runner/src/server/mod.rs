//! `sm rc`: the runner's server mode (PLAN-v2-ade.md §4.10). The machine connects to the hub,
//! advertises the harnesses it found, and runs the agent sessions that phones and desktops
//! dispatch to it.
//!
//! - With a sign-in sink, the machine asks the hub for a phone sign-in link and prints it as a QR
//!   code and a URL, at start and on `SIGUSR1` ([`signin`]).
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
//! - A directory must be trusted before a session starts in it ([`crate::trust`]): an untrusted
//!   one is asked about at a terminal, and refused (`workspace_untrusted`) without one.
//! - The spawn mode is saved per project in `<state dir>/projects.json` with the last session of
//!   the directory, which `-c` / `--continue` picks up again and registers with the hub
//!   (`thread_open`) as a session started on this runner.
//!
//! The runner's `--permission-mode` is the mode every session gets, whatever the dispatch
//! asked for: whoever sits at the machine decides how far an agent may go. The bridge applies it
//! (`Config::initial_mode`, an ACP `session/set_mode`) right after the session is attached and
//! before the first prompt; a mode the agent refuses fails the task.
//!
//! A session ends when the hub cancels its idle thread, after `--idle-timeout` (default 2 h)
//! without a task, or when `sm rc close <thread id>` asks for it ([`local`], a unix socket at
//! `<state dir>/rc.sock`). Its clean worktree is removed and its slot is free. A later prompt for
//! the thread starts the session again: a worktree is added back on the session's branch
//! (`sm/<name>`) and the agent resumes its ACP session. At start, worktrees under
//! `<state dir>/worktrees` that no session owns are removed when clean and listed in one
//! warning when they have changes.

mod local;
mod names;
mod project;
mod router;
mod session;
mod signin;
mod worktree;

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sidecar::bridge::files::FilePolicy;
use sidecar::store::Store;

use crate::harness;
use crate::login::{CredentialStore, KeychainCredentials};
use crate::sleep;
use crate::trust::{Asker, Audit, TrustStore};
use crate::tunnel::{Timing, TunnelConfig, tunnel};

pub use self::local::close_session;
pub use self::names::{host_prefix, session_name};
pub use self::project::{LastSession, Projects};
pub use self::signin::Sink;
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

    /// How strict the mode is: `plan` < `default` < `acceptEdits` < `bypassPermissions` by how
    /// much the agent may do without asking, so a lower rank is the stricter mode.
    fn rank(self) -> u8 {
        match self {
            Self::Plan => 0,
            Self::Default => 1,
            Self::AcceptEdits => 2,
            Self::BypassPermissions => 3,
        }
    }

    /// The mode a remotely requested `requested` runs in on a machine whose local mode is
    /// `self` (plan §4.10, "the local permission mode applies"): the request when it is the
    /// same or stricter, otherwise the local mode. An unknown request gets the local mode.
    pub fn capped(self, requested: &str) -> Self {
        match Self::parse(requested) {
            Some(requested) if requested.rank() <= self.rank() => requested,
            _ => self,
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
    /// Where the phone sign-in link (QR and URL) is printed: at start and on `SIGUSR1`. `None`:
    /// no link is requested from the hub (see [`signin`]).
    pub signin: Option<Sink>,
    /// The trust question of an untrusted directory. `None`: no terminal, so such a dispatch is
    /// refused.
    pub asker: Option<Asker>,
    /// `--allow-remote-terminal` or the state dir setting (see [`trust::remote_terminal_allowed`]).
    pub allow_remote_terminal: bool,
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
    /// `--allow-remote-terminal`.
    pub allow_remote_terminal: bool,
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
[--idle-timeout <n>[s|m|h]] [-c|--continue] [--allow-remote-terminal]";

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
            allow_remote_terminal: false,
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
                "--allow-remote-terminal" => parsed.allow_remote_terminal = true,
                other => bail!("unknown flag {other:?}\n{RC_USAGE}"),
            }
        }
        Ok(parsed)
    }
}

/// Runs the server until `shutdown` completes (or the tunnel stops for good). Attachments are
/// fetched under [`FilePolicy::default`].
pub async fn run(options: Options, shutdown: impl Future<Output = ()>) -> Result<()> {
    run_with(options, FilePolicy::default(), None, shutdown).await
}

/// [`run`], and `session_started` gets the link (`<hub>/t/<thread id>`) of each session as it starts.
pub async fn run_announcing(
    options: Options,
    session_started: Sink,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    run_with(
        options,
        FilePolicy::default(),
        Some(session_started),
        shutdown,
    )
    .await
}

/// TEST ONLY: [`run`] with another attachment policy, such as
/// [`FilePolicy::loopback_for_tests`]. Compiled only with the `test-support` feature.
#[cfg(feature = "test-support")]
pub async fn run_with_file_policy(
    options: Options,
    file_policy: FilePolicy,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    run_with(options, file_policy, None, shutdown).await
}

async fn run_with(
    options: Options,
    file_policy: FilePolicy,
    session_started: Option<Sink>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let login = options
        .credentials
        .load()?
        .ok_or_else(|| anyhow!("not logged in: run `sm login --hub <url>`"))?;
    let found = harness::discover(&options.path_env, &options.home);
    let projects = Projects::new(&options.state_dir);
    std::fs::create_dir_all(&options.state_dir)
        .with_context(|| format!("create {}", options.state_dir.display()))?;
    // Held until `run` ends: the socket is removed with it.
    let (_control, local_requests) = local::Listener::bind(&options.state_dir)?;

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
    // The machine's local mode caps what a remote request may ask for; the composer reads it.
    capabilities.features.push(format!(
        "permission_mode:{}",
        options.permission_mode.as_str()
    ));
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
    let credentials = KeychainCredentials::new(options.credentials);
    let signin_task = options
        .signin
        .map(|sink| signin::spawn(sink, login.clone(), credentials.clone()));
    let (client, sink, commands) = tunnel(config, tunnel_store, credentials);
    // The hook starts before the tunnel runs, so a sleep that comes early is still hinted.
    let sleep_hook = sleep::spawn(client.sleep_hint());
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let client_task = tokio::spawn(client.run(async {
        let _ = stop_rx.await;
    }));

    if options.allow_remote_terminal {
        tracing::warn!("remote terminal is allowed on this machine");
    }
    let trust = TrustStore::new(&options.state_dir, &options.home);
    let audit = Audit::new(&options.state_dir);

    let result = router::Router::new(
        router::Context {
            hub: login.hub.clone(),
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
            trust,
            audit,
            asker: options.asker,
            file_policy,
            session_started,
        },
        sink,
        commands,
        projects,
        local_requests,
    )
    .run(options.continue_last, shutdown, client_task, stop_tx)
    .await;
    sleep_hook.abort();
    if let Some(task) = signin_task {
        task.abort();
    }
    result
}

/// `state_dir/sessions/<thread id>`: the sidecar store of one session.
pub(crate) fn session_dir(state_dir: &Path, thread_id: &str) -> PathBuf {
    state_dir.join("sessions").join(thread_id)
}

#[cfg(test)]
mod file_policy_tests {
    use super::FilePolicy;

    /// The policy `run` uses in every build: `http` and loopback are refused.
    #[test]
    fn the_production_policy_refuses_loopback() {
        assert!(!FilePolicy::default().insecure_loopback_for_tests);
    }

    /// The loopback policy is built only by the `test-support` constructor, so a build without
    /// the feature has no way to make one.
    #[cfg(feature = "test-support")]
    #[test]
    fn the_loopback_policy_comes_only_from_the_test_constructor() {
        let policy = FilePolicy::loopback_for_tests(1024);
        assert!(policy.insecure_loopback_for_tests);
        assert_eq!(policy.max_bytes, 1024);
    }
}
