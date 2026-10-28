//! The runner bridge: ACP client ⇄ TurnEvents ⇄ tunnel commands (PLAN-v2 §4.1).
//!
//! [`Bridge`] owns one agent connection ([`AcpClient`]) and turns hub commands into ACP calls
//! and ACP traffic into producer-side TurnEvent envelopes (`seq: 0`):
//!
//! - **Commands** come from a [`CommandSource`]: `prompt` and `dispatch` start a task,
//!   `approval` answers an open interrupt, `cancel` ends the running task. Each command is
//!   applied once per idempotency key (the key and the down-stream seq are committed to the
//!   [`Store`] first). A command that cannot be applied is reported with
//!   [`EventSink::command_result`] (`capacity`, `unknown_thread`, `unknown_interrupt`,
//!   `unsupported`, `invalid`).
//! - **Events** go to an [`EventSink`]. [`AcpTurnMapper`] (one per thread) produces them: the
//!   bridge feeds it `session/update` (as JSON), the permission request, the prompt response,
//!   resume, cancel, expiry and errors, and sends what it returns, in order.
//! - **One prompt per session.** A thread with a running task refuses another `prompt`
//!   (`invalid`): the hub owns the pending-task queue (tunnel.md §3.5), so the ACP client's own
//!   queue is never used.
//! - **Interrupts.** `session/request_permission` becomes `Interrupt{approval}` +
//!   `RunFinished{interrupt}`, is stored with its deadline, and the agent's request waits. The
//!   hub has already emitted `InterruptResolved` when `approval` arrives (client-api.md §6), so
//!   the bridge answers ACP and emits the resume run. A timer ends overdue interrupts with
//!   `InterruptResolved{expired}` + `RunError{expired}`.
//! - **Client side of ACP.** `fs/*` is served by [`Workspace`](crate::acp::fs::Workspace) and
//!   `terminal/*` by [`Terminals`](crate::acp::terminal::Terminals); the terminals' Activity
//!   envelopes go to the sink inside the run they belong to.
//! - **Durability.** Threads, tasks, sessions and open interrupts are stored; a known thread's
//!   session is re-attached (`session/resume`, then `session/load`) before its next prompt.
//!
//! `UserMessage` is the hub's (client-api.md §6): a `prompt` or `dispatch` command starts with
//! `RunStarted{prompt}`. The bridge emits it only for a prompt typed locally (a command whose
//! idempotency key starts with [`LOCAL_KEY_PREFIX`]), which the hub never saw.
//!
//! **Harness restart.** When the agent process exits, every running task ends with
//! `InterruptResolved{cancelled}` for each open interrupt and then `RunError{error,
//! code:"harness_exited"}` (SPEC-004). [`Reattacher::connection_lost`] marks the stored sessions
//! detached, and the next `prompt` respawns the agent (when the bridge was built with
//! [`Bridge::connect_with`] or [`Bridge::spawn_with`]) and resumes the thread's session. At
//! startup the interrupts an earlier process left open end the same way (see `stale`).
//!
//! **Raw updates.** The ACP client hands `session/update` over as the JSON the agent sent, so
//! `raw.payload` keeps unknown fields and an update this build cannot parse becomes a `Raw`
//! event.
//!
//! Ordering. The ACP client delivers `session/update` on one channel and requests (permission,
//! prompt end, terminal tee) on another. The reader enqueues an update before it reads the next
//! line, so before the bridge acts on a request it drains the update channel first, and the
//! events keep the order the agent produced them in.

mod applied;
mod elicitation;
pub mod files;
mod handler;
mod ids;
mod io;
mod permission;
mod runtime;
mod session_config;
mod stale;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use protocol::acp::{McpCapabilities, McpServer, RequestPermissionResponse, SessionId};
use protocol::mapping::acp_turn::AcpTurnMapper;
use protocol::tunnel::{Actor, ContentPart};
use protocol::turn_event::Envelope;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

use self::applied::AppliedKeys;
use self::handler::{BridgeHandler, Internal};
use self::ids::SharedIds;
pub use self::ids::{IdGenerator, SequentialGenerator, UlidIds};
pub use self::io::{
    CommandFailure, CommandFeed, CommandSource, EventSink, IncomingCommand, MemoryCommands,
    MemorySink, SinkError, SinkItem,
};
use crate::acp::client::{AcpClient, ClientError, Transport};
use crate::acp::reattach::Reattacher;
use crate::store::{Store, StoreError};

/// Idempotency keys of prompts typed on the runner itself start with this. The hub never saw
/// such a prompt, so the bridge emits its `UserMessage` (client-api.md §6). A key from the
/// hub must not start with it.
pub const LOCAL_KEY_PREFIX: &str = "local:";

/// Default approval TTL (PLAN-v2 §4.1): 10 minutes.
pub const DEFAULT_APPROVAL_TTL: Duration = Duration::from_secs(10 * 60);

type McpFn = dyn Fn(&str, &McpCapabilities) -> Vec<McpServer> + Send + Sync;

/// Builds the MCP servers of a thread once the agent's `mcp_capabilities` (from its `initialize`
/// response) are known: `(thread id, capabilities) -> servers`.
#[derive(Clone)]
pub struct McpProvider(Arc<McpFn>);

impl McpProvider {
    pub fn new(
        provider: impl Fn(&str, &McpCapabilities) -> Vec<McpServer> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(provider))
    }
}

impl std::fmt::Debug for McpProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("McpProvider")
    }
}

/// What the bridge needs to know about its runner.
#[derive(Debug, Clone)]
pub struct Config {
    /// `machine_id` carried by the first `RunStarted` of a task.
    pub machine_id: String,
    /// `agent_id` of `RunStarted`; also the harness name stored with threads.
    pub agent_id: String,
    /// Working directory for threads the bridge learns of from a `prompt` (not `dispatch`).
    pub cwd: PathBuf,
    /// How long an interrupt waits for its answer.
    pub approval_ttl: Duration,
    /// How often overdue interrupts are looked for.
    pub expiry_check_interval: Duration,
    /// Most tasks (running or paused) at once; a `prompt` past it fails with `capacity`.
    pub max_running_tasks: usize,
    /// MCP servers passed to `session/new`, `session/resume` and `session/load` for every
    /// thread without an entry in [`Config::thread_mcp_servers`].
    pub mcp_servers: Vec<McpServer>,
    /// MCP servers per thread id; replaces [`Config::mcp_servers`] for that thread.
    pub thread_mcp_servers: HashMap<String, Vec<McpServer>>,
    /// Builds the MCP servers from the agent's advertised `mcp_capabilities`; replaces
    /// [`Config::mcp_servers`] and [`Config::thread_mcp_servers`] when set.
    pub mcp_provider: Option<McpProvider>,
    /// The ACP session mode every session is put in right after `session/new`, `session/resume`
    /// or `session/load` and before its first prompt. A mode the agent does not advertise, or
    /// that it refuses, fails the task (`RunError`) instead of running in another mode.
    pub initial_mode: Option<String>,
}

impl Config {
    pub fn new(
        machine_id: impl Into<String>,
        agent_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            machine_id: machine_id.into(),
            agent_id: agent_id.into(),
            cwd: cwd.into(),
            approval_ttl: DEFAULT_APPROVAL_TTL,
            expiry_check_interval: Duration::from_secs(5),
            max_running_tasks: 32,
            mcp_servers: Vec::new(),
            thread_mcp_servers: HashMap::new(),
            mcp_provider: None,
            initial_mode: None,
        }
    }

    /// The MCP servers a session of `thread_id` is given.
    pub fn mcp_servers_for(&self, thread_id: &str) -> Vec<McpServer> {
        self.thread_mcp_servers
            .get(thread_id)
            .unwrap_or(&self.mcp_servers)
            .clone()
    }
}

/// Opens a new connection to the agent: called again whenever the agent process has exited and
/// a prompt needs it.
pub type Connector = Box<dyn FnMut() -> Result<Transport, ClientError> + Send>;

/// Why the bridge stopped.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("ACP: {0}")]
    Client(#[from] ClientError),
    #[error(transparent)]
    Sink(#[from] SinkError),
}

/// A task that is running or paused on an interrupt.
struct ActiveTask {
    task_id: String,
    /// Terminal output teed while the run was paused; sent after the resume `RunStarted`.
    held: Vec<Envelope>,
}

struct ThreadState {
    harness: String,
    cwd: PathBuf,
    mapper: AcpTurnMapper<SharedIds>,
    session: Option<SessionId>,
    task: Option<ActiveTask>,
}

/// An approval the agent waits for.
struct PendingApproval {
    thread_id: String,
    task_id: String,
    reply: oneshot::Sender<RequestPermissionResponse>,
    expires_at_ms: u64,
}

/// A `prompt` or `dispatch`, normalised.
struct Start {
    thread_id: String,
    task_id: String,
    message_id: String,
    parts: Vec<ContentPart>,
    author: Actor,
    /// A prompt typed locally: the bridge emits its `UserMessage`.
    local: bool,
    /// `dispatch` names the harness and the directory; `prompt` uses the thread's.
    harness: Option<String>,
    cwd: Option<PathBuf>,
}

/// The runner bridge. See the module docs.
pub struct Bridge<S: EventSink, C: CommandSource> {
    config: Config,
    store: Arc<Store>,
    ids: Arc<dyn IdGenerator>,
    sink: S,
    commands: C,
    client: AcpClient<BridgeHandler>,
    handler: BridgeHandler,
    reattacher: Reattacher,
    /// `session/update` params as the agent sent them.
    updates: mpsc::UnboundedReceiver<Value>,
    updates_tx: mpsc::UnboundedSender<Value>,
    internal: mpsc::UnboundedReceiver<Internal>,
    internal_tx: mpsc::UnboundedSender<Internal>,
    /// Starts the agent again; `None` for a bridge over a fixed connection.
    connector: Option<std::sync::Mutex<Connector>>,
    /// The agent process exited and has not been started again.
    harness_down: bool,
    /// What the agent's last `initialize` advertised for MCP.
    agent_mcp: McpCapabilities,
    /// ACP sessions already put in [`Config::initial_mode`] on this connection.
    moded: std::collections::HashSet<String>,
    threads: HashMap<String, ThreadState>,
    /// ACP session id → thread id.
    by_session: HashMap<String, String>,
    approvals: HashMap<String, PendingApproval>,
    /// Keys applied by this process and not yet recorded by the store (see [`AppliedKeys`]).
    applied: AppliedKeys,
}

/// The channels between the bridge loop and the ACP client's handler.
struct Wiring {
    handler: BridgeHandler,
    internal_tx: mpsc::UnboundedSender<Internal>,
    internal: mpsc::UnboundedReceiver<Internal>,
    updates_tx: mpsc::UnboundedSender<Value>,
    updates: mpsc::UnboundedReceiver<Value>,
}

impl Wiring {
    /// A client over `transport` that reports to this wiring.
    fn client(&self, transport: Transport) -> AcpClient<BridgeHandler> {
        AcpClient::from_transport(transport, self.handler.clone(), self.updates_tx.clone())
    }

    fn new() -> Self {
        let (internal_tx, internal) = mpsc::unbounded_channel();
        let (updates_tx, updates) = mpsc::unbounded_channel();
        Self {
            handler: BridgeHandler::new(internal_tx.clone()),
            internal_tx,
            internal,
            updates_tx,
            updates,
        }
    }
}

impl<S: EventSink, C: CommandSource> Bridge<S, C> {
    /// A bridge talking to an agent over `reader` and `writer` (a duplex pipe in tests). The
    /// connection cannot be replaced: when the agent exits, prompts fail with `harness_exited`.
    pub fn connect<R, W>(
        config: Config,
        reader: R,
        writer: W,
        store: Arc<Store>,
        ids: Arc<dyn IdGenerator>,
        sink: S,
        commands: C,
    ) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let wiring = Wiring::new();
        let client = wiring.client(Transport::new(reader, writer));
        Self::assemble(config, store, ids, sink, commands, client, None, wiring)
    }

    /// A bridge that starts the agent with `connector` now and again each time a prompt finds
    /// the agent gone.
    pub fn connect_with(
        config: Config,
        mut connector: Connector,
        store: Arc<Store>,
        ids: Arc<dyn IdGenerator>,
        sink: S,
        commands: C,
    ) -> Result<Self, ClientError> {
        let wiring = Wiring::new();
        let client = wiring.client(connector()?);
        Ok(Self::assemble(
            config,
            store,
            ids,
            sink,
            commands,
            client,
            Some(std::sync::Mutex::new(connector)),
            wiring,
        ))
    }

    /// A bridge that spawns the agent as a child process (killed when the bridge is dropped).
    pub fn spawn(
        config: Config,
        command: tokio::process::Command,
        store: Arc<Store>,
        ids: Arc<dyn IdGenerator>,
        sink: S,
        commands: C,
    ) -> Result<Self, ClientError> {
        let wiring = Wiring::new();
        let client = wiring.client(Transport::spawn(command)?);
        Ok(Self::assemble(
            config, store, ids, sink, commands, client, None, wiring,
        ))
    }

    /// [`Bridge::spawn`] that spawns the agent again (with a command from `command`) when it
    /// has exited and a prompt needs it.
    pub fn spawn_with(
        config: Config,
        mut command: impl FnMut() -> tokio::process::Command + Send + 'static,
        store: Arc<Store>,
        ids: Arc<dyn IdGenerator>,
        sink: S,
        commands: C,
    ) -> Result<Self, ClientError> {
        Self::connect_with(
            config,
            Box::new(move || Transport::spawn(command())),
            store,
            ids,
            sink,
            commands,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        config: Config,
        store: Arc<Store>,
        ids: Arc<dyn IdGenerator>,
        sink: S,
        commands: C,
        client: AcpClient<BridgeHandler>,
        connector: Option<std::sync::Mutex<Connector>>,
        wiring: Wiring,
    ) -> Self {
        let reattacher = Reattacher::new(Arc::clone(&store));
        Self {
            config,
            store,
            ids,
            sink,
            commands,
            client,
            handler: wiring.handler,
            reattacher,
            updates: wiring.updates,
            updates_tx: wiring.updates_tx,
            internal: wiring.internal,
            internal_tx: wiring.internal_tx,
            connector,
            harness_down: false,
            agent_mcp: McpCapabilities::default(),
            moded: std::collections::HashSet::new(),
            threads: HashMap::new(),
            by_session: HashMap::new(),
            approvals: HashMap::new(),
            applied: AppliedKeys::default(),
        }
    }
}
