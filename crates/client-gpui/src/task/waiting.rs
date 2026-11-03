//! "Waiting for <machine>" (plan §4.10 "Sleep and disconnects", M2B-013): while the machine that
//! hosts the open thread is offline or asleep, the task view says so and lists the messages that
//! wait on the hub for it. The shell builds a [`Waiting`] from the machines feed and the thread's
//! queued messages; nothing is shown while the machine is online or not in the feed.

use gpui_kit::TestSupportExt as _;
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::compose::MachineOption;
use crate::machines::{tr, tr_args};

/// Element id of the banner.
pub const WAITING_ID: &str = "thread-waiting";
/// Element id carried by every queued item under the banner.
pub const WAITING_ITEM_ID: &str = "thread-waiting-item";

/// The banner's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    pub machine_name: String,
    /// Previews of the queued messages, oldest first.
    pub items: Vec<String>,
}

/// `Some` when `machine` (the thread's host as the machines feed lists it) is not online.
pub fn waiting_for(machine: Option<&MachineOption>, items: Vec<String>) -> Option<Waiting> {
    let machine = machine.filter(|machine| !machine.online)?;
    Some(Waiting {
        machine_name: machine.name.clone(),
        items,
    })
}

/// Draw the banner and its queued items.
pub fn render_waiting(waiting: &Waiting, cx: &App) -> impl IntoElement + use<> {
    let machine = waiting.machine_name.as_str();
    let text = tr_args("machines-waiting", &[("machine", machine)]);
    let a11y = tr_args(
        "machines-waiting-a11y",
        &[
            ("machine", machine),
            ("count", &waiting.items.len().to_string()),
        ],
    );
    let items = waiting.items.iter().map(|preview| {
        let preview = if preview.trim().is_empty() {
            tr("machines-waiting-item-empty")
        } else {
            preview.clone()
        };
        div()
            .id(WAITING_ITEM_ID)
            .test_support()
            .aria_label(tr_args(
                "machines-waiting-item-a11y",
                &[("machine", machine), ("preview", &preview)],
            ))
            .text_sm()
            .truncate()
            .child(preview)
    });
    v_flex()
        .id(WAITING_ID)
        .test_support()
        .role(Role::Status)
        .aria_label(a11y)
        .px_3()
        .py_1()
        .gap_1()
        .bg(cx.theme().warning)
        .text_color(cx.theme().warning_foreground)
        .child(text)
        .children(items)
}
