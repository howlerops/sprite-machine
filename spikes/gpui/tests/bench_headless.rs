//! Headless checks for the bench hooks: the composer has focus at startup, the window title is
//! `gpui-spike`, a key delivered to the composer is logged as `key_to_glyph` once the frame that
//! paints it is presented, and the transcript pane folds envelopes the way `build_rows` does.
use std::path::Path;
use std::rc::Rc;

use agui_fixture::{Scenario, complete_log};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AppContext, TestAppContext};
use gpui_spike::bench::BenchLog;
use gpui_spike::composer::COMPOSER_ID;
use gpui_spike::transcript_model::build_rows;
use gpui_spike::transcript_pane::TranscriptPane;
use gpui_spike::transcript_shell::{WINDOW_TITLE, open_shell_with, spike_window_options};

#[gpui_kit::test]
fn bench_composer_has_keyboard_focus_and_the_window_is_titled_gpui_spike(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let title = spike_window_options()
        .titlebar
        .and_then(|titlebar| titlebar.title)
        .expect("window has a title");
    assert_eq!(&*title, WINDOW_TITLE);
    assert_eq!(WINDOW_TITLE, "gpui-spike");

    let (window, shell) = cx.update(|cx| {
        open_shell_with(spike_window_options(), Vec::new(), None, None, cx)
            .expect("open test window")
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let focus = shell.read(cx).composer().read(cx).focus_handle().clone();
        assert!(
            focus.is_focused(window),
            "the composer is focused at startup"
        );
        assert!(window.find(COMPOSER_ID).visible());
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn bench_keys_reach_the_composer_and_are_logged_once_painted(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("bench_headless_keys.jsonl");
    let _ = std::fs::remove_file(&path);
    let bench = Rc::new(BenchLog::open(&path).expect("open bench log"));
    let (window, shell) = cx.update(|cx| {
        open_shell_with(spike_window_options(), Vec::new(), None, Some(bench), cx)
            .expect("open test window")
    });

    cx.update_window(window, |_, window, cx| {
        window.input("ls", cx);
        window.press("backspace", cx);
        window.render_frame(cx);
        // The frame callbacks run after the frames that scheduled them are presented.
        window.simulate_next_frame(cx);
        assert_eq!(shell.read(cx).composer().read(cx).text(), "l");
    })
    .expect("window is open");

    let text = std::fs::read_to_string(&path).expect("bench log was written");
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("each bench line is JSON"))
        .collect();
    let keys: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|line| line["kind"] == "key_to_glyph")
        .collect();
    let ids: Vec<Option<u64>> = keys.iter().map(|key| key["id"].as_u64()).collect();
    assert_eq!(ids, vec![Some(1), Some(2), Some(3)], "one record per key");
    for key in &keys {
        let key_us = key["key_us"].as_u64().expect("key_us");
        let glyph_us = key["glyph_us"].as_u64().expect("glyph_us");
        assert!(key_us <= glyph_us, "the glyph is presented after the key");
    }
    let frames: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|line| line["kind"] == "frame")
        .collect();
    assert!(!frames.is_empty(), "at least one frame record");
    assert_eq!(frames[0]["dt_us"], 0, "the first frame has no interval");
}

#[gpui_kit::test]
fn bench_pane_apply_folds_envelopes_like_build_rows(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let log = complete_log(Scenario::Transcript10k, "th_fixture").expect("fixture log");
    let pane = cx.update(|cx| cx.new(|_| TranscriptPane::new(Vec::new(), None)));
    let (head, tail) = log.split_at(log.len() / 2);
    cx.update(|cx| pane.update(cx, |pane, cx| pane.apply(head, cx)));
    cx.update(|cx| pane.update(cx, |pane, cx| pane.apply(tail, cx)));
    cx.update(|cx| assert_eq!(pane.read(cx).rows(), build_rows(&log).as_slice()));
}
