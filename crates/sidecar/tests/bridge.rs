//! Runner bridge tests: an in-process acp-mock (or a hand-played agent) on one side, in-memory
//! `CommandSource` and `EventSink` on the other. The store runs on a manual clock and ids are
//! sequential, so the produced stream is deterministic.
//!
//! The hub emits `UserMessage` and the accepted `InterruptResolved` (client-api.md §6). The
//! harness plays the hub: it appends those two to the log it checks, at the point where the hub
//! would have appended them, so the whole log can go through `SequenceValidator`.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use jsonschema::{Retrieve, Uri};
use protocol::mapping::acp_turn::{SequenceValidator, validate_complete};
use protocol::tunnel::{Actor, ActorKind, Command, CommandErrorCode, ContentPart, Outcome};
use protocol::turn_event::{
    ActivityMode, ActivityType, Envelope, Event, InterruptResolved, ResolvedOutcome,
    RunErrorReason, RunOutcome, RunReason, ThreadSeq, ToolCallEndStatus, UserMessage,
};
use serde_json::{Value, json};
use sidecar::bridge::{
    Bridge, BridgeError, CommandFailure, CommandFeed, Config, MemoryCommands, MemorySink,
    SequentialGenerator, SinkItem,
};
use sidecar::store::{ManualClock, Store};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(20);
const T0: u64 = 1_700_000_000_000;
const SCHEMA_BASE: &str = "https://sprite-machine.dev/schemas/turn-event/v1/";

fn user() -> Actor {
    Actor {
        kind: ActorKind::User,
        id: "usr_1".into(),
        name: None,
        device_id: Some("dev_1".into()),
    }
}

fn allow_deny() -> Value {
    json!([
        { "optionId": "allow_once", "name": "Allow", "kind": "allow_once" },
        { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" }
    ])
}

fn permission_step(call: &str, expect: &str) -> Value {
    json!({ "request_permission": {
        "toolCall": { "toolCallId": call, "title": "Run tests", "kind": "execute" },
        "options": allow_deny() },
      "expect": expect })
}

// ---- harness ----------------------------------------------------------------------------------

struct Harness {
    feed: CommandFeed,
    events: mpsc::UnboundedReceiver<SinkItem>,
    pending: VecDeque<Envelope>,
    failures: VecDeque<CommandFailure>,
    /// Everything consumed so far, with the hub's events interleaved where the hub emits them.
    log: Vec<Envelope>,
    clock: Arc<ManualClock>,
    bridge: JoinHandle<Result<(), BridgeError>>,
    workdir: PathBuf,
    _dirs: Vec<TempDir>,
}

impl Harness {
    async fn start(script: Value, tune: impl FnOnce(&mut Config)) -> Self {
        let script = acp_mock::Script::from_value(&script).expect("script");
        let (client_io, agent_io) = tokio::io::duplex(1 << 16);
        let (agent_r, agent_w) = tokio::io::split(agent_io);
        tokio::spawn(acp_mock::run(script, agent_r, agent_w));
        let (client_r, client_w) = tokio::io::split(client_io);
        Self::with_agent(client_r, client_w, tune)
    }

    fn with_agent<R, W>(reader: R, writer: W, tune: impl FnOnce(&mut Config)) -> Self
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
        W: tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        let state = TempDir::new().expect("state dir");
        let work = TempDir::new().expect("work dir");
        let mut h = Self::with_state(reader, writer, state.path(), work.path(), tune);
        h._dirs = vec![state, work];
        h
    }

    fn with_state<R, W>(
        reader: R,
        writer: W,
        state: &Path,
        workdir: &Path,
        tune: impl FnOnce(&mut Config),
    ) -> Self
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
        W: tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        let clock = Arc::new(ManualClock::new(T0));
        let store = Arc::new(Store::open_with_clock(state, clock.clone()).expect("store"));
        let mut config = Config::new("mch_1", "agt_mock", workdir);
        config.expiry_check_interval = Duration::from_millis(20);
        tune(&mut config);
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
            pending: VecDeque::new(),
            failures: VecDeque::new(),
            log: Vec::new(),
            clock,
            bridge: tokio::spawn(bridge.run()),
            workdir: workdir.to_path_buf(),
            _dirs: Vec::new(),
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

    fn hub_event(&mut self, thread: &str, task: &str, event: impl Into<Event>) {
        self.log.push(Envelope::new(
            thread,
            Some(task.to_owned()),
            None,
            "2023-11-14T22:13:20.000Z",
            event,
        ));
    }

    /// The hub appends `UserMessage`, then the command reaches the runner.
    fn prompt(&mut self, thread: &str, task: &str, text: &str) -> String {
        self.hub_event(
            thread,
            task,
            UserMessage {
                message_id: format!("msg_u_{task}"),
                parts: vec![ContentPart::Text { text: text.into() }],
                author: user(),
            },
        );
        // Keys are unique per task: the store remembers them across a restart.
        let key = format!("idk_{task}");
        self.feed.send_with_key(
            &key,
            Command::Prompt {
                thread_id: thread.into(),
                task_id: task.into(),
                message_id: format!("msg_u_{task}"),
                parts: vec![ContentPart::Text { text: text.into() }],
                author: user(),
            },
        );
        key
    }

    /// The hub appends `InterruptResolved` for the accepted answer, then sends the command.
    fn approve(&mut self, thread: &str, task: &str, interrupt: &str, option: &str) -> String {
        self.hub_event(
            thread,
            task,
            InterruptResolved {
                interrupt_id: interrupt.into(),
                outcome: ResolvedOutcome::Selected {
                    option_id: option.into(),
                },
                resolved_by: user(),
            },
        );
        self.feed.send(Command::Approval {
            thread_id: thread.into(),
            task_id: task.into(),
            interrupt_id: interrupt.into(),
            outcome: Outcome::Selected {
                option_id: option.into(),
            },
            resolved_by: user(),
        })
    }

    async fn interrupt_id(&mut self) -> String {
        let seen = self
            .until(|e| matches!(e.event, Event::RunFinished(_)))
            .await;
        interrupt_of(&seen)
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

fn interrupt_of(events: &[Envelope]) -> String {
    events
        .iter()
        .find_map(|e| match &e.event {
            Event::Interrupt(interrupt) => Some(interrupt.interrupt_id.clone()),
            _ => None,
        })
        .expect("an Interrupt")
}

fn is_success(envelope: &Envelope) -> bool {
    matches!(&envelope.event, Event::RunFinished(f) if matches!(f.outcome, RunOutcome::Success { .. }))
}

fn is_run_error(envelope: &Envelope) -> bool {
    matches!(envelope.event, Event::RunError(_))
}

fn types(events: &[Envelope]) -> Vec<&'static str> {
    events.iter().map(|e| e.event.type_name()).collect()
}

// ---- schema -------------------------------------------------------------------------------------

struct SchemaFiles {
    dir: PathBuf,
}

impl Retrieve for SchemaFiles {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let relative = uri
            .as_str()
            .strip_prefix(SCHEMA_BASE)
            .ok_or_else(|| format!("unexpected schema reference {uri}"))?;
        let text = fs::read_to_string(self.dir.join(relative))?;
        Ok(serde_json::from_str(&text)?)
    }
}

fn assert_schema_valid(envelopes: &[Envelope]) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../protocol/schemas/turn-event");
    let text = fs::read_to_string(dir.join("envelope.schema.json")).expect("envelope schema");
    let root: Value = serde_json::from_str(&text).expect("schema is JSON");
    let validator = jsonschema::options()
        .with_retriever(SchemaFiles { dir })
        .build(&root)
        .expect("schema compiles");
    for envelope in envelopes {
        let value = serde_json::to_value(envelope).expect("envelope serializes");
        let errors: Vec<String> = validator
            .iter_errors(&value)
            .map(|error| error.to_string())
            .collect();
        assert!(
            errors.is_empty(),
            "{value}\nviolates the schema: {errors:?}"
        );
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
    assert_schema_valid(log);
}

fn golden_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/bridge")
        .join(name)
}

/// Compares the stamped log with the fixture. Ids are sequential and the clock is manual, so
/// the comparison is exact. `BRIDGE_UPDATE_GOLDEN=1` rewrites the fixture.
fn assert_golden(name: &str, log: &[Envelope]) {
    let mut seq = ThreadSeq::new();
    let stamped: Vec<Value> = log
        .iter()
        .map(|e| serde_json::to_value(seq.stamp(e.clone()).expect("seq 0")).expect("json"))
        .collect();
    let actual = Value::Array(stamped);
    let path = golden_path(name);
    if std::env::var_os("BRIDGE_UPDATE_GOLDEN").is_some() {
        let mut text = serde_json::to_string_pretty(&actual).expect("json");
        text.push('\n');
        fs::write(&path, text).expect("write fixture");
        return;
    }
    let expected: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("fixture exists")).expect("JSON");
    assert_eq!(
        actual,
        expected,
        "stream differs from {}; rerun with BRIDGE_UPDATE_GOLDEN=1 to rewrite it",
        path.display()
    );
}

// ---- tests --------------------------------------------------------------------------------------

fn golden_script() -> Value {
    json!({ "turns": [{ "steps": [
        { "text": "Reading the file." },
        { "thought": "The parser drops the newline." },
        { "tool_call": { "toolCallId": "call_1", "title": "Edit parser.rs",
                         "kind": "edit", "status": "in_progress" } },
        { "tool_call_update": { "toolCallId": "call_1", "status": "completed",
            "content": [{ "type": "diff", "path": "/work/parser.rs",
                          "oldText": "a", "newText": "b" }] } },
        { "plan": [{ "content": "Run the tests", "priority": "high", "status": "pending" }] },
        { "usage": { "used": 120, "size": 1000 } },
        permission_step("call_2", "allow"),
        { "tool_call_update": { "toolCallId": "call_2", "status": "completed" } },
        { "text": "All green." },
        { "stop": "end_turn" }
    ]}]})
}

#[tokio::test]
async fn scripted_turn_with_permission_matches_the_golden_stream() {
    let mut h = Harness::start(golden_script(), |_| {}).await;
    h.prompt("th_1", "tk_1", "fix the parser");
    let first = h
        .until(|e| matches!(&e.event, Event::RunFinished(f) if matches!(f.outcome, RunOutcome::Interrupt { .. })))
        .await;
    let interrupt = interrupt_of(&first);
    assert_eq!(
        types(&first)[..2],
        ["RunStarted", "TextMessageStart"],
        "the hub's UserMessage is not the runner's: {first:#?}"
    );
    h.approve("th_1", "tk_1", &interrupt, "allow_once");
    let second = h.until(is_success).await;
    let Event::RunStarted(resume) = &second[0].event else {
        panic!("the resume run starts first: {second:#?}");
    };
    assert_eq!(resume.reason, RunReason::Resume);
    assert_eq!(resume.parent_run_id, first[0].run_id);
    let log = h.finish().await;

    let all = types(&log);
    for expected in [
        "TextMessageStart",
        "ReasoningMessageStart",
        "ToolCallStart",
        "Activity",
        "StateSnapshot",
        "Interrupt",
        "RunFinished",
        "InterruptResolved",
        "RunStarted",
        "ToolCallEnd",
    ] {
        assert!(all.contains(&expected), "{expected} missing from {all:?}");
    }
    assert!(log.iter().all(|e| e.seq == 0), "producers send seq 0");
    assert_valid_log(&log);
    assert_golden("scripted_turn.json", &log);
}

#[tokio::test]
async fn emit_user_message_flag_makes_the_bridge_emit_it() {
    let script = json!({ "turns": [{ "steps": [{ "text": "hi" }] }] });
    let mut h = Harness::start(script, |config| config.emit_user_message = true).await;
    h.feed.send(Command::Prompt {
        thread_id: "th_1".into(),
        task_id: "tk_1".into(),
        message_id: "msg_u1".into(),
        parts: vec![ContentPart::Text { text: "go".into() }],
        author: user(),
    });
    let stream = h.until(is_success).await;
    assert_eq!(types(&stream)[..2], ["UserMessage", "RunStarted"]);
    let Event::UserMessage(message) = &stream[0].event else {
        panic!("UserMessage first");
    };
    assert_eq!(message.message_id, "msg_u1", "the command's id is kept");
    let log = h.finish().await;
    assert_valid_log(&log);
}

#[tokio::test]
async fn approval_for_an_unknown_interrupt_yields_command_result() {
    let script =
        json!({ "turns": [{ "steps": [permission_step("call_1", "allow"), { "text": "ok" }] }] });
    let mut h = Harness::start(script, |_| {}).await;
    h.prompt("th_1", "tk_1", "go");
    let interrupt = h.interrupt_id().await;

    // Unknown interrupt id in a known thread.
    let key = h.feed.send(Command::Approval {
        thread_id: "th_1".into(),
        task_id: "tk_1".into(),
        interrupt_id: "int_nope".into(),
        outcome: Outcome::Cancelled,
        resolved_by: user(),
    });
    let failure = h.failure().await;
    assert_eq!(failure.code, CommandErrorCode::UnknownInterrupt);
    assert_eq!(failure.idempotency_key, key);

    // Unknown thread.
    let key = h.feed.send(Command::Approval {
        thread_id: "th_other".into(),
        task_id: "tk_1".into(),
        interrupt_id: interrupt.clone(),
        outcome: Outcome::Cancelled,
        resolved_by: user(),
    });
    let failure = h.failure().await;
    assert_eq!(failure.code, CommandErrorCode::UnknownThread);
    assert_eq!(failure.idempotency_key, key);

    // The real interrupt is still open and works.
    h.approve("th_1", "tk_1", &interrupt, "allow_once");
    h.until(is_success).await;
    let processed = h.feed.processed();
    assert_eq!(processed, [1, 2, 3, 4], "every frame is reported processed");
    let log = h.finish().await;
    assert_valid_log(&log);
}

#[tokio::test]
async fn duplicate_idempotency_keys_are_ignored() {
    let script = json!({ "turns": [
        { "steps": [{ "text": "first" }] },
        { "steps": [{ "text": "second" }] }
    ]});
    let mut h = Harness::start(script, |_| {}).await;
    let key = h.prompt("th_1", "tk_1", "one");
    h.until(is_success).await;
    // A retransmit of the same command (new seq, same key) has no effect ...
    h.feed.send_with_key(
        &key,
        Command::Prompt {
            thread_id: "th_1".into(),
            task_id: "tk_1".into(),
            message_id: "msg_u_tk_1".into(),
            parts: vec![ContentPart::Text { text: "one".into() }],
            author: user(),
        },
    );
    // ... and a command with a new key still runs, after it.
    h.prompt("th_1", "tk_2", "two");
    let second = h.until(is_success).await;
    assert_eq!(
        types(&second)[0],
        "RunStarted",
        "the retransmit started nothing: {second:#?}"
    );
    assert_eq!(second[0].task_id.as_deref(), Some("tk_2"));
    assert_eq!(
        h.feed.processed(),
        [1, 2, 3],
        "the duplicate is still acked"
    );
    let log = h.finish().await;
    let starts = log
        .iter()
        .filter(|e| matches!(&e.event, Event::RunStarted(r) if r.reason == RunReason::Prompt))
        .count();
    assert_eq!(starts, 2);
    assert_valid_log(&log);
}

#[tokio::test]
async fn cancel_during_a_paused_approval_resolves_resumes_and_errors() {
    let script = json!({ "turns": [{ "steps": [
        { "text": "Working." },
        { "tool_call": { "toolCallId": "call_0", "title": "Build", "kind": "execute",
                         "status": "in_progress" } },
        permission_step("call_1", "any"),
        { "text": "never sent" }
    ]}]});
    let mut h = Harness::start(script, |_| {}).await;
    h.prompt("th_1", "tk_1", "go");
    let first = h.until(|e| matches!(&e.event, Event::RunFinished(_))).await;
    let interrupt = interrupt_of(&first);

    h.feed.send(Command::Cancel {
        thread_id: "th_1".into(),
        task_id: "tk_1".into(),
    });
    let rest = h.until(is_run_error).await;
    // turn-event.md §4.9: InterruptResolved{cancelled}, a short resume run that closes the open
    // tool calls (both of them), then RunError{cancelled}.
    assert_eq!(
        types(&rest),
        [
            "InterruptResolved",
            "RunStarted",
            "ToolCallEnd",
            "ToolCallEnd",
            "RunError"
        ]
    );
    let Event::InterruptResolved(resolved) = &rest[0].event else {
        panic!("InterruptResolved first");
    };
    assert_eq!(resolved.interrupt_id, interrupt);
    assert_eq!(resolved.outcome, ResolvedOutcome::Cancelled);
    let Event::RunStarted(resume) = &rest[1].event else {
        panic!("resume RunStarted");
    };
    assert_eq!(resume.reason, RunReason::Resume);
    let Event::RunError(error) = &rest[4].event else {
        panic!("RunError last");
    };
    assert_eq!(error.reason, RunErrorReason::Cancelled);
    for end in &rest[2..4] {
        assert!(
            matches!(&end.event, Event::ToolCallEnd(end) if end.status == ToolCallEndStatus::Failed)
        );
    }

    // The thread takes the next prompt: the cancelled task is gone, and its late stop reason
    // from the agent adds nothing.
    let log = h.finish().await;
    assert_valid_log(&log);
    let errors = log.iter().filter(|e| is_run_error(e)).count();
    assert_eq!(errors, 1, "one RunError for the task: {:#?}", types(&log));
}

#[tokio::test]
async fn a_second_prompt_while_a_task_runs_and_capacity_are_refused() {
    let script = json!({ "turns": [
        { "steps": [permission_step("call_1", "any")] }
    ]});
    let mut h = Harness::start(script, |config| config.max_running_tasks = 1).await;
    h.prompt("th_1", "tk_1", "go");
    h.interrupt_id().await;

    let key = h.feed.send(Command::Prompt {
        thread_id: "th_1".into(),
        task_id: "tk_2".into(),
        message_id: "msg_x".into(),
        parts: vec![ContentPart::Text {
            text: "again".into(),
        }],
        author: user(),
    });
    let failure = h.failure().await;
    assert_eq!(failure.code, CommandErrorCode::Invalid);
    assert_eq!(failure.idempotency_key, key);

    let key = h.feed.send(Command::Prompt {
        thread_id: "th_2".into(),
        task_id: "tk_3".into(),
        message_id: "msg_y".into(),
        parts: vec![ContentPart::Text {
            text: "other".into(),
        }],
        author: user(),
    });
    let failure = h.failure().await;
    assert_eq!(failure.code, CommandErrorCode::Capacity);
    assert_eq!(failure.idempotency_key, key);
    assert_eq!(failure.message, "1 sessions already running");

    // Cancel for a thread nobody knows.
    let key = h.feed.send(Command::Cancel {
        thread_id: "th_none".into(),
        task_id: "tk_9".into(),
    });
    let failure = h.failure().await;
    assert_eq!(failure.code, CommandErrorCode::UnknownThread);
    assert_eq!(failure.idempotency_key, key);
}

#[tokio::test]
async fn terminal_requests_run_on_a_pty_and_appear_as_activity() {
    let script = json!({ "turns": [{ "steps": [
        { "terminal": { "command": "pwd", "toolCallId": "call_t" } },
        { "text": "done" }
    ]}]});
    let mut h = Harness::start(script, |_| {}).await;
    h.prompt("th_1", "tk_1", "where am I");
    h.until(is_success).await;
    let workdir_name = h
        .workdir
        .as_path()
        .file_name()
        .and_then(|name| name.to_str())
        .expect("work dir name")
        .to_owned();
    let log = h.finish().await;

    let activity: Vec<&Envelope> = log
        .iter()
        .filter(
            |e| matches!(&e.event, Event::Activity(a) if a.activity_type == ActivityType::Terminal),
        )
        .collect();
    assert!(activity.len() >= 3, "snapshot, append(s), closing snapshot");
    let modes: Vec<ActivityMode> = activity
        .iter()
        .map(|e| match &e.event {
            Event::Activity(a) => a.mode,
            _ => unreachable!("filtered"),
        })
        .collect();
    assert_eq!(modes.first(), Some(&ActivityMode::Snapshot));
    assert_eq!(modes.last(), Some(&ActivityMode::Snapshot));
    assert!(modes.contains(&ActivityMode::Append));
    let output: String = activity
        .iter()
        .filter_map(|e| match &e.event {
            Event::Activity(a) if a.mode == ActivityMode::Append => a
                .content
                .as_ref()
                .and_then(|c| c.get("output"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            _ => None,
        })
        .collect();
    assert!(
        output.contains(&workdir_name),
        "pwd prints the workspace root ({workdir_name}): {output:?}"
    );
    let closing = match &activity[activity.len() - 1].event {
        Event::Activity(a) => a.content.clone().expect("content"),
        _ => unreachable!("filtered"),
    };
    assert_eq!(closing["exit"]["exit_code"], 0);
    assert!(
        activity
            .iter()
            .all(|e| e.task_id.as_deref() == Some("tk_1") && e.run_id.as_deref() == Some("run_1"))
    );
    assert_valid_log(&log);
}

/// An agent played by hand: it has no fs steps in acp-mock.
async fn fs_agent(
    reader: tokio::io::ReadHalf<tokio::io::DuplexStream>,
    mut writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    outside: PathBuf,
    done: tokio::sync::oneshot::Sender<Vec<Value>>,
) {
    let mut lines = BufReader::new(reader).lines();
    let mut results = Vec::new();
    let mut done = Some(done);
    while let Ok(Some(line)) = lines.next_line().await {
        let message: Value = serde_json::from_str(&line).expect("client sends JSON");
        let (Some(method), Some(id)) = (message["method"].as_str(), message.get("id")) else {
            continue;
        };
        let result = match method {
            "initialize" => json!({ "protocolVersion": 1, "agentCapabilities": {} }),
            "session/new" => json!({ "sessionId": "s1" }),
            "session/prompt" => {
                let requests = [
                    (
                        "fs/write_text_file",
                        json!({ "sessionId": "s1", "path": "notes/a.txt", "content": "hello\nworld\n" }),
                    ),
                    (
                        "fs/read_text_file",
                        json!({ "sessionId": "s1", "path": "notes/a.txt", "line": 2 }),
                    ),
                    (
                        "fs/read_text_file",
                        json!({ "sessionId": "s1", "path": outside }),
                    ),
                    (
                        "fs/read_text_file",
                        json!({ "sessionId": "s1", "path": "../escape.txt" }),
                    ),
                ];
                for (n, (method, params)) in requests.into_iter().enumerate() {
                    let request = json!({ "jsonrpc": "2.0", "id": 100 + n, "method": method, "params": params });
                    writer
                        .write_all(format!("{request}\n").as_bytes())
                        .await
                        .expect("write request");
                    loop {
                        let line = lines.next_line().await.expect("read").expect("client open");
                        let reply: Value = serde_json::from_str(&line).expect("JSON");
                        if reply["id"] == json!(100 + n) {
                            results.push(reply);
                            break;
                        }
                    }
                }
                if let Some(done) = done.take() {
                    // The test reads the results right after the prompt ends.
                    let _ = done.send(std::mem::take(&mut results));
                }
                json!({ "stopReason": "end_turn" })
            }
            _ => json!({}),
        };
        let response = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        writer
            .write_all(format!("{response}\n").as_bytes())
            .await
            .expect("write response");
    }
}

#[tokio::test]
async fn fs_requests_are_served_by_the_workspace_and_confined() {
    let outside_dir = TempDir::new().expect("outside dir");
    let outside = outside_dir.path().join("secret.txt");
    fs::write(&outside, "secret").expect("write outside");

    let (client_io, agent_io) = tokio::io::duplex(1 << 16);
    let (agent_r, agent_w) = tokio::io::split(agent_io);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(fs_agent(agent_r, agent_w, outside, done_tx));
    let (client_r, client_w) = tokio::io::split(client_io);
    let mut h = Harness::with_agent(client_r, client_w, |_| {});

    h.prompt("th_1", "tk_1", "write a note");
    h.until(is_success).await;
    let results = timeout(STEP, done_rx)
        .await
        .expect("timely")
        .expect("results");

    let written = fs::read_to_string(h.workdir.join("notes/a.txt")).expect("file written");
    assert_eq!(written, "hello\nworld\n");
    assert!(results[0]["result"].is_object(), "write ok: {}", results[0]);
    assert_eq!(results[1]["result"]["content"], "world\n", "line 2 onward");
    assert!(
        results[2].get("error").is_some(),
        "an absolute path outside the workspace is refused: {}",
        results[2]
    );
    assert!(
        results[3].get("error").is_some(),
        "a .. path is refused: {}",
        results[3]
    );
    let log = h.finish().await;
    assert_valid_log(&log);
}

#[tokio::test]
async fn an_unanswered_approval_expires_at_its_ttl() {
    let script = json!({ "turns": [{ "steps": [permission_step("call_1", "any")] }] });
    let mut h = Harness::start(script, |config| {
        config.approval_ttl = Duration::from_secs(60);
    })
    .await;
    h.prompt("th_1", "tk_1", "go");
    let interrupt = h.interrupt_id().await;
    let open = h
        .log
        .iter()
        .find_map(|e| match &e.event {
            Event::Interrupt(interrupt) => Some(interrupt.clone()),
            _ => None,
        })
        .expect("the Interrupt is in the log");
    assert_eq!(open.expires_at, "2023-11-14T22:14:20.000Z", "T0 + 60 s");

    // Inside the TTL nothing happens; past it the task ends.
    h.clock.advance(59_000);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.pending.is_empty());
    h.clock.advance(2_000);
    let rest = h.until(is_run_error).await;
    assert_eq!(
        types(&rest),
        ["InterruptResolved", "RunStarted", "ToolCallEnd", "RunError"]
    );
    let Event::InterruptResolved(resolved) = &rest[0].event else {
        panic!("InterruptResolved first");
    };
    assert_eq!(resolved.interrupt_id, interrupt);
    assert_eq!(resolved.outcome, ResolvedOutcome::Expired);
    let Event::RunError(error) = &rest[rest.len() - 1].event else {
        panic!("RunError last");
    };
    assert_eq!(error.code.as_deref(), Some("expired"));

    // A late approval finds nothing.
    h.feed.send(Command::Approval {
        thread_id: "th_1".into(),
        task_id: "tk_1".into(),
        interrupt_id: interrupt,
        outcome: Outcome::Cancelled,
        resolved_by: user(),
    });
    assert_eq!(h.failure().await.code, CommandErrorCode::UnknownInterrupt);
    let log = h.finish().await;
    assert_valid_log(&log);
}

#[tokio::test]
async fn a_known_thread_resumes_its_session_after_a_restart() {
    // The same state directory serves both bridges, as it does across a runner restart.
    let state = TempDir::new().expect("state dir");
    let work = TempDir::new().expect("work dir");

    let first = json!({ "turns": [{ "steps": [{ "text": "one" }] }] });
    let (client_io, agent_io) = tokio::io::duplex(1 << 16);
    let (agent_r, agent_w) = tokio::io::split(agent_io);
    let script = acp_mock::Script::from_value(&first).expect("script");
    let agent = tokio::spawn(acp_mock::run(script, agent_r, agent_w));
    let (client_r, client_w) = tokio::io::split(client_io);
    let mut h = Harness::with_state(client_r, client_w, state.path(), work.path(), |_| {});
    h.prompt("th_1", "tk_1", "first");
    h.until(is_success).await;
    let first_log = h.finish().await;
    assert_valid_log(&first_log);
    agent.abort();

    // A new agent process knows no session. Its script has a single turn, which only the
    // session that was resumed can run as its first prompt; a fresh session/new would also
    // do, so the store is asked what happened.
    let second = json!({ "turns": [{ "steps": [{ "text": "two" }] }] });
    let (client_io, agent_io) = tokio::io::duplex(1 << 16);
    let (agent_r, agent_w) = tokio::io::split(agent_io);
    let script = acp_mock::Script::from_value(&second).expect("script");
    tokio::spawn(acp_mock::run(script, agent_r, agent_w));
    let (client_r, client_w) = tokio::io::split(client_io);
    let mut h = Harness::with_state(client_r, client_w, state.path(), work.path(), |_| {});
    h.prompt("th_1", "tk_2", "second");
    let stream = h.until(is_success).await;
    assert!(
        stream
            .iter()
            .any(|e| matches!(&e.event, Event::TextMessageContent(c) if c.delta == "two")),
        "the resumed session answered: {stream:#?}"
    );
    let log = h.finish().await;
    assert_valid_log(&log);

    let store =
        Store::open_with_clock(state.path(), Arc::new(ManualClock::new(T0))).expect("store");
    let session = store
        .session_for_thread("th_1")
        .expect("read")
        .expect("the thread has a session");
    assert_eq!(session.session_id, "sess_mock_1", "same session, resumed");
    assert_eq!(
        store.task("tk_2").expect("read").expect("task").state,
        "completed"
    );
}
