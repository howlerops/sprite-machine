//! Tests for the pure seq/ack state machines (docs/specs/tunnel.md §4). No IO: the hub and
//! the runner are modelled in memory, and a "reconnect" is a `hello`/`welcome` exchange.

use protocol::tunnel::{
    ACK_BATCH, DedupeWindow, Disposition, EVENT_ID_TTL_MS, IDEMPOTENCY_KEY_TTL_MS, Receiver,
    Sender, TunnelError, Verdict,
};

/// Body of an up `event` frame: only the dedupe key matters for these tests.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Up {
    event_id: String,
}

fn up(id: &str) -> Up {
    Up {
        event_id: id.to_owned(),
    }
}

/// The hub end of the up stream: the Machine DO receiver, the Thread DO append log, and the
/// resume-token state (§4.3). Tokens are single-use.
struct Hub {
    token: Option<String>,
    minted: u32,
    rx: Receiver,
    seen_event_ids: DedupeWindow,
    appended: Vec<String>,
}

impl Hub {
    fn new() -> Self {
        Self {
            token: None,
            minted: 0,
            rx: Receiver::fresh(),
            seen_event_ids: DedupeWindow::event_ids(),
            appended: Vec::new(),
        }
    }

    /// Answers a `hello`: `(resumed, new_token, last_up_seq)`.
    fn hello(&mut self, presented: Option<&str>) -> (bool, String, u64) {
        let resumed = matches!(
            (presented, self.token.as_deref()),
            (Some(p), Some(t)) if p == t
        );
        if !resumed {
            self.rx = Receiver::fresh();
        }
        self.minted += 1;
        let token = format!("rt_{}", self.minted);
        self.token = Some(token.clone());
        (resumed, token, self.rx.last())
    }

    /// Handles one up `event` frame and returns the cumulative ack to send back.
    fn recv(&mut self, seq: u64, body: &Up, now_ms: u64) -> u64 {
        if let Ok(Verdict::Process) = self.rx.on_frame(seq) {
            // Thread DO append: ignored if this event_id was already appended (§4.2).
            if self.seen_event_ids.observe(&body.event_id, now_ms) == Disposition::New {
                self.appended.push(body.event_id.clone());
            }
        }
        self.rx.last()
    }
}

#[test]
fn sender_numbers_gap_free_and_respects_max_inflight() {
    let mut tx = Sender::new(2);
    assert_eq!(tx.enqueue(up("e1")), 1);
    assert_eq!(tx.enqueue(up("e2")), 2);
    assert_eq!(tx.enqueue(up("e3")), 3);
    assert_eq!(tx.next_to_send(), Some((1, up("e1"))));
    assert_eq!(tx.next_to_send(), Some((2, up("e2"))));
    // Window of 2 is full until an ack arrives.
    assert_eq!(tx.next_to_send(), None);
    tx.on_ack(1).unwrap();
    assert_eq!(tx.next_to_send(), Some((3, up("e3"))));
    assert_eq!(tx.next_to_send(), None);
}

#[test]
fn cumulative_ack_releases_the_outbox_and_stale_acks_are_noops() {
    let mut tx = Sender::new(256);
    for id in ["e1", "e2", "e3"] {
        tx.enqueue(up(id));
    }
    while tx.next_to_send().is_some() {}
    tx.on_ack(2).unwrap();
    assert_eq!(tx.len(), 1);
    assert_eq!(tx.acked_upto(), 2);
    tx.on_ack(1).unwrap(); // stale: already covered by ack 2
    assert_eq!(tx.acked_upto(), 2);
    tx.on_ack(3).unwrap();
    assert!(tx.is_empty());
}

#[test]
fn nack_counts_as_ack_and_returns_the_rejected_body() {
    let mut tx = Sender::new(256);
    for id in ["e1", "e2", "e3"] {
        tx.enqueue(up(id));
    }
    while tx.next_to_send().is_some() {}

    // The hub rejects frame 2. It counts as acked, so frame 3 is not wedged behind it.
    let rejected = tx.on_nack(2).unwrap();
    assert_eq!(rejected, Some(up("e2")));
    assert_eq!(tx.acked_upto(), 2);
    assert_eq!(tx.len(), 1);
    tx.on_ack(3).unwrap();
    assert!(tx.is_empty());
}

#[test]
fn ack_for_a_seq_never_sent_is_a_protocol_error() {
    let mut tx = Sender::new(256);
    tx.enqueue(up("e1"));
    assert_eq!(
        tx.on_ack(1),
        Err(TunnelError::AckAhead {
            seq: 1,
            sent_upto: 0
        })
    );
}

#[test]
fn receiver_processes_in_order_drops_duplicates_and_rejects_gaps() {
    let mut rx = Receiver::fresh();
    assert_eq!(rx.on_frame(1), Ok(Verdict::Process));
    assert_eq!(rx.on_frame(2), Ok(Verdict::Process));
    // Duplicate: dropped, and `last` is re-acked.
    assert_eq!(rx.on_frame(2), Ok(Verdict::Duplicate { ack: 2 }));
    assert_eq!(rx.on_frame(1), Ok(Verdict::Duplicate { ack: 2 }));
    // Gap: protocol error (the caller closes with protocol_error).
    assert_eq!(
        rx.on_frame(4),
        Err(TunnelError::Gap {
            expected: 3,
            got: 4
        })
    );
    assert_eq!(rx.last(), 2);
    assert_eq!(rx.on_frame(0), Err(TunnelError::ZeroSeq));
}

#[test]
fn receiver_acks_in_batches_of_fifty() {
    let mut rx = Receiver::fresh();
    for seq in 1..ACK_BATCH {
        rx.on_frame(seq).unwrap();
    }
    assert!(!rx.ack_due());
    rx.on_frame(ACK_BATCH).unwrap();
    assert!(rx.ack_due());
    assert_eq!(rx.take_ack(), Some(ACK_BATCH));
    assert!(!rx.ack_due());
    assert_eq!(rx.take_ack(), None);
}

#[test]
fn resumed_reconnect_delivers_exactly_once() {
    let mut tx: Sender<Up> = Sender::new(256);
    for id in ["e1", "e2", "e3", "e4", "e5"] {
        tx.enqueue(up(id));
    }
    let mut hub = Hub::new();

    // Connection 1: first hello has no token, so the stream is fresh.
    let (resumed, token, last_up) = hub.hello(None);
    assert!(!resumed);
    tx.on_welcome(resumed, last_up).unwrap();

    // Send frames 1..=3. The acks for 1 and 2 reach the runner. The ack for 3 is lost.
    for _ in 0..3 {
        let (seq, body) = tx.next_to_send().expect("frame to send");
        let ack = hub.recv(seq, &body, 0);
        if seq <= 2 {
            tx.on_ack(ack).unwrap();
        }
    }
    assert_eq!(tx.acked_upto(), 2);

    // Connection drops. Reconnect presenting the resume token: resumed: true.
    let (resumed, token2, last_up) = hub.hello(Some(&token));
    assert!(resumed);
    assert_eq!(last_up, 3, "hub durably processed 1..=3");
    tx.on_welcome(resumed, last_up).unwrap();
    assert_eq!(
        tx.acked_upto(),
        3,
        "frame 3 is dropped from the outbox, not resent"
    );

    // Frames 4 and 5 go out and are acked.
    while let Some((seq, body)) = tx.next_to_send() {
        let ack = hub.recv(seq, &body, 1);
        tx.on_ack(ack).unwrap();
    }
    assert!(tx.is_empty());

    // A retransmitted frame 3 is a duplicate on the hub and is not appended twice.
    hub.recv(3, &up("e3"), 2);
    assert_eq!(hub.appended, ["e1", "e2", "e3", "e4", "e5"]);

    // The token was single-use: presenting the old one again yields a fresh stream.
    let (resumed_again, _, _) = hub.hello(Some(&token));
    assert!(!resumed_again);
    assert_ne!(token, token2);
}

#[test]
fn fresh_stream_renumbers_the_outbox_and_dedupes_by_event_id() {
    let mut tx: Sender<Up> = Sender::new(256);
    for id in ["e1", "e2", "e3", "e4", "e5"] {
        tx.enqueue(up(id));
    }
    let mut hub = Hub::new();
    let (_, _, _) = hub.hello(None);
    tx.on_welcome(false, 0).unwrap();

    // Old stream: the hub processed 1..=4, but only the acks for 1 and 2 got back.
    for _ in 0..4 {
        let (seq, body) = tx.next_to_send().unwrap();
        let ack = hub.recv(seq, &body, 0);
        if seq <= 2 {
            tx.on_ack(ack).unwrap();
        }
    }
    assert_eq!(tx.len(), 3, "e3, e4, e5 are still unacked");

    // Connection drops and the runner's resume token is no longer valid: resumed: false.
    let (resumed, _, last_up) = hub.hello(Some("rt_stale"));
    assert!(!resumed);
    assert_eq!(last_up, 0);
    tx.on_welcome(resumed, last_up).unwrap();

    // The outbox restarts at 1, in the same order with the same event_ids.
    assert_eq!(tx.next_to_send(), Some((1, up("e3"))));
    assert_eq!(tx.next_to_send(), Some((2, up("e4"))));
    assert_eq!(tx.next_to_send(), Some((3, up("e5"))));
    assert_eq!(tx.next_seq(), 4);

    // The hub dedupes e3 and e4 by event_id, so only e5 is new.
    hub.recv(1, &up("e3"), 1);
    hub.recv(2, &up("e4"), 1);
    hub.recv(3, &up("e5"), 1);
    assert_eq!(hub.appended, ["e1", "e2", "e3", "e4", "e5"]);
}

#[test]
fn replayed_commands_apply_once_across_hub_retries_and_fresh_streams() {
    // Runner side of the down stream: a Receiver for seq plus a DedupeWindow for the
    // command's idempotency_key (§3.5, §4.4).
    /// Delivers one command frame to the runner and applies it if it is new.
    fn deliver(
        rx: &mut Receiver,
        keys: &mut DedupeWindow,
        applied: &mut Vec<String>,
        seq: u64,
        key: &str,
    ) {
        let Ok(Verdict::Process) = rx.on_frame(seq) else {
            return;
        };
        if keys.observe(key, 0) == Disposition::New {
            applied.push(key.to_owned());
        }
    }

    let mut rx = Receiver::fresh();
    let mut keys = DedupeWindow::idempotency_keys();
    let mut applied: Vec<String> = Vec::new();

    // The hub sends k1 (seq 1) and k2 (seq 2), and the runner processes both.
    deliver(&mut rx, &mut keys, &mut applied, 1, "k1");
    deliver(&mut rx, &mut keys, &mut applied, 2, "k2");
    // A hub retry of k1 gets a new seq (3). Same key, so no second effect.
    deliver(&mut rx, &mut keys, &mut applied, 3, "k1");
    assert_eq!(applied, ["k1", "k2"]);

    // Fresh stream after a reconnect: the hub renumbers its queue from 1 and replays k1, k2
    // and then sends k3. The runner's stream is fresh too, so the seqs are accepted and the
    // keys dedupe the replays.
    let mut rx = Receiver::fresh();
    deliver(&mut rx, &mut keys, &mut applied, 1, "k1");
    deliver(&mut rx, &mut keys, &mut applied, 2, "k2");
    deliver(&mut rx, &mut keys, &mut applied, 3, "k3");
    assert_eq!(applied, ["k1", "k2", "k3"]);
}

#[test]
fn dedupe_windows_expire_after_their_ttl() {
    let mut hub_ids = DedupeWindow::event_ids();
    assert_eq!(hub_ids.observe("evt_1", 0), Disposition::New);
    assert_eq!(
        hub_ids.observe("evt_1", EVENT_ID_TTL_MS - 1),
        Disposition::Duplicate
    );
    assert_eq!(hub_ids.observe("evt_1", EVENT_ID_TTL_MS), Disposition::New);

    let mut runner_keys = DedupeWindow::idempotency_keys();
    assert_eq!(runner_keys.observe("idk_1", 0), Disposition::New);
    assert_eq!(
        runner_keys.observe("idk_1", IDEMPOTENCY_KEY_TTL_MS - 1),
        Disposition::Duplicate
    );
    assert_eq!(
        runner_keys.observe("idk_1", IDEMPOTENCY_KEY_TTL_MS),
        Disposition::New
    );
}

#[test]
fn welcome_cursors_that_contradict_the_outbox_are_rejected() {
    let mut tx: Sender<Up> = Sender::new(256);
    tx.enqueue(up("e1"));
    tx.enqueue(up("e2"));
    // The hub claims to have processed seq 3, which the runner never enqueued.
    assert_eq!(
        tx.on_welcome(true, 3),
        Err(TunnelError::PeerCursorAhead {
            peer_cursor: 3,
            last_enqueued: 2
        })
    );
    // Ack 2 then claim a cursor of 1, which would un-ack a frame.
    while tx.next_to_send().is_some() {}
    tx.on_ack(2).unwrap();
    assert_eq!(
        tx.on_welcome(true, 1),
        Err(TunnelError::PeerCursorBehind {
            peer_cursor: 1,
            acked_upto: 2
        })
    );
}
