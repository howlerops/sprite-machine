//! The task transcript pane: a `v_virtual_list` over text rows built from `client-core` view
//! models (`ThreadView` -> `TaskView` -> `Transcript` items).
//!
//! This file holds no decisions about the data. `client-core` decides what the items are; here
//! every item is laid out as a run of fixed-height rows (so the virtual list knows every size up
//! front, CONVENTIONS.md §8) and each row is drawn as plain text. Untrusted strings (messages,
//! tool output, paths) only ever reach a `div().child(String)`, never a markup parser.
//!
//! Item bodies are wrapped at [`WRAP_COLS`] columns and long bodies are capped (see
//! [`crate::task::tools`]). Row wrapping is by character count, not measured pixels.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use client_core::TaskStatus;
use client_core::view::{Item, TaskView, ThreadView};
use gpui_kit::component::{
    ActiveTheme as _, VirtualListScrollHandle,
    scroll::{ScrollableElement, ScrollbarAxis},
    v_flex, v_virtual_list,
};
use gpui_kit::prelude::*;
use gpui_kit::*;
use protocol::tunnel::ActorKind;
use protocol::turn_event::MachinePresence;

use crate::approval::{self, ApprovalCard, CardHandler};
use crate::diff::{DiffEvent, DiffInput, DiffView};
use crate::shell::i18n::{tr, tr_args};
use crate::task::media::{self, HttpFetcher, ImageFetcher, MediaCache, MediaRef};
use crate::task::row_cache::{RowCache, UpdateStats};
use crate::task::tools::{self, DiffStats};

/// Height of one transcript row.
pub const ROW_HEIGHT: Pixels = px(22.);
/// Element id of the pane (scroll target in tests).
pub const PANE_ID: &str = "transcript";
/// Element id carried by every laid-out row's text element.
pub const ROW_ID: &str = "transcript-row";
/// Element id of a clickable "more lines" / "show less" row.
pub const MORE_ID: &str = "transcript-more";
/// Element id of a "View diff" action.
pub const VIEW_DIFF_ID: &str = "view-diff-action";
/// Element id of the diff panel that covers the pane while a diff is open.
pub const DIFF_PANEL_ID: &str = "diff-panel-overlay";
/// Element id of the diff panel's close button.
pub const CLOSE_DIFF_ID: &str = "diff-panel-close";
/// Characters per wrapped line.
pub const WRAP_COLS: usize = 110;
/// Lines shown from a tool input/output, a reasoning block or a diff before "N more lines".
pub const MAX_BODY_LINES: usize = 12;
/// Diff lines shown per file.
pub const MAX_DIFF_LINES: usize = 30;
/// Terminal output lines shown (the tail).
pub const MAX_TERMINAL_LINES: usize = 20;

/// What a row shows; decides its font and weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// Thread title, machine presence, task status.
    Header,
    User,
    Text,
    Reasoning,
    ToolTitle,
    ToolDetail,
    DiffTitle,
    /// `path: +added -removed` with the "View diff" action (see [`Row::view_diff`]).
    DiffSummary,
    DiffLine,
    Plan,
    Terminal,
    Policy,
    Interrupt,
    Checkpoint,
    Step,
    Subagent,
    Raw,
    Error,
    /// "... N more lines" after a capped body.
    More,
    /// An inline image (see [`Row::media`]); taller than a text row.
    Image,
}

/// Colour role of a row (resolved against the theme at draw time; no hex in views).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Muted,
    Success,
    Danger,
    Warning,
}

/// One line of the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    pub tone: Tone,
    /// Indent steps (one step is a small left padding).
    pub indent: u8,
    pub text: String,
    /// For a "more" or "less" row: the key of the item it expands or collapses.
    pub expand: Option<usize>,
    /// For a [`RowKind::DiffSummary`] row: the id of the diff activity its "View diff" opens.
    pub view_diff: Option<String>,
    /// For a [`RowKind::Image`] row: the image drawn as a thumbnail.
    pub media: Option<MediaRef>,
}

impl Row {
    pub fn new(kind: RowKind, tone: Tone, indent: u8, text: impl Into<String>) -> Self {
        Self {
            kind,
            tone,
            indent,
            text: text.into(),
            expand: None,
            view_diff: None,
            media: None,
        }
    }

    /// The height this row is laid out at.
    pub fn height(&self) -> Pixels {
        if self.media.is_some() {
            media::THUMB_ROW_HEIGHT
        } else {
            ROW_HEIGHT
        }
    }

    fn monospace(&self) -> bool {
        matches!(
            self.kind,
            RowKind::ToolDetail | RowKind::DiffLine | RowKind::Terminal
        )
    }
}

/// Makes `text` safe to show on one line: tabs become spaces, other control characters are
/// dropped (an escape sequence in tool output must not reach the renderer).
pub fn sanitize(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

/// `text` split on newlines and hard-wrapped at `cols` characters, sanitized.
pub fn wrap_lines(text: &str, cols: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let line = sanitize(raw.trim_end_matches('\r'));
        if line.is_empty() {
            lines.push(String::new());
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        for chunk in chars.chunks(cols.max(1)) {
            lines.push(chunk.iter().collect());
        }
    }
    lines
}

/// Pushes at most `max` wrapped lines of `text`, then a "N more lines" row when capped.
pub fn push_body(
    out: &mut Vec<Row>,
    text: &str,
    max: usize,
    kind: RowKind,
    tone: Tone,
    indent: u8,
) {
    let lines = wrap_lines(text, WRAP_COLS);
    let total = lines.len();
    for line in lines.into_iter().take(max) {
        out.push(Row::new(kind, tone, indent, line));
    }
    if total > max {
        push_more(out, total - max, indent);
    }
}

/// The "... N more lines" row.
pub fn push_more(out: &mut Vec<Row>, hidden: usize, indent: u8) {
    out.push(Row::new(
        RowKind::More,
        Tone::Muted,
        indent,
        tr_args("transcript-more-lines", &[("detail", &hidden.to_string())]),
    ));
}

/// The rows of one transcript item.
pub fn item_rows(item: &Item, expanded: bool, out: &mut Vec<Row>) {
    item_rows_with_cards(item, expanded, &[], out);
}

/// Like [`item_rows`]; an approval interrupt that has a card in `cards` is shown by
/// [`approval::card_rows`] (the unanswered card itself is pinned above the list).
pub fn item_rows_with_cards(
    item: &Item,
    expanded: bool,
    cards: &[ApprovalCard],
    out: &mut Vec<Row>,
) {
    item_rows_with(item, expanded, cards, &mut DiffStats::default(), out);
}

/// Like [`item_rows_with_cards`], taking diff counts from `stats`.
pub fn item_rows_with(
    item: &Item,
    expanded: bool,
    cards: &[ApprovalCard],
    stats: &mut DiffStats,
    out: &mut Vec<Row>,
) {
    match item {
        Item::User(user) => {
            let who = if user.author.kind == ActorKind::User {
                tr("transcript-you")
            } else {
                sanitize(user.author.name.as_deref().unwrap_or(&user.author.id))
            };
            let start = out.len();
            push_body(out, &user.text, usize::MAX, RowKind::User, Tone::Normal, 0);
            match out.get_mut(start) {
                Some(first) => first.text = format!("{who}: {}", first.text),
                None => out.push(Row::new(RowKind::User, Tone::Normal, 0, format!("{who}:"))),
            }
        }
        Item::Text(text) => {
            let start = out.len();
            push_body(out, &text.text, usize::MAX, RowKind::Text, Tone::Normal, 0);
            if out.len() == start || (out.len() == start + 1 && out[start].text.is_empty()) {
                out.truncate(start);
                out.push(Row::new(
                    RowKind::Text,
                    Tone::Muted,
                    0,
                    tr("transcript-empty-message"),
                ));
            }
        }
        Item::Reasoning(text) => {
            out.push(Row::new(
                RowKind::Reasoning,
                Tone::Muted,
                0,
                tr("transcript-reasoning"),
            ));
            push_body(
                out,
                &text.text,
                cap(expanded, MAX_BODY_LINES),
                RowKind::Reasoning,
                Tone::Muted,
                1,
            );
        }
        Item::Tool(tool) => {
            tools::tool_rows(tool, expanded, out);
            media::push_image_rows(&tool.content, 3, out);
        }
        Item::Activity(activity) => tools::activity_rows_with(activity, expanded, stats, out),
        Item::Interrupt(interrupt) => {
            match cards
                .iter()
                .find(|card| card.interrupt_id == interrupt.id())
            {
                Some(card) => approval::card_rows(card, out),
                None => tools::interrupt_rows(interrupt, expanded, out),
            }
        }
        Item::Checkpoint(checkpoint) => tools::checkpoint_rows(checkpoint, out),
        Item::Step(step) => tools::step_rows(step, out),
        Item::Subagent(sub) => tools::subagent_rows(sub, expanded, out),
        Item::Raw(raw) => tools::raw_rows(raw, out),
        Item::Error { code, message } => {
            let message = sanitize(message);
            let text = match code {
                Some(code) => tr_args(
                    "transcript-error-code",
                    &[("code", &sanitize(code)), ("detail", &message)],
                ),
                None => tr_args("transcript-error", &[("detail", &message)]),
            };
            out.push(Row::new(RowKind::Error, Tone::Danger, 0, text));
        }
    }
}

/// The status line of a task.
pub fn status_row(status: TaskStatus) -> Row {
    let (id, tone) = match status {
        TaskStatus::Starting => ("task-status-starting", Tone::Muted),
        TaskStatus::Running => ("task-status-running", Tone::Muted),
        TaskStatus::NeedsYou => ("task-status-needs-you", Tone::Warning),
        TaskStatus::Succeeded => ("task-status-succeeded", Tone::Success),
        TaskStatus::Failed => ("task-status-failed", Tone::Danger),
        TaskStatus::Cancelled => ("task-status-cancelled", Tone::Warning),
    };
    Row::new(RowKind::Header, tone, 0, tr(id))
}

/// Line limit for a body: `max`, or no limit when the item is expanded.
pub fn cap(expanded: bool, max: usize) -> usize {
    if expanded { usize::MAX } else { max }
}

/// The rows of one task: its items, then its status. `next_key` numbers the items of the
/// thread; an item whose key is in `expanded` shows its capped bodies in full.
pub fn task_rows(
    task: &TaskView,
    expanded: &BTreeSet<usize>,
    next_key: &mut usize,
    cards: &[ApprovalCard],
    out: &mut Vec<Row>,
) {
    let mut stats = DiffStats::default();
    for item in task.transcript().items() {
        let key = *next_key;
        *next_key += 1;
        item_block(item, key, expanded, cards, &mut stats, out);
    }
    out.push(status_row(task.status()));
}

/// The rows of the item numbered `key`, with its "more" / "show less" rows wired to `key`.
/// Returns `true` when the item has a capped body (its rows then depend on `key` and on
/// whether `key` is in `expanded`).
pub fn item_block(
    item: &Item,
    key: usize,
    expanded: &BTreeSet<usize>,
    cards: &[ApprovalCard],
    stats: &mut DiffStats,
    out: &mut Vec<Row>,
) -> bool {
    let start = out.len();
    item_rows_with(item, false, cards, stats, out);
    if !out[start..].iter().any(|row| row.kind == RowKind::More) {
        return false;
    }
    if expanded.contains(&key) {
        out.truncate(start);
        item_rows_with(item, true, cards, stats, out);
        let mut less = Row::new(RowKind::More, Tone::Muted, 1, tr("transcript-show-less"));
        less.expand = Some(key);
        out.push(less);
    } else {
        for row in out[start..].iter_mut().filter(|r| r.kind == RowKind::More) {
            row.expand = Some(key);
        }
    }
    true
}

/// The rows of a whole thread with every body capped.
pub fn thread_rows(thread: &ThreadView) -> Vec<Row> {
    thread_rows_expanded(thread, &BTreeSet::new())
}

/// The rows of a whole thread: title and machine presence, then each task in order. Items are
/// numbered in order across tasks; those numbered in `expanded` are shown in full.
pub fn thread_rows_expanded(thread: &ThreadView, expanded: &BTreeSet<usize>) -> Vec<Row> {
    thread_rows_with_cards(thread, expanded, &[])
}

/// [`thread_rows_expanded`] with approval cards: see [`item_rows_with_cards`].
pub fn thread_rows_with_cards(
    thread: &ThreadView,
    expanded: &BTreeSet<usize>,
    cards: &[ApprovalCard],
) -> Vec<Row> {
    let mut out = header_rows(thread);
    let mut next_key = 0;
    for task in thread.tasks() {
        task_rows(task, expanded, &mut next_key, cards, &mut out);
    }
    out
}

/// The rows above the tasks: title, mode, context and machine presence.
pub fn header_rows(thread: &ThreadView) -> Vec<Row> {
    let mut out = Vec::new();
    let title = thread
        .title()
        .map(sanitize)
        .unwrap_or_else(|| tr("task-untitled"));
    out.push(Row::new(RowKind::Header, Tone::Normal, 0, title));
    if let Some(mode) = thread
        .state("session")
        .and_then(|session| session.get("current_mode_id"))
        .and_then(|mode| mode.as_str())
    {
        out.push(Row::new(
            RowKind::Header,
            Tone::Muted,
            0,
            tr_args("task-mode", &[("detail", &sanitize(mode))]),
        ));
    }
    if let Some(usage) = thread.state("usage")
        && let (Some(used), Some(window)) = (
            usage.get("context_used").and_then(|v| v.as_u64()),
            usage.get("context_size").and_then(|v| v.as_u64()),
        )
    {
        out.push(Row::new(
            RowKind::Header,
            Tone::Muted,
            0,
            tr_args(
                "task-context",
                &[
                    ("detail", &used.to_string()),
                    ("choice", &window.to_string()),
                ],
            ),
        ));
    }
    if let (Some(machine), Some(presence)) = (thread.machine_id(), thread.presence()) {
        let state = match presence.state {
            MachinePresence::Online => "task-presence-online",
            MachinePresence::Asleep => "task-presence-asleep",
            MachinePresence::Offline => "task-presence-offline",
        };
        out.push(Row::new(
            RowKind::Header,
            if presence.state == MachinePresence::Online {
                Tone::Muted
            } else {
                Tone::Warning
            },
            0,
            tr_args(
                "task-machine-presence",
                &[("detail", &sanitize(machine)), ("choice", &tr(state))],
            ),
        ));
    }
    out
}

/// The transcript pane. Call [`TranscriptPane::set_thread`] whenever the thread view changes.
pub struct TranscriptPane {
    cache: RowCache,
    item_sizes: Rc<Vec<Size<Pixels>>>,
    scroll_handle: VirtualListScrollHandle,
    follow_tail: bool,
    has_thread: bool,
    expanded: BTreeSet<usize>,
    approvals: Vec<ApprovalCard>,
    approval_handler: Option<CardHandler>,
    workspace_root: Option<PathBuf>,
    open_diff: Option<OpenDiff>,
    media: MediaCache,
    media_polling: bool,
    /// `item_sizes` was built from row heights (some row holds an image).
    image_sized: bool,
    /// The image shown full size over the pane.
    open_image: Option<MediaRef>,
}

/// What the pane asks its host to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEvent {
    /// Open `path` (resolved inside the workspace root) at 1-based `line`; re-emitted from the
    /// diff viewer's `DiffEvent::OpenFile`.
    OpenFile { path: PathBuf, line: u32 },
}

impl EventEmitter<TranscriptEvent> for TranscriptPane {}

/// The diff shown over the pane.
struct OpenDiff {
    activity_id: String,
    input: DiffInput,
    view: Entity<DiffView>,
    _subscription: Subscription,
}

impl Default for TranscriptPane {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptPane {
    pub fn new() -> Self {
        Self {
            cache: RowCache::new(),
            item_sizes: Rc::new(Vec::new()),
            scroll_handle: VirtualListScrollHandle::new(),
            follow_tail: false,
            has_thread: false,
            expanded: BTreeSet::new(),
            approvals: Vec::new(),
            approval_handler: None,
            workspace_root: None,
            open_diff: None,
            media: MediaCache::new(Arc::new(HttpFetcher)),
            media_polling: false,
            image_sized: false,
            open_image: None,
        }
    }

    /// Fetch images with `fetcher` instead of plain HTTP (tests inject a fake).
    pub fn set_image_fetcher(&mut self, fetcher: Arc<dyn ImageFetcher>) {
        self.media.set_fetcher(fetcher);
    }

    /// The load state of the image at `url`, once it was asked for.
    pub fn image_state(&self, url: &str) -> Option<&media::LoadState> {
        self.media.state(url)
    }

    /// The image open full size, if any.
    pub fn open_image_ref(&self) -> Option<&MediaRef> {
        self.open_image.as_ref()
    }

    /// Show `image` full size over the pane.
    pub fn open_image(&mut self, image: &MediaRef, cx: &mut Context<Self>) {
        self.media.ensure(&image.url);
        self.open_image = Some(image.clone());
        self.watch_media(cx);
        cx.notify();
    }

    pub fn close_image(&mut self, cx: &mut Context<Self>) {
        if self.open_image.take().is_some() {
            cx.notify();
        }
    }

    /// Start fetching the images the rows show, and poll until they are in.
    fn fetch_images(&mut self, cx: &mut Context<Self>) {
        let urls: Vec<String> = self
            .cache
            .rows()
            .iter()
            .filter_map(|row| row.media.as_ref().map(|media| media.url.clone()))
            .collect();
        let mut started = false;
        for url in urls {
            started |= self.media.ensure(&url);
        }
        if started {
            self.watch_media(cx);
        }
    }

    fn watch_media(&mut self, cx: &mut Context<Self>) {
        if self.media_polling {
            return;
        }
        self.media_polling = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
                let keep_going = this
                    .update(cx, |this, cx| {
                        let (changed, loading) = this.media.pump();
                        if changed {
                            cx.notify();
                        }
                        this.media_polling = loading;
                        loading
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    /// Move finished image fetches into their states (the polling loop does this too).
    pub fn pump_images(&mut self, cx: &mut Context<Self>) -> bool {
        let (changed, loading) = self.media.pump();
        if changed {
            cx.notify();
        }
        loading
    }

    /// The workspace root of the open thread. "Open file" is offered by the diff viewer only
    /// with a root, and only for paths inside it.
    pub fn set_workspace_root(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        self.workspace_root = root.clone();
        if let Some(open) = &self.open_diff {
            open.view
                .update(cx, |view, cx| view.set_workspace_root(root, cx));
        }
        cx.notify();
    }

    /// The id of the diff activity shown over the pane, if any.
    pub fn open_diff_id(&self) -> Option<&str> {
        self.open_diff
            .as_ref()
            .map(|open| open.activity_id.as_str())
    }

    /// The diff viewer shown over the pane, if any.
    pub fn open_diff_view(&self) -> Option<&Entity<DiffView>> {
        self.open_diff.as_ref().map(|open| &open.view)
    }

    /// Shows the diff activity `activity_id` over the transcript. Returns `false` when the
    /// thread has no complete diff activity with that id.
    pub fn view_diff(&mut self, activity_id: &str, cx: &mut Context<Self>) -> bool {
        let Some(input) = self.diff_input(activity_id) else {
            return false;
        };
        let view = cx.new(|_| DiffView::new(input.clone(), self.workspace_root.clone()));
        let subscription = cx.subscribe(&view, |_, _, event: &DiffEvent, cx| match event {
            DiffEvent::OpenFile { path, line } => cx.emit(TranscriptEvent::OpenFile {
                path: path.clone(),
                line: *line,
            }),
        });
        self.open_diff = Some(OpenDiff {
            activity_id: activity_id.to_owned(),
            input,
            view,
            _subscription: subscription,
        });
        cx.notify();
        true
    }

    /// Closes the diff panel.
    pub fn close_diff(&mut self, cx: &mut Context<Self>) {
        if self.open_diff.take().is_some() {
            cx.notify();
        }
    }

    fn diff_input(&self, activity_id: &str) -> Option<DiffInput> {
        let activity = self.cache.activity(activity_id)?;
        if !activity.complete {
            return None;
        }
        activity.diff().map(|diff| DiffInput::from(&diff))
    }

    pub fn rows(&self) -> &[Row] {
        self.cache.rows()
    }

    /// What the last update of the rows rebuilt (items, rows, diff models).
    pub fn last_update(&self) -> UpdateStats {
        self.cache.last_update()
    }

    /// Keep the newest row in view when the thread changes.
    pub fn set_follow_tail(&mut self, follow: bool) {
        self.follow_tail = follow;
    }

    /// Updates the rows from `thread` and repaints. Only the items that changed since the last
    /// call have their rows built again (see [`RowCache`]).
    pub fn set_thread(&mut self, thread: &ThreadView, cx: &mut Context<Self>) {
        self.has_thread = true;
        self.cache.sync(thread, &self.expanded, &self.approvals);
        self.after_update(cx);
        let rows = self.cache.rows().len();
        if self.follow_tail && rows > 0 {
            self.scroll_handle
                .scroll_to_item(rows - 1, ScrollStrategy::Bottom);
        }
    }

    /// Sets the approval cards (from `ApprovalController::cards`) and where their clicks go.
    /// Unanswered cards are pinned above the transcript; answered ones stay in it as rows.
    pub fn set_approvals(
        &mut self,
        cards: Vec<ApprovalCard>,
        handler: Option<CardHandler>,
        cx: &mut Context<Self>,
    ) {
        self.approvals = cards;
        self.approval_handler = handler;
        self.rebuild(cx);
    }

    /// Shows the capped bodies of item `key` in full, or caps them again.
    pub fn toggle_expanded(&mut self, key: usize, cx: &mut Context<Self>) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        self.rebuild(cx);
    }

    /// Applies a change of the expanded items or the cards to the rows already built.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        if !self.has_thread {
            return;
        }
        self.cache.refresh(&self.expanded, &self.approvals);
        self.after_update(cx);
    }

    fn after_update(&mut self, cx: &mut Context<Self>) {
        self.refresh_open_diff(cx);
        let rows = self.cache.rows().len();
        let has_images = self.cache.rows().iter().any(|row| row.media.is_some());
        if has_images {
            self.item_sizes = Rc::new(
                self.cache
                    .rows()
                    .iter()
                    .map(|row| size(px(100.), row.height()))
                    .collect(),
            );
            self.image_sized = true;
            self.fetch_images(cx);
        } else if self.item_sizes.len() != rows || self.image_sized {
            self.item_sizes = Rc::new(vec![size(px(100.), ROW_HEIGHT); rows]);
            self.image_sized = false;
        }
        cx.notify();
    }

    /// Follows the open diff when its activity changes; closes it when the activity is gone.
    fn refresh_open_diff(&mut self, cx: &mut Context<Self>) {
        let Some(open) = &self.open_diff else { return };
        match self.diff_input(&open.activity_id) {
            None => self.open_diff = None,
            Some(input) if input != open.input => {
                let view = open.view.clone();
                view.update(cx, |view, cx| view.set_input(input.clone(), cx));
                if let Some(open) = &mut self.open_diff {
                    open.input = input;
                }
            }
            Some(_) => {}
        }
    }

    fn render_diff_panel(&self, open: &OpenDiff, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (background, border, foreground) = (theme.background, theme.border, theme.foreground);
        let close = tr("transcript-close-diff");
        v_flex()
            .id(DIFF_PANEL_ID)
            .test_support()
            .aria_label(tr_args(
                "transcript-diff-panel",
                &[("detail", &sanitize(&open.input.path))],
            ))
            .absolute()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .h(ROW_HEIGHT)
                    .px_3()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(sanitize(&open.input.path)),
                    )
                    .child(
                        div()
                            .id(CLOSE_DIFF_ID)
                            .test_support()
                            .aria_label(SharedString::from(close.clone()))
                            .px_2()
                            .cursor_pointer()
                            .text_sm()
                            .on_click(cx.listener(|pane, _, _, cx| pane.close_diff(cx)))
                            .child(close),
                    ),
            )
            .child(div().flex_1().min_h_0().child(open.view.clone()))
    }

    fn render_image_panel(
        &self,
        image: &MediaRef,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (background, border, foreground) = (theme.background, theme.border, theme.foreground);
        let close = media::close_label();
        v_flex()
            .id(media::PANEL_ID)
            .role(Role::Dialog)
            .test_support()
            .aria_label(crate::compose::tr_args(
                "compose-media-panel-a11y",
                &[("name", &image.name)],
            ))
            .absolute()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .h(ROW_HEIGHT)
                    .px_3()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(image.name.clone()),
                    )
                    .child(
                        div()
                            .id(media::CLOSE_ID)
                            .test_support()
                            .aria_label(SharedString::from(crate::compose::tr(
                                "compose-media-close-a11y",
                            )))
                            .px_2()
                            .cursor_pointer()
                            .text_sm()
                            .on_click(cx.listener(|pane, _, _, cx| pane.close_image(cx)))
                            .child(close),
                    ),
            )
            .child(div().flex_1().min_h_0().p_3().child(media::render_image(
                image,
                self.media.state(&image.url),
                media::FULL_MAX,
            )))
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let row = &self.cache.rows()[ix];
        if let Some(image) = row.media.clone() {
            return self.render_image_row(ix, row.indent, image, cx);
        }
        let text = SharedString::from(row.text.clone());
        let theme = cx.theme();
        let color = match row.tone {
            Tone::Normal => theme.foreground,
            Tone::Muted => theme.muted_foreground,
            Tone::Success => theme.success,
            Tone::Danger => theme.danger,
            Tone::Warning => theme.warning,
        };
        let mono = row.monospace().then(|| theme.mono_font_family.clone());
        let bold = matches!(
            row.kind,
            RowKind::User | RowKind::ToolTitle | RowKind::DiffTitle | RowKind::Header
        );
        let shaded = matches!(row.kind, RowKind::User);
        let background = theme.secondary;
        let accent = theme.primary;
        let expand = row.expand;
        let view_diff = row.view_diff.clone();
        let outer_id: ElementId = if expand.is_some() {
            MORE_ID.into()
        } else {
            ("row", ix).into()
        };
        let listener = cx.listener(move |pane, _, _, cx| {
            if let Some(key) = expand {
                pane.toggle_expanded(key, cx);
            }
        });
        div()
            .id(outer_id)
            .test_support()
            .w_full()
            .h(ROW_HEIGHT)
            .when(expand.is_some(), |el| {
                el.cursor_pointer().on_click(listener)
            })
            .child(
                div()
                    .id(ROW_ID)
                    .test_support()
                    .aria_label(text.clone())
                    .size_full()
                    .px_3()
                    .pl(px(12. + 16. * f32::from(row.indent)))
                    .text_sm()
                    .text_color(color)
                    .when_some(mono, |el, family| el.font_family(family))
                    .when(bold, |el| el.font_weight(FontWeight::SEMIBOLD))
                    .when(shaded, |el| el.bg(background))
                    .when_some(view_diff, |el, activity_id| {
                        let label = tr("transcript-view-diff");
                        el.flex().flex_row().items_center().gap_2().child(
                            div()
                                .id(VIEW_DIFF_ID)
                                .test_support()
                                .aria_label(SharedString::from(label.clone()))
                                .flex_none()
                                .cursor_pointer()
                                .text_color(accent)
                                .on_click(cx.listener(move |pane, _, _, cx| {
                                    pane.view_diff(&activity_id, cx);
                                }))
                                .child(label),
                        )
                    })
                    .child(div().truncate().child(text)),
            )
            .into_any_element()
    }

    /// A row that holds an image: the thumbnail (or the note that it is loading or could not
    /// load), clickable to open the full-size panel.
    fn render_image_row(
        &self,
        ix: usize,
        indent: u8,
        image: MediaRef,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = media::open_label(&image);
        let muted = cx.theme().muted_foreground;
        let content = media::render_image(&image, self.media.state(&image.url), media::THUMB_MAX);
        let opened = image.clone();
        div()
            .id(("row", ix))
            .test_support()
            .w_full()
            .h(media::THUMB_ROW_HEIGHT)
            .px_3()
            .pl(px(12. + 16. * f32::from(indent)))
            .text_color(muted)
            .child(
                div()
                    .id(media::THUMB_ID)
                    .test_support()
                    .aria_label(SharedString::from(label))
                    .min_w(px(32.))
                    .min_h(px(32.))
                    .cursor_pointer()
                    .on_click(cx.listener(move |pane, _, _, cx| pane.open_image(&opened, cx)))
                    .child(content),
            )
            .into_any_element()
    }
}

impl Render for TranscriptPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pinned: Vec<ApprovalCard> = self
            .approvals
            .iter()
            .filter(|card| !matches!(card.state, approval::CardState::Resolved { .. }))
            .cloned()
            .collect();
        v_flex()
            .id(PANE_ID)
            .test_support()
            .aria_label(tr("task-label"))
            .relative()
            .size_full()
            .when(!pinned.is_empty(), |el| {
                el.child(approval::render_cards(
                    &pinned,
                    self.approval_handler.clone(),
                ))
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .when_some(
                        self.open_diff
                            .as_ref()
                            .map(|open| self.render_diff_panel(open, cx)),
                        |el, panel| el.child(panel),
                    )
                    .when_some(
                        self.open_image
                            .clone()
                            .map(|image| self.render_image_panel(&image, cx)),
                        |el, panel| el.child(panel),
                    )
                    .child(
                        v_flex()
                            .id("transcript-scroll")
                            .relative()
                            .size_full()
                            .child(
                                v_virtual_list(
                                    cx.entity().clone(),
                                    "transcript-list",
                                    self.item_sizes.clone(),
                                    |pane, visible_range, _, cx| {
                                        visible_range.map(|ix| pane.render_row(ix, cx)).collect()
                                    },
                                )
                                .track_scroll(&self.scroll_handle),
                            )
                            .scrollbar(&self.scroll_handle, ScrollbarAxis::Vertical),
                    ),
            )
    }
}
