//! The app shell: a title bar area, a left sidebar placeholder and the transcript pane.

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::transcript_model::{Row, build_rows};
use crate::transcript_pane::TranscriptPane;

/// Window title / title bar text.
pub const TITLE: &str = "Sprite Machine — transcript spike";
/// Thread id the fixture log is generated for.
pub const FIXTURE_THREAD: &str = "th_spike";

pub struct AppShell {
    pane: Entity<TranscriptPane>,
}

impl AppShell {
    pub fn new(rows: Vec<Row>, cx: &mut Context<Self>) -> Self {
        Self {
            pane: cx.new(|_| TranscriptPane::new(rows)),
        }
    }

    pub fn pane(&self) -> &Entity<TranscriptPane> {
        &self.pane
    }
}

/// The rows of the whole `transcript-10k` fixture log, generated in memory.
pub fn fixture_rows() -> Vec<Row> {
    let log = agui_fixture::complete_log(agui_fixture::Scenario::Transcript10k, FIXTURE_THREAD)
        .expect("transcript-10k fixture log");
    build_rows(&log)
}

/// Opens the shell window over `rows` (the window must be opened via `gpui_kit::open_window`
/// so it gets the `Root` wrapper). Call `gpui_kit::init` first.
pub fn open_shell(
    options: WindowOptions,
    rows: Vec<Row>,
    cx: &mut App,
) -> Result<(AnyWindowHandle, Entity<AppShell>)> {
    gpui_kit::open_window(options, cx, |_, cx| cx.new(|cx| AppShell::new(rows, cx)))
}

impl Render for AppShell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .id("app-shell")
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                h_flex()
                    .id("title-bar")
                    .test_support()
                    .aria_label(TITLE)
                    .h(px(36.))
                    .flex_none()
                    .px_3()
                    .bg(theme.title_bar)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(TITLE),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .id("sidebar")
                            .test_support()
                            .aria_label("Threads")
                            .w(px(220.))
                            .h_full()
                            .flex_none()
                            .p_3()
                            .gap_2()
                            .bg(theme.sidebar)
                            .border_r_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child("Threads"),
                            )
                            .child(div().text_sm().child("Synthetic transcript")),
                    )
                    .child(div().flex_1().h_full().min_w_0().child(self.pane.clone())),
            )
    }
}
