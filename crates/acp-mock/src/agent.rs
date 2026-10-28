//! The runtime: JSON-RPC 2.0 over newline-delimited JSON, one reader loop and one task
//! per prompt turn, so a turn that waits on the client can still receive a cancel.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow, bail};
use protocol::acp::{
    AgentCapabilities, CancelNotification, CreateElicitationResponse, CreateTerminalRequest,
    CreateTerminalResponse, Implementation, InitializeRequest, InitializeResponse,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse,
    PermissionOption, PromptRequest, PromptResponse, ReleaseTerminalRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionId, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionModeRequest, StopReason, TerminalOutputRequest,
    TerminalOutputResponse, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, oneshot, watch};
use tokio::task::JoinSet;

use crate::script::{Script, Step, typed_update};

const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// Every message the agent received, in order. Clones share one log, so a test keeps a clone
/// and reads it while the agent runs.
#[derive(Debug, Clone, Default)]
pub struct Recorder {
    messages: Arc<std::sync::Mutex<Vec<Value>>>,
}

impl Recorder {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// The messages received so far (requests, notifications and responses), in order, as the
    /// client sent them.
    pub fn messages(&self) -> Vec<Value> {
        self.messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn push(&self, message: Value) {
        self.messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(message);
    }
}

/// Serves ACP for one connection until the reader reaches end of file.
///
/// `reader` carries the client's messages and `writer` carries the agent's. Each
/// `session/prompt` runs the next turn of that session's script; the call returns after
/// the reader closes and any in-flight turn has been dropped.
pub async fn run<R, W>(script: Script, reader: R, writer: W) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    run_recorded(script, Recorder::new(), reader, writer).await
}

/// [`run`] that also appends every message it receives to `recorder`.
pub async fn run_recorded<R, W>(
    script: Script,
    recorder: Recorder,
    reader: R,
    writer: W,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mock = Arc::new(Mock {
        script,
        recorder,
        writer: Mutex::new(writer),
        next_request_id: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
        next_session: AtomicU64::new(1),
    });
    let mut turns = JoinSet::new();
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                let response = error_response(Value::Null, PARSE_ERROR, error.to_string());
                mock.send(&response).await?;
                continue;
            }
        };
        mock.recorder.push(message.clone());
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (
            message.get("method").and_then(Value::as_str),
            message.get("id"),
        ) {
            (Some("session/prompt"), Some(id)) => {
                Mock::start_prompt(&mock, id.clone(), params, &mut turns).await?;
            }
            (Some(method), Some(id)) => {
                let response = match mock.handle(method, params).await {
                    Ok(result) => ok_response(id.clone(), result),
                    Err(error) => error_response(id.clone(), error.code, error.message),
                };
                mock.send(&response).await?;
            }
            (Some(method), None) => mock.notification(method, params).await,
            (None, Some(id)) => {
                if let Some(id) = id.as_u64() {
                    mock.resolve(id, message).await;
                }
            }
            (None, None) => {}
        }
    }
    turns.abort_all();
    Ok(())
}

/// Shared state for one connection.
struct Mock<W> {
    script: Script,
    recorder: Recorder,
    writer: Mutex<W>,
    next_request_id: AtomicU64,
    /// Agent-to-client requests waiting for the client's answer, by request id.
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    sessions: Mutex<HashMap<String, SessionState>>,
    next_session: AtomicU64,
}

struct SessionState {
    /// Number of prompts started in this session; selects the next turn.
    prompts: usize,
    /// `true` after `session/cancel`; reset when the next prompt starts.
    cancel: watch::Sender<bool>,
    /// The current mode id: the script's at start, then what `session/set_mode` chose.
    current_mode: Option<String>,
    /// The config options as ACP serializes them, with the current values that
    /// `session/set_config_option` has set.
    config_options: Vec<Value>,
}

impl SessionState {
    /// A session that starts with the script's modes and config options.
    fn new(script: &Script) -> Self {
        let (cancel, _) = watch::channel(false);
        Self {
            prompts: 0,
            cancel,
            current_mode: script
                .modes()
                .map(|modes| modes.current_mode_id.0.to_string()),
            config_options: script
                .config_options()
                .unwrap_or_default()
                .iter()
                .filter_map(|option| serde_json::to_value(option).ok())
                .collect(),
        }
    }
}

struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn new(code: i64, message: impl ToString) -> Self {
        Self {
            code,
            message: message.to_string(),
        }
    }
}

type RpcResult<T> = std::result::Result<T, RpcError>;

fn parse<T: DeserializeOwned>(params: Value) -> RpcResult<T> {
    serde_json::from_value(params).map_err(|error| RpcError::new(INVALID_PARAMS, error))
}

fn to_value(value: impl serde::Serialize) -> RpcResult<Value> {
    serde_json::to_value(value).map_err(|error| RpcError::new(INTERNAL_ERROR, error))
}

fn ok_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: impl ToString) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.to_string() },
    })
}

impl<W> Mock<W>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    async fn send(&self, message: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(b'\n');
        let mut writer = self.writer.lock().await;
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn notify_update(&self, session: &SessionId, update: SessionUpdate) -> Result<()> {
        let notification = SessionNotification::new(session.clone(), update);
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": serde_json::to_value(&notification)?,
        }))
        .await
    }

    /// Answers a request that does not run a turn.
    async fn handle(&self, method: &str, params: Value) -> RpcResult<Value> {
        match method {
            "initialize" => {
                let _: InitializeRequest = parse(params)?;
                let capabilities = AgentCapabilities::new()
                    .load_session(true)
                    .mcp_capabilities(self.script.mcp_capabilities());
                // ACP v1 is `1`. The version type is not re-exported by `protocol::acp`,
                // so it is inferred from the constructor.
                let version = serde_json::from_value(json!(1))
                    .map_err(|error| RpcError::new(INTERNAL_ERROR, error))?;
                let response = InitializeResponse::new(version)
                    .agent_capabilities(capabilities)
                    .agent_info(Implementation::new("acp-mock", env!("CARGO_PKG_VERSION")));
                to_value(response)
            }
            "session/new" => {
                let _: NewSessionRequest = parse(params)?;
                let number = self.next_session.fetch_add(1, Ordering::Relaxed);
                let id = format!("sess_mock_{number}");
                self.sessions
                    .lock()
                    .await
                    .insert(id.clone(), SessionState::new(&self.script));
                let mut response = NewSessionResponse::new(id);
                response.modes = self.script.modes().cloned();
                response.config_options = self.script.config_options().map(<[_]>::to_vec);
                to_value(response)
            }
            "session/load" => {
                let request: LoadSessionRequest = parse(params)?;
                self.ensure_session(&request.session_id).await;
                let mut response = LoadSessionResponse::new();
                response.modes = self.script.modes().cloned();
                response.config_options = self.script.config_options().map(<[_]>::to_vec);
                to_value(response)
            }
            "session/resume" => {
                let request: ResumeSessionRequest = parse(params)?;
                self.ensure_session(&request.session_id).await;
                let mut response = ResumeSessionResponse::new();
                response.modes = self.script.modes().cloned();
                response.config_options = self.script.config_options().map(<[_]>::to_vec);
                to_value(response)
            }
            "session/set_mode" => {
                let request: SetSessionModeRequest = parse(params)?;
                let mode = request.mode_id.0.to_string();
                let known = self.script.modes().is_some_and(|modes| {
                    modes
                        .available_modes
                        .iter()
                        .any(|available| *available.id.0 == *mode)
                });
                if !known {
                    return Err(RpcError::new(
                        INVALID_PARAMS,
                        format!("unknown mode {mode}"),
                    ));
                }
                {
                    let mut sessions = self.sessions.lock().await;
                    let session = sessions
                        .get_mut(&session_key(&request.session_id))
                        .ok_or_else(|| unknown_session(&request.session_id))?;
                    session.current_mode = Some(mode.clone());
                }
                let changed = typed_update("current_mode_update", json!({ "currentModeId": mode }))
                    .map_err(internal)?;
                self.notify_update(&request.session_id, changed)
                    .await
                    .map_err(internal)?;
                Ok(json!({}))
            }
            "session/set_config_option" => {
                let request: SetSessionConfigOptionRequest = parse(params)?;
                // The request as the wire has it: `configId`, and `value` (with `type` for a
                // boolean).
                let wire = to_value(&request)?;
                let config_id = wire["configId"].as_str().unwrap_or_default().to_owned();
                let options = {
                    let mut sessions = self.sessions.lock().await;
                    let session = sessions
                        .get_mut(&session_key(&request.session_id))
                        .ok_or_else(|| unknown_session(&request.session_id))?;
                    let index = session
                        .config_options
                        .iter()
                        .position(|option| option["id"] == config_id.as_str())
                        .ok_or_else(|| {
                            RpcError::new(
                                INVALID_PARAMS,
                                format!("unknown config option {config_id}"),
                            )
                        })?;
                    let value = config_value(&session.config_options[index], &wire)?;
                    session.config_options[index]["currentValue"] = value;
                    session.config_options.clone()
                };
                let changed =
                    typed_update("config_option_update", json!({ "configOptions": options }))
                        .map_err(internal)?;
                self.notify_update(&request.session_id, changed)
                    .await
                    .map_err(internal)?;
                Ok(json!({ "configOptions": options }))
            }
            _ => Err(RpcError::new(
                METHOD_NOT_FOUND,
                format!("method not found: {method}"),
            )),
        }
    }

    /// Creates the session if the id is unknown. A resumed session keeps its prompt count,
    /// so its next prompt runs the next turn.
    async fn ensure_session(&self, session: &SessionId) {
        self.sessions
            .lock()
            .await
            .entry(session_key(session))
            .or_insert_with(|| SessionState::new(&self.script));
    }

    async fn notification(&self, method: &str, params: Value) {
        if method != "session/cancel" {
            return;
        }
        let Ok(cancel) = serde_json::from_value::<CancelNotification>(params) else {
            return;
        };
        if let Some(session) = self
            .sessions
            .lock()
            .await
            .get(&session_key(&cancel.session_id))
        {
            session.cancel.send_replace(true);
        }
    }

    /// Hands a client's answer to the turn waiting on it.
    async fn resolve(&self, id: u64, message: Value) {
        if let Some(waiter) = self.pending.lock().await.remove(&id) {
            let _ = waiter.send(message);
        }
    }

    /// Starts the turn for a `session/prompt` in its own task. Errors that happen before
    /// the turn starts are answered at once.
    async fn start_prompt(
        mock: &Arc<Self>,
        id: Value,
        params: Value,
        turns: &mut JoinSet<()>,
    ) -> Result<()> {
        let request: PromptRequest = match serde_json::from_value(params) {
            Ok(request) => request,
            Err(error) => {
                let response = error_response(id, INVALID_PARAMS, error);
                return mock.send(&response).await;
            }
        };
        let (turn_index, cancel) = {
            let mut sessions = mock.sessions.lock().await;
            let Some(session) = sessions.get_mut(&session_key(&request.session_id)) else {
                let message = format!("unknown session {}", session_key(&request.session_id));
                drop(sessions);
                return mock
                    .send(&error_response(id, INVALID_PARAMS, message))
                    .await;
            };
            let index = session.prompts;
            session.prompts += 1;
            session.cancel.send_replace(false);
            (index, session.cancel.subscribe())
        };
        let Some(turn) = mock.script.turns().get(turn_index) else {
            let message = format!(
                "script has no turn {turn_index} for session {}",
                session_key(&request.session_id)
            );
            return mock
                .send(&error_response(id, INTERNAL_ERROR, message))
                .await;
        };
        let steps = turn.steps().to_vec();
        let mock = Arc::clone(mock);
        turns.spawn(async move {
            let response = match mock.run_turn(&request.session_id, steps, cancel).await {
                Ok(stop) => match to_value(PromptResponse::new(stop)) {
                    Ok(result) => ok_response(id, result),
                    Err(error) => error_response(id, error.code, error.message),
                },
                Err(error) => error_response(id, INTERNAL_ERROR, format!("{error:#}")),
            };
            // The client may have gone away; there is nobody left to tell.
            let _ = mock.send(&response).await;
        });
        Ok(())
    }

    /// Runs the steps of one turn. Returns the stop reason for the prompt response.
    async fn run_turn(
        &self,
        session: &SessionId,
        steps: Vec<Step>,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<StopReason> {
        for step in steps {
            if *cancel.borrow() {
                return Ok(StopReason::Cancelled);
            }
            if let Some(stop) = self.run_step(session, step, &mut cancel).await? {
                return Ok(stop);
            }
        }
        Ok(StopReason::EndTurn)
    }

    /// Runs one step. Returns `Some(stop)` when the step ends the turn.
    async fn run_step(
        &self,
        session: &SessionId,
        step: Step,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<Option<StopReason>> {
        match step {
            Step::Update(update) => self.notify_update(session, *update).await?,
            Step::Stop(reason) => return Ok(Some(reason)),
            Step::Permission { request, expect } => {
                let params = with_session(&request, session);
                let Some(result) = self
                    .request("session/request_permission", params.clone(), cancel)
                    .await?
                else {
                    return Ok(Some(StopReason::Cancelled));
                };
                let typed: RequestPermissionRequest = serde_json::from_value(params)?;
                let response: RequestPermissionResponse = serde_json::from_value(result)?;
                check_permission(&expect, &typed.options, &response)?;
            }
            Step::Elicitation { request, expect } => {
                let params = with_session(&request, session);
                let Some(result) = self.request("elicitation/create", params, cancel).await? else {
                    return Ok(Some(StopReason::Cancelled));
                };
                let response: CreateElicitationResponse = serde_json::from_value(result)?;
                let action = serde_json::to_value(&response)?
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if expect != "any" && expect != action {
                    bail!("elicitation expected {expect:?}, client answered {action:?}");
                }
            }
            Step::Terminal {
                command,
                tool_call_id,
            } => {
                return self
                    .run_terminal(session, &command, &tool_call_id, cancel)
                    .await;
            }
        }
        Ok(None)
    }

    /// Sends an agent-to-client request and waits for the answer. Returns `None` when the
    /// turn is cancelled first; the client's late answer is then ignored.
    async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<Option<Value>> {
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (waiter, reply) = oneshot::channel();
        self.pending.lock().await.insert(id, waiter);
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        let message = tokio::select! {
            reply = reply => Some(reply.map_err(|_| anyhow!("client never answered {method}"))?),
            _ = cancel.wait_for(|cancelled| *cancelled) => None,
        };
        let Some(message) = message else {
            self.pending.lock().await.remove(&id);
            return Ok(None);
        };
        if let Some(error) = message.get("error") {
            bail!("client returned an error for {method}: {error}");
        }
        Ok(Some(message.get("result").cloned().unwrap_or(Value::Null)))
    }

    /// Runs a client terminal end to end and reports it as a tool call.
    async fn run_terminal(
        &self,
        session: &SessionId,
        command: &str,
        tool_call_id: &str,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<Option<StopReason>> {
        let started = typed_update(
            "tool_call",
            json!({
                "toolCallId": tool_call_id,
                "title": command,
                "kind": "execute",
                "status": "in_progress",
            }),
        )?;
        self.notify_update(session, started).await?;

        let create = serde_json::to_value(CreateTerminalRequest::new(session.clone(), command))?;
        let Some(created) = self.request("terminal/create", create, cancel).await? else {
            return Ok(Some(StopReason::Cancelled));
        };
        let created: CreateTerminalResponse = serde_json::from_value(created)?;
        let terminal = serde_json::to_value(&created.terminal_id)?;
        let attached = typed_update(
            "tool_call_update",
            json!({
                "toolCallId": tool_call_id,
                "status": "in_progress",
                "content": [{ "type": "terminal", "terminalId": terminal }],
            }),
        )?;
        self.notify_update(session, attached).await?;

        let wait = serde_json::to_value(WaitForTerminalExitRequest::new(
            session.clone(),
            created.terminal_id.clone(),
        ))?;
        let Some(exit) = self.request("terminal/wait_for_exit", wait, cancel).await? else {
            return Ok(Some(StopReason::Cancelled));
        };
        let _: WaitForTerminalExitResponse = serde_json::from_value(exit)?;

        let output = serde_json::to_value(TerminalOutputRequest::new(
            session.clone(),
            created.terminal_id.clone(),
        ))?;
        let Some(output) = self.request("terminal/output", output, cancel).await? else {
            return Ok(Some(StopReason::Cancelled));
        };
        let output: TerminalOutputResponse = serde_json::from_value(output)?;

        let release = serde_json::to_value(ReleaseTerminalRequest::new(
            session.clone(),
            created.terminal_id.clone(),
        ))?;
        if self
            .request("terminal/release", release, cancel)
            .await?
            .is_none()
        {
            return Ok(Some(StopReason::Cancelled));
        }

        let finished = typed_update(
            "tool_call_update",
            json!({
                "toolCallId": tool_call_id,
                "status": "completed",
                "content": [
                    { "type": "content", "content": { "type": "text", "text": output.output } },
                    { "type": "terminal", "terminalId": terminal },
                ],
            }),
        )?;
        self.notify_update(session, finished).await?;
        Ok(None)
    }
}

/// Copies a payload and sets its `sessionId` to the session the turn runs in.
fn with_session(payload: &Value, session: &SessionId) -> Value {
    let mut payload = payload.clone();
    if let Some(object) = payload.as_object_mut() {
        object.insert("sessionId".to_owned(), Value::String(session_key(session)));
    }
    payload
}

fn session_key(session: &SessionId) -> String {
    session.0.to_string()
}

fn unknown_session(session: &SessionId) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        format!("unknown session {}", session_key(session)),
    )
}

fn internal(error: impl std::fmt::Display) -> RpcError {
    RpcError::new(INTERNAL_ERROR, format!("{error:#}"))
}

/// The option value a `session/set_config_option` request sets, if it fits `option`: a boolean
/// for a boolean option, a listed value id for a select option.
fn config_value(option: &Value, request: &Value) -> RpcResult<Value> {
    let value = &request["value"];
    let fits = match option["type"].as_str() {
        Some("boolean") => value.is_boolean(),
        Some("select") => value
            .as_str()
            .is_some_and(|text| select_values(&option["options"]).contains(&text)),
        _ => false,
    };
    if fits {
        Ok(value.clone())
    } else {
        Err(RpcError::new(
            INVALID_PARAMS,
            format!("the value does not fit config option {}", option["id"]),
        ))
    }
}

/// Every value id of a select option's `options`, flat or grouped.
fn select_values(options: &Value) -> Vec<&str> {
    options
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|entry| match entry.get("options") {
            Some(group) => select_values(group),
            None => entry
                .get("value")
                .and_then(Value::as_str)
                .into_iter()
                .collect(),
        })
        .collect()
}

/// Checks a permission answer against the script's `expect`.
fn check_permission(
    expect: &str,
    options: &[PermissionOption],
    response: &RequestPermissionResponse,
) -> Result<()> {
    let answered = match &response.outcome {
        RequestPermissionOutcome::Cancelled => "cancelled".to_owned(),
        RequestPermissionOutcome::Selected(selected) => selected.option_id.0.to_string(),
        _ => "unknown".to_owned(),
    };
    let kind = options
        .iter()
        .find(|option| option.option_id.0.as_ref() == answered)
        .and_then(|option| serde_json::to_value(option.kind).ok())
        .and_then(|kind| kind.as_str().map(str::to_owned))
        .unwrap_or_default();
    let matches = match expect {
        "any" => true,
        "cancelled" => answered == "cancelled",
        "allow" => kind.starts_with("allow"),
        "reject" => kind.starts_with("reject"),
        other => answered == other,
    };
    if matches {
        Ok(())
    } else {
        Err(anyhow!(
            "permission expected {expect:?}, client answered {answered:?}"
        ))
    }
}
