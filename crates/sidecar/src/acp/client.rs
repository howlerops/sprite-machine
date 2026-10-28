//! The ACP client: JSON-RPC 2.0 over newline-delimited JSON to one agent connection.
//!
//! [`AcpClient`] talks to one agent, either a child process it spawns
//! ([`AcpClient::spawn`], killed on drop) or any reader and writer pair
//! ([`AcpClient::connect`]). It advertises the client capabilities the runner supports
//! ([`client_capabilities`]) and keeps the session table ([`super::sessions`]). Each
//! `session/update` goes to the channel the caller passes in. Requests the agent sends
//! (permissions, elicitation, `fs/*`, `terminal/*`) go to the caller's [`ClientHandler`].
//!
//! One prompt runs at a time per session. A prompt sent while one is in flight waits as a
//! pending task and runs after it. `session/cancel` ends the running prompt with
//! `cancelled`, and any permission or elicitation request the agent is waiting on is
//! answered with a cancel.
//!
//! The client also remembers what each session advertises (its modes and config options, from
//! the `session/new`, `session/load` or `session/resume` response and the later
//! `current_mode_update` / `config_option_update`), so the bridge can check a `session_config`
//! command before it reaches the agent ([`AcpClient::advertised`]).

use std::collections::HashMap;
use std::fmt::Display;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use protocol::acp::{
    CancelNotification, ClientCapabilities, ContentBlock, CreateElicitationRequest,
    CreateElicitationResponse, CreateTerminalRequest, CreateTerminalResponse, ElicitationAction,
    ElicitationCapabilities, ElicitationFormCapabilities, ElicitationMode, ElicitationScope,
    ElicitationUrlCapabilities, FileSystemCapabilities, Implementation, InitializeRequest,
    InitializeResponse, KillTerminalRequest, KillTerminalResponse, LoadSessionResponse, McpServer,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, ReadTextFileRequest,
    ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionResponse, SessionConfigId, SessionConfigOption, SessionConfigOptionValue,
    SessionId, SessionModeId, SessionModeState, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, SetSessionModeRequest,
    StopReason, TerminalOutputRequest, TerminalOutputResponse, WaitForTerminalExitRequest,
    WaitForTerminalExitResponse, WriteTextFileRequest, WriteTextFileResponse,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use super::sessions::{PendingTask, PromptResult, SessionTable, TaskId, lock};

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// Errors from the ACP client.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Reading from or writing to the agent failed, or the child could not be spawned.
    #[error("I/O error talking to the agent: {0}")]
    Io(#[from] std::io::Error),
    /// A message could not be encoded or decoded.
    #[error("malformed ACP message: {0}")]
    Json(#[from] serde_json::Error),
    /// The agent answered a request with a JSON-RPC error.
    #[error("agent error {code}: {message}")]
    Rpc {
        /// The JSON-RPC error code.
        code: i64,
        /// The JSON-RPC error message.
        message: String,
    },
    /// The connection closed before the request was answered.
    #[error("the agent connection is closed")]
    Closed,
    /// No session has this id.
    #[error("unknown session {0}")]
    UnknownSession(String),
    /// The pending task was removed before it ran.
    #[error("the pending task was removed before it ran")]
    Removed,
}

/// The client-side handler for requests the agent sends.
///
/// The client answers each request with what the handler returns. An `Err` becomes a
/// JSON-RPC internal error for the agent. A permission or elicitation request is answered
/// with a cancel, without asking the handler further, when the session is cancelled while
/// the handler is still working on it.
pub trait ClientHandler: Send + Sync + 'static {
    /// Answers `session/request_permission`.
    fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> impl Future<Output = anyhow::Result<RequestPermissionResponse>> + Send;

    /// Answers `elicitation/create`, in form or URL mode.
    fn create_elicitation(
        &self,
        request: CreateElicitationRequest,
    ) -> impl Future<Output = anyhow::Result<CreateElicitationResponse>> + Send;

    /// Answers `fs/read_text_file`.
    fn read_text_file(
        &self,
        request: ReadTextFileRequest,
    ) -> impl Future<Output = anyhow::Result<ReadTextFileResponse>> + Send;

    /// Answers `fs/write_text_file`.
    fn write_text_file(
        &self,
        request: WriteTextFileRequest,
    ) -> impl Future<Output = anyhow::Result<WriteTextFileResponse>> + Send;

    /// Answers `terminal/create`.
    fn create_terminal(
        &self,
        request: CreateTerminalRequest,
    ) -> impl Future<Output = anyhow::Result<CreateTerminalResponse>> + Send;

    /// Answers `terminal/output`.
    fn terminal_output(
        &self,
        request: TerminalOutputRequest,
    ) -> impl Future<Output = anyhow::Result<TerminalOutputResponse>> + Send;

    /// Answers `terminal/wait_for_exit`.
    fn wait_for_terminal_exit(
        &self,
        request: WaitForTerminalExitRequest,
    ) -> impl Future<Output = anyhow::Result<WaitForTerminalExitResponse>> + Send;

    /// Answers `terminal/kill`.
    fn kill_terminal(
        &self,
        request: KillTerminalRequest,
    ) -> impl Future<Output = anyhow::Result<KillTerminalResponse>> + Send;

    /// Answers `terminal/release`.
    fn release_terminal(
        &self,
        request: ReleaseTerminalRequest,
    ) -> impl Future<Output = anyhow::Result<ReleaseTerminalResponse>> + Send;
}

/// The client capabilities the runner advertises in `initialize`: text file read and
/// write, terminals, and elicitation in form and URL modes.
pub fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities::new()
        .fs(FileSystemCapabilities::new()
            .read_text_file(true)
            .write_text_file(true))
        .terminal(true)
        .elicitation(
            ElicitationCapabilities::new()
                .form(ElicitationFormCapabilities::new())
                .url(ElicitationUrlCapabilities::new()),
        )
}

/// The modes and config options a session advertises. `None` means the agent did not advertise
/// that part of the session state.
#[derive(Debug, Clone, Default)]
pub struct Advertised {
    /// The current mode and the modes the session can switch to.
    pub modes: Option<SessionModeState>,
    /// The session's config options, with their current values.
    pub config_options: Option<Vec<SessionConfigOption>>,
}

impl Advertised {
    fn new(
        modes: Option<SessionModeState>,
        config_options: Option<Vec<SessionConfigOption>>,
    ) -> Self {
        Self {
            modes,
            config_options,
        }
    }
}

/// The receiving end of `session/update` notifications is the caller's own channel.
pub type UpdateSender = mpsc::UnboundedSender<SessionNotification>;

/// Where `session/update` goes as raw JSON: the `params` object exactly as the agent sent it,
/// parsed or not. A bridge that must keep unknown fields (and updates this build cannot parse)
/// uses this instead of [`UpdateSender`].
pub type RawUpdateSender = mpsc::UnboundedSender<Value>;

/// Where the client sends `session/update`.
pub enum Updates {
    /// Parsed [`SessionNotification`]s; an update that does not parse is logged and dropped.
    Typed(UpdateSender),
    /// The raw `params`, whether or not they parse.
    Raw(RawUpdateSender),
}

impl From<UpdateSender> for Updates {
    fn from(sender: UpdateSender) -> Self {
        Self::Typed(sender)
    }
}

impl From<RawUpdateSender> for Updates {
    fn from(sender: RawUpdateSender) -> Self {
        Self::Raw(sender)
    }
}

/// The two ends of one agent connection, and the process behind it when there is one.
pub struct Transport {
    reader: Box<dyn AsyncRead + Send + Unpin>,
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    child: Option<Child>,
}

impl Transport {
    /// Messages from the agent are read from `reader`; requests to it are written to `writer`.
    pub fn new<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self {
            reader: Box::new(reader),
            writer: Box::new(writer),
            child: None,
        }
    }

    /// Spawns an agent process and talks to it over its stdin and stdout. The process is killed
    /// when the client made from this transport is dropped. The agent's stderr is left as the
    /// command sets it.
    pub fn spawn(mut command: Command) -> Result<Self, ClientError> {
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("agent stdin is not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("agent stdout is not piped"))?;
        Ok(Self {
            reader: Box::new(stdout),
            writer: Box::new(stdin),
            child: Some(child),
        })
    }
}

/// A prompt submitted with [`AcpClient::submit`].
pub struct Submitted {
    /// The task id, for [`AcpClient::remove_pending`].
    pub task: TaskId,
    /// `true` when the session was busy and the task waits as a pending task.
    pub queued: bool,
    /// Resolves with the stop reason, or an error if the task is removed or the
    /// connection closes first.
    pub done: oneshot::Receiver<PromptResult>,
}

/// One connection to an ACP agent.
///
/// Dropping the client closes the connection, fails the waiting prompts, and kills the
/// agent process when the client spawned it.
pub struct AcpClient<H: ClientHandler> {
    inner: Arc<Inner<H>>,
    reader: JoinHandle<()>,
    child: Option<Child>,
}

impl<H: ClientHandler> AcpClient<H> {
    /// Connects to an agent over any reader and writer. Messages from the agent are read
    /// from `reader` and requests to it are written to `writer`.
    pub fn connect<R, W>(reader: R, writer: W, handler: H, updates: UpdateSender) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self::from_transport(Transport::new(reader, writer), handler, updates)
    }

    /// Spawns an agent process and talks to it over its stdin and stdout. The process is
    /// killed when the client is dropped. The agent's stderr is left as the command sets it.
    pub fn spawn(command: Command, handler: H, updates: UpdateSender) -> Result<Self, ClientError> {
        Ok(Self::from_transport(
            Transport::spawn(command)?,
            handler,
            updates,
        ))
    }

    /// A client over `transport`, sending `session/update` to `updates` (typed or raw).
    pub fn from_transport(transport: Transport, handler: H, updates: impl Into<Updates>) -> Self {
        let Transport {
            reader,
            writer,
            child,
        } = transport;
        let inner = Arc::new(Inner {
            handler,
            writer: tokio::sync::Mutex::new(writer),
            calls: Mutex::new(Calls::default()),
            sessions: SessionTable::default(),
            advertised: Mutex::new(HashMap::new()),
            updates: updates.into(),
            closed: watch::channel(false).0,
        });
        let reader = tokio::spawn(read_loop(Arc::clone(&inner), reader));
        Self {
            inner,
            reader,
            child,
        }
    }

    /// `true` once the connection has ended (the agent exited or closed its output).
    pub fn is_closed(&self) -> bool {
        *self.inner.closed.borrow()
    }

    /// Resolves when the connection ends; at once if it already has.
    pub async fn closed(&self) {
        let mut signal = self.inner.closed.subscribe();
        // The sender lives in `inner`, so `wait_for` only ends with the value.
        let _ = signal.wait_for(|closed| *closed).await;
    }

    /// The process id of the spawned agent, or `None` for a connection made with
    /// [`AcpClient::connect`].
    pub fn child_id(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    /// Sends `initialize` with the capabilities from [`client_capabilities`].
    pub async fn initialize(&self) -> Result<InitializeResponse, ClientError> {
        // ACP v1. The version type is not re-exported by `protocol::acp`, so it is built
        // from its JSON form (`1`).
        let version = serde_json::from_value(json!(1))?;
        let request = InitializeRequest::new(version)
            .client_capabilities(client_capabilities())
            .client_info(Implementation::new("sidecar", env!("CARGO_PKG_VERSION")));
        let value = self
            .inner
            .call("initialize", serde_json::to_value(request)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Sends `session/new` and adds the session to the table.
    pub async fn new_session(
        &self,
        cwd: impl Into<PathBuf>,
        mcp_servers: Vec<McpServer>,
    ) -> Result<SessionId, ClientError> {
        Ok(self
            .new_session_response(cwd, mcp_servers)
            .await?
            .session_id)
    }

    /// [`AcpClient::new_session`], returning the whole response (modes and config options).
    pub async fn new_session_response(
        &self,
        cwd: impl Into<PathBuf>,
        mcp_servers: Vec<McpServer>,
    ) -> Result<NewSessionResponse, ClientError> {
        let request = NewSessionRequest::new(cwd).mcp_servers(mcp_servers);
        let value = self
            .inner
            .call("session/new", serde_json::to_value(request)?)
            .await?;
        let response: NewSessionResponse = serde_json::from_value(value)?;
        self.inner.sessions.insert(&response.session_id);
        self.inner.remember(
            &response.session_id,
            Advertised::new(response.modes.clone(), response.config_options.clone()),
        );
        Ok(response)
    }

    /// Sends `session/resume` for a session this agent process does not know yet (after a
    /// restart) and adds it to the table. Used by `acp::reattach`.
    pub async fn resume_session(
        &self,
        session: &SessionId,
        cwd: impl Into<PathBuf>,
    ) -> Result<(), ClientError> {
        self.resume_session_with(session, cwd, Vec::new())
            .await
            .map(|_| ())
    }

    /// [`AcpClient::resume_session`] with the thread's MCP servers, returning the response. A
    /// response that does not parse only means nothing is advertised until the agent says so in
    /// an update.
    pub async fn resume_session_with(
        &self,
        session: &SessionId,
        cwd: impl Into<PathBuf>,
        mcp_servers: Vec<McpServer>,
    ) -> Result<ResumeSessionResponse, ClientError> {
        let request =
            protocol::acp::ResumeSessionRequest::new(session.clone(), cwd).mcp_servers(mcp_servers);
        let value = self
            .inner
            .call("session/resume", serde_json::to_value(request)?)
            .await?;
        let response = serde_json::from_value::<ResumeSessionResponse>(value)
            .unwrap_or_else(|_| ResumeSessionResponse::new());
        self.inner.sessions.insert(session);
        self.inner.remember(
            session,
            Advertised::new(response.modes.clone(), response.config_options.clone()),
        );
        Ok(response)
    }

    /// Sends `session/load` (the fallback when `session/resume` is unsupported or fails) and
    /// adds the session to the table. Used by `acp::reattach`.
    pub async fn load_session(
        &self,
        session: &SessionId,
        cwd: impl Into<PathBuf>,
    ) -> Result<(), ClientError> {
        self.load_session_with(session, cwd, Vec::new())
            .await
            .map(|_| ())
    }

    /// [`AcpClient::load_session`] with the thread's MCP servers, returning the response.
    pub async fn load_session_with(
        &self,
        session: &SessionId,
        cwd: impl Into<PathBuf>,
        mcp_servers: Vec<McpServer>,
    ) -> Result<LoadSessionResponse, ClientError> {
        let request =
            protocol::acp::LoadSessionRequest::new(session.clone(), cwd).mcp_servers(mcp_servers);
        let value = self
            .inner
            .call("session/load", serde_json::to_value(request)?)
            .await?;
        let response = serde_json::from_value::<LoadSessionResponse>(value)
            .unwrap_or_else(|_| LoadSessionResponse::new());
        self.inner.sessions.insert(session);
        self.inner.remember(
            session,
            Advertised::new(response.modes.clone(), response.config_options.clone()),
        );
        Ok(response)
    }

    /// What `session` advertises: its modes and config options as the agent last reported
    /// them. `None` for a session this client does not know.
    pub fn advertised(&self, session: &SessionId) -> Option<Advertised> {
        lock(&self.inner.advertised)
            .get(&session.0.to_string())
            .cloned()
    }

    /// Sends `session/set_mode`. The agent reports the change as a `current_mode_update`.
    pub async fn set_mode(
        &self,
        session: &SessionId,
        mode: SessionModeId,
    ) -> Result<(), ClientError> {
        let request = SetSessionModeRequest::new(session.clone(), mode);
        self.inner
            .call("session/set_mode", serde_json::to_value(request)?)
            .await?;
        Ok(())
    }

    /// Sends `session/set_config_option`. The agent answers with the session's full config
    /// options and reports the change as a `config_option_update`.
    pub async fn set_config_option(
        &self,
        session: &SessionId,
        config: SessionConfigId,
        value: SessionConfigOptionValue,
    ) -> Result<(), ClientError> {
        let request = SetSessionConfigOptionRequest::new(session.clone(), config, value);
        let answer = self
            .inner
            .call("session/set_config_option", serde_json::to_value(request)?)
            .await?;
        if let Ok(SetSessionConfigOptionResponse { config_options, .. }) =
            serde_json::from_value(answer)
        {
            self.inner.update_config_options(session, config_options);
        }
        Ok(())
    }

    /// Queues a prompt for `session`. It runs at once when the session is idle, and after
    /// the prompt in flight otherwise.
    pub fn submit(
        &self,
        session: &SessionId,
        prompt: Vec<ContentBlock>,
    ) -> Result<Submitted, ClientError> {
        let enqueued = self.inner.sessions.enqueue(session, prompt)?;
        if enqueued.start_driver {
            tokio::spawn(drive(Arc::clone(&self.inner), session.clone()));
        }
        Ok(Submitted {
            task: enqueued.task,
            queued: enqueued.queued,
            done: enqueued.done,
        })
    }

    /// Sends a prompt and waits for its stop reason. Updates arrive on the update channel
    /// as they stream.
    pub async fn prompt(
        &self,
        session: &SessionId,
        prompt: Vec<ContentBlock>,
    ) -> Result<StopReason, ClientError> {
        let submitted = self.submit(session, prompt)?;
        match submitted.done.await {
            Ok(result) => result,
            Err(_) => Err(ClientError::Closed),
        }
    }

    /// The pending tasks of `session` in the order they will run.
    pub fn pending_tasks(&self, session: &SessionId) -> Result<Vec<PendingTask>, ClientError> {
        self.inner.sessions.pending(session)
    }

    /// Removes a pending task. Its `done` resolves with `ClientError::Removed`. Returns the
    /// removed task, or `None` when no pending task has that id.
    pub fn remove_pending(
        &self,
        session: &SessionId,
        task: TaskId,
    ) -> Result<Option<PendingTask>, ClientError> {
        self.inner.sessions.remove_pending(session, task)
    }

    /// The task whose prompt is in flight in `session`, if any.
    pub fn running_task(&self, session: &SessionId) -> Result<Option<TaskId>, ClientError> {
        self.inner.sessions.running(session)
    }

    /// Cancels `session`. Sends `session/cancel` when a prompt is in flight, and answers
    /// the agent's waiting permission and elicitation requests with a cancel. Pending
    /// tasks are not removed, so they run after the cancelled prompt.
    pub async fn cancel(&self, session: &SessionId) -> Result<(), ClientError> {
        let running = self.inner.sessions.request_cancel(session)?;
        if !running {
            return Ok(());
        }
        let notification = serde_json::to_value(CancelNotification::new(session.clone()))?;
        self.inner.notify("session/cancel", notification).await
    }
}

impl<H: ClientHandler> Drop for AcpClient<H> {
    fn drop(&mut self) {
        self.inner.close();
        self.reader.abort();
        // `child` is dropped after this, and `kill_on_drop` kills and reaps the process.
    }
}

/// State shared by the client, its reader task and its driver tasks.
struct Inner<H> {
    handler: H,
    writer: tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    calls: Mutex<Calls>,
    sessions: SessionTable,
    /// What each session advertises, by session id (see [`AcpClient::advertised`]).
    advertised: Mutex<HashMap<String, Advertised>>,
    updates: Updates,
    /// `true` once the connection has ended.
    closed: watch::Sender<bool>,
}

impl<H> Inner<H> {
    /// Records what a session advertised in its session/new, load or resume response.
    fn remember(&self, session: &SessionId, advertised: Advertised) {
        lock(&self.advertised).insert(session.0.to_string(), advertised);
    }

    /// Replaces the config options a session advertises.
    fn update_config_options(&self, session: &SessionId, options: Vec<SessionConfigOption>) {
        let mut table = lock(&self.advertised);
        table
            .entry(session.0.to_string())
            .or_default()
            .config_options = Some(options);
    }

    /// Keeps what a session advertises current with the agent's own updates. Runs before the
    /// update is forwarded, so the bridge sees the state the update left.
    fn observe(&self, update: &SessionNotification) {
        let mut table = lock(&self.advertised);
        let Some(advertised) = table.get_mut(&update.session_id.0.to_string()) else {
            return;
        };
        match &update.update {
            SessionUpdate::CurrentModeUpdate(mode) => {
                if let Some(modes) = advertised.modes.as_mut() {
                    modes.current_mode_id = mode.current_mode_id.clone();
                }
            }
            SessionUpdate::ConfigOptionUpdate(options) => {
                advertised.config_options = Some(options.config_options.clone());
            }
            _ => {}
        }
    }
}

/// Requests we sent that are waiting for an answer.
#[derive(Default)]
struct Calls {
    next_id: u64,
    waiting: HashMap<u64, oneshot::Sender<Result<Value, ClientError>>>,
    closed: bool,
}

/// A JSON-RPC error to send back to the agent.
struct Failure {
    code: i64,
    message: String,
}

impl Failure {
    fn new(code: i64, message: impl Display) -> Self {
        Self {
            code,
            message: message.to_string(),
        }
    }
}

impl<H: ClientHandler> Inner<H> {
    async fn send(&self, message: &Value) -> Result<(), ClientError> {
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(b'\n');
        let mut writer = self.writer.lock().await;
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), ClientError> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    /// Sends a request and waits for its response.
    async fn call(&self, method: &str, params: Value) -> Result<Value, ClientError> {
        let (id, reply) = {
            let mut calls = lock(&self.calls);
            if calls.closed {
                return Err(ClientError::Closed);
            }
            calls.next_id += 1;
            let id = calls.next_id;
            let (reply, waiter) = oneshot::channel();
            calls.waiting.insert(id, reply);
            (id, waiter)
        };
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(error) = self.send(&request).await {
            lock(&self.calls).waiting.remove(&id);
            return Err(error);
        }
        match reply.await {
            Ok(result) => result,
            Err(_) => Err(ClientError::Closed),
        }
    }

    /// Stops accepting calls, fails the waiting ones and the queued prompts.
    fn close(&self) {
        let waiting = {
            let mut calls = lock(&self.calls);
            calls.closed = true;
            std::mem::take(&mut calls.waiting)
        };
        // Dropping each sender wakes its caller with `ClientError::Closed`.
        drop(waiting);
        self.sessions.fail_queued();
        self.closed.send_replace(true);
    }

    /// Runs one prompt on the wire and returns its stop reason.
    async fn run_prompt(
        &self,
        session: &SessionId,
        prompt: Vec<ContentBlock>,
    ) -> Result<StopReason, ClientError> {
        let request = serde_json::to_value(PromptRequest::new(session.clone(), prompt))?;
        let value = self.call("session/prompt", request).await?;
        let response: PromptResponse = serde_json::from_value(value)?;
        Ok(response.stop_reason)
    }

    /// Answers a request the agent sent us.
    async fn route(&self, method: &str, params: Value) -> Result<Value, Failure> {
        match method {
            "session/request_permission" => {
                let request: RequestPermissionRequest = parse(params)?;
                let cancel = self.session_cancel(&request.session_id)?;
                let answer = tokio::select! {
                    answer = self.handler.request_permission(request) => answer,
                    () = cancelled(cancel) => Ok(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    )),
                };
                reply(answer)
            }
            "elicitation/create" => {
                let request: CreateElicitationRequest = parse(params)?;
                let cancel = self.elicitation_cancel(&request)?;
                let answer = tokio::select! {
                    answer = self.handler.create_elicitation(request) => answer,
                    () = cancelled(cancel) => Ok(CreateElicitationResponse::new(
                        ElicitationAction::Cancel,
                    )),
                };
                reply(answer)
            }
            "fs/read_text_file" => {
                let request = parse(params)?;
                reply(self.handler.read_text_file(request).await)
            }
            "fs/write_text_file" => {
                let request = parse(params)?;
                reply(self.handler.write_text_file(request).await)
            }
            "terminal/create" => {
                let request = parse(params)?;
                reply(self.handler.create_terminal(request).await)
            }
            "terminal/output" => {
                let request = parse(params)?;
                reply(self.handler.terminal_output(request).await)
            }
            "terminal/wait_for_exit" => {
                let request = parse(params)?;
                reply(self.handler.wait_for_terminal_exit(request).await)
            }
            "terminal/kill" => {
                let request = parse(params)?;
                reply(self.handler.kill_terminal(request).await)
            }
            "terminal/release" => {
                let request = parse(params)?;
                reply(self.handler.release_terminal(request).await)
            }
            other => Err(Failure::new(
                METHOD_NOT_FOUND,
                format!("method not found: {other}"),
            )),
        }
    }

    fn session_cancel(&self, session: &SessionId) -> Result<watch::Receiver<bool>, Failure> {
        self.sessions
            .cancel_signal(session)
            .ok_or_else(|| Failure::new(INVALID_PARAMS, format!("unknown session {}", session.0)))
    }

    /// The cancel signal for an elicitation. A session-scoped one follows its session. A
    /// request-scoped one belongs to no session, so `session/cancel` never answers it.
    fn elicitation_cancel(
        &self,
        request: &CreateElicitationRequest,
    ) -> Result<watch::Receiver<bool>, Failure> {
        let scope = match &request.mode {
            ElicitationMode::Form(form) => Some(&form.scope),
            ElicitationMode::Url(url) => Some(&url.scope),
            _ => None,
        };
        match scope {
            Some(ElicitationScope::Session(scope)) => self.session_cancel(&scope.session_id),
            _ => Ok(watch::channel(false).1),
        }
    }
}

/// Runs the queued prompts of one session, one at a time, until its queue is empty.
async fn drive<H: ClientHandler>(inner: Arc<Inner<H>>, session: SessionId) {
    while let Some(started) = inner.sessions.take_next(&session) {
        let result = inner.run_prompt(&session, started.prompt).await;
        inner.sessions.finish(&session);
        // The caller may have stopped waiting for this prompt. Then nobody reads the result.
        let _ = started.done.send(result);
    }
}

/// Reads the agent's messages until the connection ends, then closes the client.
async fn read_loop<H: ClientHandler>(
    inner: Arc<Inner<H>>,
    reader: Box<dyn AsyncRead + Send + Unpin>,
) {
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(message) => dispatch(&inner, message),
                    // An agent that writes logs to stdout is not fatal; the line is skipped.
                    Err(error) => tracing::warn!(%error, "skipping an agent line that is not JSON"),
                }
            }
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "reading from the agent failed");
                break;
            }
        }
    }
    inner.close();
}

/// Routes one message from the agent: a request to answer, a notification, or a response
/// to one of our calls.
fn dispatch<H: ClientHandler>(inner: &Arc<Inner<H>>, message: Value) {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match (method, message.get("id").cloned()) {
        (Some(method), Some(id)) => {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let inner = Arc::clone(inner);
            tokio::spawn(async move { answer(&inner, id, &method, params).await });
        }
        (Some(method), None) => notification(inner, &method, message.get("params")),
        (None, Some(id)) => response(inner, &id, &message),
        (None, None) => tracing::warn!("ignoring an agent message with neither method nor id"),
    }
}

fn notification<H: ClientHandler>(inner: &Arc<Inner<H>>, method: &str, params: Option<&Value>) {
    if method != "session/update" {
        tracing::debug!(method, "ignoring an agent notification");
        return;
    }
    let Some(params) = params else {
        tracing::warn!("session/update without params");
        return;
    };
    let parsed = serde_json::from_value::<SessionNotification>(params.clone());
    if let Ok(update) = &parsed {
        inner.observe(update);
    }
    match (&inner.updates, parsed) {
        (Updates::Typed(sender), Ok(update)) => {
            if sender.send(update).is_err() {
                tracing::warn!("no receiver for session updates; dropping one");
            }
        }
        (Updates::Typed(_), Err(error)) => tracing::warn!(%error, "session/update does not parse"),
        (Updates::Raw(sender), _) => {
            if sender.send(params.clone()).is_err() {
                tracing::warn!("no receiver for session updates; dropping one");
            }
        }
    }
}

fn response<H: ClientHandler>(inner: &Arc<Inner<H>>, id: &Value, message: &Value) {
    let Some(id) = id.as_u64() else {
        tracing::warn!("response with an unknown id {id}");
        return;
    };
    let result = match message.get("error") {
        Some(error) => Err(ClientError::Rpc {
            code: error
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or(INTERNAL_ERROR),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
    };
    let waiter = lock(&inner.calls).waiting.remove(&id);
    match waiter {
        // The caller may have stopped waiting. Then nobody reads the result.
        Some(waiter) => {
            let _ = waiter.send(result);
        }
        None => tracing::warn!(id, "response to a request that is not waiting"),
    }
}

/// Answers one request from the agent and writes the response.
async fn answer<H: ClientHandler>(inner: &Inner<H>, id: Value, method: &str, params: Value) {
    let message = match inner.route(method, params).await {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(failure) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": failure.code, "message": failure.message },
        }),
    };
    if let Err(error) = inner.send(&message).await {
        tracing::warn!(%error, method, "could not answer a request from the agent");
    }
}

/// Waits until the session is cancelled. A closed signal never fires, so the request
/// keeps waiting on its handler.
async fn cancelled(mut signal: watch::Receiver<bool>) {
    let closed = signal.wait_for(|cancelled| *cancelled).await.is_err();
    if closed {
        std::future::pending::<()>().await;
    }
}

/// Serializes a handler's answer, mapping a handler error to an internal error.
fn reply<R: Serialize>(answer: anyhow::Result<R>) -> Result<Value, Failure> {
    let value = answer.map_err(|error| Failure::new(INTERNAL_ERROR, format!("{error:#}")))?;
    serde_json::to_value(value).map_err(|error| Failure::new(INTERNAL_ERROR, error))
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, Failure> {
    serde_json::from_value(params).map_err(|error| Failure::new(INVALID_PARAMS, error))
}
