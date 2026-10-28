//! The bridge loop: commands in, ACP traffic in, TurnEvents out.

use std::path::PathBuf;
use std::sync::Arc;

use protocol::acp::{ContentBlock, SessionNotification};
use protocol::mapping::acp_turn::{
    AcpTurnMapper, IdSource, SequentialIds, StartTask, runner_actor,
};
use protocol::tunnel::{Command, CommandErrorCode, ContentPart, Disposition};
use protocol::turn_event::{Envelope, Event, RunErrorReason, RunOutcome, UserMessage};
use serde_json::{Value, json};

use super::handler::{Internal, Resources};
use super::ids::SharedIds;
use super::io::{CommandFailure, CommandSource, EventSink, IncomingCommand};
use super::{ActiveTask, Bridge, BridgeError, Start, ThreadState};
use crate::acp::client::ClientError;
use crate::acp::fs::Workspace;
use crate::acp::reattach::{ReattachError, expire_overdue_interrupts, rfc3339};
use crate::acp::sessions::PromptResult;
use crate::acp::terminal::{ActivitySink, TerminalScope, Terminals};

impl<S: EventSink, C: CommandSource> Bridge<S, C> {
    /// Runs until the command source ends. Fails when the store, the agent connection's
    /// `initialize` or the sink fails.
    pub async fn run(mut self) -> Result<(), BridgeError> {
        self.client.initialize().await?;
        // The agent process is new: no session survives a restart of the bridge.
        self.store.detach_all_sessions()?;
        self.expire_stale().await?;

        let period = self.config.expiry_check_interval;
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        loop {
            tokio::select! {
                incoming = self.commands.next() => match incoming {
                    Some(incoming) => self.on_incoming(incoming).await?,
                    None => return Ok(()),
                },
                Some(notification) = self.updates.recv() => {
                    self.on_update(notification).await?;
                }
                Some(message) = self.internal.recv() => {
                    self.drain_updates().await?;
                    self.on_internal(message).await?;
                }
                _ = ticker.tick() => self.on_tick().await?,
            }
        }
    }

    pub(super) fn now_ms(&self) -> u64 {
        self.store.now_ms()
    }

    pub(super) fn ts(&self) -> String {
        rfc3339(self.now_ms())
    }

    // ---- commands ---------------------------------------------------------------------------

    async fn on_incoming(&mut self, incoming: IncomingCommand) -> Result<(), BridgeError> {
        let IncomingCommand {
            seq,
            idempotency_key,
            command,
        } = incoming;
        let disposition = self
            .store
            .commit_down_frame(seq, Some(idempotency_key.as_str()))?;
        if disposition == Disposition::New {
            self.apply(&idempotency_key, command).await?;
        } else {
            tracing::debug!(%idempotency_key, "duplicate command ignored");
        }
        self.commands.processed(seq).await;
        Ok(())
    }

    pub(super) async fn reject(
        &self,
        key: &str,
        code: CommandErrorCode,
        message: impl Into<String>,
    ) -> Result<(), BridgeError> {
        let message = message.into();
        tracing::warn!(%key, ?code, %message, "command rejected");
        self.sink
            .command_result(CommandFailure {
                idempotency_key: key.to_owned(),
                code,
                message,
            })
            .await?;
        Ok(())
    }

    async fn apply(&mut self, key: &str, command: Command) -> Result<(), BridgeError> {
        match command {
            Command::Prompt {
                thread_id,
                task_id,
                message_id,
                parts,
                author,
            } => {
                let start = Start {
                    thread_id,
                    task_id,
                    message_id,
                    parts,
                    author,
                    harness: None,
                    cwd: None,
                };
                self.start_task(key, start).await
            }
            Command::Dispatch {
                thread_id,
                task_id,
                agent,
                workspace,
                message_id,
                parts,
                author,
                ..
            } => {
                let start = Start {
                    thread_id,
                    task_id,
                    message_id,
                    parts,
                    author,
                    harness: Some(agent),
                    cwd: Some(PathBuf::from(workspace.path)),
                };
                self.start_task(key, start).await
            }
            Command::Approval {
                thread_id,
                task_id,
                interrupt_id,
                outcome,
                ..
            } => {
                self.approve(key, &thread_id, &task_id, &interrupt_id, outcome)
                    .await
            }
            Command::Cancel { thread_id, task_id } => self.cancel(key, &thread_id, &task_id).await,
            Command::SessionConfig {
                thread_id,
                mode_id,
                config,
            } => {
                let config = config.map(|change| (change.id, change.value));
                self.session_config(key, &thread_id, mode_id, config).await
            }
        }
    }

    async fn start_task(&mut self, key: &str, start: Start) -> Result<(), BridgeError> {
        let Start {
            thread_id,
            task_id,
            message_id,
            parts,
            author,
            harness,
            cwd,
        } = start;
        if self
            .threads
            .get(&thread_id)
            .is_some_and(|thread| thread.task.is_some())
        {
            return self
                .reject(
                    key,
                    CommandErrorCode::Invalid,
                    format!("thread {thread_id} already has a running task"),
                )
                .await;
        }
        let running = self
            .threads
            .values()
            .filter(|thread| thread.task.is_some())
            .count();
        if running >= self.config.max_running_tasks {
            return self
                .reject(
                    key,
                    CommandErrorCode::Capacity,
                    format!("{running} sessions already running"),
                )
                .await;
        }
        let (harness, cwd) = self.thread_defaults(&thread_id, harness, cwd)?;
        let workspace = match Workspace::new(&cwd) {
            Ok(workspace) => workspace,
            Err(error) => {
                return self
                    .reject(
                        key,
                        CommandErrorCode::Invalid,
                        format!("workspace {}: {error}", cwd.display()),
                    )
                    .await;
            }
        };

        // The task is announced before the session is attached, so a failure to attach ends it
        // with a RunError and the log stays valid.
        let ts = self.ts();
        let mut events = Vec::new();
        if self.config.emit_user_message {
            events.push(Envelope::new(
                thread_id.clone(),
                Some(task_id.clone()),
                None,
                ts.clone(),
                UserMessage {
                    message_id,
                    parts: parts.clone(),
                    author,
                },
            ));
        }
        let ids = Arc::clone(&self.ids);
        let thread = self
            .threads
            .entry(thread_id.clone())
            .or_insert_with(|| ThreadState {
                harness: harness.clone(),
                cwd: cwd.clone(),
                mapper: AcpTurnMapper::new(thread_id.clone(), SharedIds(ids)),
                session: None,
                task: None,
            });
        thread.harness.clone_from(&harness);
        thread.cwd.clone_from(&cwd);
        let mut start = StartTask::new(task_id.clone());
        start.agent_id = Some(self.config.agent_id.clone());
        start.machine_id = Some(self.config.machine_id.clone());
        events.extend(thread.mapper.start_task(start, &ts));
        let run_id = thread.mapper.run_id().map(str::to_owned);
        thread.task = Some(ActiveTask {
            task_id: task_id.clone(),
            held: Vec::new(),
        });
        self.store
            .upsert_thread(&thread_id, &harness, &cwd.to_string_lossy())?;
        self.store.upsert_task(&task_id, &thread_id, "running")?;
        self.emit(&thread_id, events).await?;

        let first_attach = self
            .threads
            .get(&thread_id)
            .is_some_and(|thread| thread.session.is_none());
        let session = match self
            .reattacher
            .ensure_session(&self.client, &thread_id, &harness, &cwd)
            .await
        {
            Ok((session, _how)) => session,
            Err(error) => {
                let code = match &error {
                    ReattachError::Client(ClientError::Closed | ClientError::Io(_)) => {
                        "harness_exited"
                    }
                    _ => "protocol",
                };
                return self
                    .fail_task(&thread_id, code, &format!("could not attach: {error}"))
                    .await;
            }
        };
        let session_key = session.0.to_string();
        self.by_session
            .insert(session_key.clone(), thread_id.clone());
        if let Some(thread) = self.threads.get_mut(&thread_id) {
            thread.session = Some(session.clone());
        }
        if first_attach {
            // The session's modes and config options, before any update of them (§4.6).
            let snapshot = self.session_snapshot(&thread_id, &session);
            self.emit(&thread_id, snapshot).await?;
        }
        let scope = TerminalScope {
            thread_id: thread_id.clone(),
            task_id: Some(task_id.clone()),
            run_id,
        };
        let terminals = Terminals::new(workspace.clone(), scope, self.activity_sink(&thread_id));
        self.handler.register(
            &session_key,
            Resources {
                workspace,
                terminals,
            },
        );

        let blocks = match content_blocks(&parts) {
            Ok(blocks) => blocks,
            Err(error) => {
                return self
                    .fail_task(
                        &thread_id,
                        "protocol",
                        &format!("bad prompt content: {error}"),
                    )
                    .await;
            }
        };
        match self.client.submit(&session, blocks) {
            Ok(submitted) => {
                let tx = self.internal_tx.clone();
                tokio::spawn(async move {
                    let result: PromptResult = match submitted.done.await {
                        Ok(result) => result,
                        Err(_) => Err(ClientError::Closed),
                    };
                    // The bridge may have stopped; then nobody needs the result.
                    let _ = tx.send(Internal::PromptDone {
                        thread_id,
                        task_id,
                        result,
                    });
                });
                Ok(())
            }
            Err(error) => {
                let code = prompt_error_code(&error);
                self.fail_task(&thread_id, code, &error.to_string()).await
            }
        }
    }

    /// The harness and directory of a thread: the command's, else the stored thread's, else the
    /// bridge's own.
    pub(super) fn thread_defaults(
        &self,
        thread_id: &str,
        harness: Option<String>,
        cwd: Option<PathBuf>,
    ) -> Result<(String, PathBuf), BridgeError> {
        let known = match self.threads.get(thread_id) {
            Some(thread) => Some((thread.harness.clone(), thread.cwd.clone())),
            None => self
                .store
                .thread(thread_id)?
                .map(|row| (row.harness, PathBuf::from(row.cwd))),
        };
        let (known_harness, known_cwd) = known.unzip();
        Ok((
            harness
                .or(known_harness)
                .unwrap_or_else(|| self.config.agent_id.clone()),
            cwd.or(known_cwd).unwrap_or_else(|| self.config.cwd.clone()),
        ))
    }

    fn activity_sink(&self, thread_id: &str) -> ActivitySink {
        let tx = self.internal_tx.clone();
        let thread_id = thread_id.to_owned();
        Arc::new(move |envelope| {
            // A closed channel means the bridge stopped; the output has nowhere to go.
            let _ = tx.send(Internal::Activity {
                thread_id: thread_id.clone(),
                envelope,
            });
        })
    }

    async fn cancel(
        &mut self,
        key: &str,
        thread_id: &str,
        task_id: &str,
    ) -> Result<(), BridgeError> {
        let Some(thread) = self.threads.get(thread_id) else {
            return self
                .reject(
                    key,
                    CommandErrorCode::UnknownThread,
                    format!("unknown thread {thread_id}"),
                )
                .await;
        };
        if thread.mapper.task_id() != Some(task_id) {
            tracing::debug!(%thread_id, %task_id, "cancel for a task that is not running");
            return Ok(());
        }
        let session = thread.session.clone();
        self.drain_updates().await?;
        // The ACP cancel goes first: the client answers any waiting permission `cancelled`.
        if let Some(session) = session
            && let Err(error) = self.client.cancel(&session).await
        {
            tracing::warn!(%error, "session/cancel could not be sent");
        }
        let ts = self.ts();
        let events = match self.threads.get_mut(thread_id) {
            Some(thread) => thread.mapper.on_cancel(&runner_actor(), &ts),
            None => Vec::new(),
        };
        self.emit(thread_id, events).await
    }

    // ---- traffic from the agent -------------------------------------------------------------

    async fn drain_updates(&mut self) -> Result<(), BridgeError> {
        while let Ok(notification) = self.updates.try_recv() {
            self.on_update(notification).await?;
        }
        Ok(())
    }

    async fn on_update(&mut self, notification: SessionNotification) -> Result<(), BridgeError> {
        let session = notification.session_id.0.to_string();
        let Some(thread_id) = self.by_session.get(&session).cloned() else {
            tracing::warn!(%session, "update for an unknown session dropped");
            return Ok(());
        };
        let value = match serde_json::to_value(&notification) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "update could not be re-encoded; dropped");
                return Ok(());
            }
        };
        let ts = self.ts();
        let events = match self.threads.get_mut(&thread_id) {
            Some(thread) => match thread.mapper.on_session_update_json(&value, &ts) {
                Ok(events) => events,
                Err(error) => {
                    tracing::warn!(%error, "update could not be mapped; dropped");
                    return Ok(());
                }
            },
            None => return Ok(()),
        };
        self.emit(&thread_id, events).await
    }

    async fn on_internal(&mut self, message: Internal) -> Result<(), BridgeError> {
        match message {
            Internal::Permission { request, reply } => self.on_permission(request, reply).await,
            Internal::Elicitation { request, reply } => self.on_elicitation(request, reply).await,
            Internal::Activity {
                thread_id,
                envelope,
            } => self.on_activity(&thread_id, envelope).await,
            Internal::PromptDone {
                thread_id,
                task_id,
                result,
            } => self.on_prompt_done(&thread_id, &task_id, result).await,
        }
    }

    async fn on_activity(
        &mut self,
        thread_id: &str,
        mut envelope: Envelope,
    ) -> Result<(), BridgeError> {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return Ok(());
        };
        let Some(task) = thread.task.as_mut() else {
            tracing::debug!(%thread_id, "terminal output after the task ended dropped");
            return Ok(());
        };
        // The terminal was created in an earlier run when the task was resumed since.
        envelope.task_id = Some(task.task_id.clone());
        if thread.mapper.is_paused() {
            task.held.push(envelope);
            return Ok(());
        }
        envelope.run_id = thread.mapper.run_id().map(str::to_owned);
        self.sink.send(vec![envelope]).await?;
        Ok(())
    }

    async fn on_prompt_done(
        &mut self,
        thread_id: &str,
        task_id: &str,
        result: PromptResult,
    ) -> Result<(), BridgeError> {
        let ts = self.ts();
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return Ok(());
        };
        if thread.mapper.task_id() != Some(task_id) {
            // The task already ended (cancel, expiry); this is its late stop reason.
            return Ok(());
        }
        let events = match result {
            Ok(stop) => thread.mapper.on_prompt_response(stop, &ts),
            Err(error) => {
                let code = prompt_error_code(&error);
                thread.mapper.on_error(code, &error.to_string(), &ts)
            }
        };
        self.emit(thread_id, events).await
    }

    async fn fail_task(
        &mut self,
        thread_id: &str,
        code: &str,
        message: &str,
    ) -> Result<(), BridgeError> {
        let ts = self.ts();
        let events = match self.threads.get_mut(thread_id) {
            Some(thread) => thread.mapper.on_error(code, message, &ts),
            None => Vec::new(),
        };
        self.emit(thread_id, events).await
    }

    // ---- expiry -----------------------------------------------------------------------------

    async fn on_tick(&mut self) -> Result<(), BridgeError> {
        let now = self.now_ms();
        let mut overdue: Vec<(u64, String)> = self
            .approvals
            .iter()
            .filter(|(_, pending)| pending.expires_at_ms <= now)
            .map(|(id, pending)| (pending.expires_at_ms, id.clone()))
            .collect();
        overdue.sort();
        for (_, interrupt_id) in overdue {
            self.expire_live(&interrupt_id).await?;
        }
        self.expire_stale().await
    }

    /// Ends the task of an interrupt this process is still holding the agent's request for.
    async fn expire_live(&mut self, interrupt_id: &str) -> Result<(), BridgeError> {
        let Some(pending) = self.approvals.remove(interrupt_id) else {
            return Ok(());
        };
        self.store.resolve_interrupt(interrupt_id)?;
        // The agent's request is answered `cancelled` when its reply is dropped.
        drop(self.handler.take_parked(interrupt_id));
        let ts = self.ts();
        let events = match self.threads.get_mut(&pending.thread_id) {
            Some(thread) => thread.mapper.on_interrupt_expired(interrupt_id, &ts),
            None => Vec::new(),
        };
        let session = self
            .threads
            .get(&pending.thread_id)
            .and_then(|thread| thread.session.clone());
        // Dropping the reply answers the agent's request `cancelled`; the cancel ends its prompt.
        drop(pending.reply);
        if let Some(session) = session
            && let Err(error) = self.client.cancel(&session).await
        {
            tracing::warn!(%error, "session/cancel after an expiry could not be sent");
        }
        self.emit(&pending.thread_id, events).await
    }

    /// Ends the tasks of stored interrupts nobody holds a request for (left by an earlier
    /// process).
    async fn expire_stale(&mut self) -> Result<(), BridgeError> {
        let now = self.now_ms();
        let runs = match expire_overdue_interrupts(&self.store, |interrupt| {
            // `expire_overdue_interrupts` wants the default id source. Skip ahead so the run id
            // it makes is unlikely to repeat one the thread already used.
            let mut ids = SequentialIds::default();
            for _ in 0..1_000 + (now / 1_000) % 100_000 {
                ids.next_id("run_");
            }
            AcpTurnMapper::new(interrupt.thread_id.clone(), ids)
        }) {
            Ok(runs) => runs,
            Err(ReattachError::Store(error)) => return Err(error.into()),
            Err(error) => {
                tracing::warn!(%error, "stored interrupts could not be expired");
                return Ok(());
            }
        };
        for run in runs {
            self.emit(&run.interrupt.thread_id, run.events).await?;
        }
        Ok(())
    }

    // ---- output -----------------------------------------------------------------------------

    /// Sends the events of one step, keeps the stored task states current and notices when the
    /// task ended.
    pub(super) async fn emit(
        &mut self,
        thread_id: &str,
        events: Vec<Envelope>,
    ) -> Result<(), BridgeError> {
        if events.is_empty() {
            return Ok(());
        }
        for envelope in &events {
            if let (Some(task_id), Some(state)) =
                (envelope.task_id.as_deref(), task_state(&envelope.event))
            {
                self.store
                    .upsert_task(task_id, &envelope.thread_id, state)?;
            }
        }
        self.sink.send(events).await?;
        self.reconcile(thread_id)
    }

    /// Terminal output teed during the pause, now belonging to the resumed run.
    pub(super) fn release_held(&mut self, thread_id: &str) -> Vec<Envelope> {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return Vec::new();
        };
        let run_id = thread.mapper.run_id().map(str::to_owned);
        let Some(task) = thread.task.as_mut() else {
            return Vec::new();
        };
        let mut held = std::mem::take(&mut task.held);
        for envelope in &mut held {
            envelope.run_id.clone_from(&run_id);
        }
        held
    }

    /// When the mapper has no task any more: forgets the active task, drops its approvals and
    /// releases the session's workspace and terminals.
    fn reconcile(&mut self, thread_id: &str) -> Result<(), BridgeError> {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return Ok(());
        };
        if thread.task.is_none() || thread.mapper.task_id().is_some() {
            return Ok(());
        }
        thread.task = None;
        if let Some(session) = &thread.session {
            self.handler.release(&session.0);
        }
        let ended: Vec<String> = self
            .approvals
            .iter()
            .filter(|(_, pending)| pending.thread_id == thread_id)
            .map(|(id, _)| id.clone())
            .collect();
        for interrupt_id in ended {
            self.approvals.remove(&interrupt_id);
            drop(self.handler.take_parked(&interrupt_id));
            self.store.resolve_interrupt(&interrupt_id)?;
        }
        Ok(())
    }
}

/// The stored task state an event implies, if it changes it.
fn task_state(event: &Event) -> Option<&'static str> {
    match event {
        Event::RunStarted(_) => Some("running"),
        Event::RunFinished(finished) => Some(match finished.outcome {
            RunOutcome::Success { .. } => "completed",
            RunOutcome::Interrupt { .. } => "paused",
        }),
        Event::RunError(error) => Some(match error.reason {
            RunErrorReason::Cancelled => "cancelled",
            _ => "failed",
        }),
        _ => None,
    }
}

/// The `RunError` code for a failed prompt (turn-event.md §4.1).
fn prompt_error_code(error: &ClientError) -> &'static str {
    match error {
        ClientError::Closed | ClientError::Io(_) => "harness_exited",
        _ => "protocol",
    }
}

/// ACP content blocks for a prompt: text as is, files as resource links (the agent fetches
/// the https URL itself).
fn content_blocks(parts: &[ContentPart]) -> Result<Vec<ContentBlock>, serde_json::Error> {
    parts
        .iter()
        .map(|part| {
            let block: Value = match part {
                ContentPart::Text { text } => json!({ "type": "text", "text": text }),
                ContentPart::File {
                    name,
                    mime_type,
                    url,
                    size,
                    ..
                } => json!({
                    "type": "resource_link",
                    "name": name,
                    "uri": url,
                    "mimeType": mime_type,
                    "size": size,
                }),
            };
            serde_json::from_value(block)
        })
        .collect()
}
