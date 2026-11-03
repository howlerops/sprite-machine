//! The runner as a library for a host that is not a terminal: the desktop app (M2B-015,
//! PLAN-v2-ade.md §4.10 "the desktop app embeds it").
//!
//! [`Embedded`] is one machine tunnel to the hub with the machine's stored login, and the local
//! sessions that run on it. A session is the same engine the interactive `sm` uses
//! ([`crate::tui::session::Session`] over a [`sidecar::bridge::Bridge`]); only the terminal is
//! missing. Whether the hub knows a session is a per-session switch:
//!
//! - [`Embedded::connect`] registers the session (`thread_open`, then everything the session has
//!   logged so far, then each new event), so the phone sees it and can send prompts and answers;
//! - [`Embedded::disconnect`] stops sending. The hub keeps the thread and what it already has
//!   (tunnel.md has no frame that removes a thread); commands for it are refused
//!   (`unknown_thread`) until the next `connect`.
//!
//! A `dispatch` (a new session started from the phone) is refused with `unsupported`: the
//! embedded runner runs the sessions of its own user, not `sm rc`'s server mode.
//!
//! The credential is read by the tunnel from the [`CredentialStore`] on every connect and goes
//! into the `Authorization` header only; nothing here logs or stores it.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use protocol::tunnel::{CommandErrorCode, Workspace};
use sidecar::bridge::{
    Bridge, CommandFailure, CommandSource, Config as BridgeConfig, EventSink, IdGenerator,
    McpProvider, UlidIds,
};
use sidecar::store::Store;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::harness::{self, Found};
use crate::login::{CredentialStore, KeychainCredentials};
use crate::qr;
use crate::server::{PermissionMode, host_prefix, session_dir, session_name};
use crate::sleep;
use crate::trust::{self, Audit, TrustStore, Verdict};
use crate::tui::driver::Backend;
use crate::tui::host::{TunnelUpstream, command_thread, http_base, now_ms};
use crate::tui::session::{Handle, Identity, Registrar, Registration, Session};
use crate::tunnel::{Timing, TunnelCommands, TunnelConfig, TunnelSink, tunnel};

/// How long the tunnel gets to say goodbye at [`Embedded::stop`].
const GOODBYE: Duration = Duration::from_secs(2);

/// Everything an embedded runner needs from its host.
pub struct Config {
    /// The machine's stored login (hub, machine id, credential).
    pub credentials: CredentialStore,
    /// The runner's state directory (tunnel outbox, session stores, trusted directories).
    pub state_dir: PathBuf,
    pub home: PathBuf,
    /// `PATH`, where harnesses are looked for.
    pub path_env: OsString,
    pub permission_mode: PermissionMode,
    pub timing: Timing,
}

/// A new local session.
pub struct SessionSpec {
    /// The directory the agent works in. It must be trusted ([`Embedded::trust`]).
    pub cwd: PathBuf,
    /// The session's name on the hub; `None`: a generated one.
    pub title: Option<String>,
    /// The harness to run (`claude`, `opencode`, ...); `None`: the first one found.
    pub harness: Option<String>,
}

/// What the host shows about a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub thread_id: String,
    pub title: String,
    pub harness: String,
    pub cwd: PathBuf,
    /// The hub knows this session now.
    pub connected: bool,
    /// The session's link (`<hub>/t/<thread id>`).
    pub url: String,
}

struct LocalSession {
    thread_id: String,
    title: String,
    harness: String,
    cwd: PathBuf,
    url: String,
    connected: AtomicBool,
    handle: Handle,
    session: tokio::sync::Mutex<Session>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for LocalSession {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

type Sessions = Mutex<HashMap<String, Arc<LocalSession>>>;

/// The runner inside the host's process: the tunnel and the local sessions on it.
pub struct Embedded {
    state_dir: PathBuf,
    home: PathBuf,
    permission_mode: PermissionMode,
    hub: String,
    machine_id: String,
    found: Vec<Found>,
    sink: TunnelSink,
    ids: Arc<UlidIds>,
    sessions: Arc<Sessions>,
    processed: mpsc::UnboundedSender<(u64, String)>,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    background: Mutex<Vec<JoinHandle<()>>>,
    client: Mutex<Option<JoinHandle<()>>>,
}

impl Embedded {
    /// Starts the machine's tunnel with the stored login. Fails when `sm login` was never run.
    /// Must be called inside a tokio runtime.
    pub async fn start(config: Config) -> Result<Self> {
        let login = config
            .credentials
            .load()?
            .ok_or_else(|| anyhow!("not logged in: this computer is not enrolled with a hub"))?;
        let found = harness::discover(&config.path_env, &config.home);
        let mut capabilities = harness::capabilities(&found, 32);
        capabilities.features.push(format!(
            "permission_mode:{}",
            config.permission_mode.as_str()
        ));
        let tunnel_config = TunnelConfig {
            hub_ws_base: login.hub.clone(),
            machine_id: login.machine_id.clone(),
            runner_version: env!("CARGO_PKG_VERSION").to_owned(),
            capabilities,
            timing: config.timing,
        };
        std::fs::create_dir_all(&config.state_dir)
            .with_context(|| format!("create {}", config.state_dir.display()))?;
        let store = Arc::new(
            Store::open(&config.state_dir.join("desktop")).context("open the tunnel store")?,
        );
        // The tunnel reads the login again on every reconnect, so a re-login is picked up.
        let credentials = KeychainCredentials::new(config.credentials);
        let (client, sink, commands) = tunnel(tunnel_config, store, credentials);
        let sleep_hook = sleep::spawn(client.sleep_hint());
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let client_task = tokio::spawn(async move {
            let stopped = async {
                let _ = stop_rx.await;
            };
            if let Err(error) = client.run(stopped).await {
                tracing::warn!("the tunnel stopped: {error:#}");
            }
        });

        let sessions: Arc<Sessions> = Arc::default();
        let (processed_tx, processed_rx) = mpsc::unbounded_channel();
        let router = tokio::spawn(route(
            commands,
            sink.clone(),
            Arc::clone(&sessions),
            processed_rx,
        ));
        // A prompt the hub rejected has no screen to go back to; it is only noted.
        let mut background = vec![router, sleep_hook];
        if let Some(mut refusals) = sink.prompt_refusals() {
            background.push(tokio::spawn(async move {
                while let Some(refusal) = refusals.recv().await {
                    tracing::warn!("the hub refused a prompt: {}", refusal.message);
                }
            }));
        }
        Ok(Self {
            state_dir: config.state_dir,
            home: config.home,
            permission_mode: config.permission_mode,
            hub: login.hub,
            machine_id: login.machine_id,
            found,
            sink,
            ids: Arc::new(UlidIds::new()),
            sessions,
            processed: processed_tx,
            stop: Mutex::new(Some(stop_tx)),
            background: Mutex::new(background),
            client: Mutex::new(Some(client_task)),
        })
    }

    /// The harnesses found on this machine, in the order the hello reports them.
    pub fn harnesses(&self) -> Vec<String> {
        self.found.iter().map(|f| f.harness.to_owned()).collect()
    }

    /// Stores `dir` as trusted (what `sm trust` does).
    pub fn trust(&self, dir: &std::path::Path) -> Result<PathBuf> {
        let store = TrustStore::new(&self.state_dir, &self.home);
        let audit = Audit::new(&self.state_dir);
        trust::grant(&store, &audit, dir, "desktop").map_err(Into::into)
    }

    /// Starts a local session: its own agent process and store. It is not connected to the hub.
    pub async fn open_session(&self, spec: SessionSpec) -> Result<SessionInfo> {
        let agent = match &spec.harness {
            Some(name) => self.found.iter().find(|f| f.harness == name.as_str()),
            None => self.found.first(),
        }
        .cloned()
        .ok_or_else(|| match &spec.harness {
            Some(name) => anyhow!("{name} is not installed on this computer"),
            None => anyhow!("no coding agent found on this computer"),
        })?;
        let store = TrustStore::new(&self.state_dir, &self.home);
        let audit = Audit::new(&self.state_dir);
        if trust::decide(&store, &audit, &spec.cwd, None)? == Verdict::Refused {
            bail!("{} is not trusted", spec.cwd.display());
        }

        let thread_id = self.ids.next_id("th_");
        let title = spec.title.unwrap_or_else(|| session_name(&host_prefix()));
        let url = qr::session_url(&http_base(&self.hub)?, &thread_id)?;
        let sink = self.sink.clone();
        let registrar: Registrar = {
            let url = url.clone();
            Box::new(move |_handle| {
                let (sink, url) = (sink.clone(), url.clone());
                Box::pin(async move {
                    Ok(Registration {
                        upstream: Arc::new(TunnelUpstream(sink)),
                        url,
                    })
                })
            })
        };
        let (mut session, log_sink, commands) = Session::new(
            Identity {
                thread_id: thread_id.clone(),
                title: title.clone(),
                agent: agent.harness.to_owned(),
                workspace: Workspace {
                    path: spec.cwd.to_string_lossy().into_owned(),
                    branch: None,
                    spawn: None,
                },
            },
            Arc::new(now_ms),
            registrar,
        );
        let handle = session.handle();

        let mut config =
            BridgeConfig::new(self.machine_id.clone(), agent.harness, spec.cwd.clone());
        if self.permission_mode != PermissionMode::Default {
            config.initial_mode = Some(self.permission_mode.as_str().to_owned());
        }
        let (home, mcp_cwd) = (self.home.clone(), spec.cwd.clone());
        config.mcp_provider = Some(McpProvider::new(move |_thread, caps| {
            harness::local_mcp_servers(&home, &mcp_cwd, caps, &harness::mcp::process_env())
                .unwrap_or_else(|error| {
                    tracing::warn!("no local MCP servers for the session: {error:#}");
                    Vec::new()
                })
        }));
        let session_store = Arc::new(
            Store::open(&session_dir(&self.state_dir, &thread_id))
                .context("open the session store")?,
        );
        let (spawn_agent, spawn_cwd) = (agent.clone(), spec.cwd.clone());
        let ids: Arc<dyn IdGenerator> = self.ids.clone();
        let bridge = Bridge::spawn_with(
            config,
            move || tokio::process::Command::from(harness::spawn_command(&spawn_agent, &spawn_cwd)),
            session_store,
            ids,
            log_sink,
            commands,
        )
        .map_err(|error| anyhow!("could not start {}: {error}", agent.harness))?;
        let mut tasks = vec![tokio::spawn(async move {
            if let Err(error) = bridge.run().await {
                tracing::warn!("the session's bridge stopped: {error}");
            }
        })];
        // The acks of the hub commands the bridge has processed go to the tunnel.
        if let Some(mut processed) = handle.take_processed() {
            let up = self.processed.clone();
            tasks.push(tokio::spawn(async move {
                while let Some(item) = processed.recv().await {
                    if up.send(item).is_err() {
                        return;
                    }
                }
            }));
        }
        // Nothing here shows the log; without a reader it would only grow.
        let mut feed = session.take_feed();
        tasks.push(tokio::spawn(
            async move { while feed.recv().await.is_some() {} },
        ));

        let local = Arc::new(LocalSession {
            thread_id: thread_id.clone(),
            title,
            harness: agent.harness.to_owned(),
            cwd: spec.cwd,
            url,
            connected: AtomicBool::new(false),
            handle,
            session: tokio::sync::Mutex::new(session),
            tasks,
        });
        let info = info_of(&local);
        self.lock_sessions().insert(thread_id, local);
        Ok(info)
    }

    /// The sessions of this runner, oldest first by id.
    pub fn sessions(&self) -> Vec<SessionInfo> {
        let mut all: Vec<SessionInfo> = self.lock_sessions().values().map(|s| info_of(s)).collect();
        all.sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
        all
    }

    /// Makes the session known to the hub (and so to the phone). Returns its link. Connecting a
    /// connected session is a no-op.
    pub async fn connect(&self, thread_id: &str) -> Result<String> {
        let local = self.session(thread_id)?;
        let mut session = local.session.lock().await;
        if local.connected.load(Ordering::SeqCst) {
            return Ok(local.url.clone());
        }
        let url = session.register().await.map_err(|error| anyhow!(error))?;
        local.connected.store(true, Ordering::SeqCst);
        Ok(url)
    }

    /// Stops sending the session to the hub. The thread stays on the hub; it gets nothing new.
    pub async fn disconnect(&self, thread_id: &str) -> Result<()> {
        let local = self.session(thread_id)?;
        let mut session = local.session.lock().await;
        local.connected.store(false, Ordering::SeqCst);
        session.unregister().await;
        Ok(())
    }

    /// A prompt typed on this computer. Shared: it goes up and comes back in the hub's order.
    pub async fn prompt(&self, thread_id: &str, text: &str) -> Result<()> {
        let local = self.session(thread_id)?;
        let task_id = self.ids.next_id("tk_");
        let message_id = self.ids.next_id("msg_");
        local
            .session
            .lock()
            .await
            .prompt(task_id, message_id, text.to_owned())
            .await
            .map_err(|error| anyhow!(error))
    }

    /// Whether the tunnel task is still running (a revoked machine or a refused login stops it).
    pub fn is_running(&self) -> bool {
        self.client
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|task| !task.is_finished())
    }

    /// Ends the sessions and says goodbye to the hub.
    pub async fn stop(&self) {
        self.lock_sessions().clear();
        for task in self
            .background
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            task.abort();
        }
        let stop = self
            .stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let client = self
            .client
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(stop) = stop {
            let _ = stop.send(());
        }
        if let Some(client) = client {
            let _ = tokio::time::timeout(GOODBYE, client).await;
        }
    }

    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<LocalSession>>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn session(&self, thread_id: &str) -> Result<Arc<LocalSession>> {
        self.lock_sessions()
            .get(thread_id)
            .cloned()
            .ok_or_else(|| anyhow!("no session {thread_id} on this computer"))
    }
}

fn info_of(local: &LocalSession) -> SessionInfo {
    SessionInfo {
        thread_id: local.thread_id.clone(),
        title: local.title.clone(),
        harness: local.harness.clone(),
        cwd: local.cwd.clone(),
        connected: local.connected.load(Ordering::SeqCst),
        url: local.url.clone(),
    }
}

/// Hands each hub command to its session while the session is connected; refuses the rest
/// (`unsupported` for a dispatch, `unknown_thread` for a thread that is not connected) so the
/// hub is never left waiting. Acks what the sessions' bridges have processed.
async fn route(
    mut commands: TunnelCommands,
    sink: TunnelSink,
    sessions: Arc<Sessions>,
    mut processed: mpsc::UnboundedReceiver<(u64, String)>,
) {
    loop {
        tokio::select! {
            incoming = commands.next() => {
                let Some(incoming) = incoming else { return };
                let target = {
                    let sessions = sessions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    sessions
                        .get(command_thread(&incoming.command))
                        .filter(|s| s.connected.load(Ordering::SeqCst))
                        .map(|s| s.handle.clone())
                };
                if let Some(handle) = target {
                    handle.hub_command(incoming).await;
                    continue;
                }
                let (code, message) = match incoming.command {
                    protocol::tunnel::Command::Dispatch { .. } => (
                        CommandErrorCode::Unsupported,
                        "the desktop app does not start sessions from the phone",
                    ),
                    _ => (
                        CommandErrorCode::UnknownThread,
                        "this session is not connected to the phone",
                    ),
                };
                let failure = CommandFailure {
                    idempotency_key: incoming.idempotency_key.clone(),
                    code,
                    message: message.to_owned(),
                };
                let _ = sink.command_result(failure).await;
                commands.processed(incoming.seq, &incoming.idempotency_key).await;
            }
            Some((seq, key)) = processed.recv() => commands.processed(seq, &key).await,
        }
    }
}
