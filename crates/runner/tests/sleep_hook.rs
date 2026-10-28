//! The OS sleep hook: each pre-sleep notification becomes `presence{asleep}` on the live tunnel
//! (tunnel.md §3.7). The OS watcher is a fake here; the Linux watcher is only checked for its
//! no-bus error.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use protocol::tunnel::Capabilities;
use runner::sleep::{SleepWatcher, forward};
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

/// A watcher whose notifications the test sends. Ends with an error when the sender is dropped.
struct FakeWatcher(mpsc::UnboundedReceiver<()>);

impl SleepWatcher for FakeWatcher {
    async fn pre_sleep(&mut self) -> Result<()> {
        self.0
            .recv()
            .await
            .ok_or_else(|| anyhow!("fake watcher closed"))
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
    let (notify, watcher) = mpsc::unbounded_channel();
    let forwarding = tokio::spawn(forward(FakeWatcher(watcher), hint));
    for _ in 0..2 {
        notify.send(()).unwrap();
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

#[cfg(target_os = "linux")]
#[tokio::test]
async fn without_a_system_bus_the_linux_watcher_fails_and_does_not_panic() {
    let missing = "unix:path=/nonexistent/sm-test-bus/socket";
    let result = runner::sleep::LogindSleepWatcher::connect_to(missing).await;
    assert!(result.is_err(), "no bus must be an error");
}
