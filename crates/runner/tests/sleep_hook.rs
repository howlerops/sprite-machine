//! The OS sleep hook: each `WillSleep` becomes `presence{asleep}` on the live tunnel before the
//! sleep delay is released (tunnel.md §3.7). The OS watcher is a fake here, with a fake delay
//! that records `take` and `release`. The Linux watcher is only checked for its no-bus error,
//! and the macOS decision logic (`power_action`) is checked on every host.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use protocol::tunnel::Capabilities;
use runner::sleep::{
    Hint, PowerAction, PowerEvent, PowerMessage, SleepWatcher, forward, power_action,
};
use runner::tunnel::{StaticCredential, Timing, TunnelConfig, tunnel};
use serde_json::{Value, json};
use sidecar::store::Store;
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;

const WAIT: Duration = Duration::from_secs(5);

/// Accepts the upgrade and echoes the offered `Sec-WebSocket-Protocol` (the runner requires it).
struct EchoProtocol;

impl Callback for EchoProtocol {
    fn on_request(self, req: &Request, mut resp: Response) -> Result<Response, ErrorResponse> {
        if let Some(protocol) = req.headers().get(SEC_WEBSOCKET_PROTOCOL) {
            resp.headers_mut()
                .insert(SEC_WEBSOCKET_PROTOCOL, protocol.clone());
        }
        Ok(resp)
    }
}

/// Shared record of what the forwarder did, in order.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<&'static str>>>);

impl Log {
    fn push(&self, entry: &'static str) {
        self.0.lock().unwrap().push(entry);
    }

    fn entries(&self) -> Vec<&'static str> {
        self.0.lock().unwrap().clone()
    }
}

/// A watcher whose events the test sends. Its delay is a fake inhibitor: `take_delay` and
/// `release_delay` are recorded in `log`. Ends with an error when the sender is dropped.
struct FakeWatcher {
    events: mpsc::UnboundedReceiver<PowerEvent>,
    log: Log,
    take_fails: bool,
}

impl SleepWatcher for FakeWatcher {
    async fn next_event(&mut self) -> Result<PowerEvent> {
        self.events
            .recv()
            .await
            .ok_or_else(|| anyhow!("fake watcher closed"))
    }

    async fn take_delay(&mut self) -> Result<()> {
        self.log.push("take");
        if self.take_fails {
            bail!("fake logind refused the inhibitor");
        }
        Ok(())
    }

    fn release_delay(&mut self) -> Result<()> {
        self.log.push("release");
        Ok(())
    }
}

/// A hint that records when it is written.
struct RecordingHint(Log);

impl Hint for RecordingHint {
    async fn send_asleep(&self) -> bool {
        self.0.push("hint");
        true
    }
}

async fn next_frame(ws: &mut WebSocketStream<TcpStream>) -> Value {
    loop {
        let message = tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("the runner sent no frame")
            .expect("hub stream ended")
            .expect("hub read failed");
        if let Message::Text(text) = message {
            return serde_json::from_str(text.as_str()).unwrap();
        }
    }
}

#[tokio::test]
async fn each_pre_sleep_notification_sends_presence_asleep() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("ws://{}", listener.local_addr().unwrap());
    let dir = TempDir::new().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let config = TunnelConfig {
        hub_ws_base: base,
        machine_id: "mch_test".to_owned(),
        runner_version: "0.0.0-test".to_owned(),
        capabilities: Capabilities {
            host_mode: "local".to_owned(),
            harnesses: Vec::new(),
            max_sessions: 1,
            spawn: Vec::new(),
            features: Vec::new(),
        },
        timing: Timing::default(),
    };
    let (client, _sink, _commands) =
        tunnel(config, store, StaticCredential::new("smm_test_credential"));
    let hint = client.sleep_hint();
    let run = tokio::spawn(client.run(std::future::pending::<()>()));

    // The fake hub: accept, answer hello with a fresh-stream welcome.
    let (stream, _) = tokio::time::timeout(WAIT, listener.accept())
        .await
        .expect("the runner did not connect")
        .unwrap();
    let mut ws = tokio_tungstenite::accept_hdr_async(stream, EchoProtocol)
        .await
        .unwrap();
    assert_eq!(next_frame(&mut ws).await["t"], "hello");
    ws.send(Message::text(
        json!({
            "t": "welcome", "proto": 1, "resumed": false, "resume_token": "rt_1",
            "last_up_seq": 0, "heartbeat_ms": 20_000, "max_inflight": 256,
        })
        .to_string(),
    ))
    .await
    .unwrap();

    // Two pre-sleep notifications (say, two sleeps) give two presence frames.
    let (notify, events) = mpsc::unbounded_channel();
    let watcher = FakeWatcher {
        events,
        log: Log::default(),
        take_fails: false,
    };
    let forwarding = tokio::spawn(forward(watcher, hint));
    for _ in 0..2 {
        notify.send(PowerEvent::WillSleep).unwrap();
        assert_eq!(
            next_frame(&mut ws).await,
            json!({ "t": "presence", "state": "asleep", "reason": "os_sleep" })
        );
    }

    // The watcher is gone: forwarding ends with its error, and the client keeps running.
    drop(notify);
    let outcome = forwarding.await.unwrap();
    assert!(
        outcome
            .unwrap_err()
            .to_string()
            .contains("fake watcher closed")
    );
    assert!(!run.is_finished(), "the tunnel client must keep running");
    run.abort();
}

#[tokio::test]
async fn the_hint_is_written_before_the_delay_is_released() {
    let log = Log::default();
    let (notify, events) = mpsc::unbounded_channel();
    let watcher = FakeWatcher {
        events,
        log: log.clone(),
        take_fails: false,
    };
    for event in [
        PowerEvent::WillSleep,
        PowerEvent::Resumed,
        PowerEvent::WillSleep,
    ] {
        notify.send(event).unwrap();
    }
    drop(notify);

    // Queued events are drained before the closed channel ends the loop.
    let outcome = forward(watcher, RecordingHint(log.clone())).await;
    assert!(
        outcome
            .unwrap_err()
            .to_string()
            .contains("fake watcher closed")
    );
    assert_eq!(
        log.entries(),
        // Startup take; sleep: hint then release; resume: take again; sleep: hint then release.
        ["take", "hint", "release", "take", "hint", "release"],
    );
}

#[tokio::test]
async fn a_refused_delay_still_gets_the_hint_before_the_sleep() {
    let log = Log::default();
    let (notify, events) = mpsc::unbounded_channel();
    let watcher = FakeWatcher {
        events,
        log: log.clone(),
        take_fails: true,
    };
    notify.send(PowerEvent::WillSleep).unwrap();
    drop(notify);

    let _ = forward(watcher, RecordingHint(log.clone())).await;
    // No delay was held, but the hint is still written before the (no-op) release.
    assert_eq!(log.entries(), ["take", "hint", "release"]);
}

#[test]
fn can_system_sleep_is_allowed_at_once_without_a_hint() {
    assert_eq!(
        power_action(PowerMessage::CanSystemSleep),
        PowerAction::AllowNow
    );
}

#[test]
fn will_system_sleep_is_hinted_and_then_allowed() {
    assert_eq!(
        power_action(PowerMessage::SystemWillSleep),
        PowerAction::HintThenAllow
    );
}

#[test]
fn other_power_messages_are_ignored() {
    assert_eq!(power_action(PowerMessage::Other), PowerAction::Ignore);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn without_a_system_bus_the_linux_watcher_fails_and_does_not_panic() {
    let missing = "unix:path=/nonexistent/sm-test-bus/socket";
    let result = runner::sleep::LogindSleepWatcher::connect_to(missing).await;
    assert!(result.is_err(), "no bus must be an error");
}
