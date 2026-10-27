//! Proves the headless test pattern from CONVENTIONS.md §8 against the pinned gpui-kit:
//! open a real (test-platform) window, click a button, read the status back.
use gpui_kit::component::*;
use gpui_kit::test::{TestSupportExt, TestWindowExt};
use gpui_kit::{
    AppContext, Bounds, Context, Point, Role, SharedString, TestAppContext, Window, WindowBounds,
    WindowOptions, component::button::Button, div, prelude::*, px, size,
};

struct Counter {
    clicks: usize,
}

impl Render for Counter {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = SharedString::from(format!("Clicked {} times", self.clicks));
        div()
            .size_full()
            .v_flex()
            .gap_2()
            .child(
                Button::new("bump")
                    .label("Bump")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.clicks += 1;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("status")
                    .role(Role::Status)
                    .test_support()
                    .aria_label(status.clone())
                    .child(status),
            )
    }
}

#[gpui_kit::test]
fn clicking_the_button_updates_the_status(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (window, counter) = cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(400.), px(300.)),
        };
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |_, cx| cx.new(|_| Counter { clicks: 0 }),
        )
        .expect("open test window")
    });

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("status").label(), Some("Clicked 0 times"));
        window.click("bump", cx);
        window.click("bump", cx);
        window.render_frame(cx);
        assert_eq!(window.find("status").label(), Some("Clicked 2 times"));
    })
    .expect("window is open");
    cx.update(|cx| assert_eq!(counter.read(cx).clicks, 2));
}
