//! The transcript pane: a `v_virtual_list` over the rows of [`crate::transcript_model`].
//!
//! Only the rows in the viewport are built each frame. Every row is a fixed-height single line
//! (`ROW_HEIGHT`), so the list knows all item sizes up front without measuring.

use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, VirtualListScrollHandle, h_flex,
    scroll::{ScrollableElement, ScrollbarAxis},
    v_flex, v_virtual_list,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::transcript_model::{Row, ToolStatus};

/// Height of one transcript row.
pub const ROW_HEIGHT: Pixels = px(28.);
/// Element id of the pane (scroll target in tests).
pub const PANE_ID: &str = "transcript";
/// Element id carried by every laid-out row's label element.
pub const ROW_ID: &str = "transcript-row";

pub struct TranscriptPane {
    rows: Vec<Row>,
    item_sizes: Rc<Vec<Size<Pixels>>>,
    scroll_handle: VirtualListScrollHandle,
}

impl TranscriptPane {
    pub fn new(rows: Vec<Row>) -> Self {
        let item_sizes = Rc::new(vec![size(px(100.), ROW_HEIGHT); rows.len()]);
        Self {
            rows,
            item_sizes,
            scroll_handle: VirtualListScrollHandle::new(),
        }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    fn render_row(&self, ix: usize, cx: &App) -> impl IntoElement + use<> {
        let row = &self.rows[ix];
        let label = SharedString::from(row.label());
        let theme = cx.theme();
        let color = match row {
            Row::Reasoning { .. } | Row::Usage { .. } => theme.muted_foreground,
            Row::ToolCall {
                status: ToolStatus::Failed,
                ..
            } => theme.danger,
            Row::ToolCall {
                status: ToolStatus::Completed,
                ..
            } => theme.success,
            _ => theme.foreground,
        };
        div().id(("row", ix)).w_full().h(ROW_HEIGHT).child(
            h_flex()
                .id(ROW_ID)
                .test_support()
                .aria_label(label.clone())
                .size_full()
                .px_3()
                .text_sm()
                .text_color(color)
                .when(matches!(row, Row::User { .. }), |el| {
                    el.bg(theme.secondary).font_weight(FontWeight::SEMIBOLD)
                })
                .when(
                    matches!(row, Row::ToolCall { .. } | Row::Plan { .. }),
                    |el| el.pl_6(),
                )
                .child(div().truncate().child(label)),
        )
    }
}

impl Render for TranscriptPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(PANE_ID)
            .test_support()
            .relative()
            .size_full()
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
            )
    }
}
