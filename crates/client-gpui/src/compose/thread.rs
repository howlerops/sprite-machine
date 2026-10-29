//! [`ThreadComposer`]: the composer under a running thread. Slash commands, the mode and model
//! pickers, queued-message chips and Stop all read the thread's state ([`ThreadView`]) and turn
//! clicks into [`ComposerEvent`]s; the shell sends them through the outbox / hub. Nothing is
//! assumed done until the hub's `StateDelta` comes back and the owner calls
//! [`ThreadComposer::set_thread`] again.

use client_core::ThreadView;
use gpui_kit::TestSupportExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, *};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::commands::{
    ComposerEvent, SessionOptions, SlashAccept, SlashCommand, ThreadRequest, accept_command,
    filter_commands, slash_query,
};
use super::queue::{QueuedChip, queued_chips, remove_request, stop_intent};
use super::{send_shortcut_label, tr, tr_args};

actions!(
    thread_composer,
    [SlashAcceptAction, SlashNext, SlashPrevious]
);

const CONTEXT: &str = "ThreadComposer";
const SLASH_CONTEXT: &str = "SlashOpen";

/// Key bindings: while the command list is open, Enter takes the highlighted command and the
/// arrow keys move the highlight. Called from [`super::init`].
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", SlashAcceptAction, Some("SlashOpen > Input")),
        KeyBinding::new("down", SlashNext, Some("SlashOpen > Input")),
        KeyBinding::new("up", SlashPrevious, Some("SlashOpen > Input")),
        KeyBinding::new(
            if cfg!(target_os = "macos") {
                "cmd-enter"
            } else {
                "ctrl-enter"
            },
            super::SendTask,
            Some("ThreadComposer > Input"),
        ),
    ]);
}

pub struct ThreadComposer {
    thread: Option<ThreadView>,
    /// The runner's local mode: the loosest mode offered.
    cap: Option<String>,
    prompt: Entity<TextareaState>,
    slash_selected: usize,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ComposerEvent> for ThreadComposer {}

impl ThreadComposer {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 8)
                .placeholder(tr_args(
                    "compose-thread-placeholder",
                    &[("shortcut", &send_shortcut_label())],
                ))
        });
        let subscriptions = vec![cx.subscribe_in(&prompt, window, |this, _, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                this.slash_selected = 0;
                cx.notify();
            }
        })];
        Self {
            thread: None,
            cap: None,
            prompt,
            slash_selected: 0,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    /// Show the thread's current state. Call it after every applied envelope.
    pub fn set_thread(&mut self, thread: &ThreadView, cx: &mut Context<Self>) {
        self.thread = Some(thread.clone());
        cx.notify();
    }

    /// The host machine's local permission mode (`capabilities.features`
    /// `permission_mode:<mode>`, see [`super::cap_from_features`]); `None` when unknown.
    pub fn set_runner_mode(&mut self, cap: Option<String>, cx: &mut Context<Self>) {
        if self.cap != cap {
            self.cap = cap;
            cx.notify();
        }
    }

    pub fn prompt_input(&self) -> &Entity<TextareaState> {
        &self.prompt
    }

    pub fn options(&self) -> SessionOptions {
        SessionOptions::from_state(
            self.thread
                .as_ref()
                .and_then(|thread| thread.state("session")),
        )
    }

    pub fn queued(&self) -> Vec<QueuedChip> {
        self.thread.as_ref().map(queued_chips).unwrap_or_default()
    }

    pub fn is_running(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| stop_intent(thread).is_some())
    }

    /// The commands the list shows right now (empty when the text is not a `/` command).
    pub fn slash_matches(&self, cx: &App) -> Vec<SlashCommand> {
        let text = self.prompt.read(cx).value();
        let Some(query) = slash_query(&text) else {
            return Vec::new();
        };
        filter_commands(&self.options().commands, query)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Index of the highlighted command in [`ThreadComposer::slash_matches`].
    pub fn slash_selected(&self) -> usize {
        self.slash_selected
    }

    fn thread_id(&self) -> Option<String> {
        self.thread
            .as_ref()
            .map(|thread| thread.thread_id().to_owned())
    }

    /// Send the text as a prompt and clear the field.
    pub fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.prompt.read(cx).value().trim().to_owned();
        self.send_text(&text, window, cx);
    }

    fn send_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread_id) = self.thread_id() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        cx.emit(ComposerEvent::Send {
            thread_id,
            text: text.to_owned(),
        });
        self.prompt
            .update(cx, |state, cx| state.set_value("", window, cx));
        cx.notify();
    }

    /// Enter on the highlighted command.
    pub fn accept_slash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let matches = self.slash_matches(cx);
        let Some(command) = matches.get(self.slash_selected.min(matches.len().saturating_sub(1)))
        else {
            return;
        };
        match accept_command(command) {
            SlashAccept::Insert(text) => {
                self.prompt
                    .update(cx, |state, cx| state.set_value(text, window, cx));
            }
            SlashAccept::Send(text) => self.send_text(&text, window, cx),
        }
        cx.notify();
    }

    /// Move the highlight by `delta`, wrapping.
    pub fn move_slash(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.slash_matches(cx).len();
        if count == 0 {
            return;
        }
        self.slash_selected =
            (self.slash_selected as isize + delta).rem_euclid(count as isize) as usize;
        cx.notify();
    }

    /// Ask for a mode change. Ignored for a mode the runner does not allow.
    pub fn pick_mode(&mut self, mode_id: &str, cx: &mut Context<Self>) {
        let (Some(thread_id), options) = (self.thread_id(), self.options()) else {
            return;
        };
        if !options
            .allowed_modes(self.cap.as_deref())
            .iter()
            .any(|mode| mode.id == mode_id)
        {
            return;
        }
        cx.emit(ComposerEvent::Request(ThreadRequest::SetMode {
            thread_id,
            mode_id: mode_id.to_owned(),
        }));
    }

    /// Ask for a model change.
    pub fn pick_model(&mut self, value: &str, cx: &mut Context<Self>) {
        let (Some(thread_id), options) = (self.thread_id(), self.options()) else {
            return;
        };
        let Some(model) = options.model else {
            return;
        };
        if !model.options.iter().any(|choice| choice.value == value) {
            return;
        }
        cx.emit(ComposerEvent::Request(ThreadRequest::SetConfig {
            thread_id,
            id: model.id,
            value: value.to_owned(),
        }));
    }

    /// Stop the running task.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(intent) = self.thread.as_ref().and_then(stop_intent) {
            cx.emit(ComposerEvent::Cancel(intent));
        }
    }

    /// Drop a queued message.
    pub fn remove_queued(&mut self, task_id: &str, cx: &mut Context<Self>) {
        if let Some(request) = self
            .thread
            .as_ref()
            .and_then(|thread| remove_request(thread, task_id))
        {
            cx.emit(ComposerEvent::Request(request));
        }
    }

    fn picker(
        &self,
        id_prefix: &'static str,
        label: String,
        items: Vec<(String, String, bool)>,
        on_pick: impl Fn(&mut Self, &str, &mut Context<Self>) + Clone + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .gap_2()
            .flex_wrap()
            .items_center()
            .child(div().child(label.clone()))
            .children(items.into_iter().map(move |(value, name, selected)| {
                let on_pick = on_pick.clone();
                let pick = value.clone();
                let a11y = tr_args(
                    if selected {
                        "compose-thread-choice-selected-a11y"
                    } else {
                        "compose-thread-choice-a11y"
                    },
                    &[("label", &label), ("name", &name)],
                );
                Button::new(SharedString::from(format!("{id_prefix}-{value}")))
                    .label(name)
                    .accessibility_label(a11y)
                    .selected(selected)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        on_pick(this, &pick, cx);
                    }))
            }))
    }

    fn render_chips(&self, chips: Vec<QueuedChip>, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("thread-queue")
            .gap_2()
            .flex_wrap()
            .items_center()
            .child(div().child(tr("compose-thread-queued")))
            .children(chips.into_iter().map(|chip| {
                let task_id = chip.task_id.clone();
                let label = if chip.preview.is_empty() {
                    tr("compose-thread-queued-empty")
                } else {
                    chip.preview.clone()
                };
                h_flex()
                    .id(SharedString::from(format!("queued-{}", chip.task_id)))
                    .test_support()
                    .aria_label(tr_args(
                        "compose-thread-queued-a11y",
                        &[("preview", &label)],
                    ))
                    .gap_1()
                    .items_center()
                    .child(div().child(label.clone()))
                    .child(
                        Button::new(SharedString::from(format!(
                            "queued-remove-{}",
                            chip.task_id
                        )))
                        .label(tr("compose-thread-queued-remove"))
                        .accessibility_label(tr_args(
                            "compose-thread-queued-remove-a11y",
                            &[("preview", &label)],
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.remove_queued(&task_id, cx);
                        })),
                    )
            }))
    }

    fn render_slash(&self, matches: Vec<SlashCommand>, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.slash_selected.min(matches.len().saturating_sub(1));
        v_flex()
            .id("thread-slash")
            .role(Role::List)
            .test_support()
            .aria_label(tr("compose-thread-slash-a11y"))
            .gap_1()
            .children(matches.into_iter().enumerate().map(|(index, command)| {
                let hint = command.input_hint.clone().unwrap_or_default();
                let a11y = tr_args(
                    "compose-thread-slash-command-a11y",
                    &[
                        ("name", &command.name),
                        ("description", &command.description),
                    ],
                );
                Button::new(SharedString::from(format!("slash-{}", command.name)))
                    .label(format!(
                        "/{} {} {}",
                        command.name, hint, command.description
                    ))
                    .accessibility_label(a11y)
                    .selected(index == selected)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.slash_selected = index;
                        this.accept_slash(window, cx);
                    }))
            }))
    }
}

impl Focusable for ThreadComposer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ThreadComposer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let options = self.options();
        let chips = self.queued();
        let running = self.is_running();
        let matches = self.slash_matches(cx);
        let slash_open = !matches.is_empty();
        let modes: Vec<_> = options
            .allowed_modes(self.cap.as_deref())
            .into_iter()
            .map(|mode| {
                (
                    mode.id.clone(),
                    mode.name.clone(),
                    options.current_mode.as_deref() == Some(mode.id.as_str()),
                )
            })
            .collect();
        let models: Vec<_> = options
            .model
            .iter()
            .flat_map(|model| {
                model.options.iter().map(|choice| {
                    (
                        choice.value.clone(),
                        choice.name.clone(),
                        model.current.as_deref() == Some(choice.value.as_str()),
                    )
                })
            })
            .collect();
        let shortcut = send_shortcut_label();
        v_flex()
            .id("thread-composer")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .role(Role::Form)
            .test_support()
            .aria_label(tr("compose-thread-region-a11y"))
            .gap_2()
            .on_action(cx.listener(|this, _: &super::SendTask, window, cx| this.send(window, cx)))
            .on_action(
                cx.listener(|this, _: &SlashAcceptAction, window, cx| {
                    this.accept_slash(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &SlashNext, _, cx| this.move_slash(1, cx)))
            .on_action(cx.listener(|this, _: &SlashPrevious, _, cx| this.move_slash(-1, cx)))
            .when(!chips.is_empty(), |el| {
                el.child(self.render_chips(chips, cx))
            })
            .when(slash_open, |el| el.child(self.render_slash(matches, cx)))
            .child(
                div()
                    .id("thread-prompt")
                    .when(slash_open, |el| el.key_context(SLASH_CONTEXT))
                    .test_support()
                    .aria_label(tr("compose-thread-prompt-a11y"))
                    .child(Textarea::new(&self.prompt)),
            )
            .when(!modes.is_empty(), |el| {
                el.child(self.picker(
                    "thread-mode",
                    tr("compose-thread-mode"),
                    modes,
                    |this, id, cx| this.pick_mode(id, cx),
                    cx,
                ))
            })
            .when(!models.is_empty(), |el| {
                el.child(self.picker(
                    "thread-model",
                    tr("compose-thread-model"),
                    models,
                    |this, value, cx| this.pick_model(value, cx),
                    cx,
                ))
            })
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        Button::new("thread-send")
                            .label(tr("compose-send"))
                            .accessibility_label(tr_args(
                                "compose-send-a11y",
                                &[("shortcut", &shortcut)],
                            ))
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                    )
                    .when(running, |el| {
                        el.child(
                            Button::new("thread-stop")
                                .label(tr("compose-thread-stop"))
                                .accessibility_label(tr("compose-thread-stop-a11y"))
                                .text_color(cx.theme().danger)
                                .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                        )
                    }),
            )
    }
}
