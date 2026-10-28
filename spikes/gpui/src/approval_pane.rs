//! The approval card: an `Interrupt{approval}` as a titled card with its tool-call summary, the
//! time left (or who resolved it), and one button per option. Allow kinds are green, reject kinds
//! red. Every click goes to the view model, and the answer goes to the injected [`AnswerSink`].

use std::sync::Arc;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::approval_model::{ApprovalModel, ButtonTone};
use crate::approval_sink::AnswerSink;

/// Element id of the card (its aria label is the title).
pub const CARD_ID: &str = "approval-card";
/// Element id of the status line (time left, or who resolved the approval).
pub const STATUS_ID: &str = "approval-status";
/// Element id of the summary line (the interrupt's message, or its title).
pub const SUMMARY_ID: &str = "approval-summary";

/// Element id of the button for `option_id`.
pub fn option_button_id(option_id: &str) -> String {
    format!("approval-option-{option_id}")
}

pub struct ApprovalCard {
    model: ApprovalModel,
    sink: Arc<dyn AnswerSink>,
    now_ms: i64,
}

impl ApprovalCard {
    /// A card for `model`, judging time left at `now_ms` (epoch milliseconds).
    pub fn new(model: ApprovalModel, sink: Arc<dyn AnswerSink>, now_ms: i64) -> Self {
        Self {
            model,
            sink,
            now_ms,
        }
    }

    pub fn model(&self) -> &ApprovalModel {
        &self.model
    }

    /// Moves the clock; the time left on the card follows.
    pub fn set_now(&mut self, now_ms: i64, cx: &mut Context<Self>) {
        self.now_ms = now_ms;
        cx.notify();
    }

    /// A click on an option: the model decides whether it is an answer; if so the sink records
    /// it on the background executor, and the reply lands back in the model.
    fn choose(&mut self, option_id: &str, cx: &mut Context<Self>) {
        let Some(request) = self.model.begin_answer(option_id, self.now_ms) else {
            return;
        };
        cx.notify();
        let sink = Arc::clone(&self.sink);
        cx.spawn(async move |this, cx| {
            let reply = cx
                .background_executor()
                .spawn(async move { sink.answer(&request) })
                .await;
            this.update(cx, |this, cx| {
                this.model.apply_reply(reply);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// One option's button: allow kinds green, reject kinds red, disabled once the card takes no
/// more answers. Takes owned data only (Rust 2024 captures every argument in the return type).
/// The button sits in an observed wrapper that carries the option's test id: a `Button` is not
/// itself observable, and clicking the wrapper's centre clicks the button.
fn option_button(
    id: String,
    name: String,
    tone: ButtonTone,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let button = Button::new(format!("{id}-button"))
        .label(name)
        .disabled(!enabled);
    let button = match tone {
        ButtonTone::Allow => button.success(),
        ButtonTone::Reject => button.danger(),
    };
    div().id(id).test_support().child(button.on_click(on_click))
}

impl Render for ApprovalCard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = self.model.buttons_enabled(self.now_ms);
        let status = self.model.status_text(self.now_ms);
        let title = self.model.title().to_owned();
        let summary = self.model.summary().to_owned();
        let tool_call = self
            .model
            .tool_call_id()
            .map(|id| format!("Tool call {id}"))
            .unwrap_or_default();
        let mut buttons = Vec::new();
        for option in self.model.options() {
            let option_id = option.option_id.clone();
            let on_click = cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.choose(&option_id, cx);
            });
            buttons.push(
                option_button(
                    option_button_id(&option.option_id),
                    option.name.clone(),
                    option.tone,
                    enabled,
                    on_click,
                )
                .into_any_element(),
            );
        }
        let theme = cx.theme();
        let (foreground, muted, border, background) = (
            theme.foreground,
            theme.muted_foreground,
            theme.border,
            theme.background,
        );
        v_flex()
            .id(CARD_ID)
            .test_support()
            .aria_label(title.clone())
            .gap_2()
            .p_3()
            .rounded(px(6.))
            .border_1()
            .border_color(border)
            .bg(background)
            .text_color(foreground)
            .child(
                div()
                    .id("approval-title")
                    .font_weight(FontWeight::BOLD)
                    .child(title),
            )
            .child(
                div()
                    .id(SUMMARY_ID)
                    .test_support()
                    .aria_label(summary.clone())
                    .text_sm()
                    .child(summary),
            )
            .when(!tool_call.is_empty(), |el| {
                el.child(
                    div()
                        .id("approval-tool-call")
                        .text_xs()
                        .text_color(muted)
                        .child(tool_call),
                )
            })
            .child(
                div()
                    .id(STATUS_ID)
                    .test_support()
                    .aria_label(status.clone())
                    .text_sm()
                    .text_color(muted)
                    .child(status),
            )
            .child(h_flex().id("approval-options").gap_2().children(buttons))
    }
}

/// Opens an approval card in a window (via `gpui_kit::open_window`, so it gets the `Root`
/// wrapper). Call `gpui_kit::init` first.
pub fn open_approval_card(
    options: WindowOptions,
    model: ApprovalModel,
    sink: Arc<dyn AnswerSink>,
    now_ms: i64,
    cx: &mut App,
) -> Result<(AnyWindowHandle, Entity<ApprovalCard>)> {
    gpui_kit::open_window(options, cx, |_, cx| {
        cx.new(|_| ApprovalCard::new(model, sink, now_ms))
    })
}
