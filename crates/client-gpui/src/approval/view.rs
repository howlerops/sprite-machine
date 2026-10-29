//! Drawing an [`ApprovalCard`]. Stateless: the host passes cards and a handler, the card turns
//! clicks into [`CardAction`]s and never touches the network or the outbox (CONVENTIONS.md §6).
//! Every string from the agent (title, message, diff, command) reaches `div().child(String)`
//! only, never a markup parser.

use std::rc::Rc;

use gpui_kit::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::{Disableable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use protocol::turn_event::PermissionOptionKind;

use super::model::{ApprovalCard, CardState, DiffLine, Pending};
use super::{tr, tr_args};

/// What the user did on a card. The host turns it into calls on the controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardAction {
    /// An option button was clicked; `option_id` is the agent's own id for it.
    Answer {
        interrupt_id: String,
        option_id: String,
    },
    /// The "save as this workspace's preset" box was ticked or cleared.
    SetPreset { interrupt_id: String, checked: bool },
}

/// Receives the card's actions.
pub type CardHandler = Rc<dyn Fn(CardAction, &mut Window, &mut App)>;

/// A stack of cards.
pub fn render_cards(cards: &[ApprovalCard], handler: Option<CardHandler>) -> impl IntoElement {
    v_flex()
        .id("approval-cards")
        .test_support()
        .w_full()
        .gap_2()
        .p_2()
        .children(
            cards
                .iter()
                .cloned()
                .map(|card| render_card(card, handler.clone())),
        )
}

/// One card, by value so the element owns its data.
pub fn render_card(card: ApprovalCard, handler: Option<CardHandler>) -> impl IntoElement {
    let id = card.interrupt_id.clone();
    let label = tr_args("approval-region-a11y", &[("detail", &card.title)]);
    v_flex()
        .id(SharedString::from(format!("approval-card-{id}")))
        .role(Role::Group)
        .test_support()
        .aria_label(label)
        .w_full()
        .gap_2()
        .p_3()
        .rounded_md()
        .border_1()
        .child(header(&card))
        .when_some(card.message.clone(), |el, message| {
            el.child(
                div()
                    .id("approval-message")
                    .test_support()
                    .aria_label(message.clone())
                    .text_sm()
                    .child(message),
            )
        })
        .children(card.pending.iter().cloned().map(pending_block))
        .child(options_row(&card, handler.clone()))
        .when_some(card.preset.clone(), |el, offer| {
            el.child(preset_row(&card.interrupt_id, offer, &card.state, handler))
        })
        .child(status_line(&card.state))
}

fn header(card: &ApprovalCard) -> impl IntoElement + use<> {
    let title = SharedString::from(card.title.clone());
    let remaining = card.remaining_text.clone().map(SharedString::from);
    let expired = card.state == CardState::Expired;
    h_flex()
        .w_full()
        .gap_2()
        .items_center()
        .justify_between()
        .child(
            div()
                .id("approval-title")
                .test_support()
                .aria_label(title.clone())
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
        .when_some(remaining, |el, text| {
            el.child(
                div()
                    .id("approval-time-left")
                    .role(Role::Timer)
                    .test_support()
                    .aria_label(text.clone())
                    .text_sm()
                    .child(text),
            )
        })
        .when(expired, |el| {
            el.child(
                div()
                    .id("approval-expired")
                    .test_support()
                    .aria_label(tr("approval-expired"))
                    .text_sm()
                    .child(tr("approval-expired")),
            )
        })
}

fn pending_block(pending: Pending) -> impl IntoElement {
    match pending {
        Pending::Diff {
            path,
            new_file,
            lines,
            hidden,
            complete,
        } => {
            let heading = if new_file {
                tr_args("approval-diff-new-file", &[("detail", &path)])
            } else {
                tr_args("approval-diff", &[("detail", &path)])
            };
            v_flex()
                .id("approval-diff")
                .test_support()
                .aria_label(heading.clone())
                .gap_1()
                .child(div().text_sm().child(heading))
                .children(
                    lines
                        .into_iter()
                        .enumerate()
                        .map(|(ix, line)| diff_line(ix, line)),
                )
                .when(hidden > 0, |el| el.child(more_lines(hidden)))
                .when(!complete, |el| {
                    el.child(div().text_sm().child(tr("approval-diff-incomplete")))
                })
                .into_any_element()
        }
        Pending::Command {
            command,
            cwd,
            hidden,
        } => v_flex()
            .id("approval-command")
            .test_support()
            .aria_label(command.clone())
            .gap_1()
            .child(div().text_sm().child(tr("approval-command")))
            .when_some(cwd, |el, cwd| {
                el.child(
                    div()
                        .text_sm()
                        .child(tr_args("approval-command-cwd", &[("detail", &cwd)])),
                )
            })
            .child(div().font_family("monospace").child(command))
            .when(hidden > 0, |el| el.child(more_lines(hidden)))
            .into_any_element(),
    }
}

fn diff_line(ix: usize, line: DiffLine) -> impl IntoElement {
    let text = format!("{} {}", line.sign, line.text);
    // A unique outer id per row; the shared inner id is what tests count.
    div().id(("approval-diff-row", ix)).child(
        div()
            .id("approval-diff-line")
            .test_support()
            .aria_label(text.clone())
            .font_family("monospace")
            .child(text),
    )
}

fn more_lines(hidden: usize) -> impl IntoElement {
    div().text_sm().child(tr_args(
        "approval-more-lines",
        &[("detail", &hidden.to_string())],
    ))
}

fn options_row(card: &ApprovalCard, handler: Option<CardHandler>) -> impl IntoElement + use<> {
    let interrupt_id = card.interrupt_id.clone();
    h_flex()
        .id("approval-options")
        .gap_2()
        .flex_wrap()
        .children(card.options.clone().into_iter().map(move |option| {
            let handler = handler.clone();
            let action = CardAction::Answer {
                interrupt_id: interrupt_id.clone(),
                option_id: option.option_id.clone(),
            };
            let button = Button::new(SharedString::from(format!(
                "approval-option-{}",
                option.option_id
            )))
            .label(option.name.clone())
            .accessibility_label(option.a11y.clone())
            .disabled(!option.enabled);
            let button = match option.kind {
                PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways => {
                    button.primary()
                }
                PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways => {
                    button.danger()
                }
            };
            button.on_click(move |_, window, cx| {
                if let Some(handler) = &handler {
                    handler(action.clone(), window, cx);
                }
            })
        }))
}

fn preset_row(
    interrupt_id: &str,
    offer: super::model::PresetOffer,
    state: &CardState,
    handler: Option<CardHandler>,
) -> impl IntoElement + use<> {
    let interrupt_id = interrupt_id.to_owned();
    let enabled = *state == CardState::Open;
    v_flex()
        .id("approval-preset-offer")
        .test_support()
        .aria_label(offer.a11y.clone())
        .gap_1()
        .child(
            Checkbox::new("approval-preset")
                .label(offer.label.clone())
                .checked(offer.checked)
                .disabled(!enabled)
                .on_click(move |checked: &bool, window, cx| {
                    if let Some(handler) = &handler {
                        handler(
                            CardAction::SetPreset {
                                interrupt_id: interrupt_id.clone(),
                                checked: *checked,
                            },
                            window,
                            cx,
                        );
                    }
                }),
        )
        .child(
            div()
                .text_sm()
                .id("approval-preset-note")
                .test_support()
                .aria_label(if offer.saved {
                    tr("approval-preset-saved")
                } else {
                    tr("approval-preset-note")
                })
                .child(if offer.saved {
                    tr("approval-preset-saved")
                } else {
                    tr("approval-preset-note")
                }),
        )
}

fn status_line(state: &CardState) -> impl IntoElement + use<> {
    let text = match state {
        CardState::Open | CardState::Expired => None,
        CardState::Sending => Some(tr("approval-sending")),
        CardState::Failed { message } => {
            Some(tr_args("approval-failed", &[("detail", message.as_str())]))
        }
        CardState::Resolved { summary, .. } | CardState::AlreadyResolved { summary, .. } => {
            Some(summary.clone())
        }
    };
    div().children(text.map(|text| {
        div()
            .id("approval-status")
            .role(Role::Status)
            .test_support()
            .aria_label(text.clone())
            .text_sm()
            .child(text)
    }))
}
