//! `session_config` and the session state of the bridge (tunnel.md §3.5, turn-event.md §4.6).
//!
//! The agent is a small fake written here, not acp-mock: acp-mock has no `session/set_mode` or
//! `session/set_config_option`. The fake advertises modes and config options in `session/new`,
//! sends the updates a test asks for, answers the set calls like an agent would (reporting the
//! change as an update), and records every call it receives so a test can check what reached
//! ACP. The harness plays the hub: it appends `UserMessage` to the log it validates.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use protocol::mapping::acp_turn::{SequenceValidator, validate_complete};
use protocol::tunnel::{Actor, ActorKind, Command, CommandErrorCode, ContentPart};
use protocol::turn_event::{Envelope, Event, StateScope, ThreadSeq, UserMessage};
use serde_json::{Value, json};
use sidecar::bridge::{
    Bridge, BridgeError, CommandFailure, CommandFeed, Config, MemoryCommands, MemorySink,
    SequentialGenerator, SinkItem,
};
use sidecar::store::{ManualClock, Store};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(20);
const T0: u64 = 1_700_000_000_000;
const SESSION: &str = "sess_1";

fn user() -> Actor {
    Actor {
        kind: ActorKind::User,
        id: "usr_1".into(),
        name: None,
        device_id: Some("dev_1".into()),
    }
}

// ---- the fake agent ---------------------------------------------------------------------------

/// The config options the fake advertises: a select option and a boolean one.
fn options(model: &str, auto_fix: bool) -> Value {
    json!([
        {
            "id": "model", "name": "Model", "type": "select", "currentValue": model,
            "options": [
                { "value": "sonnet", "name": "Sonnet" },
                { "value": "opus", "name": "Opus" }
            ]
        },
        { "id": "auto_fix", "name": "Auto fix", "type": "boolean", "currentValue": auto_fix }
    ])
}

fn modes(current: &str) -> Value {
    json!({
        "currentModeId": current,
        "availableModes": [
            { "id": "code", "name": "Code" },
            { "id": "plan", "name": "Plan" }
        ]
    })
}

/// A `session/update` notification for `update` on the test session.
fn update(update: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": "session/update",
        "params": { "sessionId": SESSION, "update": update } })
}

fn reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

async fn write<W: AsyncWrite + Unpin>(writer: &mut W, message: &Value) {
    let mut bytes = serde_json::to_vec(message).expect("json");
    bytes.push(b'\n');
    writer.write_all(&bytes).await.expect("write to the bridge");
    writer.flush().await.expect("flush to the bridge");
}

/// Plays the agent until the bridge closes the connection. `prompt_updates` are sent, in order,
/// before each prompt's answer. `calls` gets every `session/set_*` request as (method, params).
async fn fake_agent<R, W>(
    reader: R,
    mut writer: W,
    calls: mpsc::UnboundedSender<(String, Value)>,
    prompt_updates: Vec<Value>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut current_mode = "code".to_owned();
    let mut config = options("sonnet", false);
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let message: Value = serde_json::from_str(&line).expect("the bridge sends JSON");
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            continue;
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        // Notifications (no id) need no answer; `session/cancel` is the only one sent here.
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        match method {
            "initialize" => {
                write(
                    &mut writer,
                    &reply(id, json!({ "protocolVersion": 1, "agentCapabilities": {} })),
                )
                .await;
            }
            "session/new" => {
                let answer = json!({
                    "sessionId": SESSION,
                    "modes": modes(&current_mode),
                    "configOptions": config,
                });
                write(&mut writer, &reply(id, answer)).await;
            }
            "session/prompt" => {
                for sent in &prompt_updates {
                    write(&mut writer, sent).await;
                }
                write(&mut writer, &reply(id, json!({ "stopReason": "end_turn" }))).await;
            }
            "session/set_mode" => {
                calls
                    .send((method.to_owned(), params.clone()))
                    .expect("the test listens");
                current_mode = params["modeId"].as_str().expect("modeId").to_owned();
                let changed = update(json!({
                    "sessionUpdate": "current_mode_update",
                    "currentModeId": current_mode,
                }));
                write(&mut writer, &changed).await;
                write(&mut writer, &reply(id, json!({}))).await;
            }
            "session/set_config_option" => {
                calls
                    .send((method.to_owned(), params.clone()))
                    .expect("the test listens");
                let config_id = params["configId"].as_str().expect("configId");
                for option in config.as_array_mut().expect("options") {
                    if option["id"] == config_id {
                        option["currentValue"] = params["value"].clone();
                    }
                }
                let changed = update(json!({
                    "sessionUpdate": "config_option_update",
                    "configOptions": config,
                }));
                write(&mut writer, &changed).await;
                write(&mut writer, &reply(id, json!({ "configOptions": config }))).await;
            }
            other => panic!("the fake agent got an unexpected `{other}`"),
        }
    }
}

// ---- harness ----------------------------------------------------------------------------------

struct Rig {
    feed: CommandFeed,
    events: mpsc::UnboundedReceiver<SinkItem>,
    failures: VecDeque<CommandFailure>,
    pending: VecDeque<Envelope>,
    calls: mpsc::UnboundedReceiver<(String, Value)>,
    log: Vec<Envelope>,
    bridge: JoinHandle<Result<(), BridgeError>>,
    _dirs: Vec<TempDir>,
}

impl Rig {
    /// A bridge with a fake agent that sends `prompt_updates` during every prompt.
    async fn start(prompt_updates: Vec<Value>) -> Self {
        let (client_io, agent_io) = tokio::io::duplex(1 << 16);
        let (agent_r, agent_w) = tokio::io::split(agent_io);
        let (calls_tx, calls) = mpsc::unbounded_channel();
        tokio::spawn(fake_agent(agent_r, agent_w, calls_tx, prompt_updates));
        let (reader, writer) = tokio::io::split(client_io);

        let state = TempDir::new().expect("state dir");
        let work = TempDir::new().expect("work dir");
        let clock = Arc::new(ManualClock::new(T0));
        let store = Arc::new(Store::open_with_clock(state.path(), clock).expect("store"));
        let config = Config::new("mch_1", "agt_fake", work.path());
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
            failures: VecDeque::new(),
            pending: VecDeque::new(),
            calls,
            log: Vec::new(),
            bridge: tokio::spawn(bridge.run()),
            _dirs: vec![state, work],
        }
    }

    async fn pump(&mut self) {
        match timeout(STEP, self.events.recv()).await {
            Ok(Some(SinkItem::Events(envelopes))) => self.pending.extend(envelopes),
            Ok(Some(SinkItem::Failure(failure))) => self.failures.push_back(failure),
            Ok(None) => panic!("the bridge stopped; log so far: {:#?}", self.log),
            Err(_) => panic!("timed out; log so far: {:#?}", self.log),
        }
    }

    /// Consumes envelopes up to and including the first one `stop` accepts.
    async fn until(&mut self, stop: impl Fn(&Envelope) -> bool) -> Vec<Envelope> {
        let mut got = Vec::new();
        loop {
            while let Some(envelope) = self.pending.pop_front() {
                let done = stop(&envelope);
                self.log.push(envelope.clone());
                got.push(envelope);
                if done {
                    return got;
                }
            }
            self.pump().await;
        }
    }

    async fn failure(&mut self) -> CommandFailure {
        loop {
            if let Some(failure) = self.failures.pop_front() {
                return failure;
            }
            self.pump().await;
        }
    }

    /// The hub appends `UserMessage`, the prompt reaches the runner, and the turn's events are
    /// consumed up to its `RunFinished`.
    async fn turn(&mut self) -> Vec<Envelope> {
        self.log.push(Envelope::new(
            "th_1",
            Some("tk_1".to_owned()),
            None,
            "2023-11-14T22:13:20.000Z",
            UserMessage {
                message_id: "msg_u".into(),
                parts: vec![ContentPart::Text { text: "go".into() }],
                author: user(),
            },
        ));
        self.feed.send(Command::Prompt {
            thread_id: "th_1".into(),
            task_id: "tk_1".into(),
            message_id: "msg_u".into(),
            parts: vec![ContentPart::Text { text: "go".into() }],
            author: user(),
        });
        self.until(|e| matches!(e.event, Event::RunFinished(_)))
            .await
    }

    /// Sends a `session_config` command built from its wire form (the config type is not
    /// nameable outside the protocol crate's command module).
    fn session_config(
        &mut self,
        thread: &str,
        mode: Option<&str>,
        config: Option<Value>,
    ) -> String {
        let mut wire = json!({ "kind": "session_config", "thread_id": thread });
        if let Some(mode) = mode {
            wire["mode_id"] = json!(mode);
        }
        if let Some(config) = config {
            wire["config"] = config;
        }
        let command: Command = serde_json::from_value(wire).expect("a session_config command");
        self.feed.send(command)
    }

    /// The ACP calls the fake agent received so far, with their methods.
    fn acp_calls(&mut self) -> Vec<(String, Value)> {
        let mut calls = Vec::new();
        while let Ok(call) = self.calls.try_recv() {
            calls.push(call);
        }
        calls
    }

    /// Ends the command stream, waits for the bridge and checks the whole log.
    async fn finish(self) -> Vec<Envelope> {
        let Self {
            feed,
            mut events,
            mut pending,
            mut log,
            bridge,
            ..
        } = self;
        drop(feed);
        timeout(STEP, bridge)
            .await
            .expect("the bridge stops when the commands end")
            .expect("bridge task")
            .expect("bridge ok");
        while let Ok(item) = events.try_recv() {
            if let SinkItem::Events(envelopes) = item {
                pending.extend(envelopes);
            }
        }
        log.extend(pending);
        assert_valid_log(&log);
        log
    }
}

/// The log as the Thread DO would store it: valid in order and gap-free once stamped.
fn assert_valid_log(log: &[Envelope]) {
    validate_complete(log).unwrap_or_else(|error| panic!("{error}\n{log:#?}"));
    let mut seq = ThreadSeq::new();
    let mut validator = SequenceValidator::new();
    for envelope in log {
        let stamped = seq.stamp(envelope.clone()).expect("producers send seq 0");
        validator
            .push(&stamped)
            .unwrap_or_else(|error| panic!("{error}"));
    }
    validator.finish().expect("the log ends between tasks");
}

/// The JSON Pointer paths of the patch of the session state delta `envelope` carries, if it is one.
fn session_delta_paths(envelope: &Envelope) -> Option<Vec<String>> {
    match &envelope.event {
        Event::StateDelta(delta) if delta.scope == StateScope::Session => {
            Some(delta.patch.iter().map(|op| op.path.clone()).collect())
        }
        _ => None,
    }
}

fn is_session_delta(envelope: &Envelope) -> bool {
    session_delta_paths(envelope).is_some()
}

fn session_delta_with(envelope: &Envelope, path: &str) -> bool {
    session_delta_paths(envelope).is_some_and(|paths| paths.iter().any(|p| p == path))
}

// ---- tests ------------------------------------------------------------------------------------

#[tokio::test]
async fn session_start_snapshot_precedes_the_state_deltas_of_the_agents_updates() {
    let updates = vec![
        update(json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [{ "name": "review", "description": "Review the diff" }]
        })),
        update(json!({ "sessionUpdate": "current_mode_update", "currentModeId": "plan" })),
        update(json!({
            "sessionUpdate": "config_option_update",
            "configOptions": options("opus", false)
        })),
    ];
    let mut rig = Rig::start(updates).await;
    let turn = rig.turn().await;

    // §4.6: the session snapshot comes at session start, with the advertised modes and options.
    let first_snapshot = turn
        .iter()
        .position(|e| matches!(&e.event, Event::StateSnapshot(s) if s.scope == StateScope::Session))
        .expect("a StateSnapshot{session} at session start");
    let first_delta = turn
        .iter()
        .position(is_session_delta)
        .expect("StateDelta{session} for the agent's updates");
    assert!(first_snapshot < first_delta, "snapshot before deltas");
    let Event::StateSnapshot(snapshot) = &turn[first_snapshot].event else {
        unreachable!("matched above");
    };
    assert_eq!(snapshot.snapshot["current_mode_id"], json!("code"));
    assert_eq!(snapshot.snapshot["modes"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        snapshot.snapshot["config_options"].as_array().map(Vec::len),
        Some(2)
    );

    // Each update of the turn is a delta on the session scope, after the snapshot.
    assert!(
        turn.iter()
            .any(|e| session_delta_with(e, "/available_commands"))
    );
    assert!(
        turn.iter()
            .any(|e| session_delta_with(e, "/current_mode_id"))
    );
    assert!(
        turn.iter()
            .any(|e| session_delta_with(e, "/config_options"))
    );
    rig.finish().await;
}

#[tokio::test]
async fn session_config_mode_is_sent_to_acp_and_its_update_reaches_the_sink() {
    let mut rig = Rig::start(Vec::new()).await;
    rig.turn().await;

    rig.session_config("th_1", Some("plan"), None);
    let delta = rig.until(is_session_delta).await;
    let last = delta.last().expect("the delta");
    assert!(session_delta_with(last, "/current_mode_id"));
    let Event::StateDelta(change) = &last.event else {
        unreachable!("the delta is last");
    };
    assert_eq!(change.patch[0].value, Some(json!("plan")));

    let calls = rig.acp_calls();
    assert_eq!(calls.len(), 1, "one ACP call: {calls:?}");
    assert_eq!(calls[0].0, "session/set_mode");
    assert_eq!(
        calls[0].1,
        json!({ "sessionId": SESSION, "modeId": "plan" })
    );
    rig.finish().await;
}

#[tokio::test]
async fn session_config_options_are_sent_to_acp_and_their_updates_reach_the_sink() {
    let mut rig = Rig::start(Vec::new()).await;
    rig.turn().await;

    rig.session_config(
        "th_1",
        None,
        Some(json!({ "id": "model", "value": "opus" })),
    );
    let select = rig
        .until(|e| session_delta_with(e, "/config_options"))
        .await;
    assert!(is_session_delta(select.last().expect("delta")));

    rig.session_config(
        "th_1",
        None,
        Some(json!({ "id": "auto_fix", "value": true })),
    );
    rig.until(|e| session_delta_with(e, "/config_options"))
        .await;

    let calls = rig.acp_calls();
    assert_eq!(calls.len(), 2, "two ACP calls: {calls:?}");
    assert_eq!(calls[0].0, "session/set_config_option");
    assert_eq!(
        calls[0].1,
        json!({ "sessionId": SESSION, "configId": "model", "value": "opus" })
    );
    assert_eq!(
        calls[1].1,
        json!({ "sessionId": SESSION, "configId": "auto_fix", "type": "boolean", "value": true })
    );
    rig.finish().await;
}

#[tokio::test]
async fn unknown_or_ill_fitting_mode_and_options_are_invalid_and_never_reach_acp() {
    let mut rig = Rig::start(Vec::new()).await;
    rig.turn().await;

    let refused = [
        rig.session_config("th_1", Some("nope"), None),
        rig.session_config("th_1", None, Some(json!({ "id": "nope", "value": "x" }))),
        rig.session_config("th_1", None, Some(json!({ "id": "model", "value": "gpt" }))),
        rig.session_config(
            "th_1",
            None,
            Some(json!({ "id": "auto_fix", "value": "yes" })),
        ),
        rig.session_config("th_1", None, None),
    ];
    for key in refused {
        let failure = rig.failure().await;
        assert_eq!(failure.idempotency_key, key);
        assert!(
            matches!(failure.code, CommandErrorCode::Invalid),
            "expected invalid, got {:?}: {}",
            failure.code,
            failure.message
        );
    }
    assert!(
        rig.acp_calls().is_empty(),
        "a refused command must not call ACP"
    );
    rig.finish().await;
}

#[tokio::test]
async fn session_config_for_an_unknown_thread_is_unknown_thread() {
    let mut rig = Rig::start(Vec::new()).await;

    let key = rig.session_config("th_missing", Some("plan"), None);
    let failure = rig.failure().await;
    assert_eq!(failure.idempotency_key, key);
    assert!(matches!(failure.code, CommandErrorCode::UnknownThread));
    assert!(rig.acp_calls().is_empty());
    rig.finish().await;
}
