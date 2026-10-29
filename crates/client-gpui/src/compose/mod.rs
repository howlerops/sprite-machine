//! `compose`: the new-task composer (plan §3, surface 1). [`NewTaskModel`] (in `new.rs`) holds
//! every decision; [`ComposeView`] renders it and forwards input.
//!
//! Shortcut: Cmd+Enter on macOS, Ctrl+Enter elsewhere sends (context `Compose`).

mod new;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use gpui_kit::TestSupportExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, *};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::shell::i18n::Catalog;

pub use new::{
    AGENTS, DRAFT_FILE, Draft, Invalid, MODES, MachineOption, NewTaskModel, PreparedSend, SPAWNS,
    execute, fetch_machines, parse_machines,
};

/// The composer's English strings (`locales/en/compose.ftl`).
pub const EN_FTL: &str = include_str!("../../locales/en/compose.ftl");

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG
        .get_or_init(|| Catalog::from_source("en", EN_FTL).expect("locales/en/compose.ftl parses"))
}

fn tr(id: &str) -> String {
    catalog().tr(id, &[])
}

fn tr_args(id: &str, args: &[(&str, &str)]) -> String {
    catalog().tr(id, args)
}

actions!(compose, [SendTask]);

const CONTEXT: &str = "Compose";

/// The key that sends, as shown to the user.
pub fn send_shortcut_label() -> String {
    if cfg!(target_os = "macos") {
        tr("compose-shortcut-mac")
    } else {
        tr("compose-shortcut-other")
    }
}

/// Register the composer's key bindings. Call once, after `gpui_kit::init`. The binding is
/// scoped to inputs inside the composer so it wins over the text field's own Cmd+Enter.
pub fn init(cx: &mut App) {
    let send = if cfg!(target_os = "macos") {
        "cmd-enter"
    } else {
        "ctrl-enter"
    };
    cx.bind_keys([KeyBinding::new(send, SendTask, Some("Compose > Input"))]);
    cx.bind_keys([KeyBinding::new(send, SendTask, Some(CONTEXT))]);
}

/// Where and as whom requests go: the `…/v1` base URL and the bearer token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub api: String,
    pub token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Idle,
    Sending,
    Sent { thread_id: String },
    Failed(String),
    Invalid(Invalid),
    NoHub,
}

enum MachinesState {
    Idle,
    Loading,
    Failed(String),
}

/// Run `future` to completion on its own thread and runtime; views never touch tokio. The
/// answer is read with `try_recv` from [`ComposeView::pump`], so no foreign thread ever wakes a
/// GPUI task.
fn run_off_thread<T: Send + 'static>(
    future: impl std::future::Future<Output = T> + Send + 'static,
) -> Receiver<T> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            let _ = sender.send(runtime.block_on(future));
        }
    });
    receiver
}

type SendAnswer = Result<client_core::ThreadCreated, client_core::CreateThreadError>;

pub struct ComposeView {
    model: NewTaskModel,
    status: Status,
    machines_state: MachinesState,
    target: Option<Target>,
    machines_rx: Option<Receiver<Result<Vec<MachineOption>, String>>>,
    send_rx: Option<Receiver<SendAnswer>>,
    prompt: Entity<TextareaState>,
    workspace: Entity<InputState>,
    title: Entity<InputState>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl ComposeView {
    /// `config_dir` is where the draft is kept; `None` keeps it in memory only.
    pub fn new(config_dir: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = NewTaskModel::open(config_dir.as_deref());
        let draft = model.draft().clone();
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(4, 12)
                .placeholder(tr_args(
                    "compose-prompt-placeholder",
                    &[("shortcut", &send_shortcut_label())],
                ))
        });
        prompt.update(cx, |state, cx| {
            state.set_value(draft.prompt.clone(), window, cx);
        });
        let workspace = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr("compose-workspace-placeholder"))
                .default_value(draft.workspace_path.clone())
        });
        let title = cx.new(|cx| InputState::new(window, cx).default_value(draft.title.clone()));
        let subscriptions = vec![
            cx.subscribe_in(&prompt, window, |this, state, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    let text = state.read(cx).value();
                    this.model.set_prompt(&text);
                    cx.notify();
                }
            }),
            cx.subscribe_in(&workspace, window, |this, state, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    let text = state.read(cx).value();
                    this.model.set_workspace_path(&text);
                    cx.notify();
                }
            }),
            cx.subscribe_in(&title, window, |this, state, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    let text = state.read(cx).value();
                    this.model.set_title(&text);
                    cx.notify();
                }
            }),
        ];
        Self {
            model,
            status: Status::Idle,
            machines_state: MachinesState::Idle,
            target: None,
            machines_rx: None,
            send_rx: None,
            prompt,
            workspace,
            title,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    pub fn model(&self) -> &NewTaskModel {
        &self.model
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    /// The multi-line prompt field (tests focus it to type).
    pub fn prompt_input(&self) -> &Entity<TextareaState> {
        &self.prompt
    }

    /// The workspace folder field (tests focus it to type).
    pub fn workspace_input(&self) -> &Entity<InputState> {
        &self.workspace
    }

    /// Point the composer at a hub (or at nothing, when signed out). A new target reloads the
    /// machines list; the same target is a no-op.
    pub fn set_target(
        &mut self,
        target: Option<Target>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.target == target {
            return;
        }
        self.target = target;
        if self.target.is_some() {
            self.load_machines(window, cx);
        }
        cx.notify();
    }

    fn load_machines(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            return;
        };
        self.machines_state = MachinesState::Loading;
        self.machines_rx = Some(run_off_thread(async move {
            fetch_machines(&target.api, target.token.as_deref()).await
        }));
        self.watch(window, cx);
    }

    /// Poll the background jobs while any is running.
    fn watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let keep_going = this
                    .update_in(cx, |this, window, cx| this.pump(window, cx))
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    /// Move finished background answers into the model. Returns whether a job is still running.
    pub fn pump(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(receiver) = &self.machines_rx {
            match receiver.try_recv() {
                Ok(result) => {
                    self.machines_rx = None;
                    match result {
                        Ok(machines) => {
                            self.model.set_machines(machines);
                            self.machines_state = MachinesState::Idle;
                        }
                        Err(detail) => self.machines_state = MachinesState::Failed(detail),
                    }
                    cx.notify();
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.machines_rx = None;
                    self.machines_state =
                        MachinesState::Failed("the request did not finish".to_owned());
                    cx.notify();
                }
            }
        }
        if let Some(receiver) = &self.send_rx {
            let answer = match receiver.try_recv() {
                Ok(answer) => Some(Ok(answer)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(())),
            };
            if let Some(answer) = answer {
                self.send_rx = None;
                match answer {
                    Ok(Ok(created)) => {
                        self.model.sent();
                        self.prompt
                            .update(cx, |state, cx| state.set_value("", window, cx));
                        self.title
                            .update(cx, |state, cx| state.set_value("", window, cx));
                        self.status = Status::Sent {
                            thread_id: created.thread_id,
                        };
                    }
                    Ok(Err(error)) => self.status = Status::Failed(error.to_string()),
                    Err(()) => {
                        self.status = Status::Failed("the request did not finish".to_owned());
                    }
                }
                cx.notify();
            }
        }
        self.machines_rx.is_some() || self.send_rx.is_some()
    }

    /// Validate and send the task. Ctrl/Cmd+Enter and the Send button both come here.
    pub fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.status == Status::Sending {
            return;
        }
        let Some(target) = self.target.clone() else {
            self.status = Status::NoHub;
            cx.notify();
            return;
        };
        let prepared = match self.model.prepare_send() {
            Ok(prepared) => prepared,
            Err(problem) => {
                self.status = Status::Invalid(problem);
                cx.notify();
                return;
            }
        };
        self.status = Status::Sending;
        cx.notify();
        self.send_rx = Some(run_off_thread(async move {
            execute(&target.api, target.token.as_deref(), &prepared).await
        }));
        self.watch(window, cx);
    }

    fn status_text(&self) -> String {
        match &self.status {
            Status::Idle => self
                .model
                .save_error()
                .map(|detail| tr_args("compose-draft-error", &[("detail", detail)]))
                .unwrap_or_else(|| tr("compose-status-idle")),
            Status::Sending => tr("compose-status-sending"),
            Status::Sent { thread_id } => {
                tr_args("compose-status-sent", &[("detail", thread_id.as_str())])
            }
            Status::Failed(detail) => {
                tr_args("compose-status-failed", &[("detail", detail.as_str())])
            }
            Status::Invalid(problem) => tr(match problem {
                Invalid::NoMachine => "compose-invalid-no-machine",
                Invalid::MachineOffline => "compose-invalid-machine-offline",
                Invalid::NoAgent => "compose-invalid-no-agent",
                Invalid::AgentNotOnMachine => "compose-invalid-agent-not-on-machine",
                Invalid::NoWorkspace => "compose-invalid-no-workspace",
                Invalid::NoPrompt => "compose-invalid-no-prompt",
            }),
            Status::NoHub => tr("compose-status-no-hub"),
        }
    }

    /// A row of choices, one button each; `selected` marks the current one.
    fn choices(
        &self,
        id_prefix: &'static str,
        items: Vec<(String, String, String)>,
        selected: Option<String>,
        cx: &mut Context<Self>,
        on_pick: impl Fn(&mut Self, &str, &mut Context<Self>) + Clone + 'static,
    ) -> impl IntoElement {
        h_flex()
            .gap_2()
            .flex_wrap()
            .children(items.into_iter().map(move |(value, label, a11y)| {
                let on_pick = on_pick.clone();
                let is_selected = selected.as_deref() == Some(value.as_str());
                let pick = value.clone();
                Button::new(SharedString::from(format!("{id_prefix}-{value}")))
                    .label(label)
                    .accessibility_label(a11y)
                    .selected(is_selected)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        on_pick(this, &pick, cx);
                        cx.notify();
                    }))
            }))
    }

    fn render_machines(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.model.draft().machine_id.clone();
        let items = self
            .model
            .machines()
            .iter()
            .map(|machine| {
                let label = if machine.online {
                    machine.name.clone()
                } else {
                    tr_args("compose-machine-offline", &[("name", &machine.name)])
                };
                let key = if selected.as_deref() == Some(machine.machine_id.as_str()) {
                    "compose-machine-selected-a11y"
                } else {
                    "compose-machine-a11y"
                };
                let a11y = tr_args(key, &[("name", &machine.name)]);
                (machine.machine_id.clone(), label, a11y)
            })
            .collect::<Vec<_>>();
        let note = match &self.machines_state {
            MachinesState::Loading => Some(tr("compose-machines-loading")),
            MachinesState::Failed(detail) => Some(tr_args(
                "compose-machines-error",
                &[("detail", detail.as_str())],
            )),
            MachinesState::Idle if items.is_empty() && self.target.is_some() => {
                Some(tr("compose-machines-empty"))
            }
            MachinesState::Idle => None,
        };
        v_flex()
            .gap_1()
            .child(div().child(tr("compose-machine")))
            .child(self.choices("machine", items, selected, cx, |this, id, _| {
                this.model.set_machine(id);
            }))
            .when_some(note, |el, text| {
                el.child(
                    div()
                        .id("compose-machines-note")
                        .test_support()
                        .aria_label(text.clone())
                        .text_color(cx.theme().muted_foreground)
                        .child(text),
                )
            })
    }

    fn render_agents(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.model.draft().agent.clone();
        let items = self
            .model
            .agents()
            .into_iter()
            .map(|agent| {
                let key = if agent == selected {
                    "compose-agent-selected-a11y"
                } else {
                    "compose-agent-a11y"
                };
                let a11y = tr_args(key, &[("name", &agent)]);
                (agent.clone(), agent, a11y)
            })
            .collect();
        v_flex()
            .gap_1()
            .child(div().child(tr("compose-agent")))
            .child(
                self.choices("agent", items, Some(selected), cx, |this, agent, _| {
                    this.model.set_agent(agent);
                }),
            )
    }

    fn render_workspace(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.model.draft().spawn.clone();
        let items = self
            .model
            .spawns()
            .into_iter()
            .map(|spawn| {
                let name = tr(&format!("compose-spawn-{spawn}"));
                let key = if spawn == selected {
                    "compose-spawn-selected-a11y"
                } else {
                    "compose-spawn-a11y"
                };
                let a11y = tr_args(key, &[("name", &name)]);
                (spawn, name, a11y)
            })
            .collect();
        v_flex()
            .gap_1()
            .child(div().child(tr("compose-workspace")))
            .child(
                div()
                    .id("compose-workspace")
                    .test_support()
                    .aria_label(tr("compose-workspace"))
                    .child(Input::new(&self.workspace)),
            )
            .child(div().child(tr("compose-spawn")))
            .child(
                self.choices("spawn", items, Some(selected), cx, |this, spawn, _| {
                    this.model.set_spawn(spawn);
                }),
            )
    }

    fn render_modes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.model.draft().mode.clone();
        let mut items = vec![(
            "default-choice".to_owned(),
            tr("compose-mode-default-choice"),
            String::new(),
        )];
        items.extend(self.model.modes().into_iter().map(|mode| {
            (
                mode.to_owned(),
                tr(&format!("compose-mode-{mode}")),
                String::new(),
            )
        }));
        let items = items
            .into_iter()
            .map(|(value, label, _)| {
                let is_selected = match &selected {
                    Some(mode) => *mode == value,
                    None => value == "default-choice",
                };
                let key = if is_selected {
                    "compose-mode-selected-a11y"
                } else {
                    "compose-mode-a11y"
                };
                let a11y = tr_args(key, &[("name", &label)]);
                (value, label, a11y)
            })
            .collect();
        let current = Some(selected.unwrap_or_else(|| "default-choice".to_owned()));
        // What the machine's local mode does to the choices (plan §4.10): never looser.
        let note = match (self.model.capped_from(), self.model.local_mode()) {
            (Some(from), Some(local)) => Some(tr_args(
                "compose-mode-capped",
                &[
                    ("from", &tr(&format!("compose-mode-{from}"))),
                    ("local", &tr(&format!("compose-mode-{local}"))),
                ],
            )),
            (None, Some(local)) if local != "bypassPermissions" => Some(tr_args(
                "compose-mode-local",
                &[("local", &tr(&format!("compose-mode-{local}")))],
            )),
            _ => None,
        };
        v_flex()
            .gap_1()
            .child(div().child(tr("compose-mode")))
            .children(note.map(|note| {
                div()
                    .id("compose-mode-note")
                    .test_support()
                    .aria_label(note.clone())
                    .child(note)
            }))
            .child(self.choices("mode", items, current, cx, |this, mode, _| {
                this.model
                    .set_mode((mode != "default-choice").then_some(mode));
            }))
    }
}

impl Focusable for ComposeView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ComposeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.status_text();
        let shortcut = send_shortcut_label();
        v_flex()
            .id("compose")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .role(Role::Form)
            .test_support()
            .aria_label(tr("compose-region-a11y"))
            .size_full()
            .gap_3()
            .on_action(cx.listener(|this, _: &SendTask, window, cx| this.send(window, cx)))
            .child(div().text_xl().child(tr("compose-title")))
            .child(self.render_machines(cx))
            .child(self.render_agents(cx))
            .child(self.render_workspace(cx))
            .child(self.render_modes(cx))
            .child(
                v_flex()
                    .gap_1()
                    .child(div().child(tr("compose-title-field")))
                    .child(
                        div()
                            .id("compose-title-input")
                            .test_support()
                            .aria_label(tr("compose-title-field"))
                            .child(Input::new(&self.title)),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(div().child(tr("compose-prompt")))
                    .child(
                        div()
                            .id("compose-prompt")
                            .test_support()
                            .aria_label(tr("compose-prompt"))
                            .child(Textarea::new(&self.prompt)),
                    ),
            )
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        Button::new("compose-send")
                            .label(tr("compose-send"))
                            .accessibility_label(tr_args(
                                "compose-send-a11y",
                                &[("shortcut", &shortcut)],
                            ))
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                    )
                    .child(
                        div()
                            .id("compose-status")
                            .role(Role::Status)
                            .test_support()
                            .aria_label(status.clone())
                            .text_color(cx.theme().muted_foreground)
                            .child(status),
                    ),
            )
    }
}
