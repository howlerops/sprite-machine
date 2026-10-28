//! Headless common workload (M1B-008b): one window holds the transcript, the 5,000-line diff, the
//! terminal and one pending approval card. The four panes are laid out with their ids, the diff
//! and the transcript are virtualized, the terminal receives its output over time, and clicking
//! Allow on the card reaches the approval fixture, which records the answer.
use std::time::{Duration, Instant};

use gpui_kit::test::TestWindowExt;
use gpui_kit::{AppContext, TestAppContext};
use gpui_spike::approval_pane::{CARD_ID, STATUS_ID, option_button_id};
use gpui_spike::common_workload::{ApprovalFixture, CommonInputs};
use gpui_spike::diff_pane::{LINE_ID as DIFF_LINE_ID, PANE_ID as DIFF_PANE_ID};
use gpui_spike::terminal_pane::PANE_ID as TERMINAL_PANE_ID;
use gpui_spike::transcript_pane::{PANE_ID as TRANSCRIPT_PANE_ID, ROW_ID as TRANSCRIPT_ROW_ID};
use gpui_spike::transcript_shell::{fixture_rows, open_shell_in, spike_window_options};
use protocol::turn_event::{Event, InterruptResolved, ResolvedOutcome};

/// Generous bound on rows laid out in the 1280 x 800 window (about 25 fit, plus overdraw). The
/// diff has 5,000 rows and the transcript far more.
const MAX_LAID_OUT: usize = 200;
/// The last line of the `terminal` fixture's output.
const TERMINAL_LAST_LINE: &str = "test case_300 ... ok ✓ (300 ms)";
/// How often the terminal's chunks arrive in the spike (its streaming interval).
const TERMINAL_STEP: Duration = Duration::from_millis(200);
/// Chunks of the `terminal` fixture's output (about 14), with room to spare.
const TERMINAL_STEPS: usize = 40;

#[gpui_kit::test]
fn common_workload_lays_out_all_four_panes_and_virtualizes_the_diff(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let approval = ApprovalFixture::start().expect("approval fixture");
    let inputs = CommonInputs::from_fixtures(&approval).expect("common inputs");
    let (window, _shell) = cx.update(|cx| {
        open_shell_in(
            spike_window_options(),
            fixture_rows(),
            None,
            None,
            Some(inputs),
            cx,
        )
        .expect("open test window")
    });

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);

        for id in [TRANSCRIPT_PANE_ID, DIFF_PANE_ID, TERMINAL_PANE_ID, CARD_ID] {
            assert!(window.find(id).visible(), "pane `{id}` is laid out");
        }

        let diff_rows = window.find_all(DIFF_LINE_ID);
        assert!(
            !diff_rows.is_empty() && diff_rows.len() <= MAX_LAID_OUT,
            "laid out {} of 5000 diff rows",
            diff_rows.len()
        );
        let transcript_rows = window.find_all(TRANSCRIPT_ROW_ID);
        assert!(
            !transcript_rows.is_empty() && transcript_rows.len() <= MAX_LAID_OUT,
            "laid out {} transcript rows",
            transcript_rows.len()
        );

        // The card is pending and counts down from the approval's TTL at window open.
        assert_eq!(window.find(STATUS_ID).label(), Some("10m 0s left"));
        assert!(window.find(option_button_id("allow")).visible());
        assert!(window.find(option_button_id("reject")).visible());
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn common_workload_terminal_receives_its_output_while_the_window_is_open(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let approval = ApprovalFixture::start().expect("approval fixture");
    let inputs = CommonInputs::from_fixtures(&approval).expect("common inputs");
    let (_window, shell) = cx.update(|cx| {
        open_shell_in(
            spike_window_options(),
            Vec::new(),
            None,
            None,
            Some(inputs),
            cx,
        )
        .expect("open test window")
    });

    let terminal = cx.update(|cx| {
        shell
            .read(cx)
            .common()
            .expect("common panes")
            .terminal()
            .clone()
    });
    let has_last_line = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            terminal
                .read(cx)
                .model()
                .rows()
                .iter()
                .any(|row| row.text() == TERMINAL_LAST_LINE)
        })
    };
    assert!(!has_last_line(cx), "no output before the stream starts");
    for _ in 0..TERMINAL_STEPS {
        cx.executor().advance_clock(TERMINAL_STEP);
        cx.run_until_parked();
    }
    assert!(
        has_last_line(cx),
        "the terminal shows the fixture's last line"
    );
}

#[gpui_kit::test]
fn common_workload_approval_is_answered_from_the_window_and_recorded_by_the_fixture(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let approval = ApprovalFixture::start().expect("approval fixture");
    let inputs = CommonInputs::from_fixtures(&approval).expect("common inputs");
    let (window, _shell) = cx.update(|cx| {
        open_shell_in(
            spike_window_options(),
            Vec::new(),
            None,
            None,
            Some(inputs),
            cx,
        )
        .expect("open test window")
    });

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(option_button_id("allow"), cx);
    })
    .expect("window is open");
    cx.run_until_parked();

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(STATUS_ID).label(),
            Some("Resolved by Fixture user")
        );
    })
    .expect("window is open");

    let resolved = recorded_resolution(&approval).expect("the fixture logs the resolution");
    assert_eq!(resolved.interrupt_id, "int_00001");
    assert_eq!(
        resolved.outcome,
        ResolvedOutcome::Selected {
            option_id: "allow".to_owned()
        }
    );
    assert_eq!(resolved.resolved_by.name.as_deref(), Some("Fixture user"));
}

/// The fixture's `InterruptResolved` for the approval, read from its subscription.
fn recorded_resolution(approval: &ApprovalFixture) -> Option<InterruptResolved> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let envelope = approval
            .feed()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .ok()?;
        if let Event::InterruptResolved(resolved) = envelope.event {
            return Some(resolved);
        }
    }
}
