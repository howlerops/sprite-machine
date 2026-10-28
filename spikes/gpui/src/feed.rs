//! The fixture feed: the transcript's envelopes from agui-fixture's WebSocket route
//! `ws://<addr>/threads/th_fixture/subscribe?after=0` (turn-event.md §6).
//!
//! The socket is read on a background thread, which parses each text frame and sends the
//! envelopes over a channel. The UI thread only drains that channel (see `AppShell`), so a slow
//! or stalled server never blocks rendering.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;

use protocol::turn_event::Envelope;
use serde_json::Value;
use tungstenite::Message;

/// The thread the fixture serves by default (`agui_fixture::DEFAULT_THREAD_ID`).
pub const FEED_THREAD_ID: &str = "th_fixture";

/// The subscription URL for the fixture at `addr` (`host:port`), from the start of the log.
pub fn subscribe_url(addr: &str) -> String {
    format!("ws://{addr}/threads/{FEED_THREAD_ID}/subscribe?after=0")
}

/// Parses one text frame. `Ok(Some(envelope))` for an `event` frame, `Ok(None)` for the control
/// frames (`hello`, `snapshot`, `live`, `pong`), `Err` for anything else.
pub fn parse_feed_message(text: &str) -> Result<Option<Envelope>, String> {
    let value: Value = serde_json::from_str(text).map_err(|err| format!("feed frame: {err}"))?;
    match value.get("type").and_then(Value::as_str) {
        Some("event") => {
            let envelope = value
                .get("envelope")
                .cloned()
                .ok_or("feed event frame has no envelope")?;
            serde_json::from_value::<Envelope>(envelope)
                .map(Some)
                .map_err(|err| format!("feed envelope: {err}"))
        }
        Some("hello" | "snapshot" | "live" | "pong") => Ok(None),
        Some(other) => Err(format!("unknown feed frame type `{other}`")),
        None => Err("feed frame has no type".to_owned()),
    }
}

/// Everything queued on `receiver` right now, and whether the sender has gone (the feed ended).
pub fn drain(receiver: &Receiver<Envelope>) -> (Vec<Envelope>, bool) {
    let mut batch = Vec::new();
    loop {
        match receiver.try_recv() {
            Ok(envelope) => batch.push(envelope),
            Err(TryRecvError::Empty) => return (batch, false),
            Err(TryRecvError::Disconnected) => return (batch, true),
        }
    }
}

/// Starts the feed for `addr` on a background thread and returns the envelope channel. The
/// channel disconnects when the socket closes or fails (the reason goes to stderr).
pub fn spawn_feed(addr: &str) -> Receiver<Envelope> {
    spawn_feed_url(subscribe_url(addr))
}

/// [`spawn_feed`] for a subscription URL of any thread (`ws://<addr>/threads/<id>/subscribe`).
pub fn spawn_feed_url(url: String) -> Receiver<Envelope> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("fixture-feed".to_owned())
        .spawn(move || {
            if let Err(err) = run_feed(&url, &sender) {
                eprintln!("fixture feed {url}: {err}");
            }
        })
        .expect("spawn fixture feed thread");
    receiver
}

fn run_feed(url: &str, sender: &Sender<Envelope>) -> Result<(), String> {
    let (mut socket, _response) = tungstenite::connect(url).map_err(|err| err.to_string())?;
    loop {
        match socket.read().map_err(|err| err.to_string())? {
            Message::Text(text) => {
                if let Some(envelope) = parse_feed_message(text.as_str())?
                    && sender.send(envelope).is_err()
                {
                    // The shell is gone: nothing left to feed.
                    return Ok(());
                }
            }
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{drain, parse_feed_message, subscribe_url};
    use agui_fixture::{Scenario, complete_log};
    use std::sync::mpsc;

    #[test]
    fn feed_subscribe_url_is_the_fixture_thread_from_zero() {
        assert_eq!(
            subscribe_url("127.0.0.1:4317"),
            "ws://127.0.0.1:4317/threads/th_fixture/subscribe?after=0"
        );
    }

    #[test]
    fn feed_event_frame_parses_to_the_envelope_it_carries() {
        let log = complete_log(Scenario::Transcript10k, "th_fixture").expect("fixture log");
        let frame = serde_json::json!({"type": "event", "envelope": log[0]}).to_string();
        let parsed = parse_feed_message(&frame)
            .expect("event frame parses")
            .expect("event frame carries an envelope");
        assert_eq!(parsed, log[0]);
    }

    #[test]
    fn feed_control_frames_carry_no_envelope() {
        for frame in [
            r#"{"type":"hello","head_seq":10,"replay_from":1}"#,
            r#"{"type":"snapshot","at_seq":0,"states":{}}"#,
            r#"{"type":"live"}"#,
            r#"{"type":"pong"}"#,
        ] {
            assert_eq!(parse_feed_message(frame), Ok(None), "{frame}");
        }
    }

    #[test]
    fn feed_rejects_broken_and_unknown_frames() {
        assert!(parse_feed_message("not json").is_err());
        assert!(parse_feed_message(r#"{"type":"event"}"#).is_err());
        assert!(parse_feed_message(r#"{"type":"event","envelope":{"seq":1}}"#).is_err());
        assert!(parse_feed_message(r#"{"type":"mystery"}"#).is_err());
        assert!(parse_feed_message(r#"{"no":"type"}"#).is_err());
    }

    #[test]
    fn feed_drain_reports_the_queue_and_a_disconnected_sender() {
        let log = complete_log(Scenario::Transcript10k, "th_fixture").expect("fixture log");
        let (sender, receiver) = mpsc::channel();
        sender.send(log[0].clone()).expect("send");
        sender.send(log[1].clone()).expect("send");
        assert_eq!(
            drain(&receiver),
            (vec![log[0].clone(), log[1].clone()], false)
        );
        drop(sender);
        assert_eq!(drain(&receiver), (Vec::new(), true));
    }
}
