//! [`OnboardingView`]: the first-run page. A thin view over [`super::model`].

use gpui_kit::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::model::{PathId, RC_COMMAND, login_command, paths};
use super::tr;

/// Element ids, for tests and the accessibility tree.
pub const VIEW_ID: &str = "onboarding";
pub const MODEL_KEY_ID: &str = "onboarding-model-key";
pub const MODEL_KEY_REASON_ID: &str = "onboarding-model-key-reason";
pub const BYO_ID: &str = "onboarding-byo";
pub const BYO_REASON_ID: &str = "onboarding-byo-reason";
pub const LOGIN_COMMAND_ID: &str = "onboarding-login-command";
pub const RC_COMMAND_ID: &str = "onboarding-rc-command";
pub const EXPECT_ID: &str = "onboarding-rc-expect";
pub const COPY_LOGIN_ID: &str = "onboarding-copy-login";
pub const COPY_RC_ID: &str = "onboarding-copy-rc";
pub const WAITING_ID: &str = "onboarding-waiting";
pub const SKIP_ID: &str = "onboarding-skip";

/// What the page tells the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingEvent {
    /// The user chose to look around without a machine.
    Skip,
}

impl EventEmitter<OnboardingEvent> for OnboardingView {}

/// The texts and ids of one path whose hub API does not exist yet.
struct DisabledPath {
    title: &'static str,
    body: &'static str,
    button_id: &'static str,
    button: &'static str,
    a11y: &'static str,
    reason_id: &'static str,
    reason: Option<&'static str>,
}

pub struct OnboardingView {
    hub: String,
    /// Which command was copied last (shown as "Copied" until the page is left).
    copied: Option<&'static str>,
}

impl OnboardingView {
    pub fn new(hub: &str) -> Self {
        Self {
            hub: hub.to_owned(),
            copied: None,
        }
    }

    /// The hub the sign-in command names.
    pub fn set_hub(&mut self, hub: &str, cx: &mut Context<Self>) {
        if self.hub != hub {
            hub.clone_into(&mut self.hub);
            cx.notify();
        }
    }

    fn text(id: &'static str, text: String, cx: &App) -> impl IntoElement + use<> {
        div()
            .id(id)
            .test_support()
            .aria_label(text.clone())
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(text)
    }

    /// A command in a box, with its Copy button.
    fn command(
        &self,
        id: &'static str,
        command: String,
        copy_id: &'static str,
        copy_a11y: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let copied = self.copied == Some(copy_id);
        let to_copy = command.clone();
        h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .id(id)
                    .test_support()
                    .aria_label(command.clone())
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().muted)
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(command),
            )
            .child(
                Button::new(copy_id)
                    .label(tr(if copied {
                        "onboarding-copied"
                    } else {
                        "onboarding-copy"
                    }))
                    .accessibility_label(tr(copy_a11y))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(to_copy.clone()));
                        this.copied = Some(copy_id);
                        cx.notify();
                    })),
            )
    }

    /// A path that cannot be used yet: its title, a disabled button, and why.
    fn disabled_path(path: DisabledPath, cx: &App) -> impl IntoElement + use<> {
        let DisabledPath {
            title,
            body,
            button_id,
            button,
            a11y,
            reason_id,
            reason,
        } = path;
        v_flex()
            .gap_1()
            .child(div().text_lg().child(tr(title)))
            .child(div().text_sm().child(tr(body)))
            .child(
                div().child(
                    Button::new(button_id)
                        .label(tr(button))
                        .accessibility_label(tr(a11y))
                        .disabled(reason.is_some()),
                ),
            )
            .when_some(reason, |el, reason| {
                el.child(Self::text(reason_id, tr(reason), cx))
            })
    }
}

impl Render for OnboardingView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let [key, byo, _rc] = paths();
        let divider = cx.theme().border;
        v_flex()
            .id(VIEW_ID)
            .role(Role::Region)
            .test_support()
            .aria_label(tr("onboarding-region-a11y"))
            .size_full()
            .gap_4()
            .child(div().text_xl().child(tr("onboarding-title")))
            .child(div().child(tr("onboarding-intro")))
            .child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(divider)
                    .child(div().text_lg().child(tr("onboarding-rc-title")))
                    .child(div().text_sm().child(tr("onboarding-rc-body")))
                    .child(div().child(tr("onboarding-rc-step-login")))
                    .child(self.command(
                        LOGIN_COMMAND_ID,
                        login_command(&self.hub),
                        COPY_LOGIN_ID,
                        "onboarding-copy-login-a11y",
                        cx,
                    ))
                    .child(div().child(tr("onboarding-rc-step-rc")))
                    .child(self.command(
                        RC_COMMAND_ID,
                        RC_COMMAND.to_owned(),
                        COPY_RC_ID,
                        "onboarding-copy-rc-a11y",
                        cx,
                    ))
                    .child(Self::text(EXPECT_ID, tr("onboarding-rc-expect"), cx))
                    .child(div().child(tr("onboarding-rc-step-wait")))
                    .child(
                        div()
                            .id(WAITING_ID)
                            .role(Role::Status)
                            .test_support()
                            .aria_label(tr("onboarding-waiting"))
                            .text_color(cx.theme().muted_foreground)
                            .child(tr("onboarding-waiting")),
                    ),
            )
            .child(Self::disabled_path(
                DisabledPath {
                    title: "onboarding-model-key-title",
                    body: "onboarding-model-key-body",
                    button_id: MODEL_KEY_ID,
                    button: "onboarding-model-key-button",
                    a11y: "onboarding-model-key-a11y",
                    reason_id: MODEL_KEY_REASON_ID,
                    reason: key.disabled_reason,
                },
                cx,
            ))
            .child(Self::disabled_path(
                DisabledPath {
                    title: "onboarding-byo-title",
                    body: "onboarding-byo-body",
                    button_id: BYO_ID,
                    button: "onboarding-byo-button",
                    a11y: "onboarding-byo-a11y",
                    reason_id: BYO_REASON_ID,
                    reason: byo.disabled_reason,
                },
                cx,
            ))
            .child(
                div().child(
                    Button::new(SKIP_ID)
                        .ghost()
                        .label(tr("onboarding-skip"))
                        .accessibility_label(tr("onboarding-skip-a11y"))
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(OnboardingEvent::Skip))),
                ),
            )
    }
}

impl PathId {
    /// Element id of the path's button (or of its command, for the `sm rc` path).
    pub fn element_id(self) -> &'static str {
        match self {
            Self::ModelKey => MODEL_KEY_ID,
            Self::ByoUrl => BYO_ID,
            Self::RunRc => RC_COMMAND_ID,
        }
    }
}
