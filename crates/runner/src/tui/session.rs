//! The TUI's own session: the log the screen follows, and the two ways it is shared.
//!
//! The terminal session runs its agent through the same [`sidecar::bridge::Bridge`] as `sm rc`.
//! This module is the bridge's sink and command source, and the [`Backend`] of the UI:
//!
//! - **The log.** Everything the screen shows is one numbered list of `TurnEvent` envelopes
//!   (`seq` 1, 2, ...), in the order this runner saw it: what the bridge produces; the
//!   `UserMessage` of a prompt that came from a phone and the `InterruptResolved` of an answer
//!   that came from a phone (the hub has already put both in its own log and sends the runner
//!   only the command, so the runner writes them here); and the `InterruptResolved` of an answer
//!   typed here. The UI reads it through client-core's `ThreadSync` like any other thread.
//! - **Sharing.** While the session is registered, the events the hub does not have already (the
//!   bridge's output and the answers typed here) also go up to it, after a `thread_open`; what
//!   happened before `/rc` goes first, in order. `/rc` again stops sending; the next `/rc` sends
//!   what was missed.
//! - **The hub orders prompts.** While the session is shared, a line typed here goes up as a
//!   `prompt_submit` (tunnel.md §3.6b) and is not given to the bridge; the Thread DO queues it
//!   like a phone's prompt and the `prompt` command that comes back is what the bridge runs. A
//!   session that is not shared sends its lines straight to the bridge.
//! - **First answer wins.** While the session is shared the hub decides (tunnel.md §3.6a): an
//!   answer typed here goes up as an `interrupt_answer` and is applied only after the hub accepts
//!   it, through the `approval` command the hub sends back like it does for a phone's answer.
//!   If the phone answered first the hub says so ([`AnswerResult::AlreadyAnswered`] with the
//!   winner) and the agent runs the phone's choice. Only a session that is not shared decides
//!   alone: the first answer to reach its own log wins, and the bridge refuses a second
//!   command (`unknown_interrupt`).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use client_core::Connectivity;
use client_core::sync::Incoming;
use protocol::tunnel::{Actor, ActorKind, Command, ContentPart, Outcome, Workspace};
use protocol::turn_event::{
    Envelope, Event, InterruptResolved, ResolvedOutcome, RunFinished, RunOutcome, UserMessage,
};
use sidecar::acp::reattach::rfc3339;
use sidecar::bridge::{
    CommandFailure, CommandSource, EventSink, IncomingCommand, Origin, SinkError,
};
use tokio::sync::{Mutex, mpsc};

use super::app::AnswerResult;
use super::driver::{Backend, Feed};

/// The future of [`Upstream::send`].
pub type UpstreamFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// The hub, as far as a shared session needs it. The production one is the tunnel
/// ([`super::host`]); the tests record what is sent.
pub trait Upstream: Send + Sync {
    /// Registers the thread (`thread_open`). Called once per session.
    fn open(
        &self,
        thread_id: &str,
        title: &str,
        agent: &str,
        workspace: &Workspace,
    ) -> Result<(), String>;

    /// [`Upstream::open`] for a session whose agent is already running a task that started
    /// before the registration (`running_task`, tunnel.md §3.4): the hub counts that task as
    /// busy from the moment the thread exists. The default ignores the task.
    fn open_running(
        &self,
        thread_id: &str,
        title: &str,
        agent: &str,
        workspace: &Workspace,
        running_task: Option<&str>,
    ) -> Result<(), String> {
        let _ = running_task;
        self.open(thread_id, title, agent, workspace)
    }

    /// Sends envelopes (producer side, `seq` 0) in order.
    fn send<'a>(&'a self, envelopes: Vec<Envelope>) -> UpstreamFuture<'a>;

    /// Sends a prompt typed here up (`prompt_submit`, tunnel.md §3.6b). Nothing is applied
    /// here: the hub queues it with the phones' prompts and sends it back as a `prompt` command
    /// in queue order. `Ok` means it is queued for sending, not that it ran.
    fn prompt(
        &self,
        thread_id: &str,
        task_id: &str,
        message_id: &str,
        parts: &[ContentPart],
    ) -> Result<(), String>;

    /// Sends an interrupt answer typed here and waits for the hub's verdict (tunnel.md §3.6a).
    /// `Err` is "no verdict"; the answer then is not applied here.
    fn answer<'a>(
        &'a self,
        thread_id: &'a str,
        interrupt_id: &'a str,
        outcome: &'a Outcome,
    ) -> HubAnswerFuture<'a>;
}

/// The hub's verdict on an answer typed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubVerdict {
    /// This answer won: the hub's `approval` command applies it.
    Accepted,
    /// Another answer won first.
    AlreadyAnswered { by: Option<Actor> },
    /// Unknown, expired or not allowed.
    Rejected(String),
}

/// The future of [`Upstream::answer`].
pub type HubAnswerFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HubVerdict, String>> + Send + 'a>>;

/// What a successful registration gives back.
pub struct Registration {
    pub upstream: Arc<dyn Upstream>,
    /// The session link (`<hub>/t/<thread id>`).
    pub url: String,
}

/// Starts the connection to the hub when `/rc` first asks for it. It gets the session's
/// [`Handle`], which the connection's command task feeds.
pub type Registrar = Box<
    dyn Fn(Handle) -> Pin<Box<dyn Future<Output = Result<Registration, String>> + Send>>
        + Send
        + Sync,
>;

/// The `seq` and key of each hub command the bridge has processed, for the tunnel to ack.
pub type Processed = mpsc::UnboundedReceiver<(u64, String)>;

/// Who the terminal's own prompts and answers are from.
pub fn local_actor() -> Actor {
    Actor {
        kind: ActorKind::User,
        id: "local".to_owned(),
        name: Some("terminal".to_owned()),
        device_id: None,
    }
}

struct Entry {
    envelope: Envelope,
    /// The hub does not have this event: it goes up while the session is shared.
    to_hub: bool,
    sent: bool,
}

struct Inner {
    thread_id: String,
    title: String,
    agent: String,
    workspace: Workspace,
    log: Vec<Entry>,
    feed: mpsc::UnboundedSender<Feed>,
    /// Set while the session is shared.
    upstream: Option<Arc<dyn Upstream>>,
    opened: bool,
    seen_keys: HashSet<String>,
    /// The task of each prompt typed here, by its command key, until the bridge takes it. A
    /// refusal of one of these goes back to the UI as [`Feed::Refused`].
    local_prompts: HashMap<String, String>,
    next_key: u64,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Inner {
    fn ts(&self) -> String {
        rfc3339((self.now_ms)())
    }

    /// Appends an event to the log, numbered, and shows it. Returns its index.
    fn push(&mut self, mut envelope: Envelope, to_hub: bool) {
        envelope.seq = self.log.len() as u64 + 1;
        // The bridge took a prompt typed here once its task has events: it is no longer one the
        // bridge could refuse.
        if let Some(task) = &envelope.task_id {
            self.local_prompts
                .retain(|_, pending| pending.as_str() != task.as_str());
        }
        let _ = self
            .feed
            .send(Feed::Thread(Incoming::Event(Box::new(envelope.clone()))));
        self.log.push(Entry {
            envelope,
            to_hub,
            sent: false,
        });
    }

    /// The resolution of an interrupt, if the log has one.
    fn resolution(&self, interrupt_id: &str) -> Option<&InterruptResolved> {
        self.log
            .iter()
            .find_map(|entry| match &entry.envelope.event {
                Event::InterruptResolved(resolved) if resolved.interrupt_id == interrupt_id => {
                    Some(resolved)
                }
                _ => None,
            })
    }

    /// The task the agent is running now: the last one in the log that has begun (its
    /// `UserMessage`) and not ended (`RunFinished{success}` or `RunError`, as the hub counts it).
    fn running_task(&self) -> Option<String> {
        let mut running: Option<&str> = None;
        for entry in &self.log {
            let Some(task) = entry.envelope.task_id.as_deref() else {
                continue;
            };
            match &entry.envelope.event {
                Event::UserMessage(_) => running = Some(task),
                Event::RunFinished(RunFinished {
                    outcome: RunOutcome::Success { .. },
                    ..
                })
                | Event::RunError(_)
                    if running == Some(task) =>
                {
                    running = None;
                }
                _ => {}
            }
        }
        running.map(str::to_owned)
    }

    /// The task an interrupt belongs to.
    fn task_of(&self, interrupt_id: &str) -> Option<String> {
        self.log
            .iter()
            .find_map(|entry| match &entry.envelope.event {
                Event::Interrupt(interrupt) if interrupt.interrupt_id == interrupt_id => {
                    entry.envelope.task_id.clone()
                }
                _ => None,
            })
    }

    fn command_key(&mut self) -> String {
        self.next_key += 1;
        format!("idk_local_{}", self.next_key)
    }

    /// Sends what the hub is missing, in order. Marks each as sent only when it went.
    async fn flush(&mut self) -> Result<(), String> {
        let Some(upstream) = self.upstream.clone() else {
            return Ok(());
        };
        let pending: Vec<usize> = (0..self.log.len())
            .filter(|&i| self.log[i].to_hub && !self.log[i].sent)
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        // The hub numbers them: they go up as the producer made them.
        let batch = pending
            .iter()
            .map(|&i| {
                let mut envelope = self.log[i].envelope.clone();
                envelope.seq = 0;
                envelope
            })
            .collect();
        upstream.send(batch).await?;
        for i in pending {
            self.log[i].sent = true;
        }
        Ok(())
    }
}

/// A clonable handle on the session: what the tunnel's command task and the UI both use.
#[derive(Clone)]
pub struct Handle {
    inner: Arc<Mutex<Inner>>,
    commands: mpsc::UnboundedSender<IncomingCommand>,
    processed: Arc<std::sync::Mutex<Option<Processed>>>,
}

impl Handle {
    /// The hub commands the bridge has processed. Taken once, by the task that acks them.
    /// How many prompts typed here the bridge has not taken yet (or refused). For tests.
    pub async fn tracked_prompts(&self) -> usize {
        self.inner.lock().await.local_prompts.len()
    }

    pub fn take_processed(&self) -> Option<Processed> {
        self.processed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// A command from the hub (a phone's prompt, a phone's answer, a cancel). The log gets what
    /// the hub already has but this runner does not, then the bridge gets the command.
    pub async fn hub_command(&self, command: IncomingCommand) {
        let mut inner = self.inner.lock().await;
        // A command resent after a reconnect is in the log already.
        let fresh = inner.seen_keys.insert(command.idempotency_key.clone());
        if fresh {
            let ts = inner.ts();
            let thread_id = inner.thread_id.clone();
            match &command.command {
                Command::Prompt {
                    task_id,
                    message_id,
                    parts,
                    author,
                    ..
                } => {
                    let message = UserMessage {
                        message_id: message_id.clone(),
                        parts: parts.clone(),
                        author: author.clone(),
                    };
                    let envelope =
                        Envelope::new(thread_id, Some(task_id.clone()), None, ts, message);
                    inner.push(envelope, false);
                }
                Command::Approval {
                    task_id,
                    interrupt_id,
                    outcome,
                    resolved_by,
                    ..
                } if inner.resolution(interrupt_id).is_none() => {
                    let resolved = InterruptResolved {
                        interrupt_id: interrupt_id.clone(),
                        outcome: resolved_outcome(outcome),
                        resolved_by: resolved_by.clone(),
                    };
                    let envelope =
                        Envelope::new(thread_id, Some(task_id.clone()), None, ts, resolved);
                    inner.push(envelope, false);
                }
                _ => {}
            }
        }
        let _ = self.commands.send(command);
    }
}

fn resolved_outcome(outcome: &Outcome) -> ResolvedOutcome {
    match outcome {
        Outcome::Selected { option_id } => ResolvedOutcome::Selected {
            option_id: option_id.clone(),
        },
        Outcome::Submitted { data } => ResolvedOutcome::Submitted { data: data.clone() },
        Outcome::Declined => ResolvedOutcome::Declined,
        Outcome::Cancelled => ResolvedOutcome::Cancelled,
    }
}

/// The bridge's [`EventSink`]: logs what the bridge produces, shows it, and sends it up while
/// the session is shared.
pub struct LogSink {
    inner: Arc<Mutex<Inner>>,
}

impl EventSink for LogSink {
    async fn send(&self, envelopes: Vec<Envelope>) -> Result<(), SinkError> {
        let mut inner = self.inner.lock().await;
        for envelope in envelopes {
            inner.push(envelope, true);
        }
        // The screen has it either way; a hub that is down is retried by the next send.
        if let Err(error) = inner.flush().await {
            tracing::warn!("could not send to the hub: {error}");
        }
        Ok(())
    }

    async fn command_result(&self, failure: CommandFailure) -> Result<(), SinkError> {
        // A command the bridge refused is the bridge's to tell the hub. A prompt typed here comes
        // back to the screen, which queues it again.
        tracing::warn!(
            code = ?failure.code,
            "command {} refused: {}",
            failure.idempotency_key,
            failure.message
        );
        let mut inner = self.inner.lock().await;
        if let Some(task_id) = inner.local_prompts.remove(&failure.idempotency_key) {
            let _ = inner.feed.send(Feed::Refused {
                task_id,
                message: failure.message,
            });
        }
        Ok(())
    }
}

/// The bridge's [`CommandSource`]: commands typed here and commands the hub sent.
pub struct Commands {
    rx: mpsc::UnboundedReceiver<IncomingCommand>,
    /// Each processed hub command (`seq` and key), for the tunnel to ack. Commands typed here
    /// have no hub seq and are not reported.
    processed: mpsc::UnboundedSender<(u64, String)>,
}

impl CommandSource for Commands {
    async fn next(&mut self) -> Option<IncomingCommand> {
        self.rx.recv().await
    }

    async fn processed(&mut self, seq: u64, idempotency_key: &str) {
        if seq != 0 {
            let _ = self.processed.send((seq, idempotency_key.to_owned()));
        }
    }
}

/// The session, as the UI's [`Backend`].
pub struct Session {
    handle: Handle,
    feed: mpsc::UnboundedReceiver<Feed>,
    registrar: Registrar,
}

/// What a session is called and where it runs (`thread_open` carries both).
pub struct Identity {
    pub thread_id: String,
    pub title: String,
    pub agent: String,
    pub workspace: Workspace,
}

impl Session {
    /// A session with its sink and command source, to hand to a `Bridge`. `now_ms` is the clock
    /// for the events the session writes itself. `registrar` connects to the hub on `/rc`.
    pub fn new(
        identity: Identity,
        now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
        registrar: Registrar,
    ) -> (Self, LogSink, Commands) {
        let (feed_tx, feed) = mpsc::unbounded_channel();
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let (processed_tx, processed_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Mutex::new(Inner {
            thread_id: identity.thread_id,
            title: identity.title,
            agent: identity.agent,
            workspace: identity.workspace,
            log: Vec::new(),
            feed: feed_tx,
            upstream: None,
            opened: false,
            seen_keys: HashSet::new(),
            local_prompts: HashMap::new(),
            next_key: 0,
            now_ms,
        }));
        let sink = LogSink {
            inner: Arc::clone(&inner),
        };
        let session = Self {
            handle: Handle {
                inner,
                commands: commands_tx,
                processed: Arc::new(std::sync::Mutex::new(Some(processed_rx))),
            },
            feed,
            registrar,
        };
        let commands = Commands {
            rx: commands_rx,
            processed: processed_tx,
        };
        (session, sink, commands)
    }

    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    async fn send_command(
        &self,
        track: Option<String>,
        make: impl FnOnce(&str) -> Command,
    ) -> String {
        let mut inner = self.handle.inner.lock().await;
        let key = inner.command_key();
        if let Some(task_id) = track {
            inner.local_prompts.insert(key.clone(), task_id);
        }
        let command = make(&inner.thread_id);
        let _ = self.handle.commands.send(IncomingCommand {
            seq: 0,
            idempotency_key: key.clone(),
            command,
            origin: Origin::Local,
        });
        key
    }
}

impl Backend for Session {
    async fn register(&mut self) -> Result<String, String> {
        let registration = (self.registrar)(self.handle.clone()).await?;
        let mut inner = self.handle.inner.lock().await;
        if !inner.opened {
            // A task the agent started before `/rc` is named in the registration, so the hub
            // counts it as busy before its events arrive.
            registration.upstream.open_running(
                &inner.thread_id,
                &inner.title,
                &inner.agent,
                &inner.workspace,
                inner.running_task().as_deref(),
            )?;
            inner.opened = true;
        }
        inner.upstream = Some(registration.upstream);
        let _ = inner.feed.send(Feed::Connectivity(Connectivity::Online));
        // What happened before `/rc` goes first.
        inner.flush().await?;
        Ok(registration.url)
    }

    async fn unregister(&mut self) {
        self.handle.inner.lock().await.upstream = None;
    }

    async fn prompt(
        &mut self,
        task_id: String,
        message_id: String,
        text: String,
    ) -> Result<(), String> {
        // Shared: the hub orders every prompt of the thread. The line goes up and comes back as
        // a `prompt` command; the bridge never sees it from here.
        let (upstream, thread_id) = {
            let inner = self.handle.inner.lock().await;
            (inner.upstream.clone(), inner.thread_id.clone())
        };
        if let Some(upstream) = upstream {
            return upstream.prompt(
                &thread_id,
                &task_id,
                &message_id,
                &[ContentPart::Text { text }],
            );
        }
        self.send_command(Some(task_id.clone()), |thread_id| Command::Prompt {
            thread_id: thread_id.to_owned(),
            task_id,
            message_id,
            parts: vec![ContentPart::Text { text }],
            author: local_actor(),
        })
        .await;
        Ok(())
    }

    async fn answer(&mut self, interrupt_id: &str, outcome: Outcome) -> AnswerResult {
        let mut inner = self.handle.inner.lock().await;
        if let Some(resolved) = inner.resolution(interrupt_id) {
            return AnswerResult::AlreadyAnswered {
                by: Some(resolved.resolved_by.clone()),
            };
        }
        let Some(task_id) = inner.task_of(interrupt_id) else {
            return AnswerResult::Failed("unknown approval".to_owned());
        };
        // Shared: the hub decides. Nothing is applied here; the hub's `approval` command (which
        // reaches the log and the agent like a phone's answer) is the only way it gets applied.
        if let Some(upstream) = inner.upstream.clone() {
            let thread_id = inner.thread_id.clone();
            drop(inner);
            return match upstream.answer(&thread_id, interrupt_id, &outcome).await {
                Ok(HubVerdict::Accepted) => AnswerResult::Accepted,
                Ok(HubVerdict::AlreadyAnswered { by }) => AnswerResult::AlreadyAnswered { by },
                Ok(HubVerdict::Rejected(message)) | Err(message) => AnswerResult::Failed(message),
            };
        }
        // Not shared: the first answer to reach this log wins.
        // In the log first, so the bridge's events that follow come after it.
        let resolved = InterruptResolved {
            interrupt_id: interrupt_id.to_owned(),
            outcome: resolved_outcome(&outcome),
            resolved_by: local_actor(),
        };
        let envelope = Envelope::new(
            inner.thread_id.clone(),
            Some(task_id.clone()),
            None,
            inner.ts(),
            resolved,
        );
        inner.push(envelope, true);
        let key = inner.command_key();
        let command = Command::Approval {
            thread_id: inner.thread_id.clone(),
            task_id,
            interrupt_id: interrupt_id.to_owned(),
            outcome,
            resolved_by: local_actor(),
        };
        let _ = self.handle.commands.send(IncomingCommand {
            seq: 0,
            idempotency_key: key,
            command,
            origin: Origin::Local,
        });
        AnswerResult::Accepted
    }

    async fn cancel(&mut self, task_id: &str) -> Result<(), String> {
        let task_id = task_id.to_owned();
        self.send_command(None, |thread_id| Command::Cancel {
            thread_id: thread_id.to_owned(),
            task_id,
        })
        .await;
        Ok(())
    }

    async fn next(&mut self) -> Option<Feed> {
        self.feed.recv().await
    }
}
