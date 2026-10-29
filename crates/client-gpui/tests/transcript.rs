//! M3-008: the task view's transcript pane, tested headlessly.
//!
//! - every TurnEvent example in `crates/protocol/schemas/turn-event/examples` goes through
//!   `client-core` into the view and must produce rows (a new example with no expectation here,
//!   or one that renders nothing, fails the test);
//! - the pane is virtualized: with the 10k fixture only a window of rows is laid out;
//! - layout time and memory for that window are bounded (see the report for what is not measured:
//!   real frame time needs a GPU).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use client_core::ThreadView;
use client_gpui::diff::view::{OPEN_ID as DIFF_OPEN_ID, PANE_ID as DIFF_PANE_ID};
use client_gpui::shell::i18n::{self, Catalog};
use client_gpui::task::tools::{activity_rows, tool_rows};
use client_gpui::task::transcript::{
    CLOSE_DIFF_ID, DIFF_PANEL_ID, MAX_BODY_LINES, MAX_DIFF_LINES, MAX_TERMINAL_LINES, MORE_ID,
    PANE_ID, ROW_HEIGHT, ROW_ID, VIEW_DIFF_ID, WRAP_COLS, sanitize, thread_rows,
    thread_rows_expanded, thread_rows_with_cards, wrap_lines,
};
use client_gpui::task::{Row, RowCache, RowKind, TranscriptEvent, TranscriptPane};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AppContext, Bounds, Entity, Point, ScrollDelta, TestAppContext, WindowBounds, WindowOptions,
    point, px, size,
};
use protocol::turn_event::Envelope;
use serde_json::json;

// ---- fixtures --------------------------------------------------------------------------------

fn examples_dir() -> PathBuf {
    [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "protocol",
        "schemas",
        "turn-event",
        "examples",
    ]
    .iter()
    .collect()
}

fn example(file: &str) -> Envelope {
    let path = examples_dir().join(file);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{file}: {error}"))
}

fn example_files() -> BTreeSet<String> {
    std::fs::read_dir(examples_dir())
        .expect("examples dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.ends_with(".json"))
        .collect()
}

/// A thread view fed with `files` (in order, fresh `seq`s) after `setup`.
fn thread_from(setup: fn(&mut ThreadView), files: &[&str]) -> ThreadView {
    let mut thread = ThreadView::new("th_1");
    setup(&mut thread);
    for (n, file) in files.iter().enumerate() {
        let mut envelope = example(file);
        envelope.seq = n as u64 + 1;
        thread
            .apply(&envelope)
            .unwrap_or_else(|error| panic!("{file}: {error}"));
    }
    thread
}

fn texts(rows: &[Row]) -> Vec<String> {
    rows.iter().map(|row| row.text.clone()).collect()
}

fn no_setup(_: &mut ThreadView) {}

fn usage_snapshot(thread: &mut ThreadView) {
    let mut states = serde_json::Map::new();
    states.insert(
        "usage".into(),
        json!({"context_used": 0, "context_size": 200000, "cost": {"amount": 0.0, "currency": "USD"}}),
    );
    thread.install_snapshot(states);
}

// ---- every TurnEvent example renders ---------------------------------------------------------

/// What an example must put on screen: the example, the examples it needs first, the thread
/// setup, and text its rendering must contain.
struct Case {
    file: &'static str,
    before: &'static [&'static str],
    setup: fn(&mut ThreadView),
    shows: &'static [&'static str],
}

const CASES: &[Case] = &[
    Case {
        file: "UserMessage.json",
        before: &[],
        setup: no_setup,
        shows: &["You: Add a /healthz route", "[file: screen.png]"],
    },
    Case {
        file: "TextMessageStart.json",
        before: &[],
        setup: no_setup,
        shows: &["..."],
    },
    Case {
        file: "TextMessageContent.json",
        before: &[],
        setup: no_setup,
        shows: &["Adding the route now."],
    },
    Case {
        file: "TextMessageEnd.json",
        before: &["TextMessageContent.json"],
        setup: no_setup,
        shows: &["Adding the route now."],
    },
    Case {
        file: "ReasoningMessageStart.json",
        before: &[],
        setup: no_setup,
        shows: &["Thinking"],
    },
    Case {
        file: "ReasoningMessageContent.json",
        before: &[],
        setup: no_setup,
        shows: &["Thinking", "The router lives in src/main.rs."],
    },
    Case {
        file: "ReasoningMessageEnd.json",
        before: &["ReasoningMessageContent.json"],
        setup: no_setup,
        shows: &["The router lives in src/main.rs."],
    },
    Case {
        file: "ToolCallStart.json",
        before: &[],
        setup: no_setup,
        shows: &["Edit src/main.rs (edit) - waiting", "src/main.rs:12"],
    },
    Case {
        file: "ToolCallArgs.json",
        before: &[],
        setup: no_setup,
        shows: &["Input", "\"file_path\": \"src/main.rs\""],
    },
    Case {
        file: "ToolCallResult.json",
        before: &[],
        setup: no_setup,
        shows: &["Output", "Edited 1 file"],
    },
    Case {
        file: "ToolCallEnd.json",
        before: &["ToolCallStart.json"],
        setup: no_setup,
        shows: &["Edit src/main.rs (edit) - done"],
    },
    Case {
        file: "Activity.json",
        before: &[],
        setup: no_setup,
        shows: &[
            "Changed src/main.rs",
            "- let app = Router::new();",
            "+ let app = Router::new().route(\"/healthz\", get(ok));",
        ],
    },
    Case {
        file: "Activity.plan.json",
        before: &[],
        setup: no_setup,
        shows: &["Plan", "[doing] Add route"],
    },
    Case {
        file: "Activity.policy.json",
        before: &[],
        setup: no_setup,
        shows: &["Policy allowed preset:read"],
    },
    Case {
        file: "Activity.terminal.json",
        before: &[],
        setup: no_setup,
        shows: &["Terminal term_1", "test result: ok. 3 passed", "running"],
    },
    Case {
        file: "Interrupt.json",
        before: &[],
        setup: no_setup,
        shows: &[
            "Approval needed: Allow edit to src/main.rs?",
            "Options: Allow / Deny",
        ],
    },
    Case {
        file: "InterruptResolved.json",
        before: &["Interrupt.json"],
        setup: no_setup,
        shows: &["Answered by Jacob: Allow"],
    },
    Case {
        file: "Checkpoint.json",
        before: &[],
        setup: no_setup,
        shows: &["Checkpoint cp_7, 1 files changed"],
    },
    Case {
        file: "RunStarted.json",
        before: &[],
        setup: no_setup,
        shows: &["Task running"],
    },
    Case {
        file: "RunStarted.resume.json",
        before: &[],
        setup: no_setup,
        shows: &["Task running"],
    },
    Case {
        file: "RunFinished.json",
        before: &["RunStarted.json"],
        setup: no_setup,
        shows: &["Task finished"],
    },
    Case {
        file: "RunFinished.interrupt.json",
        before: &["RunStarted.json", "Interrupt.json"],
        setup: no_setup,
        shows: &["Task needs your answer"],
    },
    Case {
        file: "RunError.json",
        before: &["RunStarted.json"],
        setup: no_setup,
        shows: &[
            "Run stopped (expired): approval expired after 10 min",
            "Task failed",
        ],
    },
    Case {
        file: "StateSnapshot.json",
        before: &[],
        setup: no_setup,
        shows: &["Mode: default"],
    },
    Case {
        file: "StateDelta.json",
        before: &[],
        setup: usage_snapshot,
        shows: &["Context: 48211 of 200000 tokens"],
    },
    Case {
        file: "Presence.json",
        before: &[],
        setup: no_setup,
        shows: &["Machine mch_laptop is asleep"],
    },
    Case {
        file: "StepStarted.json",
        before: &[],
        setup: no_setup,
        shows: &["Step verify (round 2): running"],
    },
    Case {
        file: "StepFinished.json",
        before: &["StepStarted.json"],
        setup: no_setup,
        shows: &["Step verify (round 2): done"],
    },
    Case {
        file: "SubagentStarted.json",
        before: &[],
        setup: no_setup,
        shows: &["Subagent reviewer: running", "Runs as tk_child1"],
    },
    Case {
        file: "SubagentFinished.json",
        before: &["SubagentStarted.json"],
        setup: no_setup,
        shows: &["Subagent reviewer: done", "\"verdict\": \"approve\""],
    },
    Case {
        file: "SubagentError.json",
        before: &["SubagentStarted.json"],
        setup: no_setup,
        shows: &["Subagent reviewer: failed (cancelled): child cancelled"],
    },
    Case {
        file: "Raw.json",
        before: &[],
        setup: no_setup,
        shows: &["Child thread th_child1: TextMessageContent"],
    },
];

#[test]
fn every_turn_event_example_has_an_expectation() {
    let covered: BTreeSet<String> = CASES.iter().map(|case| case.file.to_owned()).collect();
    assert_eq!(
        example_files(),
        covered,
        "an example has no rendering expectation (or an expectation names a missing example)"
    );
}

#[test]
fn every_turn_event_example_renders_from_the_view_models() {
    for case in CASES {
        let mut files = case.before.to_vec();
        files.push(case.file);
        let rows = texts(&thread_rows(&thread_from(case.setup, &files)));
        for expected in case.shows {
            assert!(
                rows.iter().any(|row| row.contains(expected)),
                "{}: no row contains {expected:?}; rows: {rows:#?}",
                case.file
            );
        }
        // Examples that stand alone must be what puts their text on screen.
        if case.before.is_empty() {
            let without = texts(&thread_rows(&thread_from(case.setup, &[])));
            assert!(
                case.shows
                    .iter()
                    .any(|s| !without.iter().any(|r| r.contains(s))),
                "{}: its text is already shown without it",
                case.file
            );
        }
    }
}

#[test]
fn steps_subagents_and_raw_render_in_order() {
    let rows = texts(&thread_rows(&thread_from(
        no_setup,
        &[
            "StepStarted.json",
            "SubagentStarted.json",
            "Raw.json",
            "SubagentError.json",
            "StepFinished.json",
        ],
    )));
    let at = |needle: &str| {
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("{needle} in {rows:#?}"))
    };
    // One row per step (updated in place), subagent and raw event, in arrival order.
    assert_eq!(rows.iter().filter(|r| r.starts_with("Step ")).count(), 1);
    assert!(at("Step verify (round 2): done") < at("Subagent reviewer"));
    assert!(at("Subagent reviewer: failed") < at("Child thread th_child1"));
    assert!(!rows.iter().any(|r| r.contains("running")), "{rows:#?}");
}

#[test]
fn step_and_subagent_names_are_text() {
    let mut thread = ThreadView::new("th_1");
    for (n, event) in [
        json!({"type": "StepStarted", "step_id": "s", "step_name": "<b>x</b>\u{1b}[2J"}),
        json!({"type": "SubagentStarted", "subagent_run_id": "r", "name": "**a**\u{7}"}),
        json!({"type": "Raw", "source": "acp", "event": {"type": "evil\u{1b}[31m"}}),
    ]
    .into_iter()
    .enumerate()
    {
        let envelope: Envelope = serde_json::from_value(json!({
            "v": 1, "seq": n + 1, "thread_id": "th_1", "task_id": "tk_1", "run_id": "run_1",
            "ts": "2026-10-27T22:00:00.000Z", "event": event
        }))
        .expect("envelope");
        thread.apply(&envelope).expect("apply");
    }
    let rows = texts(&thread_rows(&thread));
    assert!(
        rows.contains(&"Step <b>x</b>[2J: running".to_owned()),
        "{rows:?}"
    );
    assert!(
        rows.contains(&"Subagent **a**: running".to_owned()),
        "{rows:?}"
    );
    assert!(
        rows.contains(&"Agent event: evil[31m".to_owned()),
        "{rows:?}"
    );
}

// ---- row building ----------------------------------------------------------------------------

#[test]
fn untrusted_text_is_shown_as_text() {
    assert_eq!(sanitize("a\u{1b}[31mred\u{7}\tb"), "a[31mred b");
    let lines = wrap_lines(&"x".repeat(WRAP_COLS * 2 + 5), WRAP_COLS);
    assert_eq!(
        lines.iter().map(|l| l.chars().count()).collect::<Vec<_>>(),
        [WRAP_COLS, WRAP_COLS, 5]
    );
    // Markup in a message stays literal text and control characters never reach a row.
    let mut envelope = example("TextMessageContent.json");
    if let protocol::turn_event::Event::TextMessageContent(content) = &mut envelope.event {
        content.delta = "<b>hi</b> **x** \u{1b}[2J\nsecond".into();
    }
    let mut thread = ThreadView::new("th_1");
    thread.apply(&envelope).expect("apply");
    let rows = texts(&thread_rows(&thread));
    assert!(rows.contains(&"<b>hi</b> **x** [2J".to_owned()), "{rows:?}");
    assert!(rows.contains(&"second".to_owned()));
}

#[test]
fn long_bodies_are_capped_with_a_more_row() {
    let long = (0..100)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut thread = ThreadView::new("th_1");
    for (n, event) in [
        json!({"type": "ToolCallResult", "tool_call_id": "t", "content": [{"type": "text", "text": long}]}),
        json!({"type": "Activity", "activity_id": "term", "activity_type": "terminal",
               "mode": "snapshot", "content": {"terminal_id": "term", "output": long}}),
        json!({"type": "Activity", "activity_id": "d", "activity_type": "diff", "mode": "snapshot",
               "content": {"path": "a.rs", "old_text": null, "new_text": long}}),
    ]
    .into_iter()
    .enumerate()
    {
        let envelope: Envelope = serde_json::from_value(json!({
            "v": 1, "seq": n + 1, "thread_id": "th_1", "task_id": "tk_1", "run_id": "run_1",
            "ts": "2026-10-27T22:00:00.000Z", "event": event
        }))
        .expect("envelope");
        thread.apply(&envelope).expect("apply");
    }
    let task = &thread.tasks()[0];
    let items = task.transcript().items();
    let mut tool = Vec::new();
    let mut terminal = Vec::new();
    let mut diff = Vec::new();
    for item in items {
        match item {
            client_core::view::Item::Tool(view) => tool_rows(view, false, &mut tool),
            client_core::view::Item::Activity(view) if view.content.contains_key("output") => {
                activity_rows(view, false, &mut terminal);
            }
            client_core::view::Item::Activity(view) => activity_rows(view, false, &mut diff),
            _ => {}
        }
    }
    assert!(tool.len() <= MAX_BODY_LINES + 3, "{}", tool.len());
    assert!(
        terminal.len() <= MAX_TERMINAL_LINES + 3,
        "{}",
        terminal.len()
    );
    // Title, summary (M3-010a), the capped lines, "more".
    assert!(diff.len() <= MAX_DIFF_LINES + 3, "{}", diff.len());
    for rows in [&tool, &terminal, &diff] {
        assert!(
            rows.iter()
                .any(|r| r.kind == RowKind::More && r.text.contains("more lines"))
        );
    }
    // The terminal shows the tail of the output, not the head.
    assert!(terminal.iter().any(|r| r.text == "line 99"));
    assert!(!terminal.iter().any(|r| r.text == "line 0"));
}

// ---- strings ---------------------------------------------------------------------------------

#[test]
fn every_task_string_id_exists_in_the_catalog_and_is_used() {
    let catalog = Catalog::english();
    let prefixes = ["task-", "transcript-", "tool-", "activity-", "interrupt-"];
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/task");
    let mut used = BTreeSet::new();
    for file in ["transcript.rs", "tools.rs"] {
        let text = std::fs::read_to_string(dir.join(file)).expect("source");
        for literal in text.split('"').skip(1).step_by(2) {
            let is_id = literal.contains('-')
                && literal.chars().all(|c| c.is_ascii_lowercase() || c == '-')
                && prefixes.iter().any(|p| literal.starts_with(p));
            // Element ids of the pane, not strings.
            let is_element = [
                "transcript-list",
                "transcript-scroll",
                "transcript-row",
                "transcript-more",
            ]
            .contains(&literal);
            if is_id && !is_element {
                used.insert(literal.to_owned());
            }
        }
    }
    assert!(used.len() > 40, "scan found {} ids", used.len());
    for id in &used {
        assert!(catalog.has(id), "{id} is missing from shell.ftl");
    }
    let defined: BTreeSet<String> = Catalog::ids(i18n::EN_FTL)
        .into_iter()
        .filter(|id| prefixes.iter().any(|p| id.starts_with(p)))
        .collect();
    // `task-label` is used by the pane through `tr("task-label")`, covered by the scan above.
    assert_eq!(defined, used, "unused or undefined task strings");
}

// ---- headless window: virtualization, bounds, timing ----------------------------------------

fn options(width: f32, height: f32) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: Point::default(),
            size: size(px(width), px(height)),
        })),
        ..Default::default()
    }
}

fn fixture_thread() -> (ThreadView, usize) {
    let log = agui_fixture::complete_log(agui_fixture::Scenario::Transcript10k, "th_1")
        .expect("transcript-10k log");
    let mut thread = ThreadView::new("th_1");
    for envelope in &log {
        thread.apply(envelope).expect("fixture event applies");
    }
    (thread, log.len())
}

fn open_pane(
    cx: &mut TestAppContext,
    options: WindowOptions,
) -> (gpui_kit::AnyWindowHandle, Entity<TranscriptPane>) {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| TranscriptPane::new()))
            .expect("open test window")
    })
}

#[gpui_kit::test]
fn all_example_variants_are_laid_out_in_the_window(cx: &mut TestAppContext) {
    let files: Vec<&str> = CASES
        .iter()
        .map(|case| case.file)
        .filter(|file| {
            // One coherent task: leave out the examples that end or reset it.
            !file.starts_with("RunFinished") && !file.starts_with("RunStarted.resume")
        })
        .collect();
    let mut thread = ThreadView::new("th_1");
    usage_snapshot(&mut thread);
    for (n, file) in files.iter().enumerate() {
        let mut envelope = example(file);
        envelope.seq = n as u64 + 1;
        thread.apply(&envelope).expect(file);
    }
    let (window, pane) = open_pane(cx, options(900., 1600.));
    cx.update(|cx| pane.update(cx, |pane, cx| pane.set_thread(&thread, cx)));
    let expected = cx.update(|cx| texts(pane.read(cx).rows()));
    assert!(expected.len() > 30, "{} rows", expected.len());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find(PANE_ID).visible());
        let laid_out: Vec<String> = window
            .find_all(ROW_ID)
            .iter()
            .map(|row| row.label().unwrap_or_default().to_owned())
            .collect();
        for text in &expected {
            assert!(laid_out.contains(text), "row {text:?} was not laid out");
        }
    })
    .expect("window is open");
}

/// Laid-out rows allowed for a 600 px window (~27 rows fit; the list overdraws a little).
const MAX_LAID_OUT: usize = 60;
/// Rows a 10,000-event transcript may fold into (about 4 per event; measured value is lower).
const MAX_ROWS_PER_EVENT: usize = 8;
/// Layout budget per frame of the visible window, in debug builds (gates.yaml asks 16.7 ms p95
/// for a *painted* frame on a GPU; here only layout runs).
const FRAME_BUDGET_DEBUG: Duration = Duration::from_millis(100);
/// The same budget in release builds: the gates.yaml frame time.
const FRAME_BUDGET_RELEASE: Duration = Duration::from_micros(16_700);

#[gpui_kit::test]
fn ten_thousand_events_are_virtualized_and_within_budget(cx: &mut TestAppContext) {
    let (thread, events) = fixture_thread();
    assert!(events >= 10_000, "fixture has {events} events");

    let started = Instant::now();
    let rows = thread_rows(&thread);
    let build = started.elapsed();
    let total = rows.len();
    let bytes: usize = rows.iter().map(|row| row.text.len()).sum();
    eprintln!(
        "10k fixture: {events} events -> {total} rows, {bytes} text bytes, built in {build:?}"
    );
    assert!(
        total > 2_000 && total <= events * MAX_ROWS_PER_EVENT,
        "{total} rows"
    );
    assert!(bytes < 16 * 1024 * 1024, "row text is {bytes} bytes");

    let (window, pane) = open_pane(cx, options(900., 600.));
    let started = Instant::now();
    cx.update(|cx| pane.update(cx, |pane, cx| pane.set_thread(&thread, cx)));
    eprintln!("set_thread (rows + sizes): {:?}", started.elapsed());
    let labels = cx.update(|cx| texts(pane.read(cx).rows()));
    assert_eq!(labels.len(), total);

    let budget = if cfg!(debug_assertions) {
        FRAME_BUDGET_DEBUG
    } else {
        FRAME_BUDGET_RELEASE
    };
    let mut frames = Vec::new();
    cx.update_window(window, |_, window, cx| {
        let started = Instant::now();
        window.render_frame(cx);
        eprintln!("first frame: {:?}", started.elapsed());

        let laid_out = window.find_all(ROW_ID);
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
        // Rows far below the viewport are not built at all (pick one whose text is unique:
        // the diff summary rows repeat).
        let far = (300..labels.len())
            .find(|&ix| labels.iter().filter(|label| **label == labels[ix]).count() == 1)
            .expect("a unique row");
        assert!(
            laid_out
                .iter()
                .all(|row| row.label() != Some(labels[far].as_str()))
        );

        // Scroll down through the transcript, one screen at a time, timing each frame.
        let screen = 25usize;
        for step in 1..=40usize {
            let delta = -(ROW_HEIGHT * (screen * step) as f32);
            window.scroll(PANE_ID, ScrollDelta::Pixels(point(px(0.), delta)), cx);
            let started = Instant::now();
            window.render_frame(cx);
            frames.push(started.elapsed());
            let laid_out = window.find_all(ROW_ID);
            assert!(
                !laid_out.is_empty() && laid_out.len() <= MAX_LAID_OUT,
                "laid out {} rows after scrolling",
                laid_out.len()
            );
        }
        let laid_out = window.find_all(ROW_ID);
        assert!(
            laid_out
                .iter()
                .all(|row| row.label() != Some(labels[0].as_str())
                    || labels[0] == labels[labels.len() - 1]),
            "the first row is no longer laid out after scrolling"
        );
    })
    .expect("window is open");

    frames.sort();
    let p95 = frames[(frames.len() * 95).div_ceil(100) - 1];
    eprintln!(
        "visible-window layout over {} frames: median {:?}, p95 {p95:?} (budget {budget:?})",
        frames.len(),
        frames[frames.len() / 2]
    );
    assert!(p95 <= budget, "p95 layout time {p95:?} exceeds {budget:?}");
}

#[gpui_kit::test]
fn the_pane_follows_a_growing_transcript(cx: &mut TestAppContext) {
    let (window, pane) = open_pane(cx, options(900., 300.));
    cx.update(|cx| pane.update(cx, |pane, _| pane.set_follow_tail(true)));
    let mut thread = ThreadView::new("th_1");
    for n in 0..60u64 {
        let envelope: Envelope = serde_json::from_value(json!({
            "v": 1, "seq": n + 1, "thread_id": "th_1", "task_id": "tk_1", "run_id": "run_1",
            "ts": "2026-10-27T22:00:00.000Z",
            "event": {"type": "TextMessageContent", "message_id": format!("m{n}"),
                      "delta": format!("message {n}")}
        }))
        .expect("envelope");
        thread.apply(&envelope).expect("apply");
    }
    cx.update(|cx| pane.update(cx, |pane, cx| pane.set_thread(&thread, cx)));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        let visible: Vec<String> = window
            .find_all(ROW_ID)
            .iter()
            .filter(|row| row.visible())
            .map(|row| row.label().unwrap_or_default().to_owned())
            .collect();
        assert!(visible.contains(&"Task starting".to_owned()), "{visible:?}");
        assert!(!visible.contains(&"message 0".to_owned()), "{visible:?}");
    })
    .expect("window is open");
}

// ---- expandable bodies -----------------------------------------------------------------------

fn thread_with(events: Vec<serde_json::Value>) -> ThreadView {
    let mut thread = ThreadView::new("th_1");
    for (n, event) in events.into_iter().enumerate() {
        let envelope: Envelope = serde_json::from_value(json!({
            "v": 1, "seq": n + 1, "thread_id": "th_1", "task_id": "tk_1", "run_id": "run_1",
            "ts": "2026-10-27T22:00:00.000Z", "event": event
        }))
        .expect("envelope");
        thread.apply(&envelope).expect("apply");
    }
    thread
}

fn numbered_lines(count: usize) -> String {
    (0..count)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn expanded_tool_terminal_and_diff_bodies_show_every_line() {
    let long = numbered_lines(100);
    let thread = thread_with(vec![
        json!({"type": "ToolCallResult", "tool_call_id": "t", "content": [{"type": "text", "text": long}]}),
        json!({"type": "Activity", "activity_id": "term", "activity_type": "terminal",
               "mode": "snapshot", "content": {"terminal_id": "term", "output": long}}),
        json!({"type": "Activity", "activity_id": "d", "activity_type": "diff", "mode": "snapshot",
               "content": {"path": "a.rs", "old_text": null, "new_text": long}}),
    ]);
    let collapsed = thread_rows(&thread);
    let keys: Vec<usize> = collapsed.iter().filter_map(|row| row.expand).collect();
    // Each of the three items has a "more lines" row that names it.
    assert_eq!(keys, [0, 1, 2], "{:#?}", texts(&collapsed));

    let all: BTreeSet<usize> = keys.into_iter().collect();
    let rows = thread_rows_expanded(&thread, &all);
    let shown = texts(&rows);
    assert!(!rows.iter().any(|r| r.text.contains("more lines")));
    // Tool body, terminal output and diff each show the first and the last line.
    assert_eq!(shown.iter().filter(|t| *t == "line 0").count(), 2);
    assert_eq!(shown.iter().filter(|t| *t == "line 99").count(), 2);
    assert_eq!(shown.iter().filter(|t| *t == "+ line 0").count(), 1);
    assert_eq!(shown.iter().filter(|t| *t == "+ line 99").count(), 1);
    // Collapsing again is a row away for each expanded item.
    let less: Vec<_> = rows.iter().filter(|r| r.text == "Show less").collect();
    assert_eq!(less.len(), 3);
    // Expanding one item leaves the others capped.
    let one = thread_rows_expanded(&thread, &BTreeSet::from([1]));
    assert_eq!(one.iter().filter(|r| r.expand.is_some()).count(), 3);
    assert!(texts(&one).contains(&"line 50".to_owned()));
    assert!(!texts(&collapsed).contains(&"line 50".to_owned()));
    assert!(
        one.iter()
            .any(|r| r.expand == Some(0) && r.text.contains("more lines"))
    );
}

#[gpui_kit::test]
fn clicking_more_lines_expands_the_body_and_show_less_collapses_it(cx: &mut TestAppContext) {
    let thread = thread_with(vec![json!({"type": "ToolCallResult", "tool_call_id": "t",
               "content": [{"type": "text", "text": numbered_lines(40)}]})]);
    let (window, pane) = open_pane(cx, options(900., 1400.));
    cx.update(|cx| pane.update(cx, |pane, cx| pane.set_thread(&thread, cx)));
    let rows_text = |cx: &mut TestAppContext| cx.update(|cx| texts(pane.read(cx).rows()));
    let collapsed = rows_text(cx);
    assert!(!collapsed.contains(&"line 39".to_owned()));
    assert!(collapsed.iter().any(|t| t.contains("more lines")));

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(MORE_ID, cx);
        window.render_frame(cx);
    })
    .expect("window is open");
    let expanded = rows_text(cx);
    assert!(expanded.contains(&"line 39".to_owned()), "{expanded:?}");
    assert!(!expanded.iter().any(|t| t.contains("more lines")));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let laid_out: Vec<String> = window
            .find_all(ROW_ID)
            .iter()
            .map(|row| row.label().unwrap_or_default().to_owned())
            .collect();
        assert!(laid_out.contains(&"line 39".to_owned()), "{laid_out:?}");
        window.click(MORE_ID, cx);
        window.render_frame(cx);
    })
    .expect("window is open");
    assert_eq!(rows_text(cx), collapsed);

    // A new snapshot of the thread keeps what the viewer expanded.
    cx.update_window(window, |_, window, cx| {
        window.click(MORE_ID, cx);
    })
    .expect("window is open");
    cx.update(|cx| pane.update(cx, |pane, cx| pane.set_thread(&thread, cx)));
    assert!(rows_text(cx).contains(&"line 39".to_owned()));
}

// ---- M3-010a: the diff viewer in the task view -----------------------------------------------

fn diff_thread(path: &str, old: &str, new: &str, extra: serde_json::Value) -> ThreadView {
    let mut content = json!({"path": path, "old_text": old, "new_text": new});
    content
        .as_object_mut()
        .expect("object")
        .extend(extra.as_object().expect("object").clone());
    thread_with(vec![json!({"type": "Activity", "activity_id": "d",
        "activity_type": "diff", "mode": "snapshot", "content": content})])
}

fn captured(
    cx: &mut TestAppContext,
    pane: &Entity<TranscriptPane>,
) -> std::rc::Rc<std::cell::RefCell<Vec<TranscriptEvent>>> {
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = log.clone();
    cx.update(|cx| {
        cx.subscribe(pane, move |_, event: &TranscriptEvent, _| {
            sink.borrow_mut().push(event.clone());
        })
        .detach();
    });
    log
}

#[test]
fn a_diff_activity_has_a_summary_row_with_a_view_diff_action() {
    let thread = diff_thread("src/lib.rs", "a\nb\nc\n", "a\nB\nc\nd\n", json!({}));
    let rows = thread_rows(&thread);
    let summary: Vec<&Row> = rows
        .iter()
        .filter(|row| row.kind == RowKind::DiffSummary)
        .collect();
    assert_eq!(summary.len(), 1, "{:#?}", texts(&rows));
    assert_eq!(summary[0].text, "src/lib.rs: +2 -1");
    assert_eq!(summary[0].view_diff.as_deref(), Some("d"));
    // The minimal lines are still there under it.
    assert!(texts(&rows).contains(&"- b".to_owned()));
}

#[test]
fn a_diff_still_arriving_has_no_view_diff_action() {
    let thread = diff_thread(
        "src/lib.rs",
        "a\n",
        "b\n",
        json!({"chunk": {"index": 0, "last": false}}),
    );
    let rows = thread_rows(&thread);
    assert!(rows.iter().any(|row| row.kind == RowKind::DiffTitle));
    assert!(rows.iter().all(|row| row.view_diff.is_none()));
}

#[gpui_kit::test]
fn view_diff_opens_a_diff_view_over_the_pane_and_close_removes_it(cx: &mut TestAppContext) {
    let thread = diff_thread("src/lib.rs", "a\nb\nc\n", "a\nB\nc\n", json!({}));
    let (window, pane) = open_pane(cx, options(900., 600.));
    cx.update(|cx| pane.update(cx, |pane, cx| pane.set_thread(&thread, cx)));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find_all(DIFF_PANEL_ID).is_empty());
        assert!(window.find_all(DIFF_PANE_ID).is_empty());
        assert_eq!(window.find_all(VIEW_DIFF_ID).len(), 1);
        window.click(VIEW_DIFF_ID, cx);
        window.render_frame(cx);
        assert_eq!(window.find_all(DIFF_PANEL_ID).len(), 1);
        assert_eq!(window.find_all(DIFF_PANE_ID).len(), 1);
        window.click(CLOSE_DIFF_ID, cx);
        window.render_frame(cx);
        assert!(window.find_all(DIFF_PANEL_ID).is_empty());
        assert!(window.find_all(DIFF_PANE_ID).is_empty());
    })
    .expect("window is open");
    assert_eq!(
        cx.update(|cx| pane.read(cx).open_diff_id().map(str::to_owned)),
        None
    );
}

#[gpui_kit::test]
fn open_file_from_the_diff_is_re_emitted_with_the_workspace_root(cx: &mut TestAppContext) {
    let old = numbered_lines(20) + "\n";
    let new = old.replace("line 12\n", "twelve\n");
    let thread = diff_thread("src/lib.rs", &old, &new, json!({}));
    let (window, pane) = open_pane(cx, options(900., 600.));
    let log = captured(cx, &pane);
    cx.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_workspace_root(Some(PathBuf::from("/work/repo")), cx);
            pane.set_thread(&thread, cx);
        })
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(VIEW_DIFF_ID, cx);
        window.render_frame(cx);
        assert_eq!(window.find_all(DIFF_OPEN_ID).len(), 1);
        window.click(DIFF_OPEN_ID, cx);
    })
    .expect("window is open");
    assert_eq!(
        *log.borrow(),
        vec![TranscriptEvent::OpenFile {
            path: PathBuf::from("/work/repo/src/lib.rs"),
            line: 13
        }]
    );
}

#[gpui_kit::test]
fn without_a_workspace_root_the_diff_offers_no_open_file(cx: &mut TestAppContext) {
    let thread = diff_thread("src/lib.rs", "a\nb\n", "a\nc\n", json!({}));
    let (window, pane) = open_pane(cx, options(900., 600.));
    let log = captured(cx, &pane);
    cx.update(|cx| pane.update(cx, |pane, cx| pane.set_thread(&thread, cx)));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(VIEW_DIFF_ID, cx);
        window.render_frame(cx);
        assert_eq!(window.find_all(DIFF_PANE_ID).len(), 1);
        assert!(window.find_all(DIFF_OPEN_ID).is_empty());
    })
    .expect("window is open");
    assert!(log.borrow().is_empty());
}

// ---- incremental rows (M3-008b) ---------------------------------------------------------------

fn push_event(thread: &mut ThreadView, seq: &mut u64, event: serde_json::Value) {
    *seq += 1;
    let envelope: Envelope = serde_json::from_value(json!({
        "v": 1, "seq": *seq, "thread_id": "th_1", "task_id": "tk_1", "run_id": "run_1",
        "ts": "2026-10-27T22:00:00.000Z", "event": event
    }))
    .expect("envelope");
    thread.apply(&envelope).expect("apply");
}

fn text_delta(message: &str, delta: &str) -> serde_json::Value {
    json!({"type": "TextMessageContent", "message_id": message, "delta": delta})
}

/// A thread of `count` one-line messages, and the last seq used.
fn message_thread(count: usize) -> (ThreadView, u64) {
    let mut thread = ThreadView::new("th_1");
    let mut seq = 0;
    for n in 0..count {
        push_event(&mut thread, &mut seq, text_delta(&format!("m{n}"), "hello"));
    }
    (thread, seq)
}

fn full_rows(thread: &ThreadView, expanded: &BTreeSet<usize>) -> Vec<Row> {
    thread_rows_with_cards(thread, expanded, &[])
}

#[test]
fn one_appended_text_delta_rebuilds_one_items_rows() {
    let (mut thread, mut seq) = message_thread(200);
    let none = BTreeSet::new();
    let mut cache = RowCache::new();
    cache.sync(&thread, &none, &[]);
    let first = cache.last_update();
    // Everything is built the first time: 200 items.
    assert_eq!(first.items_rebuilt, 200, "{first:?}");
    assert_eq!(cache.rows(), full_rows(&thread, &none));

    // One delta for one message: one item, one row.
    push_event(&mut thread, &mut seq, text_delta("m77", " world"));
    cache.sync(&thread, &none, &[]);
    let update = cache.last_update();
    assert_eq!(update.items_visited, 200);
    assert_eq!(update.items_rebuilt, 1, "{update:?}");
    assert_eq!(update.rows_rebuilt, 1, "{update:?}");
    assert_eq!(cache.rows(), full_rows(&thread, &none));
    assert!(cache.rows().iter().any(|row| row.text == "hello world"));

    // A new message adds its item only.
    push_event(&mut thread, &mut seq, text_delta("new", "fresh"));
    cache.sync(&thread, &none, &[]);
    let update = cache.last_update();
    assert_eq!(
        (update.items_rebuilt, update.rows_rebuilt),
        (1, 1),
        "{update:?}"
    );
    assert_eq!(cache.rows(), full_rows(&thread, &none));

    // The same thread again: nothing is built.
    cache.sync(&thread, &none, &[]);
    let update = cache.last_update();
    assert_eq!(
        (update.items_rebuilt, update.rows_rebuilt),
        (0, 0),
        "{update:?}"
    );
}

#[test]
fn expanding_an_item_rebuilds_that_item_alone() {
    let long = numbered_lines(100);
    let mut thread = ThreadView::new("th_1");
    let mut seq = 0;
    for n in 0..20 {
        push_event(&mut thread, &mut seq, text_delta(&format!("m{n}"), "hello"));
        push_event(
            &mut thread,
            &mut seq,
            json!({"type": "ToolCallResult", "tool_call_id": format!("t{n}"),
                   "content": [{"type": "text", "text": long}]}),
        );
    }
    let none = BTreeSet::new();
    let mut cache = RowCache::new();
    cache.sync(&thread, &none, &[]);
    assert_eq!(cache.rows(), full_rows(&thread, &none));

    // Item 11 is the tool call of the sixth pair.
    let open = BTreeSet::from([11]);
    cache.refresh(&open, &[]);
    let update = cache.last_update();
    assert_eq!(update.items_rebuilt, 1, "{update:?}");
    assert_eq!(cache.rows(), full_rows(&thread, &open));
    assert!(cache.rows().iter().any(|row| row.text == "Show less"));

    // A later sync keeps it expanded and rebuilds nothing.
    cache.sync(&thread, &open, &[]);
    assert_eq!(cache.last_update().items_rebuilt, 0);
    cache.refresh(&none, &[]);
    assert_eq!(cache.last_update().items_rebuilt, 1);
    assert_eq!(cache.rows(), full_rows(&thread, &none));
}

#[test]
fn diff_summary_counts_are_computed_once_per_diff_content() {
    let long = numbered_lines(100);
    let mut thread = ThreadView::new("th_1");
    let mut seq = 0;
    push_event(
        &mut thread,
        &mut seq,
        json!({"type": "Activity", "activity_id": "d", "activity_type": "diff", "mode": "snapshot",
               "content": {"path": "a.rs", "old_text": "x", "new_text": long}}),
    );
    let none = BTreeSet::new();
    let mut cache = RowCache::new();
    cache.sync(&thread, &none, &[]);
    assert_eq!(cache.last_update().diff_builds, 1);
    assert!(cache.rows().iter().any(|row| row.text == "a.rs: +100 -1"));

    // Other items streaming in, and the diff expanded and collapsed: no new model.
    for n in 0..10 {
        push_event(&mut thread, &mut seq, text_delta(&format!("m{n}"), "hi"));
        cache.sync(&thread, &none, &[]);
        assert_eq!(cache.last_update().diff_builds, 0);
    }
    let open = BTreeSet::from([0]);
    cache.refresh(&open, &[]);
    assert_eq!(cache.last_update().items_rebuilt, 1);
    assert_eq!(cache.last_update().diff_builds, 0);
    cache.refresh(&none, &[]);
    assert_eq!(cache.last_update().diff_builds, 0);

    // New diff content for the same activity is counted again.
    push_event(
        &mut thread,
        &mut seq,
        json!({"type": "Activity", "activity_id": "d", "activity_type": "diff", "mode": "snapshot",
               "content": {"path": "a.rs", "old_text": "x", "new_text": "y\nz"}}),
    );
    cache.sync(&thread, &none, &[]);
    assert_eq!(cache.last_update().diff_builds, 1);
    assert!(cache.rows().iter().any(|row| row.text == "a.rs: +2 -1"));
    assert_eq!(cache.rows(), full_rows(&thread, &none));
}

/// The 10k fixture as a stream: batch `f` holds the envelopes of frame `f` at 200 events/s and
/// 60 frames/s (3 or 4 events).
fn frame_batches() -> Vec<Vec<Envelope>> {
    let log = agui_fixture::complete_log(agui_fixture::Scenario::Transcript10k, "th_1")
        .expect("transcript-10k log");
    let mut batches = Vec::new();
    let mut taken = 0;
    let mut frame = 1usize;
    while taken < log.len() {
        let upto = (frame * 200 / 60).min(log.len());
        batches.push(log[taken..upto].to_vec());
        taken = upto;
        frame += 1;
    }
    batches
}

#[gpui_kit::test]
fn streaming_the_fixture_at_200_events_a_second_updates_few_rows_per_frame(
    cx: &mut TestAppContext,
) {
    let batches = frame_batches();
    let events: usize = batches.iter().map(Vec::len).sum();
    assert!(events >= 10_000 && batches.iter().all(|b| (1..=4).contains(&b.len())));
    let (_window, pane) = open_pane(cx, options(900., 600.));
    cx.update(|cx| pane.update(cx, |pane, _| pane.set_follow_tail(true)));

    let mut thread = ThreadView::new("th_1");
    let mut times = Vec::new();
    let mut first = Duration::ZERO;
    let (mut max_items, mut max_rows, mut diff_builds) = (0, 0, 0);
    let none = BTreeSet::new();
    for (frame, batch) in batches.iter().enumerate() {
        for envelope in batch {
            thread.apply(envelope).expect("fixture event applies");
        }
        let started = Instant::now();
        cx.update(|cx| pane.update(cx, |pane, cx| pane.set_thread(&thread, cx)));
        let elapsed = started.elapsed();
        let update = cx.update(|cx| pane.read(cx).last_update());
        if frame == 0 {
            first = elapsed;
        } else {
            times.push(elapsed);
            max_items = max_items.max(update.items_rebuilt);
            max_rows = max_rows.max(update.rows_rebuilt);
            diff_builds += update.diff_builds;
            // Every event touches at most one item; never the whole thread.
            assert!(
                update.items_rebuilt <= batch.len(),
                "frame {frame}: {update:?} for {} events",
                batch.len()
            );
        }
        if frame % 400 == 0 || frame + 1 == batches.len() {
            let rows = cx.update(|cx| pane.read(cx).rows().to_vec());
            assert_eq!(rows, full_rows(&thread, &none), "frame {frame}");
        }
    }
    // The diff models are built once per diff, not once per frame.
    let diffs = thread_rows(&thread)
        .iter()
        .filter(|row| row.kind == RowKind::DiffSummary)
        .count();
    assert!(
        diff_builds <= diffs,
        "{diff_builds} builds for {diffs} diffs"
    );

    times.sort();
    let p95 = times[(times.len() * 95).div_ceil(100) - 1];
    let budget = if cfg!(debug_assertions) {
        FRAME_BUDGET_DEBUG
    } else {
        FRAME_BUDGET_RELEASE
    };
    eprintln!(
        "streaming {events} events in {} frames (first frame {first:?}): per-frame row update \
         median {:?}, p95 {p95:?}, max {:?} (budget {budget:?}); at most {max_items} items / {max_rows} rows rebuilt per \
         frame, {diff_builds} diff models for {diffs} diffs",
        times.len(),
        times[times.len() / 2],
        times[times.len() - 1],
    );
    assert!(p95 <= budget, "p95 {p95:?} exceeds {budget:?}");
}
