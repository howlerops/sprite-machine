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
use client_gpui::shell::i18n::{self, Catalog};
use client_gpui::task::tools::{activity_rows, tool_rows};
use client_gpui::task::transcript::{
    MAX_BODY_LINES, MAX_DIFF_LINES, MAX_TERMINAL_LINES, MORE_ID, PANE_ID, ROW_HEIGHT, ROW_ID,
    WRAP_COLS, sanitize, thread_rows, thread_rows_expanded, wrap_lines,
};
use client_gpui::task::{Row, RowKind, TranscriptPane};
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
    assert!(diff.len() <= MAX_DIFF_LINES + 2, "{}", diff.len());
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
        // Rows far below the viewport are not built at all.
        assert!(
            laid_out
                .iter()
                .all(|row| row.label() != Some(labels[300].as_str()))
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
