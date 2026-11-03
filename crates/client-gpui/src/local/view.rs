//! The settings section "Phone": the global auto-connect switch and one switch per local session.
//! A thin view over [`LocalRunner`]'s snapshot; the shell mounts it in Settings and saves the
//! global setting when it emits [`PhoneEvent::AutoChanged`].

use std::time::Duration;

use gpui_kit::TestSupportExt as _;
use gpui_kit::component::Selectable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::v_flex;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::runner::{LocalRunner, Snapshot};
use super::{LinkState, tr, tr_args};

/// How often the view looks for a change made on the runner's thread.
const POLL: Duration = Duration::from_millis(250);

/// Element id of the global switch.
pub const AUTO_ID: &str = "phone-auto";

/// Element id of a session's switch.
pub fn session_toggle_id(thread_id: &str) -> String {
    format!("phone-session-{thread_id}")
}

/// What the shell must react to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhoneEvent {
    /// The user changed "Connect to phone automatically": persist it.
    AutoChanged(bool),
}

pub struct PhoneSettingsView {
    runner: LocalRunner,
    seen: u64,
    _poll: Task<()>,
}

impl EventEmitter<PhoneEvent> for PhoneSettingsView {}

impl PhoneSettingsView {
    pub fn new(runner: LocalRunner, cx: &mut Context<Self>) -> Self {
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let alive = this
                    .update(cx, |this, cx| {
                        let version = this.runner.snapshot().version;
                        if version != this.seen {
                            this.seen = version;
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });
        Self {
            runner,
            seen: 0,
            _poll: poll,
        }
    }

    pub fn runner(&self) -> &LocalRunner {
        &self.runner
    }

    /// What a click on the global switch does.
    pub fn set_auto(&mut self, on: bool, cx: &mut Context<Self>) {
        let runner = self.runner.clone();
        self.runner
            .spawn(async move { runner.set_auto_connect(on).await });
        cx.emit(PhoneEvent::AutoChanged(on));
        cx.notify();
    }

    /// What a click on a session's switch does.
    pub fn toggle(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        let (runner, thread_id) = (self.runner.clone(), thread_id.to_owned());
        self.runner
            .spawn(async move { runner.toggle(&thread_id).await });
        cx.notify();
    }

    fn status_line(snapshot: &Snapshot) -> String {
        match snapshot.status.message() {
            (id, Some(reason)) => tr_args(id, &[("reason", reason)]),
            (id, None) => tr(id),
        }
    }
}

impl Render for PhoneSettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.runner.snapshot();
        let title = tr("phone-title");
        let status = Self::status_line(&snapshot);
        let rows: Vec<_> = snapshot
            .rows
            .iter()
            .map(|row| {
                let action = tr(row.toggle_label_id());
                let a11y = tr_args(
                    "phone-session-a11y",
                    &[("title", &row.title), ("action", &action)],
                );
                let state = match &row.state {
                    LinkState::Failed(reason) => tr_args(row.state_id(), &[("reason", reason)]),
                    _ => tr(row.state_id()),
                };
                let (id, thread_id) = (session_toggle_id(&row.thread_id), row.thread_id.clone());
                v_flex()
                    .id(format!("phone-row-{}", row.thread_id))
                    .test_support()
                    .aria_label(format!("{}: {state}", row.title))
                    .gap_1()
                    .child(div().child(row.title.clone()))
                    .child(div().text_sm().child(state))
                    .child(
                        Button::new(SharedString::from(id))
                            .label(action)
                            .accessibility_label(a11y)
                            .selected(row.is_on())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle(&thread_id, cx);
                            })),
                    )
            })
            .collect();
        let empty = rows.is_empty();
        v_flex()
            .id("settings-phone")
            .test_support()
            .aria_label(title.clone())
            .gap_2()
            .child(div().text_lg().child(title))
            .child(
                Checkbox::new(AUTO_ID)
                    .label(tr("phone-auto"))
                    .checked(snapshot.auto)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.set_auto(*checked, cx);
                    })),
            )
            .child(div().text_sm().child(status))
            .children(rows)
            .when(empty, |el| {
                el.child(div().text_sm().child(tr("phone-no-sessions")))
            })
    }
}
