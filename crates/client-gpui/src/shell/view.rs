//! [`AppShell`]: the root view. Login screen, or sidebar + content once signed in. Thin over
//! [`ShellModel`]; every decision lives there.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use client_core::Connectivity;
use client_core::outbox::Clock;
use gpui_kit::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Theme, ThemeMode, *};
use gpui_kit::prelude::*;
use gpui_kit::*;
use gpui_kit::{Task, black, white};
use tokio::sync::watch;

use super::browser::Browser;
use super::i18n::{tr, tr_args};
use super::link::{LinkConfig, SessionLink, ThreadSnapshot};
use super::login::{LoginDriver, LoginEvent, LoginMethod, LoginStage};
use super::model::{FocusSlot, HubOutcome, Screen, ShellModel, theme_a11y};
use super::nav::Section;
use super::pairing::QrMatrix;
use super::prefs::{Appearance, Preferences, ThemeChoice};
use super::session::SessionStore;

use crate::approval::{CardAction, CardHandler};
use crate::inbox::InboxEvent;
use crate::task::TranscriptPane;

actions!(shell, [GoNewTask, NextSection, PreviousSection]);

const CONTEXT: &str = "Shell";

/// How often the open thread's approval cards are recomputed (time left).
pub const APPROVAL_TICK: Duration = Duration::from_secs(1);

/// Register the shell's key bindings. Call once, after `gpui_kit::init`.
pub fn init(cx: &mut App) {
    let new_task = if cfg!(target_os = "macos") {
        "cmd-n"
    } else {
        "ctrl-n"
    };
    crate::compose::init(cx);
    cx.bind_keys([
        KeyBinding::new(new_task, GoNewTask, Some(CONTEXT)),
        KeyBinding::new("ctrl-tab", NextSection, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-tab", PreviousSection, Some(CONTEXT)),
    ]);
}

/// Everything the shell needs from the outside world.
pub struct AppConfig {
    pub hub: String,
    pub browser: Arc<dyn Browser>,
    pub store: Arc<dyn SessionStore>,
    pub prefs: Preferences,
    pub prefs_path: Option<PathBuf>,
    /// Divides the polling sleeps; 1 for a real launch, larger in tests against a local hub.
    pub poll_speedup: u32,
    /// The thread the session's connection follows from the start; empty for none. It follows
    /// whatever thread the screens open from then on ([`AppShell::open_thread`]).
    pub link_thread: String,
    /// Time for the outbox and the approval cards.
    pub clock: Arc<dyn Clock>,
}

pub struct AppShell {
    model: ShellModel,
    driver: LoginDriver,
    focus: FocusHandle,
    hub_input: Entity<InputState>,
    compose: Entity<crate::compose::ComposeView>,
    inbox: Entity<crate::inbox::InboxView>,
    _appearance: Subscription,
    _hub_input_events: Subscription,
    _inbox_events: Subscription,
    connectivity_task: Option<Task<()>>,
    /// The signed-in session's hub link; present exactly while a session is.
    link: Option<SessionLink>,
    link_thread: String,
    clock: Arc<dyn Clock>,
    presets_path: Option<PathBuf>,
    /// The thread shown in the Inbox page, if one was opened.
    open_thread: Option<String>,
    pane: Entity<TranscriptPane>,
    snapshot_task: Option<Task<()>>,
    tick_task: Option<Task<()>>,
    link_retry: Duration,
    cache_path: Option<PathBuf>,
    /// What the last Save of the hub field did, shown under the field.
    hub_feedback: Option<HubOutcome>,
    /// Why the shell is on the login screen without the user having signed out.
    notice: Option<String>,
}

impl AppShell {
    pub fn new(config: AppConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let draft_dir = config
            .prefs_path
            .as_deref()
            .and_then(|p| p.parent())
            .map(PathBuf::from);
        let cache_path = config
            .prefs_path
            .as_deref()
            .and_then(|p| p.parent())
            .map(|dir| dir.join("cache.sqlite"));
        let presets_path = config
            .prefs_path
            .as_deref()
            .and_then(|p| p.parent())
            .map(|dir| dir.join("approval-presets.json"));
        let link_retry =
            Duration::from_millis((1000 / u64::from(config.poll_speedup.max(1))).max(5));
        let model = ShellModel::new(config.store, config.prefs, config.prefs_path, &config.hub);
        let driver = LoginDriver::new(model.hub().to_string(), config.browser)
            .with_sleep_divisor(config.poll_speedup);
        let focus = cx.focus_handle();
        let appearance = cx.observe_window_appearance(window, |this, window, cx| {
            this.apply_theme(window, cx);
        });
        let hub = model.hub().to_string();
        let hub_input = cx.new(|cx| InputState::new(window, cx).default_value(hub));
        let hub_input_events = cx.subscribe_in(&hub_input, window, Self::on_hub_event);
        let compose = cx.new(|cx| crate::compose::ComposeView::new(draft_dir, window, cx));
        let inbox = cx.new(|cx| crate::inbox::InboxView::new(window, cx));
        // A row click in the inbox opens that thread on the Inbox page.
        let inbox_events = cx.subscribe(&inbox, |this, _, event: &InboxEvent, cx| {
            let InboxEvent::OpenThread {
                thread_id,
                workspace,
            } = event;
            this.open_thread(thread_id, workspace.as_deref(), cx);
        });
        let pane = cx.new(|_| TranscriptPane::new());
        let mut shell = Self {
            model,
            driver,
            focus,
            hub_input,
            compose,
            inbox,
            _appearance: appearance,
            _hub_input_events: hub_input_events,
            _inbox_events: inbox_events,
            connectivity_task: None,
            link: None,
            link_thread: config.link_thread,
            clock: config.clock,
            presets_path,
            open_thread: None,
            pane,
            snapshot_task: None,
            tick_task: None,
            link_retry,
            cache_path,
            hub_feedback: None,
            notice: None,
        };
        shell.apply_theme(window, cx);
        shell.refresh_link(cx);
        shell
    }

    pub fn model(&self) -> &ShellModel {
        &self.model
    }

    pub fn driver(&self) -> &LoginDriver {
        &self.driver
    }

    /// The settings field that edits the hub address.
    pub fn hub_input(&self) -> &Entity<InputState> {
        &self.hub_input
    }

    /// The signed-in session's hub link, if there is a session.
    pub fn link(&self) -> Option<&SessionLink> {
        self.link.as_ref()
    }

    /// What the last Save of the hub field did.
    pub fn hub_feedback(&self) -> Option<HubOutcome> {
        self.hub_feedback
    }

    /// Keep the hub link in step with the session: open one when a session appears (at launch
    /// or after sign-in) and follow its connectivity; drop it when the session ends.
    fn refresh_link(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.model.session() else {
            if self.link.take().is_some() {
                self.connectivity_task = None;
                self.snapshot_task = None;
                self.tick_task = None;
                self.open_thread = None;
                self.set_connectivity(Connectivity::Connecting, cx);
            }
            return;
        };
        if self.link.is_some() {
            return;
        }
        let config = LinkConfig {
            hub: session.hub.clone(),
            token: session.grant.token.clone(),
            thread_id: self.link_thread.clone(),
            cache_path: self.cache_path.clone(),
            presets_path: self.presets_path.clone(),
            retry: self.link_retry,
            clock: self.clock.clone(),
        };
        match SessionLink::open(config) {
            Ok(link) => {
                let feed = link.connectivity_feed();
                let snapshots = link.snapshot_feed();
                self.link = Some(link);
                self.follow_connectivity(feed, cx);
                self.follow_snapshots(snapshots, cx);
                self.start_tick(cx);
            }
            Err(_) => self.set_connectivity(Connectivity::Offline, cx),
        }
    }

    /// The thread the Inbox page shows, if one was opened.
    pub fn open_thread_id(&self) -> Option<&str> {
        self.open_thread.as_deref()
    }

    /// The pane that shows the open thread and its approval cards.
    pub fn thread_pane(&self) -> &Entity<TranscriptPane> {
        &self.pane
    }

    /// Shows `thread_id` on the Inbox page and makes it the thread the session link follows
    /// (the inbox calls this when a row is opened). `workspace` is the thread's workspace, for
    /// `allow_always` presets. Does nothing while signed out.
    pub fn open_thread(
        &mut self,
        thread_id: &str,
        workspace: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let Some(link) = &self.link else { return };
        link.open_thread(thread_id, workspace.unwrap_or_default());
        self.link_thread = thread_id.to_owned();
        if self.open_thread.as_deref() != Some(thread_id) {
            self.open_thread = Some(thread_id.to_owned());
            self.pane = cx.new(|_| {
                let mut pane = TranscriptPane::new();
                pane.set_follow_tail(true);
                pane
            });
            // The link may already hold this thread (opened again after going back).
            let current = self
                .link
                .as_ref()
                .and_then(|link| link.snapshot_feed().borrow().clone());
            if let Some(snapshot) = current {
                self.apply_snapshot(snapshot, cx);
            }
        }
        self.model.nav.select(Section::Inbox);
        cx.notify();
    }

    /// Back to the thread list. The link keeps following the thread it has.
    pub fn close_thread(&mut self, cx: &mut Context<Self>) {
        if self.open_thread.take().is_some() {
            cx.notify();
        }
    }

    /// Show every snapshot of the open thread in the pane: the transcript, and the approval
    /// cards pinned above it with clicks routed to the link.
    fn follow_snapshots(
        &mut self,
        mut feed: watch::Receiver<Option<ThreadSnapshot>>,
        cx: &mut Context<Self>,
    ) {
        self.snapshot_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let snapshot = feed.borrow_and_update().clone();
                if let Some(snapshot) = snapshot
                    && this
                        .update(cx, |this, cx| this.apply_snapshot(snapshot, cx))
                        .is_err()
                {
                    break;
                }
                if feed.changed().await.is_err() {
                    break;
                }
            }
        }));
    }

    fn apply_snapshot(&mut self, snapshot: ThreadSnapshot, cx: &mut Context<Self>) {
        if self.open_thread.as_deref() != Some(snapshot.thread_id.as_str()) {
            return;
        }
        let shell = cx.weak_entity();
        let handler: CardHandler = Rc::new(move |action, _, cx| {
            let _ = shell.update(cx, |shell, _| shell.on_card_action(action));
        });
        self.pane.update(cx, |pane, cx| {
            pane.set_thread(&snapshot.view, cx);
            pane.set_approvals(snapshot.cards, Some(handler), cx);
        });
        cx.notify();
    }

    /// A click on a card: the answer goes through the link's outbox (and is flushed there).
    fn on_card_action(&mut self, action: CardAction) {
        let Some(link) = &self.link else { return };
        match action {
            CardAction::Answer {
                interrupt_id,
                option_id,
            } => drop(link.answer(&interrupt_id, &option_id)),
            CardAction::SetPreset {
                interrupt_id,
                checked,
            } => link.set_remember_preset(&interrupt_id, checked),
        }
    }

    /// Once per [`APPROVAL_TICK`], while a thread is open, ask the link to recompute the cards
    /// so the time left repaints.
    fn start_tick(&mut self, cx: &mut Context<Self>) {
        self.tick_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(APPROVAL_TICK).await;
                let alive = this
                    .update(cx, |this, _| {
                        if let (Some(link), Some(_)) = (&this.link, &this.open_thread) {
                            link.refresh();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        }));
    }

    /// Apply the chosen theme (or the system's) to every window.
    fn apply_theme(&self, window: &mut Window, cx: &mut App) {
        let system_dark = ThemeMode::from(window.appearance()).is_dark();
        let mode = match self.model.appearance(system_dark) {
            Appearance::Light => ThemeMode::Light,
            Appearance::Dark => ThemeMode::Dark,
        };
        Theme::change(mode, Some(window), cx);
    }

    pub fn choose_theme(
        &mut self,
        choice: ThemeChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A failed save keeps the choice for this run; the next launch falls back to the old one.
        let _ = self.model.set_theme(choice);
        self.apply_theme(window, cx);
        cx.notify();
    }

    pub fn set_connectivity(&mut self, connectivity: Connectivity, cx: &mut Context<Self>) {
        self.model.set_connectivity(connectivity);
        cx.notify();
    }

    /// Drive the banner from a client-core connection: its state now, then every change of the
    /// banner state (see `Connection::watch_connectivity`). Replaces any earlier feed.
    pub fn follow_connectivity(
        &mut self,
        mut feed: watch::Receiver<Connectivity>,
        cx: &mut Context<Self>,
    ) {
        let current = *feed.borrow_and_update();
        self.set_connectivity(current, cx);
        self.connectivity_task = Some(cx.spawn(async move |this, cx| {
            while feed.changed().await.is_ok() {
                let state = *feed.borrow_and_update();
                if this
                    .update(cx, |this, cx| this.set_connectivity(state, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn on_hub_event(
        &mut self,
        _input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { .. } => self.commit_hub(window, cx),
            // Typing makes the last answer stale; the field showing the saved address again
            // (after a Save) is not typing.
            InputEvent::Change => {
                let typed = self.hub_input.read(cx).value();
                if typed.as_ref() != self.model.hub() && self.hub_feedback.take().is_some() {
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    /// Enter or the Save control saves the address. A refused address changes nothing, so the
    /// field goes back to the hub in use. Saving a different hub while signed in signs out, and
    /// the login screen says why.
    fn commit_hub(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.hub_input.read(cx).value().to_string();
        let outcome = self.model.change_hub(&typed);
        self.hub_feedback = Some(outcome);
        if outcome == HubOutcome::SavedSignedOut {
            self.notice = Some(tr_args(
                "login-notice-hub-changed",
                &[("hub", self.model.hub())],
            ));
            self.refresh_link(cx);
        }
        self.driver.set_hub(self.model.hub());
        let hub = self.model.hub().to_string();
        self.hub_input
            .update(cx, |input, cx| input.set_value(hub, window, cx));
        cx.notify();
    }

    pub fn select(&mut self, section: Section, cx: &mut Context<Self>) {
        self.model.nav.select(section);
        cx.notify();
    }

    pub fn start_login(&mut self, method: LoginMethod, cx: &mut Context<Self>) {
        self.notice = None;
        let _ = self.model.apply_login(LoginEvent::Started(method));
        self.driver.set_hub(self.model.hub());
        self.driver.start(method);
        self.watch_login(cx);
        cx.notify();
    }

    pub fn cancel_login(&mut self, cx: &mut Context<Self>) {
        self.driver.cancel();
        let _ = self.model.apply_login(LoginEvent::Reset);
        cx.notify();
    }

    pub fn sign_out(&mut self, cx: &mut Context<Self>) {
        let _ = self.model.sign_out();
        self.notice = None;
        self.refresh_link(cx);
        cx.notify();
    }

    /// Move events from the background sign-in into the model.
    pub fn pump_login(&mut self, cx: &mut Context<Self>) {
        let events = self.driver.drain();
        if events.is_empty() {
            return;
        }
        for event in events {
            let _ = self.model.apply_login(event);
        }
        self.refresh_link(cx);
        cx.notify();
    }

    /// Poll the driver while a flow is running.
    fn watch_login(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let keep_going = this
                    .update(cx, |this, cx| {
                        this.pump_login(cx);
                        this.model.login.is_busy()
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    fn tab_index(slots: &[FocusSlot], id: &str) -> isize {
        slots
            .iter()
            .position(|slot| slot.id == id)
            .map_or(0, |position| position as isize + 1)
    }

    fn button(slots: &[FocusSlot], id: &str, label: String) -> Button {
        let a11y = slots
            .iter()
            .find(|slot| slot.id == id)
            .map(|slot| slot.label.clone())
            .unwrap_or_else(|| label.clone());
        Button::new(SharedString::from(id.to_string()))
            .label(label)
            .accessibility_label(a11y)
            .tab_index(Self::tab_index(slots, id))
    }

    fn status(id: &'static str, text: String, cx: &App) -> impl IntoElement {
        div()
            .id(id)
            .role(Role::Status)
            .test_support()
            .aria_label(text.clone())
            .text_color(cx.theme().muted_foreground)
            .child(text)
    }

    fn render_login(&mut self, slots: &[FocusSlot], cx: &mut Context<Self>) -> impl IntoElement {
        let stage = self.model.login.stage().clone();
        let status = self.model.login.status_text();
        let failed = matches!(stage, LoginStage::Failed(_));
        let choosing = matches!(stage, LoginStage::Choose);
        v_flex()
            .id("login")
            .size_full()
            .items_center()
            .justify_center()
            .gap_4()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(div().id("login-title").text_xl().child(tr("login-title")))
            .child(Self::status("login-status", status, cx))
            .when_some(self.notice.clone(), |el, notice| {
                el.child(Self::status("login-notice", notice, cx))
            })
            .when(choosing, |el| {
                el.child(
                    h_flex()
                        .gap_3()
                        .child(
                            Self::button(slots, "login-pair", tr("login-pair"))
                                .primary()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.start_login(LoginMethod::Pairing, cx);
                                })),
                        )
                        .child(
                            Self::button(slots, "login-github", tr("login-github")).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.start_login(LoginMethod::GitHub, cx);
                                }),
                            ),
                        ),
                )
            })
            .when_some(pairing_panel(&stage), |el, panel| el.child(panel))
            .when(failed, |el| {
                el.child(
                    Self::button(slots, "login-retry", tr("login-retry")).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.cancel_login(cx);
                        },
                    )),
                )
            })
            .when(self.model.login.is_busy(), |el| {
                el.child(
                    Self::button(slots, "login-cancel", tr("login-cancel")).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.cancel_login(cx);
                        },
                    )),
                )
            })
    }

    fn render_main(&mut self, slots: &[FocusSlot], cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.model.nav.current();
        let banner = self.model.banner();
        let sidebar = v_flex()
            .id("sidebar")
            .role(Role::Navigation)
            .test_support()
            .aria_label(tr("nav-label"))
            .w(px(220.))
            .h_full()
            .p_2()
            .gap_1()
            .border_r_1()
            .border_color(cx.theme().border)
            .children(Section::ALL.map(|section| {
                Self::button(slots, &section.nav_id(), section.title())
                    .ghost()
                    .w_full()
                    .selected(section == current)
                    .on_click(cx.listener(move |this, _, _, cx| this.select(section, cx)))
            }));
        let content = if current == Section::Settings {
            self.render_settings(slots, cx).into_any_element()
        } else if current == Section::NewTask {
            self.compose.clone().into_any_element()
        } else if current == Section::Inbox && self.open_thread.is_some() {
            self.render_thread(slots, cx).into_any_element()
        } else if current == Section::Inbox {
            self.inbox.clone().into_any_element()
        } else {
            let text = tr_args("content-placeholder", &[("section", &current.title())]);
            div().child(text).into_any_element()
        };
        h_flex()
            .id("main")
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(sidebar)
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .when_some(banner, |el, text| {
                        el.child(
                            div()
                                .id("connectivity-banner")
                                .role(Role::Status)
                                .test_support()
                                .aria_label(text.clone())
                                .px_3()
                                .py_1()
                                .bg(cx.theme().warning)
                                .text_color(cx.theme().warning_foreground)
                                .child(text),
                        )
                    })
                    .child(
                        div()
                            .id("content")
                            .role(Role::Main)
                            .test_support()
                            .aria_label(tr("content-a11y"))
                            .flex_1()
                            .p_4()
                            .child(content),
                    ),
            )
    }

    /// The open thread: a way back to the list, then the transcript with its approval cards.
    fn render_thread(&mut self, slots: &[FocusSlot], cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("thread")
            .size_full()
            .gap_2()
            .child(
                Self::button(slots, "thread-back", Section::Inbox.title())
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| this.close_thread(cx))),
            )
            .child(div().flex_1().min_h_0().child(self.pane.clone()))
    }

    fn render_settings(&mut self, slots: &[FocusSlot], cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.model.prefs().theme;
        let account = self
            .model
            .session()
            .map(|session| {
                tr_args(
                    "settings-signed-in-as",
                    &[
                        ("user", &session.user_label()),
                        ("org", &session.org_label()),
                    ],
                )
            })
            .unwrap_or_else(|| tr("settings-not-signed-in"));
        v_flex()
            .id("settings")
            .gap_4()
            .child(div().text_xl().child(tr("settings-title")))
            .child(div().child(tr("settings-appearance")))
            .child(h_flex().gap_2().children(ThemeChoice::ALL.map(|choice| {
                let id = format!("theme-{}", choice.slug());
                Self::button(slots, &id, tr(choice.message_id()))
                    .selected(choice == selected)
                    .accessibility_label(theme_a11y(choice, choice == selected))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.choose_theme(choice, window, cx);
                    }))
            })))
            .child(div().child(format!(
                "{}: {}",
                tr("settings-language"),
                tr("settings-language-en")
            )))
            .child(
                v_flex()
                    .gap_1()
                    .child(div().child(tr("settings-hub")))
                    .child(
                        div()
                            .id("hub-address")
                            .test_support()
                            .aria_label(tr("settings-hub"))
                            .child(Input::new(&self.hub_input)),
                    )
                    .child(
                        Self::button(slots, "hub-save", tr("settings-hub-save")).on_click(
                            cx.listener(|this, _, window, cx| this.commit_hub(window, cx)),
                        ),
                    )
                    .when_some(self.hub_feedback, |el, outcome| {
                        el.child(Self::status("hub-feedback", hub_feedback_text(outcome), cx))
                    }),
            )
            .child(
                div()
                    .id("account")
                    .test_support()
                    .aria_label(account.clone())
                    .child(account),
            )
            .child(
                Self::button(slots, "sign-out", tr("settings-sign-out")).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.sign_out(cx);
                    },
                )),
            )
    }
}

fn hub_feedback_text(outcome: HubOutcome) -> String {
    tr(match outcome {
        HubOutcome::Saved => "settings-hub-saved",
        HubOutcome::SavedSignedOut => "settings-hub-saved-signed-out",
        HubOutcome::Refused => "settings-hub-refused",
        HubOutcome::NotStored => "settings-hub-not-stored",
    })
}

/// The QR code, the typed code and the instructions while pairing.
fn pairing_panel(stage: &LoginStage) -> Option<impl IntoElement + use<>> {
    let LoginStage::Pairing {
        user_code,
        qr_target,
        ..
    } = stage
    else {
        return None;
    };
    let qr = QrMatrix::encode(qr_target)?;
    let quiet = 2usize;
    let module = 4.0_f32;
    let side = (qr.width() + quiet * 2) as f32 * module;
    let rows = (0..qr.width()).map(|y| {
        qr.dark_runs(y).into_iter().map(move |(start, len)| {
            div()
                .absolute()
                .left(px((start + quiet) as f32 * module))
                .top(px((y + quiet) as f32 * module))
                .w(px(len as f32 * module))
                .h(px(module))
                .bg(black())
        })
    });
    Some(
        v_flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .id("login-qr")
                    .role(Role::Image)
                    .test_support()
                    .aria_label(tr("login-qr-a11y"))
                    .relative()
                    .w(px(side))
                    .h(px(side))
                    .bg(white())
                    .children(rows.flatten()),
            )
            .child(
                div()
                    .id("login-user-code")
                    .test_support()
                    .aria_label(tr_args("login-pairing-code-a11y", &[("code", user_code)]))
                    .text_xl()
                    .child(tr_args("login-pairing-code", &[("code", user_code)])),
            ),
    )
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The shell takes focus only when nothing has it, so a field the user is typing in keeps it.
        if window.focused(cx).is_none() {
            window.focus(&self.focus, cx);
        }
        let target = self.model.session().map(|session| crate::compose::Target {
            api: format!("{}/v1", session.hub.trim_end_matches('/')),
            token: Some(session.grant.token.clone()),
        });
        self.compose
            .update(cx, |compose, cx| compose.set_target(target, window, cx));
        let inbox_target = self.model.session().map(|session| crate::inbox::Target {
            api: format!("{}/v1", session.hub.trim_end_matches('/')),
            token: Some(session.grant.token.clone()),
        });
        self.inbox
            .update(cx, |inbox, cx| inbox.set_target(inbox_target, window, cx));
        let slots = self.model.focus_order();
        let body = match self.model.screen() {
            Screen::Login => self.render_login(&slots, cx).into_any_element(),
            Screen::Main => self.render_main(&slots, cx).into_any_element(),
        };
        div()
            .id("app-shell")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .on_action(cx.listener(|this, _: &GoNewTask, _, cx| {
                if this.model.screen() == Screen::Main {
                    this.select(Section::NewTask, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextSection, _, cx| {
                if this.model.screen() == Screen::Main {
                    this.model.nav.next();
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &PreviousSection, _, cx| {
                if this.model.screen() == Screen::Main {
                    this.model.nav.previous();
                    cx.notify();
                }
            }))
            .child(body)
    }
}
