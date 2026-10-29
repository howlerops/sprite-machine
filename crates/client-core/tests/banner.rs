//! One banner state for the UI, from the subscription's and the outbox's connectivity
//! (connectivity.rs): offline if either is offline, connecting if either is connecting, else
//! online. [`Connection`] owns the two links and publishes that one state.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Mutex;

use client_core::cache::Cache;
use client_core::outbox::{
    Clock, FixedRandom, ManualClock, PostResponse, RetryPolicy, TransportError,
};
use client_core::{
    Applied, Connection, Connectivity, Intent, Outbox, Subscription, SubscriptionError, ThreadSync,
    Transport,
};
use futures_util::SinkExt;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

const ALL: [Connectivity; 3] = [
    Connectivity::Connecting,
    Connectivity::Online,
    Connectivity::Offline,
];

#[test]
fn offline_if_either_link_is_offline() {
    for other in ALL {
        assert_eq!(
            Connectivity::Offline.combine(other),
            Connectivity::Offline,
            "subscription offline, outbox {other:?}"
        );
        assert_eq!(
            other.combine(Connectivity::Offline),
            Connectivity::Offline,
            "outbox offline, subscription {other:?}"
        );
    }
}

#[test]
fn connecting_if_either_link_is_connecting_and_none_is_offline() {
    assert_eq!(
        Connectivity::Connecting.combine(Connectivity::Connecting),
        Connectivity::Connecting
    );
    assert_eq!(
        Connectivity::Connecting.combine(Connectivity::Online),
        Connectivity::Connecting
    );
    assert_eq!(
        Connectivity::Online.combine(Connectivity::Connecting),
        Connectivity::Connecting
    );
}

#[test]
fn online_only_when_both_links_are_online() {
    assert_eq!(
        Connectivity::Online.combine(Connectivity::Online),
        Connectivity::Online
    );
}

#[test]
fn the_combined_state_drives_one_banner() {
    let combined = Connectivity::Online.combine(Connectivity::Offline);
    assert_eq!(
        combined.banner(),
        Connectivity::Offline.banner(),
        "one banner text for the combined state"
    );
    assert_eq!(
        Connectivity::Online.combine(Connectivity::Online).banner(),
        None
    );
}

/// Answers each POST with the next scripted result; a POST with no script left fails.
struct Scripted(Mutex<VecDeque<Result<PostResponse, TransportError>>>);

impl Transport for Scripted {
    fn post(
        &self,
        _request: client_core::PostRequest,
    ) -> impl Future<Output = Result<PostResponse, TransportError>> + Send {
        let next = self
            .0
            .lock()
            .expect("script lock")
            .pop_front()
            .unwrap_or_else(|| Err(TransportError("script exhausted".to_owned())));
        async move { next }
    }
}

fn answered(status: u16) -> Result<PostResponse, TransportError> {
    Ok(PostResponse {
        status,
        body: serde_json::json!({}),
    })
}

fn cancel(task: &str) -> Intent {
    Intent::Cancel {
        thread_id: "th_1".to_owned(),
        task_id: task.to_owned(),
    }
}

const HELLO: &str = r#"{"type":"hello","head_seq":0,"replay_from":0}"#;
const LIVE: &str = r#"{"type":"live"}"#;

/// Records the banner of each published state change, after the initial one.
fn record(rx: &mut watch::Receiver<Connectivity>, seen: &mut Vec<Connectivity>) {
    if rx.has_changed().expect("owner alive") {
        seen.push(*rx.borrow_and_update());
    }
}

/// Drives the subscription until it reports live.
async fn until_live(owner: &mut Connection, cache: &mut Cache) {
    loop {
        if let Applied::Live = owner.next_persisted(cache).await.expect("reads") {
            return;
        }
    }
}

#[tokio::test]
async fn an_unreachable_hub_starts_offline_and_reconnect_brings_it_live() {
    // Reserve a local address, then free it: nothing listens there yet, so the connect fails.
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let addr = probe.local_addr().expect("address");
    drop(probe);
    let base = format!("http://{addr}");

    let mut cache = Cache::open_in_memory().expect("cache");
    let subscription = Subscription::connect_or_offline(&base, ThreadSync::new("th_1"), None)
        .await
        .expect("an unreachable hub is not an error");
    assert_eq!(subscription.connectivity(), Connectivity::Offline);
    let outbox = Outbox::load(&cache, RetryPolicy::default()).expect("loads");
    let mut owner = Connection::new(subscription, outbox);
    let mut rx = owner.watch_connectivity();
    let mut seen = vec![*rx.borrow_and_update()];
    assert_eq!(owner.connectivity(), Connectivity::Offline);
    assert_eq!(
        owner.connectivity().banner(),
        Some("Offline: changes will be sent when you reconnect")
    );

    // The hub comes up on that address: one socket says hello and live, then stays open.
    let listener = tokio::net::TcpListener::bind(addr).await.expect("rebinds");
    let hub = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accepts");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake");
        for frame in [HELLO, LIVE] {
            socket
                .send(Message::Text(frame.into()))
                .await
                .expect("sends");
        }
        std::future::pending::<()>().await;
    });

    // Reconnecting opens the socket: the subscription is connecting.
    owner.reconnect().await.expect("reconnects");
    record(&mut rx, &mut seen);
    assert_eq!(owner.connectivity(), Connectivity::Connecting);

    // Live: the subscription is online and nothing is queued, so the outbox has no opinion.
    until_live(&mut owner, &mut cache).await;
    assert_eq!(owner.subscription().connectivity(), Connectivity::Online);
    assert_eq!(owner.outbox().connectivity(), Connectivity::Connecting);
    record(&mut rx, &mut seen);
    assert_eq!(owner.connectivity(), Connectivity::Online);

    // A queued intent the hub has not answered yet: connecting, as before.
    let clock = ManualClock::new(0);
    let mut rng = FixedRandom(u64::MAX);
    owner
        .enqueue(&cache, cancel("tk_1"), clock.now_ms())
        .expect("enqueues");
    record(&mut rx, &mut seen);
    assert_eq!(owner.connectivity(), Connectivity::Connecting);

    // The hub answers it: the outbox is online, so the combined state is online.
    let answering = Scripted(Mutex::new(VecDeque::from([answered(202)])));
    owner
        .flush(&cache, &answering, &base, &clock, &mut rng)
        .await
        .expect("flushes");
    record(&mut rx, &mut seen);
    assert_eq!(owner.connectivity(), Connectivity::Online);

    hub.abort();
    assert_eq!(
        seen,
        vec![
            Connectivity::Offline,
            Connectivity::Connecting,
            Connectivity::Online,
            Connectivity::Connecting,
            Connectivity::Online,
        ],
        "offline, connecting, online when live and idle, connecting while a send is unanswered, online"
    );
}

#[tokio::test]
async fn the_owner_publishes_the_banner_sequence_as_either_link_changes() {
    // Hub: the first socket says hello and live, then closes. The second one (the reconnect)
    // says hello and live and stays open.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let hub = tokio::spawn(async move {
        {
            let (stream, _) = listener.accept().await.expect("accepts");
            let mut socket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("handshake");
            for frame in [HELLO, LIVE] {
                socket
                    .send(Message::Text(frame.into()))
                    .await
                    .expect("sends");
            }
            SinkExt::close(&mut socket).await.ok();
        }
        let (stream, _) = listener.accept().await.expect("accepts");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake");
        for frame in [HELLO, LIVE] {
            socket
                .send(Message::Text(frame.into()))
                .await
                .expect("sends");
        }
        std::future::pending::<()>().await;
    });

    let mut cache = Cache::open_in_memory().expect("cache");
    let subscription = Subscription::connect(&base, ThreadSync::new("th_1"), None)
        .await
        .expect("connects");
    let outbox = Outbox::load(&cache, RetryPolicy::default()).expect("loads");
    let mut owner = Connection::new(subscription, outbox);
    let mut rx = owner.watch_connectivity();
    let mut seen = vec![*rx.borrow_and_update()];
    let clock = ManualClock::new(0);
    let mut rng = FixedRandom(u64::MAX);

    // 1. Both links connecting.
    assert_eq!(owner.connectivity(), Connectivity::Connecting);

    // 2. The subscription goes live with nothing queued: the idle outbox has no opinion.
    until_live(&mut owner, &mut cache).await;
    record(&mut rx, &mut seen);
    assert_eq!(owner.connectivity(), Connectivity::Online);

    // 3. A queued intent with no answer yet: connecting again. Then the hub answers it.
    owner
        .enqueue(&cache, cancel("tk_1"), clock.now_ms())
        .expect("enqueues");
    record(&mut rx, &mut seen);
    assert_eq!(owner.connectivity(), Connectivity::Connecting);
    let answering = Scripted(Mutex::new(VecDeque::from([answered(202)])));
    let report = owner
        .flush(&cache, &answering, &base, &clock, &mut rng)
        .await
        .expect("flushes");
    assert_eq!(report.done, 1);
    record(&mut rx, &mut seen);
    assert_eq!(owner.connectivity(), Connectivity::Online);

    // 4. The hub stops answering the next intent: the outbox goes offline, the banner shows.
    owner
        .enqueue(&cache, cancel("tk_2"), clock.now_ms())
        .expect("enqueues");
    let down = Scripted(Mutex::new(VecDeque::from([Err(TransportError(
        "connection refused".to_owned(),
    ))])));
    let report = owner
        .flush(&cache, &down, &base, &clock, &mut rng)
        .await
        .expect("flushes");
    assert_eq!(report.transport_failures, 1);
    record(&mut rx, &mut seen);
    assert_eq!(owner.connectivity(), Connectivity::Offline);

    // 5. The socket closes: the subscription is offline too. Still one offline state.
    let closed = owner.next_persisted(&mut cache).await;
    assert!(
        matches!(closed, Err(SubscriptionError::Closed)),
        "{closed:?}"
    );
    record(&mut rx, &mut seen);
    assert_eq!(owner.connectivity(), Connectivity::Offline);

    // 6. The retry is due and the hub answers, so the outbox is online again. The socket is
    // still closed, so the combined state stays offline and nothing is published.
    clock.advance(500);
    let answering = Scripted(Mutex::new(VecDeque::from([answered(202)])));
    let report = owner
        .flush(&cache, &answering, &base, &clock, &mut rng)
        .await
        .expect("flushes");
    assert_eq!(report.done, 1);
    assert_eq!(owner.outbox().connectivity(), Connectivity::Online);
    record(&mut rx, &mut seen);
    assert_eq!(owner.connectivity(), Connectivity::Offline);

    // 7. The subscription reconnects: connecting again, then live: online.
    owner.reconnect().await.expect("reconnects");
    record(&mut rx, &mut seen);
    assert_eq!(owner.connectivity(), Connectivity::Connecting);
    until_live(&mut owner, &mut cache).await;
    record(&mut rx, &mut seen);
    assert_eq!(owner.connectivity(), Connectivity::Online);

    hub.abort();
    let banners: Vec<Option<&str>> = seen.iter().map(|state| state.banner()).collect();
    assert_eq!(
        seen,
        vec![
            Connectivity::Connecting,
            Connectivity::Online,
            Connectivity::Connecting,
            Connectivity::Online,
            Connectivity::Offline,
            Connectivity::Connecting,
            Connectivity::Online,
        ],
        "one entry per change of the combined state"
    );
    assert_eq!(
        banners,
        vec![
            Some("Connecting..."),
            None,
            Some("Connecting..."),
            None,
            Some("Offline: changes will be sent when you reconnect"),
            Some("Connecting..."),
            None,
        ]
    );
}

#[tokio::test]
async fn an_idle_outbox_follows_the_subscription_and_a_failed_send_still_shows_offline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let hub = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accepts");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake");
        for frame in [HELLO, LIVE] {
            socket
                .send(Message::Text(frame.into()))
                .await
                .expect("sends");
        }
        std::future::pending::<()>().await;
    });
    let mut cache = Cache::open_in_memory().expect("cache");
    let subscription = Subscription::connect(&base, ThreadSync::new("th_1"), None)
        .await
        .expect("connects");
    let outbox = Outbox::load(&cache, RetryPolicy::default()).expect("loads");
    let mut owner = Connection::new(subscription, outbox);
    let clock = ManualClock::new(0);
    let mut rng = FixedRandom(u64::MAX);

    // Live and idle: no banner, although no HTTP request has ever been made.
    until_live(&mut owner, &mut cache).await;
    assert_eq!(owner.connectivity(), Connectivity::Online);
    assert_eq!(owner.connectivity().banner(), None);

    // A send that fails: offline.
    owner
        .enqueue(&cache, cancel("tk_1"), clock.now_ms())
        .expect("enqueues");
    let down = Scripted(Mutex::new(VecDeque::from([Err(TransportError(
        "connection refused".to_owned(),
    ))])));
    owner
        .flush(&cache, &down, &base, &clock, &mut rng)
        .await
        .expect("flushes");
    assert_eq!(owner.connectivity(), Connectivity::Offline);
    hub.abort();
}
