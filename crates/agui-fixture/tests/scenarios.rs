//! Acceptance tests for the fixture (M1B-002): the four canned scenarios over the real HTTP and
//! WebSocket surface, checked against the TurnEvent JSON schemas (`protocol` schemas dir) and
//! the ordering rules of `docs/specs/turn-event.md` §5.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use agui_fixture::{FixtureConfig, RunningFixture, Scenario, complete_log, start};
use futures_util::{SinkExt, StreamExt};
use jsonschema::{Retrieve, Uri};
use protocol::turn_event::MAX_ENVELOPE_BYTES;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const WAIT: Duration = Duration::from_secs(60);
const SCHEMA_BASE: &str = "https://sprite-machine.dev/schemas/turn-event/v1/";
const TRANSCRIPT_MIN_EVENTS: usize = 10_000;
const DIFF_LINES: usize = 5_000;
const TERMINAL_CHUNK_LIMIT: usize = 1024;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

// ---- helpers: logs, schema, server ---------------------------------------------------------

fn schema_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../protocol/schemas/turn-event")
}

fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).expect("schema file is readable");
    serde_json::from_str(&text).expect("schema file is JSON")
}

/// Resolves `SCHEMA_BASE/<path>` to the file under the schema directory.
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
        let text = std::fs::read_to_string(self.dir.join(relative))?;
        Ok(serde_json::from_str(&text)?)
    }
}

fn envelope_validator() -> jsonschema::Validator {
    let dir = schema_dir();
    let root = read_json(&dir.join("envelope.schema.json"));
    jsonschema::options()
        .with_retriever(SchemaFiles { dir })
        .build(&root)
        .expect("envelope schema compiles")
}

fn schema_errors(validator: &jsonschema::Validator, instance: &Value) -> Vec<String> {
    validator
        .iter_errors(instance)
        .map(|error| error.to_string())
        .collect()
}

/// The stored log of `scenario` as wire JSON, with `seq` 1..=N.
fn wire_log(scenario: Scenario) -> Vec<Value> {
    complete_log(scenario, "th_fixture")
        .expect("script builds")
        .iter()
        .map(|envelope| serde_json::to_value(envelope).expect("envelope serializes"))
        .collect()
}

async fn start_fixture(scenario: Scenario) -> RunningFixture {
    start(FixtureConfig::new(scenario))
        .await
        .expect("fixture starts")
}

async fn subscribe(fixture: &RunningFixture, after: u64) -> Socket {
    let (socket, _) = connect_async(fixture.ws_url(after))
        .await
        .expect("websocket upgrade succeeds");
    socket
}

async fn read_one(socket: &mut Socket) -> Value {
    let frame = timeout(WAIT, socket.next())
        .await
        .expect("no frame within the wait")
        .expect("socket open")
        .expect("frame is valid");
    match frame {
        Message::Text(body) => serde_json::from_str(body.as_str()).expect("text frame is JSON"),
        other => panic!("unexpected frame {other:?}"),
    }
}

/// Reads messages until one satisfies `done` (that message is included).
async fn read_until(socket: &mut Socket, done: impl Fn(&Value) -> bool) -> Vec<Value> {
    let mut messages = Vec::new();
    loop {
        let message = read_one(socket).await;
        let stop = done(&message);
        messages.push(message);
        if stop {
            return messages;
        }
    }
}

/// The envelope of an `event` message, if `message` is one.
fn envelope(message: &Value) -> Option<&Value> {
    if message["type"] == "event" {
        message.get("envelope")
    } else {
        None
    }
}

fn envelope_seq(message: &Value) -> Option<u64> {
    envelope(message).and_then(|e| e["seq"].as_u64())
}

/// The envelopes among `messages`, in order.
fn envelopes(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|message| envelope(message).cloned())
        .collect()
}

fn remove_first(log: &mut Vec<Value>, event_type: &str) {
    let index = log
        .iter()
        .position(|e| e["event"]["type"] == event_type)
        .expect("event is in the log");
    log.remove(index);
}

fn renumber(log: &mut [Value]) {
    for (index, envelope) in log.iter_mut().enumerate() {
        envelope["seq"] = json!(index + 1);
    }
}

// ---- §5 ordering checker (over wire JSON, independent of the fixture's own code) -----------

#[derive(Default)]
struct OpenRun {
    id: String,
    pending: BTreeSet<String>,
    open_messages: BTreeSet<String>,
    seen_messages: BTreeSet<String>,
}

/// A run that ended `RunFinished{interrupt}` and waits for its answers.
struct Paused {
    run_id: String,
    unresolved: BTreeSet<String>,
}

#[derive(Default)]
struct Checker {
    last_seq: u64,
    scopes: BTreeSet<String>,
    activities: BTreeSet<String>,
    task: Option<String>,
    finished: BTreeSet<String>,
    runs_in_task: usize,
    tool_calls: BTreeMap<String, bool>,
    paused: Option<Paused>,
    run: Option<OpenRun>,
}

fn str_of(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_owned()
}

impl Checker {
    fn push(&mut self, envelope: &Value) -> Result<(), String> {
        let seq = envelope["seq"].as_u64().ok_or("envelope without seq")?;
        let ty = envelope["event"]["type"].as_str().unwrap_or_default();
        let fail = |message: &str| format!("seq {seq} ({ty}): {message}");
        if seq != self.last_seq + 1 {
            return Err(format!(
                "seq {seq} ({ty}): expected seq {} after {}",
                self.last_seq + 1,
                self.last_seq
            ));
        }
        self.last_seq = seq;

        let event = &envelope["event"];
        let task = envelope["task_id"].as_str();
        let run = envelope["run_id"].as_str();

        // Thread-scoped events may appear anywhere (§5.9).
        match ty {
            "Presence" => return Ok(()),
            "StateSnapshot" => {
                self.scopes.insert(str_of(event, "scope"));
                return Ok(());
            }
            "StateDelta" => {
                if !self.scopes.contains(&str_of(event, "scope")) {
                    return Err(fail("delta before the snapshot of its scope (§5.8)"));
                }
                return Ok(());
            }
            _ => {}
        }

        // Task shape (§5.1, §5.2).
        if ty == "UserMessage" {
            let Some(task) = task else {
                return Err(fail("UserMessage without task_id"));
            };
            if self.task.is_some() {
                return Err(fail("a task is already running (§5.2)"));
            }
            if self.finished.contains(task) {
                return Err(fail("task id reused"));
            }
            self.task = Some(task.to_owned());
            self.runs_in_task = 0;
            self.paused = None;
            self.tool_calls.clear();
            return Ok(());
        }
        let Some(open_task) = self.task.clone() else {
            return Err(fail("event outside any task"));
        };
        if task != Some(open_task.as_str()) {
            return Err(fail("task_id is not the running task"));
        }

        if ty == "RunStarted" {
            if self.run.is_some() {
                return Err(fail("a run is already open (§5.2)"));
            }
            match str_of(event, "reason").as_str() {
                "prompt" => {
                    if self.runs_in_task > 0 {
                        return Err(fail("prompt run after an earlier run of the task"));
                    }
                }
                "resume" => {
                    let paused = self
                        .paused
                        .take()
                        .ok_or_else(|| fail("resume without a paused run (§5.6)"))?;
                    if !paused.unresolved.is_empty() {
                        return Err(fail(&format!(
                            "resume before InterruptResolved for {:?} (§5.6)",
                            paused.unresolved
                        )));
                    }
                    if event["parent_run_id"].as_str() != Some(paused.run_id.as_str()) {
                        return Err(fail("resume parent_run_id is not the paused run"));
                    }
                }
                other => return Err(fail(&format!("unknown RunStarted reason {other}"))),
            }
            let Some(run_id) = run else {
                return Err(fail("RunStarted without run_id"));
            };
            self.runs_in_task += 1;
            self.run = Some(OpenRun {
                id: run_id.to_owned(),
                ..OpenRun::default()
            });
            return Ok(());
        }

        if ty == "InterruptResolved" {
            if run.is_some() {
                return Err(fail("InterruptResolved must be task-scoped (§4.5)"));
            }
            let id = str_of(event, "interrupt_id");
            let mut found = false;
            if let Some(open_run) = self.run.as_mut() {
                found |= open_run.pending.remove(&id);
            }
            if let Some(paused) = self.paused.as_mut() {
                found |= paused.unresolved.remove(&id);
            }
            if !found {
                return Err(fail("InterruptResolved for an interrupt that is not open"));
            }
            return Ok(());
        }

        if matches!(ty, "RunFinished" | "RunError") {
            let open_run = self
                .run
                .take()
                .ok_or_else(|| fail("terminal event with no open run"))?;
            if run != Some(open_run.id.as_str()) {
                return Err(fail("run_id is not the open run"));
            }
            // Messages close before the run's terminal event (§5.4).
            if !open_run.open_messages.is_empty() {
                return Err(fail("a message is still open at the run's end (§5.4)"));
            }
            let open_tool_call = self.tool_calls.values().any(|open| *open);
            if ty == "RunError" {
                // §5.5: the producer closes every open tool call before RunError.
                if open_tool_call {
                    return Err(fail("tool call still open at RunError (§5.5)"));
                }
                self.finish_task(&open_task);
                return Ok(());
            }
            match event["outcome"]["type"].as_str() {
                Some("success") => {
                    if !open_run.pending.is_empty() {
                        return Err(fail("success with open interrupts (§5.6)"));
                    }
                    if open_tool_call {
                        return Err(fail("tool call still open at the task's end (§5.5)"));
                    }
                    self.finish_task(&open_task);
                }
                Some("interrupt") => {
                    let ids: BTreeSet<String> = event["outcome"]["interrupt_ids"]
                        .as_array()
                        .map(|ids| {
                            ids.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default();
                    if ids.is_empty() {
                        return Err(fail("interrupt outcome without interrupt_ids"));
                    }
                    if !open_run.pending.is_subset(&ids) {
                        return Err(fail("interrupt_ids misses an open interrupt (§5.6)"));
                    }
                    self.paused = Some(Paused {
                        run_id: open_run.id,
                        unresolved: ids,
                    });
                }
                other => return Err(fail(&format!("unknown run outcome {other:?}"))),
            }
            return Ok(());
        }

        // Run-scoped events need the open run of the task (§5.3).
        let Some(current) = self.run.as_mut() else {
            return Err(fail("run-scoped event with no open run"));
        };
        if run != Some(current.id.as_str()) {
            return Err(fail("run_id is not the open run (§5.3)"));
        }
        match ty {
            "TextMessageStart" | "ReasoningMessageStart" => {
                let id = str_of(event, "message_id");
                if !current.seen_messages.insert(id.clone()) {
                    return Err(fail("message started twice (§5.4)"));
                }
                current.open_messages.insert(id);
            }
            "TextMessageContent" | "ReasoningMessageContent" => {
                if !current.open_messages.contains(&str_of(event, "message_id")) {
                    return Err(fail("content for a message that is not open (§5.4)"));
                }
            }
            "TextMessageEnd" | "ReasoningMessageEnd" => {
                if !current.open_messages.remove(&str_of(event, "message_id")) {
                    return Err(fail("end for a message that is not open (§5.4)"));
                }
            }
            "ToolCallStart" => {
                let id = str_of(event, "tool_call_id");
                match self.tool_calls.get(&id) {
                    Some(false) => return Err(fail("ToolCallStart after ToolCallEnd (§5.5)")),
                    Some(true) => {}
                    None => {
                        self.tool_calls.insert(id, true);
                    }
                }
            }
            "ToolCallArgs" | "ToolCallResult" => {
                if self.tool_calls.get(&str_of(event, "tool_call_id")) != Some(&true) {
                    return Err(fail("tool call update while it is not open (§5.5)"));
                }
            }
            "ToolCallEnd" => match self.tool_calls.get_mut(&str_of(event, "tool_call_id")) {
                Some(open) if *open => *open = false,
                _ => return Err(fail("ToolCallEnd for a tool call that is not open (§5.5)")),
            },
            "Activity" => {
                let id = str_of(event, "activity_id");
                if event["mode"].as_str() == Some("snapshot") {
                    self.activities.insert(id);
                } else if !self.activities.contains(&id) {
                    return Err(fail("append or patch before the snapshot (§5.8)"));
                }
            }
            "Interrupt" if !current.pending.insert(str_of(event, "interrupt_id")) => {
                return Err(fail("interrupt raised twice"));
            }
            // Checkpoints and other run-scoped events need no further state.
            _ => {}
        }
        Ok(())
    }

    fn finish_task(&mut self, task: &str) {
        self.finished.insert(task.to_owned());
        self.task = None;
        self.paused = None;
        self.tool_calls.clear();
    }

    fn finish(&self) -> Result<(), String> {
        match &self.task {
            Some(task) => Err(format!("log ends inside task {task}")),
            None => Ok(()),
        }
    }
}

/// `Ok` when `log` (wire JSON, in seq order) satisfies §3 and §5 and ends at a task boundary.
fn check_log(log: &[Value]) -> Result<(), String> {
    let mut checker = Checker::default();
    for envelope in log {
        checker.push(envelope)?;
    }
    checker.finish()
}

// ---- tests ---------------------------------------------------------------------------------

#[test]
fn schema_validates_every_emitted_event() {
    let validator = envelope_validator();
    let mut checked = 0;
    for scenario in Scenario::ALL {
        for envelope in wire_log(scenario) {
            let errors = schema_errors(&validator, &envelope);
            assert!(
                errors.is_empty(),
                "{scenario} seq {}: {errors:?}",
                envelope["seq"]
            );
            checked += 1;
        }
    }
    assert!(checked > TRANSCRIPT_MIN_EVENTS, "checked {checked} events");

    // The shipped examples validate, and the schema rejects what it must.
    let mut examples = 0;
    for entry in std::fs::read_dir(schema_dir().join("examples")).expect("examples dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let errors = schema_errors(&validator, &read_json(&path));
        assert!(errors.is_empty(), "{}: {errors:?}", path.display());
        examples += 1;
    }
    assert!(examples >= 27, "found {examples} examples");

    let mut unknown_type = read_json(&schema_dir().join("examples/RunStarted.json"));
    unknown_type["event"]["type"] = json!("Nope");
    assert!(!schema_errors(&validator, &unknown_type).is_empty());
    let mut bad_version = read_json(&schema_dir().join("examples/RunStarted.json"));
    bad_version["v"] = json!(2);
    assert!(!schema_errors(&validator, &bad_version).is_empty());
}

#[test]
fn ordering_checker_accepts_every_scenario_and_rejects_violations() {
    for scenario in Scenario::ALL {
        let log = wire_log(scenario);
        check_log(&log).unwrap_or_else(|error| panic!("{scenario}: {error}"));
    }

    // A gap in seq.
    let mut gap = wire_log(Scenario::Approval);
    gap.remove(10);
    let error = check_log(&gap).expect_err("gap is rejected");
    assert!(error.contains("expected seq"), "{error}");

    // A message left open at the run's RunFinished.
    let mut open_message = wire_log(Scenario::Approval);
    remove_first(&mut open_message, "TextMessageEnd");
    renumber(&mut open_message);
    assert!(check_log(&open_message).is_err());

    // A resume run without InterruptResolved for its interrupt.
    let mut unresolved = wire_log(Scenario::Approval);
    remove_first(&mut unresolved, "InterruptResolved");
    renumber(&mut unresolved);
    let error = check_log(&unresolved).expect_err("unresolved interrupt is rejected");
    assert!(error.contains("resume"), "{error}");

    // A tool call that gets an update after its ToolCallEnd.
    let mut reopened = wire_log(Scenario::Diff5k);
    let end = reopened
        .iter()
        .position(|e| e["event"]["type"] == "ToolCallEnd")
        .expect("diff scenario ends a tool call");
    let mut late = reopened[end].clone();
    late["event"] =
        json!({"type": "ToolCallArgs", "tool_call_id": "tc_00001_write", "raw_input": {}});
    reopened.insert(end + 1, late);
    renumber(&mut reopened);
    assert!(check_log(&reopened).is_err());
}

#[tokio::test]
async fn transcript_10k_streams_gap_free_over_websocket() {
    let scenario = Scenario::Transcript10k;
    let expected = wire_log(scenario);
    assert!(
        expected.len() >= TRANSCRIPT_MIN_EVENTS,
        "{} events",
        expected.len()
    );
    let last = expected.len() as u64;

    let fixture = start_fixture(scenario).await;
    let mut socket = subscribe(&fixture, 0).await;
    let mut messages = read_until(&mut socket, |m| envelope_seq(m) == Some(last)).await;
    // `live` follows the catch-up, so it can arrive just after the last event.
    if !messages.iter().any(|m| m["type"] == "live") {
        messages.extend(read_until(&mut socket, |m| m["type"] == "live").await);
    }

    assert_eq!(messages[0]["type"], "hello");
    assert_eq!(messages[0]["replay_from"], 1);
    assert_eq!(messages[1]["type"], "snapshot");
    assert_eq!(messages[1]["at_seq"], 0);
    assert_eq!(messages.iter().filter(|m| m["type"] == "live").count(), 1);

    let received = envelopes(&messages);
    let validator = envelope_validator();
    for envelope in &received {
        let errors = schema_errors(&validator, envelope);
        assert!(errors.is_empty(), "seq {}: {errors:?}", envelope["seq"]);
    }
    let seqs: Vec<u64> = received
        .iter()
        .map(|e| e["seq"].as_u64().unwrap_or(0))
        .collect();
    assert_eq!(
        seqs,
        (1..=last).collect::<Vec<_>>(),
        "seq is gap-free and in order"
    );
    assert!(received == expected, "wire log equals the scripted log");
    check_log(&received).expect("received log satisfies section 5");

    // Keep-alive: a ping is answered with a pong on the same socket.
    socket
        .send(Message::Text(r#"{"type":"ping"}"#.into()))
        .await
        .expect("ping sends");
    let pong = read_one(&mut socket).await;
    assert_eq!(pong["type"], "pong");
}

#[test]
fn diff_5k_chunks_reassemble_within_envelope_limit() {
    let log = wire_log(Scenario::Diff5k);
    let mut chunks = Vec::new();
    for envelope in &log {
        let size = serde_json::to_string(envelope).expect("serializes").len();
        assert!(
            size <= MAX_ENVELOPE_BYTES,
            "seq {} is {size} bytes",
            envelope["seq"]
        );
        let event = &envelope["event"];
        if event["type"] == "Activity" && event["activity_type"] == "diff" {
            chunks.push(event.clone());
        }
    }
    assert!(
        chunks.len() > 1,
        "the diff is chunked, got {} activity events",
        chunks.len()
    );

    let mut text = String::new();
    for (index, activity) in chunks.iter().enumerate() {
        let content = &activity["content"];
        let chunk = &content["chunk"];
        assert_eq!(chunk["index"], index as u64, "chunk indexes are sequential");
        assert_eq!(
            chunk["last"],
            index == chunks.len() - 1,
            "only the final chunk is last"
        );
        let expected_mode = if index == 0 { "snapshot" } else { "append" };
        assert_eq!(activity["mode"], expected_mode);
        assert_eq!(activity["activity_id"], "tc_00001_write:src/generated.rs");
        text.push_str(content["new_text"].as_str().expect("new_text"));
    }

    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), DIFF_LINES);
    assert!(lines[0].starts_with("// generated line 0001:"));
    assert!(lines[DIFF_LINES - 1].starts_with(&format!("// generated line {DIFF_LINES:04}:")));
}

#[test]
fn terminal_appends_then_closes_with_exit() {
    let log = wire_log(Scenario::Terminal);
    let terminal: Vec<&Value> = log
        .iter()
        .map(|e| &e["event"])
        .filter(|e| e["type"] == "Activity" && e["activity_type"] == "terminal")
        .collect();
    assert!(terminal.len() > 2, "{} terminal events", terminal.len());

    let opening = &terminal[0]["content"];
    assert_eq!(terminal[0]["mode"], "snapshot");
    assert_eq!(opening["command"], "cargo test -p agui-fixture");
    assert_eq!(opening["cwd"], "/workspace/agui-fixture");

    let mut streamed = String::new();
    for append in &terminal[1..terminal.len() - 1] {
        assert_eq!(append["mode"], "append");
        let piece = append["content"]["output"]
            .as_str()
            .expect("append carries output");
        assert!(
            piece.len() <= TERMINAL_CHUNK_LIMIT,
            "chunk of {} bytes",
            piece.len()
        );
        assert!(append["content"]["exit"].is_null());
        streamed.push_str(piece);
    }

    let closing = terminal[terminal.len() - 1];
    assert_eq!(closing["mode"], "snapshot");
    assert_eq!(closing["content"]["exit"]["exit_code"], 0);
    assert_eq!(
        closing["content"]["output"].as_str(),
        Some(streamed.as_str()),
        "closing snapshot carries the whole output the appends streamed"
    );

    let finished = log
        .iter()
        .rev()
        .find(|e| e["event"]["type"] == "RunFinished")
        .expect("run finishes");
    assert_eq!(finished["event"]["outcome"]["type"], "success");
}

#[tokio::test]
async fn approval_pauses_then_resumes_and_second_answer_gets_409() {
    let scenario = Scenario::Approval;
    let expected = wire_log(scenario);
    let last = expected.len() as u64;
    let fixture = start_fixture(scenario).await;
    let mut socket = subscribe(&fixture, 0).await;

    // The run pauses on the interrupt; nothing after it arrives until it is answered.
    let paused = read_until(&mut socket, |m| {
        envelope(m).is_some_and(|e| e["event"]["type"] == "RunFinished")
    })
    .await;
    let finished = envelopes(&paused).pop().expect("run finished");
    assert_eq!(finished["event"]["outcome"]["type"], "interrupt");
    assert_eq!(
        finished["event"]["outcome"]["interrupt_ids"],
        json!(["int_00001"])
    );

    let url = fixture.http_url(&format!(
        "/threads/{}/interrupts/int_00001",
        fixture.thread_id()
    ));
    let client = reqwest::Client::new();
    let alice = json!({"kind": "user", "id": "usr_alice", "name": "Alice", "device_id": "dev_a"});
    let answer = |option: &str, actor: &Value| {
        json!({"outcome": {"type": "selected", "option_id": option}, "resolved_by": actor})
            .to_string()
    };

    // First answer wins.
    let first = client
        .post(&url)
        .header("content-type", "application/json")
        .header("idempotency-key", "k-1")
        .body(answer("allow", &alice))
        .send()
        .await
        .expect("first answer sends");
    assert_eq!(first.status().as_u16(), 200);
    let first: Value = serde_json::from_str(&first.text().await.expect("body")).expect("JSON");
    assert_eq!(first["resolved_by"]["id"], "usr_alice");

    // The same Idempotency-Key replays the same answer.
    let replay = client
        .post(&url)
        .header("content-type", "application/json")
        .header("idempotency-key", "k-1")
        .body(answer("allow", &alice))
        .send()
        .await
        .expect("replay sends");
    assert_eq!(replay.status().as_u16(), 200);

    // Any other answer is refused and names who resolved it.
    let bob = json!({"kind": "user", "id": "usr_bob", "name": "Bob", "device_id": "dev_b"});
    for (key, option, actor) in [(None, "reject", &bob), (Some("k-2"), "allow", &bob)] {
        let mut request = client
            .post(&url)
            .header("content-type", "application/json")
            .body(answer(option, actor));
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        let conflict = request.send().await.expect("conflicting answer sends");
        assert_eq!(conflict.status().as_u16(), 409);
        let body: Value =
            serde_json::from_str(&conflict.text().await.expect("body")).expect("JSON");
        assert_eq!(body["error"], "already_resolved");
        assert_eq!(body["interrupt_id"], "int_00001");
        assert_eq!(body["resolved_by"]["id"], "usr_alice");
    }

    // The resume run follows, and the whole log matches the script with Alice as the resolver.
    let rest = read_until(&mut socket, |m| envelope_seq(m) == Some(last)).await;
    let mut received = envelopes(&paused);
    received.extend(envelopes(&rest));
    let mut expected = expected;
    let resolution = expected
        .iter_mut()
        .find(|e| e["event"]["type"] == "InterruptResolved")
        .expect("script resolves the interrupt");
    resolution["event"]["resolved_by"] = alice.clone();
    assert!(received == expected, "resumed log equals the script");
    check_log(&received).expect("received log satisfies section 5");
}

#[tokio::test]
async fn mid_run_reconnect_replays_from_run_boundary_without_gaps() {
    let scenario = Scenario::Transcript10k;
    let expected = wire_log(scenario);
    let last = expected.len() as u64;

    // Pick a cut inside a run, at least 5000 events in, with the run started a few events back.
    let mut open: Option<(String, u64)> = None;
    let mut cut = None;
    for (index, envelope) in expected.iter().enumerate() {
        let seq = index as u64 + 1;
        let event_type = envelope["event"]["type"].as_str().unwrap_or_default();
        match (event_type, envelope["run_id"].as_str()) {
            ("RunStarted", Some(run)) => open = Some((run.to_owned(), seq)),
            ("RunFinished" | "RunError", Some(run))
                if open.as_ref().is_some_and(|(open_run, _)| open_run == run) =>
            {
                open = None;
            }
            _ => {}
        }
        if let Some((_, started)) = open
            .as_ref()
            .filter(|(_, started)| seq >= 5_000 && started + 2 <= seq)
        {
            cut = Some((seq, *started));
            break;
        }
    }
    let (after, boundary) = cut.expect("transcript has a cut inside a run");

    let fixture = start_fixture(scenario).await;
    // First connection: everything up to the cut, then drop.
    let mut first = subscribe(&fixture, 0).await;
    let first_messages = read_until(&mut first, |m| envelope_seq(m) == Some(after)).await;
    drop(first);
    let first_seqs: Vec<u64> = envelopes(&first_messages)
        .iter()
        .map(|e| e["seq"].as_u64().unwrap_or(0))
        .collect();
    assert_eq!(first_seqs, (1..=after).collect::<Vec<_>>());

    // Reconnect mid-run: the replay starts at the run boundary, not at after + 1.
    let mut second = subscribe(&fixture, after).await;
    let second_messages = read_until(&mut second, |m| envelope_seq(m) == Some(last)).await;
    assert_eq!(second_messages[0]["type"], "hello");
    assert_eq!(
        second_messages[0]["replay_from"], boundary,
        "replay starts at the run boundary"
    );
    assert!(second_messages[0]["head_seq"].as_u64().unwrap_or(0) >= after);
    assert_eq!(second_messages[1]["type"], "snapshot");
    assert_eq!(second_messages[1]["at_seq"], boundary - 1);

    let second_seqs: Vec<u64> = envelopes(&second_messages)
        .iter()
        .map(|e| e["seq"].as_u64().unwrap_or(0))
        .collect();
    assert_eq!(
        second_seqs,
        (boundary..=last).collect::<Vec<_>>(),
        "replay then live, gap-free"
    );

    // Together the two connections cover 1..=N; the only duplicates are the replayed run.
    let mut seen: BTreeMap<u64, usize> = BTreeMap::new();
    for seq in first_seqs.iter().chain(second_seqs.iter()) {
        *seen.entry(*seq).or_default() += 1;
    }
    assert_eq!(
        seen.keys().copied().collect::<Vec<_>>(),
        (1..=last).collect::<Vec<_>>()
    );
    let duplicates: Vec<u64> = seen
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(seq, _)| *seq)
        .collect();
    assert_eq!(duplicates, (boundary..=after).collect::<Vec<_>>());

    // Every replayed envelope is the stored one.
    for envelope in envelopes(&second_messages) {
        let seq = envelope["seq"].as_u64().unwrap_or(0) as usize;
        assert!(
            envelope == expected[seq - 1],
            "seq {seq} differs from the stored event"
        );
    }
}
