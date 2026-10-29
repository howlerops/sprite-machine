//! [`InboxView`]: a thin, virtualized view over [`InboxModel`]. Background requests run on their
//! own thread and runtime (as in `compose`) and are collected in [`InboxView::pump`].

use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use gpui_kit::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Selectable as _, VirtualListScrollHandle,
    scroll::{ScrollableElement, ScrollbarAxis},
    v_flex, v_virtual_list, *,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::api::{answer_interrupt, fetch_threads, patch_thread};
use super::model::{
    Clock, Failure, InboxModel, Item, Section, Status, SystemClock, ThreadRow, Verdict,
};
use super::{tr, tr_args};
use crate::compose::AGENTS;

/// Height of a section header and of a thread row.
pub const HEADER_HEIGHT: Pixels = px(30.);
pub const ROW_HEIGHT: Pixels = px(48.);
/// Element id of the list (scroll target in tests) and of each laid-out row.
pub const LIST_ID: &str = "inbox-list";
pub const ROW_ID: &str = "inbox-row";

/// Where and as whom requests go: the `…/v1` base URL and the bearer token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub api: String,
    pub token: Option<String>,
}

/// What the inbox tells the shell. A click on a row's title opens that thread; the shell shows
/// it and follows it. Quick Allow / Deny and the row's other buttons emit nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboxEvent {
    OpenThread {
        thread_id: String,
        workspace: Option<String>,
    },
    /// The empty list's primary action: start a task.
    NewTask,
}

/// Element ids of the empty state's two actions: one shows at a time.
pub const EMPTY_NEW_TASK_ID: &str = "inbox-empty-action-new";
pub const EMPTY_CLEAR_FILTERS_ID: &str = "inbox-empty-action-clear";

impl EventEmitter<InboxEvent> for InboxView {}

fn run_off_thread<T: Send + 'static>(
    future: impl std::future::Future<Output = T> + Send + 'static,
) -> Receiver<T> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            let _ = sender.send(runtime.block_on(future));
        }
    });
    receiver
}

type OpAnswer = Result<(), Failure>;
type ListAnswer = Result<Vec<ThreadRow>, Failure>;

/// How often the shell refetches the inbox list on its own, so notifications fire without the
/// user doing anything. It also refetches when the window comes to the front.
pub const REFRESH_EVERY: Duration = Duration::from_secs(20);

pub struct InboxView {
    model: InboxModel,
    target: Option<Target>,
    loading: bool,
    /// How many list answers the model has taken; the shell compares it to see a new fetch.
    lists_applied: u64,
    list_rx: Option<(u64, Receiver<ListAnswer>)>,
    op_rx: Vec<(u64, Receiver<OpAnswer>)>,
    renaming: Option<String>,
    sizes: Rc<Vec<gpui_kit::Size<Pixels>>>,
    scroll: VirtualListScrollHandle,
    search: Entity<InputState>,
    workspace: Entity<InputState>,
    rename: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl InboxView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::with_clock(Arc::new(SystemClock::new()), window, cx)
    }

    /// The same with an injected clock (tests step the search debounce by hand).
    pub fn with_clock(clock: Arc<dyn Clock>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("inbox-search-placeholder")));
        let workspace = cx.new(|cx| {
            InputState::new(window, cx).placeholder(tr("inbox-filter-workspace-placeholder"))
        });
        let rename = cx.new(|cx| InputState::new(window, cx));
        let subscriptions = vec![
            cx.subscribe_in(&search, window, |this, state, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    let text = state.read(cx).value();
                    this.model.set_search(&text);
                    this.watch(window, cx);
                    cx.notify();
                }
            }),
            cx.subscribe_in(&workspace, window, |this, state, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let text = state.read(cx).value();
                    if this.model.set_workspace_filter(&text) {
                        this.refresh(window, cx);
                    }
                    cx.notify();
                }
            }),
            cx.subscribe_in(&rename, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.commit_rename(window, cx);
                }
            }),
        ];
        Self {
            model: InboxModel::new(clock),
            target: None,
            loading: false,
            lists_applied: 0,
            list_rx: None,
            op_rx: Vec::new(),
            renaming: None,
            sizes: Rc::new(Vec::new()),
            scroll: VirtualListScrollHandle::new(),
            search,
            workspace,
            rename,
            _subscriptions: subscriptions,
        }
    }

    pub fn model(&self) -> &InboxModel {
        &self.model
    }

    /// How many fetched lists the model has taken so far. It grows by one for every list that
    /// replaced the rows (a late answer to an older request does not count), so an observer can
    /// tell a fresh fetch from any other repaint.
    pub fn lists_applied(&self) -> u64 {
        self.lists_applied
    }

    pub fn search_input(&self) -> &Entity<InputState> {
        &self.search
    }

    pub fn rename_input(&self) -> &Entity<InputState> {
        &self.rename
    }

    /// The thread being renamed, if any.
    pub fn renaming(&self) -> Option<&str> {
        self.renaming.as_deref()
    }

    /// Point the inbox at a hub (or at nothing, when signed out). A new target loads the list;
    /// the same target is a no-op.
    pub fn set_target(
        &mut self,
        target: Option<Target>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.target == target {
            return;
        }
        self.target = target;
        if self.target.is_some() {
            self.refresh(window, cx);
        }
        cx.notify();
    }

    /// Fetch the list for the current search and filters.
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let request = self.model.list_request();
        let generation = request.generation;
        self.loading = true;
        self.list_rx = Some((
            generation,
            run_off_thread(async move {
                fetch_threads(&target.api, target.token.as_deref(), &request).await
            }),
        ));
        self.watch(window, cx);
        cx.notify();
    }

    /// The background refresh (timer tick, window coming to the front): fetch the list again
    /// unless that would be wrong or wasteful. Returns whether a fetch started.
    ///
    /// - No target (signed out): nothing to fetch.
    /// - A fetch already in flight: not doubled, the running one answers.
    /// - A filtered or searched list (or a search about to fire): skipped. The notifier only
    ///   trusts the unfiltered list, and replacing the user's filtered view behind their back is
    ///   worse than waiting; the next unfiltered list catches up.
    pub fn refresh_if_idle(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.target.is_none()
            || self.list_rx.is_some()
            || *self.model.filters() != super::model::Filters::default()
            || !self.model.search_text().trim().is_empty()
            || self.model.search_pending()
        {
            return false;
        }
        self.refresh(window, cx);
        true
    }

    fn watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let keep_going = this
                    .update_in(cx, |this, window, cx| this.pump(window, cx))
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    /// Move finished answers into the model and fire a search whose debounce has passed. Returns
    /// whether anything is still pending.
    pub fn pump(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some((generation, receiver)) = &self.list_rx {
            let generation = *generation;
            let answer = match receiver.try_recv() {
                Ok(answer) => Some(answer),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(Failure {
                    status: None,
                    message: "the request did not finish".to_owned(),
                })),
            };
            if let Some(answer) = answer {
                self.list_rx = None;
                self.loading = false;
                match answer {
                    Ok(rows) => {
                        if self.model.apply_list(generation, rows) {
                            self.lists_applied += 1;
                        }
                    }
                    Err(failure) => {
                        if generation == self.model_generation() {
                            self.model.report(failure);
                        }
                    }
                }
                self.rebuild();
                cx.notify();
            }
        }
        let mut finished = Vec::new();
        self.op_rx
            .retain(|(op, receiver)| match receiver.try_recv() {
                Ok(answer) => {
                    finished.push((*op, answer));
                    false
                }
                Err(TryRecvError::Empty) => true,
                Err(TryRecvError::Disconnected) => {
                    finished.push((
                        *op,
                        Err(Failure {
                            status: None,
                            message: "the request did not finish".to_owned(),
                        }),
                    ));
                    false
                }
            });
        if !finished.is_empty() {
            for (op, answer) in finished {
                self.model.finish(op, answer);
            }
            self.rebuild();
            // The hub is the truth: look again after every write, accepted or not.
            self.refresh(window, cx);
        }
        if self.model.poll_search() {
            self.refresh(window, cx);
        }
        cx.notify();
        self.list_rx.is_some() || !self.op_rx.is_empty() || self.model.search_pending()
    }

    fn model_generation(&self) -> u64 {
        self.model.generation()
    }

    fn rebuild(&mut self) {
        let sizes = self
            .model
            .items()
            .into_iter()
            .map(|item| {
                size(
                    px(100.),
                    match item {
                        Item::Header { .. } => HEADER_HEIGHT,
                        Item::Row(_) => ROW_HEIGHT,
                    },
                )
            })
            .collect();
        self.sizes = Rc::new(sizes);
    }

    // ---- user intents ---------------------------------------------------------------------

    /// The empty state's action for a list that is empty because of filters or a search: drop
    /// them all and fetch again.
    pub fn clear_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model.set_status_filter(None);
        self.model.set_agent_filter(None);
        self.model.set_workspace_filter("");
        for input in [self.workspace.clone(), self.search.clone()] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.model.set_search("");
        self.refresh(window, cx);
        cx.notify();
    }

    /// The empty state's action for a list that is empty because there are no tasks.
    pub fn request_new_task(&mut self, cx: &mut Context<Self>) {
        cx.emit(InboxEvent::NewTask);
    }

    pub fn set_status_filter(
        &mut self,
        status: Option<Status>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.model.set_status_filter(status) {
            self.refresh(window, cx);
        }
    }

    pub fn set_agent_filter(
        &mut self,
        agent: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.model.set_agent_filter(agent) {
            self.refresh(window, cx);
        }
    }

    pub fn set_show_archived(&mut self, show: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.set_show_archived(show) {
            self.refresh(window, cx);
        }
    }

    pub fn archive(
        &mut self,
        thread_id: &str,
        archived: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(target), Some(request)) = (
            self.target.clone(),
            self.model.begin_archive(thread_id, archived),
        ) else {
            return;
        };
        self.op_rx.push((
            request.op,
            run_off_thread(async move {
                patch_thread(&target.api, target.token.as_deref(), &request).await
            }),
        ));
        self.rebuild();
        self.watch(window, cx);
        cx.notify();
    }

    /// Open the rename field for a thread, filled with its title.
    pub fn start_rename(&mut self, thread_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(title) = self.model.row(thread_id).map(|row| row.title.clone()) else {
            return;
        };
        self.renaming = Some(thread_id.to_owned());
        self.rename
            .update(cx, |state, cx| state.set_value(title, window, cx));
        cx.notify();
    }

    pub fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.renaming = None;
        cx.notify();
    }

    /// Send the title in the rename field.
    pub fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread_id) = self.renaming.take() else {
            return;
        };
        let title = self.rename.read(cx).value().to_string();
        if let (Some(target), Some(request)) = (
            self.target.clone(),
            self.model.begin_rename(&thread_id, &title),
        ) {
            self.op_rx.push((
                request.op,
                run_off_thread(async move {
                    patch_thread(&target.api, target.token.as_deref(), &request).await
                }),
            ));
            self.rebuild();
            self.watch(window, cx);
        }
        cx.notify();
    }

    /// Open a thread from its row: emits [`InboxEvent::OpenThread`] with the row's thread and
    /// workspace. Unknown threads emit nothing.
    pub fn open_thread(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        let Some(row) = self.model.row(thread_id) else {
            return;
        };
        let event = InboxEvent::OpenThread {
            thread_id: row.thread_id.clone(),
            workspace: row.workspace.clone(),
        };
        cx.emit(event);
    }

    /// Quick Allow / Deny on the thread's first open approval (forms and auth are skipped).
    pub fn answer(
        &mut self,
        thread_id: &str,
        verdict: Verdict,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(target), Some(request)) = (
            self.target.clone(),
            self.model.begin_answer(thread_id, verdict),
        ) else {
            return;
        };
        self.op_rx.push((
            request.op,
            run_off_thread(async move {
                answer_interrupt(&target.api, target.token.as_deref(), &request).await
            }),
        ));
        self.rebuild();
        self.watch(window, cx);
        cx.notify();
    }

    // ---- rendering ------------------------------------------------------------------------

    /// Whether the list is empty with nothing else to say (no error, loaded, signed in), and if
    /// so whether filters or a search are the reason.
    fn empty_state(&self) -> Option<bool> {
        let settled = self.model.error().is_none()
            && self.target.is_some()
            && !self.loading
            && self.model.rows().is_empty();
        settled.then(|| {
            let f = self.model.filters();
            f.status.is_some()
                || f.agent.is_some()
                || !f.workspace.is_empty()
                || !self.model.search_text().is_empty()
        })
    }

    fn status_text(&self) -> String {
        if let Some(detail) = self.model.error() {
            tr_args("inbox-status-error", &[("detail", detail)])
        } else if self.target.is_none() {
            tr("inbox-status-no-hub")
        } else if self.loading && self.model.rows().is_empty() {
            tr("inbox-status-loading")
        } else if self.model.rows().is_empty() {
            let f = self.model.filters();
            if f.status.is_some()
                || f.agent.is_some()
                || !f.workspace.is_empty()
                || !self.model.search_text().is_empty()
            {
                tr("inbox-empty-filtered")
            } else {
                tr("inbox-empty")
            }
        } else {
            tr_args(
                "inbox-status-ready",
                &[("count", &self.model.rows().len().to_string())],
            )
        }
    }

    fn render_filters(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let filters = self.model.filters().clone();
        let statuses: [(Option<Status>, &str); 6] = [
            (None, "all"),
            (Some(Status::NeedsYou), "needs_you"),
            (Some(Status::Running), "running"),
            (Some(Status::Done), "done"),
            (Some(Status::Failed), "failed"),
            (Some(Status::Cancelled), "cancelled"),
        ];
        h_flex()
            .gap_2()
            .flex_wrap()
            .items_center()
            .child(div().child(tr("inbox-filter-status")))
            .children(statuses.map(|(status, slug)| {
                let name = tr(&format!("inbox-filter-status-{slug}"));
                let selected = filters.status == status;
                let key = if selected {
                    "inbox-filter-status-selected-a11y"
                } else {
                    "inbox-filter-status-a11y"
                };
                Button::new(SharedString::from(format!("inbox-status-{slug}")))
                    .label(name.clone())
                    .accessibility_label(tr_args(key, &[("name", &name)]))
                    .selected(selected)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_status_filter(status, window, cx);
                    }))
            }))
            .child(div().child(tr("inbox-filter-agent")))
            .child({
                let selected = filters.agent.is_none();
                let name = tr("inbox-filter-agent-all");
                Button::new("inbox-agent-any")
                    .label(name.clone())
                    .accessibility_label(tr_args(
                        if selected {
                            "inbox-filter-agent-selected-a11y"
                        } else {
                            "inbox-filter-agent-a11y"
                        },
                        &[("name", &name)],
                    ))
                    .selected(selected)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.set_agent_filter(None, window, cx);
                    }))
            })
            .children(AGENTS.map(|agent| {
                let selected = filters.agent.as_deref() == Some(agent);
                Button::new(SharedString::from(format!("inbox-agent-{agent}")))
                    .label(agent)
                    .accessibility_label(tr_args(
                        if selected {
                            "inbox-filter-agent-selected-a11y"
                        } else {
                            "inbox-filter-agent-a11y"
                        },
                        &[("name", agent)],
                    ))
                    .selected(selected)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_agent_filter(Some(agent), window, cx);
                    }))
            }))
            .child(
                div()
                    .id("inbox-workspace")
                    .test_support()
                    .aria_label(tr("inbox-filter-workspace-a11y"))
                    .w(px(220.))
                    .child(Input::new(&self.workspace)),
            )
            .child({
                let shown = filters.show_archived;
                Button::new("inbox-archived-toggle")
                    .label(tr("inbox-filter-archived"))
                    .accessibility_label(tr(if shown {
                        "inbox-filter-archived-selected-a11y"
                    } else {
                        "inbox-filter-archived-a11y"
                    }))
                    .selected(shown)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_show_archived(!shown, window, cx);
                    }))
            })
    }

    fn render_rename_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let thread_id = self.renaming.clone()?;
        let title = self
            .model
            .row(&thread_id)
            .map(|row| row.title.clone())
            .unwrap_or_default();
        Some(
            h_flex()
                .id("inbox-rename-bar")
                .gap_2()
                .items_center()
                .child(
                    div()
                        .id("inbox-rename-input")
                        .test_support()
                        .aria_label(tr_args("inbox-rename-field-a11y", &[("title", &title)]))
                        .flex_1()
                        .child(Input::new(&self.rename)),
                )
                .child(
                    Button::new("inbox-rename-save")
                        .label(tr("inbox-rename-save"))
                        .primary()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.commit_rename(window, cx);
                        })),
                )
                .child(
                    Button::new("inbox-rename-cancel")
                        .label(tr("inbox-rename-cancel"))
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_rename(cx))),
                ),
        )
    }

    fn render_item(&self, ix: usize, items: &[Item], cx: &mut Context<Self>) -> AnyElement {
        match items.get(ix).copied() {
            Some(Item::Header { section, count }) => self.render_header(section, count, cx),
            Some(Item::Row(row_ix)) => match self.model.rows().get(row_ix) {
                Some(row) => self.render_row(ix, row.clone(), cx),
                None => div().into_any_element(),
            },
            None => div().into_any_element(),
        }
    }

    fn render_header(&self, section: Section, count: usize, cx: &mut Context<Self>) -> AnyElement {
        let name = tr(&format!("inbox-section-{}", section.slug()));
        let a11y = tr_args(
            "inbox-section-a11y",
            &[("name", &name), ("count", &count.to_string())],
        );
        div()
            .id(SharedString::from(format!(
                "inbox-section-{}",
                section.slug()
            )))
            .role(Role::Heading)
            .test_support()
            .aria_label(a11y)
            .h(HEADER_HEIGHT)
            .w_full()
            .px_3()
            .flex()
            .items_center()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().muted_foreground)
            .child(format!("{name} ({count})"))
            .into_any_element()
    }

    fn render_row(&self, ix: usize, row: ThreadRow, cx: &mut Context<Self>) -> AnyElement {
        let title = if row.title.trim().is_empty() {
            tr("inbox-row-untitled")
        } else {
            row.title.clone()
        };
        let status = tr(&format!(
            "inbox-filter-status-{}",
            match row.status {
                Status::NeedsYou => "needs_you",
                Status::Running => "running",
                Status::Done | Status::Idle => "done",
                Status::Failed => "failed",
                Status::Cancelled => "cancelled",
            }
        ));
        let meta = [
            Some(row.agent.clone()),
            row.workspace.clone(),
            row.archived.then(|| tr("inbox-row-archived")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" - ");
        // The quick buttons answer the first approval; the detail line is the first open
        // interrupt of any kind.
        let approval = row.first_approval().cloned();
        let id = row.thread_id.clone();
        let rename_id = id.clone();
        let archive_id = id.clone();
        let archived = row.archived;
        let (allow, deny) = approval.as_ref().map_or((false, false), |p| {
            (
                p.option_for(Verdict::Allow).is_some(),
                p.option_for(Verdict::Deny).is_some(),
            )
        });
        let approval_title = approval.map(|p| p.title).unwrap_or_default();
        let detail = row.detail().unwrap_or_default().to_owned();
        let (allow_id, deny_id) = (id.clone(), id.clone());
        let open_id = id.clone();
        div()
            .id(("row", ix))
            .test_support()
            .w_full()
            .h(ROW_HEIGHT)
            .child(
                h_flex()
                    .id(ROW_ID)
                    .test_support()
                    .aria_label(tr_args(
                        "inbox-row-a11y",
                        &[
                            ("title", &title),
                            ("agent", &row.agent),
                            ("status", &status),
                        ],
                    ))
                    .size_full()
                    .px_3()
                    .gap_2()
                    .items_center()
                    // The title and detail area is the click target that opens the thread. It is
                    // a sibling of the buttons, so their clicks never reach it.
                    .child(
                        v_flex()
                            .id(SharedString::from(format!("inbox-open-{id}")))
                            .test_support()
                            .flex_1()
                            .min_w_0()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_thread(&open_id, cx);
                            }))
                            .child(div().truncate().child(title.clone()))
                            .child(
                                div()
                                    .truncate()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(if detail.is_empty() {
                                        meta
                                    } else {
                                        detail.clone()
                                    }),
                            ),
                    )
                    .when(allow, |el| {
                        el.child(
                            Button::new(SharedString::from(format!("inbox-allow-{id}")))
                                .label(tr("inbox-allow"))
                                .accessibility_label(tr_args(
                                    "inbox-allow-a11y",
                                    &[("detail", &approval_title)],
                                ))
                                .primary()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.answer(&allow_id, Verdict::Allow, window, cx);
                                })),
                        )
                    })
                    .when(deny, |el| {
                        el.child(
                            Button::new(SharedString::from(format!(
                                "inbox-deny-{}",
                                row.thread_id
                            )))
                            .label(tr("inbox-deny"))
                            .accessibility_label(tr_args(
                                "inbox-deny-a11y",
                                &[("detail", &approval_title)],
                            ))
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.answer(&deny_id, Verdict::Deny, window, cx);
                                },
                            )),
                        )
                    })
                    .child(
                        Button::new(SharedString::from(format!(
                            "inbox-rename-{}",
                            row.thread_id
                        )))
                        .label(tr("inbox-rename"))
                        .ghost()
                        .accessibility_label(tr_args("inbox-rename-a11y", &[("title", &title)]))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.start_rename(&rename_id, window, cx);
                            },
                        )),
                    )
                    .child(
                        Button::new(SharedString::from(format!(
                            "inbox-archive-{}",
                            row.thread_id
                        )))
                        .label(tr(if archived {
                            "inbox-restore"
                        } else {
                            "inbox-archive"
                        }))
                        .ghost()
                        .accessibility_label(tr_args(
                            if archived {
                                "inbox-restore-a11y"
                            } else {
                                "inbox-archive-a11y"
                            },
                            &[("title", &title)],
                        ))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.archive(&archive_id, !archived, window, cx);
                            },
                        )),
                    ),
            )
            .into_any_element()
    }
}

impl Render for InboxView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.sizes.len() != self.model.items().len() {
            self.rebuild();
        }
        let status = self.status_text();
        let empty = self.empty_state();
        let items = Rc::new(self.model.items());
        v_flex()
            .id("inbox")
            .role(Role::Region)
            .test_support()
            .aria_label(tr("inbox-region-a11y"))
            .size_full()
            .gap_2()
            .child(div().text_xl().child(tr("inbox-title")))
            .child(
                div()
                    .id("inbox-search")
                    .test_support()
                    .aria_label(tr("inbox-search-a11y"))
                    .child(Input::new(&self.search)),
            )
            .child(self.render_filters(cx))
            .children(self.render_rename_bar(cx))
            .child(
                div()
                    .id("inbox-status")
                    .role(Role::Status)
                    .test_support()
                    .aria_label(status.clone())
                    .text_color(cx.theme().muted_foreground)
                    .child(status),
            )
            .when_some(empty, |el, filtered| {
                let (id, label, a11y) = if filtered {
                    (
                        EMPTY_CLEAR_FILTERS_ID,
                        tr("inbox-empty-clear-filters"),
                        tr("inbox-empty-clear-filters-a11y"),
                    )
                } else {
                    (
                        EMPTY_NEW_TASK_ID,
                        tr("inbox-empty-new-task"),
                        tr("inbox-empty-new-task-a11y"),
                    )
                };
                el.child(
                    div().child(
                        Button::new(id)
                            .primary()
                            .label(label)
                            .accessibility_label(a11y)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if filtered {
                                    this.clear_filters(window, cx);
                                } else {
                                    this.request_new_task(cx);
                                }
                            })),
                    ),
                )
            })
            .child(
                div()
                    .id(LIST_ID)
                    .test_support()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .id("inbox-scroll")
                            .relative()
                            .size_full()
                            .child(
                                v_virtual_list(
                                    cx.entity().clone(),
                                    "inbox-virtual-list",
                                    self.sizes.clone(),
                                    move |view, visible, _, cx| {
                                        visible.map(|ix| view.render_item(ix, &items, cx)).collect()
                                    },
                                )
                                .track_scroll(&self.scroll),
                            )
                            .scrollbar(&self.scroll, ScrollbarAxis::Vertical),
                    ),
            )
    }
}
