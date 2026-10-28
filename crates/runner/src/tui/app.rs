//! The TUI as a state machine. Input events and client-core messages go in; [`Action`]s for the
//! outside world come out; [`super::render`] draws the state. Nothing here touches a terminal,
//! the network or the clock, so the tests drive it with a fake hub.
//!
//! The thread is read through client-core ([`ThreadSync`]): the same dedupe-by-seq reducer the
//! phone's page uses. A prompt typed here and a prompt typed on the phone are both `UserMessage`
//! events of the thread, so they show in the one order the hub gave them. This module does not
//! add a typed message to the transcript itself.

use client_core::sync::Incoming;
use client_core::view::{InterruptView, LocalAnswer, TaskView};
use client_core::{Applied, Connectivity, IntentState, TaskStatus, ThreadSync, ThreadView};
use protocol::tunnel::{Actor, ActorKind, Outcome};
use protocol::turn_event::InterruptKind;

use crate::settings::Settings;

/// Lines moved by one PageUp / PageDown.
const PAGE: usize = 5;

/// A key, as far as the TUI cares. The terminal glue ([`super::term`]) maps crossterm's to this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Enter,
    Backspace,
    Esc,
    Up,
    Down,
    PageUp,
    PageDown,
}

/// Whether this session is registered with the hub, so that other devices can see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remote {
    Off,
    /// Registration was asked for and has not answered.
    Connecting,
    /// Registered; `url` is the session link (`<hub>/t/<id>`).
    On {
        url: String,
    },
    /// The last registration failed.
    Failed {
        reason: String,
    },
}

/// What the hub (or the local host) said to an answer this device sent.
#[derive(Debug, Clone, PartialEq)]
pub enum AnswerResult {
    Accepted,
    /// `409 already_resolved`: another device answered first. `by` is who, when the hub said.
    AlreadyAnswered {
        by: Option<Actor>,
    },
    /// Not delivered for good (for example the interrupt expired).
    Failed(String),
}

impl AnswerResult {
    /// The result of an answer that went through client-core's outbox: a `2xx` is accepted, a
    /// `409 already_resolved` means another device won, and anything the hub refused for good (or
    /// the outbox gave up on) is a failure. `None` while the intent is still queued.
    pub fn from_intent_state(state: &IntentState) -> Option<Self> {
        match state {
            IntentState::Queued => None,
            IntentState::Done { .. } => Some(Self::Accepted),
            IntentState::Conflict { resolved_by, .. } => Some(Self::AlreadyAnswered {
                by: resolved_by.clone(),
            }),
            IntentState::Rejected { status, error } => {
                Some(Self::Failed(format!("HTTP {status}: {error}")))
            }
            IntentState::Exhausted { last_error } => Some(Self::Failed(last_error.clone())),
        }
    }
}

/// What the TUI asks the outside world to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Register this session with the hub (`/rc`, or at start with `autoConnect`).
    Register,
    /// Stop sharing it.
    Unregister,
    /// Send a prompt. The ids are new and unique to the session.
    Prompt {
        task_id: String,
        message_id: String,
        text: String,
    },
    /// Answer an interrupt.
    Answer {
        interrupt_id: String,
        outcome: Outcome,
    },
    /// Cancel a running task.
    Cancel {
        task_id: String,
    },
    Quit,
}

/// The whole UI state.
pub struct App {
    sync: ThreadSync,
    auto_connect: bool,
    remote: Remote,
    connectivity: Connectivity,
    input: String,
    notice: Option<String>,
    /// The option highlighted in the open approval.
    selected: usize,
    /// Lines scrolled back from the newest.
    scroll: usize,
    seed: String,
    counter: u64,
    quit: bool,
}

impl App {
    /// `seed` makes the ids of the prompts this app sends unique to the session (letters and
    /// digits). `settings` are the runner's settings: with `auto_connect` the session registers
    /// on [`App::start`].
    pub fn new(thread_id: &str, settings: Settings, seed: &str) -> Self {
        Self {
            sync: ThreadSync::new(thread_id),
            auto_connect: settings.auto_connect,
            remote: Remote::Off,
            connectivity: Connectivity::Connecting,
            input: String::new(),
            notice: None,
            selected: 0,
            scroll: 0,
            seed: seed.to_owned(),
            counter: 0,
            quit: false,
        }
    }

    /// Called once when the session starts. With `autoConnect` the session registers now;
    /// without, only `/rc` does.
    pub fn start(&mut self) -> Vec<Action> {
        if self.auto_connect {
            self.remote = Remote::Connecting;
            return vec![Action::Register];
        }
        Vec::new()
    }

    // ---- state the renderer reads -----------------------------------------------------------

    pub fn view(&self) -> &ThreadView {
        self.sync.view()
    }

    pub fn remote(&self) -> &Remote {
        &self.remote
    }

    pub fn connectivity(&self) -> Connectivity {
        self.connectivity
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// The approval the user can answer now: the first open one this device has not answered.
    pub fn pending_approval(&self) -> Option<&InterruptView> {
        self.sync
            .view()
            .open_interrupts()
            .into_iter()
            .find(|interrupt| {
                interrupt.kind() == InterruptKind::Approval
                    && !interrupt.options().is_empty()
                    && !matches!(interrupt.local, Some(LocalAnswer::Pending))
            })
    }

    /// The newest task that has not ended.
    fn running_task(&self) -> Option<&TaskView> {
        self.sync
            .view()
            .tasks()
            .iter()
            .rev()
            .find(|task| !task.status().is_finished())
    }

    /// Whether a task is running (the renderer shows it in the status line).
    pub fn busy(&self) -> bool {
        self.running_task().is_some()
    }

    // ---- events in --------------------------------------------------------------------------

    /// One message of the thread subscription (or of the local host before the session is shared).
    pub fn incoming(&mut self, incoming: Incoming) {
        match self.sync.feed(incoming) {
            Ok(Applied::Event(_)) => {
                // A new answer to the highlighted approval can leave the selection out of range.
                self.selected = 0;
            }
            Ok(_) => {}
            Err(error) => self.notice = Some(format!("hub message refused: {error}")),
        }
    }

    pub fn set_notice(&mut self, notice: String) {
        self.notice = Some(notice);
    }

    /// Where the link to the hub stands, for the status line.
    pub fn set_connectivity(&mut self, connectivity: Connectivity) {
        self.connectivity = connectivity;
    }

    /// Replaces the thread (for example when the session starts being read from the hub instead
    /// of from the local host). Everything shown is rebuilt from the new sync.
    pub fn replace_sync(&mut self, sync: ThreadSync) {
        self.sync = sync;
        self.selected = 0;
    }

    /// The answer to [`Action::Register`].
    pub fn registered(&mut self, result: Result<String, String>) {
        match result {
            Ok(url) => {
                self.notice = Some(format!("remote on: {url}"));
                self.remote = Remote::On { url };
            }
            Err(reason) => {
                self.notice = Some(format!("remote failed: {reason}"));
                self.remote = Remote::Failed { reason };
            }
        }
    }

    /// The result of an [`Action::Answer`] this device sent.
    pub fn answered(&mut self, interrupt_id: &str, result: AnswerResult) {
        match result {
            // The hub's `InterruptResolved` follows and clears the pending mark.
            AnswerResult::Accepted => {}
            AnswerResult::AlreadyAnswered { by } => {
                let winner = by.unwrap_or_else(|| Actor {
                    kind: ActorKind::User,
                    id: "unknown".to_owned(),
                    name: Some("another device".to_owned()),
                    device_id: None,
                });
                let who = winner.name.clone().unwrap_or_else(|| winner.id.clone());
                let view = self.sync.view_mut();
                view.set_local_answer(interrupt_id, None);
                view.note_conflict(interrupt_id, winner);
                self.notice = Some(format!("already answered by {who}"));
            }
            AnswerResult::Failed(message) => {
                self.sync
                    .view_mut()
                    .set_local_answer(interrupt_id, Some(LocalAnswer::Failed { message }));
                self.notice = Some("answer not sent".to_owned());
            }
        }
    }

    /// Sharing ended (after [`Action::Unregister`]).
    pub fn unregistered(&mut self) {
        self.remote = Remote::Off;
        self.notice = Some("remote off".to_owned());
    }

    // ---- keys -------------------------------------------------------------------------------

    pub fn key(&mut self, key: Key) -> Vec<Action> {
        match key {
            Key::Ctrl('c') => return self.quit_now(),
            Key::Ctrl('d') if self.input.is_empty() => return self.quit_now(),
            Key::Ctrl('u') => self.input.clear(),
            Key::Char(c) => {
                self.notice = None;
                self.input.push(c);
            }
            Key::Backspace => {
                self.input.pop();
            }
            Key::Esc => self.input.clear(),
            Key::Up => self.select(-1),
            Key::Down => self.select(1),
            Key::PageUp => self.scroll = self.scroll.saturating_add(PAGE),
            Key::PageDown => self.scroll = self.scroll.saturating_sub(PAGE),
            Key::Enter => return self.enter(),
            Key::Ctrl(_) => {}
        }
        Vec::new()
    }

    fn quit_now(&mut self) -> Vec<Action> {
        self.quit = true;
        vec![Action::Quit]
    }

    fn select(&mut self, step: isize) {
        let count = self.pending_approval().map_or(0, |a| a.options().len());
        if count > 0 {
            self.selected = self.selected.saturating_add_signed(step).min(count - 1);
        }
    }

    fn enter(&mut self) -> Vec<Action> {
        let text = std::mem::take(&mut self.input);
        let text = text.trim();
        if text.is_empty() {
            return self.answer_selected();
        }
        self.scroll = 0;
        match text.strip_prefix('/') {
            Some(command) => self.command(command.trim()),
            None => self.prompt(text),
        }
    }

    /// Enter on an empty line answers the open approval with the highlighted option.
    fn answer_selected(&mut self) -> Vec<Action> {
        let Some(approval) = self.pending_approval() else {
            return Vec::new();
        };
        let Some(option) = approval.options().get(self.selected) else {
            return Vec::new();
        };
        let interrupt_id = approval.id().to_owned();
        let outcome = Outcome::Selected {
            option_id: option.option_id.clone(),
        };
        self.sync
            .view_mut()
            .set_local_answer(&interrupt_id, Some(LocalAnswer::Pending));
        self.notice = None;
        vec![Action::Answer {
            interrupt_id,
            outcome,
        }]
    }

    fn prompt(&mut self, text: &str) -> Vec<Action> {
        self.counter += 1;
        let n = self.counter;
        let seed = &self.seed;
        vec![Action::Prompt {
            task_id: format!("tk_{seed}{n}"),
            message_id: format!("msg_{seed}{n}"),
            text: text.to_owned(),
        }]
    }

    fn command(&mut self, command: &str) -> Vec<Action> {
        match command {
            "rc" => match self.remote {
                Remote::Off | Remote::Failed { .. } => {
                    self.remote = Remote::Connecting;
                    self.notice = Some("connecting to the hub...".to_owned());
                    vec![Action::Register]
                }
                Remote::On { .. } => {
                    self.remote = Remote::Off;
                    self.notice = Some("remote off".to_owned());
                    vec![Action::Unregister]
                }
                Remote::Connecting => {
                    self.notice = Some("still connecting to the hub".to_owned());
                    Vec::new()
                }
            },
            "cancel" => match self.running_task() {
                Some(task) => vec![Action::Cancel {
                    task_id: task.task_id.clone(),
                }],
                None => {
                    self.notice = Some("nothing is running".to_owned());
                    Vec::new()
                }
            },
            "quit" | "exit" => self.quit_now(),
            other => {
                self.notice = Some(format!(
                    "unknown command /{other} (try /rc, /cancel, /quit)"
                ));
                Vec::new()
            }
        }
    }

    /// The status of the newest task, for the status line.
    pub fn status(&self) -> Option<TaskStatus> {
        self.sync.view().tasks().last().map(TaskView::status)
    }
}
