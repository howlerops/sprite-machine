//! Opens the spike shell over the in-memory `transcript-10k` fixture log.

use gpui_kit::{Bounds, Point, WindowBounds, WindowOptions, px, size};
use gpui_spike::transcript_shell::{fixture_rows, open_shell};

fn main() {
    let rows = fixture_rows();
    gpui_kit::application().run(move |cx| {
        // This must be called before using any GPUI Component features.
        gpui_kit::init(cx);
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(1280.), px(800.)),
        };
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        };
        open_shell(options, rows, cx).expect("open main window");
    });
}
