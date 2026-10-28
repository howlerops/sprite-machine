//! Headless render of the spike shell over the full `transcript-10k` fixture: the transcript is
//! virtualized (only the viewport's rows are laid out) and scrolls.
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AppContext, Bounds, Point, ScrollDelta, TestAppContext, WindowBounds, WindowOptions, point, px,
    size,
};
use gpui_spike::transcript_pane::{PANE_ID, ROW_HEIGHT, ROW_ID};
use gpui_spike::transcript_shell::{TITLE, fixture_rows, open_shell};

/// Generous bound on rows laid out for a 600 px tall window (~20 rows fit).
const MAX_LAID_OUT: usize = 60;
/// How many rows the test scrolls down by.
const SCROLL_ROWS: usize = 200;

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
fn shell_virtualizes_and_scrolls_the_10k_transcript(cx: &mut TestAppContext) {
    let rows = fixture_rows();
    let total = rows.len();
    assert!(total > 2_000, "fixture folds into {total} rows");
    let labels: Vec<String> = rows.iter().map(|row| row.label()).collect();

    cx.update(gpui_kit::init);
    let (window, shell) =
        cx.update(|cx| open_shell(options_800x600(), rows, cx).expect("open test window"));

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);

        // Shell chrome.
        assert_eq!(window.find("title-bar").label(), Some(TITLE));
        assert!(window.find("sidebar").visible());
        assert!(window.find(PANE_ID).visible());

        // Only the viewport's rows are laid out, not 2.5k rows / 10k events.
        let laid_out = window.find_all(ROW_ID);
        eprintln!("laid out {} of {total} rows", laid_out.len());
        assert!(
            !laid_out.is_empty() && laid_out.len() <= MAX_LAID_OUT,
            "laid out {} of {total} rows",
            laid_out.len()
        );
        let first = laid_out
            .iter()
            .find(|row| row.visible())
            .expect("a visible row");
        assert_eq!(first.label(), Some(labels[0].as_str()));
        assert_eq!(first.label(), Some("You: Work on the router (task 1)."));
        // Rows past the viewport are not built at all.
        assert!(
            laid_out
                .iter()
                .all(|row| row.label() != Some(labels[SCROLL_ROWS].as_str()))
        );

        // Wheel-scroll the pane down by SCROLL_ROWS rows.
        let delta = -(ROW_HEIGHT * SCROLL_ROWS as f32);
        window.scroll(PANE_ID, ScrollDelta::Pixels(point(px(0.), delta)), cx);
        window.render_frame(cx);

        let laid_out = window.find_all(ROW_ID);
        assert!(
            !laid_out.is_empty() && laid_out.len() <= MAX_LAID_OUT,
            "laid out {} rows after scrolling",
            laid_out.len()
        );
        let first = laid_out
            .iter()
            .find(|row| row.visible())
            .expect("a visible row after scrolling");
        assert_eq!(first.label(), Some(labels[SCROLL_ROWS].as_str()));
        assert!(
            laid_out
                .iter()
                .all(|row| row.label() != Some(labels[0].as_str())),
            "the first row is no longer laid out"
        );
    })
    .expect("window is open");

    cx.update(|cx| {
        let pane = shell.read(cx).pane().clone();
        assert_eq!(pane.read(cx).rows().len(), total);
    });
}
