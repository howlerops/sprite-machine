//! The app shell: a title bar area, a left sidebar placeholder, the transcript pane and the
//! composer under it.

use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, SystemTime};

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use protocol::turn_event::Envelope;

use crate::bench::{BenchLog, print_marker, unix_micros};
use crate::composer::Composer;
use crate::feed::drain;
use crate::transcript_model::{Row, build_rows};
use crate::transcript_pane::TranscriptPane;

/// Title bar text (in the app).
pub const TITLE: &str = "Sprite Machine — transcript spike";
/// The OS window title; the bench driver finds the window by it.
pub const WINDOW_TITLE: &str = "gpui-spike";
/// Thread id the fixture log is generated for.
pub const FIXTURE_THREAD: &str = "th_spike";
/// How often the UI thread drains the fixture feed.
const FEED_POLL: Duration = Duration::from_millis(16);

pub struct AppShell {
    pane: Entity<TranscriptPane>,
    composer: Entity<Composer>,
    bench: Option<Rc<BenchLog>>,
    marker_printed: bool,
}

impl AppShell {
    /// `feed`, when given, appends the envelopes it carries to the transcript as they arrive;
    /// the pane starts from `rows` (empty for a feed).
    pub fn new(
        rows: Vec<Row>,
        feed: Option<Receiver<Envelope>>,
        bench: Option<Rc<BenchLog>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let pane = cx.new(|_| TranscriptPane::new(rows, bench.clone()));
        let composer = cx.new(|cx| Composer::new(bench.clone(), cx));
        if let Some(feed) = feed {
            let pane = pane.clone();
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(FEED_POLL).await;
                    let (batch, ended) = drain(&feed);
                    if !batch.is_empty() {
                        pane.update(cx, |pane, cx| pane.apply(&batch, cx));
                    }
                    // Stop once the window (and so this shell) is gone, or the feed has ended.
                    if ended || this.upgrade().is_none() {
                        return;
                    }
                }
            })
            .detach();
        }
        Self {
            pane,
            composer,
            bench,
            marker_printed: false,
        }
    }

    pub fn pane(&self) -> &Entity<TranscriptPane> {
        &self.pane
    }

    pub fn composer(&self) -> &Entity<Composer> {
        &self.composer
    }
}

/// The rows of the whole `transcript-10k` fixture log, generated in memory.
pub fn fixture_rows() -> Vec<Row> {
    let log = agui_fixture::complete_log(agui_fixture::Scenario::Transcript10k, FIXTURE_THREAD)
        .expect("transcript-10k fixture log");
    build_rows(&log)
}

/// The spike window: 1280 x 800, titled [`WINDOW_TITLE`].
pub fn spike_window_options() -> WindowOptions {
    let bounds = Bounds {
        origin: Point::default(),
        size: size(px(1280.), px(800.)),
    };
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(WINDOW_TITLE.into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Opens the shell window over `rows` (the window must be opened via `gpui_kit::open_window`
/// so it gets the `Root` wrapper). Call `gpui_kit::init` first.
pub fn open_shell(
    options: WindowOptions,
    rows: Vec<Row>,
    cx: &mut App,
) -> Result<(AnyWindowHandle, Entity<AppShell>)> {
    open_shell_with(options, rows, None, None, cx)
}

/// [`open_shell`] with the fixture feed and the bench hooks. The composer has keyboard focus.
pub fn open_shell_with(
    options: WindowOptions,
    rows: Vec<Row>,
    feed: Option<Receiver<Envelope>>,
    bench: Option<Rc<BenchLog>>,
    cx: &mut App,
) -> Result<(AnyWindowHandle, Entity<AppShell>)> {
    gpui_kit::open_window(options, cx, |window, cx| {
        let shell = cx.new(|cx| AppShell::new(rows, feed, bench, cx));
        let focus = shell.read(cx).composer().read(cx).focus_handle().clone();
        window.focus(&focus, cx);
        shell
    })
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(bench) = &self.bench {
            bench.note_render(window);
        }
        // The first frame with the composer focused is the first interactive frame.
        if !self.marker_printed && self.composer.read(cx).focus_handle().is_focused(window) {
            self.marker_printed = true;
            window.on_next_frame(|_, _| print_marker(unix_micros(SystemTime::now())));
        }
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
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .child(div().flex_1().min_h_0().w_full().child(self.pane.clone()))
                            .child(div().flex_none().p_2().child(self.composer.clone())),
                    ),
            )
    }
}
