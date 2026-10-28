//! The bridge does not own the down-stream commit: it reads `Store::seen_key`, applies, and
//! reports `processed(seq, key)`. `last_down_seq` and the idempotency key are committed by the
//! command source (the tunnel client), so a crash between receive and apply loses nothing.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use protocol::tunnel::{Actor, ActorKind, Command, ContentPart};
use protocol::turn_event::Event;
use serde_json::json;
use sidecar::bridge::{
    Bridge, BridgeError, CommandFeed, Config, MemoryCommands, MemorySink, SequentialGenerator,
    SinkItem,
};
use sidecar::store::Store;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

struct Harness {
    feed: CommandFeed,
    events: mpsc::UnboundedReceiver<SinkItem>,
    store: Arc<Store>,
    _bridge: JoinHandle<Result<(), BridgeError>>,
    _dir: TempDir,
}

fn start(store_setup: impl FnOnce(&Store)) -> Harness {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    store_setup(&store);
    let script =
        acp_mock::Script::from_value(&json!({ "turns": [{ "steps": [{ "text": "hi" }] }] }))
            .unwrap();
    let (client_io, agent_io) = tokio::io::duplex(1 << 16);
    let (agent_r, agent_w) = tokio::io::split(agent_io);
    tokio::spawn(async move {
        let _ = acp_mock::run(script, agent_r, agent_w).await;
    });
    let (client_r, client_w) = tokio::io::split(client_io);
    let (sink, events) = MemorySink::new();
    let (commands, feed) = MemoryCommands::new();
    let bridge = Bridge::connect(
        Config::new("mch_1", "agt_mock", Path::new(".")),
        client_r,
        client_w,
        Arc::clone(&store),
        Arc::new(SequentialGenerator::new()),
        sink,
        commands,
    );
    Harness {
        feed,
        events,
        store,
        _bridge: tokio::spawn(bridge.run()),
        _dir: dir,
    }
}

fn prompt() -> Command {
    Command::Prompt {
        thread_id: "th_1".into(),
        task_id: "tk_1".into(),
        message_id: "msg_1".into(),
        parts: vec![ContentPart::Text { text: "go".into() }],
        author: Actor {
            kind: ActorKind::User,
            id: "usr_1".into(),
            name: None,
            device_id: None,
        },
    }
}

async fn wait_processed(feed: &CommandFeed, count: usize) {
    for _ in 0..1000 {
        if feed.processed_keys().len() >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the bridge did not report {count} command(s) processed");
}

#[tokio::test]
async fn the_bridge_applies_a_command_and_reports_seq_and_key_without_committing() {
    let mut h = start(|_| {});
    let key = h.feed.send(prompt());
    wait_processed(&h.feed, 1).await;
    assert_eq!(h.feed.processed_keys(), [(1, key.clone())]);
    // The command source commits, not the bridge.
    assert_eq!(h.store.last_down_seq().unwrap(), 0);
    assert!(!h.store.seen_key(&key).unwrap());
    // It was applied: the sink got the turn's events.
    assert!(matches!(h.events.recv().await, Some(SinkItem::Events(_))));
}

#[tokio::test]
async fn a_key_the_store_has_seen_is_reported_processed_but_not_applied() {
    let mut h = start(|store| {
        store.commit_down_frame(1, Some("idk_seen")).unwrap();
    });
    h.feed.send_with_key("idk_seen", prompt());
    wait_processed(&h.feed, 1).await;
    assert_eq!(h.feed.processed_keys(), [(1, "idk_seen".to_owned())]);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.events.try_recv().is_err(), "a seen key was applied");
    assert_eq!(h.store.last_down_seq().unwrap(), 1);
}

#[tokio::test]
async fn a_repeat_before_the_source_commits_is_applied_once() {
    let mut h = start(|_| {});
    h.feed.send_with_key("idk_dup", prompt());
    h.feed.send_with_key("idk_dup", prompt());
    wait_processed(&h.feed, 2).await;
    assert_eq!(h.store.last_down_seq().unwrap(), 0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut starts = 0;
    while let Ok(SinkItem::Events(envelopes)) = h.events.try_recv() {
        starts += envelopes
            .iter()
            .filter(|e| matches!(e.event, Event::RunStarted(_)))
            .count();
    }
    assert_eq!(starts, 1, "the repeat started a second run");
}
