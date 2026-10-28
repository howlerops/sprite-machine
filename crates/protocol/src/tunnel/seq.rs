//! Pure seq/ack state machines for one direction of a tunnel stream (§4.2–§4.4).
//!
//! Nothing here does IO. The caller persists the outbox and `last` before it sends or acks,
//! as §4.2 requires, and passes in what the wire reported.

use std::collections::VecDeque;

use super::TunnelError;

/// At most this many processed frames go unacked before an ack is due (§3.7). The 200 ms
/// timer is the caller's job.
pub const ACK_BATCH: u64 = 50;

/// Sending side of one stream (§4.2 sender rules).
///
/// The outbox holds every frame that is not yet acked, in `seq` order and gap-free: the
/// front entry is always `acked_upto + 1`.
#[derive(Debug, Clone)]
pub struct Sender<T> {
    outbox: VecDeque<(u64, T)>,
    /// The seq the next `enqueue` gets.
    next_seq: u64,
    /// Highest seq handed out by `next_to_send` on the current connection.
    sent_upto: u64,
    /// Highest seq acked (or nacked) by the peer on this stream.
    acked_upto: u64,
    max_inflight: u64,
}

impl<T> Sender<T> {
    /// A new stream whose unacked frames may not exceed `max_inflight` (default 256).
    pub fn new(max_inflight: u64) -> Self {
        Self {
            outbox: VecDeque::new(),
            next_seq: 1,
            sent_upto: 0,
            acked_upto: 0,
            max_inflight: max_inflight.max(1),
        }
    }

    /// Rebuilds a sender from a durable outbox (a restarted runner): `acked_upto` is the
    /// highest seq the peer acked, `next_seq` the seq the next `enqueue` gets, and `unacked`
    /// the outbox in seq order. Nothing counts as sent on the new connection yet. The window is
    /// the default 256 (§4.2).
    ///
    /// Panics if `unacked` is not gap-free from `acked_upto + 1` up to `next_seq - 1`, since a
    /// store that hands out such an outbox is corrupt.
    pub fn from_parts(acked_upto: u64, next_seq: u64, unacked: Vec<(u64, T)>) -> Self {
        for (i, (seq, _)) in unacked.iter().enumerate() {
            assert_eq!(
                *seq,
                acked_upto + 1 + i as u64,
                "outbox is not gap-free after the acked cursor"
            );
        }
        assert_eq!(
            next_seq,
            acked_upto + 1 + unacked.len() as u64,
            "next_seq does not follow the outbox"
        );
        Self {
            outbox: unacked.into(),
            next_seq,
            sent_upto: acked_upto,
            acked_upto,
            max_inflight: 256,
        }
    }

    /// Sets the window to the hub's `welcome.max_inflight` (at least 1). Frames already in
    /// flight stay in flight; no new frame goes out until the unacked count drops below it.
    pub fn set_max_inflight(&mut self, max_inflight: u64) {
        self.max_inflight = max_inflight.max(1);
    }

    /// Appends a frame body with the next seq and returns that seq. The caller writes the
    /// body to its durable outbox before this call returns to the wire layer.
    pub fn enqueue(&mut self, body: T) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.outbox.push_back((seq, body));
        seq
    }

    /// Frames still in the outbox (sent or not, unacked).
    pub fn len(&self) -> usize {
        self.outbox.len()
    }

    /// `true` when every enqueued frame has been acked.
    pub fn is_empty(&self) -> bool {
        self.outbox.is_empty()
    }

    /// The seq the next `enqueue` will get.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Highest seq the peer has acked on this stream.
    pub fn acked_upto(&self) -> u64 {
        self.acked_upto
    }

    /// Acknowledges every frame up to and including `seq` (cumulative, §4.2 step 3).
    ///
    /// Stale acks (at or below the current cursor) are no-ops. An ack for a seq that was
    /// never sent is a protocol error.
    pub fn on_ack(&mut self, seq: u64) -> Result<(), TunnelError> {
        self.release(seq).map(|_| ())
    }

    /// Records a nack. A nack counts as an ack for its seq (§3.7), so the stream is not wedged
    /// by one bad frame. Returns the rejected body if it was still in the outbox, so the caller
    /// can fail the affected task.
    pub fn on_nack(&mut self, seq: u64) -> Result<Option<T>, TunnelError> {
        self.release(seq)
    }

    /// Like [`Sender::on_ack`], with the sent-range check.
    fn release(&mut self, seq: u64) -> Result<Option<T>, TunnelError> {
        if seq > self.sent_upto {
            return Err(TunnelError::AckAhead {
                seq,
                sent_upto: self.sent_upto,
            });
        }
        if seq <= self.acked_upto {
            return Ok(None);
        }
        Ok(self.drop_through(seq))
    }

    /// Removes every outbox frame with `seq ≤ through` and advances `acked_upto`. Returns the
    /// body with exactly `seq == through`, if it was there.
    fn drop_through(&mut self, through: u64) -> Option<T> {
        let mut at_through = None;
        while self.outbox.front().is_some_and(|(seq, _)| *seq <= through) {
            let Some((seq, body)) = self.outbox.pop_front() else {
                break;
            };
            if seq == through {
                at_through = Some(body);
            }
        }
        self.acked_upto = self.acked_upto.max(through);
        at_through
    }

    /// Applies the `welcome` answer to a `hello` (§3.2, §4.3, §4.4).
    ///
    /// - `resumed == true`: drop what the peer has processed (`peer_cursor`) and resend the
    ///   rest from there, in order.
    /// - `resumed == false`: a fresh stream. Renumber the whole outbox from 1, keeping order
    ///   and `event_id`s, and resend it all. The peer dedupes by `event_id` (§4.4).
    pub fn on_welcome(&mut self, resumed: bool, peer_cursor: u64) -> Result<(), TunnelError> {
        if resumed {
            let last_enqueued = self.next_seq - 1;
            if peer_cursor > last_enqueued {
                return Err(TunnelError::PeerCursorAhead {
                    peer_cursor,
                    last_enqueued,
                });
            }
            if peer_cursor < self.acked_upto {
                return Err(TunnelError::PeerCursorBehind {
                    peer_cursor,
                    acked_upto: self.acked_upto,
                });
            }
            self.drop_through(peer_cursor);
            self.sent_upto = peer_cursor;
        } else {
            for (i, (seq, _)) in self.outbox.iter_mut().enumerate() {
                *seq = i as u64 + 1;
            }
            self.next_seq = self.outbox.len() as u64 + 1;
            self.acked_upto = 0;
            self.sent_upto = 0;
        }
        Ok(())
    }
}

impl<T: Clone> Sender<T> {
    /// The next frame to put on the wire, as `(seq, body)`, or `None` when the window is full
    /// or nothing is unsent. Call repeatedly to drain it.
    pub fn next_to_send(&mut self) -> Option<(u64, T)> {
        let inflight = self.sent_upto - self.acked_upto;
        if inflight >= self.max_inflight {
            return None;
        }
        // The outbox front is `acked_upto + 1`, so the unsent frame `sent_upto + 1` sits at
        // index `sent_upto - acked_upto`.
        let idx = usize::try_from(inflight).ok()?;
        let (seq, body) = self.outbox.get(idx)?;
        self.sent_upto = *seq;
        Some((*seq, body.clone()))
    }
}

/// Receiving side of one stream (§4.2 receiver rules).
///
/// `last` is the highest seq durably processed. The caller must persist the effect of a
/// `Process` verdict together with `last()` (in one transaction where possible) before it acks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Receiver {
    last: u64,
    last_acked: u64,
}

/// What to do with a frame that passed the sequence check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The next frame in order: apply it, then ack in due course.
    Process,
    /// Already processed: drop it and re-ack `ack` (the current `last`).
    Duplicate { ack: u64 },
}

impl Receiver {
    /// A fresh stream: nothing processed yet.
    pub fn fresh() -> Self {
        Self::default()
    }

    /// Continues a resumed stream, with `last` as the durably processed seq.
    pub fn resume(last: u64) -> Self {
        Self {
            last,
            last_acked: last,
        }
    }

    /// The highest seq durably processed.
    pub fn last(&self) -> u64 {
        self.last
    }

    /// Checks an incoming sequenced frame. A `Process` verdict advances `last`.
    ///
    /// `seq == 0` is an error. `seq ≤ last` is a duplicate. `seq > last + 1` is a gap, which
    /// the caller answers with `close{protocol_error}`.
    pub fn on_frame(&mut self, seq: u64) -> Result<Verdict, TunnelError> {
        if seq == 0 {
            return Err(TunnelError::ZeroSeq);
        }
        if seq <= self.last {
            return Ok(Verdict::Duplicate { ack: self.last });
        }
        if seq > self.last + 1 {
            return Err(TunnelError::Gap {
                expected: self.last + 1,
                got: seq,
            });
        }
        self.last = seq;
        Ok(Verdict::Process)
    }

    /// `true` when at least [`ACK_BATCH`] processed frames are waiting to be acked.
    pub fn ack_due(&self) -> bool {
        self.last - self.last_acked >= ACK_BATCH
    }

    /// The cumulative ack to send now, if `last` has moved since the last ack.
    pub fn take_ack(&mut self) -> Option<u64> {
        if self.last > self.last_acked {
            self.last_acked = self.last;
            Some(self.last)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Sender;

    #[test]
    fn from_parts_rebuilds_a_sender_from_a_durable_outbox() {
        let mut sender = Sender::from_parts(4, 8, vec![(5, "a"), (6, "b"), (7, "c")]);
        assert_eq!(sender.acked_upto(), 4);
        assert_eq!(sender.next_seq(), 8);
        assert_eq!(sender.len(), 3);
        // Nothing was sent on this connection: a resumed welcome drops the processed frames
        // and the rest goes out in order.
        sender.on_welcome(true, 5).unwrap();
        assert_eq!(sender.next_to_send(), Some((6, "b")));
        assert_eq!(sender.next_to_send(), Some((7, "c")));
        assert_eq!(sender.next_to_send(), None);
        assert_eq!(sender.enqueue("d"), 8);
        sender.on_ack(7).unwrap();
        assert_eq!(sender.next_to_send(), Some((8, "d")));
    }

    #[test]
    fn welcome_max_inflight_caps_the_unacked_frames_in_flight() {
        let mut sender = Sender::from_parts(0, 6, (1..=5).map(|n| (n, n)).collect());
        sender.set_max_inflight(2);
        sender.on_welcome(false, 0).unwrap();
        assert_eq!(sender.next_to_send(), Some((1, 1)));
        assert_eq!(sender.next_to_send(), Some((2, 2)));
        assert_eq!(sender.next_to_send(), None, "window of 2 is full");
        sender.on_ack(1).unwrap();
        assert_eq!(sender.next_to_send(), Some((3, 3)));
        assert_eq!(sender.next_to_send(), None);
        sender.on_ack(3).unwrap();
        assert_eq!(sender.next_to_send(), Some((4, 4)));
        assert_eq!(sender.next_to_send(), Some((5, 5)));
        assert_eq!(sender.next_to_send(), None);
        // A window of 0 is clamped to 1.
        sender.set_max_inflight(0);
        sender.on_ack(5).unwrap();
        let seq = sender.enqueue(6);
        sender.enqueue(7);
        assert_eq!(sender.next_to_send(), Some((seq, 6)));
        assert_eq!(sender.next_to_send(), None);
    }

    #[test]
    fn from_parts_then_a_fresh_welcome_renumbers_from_one() {
        let mut sender = Sender::from_parts(4, 7, vec![(5, "a"), (6, "b")]);
        sender.on_welcome(false, 0).unwrap();
        assert_eq!(sender.next_to_send(), Some((1, "a")));
        assert_eq!(sender.next_to_send(), Some((2, "b")));
        assert_eq!(sender.next_seq(), 3);
    }

    #[test]
    #[should_panic(expected = "gap-free")]
    fn from_parts_rejects_a_gap() {
        let _ = Sender::from_parts(0, 4, vec![(1, "a"), (3, "b")]);
    }
}
