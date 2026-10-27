// TRACER STUB: replaced by M1A-006b (runner bridge: ACP client ⇄ TurnEvents ⇄ tunnel commands)
//! The tracer bullet's in-process runner loop: a minimal hand-written ACP client (JSON-RPC 2.0
//! over newline-delimited JSON) that turns tunnel [`Command`]s into ACP calls and ACP traffic
//! into producer-side TurnEvent [`Envelope`]s (`seq: 0`).
//!
//! What it does (turn-event.md §5):
//!
//! - `prompt`: `UserMessage`, `session/new` on the first prompt of a thread,
//!   `RunStarted{prompt}`, then `session/prompt`.
//! - `session/update`: mapped by [`crate::tracer_mapping`]; buffered while the run is paused.
//! - `session/request_permission`: closes the open message, `Interrupt{approval}`,
//!   `RunFinished{interrupt}`; the run is paused until the matching `approval` command.
//! - `approval`: `RunStarted{resume, parent_run_id}`, flushes buffered updates, answers ACP.
//! - `session/prompt` response: closes open messages and tool calls, `RunFinished{success}`
//!   (or `RunError`).
//!
//! Stub limits: one task per thread, one open interrupt per run, no durable store, no
//! idempotency-key dedupe, no `command_result`, `cancel`/`dispatch` ignored, every
//! client-side ACP request other than `session/request_permission` answered with an error.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use protocol::acp::{
    RequestPermissionOutcome, RequestPermissionResponse, SelectedPermissionOutcome,
};
use protocol::tunnel::{Command, ContentPart, Outcome};
use protocol::turn_event::{
    Envelope, Event, RunError, RunErrorReason, RunFinished, RunOutcome, RunReason, RunStarted,
    StopReason, ToolCallEndStatus, UserMessage, format_timestamp,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::tracer_mapping::{AcpMapper, Mapped};

/// Default approval TTL (plan §4.1): 10 minutes.
pub const APPROVAL_TTL_MS: u64 = 10 * 60 * 1000;

/// How the bridge identifies itself in `RunStarted`.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub machine_id: String,
    pub agent_id: String,
    /// Absolute working directory for `session/new`.
    pub cwd: PathBuf,
}

/// Runs the bridge until `commands` closes (the tunnel client shut down) or the agent's
/// output ends. Envelopes go to `events` in production order.
pub async fn run<R, W>(
    config: BridgeConfig,
    acp_reader: R,
    acp_writer: W,
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Envelope>,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (incoming_tx, incoming) = mpsc::channel::<Value>(256);
    let reader = tokio::spawn(async move {
        let mut lines = BufReader::new(acp_reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                tracing::warn!(%line, "agent sent a line that is not JSON");
                continue;
            };
            if incoming_tx.send(message).await.is_err() {
                break;
            }
        }
    });
    let mut bridge = Bridge {
        config,
        writer: acp_writer,
        incoming,
        backlog: VecDeque::new(),
        next_rpc_id: 1,
        events,
        sessions: HashMap::new(),
        tasks: HashMap::new(),
        id_seed: format!("{:x}", now_ms()),
    };
    let result = bridge.serve(commands).await;
    reader.abort();
    result
}

/// One running task (one prompt turn) of a thread.
struct Task {
    thread_id: String,
    task_id: String,
    run_id: String,
    mapper: AcpMapper,
    /// JSON-RPC id of the outstanding `session/prompt`.
    prompt_rpc_id: u64,
    paused: Option<Paused>,
    /// ACP updates received while paused; emitted after the resume `RunStarted` (§5 rule 6).
    buffered: Vec<Value>,
}

/// A run paused on an interrupt.
struct Paused {
    interrupt_id: String,
    /// JSON-RPC id of the agent's `session/request_permission`, answered on approval.
    rpc_id: Value,
}

struct Bridge<W> {
    config: BridgeConfig,
    writer: W,
    incoming: mpsc::Receiver<Value>,
    /// Agent messages that arrived while [`Bridge::call`] waited for a response.
    backlog: VecDeque<Value>,
    next_rpc_id: u64,
    events: mpsc::Sender<Envelope>,
    /// thread id → ACP session id.
    sessions: HashMap<String, String>,
    /// ACP session id → the running task.
    tasks: HashMap<String, Task>,
    id_seed: String,
}

impl<W: AsyncWrite + Unpin> Bridge<W> {
    async fn serve(&mut self, mut commands: mpsc::Receiver<Command>) -> Result<()> {
        self.call(
            "initialize",
            json!({ "protocolVersion": 1, "clientCapabilities": {} }),
        )
        .await
        .context("ACP initialize")?;
        loop {
            if let Some(message) = self.backlog.pop_front() {
                self.on_agent_message(message).await?;
                continue;
            }
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.on_command(command).await?,
                    None => return Ok(()),
                },
                message = self.incoming.recv() => match message {
                    Some(message) => self.on_agent_message(message).await?,
                    None => bail!("the agent closed its output"),
                },
            }
        }
    }

    // ---- ACP plumbing ---------------------------------------------------------------------

    async fn send(&mut self, message: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(b'\n');
        self.writer
            .write_all(&bytes)
            .await
            .context("write to agent")?;
        self.writer.flush().await.context("flush to agent")?;
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next_rpc_id;
        self.next_rpc_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        Ok(id)
    }

    /// Sends a request and waits for its response; anything else that arrives meanwhile is
    /// kept in the backlog, in order.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.request(method, params).await?;
        loop {
            let message = self
                .incoming
                .recv()
                .await
                .ok_or_else(|| anyhow!("agent closed while waiting for {method}"))?;
            let is_response = message.get("method").is_none()
                && message.get("id").and_then(Value::as_u64) == Some(id);
            if !is_response {
                self.backlog.push_back(message);
                continue;
            }
            if let Some(error) = message.get("error") {
                bail!("{method} failed: {error}");
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    async fn respond(&mut self, id: Value, result: Value) -> Result<()> {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
            .await
    }

    async fn respond_error(&mut self, id: Value, code: i64, message: &str) -> Result<()> {
        self.send(&json!({
            "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message },
        }))
        .await
    }

    // ---- commands from the hub ------------------------------------------------------------

    async fn on_command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Prompt {
                thread_id,
                task_id,
                message_id,
                parts,
                author,
            } => {
                self.start_task(thread_id, task_id, message_id, parts, author)
                    .await
            }
            Command::Approval {
                thread_id,
                interrupt_id,
                outcome,
                ..
            } => self.approve(&thread_id, &interrupt_id, outcome).await,
            other => {
                tracing::warn!(?other, "tracer bridge ignores this command");
                Ok(())
            }
        }
    }

    async fn start_task(
        &mut self,
        thread_id: String,
        task_id: String,
        message_id: String,
        parts: Vec<ContentPart>,
        author: protocol::tunnel::Actor,
    ) -> Result<()> {
        if let Some(session) = self.sessions.get(&thread_id)
            && self.tasks.contains_key(session)
        {
            tracing::warn!(%thread_id, "a task is already running; prompt dropped");
            return Ok(());
        }
        let prompt: Vec<Value> = parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(json!({ "type": "text", "text": text })),
                // The tracer does not fetch attachments.
                ContentPart::File { .. } => None,
            })
            .collect();
        self.emit_raw(
            &thread_id,
            Some(&task_id),
            None,
            UserMessage {
                message_id,
                parts,
                author,
            }
            .into(),
            None,
        )
        .await?;

        let session_id = match self.sessions.get(&thread_id) {
            Some(session) => session.clone(),
            None => {
                let cwd = self.config.cwd.display().to_string();
                let created = self
                    .call("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
                    .await?;
                let session = created
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("session/new returned no sessionId: {created}"))?
                    .to_owned();
                self.sessions.insert(thread_id.clone(), session.clone());
                session
            }
        };

        let mut mapper = AcpMapper::new(format!("{}_{}", self.id_seed, task_id));
        let run_id = mapper.fresh_id("run_");
        self.emit_raw(
            &thread_id,
            Some(&task_id),
            Some(&run_id),
            RunStarted {
                reason: RunReason::Prompt,
                parent_run_id: None,
                agent_id: Some(self.config.agent_id.clone()),
                machine_id: Some(self.config.machine_id.clone()),
            }
            .into(),
            None,
        )
        .await?;
        let prompt_rpc_id = self
            .request(
                "session/prompt",
                json!({ "sessionId": session_id, "prompt": prompt }),
            )
            .await?;
        self.tasks.insert(
            session_id,
            Task {
                thread_id,
                task_id,
                run_id,
                mapper,
                prompt_rpc_id,
                paused: None,
                buffered: Vec::new(),
            },
        );
        Ok(())
    }

    async fn approve(
        &mut self,
        thread_id: &str,
        interrupt_id: &str,
        outcome: Outcome,
    ) -> Result<()> {
        let Some(session) = self.sessions.get(thread_id).cloned() else {
            tracing::warn!(%thread_id, "approval for an unknown thread");
            return Ok(());
        };
        let Some(task) = self.tasks.get_mut(&session) else {
            tracing::warn!(%thread_id, "approval but no task is running");
            return Ok(());
        };
        if task.paused.as_ref().map(|p| p.interrupt_id.as_str()) != Some(interrupt_id) {
            tracing::warn!(%interrupt_id, "approval for an interrupt that is not open");
            return Ok(());
        }
        let Some(paused) = task.paused.take() else {
            return Ok(());
        };
        let parent_run_id = std::mem::replace(&mut task.run_id, task.mapper.fresh_id("run_"));
        let buffered = std::mem::take(&mut task.buffered);
        let resume = RunStarted {
            reason: RunReason::Resume,
            parent_run_id: Some(parent_run_id),
            agent_id: None,
            machine_id: None,
        };
        self.emit_task(&session, vec![Mapped::plain(resume)])
            .await?;
        for update in buffered {
            self.on_update(&session, update).await?;
        }
        let outcome = match outcome {
            Outcome::Selected { option_id } => {
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id))
            }
            Outcome::Cancelled => RequestPermissionOutcome::Cancelled,
        };
        let response = serde_json::to_value(RequestPermissionResponse::new(outcome))?;
        self.respond(paused.rpc_id, response).await
    }

    // ---- traffic from the agent -----------------------------------------------------------

    async fn on_agent_message(&mut self, message: Value) -> Result<()> {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (method.as_deref(), id) {
            (Some("session/update"), None) => {
                let session = session_of(&params)?;
                let update = params.get("update").cloned().unwrap_or(Value::Null);
                let paused = self.tasks.get(&session).map(|task| task.paused.is_some());
                match paused {
                    Some(true) => {
                        if let Some(task) = self.tasks.get_mut(&session) {
                            task.buffered.push(update);
                        }
                        Ok(())
                    }
                    Some(false) => self.on_update(&session, update).await,
                    None => {
                        tracing::warn!(%session, "update outside a task dropped");
                        Ok(())
                    }
                }
            }
            (Some("session/request_permission"), Some(id)) => self.on_permission(id, params).await,
            (Some(method), Some(id)) => {
                tracing::warn!(%method, "tracer bridge does not serve this client method");
                self.respond_error(id, -32601, "method not supported by the tracer client")
                    .await
            }
            (Some(_), None) => Ok(()),
            (None, Some(id)) => self.on_response(id, &message).await,
            (None, None) => Ok(()),
        }
    }

    async fn on_update(&mut self, session: &str, update: Value) -> Result<()> {
        let Some(task) = self.tasks.get_mut(session) else {
            return Ok(());
        };
        let mapped = task.mapper.session_update(&update);
        self.emit_task(session, mapped).await
    }

    async fn on_permission(&mut self, rpc_id: Value, params: Value) -> Result<()> {
        let session = session_of(&params)?;
        let Some(task) = self.tasks.get_mut(&session) else {
            return self
                .respond_error(rpc_id, -32602, "no prompt is running in this session")
                .await;
        };
        if task.paused.is_some() {
            // Stub limit: one open interrupt per run. Answer the extra one `cancelled`.
            let response = serde_json::to_value(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ))?;
            return self.respond(rpc_id, response).await;
        }
        let interrupt_id = task.mapper.fresh_id("int_");
        let expires_at = format_timestamp(now_ms() + APPROVAL_TTL_MS);
        let mut mapped = task.mapper.permission(&params, &interrupt_id, &expires_at);
        mapped.push(Mapped::plain(RunFinished {
            outcome: RunOutcome::Interrupt {
                interrupt_ids: vec![interrupt_id.clone()],
            },
        }));
        task.paused = Some(Paused {
            interrupt_id,
            rpc_id,
        });
        self.emit_task(&session, mapped).await
    }

    async fn on_response(&mut self, id: Value, message: &Value) -> Result<()> {
        let Some(id) = id.as_u64() else {
            return Ok(());
        };
        let Some(session) = self
            .tasks
            .iter()
            .find(|(_, task)| task.prompt_rpc_id == id)
            .map(|(session, _)| session.clone())
        else {
            tracing::warn!(id, "response to an unknown request");
            return Ok(());
        };
        let Some(mut task) = self.tasks.remove(&session) else {
            return Ok(());
        };
        let mut mapped = task.mapper.close_messages();
        let stop = message
            .get("result")
            .and_then(|result| result.get("stopReason"))
            .and_then(Value::as_str);
        let terminal: Event = match (message.get("error"), stop) {
            (Some(error), _) => {
                mapped.extend(task.mapper.close_tool_calls(ToolCallEndStatus::Failed));
                RunError {
                    reason: RunErrorReason::Error,
                    code: Some("protocol".to_owned()),
                    message: format!("session/prompt failed: {error}"),
                }
                .into()
            }
            (None, Some("cancelled")) => {
                mapped.extend(task.mapper.close_tool_calls(ToolCallEndStatus::Failed));
                RunError {
                    reason: RunErrorReason::Cancelled,
                    code: None,
                    message: "cancelled".to_owned(),
                }
                .into()
            }
            (None, stop) => {
                // Stub: a tool call the agent left open is closed as failed (§5 rule 5).
                mapped.extend(task.mapper.close_tool_calls(ToolCallEndStatus::Failed));
                let stop_reason = stop.and_then(|s| {
                    serde_json::from_value::<StopReason>(Value::String(s.to_owned())).ok()
                });
                RunFinished {
                    outcome: RunOutcome::Success { stop_reason },
                }
                .into()
            }
        };
        mapped.push(Mapped {
            event: terminal,
            raw: None,
        });
        for item in mapped {
            self.emit_raw(
                &task.thread_id,
                Some(&task.task_id),
                Some(&task.run_id),
                item.event,
                item.raw,
            )
            .await?;
        }
        Ok(())
    }

    // ---- envelopes ------------------------------------------------------------------------

    /// Emits run-scoped events for the task running in `session`.
    async fn emit_task(&mut self, session: &str, mapped: Vec<Mapped>) -> Result<()> {
        let Some(task) = self.tasks.get(session) else {
            return Ok(());
        };
        let (thread_id, task_id, run_id) = (
            task.thread_id.clone(),
            task.task_id.clone(),
            task.run_id.clone(),
        );
        for item in mapped {
            self.emit_raw(
                &thread_id,
                Some(&task_id),
                Some(&run_id),
                item.event,
                item.raw,
            )
            .await?;
        }
        Ok(())
    }

    async fn emit_raw(
        &mut self,
        thread_id: &str,
        task_id: Option<&str>,
        run_id: Option<&str>,
        event: Event,
        raw: Option<protocol::turn_event::RawPayload>,
    ) -> Result<()> {
        let mut envelope = Envelope::new(
            thread_id,
            task_id.map(str::to_owned),
            run_id.map(str::to_owned),
            format_timestamp(now_ms()),
            event,
        );
        envelope.raw = raw;
        self.events
            .send(envelope)
            .await
            .map_err(|_| anyhow!("the tunnel client stopped taking events"))
    }
}

fn session_of(params: &Value) -> Result<String> {
    params
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("agent message without sessionId: {params}"))
}

/// Milliseconds since the Unix epoch (0 if the clock is before 1970).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
