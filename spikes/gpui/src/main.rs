//! Opens the spike shell. By default over the in-memory `transcript-10k` fixture log.
//!
//! Environment (the bench contract in `spikes/bench/run-desktop.sh`):
//! - `GPUI_SPIKE_FIXTURE_ADDR=<host:port>`: render the transcript from the fixture's WebSocket
//!   feed instead of the in-memory rows.
//! - `GPUI_SPIKE_BENCH_LOG=<path>`: append frame and key-to-glyph JSON lines to `<path>`.
//! - `GPUI_SPIKE_WORKLOAD=common`: also render the common workload's panes (a 5k-line diff, a
//!   terminal receiving output, one approval card) beside the transcript.

use gpui_spike::bench::BenchLog;
use gpui_spike::common_workload::{ApprovalFixture, CommonInputs};
use gpui_spike::feed::spawn_feed;
use gpui_spike::transcript_shell::{fixture_rows, open_shell_in, spike_window_options};
use gpui_spike::workload::Workload;

const FIXTURE_ADDR_ENV: &str = "GPUI_SPIKE_FIXTURE_ADDR";

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("error: {message}");
    std::process::exit(2)
}

fn main() {
    let workload = Workload::from_env().unwrap_or_else(|err| fail(err));
    let feed = std::env::var(FIXTURE_ADDR_ENV)
        .ok()
        .map(|addr| spawn_feed(&addr));
    let rows = if feed.is_some() {
        Vec::new()
    } else {
        fixture_rows()
    };
    // The approval fixture serves the common workload's approval; it must outlive the window, so
    // it lives until `main` returns, after `run` has returned.
    let approval = match workload {
        Workload::Transcript => None,
        Workload::Common => Some(ApprovalFixture::start().unwrap_or_else(|err| fail(err))),
    };
    let common = approval
        .as_ref()
        .map(|fixture| CommonInputs::from_fixtures(fixture).unwrap_or_else(|err| fail(err)));
    gpui_kit::application().run(move |cx| {
        // This must be called before using any GPUI Component features.
        gpui_kit::init(cx);
        open_shell_in(
            spike_window_options(),
            rows,
            feed,
            BenchLog::from_env(),
            common,
            cx,
        )
        .expect("open main window");
    });
}
