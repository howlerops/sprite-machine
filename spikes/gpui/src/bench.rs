//! Bench hooks for `spikes/bench/run-desktop.sh` (the instrumentation contract in its header).
//!
//! - `BENCH_MARKER first_interactive_frame unix_us=<n>` on stdout, once per run, when the shell is
//!   first interactive (transcript visible, composer focused) and that frame is presented.
//! - With `GPUI_SPIKE_BENCH_LOG` set, JSON lines are appended to that file: one `frame` line per
//!   presented frame and one `key_to_glyph` line per key delivered to the composer.
//!
//! Times are microseconds since the Unix epoch (CLOCK_REALTIME). The line formats are plain
//! functions so they can be tested without a window.
//!
//! Presentation is observed through [`gpui_kit::Window::on_next_frame`]: GPUI runs those
//! callbacks at the start of the next frame loop tick, before that tick draws, so a callback
//! scheduled while rendering runs after the frame it rendered has been presented.

use std::cell::{Cell, RefCell};
use std::fs::{File, OpenOptions};
use std::io::{Write, stdout};
use std::path::Path;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::Window;

/// The environment variable naming the JSON lines file.
pub const BENCH_LOG_ENV: &str = "GPUI_SPIKE_BENCH_LOG";

/// Microseconds since the Unix epoch.
pub fn unix_micros(now: SystemTime) -> u64 {
    u64::try_from(
        now.duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros(),
    )
    .unwrap_or(u64::MAX)
}

/// The first-interactive marker line (without the newline).
pub fn marker_line(unix_us: u64) -> String {
    format!("BENCH_MARKER first_interactive_frame unix_us={unix_us}")
}

/// One `frame` JSON line (without the newline).
pub fn frame_line(t_us: u64, dt_us: u64) -> String {
    format!(r#"{{"kind":"frame","t_us":{t_us},"dt_us":{dt_us}}}"#)
}

/// One `key_to_glyph` JSON line (without the newline).
pub fn key_to_glyph_line(id: u64, key_us: u64, glyph_us: u64) -> String {
    format!(r#"{{"kind":"key_to_glyph","id":{id},"key_us":{key_us},"glyph_us":{glyph_us}}}"#)
}

/// Time between two presented frames; `0` for the first frame.
pub fn frame_interval(previous_us: Option<u64>, now_us: u64) -> u64 {
    previous_us.map_or(0, |previous| now_us.saturating_sub(previous))
}

/// Prints the first-interactive marker on stdout (flushed: the wrapper waits for it).
pub fn print_marker(unix_us: u64) {
    let mut out = stdout().lock();
    writeln!(out, "{}", marker_line(unix_us)).expect("write BENCH_MARKER");
    out.flush().expect("flush BENCH_MARKER");
}

/// A key delivered to the composer whose glyph has not been presented yet: `(id, key_us)`.
pub type PendingKey = (u64, u64);

/// The JSON lines file, and the per-window state for the frame hook.
pub struct BenchLog {
    file: RefCell<File>,
    last_frame_us: Cell<Option<u64>>,
    frame_scheduled: Cell<bool>,
}

impl BenchLog {
    /// Opens `path` for appending.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: RefCell::new(file),
            last_frame_us: Cell::new(None),
            frame_scheduled: Cell::new(false),
        })
    }

    /// The log named by `GPUI_SPIKE_BENCH_LOG`, if it is set.
    pub fn from_env() -> Option<Rc<Self>> {
        let path = std::env::var_os(BENCH_LOG_ENV)?;
        let log = Self::open(Path::new(&path))
            .unwrap_or_else(|err| panic!("open {BENCH_LOG_ENV} {path:?}: {err}"));
        Some(Rc::new(log))
    }

    /// Called from a view's render. Schedules one `frame` record for the frame being drawn
    /// (at most one per frame, however many views render in it).
    pub fn note_render(self: &Rc<Self>, window: &Window) {
        if self.frame_scheduled.replace(true) {
            return;
        }
        let log = Rc::clone(self);
        window.on_next_frame(move |_, _| {
            log.frame_scheduled.set(false);
            log.frame_presented(unix_micros(SystemTime::now()));
        });
    }

    /// Schedules the `key_to_glyph` records for `keys`, which this frame paints.
    pub fn keys_painted(self: &Rc<Self>, window: &Window, keys: Vec<PendingKey>) {
        if keys.is_empty() {
            return;
        }
        let log = Rc::clone(self);
        window.on_next_frame(move |_, _| {
            let glyph_us = unix_micros(SystemTime::now());
            for (id, key_us) in keys {
                log.write_line(&key_to_glyph_line(id, key_us, glyph_us));
            }
        });
    }

    fn frame_presented(&self, t_us: u64) {
        let dt_us = frame_interval(self.last_frame_us.get(), t_us);
        self.last_frame_us.set(Some(t_us));
        self.write_line(&frame_line(t_us, dt_us));
    }

    /// One unbuffered write per line, so a killed run keeps every line it produced.
    fn write_line(&self, line: &str) {
        let mut file = self.file.borrow_mut();
        if let Err(err) = file.write_all(format!("{line}\n").as_bytes()) {
            eprintln!("{BENCH_LOG_ENV}: write failed: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{frame_interval, frame_line, key_to_glyph_line, marker_line, unix_micros};
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn bench_marker_line_is_the_contract_line() {
        assert_eq!(
            marker_line(1_700_000_000_123_456),
            "BENCH_MARKER first_interactive_frame unix_us=1700000000123456"
        );
    }

    #[test]
    fn bench_frame_line_is_json_with_the_contract_fields() {
        let line = frame_line(1_000_016, 16_667);
        assert_eq!(line, r#"{"kind":"frame","t_us":1000016,"dt_us":16667}"#);
        let value: serde_json::Value = serde_json::from_str(&line).expect("frame line is JSON");
        assert_eq!(value["kind"], "frame");
        assert_eq!(value["t_us"], 1_000_016);
        assert_eq!(value["dt_us"], 16_667);
    }

    #[test]
    fn bench_key_to_glyph_line_is_json_with_the_contract_fields() {
        let line = key_to_glyph_line(7, 2_000_000, 2_012_345);
        assert_eq!(
            line,
            r#"{"kind":"key_to_glyph","id":7,"key_us":2000000,"glyph_us":2012345}"#
        );
        let value: serde_json::Value = serde_json::from_str(&line).expect("key line is JSON");
        assert_eq!(value["kind"], "key_to_glyph");
        assert_eq!(value["id"], 7);
        assert_eq!(value["glyph_us"], 2_012_345);
    }

    #[test]
    fn bench_frame_interval_is_zero_for_the_first_frame_then_the_gap() {
        assert_eq!(frame_interval(None, 500), 0);
        assert_eq!(frame_interval(Some(500), 16_500), 16_000);
        // A clock step backwards never produces a huge unsigned interval.
        assert_eq!(frame_interval(Some(500), 400), 0);
    }

    #[test]
    fn bench_unix_micros_counts_from_the_epoch() {
        let t = UNIX_EPOCH + Duration::from_micros(1_234_567);
        assert_eq!(unix_micros(t), 1_234_567);
    }
}
