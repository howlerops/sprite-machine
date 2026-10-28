//! Subscription behaviour (turn-event.md §3, §6): dedupe by seq, gap detection, resume with
//! `?after=`, and cold restart from the cache. The pure tests need no network; the others run
//! against the agui-fixture subscription server on a free port.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agui_fixture::{FixtureConfig, RunningFixture, Scenario, start};
use client_core::cache::Cache;
use client_core::sync::{Incoming, parse_incoming};
use client_core::view::Item;
use client_core::{Applied, Subscription, SubscriptionError, ThreadSync};
use futures_util::SinkExt;
use protocol::tunnel::{Actor, ActorKind, ContentPart};
use protocol::turn_event::{
    AssistantRole, Envelope, Event, RunFinished, RunOutcome, RunReason, RunStarted,
    TextMessageContent, TextMessageStart, UserMessage,
};
use serde_json::json;

const THREAD: &str = "th_1";

fn user() -> Actor {
    Actor {
        kind: ActorKind::User,
        id: "usr_test".to_owned(),
        name: None,
        device_id: None,
    }
}

fn envelope(seq: u64, task: Option<&str>, run: Option<&str>, event: Event) -> Envelope {
    let mut envelope = Envelope::new(
        THREAD,
        task.map(str::to_owned),
        run.map(str::to_owned),
        "2026-10-27T22:00:00.000Z",
        event,
    );
    envelope.seq = seq;
    envelope
}

fn prompt(seq: u64) -> Envelope {
    envelope(
        seq,
        Some("tk_1"),
        None,
        Event::UserMessage(UserMessage {
            message_id: "msg_u1".to_owned(),
            parts: vec![ContentPart::Text {
                text: "hello".to_owned(),
            }],
            author: user(),
        }),
    )
}

fn run_started(seq: u64) -> Envelope {
    envelope(
        seq,
        Some("tk_1"),
        Some("run_1"),
        Event::RunStarted(RunStarted {
            reason: RunReason::Prompt,
            parent_run_id: None,
            agent_id: None,
            machine_id: None,
        }),
    )
}

fn text_start(seq: u64) -> Envelope {
    envelope(
        seq,
        Some("tk_1"),
        Some("run_1"),
        Event::TextMessageStart(TextMessageStart {
            message_id: "msg_a1".to_owned(),
            role: AssistantRole::Assistant,
        }),
    )
}

fn text_delta(seq: u64, delta: &str) -> Envelope {
    envelope(
        seq,
        Some("tk_1"),
        Some("run_1"),
        Event::TextMessageContent(TextMessageContent {
            message_id: "msg_a1".to_owned(),
            delta: delta.to_owned(),
        }),
    )
}

fn feed_event(sync: &mut ThreadSync, envelope: Envelope) -> Applied {
    sync.feed(Incoming::Event(Box::new(envelope)))
        .expect("message is in contract")
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
fn dedupes_by_seq_and_keeps_the_text_once() {
    let mut sync = ThreadSync::new(THREAD);
    assert!(matches!(
        feed_event(&mut sync, prompt(1)),
        Applied::Event(_)
    ));
    assert!(matches!(
        feed_event(&mut sync, run_started(2)),
        Applied::Event(_)
    ));
    assert!(matches!(
        feed_event(&mut sync, text_start(3)),
        Applied::Event(_)
    ));
    assert!(matches!(
        feed_event(&mut sync, text_delta(4, "Hi")),
        Applied::Event(_)
    ));

    // A replayed copy of seq 4 (the run-boundary replay after a reconnect) is dropped.
    assert_eq!(
        feed_event(&mut sync, text_delta(4, "Hi")),
        Applied::Duplicate { seq: 4 }
    );
    assert_eq!(sync.last_seq(), 4);

    let task = sync.view().task("tk_1").expect("task exists");
    let text = task
        .transcript()
        .items()
        .iter()
        .find_map(|item| match item {
            Item::Text(view) => Some(view.text.clone()),
            _ => None,
        })
        .expect("assistant text");
    assert_eq!(text, "Hi", "a duplicate must not append its delta twice");
}

#[test]
fn gap_asks_for_a_resubscribe_from_the_last_contiguous_seq() {
    let mut sync = ThreadSync::new(THREAD);
    feed_event(&mut sync, prompt(1));
    feed_event(&mut sync, run_started(2));

    // seq 3 is missing: seq 4 is not applied, and the caller resubscribes with ?after=2.
    assert_eq!(
        feed_event(&mut sync, text_delta(4, "late")),
        Applied::Gap { after: 2, got: 4 }
    );
    assert_eq!(sync.last_seq(), 2);

    // Resubscribing replays from the run boundary; the missing event then arrives in order.
    assert!(matches!(
        feed_event(&mut sync, text_start(3)),
        Applied::Event(_)
    ));
    assert!(matches!(
        feed_event(&mut sync, text_delta(4, "late")),
        Applied::Event(_)
    ));
    assert_eq!(sync.last_seq(), 4);
}

#[test]
fn parses_every_subscription_message_and_ignores_unknown_ones() {
    assert_eq!(
        parse_incoming(r#"{"type":"hello","head_seq":5,"replay_from":2}"#).expect("hello"),
        Some(Incoming::Hello {
            head_seq: 5,
            replay_from: 2
        })
    );
    let snapshot =
        parse_incoming(r#"{"type":"snapshot","at_seq":1,"states":{"usage":{"context_used":3}}}"#)
            .expect("snapshot")
            .expect("known type");
    match snapshot {
        Incoming::Snapshot { at_seq, states } => {
            assert_eq!(at_seq, 1);
            assert_eq!(states["usage"]["context_used"], json!(3));
        }
        other => panic!("expected a snapshot, got {other:?}"),
    }
    assert_eq!(
        parse_incoming(r#"{"type":"live"}"#).expect("live"),
        Some(Incoming::Live)
    );
    assert_eq!(
        parse_incoming(r#"{"type":"pong"}"#).expect("pong"),
        Some(Incoming::Pong)
    );
    assert_eq!(
        parse_incoming(r#"{"type":"from_the_future"}"#).expect("unknown"),
        None,
        "an unknown message type is ignored, not an error"
    );

    // An event whose type this build does not know still takes its seq (§2).
    let mut future = serde_json::to_value(prompt(1)).expect("envelope JSON");
    future["event"] = json!({"type": "FromTheFuture", "x": 1});
    let text = json!({"type": "event", "envelope": future}).to_string();
    let Some(Incoming::Event(parsed)) = parse_incoming(&text).expect("event") else {
        panic!("expected an event");
    };
    assert_eq!(parsed.event, Event::Unknown);
    let mut sync = ThreadSync::new(THREAD);
    assert!(matches!(
        sync.feed(Incoming::Event(parsed)).expect("applies"),
        Applied::Event(_)
    ));
    assert_eq!(sync.last_seq(), 1);
}

#[test]
fn snapshot_older_than_applied_is_kept_out_and_newer_one_is_refused() {
    let mut sync = ThreadSync::new(THREAD);
    feed_event(&mut sync, prompt(1));
    feed_event(&mut sync, run_started(2));

    let older = sync
        .feed(Incoming::Snapshot {
            at_seq: 1,
            states: serde_json::Map::new(),
        })
        .expect("older snapshot");
    assert_eq!(
        older,
        Applied::Snapshot {
            at_seq: 1,
            installed: false
        }
    );

    let newer = sync.feed(Incoming::Snapshot {
        at_seq: 9,
        states: serde_json::Map::new(),
    });
    assert!(
        newer.is_err(),
        "a snapshot ahead of the applied seq is out of contract"
    );
}

/// Reads messages until `pred` accepts one, collecting every new event into `log`.
async fn read_until(
    subscription: &mut Subscription,
    log: &mut Vec<Envelope>,
    pred: impl Fn(&Envelope) -> bool,
) -> Envelope {
    loop {
        if let Applied::Event(event) = subscription.next().await.expect("subscription read") {
            let envelope = *event;
            let hit = pred(&envelope);
            log.push(envelope.clone());
            if hit {
                return envelope;
            }
        }
    }
}

/// Answers the fixture's approval `int_00001` with `allow`, as the hub's answer route expects.
async fn answer_allow(fixture: &RunningFixture) {
    let http = reqwest::Client::new();
    let url = fixture.http_url(&format!(
        "/threads/{}/interrupts/int_00001",
        fixture.thread_id()
    ));
    let response = http
        .post(url)
        .header("Idempotency-Key", "idk_test_allow")
        .json(&json!({"outcome": {"type": "selected", "option_id": "allow"}}))
        .send()
        .await
        .expect("answer request");
    assert_eq!(response.status().as_u16(), 200, "first answer is accepted");
}

fn is_success(envelope: &Envelope) -> bool {
    matches!(
        &envelope.event,
        Event::RunFinished(RunFinished {
            outcome: RunOutcome::Success { .. }
        })
    )
}

fn assert_contiguous(log: &[Envelope]) {
    for (index, envelope) in log.iter().enumerate() {
        assert_eq!(
            envelope.seq,
            index as u64 + 1,
            "seq gap-free in the applied log"
        );
    }
}

#[tokio::test]
async fn resumes_mid_run_with_after_and_drops_the_replayed_prefix() {
    let fixture = start(FixtureConfig::new(Scenario::Approval))
        .await
        .expect("fixture starts");
    let base = format!("http://{}", fixture.addr());
    let mut subscription = Subscription::connect(&base, ThreadSync::new(fixture.thread_id()), None)
        .await
        .expect("connects");
    let mut log = Vec::new();
    let interrupt = read_until(&mut subscription, &mut log, |e| {
        matches!(e.event, Event::Interrupt(_))
    })
    .await;
    assert_eq!(subscription.sync().last_seq(), interrupt.seq);

    // Drop the socket mid-run and resubscribe from the last applied seq.
    subscription.reconnect().await.expect("resubscribes");
    assert_eq!(
        subscription.connected_after(),
        interrupt.seq,
        "resumes with ?after="
    );

    // The open run is replayed from its RunStarted: those copies are duplicates, not new.
    let mut duplicates = 0;
    loop {
        match subscription.next().await.expect("read after resume") {
            Applied::Duplicate { .. } => duplicates += 1,
            Applied::Event(event) => log.push(*event),
            Applied::Live => break,
            _ => {}
        }
    }
    assert!(
        duplicates > 0,
        "the open run's prefix was replayed and deduplicated"
    );

    // Answer on the HTTP API (never over the socket) and finish the task.
    answer_allow(&fixture).await;
    read_until(&mut subscription, &mut log, is_success).await;

    assert_contiguous(&log);
    let interrupt_view = subscription
        .view()
        .task("tk_00001")
        .and_then(|task| task.transcript().interrupt("int_00001"))
        .expect("interrupt in the transcript");
    assert!(
        !interrupt_view.is_open(),
        "the hub's InterruptResolved closed it"
    );
}

#[tokio::test]
async fn cold_restart_resumes_from_the_cached_seq_not_from_zero() {
    let fixture = start(FixtureConfig::new(Scenario::Approval))
        .await
        .expect("fixture starts");
    let base = format!("http://{}", fixture.addr());
    let thread = fixture.thread_id().to_owned();
    let path = temp_db("restart");

    // Session 1: persist everything up to the interrupt, then stop as if the app was killed.
    let stored_seq = {
        let mut cache = Cache::open(&path).expect("cache opens");
        let mut subscription = Subscription::connect(&base, ThreadSync::new(thread.clone()), None)
            .await
            .expect("connects");
        let mut log = Vec::new();
        let interrupt = read_until(&mut subscription, &mut log, |e| {
            matches!(e.event, Event::Interrupt(_))
        })
        .await;
        cache.append_events(&thread, &log).expect("persist");
        interrupt.seq
    };

    // Session 2: reopen the cache, rebuild the view from it, and resume from its last seq.
    let mut cache = Cache::open(&path).expect("cache reopens");
    assert_eq!(cache.last_seq(&thread).expect("last seq"), stored_seq);
    let restored = ThreadSync::restore(&thread, cache.events(&thread).expect("events"))
        .expect("stored log is contiguous");
    assert_eq!(restored.last_seq(), stored_seq);
    let mut subscription = Subscription::connect(&base, restored, None)
        .await
        .expect("connects");
    assert_eq!(
        subscription.connected_after(),
        stored_seq,
        "resumes from the cached seq"
    );
    assert_eq!(
        subscription.view().open_interrupts().len(),
        1,
        "the restored view still shows the open approval"
    );

    let mut log = Vec::new();
    loop {
        match subscription.next().await.expect("read after restart") {
            Applied::Live => break,
            Applied::Event(event) => log.push(*event),
            _ => {}
        }
    }
    answer_allow(&fixture).await;
    read_until(&mut subscription, &mut log, is_success).await;

    let new_events: Vec<Envelope> = log
        .into_iter()
        .filter(|envelope| envelope.seq > stored_seq)
        .collect();
    assert_eq!(new_events.first().map(|e| e.seq), Some(stored_seq + 1));
    cache
        .append_events(&thread, &new_events)
        .expect("persist the rest");
    let total = cache.events(&thread).expect("events");
    assert_eq!(total.len() as u64, subscription.sync().last_seq());
    assert_contiguous(&total);

    let _ = std::fs::remove_file(&path);
}

/// The hub's wire text of an event this build does not know. The spacing, key order and the
/// `}` and escaped quote inside a string are deliberate: only a verbatim copy reproduces them.
const UNKNOWN_ENVELOPE: &str = r#"{ "v": 1,  "seq": 2, "thread_id": "th_1", "task_id": null, "run_id": null, "ts": "2026-10-27T22:00:04.000Z", "event": { "type": "SomethingFromTheFuture", "note": "a } and \"quote\"", "x": [1, 2.50, "y"] } }"#;

#[tokio::test]
async fn a_failed_append_leaves_the_view_where_the_cache_is_and_the_next_event_is_not_a_gap() {
    // The first event is for another thread, so the cache refuses it (WrongThread) and stores
    // nothing. The view must not move on it, so the next correct event applies at seq 1.
    let mut foreign: serde_json::Value = serde_json::from_str(
        &serde_json::to_string(&envelope(
            1,
            Some("tk_1"),
            None,
            Event::UserMessage(UserMessage {
                message_id: "msg_u1".to_owned(),
                parts: vec![ContentPart::Text {
                    text: "hello".to_owned(),
                }],
                author: user(),
            }),
        ))
        .expect("serializes"),
    )
    .expect("parses");
    foreign["thread_id"] = json!("th_other");
    let foreign = foreign.to_string();
    let known = serde_json::to_string(&prompt(1)).expect("serializes");
    let frames = vec![
        r#"{"type":"hello","head_seq":2,"replay_from":1}"#.to_owned(),
        format!(r#"{{"type":"event","envelope":{foreign}}}"#),
        format!(r#"{{"type":"event","envelope":{known}}}"#),
        format!(r#"{{"type":"event","envelope":{UNKNOWN_ENVELOPE}}}"#),
        r#"{"type":"live"}"#.to_owned(),
    ];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let hub = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accepts");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake");
        for frame in frames {
            socket
                .send(tokio_tungstenite::tungstenite::Message::Text(frame.into()))
                .await
                .expect("sends");
        }
        std::future::pending::<()>().await;
    });

    let path = temp_db("append-first");
    let mut cache = Cache::open(&path).expect("opens");
    let mut subscription = Subscription::connect(&base, ThreadSync::new(THREAD), None)
        .await
        .expect("connects");

    let hello = subscription.next_persisted(&mut cache).await;
    assert!(matches!(hello, Ok(Applied::Hello { .. })), "{hello:?}");
    let refused = subscription.next_persisted(&mut cache).await;
    assert!(
        matches!(refused, Err(SubscriptionError::Cache(_))),
        "the cache refuses the foreign event: {refused:?}"
    );
    assert_eq!(
        subscription.sync().last_seq(),
        0,
        "the view did not advance past the cache"
    );
    assert_eq!(cache.last_seq(THREAD).expect("last seq"), 0);

    let mut events = 0;
    loop {
        match subscription
            .next_persisted(&mut cache)
            .await
            .expect("no gap after the failed append")
        {
            Applied::Event(_) => events += 1,
            Applied::Live => break,
            _ => {}
        }
    }
    hub.abort();
    assert_eq!(events, 2);
    assert_eq!(subscription.sync().last_seq(), 2);
    assert_eq!(
        cache.raw_events(THREAD).expect("raw events"),
        vec![known, UNKNOWN_ENVELOPE.to_owned()]
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_live_unknown_event_is_stored_as_its_envelope_text_and_survives_a_restart() {
    let known = serde_json::to_string(&prompt(1)).expect("serializes");
    let frames = vec![
        r#"{"type":"hello","head_seq":2,"replay_from":1}"#.to_owned(),
        format!(r#"{{"type":"event",   "envelope": {known}}}"#),
        format!(r#"{{ "envelope" : {UNKNOWN_ENVELOPE}, "type": "event" }}"#),
        r#"{"type":"live"}"#.to_owned(),
    ];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let hub = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accepts");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake");
        for frame in frames {
            socket
                .send(tokio_tungstenite::tungstenite::Message::Text(frame.into()))
                .await
                .expect("sends");
        }
        std::future::pending::<()>().await;
    });

    let path = temp_db("live-unknown");
    let mut cache = Cache::open(&path).expect("opens");
    let mut subscription = Subscription::connect(&base, ThreadSync::new(THREAD), None)
        .await
        .expect("connects");
    let mut events = 0;
    loop {
        match subscription
            .next_persisted(&mut cache)
            .await
            .expect("subscription read")
        {
            Applied::Event(_) => events += 1,
            Applied::Live => break,
            _ => {}
        }
    }
    hub.abort();
    assert_eq!(events, 2);
    drop(subscription);
    drop(cache);

    let restored = Cache::open(&path).expect("reopens");
    assert_eq!(
        restored.raw_events(THREAD).expect("raw events"),
        vec![known, UNKNOWN_ENVELOPE.to_owned()],
        "each stored text is the envelope exactly as the hub sent it"
    );
    assert_eq!(restored.last_seq(THREAD).expect("last seq"), 2);
    let _ = std::fs::remove_file(&path);
}
