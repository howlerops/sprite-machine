//! The Machines screen: a list with presence, rename in place, and revoke behind a confirmation.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use gpui_kit::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::api::run_off_thread;
use super::model::{
    Action, Failure, MAX_NAME_CHARS, MachineRow, NameError, Screen, rows_from, validate_name,
};
use super::{tr, tr_args};
use crate::agents::MachineInfo;
use crate::compose::Target;

/// Element id of the status line.
pub const STATUS_ID: &str = "machines-status";
/// Element id of the revoke confirmation line.
pub const CONFIRM_ID: &str = "machines-revoke-prompt";
/// Element id of the rename field.
pub const RENAME_INPUT_ID: &str = "machines-rename-input";
/// Element id of the empty state.
pub const EMPTY_ID: &str = "machines-empty";

/// What the screen tells its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachinesEvent {
    /// A rename or revoke went through: the machines list is out of date.
    Changed,
}

/// What the status line says.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Idle,
    NoHub,
    Working,
    Done(Action),
    Invalid(NameError),
    Failed(Failure),
}

pub struct MachinesView {
    rows: Vec<MachineRow>,
    target: Option<Target>,
    screen: Screen,
    status: Status,
    name: Entity<InputState>,
    rx: Option<Receiver<Result<(), Failure>>>,
    action: Option<Action>,
    pump: Option<Task<()>>,
}

impl EventEmitter<MachinesEvent> for MachinesView {}

impl MachinesView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let placeholder = tr("machines-rename-placeholder");
        Self {
            rows: Vec::new(),
            target: None,
            screen: Screen::List,
            status: Status::Idle,
            name: cx.new(|cx| InputState::new(window, cx).placeholder(placeholder)),
            rx: None,
            action: None,
            pump: None,
        }
    }

    /// Replace the machines (after a refresh of `GET /v1/machines`). A row that is gone ends a
    /// rename or confirmation that was open on it.
    pub fn set_machines(&mut self, machines: &[MachineInfo], cx: &mut Context<Self>) {
        self.rows = rows_from(machines);
        let open = match &self.screen {
            Screen::List => None,
            Screen::Renaming { machine_id } | Screen::ConfirmRevoke { machine_id } => {
                Some(machine_id.clone())
            }
        };
        if open.is_some_and(|id| !self.rows.iter().any(|row| row.machine_id == id)) {
            self.screen = Screen::List;
        }
        cx.notify();
    }

    /// The hub the requests go to; `None` when signed out.
    pub fn set_target(&mut self, target: Option<Target>, cx: &mut Context<Self>) {
        if self.target == target {
            return;
        }
        self.target = target;
        self.screen = Screen::List;
        self.status = Status::Idle;
        cx.notify();
    }

    pub fn rows(&self) -> &[MachineRow] {
        &self.rows
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    pub fn name_input(&self) -> &Entity<InputState> {
        &self.name
    }

    fn row(&self, machine_id: &str) -> Option<&MachineRow> {
        self.rows.iter().find(|row| row.machine_id == machine_id)
    }

    /// Open the name field on a row, filled with its current name.
    pub fn begin_rename(&mut self, machine_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(current) = self.row(machine_id).map(|row| row.name.clone()) else {
            return;
        };
        self.name
            .update(cx, |input, cx| input.set_value(current, window, cx));
        self.screen = Screen::Renaming {
            machine_id: machine_id.to_owned(),
        };
        self.status = Status::Idle;
        cx.notify();
    }

    /// Send the typed name.
    pub fn save_rename(&mut self, cx: &mut Context<Self>) {
        let Screen::Renaming { machine_id } = self.screen.clone() else {
            return;
        };
        let typed = self.name.read(cx).value().to_string();
        match validate_name(&typed) {
            Ok(name) => self.start(Action::Rename { machine_id, name }, cx),
            Err(error) => {
                self.status = Status::Invalid(error);
                cx.notify();
            }
        }
    }

    /// Ask "revoke it?" on a row; nothing is sent yet.
    pub fn ask_revoke(&mut self, machine_id: &str, cx: &mut Context<Self>) {
        if self.row(machine_id).is_none() {
            return;
        }
        self.screen = Screen::ConfirmRevoke {
            machine_id: machine_id.to_owned(),
        };
        self.status = Status::Idle;
        cx.notify();
    }

    /// The user confirmed: revoke.
    pub fn confirm_revoke(&mut self, cx: &mut Context<Self>) {
        let Screen::ConfirmRevoke { machine_id } = self.screen.clone() else {
            return;
        };
        let Some(name) = self.row(&machine_id).map(|row| row.name.clone()) else {
            return;
        };
        self.start(Action::Revoke { machine_id, name }, cx);
    }

    /// Close the name field or the confirmation without sending anything.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.screen = Screen::List;
        self.status = Status::Idle;
        cx.notify();
    }

    fn start(&mut self, action: Action, cx: &mut Context<Self>) {
        if self.rx.is_some() {
            return;
        }
        let Some(target) = self.target.clone() else {
            self.status = Status::NoHub;
            cx.notify();
            return;
        };
        self.status = Status::Working;
        self.rx = Some(run_off_thread(target, action.clone()));
        self.action = Some(action);
        self.pump = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let Ok(more) = this.update(cx, |this, cx| this.poll(cx)) else {
                    break;
                };
                if !more {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Move a finished request into the screen. Returns whether one is still running.
    fn poll(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(receiver) = &self.rx else {
            return false;
        };
        let answer = match receiver.try_recv() {
            Ok(answer) => answer,
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => Err(Failure {
                status: None,
                detail: "the request did not finish".to_owned(),
            }),
        };
        self.rx = None;
        let action = self.action.take();
        match (answer, action) {
            (Ok(()), Some(action)) => {
                self.screen = Screen::List;
                self.status = Status::Done(action);
                cx.emit(MachinesEvent::Changed);
            }
            (Ok(()), None) => {}
            (Err(failure), _) => self.status = Status::Failed(failure),
        }
        cx.notify();
        false
    }

    fn status_text(&self) -> Option<String> {
        Some(match &self.status {
            Status::Idle => return None,
            Status::NoHub => tr("machines-status-no-hub"),
            Status::Working => tr("machines-status-working"),
            Status::Done(Action::Rename { name, .. }) => {
                tr_args("machines-status-renamed", &[("name", name)])
            }
            Status::Done(Action::Revoke { name, .. }) => {
                tr_args("machines-status-revoked", &[("name", name)])
            }
            Status::Invalid(NameError::Empty) => tr("machines-name-empty"),
            Status::Invalid(NameError::TooLong) => tr_args(
                "machines-name-too-long",
                &[("max", &MAX_NAME_CHARS.to_string())],
            ),
            Status::Failed(failure) => match failure.status {
                Some(403) => tr("machines-error-forbidden"),
                Some(404) => tr("machines-error-not-found"),
                Some(status) => tr_args(
                    "machines-error-other",
                    &[("detail", &format!("{status} {}", failure.detail))],
                ),
                None => tr_args("machines-error-network", &[("detail", &failure.detail)]),
            },
        })
    }

    fn header(cx: &App) -> impl IntoElement + use<> {
        h_flex()
            .id("machines-header")
            .gap_3()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .children(
                [
                    "machines-col-name",
                    "machines-col-kind",
                    "machines-col-presence",
                    "machines-col-agents",
                ]
                .map(|id| div().flex_1().child(tr(id))),
            )
            .child(div().w(px(200.)))
    }

    fn render_row(&self, row: &MachineRow, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = row.machine_id.clone();
        let presence = {
            let state = tr(&format!("machines-presence-{}", row.presence.slug()));
            match &row.since {
                Some(since) => tr_args(
                    "machines-presence-since",
                    &[("state", &state), ("since", since)],
                ),
                None => state,
            }
        };
        let kind = match row.kind.as_str() {
            "local" => tr("machines-kind-local"),
            "sandbox" => tr("machines-kind-sandbox"),
            _ => tr("machines-kind-unknown"),
        };
        let agents = if row.harnesses.is_empty() {
            tr("machines-agents-none")
        } else {
            row.harnesses.join(", ")
        };
        let a11y = tr_args(
            "machines-row-a11y",
            &[
                ("name", &row.name),
                ("kind", &kind),
                ("presence", &presence),
            ],
        );
        let renaming = self.screen
            == Screen::Renaming {
                machine_id: id.clone(),
            };
        let confirming = self.screen
            == Screen::ConfirmRevoke {
                machine_id: id.clone(),
            };
        let name = row.name.clone();
        let (rename_id, revoke_id) = (id.clone(), id.clone());
        let rename_name = name.clone();
        let revoke_name = name.clone();

        let name_cell = if renaming {
            div()
                .flex_1()
                .child(
                    div()
                        .id(RENAME_INPUT_ID)
                        .test_support()
                        .aria_label(tr_args("machines-rename-field-a11y", &[("name", &name)]))
                        .child(Input::new(&self.name)),
                )
                .into_any_element()
        } else {
            div().flex_1().child(name.clone()).into_any_element()
        };

        let actions = if renaming {
            h_flex()
                .gap_1()
                .w(px(200.))
                .child(
                    Button::new("machines-rename-save")
                        .label(tr("machines-rename-save"))
                        .accessibility_label(tr_args(
                            "machines-rename-save-a11y",
                            &[("name", &name)],
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.save_rename(cx))),
                )
                .child(
                    Button::new("machines-rename-cancel")
                        .ghost()
                        .label(tr("machines-cancel"))
                        .accessibility_label(tr("machines-cancel-a11y"))
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                )
                .into_any_element()
        } else if confirming {
            h_flex()
                .gap_1()
                .w(px(200.))
                .child(
                    Button::new("machines-revoke-yes")
                        .label(tr("machines-revoke-yes"))
                        .accessibility_label(tr_args(
                            "machines-revoke-yes-a11y",
                            &[("name", &name)],
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.confirm_revoke(cx))),
                )
                .child(
                    Button::new("machines-revoke-cancel")
                        .ghost()
                        .label(tr("machines-cancel"))
                        .accessibility_label(tr("machines-cancel-a11y"))
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                )
                .into_any_element()
        } else {
            h_flex()
                .gap_1()
                .w(px(200.))
                .child(
                    Button::new(SharedString::from(format!("machine-rename-{id}")))
                        .ghost()
                        .label(tr("machines-rename"))
                        .accessibility_label(tr_args(
                            "machines-rename-a11y",
                            &[("name", &rename_name)],
                        ))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.begin_rename(&rename_id, window, cx);
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("machine-revoke-{id}")))
                        .ghost()
                        .label(tr("machines-revoke"))
                        .accessibility_label(tr_args(
                            "machines-revoke-a11y",
                            &[("name", &revoke_name)],
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.ask_revoke(&revoke_id, cx);
                        })),
                )
                .into_any_element()
        };

        v_flex()
            .id(SharedString::from(format!("machine-row-{id}")))
            .test_support()
            .aria_label(a11y)
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(name_cell)
                    .child(div().flex_1().child(kind))
                    .child(div().flex_1().child(presence))
                    .child(div().flex_1().child(agents))
                    .child(actions),
            )
            .when(confirming, |el| {
                let prompt = tr_args("machines-revoke-confirm", &[("name", &name)]);
                el.child(
                    div()
                        .id(CONFIRM_ID)
                        .test_support()
                        .role(Role::Alert)
                        .aria_label(prompt.clone())
                        .text_sm()
                        .text_color(cx.theme().warning)
                        .child(prompt),
                )
            })
    }
}

impl Render for MachinesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = self
            .rows
            .clone()
            .iter()
            .map(|row| self.render_row(row, cx))
            .collect();
        let status = self.status_text();
        let title = tr("machines-title");
        v_flex()
            .id("machines")
            .test_support()
            .role(Role::Region)
            .aria_label(tr("machines-region-a11y"))
            .gap_3()
            .p_4()
            .child(
                div()
                    .id("machines-title")
                    .test_support()
                    .aria_label(title.clone())
                    .text_lg()
                    .child(title),
            )
            .child(div().text_sm().child(tr("machines-intro")))
            .child(
                v_flex()
                    .id("machines-table")
                    .test_support()
                    .aria_label(tr("machines-table-a11y"))
                    .child(Self::header(cx))
                    .children(rows)
                    .when(self.rows.is_empty(), |el| {
                        let empty = tr("machines-empty");
                        el.child(
                            div()
                                .id(EMPTY_ID)
                                .test_support()
                                .aria_label(empty.clone())
                                .text_sm()
                                .child(empty),
                        )
                    }),
            )
            .when_some(status, |el, text| {
                el.child(
                    div()
                        .id(STATUS_ID)
                        .test_support()
                        .role(Role::Status)
                        .aria_label(text.clone())
                        .text_sm()
                        .child(text),
                )
            })
    }
}
