//! `session_config` and the session state of the bridge (tunnel.md §3.5, turn-event.md §4.6).
//!
//! The agent is acp-mock: its script advertises the modes and config options in `session/new`,
//! its one turn sends the updates a test asks for, and it answers `session/set_mode` and
//! `session/set_config_option`, reporting each change as an update. A `Recorder` keeps every
//! message the bridge sent, so a test can check what reached ACP. The harness plays the hub: it
//! appends `UserMessage` to the log it validates.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use acp_mock::Recorder;
use protocol::mapping::acp_turn::{SequenceValidator, validate_complete};
use protocol::tunnel::{Actor, ActorKind, Command, CommandErrorCode, ContentPart};
use protocol::turn_event::{Envelope, Event, StateScope, ThreadSeq, UserMessage};
use serde_json::{Value, json};
use sidecar::bridge::{
    Bridge, BridgeError, CommandFailure, CommandFeed, Config, McpProvider, MemoryCommands,
    MemorySink, SequentialGenerator, SinkItem,
};
use sidecar::store::{ManualClock, Store};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(20);
const T0: u64 = 1_700_000_000_000;
const SESSION: &str = "sess_mock_1";

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

/// The acp-mock script: the modes and config options of `session/new`, and one turn whose steps
/// are `steps` (they run during the test's prompt).
fn script(steps: Vec<Value>) -> Value {
    json!({
        "modes": modes("code"),
        "config_options": options("sonnet", false),
        "turns": [{ "steps": steps }],
    })
}

// ---- harness ----------------------------------------------------------------------------------

struct Rig {
    feed: CommandFeed,
    events: mpsc::UnboundedReceiver<SinkItem>,
    failures: VecDeque<CommandFailure>,
    pending: VecDeque<Envelope>,
    recorder: Recorder,
    log: Vec<Envelope>,
    bridge: JoinHandle<Result<(), BridgeError>>,
    _dirs: Vec<TempDir>,
}

impl Rig {
    /// A bridge over acp-mock. The agent's one turn runs `steps` during the test's prompt.
    async fn start(steps: Vec<Value>) -> Self {
        Self::start_with(steps, false, |_| {}).await
    }

    /// [`Rig::start`] with `configure` applied to the bridge's config. With `advertise_http` the
    /// agent's script says it takes http MCP servers (`mcp_capabilities`).
    async fn start_with(
        steps: Vec<Value>,
        advertise_http: bool,
        configure: impl FnOnce(&mut Config),
    ) -> Self {
        let (client_io, agent_io) = tokio::io::duplex(1 << 16);
        let (agent_r, agent_w) = tokio::io::split(agent_io);
        let mut agent_script = script(steps);
        if advertise_http {
            agent_script["mcp_capabilities"] = json!({ "http": true });
        }
        let agent = acp_mock::Script::from_value(&agent_script).expect("a valid script");
        let recorder = Recorder::new();
        let agent_recorder = recorder.clone();
        tokio::spawn(async move {
            acp_mock::run_recorded(agent, agent_recorder, agent_r, agent_w)
                .await
                .expect("acp-mock runs");
        });
        let (reader, writer) = tokio::io::split(client_io);

        let state = TempDir::new().expect("state dir");
        let work = TempDir::new().expect("work dir");
        let clock = Arc::new(ManualClock::new(T0));
        let store = Arc::new(Store::open_with_clock(state.path(), clock).expect("store"));
        let mut config = Config::new("mch_1", "agt_fake", work.path());
        configure(&mut config);
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
            recorder,
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

    /// The `session/set_*` requests the bridge sent to the agent so far, as (method, params).
    fn acp_calls(&self) -> Vec<(String, Value)> {
        self.recorder
            .messages()
            .into_iter()
            .filter_map(|message| {
                let method = message.get("method")?.as_str()?;
                method
                    .starts_with("session/set_")
                    .then(|| (method.to_owned(), message["params"].clone()))
            })
            .collect()
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
    let steps = vec![
        json!({ "available_commands": [{ "name": "review", "description": "Review the diff" }] }),
        json!({ "current_mode": "plan" }),
        json!({ "config_option": options("opus", false) }),
    ];
    let mut rig = Rig::start(steps).await;
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

/// The methods the agent received, with the mode id of `session/set_mode`.
fn received(rig: &Rig) -> Vec<String> {
    rig.recorder
        .messages()
        .iter()
        .filter_map(|m| {
            let method = m["method"].as_str()?;
            Some(match m["params"]["modeId"].as_str() {
                Some(mode) => format!("{method}:{mode}"),
                None => method.to_owned(),
            })
        })
        .collect()
}

#[tokio::test]
async fn the_initial_mode_is_set_before_the_first_prompt() {
    let mut rig = Rig::start_with(Vec::new(), false, |config| {
        config.initial_mode = Some("plan".to_owned());
    })
    .await;
    rig.turn().await;
    let methods = received(&rig);
    let at = |name: &str| methods.iter().position(|m| m == name).unwrap();
    assert!(
        at("session/new") < at("session/set_mode:plan"),
        "{methods:?}"
    );
    assert!(
        at("session/set_mode:plan") < at("session/prompt"),
        "{methods:?}"
    );
    rig.finish().await;
}

#[tokio::test]
async fn an_initial_mode_the_agent_does_not_advertise_fails_the_task_without_a_prompt() {
    let mut rig = Rig::start_with(Vec::new(), false, |config| {
        config.initial_mode = Some("yolo".to_owned());
    })
    .await;
    rig.feed.send(Command::Prompt {
        thread_id: "th_1".into(),
        task_id: "tk_1".into(),
        message_id: "msg_u".into(),
        parts: vec![ContentPart::Text { text: "go".into() }],
        author: user(),
    });
    let events = rig.until(|e| matches!(e.event, Event::RunError(_))).await;
    assert!(matches!(events.last().unwrap().event, Event::RunError(_)));
    let methods = received(&rig);
    assert!(
        !methods.iter().any(|m| m == "session/prompt"),
        "{methods:?}"
    );
    assert!(
        !methods.iter().any(|m| m.starts_with("session/set_mode")),
        "{methods:?}"
    );
}

#[tokio::test]
async fn the_mcp_provider_gets_the_agents_advertised_capabilities() {
    for advertised in [true, false] {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let sink = Arc::clone(&seen);
        let mut rig = Rig::start_with(Vec::new(), advertised, move |config| {
            config.mcp_provider = Some(McpProvider::new(move |_thread, caps| {
                *sink.lock().unwrap() = Some(caps.http);
                Vec::new()
            }));
        })
        .await;
        rig.turn().await;
        assert_eq!(*seen.lock().unwrap(), Some(advertised));
        rig.finish().await;
    }
}
