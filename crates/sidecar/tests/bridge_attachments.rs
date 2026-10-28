//! Bridge tests for user file parts: a failed attachment fetch ends the task with
//! `RunError{code:"attachment"}` and the agent never receives the prompt.
//!
//! The agent is acp-mock. Between the bridge and acp-mock sits a relay that records the ACP
//! method of every line the bridge sends, so the test asserts on what the agent received, not
//! only on the order of events. The bridge runs under the production `FilePolicy::default()`:
//! the failing URLs are refused before any connection is made (plain `http`, and a loopback
//! address), so no network or DNS is used.

use std::sync::Arc;
use std::time::Duration;

use protocol::tunnel::{Actor, ActorKind, Command, ContentPart};
use protocol::turn_event::{Envelope, Event, RunOutcome};
use serde_json::{Value, json};
use sidecar::bridge::{
    Bridge, BridgeError, CommandFeed, Config, MemoryCommands, MemorySink, SequentialGenerator,
    SinkItem,
};
use sidecar::store::{ManualClock, Store};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(20);
const T0: u64 = 1_700_000_000_000;

fn user() -> Actor {
    Actor {
        kind: ActorKind::User,
        id: "usr_1".into(),
        name: None,
        device_id: Some("dev_1".into()),
    }
}

fn text(text: &str) -> ContentPart {
    ContentPart::Text { text: text.into() }
}

fn file(name: &str, mime_type: &str, url: &str) -> ContentPart {
    ContentPart::File {
        name: name.to_owned(),
        mime_type: mime_type.to_owned(),
        url: url.to_owned(),
        size: None,
        sha256: None,
    }
}

/// Two turns, so a prompt that wrongly reached the agent would still leave a turn for the
/// prompt that follows it.
fn script() -> Value {
    json!({ "turns": [
        { "steps": [{ "text": "ok" }, { "stop": "end_turn" }] },
        { "steps": [{ "text": "ok" }, { "stop": "end_turn" }] }
    ]})
}

// ---- the agent: acp-mock behind a recording relay -----------------------------------------------

/// Starts acp-mock and returns the client's end of the connection, plus the ACP methods the
/// client sent, in order.
fn start_agent() -> (tokio::io::DuplexStream, mpsc::UnboundedReceiver<String>) {
    let script = acp_mock::Script::from_value(&script()).expect("script");
    let (client_io, proxy_io) = tokio::io::duplex(1 << 16);
    let (mock_io, relay_io) = tokio::io::duplex(1 << 16);
    let (mock_r, mock_w) = tokio::io::split(mock_io);
    tokio::spawn(acp_mock::run(script, mock_r, mock_w));

    let (proxy_r, mut proxy_w) = tokio::io::split(proxy_io);
    let (mut relay_r, mut relay_w) = tokio::io::split(relay_io);
    let (methods_tx, methods_rx) = mpsc::unbounded_channel();
    // Client to agent: record the method, then pass the line on.
    tokio::spawn(async move {
        let mut lines = BufReader::new(proxy_r).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(method) = serde_json::from_str::<Value>(&line)
                .ok()
                .and_then(|message| message["method"].as_str().map(str::to_owned))
            {
                let _ = methods_tx.send(method);
            }
            if relay_w
                .write_all(format!("{line}\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
    });
    // Agent to client: passed on unchanged.
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut relay_r, &mut proxy_w).await;
    });
    (client_io, methods_rx)
}

// ---- the bridge -------------------------------------------------------------------------------

struct Run {
    feed: CommandFeed,
    events: mpsc::UnboundedReceiver<SinkItem>,
    methods: mpsc::UnboundedReceiver<String>,
    bridge: JoinHandle<Result<(), BridgeError>>,
    _dirs: (TempDir, TempDir),
}

impl Run {
    fn start() -> Self {
        let (client_io, methods) = start_agent();
        let (reader, writer) = tokio::io::split(client_io);
        let state = TempDir::new().expect("state dir");
        let work = TempDir::new().expect("work dir");
        let clock = Arc::new(ManualClock::new(T0));
        let store = Arc::new(Store::open_with_clock(state.path(), clock).expect("store"));
        let mut config = Config::new("mch_1", "agt_mock", work.path());
        config.expiry_check_interval = Duration::from_millis(20);
        let (sink, events) = MemorySink::new();
        let (commands, feed) = MemoryCommands::new();
        let bridge = Bridge::connect(
            config,
            reader,
            writer,
            store,
            Arc::new(SequentialGenerator::new()),
            sink,
            commands,
        );
        Self {
            feed,
            events,
            methods,
            bridge: tokio::spawn(bridge.run()),
            _dirs: (state, work),
        }
    }

    /// The user's prompt for `task` on thread `th_1`.
    fn prompt(&self, task: &str, parts: Vec<ContentPart>) {
        self.feed.send(Command::Prompt {
            thread_id: "th_1".into(),
            task_id: task.into(),
            message_id: format!("msg_u_{task}"),
            parts,
            author: user(),
        });
    }

    /// Consumes events up to and including the first one `stop` accepts.
    async fn until(&mut self, stop: impl Fn(&Envelope) -> bool) -> Vec<Envelope> {
        let mut got = Vec::new();
        loop {
            let item = timeout(STEP, self.events.recv())
                .await
                .expect("timely")
                .expect("the sink stays open");
            match item {
                SinkItem::Events(envelopes) => {
                    for envelope in envelopes {
                        let done = stop(&envelope);
                        got.push(envelope);
                        if done {
                            return got;
                        }
                    }
                }
                SinkItem::Failure(failure) => {
                    panic!("command rejected: {:?}", failure.code)
                }
            }
        }
    }

    /// The ACP methods the client has sent so far.
    fn sent_methods(&mut self) -> Vec<String> {
        let mut sent = Vec::new();
        while let Ok(method) = self.methods.try_recv() {
            sent.push(method);
        }
        sent
    }
}

fn is_success(envelope: &Envelope) -> bool {
    matches!(&envelope.event, Event::RunFinished(f) if matches!(f.outcome, RunOutcome::Success { .. }))
}

fn is_run_error(envelope: &Envelope) -> bool {
    matches!(envelope.event, Event::RunError(_))
}

/// Asserts the task ended with a `RunError` whose code is `attachment`, and returns its message.
fn attachment_error(events: &[Envelope]) -> String {
    let last = events.last().expect("events");
    let Event::RunError(error) = &last.event else {
        panic!("the task ends with a RunError: {events:#?}");
    };
    assert_eq!(error.code.as_deref(), Some("attachment"), "{error:?}");
    error.message.clone()
}

// ---- the criterion --------------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_attachment_fetch_ends_the_task_and_the_agent_never_gets_the_prompt() {
    let mut run = Run::start();

    // Plain http is refused under the production default: no connection is made.
    run.prompt(
        "tk_1",
        vec![
            text("look at this"),
            file(
                "a.txt",
                "text/plain",
                "http://files.example.test/a.txt?sig=s3cret",
            ),
        ],
    );
    let first = run.until(is_run_error).await;
    let message = attachment_error(&first);
    assert!(!message.contains("s3cret"), "the URL is not in the error");
    assert!(
        matches!(first[0].event, Event::RunStarted(_)),
        "the task was announced first: {first:#?}"
    );

    // A loopback address is refused as non-public before any connection is made.
    run.prompt(
        "tk_2",
        vec![file(
            "b.png",
            "image/png",
            "https://127.0.0.1:1/b.png?sig=s3cret",
        )],
    );
    let second = run.until(is_run_error).await;
    assert!(!attachment_error(&second).contains("s3cret"));

    // The agent has not seen either prompt. A prompt that follows is the first one it gets:
    // the relay records in order, so once this task succeeds every earlier line is recorded.
    run.prompt("tk_3", vec![text("hello")]);
    run.until(is_success).await;
    let sent = run.sent_methods();
    let prompts = sent
        .iter()
        .filter(|m| m.as_str() == "session/prompt")
        .count();
    assert_eq!(
        prompts, 1,
        "only tk_3 reached session/prompt; the agent received: {sent:?}"
    );

    // The bridge stops cleanly when the commands end.
    let Run { feed, bridge, .. } = run;
    drop(feed);
    timeout(STEP, bridge)
        .await
        .expect("the bridge stops")
        .expect("bridge task")
        .expect("bridge ok");
}
