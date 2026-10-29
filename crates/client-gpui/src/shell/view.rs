//! [`AppShell`]: the root view. Login screen, or sidebar + content once signed in. Thin over
//! [`ShellModel`]; every decision lives there.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use client_core::outbox::{Clock, Intent};
use client_core::{Connectivity, TaskStatus};
use gpui_kit::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Theme, ThemeMode, *};
use gpui_kit::prelude::*;
use gpui_kit::*;
use gpui_kit::{Task, black, white};
use protocol::tunnel::ContentPart;
use tokio::sync::watch;

use super::browser::Browser;
use super::i18n::{tr, tr_args};
use super::link::{LinkConfig, SessionLink, ThreadSnapshot};
use super::login::{LoginDriver, LoginEvent, LoginMethod, LoginStage};
use super::machines::{self, MachinesFeed};
use super::model::{FocusSlot, HubOutcome, Screen, ShellModel, theme_a11y};
use super::nav::Section;
use super::pairing::QrMatrix;
use super::prefs::{Appearance, Preferences, ThemeChoice};
use super::session::SessionStore;

use crate::approval::{CardAction, CardHandler};
use crate::compose::{ComposerEvent, ThreadComposer};
use crate::inbox::{Filters, InboxEvent};
use crate::notify::{
    EventQueue, Notifier, NotifyConfig, NotifyEvent, NotifySettingsView, ToastSink, hub_link_thread,
};
use crate::onboarding::{self, OnboardingEvent, OnboardingView};
use crate::palette::{
    self, CommandId, NextThread, OpenPalette, PaletteEvent, PaletteView, PreviousThread, ShortcutId,
};
use crate::task::{
    PlanModel, TranscriptEvent, TranscriptPane, UsageModel, render_plan, render_usage,
};

actions!(shell, [GoNewTask, NextSection, PreviousSection]);

const CONTEXT: &str = "Shell";

/// How often clicks on notifications are picked up.
pub const NOTIFY_TICK: Duration = Duration::from_millis(100);

/// How often the open thread's approval cards are recomputed (time left).
pub const APPROVAL_TICK: Duration = Duration::from_secs(1);

/// Register the shell's key bindings. Call once, after `gpui_kit::init`.
pub fn init(cx: &mut App) {
    crate::compose::init(cx);
    palette::init(cx);
    // Also inside text fields, which would otherwise take some of these strokes.
    for context in [CONTEXT, "Shell > Input"] {
        cx.bind_keys([
            KeyBinding::new(
                palette::keystroke(ShortcutId::NewTask),
                GoNewTask,
                Some(context),
            ),
            KeyBinding::new(
                palette::keystroke(ShortcutId::NextSection),
                NextSection,
                Some(context),
            ),
            KeyBinding::new(
                palette::keystroke(ShortcutId::PreviousSection),
                PreviousSection,
                Some(context),
            ),
        ]);
    }
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
    palette: Entity<PaletteView>,
    _palette_events: Subscription,
    _appearance: Subscription,
    _hub_input_events: Subscription,
    _inbox_events: Subscription,
    /// Follows the open thread's pane; replaced with the pane.
    _pane_events: Subscription,
    connectivity_task: Option<Task<()>>,
    /// The signed-in session's hub link; present exactly while a session is.
    link: Option<SessionLink>,
    link_thread: String,
    clock: Arc<dyn Clock>,
    presets_path: Option<PathBuf>,
    /// The thread shown in the Inbox page, if one was opened.
    open_thread: Option<String>,
    pane: Entity<TranscriptPane>,
    /// The composer under the open thread's transcript.
    composer: Entity<ThreadComposer>,
    _composer_events: Subscription,
    /// The agents page, fed by the shell's machines feed.
    agents: Entity<crate::agents::AgentsView>,
    /// First-run page; shown in place of the inbox while there is no machine (see
    /// [`AppShell::onboarding_visible`]).
    onboarding: Entity<OnboardingView>,
    _onboarding_events: [Subscription; 3],
    onboarding_skipped: bool,
    /// The empty agents table or composer asked for the steps.
    onboarding_requested: bool,
    /// A machine has been seen for this session: onboarding is done.
    onboarding_completed: bool,
    /// Put the cursor in the thread composer on the next render (the empty transcript's action).
    focus_composer: bool,
    /// The one `GET /v1/machines` feed: agents page, new-task composer and runner cap.
    machines: MachinesFeed,
    machines_pump: Option<Task<()>>,
    _machines_timer: Option<Task<()>>,
    /// The thread the composer's prompt text belongs to; the text is cleared when it changes.
    composer_thread: Option<String>,
    /// The machine hosting the open thread, from its last snapshot.
    thread_machine: Option<String>,
    /// The open thread's plan and usage widgets, as of its last snapshot.
    plan: PlanModel,
    usage: UsageModel,
    /// The last "open file" request from the open thread's diff: absolute path and 1-based line.
    last_open_file: Option<(PathBuf, u32)>,
    snapshot_task: Option<Task<()>>,
    tick_task: Option<Task<()>>,
    link_retry: Duration,
    cache_path: Option<PathBuf>,
    /// What the last Save of the hub field did, shown under the field.
    hub_feedback: Option<HubOutcome>,
    /// Why the shell is on the login screen without the user having signed out.
    notice: Option<String>,
    /// Decides which inbox changes notify; shows them through `toasts`.
    notifier: Notifier,
    toasts: ToastSink,
    /// Clicks on notifications, waiting for [`AppShell::pump_notifications`].
    notify_events: EventQueue,
    notify_settings: Entity<NotifySettingsView>,
    /// Whether the window is the active one; a background window notifies for every thread.
    window_active: bool,
    /// Toasts shown so far.
    toasts_shown: usize,
    /// The first inbox list is a baseline, not a burst of notifications.
    inbox_seeded: bool,
    /// [`crate::inbox::InboxView::lists_applied`] as of the last look.
    lists_seen: u64,
    _inbox_lists: Subscription,
    /// The open thread's last snapshot status as a hub status string. The snapshot can be ahead
    /// of the inbox list; see [`AppShell::on_inbox_changed`].
    snapshot_status: Option<(String, &'static str)>,
    _inbox_timer: Option<Task<()>>,
    _activation: Subscription,
    _notify_task: Option<Task<()>>,
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
        let notify_dir = draft_dir.clone();
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
        let inbox_events = cx.subscribe_in(
            &inbox,
            window,
            |this, _, event: &InboxEvent, window, cx| match event {
                InboxEvent::OpenThread {
                    thread_id,
                    workspace,
                } => this.open_thread(thread_id, workspace.as_deref(), cx),
                InboxEvent::NewTask => this.new_task(window, cx),
            },
        );
        let inbox_lists = cx.observe_in(&inbox, window, |this, _, window, cx| {
            this.on_inbox_changed(window, cx);
        });
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            let active = window.is_window_active();
            this.activate(active, window, cx);
        });
        let notify_events = EventQueue::default();
        let toasts = ToastSink::new(notify_events.clone());
        let notify_prefs_path = notify_dir
            .unwrap_or_else(std::env::temp_dir)
            .join("notifications.json");
        let notifier = Notifier::new(
            &NotifyConfig {
                prefs_path: notify_prefs_path,
            },
            Box::new(toasts.clone()),
        );
        let notify_settings = cx.new(|_| NotifySettingsView::new(notifier.prefs().clone()));
        let palette = cx.new(|cx| PaletteView::new(window, cx));
        let palette_events = cx.subscribe_in(&palette, window, Self::on_palette_event);
        let pane = cx.new(|_| TranscriptPane::new());
        let pane_events = Self::subscribe_pane(&pane, cx);
        let composer = cx.new(|cx| ThreadComposer::new(window, cx));
        let composer_events = cx.subscribe(&composer, Self::on_composer_event);
        let agents = cx.new(|cx| {
            crate::agents::AgentsView::new(
                &[],
                crate::agents::LaunchConfig {
                    model_key_configured: false,
                },
                Arc::new(crate::agents::UnavailableManagedClient),
                window,
                cx,
            )
        });
        let onboarding = cx.new(|_| OnboardingView::new(model.hub()));
        let onboarding_events = [
            cx.subscribe(&onboarding, |this, _, event: &OnboardingEvent, cx| {
                let OnboardingEvent::Skip = event;
                this.onboarding_skipped = true;
                this.onboarding_requested = false;
                cx.notify();
            }),
            cx.subscribe(&agents, |this, _, _: &crate::agents::AgentsEvent, cx| {
                this.show_setup(cx);
            }),
            cx.subscribe(&compose, |this, _, _: &crate::compose::ComposeEvent, cx| {
                this.show_setup(cx);
            }),
        ];
        let mut shell = Self {
            model,
            driver,
            focus,
            hub_input,
            compose,
            inbox,
            palette,
            _palette_events: palette_events,
            _appearance: appearance,
            _hub_input_events: hub_input_events,
            _inbox_events: inbox_events,
            _pane_events: pane_events,
            connectivity_task: None,
            link: None,
            link_thread: config.link_thread,
            clock: config.clock,
            presets_path,
            open_thread: None,
            pane,
            composer,
            _composer_events: composer_events,
            agents,
            onboarding,
            _onboarding_events: onboarding_events,
            onboarding_skipped: false,
            onboarding_requested: false,
            onboarding_completed: false,
            focus_composer: false,
            machines: MachinesFeed::default(),
            machines_pump: None,
            _machines_timer: None,
            composer_thread: None,
            thread_machine: None,
            plan: PlanModel::default(),
            usage: UsageModel::default(),
            last_open_file: None,
            snapshot_task: None,
            tick_task: None,
            link_retry,
            cache_path,
            hub_feedback: None,
            notice: None,
            notifier,
            toasts,
            notify_events,
            notify_settings,
            window_active: true,
            toasts_shown: 0,
            inbox_seeded: false,
            lists_seen: 0,
            _inbox_lists: inbox_lists,
            snapshot_status: None,
            _inbox_timer: None,
            _activation: activation,
            _notify_task: None,
        };
        shell.start_notify_task(window, cx);
        shell.start_machines_timer(cx);
        shell.start_inbox_timer(window, cx);
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
                self.snapshot_status = None;
                self.inbox_seeded = false;
                self.sync_notify_focus();
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

    /// The inbox list.
    pub fn inbox(&self) -> &Entity<crate::inbox::InboxView> {
        &self.inbox
    }

    /// The agents page.
    pub fn agents(&self) -> &Entity<crate::agents::AgentsView> {
        &self.agents
    }

    /// The machines the shell's feed last received (empty before the first answer).
    pub fn machines(&self) -> &[crate::compose::MachineOption] {
        &self.machines.snapshot.options
    }

    /// Fetch `GET /v1/machines` now (a fetch already running is not doubled). Called when the
    /// session appears, when the Agents page opens and by the timer. Does nothing signed out.
    pub fn refresh_machines(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.machines.target.clone() else {
            return;
        };
        if self.machines.rx.is_some() {
            return;
        }
        if !self.machines.loaded {
            self.compose
                .update(cx, |compose, cx| compose.machines_loading(cx));
        }
        self.machines.rx = Some(machines::fetch_off_thread(target));
        self.machines_pump = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let Ok(more) = this.update(cx, |this, cx| this.pump_machines(cx)) else {
                    break;
                };
                if !more {
                    break;
                }
            }
        }));
    }

    /// Move a finished fetch into the pages. Returns whether one is still running.
    fn pump_machines(&mut self, cx: &mut Context<Self>) -> bool {
        use std::sync::mpsc::TryRecvError;
        let Some(receiver) = &self.machines.rx else {
            return false;
        };
        let answer = match receiver.try_recv() {
            Ok(answer) => answer,
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => Err("the request did not finish".to_owned()),
        };
        self.machines.rx = None;
        self.machines.loaded = true;
        match answer {
            Ok(snapshot) => {
                self.machines.error = None;
                self.machines.snapshot = snapshot;
                if !self.machines.snapshot.options.is_empty() {
                    // The first machine ends onboarding; the Inbox page shows from now on.
                    self.onboarding_completed = true;
                    self.onboarding_requested = false;
                }
                let info = self.machines.snapshot.info.clone();
                let options = self.machines.snapshot.options.clone();
                self.agents
                    .update(cx, |agents, cx| agents.set_machines(&info, cx));
                self.compose
                    .update(cx, |compose, cx| compose.apply_machines(options, cx));
                self.sync_runner_cap(cx);
            }
            Err(detail) => {
                self.machines.error = Some(detail.clone());
                self.compose
                    .update(cx, |compose, cx| compose.machines_failed(detail, cx));
            }
        }
        cx.notify();
        false
    }

    /// Refresh the machines list every [`machines::REFRESH_EVERY`].
    fn start_machines_timer(&mut self, cx: &mut Context<Self>) {
        self._machines_timer = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(machines::REFRESH_EVERY)
                    .await;
                if this
                    .update(cx, |this, cx| this.refresh_machines(cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    /// Follow the session: a new hub or account starts a fresh feed and fetches at once.
    fn sync_machines_target(
        &mut self,
        target: Option<crate::compose::Target>,
        cx: &mut Context<Self>,
    ) {
        if self.machines.target == target {
            return;
        }
        self.machines_pump = None;
        self.onboarding_skipped = false;
        self.onboarding_requested = false;
        self.onboarding_completed = false;
        self.machines.reset(target.clone());
        let (info, options) = (Vec::new(), Vec::new());
        self.agents
            .update(cx, |agents, cx| agents.set_machines(&info, cx));
        self.compose.update(cx, |compose, cx| {
            compose.set_send_target(target.clone(), cx);
            compose.apply_machines(options, cx);
        });
        self.sync_runner_cap(cx);
        if target.is_some() {
            self.refresh_machines(cx);
        }
    }

    /// Whether the first-run page is showing in place of the inbox: signed in, the machines feed
    /// answered with none and the inbox has nothing to show, until the user skips or a machine
    /// appears. The empty agents table and composer bring it back with [`AppShell::show_setup`].
    pub fn onboarding_visible(&self, cx: &App) -> bool {
        if self.model.screen() != Screen::Main {
            return false;
        }
        let inbox = self.inbox.read(cx);
        onboarding::needed(onboarding::FeedState {
            loaded: self.machines.loaded,
            failed: self.machines.error.is_some(),
            machines: self.machines.snapshot.options.len(),
            inbox_empty: inbox.lists_applied() > 0 && inbox.model().rows().is_empty(),
            completed: self.onboarding_completed,
            skipped: self.onboarding_skipped,
            requested: self.onboarding_requested,
        })
    }

    /// Show the steps to connect a machine (the action of the empty agents table and composer).
    pub fn show_setup(&mut self, cx: &mut Context<Self>) {
        if self.model.screen() != Screen::Main || self.onboarding_completed {
            return;
        }
        self.onboarding_requested = true;
        self.close_thread(cx);
        self.model.nav.select(Section::Inbox);
        cx.notify();
    }

    /// The composer under the open thread's transcript.
    pub fn composer(&self) -> &Entity<ThreadComposer> {
        &self.composer
    }

    /// The first-run page.
    pub fn onboarding(&self) -> &Entity<OnboardingView> {
        &self.onboarding
    }

    /// The new-task composer.
    pub fn compose(&self) -> &Entity<crate::compose::ComposeView> {
        &self.compose
    }

    /// The command palette overlay.
    pub fn palette(&self) -> &Entity<PaletteView> {
        &self.palette
    }

    /// Open the palette over the pages and the inbox's threads; while it is open, close it.
    pub fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.screen() != Screen::Main {
            return;
        }
        if self.palette.read(cx).is_open() {
            self.palette
                .update(cx, |palette, cx| palette.close(window, cx));
            return;
        }
        let commands = palette::build_commands(&palette::thread_order(self.inbox.read(cx).model()));
        self.palette
            .update(cx, |palette, cx| palette.open(commands, window, cx));
        cx.notify();
    }

    fn on_palette_event(
        &mut self,
        _: &Entity<PaletteView>,
        event: &PaletteEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            PaletteEvent::Closed => cx.notify(),
            PaletteEvent::Run(command) => self.run_command(command, window, cx),
        }
    }

    /// Do what a palette command says.
    pub fn run_command(
        &mut self,
        command: &CommandId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            CommandId::Go(section) => {
                window.focus(&self.focus, cx);
                self.select(*section, cx);
            }
            CommandId::NewTask => self.new_task(window, cx),
            CommandId::OpenThread(thread_id) => self.open_inbox_thread(thread_id, cx),
            CommandId::NextThread => self.step_thread(1, cx),
            CommandId::PreviousThread => self.step_thread(-1, cx),
        }
        cx.notify();
    }

    /// Show the composer and put the cursor in its prompt, so Cmd/Ctrl+Enter sends.
    pub fn new_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.screen() != Screen::Main {
            return;
        }
        self.select(Section::NewTask, cx);
        let prompt = self.compose.read(cx).prompt_input().focus_handle(cx);
        window.focus(&prompt, cx);
    }

    /// Open a thread of the inbox's list by id (its workspace comes from the row).
    fn open_inbox_thread(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        let workspace = self
            .inbox
            .read(cx)
            .model()
            .row(thread_id)
            .and_then(|row| row.workspace.clone());
        self.open_thread(thread_id, workspace.as_deref(), cx);
    }

    /// Open the thread `delta` places from the open one in the inbox's order.
    fn step_thread(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.model.screen() != Screen::Main {
            return;
        }
        let threads = palette::thread_order(self.inbox.read(cx).model());
        let ids: Vec<&str> = threads
            .iter()
            .map(|thread| thread.thread_id.as_str())
            .collect();
        if let Some(ix) = palette::adjacent(&ids, self.open_thread.as_deref(), delta) {
            let thread = &threads[ix];
            self.open_thread(&thread.thread_id, thread.workspace.as_deref(), cx);
        }
    }

    // ---- notifications and links --------------------------------------------------------------

    /// The thread that must not notify: the open one while the window is the active one.
    fn sync_notify_focus(&mut self) {
        let focused = if self.window_active {
            self.open_thread.as_deref()
        } else {
            None
        };
        self.notifier.set_focused(focused);
    }

    /// Tell the shell the window went to the foreground or the background. A background window
    /// notifies for the open thread too.
    pub fn set_window_active(&mut self, active: bool, _cx: &mut Context<Self>) {
        self.window_active = active;
        self.sync_notify_focus();
    }

    /// The window went to the front or the back. Coming to the front also refetches the inbox
    /// (a fetch already running is not doubled), so what changed while away shows at once.
    pub fn activate(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.set_window_active(active, cx);
        if active {
            self.inbox
                .update(cx, |inbox, cx| inbox.refresh_if_idle(window, cx));
        }
    }

    /// Refetch the inbox every [`crate::inbox::REFRESH_EVERY`] while signed in.
    fn start_inbox_timer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._inbox_timer = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(crate::inbox::REFRESH_EVERY)
                    .await;
                let result = this.update_in(cx, |this, window, cx| {
                    this.inbox
                        .update(cx, |inbox, cx| inbox.refresh_if_idle(window, cx));
                });
                if result.is_err() {
                    break;
                }
            }
        }));
    }

    /// The open thread's snapshot status goes to the notifier too, so a run that ends while its
    /// thread is open (and the window is in the background) notifies without waiting for a list,
    /// and the list that reports the same status later does not notify again (the notifier
    /// de-duplicates by thread and status). A thread the notifier has not seen yet is only
    /// recorded: its first status is not a change.
    fn observe_snapshot_status(&mut self, thread_id: &str, status: TaskStatus) {
        if !self.inbox_seeded {
            return;
        }
        let status = match status {
            TaskStatus::Starting | TaskStatus::Running => "running",
            TaskStatus::NeedsYou => "needs_you",
            TaskStatus::Succeeded => "done",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
        };
        if self.notifier.knows(thread_id) {
            self.notifier.observe(thread_id, status);
        } else {
            self.notifier.seed([(thread_id, status)]);
        }
        self.snapshot_status = Some((thread_id.to_owned(), status));
    }

    /// The notifier's prefs, shared with the notification settings.
    pub fn notify_settings(&self) -> &Entity<NotifySettingsView> {
        &self.notify_settings
    }

    /// Clicks on notifications queue here; [`AppShell::pump_notifications`] acts on them.
    pub fn notify_events(&self) -> EventQueue {
        self.notify_events.clone()
    }

    /// How many toasts the shell has shown.
    pub fn toasts_shown(&self) -> usize {
        self.toasts_shown
    }

    /// Show the queued toasts and act on the clicked ones (`Open` opens the thread).
    pub fn pump_notifications(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toasts_shown += self.toasts.flush(window, cx);
        for event in self.notify_events.drain() {
            match event {
                NotifyEvent::Open { thread_id } => self.open_inbox_thread(&thread_id, cx),
            }
        }
    }

    fn start_notify_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._notify_task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(NOTIFY_TICK).await;
                if this
                    .update_in(cx, |this, window, cx| this.pump_notifications(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    /// A fetched inbox list is in: the first one is the baseline, later ones notify for threads
    /// that moved to `needs_you`, `done` or `failed`. A filtered or searched list is not looked at:
    /// it holds threads the user has not seen change.
    fn on_inbox_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let inbox = self.inbox.read(cx);
        let applied = inbox.lists_applied();
        if applied == self.lists_seen {
            return;
        }
        self.lists_seen = applied;
        let model = inbox.model();
        if *model.filters() != Filters::default() || !model.search_text().trim().is_empty() {
            return;
        }
        // The open thread's snapshot can be ahead of the list (a failed run the list still shows
        // as running). That stale row is left out, so it cannot undo what the notifier knows and
        // make the list that catches up notify a second time. The guard lasts until a list agrees.
        let ahead = self.snapshot_status.clone();
        let is_stale = |thread_id: &str, status: &str| {
            matches!(&ahead, Some((id, ahead_status))
                if id == thread_id
                    && status == "running"
                    && matches!(*ahead_status, "done" | "failed" | "cancelled"))
        };
        let stale = model
            .rows()
            .iter()
            .any(|row| is_stale(&row.thread_id, row.status.as_query()));
        let rows: Vec<(String, &'static str)> = model
            .rows()
            .iter()
            .filter(|row| !is_stale(&row.thread_id, row.status.as_query()))
            .map(|row| (row.thread_id.clone(), row.status.as_query()))
            .collect();
        if !stale {
            self.snapshot_status = None;
        }
        let threads = rows.iter().map(|(id, status)| (id.as_str(), *status));
        if self.inbox_seeded {
            self.notifier.observe_all(threads);
        } else {
            self.notifier.seed(threads);
            self.inbox_seeded = true;
        }
        self.pump_notifications(window, cx);
    }

    /// Open a hub link (`https://<hub>/t/<thread_id>`) in the app. Returns whether it opened a
    /// thread; any other URL is ignored.
    pub fn open_url(&mut self, url: &str, cx: &mut Context<Self>) -> bool {
        let hub = self
            .model
            .session()
            .map(|session| session.hub.clone())
            .unwrap_or_else(|| self.model.hub().to_string());
        let Some(thread_id) = hub_link_thread(url, &hub) else {
            return false;
        };
        if self.link.is_none() {
            return false;
        }
        self.open_inbox_thread(&thread_id, cx);
        true
    }

    /// The thread the Inbox page shows, if one was opened.
    pub fn open_thread_id(&self) -> Option<&str> {
        self.open_thread.as_deref()
    }

    /// The composer under the open thread's transcript.
    pub fn thread_composer(&self) -> &Entity<ThreadComposer> {
        &self.composer
    }

    /// The pane that shows the open thread and its approval cards.
    pub fn thread_pane(&self) -> &Entity<TranscriptPane> {
        &self.pane
    }

    /// The last "open file" request the open thread's diff made: the absolute path (inside the
    /// thread's workspace) and the 1-based line. There is no editor yet, so nothing else acts on
    /// it.
    pub fn last_open_file(&self) -> Option<(&Path, u32)> {
        self.last_open_file
            .as_ref()
            .map(|(path, line)| (path.as_path(), *line))
    }

    fn subscribe_pane(pane: &Entity<TranscriptPane>, cx: &mut Context<Self>) -> Subscription {
        cx.subscribe(pane, |this, _, event: &TranscriptEvent, cx| match event {
            TranscriptEvent::OpenFile { path, line } => {
                this.last_open_file = Some((path.clone(), *line));
                cx.notify();
            }
            TranscriptEvent::WriteMessage => {
                this.focus_composer = true;
                cx.notify();
            }
        })
    }

    /// Shows `thread_id` on the Inbox page and makes it the thread the session link follows
    /// (the inbox calls this when a row is opened). `workspace` is the thread's workspace, for
    /// `allow_always` presets and the root that "Open file" resolves paths against; `None` or an
    /// empty string means no workspace, so the diff offers no "Open file". Does nothing while
    /// signed out.
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
            self.snapshot_status = None;
            self.pane = cx.new(|_| {
                let mut pane = TranscriptPane::new();
                pane.set_follow_tail(true);
                pane
            });
            self._pane_events = Self::subscribe_pane(&self.pane, cx);
            self.plan = PlanModel::default();
            self.usage = UsageModel::default();
            self.thread_machine = None;
            self.composer.update(cx, |composer, cx| {
                composer.clear_thread(cx);
                composer.set_runner_mode(None, cx);
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
        // Also for a thread that is already open: its workspace may have been set since.
        let root = workspace
            .filter(|workspace| !workspace.is_empty())
            .map(PathBuf::from);
        self.pane
            .update(cx, |pane, cx| pane.set_workspace_root(root, cx));
        self.model.nav.select(Section::Inbox);
        self.sync_notify_focus();
        cx.notify();
    }

    /// Back to the thread list. The link keeps following the thread it has.
    pub fn close_thread(&mut self, cx: &mut Context<Self>) {
        if self.open_thread.take().is_some() {
            self.snapshot_status = None;
            if let Some(link) = &self.link {
                link.forget_refusals();
            }
            self.sync_notify_focus();
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
        if let Some(task) = snapshot.view.tasks().last() {
            self.observe_snapshot_status(&snapshot.thread_id, task.status());
        }
        self.plan = snapshot.plan;
        self.usage = snapshot.usage;
        self.thread_machine = snapshot.view.machine_id().map(str::to_owned);
        self.composer.update(cx, |composer, cx| {
            composer.set_thread(&snapshot.view, cx);
            composer.set_started_removals(snapshot.started_removals.clone(), cx);
            composer.set_refusals(snapshot.refusals.clone(), cx);
        });
        self.sync_runner_cap(cx);
        self.pane.update(cx, |pane, cx| {
            pane.set_thread(&snapshot.view, cx);
            pane.set_approvals(snapshot.cards, Some(handler), cx);
        });
        cx.notify();
    }

    /// The runner's permission mode for the open thread: from the `features`
    /// (`permission_mode:<mode>`) of the machine that hosts it, as the new-task composer's
    /// shell's machines feed knows them. Unset while the machine or its list is not known.
    fn sync_runner_cap(&mut self, cx: &mut Context<Self>) {
        let cap = self.thread_machine.as_deref().and_then(|machine_id| {
            self.machines
                .snapshot
                .options
                .iter()
                .find(|machine| machine.machine_id == machine_id)
                .and_then(|machine| machine.permission_mode.clone())
        });
        self.composer
            .update(cx, |composer, cx| composer.set_runner_mode(cap, cx));
    }

    /// The composer asked for something: it all goes through the link's outbox.
    fn on_composer_event(
        &mut self,
        _: Entity<ThreadComposer>,
        event: &ComposerEvent,
        _: &mut Context<Self>,
    ) {
        let Some(link) = &self.link else { return };
        match event {
            ComposerEvent::Send { thread_id, text } => link.enqueue(Intent::Prompt {
                thread_id: thread_id.clone(),
                task_id: format!("tk_{:032x}", rand::random::<u128>()),
                message_id: format!("msg_{:032x}", rand::random::<u128>()),
                parts: vec![ContentPart::Text { text: text.clone() }],
                machine_id: None,
                author: None,
            }),
            ComposerEvent::Intent(intent) => link.enqueue(intent.clone()),
            ComposerEvent::Cancel(intent) => {
                // Stopping answers a refused removal: forget the conflict so Stop goes away
                // once the task finishes.
                if let Intent::Cancel { task_id, .. } = intent {
                    let task_id = task_id.clone();
                    drop(link.with_thread(move |access| {
                        for removal in access.outbox.started_removals() {
                            if removal.task_id == task_id {
                                let _ = access.outbox.dismiss(access.cache, removal.local_id);
                            }
                        }
                    }));
                }
                link.enqueue(intent.clone());
            }
        }
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
        self.refresh_if_agents(cx);
        cx.notify();
    }

    /// Opening the Agents page refreshes presence.
    fn refresh_if_agents(&mut self, cx: &mut Context<Self>) {
        if self.model.nav.current() == Section::Agents {
            self.refresh_machines(cx);
        }
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
        } else if current == Section::Agents {
            self.render_agents(cx).into_any_element()
        } else if current == Section::Inbox && self.open_thread.is_some() {
            self.render_thread(slots, cx).into_any_element()
        } else if current == Section::Inbox && self.onboarding_visible(cx) {
            let hub = self.model.hub().to_owned();
            self.onboarding
                .update(cx, |onboarding, cx| onboarding.set_hub(&hub, cx));
            self.onboarding.clone().into_any_element()
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

    /// The agents table and launch form, with a note when the machines fetch failed.
    fn render_agents(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let error = self
            .machines
            .error
            .as_ref()
            .map(|detail| tr_args("content-machines-error", &[("detail", detail)]));
        v_flex()
            .id("agents-page")
            .size_full()
            .gap_2()
            .when_some(error, |el, text| {
                el.child(Self::status("agents-feed-error", text, cx))
            })
            .child(self.agents.clone())
    }

    /// The open thread: a way back to the list, then the transcript with its approval cards, and
    /// beside it the plan and usage widgets. Both widgets show their empty states when the thread
    /// has no plan or usage yet.
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
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .gap_2()
                            .child(div().flex_1().min_h_0().child(self.pane.clone()))
                            .child(self.composer.clone()),
                    )
                    .child(
                        v_flex()
                            .id("thread-side")
                            .w(px(260.))
                            .flex_none()
                            .gap_2()
                            .child(render_plan(&self.plan))
                            .child(render_usage(&self.usage)),
                    ),
            )
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
            .child(self.notify_settings.clone())
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
        if self.composer_thread != self.open_thread {
            self.composer_thread = self.open_thread.clone();
            self.composer
                .update(cx, |composer, cx| composer.clear_prompt(window, cx));
        }
        if std::mem::take(&mut self.focus_composer) {
            let prompt = self.composer.read(cx).prompt_input().focus_handle(cx);
            window.focus(&prompt, cx);
        }
        let target = self.model.session().map(|session| crate::compose::Target {
            api: format!("{}/v1", session.hub.trim_end_matches('/')),
            token: Some(session.grant.token.clone()),
        });
        self.sync_machines_target(target, cx);
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
        let palette_open = self.model.screen() == Screen::Main && self.palette.read(cx).is_open();
        div()
            .id("app-shell")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .on_action(cx.listener(|this, _: &GoNewTask, window, cx| this.new_task(window, cx)))
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| {
                this.toggle_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NextThread, _, cx| this.step_thread(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousThread, _, cx| this.step_thread(-1, cx)))
            .on_action(cx.listener(|this, _: &NextSection, window, cx| {
                if this.model.screen() == Screen::Main {
                    // The page may take the focused field with it; keep the shortcuts live.
                    window.focus(&this.focus, cx);
                    this.model.nav.next();
                    this.refresh_if_agents(cx);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &PreviousSection, window, cx| {
                if this.model.screen() == Screen::Main {
                    // The page may take the focused field with it; keep the shortcuts live.
                    window.focus(&this.focus, cx);
                    this.model.nav.previous();
                    this.refresh_if_agents(cx);
                    cx.notify();
                }
            }))
            .child(body)
            .when(palette_open, |el| el.child(self.palette.clone()))
    }
}
