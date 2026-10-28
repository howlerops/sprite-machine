//! Golden tests for `protocol::mapping::acp_turn` (M1A-006a): ACP JSON in → TurnEvent JSON out.
//!
//! Each case is a directory `tests/golden/acp_turn/<case>/` with
//! - `input.json`: `{thread_id, steps: [...]}`, the calls a runner makes on the mapper (see
//!   [`execute`] for the step vocabulary), with ACP payloads exactly as the agent sends them;
//! - `expected.json`: the envelopes the mapper returns, stamped `seq` 1..N by [`ThreadSeq`].
//!
//! `UPDATE_GOLDEN=1 cargo test -p protocol --test golden_acp_turn` rewrites every
//! `expected.json`; review the diff before committing.
//!
//! Every case must also validate against the JSON schemas in `schemas/turn-event`, pass
//! `validate_complete` (turn-event.md §5), round-trip through the serde types, and keep the
//! verbatim ACP payload in `raw`.

use std::fs;
use std::path::{Path, PathBuf};

use jsonschema::{Retrieve, Uri};
use protocol::acp;
use protocol::mapping::acp_turn::{
    AcpTurnMapper, LocalPrompt, Rule, SequenceError, StartTask, runner_actor, validate_complete,
    validate_sequence,
};
use protocol::turn_event::{
    Actor, ActorKind, ContentPart, Envelope, Event, MAX_ENVELOPE_BYTES, ResolvedOutcome, ThreadSeq,
    format_timestamp,
};
use serde_json::{Value, json};

/// 2026-10-27T22:00:00.000Z; steps without a `ts` get this plus one second per step.
const BASE_MS: u64 = 1_793_138_400_000;
const SCHEMA_BASE: &str = "https://sprite-machine.dev/schemas/turn-event/v1/";

// ---- the step runner ----------------------------------------------------------------------

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/acp_turn")
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn actor(value: &Value) -> Actor {
    serde_json::from_value(value.clone()).expect("actor")
}

/// One step's output and the step itself.
struct Executed {
    step: Value,
    produced: Vec<Envelope>,
}

/// Runs the steps of `input` against a fresh mapper and stamps the result with one
/// [`ThreadSeq`]. Steps (`op`):
///
/// - `start_task`: `task_id`, optional `user_message: {text, author?}`, `agent_id`, `machine_id`
/// - `session_new`: `response` (ACP `NewSessionResponse`)
/// - `session_update`: `notification` (ACP `SessionNotification`, as JSON)
/// - `request_permission`, `elicitation`: `interrupt_id`, `expires_at`, `request`
/// - `hub_event`: `task_id`, `event`: an envelope the hub emits (UserMessage, InterruptResolved)
/// - `note_interrupt_resolved`: `interrupt_id` (the hub resolved it)
/// - `interrupt_resolved`: `interrupt_id`, `outcome`, `resolved_by` (the runner resolves it)
/// - `resume`, `prompt_response` (`stop_reason`), `cancel` (`by?`), `expire` (`interrupt_id`),
///   `error` (`code`, `message`)
/// - `terminal_started` (`terminal_id`, `tool_call_id?`, `command?`, `cwd?`), `terminal_output`
///   (`terminal_id`, `output`), `terminal_exit` (`terminal_id`, `exit_code?`, `signal?`)
///
/// Every step may carry `ts`.
fn execute(input: &Value) -> Vec<Executed> {
    let thread_id = input["thread_id"].as_str().expect("thread_id");
    let mut mapper = AcpTurnMapper::with_sequential_ids(thread_id);
    let mut seq = ThreadSeq::new();
    let mut executed = Vec::new();
    for (index, step) in input["steps"].as_array().expect("steps").iter().enumerate() {
        let ts = step["ts"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format_timestamp(BASE_MS + 1000 * (index as u64 + 1)));
        let ts = ts.as_str();
        let text = |key: &str| step[key].as_str().expect(key);
        let optional = |key: &str| step[key].as_str();
        let produced = match step["op"].as_str().expect("op") {
            "start_task" => {
                let mut start = StartTask::new(text("task_id"));
                start.agent_id = optional("agent_id").map(str::to_owned);
                start.machine_id = optional("machine_id").map(str::to_owned);
                if let Some(message) = step.get("user_message") {
                    start.user_message = Some(LocalPrompt {
                        parts: vec![ContentPart::Text {
                            text: message["text"].as_str().expect("text").to_owned(),
                        }],
                        author: message.get("author").map_or_else(
                            || Actor {
                                kind: ActorKind::User,
                                id: "usr_local".to_owned(),
                                name: None,
                                device_id: None,
                            },
                            actor,
                        ),
                    });
                }
                mapper.start_task(start, ts)
            }
            "session_new" => {
                let response: acp::NewSessionResponse =
                    serde_json::from_value(step["response"].clone()).expect("NewSessionResponse");
                mapper.on_session_new(&response, ts)
            }
            "session_update" => mapper
                .on_session_update_json(&step["notification"], ts)
                .expect("notification"),
            "request_permission" => {
                let request: acp::RequestPermissionRequest =
                    serde_json::from_value(step["request"].clone()).expect("permission request");
                mapper.on_request_permission(&request, text("interrupt_id"), text("expires_at"), ts)
            }
            "elicitation" => {
                let request: acp::CreateElicitationRequest =
                    serde_json::from_value(step["request"].clone()).expect("elicitation request");
                mapper.on_elicitation(&request, text("interrupt_id"), text("expires_at"), ts)
            }
            "hub_event" => vec![Envelope::new(
                thread_id,
                Some(text("task_id").to_owned()),
                None,
                ts,
                serde_json::from_value::<Event>(step["event"].clone()).expect("hub event"),
            )],
            "note_interrupt_resolved" => {
                assert!(mapper.note_interrupt_resolved(text("interrupt_id")));
                Vec::new()
            }
            "interrupt_resolved" => mapper.on_interrupt_resolved(
                text("interrupt_id"),
                serde_json::from_value::<ResolvedOutcome>(step["outcome"].clone())
                    .expect("outcome"),
                actor(&step["resolved_by"]),
                ts,
            ),
            "resume" => mapper.resume(ts),
            "prompt_response" => {
                let stop_reason: acp::StopReason =
                    serde_json::from_value(step["stop_reason"].clone()).expect("stop_reason");
                mapper.on_prompt_response(stop_reason, ts)
            }
            "cancel" => {
                let by = step.get("by").map_or_else(
                    || Actor {
                        kind: ActorKind::User,
                        id: "usr_1".to_owned(),
                        name: None,
                        device_id: None,
                    },
                    actor,
                );
                mapper.on_cancel(&by, ts)
            }
            "expire" => mapper.on_interrupt_expired(text("interrupt_id"), ts),
            "error" => mapper.on_error(text("code"), text("message"), ts),
            "terminal_started" => mapper.on_terminal_started(
                text("terminal_id"),
                optional("tool_call_id"),
                optional("command"),
                optional("cwd"),
                ts,
            ),
            "terminal_output" => mapper.on_terminal_output(text("terminal_id"), text("output"), ts),
            "terminal_exit" => mapper.on_terminal_exit(
                text("terminal_id"),
                step["exit_code"].as_i64(),
                optional("signal"),
                ts,
            ),
            other => panic!("unknown step op {other}"),
        };
        let produced = produced
            .into_iter()
            .map(|envelope| seq.stamp(envelope).expect("producer envelopes have seq 0"))
            .collect();
        executed.push(Executed {
            step: step.clone(),
            produced,
        });
    }
    executed
}

fn envelopes(executed: &[Executed]) -> Vec<Envelope> {
    executed
        .iter()
        .flat_map(|step| step.produced.iter().cloned())
        .collect()
}

// ---- schema -------------------------------------------------------------------------------

/// Resolves `SCHEMA_BASE/<path>` to the file under `schemas/turn-event`.
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

fn envelope_validator() -> jsonschema::Validator {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas/turn-event");
    let root = read_json(&dir.join("envelope.schema.json"));
    jsonschema::options()
        .with_retriever(SchemaFiles { dir })
        .build(&root)
        .expect("envelope schema compiles")
}

fn assert_schema_valid(validator: &jsonschema::Validator, what: &str, envelope: &Value) {
    let errors: Vec<String> = validator
        .iter_errors(envelope)
        .map(|error| error.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "{what}: {envelope}\nviolates the envelope schema: {errors:?}"
    );
}

// ---- the golden check ---------------------------------------------------------------------

/// Everything every emitted log must satisfy, golden or not.
fn assert_well_formed(what: &str, log: &[Envelope]) -> Vec<Value> {
    let validator = envelope_validator();
    let wire: Vec<Value> = log
        .iter()
        .map(|envelope| serde_json::to_value(envelope).expect("serializes"))
        .collect();
    for (envelope, value) in log.iter().zip(&wire) {
        assert_schema_valid(&validator, what, value);
        let back: Envelope = serde_json::from_value(value.clone()).expect("round trips");
        assert_eq!(&back, envelope, "{what}: serde round trip");
        let size = serde_json::to_vec(value).expect("bytes").len();
        assert!(
            size <= MAX_ENVELOPE_BYTES,
            "{what}: envelope of {size} bytes exceeds the 256 KiB limit"
        );
    }
    if let Err(error) = validate_complete(log) {
        panic!("{what}: violates turn-event.md §5: {error}");
    }
    wire
}

/// `raw` is the verbatim ACP payload: for `session_update` steps the `update` object, for
/// requests the request itself. Synthetic events carry none.
fn assert_raw_preserved(case: &str, executed: &Executed) {
    let step = &executed.step;
    let expected = match step["op"].as_str().unwrap_or_default() {
        "session_update" => Some(&step["notification"]["update"]),
        "request_permission" | "elicitation" => Some(&step["request"]),
        _ => None,
    };
    let synthetic = [
        "TextMessageStart",
        "TextMessageEnd",
        "ReasoningMessageStart",
        "ReasoningMessageEnd",
        "RunStarted",
        "RunFinished",
        "RunError",
        "InterruptResolved",
        "UserMessage",
    ];
    let mut with_raw = 0;
    for envelope in &executed.produced {
        let Some(raw) = &envelope.raw else { continue };
        with_raw += 1;
        assert_eq!(
            serde_json::to_value(raw.source).expect("source"),
            json!("acp"),
            "{case}: raw.source"
        );
        if let Some(expected) = expected {
            // The tool call of a permission request is mapped from `toolCall` alone.
            let tool_call = expected.get("toolCall");
            assert!(
                &raw.payload == expected || Some(&raw.payload) == tool_call,
                "{case}: {} carries a raw payload that is not the ACP input:\n{}",
                envelope.event.type_name(),
                raw.payload
            );
        }
    }
    if expected.is_some()
        && executed
            .produced
            .iter()
            .any(|e| !synthetic.contains(&e.event.type_name()))
    {
        assert!(with_raw > 0, "{case}: ACP-derived events carry no raw");
    }
}

fn run_golden(case: &str) {
    let dir = golden_dir().join(case);
    let input = read_json(&dir.join("input.json"));
    let executed = execute(&input);
    let log = envelopes(&executed);
    let wire = assert_well_formed(case, &log);
    for step in &executed {
        assert_raw_preserved(case, step);
    }

    let actual = Value::Array(wire);
    let expected_path = dir.join("expected.json");
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        let mut text = serde_json::to_string_pretty(&actual).expect("pretty");
        text.push('\n');
        fs::write(&expected_path, text).expect("write expected.json");
        return;
    }
    let expected = read_json(&expected_path);
    assert!(
        expected.as_array().is_some_and(|log| !log.is_empty()),
        "{case}: expected.json is empty"
    );
    assert_eq!(
        actual,
        expected,
        "{case}: output differs from expected.json (UPDATE_GOLDEN=1 rewrites it)\nactual:\n{}",
        serde_json::to_string_pretty(&actual).expect("pretty")
    );
}

macro_rules! golden_cases {
    ($($case:ident),* $(,)?) => {
        const CASES: &[&str] = &[$(stringify!($case)),*];
        $(
            #[test]
            fn $case() {
                run_golden(stringify!($case));
            }
        )*
    };
}

golden_cases!(
    agent_message_chunk,
    agent_thought_chunk,
    message_id_boundary,
    non_text_chunk,
    user_message_chunk,
    hub_user_message,
    updates_outside_task,
    tool_call_lifecycle,
    tool_call_update_upsert,
    tool_call_diff,
    tool_call_terminal,
    tool_call_failed_on_creation,
    tool_call_result_content,
    tool_call_invalid_id,
    plan,
    available_commands_update,
    current_mode_update,
    config_option_update,
    usage_update,
    session_info_update,
    session_new_state,
    unknown_update_raw,
    request_permission_roundtrip,
    request_permission_two_pauses,
    request_permission_cancel,
    request_permission_expired,
    elicitation_form,
    elicitation_url,
    second_prompt_new_task,
    cancel_running,
    stop_reason_cancelled,
    stop_reason_max_tokens,
    stop_reason_max_turn_requests,
    stop_reason_refusal,
    stop_reason_end_turn_open_tool,
    harness_error,
    new_task_while_active,
);

#[test]
fn every_golden_directory_has_a_test() {
    let mut on_disk: Vec<String> = fs::read_dir(golden_dir())
        .expect("golden dir")
        .map(|entry| entry.expect("entry"))
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let mut tested: Vec<String> = CASES.iter().map(|case| (*case).to_owned()).collect();
    tested.sort();
    assert_eq!(on_disk, tested);
    for case in CASES {
        for file in ["input.json", "expected.json"] {
            assert!(
                golden_dir().join(case).join(file).is_file(),
                "{case}/{file}"
            );
        }
    }
}

// ---- behaviours that are awkward as fixtures ------------------------------------------------

fn text_of(envelope: &Envelope) -> &str {
    match &envelope.event {
        Event::TextMessageContent(content) => &content.delta,
        other => panic!("not a text delta: {}", other.type_name()),
    }
}

fn started(mapper: &mut AcpTurnMapper, task: &str) -> Vec<Envelope> {
    mapper.start_task(StartTask::new(task), "2026-10-27T22:00:00.000Z")
}

const TS: &str = "2026-10-27T22:00:01.000Z";

/// The `UserMessage` the hub emits when a task starts (client-api.md §6).
fn hub_user_message_for(task: &str) -> Envelope {
    Envelope::new(
        "th_1",
        Some(task.to_owned()),
        None,
        TS,
        protocol::turn_event::UserMessage {
            message_id: format!("msg_{task}"),
            parts: vec![ContentPart::Text { text: "go".into() }],
            author: runner_actor(),
        },
    )
}

#[test]
fn acp_fixtures_map_to_valid_sequences() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp");
    let mut checked = 0;
    for entry in fs::read_dir(fixtures).expect("fixtures") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        let fixture = read_json(&path);
        let mut mapper = AcpTurnMapper::with_sequential_ids("th_1");
        let mut log = vec![hub_user_message_for("tk_1")];
        log.extend(started(&mut mapper, "tk_1"));
        if name.starts_with("session_update_") {
            log.extend(mapper.on_session_update_json(&fixture, TS).expect("update"));
            log.extend(mapper.on_prompt_response(acp::StopReason::EndTurn, TS));
        } else if name == "request_permission.json" {
            let request = serde_json::from_value(fixture).expect("request");
            log.extend(mapper.on_request_permission(&request, "int_1", TS, TS));
            log.extend(mapper.on_interrupt_resolved(
                "int_1",
                ResolvedOutcome::Cancelled,
                runner_actor(),
                TS,
            ));
            log.extend(mapper.resume(TS));
            log.extend(mapper.on_prompt_response(acp::StopReason::EndTurn, TS));
        } else if name == "elicitation_create.json" {
            let request = serde_json::from_value(fixture).expect("request");
            log.extend(mapper.on_elicitation(&request, "int_1", TS, TS));
            log.extend(mapper.on_cancel(&runner_actor(), TS));
        } else {
            panic!("unhandled fixture {name}");
        }
        let mut seq = ThreadSeq::new();
        let log: Vec<Envelope> = log
            .into_iter()
            .map(|e| seq.stamp(e).expect("producer envelopes have seq 0"))
            .collect();
        assert_well_formed(&name, &log);
        checked += 1;
    }
    assert!(checked >= 10, "checked {checked} fixtures");
}

#[test]
fn typed_and_json_entry_points_agree() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp");
    for name in [
        "session_update_agent_message_chunk.json",
        "session_update_tool_call.json",
        "session_update_tool_call_update.json",
        "session_update_plan.json",
        "session_update_usage_update.json",
    ] {
        let fixture = read_json(&fixtures.join(name));
        let typed: acp::SessionNotification =
            serde_json::from_value(fixture.clone()).expect("typed");
        let mut by_type = AcpTurnMapper::with_sequential_ids("th_1");
        let mut by_json = AcpTurnMapper::with_sequential_ids("th_1");
        started(&mut by_type, "tk_1");
        started(&mut by_json, "tk_1");
        assert_eq!(
            by_type.on_session_update(&typed, TS),
            by_json.on_session_update_json(&fixture, TS).expect("json"),
            "{name}"
        );
    }
}

#[test]
fn session_update_json_needs_an_update_object() {
    let mut mapper = AcpTurnMapper::with_sequential_ids("th_1");
    assert!(
        mapper
            .on_session_update_json(&json!({"sessionId": "s"}), TS)
            .is_err()
    );
}

#[test]
fn mapper_output_is_deterministic_and_has_seq_zero() {
    let input = read_json(&golden_dir().join("request_permission_roundtrip/input.json"));
    let first = envelopes(&execute(&input));
    let second = envelopes(&execute(&input));
    assert_eq!(first, second);

    let mut mapper = AcpTurnMapper::with_sequential_ids("th_1");
    let mut out = started(&mut mapper, "tk_1");
    out.extend(mapper.on_prompt_response(acp::StopReason::EndTurn, TS));
    assert!(out.iter().all(|envelope| envelope.seq == 0));
    assert_eq!(out.len(), 2);
}

#[test]
fn accessors_follow_the_task_lifecycle() {
    let mut mapper = AcpTurnMapper::with_sequential_ids("th_9");
    assert_eq!(mapper.thread_id(), "th_9");
    assert_eq!((mapper.task_id(), mapper.run_id()), (None, None));
    started(&mut mapper, "tk_1");
    assert_eq!(mapper.task_id(), Some("tk_1"));
    assert_eq!(mapper.run_id(), Some("run_1"));
    let request: acp::RequestPermissionRequest = serde_json::from_value(read_json(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp/request_permission.json"),
    ))
    .expect("request");
    mapper.on_request_permission(&request, "int_1", TS, TS);
    assert!(mapper.is_paused());
    assert_eq!((mapper.task_id(), mapper.run_id()), (Some("tk_1"), None));
    mapper.resume(TS);
    assert!(!mapper.is_paused());
    assert_eq!(mapper.run_id(), Some("run_2"));
    mapper.on_prompt_response(acp::StopReason::EndTurn, TS);
    assert_eq!(mapper.task_id(), None);
    // Nothing is open any more: stray calls produce nothing.
    assert!(mapper.resume(TS).is_empty());
    assert!(mapper.on_cancel(&runner_actor(), TS).is_empty());
    assert!(
        mapper
            .on_prompt_response(acp::StopReason::EndTurn, TS)
            .is_empty()
    );
}

#[test]
fn a_big_diff_is_split_into_chunks_that_fit_the_envelope_limit() {
    let new_text: String = (0..6000)
        .map(|i| format!("// generated line {i:05}: lorem ipsum dolor sit amet, consectetur\n"))
        .collect();
    let old_text = "old\n".repeat(60_000);
    let update = json!({"sessionId": "s", "update": {
        "sessionUpdate": "tool_call", "toolCallId": "call_big", "title": "Write big.rs",
        "kind": "edit", "status": "in_progress",
        "content": [{"type": "diff", "path": "src/big.rs", "oldText": old_text, "newText": new_text}]
    }});
    let mut mapper = AcpTurnMapper::with_sequential_ids("th_1");
    let mut seq = ThreadSeq::new();
    let mut log = vec![hub_user_message_for("tk_1")];
    log.extend(started(&mut mapper, "tk_1"));
    log.extend(mapper.on_session_update_json(&update, TS).expect("update"));
    log.extend(mapper.on_prompt_response(acp::StopReason::EndTurn, TS));
    let log: Vec<Envelope> = log
        .into_iter()
        .map(|e| seq.stamp(e).expect("stamp"))
        .collect();
    assert_well_formed("big diff", &log);

    let chunks: Vec<&protocol::turn_event::Activity> = log
        .iter()
        .filter_map(|e| match &e.event {
            Event::Activity(a) => Some(a),
            _ => None,
        })
        .collect();
    assert!(chunks.len() >= 3, "{} chunks", chunks.len());
    let mut rebuilt_new = String::new();
    let mut rebuilt_old = String::new();
    for (index, activity) in chunks.iter().enumerate() {
        assert_eq!(activity.activity_id, "call_big:src/big.rs");
        let content: protocol::turn_event::DiffContent = activity
            .content_as()
            .expect("content")
            .expect("diff content");
        let chunk = content.chunk.expect("chunk");
        assert_eq!(chunk.index, index as u64);
        assert_eq!(chunk.last, index + 1 == chunks.len());
        assert_eq!(
            activity.mode,
            if index == 0 {
                protocol::turn_event::ActivityMode::Snapshot
            } else {
                protocol::turn_event::ActivityMode::Append
            }
        );
        rebuilt_new.push_str(&content.new_text);
        rebuilt_old.push_str(&content.old_text.expect("old text"));
    }
    assert_eq!(rebuilt_new, new_text);
    assert_eq!(rebuilt_old, old_text);
    // The payload alone is over the limit: no chunk may carry it past the limit.
    assert!(
        log.iter()
            .all(|e| { serde_json::to_vec(e).expect("bytes").len() <= MAX_ENVELOPE_BYTES })
    );
}

#[test]
fn a_big_text_delta_is_split_and_raw_is_dropped_rather_than_truncated() {
    let text = "x".repeat(300 * 1024);
    let update = json!({"sessionId": "s", "update": {
        "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}
    }});
    let mut mapper = AcpTurnMapper::with_sequential_ids("th_1");
    started(&mut mapper, "tk_1");
    let out = mapper.on_session_update_json(&update, TS).expect("update");
    let deltas: Vec<&Envelope> = out
        .iter()
        .filter(|e| matches!(e.event, Event::TextMessageContent(_)))
        .collect();
    assert_eq!(deltas.len(), 4);
    assert_eq!(deltas.iter().map(|e| text_of(e)).collect::<String>(), text);
    assert!(
        out.iter()
            .all(|e| serde_json::to_vec(e).expect("bytes").len() <= MAX_ENVELOPE_BYTES)
    );
    assert!(out.iter().all(|e| e.raw.is_none()), "raw is dropped whole");

    // A payload that fits stays.
    let small = json!({"sessionId": "s", "update": {
        "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"}
    }});
    let mut mapper = AcpTurnMapper::with_sequential_ids("th_1");
    started(&mut mapper, "tk_1");
    let out = mapper.on_session_update_json(&small, TS).expect("update");
    assert!(out.iter().any(|e| e.raw.is_some()));
}

#[test]
fn terminal_output_is_split_and_the_closing_snapshot_fits() {
    let mut mapper = AcpTurnMapper::with_sequential_ids("th_1");
    let mut seq = ThreadSeq::new();
    let mut log = vec![hub_user_message_for("tk_1")];
    log.extend(started(&mut mapper, "tk_1"));
    log.extend(mapper.on_terminal_started("term 1", None, Some("cargo build"), Some("/w"), TS));
    // 600 KiB of output in one call: split into append events, the snapshot keeps the tail.
    let output: String = (0..10_000)
        .map(|i| format!("line {i:05} of the build output, padded to sixty bytes....\n"))
        .collect();
    log.extend(mapper.on_terminal_output("term 1", &output, TS));
    log.extend(mapper.on_terminal_exit("term 1", Some(0), None, TS));
    log.extend(mapper.on_prompt_response(acp::StopReason::EndTurn, TS));
    let log: Vec<Envelope> = log
        .into_iter()
        .map(|e| seq.stamp(e).expect("stamp"))
        .collect();
    assert_well_formed("big terminal", &log);

    let activities: Vec<&protocol::turn_event::Activity> = log
        .iter()
        .filter_map(|e| match &e.event {
            Event::Activity(a) => Some(a),
            _ => None,
        })
        .collect();
    let appended: String = activities
        .iter()
        .filter(|a| a.mode == protocol::turn_event::ActivityMode::Append)
        .map(|a| {
            let content: protocol::turn_event::TerminalContent =
                a.content_as().expect("content").expect("terminal");
            content.output.expect("output")
        })
        .collect();
    assert_eq!(appended, output);
    assert!(activities.len() >= 5);
    let closing: protocol::turn_event::TerminalContent = activities
        .last()
        .expect("closing")
        .content_as()
        .expect("content")
        .expect("terminal");
    assert!(closing.exit.is_some());
    assert_eq!(closing.command.as_deref(), Some("cargo build"));
    let kept = closing.output.expect("output");
    assert!(output.ends_with(&kept) && kept.len() < output.len());
    // The invalid ACP terminal id was hashed into a valid Id.
    assert!(
        activities
            .iter()
            .all(|a| protocol::turn_event::is_valid_id(&a.activity_id))
    );
}

#[test]
fn a_new_task_ends_the_previous_one_so_the_log_stays_valid() {
    let mut mapper = AcpTurnMapper::with_sequential_ids("th_1");
    let mut seq = ThreadSeq::new();
    let mut log = vec![hub_user_message_for("tk_1")];
    log.extend(started(&mut mapper, "tk_1"));
    // The runner forgot the end of tk_1; the hub's UserMessage for tk_2 would be rejected, so
    // the runner ends tk_1 first (start_task does it when it is called with a task open).
    log.extend(mapper.on_error("protocol", "gave up", TS));
    log.push(hub_user_message_for("tk_2"));
    log.extend(started(&mut mapper, "tk_2"));
    log.extend(mapper.on_prompt_response(acp::StopReason::EndTurn, TS));
    let log: Vec<Envelope> = log
        .into_iter()
        .map(|e| seq.stamp(e).expect("stamp"))
        .collect();
    assert_well_formed("two tasks", &log);
    let run_ids: Vec<&str> = log
        .iter()
        .filter(|e| matches!(e.event, Event::RunStarted(_)))
        .filter_map(|e| e.run_id.as_deref())
        .collect();
    assert_eq!(run_ids, ["run_1", "run_2"]);
}

// ---- validate_sequence: negative tests, one per rule ----------------------------------------

mod validate {
    use super::*;
    use protocol::turn_event::{
        ActivityMode, ActivityType, Interrupt, InterruptKind, InterruptResolved, RunError,
        RunErrorReason, RunFinished, RunOutcome, RunReason, RunStarted, StateDelta, StateScope,
        StateSnapshot, StepFinished, StepOutcome, StepStarted, SubagentStarted, TextMessageContent,
        TextMessageEnd, TextMessageStart, ToolCallArgs, ToolCallEnd, ToolCallEndStatus,
        ToolCallStart, UserMessage,
    };

    const T: &str = "tk_1";
    const R: &str = "run_1";

    fn at(task: Option<&str>, run: Option<&str>, event: impl Into<Event>) -> Envelope {
        Envelope::new(
            "th_1",
            task.map(str::to_owned),
            run.map(str::to_owned),
            TS,
            event,
        )
    }

    fn user(task: &str) -> Envelope {
        at(
            Some(task),
            None,
            UserMessage {
                message_id: format!("msg_{task}"),
                parts: vec![ContentPart::Text { text: "go".into() }],
                author: runner_actor(),
            },
        )
    }

    fn run_started(task: &str, run: &str, reason: RunReason, parent: Option<&str>) -> Envelope {
        at(
            Some(task),
            Some(run),
            RunStarted {
                reason,
                parent_run_id: parent.map(str::to_owned),
                agent_id: None,
                machine_id: None,
            },
        )
    }

    fn start(task: &str, run: &str) -> Envelope {
        run_started(task, run, RunReason::Prompt, None)
    }

    fn success(task: &str, run: &str) -> Envelope {
        at(
            Some(task),
            Some(run),
            RunFinished {
                outcome: RunOutcome::Success { stop_reason: None },
            },
        )
    }

    fn paused_on(task: &str, run: &str, ids: &[&str]) -> Envelope {
        at(
            Some(task),
            Some(run),
            RunFinished {
                outcome: RunOutcome::Interrupt {
                    interrupt_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
                },
            },
        )
    }

    fn run_error(task: &str, run: &str) -> Envelope {
        at(
            Some(task),
            Some(run),
            RunError {
                reason: RunErrorReason::Error,
                code: None,
                message: "boom".into(),
            },
        )
    }

    fn interrupt(id: &str) -> Envelope {
        at(
            Some(T),
            Some(R),
            Interrupt {
                interrupt_id: id.into(),
                kind: InterruptKind::Approval,
                title: "Allow?".into(),
                message: None,
                expires_at: TS.into(),
                tool_call_id: None,
                options: None,
                response_schema: None,
                url: None,
            },
        )
    }

    fn resolved(id: &str) -> Envelope {
        at(
            Some(T),
            None,
            InterruptResolved {
                interrupt_id: id.into(),
                outcome: ResolvedOutcome::Cancelled,
                resolved_by: runner_actor(),
            },
        )
    }

    fn msg_start(id: &str) -> Envelope {
        at(
            Some(T),
            Some(R),
            TextMessageStart {
                message_id: id.into(),
                role: protocol::turn_event::AssistantRole::Assistant,
            },
        )
    }

    fn msg_content(id: &str) -> Envelope {
        at(
            Some(T),
            Some(R),
            TextMessageContent {
                message_id: id.into(),
                delta: "hi".into(),
            },
        )
    }

    fn msg_end(id: &str) -> Envelope {
        at(
            Some(T),
            Some(R),
            TextMessageEnd {
                message_id: id.into(),
            },
        )
    }

    fn tool_start(id: &str) -> Envelope {
        at(
            Some(T),
            Some(R),
            ToolCallStart {
                tool_call_id: id.into(),
                title: None,
                kind: None,
                status: None,
                locations: None,
                parent_message_id: None,
            },
        )
    }

    fn tool_args(id: &str) -> Envelope {
        at(
            Some(T),
            Some(R),
            ToolCallArgs {
                tool_call_id: id.into(),
                raw_input: json!({}),
            },
        )
    }

    fn tool_end(id: &str) -> Envelope {
        at(
            Some(T),
            Some(R),
            ToolCallEnd {
                tool_call_id: id.into(),
                status: ToolCallEndStatus::Completed,
            },
        )
    }

    fn stamped(log: Vec<Envelope>) -> Vec<Envelope> {
        let mut seq = ThreadSeq::new();
        log.into_iter()
            .map(|e| seq.stamp(e).expect("stamp"))
            .collect()
    }

    /// The minimal valid prefix of a task: `UserMessage`, `RunStarted`.
    fn open_run() -> Vec<Envelope> {
        vec![user(T), start(T, R)]
    }

    fn with(mut log: Vec<Envelope>, more: Vec<Envelope>) -> Vec<Envelope> {
        log.extend(more);
        log
    }

    #[track_caller]
    fn violates(log: Vec<Envelope>, rule: Rule, index: usize) -> SequenceError {
        let log = stamped(log);
        let error = validate_sequence(&log).expect_err("the log violates a rule");
        assert_eq!((error.rule, error.index), (rule, index), "{error}");
        assert!(!error.to_string().is_empty());
        error
    }

    #[test]
    fn a_complete_task_with_an_approval_round_trip_is_valid() {
        let log = stamped(vec![
            user(T),
            start(T, "run_1"),
            msg_start("msg_1"),
            msg_content("msg_1"),
            msg_end("msg_1"),
            tool_start("tc_1"),
            interrupt("int_1"),
            paused_on(T, "run_1", &["int_1"]),
            resolved("int_1"),
            run_started(T, "run_2", RunReason::Resume, Some("run_1")),
            at(
                Some(T),
                Some("run_2"),
                ToolCallEnd {
                    tool_call_id: "tc_1".into(),
                    status: ToolCallEndStatus::Completed,
                },
            ),
            success(T, "run_2"),
            user("tk_2"),
            start("tk_2", "run_3"),
            run_error("tk_2", "run_3"),
        ]);
        validate_complete(&log).expect("valid");
        // Producer-side logs (all seq 0) are valid too.
        let unstamped: Vec<Envelope> = vec![user(T), start(T, R), success(T, R)];
        validate_complete(&unstamped).expect("valid");
    }

    #[test]
    fn a_pause_may_leave_tool_calls_subagents_and_steps_open() {
        let log = stamped(vec![
            user(T),
            start(T, "run_1"),
            tool_start("tc_1"),
            at(
                Some(T),
                Some("run_1"),
                SubagentStarted {
                    subagent_run_id: "sub_1".into(),
                    name: "reviewer".into(),
                    description: None,
                    child_thread_id: None,
                    child_task_id: None,
                    agent_id: None,
                    parent_tool_call_id: None,
                    parent_subagent_run_id: None,
                },
            ),
            at(
                Some(T),
                Some("run_1"),
                StepStarted {
                    step_id: "implement.verify".into(),
                    step_name: "Verify".into(),
                    iteration: Some(1),
                },
            ),
            interrupt("int_1"),
            paused_on(T, "run_1", &["int_1"]),
        ]);
        validate_sequence(&log).expect("a paused run may leave them open");
        assert!(validate_complete(&log).is_err(), "but the task is not over");
    }

    // §3
    #[test]
    fn rule_seq_gap() {
        let mut log = stamped(with(open_run(), vec![success(T, R)]));
        log[2].seq = 4;
        let error = validate_sequence(&log).expect_err("gap");
        assert_eq!((error.rule, error.index), (Rule::Seq, 2));
        let mut duplicate = stamped(with(open_run(), vec![success(T, R)]));
        duplicate[2].seq = 2;
        assert_eq!(
            validate_sequence(&duplicate).expect_err("reuse").rule,
            Rule::Seq
        );
    }

    // Rule 1
    #[test]
    fn rule_1_run_started_without_a_user_message() {
        violates(vec![start(T, R)], Rule::TaskShape, 0);
    }

    #[test]
    fn rule_1_event_after_the_task_ended() {
        violates(
            with(open_run(), vec![success(T, R), msg_start("msg_1")]),
            Rule::TaskShape,
            3,
        );
    }

    #[test]
    fn rule_1_resume_without_a_paused_run() {
        violates(
            with(
                open_run(),
                vec![run_started(T, "run_2", RunReason::Resume, Some(R))],
            ),
            Rule::OneOpenRun,
            2,
        );
        violates(
            vec![user(T), run_started(T, R, RunReason::Resume, Some("run_0"))],
            Rule::TaskShape,
            1,
        );
    }

    #[test]
    fn rule_1_resume_names_the_wrong_parent() {
        violates(
            vec![user(T), start(T, R), paused_on(T, R, &[])],
            Rule::Interrupts,
            2,
        );
        violates(
            with(
                open_run(),
                vec![
                    interrupt("int_1"),
                    paused_on(T, R, &["int_1"]),
                    resolved("int_1"),
                    run_started(T, "run_2", RunReason::Resume, Some("run_9")),
                ],
            ),
            Rule::TaskShape,
            5,
        );
    }

    #[test]
    fn rule_1_prompt_run_after_an_earlier_run() {
        violates(
            with(
                open_run(),
                vec![
                    interrupt("int_1"),
                    paused_on(T, R, &["int_1"]),
                    resolved("int_1"),
                    run_started(T, "run_2", RunReason::Prompt, None),
                ],
            ),
            Rule::TaskShape,
            5,
        );
    }

    #[test]
    fn rule_1_the_log_must_end_between_tasks() {
        let log = stamped(open_run());
        validate_sequence(&log).expect("a valid prefix");
        let error = validate_complete(&log).expect_err("ends inside the task");
        assert_eq!(error.rule, Rule::TaskShape);
    }

    #[test]
    fn rule_1_task_ids_are_not_reused() {
        violates(
            with(open_run(), vec![success(T, R), user(T)]),
            Rule::TaskShape,
            3,
        );
    }

    // Rule 2
    #[test]
    fn rule_2_one_running_task_per_thread() {
        violates(with(open_run(), vec![user("tk_2")]), Rule::OneOpenRun, 2);
    }

    #[test]
    fn rule_2_one_open_run_per_task() {
        violates(
            with(open_run(), vec![start(T, "run_2")]),
            Rule::OneOpenRun,
            2,
        );
    }

    // Rule 3
    #[test]
    fn rule_3_run_events_carry_the_open_run_id() {
        violates(
            with(
                open_run(),
                vec![at(
                    Some(T),
                    Some("run_9"),
                    TextMessageStart {
                        message_id: "msg_1".into(),
                        role: protocol::turn_event::AssistantRole::Assistant,
                    },
                )],
            ),
            Rule::RunMembership,
            2,
        );
    }

    #[test]
    fn rule_3_run_events_need_an_open_run() {
        violates(vec![user(T), msg_start("msg_1")], Rule::RunMembership, 1);
        // Between the pause and the resume there is no open run.
        violates(
            with(
                open_run(),
                vec![
                    interrupt("int_1"),
                    paused_on(T, R, &["int_1"]),
                    msg_start("msg_1"),
                ],
            ),
            Rule::RunMembership,
            4,
        );
    }

    #[test]
    fn rule_3_events_belong_to_the_running_task() {
        violates(
            with(
                open_run(),
                vec![at(
                    Some("tk_2"),
                    Some(R),
                    TextMessageEnd {
                        message_id: "msg_1".into(),
                    },
                )],
            ),
            Rule::RunMembership,
            2,
        );
    }

    #[test]
    fn rule_3_scopes_are_checked() {
        // Run-scoped without run_id; task-scoped with run_id; thread-scoped with ids.
        violates(
            with(
                open_run(),
                vec![at(
                    Some(T),
                    None,
                    TextMessageEnd {
                        message_id: "msg_1".into(),
                    },
                )],
            ),
            Rule::RunMembership,
            2,
        );
        violates(
            vec![at(
                Some(T),
                Some(R),
                UserMessage {
                    message_id: "msg_1".into(),
                    parts: vec![ContentPart::Text { text: "x".into() }],
                    author: runner_actor(),
                },
            )],
            Rule::RunMembership,
            0,
        );
        violates(
            vec![at(
                Some(T),
                None,
                StateSnapshot {
                    scope: StateScope::Usage,
                    snapshot: serde_json::Map::new(),
                },
            )],
            Rule::RunMembership,
            0,
        );
    }

    // Rule 4
    #[test]
    fn rule_4_content_before_start() {
        violates(
            with(open_run(), vec![msg_content("msg_1")]),
            Rule::MessageOrder,
            2,
        );
    }

    #[test]
    fn rule_4_one_start_per_message() {
        violates(
            with(
                open_run(),
                vec![msg_start("msg_1"), msg_end("msg_1"), msg_start("msg_1")],
            ),
            Rule::MessageOrder,
            4,
        );
    }

    #[test]
    fn rule_4_end_needs_an_open_message() {
        violates(
            with(open_run(), vec![msg_end("msg_1")]),
            Rule::MessageOrder,
            2,
        );
        violates(
            with(
                open_run(),
                vec![msg_start("msg_1"), msg_end("msg_1"), msg_content("msg_1")],
            ),
            Rule::MessageOrder,
            4,
        );
    }

    #[test]
    fn rule_4_messages_close_before_the_terminal_event() {
        violates(
            with(open_run(), vec![msg_start("msg_1"), success(T, R)]),
            Rule::MessageOrder,
            3,
        );
        violates(
            with(open_run(), vec![msg_start("msg_1"), run_error(T, R)]),
            Rule::MessageOrder,
            3,
        );
        violates(
            with(
                open_run(),
                vec![
                    msg_start("msg_1"),
                    interrupt("int_1"),
                    paused_on(T, R, &["int_1"]),
                ],
            ),
            Rule::MessageOrder,
            4,
        );
    }

    // Rule 5
    #[test]
    fn rule_5_the_first_tool_call_event_is_the_start() {
        violates(
            with(open_run(), vec![tool_args("tc_1")]),
            Rule::ToolCalls,
            2,
        );
        violates(with(open_run(), vec![tool_end("tc_1")]), Rule::ToolCalls, 2);
    }

    #[test]
    fn rule_5_nothing_follows_the_end() {
        violates(
            with(
                open_run(),
                vec![tool_start("tc_1"), tool_end("tc_1"), tool_args("tc_1")],
            ),
            Rule::ToolCalls,
            4,
        );
        violates(
            with(
                open_run(),
                vec![tool_start("tc_1"), tool_end("tc_1"), tool_start("tc_1")],
            ),
            Rule::ToolCalls,
            4,
        );
        violates(
            with(
                open_run(),
                vec![tool_start("tc_1"), tool_end("tc_1"), tool_end("tc_1")],
            ),
            Rule::ToolCalls,
            4,
        );
    }

    #[test]
    fn rule_5_tool_calls_close_before_the_task_ends() {
        violates(
            with(open_run(), vec![tool_start("tc_1"), success(T, R)]),
            Rule::ToolCalls,
            3,
        );
        // On RunError the producer first closes every open tool call.
        violates(
            with(open_run(), vec![tool_start("tc_1"), run_error(T, R)]),
            Rule::ToolCalls,
            3,
        );
    }

    // Rule 6
    #[test]
    fn rule_6_the_pause_lists_every_open_interrupt() {
        violates(
            with(
                open_run(),
                vec![
                    interrupt("int_1"),
                    interrupt("int_2"),
                    paused_on(T, R, &["int_1"]),
                ],
            ),
            Rule::Interrupts,
            4,
        );
        violates(
            with(
                open_run(),
                vec![interrupt("int_1"), paused_on(T, R, &["int_1", "int_9"])],
            ),
            Rule::Interrupts,
            3,
        );
    }

    #[test]
    fn rule_6_interrupts_are_resolved_before_the_resume() {
        violates(
            with(
                open_run(),
                vec![
                    interrupt("int_1"),
                    paused_on(T, R, &["int_1"]),
                    run_started(T, "run_2", RunReason::Resume, Some(R)),
                ],
            ),
            Rule::Interrupts,
            4,
        );
    }

    #[test]
    fn rule_6_the_task_cannot_end_with_an_open_interrupt() {
        violates(
            with(open_run(), vec![interrupt("int_1"), success(T, R)]),
            Rule::Interrupts,
            3,
        );
        violates(
            with(open_run(), vec![interrupt("int_1"), run_error(T, R)]),
            Rule::Interrupts,
            3,
        );
    }

    #[test]
    fn rule_6_only_open_interrupts_are_resolved_and_only_once() {
        violates(
            with(open_run(), vec![resolved("int_1")]),
            Rule::Interrupts,
            2,
        );
        violates(
            with(
                open_run(),
                vec![
                    interrupt("int_1"),
                    paused_on(T, R, &["int_1"]),
                    resolved("int_1"),
                    resolved("int_1"),
                ],
            ),
            Rule::Interrupts,
            5,
        );
        violates(
            with(open_run(), vec![interrupt("int_1"), interrupt("int_1")]),
            Rule::Interrupts,
            3,
        );
    }

    // Rule 7
    #[test]
    fn rule_7_subagents_close_before_the_task_ends() {
        let subagent = at(
            Some(T),
            Some(R),
            SubagentStarted {
                subagent_run_id: "sub_1".into(),
                name: "reviewer".into(),
                description: None,
                child_thread_id: None,
                child_task_id: None,
                agent_id: None,
                parent_tool_call_id: None,
                parent_subagent_run_id: None,
            },
        );
        violates(
            with(open_run(), vec![subagent, success(T, R)]),
            Rule::SubagentsAndSteps,
            3,
        );
        violates(
            with(
                open_run(),
                vec![at(
                    Some(T),
                    Some(R),
                    protocol::turn_event::SubagentFinished {
                        subagent_run_id: "sub_9".into(),
                        result: None,
                    },
                )],
            ),
            Rule::SubagentsAndSteps,
            2,
        );
    }

    #[test]
    fn rule_7_steps_close_with_the_same_step_and_iteration() {
        let step = |iteration| StepStarted {
            step_id: "implement.verify".into(),
            step_name: "Verify".into(),
            iteration,
        };
        violates(
            with(
                open_run(),
                vec![at(Some(T), Some(R), step(Some(1))), run_error(T, R)],
            ),
            Rule::SubagentsAndSteps,
            3,
        );
        let finished = |iteration| StepFinished {
            step_id: "implement.verify".into(),
            step_name: "Verify".into(),
            iteration,
            outcome: StepOutcome::Success,
        };
        violates(
            with(
                open_run(),
                vec![
                    at(Some(T), Some(R), step(Some(1))),
                    at(Some(T), Some(R), finished(Some(2))),
                ],
            ),
            Rule::SubagentsAndSteps,
            3,
        );
        // Closed correctly: fine.
        validate_complete(&stamped(with(
            open_run(),
            vec![
                at(Some(T), Some(R), step(Some(1))),
                at(Some(T), Some(R), finished(Some(1))),
                success(T, R),
            ],
        )))
        .expect("closed");
    }

    // Rule 8
    #[test]
    fn rule_8_a_delta_needs_the_snapshot_of_its_scope() {
        let delta = |scope| {
            at(
                None,
                None,
                StateDelta {
                    scope,
                    patch: vec![],
                },
            )
        };
        violates(vec![delta(StateScope::Usage)], Rule::SnapshotBeforeDelta, 0);
        violates(
            vec![
                at(
                    None,
                    None,
                    StateSnapshot {
                        scope: StateScope::Session,
                        snapshot: serde_json::Map::new(),
                    },
                ),
                delta(StateScope::Usage),
            ],
            Rule::SnapshotBeforeDelta,
            1,
        );
    }

    #[test]
    fn rule_8_an_activity_append_or_patch_needs_its_snapshot() {
        let activity = |mode, id: &str| {
            at(
                Some(T),
                Some(R),
                protocol::turn_event::Activity {
                    activity_id: id.into(),
                    activity_type: ActivityType::Terminal,
                    mode,
                    content: Some(serde_json::Map::new()),
                    patch: Some(vec![]),
                },
            )
        };
        violates(
            with(open_run(), vec![activity(ActivityMode::Append, "term_1")]),
            Rule::SnapshotBeforeDelta,
            2,
        );
        violates(
            with(
                open_run(),
                vec![
                    activity(ActivityMode::Snapshot, "term_1"),
                    activity(ActivityMode::Patch, "term_2"),
                ],
            ),
            Rule::SnapshotBeforeDelta,
            3,
        );
        validate_sequence(&stamped(with(
            open_run(),
            vec![
                activity(ActivityMode::Snapshot, "term_1"),
                activity(ActivityMode::Append, "term_1"),
            ],
        )))
        .expect("snapshot first");
    }

    #[test]
    fn thread_scoped_events_may_appear_anywhere_and_unknown_types_are_ignored() {
        let presence = at(
            None,
            None,
            protocol::turn_event::Presence {
                machine_id: "mch_1".into(),
                state: protocol::turn_event::MachinePresence::Online,
                since: TS.into(),
            },
        );
        let log = stamped(vec![
            presence.clone(),
            user(T),
            presence.clone(),
            start(T, R),
            presence,
            at(None, None, Event::Unknown),
            success(T, R),
        ]);
        validate_complete(&log).expect("valid");
    }
}
