//! Opens the spike shell. By default over the in-memory `transcript-10k` fixture log.
//!
//! Environment (the bench contract in `spikes/bench/run-desktop.sh`):
//! - `GPUI_SPIKE_FIXTURE_ADDR=<host:port>`: render the transcript from the fixture's WebSocket
//!   feed instead of the in-memory rows.
//! - `GPUI_SPIKE_BENCH_LOG=<path>`: append frame and key-to-glyph JSON lines to `<path>`.

use gpui_spike::bench::BenchLog;
use gpui_spike::feed::spawn_feed;
use gpui_spike::transcript_shell::{fixture_rows, open_shell_with, spike_window_options};

const FIXTURE_ADDR_ENV: &str = "GPUI_SPIKE_FIXTURE_ADDR";

fn main() {
    let feed = std::env::var(FIXTURE_ADDR_ENV)
        .ok()
        .map(|addr| spawn_feed(&addr));
    let rows = if feed.is_some() {
        Vec::new()
    } else {
        fixture_rows()
    };
    gpui_kit::application().run(move |cx| {
        // This must be called before using any GPUI Component features.
        gpui_kit::init(cx);
        open_shell_with(spike_window_options(), rows, feed, BenchLog::from_env(), cx)
            .expect("open main window");
    });
}
