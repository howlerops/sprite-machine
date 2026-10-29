//! The agents screen: table, disabled BYO button, launch form.

use std::sync::Arc;

use gpui_kit::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::model::{
    AgentRow, LaunchConfig, LaunchForm, LaunchState, MachineInfo, ManagedAgentClient,
    agents_from_machines, byo_disabled_reason, submit,
};
use super::{tr, tr_args};

/// What the agents page tells the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentsEvent {
    /// The empty table's primary action: show how to connect a machine.
    SetupMachine,
}

/// Element id of the empty table's action.
pub const EMPTY_SETUP_ID: &str = "no-agents-action";

impl EventEmitter<AgentsEvent> for AgentsView {}

pub struct AgentsView {
    rows: Vec<AgentRow>,
    config: LaunchConfig,
    client: Arc<dyn ManagedAgentClient>,
    state: LaunchState,
    name: Entity<InputState>,
    harness: Entity<InputState>,
    model: Entity<InputState>,
    egress: Entity<InputState>,
    loadout: Entity<InputState>,
}

impl AgentsView {
    pub fn new(
        machines: &[MachineInfo],
        config: LaunchConfig,
        client: Arc<dyn ManagedAgentClient>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let field = |label: &str, value: &str, window: &mut Window, cx: &mut Context<Self>| {
            let (label, value) = (label.to_owned(), value.to_owned());
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(label)
                    .default_value(value)
            })
        };
        Self {
            rows: agents_from_machines(machines),
            config,
            client,
            state: LaunchState::Idle,
            name: field(&tr("agents-field-name"), "", window, cx),
            harness: field(&tr("agents-field-harness"), "claude", window, cx),
            model: field(&tr("agents-field-model"), "", window, cx),
            egress: field(&tr("agents-field-egress"), "", window, cx),
            loadout: field(&tr("agents-field-loadout"), "", window, cx),
        }
    }

    /// Replace the machines (after a refresh of `GET /v1/machines`).
    pub fn set_machines(&mut self, machines: &[MachineInfo], cx: &mut Context<Self>) {
        self.rows = agents_from_machines(machines);
        cx.notify();
    }

    /// The injected "a model key is configured" flag changed.
    pub fn set_model_key_configured(&mut self, configured: bool, cx: &mut Context<Self>) {
        self.config.model_key_configured = configured;
        cx.notify();
    }

    pub fn rows(&self) -> &[AgentRow] {
        &self.rows
    }

    pub fn launch_state(&self) -> &LaunchState {
        &self.state
    }

    pub fn byo_enabled(&self) -> bool {
        byo_disabled_reason().is_none()
    }

    pub fn launch_enabled(&self) -> bool {
        self.config.disabled_reason().is_none()
    }

    pub fn name_input(&self) -> &Entity<InputState> {
        &self.name
    }

    pub fn harness_input(&self) -> &Entity<InputState> {
        &self.harness
    }

    pub fn model_input(&self) -> &Entity<InputState> {
        &self.model
    }

    pub fn egress_input(&self) -> &Entity<InputState> {
        &self.egress
    }

    fn form(&self, cx: &App) -> LaunchForm {
        let text = |input: &Entity<InputState>| input.read(cx).value().to_string();
        LaunchForm {
            name: text(&self.name),
            harness: text(&self.harness),
            model: text(&self.model),
            egress: text(&self.egress),
            loadout: text(&self.loadout),
        }
    }

    /// What a click on "Launch" does: validate, then call the client. Does nothing without a key.
    pub fn launch(&mut self, cx: &mut Context<Self>) {
        let form = self.form(cx);
        self.state = submit(self.config, &form, self.client.as_ref());
        cx.notify();
    }

    fn header(ids: &[&str]) -> impl IntoElement + use<> {
        let cells: Vec<_> = ids.iter().map(|id| tr(id)).collect();
        h_flex()
            .id("agents-header")
            .gap_3()
            .text_sm()
            .children(cells.into_iter().map(|c| div().flex_1().child(c)))
    }

    fn row(ix: usize, row: AgentRow, cx: &App) -> impl IntoElement + use<> {
        let kind = tr(&format!("agents-kind-{}", row.kind.slug()));
        let presence = tr(&format!("agents-presence-{}", row.presence.slug()));
        let health = tr(&format!("agents-health-{}", row.health.slug()));
        let skills = if row.skills.is_empty() {
            tr("agents-skills-none")
        } else {
            row.skills.join(", ")
        };
        let a11y = tr_args(
            "agents-row-a11y",
            &[
                ("name", &row.name),
                ("kind", &kind),
                ("machine", &row.machine_name),
                ("presence", &presence),
                ("health", &health),
            ],
        );
        h_flex()
            .id(format!("agents-row-{ix}"))
            .test_support()
            .aria_label(a11y)
            .gap_3()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().flex_1().child(row.name))
            .child(div().flex_1().child(kind))
            .child(div().flex_1().child(row.machine_name))
            .child(div().flex_1().child(skills))
            .child(div().flex_1().child(health))
            .child(div().flex_1().child(presence))
    }

    fn note(id: &'static str, text: String) -> impl IntoElement + use<> {
        div()
            .id(id)
            .test_support()
            .aria_label(text.clone())
            .text_sm()
            .child(text)
    }
}

impl Render for AgentsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = self
            .rows
            .iter()
            .cloned()
            .enumerate()
            .map(|(ix, row)| Self::row(ix, row, cx))
            .collect();
        let byo_reason = byo_disabled_reason().map(tr);
        let launch_reason = self.config.disabled_reason().map(tr);
        let launch_enabled = self.launch_enabled();
        let outcome = match &self.state {
            LaunchState::Idle => None,
            LaunchState::Invalid(errors) => Some(
                errors
                    .iter()
                    .map(|e| match &e.value {
                        Some(value) => tr_args(e.id, &[("value", value)]),
                        None => tr(e.id),
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            LaunchState::Launched(agent) => {
                Some(tr_args("agents-launch-ok", &[("id", &agent.agent_id)]))
            }
            LaunchState::Failed(reason) => {
                Some(tr_args("agents-launch-failed", &[("reason", reason)]))
            }
        };
        let title = tr("agents-title");
        v_flex()
            .id("agents")
            .test_support()
            .aria_label(tr("agents-region-a11y"))
            .gap_3()
            .p_4()
            .child(
                div()
                    .id("agents-title")
                    .test_support()
                    .aria_label(title.clone())
                    .text_lg()
                    .child(title),
            )
            .child(div().text_sm().child(tr("agents-intro")))
            .child(
                v_flex()
                    .id("agents-table")
                    .test_support()
                    .aria_label(tr("agents-region-a11y"))
                    .child(Self::header(&[
                        "agents-col-name",
                        "agents-col-kind",
                        "agents-col-runs-on",
                        "agents-col-skills",
                        "agents-col-health",
                        "agents-col-presence",
                    ]))
                    .children(rows)
                    .when(self.rows.is_empty(), |el| {
                        el.child(Self::note("agents-empty", tr("agents-empty")))
                            .child(
                                div().child(
                                    Button::new(EMPTY_SETUP_ID)
                                        .primary()
                                        .label(tr("agents-empty-setup"))
                                        .accessibility_label(tr("agents-empty-setup-a11y"))
                                        .on_click(cx.listener(|_, _, _, cx| {
                                            cx.emit(AgentsEvent::SetupMachine);
                                        })),
                                ),
                            )
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("agents-byo")
                            .label(tr("agents-byo-button"))
                            .disabled(byo_reason.is_some()),
                    )
                    .when_some(byo_reason, |el, reason| {
                        el.child(Self::note("agents-byo-reason", reason))
                    }),
            )
            .child(
                v_flex()
                    .id("agents-launch")
                    .gap_2()
                    .child(div().text_lg().child(tr("agents-launch-title")))
                    .when_some(launch_reason, |el, reason| {
                        el.child(Self::note("agents-launch-reason", reason))
                    })
                    .child(Input::new(&self.name).disabled(!launch_enabled))
                    .child(Input::new(&self.harness).disabled(!launch_enabled))
                    .child(Input::new(&self.model).disabled(!launch_enabled))
                    .child(Input::new(&self.egress).disabled(!launch_enabled))
                    .child(Input::new(&self.loadout).disabled(!launch_enabled))
                    .child(
                        Button::new("agents-launch-submit")
                            .label(tr("agents-launch-button"))
                            .disabled(!launch_enabled)
                            .on_click(cx.listener(|this, _, _, cx| this.launch(cx))),
                    )
                    .when_some(outcome, |el, message| {
                        el.child(Self::note("agents-launch-outcome", message))
                    }),
            )
    }
}
