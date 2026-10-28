//! Headless approval card: clicking "Allow" posts the selected outcome to the sink and the card
//! shows who resolved it; a 409 from the sink shows "already resolved by ...". The sink is a fake
//! that records every request.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use gpui_kit::test::TestWindowExt;
use gpui_kit::{AppContext, Bounds, Point, TestAppContext, WindowBounds, WindowOptions, px, size};
use gpui_spike::approval_model::{
    AnswerError, AnswerRequest, ApprovalModel, FIXTURE_THREAD, fixture_interrupt,
};
use gpui_spike::approval_pane::{STATUS_ID, open_approval_card, option_button_id};
use gpui_spike::approval_sink::AnswerSink;
use protocol::turn_event::{Actor, ActorKind};
use serde_json::json;

/// Records every request and answers from a queue (an empty queue answers 200 by the fixture
/// user).
#[derive(Default)]
struct FakeSink {
    requests: Mutex<Vec<AnswerRequest>>,
    replies: Mutex<VecDeque<Result<Actor, AnswerError>>>,
}

impl FakeSink {
    fn with_replies(replies: Vec<Result<Actor, AnswerError>>) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into()),
        })
    }

    fn requests(&self) -> Vec<AnswerRequest> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl AnswerSink for FakeSink {
    fn answer(&self, request: &AnswerRequest) -> Result<Actor, AnswerError> {
        self.requests
            .lock()
            .expect("requests lock")
            .push(request.clone());
        self.replies
            .lock()
            .expect("replies lock")
            .pop_front()
            .unwrap_or_else(|| Ok(fixture_user()))
    }
}

fn fixture_user() -> Actor {
    Actor {
        kind: ActorKind::User,
        id: "usr_fixture".to_owned(),
        name: Some("Fixture user".to_owned()),
        device_id: Some("dev_fixture".to_owned()),
    }
}

fn other_device() -> Actor {
    Actor {
        kind: ActorKind::User,
        id: "usr_dana".to_owned(),
        name: Some("Dana (phone)".to_owned()),
        device_id: None,
    }
}

/// Five minutes before the fixture's expiry: every button is live.
fn model_and_now() -> (ApprovalModel, i64) {
    let interrupt = fixture_interrupt().expect("approval fixture");
    let model = ApprovalModel::from_interrupt(FIXTURE_THREAD, &interrupt).expect("approval");
    let now = model.expires_at_ms().expect("fixture expiry") - 300_000;
    (model, now)
}

fn options_640x400() -> WindowOptions {
    let bounds = Bounds {
        origin: Point::default(),
        size: size(px(640.), px(400.)),
    };
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        ..Default::default()
    }
}

#[gpui_kit::test]
fn approval_card_allow_posts_the_selected_outcome_and_names_the_resolver(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (model, now) = model_and_now();
    let sink = FakeSink::with_replies(vec![Ok(fixture_user())]);
    let (window, _card) = cx.update(|cx| {
        open_approval_card(options_640x400(), model, sink.clone(), now, cx)
            .expect("open test window")
    });

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(STATUS_ID).label(),
            Some("5m 0s left"),
            "the card shows time left while pending"
        );
        window.click(option_button_id("allow"), cx);
    })
    .expect("window is open");
    cx.run_until_parked();

    let requests = sink.requests();
    assert_eq!(requests.len(), 1, "one click, one answer");
    assert_eq!(requests[0].thread_id, FIXTURE_THREAD);
    assert_eq!(requests[0].interrupt_id, "int_00001");
    assert_eq!(
        requests[0].body,
        json!({"outcome": {"type": "selected", "option_id": "allow"}})
    );

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(STATUS_ID).label(),
            Some("Resolved by Fixture user")
        );
        window.click(option_button_id("reject"), cx);
        window.render_frame(cx);
    })
    .expect("window is open");
    cx.run_until_parked();
    assert_eq!(
        sink.requests().len(),
        1,
        "a resolved card takes no second answer"
    );
}

#[gpui_kit::test]
fn approval_card_shows_already_resolved_by_on_a_409(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (model, now) = model_and_now();
    let sink = FakeSink::with_replies(vec![Err(AnswerError::AlreadyResolved {
        resolved_by: other_device(),
    })]);
    let (window, _card) = cx.update(|cx| {
        open_approval_card(options_640x400(), model, sink.clone(), now, cx)
            .expect("open test window")
    });

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(option_button_id("reject"), cx);
    })
    .expect("window is open");
    cx.run_until_parked();

    let requests = sink.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].body,
        json!({"outcome": {"type": "selected", "option_id": "reject"}})
    );

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(STATUS_ID).label(),
            Some("Already resolved by Dana (phone)")
        );
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn approval_card_renders_the_summary_and_one_button_per_option(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (model, now) = model_and_now();
    let sink = FakeSink::with_replies(Vec::new());
    let (window, _card) = cx.update(|cx| {
        open_approval_card(options_640x400(), model, sink.clone(), now, cx)
            .expect("open test window")
    });

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("approval-card").visible());
        assert_eq!(
            window.find("approval-summary").label(),
            Some("Allow the fixture agent to edit src/routes.rs?")
        );
        assert!(window.find(option_button_id("allow")).visible());
        assert!(window.find(option_button_id("reject")).visible());
    })
    .expect("window is open");
    assert!(sink.requests().is_empty(), "rendering answers nothing");
}
