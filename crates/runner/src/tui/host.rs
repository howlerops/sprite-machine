//! `sm` / `sm --rc [name]`: the production wiring of the TUI. The terminal, the agent (through the
//! bridge, as `sm rc` runs it) and, on `/rc`, the tunnel to the hub. The decisions are in
//! [`super::session`] and [`super::app`]; this module only connects them to the machine.
//!
//! - The directory must be trusted ([`crate::trust`]); an untrusted one is asked about before the
//!   screen is taken over.
//! - The tunnel is started the first time the session is shared, with the machine's stored
//!   login. Its outbox lives in `<state dir>/tui`, apart from `sm rc`'s.
//! - Hub commands for any other thread are refused (`capacity`): this machine runs one session.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use protocol::tunnel::{AnswerStatus, Command, CommandErrorCode, Outcome, Workspace};
use protocol::turn_event::Envelope;
use sidecar::bridge::{
    Bridge, CommandFailure, CommandSource, Config, EventSink, IdGenerator, McpProvider, UlidIds,
};
use sidecar::store::Store;
use tokio::sync::{OnceCell, oneshot};

use super::app::App;
use super::driver;
use super::session::{
    Handle, HubAnswerFuture, HubVerdict, Identity, Processed, Registrar, Registration, Session,
    Upstream, UpstreamFuture,
};
use super::term::{self, TerminalGuard};
use crate::harness;
use crate::login::flow::normalize_hub;
use crate::login::{CredentialStore, KeychainCredentials};
use crate::qr;
use crate::server::{PermissionMode, host_prefix, session_dir, session_name};
use crate::settings::Settings;
use crate::trust::{self, Asker, Audit, TrustStore};
use crate::tunnel::{Timing, TunnelCommands, TunnelConfig, TunnelSink, tunnel};

/// How long an answer typed here waits for the hub's verdict.
const ANSWER_WAIT: Duration = Duration::from_secs(10);

/// How long the tunnel gets to say goodbye at exit.
const GOODBYE: Duration = Duration::from_secs(2);

/// Everything `sm` needs from its environment.
pub struct Options {
    pub credentials: CredentialStore,
    pub state_dir: PathBuf,
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub path_env: OsString,
    pub permission_mode: PermissionMode,
    /// The runner's settings (`autoConnect`).
    pub settings: Settings,
    /// `sm --rc`: register at start, whatever `autoConnect` says.
    pub rc: bool,
    /// `sm --rc <name>`: the session's name on the hub.
    pub name: Option<String>,
}

/// Runs the interactive session until the user quits.
pub async fn run(options: Options) -> Result<()> {
    let found = harness::discover(&options.path_env, &options.home);
    let Some(agent) = found.first().cloned() else {
        bail!(
            "no coding agent found on PATH: install one of {}",
            harness::HARNESSES
                .iter()
                .map(|harness| harness.name)
                .collect::<Vec<_>>()
                .join(", ")
        );
    };

    // The trust question, at the plain terminal, before the screen is taken over.
    let store = TrustStore::new(&options.state_dir, &options.home);
    let audit = Audit::new(&options.state_dir);
    let asker: Asker = Arc::new(|dir: &std::path::Path| {
        let mut input = std::io::stdin().lock();
        trust::ask_on(dir, &mut input, &mut std::io::stderr()).unwrap_or(false)
    });
    if trust::decide(&store, &audit, &options.cwd, Some(&asker))? == trust::Verdict::Refused {
        bail!(
            "{} is not trusted (run `sm trust {}`)",
            options.cwd.display(),
            options.cwd.display()
        );
    }

    let login = options.credentials.load()?;
    let ids = Arc::new(UlidIds::new());
    let thread_id = ids.next_id("th_");
    let title = options.name.unwrap_or_else(|| session_name(&host_prefix()));
    let workspace = Workspace {
        path: options.cwd.to_string_lossy().into_owned(),
        branch: None,
        spawn: None,
    };

    let link = Arc::new(Link {
        thread_id: thread_id.clone(),
        login: login
            .as_ref()
            .map(|login| (login.hub.clone(), login.machine_id.clone())),
        credentials: std::sync::Mutex::new(Some(options.credentials)),
        state_dir: options.state_dir.clone(),
        capabilities: harness::capabilities(&found, 1),
        started: OnceCell::new(),
    });
    let registrar: Registrar = {
        let link = Arc::clone(&link);
        Box::new(move |handle| {
            let link = Arc::clone(&link);
            Box::pin(async move { link.connect(handle).await })
        })
    };
    let (mut session, sink, commands) = Session::new(
        Identity {
            thread_id: thread_id.clone(),
            title,
            agent: agent.harness.to_owned(),
            workspace,
        },
        Arc::new(now_ms),
        registrar,
    );

    let machine_id = login
        .as_ref()
        .map_or_else(|| "local".to_owned(), |login| login.machine_id.clone());
    let mut config = Config::new(machine_id, agent.harness, options.cwd.clone());
    if options.permission_mode != PermissionMode::Default {
        config.initial_mode = Some(options.permission_mode.as_str().to_owned());
    }
    let (home, mcp_cwd) = (options.home.clone(), options.cwd.clone());
    config.mcp_provider =
        Some(McpProvider::new(
            move |_thread, caps| match harness::local_mcp_servers(
                &home,
                &mcp_cwd,
                caps,
                &harness::mcp::process_env(),
            ) {
                Ok(servers) => servers,
                Err(error) => {
                    tracing::warn!("no local MCP servers for the session: {error:#}");
                    Vec::new()
                }
            },
        ));
    let session_store = Arc::new(
        Store::open(&session_dir(&options.state_dir, &thread_id))
            .context("open the session store")?,
    );
    let (spawn_agent, spawn_cwd) = (agent.clone(), options.cwd.clone());
    let bridge = Bridge::spawn_with(
        config,
        move || tokio::process::Command::from(harness::spawn_command(&spawn_agent, &spawn_cwd)),
        session_store,
        ids,
        sink,
        commands,
    )
    .map_err(|error| anyhow!("could not start {}: {error}", agent.harness))?;
    let bridge_task = tokio::spawn(async move {
        if let Err(error) = bridge.run().await {
            tracing::warn!("the session's bridge stopped: {error}");
        }
    });

    let settings = Settings {
        auto_connect: options.rc || options.settings.auto_connect,
    };
    let seed = thread_id.trim_start_matches("th_").to_ascii_lowercase();
    let mut app = App::new(&thread_id, settings, &seed);
    let result = {
        let mut screen = TerminalGuard::enter().context("take over the terminal")?;
        let mut keys = term::read_keys();
        driver::run(&mut app, &mut session, screen.terminal(), &mut keys).await
    };
    bridge_task.abort();
    link.shutdown().await;
    result.context("the terminal failed")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The hub's HTTP base: the stored login may name the tunnel's `ws(s)` form.
fn http_base(hub: &str) -> Result<String> {
    let hub = hub.trim();
    let hub = if let Some(rest) = hub.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = hub.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        hub.to_owned()
    };
    normalize_hub(&hub)
}

/// The tunnel to the hub, started on the first `/rc`.
struct Link {
    thread_id: String,
    /// The stored login's hub and machine id; `None` when `sm login` was never run.
    login: Option<(String, String)>,
    credentials: std::sync::Mutex<Option<CredentialStore>>,
    state_dir: PathBuf,
    capabilities: protocol::tunnel::Capabilities,
    started: OnceCell<Started>,
}

struct Started {
    upstream: Arc<dyn Upstream>,
    url: String,
    stop: std::sync::Mutex<Option<oneshot::Sender<()>>>,
    client: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Link {
    async fn connect(&self, handle: Handle) -> Result<Registration, String> {
        let started = self
            .started
            .get_or_try_init(|| self.start(handle))
            .await
            .map_err(|error| format!("{error:#}"))?;
        Ok(Registration {
            upstream: Arc::clone(&started.upstream),
            url: started.url.clone(),
        })
    }

    async fn start(&self, handle: Handle) -> Result<Started> {
        let (hub, machine_id) = self
            .login
            .clone()
            .ok_or_else(|| anyhow!("not logged in: run `sm login --hub <url>`"))?;
        let url = qr::session_url(&http_base(&hub)?, &self.thread_id)?;
        let credentials = self
            .credentials
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| anyhow!("the login is already in use"))?;
        let store = Arc::new(Store::open(&self.state_dir.join("tui")).context("open the store")?);
        let (client, sink, commands) = tunnel(
            TunnelConfig {
                hub_ws_base: hub,
                machine_id,
                runner_version: env!("CARGO_PKG_VERSION").to_owned(),
                capabilities: self.capabilities.clone(),
                timing: Timing::default(),
            },
            store,
            KeychainCredentials::new(credentials),
        );
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let client_task = tokio::spawn(async move {
            let stopped = async {
                let _ = stop_rx.await;
            };
            if let Err(error) = client.run(stopped).await {
                tracing::warn!("the tunnel stopped: {error:#}");
            }
        });
        if let Some(processed) = handle.take_processed() {
            tokio::spawn(forward(
                commands,
                sink.clone(),
                processed,
                handle,
                self.thread_id.clone(),
            ));
        }
        Ok(Started {
            upstream: Arc::new(TunnelUpstream(sink)),
            url,
            stop: std::sync::Mutex::new(Some(stop_tx)),
            client: std::sync::Mutex::new(Some(client_task)),
        })
    }

    /// Says goodbye to the hub, if the tunnel was ever started.
    async fn shutdown(&self) {
        let Some(started) = self.started.get() else {
            return;
        };
        let stop = started
            .stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let client = started
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
}

/// The tunnel as the session's hub.
struct TunnelUpstream(TunnelSink);

impl Upstream for TunnelUpstream {
    fn open(
        &self,
        thread_id: &str,
        title: &str,
        agent: &str,
        workspace: &Workspace,
    ) -> Result<(), String> {
        self.0.mark_local_thread(thread_id);
        self.0
            .thread_open(thread_id, title, agent, workspace)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn send<'a>(&'a self, envelopes: Vec<Envelope>) -> UpstreamFuture<'a> {
        Box::pin(async move {
            self.0
                .send(envelopes)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn answer<'a>(
        &'a self,
        thread_id: &'a str,
        interrupt_id: &'a str,
        outcome: &'a Outcome,
    ) -> HubAnswerFuture<'a> {
        Box::pin(async move {
            self.0
                .interrupt_answer(thread_id, interrupt_id, outcome, ANSWER_WAIT)
                .await
                .map(Self::verdict)
        })
    }
}

impl TunnelUpstream {
    fn verdict(reply: crate::tunnel::AnswerReply) -> HubVerdict {
        match reply.status {
            AnswerStatus::Accepted => HubVerdict::Accepted,
            AnswerStatus::AlreadyResolved => HubVerdict::AlreadyAnswered {
                by: reply.resolved_by,
            },
            AnswerStatus::Rejected => HubVerdict::Rejected(reply.message),
        }
    }
}

/// Feeds the hub's commands for this thread to the session and acks what the bridge processed.
/// A command for another thread is refused: this machine runs one session here.
async fn forward(
    mut commands: TunnelCommands,
    sink: TunnelSink,
    mut processed: Processed,
    handle: Handle,
    thread_id: String,
) {
    loop {
        tokio::select! {
            incoming = commands.next() => {
                let Some(incoming) = incoming else { return };
                if command_thread(&incoming.command) == thread_id {
                    handle.hub_command(incoming).await;
                    continue;
                }
                let failure = CommandFailure {
                    idempotency_key: incoming.idempotency_key.clone(),
                    code: CommandErrorCode::Capacity,
                    message: "this machine is running an interactive session".to_owned(),
                };
                let _ = sink.command_result(failure).await;
                commands.processed(incoming.seq, &incoming.idempotency_key).await;
            }
            Some((seq, key)) = processed.recv() => commands.processed(seq, &key).await,
        }
    }
}

fn command_thread(command: &Command) -> &str {
    match command {
        Command::Prompt { thread_id, .. }
        | Command::Approval { thread_id, .. }
        | Command::Dispatch { thread_id, .. }
        | Command::Cancel { thread_id, .. }
        | Command::SessionConfig { thread_id, .. } => thread_id,
    }
}
