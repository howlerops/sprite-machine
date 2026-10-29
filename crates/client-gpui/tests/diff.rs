//! M3-010: the diff viewer. The model is tested natively in `src/diff/model.rs`; here the pane is
//! driven headlessly: unified and split layouts, virtualization of a large diff, collapsed gaps,
//! the "open file" event, and the notices (no newline, binary, too large, identical).

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::Rc;

use client_gpui::diff::view::{
    EXPAND_ALL_ID, GAP_ID, HUNK_ID, NOTICE_ID, OPEN_ID, PANE_ID, ROW_ID, SPLIT_ID, TITLE_ID,
    UNIFIED_ID,
};
use client_gpui::diff::{DiffEvent, DiffInput, DiffView, EN_FTL, Mode, Row};
use client_gpui::shell::i18n::Catalog;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AppContext as _, Bounds, Entity, Point, ScrollDelta, TestAppContext, WindowBounds,
    WindowOptions, point, px, size,
};

fn options(width: f32, height: f32) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: Point::default(),
            size: size(px(width), px(height)),
        })),
        ..Default::default()
    }
}

fn numbered(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

fn open_view(
    cx: &mut TestAppContext,
    input: DiffInput,
    root: Option<&str>,
    window_size: (f32, f32),
) -> (gpui_kit::AnyWindowHandle, Entity<DiffView>) {
    cx.update(gpui_kit::init);
    let root = root.map(PathBuf::from);
    cx.update(|cx| {
        gpui_kit::open_window(options(window_size.0, window_size.1), cx, |_, cx| {
            cx.new(|_| DiffView::new(input, root))
        })
        .expect("open test window")
    })
}

fn labels(window: &mut gpui_kit::Window, id: &'static str) -> Vec<String> {
    window
        .find_all(id)
        .iter()
        .map(|el| el.label().unwrap_or_default().to_owned())
        .collect()
}

fn small_input() -> DiffInput {
    DiffInput::new(
        "src/lib.rs",
        Some("a\nb\nc\n".into()),
        "a\nB\nc\nd\n".into(),
    )
}

// ---- unified and split ----------------------------------------------------------------------

#[gpui_kit::test]
fn unified_then_split_layouts_show_numbered_lines(cx: &mut TestAppContext) {
    let (window, view) = open_view(cx, small_input(), None, (900., 400.));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            labels(window, TITLE_ID),
            vec!["src/lib.rs: 2 added, 1 removed"]
        );
        assert_eq!(
            labels(window, ROW_ID),
            vec![
                "Line 1: a",
                "Removed line 2: b",
                "Added line 2: B",
                "Line 3: c",
                "Added line 4: d",
            ]
        );

        window.click(SPLIT_ID, cx);
        window.render_frame(cx);
        assert_eq!(
            labels(window, ROW_ID),
            vec![
                "Old line 1: a. New line 1: a.",
                "Old line 2: b. New line 2: B.",
                "Old line 3: c. New line 3: c.",
                "Old line (none): (none). New line 4: d.",
            ]
        );

        window.click(UNIFIED_ID, cx);
        window.render_frame(cx);
        assert_eq!(labels(window, ROW_ID).len(), 5);
    })
    .expect("window is open");
    assert_eq!(cx.update(|cx| view.read(cx).mode()), Mode::Unified);
}

// ---- virtualization -------------------------------------------------------------------------

#[gpui_kit::test]
fn a_large_diff_lays_out_only_the_visible_rows(cx: &mut TestAppContext) {
    let old = numbered(30_000);
    let new: String = (1..=30_000)
        .map(|i| {
            if i % 7 == 0 {
                format!("changed {i}\n")
            } else {
                format!("line {i}\n")
            }
        })
        .collect();
    let (window, view) = open_view(
        cx,
        DiffInput::new("big.txt", Some(old), new),
        None,
        (900., 600.),
    );
    let total = cx.update(|cx| view.read(cx).rows().len());
    assert!(total > 30_000, "{total} rows");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let first = labels(window, ROW_ID);
        assert!(
            !first.is_empty() && first.len() <= 60,
            "laid out {} of {total} rows",
            first.len()
        );
        assert_eq!(first[0], "Line 4: line 4");

        // Scroll far down: different rows are laid out, still a bounded number.
        window.scroll(
            PANE_ID,
            ScrollDelta::Pixels(point(px(0.), px(-20. * 15_000.))),
            cx,
        );
        window.render_frame(cx);
        let later = labels(window, ROW_ID);
        assert!(
            !later.is_empty() && later.len() <= 60,
            "{} rows",
            later.len()
        );
        assert!(!later.contains(&first[0]));
        assert!(
            later
                .iter()
                .any(|l| l.contains("line 1") || l.contains("changed")),
            "{later:?}"
        );
    })
    .expect("window is open");
}

// ---- gaps -----------------------------------------------------------------------------------

#[gpui_kit::test]
fn unchanged_context_collapses_into_expandable_gaps(cx: &mut TestAppContext) {
    let old = numbered(60);
    let new = old
        .replace("line 2\n", "two\n")
        .replace("line 58\n", "fifty-eight\n");
    let (window, view) = open_view(
        cx,
        DiffInput::new("src/x.rs", Some(old), new),
        None,
        (900., 2000.),
    );
    let collapsed_rows = cx.update(|cx| view.read(cx).rows().len());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(labels(window, GAP_ID), vec!["Show 49 unchanged lines"]);
        assert!(!labels(window, ROW_ID).iter().any(|l| l.contains("line 30")));
        assert_eq!(labels(window, HUNK_ID).len(), 2);

        window.click(GAP_ID, cx);
        window.render_frame(cx);
        assert!(window.find_all(GAP_ID).is_empty());
        assert!(
            labels(window, ROW_ID)
                .iter()
                .any(|l| l == "Line 30: line 30")
        );
    })
    .expect("window is open");
    cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.rows().len(), collapsed_rows - 1 + 49);
        assert_eq!(view.expanded_gaps().len(), 1);
    });
}

#[gpui_kit::test]
fn expand_all_opens_every_gap(cx: &mut TestAppContext) {
    let old = numbered(100);
    let new = old
        .replace("line 1\n", "one\n")
        .replace("line 50\n", "fifty\n")
        .replace("line 100\n", "hundred\n");
    let (window, view) = open_view(
        cx,
        DiffInput::new("src/x.rs", Some(old), new),
        None,
        (900., 600.),
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find_all(GAP_ID).len(), 2);
        window.click(EXPAND_ALL_ID, cx);
        window.render_frame(cx);
        assert!(window.find_all(GAP_ID).is_empty());
        assert!(window.find_all(EXPAND_ALL_ID).len() <= 1);
    })
    .expect("window is open");
    let all_expanded = cx.update(|cx| {
        view.read(cx)
            .rows()
            .iter()
            .all(|row| !matches!(row, Row::Gap(_)))
    });
    assert!(all_expanded);
}

// ---- open file ------------------------------------------------------------------------------

fn capture(cx: &mut TestAppContext, view: &Entity<DiffView>) -> Rc<RefCell<Vec<DiffEvent>>> {
    let log = Rc::new(RefCell::new(Vec::new()));
    let sink = log.clone();
    cx.update(|cx| {
        cx.subscribe(view, move |_, event: &DiffEvent, _| {
            sink.borrow_mut().push(event.clone());
        })
        .detach();
    });
    log
}

#[gpui_kit::test]
fn open_file_from_a_hunk_emits_path_and_line(cx: &mut TestAppContext) {
    let old = numbered(20);
    let new = old.replace("line 12\n", "twelve\n");
    let (window, view) = open_view(
        cx,
        DiffInput::new("src/lib.rs", Some(old), new),
        Some("/work/repo"),
        (900., 600.),
    );
    let log = capture(cx, &view);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(labels(window, OPEN_ID), vec!["Open src/lib.rs at line 12"]);
        window.click(OPEN_ID, cx);
    })
    .expect("window is open");
    assert_eq!(
        *log.borrow(),
        vec![DiffEvent::OpenFile {
            path: PathBuf::from("/work/repo/src/lib.rs"),
            line: 12
        }]
    );
}

#[gpui_kit::test]
fn open_file_is_absent_without_a_workspace_root(cx: &mut TestAppContext) {
    let (window, view) = open_view(cx, small_input(), None, (900., 400.));
    let log = capture(cx, &view);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find_all(HUNK_ID).len(), 1);
        assert!(window.find_all(OPEN_ID).is_empty());
    })
    .expect("window is open");
    let opened = cx.update(|cx| view.update(cx, |view, cx| view.open_hunk(0, cx)));
    assert!(!opened);
    assert!(log.borrow().is_empty());
}

#[gpui_kit::test]
fn open_file_is_absent_for_a_path_outside_the_workspace(cx: &mut TestAppContext) {
    let input = DiffInput::new("/etc/passwd", Some("a\n".into()), "b\n".into());
    let (window, view) = open_view(cx, input, Some("/work/repo"), (900., 400.));
    let log = capture(cx, &view);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find_all(OPEN_ID).is_empty());
    })
    .expect("window is open");
    assert!(!cx.update(|cx| view.update(cx, |view, cx| view.open_hunk(0, cx))));
    assert!(log.borrow().is_empty());
}

// ---- notices --------------------------------------------------------------------------------

#[gpui_kit::test]
fn missing_newline_binary_too_large_and_identical_say_so(cx: &mut TestAppContext) {
    // No newline at end of file.
    let input = DiffInput::new("a.txt", Some("a\nb".into()), "a\nb\n".into());
    let (window, _) = open_view(cx, input, None, (900., 400.));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            labels(window, ROW_ID).contains(&"No newline at end of file".to_owned()),
            "{:?}",
            labels(window, ROW_ID)
        );
    })
    .expect("window is open");

    let cases: Vec<(DiffInput, &str)> = vec![
        (
            DiffInput::new("img.png", Some("\u{0}PNG".into()), "x".into()),
            "Binary file: the contents are not shown.",
        ),
        (
            DiffInput::new("big.txt", None, "x\n".repeat(1_100_000)),
            "Too large to compare (2.1 MiB; the limit is 2.0 MiB).",
        ),
        (
            DiffInput::new("same.txt", Some("a\n".into()), "a\n".into()),
            "No changes: the old and new text are the same.",
        ),
    ];
    for (input, message) in cases {
        let (window, _) = open_view(cx, input, None, (900., 400.));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(labels(window, NOTICE_ID), vec![message.to_owned()]);
            assert!(window.find_all(ROW_ID).is_empty());
            assert!(window.find_all(SPLIT_ID).is_empty());
        })
        .expect("window is open");
    }
}

// ---- catalog --------------------------------------------------------------------------------

#[test]
fn every_string_id_in_the_code_is_in_the_catalog_and_every_catalog_id_is_used() {
    let catalog = Catalog::from_source("en", EN_FTL).expect("diff.ftl parses");
    let element_ids: BTreeSet<&str> = [
        PANE_ID,
        ROW_ID,
        HUNK_ID,
        GAP_ID,
        OPEN_ID,
        NOTICE_ID,
        TITLE_ID,
        UNIFIED_ID,
        SPLIT_ID,
        EXPAND_ALL_ID,
        "diff-list",
        "diff-scroll",
    ]
    .into_iter()
    .collect();
    let sources = [
        include_str!("../src/diff/mod.rs"),
        include_str!("../src/diff/model.rs"),
        include_str!("../src/diff/view.rs"),
    ];
    let mut used = BTreeSet::new();
    for source in sources {
        let mut rest = source;
        while let Some(start) = rest.find("\"diff-") {
            let tail = &rest[start + 1..];
            let end = tail.find('"').expect("closing quote");
            let literal = &tail[..end];
            if literal
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                used.insert(literal.to_owned());
            }
            rest = &tail[end + 1..];
        }
    }
    for id in &used {
        if element_ids.contains(id.as_str()) && !catalog.has(id) {
            continue;
        }
        assert!(catalog.has(id), "string id {id} is not in diff.ftl");
    }
    for id in Catalog::ids(EN_FTL) {
        assert!(
            used.contains(&id),
            "diff.ftl id {id} is not used by the code"
        );
    }
}
