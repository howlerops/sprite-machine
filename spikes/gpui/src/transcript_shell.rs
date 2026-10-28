//! The app shell: a title bar area, a left sidebar placeholder, the transcript pane and the
//! composer under it. In the common workload (M1B-008b) a column beside the transcript holds the
//! diff pane, the terminal pane and the approval card.

use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, SystemTime};

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use protocol::turn_event::Envelope;

use crate::approval_pane::ApprovalCard;
use crate::bench::{BenchLog, print_marker, unix_micros};
use crate::common_workload::{CardClock, CommonInputs};
use crate::composer::Composer;
use crate::diff_pane::DiffPane;
use crate::feed::drain;
use crate::terminal_pane::TerminalPane;
use crate::transcript_model::{Row, build_rows};
use crate::transcript_pane::TranscriptPane;

/// Title bar text (in the app).
pub const TITLE: &str = "Sprite Machine — transcript spike";
/// The OS window title; the bench driver finds the window by it.
pub const WINDOW_TITLE: &str = "gpui-spike";
/// Thread id the fixture log is generated for.
pub const FIXTURE_THREAD: &str = "th_spike";
/// How often the UI thread drains the fixture feed and moves the approval card's clock.
const FEED_POLL: Duration = Duration::from_millis(16);
/// Width of the column of common-workload panes beside the transcript.
pub const COMMON_COLUMN_WIDTH: f32 = 480.;
/// Height of the terminal pane in the common workload.
const TERMINAL_HEIGHT: f32 = 220.;
/// Pause between two terminal output chunks; one pass of the `terminal` fixture takes a few seconds.
const TERMINAL_CHUNK_INTERVAL: Duration = Duration::from_millis(200);

/// The panes of the common workload, beside the transcript.
pub struct CommonPanes {
    diff: Entity<DiffPane>,
    terminal: Entity<TerminalPane>,
    approval: Entity<ApprovalCard>,
    /// Keys typed in the terminal pane. The spike has no child process to send them to; keeping
    /// the receiver alive means the pane's sends succeed and queue here.
    _keys: Receiver<Vec<u8>>,
}

pub struct AppShell {
    pane: Entity<TranscriptPane>,
    composer: Entity<Composer>,
    bench: Option<Rc<BenchLog>>,
    marker_printed: bool,
    common: Option<CommonPanes>,
}

impl AppShell {
    /// `feed`, when given, appends the envelopes it carries to the transcript as they arrive;
    /// the pane starts from `rows` (empty for a feed). `common`, when given, adds the common
    /// workload's panes beside the transcript.
    pub fn new(
        rows: Vec<Row>,
        feed: Option<Receiver<Envelope>>,
        bench: Option<Rc<BenchLog>>,
        common: Option<CommonInputs>,
        cx: &mut Context<Self>,
    ) -> Self {
        let pane = cx.new(|_| TranscriptPane::new(rows, bench.clone()));
        let composer = cx.new(|cx| Composer::new(bench.clone(), cx));
        let mut card_clock = None;
        let common = common.map(|inputs| {
            let CommonInputs {
                diff_files,
                terminal_output,
                approval,
                sink,
                approval_clock_start_ms,
            } = inputs;
            let (keys, keys_received) = mpsc::channel();
            let diff = cx.new(|cx| DiffPane::new(diff_files, cx));
            let terminal = cx.new(|cx| {
                TerminalPane::new(Vec::<Vec<u8>>::new(), keys, cx).with_bench(bench.clone())
            });
            let approval = cx.new(|_| {
                ApprovalCard::new(approval, sink, approval_clock_start_ms).with_bench(bench.clone())
            });
            stream_terminal(terminal.clone(), terminal_output, cx);
            card_clock = Some((approval.clone(), CardClock::new(approval_clock_start_ms)));
            CommonPanes {
                diff,
                terminal,
                approval,
                _keys: keys_received,
            }
        });
        if feed.is_some() || card_clock.is_some() {
            let pane = pane.clone();
            let mut feed = feed;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(FEED_POLL).await;
                    if let Some(receiver) = &feed {
                        let (batch, ended) = drain(receiver);
                        if !batch.is_empty() {
                            pane.update(cx, |pane, cx| pane.apply(&batch, cx));
                        }
                        if ended {
                            feed = None;
                        }
                    }
                    if let Some((card, clock)) = card_clock.as_mut()
                        && let Some(now) = clock.changed()
                    {
                        card.update(cx, |card, cx| card.set_now(now, cx));
                    }
                    // Stop once the window (and so this shell) is gone, or nothing is left to
                    // drive: the feed has ended and there is no approval clock.
                    if this.upgrade().is_none() || (feed.is_none() && card_clock.is_none()) {
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
            common,
        }
    }

    pub fn pane(&self) -> &Entity<TranscriptPane> {
        &self.pane
    }

    pub fn composer(&self) -> &Entity<Composer> {
        &self.composer
    }

    /// The common workload's panes, when the window has them.
    pub fn common(&self) -> Option<&CommonPanes> {
        self.common.as_ref()
    }
}

impl CommonPanes {
    pub fn diff(&self) -> &Entity<DiffPane> {
        &self.diff
    }

    pub fn terminal(&self) -> &Entity<TerminalPane> {
        &self.terminal
    }

    pub fn approval(&self) -> &Entity<ApprovalCard> {
        &self.approval
    }
}

/// Feeds the terminal's output chunks one by one, [`TERMINAL_CHUNK_INTERVAL`] apart, and then
/// starts the output again from its first chunk, so the pane keeps receiving output until the app
/// exits (a live terminal; the bench run is longer than one pass of the fixture).
fn stream_terminal(
    terminal: Entity<TerminalPane>,
    output: Vec<Vec<u8>>,
    cx: &mut Context<AppShell>,
) {
    let mut output = output.into_iter().cycle();
    cx.spawn(async move |_, cx| {
        while let Some(chunk) = output.next() {
            cx.background_executor()
                .timer(TERMINAL_CHUNK_INTERVAL)
                .await;
            terminal.update(cx, |pane, cx| pane.feed(&chunk, cx));
        }
    })
    .detach();
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
    open_shell_in(options, rows, feed, bench, None, cx)
}

/// [`open_shell_with`] plus the common workload's panes when `common` is given.
pub fn open_shell_in(
    options: WindowOptions,
    rows: Vec<Row>,
    feed: Option<Receiver<Envelope>>,
    bench: Option<Rc<BenchLog>>,
    common: Option<CommonInputs>,
    cx: &mut App,
) -> Result<(AnyWindowHandle, Entity<AppShell>)> {
    gpui_kit::open_window(options, cx, |window, cx| {
        let shell = cx.new(|cx| AppShell::new(rows, feed, bench, common, cx));
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
        let transcript = div().flex_1().min_h_0().w_full().child(self.pane.clone());
        let body = match &self.common {
            None => transcript.into_any_element(),
            Some(common) => h_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(transcript.min_w_0().h_full())
                .child(
                    v_flex()
                        .id("common-column")
                        .w(px(COMMON_COLUMN_WIDTH))
                        .flex_none()
                        .h_full()
                        .min_h_0()
                        .gap_2()
                        .p_2()
                        .border_l_1()
                        .border_color(theme.border)
                        .child(div().flex_1().min_h_0().w_full().child(common.diff.clone()))
                        .child(
                            div()
                                .flex_none()
                                .h(px(TERMINAL_HEIGHT))
                                .w_full()
                                .overflow_hidden()
                                .child(common.terminal.clone()),
                        )
                        .child(div().flex_none().w_full().child(common.approval.clone())),
                )
                .into_any_element(),
        };
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
                            .child(body)
                            .child(div().flex_none().p_2().child(self.composer.clone())),
                    ),
            )
    }
}
