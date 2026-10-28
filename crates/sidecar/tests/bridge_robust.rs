//! Runner bridge robustness: harness restart, byte-faithful raw updates, the session snapshot and
//! MCP servers at session start, and interrupts left open by an earlier process.
//!
//! An in-process acp-mock (behind a tee that records what the bridge sent it) or a hand-played
//! agent sits on one side, in-memory `CommandSource` and `EventSink` on the other. The store runs
//! on a manual clock and ids are sequential.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use protocol::tunnel::{Actor, ActorKind, Command, ContentPart};
use protocol::turn_event::{Envelope, Event, ResolvedOutcome, RunErrorReason, StateScope};
use serde_json::{Value, json};
use sidecar::acp::client::{ClientError, Transport};
use sidecar::bridge::{
    Bridge, BridgeError, CommandFeed, Config, Connector, MemoryCommands, MemorySink,
    SequentialGenerator, SinkItem,
};
use sidecar::store::{InterruptKind, ManualClock, PendingInterrupt, SessionState, Store};
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

// ---- agents -------------------------------------------------------------------------------------

/// Every JSON message the bridge sent an agent.
type Received = Arc<Mutex<Vec<Value>>>;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The requests (with an id) the agent received for `method`.
fn requests(received: &Received, method: &str) -> Vec<Value> {
    lock(received)
        .iter()
        .filter(|message| message["method"] == method)
        .cloned()
        .collect()
}

/// An acp-mock behind a tee, so the test sees what the bridge sent, and can kill the agent.
struct MockAgent {
    received: Received,
    tasks: Vec<JoinHandle<()>>,
}

impl MockAgent {
    fn start(script: &Value) -> (Transport, Self) {
        let script = acp_mock::Script::from_value(script).expect("script");
        let (client_io, proxy_client) = tokio::io::duplex(1 << 16);
        let (proxy_agent, agent_io) = tokio::io::duplex(1 << 16);
        let (agent_r, agent_w) = tokio::io::split(agent_io);
        let mock = tokio::spawn(async move {
            let _ = acp_mock::run(script, agent_r, agent_w).await;
        });
        let (client_up, mut to_client) = tokio::io::split(proxy_client);
        let (mut from_agent, mut to_agent) = tokio::io::split(proxy_agent);
        let received = Received::default();
        let record = Arc::clone(&received);
        let up = tokio::spawn(async move {
            let mut lines = BufReader::new(client_up).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(message) = serde_json::from_str::<Value>(&line) {
                    lock(&record).push(message);
                }
                if to_agent
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let down = tokio::spawn(async move {
            let _ = tokio::io::copy(&mut from_agent, &mut to_client).await;
        });
        let (client_r, client_w) = tokio::io::split(client_io);
        (
            Transport::new(client_r, client_w),
            Self {
                received,
                tasks: vec![mock, up, down],
            },
        )
    }

    /// The agent process dies: the bridge's reader sees the end of the stream.
    fn kill(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// What a hand-played agent says.
struct Play {
    /// The mode `session/new` and `session/resume` answer with.
    mode: &'static str,
    /// `session/update` params (without `sessionId`) sent during each prompt.
    updates: Vec<Value>,
}

/// An agent played by hand: it answers with a mode and sends the given updates.
fn play(spec: Play) -> (Transport, Received) {
    let (client_io, agent_io) = tokio::io::duplex(1 << 16);
    let (agent_r, mut agent_w) = tokio::io::split(agent_io);
    let received = Received::default();
    let record = Arc::clone(&received);
    tokio::spawn(async move {
        let modes = json!({
            "currentModeId": spec.mode,
            "availableModes": [
                { "id": "code", "name": "Code" },
                { "id": "plan", "name": "Plan" }
            ]
        });
        let mut next_session = 0;
        let mut lines = BufReader::new(agent_r).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let message: Value = serde_json::from_str(&line).expect("client sends JSON");
            lock(&record).push(message.clone());
            let (Some(method), Some(id)) = (message["method"].as_str(), message.get("id")) else {
                continue;
            };
            let result = match method {
                "initialize" => json!({ "protocolVersion": 1, "agentCapabilities": {} }),
                "session/new" => {
                    next_session += 1;
                    json!({ "sessionId": format!("s{next_session}"), "modes": modes })
                }
                "session/resume" => json!({ "modes": modes }),
                "session/prompt" => {
                    for update in &spec.updates {
                        let mut params = update.clone();
                        params["sessionId"] = message["params"]["sessionId"].clone();
                        let notification = json!({ "jsonrpc": "2.0", "method": "session/update", "params": params });
                        agent_w
                            .write_all(format!("{notification}\n").as_bytes())
                            .await
                            .expect("write update");
                    }
                    json!({ "stopReason": "end_turn" })
                }
                _ => json!({}),
            };
            let response = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            agent_w
                .write_all(format!("{response}\n").as_bytes())
                .await
                .expect("write response");
        }
    });
    let (client_r, client_w) = tokio::io::split(client_io);
    (Transport::new(client_r, client_w), received)
}

// ---- rig ----------------------------------------------------------------------------------------

struct Rig {
    feed: CommandFeed,
    events: mpsc::UnboundedReceiver<SinkItem>,
    pending: VecDeque<Envelope>,
    log: Vec<Envelope>,
    bridge: JoinHandle<Result<(), BridgeError>>,
}

impl Rig {
    fn start(
        store: Arc<Store>,
        workdir: &Path,
        connector: Connector,
        tune: impl FnOnce(&mut Config),
    ) -> Self {
        let mut config = Config::new("mch_1", "agt_mock", workdir);
        config.expiry_check_interval = Duration::from_millis(20);
        tune(&mut config);
        let (sink, events) = MemorySink::new();
        let (commands, feed) = MemoryCommands::new();
        let bridge = Bridge::connect_with(
            config,
            connector,
            store,
            Arc::new(SequentialGenerator::new()),
            sink,
            commands,
        )
        .expect("the agent starts");
        Self {
            feed,
            events,
            pending: VecDeque::new(),
            log: Vec::new(),
            bridge: tokio::spawn(bridge.run()),
        }
    }

    fn open_store(state: &Path) -> Arc<Store> {
        Arc::new(Store::open_with_clock(state, Arc::new(ManualClock::new(T0))).expect("store"))
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
            match timeout(STEP, self.events.recv()).await {
                Ok(Some(SinkItem::Events(envelopes))) => self.pending.extend(envelopes),
                Ok(Some(SinkItem::Failure(failure))) => panic!("command failed: {failure:?}"),
                Ok(None) => panic!("the bridge stopped; log so far: {:#?}", self.log),
                Err(_) => panic!("timed out; log so far: {:#?}", self.log),
            }
        }
    }

    fn prompt(&mut self, thread: &str, task: &str) {
        self.feed.send_with_key(
            &format!("idk_{task}"),
            Command::Prompt {
                thread_id: thread.into(),
                task_id: task.into(),
                message_id: format!("msg_{task}"),
                parts: vec![ContentPart::Text {
                    text: "go".to_owned(),
                }],
                author: user(),
            },
        );
    }

    /// Ends the command stream and waits for the bridge to stop cleanly.
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
        log
    }
}

fn is_success(envelope: &Envelope) -> bool {
    matches!(&envelope.event, Event::RunFinished(f)
        if matches!(f.outcome, protocol::turn_event::RunOutcome::Success { .. }))
}

fn is_paused(envelope: &Envelope) -> bool {
    matches!(&envelope.event, Event::RunFinished(f)
        if matches!(f.outcome, protocol::turn_event::RunOutcome::Interrupt { .. }))
}

fn is_run_error(envelope: &Envelope) -> bool {
    matches!(envelope.event, Event::RunError(_))
}

fn run_error_code(envelope: &Envelope) -> Option<String> {
    match &envelope.event {
        Event::RunError(error) => error.code.clone(),
        _ => None,
    }
}

/// A connector that starts the mock agents of `scripts` one after the other and keeps their
/// handles.
fn mock_connector(scripts: Vec<Value>) -> (Connector, Arc<Mutex<Vec<MockAgent>>>) {
    let agents = Arc::new(Mutex::new(Vec::new()));
    let started = Arc::clone(&agents);
    let mut scripts: VecDeque<Value> = scripts.into();
    let connector: Connector = Box::new(move || {
        let script = scripts.pop_front().ok_or(ClientError::Closed)?;
        let (transport, agent) = MockAgent::start(&script);
        lock(&started).push(agent);
        Ok(transport)
    });
    (connector, agents)
}

fn session_snapshot_mode(log: &[Envelope]) -> Vec<String> {
    log.iter()
        .filter_map(|e| match &e.event {
            Event::StateSnapshot(snapshot) if snapshot.scope == StateScope::Session => snapshot
                .snapshot
                .get("current_mode_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            _ => None,
        })
        .collect()
}

// ---- tests --------------------------------------------------------------------------------------

#[tokio::test]
async fn a_dead_agent_fails_running_tasks_and_the_next_prompt_respawns_and_resumes() {
    let state = TempDir::new().expect("state dir");
    let work = TempDir::new().expect("work dir");
    let store = Rig::open_store(state.path());
    // The first agent stops in the middle of its turn, on an approval nobody answers.
    let first = json!({ "turns": [{ "steps": [
        { "text": "one" },
        { "request_permission": {
            "toolCall": { "toolCallId": "call_1", "title": "Run tests", "kind": "execute" },
            "options": [{ "optionId": "allow_once", "name": "Allow", "kind": "allow_once" }] },
          "expect": "any" }
    ]}]});
    let second = json!({ "turns": [{ "steps": [{ "text": "two" }] }] });
    let (connector, agents) = mock_connector(vec![first, second]);
    let mut rig = Rig::start(Arc::clone(&store), work.path(), connector, |_| {});

    rig.prompt("th_1", "tk_1");
    rig.until(is_paused).await;
    assert_eq!(store.open_interrupts().expect("read").len(), 1);
    lock(&agents)[0].kill();

    let rest = rig.until(is_run_error).await;
    let error = rest.last().expect("RunError");
    let Event::RunError(run_error) = &error.event else {
        panic!("RunError last");
    };
    assert_eq!(run_error.reason, RunErrorReason::Error);
    assert_eq!(run_error.code.as_deref(), Some("harness_exited"));
    assert_eq!(error.task_id.as_deref(), Some("tk_1"));

    // `Reattacher::connection_lost`: the stored session is detached, nothing waits on the
    // dead agent's approval.
    let detached = async {
        loop {
            let row = store.session_for_thread("th_1").expect("read");
            if row.is_some_and(|row| row.state == SessionState::Detached) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    timeout(STEP, detached).await.expect("session detached");
    assert!(store.open_interrupts().expect("read").is_empty());
    assert_eq!(
        store.task("tk_1").expect("read").expect("task").state,
        "failed"
    );

    // The next prompt starts a new agent and resumes the thread's session.
    rig.prompt("th_1", "tk_2");
    let stream = rig.until(is_success).await;
    assert!(
        stream
            .iter()
            .any(|e| matches!(&e.event, Event::TextMessageContent(c) if c.delta == "two")),
        "the new agent answered: {stream:#?}"
    );
    let (resumes, news) = {
        let agents = lock(&agents);
        assert_eq!(agents.len(), 2, "one respawn");
        (
            requests(&agents[1].received, "session/resume"),
            requests(&agents[1].received, "session/new"),
        )
    };
    assert_eq!(resumes.len(), 1, "the session is resumed");
    assert_eq!(resumes[0]["params"]["sessionId"], "sess_mock_1");
    assert!(news.is_empty(), "no new session: {news:?}");
    assert_eq!(
        store.task("tk_2").expect("read").expect("task").state,
        "completed"
    );
    rig.finish().await;
}

#[tokio::test]
async fn updates_reach_the_log_as_the_agent_sent_them_and_unparseable_ones_become_raw_events() {
    let state = TempDir::new().expect("state dir");
    let work = TempDir::new().expect("work dir");
    let store = Rig::open_store(state.path());
    // A text chunk with fields the typed schema does not know, and an update kind it rejects.
    let chunk = json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": "hi", "futureField": { "z": 1, "a": [true, null, 2.5] } },
        "vendorField": "kept"
    });
    let future = json!({
        "sessionUpdate": "brand_new_kind",
        "payload": { "n": 1.5, "list": ["x", { "deep": null }] }
    });
    assert!(
        serde_json::from_value::<protocol::acp::SessionNotification>(json!({
            "sessionId": "s1", "update": future
        }))
        .is_err(),
        "the typed schema rejects the update"
    );
    let (transport, _received) = play(Play {
        mode: "code",
        updates: vec![json!({ "update": chunk }), json!({ "update": future })],
    });
    let connector: Connector = {
        let mut transport = Some(transport);
        Box::new(move || transport.take().ok_or(ClientError::Closed))
    };
    let mut rig = Rig::start(store, work.path(), connector, |_| {});
    rig.prompt("th_1", "tk_1");
    rig.until(is_success).await;
    let log = rig.finish().await;

    let payloads: Vec<&Value> = log
        .iter()
        .filter_map(|e| e.raw.as_ref().map(|raw| &raw.payload))
        .collect();
    assert!(
        payloads.contains(&&chunk),
        "raw.payload is the update as sent, unknown fields included: {payloads:#?}"
    );
    let raw_events: Vec<&Envelope> = log
        .iter()
        .filter(|e| matches!(e.event, Event::Raw(_)))
        .collect();
    assert_eq!(raw_events.len(), 1, "the unparseable update is a Raw event");
    let Event::Raw(raw) = &raw_events[0].event else {
        unreachable!("filtered");
    };
    assert_eq!(&Value::Object(raw.event.clone()), &future);
    assert_eq!(
        raw_events[0].raw.as_ref().map(|raw| &raw.payload),
        Some(&future)
    );
}

#[tokio::test]
async fn new_and_resumed_sessions_get_the_snapshot_and_the_threads_mcp_servers() {
    let state = TempDir::new().expect("state dir");
    let work = TempDir::new().expect("work dir");
    let server = |name: &str| -> Value {
        json!({ "type": "stdio", "name": name, "command": "/usr/bin/mcp", "args": ["--serve"], "env": [] })
    };
    // What the agent receives is the schema's own encoding of the server.
    let wire = |name: &str| -> Value {
        let parsed: protocol::acp::McpServer =
            serde_json::from_value(server(name)).expect("server");
        serde_json::to_value(parsed).expect("json")
    };
    let tune = |config: &mut Config| {
        config.mcp_servers = vec![serde_json::from_value(server("shared")).expect("server")];
        config.thread_mcp_servers.insert(
            "th_2".to_owned(),
            vec![serde_json::from_value(server("only_th_2")).expect("server")],
        );
    };

    // First process: two threads, two `session/new`.
    let (transport, received) = play(Play {
        mode: "code",
        updates: Vec::new(),
    });
    let connector: Connector = {
        let mut transport = Some(transport);
        Box::new(move || transport.take().ok_or(ClientError::Closed))
    };
    let mut rig = Rig::start(Rig::open_store(state.path()), work.path(), connector, tune);
    rig.prompt("th_1", "tk_1");
    rig.until(is_success).await;
    rig.prompt("th_2", "tk_2");
    rig.until(is_success).await;
    let log = rig.finish().await;

    let news = requests(&received, "session/new");
    assert_eq!(news.len(), 2);
    assert_eq!(news[0]["params"]["mcpServers"], json!([wire("shared")]));
    assert_eq!(news[1]["params"]["mcpServers"], json!([wire("only_th_2")]));
    assert_eq!(
        session_snapshot_mode(&log),
        ["code", "code"],
        "StateSnapshot{{session}} from each session/new response"
    );
    let snapshot_at = log
        .iter()
        .position(|e| matches!(e.event, Event::StateSnapshot(_)))
        .expect("snapshot");
    let first_run_end = log.iter().position(is_success).expect("success");
    assert!(snapshot_at < first_run_end, "the snapshot opens the thread");

    // Second process, same state: the thread's session is resumed with the same servers, and
    // the resume response is snapshotted.
    let (transport, received) = play(Play {
        mode: "plan",
        updates: Vec::new(),
    });
    let connector: Connector = {
        let mut transport = Some(transport);
        Box::new(move || transport.take().ok_or(ClientError::Closed))
    };
    let mut rig = Rig::start(Rig::open_store(state.path()), work.path(), connector, tune);
    rig.prompt("th_2", "tk_3");
    rig.until(is_success).await;
    let log = rig.finish().await;
    let resumes = requests(&received, "session/resume");
    assert_eq!(resumes.len(), 1);
    assert_eq!(resumes[0]["params"]["sessionId"], "s2");
    assert_eq!(
        resumes[0]["params"]["mcpServers"],
        json!([wire("only_th_2")])
    );
    assert!(requests(&received, "session/new").is_empty());
    assert_eq!(session_snapshot_mode(&log), ["plan"]);
}

#[tokio::test]
async fn interrupts_stored_by_a_previous_process_fail_their_task_at_startup() {
    let state = TempDir::new().expect("state dir");
    let work = TempDir::new().expect("work dir");
    let store = Rig::open_store(state.path());
    // What the previous process left: a paused task with an approval 10 minutes from expiry.
    store
        .upsert_thread("th_1", "agt_mock", &work.path().to_string_lossy())
        .expect("thread");
    store.upsert_task("tk_1", "th_1", "paused").expect("task");
    store
        .save_interrupt(&PendingInterrupt {
            interrupt_id: "int_old".to_owned(),
            thread_id: "th_1".to_owned(),
            task_id: "tk_1".to_owned(),
            kind: InterruptKind::Approval,
            request: json!({
                "sessionId": "s1",
                "toolCall": { "toolCallId": "call_1", "title": "Run tests", "kind": "execute" },
                "options": [{ "optionId": "allow_once", "name": "Allow", "kind": "allow_once" }]
            }),
            expires_at_ms: T0 + 600_000,
        })
        .expect("interrupt");

    // The clock never moves, so only the startup check can end the task.
    let (connector, _agents) = mock_connector(vec![json!({ "turns": [{ "steps": [] }] })]);
    let mut rig = Rig::start(Arc::clone(&store), work.path(), connector, |_| {});
    let ended = rig.until(is_run_error).await;
    let resolved = ended
        .iter()
        .find_map(|e| match &e.event {
            Event::InterruptResolved(resolved) => Some(resolved.clone()),
            _ => None,
        })
        .expect("InterruptResolved");
    assert_eq!(resolved.interrupt_id, "int_old");
    assert_eq!(resolved.outcome, ResolvedOutcome::Expired);
    assert_eq!(
        run_error_code(ended.last().expect("RunError")).as_deref(),
        Some("expired")
    );
    assert!(ended.iter().all(|e| e.task_id.as_deref() == Some("tk_1")));
    assert!(store.open_interrupts().expect("read").is_empty());
    assert_eq!(
        store.task("tk_1").expect("read").expect("task").state,
        "failed"
    );
    rig.finish().await;
}
