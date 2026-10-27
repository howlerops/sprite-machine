//! Runner ↔ hub tunnel (SPEC-002, `docs/specs/tunnel.md`).
//!
//! Pure types and state machines only: no IO, no clocks, no randomness, so this module
//! builds for wasm32 (the hub). Callers pass times in as `now_ms` and persist state themselves.
//!
//! - [`Frame`] and its parts: serde types for every tunnel frame (§3).
//! - [`Command`]: the down-stream `command` payloads, tagged by `kind` (§3.5).
//! - [`Sender`] / [`Receiver`]: seq/ack bookkeeping for one stream (§4.2–§4.4).
//! - [`DedupeWindow`]: `event_id` (24 h) and `idempotency_key` (7 days) dedupe (§4.2, §4.4).

mod command;
mod dedupe;
mod frame;
mod seq;
mod types;

use std::fmt;

pub use command::Command;
pub use dedupe::{DedupeWindow, Disposition, EVENT_ID_TTL_MS, IDEMPOTENCY_KEY_TTL_MS};
pub use frame::{Capabilities, CloseCode, CommandErrorCode, Frame, NackCode, PresenceState};
pub use seq::{ACK_BATCH, Receiver, Sender, Verdict};
pub use types::{Actor, ActorKind, ContentPart, Outcome, Workspace};

/// Tunnel protocol version carried in `hello` / `welcome` (`proto`).
pub const PROTO_VERSION: u32 = 1;

/// `Sec-WebSocket-Protocol` for the tunnel upgrade (§2).
pub const SUBPROTOCOL: &str = "sm.tunnel.v1";

/// Largest accepted frame, in bytes of UTF-8 JSON (§2).
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// A violation of the seq/ack rules. Each one maps to `close{protocol_error}` on the wire
/// (§4.2), except [`TunnelError::AckAhead`] and the welcome cursor errors, which mean the
/// peer's state disagrees with ours and are handled the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelError {
    /// A sequenced frame carried `seq == 0`; numbering starts at 1.
    ZeroSeq,
    /// A sequenced frame skipped numbers (`seq > last + 1`): frames were lost.
    Gap { expected: u64, got: u64 },
    /// The peer acknowledged a seq this side has not sent.
    AckAhead { seq: u64, sent_upto: u64 },
    /// `welcome` reported a cursor past the last frame this side has enqueued.
    PeerCursorAhead {
        peer_cursor: u64,
        last_enqueued: u64,
    },
    /// `welcome` (resumed) reported a cursor below an ack this side already processed.
    PeerCursorBehind { peer_cursor: u64, acked_upto: u64 },
}

impl fmt::Display for TunnelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSeq => write!(f, "sequenced frame with seq 0"),
            Self::Gap { expected, got } => {
                write!(f, "sequence gap: expected seq {expected}, got {got}")
            }
            Self::AckAhead { seq, sent_upto } => {
                write!(f, "ack {seq} is ahead of sent seq {sent_upto}")
            }
            Self::PeerCursorAhead {
                peer_cursor,
                last_enqueued,
            } => write!(
                f,
                "peer cursor {peer_cursor} is ahead of last enqueued seq {last_enqueued}"
            ),
            Self::PeerCursorBehind {
                peer_cursor,
                acked_upto,
            } => write!(
                f,
                "peer cursor {peer_cursor} is behind acked seq {acked_upto}"
            ),
        }
    }
}

impl std::error::Error for TunnelError {}
