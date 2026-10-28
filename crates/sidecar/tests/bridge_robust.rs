//! Runner bridge robustness: harness restart (the agent is started again by a `Connector` or by
//! `spawn_with`), raw updates, the session snapshot and MCP servers at session start, and
//! interrupts left open by an earlier process (ended `harness_exited`, approvals for them answer
//! `unknown_interrupt`).
//!
//! acp-mock runs in process behind a [`Recorder`] (what the bridge sent it), or as a subprocess
//! started by `spawn_with`. A hand-played agent sends the updates acp-mock's typed script cannot
//! express. The store runs on a manual clock and ids are sequential.

use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use acp_mock::Recorder;
use protocol::tunnel::{Actor, ActorKind, Command, CommandErrorCode, ContentPart, Outcome};
use protocol::turn_event::{
    Envelope, Event, ResolvedOutcome, RunErrorReason, RunReason, StateScope,
};
use serde_json::{Value, json};
use sidecar::acp::client::{ClientError, Transport};
use sidecar::bridge::{
    Bridge, BridgeError, CommandFailure, CommandFeed, Config, Connector, MemoryCommands,
    MemorySink, SequentialGenerator, SinkItem,
};
use sidecar::store::{InterruptKind, ManualClock, PendingInterrupt, SessionState, Store};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(20);
const T0: u64 = 1_700_000_000_000;
/// Set by [`a_subprocess_agent_started_by_spawn_with_is_started_again_after_it_is_killed`] for
/// the agent process it starts: the script to serve.
const CHILD_SCRIPT_ENV: &str = "SIDECAR_BRIDGE_ROBUST_SCRIPT";
/// The file the agent process writes its pid to.
const CHILD_PID_ENV: &str = "SIDECAR_BRIDGE_ROBUST_PID";

fn user() -> Actor {
    Actor {
        kind: ActorKind::User,
        id: "usr_1".into(),
        name: None,
        device_id: Some("dev_1".into()),
    }
}

// ---- agents -------------------------------------------------------------------------------------

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The messages the agent received with `method`.
fn requests(recorder: &Recorder, method: &str) -> Vec<Value> {
    recorder
        .messages()
        .into_iter()
        .filter(|message| message["method"] == method)
        .collect()
}

/// An acp-mock running in process, with the log of what the bridge sent it. `kill` ends it the
/// way a dead agent process ends: the bridge's reader sees the end of the stream.
struct MockAgent {
    recorder: Recorder,
    task: JoinHandle<()>,
}

impl MockAgent {
    fn start(script: &Value) -> (Transport, Self) {
        let script = acp_mock::Script::from_value(script).expect("script");
        let recorder = Recorder::new();
        let (client_io, agent_io) = tokio::io::duplex(1 << 16);
        let (agent_r, agent_w) = tokio::io::split(agent_io);
        let task = tokio::spawn({
            let recorder = recorder.clone();
            async move {
                let _ = acp_mock::run_recorded(script, recorder, agent_r, agent_w).await;
            }
        });
        let (client_r, client_w) = tokio::io::split(client_io);
        (Transport::new(client_r, client_w), Self { recorder, task })
    }

    fn kill(&self) {
        self.task.abort();
    }
}

/// A hand-played agent: it sends `updates` (params without `sessionId`) during each prompt.
/// acp-mock cannot send an update its typed script rejects, so the raw-update test needs this.
fn play(updates: Vec<Value>) -> Transport {
    let (client_io, agent_io) = tokio::io::duplex(1 << 16);
    let (agent_r, mut agent_w) = tokio::io::split(agent_io);
    tokio::spawn(async move {
        let mut next_session = 0;
        let mut lines = BufReader::new(agent_r).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let message: Value = serde_json::from_str(&line).expect("client sends JSON");
            let (Some(method), Some(id)) = (message["method"].as_str(), message.get("id")) else {
                continue;
            };
            let result = match method {
                "initialize" => json!({ "protocolVersion": 1, "agentCapabilities": {} }),
                "session/new" => {
                    next_session += 1;
                    json!({ "sessionId": format!("s{next_session}") })
                }
                "session/prompt" => {
                    for update in &updates {
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
    Transport::new(client_r, client_w)
}

/// A connector that hands out `transport` once.
fn once(transport: Transport) -> Connector {
    let mut transport = Some(transport);
    Box::new(move || transport.take().ok_or(ClientError::Closed))
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

/// A hand-played agent that asks for one approval per id in `tool_calls` as soon as a prompt
/// arrives, without waiting for any answer, so the requests are open together. It ends when the
/// returned task is aborted (the bridge then reads the end of its stream).
fn concurrent_approvals(tool_calls: &[&str]) -> (Transport, JoinHandle<()>) {
    let tool_calls: Vec<String> = tool_calls.iter().map(|id| (*id).to_owned()).collect();
    let (client_io, agent_io) = tokio::io::duplex(1 << 16);
    let (agent_r, mut agent_w) = tokio::io::split(agent_io);
    let task = tokio::spawn(async move {
        let mut lines = BufReader::new(agent_r).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let message: Value = serde_json::from_str(&line).expect("client sends JSON");
            let (Some(method), Some(id)) = (message["method"].as_str(), message.get("id")) else {
                continue;
            };
            let result = match method {
                "initialize" => json!({ "protocolVersion": 1, "agentCapabilities": {} }),
                "session/new" => json!({ "sessionId": "s1" }),
                "session/prompt" => {
                    for (index, tool_call) in tool_calls.iter().enumerate() {
                        let request = json!({
                            "jsonrpc": "2.0",
                            "id": 1000 + index,
                            "method": "session/request_permission",
                            "params": {
                                "sessionId": message["params"]["sessionId"],
                                "toolCall": { "toolCallId": tool_call, "title": "Run tests", "kind": "execute" },
                                "options": [{ "optionId": "allow_once", "name": "Allow", "kind": "allow_once" }]
                            }
                        });
                        agent_w
                            .write_all(format!("{request}\n").as_bytes())
                            .await
                            .expect("write request");
                    }
                    continue;
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
    (Transport::new(client_r, client_w), task)
}

/// Waits until the store holds `count` open interrupts (the bridge has registered them).
async fn wait_for_open_interrupts(store: &Store, count: usize) {
    let open = async {
        while store.open_interrupts().expect("read").len() != count {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    timeout(STEP, open)
        .await
        .unwrap_or_else(|_| panic!("{count} open interrupts in the store"));
}

/// The ids of the `InterruptResolved` events with `outcome`, sorted.
fn resolved_ids(log: &[Envelope], outcome: ResolvedOutcome) -> Vec<String> {
    let mut ids: Vec<String> = log
        .iter()
        .filter_map(|e| match &e.event {
            Event::InterruptResolved(r) if r.outcome == outcome => Some(r.interrupt_id.clone()),
            _ => None,
        })
        .collect();
    ids.sort();
    ids
}

/// The runs that start with `RunStarted{resume}`.
fn resumes(log: &[Envelope]) -> usize {
    log.iter()
        .filter(|e| matches!(&e.event, Event::RunStarted(s) if s.reason == RunReason::Resume))
        .count()
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
    /// A bridge that starts its agent with `connector` and again when a prompt needs it.
    fn start(
        store: Arc<Store>,
        workdir: &Path,
        connector: Connector,
        tune: impl FnOnce(&mut Config),
    ) -> Self {
        Self::build(store, workdir, tune, |config, store, sink, commands| {
            Bridge::connect_with(
                config,
                connector,
                store,
                Arc::new(SequentialGenerator::new()),
                sink,
                commands,
            )
            .expect("the agent starts")
        })
    }

    /// A bridge that spawns its agent with `command` and spawns it again when a prompt needs it:
    /// the construction the runner uses for a real agent process.
    fn spawn(
        store: Arc<Store>,
        workdir: &Path,
        command: impl FnMut() -> tokio::process::Command + Send + 'static,
        tune: impl FnOnce(&mut Config),
    ) -> Self {
        Self::build(store, workdir, tune, |config, store, sink, commands| {
            Bridge::spawn_with(
                config,
                command,
                store,
                Arc::new(SequentialGenerator::new()),
                sink,
                commands,
            )
            .expect("the agent starts")
        })
    }

    fn build(
        store: Arc<Store>,
        workdir: &Path,
        tune: impl FnOnce(&mut Config),
        make: impl FnOnce(
            Config,
            Arc<Store>,
            MemorySink,
            MemoryCommands,
        ) -> Bridge<MemorySink, MemoryCommands>,
    ) -> Self {
        let mut config = Config::new("mch_1", "agt_mock", workdir);
        config.expiry_check_interval = Duration::from_millis(20);
        tune(&mut config);
        let (sink, events) = MemorySink::new();
        let (commands, feed) = MemoryCommands::new();
        let bridge = make(config, store, sink, commands);
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

    /// The next command failure. Events received before it are kept for `until`.
    async fn failure(&mut self) -> CommandFailure {
        loop {
            match timeout(STEP, self.events.recv()).await {
                Ok(Some(SinkItem::Failure(failure))) => return failure,
                Ok(Some(SinkItem::Events(envelopes))) => self.pending.extend(envelopes),
                Ok(None) => panic!("the bridge stopped"),
                Err(_) => panic!("no command failure within {STEP:?}"),
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

/// The agent's script for an approval it waits on, then a second turn that answers.
fn stops_on_an_approval() -> Value {
    json!({ "turns": [{ "steps": [
        { "text": "one" },
        { "request_permission": {
            "toolCall": { "toolCallId": "call_1", "title": "Run tests", "kind": "execute" },
            "options": [{ "optionId": "allow_once", "name": "Allow", "kind": "allow_once" }] },
          "expect": "any" }
    ]}]})
}

/// Waits until the thread's stored session is detached (its agent went away).
async fn wait_for_detached(store: &Store) {
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
}

/// What an earlier process left in the store: a paused task with an approval 10 minutes from
/// its expiry.
fn earlier_process_paused(store: &Store, workdir: &Path) {
    store
        .upsert_thread("th_1", "agt_mock", &workdir.to_string_lossy())
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
}

// ---- tests --------------------------------------------------------------------------------------

#[tokio::test]
async fn a_dead_agent_fails_running_tasks_and_the_next_prompt_respawns_and_resumes() {
    let state = TempDir::new().expect("state dir");
    let work = TempDir::new().expect("work dir");
    let store = Rig::open_store(state.path());
    let second = json!({ "turns": [{ "steps": [{ "text": "two" }] }] });
    let (connector, agents) = mock_connector(vec![stops_on_an_approval(), second]);
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
    // SPEC-004: the open approval is cancelled, not expired.
    assert!(
        rest.iter().any(|e| matches!(&e.event,
            Event::InterruptResolved(r) if r.outcome == ResolvedOutcome::Cancelled)),
        "{rest:#?}"
    );
    assert!(
        !rest.iter().any(|e| matches!(&e.event,
            Event::InterruptResolved(r) if r.outcome == ResolvedOutcome::Expired)),
        "{rest:#?}"
    );

    // `Reattacher::connection_lost`: the stored session is detached, nothing waits on the
    // dead agent's approval.
    wait_for_detached(&store).await;
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
            requests(&agents[1].recorder, "session/resume"),
            requests(&agents[1].recorder, "session/new"),
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
async fn a_subprocess_agent_started_by_spawn_with_is_started_again_after_it_is_killed() {
    let state = TempDir::new().expect("state dir");
    let work = TempDir::new().expect("work dir");
    let store = Rig::open_store(state.path());
    let pid_file = work.path().join("agent.pid");
    let mut scripts: VecDeque<Value> = VecDeque::from(vec![
        stops_on_an_approval(),
        json!({ "turns": [{ "steps": [{ "text": "two" }] }] }),
    ]);
    let child_pid_file = pid_file.clone();
    // Each spawn runs this test binary again as the agent (see `subprocess_acp_mock_child`).
    let command = move || {
        let script = scripts
            .pop_front()
            .unwrap_or_else(|| json!({ "turns": [] }));
        let mut command =
            tokio::process::Command::new(std::env::current_exe().expect("test binary"));
        command
            .args([
                "--ignored",
                "--exact",
                "subprocess_acp_mock_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_SCRIPT_ENV, script.to_string())
            .env(CHILD_PID_ENV, &child_pid_file)
            .stderr(Stdio::null());
        command
    };
    let mut rig = Rig::spawn(Arc::clone(&store), work.path(), command, |_| {});

    rig.prompt("th_1", "tk_1");
    rig.until(is_paused).await;
    let first_pid = read_pid(&pid_file);
    let killed = std::process::Command::new("kill")
        .args(["-KILL", &first_pid.to_string()])
        .status()
        .expect("kill");
    assert!(killed.success(), "the agent process was killed");

    let rest = rig.until(is_run_error).await;
    assert_eq!(
        run_error_code(rest.last().expect("RunError")).as_deref(),
        Some("harness_exited")
    );
    wait_for_detached(&store).await;

    rig.prompt("th_1", "tk_2");
    let stream = rig.until(is_success).await;
    assert!(
        stream
            .iter()
            .any(|e| matches!(&e.event, Event::TextMessageContent(c) if c.delta == "two")),
        "the new agent process answered: {stream:#?}"
    );
    assert_ne!(read_pid(&pid_file), first_pid, "a new agent process");
    rig.finish().await;
}

fn read_pid(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .expect("the agent wrote its pid")
        .trim()
        .parse()
        .expect("a pid")
}

/// The agent for [`a_subprocess_agent_started_by_spawn_with_is_started_again_after_it_is_killed`].
/// It is not a test: it runs only when the parent re-runs this binary with the script in
/// `CHILD_SCRIPT_ENV`.
#[tokio::test]
#[ignore = "run by a_subprocess_agent_started_by_spawn_with_is_started_again_after_it_is_killed"]
async fn subprocess_acp_mock_child() {
    let Ok(script) = std::env::var(CHILD_SCRIPT_ENV) else {
        return;
    };
    let pid_file = std::env::var(CHILD_PID_ENV).expect("the parent names the pid file");
    std::fs::write(pid_file, std::process::id().to_string()).expect("write the pid");
    let script = acp_mock::Script::from_json(&script).expect("valid script");
    // The test harness has printed a line without its newline. End that line so the first
    // JSON message starts on its own line.
    println!();
    acp_mock::run(script, tokio::io::stdin(), tokio::io::stdout())
        .await
        .expect("agent serves until stdin closes");
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
    let transport = play(vec![
        json!({ "update": chunk }),
        json!({ "update": future }),
    ]);
    let mut rig = Rig::start(store, work.path(), once(transport), |_| {});
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
    // acp-mock advertises the session's modes on session/new, session/resume and session/load.
    let script = |mode: &str| {
        json!({
            "modes": {
                "currentModeId": mode,
                "availableModes": [{ "id": "code", "name": "Code" }, { "id": "plan", "name": "Plan" }]
            },
            "turns": [{ "steps": [] }]
        })
    };

    // First process: two threads, two `session/new`.
    let (connector, agents) = mock_connector(vec![script("code")]);
    let mut rig = Rig::start(Rig::open_store(state.path()), work.path(), connector, tune);
    rig.prompt("th_1", "tk_1");
    rig.until(is_success).await;
    rig.prompt("th_2", "tk_2");
    rig.until(is_success).await;
    let log = rig.finish().await;

    let first = lock(&agents)[0].recorder.clone();
    let news = requests(&first, "session/new");
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
    let (connector, agents) = mock_connector(vec![script("plan")]);
    let mut rig = Rig::start(Rig::open_store(state.path()), work.path(), connector, tune);
    rig.prompt("th_2", "tk_3");
    rig.until(is_success).await;
    let log = rig.finish().await;
    let second = lock(&agents)[0].recorder.clone();
    let resumes = requests(&second, "session/resume");
    assert_eq!(resumes.len(), 1);
    assert_eq!(resumes[0]["params"]["sessionId"], "sess_mock_2");
    assert_eq!(
        resumes[0]["params"]["mcpServers"],
        json!([wire("only_th_2")])
    );
    assert!(requests(&second, "session/new").is_empty());
    assert_eq!(session_snapshot_mode(&log), ["plan"]);
}

#[tokio::test]
async fn interrupts_stored_by_a_previous_process_fail_their_task_at_startup() {
    let state = TempDir::new().expect("state dir");
    let work = TempDir::new().expect("work dir");
    let store = Rig::open_store(state.path());
    earlier_process_paused(&store, work.path());

    // The clock never moves, so the TTL cannot end the task: only the startup check can.
    let (connector, _agents) = mock_connector(vec![json!({ "turns": [] })]);
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
    assert_eq!(resolved.outcome, ResolvedOutcome::Cancelled);
    assert_eq!(
        run_error_code(ended.last().expect("RunError")).as_deref(),
        Some("harness_exited")
    );
    assert!(ended.iter().all(|e| e.task_id.as_deref() == Some("tk_1")));
    assert!(store.open_interrupts().expect("read").is_empty());
    assert_eq!(
        store.task("tk_1").expect("read").expect("task").state,
        "failed"
    );
    rig.finish().await;
}

#[tokio::test]
async fn an_approval_for_an_interrupt_an_earlier_process_stored_answers_unknown_interrupt() {
    let state = TempDir::new().expect("state dir");
    let work = TempDir::new().expect("work dir");
    let store = Rig::open_store(state.path());
    earlier_process_paused(&store, work.path());

    let (connector, _agents) = mock_connector(vec![json!({ "turns": [] })]);
    let mut rig = Rig::start(Arc::clone(&store), work.path(), connector, |_| {});
    // Startup ends the task; this process has not loaded the thread.
    rig.until(is_run_error).await;
    rig.feed.send(Command::Approval {
        thread_id: "th_1".into(),
        task_id: "tk_1".into(),
        interrupt_id: "int_old".into(),
        outcome: Outcome::Selected {
            option_id: "allow_once".into(),
        },
        resolved_by: user(),
    });
    let failure = rig.failure().await;
    assert_eq!(failure.code, CommandErrorCode::UnknownInterrupt);
    rig.finish().await;
}

#[tokio::test]
async fn an_approval_for_a_thread_no_process_has_stored_answers_unknown_thread() {
    let state = TempDir::new().expect("state dir");
    let work = TempDir::new().expect("work dir");
    let (connector, _agents) = mock_connector(vec![json!({ "turns": [] })]);
    let mut rig = Rig::start(
        Rig::open_store(state.path()),
        work.path(),
        connector,
        |_| {},
    );
    rig.feed.send(Command::Approval {
        thread_id: "th_9".into(),
        task_id: "tk_9".into(),
        interrupt_id: "int_9".into(),
        outcome: Outcome::Selected {
            option_id: "allow_once".into(),
        },
        resolved_by: user(),
    });
    let failure = rig.failure().await;
    assert_eq!(failure.code, CommandErrorCode::UnknownThread);
    rig.finish().await;
}

#[tokio::test]
async fn a_harness_exit_ends_every_open_interrupt_of_the_task_before_the_task_errors() {
    let state = TempDir::new().expect("state dir");
    let work = TempDir::new().expect("work dir");
    let store = Rig::open_store(state.path());
    let (transport, agent) = concurrent_approvals(&["call_1", "call_2"]);
    let mut rig = Rig::start(Arc::clone(&store), work.path(), once(transport), |_| {});

    rig.prompt("th_1", "tk_1");
    rig.until(is_paused).await;
    // The second request arrives while the run is paused: the bridge holds it open too.
    wait_for_open_interrupts(&store, 2).await;
    let mut stored: Vec<String> = store
        .open_interrupts()
        .expect("read")
        .into_iter()
        .map(|interrupt| interrupt.interrupt_id)
        .collect();
    stored.sort();
    agent.abort();

    let rest = rig.until(is_run_error).await;
    assert_eq!(
        resolved_ids(&rest, ResolvedOutcome::Cancelled),
        stored,
        "{rest:#?}"
    );
    assert!(
        resolved_ids(&rest, ResolvedOutcome::Expired).is_empty(),
        "{rest:#?}"
    );
    // Each open interrupt is resolved, then its run resumes (turn-event.md §5 rule 6).
    assert_eq!(resumes(&rest), 2, "{rest:#?}");
    assert_eq!(
        run_error_code(rest.last().expect("RunError")).as_deref(),
        Some("harness_exited")
    );
    wait_for_detached(&store).await;
    assert!(store.open_interrupts().expect("read").is_empty());
    assert_eq!(
        store.task("tk_1").expect("read").expect("task").state,
        "failed"
    );
    rig.finish().await;
}

#[tokio::test]
async fn every_interrupt_an_earlier_process_left_open_is_cancelled_at_startup() {
    let state = TempDir::new().expect("state dir");
    let work = TempDir::new().expect("work dir");
    let store = Rig::open_store(state.path());
    earlier_process_paused(&store, work.path());
    store
        .save_interrupt(&PendingInterrupt {
            interrupt_id: "int_old_2".to_owned(),
            thread_id: "th_1".to_owned(),
            task_id: "tk_1".to_owned(),
            kind: InterruptKind::Approval,
            request: json!({
                "sessionId": "s1",
                "toolCall": { "toolCallId": "call_2", "title": "Run lint", "kind": "execute" },
                "options": [{ "optionId": "allow_once", "name": "Allow", "kind": "allow_once" }]
            }),
            expires_at_ms: T0 + 600_000,
        })
        .expect("interrupt");

    let (connector, _agents) = mock_connector(vec![json!({ "turns": [] })]);
    let mut rig = Rig::start(Arc::clone(&store), work.path(), connector, |_| {});
    let ended = rig.until(is_run_error).await;
    assert_eq!(
        resolved_ids(&ended, ResolvedOutcome::Cancelled),
        ["int_old", "int_old_2"],
        "{ended:#?}"
    );
    assert_eq!(resumes(&ended), 2, "{ended:#?}");
    assert_eq!(
        run_error_code(ended.last().expect("RunError")).as_deref(),
        Some("harness_exited")
    );
    assert!(ended.iter().all(|e| e.task_id.as_deref() == Some("tk_1")));
    assert!(store.open_interrupts().expect("read").is_empty());
    rig.finish().await;
}
