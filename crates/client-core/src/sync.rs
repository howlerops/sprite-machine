//! The thread subscription (turn-event.md §6): `hello`, `snapshot`, `event`, `live`.
//!
//! Two layers:
//!
//! - [`ThreadSync`] is pure. It takes parsed [`Incoming`] messages and decides, per message,
//!   whether an event is new ([`Applied::Event`]), already applied ([`Applied::Duplicate`]),
//!   or skips a seq ([`Applied::Gap`], which means "resubscribe with `?after=` the last
//!   contiguous seq"). It owns the [`ThreadView`] reducer.
//! - [`Subscription`] is the WebSocket driver. It connects with `?after=<last seq>`, feeds
//!   every message to a [`ThreadSync`], reconnects on a gap, and reports connectivity. It never
//!   POSTs to catch up: the only write on the socket is `{"type":"ping"}`.

use futures_util::{SinkExt, StreamExt};
use protocol::turn_event::Envelope;
use serde_json::{Map, Value};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, header::AUTHORIZATION};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::cache::{Cache, CacheError};
use crate::connectivity::Connectivity;
use crate::view::{ThreadView, ViewError};

/// A malformed or out-of-contract subscription message.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("malformed subscription message: {0}")]
    Malformed(String),
    #[error("event with seq 0 on the subscription")]
    ZeroSeq,
    #[error("snapshot as of seq {at_seq} is ahead of applied seq {applied}")]
    SnapshotAhead { at_seq: u64, applied: u64 },
    #[error("cached event seq {got} does not follow seq {last}")]
    Gap { last: u64, got: u64 },
    #[error(transparent)]
    View(#[from] ViewError),
}

/// One parsed message from the hub (turn-event.md §6 table).
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Hello {
        head_seq: u64,
        replay_from: u64,
    },
    Snapshot {
        at_seq: u64,
        states: Map<String, Value>,
    },
    Event(Box<Envelope>),
    Live,
    Pong,
}

/// Parses one text frame. `Ok(None)` for a message type this client does not know: those are
/// ignored, so a newer hub does not break an older client.
pub fn parse_incoming(text: &str) -> Result<Option<Incoming>, SyncError> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| SyncError::Malformed(error.to_string()))?;
    let field = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_u64)
            .ok_or_else(|| SyncError::Malformed(format!("{text}: missing {name}")))
    };
    let incoming = match value.get("type").and_then(Value::as_str) {
        Some("hello") => Incoming::Hello {
            head_seq: field("head_seq")?,
            replay_from: field("replay_from")?,
        },
        Some("snapshot") => Incoming::Snapshot {
            at_seq: field("at_seq")?,
            states: value
                .get("states")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
        },
        Some("event") => {
            let envelope = value
                .get("envelope")
                .cloned()
                .ok_or_else(|| SyncError::Malformed("event without envelope".to_owned()))?;
            let envelope: Envelope = serde_json::from_value(envelope)
                .map_err(|error| SyncError::Malformed(error.to_string()))?;
            Incoming::Event(Box::new(envelope))
        }
        Some("live") => Incoming::Live,
        Some("pong") => Incoming::Pong,
        _ => return Ok(None),
    };
    Ok(Some(incoming))
}

/// What one message did.
#[derive(Debug, Clone, PartialEq)]
pub enum Applied {
    Hello {
        head_seq: u64,
        replay_from: u64,
    },
    /// `installed` is `false` when the snapshot is older than what this client has applied.
    Snapshot {
        at_seq: u64,
        installed: bool,
    },
    /// A new event, now applied to the view. Persist it.
    Event(Box<Envelope>),
    /// An event at or below the last applied seq: dropped.
    Duplicate {
        seq: u64,
    },
    /// `seq` skipped ahead. The event is not applied; resubscribe with `?after=after`.
    Gap {
        after: u64,
        got: u64,
    },
    Live,
    Pong,
    /// A frame that is not a subscription message (ping, binary, unknown type).
    Ignored,
}

/// Dedupe, gap detection and the view reducer for one thread.
#[derive(Debug, Clone)]
pub struct ThreadSync {
    view: ThreadView,
    head_seq: u64,
}

impl ThreadSync {
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            view: ThreadView::new(thread_id),
            head_seq: 0,
        }
    }

    /// Rebuilds the view from stored events (`seq` 1..n, contiguous) so that a restart can
    /// resume from the stored last seq.
    pub fn restore(
        thread_id: impl Into<String>,
        events: impl IntoIterator<Item = Envelope>,
    ) -> Result<Self, SyncError> {
        let mut sync = Self::new(thread_id);
        for envelope in events {
            let last = sync.last_seq();
            if envelope.seq != last + 1 {
                return Err(SyncError::Gap {
                    last,
                    got: envelope.seq,
                });
            }
            sync.view.apply(&envelope)?;
        }
        sync.head_seq = sync.last_seq();
        Ok(sync)
    }

    /// The `seq` of the last applied event: where the next subscription resumes (`?after=`).
    pub fn last_seq(&self) -> u64 {
        self.view.last_seq()
    }

    /// The hub's head seq as of the last `hello`.
    pub fn head_seq(&self) -> u64 {
        self.head_seq
    }

    pub fn view(&self) -> &ThreadView {
        &self.view
    }

    pub fn view_mut(&mut self) -> &mut ThreadView {
        &mut self.view
    }

    /// Applies one message. See [`Applied`].
    pub fn feed(&mut self, incoming: Incoming) -> Result<Applied, SyncError> {
        match incoming {
            Incoming::Hello {
                head_seq,
                replay_from,
            } => {
                self.head_seq = head_seq;
                Ok(Applied::Hello {
                    head_seq,
                    replay_from,
                })
            }
            Incoming::Snapshot { at_seq, states } => {
                let applied = self.last_seq();
                // The hub replays from at most `after + 1`, so its snapshot is never newer than
                // what this client asked for. A newer one would double-apply the deltas that
                // follow it, so it is out of contract.
                if at_seq > applied {
                    return Err(SyncError::SnapshotAhead { at_seq, applied });
                }
                // Same point: take the hub's copy. Older: ours was built in order and is newer,
                // so the replayed events below `applied` are dropped as duplicates and it stays.
                let installed = at_seq == applied;
                if installed {
                    self.view.install_snapshot(states);
                }
                Ok(Applied::Snapshot { at_seq, installed })
            }
            Incoming::Event(envelope) => {
                let seq = envelope.seq;
                if seq == 0 {
                    return Err(SyncError::ZeroSeq);
                }
                let last = self.last_seq();
                if seq <= last {
                    return Ok(Applied::Duplicate { seq });
                }
                if seq != last + 1 {
                    return Ok(Applied::Gap {
                        after: last,
                        got: seq,
                    });
                }
                self.view.apply(&envelope)?;
                self.head_seq = self.head_seq.max(seq);
                Ok(Applied::Event(envelope))
            }
            Incoming::Live => Ok(Applied::Live),
            Incoming::Pong => Ok(Applied::Pong),
        }
    }
}

/// The text of the `envelope` member of an `event` frame, exactly as it appears in `frame`.
/// `frame` must already be well-formed JSON (see [`parse_incoming`]). This walks the top-level
/// members and never re-serialises, so the bytes are the hub's.
fn envelope_text(frame: &str) -> Option<&str> {
    let bytes = frame.as_bytes();
    let mut at = skip_ws(bytes, 0);
    if bytes.get(at) != Some(&b'{') {
        return None;
    }
    at += 1;
    loop {
        at = skip_ws(bytes, at);
        if bytes.get(at) != Some(&b'"') {
            return None;
        }
        let key_end = string_end(bytes, at)?;
        let is_envelope = &frame[at + 1..key_end - 1] == "envelope";
        at = skip_ws(bytes, key_end);
        if bytes.get(at) != Some(&b':') {
            return None;
        }
        at = skip_ws(bytes, at + 1);
        let value_end = value_end(bytes, at)?;
        if is_envelope {
            return Some(&frame[at..value_end]);
        }
        at = skip_ws(bytes, value_end);
        if bytes.get(at) != Some(&b',') {
            return None;
        }
        at += 1;
    }
}

fn skip_ws(bytes: &[u8], mut at: usize) -> usize {
    while matches!(bytes.get(at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        at += 1;
    }
    at
}

/// The index just past the string whose opening quote is at `start`.
fn string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut at = start + 1;
    loop {
        match bytes.get(at)? {
            b'\\' => at += 2,
            b'"' => return Some(at + 1),
            _ => at += 1,
        }
    }
}

/// The index just past the JSON value that starts at `start`.
fn value_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut at = start;
    loop {
        match *bytes.get(at)? {
            b'"' => {
                at = string_end(bytes, at)?;
                if depth == 0 {
                    return Some(at);
                }
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' if depth == 0 => return Some(at),
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at + 1);
                }
            }
            b',' | b' ' | b'\t' | b'\n' | b'\r' if depth == 0 => return Some(at),
            _ => {}
        }
        at += 1;
    }
}

/// The subscription URL for `after` (turn-event.md §6). `base_url` is the hub's HTTP base
/// (which may include a path prefix); `http(s)` is mapped to `ws(s)`.
pub fn subscribe_url(
    base_url: &str,
    thread_id: &str,
    after: u64,
) -> Result<String, SubscriptionError> {
    let base = base_url.trim_end_matches('/');
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if base.starts_with("ws://") || base.starts_with("wss://") {
        base.to_owned()
    } else {
        return Err(SubscriptionError::Url(format!(
            "unsupported base URL {base_url}"
        )));
    };
    Ok(format!("{ws}/threads/{thread_id}/subscribe?after={after}"))
}

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// A failure of the subscription transport or of the stream it carried.
#[derive(Debug, thiserror::Error)]
pub enum SubscriptionError {
    #[error("subscription URL: {0}")]
    Url(String),
    #[error("subscription connect failed: {0}")]
    Connect(String),
    #[error("subscription read failed: {0}")]
    Read(String),
    /// The hub closed the socket. Reconnect with [`Subscription::reconnect`].
    #[error("subscription closed")]
    Closed,
    #[error(transparent)]
    Sync(#[from] SyncError),
    #[error(transparent)]
    Cache(#[from] CacheError),
}

/// A live subscription to one thread.
pub struct Subscription {
    base_url: String,
    auth: Option<String>,
    socket: Option<Socket>,
    sync: ThreadSync,
    connected_after: u64,
    connectivity: watch::Sender<Connectivity>,
}

impl Subscription {
    /// Connects with `?after=` the sync's last seq. `auth` is sent as a bearer token when set.
    pub async fn connect(
        base_url: &str,
        sync: ThreadSync,
        auth: Option<String>,
    ) -> Result<Self, SubscriptionError> {
        let (connectivity, _) = watch::channel(Connectivity::Connecting);
        let mut subscription = Self {
            base_url: base_url.to_owned(),
            auth,
            socket: None,
            sync,
            connected_after: 0,
            connectivity,
        };
        subscription.reconnect().await?;
        Ok(subscription)
    }

    /// Opens a new socket with `?after=` the last applied seq. Nothing is re-POSTed.
    pub async fn reconnect(&mut self) -> Result<(), SubscriptionError> {
        self.socket = None;
        self.set_connectivity(Connectivity::Connecting);
        let after = self.sync.last_seq();
        let url = subscribe_url(&self.base_url, self.sync.view().thread_id(), after)?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|error| SubscriptionError::Url(error.to_string()))?;
        if let Some(token) = &self.auth {
            let value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|error| SubscriptionError::Url(error.to_string()))?;
            request.headers_mut().insert(AUTHORIZATION, value);
        }
        match tokio_tungstenite::connect_async(request).await {
            Ok((socket, _)) => {
                self.socket = Some(socket);
                self.connected_after = after;
                Ok(())
            }
            Err(error) => {
                self.set_connectivity(Connectivity::Offline);
                Err(SubscriptionError::Connect(error.to_string()))
            }
        }
    }

    /// Reads and applies the next message. On a gap it reconnects from the last contiguous
    /// seq and returns [`Applied::Gap`]. On a closed socket it returns
    /// [`SubscriptionError::Closed`]; call [`Subscription::reconnect`] to continue.
    pub async fn next(&mut self) -> Result<Applied, SubscriptionError> {
        self.read(None).await
    }

    /// Like [`Subscription::next`], but a new event is appended to `cache` with
    /// [`Cache::append_raw`] before it is applied, as the envelope text the hub sent. If the
    /// append fails, the event is not applied, so the view never runs ahead of the cache, and
    /// the error is returned. The view checks the event first, so an event it refuses is not
    /// stored either. An event this build does not know survives a restart byte for
    /// byte.
    pub async fn next_persisted(
        &mut self,
        cache: &mut Cache,
    ) -> Result<Applied, SubscriptionError> {
        self.read(Some(cache)).await
    }

    /// The read behind [`Subscription::next`] and [`Subscription::next_persisted`].
    async fn read(&mut self, cache: Option<&mut Cache>) -> Result<Applied, SubscriptionError> {
        let socket = self.socket.as_mut().ok_or(SubscriptionError::Closed)?;
        let message = match socket.next().await {
            None => return Err(self.closed()),
            Some(Err(error)) => {
                let error = SubscriptionError::Read(error.to_string());
                self.socket = None;
                self.set_connectivity(Connectivity::Offline);
                return Err(error);
            }
            Some(Ok(message)) => message,
        };
        match message {
            Message::Text(body) => {
                let Some(incoming) = parse_incoming(body.as_str())? else {
                    return Ok(Applied::Ignored);
                };
                // Only the next event in line is new. The view must accept it before the cache
                // keeps it, so the cache never holds an event the view refused.
                if let (Some(cache), Incoming::Event(envelope)) = (cache, &incoming)
                    && envelope.seq == self.sync.last_seq() + 1
                {
                    self.sync.view().check(envelope).map_err(SyncError::from)?;
                    let text = envelope_text(body.as_str()).ok_or_else(|| {
                        SyncError::Malformed("event frame without an envelope".to_owned())
                    })?;
                    cache.append_raw(self.sync.view().thread_id(), &[text])?;
                }
                let applied = self.sync.feed(incoming)?;
                match applied {
                    Applied::Live => self.set_connectivity(Connectivity::Online),
                    Applied::Gap { .. } => self.reconnect().await?,
                    _ => {}
                }
                Ok(applied)
            }
            Message::Close(_) => Err(self.closed()),
            _ => Ok(Applied::Ignored),
        }
    }

    /// Sends the keep-alive `ping`; the answer arrives as [`Applied::Pong`].
    pub async fn ping(&mut self) -> Result<(), SubscriptionError> {
        let socket = self.socket.as_mut().ok_or(SubscriptionError::Closed)?;
        socket
            .send(Message::Text(r#"{"type":"ping"}"#.into()))
            .await
            .map_err(|error| SubscriptionError::Read(error.to_string()))
    }

    /// The thread view, as of the last applied event.
    pub fn view(&self) -> &ThreadView {
        self.sync.view()
    }

    pub fn sync(&self) -> &ThreadSync {
        &self.sync
    }

    pub fn sync_mut(&mut self) -> &mut ThreadSync {
        &mut self.sync
    }

    /// The `after` of the socket currently open (or last opened).
    pub fn connected_after(&self) -> u64 {
        self.connected_after
    }

    pub fn connectivity(&self) -> Connectivity {
        *self.connectivity.borrow()
    }

    /// A receiver that sees every connectivity change.
    pub fn watch_connectivity(&self) -> watch::Receiver<Connectivity> {
        self.connectivity.subscribe()
    }

    /// Gives back the sync (for example, to persist or to restore later).
    pub fn into_sync(self) -> ThreadSync {
        self.sync
    }

    fn closed(&mut self) -> SubscriptionError {
        self.socket = None;
        self.set_connectivity(Connectivity::Offline);
        SubscriptionError::Closed
    }

    fn set_connectivity(&self, next: Connectivity) {
        self.connectivity.send_replace(next);
    }
}
