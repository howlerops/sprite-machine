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
//! `UserMessage` is the hub's (client-api.md §6). [`Config::emit_user_message`] makes the
//! bridge emit it for the tracer hub, which does not.
//!
//! Ordering. The ACP client delivers `session/update` on one channel and requests (permission,
//! prompt end, terminal tee) on another. The reader enqueues an update before it reads the next
//! line, so before the bridge acts on a request it drains the update channel first, and the
//! events keep the order the agent produced them in.

mod elicitation;
mod handler;
mod ids;
mod io;
mod permission;
mod runtime;
mod session_config;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use protocol::acp::{RequestPermissionResponse, SessionId, SessionNotification};
use protocol::mapping::acp_turn::AcpTurnMapper;
use protocol::tunnel::{Actor, ContentPart};
use protocol::turn_event::Envelope;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

use self::handler::{BridgeHandler, Internal};
use self::ids::SharedIds;
pub use self::ids::{IdGenerator, SequentialGenerator, UlidIds};
pub use self::io::{
    CommandFailure, CommandFeed, CommandSource, EventSink, IncomingCommand, MemoryCommands,
    MemorySink, SinkError, SinkItem,
};
use crate::acp::client::{AcpClient, ClientError};
use crate::acp::reattach::Reattacher;
use crate::store::{Store, StoreError};

/// Default approval TTL (PLAN-v2 §4.1): 10 minutes.
pub const DEFAULT_APPROVAL_TTL: Duration = Duration::from_secs(10 * 60);

/// What the bridge needs to know about its runner.
#[derive(Debug, Clone)]
pub struct Config {
    /// `machine_id` carried by the first `RunStarted` of a task.
    pub machine_id: String,
    /// `agent_id` of `RunStarted`; also the harness name stored with threads.
    pub agent_id: String,
    /// Working directory for threads the bridge learns of from a `prompt` (not `dispatch`).
    pub cwd: PathBuf,
    /// Emit `UserMessage` at the start of a task. `false` (the default): the hub does
    /// (client-api.md §6). The tracer adapter sets it.
    pub emit_user_message: bool,
    /// How long an interrupt waits for its answer.
    pub approval_ttl: Duration,
    /// How often overdue interrupts are looked for.
    pub expiry_check_interval: Duration,
    /// Most tasks (running or paused) at once; a `prompt` past it fails with `capacity`.
    pub max_running_tasks: usize,
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
            emit_user_message: false,
            approval_ttl: DEFAULT_APPROVAL_TTL,
            expiry_check_interval: Duration::from_secs(5),
            max_running_tasks: 32,
        }
    }
}

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
    updates: mpsc::UnboundedReceiver<SessionNotification>,
    internal: mpsc::UnboundedReceiver<Internal>,
    internal_tx: mpsc::UnboundedSender<Internal>,
    threads: HashMap<String, ThreadState>,
    /// ACP session id → thread id.
    by_session: HashMap<String, String>,
    approvals: HashMap<String, PendingApproval>,
}

/// The channels between the bridge loop and the ACP client's handler.
struct Wiring {
    handler: BridgeHandler,
    internal_tx: mpsc::UnboundedSender<Internal>,
    internal: mpsc::UnboundedReceiver<Internal>,
    updates_tx: mpsc::UnboundedSender<SessionNotification>,
    updates: mpsc::UnboundedReceiver<SessionNotification>,
}

impl Wiring {
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
    /// A bridge talking to an agent over `reader` and `writer` (a duplex pipe in tests).
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
        let client = AcpClient::connect(
            reader,
            writer,
            wiring.handler.clone(),
            wiring.updates_tx.clone(),
        );
        Self::assemble(config, store, ids, sink, commands, client, wiring)
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
        let client = AcpClient::spawn(command, wiring.handler.clone(), wiring.updates_tx.clone())?;
        Ok(Self::assemble(
            config, store, ids, sink, commands, client, wiring,
        ))
    }

    fn assemble(
        config: Config,
        store: Arc<Store>,
        ids: Arc<dyn IdGenerator>,
        sink: S,
        commands: C,
        client: AcpClient<BridgeHandler>,
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
            internal: wiring.internal,
            internal_tx: wiring.internal_tx,
            threads: HashMap::new(),
            by_session: HashMap::new(),
            approvals: HashMap::new(),
        }
    }
}
