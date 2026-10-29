//! [`DiffView`]: a virtualized pane over a [`DiffModel`]. Every row has the same height so the
//! virtual list knows all sizes up front (CONVENTIONS.md §7, §8); only the rows in view are laid
//! out. Untrusted text only ever reaches `div().child(String)`, never a markup parser.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, VirtualListScrollHandle,
    scroll::{ScrollableElement, ScrollbarAxis},
    v_flex, v_virtual_list,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::model::{DiffInput, DiffModel, Line, LineKind, Mode, Row, resolve_path};
use super::{tr, tr_args};

/// Height of one row.
pub const ROW_HEIGHT: Pixels = px(20.);
/// Element id of the pane (scroll target in tests).
pub const PANE_ID: &str = "diff";
/// Element id carried by every laid-out line row (and the "no newline" marker rows).
pub const ROW_ID: &str = "diff-row";
/// Element id of a hunk header row.
pub const HUNK_ID: &str = "diff-hunk";
/// Element id of a clickable collapsed-gap row.
pub const GAP_ID: &str = "diff-gap";
/// Element id of a hunk's "Open file" action.
pub const OPEN_ID: &str = "diff-open";
/// Element id of the notice that replaces the lines (binary, too large, identical).
pub const NOTICE_ID: &str = "diff-notice";
/// Element id of the title / summary line.
pub const TITLE_ID: &str = "diff-title";
/// Element ids of the mode buttons and "Expand all".
pub const UNIFIED_ID: &str = "diff-mode-unified";
pub const SPLIT_ID: &str = "diff-mode-split";
pub const EXPAND_ALL_ID: &str = "diff-expand-all";

/// What the viewer asks its host to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffEvent {
    /// Open `path` (inside the workspace root) at 1-based `line`.
    OpenFile { path: PathBuf, line: u32 },
}

pub struct DiffView {
    model: DiffModel,
    mode: Mode,
    expanded: BTreeSet<usize>,
    rows: Vec<Row>,
    item_sizes: Rc<Vec<Size<Pixels>>>,
    scroll_handle: VirtualListScrollHandle,
    workspace_root: Option<PathBuf>,
}

impl EventEmitter<DiffEvent> for DiffView {}

impl DiffView {
    /// A viewer for `input`. "Open file" is offered only when `workspace_root` is `Some` and the
    /// diff's path is inside it.
    pub fn new(input: DiffInput, workspace_root: Option<PathBuf>) -> Self {
        let model = DiffModel::build(&input);
        let mut view = Self {
            model,
            mode: Mode::Unified,
            expanded: BTreeSet::new(),
            rows: Vec::new(),
            item_sizes: Rc::new(Vec::new()),
            scroll_handle: VirtualListScrollHandle::new(),
            workspace_root,
        };
        view.relayout();
        view
    }

    pub fn model(&self) -> &DiffModel {
        &self.model
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The display rows of the current layout.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn expanded_gaps(&self) -> &BTreeSet<usize> {
        &self.expanded
    }

    /// Replaces the diff (e.g. when more of it has arrived); expansions are dropped.
    pub fn set_input(&mut self, input: DiffInput, cx: &mut Context<Self>) {
        self.model = DiffModel::build(&input);
        self.expanded.clear();
        self.relayout();
        cx.notify();
    }

    pub fn set_workspace_root(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        self.workspace_root = root;
        cx.notify();
    }

    pub fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            self.relayout();
            cx.notify();
        }
    }

    /// Opens the collapsed gap `id` (see [`super::Gap::id`]).
    pub fn expand_gap(&mut self, id: usize, cx: &mut Context<Self>) {
        if self.model.gap_by_id(id).is_some() && self.expanded.insert(id) {
            self.relayout();
            cx.notify();
        }
    }

    pub fn expand_all(&mut self, cx: &mut Context<Self>) {
        let all: BTreeSet<usize> = self.model.gaps().map(|gap| gap.id).collect();
        if all != self.expanded {
            self.expanded = all;
            self.relayout();
            cx.notify();
        }
    }

    /// `true` when the "Open file" action is offered.
    pub fn can_open(&self) -> bool {
        resolve_path(self.workspace_root.as_deref(), &self.model.path).is_some()
    }

    /// Emits [`DiffEvent::OpenFile`] for hunk `index`. Returns `false` (and emits nothing) when
    /// there is no workspace root, the path is outside it, or the hunk does not exist.
    pub fn open_hunk(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let Some(path) = resolve_path(self.workspace_root.as_deref(), &self.model.path) else {
            return false;
        };
        let Some(hunk) = self.model.hunk_at(index) else {
            return false;
        };
        let line = hunk.open_line;
        cx.emit(DiffEvent::OpenFile { path, line });
        true
    }

    fn relayout(&mut self) {
        self.rows = self.model.rows(self.mode, &self.expanded);
        self.item_sizes = Rc::new(vec![size(px(100.), ROW_HEIGHT); self.rows.len()]);
    }

    fn gutter_width(&self) -> Pixels {
        px(8. * self.model.number_width() as f32 + 14.)
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let summary = tr_args(
            "diff-summary",
            &[
                ("path", &self.model.path),
                ("added", &self.model.added.to_string()),
                ("removed", &self.model.removed.to_string()),
            ],
        );
        let has_gaps = self.model.gaps().next().is_some();
        let mode = self.mode;
        let unified = cx.listener(|view, _, _, cx| view.set_mode(Mode::Unified, cx));
        let split = cx.listener(|view, _, _, cx| view.set_mode(Mode::Split, cx));
        let expand = cx.listener(|view, _, _, cx| view.expand_all(cx));
        let border = theme.border;
        let muted = theme.muted_foreground;
        let selected = theme.secondary;
        h_flex()
            .w_full()
            .h(px(32.))
            .px_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .id(TITLE_ID)
                    .test_support()
                    .aria_label(SharedString::from(summary.clone()))
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .truncate()
                    .child(summary)
                    .when(self.model.new_file, |el| {
                        el.child(
                            div()
                                .text_color(muted)
                                .child(format!(" ({})", tr("diff-new-file"))),
                        )
                    }),
            )
            .when(self.model.notice.is_none(), |el| {
                el.child(button(
                    UNIFIED_ID,
                    tr("diff-mode-unified"),
                    tr("diff-mode-unified-a11y"),
                    mode == Mode::Unified,
                    selected,
                    unified,
                ))
                .child(button(
                    SPLIT_ID,
                    tr("diff-mode-split"),
                    tr("diff-mode-split-a11y"),
                    mode == Mode::Split,
                    selected,
                    split,
                ))
                .when(has_gaps, |el| {
                    el.child(button(
                        EXPAND_ALL_ID,
                        tr("diff-expand-all"),
                        tr("diff-expand-all"),
                        false,
                        selected,
                        expand,
                    ))
                })
            })
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let colors = Colors {
            foreground: theme.foreground,
            muted: theme.muted_foreground,
            added: theme.success,
            removed: theme.danger,
            border: theme.border,
            header: theme.secondary,
            accent: theme.accent,
        };
        let mono = theme.mono_font_family.clone();
        let gutter = self.gutter_width();
        let row = self.rows[ix].clone();
        let inner = match row {
            Row::Hunk(index) => self.hunk_row(index, &colors, cx).into_any_element(),
            Row::Gap(id) => self.gap_row(id, &colors, cx).into_any_element(),
            Row::Unified(line) => {
                unified_row(&self.model.lines[line], gutter, &colors, &mono).into_any_element()
            }
            Row::Split { left, right } => {
                let lines = &self.model.lines;
                split_row(
                    left.map(|i| &lines[i]),
                    right.map(|i| &lines[i]),
                    gutter,
                    &colors,
                    &mono,
                )
                .into_any_element()
            }
            Row::NoNewline(_) => {
                let text = SharedString::from(tr("diff-no-newline"));
                div()
                    .id(ROW_ID)
                    .test_support()
                    .aria_label(text.clone())
                    .size_full()
                    .pl(gutter * 2. + px(24.))
                    .text_xs()
                    .italic()
                    .text_color(colors.muted)
                    .child(text)
                    .into_any_element()
            }
        };
        div()
            .id(("row", ix))
            .test_support()
            .w_full()
            .h(ROW_HEIGHT)
            .child(inner)
    }

    fn hunk_row(&self, index: usize, colors: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let hunk = self.model.hunk_at(index);
        let header = hunk.map(|h| h.header()).unwrap_or_default();
        let line = hunk.map_or(1, |h| h.open_line);
        let label = tr_args("diff-hunk-a11y", &[("line", &line.to_string())]);
        let open = self.can_open().then(|| {
            let listener = cx.listener(move |view, _, _, cx| {
                view.open_hunk(index, cx);
            });
            let a11y = tr_args(
                "diff-open-a11y",
                &[("path", &self.model.path), ("line", &line.to_string())],
            );
            div()
                .id(OPEN_ID)
                .test_support()
                .aria_label(SharedString::from(a11y))
                .px_2()
                .cursor_pointer()
                .text_xs()
                .text_color(colors.accent_text())
                .on_click(listener)
                .child(tr("diff-open"))
        });
        h_flex()
            .id(HUNK_ID)
            .test_support()
            .aria_label(SharedString::from(format!("{label} {header}")))
            .size_full()
            .px_3()
            .items_center()
            .bg(colors.header)
            .text_xs()
            .text_color(colors.muted)
            .child(div().flex_1().truncate().child(header))
            .when_some(open, |el, open| el.child(open))
    }

    fn gap_row(&self, id: usize, colors: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let hidden = self.model.gap_by_id(id).map_or(0, |gap| gap.hidden());
        let text = SharedString::from(if hidden == 1 {
            tr("diff-gap-one")
        } else {
            tr_args("diff-gap", &[("count", &hidden.to_string())])
        });
        let listener = cx.listener(move |view, _, _, cx| view.expand_gap(id, cx));
        div()
            .id(GAP_ID)
            .test_support()
            .aria_label(text.clone())
            .size_full()
            .px_3()
            .flex()
            .items_center()
            .cursor_pointer()
            .text_xs()
            .text_color(colors.muted)
            .border_y_1()
            .border_color(colors.border)
            .on_click(listener)
            .child(text)
    }
}

/// The theme colors a row needs, copied out so rows do not borrow the context.
struct Colors {
    foreground: Hsla,
    muted: Hsla,
    added: Hsla,
    removed: Hsla,
    border: Hsla,
    header: Hsla,
    accent: Hsla,
}

impl Colors {
    fn accent_text(&self) -> Hsla {
        self.accent
    }

    fn tint(&self, kind: LineKind) -> Option<Hsla> {
        match kind {
            LineKind::Added => Some(self.added.opacity(0.18)),
            LineKind::Removed => Some(self.removed.opacity(0.18)),
            LineKind::Context => None,
        }
    }
}

fn sign(kind: LineKind) -> &'static str {
    match kind {
        LineKind::Added => "+",
        LineKind::Removed => "-",
        LineKind::Context => " ",
    }
}

fn number(value: Option<u32>, width: Pixels, colors: &Colors) -> impl IntoElement {
    div()
        .w(width)
        .flex_none()
        .pr_2()
        .text_right()
        .text_color(colors.muted)
        .child(value.map(|n| n.to_string()).unwrap_or_default())
}

fn cell(
    line: Option<&Line>,
    show_old: bool,
    gutter: Pixels,
    colors: &Colors,
) -> impl IntoElement + use<> {
    let (no, kind, text) = match line {
        Some(line) => (
            if show_old { line.old_no } else { line.new_no },
            Some(line.kind),
            line.text.clone(),
        ),
        None => (None, None, String::new()),
    };
    h_flex()
        .flex_1()
        .min_w_0()
        .h_full()
        .overflow_hidden()
        .when_some(kind.and_then(|k| colors.tint(k)), |el, tint| el.bg(tint))
        .child(number(no, gutter, colors))
        .child(
            div()
                .w(px(12.))
                .flex_none()
                .text_color(colors.muted)
                .child(kind.map(sign).unwrap_or(" ")),
        )
        .child(div().flex_1().min_w_0().truncate().child(text))
}

fn unified_row(
    line: &Line,
    gutter: Pixels,
    colors: &Colors,
    mono: &SharedString,
) -> impl IntoElement {
    let (id, number_text) = match line.kind {
        LineKind::Added => ("diff-line-added", line.new_no),
        LineKind::Removed => ("diff-line-removed", line.old_no),
        LineKind::Context => ("diff-line-context", line.new_no),
    };
    let label = tr_args(
        id,
        &[
            ("line", &number_text.unwrap_or_default().to_string()),
            ("text", &line.text),
        ],
    );
    h_flex()
        .id(ROW_ID)
        .test_support()
        .aria_label(SharedString::from(label))
        .size_full()
        .overflow_hidden()
        .text_sm()
        .font_family(mono.clone())
        .text_color(colors.foreground)
        .when_some(colors.tint(line.kind), |el, tint| el.bg(tint))
        .child(number(line.old_no, gutter, colors))
        .child(number(line.new_no, gutter, colors))
        .child(
            div()
                .w(px(12.))
                .flex_none()
                .text_color(colors.muted)
                .child(sign(line.kind)),
        )
        .child(div().flex_1().min_w_0().truncate().child(line.text.clone()))
}

fn split_row(
    left: Option<&Line>,
    right: Option<&Line>,
    gutter: Pixels,
    colors: &Colors,
    mono: &SharedString,
) -> impl IntoElement {
    let none = tr("diff-split-empty");
    let describe = |line: Option<&Line>, old: bool| -> (String, String) {
        match line {
            Some(l) => (
                l.old_no
                    .filter(|_| old)
                    .or(l.new_no.filter(|_| !old))
                    .unwrap_or_default()
                    .to_string(),
                l.text.clone(),
            ),
            None => (none.clone(), none.clone()),
        }
    };
    let (old_no, old_text) = describe(left, true);
    let (new_no, new_text) = describe(right, false);
    let label = tr_args(
        "diff-split-a11y",
        &[
            ("old", &old_no),
            ("oldtext", &old_text),
            ("new", &new_no),
            ("newtext", &new_text),
        ],
    );
    h_flex()
        .id(ROW_ID)
        .test_support()
        .aria_label(SharedString::from(label))
        .size_full()
        .overflow_hidden()
        .text_sm()
        .font_family(mono.clone())
        .text_color(colors.foreground)
        .child(cell(left, true, gutter, colors))
        .child(div().w(px(1.)).h_full().flex_none().bg(colors.border))
        .child(cell(right, false, gutter, colors))
}

fn button(
    id: &'static str,
    label: String,
    a11y: String,
    active: bool,
    selected: Hsla,
    listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .test_support()
        .aria_label(SharedString::from(a11y))
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_xs()
        .when(active, |el| {
            el.bg(selected).font_weight(FontWeight::SEMIBOLD)
        })
        .on_click(listener)
        .child(label)
}

impl Render for DiffView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let region = tr_args("diff-region-a11y", &[("path", &self.model.path)]);
        let notice = self.model.notice.as_ref().map(|n| n.message());
        let toolbar = self.render_toolbar(cx);
        let muted = cx.theme().muted_foreground;
        v_flex()
            .id(PANE_ID)
            .test_support()
            .aria_label(SharedString::from(region))
            .relative()
            .size_full()
            .child(toolbar)
            .child(match notice {
                Some(message) => div()
                    .id(NOTICE_ID)
                    .test_support()
                    .aria_label(SharedString::from(message.clone()))
                    .flex_1()
                    .p_4()
                    .text_sm()
                    .text_color(muted)
                    .child(message)
                    .into_any_element(),
                None => div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(
                        v_flex()
                            .id("diff-scroll")
                            .relative()
                            .size_full()
                            .child(
                                v_virtual_list(
                                    cx.entity().clone(),
                                    "diff-list",
                                    self.item_sizes.clone(),
                                    |view, visible_range, _, cx| {
                                        visible_range.map(|ix| view.render_row(ix, cx)).collect()
                                    },
                                )
                                .track_scroll(&self.scroll_handle),
                            )
                            .scrollbar(&self.scroll_handle, ScrollbarAxis::Vertical),
                    )
                    .into_any_element(),
            })
    }
}

fn h_flex() -> Div {
    div().flex().flex_row()
}
