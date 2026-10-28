//! The outbound queue (client-api.md §3, §4; plan §4.4): backoff with full jitter, idempotency
//! keys that survive a restart, retries that reuse the key, and 409 / 422 handling. The clock
//! and the randomness are injected; the 409 and replay tests run against agui-fixture.

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use agui_fixture::{FixtureConfig, Scenario, start};
use client_core::cache::Cache;
use client_core::outbox::{
    FixedRandom, Intent, IntentState, ManualClock, Outbox, PostRequest, PostResponse, Random,
    RetryPolicy, Transport, TransportError, new_idempotency_key,
};
use client_core::{Applied, Subscription, ThreadSync};
use protocol::tunnel::{Actor, ActorKind, ContentPart, Outcome};
use protocol::turn_event::Event;
use serde_json::json;

/// A deterministic generator for spreading jitter draws in tests.
struct Lcg(u64);

impl Random for Lcg {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        if bound == 0 {
            0
        } else {
            (self.0 >> 33) % bound
        }
    }
}

/// A transport that answers from a script and records every request it was given.
struct Scripted {
    replies: Mutex<VecDeque<Result<PostResponse, TransportError>>>,
    seen: Mutex<Vec<PostRequest>>,
}

impl Scripted {
    fn new(replies: Vec<Result<PostResponse, TransportError>>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            seen: Mutex::new(Vec::new()),
        }
    }
}

impl Transport for Scripted {
    fn post(
        &self,
        request: PostRequest,
    ) -> impl Future<Output = Result<PostResponse, TransportError>> + Send {
        self.seen.lock().expect("lock").push(request);
        let reply = self
            .replies
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or_else(|| Err(TransportError("script exhausted".to_owned())));
        async move { reply }
    }
}

fn reply(status: u16, body: serde_json::Value) -> Result<PostResponse, TransportError> {
    Ok(PostResponse { status, body })
}

fn user(id: &str) -> Actor {
    Actor {
        kind: ActorKind::User,
        id: id.to_owned(),
        name: Some(id.to_owned()),
        device_id: Some(format!("dev_{id}")),
    }
}

fn prompt(thread: &str) -> Intent {
    Intent::Prompt {
        thread_id: thread.to_owned(),
        task_id: "tk_scripted".to_owned(),
        message_id: "msg_scripted".to_owned(),
        parts: vec![ContentPart::Text {
            text: "hello".to_owned(),
        }],
        machine_id: None,
        author: None,
    }
}

fn answer(thread: &str, interrupt: &str, option: &str, by: &Actor) -> Intent {
    Intent::Approval {
        thread_id: thread.to_owned(),
        interrupt_id: interrupt.to_owned(),
        outcome: Outcome::Selected {
            option_id: option.to_owned(),
        },
        resolved_by: Some(by.clone()),
    }
}

fn temp_db(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "client-core-{tag}-{}-{nanos}.sqlite",
        std::process::id()
    ))
}

#[test]
fn backoff_is_full_jitter_inside_a_capped_window() {
    let policy = RetryPolicy::default();
    for failures in 1..=20u32 {
        let window = (policy.base_ms << (failures - 1).min(32)).min(policy.cap_ms);
        assert_eq!(
            policy.delay_ms(failures, &mut FixedRandom(u64::MAX)),
            window,
            "the top of the window is the exponential value, capped"
        );
        assert_eq!(policy.delay_ms(failures, &mut FixedRandom(0)), 0);
    }

    let mut rng = Lcg(7);
    let mut spread = (u64::MAX, 0u64);
    for _ in 0..2_000 {
        let delay = policy.delay_ms(3, &mut rng);
        assert!(
            delay <= 2_000,
            "the third retry waits at most 2 s, got {delay}"
        );
        spread = (spread.0.min(delay), spread.1.max(delay));
    }
    assert!(
        spread.0 < 200 && spread.1 > 1_800,
        "full jitter spreads over the window: {spread:?}"
    );
}

#[test]
fn idempotency_key_is_generated_once_and_survives_a_restart() {
    let path = temp_db("keys");
    let (id, key) = {
        let cache = Cache::open(&path).expect("cache opens");
        let mut outbox = Outbox::load(&cache, RetryPolicy::default()).expect("loads");
        let id = outbox
            .enqueue(&cache, prompt("th_1"), 1_000)
            .expect("enqueues");
        let key = outbox.get(id).expect("entry").idempotency_key.clone();
        (id, key)
    };
    assert!(key.starts_with("idk_"), "keys carry the idk_ prefix: {key}");
    assert!(key.len() <= 128);

    // A new process loads the same queue: same entry, same key, same intent.
    let cache = Cache::open(&path).expect("cache reopens");
    let outbox = Outbox::load(&cache, RetryPolicy::default()).expect("loads after restart");
    let entry = outbox.get(id).expect("entry survives the restart");
    assert_eq!(entry.idempotency_key, key);
    assert_eq!(entry.intent, prompt("th_1"));
    assert_eq!(entry.state, IntentState::Queued);
    assert_eq!(entry.created_at_ms, 1_000);

    assert_ne!(
        new_idempotency_key(),
        new_idempotency_key(),
        "fresh keys differ"
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn transient_failures_retry_with_the_same_key_on_the_backoff_schedule() {
    let cache = Cache::open_in_memory().expect("cache");
    let mut outbox = Outbox::load(&cache, RetryPolicy::default()).expect("loads");
    let id = outbox
        .enqueue_with_key(&cache, "idk_fixed_1".to_owned(), prompt("th_1"), 0)
        .expect("enqueues");
    let transport = Scripted::new(vec![
        Err(TransportError("connection reset".to_owned())),
        reply(503, json!({"error": "busy"})),
        reply(202, json!({"task_id": "tk_scripted", "status": "started"})),
    ]);
    let clock = ManualClock::new(0);
    // The top of each window: the first retry waits 500 ms, the second 1000 ms.
    let mut rng = FixedRandom(u64::MAX);

    let first = outbox
        .flush(&cache, &transport, "http://hub", &clock, &mut rng)
        .await
        .expect("flush");
    assert_eq!(
        (first.sent, first.transport_failures, first.retrying),
        (1, 1, 1)
    );
    assert_eq!(outbox.get(id).expect("entry").next_attempt_at_ms, 500);

    let early = outbox
        .flush(&cache, &transport, "http://hub", &clock, &mut rng)
        .await
        .expect("flush");
    assert_eq!(early.sent, 0, "nothing is sent before the scheduled retry");

    clock.set(500);
    let second = outbox
        .flush(&cache, &transport, "http://hub", &clock, &mut rng)
        .await
        .expect("flush");
    assert_eq!(second.sent, 1);
    assert_eq!(outbox.get(id).expect("entry").next_attempt_at_ms, 1_500);

    clock.set(1_499);
    assert_eq!(
        outbox
            .flush(&cache, &transport, "http://hub", &clock, &mut rng)
            .await
            .expect("flush")
            .sent,
        0
    );
    clock.set(1_500);
    let third = outbox
        .flush(&cache, &transport, "http://hub", &clock, &mut rng)
        .await
        .expect("flush");
    assert_eq!(third.done, 1);

    let entry = outbox.get(id).expect("entry");
    assert!(matches!(entry.state, IntentState::Done { status: 202, .. }));
    let seen = transport.seen.lock().expect("lock");
    assert_eq!(seen.len(), 3, "one request per attempt");
    assert!(
        seen.iter()
            .all(|request| request.idempotency_key == "idk_fixed_1"),
        "every retry reuses the stored key"
    );
    assert!(
        seen.iter()
            .all(|request| request.url == "http://hub/threads/th_1/prompts")
    );
}

#[tokio::test]
async fn retry_budget_exhausts_and_other_4xx_are_terminal() {
    let policy = RetryPolicy {
        base_ms: 10,
        cap_ms: 10,
        max_failures: 2,
    };
    let cache = Cache::open_in_memory().expect("cache");
    let mut outbox = Outbox::load(&cache, policy).expect("loads");
    let flaky = outbox.enqueue(&cache, prompt("th_1"), 0).expect("enqueues");
    let refused = outbox
        .enqueue(&cache, answer("th_1", "int_1", "nope", &user("u")), 0)
        .expect("enqueues");
    // Pass 1: the flaky prompt times out, the refused approval gets 422.
    // Pass 2 (after the first backoff): the flaky prompt gets 503, which is its second failure.
    let transport = Scripted::new(vec![
        Err(TransportError("timeout".to_owned())),
        reply(422, json!({"error": "invalid_outcome"})),
        reply(503, json!({})),
    ]);
    let clock = ManualClock::new(0);
    let mut rng = FixedRandom(u64::MAX);

    let report = outbox
        .flush(&cache, &transport, "http://hub", &clock, &mut rng)
        .await
        .expect("flush");
    assert_eq!(
        report.sent, 2,
        "the flaky entry is retried later, not in this pass"
    );
    assert_eq!(report.rejected, 1, "422 is terminal at once");
    assert_eq!(report.transport_failures, 1);
    assert_eq!(report.retrying, 1);

    clock.set(10);
    outbox
        .flush(&cache, &transport, "http://hub", &clock, &mut rng)
        .await
        .expect("flush");

    assert_eq!(
        outbox.get(flaky).expect("entry").state,
        IntentState::Exhausted {
            last_error: "HTTP 503".to_owned()
        }
    );
    assert_eq!(
        outbox.get(refused).expect("entry").state,
        IntentState::Rejected {
            status: 422,
            error: "invalid_outcome".to_owned()
        }
    );
}

#[tokio::test]
async fn a_409_is_terminal_and_names_the_device_that_answered_first() {
    let fixture = start(FixtureConfig::new(Scenario::Approval))
        .await
        .expect("fixture starts");
    let base = format!("http://{}", fixture.addr());
    let thread = fixture.thread_id().to_owned();

    // Learn the interrupt the way the client does: from the subscription.
    let mut subscription = Subscription::connect(&base, ThreadSync::new(thread.clone()), None)
        .await
        .expect("connects");
    let interrupt_id = loop {
        let Applied::Event(envelope) = subscription.next().await.expect("read") else {
            continue;
        };
        if let Event::Interrupt(interrupt) = &envelope.event {
            break interrupt.interrupt_id.clone();
        }
    };

    let cache = Cache::open_in_memory().expect("cache");
    let mut outbox = Outbox::load(&cache, RetryPolicy::default()).expect("loads");
    let phone = user("phone");
    let laptop = user("laptop");
    let winner = outbox
        .enqueue(&cache, answer(&thread, &interrupt_id, "allow", &phone), 0)
        .expect("enqueues");
    let mut rng = FixedRandom(0);
    let clock = ManualClock::new(0);
    let transport = client_core::HttpTransport::new(None);

    let first = outbox
        .flush(&cache, &transport, &base, &clock, &mut rng)
        .await
        .expect("flush");
    assert_eq!(first.done, 1);
    assert!(matches!(
        outbox.get(winner).expect("entry").state,
        IntentState::Done { status: 200, .. }
    ));

    // The laptop answers the same approval a moment later: the hub says who won.
    let loser = outbox
        .enqueue(&cache, answer(&thread, &interrupt_id, "reject", &laptop), 0)
        .expect("enqueues");
    let second = outbox
        .flush(&cache, &transport, &base, &clock, &mut rng)
        .await
        .expect("flush");
    assert_eq!(second.conflicts, 1);
    assert_eq!(
        outbox.get(loser).expect("entry").state,
        IntentState::Conflict {
            status: 409,
            resolved_by: Some(phone.clone()),
        }
    );
    let conflicts = outbox.conflicts();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].interrupt_id, interrupt_id);
    assert_eq!(conflicts[0].resolved_by, Some(phone.clone()));

    // A 409 is never retried.
    clock.advance(3_600_000);
    assert_eq!(
        outbox
            .flush(&cache, &transport, &base, &clock, &mut rng)
            .await
            .expect("flush")
            .sent,
        0
    );

    // The view model shows the resolver on the approval.
    let view = subscription.sync_mut().view_mut();
    assert!(view.note_conflict(&interrupt_id, phone.clone()));
    let interrupt = view
        .tasks()
        .iter()
        .find_map(|task| task.transcript().interrupt(&interrupt_id))
        .expect("interrupt in the view");
    assert!(!interrupt.is_open());
    assert_eq!(interrupt.resolved_by(), Some(&phone));
}

#[tokio::test]
async fn a_replayed_key_gets_the_original_answer_without_a_second_effect() {
    let fixture = start(FixtureConfig::new(Scenario::Approval))
        .await
        .expect("fixture starts");
    let base = format!("http://{}", fixture.addr());
    let thread = fixture.thread_id().to_owned();
    let mut subscription = Subscription::connect(&base, ThreadSync::new(thread.clone()), None)
        .await
        .expect("connects");
    loop {
        let Applied::Event(envelope) = subscription.next().await.expect("read") else {
            continue;
        };
        if matches!(envelope.event, Event::Interrupt(_)) {
            break;
        }
    }

    // The app crashed after sending, before it recorded the answer. The restarted queue
    // resends with the same key: the hub replays its 200 instead of acting again.
    let cache = Cache::open_in_memory().expect("cache");
    let mut outbox = Outbox::load(&cache, RetryPolicy::default()).expect("loads");
    let by = user("usr_a");
    let key = "idk_replay_1".to_owned();
    let first = outbox
        .enqueue_with_key(
            &cache,
            key.clone(),
            answer(&thread, "int_00001", "allow", &by),
            0,
        )
        .expect("enqueues");
    let mut rng = FixedRandom(0);
    let clock = ManualClock::new(0);
    let transport = client_core::HttpTransport::new(None);
    outbox
        .flush(&cache, &transport, &base, &clock, &mut rng)
        .await
        .expect("flush");
    assert!(matches!(
        outbox.get(first).expect("entry").state,
        IntentState::Done { status: 200, .. }
    ));

    let again = outbox
        .enqueue_with_key(&cache, key, answer(&thread, "int_00001", "allow", &by), 0)
        .expect("enqueues");
    outbox
        .flush(&cache, &transport, &base, &clock, &mut rng)
        .await
        .expect("flush");
    assert!(
        matches!(
            outbox.get(again).expect("entry").state,
            IntentState::Done { status: 200, .. }
        ),
        "same key and body: the stored answer is replayed"
    );
    drop(subscription);
}
