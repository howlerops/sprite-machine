//! `acp_turn`: ACP v1 → TurnEvent mapping for one thread (turn-event.md §4, §5).
//!
//! [`AcpTurnMapper`] is a pure, IO-free, clock-free state machine. Callers feed it what the
//! agent sends (`session/update` notifications, `session/request_permission`,
//! `elicitation/create`, the `session/prompt` response) plus what the runner does (start a task,
//! resume after an interrupt, cancel), and it returns envelopes with `seq: 0` (§3: the Thread DO
//! stamps `seq`, golden tests use [`ThreadSeq`](crate::turn_event::ThreadSeq)). Timestamps are
//! passed in; ids come from an injected [`IdSource`], so output is deterministic.
//!
//! Every envelope derived from an ACP payload carries it verbatim in `raw`
//! (`{source: "acp", payload}`); purely synthetic events (`RunStarted`, `TextMessageEnd`,
//! the closing `ToolCallEnd` of an aborted run, …) carry none. `raw` is dropped, never truncated,
//! when keeping it would push the envelope past 256 KiB (§2).
//!
//! Behaviours the spec leaves open are listed in `docs/ledger/reports/M1A-006a.md`.
//!
//! [`validate_sequence`] checks §5 over a list of envelopes.

mod convert;
mod state;
mod validate;

use std::collections::{BTreeMap, VecDeque};

use serde::Serialize;
use serde_json::{Map, Value, json};

use self::state::StateTracker;
pub use self::validate::{
    Rule, SequenceError, SequenceValidator, validate_complete, validate_sequence,
};
use crate::acp;
use crate::turn_event::{
    self as te, Actor, ActorKind, ContentPart, Envelope, Event, MAX_ENVELOPE_BYTES, RawPayload,
    RawSource, ResolvedOutcome, StateScope,
};

pub use self::convert::{mapped_id, tool_call_id};

/// Bytes (JSON-escaped) of `new_text` / `old_text` per diff chunk (§2: a 256 KiB envelope holds
/// this twice plus overhead).
const DIFF_CHUNK_BYTES: usize = 96 * 1024;
/// Bytes (JSON-escaped) of a text or reasoning delta per `*Content` event.
const TEXT_CHUNK_BYTES: usize = 96 * 1024;
/// Bytes (JSON-escaped) of terminal output per `append` event.
const TERMINAL_CHUNK_BYTES: usize = 64 * 1024;
/// Most terminal output (JSON-escaped bytes) kept for the closing snapshot (§4.4); older output
/// is dropped from the snapshot so it fits the envelope limit.
const TERMINAL_SNAPSHOT_BYTES: usize = 128 * 1024;

// ---- ids ----------------------------------------------------------------------------------

/// Where the mapper gets the ids it generates (`msg_`, `rsn_`, `run_`). The runner supplies
/// ULIDs; tests use [`SequentialIds`].
pub trait IdSource {
    /// A fresh, valid [`Id`](crate::turn_event::is_valid_id) starting with `prefix`
    /// (`"msg_"`, `"rsn_"`, `"run_"`).
    fn next_id(&mut self, prefix: &str) -> String;
}

/// Deterministic ids: `<prefix><n>`, counted per prefix (`msg_1`, `msg_2`, `run_1`, …).
#[derive(Debug, Clone, Default)]
pub struct SequentialIds {
    counters: BTreeMap<String, u64>,
}

impl IdSource for SequentialIds {
    fn next_id(&mut self, prefix: &str) -> String {
        let counter = self.counters.entry(prefix.to_owned()).or_insert(0);
        *counter += 1;
        format!("{prefix}{counter}")
    }
}

// ---- inputs -------------------------------------------------------------------------------

/// A prompt typed locally on a runner-opened thread. The hub emits `UserMessage` for every
/// other prompt (client-api.md §6), so the runner leaves this out.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalPrompt {
    pub parts: Vec<ContentPart>,
    pub author: Actor,
}

/// Starts a task (one prompt turn): `RunStarted{prompt}`, preceded by `UserMessage` for a
/// [`LocalPrompt`].
#[derive(Debug, Clone, PartialEq)]
pub struct StartTask {
    pub task_id: String,
    pub user_message: Option<LocalPrompt>,
    pub agent_id: Option<String>,
    pub machine_id: Option<String>,
}

impl StartTask {
    pub fn new(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            user_message: None,
            agent_id: None,
            machine_id: None,
        }
    }
}

/// The actor recorded for interrupts the runner resolves itself (cancel, expiry, errors).
pub fn runner_actor() -> Actor {
    Actor {
        kind: ActorKind::System,
        id: "runner".to_owned(),
        name: None,
        device_id: None,
    }
}

// ---- the mapper ---------------------------------------------------------------------------

/// One thread's ACP → TurnEvent mapping state (see the module docs).
#[derive(Debug)]
pub struct AcpTurnMapper<I = SequentialIds> {
    thread_id: String,
    ids: I,
    task: Option<Task>,
    terminals: BTreeMap<String, Terminal>,
    state: StateTracker,
}

#[derive(Debug)]
struct Task {
    task_id: String,
    phase: Phase,
    open_message: Option<OpenMessage>,
    tool_calls: BTreeMap<String, ToolCallState>,
    /// Inputs that arrived while the run was paused on an interrupt (§5 rule 6).
    queue: VecDeque<Queued>,
}

#[derive(Debug)]
enum Phase {
    Running {
        run_id: String,
    },
    /// The run ended `RunFinished{interrupt}`; `unresolved` are the interrupts the mapper has
    /// not been told were resolved.
    Paused {
        run_id: String,
        unresolved: Vec<String>,
    },
}

/// The text or reasoning message currently open (§4.2).
#[derive(Debug)]
struct OpenMessage {
    message_id: String,
    reasoning: bool,
    /// The ACP `messageId` of the chunks, when the agent sends one.
    acp_id: Option<String>,
}

/// What the mapper has emitted for one tool call, so updates only emit what changed.
#[derive(Debug, Default)]
struct ToolCallState {
    open: bool,
    title: Option<String>,
    kind: Option<te::ToolKind>,
    status: Option<te::ToolCallOpenStatus>,
    locations: Option<Vec<te::Location>>,
    args: Option<Value>,
    result: (Vec<ContentPart>, Option<Value>),
    /// `activity_id → signature` of the diffs emitted for this call.
    diffs: BTreeMap<String, Value>,
}

#[derive(Debug, Default)]
struct Terminal {
    tool_call_id: Option<String>,
    command: Option<String>,
    cwd: Option<String>,
    output: String,
    exit: Option<te::TerminalExit>,
}

#[derive(Debug)]
struct Queued {
    ts: String,
    input: Input,
}

#[derive(Debug)]
enum Input {
    Update {
        notification: Box<acp::SessionNotification>,
        /// The `update` object as the agent sent it, when the caller has it.
        verbatim: Option<Value>,
    },
    /// An `update` this build cannot parse (a newer `sessionUpdate`, or fields that do not fit).
    UnknownUpdate(Value),
    Permission {
        request: Box<acp::RequestPermissionRequest>,
        interrupt_id: String,
        expires_at: String,
    },
    Elicitation {
        request: Box<acp::CreateElicitationRequest>,
        interrupt_id: String,
        expires_at: String,
    },
    PromptResponse(acp::StopReason),
    TerminalStarted {
        terminal_id: String,
        tool_call_id: Option<String>,
        command: Option<String>,
        cwd: Option<String>,
    },
    TerminalOutput {
        terminal_id: String,
        output: String,
    },
    TerminalExit {
        terminal_id: String,
        exit_code: Option<i64>,
        signal: Option<String>,
    },
}

impl AcpTurnMapper<SequentialIds> {
    /// A mapper with deterministic [`SequentialIds`].
    pub fn with_sequential_ids(thread_id: impl Into<String>) -> Self {
        Self::new(thread_id, SequentialIds::default())
    }
}

impl<I: IdSource> AcpTurnMapper<I> {
    pub fn new(thread_id: impl Into<String>, ids: I) -> Self {
        Self {
            thread_id: thread_id.into(),
            ids,
            task: None,
            terminals: BTreeMap::new(),
            state: StateTracker::default(),
        }
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The running task, if any (also while it is paused on an interrupt).
    pub fn task_id(&self) -> Option<&str> {
        self.task.as_ref().map(|task| task.task_id.as_str())
    }

    /// The open run, if any (`None` while paused on an interrupt).
    pub fn run_id(&self) -> Option<&str> {
        match &self.task.as_ref()?.phase {
            Phase::Running { run_id } => Some(run_id),
            Phase::Paused { .. } => None,
        }
    }

    /// `true` while the task's run ended `RunFinished{interrupt}` and awaits [`Self::resume`].
    pub fn is_paused(&self) -> bool {
        matches!(
            self.task.as_ref().map(|task| &task.phase),
            Some(Phase::Paused { .. })
        )
    }

    // ---- lifecycle ------------------------------------------------------------------------

    /// A new prompt turn: `[UserMessage,] RunStarted{prompt}` (§4.1). A second prompt in the same
    /// thread is a new task with a new `task_id`. If a task is still open it is ended first with
    /// `RunError{error, code: "protocol"}` so the log stays valid.
    pub fn start_task(&mut self, start: StartTask, ts: &str) -> Vec<Envelope> {
        let mut out = Vec::new();
        if self.task.is_some() {
            out.extend(self.abort(
                ts,
                te::RunErrorReason::Error,
                Some("protocol"),
                "a new task started before the previous one ended",
                &runner_actor(),
                None,
            ));
        }
        let run_id = self.ids.next_id("run_");
        let mut em = Emit::new(&self.thread_id, Some(&start.task_id), None, ts);
        if let Some(prompt) = start.user_message {
            let message_id = self.ids.next_id("msg_");
            em.task(
                te::UserMessage {
                    message_id,
                    parts: prompt.parts,
                    author: prompt.author,
                },
                None,
            );
        }
        em.run_id = Some(run_id.clone());
        em.run(
            te::RunStarted {
                reason: te::RunReason::Prompt,
                parent_run_id: None,
                agent_id: start.agent_id,
                machine_id: start.machine_id,
            },
            None,
        );
        out.extend(em.out);
        self.task = Some(Task {
            task_id: start.task_id,
            phase: Phase::Running { run_id },
            open_message: None,
            tool_calls: BTreeMap::new(),
            queue: VecDeque::new(),
        });
        self.terminals.clear();
        out
    }

    /// The `session/prompt` response: closes open messages and tool calls and ends the run
    /// (§4.1): `RunFinished{success, stop_reason}` for `end_turn`, `max_tokens`,
    /// `max_turn_requests`, `refusal`; `RunError{cancelled}` for `cancelled`. While the run is
    /// paused on an interrupt the response is held and applied after [`Self::resume`].
    pub fn on_prompt_response(&mut self, stop_reason: acp::StopReason, ts: &str) -> Vec<Envelope> {
        self.dispatch(Input::PromptResponse(stop_reason), ts)
    }

    /// Resumes a run paused on an interrupt after it was answered: `RunStarted{resume,
    /// parent_run_id}` with a new `run_id`, then the updates buffered while paused (§5 rule 6).
    /// If a buffered request pauses the run again, the rest stays buffered. Empty when the task
    /// is not paused.
    pub fn resume(&mut self, ts: &str) -> Vec<Envelope> {
        let mut out = self.resume_run(ts);
        while let Some(task) = self.task.as_mut() {
            if matches!(task.phase, Phase::Paused { .. }) {
                break;
            }
            let Some(queued) = task.queue.pop_front() else {
                break;
            };
            out.extend(self.process(queued.input, &queued.ts));
        }
        out
    }

    /// Emits `InterruptResolved` (task-scoped, §4.5) for an interrupt of the paused run. The hub
    /// normally emits it when the first answer arrives; the runner uses this when it resolves an
    /// interrupt itself, and golden tests use it to build whole logs. Empty when `interrupt_id`
    /// is not an unresolved interrupt of the paused run.
    pub fn on_interrupt_resolved(
        &mut self,
        interrupt_id: &str,
        outcome: ResolvedOutcome,
        resolved_by: Actor,
        ts: &str,
    ) -> Vec<Envelope> {
        if !self.note_interrupt_resolved(interrupt_id) {
            return Vec::new();
        }
        let Some(task) = &self.task else {
            return Vec::new();
        };
        let mut em = Emit::new(&self.thread_id, Some(&task.task_id), None, ts);
        em.task(
            te::InterruptResolved {
                interrupt_id: interrupt_id.to_owned(),
                outcome,
                resolved_by,
            },
            None,
        );
        em.out
    }

    /// Records that the hub resolved `interrupt_id` (it emitted `InterruptResolved` itself), so
    /// a later cancel or error does not resolve it again. Returns whether it was unresolved.
    pub fn note_interrupt_resolved(&mut self, interrupt_id: &str) -> bool {
        let Some(Task {
            phase: Phase::Paused { unresolved, .. },
            ..
        }) = self.task.as_mut()
        else {
            return false;
        };
        let before = unresolved.len();
        unresolved.retain(|id| id != interrupt_id);
        unresolved.len() != before
    }

    /// `session/cancel`: ends the task with `RunError{cancelled}` (§4.1). Open messages are
    /// closed and open tool calls end `failed`. A task paused on interrupts first resolves them
    /// `cancelled` (by `cancelled_by`) and resumes into a run that ends at once (§5 rule 6).
    /// Updates buffered during the pause are dropped.
    pub fn on_cancel(&mut self, cancelled_by: &Actor, ts: &str) -> Vec<Envelope> {
        self.abort(
            ts,
            te::RunErrorReason::Cancelled,
            None,
            "cancelled",
            cancelled_by,
            None,
        )
    }

    /// An approval or form timed out (§5 rule 6): `InterruptResolved{expired}` (by the runner),
    /// any other open interrupt `cancelled`, then `RunError{error, code: "expired"}`.
    pub fn on_interrupt_expired(&mut self, interrupt_id: &str, ts: &str) -> Vec<Envelope> {
        self.abort(
            ts,
            te::RunErrorReason::Error,
            Some("expired"),
            "the interrupt expired before it was answered",
            &runner_actor(),
            Some((interrupt_id, ResolvedOutcome::Expired)),
        )
    }

    /// A harness crash, protocol error, timeout or budget stop (§4.1): `RunError{error, code}`.
    pub fn on_error(&mut self, code: &str, message: &str, ts: &str) -> Vec<Envelope> {
        self.abort(
            ts,
            te::RunErrorReason::Error,
            Some(code),
            message,
            &runner_actor(),
            None,
        )
    }

    // ---- ACP input ------------------------------------------------------------------------

    /// One `session/update` notification (§4.2–§4.4, §4.6). Updates that arrive while the run is
    /// paused on an interrupt are buffered and emitted after the resume `RunStarted`. Outside a
    /// task only the state updates (commands, mode, config, usage, title) produce events.
    pub fn on_session_update(
        &mut self,
        notification: &acp::SessionNotification,
        ts: &str,
    ) -> Vec<Envelope> {
        self.dispatch(
            Input::Update {
                notification: Box::new(notification.clone()),
                verbatim: None,
            },
            ts,
        )
    }

    /// [`Self::on_session_update`] for the notification as JSON (the `params` of
    /// `session/update`). `raw.payload` is the `update` object exactly as sent, and an `update`
    /// this build cannot parse (a newer `sessionUpdate`) becomes a `Raw` event instead of being
    /// lost. Errors only when `notification` has no `update` object.
    pub fn on_session_update_json(
        &mut self,
        notification: &Value,
        ts: &str,
    ) -> Result<Vec<Envelope>, serde_json::Error> {
        let Some(update) = notification
            .get("update")
            .filter(|update| update.is_object())
        else {
            return Err(<serde_json::Error as serde::de::Error>::custom(
                "a session/update notification needs an `update` object",
            ));
        };
        let input = match serde_json::from_value::<acp::SessionNotification>(notification.clone()) {
            Ok(parsed) => Input::Update {
                notification: Box::new(parsed),
                verbatim: Some(update.clone()),
            },
            Err(_) => Input::UnknownUpdate(update.clone()),
        };
        Ok(self.dispatch(input, ts))
    }

    /// The `session/new` response: a `StateSnapshot{session}` with the modes and config options
    /// (§4.6). Thread-scoped, emitted at once.
    pub fn on_session_new(
        &mut self,
        response: &acp::NewSessionResponse,
        ts: &str,
    ) -> Vec<Envelope> {
        let payload = serde_json::to_value(response).unwrap_or(Value::Null);
        let mut changes = Vec::new();
        if let Some(modes) = &response.modes {
            changes.push((
                "current_mode_id",
                Value::String(modes.current_mode_id.0.to_string()),
            ));
            changes.push(("modes", convert::modes(&modes.available_modes)));
        }
        if let Some(options) = &response.config_options {
            changes.push(("config_options", convert::config_options(options)));
        }
        let snapshot = self.state.reset_session(changes);
        let mut em = Emit::new(&self.thread_id, None, None, ts);
        em.thread(
            te::StateSnapshot {
                scope: StateScope::Session,
                snapshot,
            },
            Some(&payload),
        );
        em.out
    }

    /// `session/request_permission` (§4.5): closes open messages, upserts the tool call, emits
    /// `Interrupt{approval}` and `RunFinished{interrupt}`. The run is then paused until
    /// [`Self::resume`]; later updates are buffered.
    pub fn on_request_permission(
        &mut self,
        request: &acp::RequestPermissionRequest,
        interrupt_id: &str,
        expires_at: &str,
        ts: &str,
    ) -> Vec<Envelope> {
        self.dispatch(
            Input::Permission {
                request: Box::new(request.clone()),
                interrupt_id: interrupt_id.to_owned(),
                expires_at: expires_at.to_owned(),
            },
            ts,
        )
    }

    /// `elicitation/create` (§4.5): `Interrupt{form}` for form mode, `Interrupt{auth}` for url
    /// mode, then `RunFinished{interrupt}`. Other modes become a `Raw` event and no interrupt.
    pub fn on_elicitation(
        &mut self,
        request: &acp::CreateElicitationRequest,
        interrupt_id: &str,
        expires_at: &str,
        ts: &str,
    ) -> Vec<Envelope> {
        self.dispatch(
            Input::Elicitation {
                request: Box::new(request.clone()),
                interrupt_id: interrupt_id.to_owned(),
                expires_at: expires_at.to_owned(),
            },
            ts,
        )
    }

    // ---- client terminals (tee'd, §4.4) ---------------------------------------------------

    /// A client terminal was created (`terminal/create`): `Activity{terminal, snapshot}` with the
    /// command and cwd. `tool_call_id` is the ACP `toolCallId` it belongs to, when known.
    pub fn on_terminal_started(
        &mut self,
        terminal_id: &str,
        tool_call_id: Option<&str>,
        command: Option<&str>,
        cwd: Option<&str>,
        ts: &str,
    ) -> Vec<Envelope> {
        self.dispatch(
            Input::TerminalStarted {
                terminal_id: terminal_id.to_owned(),
                tool_call_id: tool_call_id.map(str::to_owned),
                command: command.map(str::to_owned),
                cwd: cwd.map(str::to_owned),
            },
            ts,
        )
    }

    /// Terminal output (`terminal/output`, tee'd): `Activity{terminal, append}` events of at most
    /// 64 KiB each.
    pub fn on_terminal_output(
        &mut self,
        terminal_id: &str,
        output: &str,
        ts: &str,
    ) -> Vec<Envelope> {
        self.dispatch(
            Input::TerminalOutput {
                terminal_id: terminal_id.to_owned(),
                output: output.to_owned(),
            },
            ts,
        )
    }

    /// The terminal command ended: the closing `Activity{terminal, snapshot}` repeating
    /// `terminal_id`, `command`, `cwd` and the output so far, with `exit` (§4.4).
    pub fn on_terminal_exit(
        &mut self,
        terminal_id: &str,
        exit_code: Option<i64>,
        signal: Option<&str>,
        ts: &str,
    ) -> Vec<Envelope> {
        self.dispatch(
            Input::TerminalExit {
                terminal_id: terminal_id.to_owned(),
                exit_code,
                signal: signal.map(str::to_owned),
            },
            ts,
        )
    }

    // ---- internals ------------------------------------------------------------------------

    /// Buffers `input` while the run is paused, otherwise processes it.
    fn dispatch(&mut self, input: Input, ts: &str) -> Vec<Envelope> {
        if let Some(task) = self.task.as_mut()
            && matches!(task.phase, Phase::Paused { .. })
        {
            task.queue.push_back(Queued {
                ts: ts.to_owned(),
                input,
            });
            return Vec::new();
        }
        self.process(input, ts)
    }

    /// An emitter for the current task and run (thread scope when there is none).
    fn emit(&self, ts: &str) -> Emit {
        let (task_id, run_id) = match &self.task {
            Some(Task {
                task_id,
                phase: Phase::Running { run_id },
                ..
            }) => (Some(task_id.as_str()), Some(run_id.as_str())),
            Some(task) => (Some(task.task_id.as_str()), None),
            None => (None, None),
        };
        Emit::new(&self.thread_id, task_id, run_id, ts)
    }

    fn process(&mut self, input: Input, ts: &str) -> Vec<Envelope> {
        match input {
            Input::Update {
                notification,
                verbatim,
            } => self.update(&notification, verbatim, ts),
            Input::UnknownUpdate(payload) => self.with_run(ts, |ctx| {
                ctx.close_message();
                ctx.raw(&payload);
            }),
            Input::Permission {
                request,
                interrupt_id,
                expires_at,
            } => self.permission(&request, &interrupt_id, &expires_at, ts),
            Input::Elicitation {
                request,
                interrupt_id,
                expires_at,
            } => self.elicitation(&request, &interrupt_id, &expires_at, ts),
            Input::PromptResponse(stop_reason) => self.prompt_response(stop_reason, ts),
            Input::TerminalStarted {
                terminal_id,
                tool_call_id,
                command,
                cwd,
            } => self.with_run(ts, |ctx| {
                let id = mapped_id("term_", &terminal_id);
                let terminal = ctx.terminals.entry(id.clone()).or_default();
                if let Some(tool_call_id) = tool_call_id {
                    terminal.tool_call_id = Some(convert::tool_call_id(&tool_call_id));
                }
                if command.is_some() {
                    terminal.command = command;
                }
                if cwd.is_some() {
                    terminal.cwd = cwd;
                }
                ctx.terminal_snapshot(&id, None);
            }),
            Input::TerminalOutput {
                terminal_id,
                output,
            } => self.with_run(ts, |ctx| {
                let id = mapped_id("term_", &terminal_id);
                if !ctx.terminals.contains_key(&id) {
                    ctx.terminals.insert(id.clone(), Terminal::default());
                    ctx.terminal_snapshot(&id, None);
                }
                for piece in convert::split_escaped(&output, TERMINAL_CHUNK_BYTES) {
                    if piece.is_empty() {
                        continue;
                    }
                    if let Some(terminal) = ctx.terminals.get_mut(&id) {
                        terminal.output.push_str(piece);
                    }
                    ctx.em.run(
                        te::Activity {
                            activity_id: id.clone(),
                            activity_type: te::ActivityType::Terminal,
                            mode: te::ActivityMode::Append,
                            content: Some(object(&te::TerminalContent {
                                terminal_id: id.clone(),
                                tool_call_id: None,
                                command: None,
                                cwd: None,
                                output: Some(piece.to_owned()),
                                exit: None,
                            })),
                            patch: None,
                        },
                        None,
                    );
                }
                if let Some(terminal) = ctx.terminals.get_mut(&id) {
                    let kept = convert::tail_escaped(&terminal.output, TERMINAL_SNAPSHOT_BYTES);
                    if kept.len() != terminal.output.len() {
                        terminal.output = kept.to_owned();
                    }
                }
            }),
            Input::TerminalExit {
                terminal_id,
                exit_code,
                signal,
            } => self.with_run(ts, |ctx| {
                let id = mapped_id("term_", &terminal_id);
                let terminal = ctx.terminals.entry(id.clone()).or_default();
                terminal.exit = Some(te::TerminalExit { exit_code, signal });
                ctx.terminal_snapshot(&id, None);
            }),
        }
    }

    /// Runs `f` against the open run; nothing is emitted when there is none.
    fn with_run(&mut self, ts: &str, f: impl FnOnce(&mut Ctx<'_, I>)) -> Vec<Envelope> {
        let mut em = self.emit(ts);
        if let Some(task) = self.task.as_mut()
            && matches!(task.phase, Phase::Running { .. })
        {
            let mut ctx = Ctx {
                ids: &mut self.ids,
                task,
                terminals: &mut self.terminals,
                em: &mut em,
            };
            f(&mut ctx);
        }
        em.out
    }

    fn update(
        &mut self,
        notification: &acp::SessionNotification,
        verbatim: Option<Value>,
        ts: &str,
    ) -> Vec<Envelope> {
        let payload = verbatim
            .unwrap_or_else(|| serde_json::to_value(&notification.update).unwrap_or(Value::Null));
        let mut em = self.emit(ts);
        let Self {
            ids,
            task,
            terminals,
            state,
            ..
        } = self;
        let mut task = task.as_mut();
        // Any update of a different kind closes the open message (§4.2).
        let same_kind = |reasoning: bool, update: &acp::SessionUpdate| match update {
            acp::SessionUpdate::AgentMessageChunk(_) => !reasoning,
            acp::SessionUpdate::AgentThoughtChunk(_) => reasoning,
            _ => false,
        };
        if let Some(task) = task.as_deref_mut() {
            let keep_open = task
                .open_message
                .as_ref()
                .is_some_and(|open| same_kind(open.reasoning, &notification.update));
            if !keep_open {
                close_message(&mut em, task);
            }
        }

        let mut change = |scope: StateScope, changes: Vec<(&str, Value)>| {
            emit_state(&mut em, state, scope, changes, &payload);
        };
        match &notification.update {
            acp::SessionUpdate::AvailableCommandsUpdate(update) => change(
                StateScope::Session,
                vec![("available_commands", convert::available_commands(update))],
            ),
            acp::SessionUpdate::CurrentModeUpdate(update) => change(
                StateScope::Session,
                vec![(
                    "current_mode_id",
                    Value::String(update.current_mode_id.0.to_string()),
                )],
            ),
            acp::SessionUpdate::ConfigOptionUpdate(update) => change(
                StateScope::Session,
                vec![(
                    "config_options",
                    convert::config_options(&update.config_options),
                )],
            ),
            acp::SessionUpdate::UsageUpdate(update) => {
                let cost = update.cost.as_ref().map_or(
                    Value::Null,
                    |cost| json!({"amount": cost.amount, "currency": cost.currency}),
                );
                change(
                    StateScope::Usage,
                    vec![
                        ("context_used", json!(update.used)),
                        ("context_size", json!(update.size)),
                        ("cost", cost),
                    ],
                );
            }
            acp::SessionUpdate::SessionInfoUpdate(update) => {
                // `title` absent: unchanged; `null`: cleared (ACP `MaybeUndefined`).
                let title = serde_json::to_value(update)
                    .ok()
                    .and_then(|value| value.get("title").cloned());
                if let Some(title) = title {
                    change(StateScope::Thread, vec![("title", title)]);
                }
            }
            update => {
                let Some(task) = task else {
                    return em.out;
                };
                let mut ctx = Ctx {
                    ids,
                    task,
                    terminals,
                    em: &mut em,
                };
                match update {
                    acp::SessionUpdate::AgentMessageChunk(chunk) => {
                        ctx.chunk(chunk, false, &payload);
                    }
                    acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                        ctx.chunk(chunk, true, &payload);
                    }
                    acp::SessionUpdate::ToolCall(call) => {
                        ctx.tool_call(&ToolFields::from_call(call), &payload);
                    }
                    acp::SessionUpdate::ToolCallUpdate(update) => {
                        ctx.tool_call(&ToolFields::from_update(update), &payload);
                    }
                    acp::SessionUpdate::Plan(plan) => ctx.em.run(
                        te::Activity {
                            activity_id: "plan".to_owned(),
                            activity_type: te::ActivityType::Plan,
                            mode: te::ActivityMode::Snapshot,
                            content: Some(convert::plan_content(plan)),
                            patch: None,
                        },
                        Some(&payload),
                    ),
                    // user_message_chunk (the hub emitted UserMessage), notices, compaction and
                    // anything newer than this build: kept as Raw so nothing is lost.
                    _ => ctx.raw(&payload),
                }
            }
        }
        em.out
    }

    fn permission(
        &mut self,
        request: &acp::RequestPermissionRequest,
        interrupt_id: &str,
        expires_at: &str,
        ts: &str,
    ) -> Vec<Envelope> {
        let payload = serde_json::to_value(request).unwrap_or(Value::Null);
        let tool_payload = serde_json::to_value(&request.tool_call).unwrap_or(Value::Null);
        let interrupt_id = interrupt_id.to_owned();
        let expires_at = expires_at.to_owned();
        self.with_run(ts, |ctx| {
            ctx.close_message();
            let fields = ToolFields::from_update(&request.tool_call);
            ctx.tool_call(&fields, &tool_payload);
            let tool_call_id = convert::tool_call_id(fields.acp_id);
            let title = fields
                .title
                .map(str::to_owned)
                .or_else(|| {
                    ctx.task
                        .tool_calls
                        .get(&tool_call_id)
                        .and_then(|call| call.title.clone())
                })
                .unwrap_or_else(|| "Permission requested".to_owned());
            let options = request
                .options
                .iter()
                .filter(|option| te::is_valid_id(&option.option_id.0))
                .filter_map(|option| {
                    Some(te::InterruptOption {
                        option_id: option.option_id.0.to_string(),
                        name: option.name.clone(),
                        kind: convert::permission_kind(option.kind)?,
                    })
                })
                .collect();
            ctx.interrupt(
                te::Interrupt {
                    interrupt_id,
                    kind: te::InterruptKind::Approval,
                    title,
                    message: None,
                    expires_at,
                    tool_call_id: Some(tool_call_id),
                    options: Some(options),
                    response_schema: None,
                    url: None,
                },
                &payload,
            );
        })
    }

    fn elicitation(
        &mut self,
        request: &acp::CreateElicitationRequest,
        interrupt_id: &str,
        expires_at: &str,
        ts: &str,
    ) -> Vec<Envelope> {
        let payload = serde_json::to_value(request).unwrap_or(Value::Null);
        let interrupt_id = interrupt_id.to_owned();
        let expires_at = expires_at.to_owned();
        self.with_run(ts, |ctx| {
            ctx.close_message();
            let tool_call_id = match request.scope() {
                acp::ElicitationScope::Session(scope) => scope
                    .tool_call_id
                    .as_ref()
                    .map(|id| convert::tool_call_id(&id.0)),
                _ => None,
            };
            let interrupt = match &request.mode {
                acp::ElicitationMode::Form(form) => te::Interrupt {
                    interrupt_id,
                    kind: te::InterruptKind::Form,
                    title: form
                        .requested_schema
                        .title
                        .clone()
                        .unwrap_or_else(|| "Input requested".to_owned()),
                    message: Some(request.message.clone()),
                    expires_at,
                    tool_call_id,
                    options: None,
                    response_schema: match serde_json::to_value(&form.requested_schema) {
                        Ok(Value::Object(schema)) => Some(schema),
                        _ => None,
                    },
                    url: None,
                },
                acp::ElicitationMode::Url(url) => te::Interrupt {
                    interrupt_id,
                    kind: te::InterruptKind::Auth,
                    title: "Authorization required".to_owned(),
                    message: Some(request.message.clone()),
                    expires_at,
                    tool_call_id,
                    options: None,
                    response_schema: None,
                    url: Some(url.url.clone()),
                },
                _ => {
                    ctx.raw(&payload);
                    return;
                }
            };
            ctx.interrupt(interrupt, &payload);
        })
    }

    fn prompt_response(&mut self, stop_reason: acp::StopReason, ts: &str) -> Vec<Envelope> {
        if self.task.is_none() {
            return Vec::new();
        }
        let payload =
            json!({"stopReason": serde_json::to_value(stop_reason).unwrap_or(Value::Null)});
        let (event, tool_status): (Event, _) = match stop_reason {
            acp::StopReason::Cancelled => (
                te::RunError {
                    reason: te::RunErrorReason::Cancelled,
                    code: None,
                    message: "the prompt turn was cancelled".to_owned(),
                }
                .into(),
                te::ToolCallEndStatus::Failed,
            ),
            other => {
                let (stop, tool_status) = match other {
                    acp::StopReason::EndTurn => {
                        (te::StopReason::EndTurn, te::ToolCallEndStatus::Completed)
                    }
                    acp::StopReason::MaxTokens => {
                        (te::StopReason::MaxTokens, te::ToolCallEndStatus::Failed)
                    }
                    acp::StopReason::MaxTurnRequests => (
                        te::StopReason::MaxTurnRequests,
                        te::ToolCallEndStatus::Failed,
                    ),
                    _ => (te::StopReason::Refusal, te::ToolCallEndStatus::Failed),
                };
                (
                    te::RunFinished {
                        outcome: te::RunOutcome::Success {
                            stop_reason: Some(stop),
                        },
                    }
                    .into(),
                    tool_status,
                )
            }
        };
        self.end_run(ts, tool_status, event, Some(&payload))
    }

    /// `RunStarted{resume}` for the paused run. The caller flushes the queue.
    fn resume_run(&mut self, ts: &str) -> Vec<Envelope> {
        let new_run = self.ids.next_id("run_");
        let Some(task) = self.task.as_mut() else {
            return Vec::new();
        };
        let Phase::Paused { run_id: parent, .. } = &task.phase else {
            return Vec::new();
        };
        let parent = parent.clone();
        task.phase = Phase::Running {
            run_id: new_run.clone(),
        };
        let mut em = Emit::new(&self.thread_id, Some(&task.task_id), Some(&new_run), ts);
        em.run(
            te::RunStarted {
                reason: te::RunReason::Resume,
                parent_run_id: Some(parent),
                agent_id: None,
                machine_id: None,
            },
            None,
        );
        em.out
    }

    /// Closes the open message and tool calls and emits the run's terminal `event`; the task
    /// ends. The run must be open.
    fn end_run(
        &mut self,
        ts: &str,
        tool_status: te::ToolCallEndStatus,
        event: Event,
        raw: Option<&Value>,
    ) -> Vec<Envelope> {
        let mut em = self.emit(ts);
        let Some(mut task) = self.task.take() else {
            return Vec::new();
        };
        if !matches!(task.phase, Phase::Running { .. }) {
            self.task = Some(task);
            return Vec::new();
        }
        close_message(&mut em, &mut task);
        for (tool_call_id, call) in &mut task.tool_calls {
            if call.open {
                call.open = false;
                em.run(
                    te::ToolCallEnd {
                        tool_call_id: tool_call_id.clone(),
                        status: tool_status,
                    },
                    None,
                );
            }
        }
        em.run(event, raw);
        self.terminals.clear();
        em.out
    }

    /// The next request (permission or form) buffered during a pause, dropping the updates
    /// buffered before it. `None` when the task has none left.
    fn next_queued_interrupt(&mut self) -> Option<Queued> {
        let task = self.task.as_mut()?;
        while let Some(queued) = task.queue.pop_front() {
            if matches!(
                queued.input,
                Input::Permission { .. } | Input::Elicitation { .. }
            ) {
                return Some(queued);
            }
        }
        None
    }

    /// Ends the task with `RunError` (see [`Self::on_cancel`], [`Self::on_interrupt_expired`]).
    /// A paused run first resolves its open interrupts (`first` as given, the rest `cancelled`)
    /// and resumes; each request buffered during the pause is then shown and resolved the same
    /// way before the task's `RunError`.
    fn abort(
        &mut self,
        ts: &str,
        reason: te::RunErrorReason,
        code: Option<&str>,
        message: &str,
        resolved_by: &Actor,
        first: Option<(&str, ResolvedOutcome)>,
    ) -> Vec<Envelope> {
        let mut out = Vec::new();
        let mut first = first;
        loop {
            if let Some(task) = &mut self.task
                && let Phase::Paused { unresolved, .. } = &mut task.phase
            {
                let mut em = Emit::new(&self.thread_id, Some(&task.task_id), None, ts);
                for interrupt_id in std::mem::take(unresolved) {
                    let outcome = match &first {
                        Some((id, outcome)) if *id == interrupt_id => outcome.clone(),
                        _ => ResolvedOutcome::Cancelled,
                    };
                    em.task(
                        te::InterruptResolved {
                            interrupt_id,
                            outcome,
                            resolved_by: resolved_by.clone(),
                        },
                        None,
                    );
                }
                first = None;
                out.extend(em.out);
                out.extend(self.resume_run(ts));
            }
            // A request the agent sent while the run was paused is still open to the agent:
            // it is shown in a run of its own and resolved the same way (§5 rule 6).
            let Some(queued) = self.next_queued_interrupt() else {
                break;
            };
            out.extend(self.process(queued.input, &queued.ts));
        }
        out.extend(
            self.end_run(
                ts,
                te::ToolCallEndStatus::Failed,
                te::RunError {
                    reason,
                    code: code.map(str::to_owned),
                    message: message.to_owned(),
                }
                .into(),
                None,
            ),
        );
        out
    }
}

// ---- helpers ------------------------------------------------------------------------------

/// Builds envelopes for one call: fixed thread, task and run ids and one timestamp.
struct Emit {
    thread_id: String,
    task_id: Option<String>,
    run_id: Option<String>,
    ts: String,
    out: Vec<Envelope>,
}

impl Emit {
    fn new(thread_id: &str, task_id: Option<&str>, run_id: Option<&str>, ts: &str) -> Self {
        Self {
            thread_id: thread_id.to_owned(),
            task_id: task_id.map(str::to_owned),
            run_id: run_id.map(str::to_owned),
            ts: ts.to_owned(),
            out: Vec::new(),
        }
    }

    fn push(
        &mut self,
        task_id: Option<String>,
        run_id: Option<String>,
        event: impl Into<Event>,
        raw: Option<&Value>,
    ) {
        let mut envelope = Envelope::new(&self.thread_id, task_id, run_id, &self.ts, event);
        if let Some(payload) = raw {
            envelope.raw = Some(RawPayload {
                source: RawSource::Acp,
                payload: payload.clone(),
            });
            // §2: `raw.payload` is dropped (not truncated) when it would exceed the limit.
            let too_big =
                serde_json::to_vec(&envelope).is_ok_and(|bytes| bytes.len() > MAX_ENVELOPE_BYTES);
            if too_big {
                envelope.raw = None;
            }
        }
        self.out.push(envelope);
    }

    /// Run scope: `task_id` and `run_id` set.
    fn run(&mut self, event: impl Into<Event>, raw: Option<&Value>) {
        self.push(self.task_id.clone(), self.run_id.clone(), event, raw);
    }

    /// Task scope: `task_id` set, `run_id` null.
    fn task(&mut self, event: impl Into<Event>, raw: Option<&Value>) {
        self.push(self.task_id.clone(), None, event, raw);
    }

    /// Thread scope: both null.
    fn thread(&mut self, event: impl Into<Event>, raw: Option<&Value>) {
        self.push(None, None, event, raw);
    }
}

fn object<T: Serialize>(value: &T) -> Map<String, Value> {
    match serde_json::to_value(value) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

fn close_message(em: &mut Emit, task: &mut Task) {
    match task.open_message.take() {
        Some(OpenMessage {
            message_id,
            reasoning: false,
            ..
        }) => em.run(te::TextMessageEnd { message_id }, None),
        Some(OpenMessage {
            message_id,
            reasoning: true,
            ..
        }) => em.run(te::ReasoningMessageEnd { message_id }, None),
        None => {}
    }
}

fn emit_state(
    em: &mut Emit,
    state: &mut StateTracker,
    scope: StateScope,
    changes: Vec<(&str, Value)>,
    payload: &Value,
) {
    match state.apply(scope, changes) {
        state::StateChange::Snapshot(snapshot) => {
            em.thread(te::StateSnapshot { scope, snapshot }, Some(payload));
        }
        state::StateChange::Delta(patch) => {
            em.thread(te::StateDelta { scope, patch }, Some(payload));
        }
        state::StateChange::Unchanged => {}
    }
}

/// The fields of a `tool_call`, a `tool_call_update` or the tool call of a permission request.
/// `None` means "not provided" (leave what the consumer has).
struct ToolFields<'a> {
    acp_id: &'a str,
    title: Option<&'a str>,
    kind: Option<acp::ToolKind>,
    status: Option<acp::ToolCallStatus>,
    content: Option<&'a [acp::ToolCallContent]>,
    locations: Option<&'a [acp::ToolCallLocation]>,
    raw_input: Option<&'a Value>,
    raw_output: Option<&'a Value>,
}

impl<'a> ToolFields<'a> {
    fn from_call(call: &'a acp::ToolCall) -> Self {
        Self {
            acp_id: &call.tool_call_id.0,
            title: Some(&call.title),
            kind: Some(call.kind),
            status: Some(call.status),
            content: Some(&call.content),
            locations: Some(&call.locations),
            raw_input: call.raw_input.as_ref(),
            raw_output: call.raw_output.as_ref(),
        }
    }

    fn from_update(update: &'a acp::ToolCallUpdate) -> Self {
        let fields = &update.fields;
        Self {
            acp_id: &update.tool_call_id.0,
            title: fields.title.as_deref(),
            kind: fields.kind,
            status: fields.status,
            content: fields.content.as_deref(),
            locations: fields.locations.as_deref(),
            raw_input: fields.raw_input.as_ref(),
            raw_output: fields.raw_output.as_ref(),
        }
    }
}

/// The open task plus what it needs to emit events, split from the mapper so the pieces can be
/// borrowed together.
struct Ctx<'a, I> {
    ids: &'a mut I,
    task: &'a mut Task,
    terminals: &'a mut BTreeMap<String, Terminal>,
    em: &'a mut Emit,
}

impl<I: IdSource> Ctx<'_, I> {
    fn close_message(&mut self) {
        close_message(self.em, self.task);
    }

    /// An ACP payload with no TurnEvent of its own: `Raw{source: "acp"}` (§4.7).
    fn raw(&mut self, payload: &Value) {
        let event = match payload {
            Value::Object(map) => map.clone(),
            other => {
                let mut map = Map::new();
                map.insert("value".to_owned(), other.clone());
                map
            }
        };
        self.em.run(
            te::Raw {
                source: te::RawEventSource::Acp,
                child_thread_id: None,
                event,
            },
            Some(payload),
        );
    }

    /// §4.2: opens a message on the first chunk after any other update, appends text blocks as
    /// content; the message is closed by the next update of a different kind (or the run's end).
    fn chunk(&mut self, chunk: &acp::ContentChunk, reasoning: bool, payload: &Value) {
        let acp::ContentBlock::Text(block) = &chunk.content else {
            // Images, resources, …: no *Content in v1; keep the chunk as Raw.
            self.raw(payload);
            return;
        };
        if block.text.is_empty() {
            return;
        }
        let acp_id = chunk.message_id.as_ref().map(|id| id.0.to_string());
        // A different ACP `messageId` is a new message.
        let continues = self.task.open_message.as_ref().is_some_and(|open| {
            open.reasoning == reasoning
                && match (&open.acp_id, &acp_id) {
                    (Some(open_id), Some(chunk_id)) => open_id == chunk_id,
                    _ => true,
                }
        });
        if !continues {
            self.close_message();
            let message_id = self.ids.next_id(if reasoning { "rsn_" } else { "msg_" });
            if reasoning {
                self.em.run(
                    te::ReasoningMessageStart {
                        message_id: message_id.clone(),
                        role: te::ReasoningRole::Reasoning,
                    },
                    None,
                );
            } else {
                self.em.run(
                    te::TextMessageStart {
                        message_id: message_id.clone(),
                        role: te::AssistantRole::Assistant,
                    },
                    None,
                );
            }
            self.task.open_message = Some(OpenMessage {
                message_id,
                reasoning,
                acp_id,
            });
        } else if let Some(open) = self.task.open_message.as_mut()
            && open.acp_id.is_none()
        {
            open.acp_id = acp_id;
        }
        let Some(open) = &self.task.open_message else {
            return;
        };
        let message_id = open.message_id.clone();
        // A delta that would not fit one envelope (§2) is split; the payload rides on the first.
        for (index, piece) in convert::split_escaped(&block.text, TEXT_CHUNK_BYTES)
            .into_iter()
            .enumerate()
        {
            let message_id = message_id.clone();
            let delta = piece.to_owned();
            let raw = (index == 0).then_some(payload);
            if reasoning {
                self.em
                    .run(te::ReasoningMessageContent { message_id, delta }, raw);
            } else {
                self.em
                    .run(te::TextMessageContent { message_id, delta }, raw);
            }
        }
    }

    /// §4.3: upserts a tool call by id. Emits `ToolCallStart` when the call is new or its
    /// title/kind/status/locations changed, `ToolCallArgs` for a new `rawInput`, `Activity` for
    /// diff and terminal content, `ToolCallResult` for changed content/`rawOutput`, and
    /// `ToolCallEnd` for `completed`/`failed`.
    fn tool_call(&mut self, fields: &ToolFields<'_>, payload: &Value) {
        let id = convert::tool_call_id(fields.acp_id);
        let is_new = !self.task.tool_calls.contains_key(&id);
        if self.task.tool_calls.get(&id).is_some_and(|call| !call.open) {
            // §4.3: nothing may follow a ToolCallEnd; keep the update as Raw.
            self.raw(payload);
            return;
        }
        let call = self.task.tool_calls.entry(id.clone()).or_default();
        call.open = true;

        let mut start = te::ToolCallStart {
            tool_call_id: id.clone(),
            title: None,
            kind: None,
            status: None,
            locations: None,
            parent_message_id: None,
        };
        let mut changed = is_new;
        if let Some(title) = fields.title
            && call.title.as_deref() != Some(title)
        {
            call.title = Some(title.to_owned());
            start.title = Some(title.to_owned());
            changed = true;
        }
        if let Some(kind) = fields.kind.map(convert::tool_kind)
            && call.kind != Some(kind)
        {
            call.kind = Some(kind);
            start.kind = Some(kind);
            changed = true;
        }
        if let Some(status) = fields.status.and_then(convert::open_status)
            && call.status != Some(status)
        {
            call.status = Some(status);
            start.status = Some(status);
            changed = true;
        }
        if let Some(locations) = fields.locations.filter(|locations| !locations.is_empty()) {
            let locations: Vec<te::Location> = locations.iter().map(convert::location).collect();
            if call.locations.as_ref() != Some(&locations) {
                call.locations = Some(locations.clone());
                start.locations = Some(locations);
                changed = true;
            }
        }
        if changed {
            self.em.run(start, Some(payload));
        }

        if let Some(raw_input) = fields.raw_input
            && call.args.as_ref() != Some(raw_input)
        {
            call.args = Some(raw_input.clone());
            self.em.run(
                te::ToolCallArgs {
                    tool_call_id: id.clone(),
                    raw_input: raw_input.clone(),
                },
                Some(payload),
            );
        }

        // `content` replaces the whole collection (ACP): diffs and terminals become Activity,
        // `content` items become the result.
        let mut parts = None;
        if let Some(items) = fields.content {
            let mut collected = Vec::new();
            for item in items {
                match item {
                    acp::ToolCallContent::Content(content) => {
                        collected.extend(convert::content_part(&content.content));
                    }
                    acp::ToolCallContent::Diff(diff) => self.diff(&id, diff, payload),
                    acp::ToolCallContent::Terminal(terminal) => {
                        self.terminal_content(&id, &terminal.terminal_id.0, payload);
                    }
                    _ => {}
                }
            }
            parts = Some(collected);
        }
        let Some(call) = self.task.tool_calls.get_mut(&id) else {
            return;
        };
        let result = (
            parts.unwrap_or_else(|| call.result.0.clone()),
            fields.raw_output.cloned().or_else(|| call.result.1.clone()),
        );
        if result != call.result {
            call.result = result.clone();
            self.em.run(
                te::ToolCallResult {
                    tool_call_id: id.clone(),
                    content: result.0,
                    raw_output: result.1,
                },
                Some(payload),
            );
        }

        if let Some(status) = fields.status.and_then(convert::end_status) {
            call.open = false;
            self.em.run(
                te::ToolCallEnd {
                    tool_call_id: id,
                    status,
                },
                Some(payload),
            );
        }
    }

    /// ACP `diff` content → `Activity{diff}` (§4.4), split into chunks when it would not fit
    /// the envelope limit (§2). Identical diffs resent by later updates are not re-emitted.
    fn diff(&mut self, tool_call_id: &str, diff: &acp::Diff, payload: &Value) {
        let path = diff.path.to_string_lossy().into_owned();
        let mut activity_id = format!("{tool_call_id}:{path}");
        while activity_id.len() > 1024 {
            activity_id.pop();
        }
        let signature = json!([diff.old_text, diff.new_text]);
        let Some(call) = self.task.tool_calls.get_mut(tool_call_id) else {
            return;
        };
        if call.diffs.get(&activity_id) == Some(&signature) {
            return;
        }
        call.diffs.insert(activity_id.clone(), signature);

        let new_pieces = convert::split_escaped(&diff.new_text, DIFF_CHUNK_BYTES);
        let old_pieces = diff
            .old_text
            .as_deref()
            .map(|old| convert::split_escaped(old, DIFF_CHUNK_BYTES));
        let total = new_pieces
            .len()
            .max(old_pieces.as_ref().map_or(0, Vec::len));
        for index in 0..total {
            let chunked = total > 1;
            let content = te::DiffContent {
                tool_call_id: Some(tool_call_id.to_owned()),
                path: path.clone(),
                old_text: old_pieces
                    .as_ref()
                    .map(|pieces| pieces.get(index).copied().unwrap_or_default().to_owned()),
                new_text: new_pieces
                    .get(index)
                    .copied()
                    .unwrap_or_default()
                    .to_owned(),
                chunk: chunked.then_some(te::Chunk {
                    index: index as u64,
                    last: index + 1 == total,
                }),
            };
            self.em.run(
                te::Activity {
                    activity_id: activity_id.clone(),
                    activity_type: te::ActivityType::Diff,
                    mode: if index == 0 {
                        te::ActivityMode::Snapshot
                    } else {
                        te::ActivityMode::Append
                    },
                    content: Some(object(&content)),
                    patch: None,
                },
                // One copy of the payload is enough; it may not even fit once.
                (index == 0).then_some(payload),
            );
        }
    }

    /// ACP `terminal` content → `Activity{terminal, snapshot}` linking the terminal to the tool
    /// call (§4.4). A terminal the tee already reported keeps its output: it is only re-emitted
    /// (in full) the first time it learns its `tool_call_id`.
    fn terminal_content(&mut self, tool_call_id: &str, acp_terminal_id: &str, payload: &Value) {
        let id = mapped_id("term_", acp_terminal_id);
        match self.terminals.get_mut(&id) {
            Some(terminal) if terminal.tool_call_id.is_some() => return,
            Some(terminal) => terminal.tool_call_id = Some(tool_call_id.to_owned()),
            None => {
                self.terminals.insert(
                    id.clone(),
                    Terminal {
                        tool_call_id: Some(tool_call_id.to_owned()),
                        ..Terminal::default()
                    },
                );
            }
        }
        self.terminal_snapshot(&id, Some(payload));
    }

    /// The full state of terminal `id` as an `Activity{terminal, snapshot}`.
    fn terminal_snapshot(&mut self, id: &str, payload: Option<&Value>) {
        let Some(terminal) = self.terminals.get(id) else {
            return;
        };
        let content = te::TerminalContent {
            terminal_id: id.to_owned(),
            tool_call_id: terminal.tool_call_id.clone(),
            command: terminal.command.clone(),
            cwd: terminal.cwd.clone(),
            output: (!terminal.output.is_empty()).then(|| terminal.output.clone()),
            exit: terminal.exit.clone(),
        };
        self.em.run(
            te::Activity {
                activity_id: id.to_owned(),
                activity_type: te::ActivityType::Terminal,
                mode: te::ActivityMode::Snapshot,
                content: Some(object(&content)),
                patch: None,
            },
            payload,
        );
    }

    /// `Interrupt` + `RunFinished{interrupt}`: the run pauses (§5 rule 6).
    fn interrupt(&mut self, interrupt: te::Interrupt, payload: &Value) {
        let interrupt_id = interrupt.interrupt_id.clone();
        self.em.run(interrupt, Some(payload));
        let Phase::Running { run_id } = &self.task.phase else {
            return;
        };
        let run_id = run_id.clone();
        self.em.run(
            te::RunFinished {
                outcome: te::RunOutcome::Interrupt {
                    interrupt_ids: vec![interrupt_id.clone()],
                },
            },
            None,
        );
        self.task.phase = Phase::Paused {
            run_id,
            unresolved: vec![interrupt_id],
        };
    }
}
