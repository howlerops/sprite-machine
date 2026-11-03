//! The composer's **Where** picker (plan §4.10 "Dispatch", M2B-013): the machines with their
//! presence, plus cloud workspaces. Cloud workspaces do not exist yet (M4), so that choice is
//! shown disabled with its reason instead of being left out.
//!
//! An offline machine can still be chosen: the hub queues the task and delivers it when the
//! machine is back (tunnel.md §5), so the picker says so instead of blocking the send.

use gpui_kit::TestSupportExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, h_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::new::MachineOption;
use super::{tr, tr_args};

/// Element id of the disabled cloud choice.
pub const CLOUD_ID: &str = "where-cloud";
/// Element id of the line that gives the cloud choice's reason.
pub const CLOUD_REASON_ID: &str = "where-cloud-reason";
/// Element id of the "this machine is offline, the task will wait" line.
pub const WAIT_NOTE_ID: &str = "where-wait-note";

/// Whether cloud workspaces exist (they arrive with the cloud sandbox work, plan §7).
pub const CLOUD_WORKSPACES_AVAILABLE: bool = false;

/// Fluent id of why the cloud choice is disabled; `None` once cloud workspaces exist.
pub fn cloud_disabled_reason() -> Option<&'static str> {
    (!CLOUD_WORKSPACES_AVAILABLE).then_some("compose-where-cloud-unavailable")
}

/// The label and screen-reader label of a machine choice: its name, and "offline" when it is.
pub fn machine_labels(machine: &MachineOption, selected: bool) -> (String, String) {
    let label = if machine.online {
        machine.name.clone()
    } else {
        tr_args("compose-machine-offline", &[("name", &machine.name)])
    };
    let key = match (machine.online, selected) {
        (true, false) => "compose-machine-a11y",
        (true, true) => "compose-machine-selected-a11y",
        (false, false) => "compose-machine-offline-a11y",
        (false, true) => "compose-machine-offline-selected-a11y",
    };
    (label, tr_args(key, &[("name", &machine.name)]))
}

/// What the picker says about the chosen machine when it is offline.
pub fn wait_note(machine: Option<&MachineOption>) -> Option<String> {
    let machine = machine.filter(|machine| !machine.online)?;
    Some(tr_args("compose-where-waits", &[("name", &machine.name)]))
}

/// The cloud choice (disabled while cloud workspaces do not exist) and its reason.
pub fn cloud_choice(cx: &App) -> impl IntoElement + use<> {
    let name = tr("compose-where-cloud");
    let reason = cloud_disabled_reason().map(tr);
    let a11y = match &reason {
        Some(reason) => tr_args(
            "compose-where-cloud-a11y-unavailable",
            &[("reason", reason)],
        ),
        None => tr("compose-where-cloud-a11y"),
    };
    let muted = cx.theme().muted_foreground;
    h_flex()
        .gap_2()
        .items_center()
        .child(
            Button::new(CLOUD_ID)
                .label(name)
                .accessibility_label(a11y)
                .disabled(reason.is_some()),
        )
        .when_some(reason, move |el, reason| {
            el.child(
                div()
                    .id(CLOUD_REASON_ID)
                    .test_support()
                    .aria_label(reason.clone())
                    .text_sm()
                    .text_color(muted)
                    .child(reason),
            )
        })
}

/// The "offline, will wait" line.
pub fn wait_line(text: String, cx: &App) -> impl IntoElement + use<> {
    div()
        .id(WAIT_NOTE_ID)
        .test_support()
        .aria_label(text.clone())
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text)
}
