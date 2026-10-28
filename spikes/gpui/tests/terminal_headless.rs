//! Headless terminal pane: the `terminal` fixture's output renders as rows (a known line is
//! visible by id and label, a red SGR span is in the model), and key presses on the focused pane
//! reach the input sink as the bytes a terminal sends.
use std::sync::mpsc;

use gpui_kit::test::TestWindowExt;
use gpui_kit::{AppContext, Bounds, Point, TestAppContext, WindowBounds, WindowOptions, px, size};
use gpui_spike::terminal_model::ANSI_RED;
use gpui_spike::terminal_pane::{
    LINE_ID, PANE_ID, extra_test_output, fixture_output_chunks, open_terminal_pane,
};

/// The last line the fixture prints; visible at the bottom of the grid.
const KNOWN_LINE: &str = "test case_300 ... ok ✓ (300 ms)";

fn options_800x600() -> WindowOptions {
    let bounds = Bounds {
        origin: Point::default(),
        size: size(px(800.), px(600.)),
    };
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        ..Default::default()
    }
}

#[gpui_kit::test]
fn terminal_pane_renders_fixture_output_with_a_red_span(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let mut output = fixture_output_chunks().expect("terminal fixture");
    output.push(extra_test_output());
    let (input, _keys) = mpsc::channel();
    let (window, pane) = cx.update(|cx| {
        open_terminal_pane(options_800x600(), output, input, cx).expect("open test window")
    });

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);

        assert!(window.find(PANE_ID).visible());
        let lines = window.find_all(LINE_ID);
        assert!(
            lines
                .iter()
                .any(|line| line.visible() && line.label() == Some(KNOWN_LINE)),
            "the fixture's last line is visible"
        );
        let error = lines
            .iter()
            .find(|line| line.label() == Some("error: red span"))
            .expect("the red test line is laid out");
        assert!(error.visible());
    })
    .expect("window is open");

    cx.update(|cx| {
        let rows = pane.read(cx).model().rows();
        assert!(
            rows.iter().any(|row| {
                row.runs
                    .iter()
                    .any(|run| run.text == "error: red span" && run.fg == Some(ANSI_RED))
            }),
            "a red SGR span is in the model"
        );
    });
}

#[gpui_kit::test]
fn terminal_pane_forwards_key_presses_to_the_sink(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let output = fixture_output_chunks().expect("terminal fixture");
    let (input, keys) = mpsc::channel();
    let (window, pane) = cx.update(|cx| {
        open_terminal_pane(options_800x600(), output, input, cx).expect("open test window")
    });

    cx.update_window(window, |_, window, cx| {
        let focus = pane.read(cx).focus_handle().clone();
        window.focus(&focus, cx);
        window.render_frame(cx);
        window.input("ls", cx);
        window.press("enter", cx);
        window.press("up", cx);
        window.press("backspace", cx);
        window.press("ctrl-c", cx);
    })
    .expect("window is open");

    let sent: Vec<Vec<u8>> = keys.try_iter().collect();
    assert_eq!(
        sent,
        vec![
            b"l".to_vec(),
            b"s".to_vec(),
            b"\r".to_vec(),
            b"\x1b[A".to_vec(),
            b"\x7f".to_vec(),
            b"\x03".to_vec(),
        ]
    );
}
