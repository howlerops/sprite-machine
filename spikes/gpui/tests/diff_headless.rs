//! Headless render of the diff pane over the 5,000-line `diff-5k` fixture: the diff is virtualized
//! (only the viewport's rows are laid out), its first line is visible, and it scrolls.
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AppContext, Bounds, Point, ScrollDelta, TestAppContext, WindowBounds, WindowOptions, point, px,
    size,
};
use gpui_spike::diff_pane::{LINE_ID, PANE_ID, diff5k_files, open_diff_pane};

/// Lines of the fixture file.
const TOTAL_LINES: usize = 5_000;
/// Generous bound on source rows laid out for an 800x600 window (~30 rows fit, plus overdraw).
const MAX_LAID_OUT: usize = 200;
/// How many rows the test wheel-scrolls down by.
const SCROLL_ROWS: usize = 1_000;

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

/// The source text of fixture line `number` (1-based).
fn generated_line(number: usize) -> String {
    format!(
        "// generated line {number:04}: lorem ipsum dolor sit amet, consectetur adipiscing elit"
    )
}

/// The 1-based line number of a source label produced by [`generated_line`].
fn line_number(label: &str) -> Option<usize> {
    label
        .strip_prefix("// generated line ")?
        .split(':')
        .next()?
        .parse()
        .ok()
}

#[gpui_kit::test]
fn diff_pane_virtualizes_and_scrolls_the_5k_line_diff(cx: &mut TestAppContext) {
    let files = diff5k_files().expect("diff-5k fixture parses");
    assert_eq!(files[0].additions(), TOTAL_LINES);

    cx.update(gpui_kit::init);
    let (window, pane) =
        cx.update(|cx| open_diff_pane(options_800x600(), files, cx).expect("open test window"));

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);

        assert!(window.find(PANE_ID).visible());

        // Only the viewport's rows are laid out, not the 5,000 lines of the file.
        let laid_out = window.find_all(LINE_ID);
        eprintln!("laid out {} of {TOTAL_LINES} source rows", laid_out.len());
        assert!(
            !laid_out.is_empty() && laid_out.len() <= MAX_LAID_OUT,
            "laid out {} of {TOTAL_LINES} source rows",
            laid_out.len()
        );

        // The first line of the file (the first hunk's first added line) is at the top.
        let first = laid_out
            .iter()
            .find(|row| row.visible())
            .expect("a visible row");
        assert_eq!(first.label(), Some(generated_line(1).as_str()));
        // Rows past the viewport are not built at all.
        assert!(
            laid_out
                .iter()
                .all(|row| row.label() != Some(generated_line(TOTAL_LINES).as_str()))
        );

        // Row pitch from two consecutive laid-out rows, then wheel-scroll down SCROLL_ROWS rows.
        let pitch = laid_out[1].bounds().origin.y - laid_out[0].bounds().origin.y;
        assert!(pitch > px(0.), "row pitch {pitch:?}");
        let delta = -(pitch * SCROLL_ROWS as f32);
        window.scroll(PANE_ID, ScrollDelta::Pixels(point(px(0.), delta)), cx);
        window.render_frame(cx);

        let laid_out = window.find_all(LINE_ID);
        assert!(
            !laid_out.is_empty() && laid_out.len() <= MAX_LAID_OUT,
            "laid out {} source rows after scrolling",
            laid_out.len()
        );
        let first = laid_out
            .iter()
            .find(|row| row.visible())
            .expect("a visible row after scrolling");
        let top = line_number(first.label().expect("labelled row")).expect("a fixture line");
        assert!(
            (SCROLL_ROWS - 50..=SCROLL_ROWS + 50).contains(&top),
            "top visible line after scrolling is {top}"
        );
        assert!(
            laid_out
                .iter()
                .all(|row| row.label() != Some(generated_line(1).as_str())),
            "the first line is no longer laid out"
        );
    })
    .expect("window is open");

    cx.update(|cx| {
        let state = pane.read(cx).state().clone();
        assert_eq!(state.read(cx).files().len(), 1);
        assert_eq!(state.read(cx).files()[0].additions(), TOTAL_LINES);
    });
}
