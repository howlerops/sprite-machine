//! M3-011: desktop approval cards. Model logic natively (injected clock, outbox), the widgets and
//! the transcript pane headlessly.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use client_core::outbox::{
    Clock, FixedRandom, Intent, IntentState, ManualClock, Outbox, PostRequest, PostResponse,
    RetryPolicy, Transport, TransportError,
};
use client_core::view::LocalAnswer;
use client_core::{Cache, ThreadView};
use client_gpui::approval::model::{MAX_PENDING_LINES, actor_text};
use client_gpui::approval::{
    AnswerError, ApprovalCard, ApprovalController, CardAction, CardHandler, CardState,
    LocalPresets, Pending, format_remaining, parse_rfc3339_ms, render_cards,
};
use client_gpui::shell::i18n::Catalog;
use client_gpui::task::TranscriptPane;
use client_gpui::task::transcript::PANE_ID;
use gpui_kit::test::{TestSupportExt as _, TestWindowExt as _};
use gpui_kit::{
    AppContext as _, Bounds, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    Point, Render, Styled as _, TestAppContext, Window, WindowBounds, WindowOptions, div, px, size,
};
use protocol::tunnel::Outcome;
use protocol::turn_event::{Envelope, PermissionOptionKind};
use serde_json::{Value, json};

/// 2026-10-27T22:00:16Z, when the example interrupt is raised.
const RAISED_MS: u64 = 1_793_138_416_000;
/// 2026-10-27T22:10:00Z, its `expires_at`.
const EXPIRES_MS: u64 = 1_793_139_000_000;

// ---- fixtures --------------------------------------------------------------------------------

fn envelope(seq: u64, event: Value) -> Envelope {
    serde_json::from_value(json!({
        "v": 1, "seq": seq, "thread_id": "th_1", "task_id": "tk_1", "run_id": "run_1",
        "ts": "2026-10-27T22:00:16.000Z", "event": event,
    }))
    .expect("envelope")
}

fn options_json() -> Value {
    json!([
        { "option_id": "allow", "name": "Allow", "kind": "allow_once" },
        { "option_id": "always", "name": "Always allow", "kind": "allow_always" },
        { "option_id": "reject", "name": "Deny", "kind": "reject_once" },
        { "option_id": "never", "name": "Never", "kind": "reject_always" },
    ])
}

fn interrupt_event(options: Value) -> Value {
    json!({
        "type": "Interrupt", "interrupt_id": "int_1", "kind": "approval",
        "title": "Allow edit to src/main.rs?", "message": "The agent wants to change a file",
        "expires_at": "2026-10-27T22:10:00.000Z", "tool_call_id": "tc_1", "options": options,
    })
}

fn diff_event() -> Value {
    json!({
        "type": "Activity", "activity_id": "tc_1:src/main.rs", "activity_type": "diff",
        "mode": "snapshot",
        "content": { "tool_call_id": "tc_1", "path": "src/main.rs",
            "old_text": "let app = Router::new();\n",
            "new_text": "let app = Router::new().route(\"/healthz\", get(ok));\n" },
    })
}

fn resolved_event(outcome: Value, by: Value) -> Value {
    json!({ "type": "InterruptResolved", "interrupt_id": "int_1", "outcome": outcome,
        "resolved_by": by })
}

fn jacob() -> Value {
    json!({ "kind": "user", "id": "usr_1", "name": "Jacob", "device_id": "dev_phone" })
}

fn thread_with(events: Vec<Value>) -> ThreadView {
    let mut thread = ThreadView::new("th_1");
    for (n, event) in events.into_iter().enumerate() {
        thread
            .apply(&envelope(n as u64 + 1, event))
            .expect("event applies");
    }
    thread
}

fn open_thread() -> ThreadView {
    thread_with(vec![diff_event(), interrupt_event(options_json())])
}

fn controller(clock: &Arc<ManualClock>, presets: LocalPresets) -> ApprovalController {
    let clock: Arc<dyn Clock> = clock.clone();
    ApprovalController::new("th_1", "/work/repo", clock, presets)
}

fn only_card(controller: &mut ApprovalController, thread: &ThreadView) -> ApprovalCard {
    let mut cards = controller.cards(thread);
    assert_eq!(cards.len(), 1, "{cards:?}");
    cards.remove(0)
}

fn outbox() -> (Cache, Outbox) {
    let cache = Cache::open_in_memory().expect("cache");
    let outbox = Outbox::load(&cache, RetryPolicy::default()).expect("outbox");
    (cache, outbox)
}

/// Records every POST; answers with `status` and `body`.
struct Hub {
    seen: Mutex<Vec<PostRequest>>,
    status: u16,
    body: Value,
}

impl Transport for Hub {
    fn post(
        &self,
        request: PostRequest,
    ) -> impl Future<Output = Result<PostResponse, TransportError>> + Send {
        self.seen.lock().expect("lock").push(request);
        let response = PostResponse {
            status: self.status,
            body: self.body.clone(),
        };
        async move { Ok(response) }
    }
}

fn flush(outbox: &mut Outbox, cache: &Cache, hub: &Hub, clock: &ManualClock) {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
        .block_on(async {
            outbox
                .flush(cache, hub, "http://hub.test/v1", clock, &mut FixedRandom(1))
                .await
                .expect("flush");
        });
}

// ---- criterion: exact ACP options ------------------------------------------------------------

#[test]
fn card_has_exactly_the_options_the_agent_sent_in_order() {
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let mut controller = controller(&clock, LocalPresets::in_memory());
    let card = only_card(&mut controller, &open_thread());
    let got: Vec<_> = card
        .options
        .iter()
        .map(|o| (o.option_id.as_str(), o.name.as_str(), o.kind))
        .collect();
    assert_eq!(
        got,
        vec![
            ("allow", "Allow", PermissionOptionKind::AllowOnce),
            ("always", "Always allow", PermissionOptionKind::AllowAlways),
            ("reject", "Deny", PermissionOptionKind::RejectOnce),
            ("never", "Never", PermissionOptionKind::RejectAlways),
        ]
    );
    assert!(card.options.iter().all(|o| o.enabled));
    assert_eq!(
        card.options[1].a11y, "Always allow (allow always)",
        "the spoken label names the kind"
    );
    // No option is invented for an interrupt that offers fewer.
    let two = thread_with(vec![interrupt_event(json!([
        { "option_id": "ok", "name": "OK", "kind": "allow_once" },
        { "option_id": "no", "name": "No", "kind": "reject_once" },
    ]))]);
    let card = only_card(&mut controller, &two);
    assert_eq!(card.options.len(), 2);
}

#[test]
fn form_and_auth_interrupts_get_no_card() {
    let thread = thread_with(vec![json!({
        "type": "Interrupt", "interrupt_id": "int_f", "kind": "form", "title": "Pick",
        "expires_at": "2026-10-27T22:10:00.000Z",
    })]);
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    assert!(
        controller(&clock, LocalPresets::in_memory())
            .cards(&thread)
            .is_empty()
    );
}

// ---- criterion: embedded pending diff / command ----------------------------------------------

#[test]
fn card_embeds_the_pending_diff_of_its_tool_call() {
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let card = only_card(
        &mut controller(&clock, LocalPresets::in_memory()),
        &open_thread(),
    );
    assert_eq!(card.pending.len(), 1);
    let Pending::Diff {
        path,
        new_file,
        lines,
        hidden,
        ..
    } = &card.pending[0]
    else {
        panic!("{:?}", card.pending);
    };
    assert_eq!(path, "src/main.rs");
    assert!(!new_file);
    assert_eq!(*hidden, 0);
    let shown: Vec<String> = lines
        .iter()
        .map(|l| format!("{} {}", l.sign, l.text))
        .collect();
    assert_eq!(
        shown,
        vec![
            "- let app = Router::new();".to_owned(),
            "+ let app = Router::new().route(\"/healthz\", get(ok));".to_owned(),
        ]
    );
}

#[test]
fn a_diff_of_another_tool_call_is_not_embedded_and_long_diffs_are_capped() {
    let long: String = (0..100).map(|n| format!("line {n}\n")).collect();
    let thread = thread_with(vec![
        json!({ "type": "Activity", "activity_id": "other", "activity_type": "diff",
            "mode": "snapshot",
            "content": { "tool_call_id": "tc_other", "path": "x.rs", "old_text": null,
                "new_text": "x" } }),
        json!({ "type": "Activity", "activity_id": "tc_1:big", "activity_type": "diff",
            "mode": "snapshot",
            "content": { "tool_call_id": "tc_1", "path": "big.txt", "old_text": null,
                "new_text": long } }),
        interrupt_event(options_json()),
    ]);
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let card = only_card(&mut controller(&clock, LocalPresets::in_memory()), &thread);
    assert_eq!(card.pending.len(), 1, "only this tool call's diff");
    let Pending::Diff {
        path,
        new_file,
        lines,
        hidden,
        ..
    } = &card.pending[0]
    else {
        panic!("{:?}", card.pending);
    };
    assert_eq!(path, "big.txt");
    assert!(*new_file);
    assert_eq!(lines.len(), MAX_PENDING_LINES);
    assert_eq!(
        *hidden,
        100 - MAX_PENDING_LINES,
        "100 lines, the trailing newline is not a line"
    );
}

#[test]
fn card_embeds_the_pending_command_and_strips_control_characters() {
    let thread = thread_with(vec![
        json!({ "type": "Activity", "activity_id": "term_9", "activity_type": "terminal",
            "mode": "snapshot",
            "content": { "terminal_id": "term_9", "tool_call_id": "tc_1",
                "command": "rm -rf target\u{1b}[31m && cargo build", "cwd": "/work/repo" } }),
        interrupt_event(options_json()),
    ]);
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let card = only_card(&mut controller(&clock, LocalPresets::in_memory()), &thread);
    assert_eq!(
        card.pending,
        vec![Pending::Command {
            command: "rm -rf target[31m && cargo build".to_owned(),
            cwd: Some("/work/repo".to_owned()),
            hidden: 0,
        }]
    );
}

#[test]
fn the_command_falls_back_to_the_tool_calls_raw_input() {
    let thread = thread_with(vec![
        json!({ "type": "ToolCallStart", "tool_call_id": "tc_1", "title": "Run tests",
            "kind": "execute", "status": "pending" }),
        json!({ "type": "ToolCallArgs", "tool_call_id": "tc_1",
            "raw_input": { "command": "cargo test" } }),
        interrupt_event(options_json()),
    ]);
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let card = only_card(&mut controller(&clock, LocalPresets::in_memory()), &thread);
    assert_eq!(
        card.pending,
        vec![Pending::Command {
            command: "cargo test".to_owned(),
            cwd: None,
            hidden: 0,
        }]
    );
}

// ---- criterion: time remaining ---------------------------------------------------------------

#[test]
fn time_remaining_follows_the_injected_clock_and_expiry_disables_the_buttons() {
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let mut controller = controller(&clock, LocalPresets::in_memory());
    let thread = open_thread();
    let card = only_card(&mut controller, &thread);
    assert_eq!(card.expires_at_ms, Some(EXPIRES_MS));
    assert_eq!(card.remaining_ms, Some(EXPIRES_MS - RAISED_MS));
    assert_eq!(card.remaining_text.as_deref(), Some("9:44 left"));
    assert!(card.is_answerable());

    clock.advance(4 * 60_000 + 30_500);
    let card = only_card(&mut controller, &thread);
    assert_eq!(card.remaining_text.as_deref(), Some("5:14 left"));

    clock.set(EXPIRES_MS - 1);
    let card = only_card(&mut controller, &thread);
    assert_eq!(card.remaining_text.as_deref(), Some("0:01 left"));
    assert!(card.is_answerable());

    clock.set(EXPIRES_MS);
    let card = only_card(&mut controller, &thread);
    assert_eq!(card.state, CardState::Expired);
    assert_eq!(card.remaining_text.as_deref(), Some("0:00 left"));
    assert!(card.options.iter().all(|o| !o.enabled));

    let (cache, mut outbox) = outbox();
    let mut thread = thread;
    let refused = controller.answer(&mut thread, &mut outbox, &cache, "int_1", "allow");
    assert!(matches!(refused, Err(AnswerError::Expired)), "{refused:?}");
    assert_eq!(
        outbox.entries().count(),
        0,
        "nothing is queued for an expired request"
    );
}

#[test]
fn time_formats_and_parsing() {
    assert_eq!(format_remaining(600_000), "10:00 left");
    assert_eq!(format_remaining(3_725_000), "1:02:05 left");
    assert_eq!(format_remaining(59_001), "1:00 left");
    assert_eq!(
        parse_rfc3339_ms("2026-10-27T22:10:00.000Z"),
        Some(EXPIRES_MS)
    );
    assert_eq!(parse_rfc3339_ms("2026-10-27T22:10:00Z"), Some(EXPIRES_MS));
    assert_eq!(
        parse_rfc3339_ms("2026-10-28T00:10:00.5+02:00"),
        Some(EXPIRES_MS + 500)
    );
    assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(
        parse_rfc3339_ms("2024-02-29T00:00:00Z"),
        Some(1_709_164_800_000)
    );
    for bad in [
        "",
        "tomorrow",
        "2026-13-01T00:00:00Z",
        "2026-10-27 22:10",
        "2026-10-27T22:10:00",
    ] {
        assert_eq!(parse_rfc3339_ms(bad), None, "{bad:?}");
    }
}

#[test]
fn an_unparseable_expiry_shows_no_countdown_but_still_answers() {
    let mut event = interrupt_event(options_json());
    event["expires_at"] = json!("soon");
    let thread = thread_with(vec![event]);
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let card = only_card(&mut controller(&clock, LocalPresets::in_memory()), &thread);
    assert_eq!(card.remaining_text, None);
    assert!(card.is_answerable());
}

// ---- criterion: resolver and time shown ------------------------------------------------------

#[test]
fn a_resolved_card_shows_who_resolved_it_and_when() {
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let mut controller = controller(&clock, LocalPresets::in_memory());
    let mut thread = open_thread();
    assert!(only_card(&mut controller, &thread).is_answerable());

    clock.set(RAISED_MS + 63_000); // 22:01:19
    thread
        .apply(&envelope(
            3,
            resolved_event(json!({ "type": "selected", "option_id": "allow" }), jacob()),
        ))
        .expect("resolved");
    let card = only_card(&mut controller, &thread);
    assert_eq!(
        card.state,
        CardState::Resolved {
            summary: "Allow by Jacob (dev_phone) at 22:01:19 UTC".to_owned(),
            who: "Jacob (dev_phone)".to_owned(),
        }
    );
    assert_eq!(card.remaining_text, None, "no countdown once answered");
    assert!(card.options.iter().all(|o| !o.enabled));

    // The time is stable, and the hub's own time replaces the first-seen time.
    clock.set(RAISED_MS + 900_000);
    controller.set_resolved_at("int_1", RAISED_MS + 1_000);
    let CardState::Resolved { summary, .. } = only_card(&mut controller, &thread).state else {
        panic!("not resolved");
    };
    assert_eq!(summary, "Allow by Jacob (dev_phone) at 22:00:17 UTC");
}

#[test]
fn system_and_policy_resolvers_are_named_and_expiry_is_reported() {
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let mut controller = controller(&clock, LocalPresets::in_memory());
    let mut thread = open_thread();
    thread
        .apply(&envelope(
            3,
            resolved_event(
                json!({ "type": "expired" }),
                json!({ "kind": "system", "id": "hub" }),
            ),
        ))
        .expect("resolved");
    let CardState::Resolved { summary, who } = only_card(&mut controller, &thread).state else {
        panic!("not resolved");
    };
    assert_eq!(who, "the system");
    assert_eq!(summary, "Expired by the system at 22:00:16 UTC");
    let policy = protocol::tunnel::Actor {
        kind: protocol::tunnel::ActorKind::Policy,
        id: "preset:read".to_owned(),
        name: None,
        device_id: None,
    };
    assert_eq!(actor_text(&policy), "a policy");
}

// ---- criterion: answer through the Outbox; 409 shows who won ---------------------------------

#[test]
fn the_answer_goes_through_the_outbox_with_the_options_own_id_and_an_idempotency_key() {
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let mut controller = controller(&clock, LocalPresets::in_memory());
    let mut thread = open_thread();
    let (cache, mut outbox) = outbox();

    let local = controller
        .answer(&mut thread, &mut outbox, &cache, "int_1", "reject")
        .expect("queued");
    let entry = outbox.get(local).expect("entry").clone();
    assert!(
        entry.idempotency_key.starts_with("idk_"),
        "{}",
        entry.idempotency_key
    );
    assert_eq!(entry.state, IntentState::Queued);
    assert_eq!(
        entry.intent,
        Intent::Approval {
            thread_id: "th_1".to_owned(),
            interrupt_id: "int_1".to_owned(),
            outcome: Outcome::Selected {
                option_id: "reject".to_owned()
            },
            resolved_by: None,
        }
    );
    assert_eq!(
        entry.intent.body(),
        json!({ "outcome": { "type": "selected", "option_id": "reject" } })
    );

    // The view shows the answer as on its way, and a second click does not queue another.
    assert_eq!(
        thread.open_interrupts()[0].local,
        Some(LocalAnswer::Pending)
    );
    let card = only_card(&mut controller, &thread);
    assert_eq!(card.state, CardState::Sending);
    assert!(card.options.iter().all(|o| !o.enabled));
    let again = controller.answer(&mut thread, &mut outbox, &cache, "int_1", "allow");
    assert!(
        matches!(again, Err(AnswerError::AlreadyAnswering)),
        "{again:?}"
    );
    assert_eq!(outbox.entries().count(), 1);

    // Delivery: one POST to the interrupt's path with the stored key and the option's id.
    let hub = Hub {
        seen: Mutex::new(Vec::new()),
        status: 200,
        body: json!({ "interrupt_id": "int_1", "resolved_by": jacob() }),
    };
    flush(&mut outbox, &cache, &hub, &clock);
    let seen = hub.seen.lock().expect("lock");
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].url,
        "http://hub.test/v1/threads/th_1/interrupts/int_1"
    );
    assert_eq!(seen[0].idempotency_key, entry.idempotency_key);
    assert_eq!(seen[0].body["outcome"]["option_id"], "reject");
}

#[test]
fn unknown_options_and_unknown_interrupts_are_refused_without_queuing() {
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let mut controller = controller(&clock, LocalPresets::in_memory());
    let mut thread = open_thread();
    let (cache, mut outbox) = outbox();
    let bad_option = controller.answer(&mut thread, &mut outbox, &cache, "int_1", "yolo");
    assert!(
        matches!(bad_option, Err(AnswerError::UnknownOption(_))),
        "{bad_option:?}"
    );
    let bad_id = controller.answer(&mut thread, &mut outbox, &cache, "int_zzz", "allow");
    assert!(
        matches!(bad_id, Err(AnswerError::UnknownInterrupt(_))),
        "{bad_id:?}"
    );
    thread
        .apply(&envelope(
            3,
            resolved_event(json!({ "type": "selected", "option_id": "allow" }), jacob()),
        ))
        .expect("resolved");
    let late = controller.answer(&mut thread, &mut outbox, &cache, "int_1", "allow");
    assert!(matches!(late, Err(AnswerError::NotOpen)), "{late:?}");
    assert_eq!(outbox.entries().count(), 0);
}

#[test]
fn a_409_shows_already_resolved_by_the_winner() {
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let mut controller = controller(&clock, LocalPresets::in_memory());
    let mut thread = open_thread();
    let (cache, mut outbox) = outbox();
    controller
        .answer(&mut thread, &mut outbox, &cache, "int_1", "allow")
        .expect("queued");
    clock.advance(5_000); // 22:00:21
    let hub = Hub {
        seen: Mutex::new(Vec::new()),
        status: 409,
        body: json!({ "error": "already_resolved", "resolved_by": jacob() }),
    };
    flush(&mut outbox, &cache, &hub, &clock);
    controller.sync(&outbox, &mut thread);

    let card = only_card(&mut controller, &thread);
    assert_eq!(
        card.state,
        CardState::AlreadyResolved {
            summary: "Already resolved by Jacob (dev_phone) at 22:00:21 UTC".to_owned(),
            who: "Jacob (dev_phone)".to_owned(),
        }
    );
    assert!(card.options.iter().all(|o| !o.enabled));
    assert_eq!(
        thread.open_interrupts().len(),
        0,
        "a 409 counts as answered"
    );
}

#[test]
fn a_refused_answer_shows_the_failure_and_can_be_retried() {
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let mut controller = controller(&clock, LocalPresets::in_memory());
    let mut thread = open_thread();
    let (cache, mut outbox) = outbox();
    controller
        .answer(&mut thread, &mut outbox, &cache, "int_1", "allow")
        .expect("queued");
    let hub = Hub {
        seen: Mutex::new(Vec::new()),
        status: 410,
        body: json!({ "error": "expired" }),
    };
    flush(&mut outbox, &cache, &hub, &clock);
    controller.sync(&outbox, &mut thread);
    let card = only_card(&mut controller, &thread);
    assert_eq!(
        card.state,
        CardState::Failed {
            message: "expired (HTTP 410)".to_owned()
        }
    );
    // The failed answer left the card unanswered on the hub, so a retry is allowed.
    controller
        .answer(&mut thread, &mut outbox, &cache, "int_1", "reject")
        .expect("retry queues a new answer");
    assert_eq!(outbox.entries().count(), 2);
}

// ---- criterion: allow_always offers the workspace preset -------------------------------------

#[test]
fn allow_always_offers_the_workspace_preset_and_other_kinds_do_not() {
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let mut controller = controller(&clock, LocalPresets::in_memory());
    let card = only_card(&mut controller, &open_thread());
    let offer = card.preset.expect("allow_always offers a preset");
    assert_eq!(offer.option_id, "always");
    assert_eq!(
        offer.label,
        "Save \"Always allow\" as this workspace's preset"
    );
    assert!(!offer.checked, "never on by default");

    let plain = thread_with(vec![interrupt_event(json!([
        { "option_id": "allow", "name": "Allow", "kind": "allow_once" },
        { "option_id": "reject", "name": "Deny", "kind": "reject_always" },
    ]))]);
    assert_eq!(only_card(&mut controller, &plain).preset, None);
}

#[test]
fn choosing_allow_always_with_the_offer_ticked_records_the_preset_locally() {
    let dir = std::env::temp_dir().join(format!("approval-presets-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let file = dir.join("presets.json");
    let _ = std::fs::remove_file(&file);

    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let mut controller = controller(&clock, LocalPresets::open(&file).expect("open"));
    let mut thread = open_thread();
    let (cache, mut outbox) = outbox();
    controller.set_remember_preset("int_1", true);
    assert!(
        only_card(&mut controller, &thread)
            .preset
            .expect("offer")
            .checked,
        "the tick shows on the card"
    );
    controller
        .answer(&mut thread, &mut outbox, &cache, "int_1", "always")
        .expect("queued");

    // The ACP option itself is what is sent; the preset is a local record.
    let entry = outbox.entries().next().expect("entry");
    assert_eq!(
        entry.intent.body()["outcome"]["option_id"],
        "always",
        "the hub gets the option's own id"
    );
    let saved = controller.presets().entries();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].workspace, "/work/repo");
    assert_eq!(saved[0].option_id, "always");
    assert_eq!(saved[0].tool_title, "Allow edit to src/main.rs?");
    assert!(!saved[0].synced, "no hub preset API yet");
    assert!(
        only_card(&mut controller, &thread)
            .preset
            .expect("offer")
            .saved
    );
    // It survives a restart.
    let reopened = LocalPresets::open(&file).expect("reopen");
    assert_eq!(reopened.entries(), controller.presets().entries());
    assert!(reopened.has("/work/repo", "always"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_preset_is_not_recorded_unless_ticked_or_for_other_option_kinds() {
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let mut controller = controller(&clock, LocalPresets::in_memory());
    let mut thread = open_thread();
    let (cache, mut outbox) = outbox();
    controller
        .answer(&mut thread, &mut outbox, &cache, "int_1", "always")
        .expect("queued, box not ticked");
    assert!(controller.presets().entries().is_empty());

    let mut controller = self::controller(&clock, LocalPresets::in_memory());
    let mut thread = open_thread();
    controller.set_remember_preset("int_1", true);
    controller
        .answer(&mut thread, &mut outbox, &cache, "int_1", "allow")
        .expect("allow once");
    assert!(
        controller.presets().entries().is_empty(),
        "ticked, but allow_once is not a preset"
    );
}

// ---- strings ---------------------------------------------------------------------------------

fn literals_in(source: &str) -> BTreeSet<String> {
    source
        .split('"')
        .skip(1)
        .step_by(2)
        .filter(|text| text.starts_with("approval-") && !text.contains(['{', ' ']))
        .map(str::to_owned)
        .collect()
}

/// Message ids passed to `tr(` / `tr_args(` (view.rs also has element ids starting `approval-`).
fn translated_in(source: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for call in ["tr(", "tr_args("] {
        let mut rest = source;
        while let Some(at) = rest.find(call) {
            rest = &rest[at + call.len()..];
            let trimmed = rest.trim_start();
            if let Some(after) = trimmed.strip_prefix('"')
                && let Some(end) = after.find('"')
            {
                out.insert(after[..end].to_owned());
            }
        }
    }
    out
}

#[test]
fn every_string_is_in_the_catalog_and_every_catalog_string_is_used() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/approval");
    let read = |name: &str| std::fs::read_to_string(src.join(name)).expect(name);
    let mut used = BTreeSet::new();
    for name in ["model.rs", "controller.rs", "mod.rs"] {
        used.extend(literals_in(&read(name)));
    }
    used.extend(translated_in(&read("view.rs")));
    used.extend(translated_in(&read("model.rs")));
    used.extend(translated_in(&read("controller.rs")));
    used.extend(translated_in(&read("mod.rs")));
    let defined: BTreeSet<String> = Catalog::ids(client_gpui::approval::EN_FTL)
        .into_iter()
        .collect();
    let catalog = Catalog::from_source("en", client_gpui::approval::EN_FTL).expect("parses");
    for id in &defined {
        assert!(catalog.has(id));
    }
    assert_eq!(
        used.difference(&defined).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "used but not defined"
    );
    // kind ids are built from a match, so they appear as literals too.
    assert_eq!(
        defined.difference(&used).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "defined but unused"
    );
}

// ---- headless widgets ------------------------------------------------------------------------

struct Host {
    cards: Vec<ApprovalCard>,
    actions: Rc<RefCell<Vec<CardAction>>>,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let actions = self.actions.clone();
        let handler: CardHandler = Rc::new(move |action, _, _| actions.borrow_mut().push(action));
        div()
            .id("host")
            .test_support()
            .size_full()
            .child(render_cards(&self.cards, Some(handler)))
    }
}

fn window_options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: Point::default(),
            size: size(px(900.), px(900.)),
        })),
        ..Default::default()
    }
}

#[gpui_kit::test]
fn the_card_widgets_show_every_part_and_clicks_carry_the_options_own_id(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let clock = Arc::new(ManualClock::new(RAISED_MS + 120_000));
    let card = only_card(
        &mut controller(&clock, LocalPresets::in_memory()),
        &open_thread(),
    );
    let actions = Rc::new(RefCell::new(Vec::new()));
    let (window, _host) = cx.update(|cx| {
        let actions = actions.clone();
        gpui_kit::open_window(window_options(), cx, move |_, cx| {
            cx.new(|_| Host {
                cards: vec![card],
                actions,
            })
        })
        .expect("window")
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("approval-title").label(),
            Some("Allow edit to src/main.rs?")
        );
        assert_eq!(window.find("approval-time-left").label(), Some("7:44 left"));
        assert!(window.find("approval-diff").visible());
        let lines: Vec<String> = window
            .find_all("approval-diff-line")
            .iter()
            .map(|l| l.label().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(
            lines,
            vec![
                "- let app = Router::new();".to_owned(),
                "+ let app = Router::new().route(\"/healthz\", get(ok));".to_owned()
            ]
        );
        for (id, label) in [
            ("approval-option-allow", "Allow (allow once)"),
            ("approval-option-always", "Always allow (allow always)"),
            ("approval-option-reject", "Deny (reject once)"),
            ("approval-option-never", "Never (reject always)"),
        ] {
            assert_eq!(window.find(id).label(), Some(label), "{id}");
        }
        assert!(window.find("approval-preset-offer").visible());

        window.click("approval-option-reject", cx);
        window.click("approval-option-always", cx);
        window.click("approval-preset", cx);
    })
    .expect("window is open");
    assert_eq!(
        *actions.borrow(),
        vec![
            CardAction::Answer {
                interrupt_id: "int_1".to_owned(),
                option_id: "reject".to_owned()
            },
            CardAction::Answer {
                interrupt_id: "int_1".to_owned(),
                option_id: "always".to_owned()
            },
            CardAction::SetPreset {
                interrupt_id: "int_1".to_owned(),
                checked: true
            },
        ]
    );
}

#[gpui_kit::test]
fn a_card_without_allow_always_has_no_preset_offer_and_an_answered_card_disables_clicks(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let mut controller = controller(&clock, LocalPresets::in_memory());
    let mut thread = thread_with(vec![interrupt_event(json!([
        { "option_id": "ok", "name": "OK", "kind": "allow_once" },
        { "option_id": "no", "name": "No", "kind": "reject_once" },
    ]))]);
    assert!(only_card(&mut controller, &thread).preset.is_none());
    thread
        .apply(&envelope(
            2,
            resolved_event(json!({ "type": "selected", "option_id": "ok" }), jacob()),
        ))
        .expect("resolved");
    let resolved = only_card(&mut controller, &thread);
    let actions = Rc::new(RefCell::new(Vec::new()));
    let (window, _host) = cx.update(|cx| {
        let actions = actions.clone();
        gpui_kit::open_window(window_options(), cx, move |_, cx| {
            cx.new(|_| Host {
                cards: vec![resolved],
                actions,
            })
        })
        .expect("window")
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("approval-preset-offer").is_none());
        assert_eq!(
            window.find("approval-status").label(),
            Some("OK by Jacob (dev_phone) at 22:00:16 UTC")
        );
        window.click("approval-option-ok", cx);
    })
    .expect("window is open");
    assert!(actions.borrow().is_empty(), "{:?}", actions.borrow());
}

// ---- the transcript pane ---------------------------------------------------------------------

#[gpui_kit::test]
fn the_pane_pins_open_cards_and_shows_resolver_and_time_in_the_transcript(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let mut controller = controller(&clock, LocalPresets::in_memory());
    let mut thread = open_thread();
    let (window, pane) = cx.update(|cx| {
        gpui_kit::open_window(window_options(), cx, |_, cx| {
            cx.new(|_| TranscriptPane::new())
        })
        .expect("window")
    });

    // Open: the card is pinned, and the transcript no longer carries the plain interrupt rows.
    let cards = controller.cards(&thread);
    cx.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_thread(&thread, cx);
            pane.set_approvals(cards, None, cx);
        })
    });
    let rows: Vec<String> = cx.update(|cx| {
        pane.read(cx)
            .rows()
            .iter()
            .map(|r| r.text.clone())
            .collect()
    });
    assert!(
        rows.contains(&"Approval needed: Allow edit to src/main.rs?".to_owned()),
        "{rows:?}"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.starts_with("Options:") || row.starts_with("Expires")),
        "the text rows are replaced by the card: {rows:?}"
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find(PANE_ID).visible());
        assert!(window.find("approval-card-int_1").visible());
        assert_eq!(window.find("approval-time-left").label(), Some("9:44 left"));
    })
    .expect("window is open");

    // Resolved: the card leaves the pinned area; who and when stay in the transcript.
    clock.advance(30_000);
    thread
        .apply(&envelope(
            4,
            resolved_event(json!({ "type": "selected", "option_id": "allow" }), jacob()),
        ))
        .expect("resolved");
    let cards = controller.cards(&thread);
    cx.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_thread(&thread, cx);
            pane.set_approvals(cards, None, cx);
        })
    });
    let rows: Vec<String> = cx.update(|cx| {
        pane.read(cx)
            .rows()
            .iter()
            .map(|r| r.text.clone())
            .collect()
    });
    assert!(
        rows.contains(&"Allow by Jacob (dev_phone) at 22:00:46 UTC".to_owned()),
        "{rows:?}"
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("approval-card-int_1").is_none());
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn clicking_in_the_pane_reaches_the_controller_and_the_outbox(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let state = Rc::new(RefCell::new((
        controller(&clock, LocalPresets::in_memory()),
        open_thread(),
        outbox(),
    )));
    let handler: CardHandler = {
        let state = state.clone();
        Rc::new(move |action, _, _| {
            let mut guard = state.borrow_mut();
            let (controller, thread, (cache, outbox)) = &mut *guard;
            match action {
                CardAction::Answer {
                    interrupt_id,
                    option_id,
                } => {
                    controller
                        .answer(thread, outbox, cache, &interrupt_id, &option_id)
                        .expect("queued");
                }
                CardAction::SetPreset {
                    interrupt_id,
                    checked,
                } => controller.set_remember_preset(&interrupt_id, checked),
            }
        })
    };
    let (window, pane) = cx.update(|cx| {
        gpui_kit::open_window(window_options(), cx, |_, cx| {
            cx.new(|_| TranscriptPane::new())
        })
        .expect("window")
    });
    {
        let mut guard = state.borrow_mut();
        let (controller, thread, _) = &mut *guard;
        let cards = controller.cards(thread);
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.set_thread(thread, cx);
                pane.set_approvals(cards, Some(handler.clone()), cx);
            })
        });
    }
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("approval-option-allow", cx);
    })
    .expect("window is open");

    let guard = state.borrow();
    let (_, thread, (_, outbox)) = &*guard;
    let entries: Vec<_> = outbox.entries().collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].intent.body()["outcome"],
        json!({ "type": "selected", "option_id": "allow" })
    );
    assert_eq!(
        thread.open_interrupts()[0].local,
        Some(LocalAnswer::Pending)
    );
}
