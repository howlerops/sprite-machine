//! The four canned threads. Every function returns a script whose log satisfies
//! `docs/specs/turn-event.md` §5; the tests check that with an independent checker.

use protocol::tunnel::ContentPart;
use protocol::turn_event::{
    Activity, ActivityMode, ActivityType, AssistantRole, Checkpoint, CheckpointKind, Chunk,
    DiffContent, Interrupt, InterruptKind, InterruptOption, Location, MachinePresence, PatchOp,
    PatchOpKind, PermissionOptionKind, PlanContent, PlanEntry, PlanPriority, PlanStatus, Presence,
    ReasoningMessageContent, ReasoningMessageEnd, ReasoningMessageStart, ReasoningRole,
    RunFinished, RunOutcome, RunReason, RunStarted, StateDelta, StateScope, StateSnapshot,
    TerminalContent, TerminalExit, TextMessageContent, TextMessageEnd, TextMessageStart,
    ToolCallArgs, ToolCallEnd, ToolCallEndStatus, ToolCallOpenStatus, ToolCallResult,
    ToolCallStart, ToolKind, format_timestamp,
};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::script::{APPROVAL_TTL_MS, BASE_MS, Builder, Script, user_message};

/// The transcript keeps adding tasks until it holds at least this many events.
const TRANSCRIPT_MIN_EVENTS: usize = 10_000;
/// Lines in the generated file of the diff scenario.
const DIFF_LINES: usize = 5_000;
/// Bytes of `new_text` per diff chunk (a 256 KiB envelope holds this plus overhead).
const DIFF_CHUNK_BYTES: usize = 96 * 1024;
/// Lines of terminal output.
const TERMINAL_LINES: usize = 300;
/// Bytes of terminal output per `append` event.
const TERMINAL_CHUNK_BYTES: usize = 1024;

const TOPICS: [&str; 6] = [
    "the parser",
    "the router",
    "the cache layer",
    "the CLI flags",
    "the retry policy",
    "the schema",
];
const PATHS: [&str; 6] = [
    "src/parser.rs",
    "src/router.rs",
    "src/cache.rs",
    "src/cli.rs",
    "src/retry.rs",
    "src/schema.rs",
];

/// `Map` from any serializable value (infallible for the types used here).
fn content_of<T: Serialize>(value: &T) -> Map<String, Value> {
    match serde_json::to_value(value) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// A `Map` from a `json!` object literal.
fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn text(value: impl Into<String>) -> ContentPart {
    ContentPart::Text { text: value.into() }
}

fn tool_start(
    tool_call_id: &str,
    title: &str,
    kind: ToolKind,
    path: Option<&str>,
) -> ToolCallStart {
    ToolCallStart {
        tool_call_id: tool_call_id.to_owned(),
        title: Some(title.to_owned()),
        kind: Some(kind),
        status: Some(ToolCallOpenStatus::Pending),
        locations: path.map(|p| {
            vec![Location {
                path: p.to_owned(),
                line: Some(1),
            }]
        }),
        parent_message_id: None,
    }
}

fn tool_in_progress(tool_call_id: &str) -> ToolCallStart {
    ToolCallStart {
        tool_call_id: tool_call_id.to_owned(),
        title: None,
        kind: None,
        status: Some(ToolCallOpenStatus::InProgress),
        locations: None,
        parent_message_id: None,
    }
}

fn tool_end(tool_call_id: &str, status: ToolCallEndStatus) -> ToolCallEnd {
    ToolCallEnd {
        tool_call_id: tool_call_id.to_owned(),
        status,
    }
}

fn replace(path: &str, value: Value) -> PatchOp {
    PatchOp {
        op: PatchOpKind::Replace,
        path: path.to_owned(),
        from: None,
        value: Some(value),
    }
}

/// One assistant text message: `Start`, one `Content` per delta, `End`.
fn text_message(b: &mut Builder, task: &str, run: &str, message_id: &str, deltas: &[&str]) {
    b.run_event(
        task,
        run,
        TextMessageStart {
            message_id: message_id.to_owned(),
            role: AssistantRole::Assistant,
        },
    );
    for delta in deltas {
        b.run_event(
            task,
            run,
            TextMessageContent {
                message_id: message_id.to_owned(),
                delta: (*delta).to_owned(),
            },
        );
    }
    b.run_event(
        task,
        run,
        TextMessageEnd {
            message_id: message_id.to_owned(),
        },
    );
}

/// One reasoning block: `Start`, one `Content` per delta, `End`.
fn reasoning_message(b: &mut Builder, task: &str, run: &str, message_id: &str, deltas: &[&str]) {
    b.run_event(
        task,
        run,
        ReasoningMessageStart {
            message_id: message_id.to_owned(),
            role: ReasoningRole::Reasoning,
        },
    );
    for delta in deltas {
        b.run_event(
            task,
            run,
            ReasoningMessageContent {
                message_id: message_id.to_owned(),
                delta: (*delta).to_owned(),
            },
        );
    }
    b.run_event(
        task,
        run,
        ReasoningMessageEnd {
            message_id: message_id.to_owned(),
        },
    );
}

fn presence(b: &mut Builder) {
    b.thread_event(Presence {
        machine_id: "mch_fixture".to_owned(),
        state: MachinePresence::Online,
        since: format_timestamp(BASE_MS),
    });
}

/// Splits `lines` into chunks of at most `budget` bytes, each line newline-terminated.
fn chunk_lines(lines: &[String], budget: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in lines {
        if !current.is_empty() && current.len() + line.len() + 1 > budget {
            chunks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Splits `text` into pieces of at most `budget` bytes, never inside a UTF-8 character.
fn chunk_text(text: &str, budget: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + budget).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        pieces.push(&text[start..end]);
        start = end;
    }
    pieces
}

/// The `transcript-10k` scenario: many tasks, each with a prompt, reasoning, a plan, an
/// assistant message, three tool calls (read, edit with a diff, execute), a usage delta and a
/// checkpoint.
pub fn transcript(thread_id: &str) -> Script {
    let mut b = Builder::new(thread_id);
    b.thread_event(StateSnapshot {
        scope: StateScope::Session,
        snapshot: object(json!({
            "available_commands": [{"name": "review", "description": "Review the diff"}],
            "current_mode_id": "code",
            "modes": [{"id": "code", "name": "Code"}],
            "config_options": []
        })),
    });
    b.thread_event(StateSnapshot {
        scope: StateScope::Usage,
        snapshot: object(json!({
            "context_used": 0,
            "context_size": 200000,
            "cost": {"amount": 0.0, "currency": "USD"}
        })),
    });
    b.thread_event(StateSnapshot {
        scope: StateScope::Thread,
        snapshot: object(json!({"title": "Synthetic transcript", "pending_tasks": []})),
    });
    presence(&mut b);

    let mut task_no = 0;
    while b.emitted() < TRANSCRIPT_MIN_EVENTS {
        task_no += 1;
        transcript_task(&mut b, thread_id, task_no);
        if task_no % 50 == 0 {
            presence(&mut b);
        }
    }
    b.into_script()
}

fn transcript_task(b: &mut Builder, thread_id: &str, n: u64) {
    let index = usize::try_from(n).unwrap_or(0);
    let topic = TOPICS[index % TOPICS.len()];
    let path = PATHS[index % PATHS.len()];
    let task = format!("tk_{n:05}");
    let run = format!("run_{n:05}");
    let read = format!("tc_{n:05}_read");
    let edit = format!("tc_{n:05}_edit");
    let exec = format!("tc_{n:05}_exec");
    let command = "cargo test -p demo";

    b.task_event(
        &task,
        user_message(
            &format!("msg_user_{n:05}"),
            &format!("Work on {topic} (task {n})."),
        ),
    );
    b.run_started(&task, &run, RunReason::Prompt);

    reasoning_message(
        b,
        &task,
        &run,
        &format!("msg_think_{n:05}"),
        &[
            &format!("Looking at {topic}. "),
            "Reading the current code before editing.",
        ],
    );

    b.run_event(
        &task,
        &run,
        Activity {
            activity_id: "plan".to_owned(),
            activity_type: ActivityType::Plan,
            mode: ActivityMode::Snapshot,
            content: Some(content_of(&PlanContent {
                entries: vec![
                    PlanEntry {
                        content: format!("Read {path}"),
                        priority: PlanPriority::High,
                        status: PlanStatus::InProgress,
                    },
                    PlanEntry {
                        content: format!("Update {topic}"),
                        priority: PlanPriority::Medium,
                        status: PlanStatus::Pending,
                    },
                    PlanEntry {
                        content: "Run the tests".to_owned(),
                        priority: PlanPriority::Low,
                        status: PlanStatus::Pending,
                    },
                ],
            })),
            patch: None,
        },
    );
    for (entry, status) in [(0, "completed"), (1, "in_progress")] {
        b.run_event(
            &task,
            &run,
            Activity {
                activity_id: "plan".to_owned(),
                activity_type: ActivityType::Plan,
                mode: ActivityMode::Patch,
                content: None,
                patch: Some(vec![replace(
                    &format!("/entries/{entry}/status"),
                    json!(status),
                )]),
            },
        );
    }

    text_message(
        b,
        &task,
        &run,
        &format!("msg_text_{n:05}"),
        &[
            &format!("I will read {path} "),
            "and then update it.",
            " Done soon.",
        ],
    );

    // Read: a start, its args, a result and the end.
    b.run_event_raw(
        &task,
        &run,
        tool_start(&read, &format!("Read {path}"), ToolKind::Read, Some(path)),
        json!({"sessionUpdate": "tool_call", "toolCallId": read, "kind": "read"}),
    );
    b.run_event(&task, &run, tool_in_progress(&read));
    b.run_event(
        &task,
        &run,
        ToolCallArgs {
            tool_call_id: read.clone(),
            raw_input: json!({"path": path}),
        },
    );
    b.run_event(
        &task,
        &run,
        ToolCallResult {
            tool_call_id: read.clone(),
            content: vec![text(format!("// {path}: 42 lines, {topic}"))],
            raw_output: Some(json!({"lines": 42})),
        },
    );
    b.run_event(&task, &run, tool_end(&read, ToolCallEndStatus::Completed));

    // Edit: a start, its args, the diff it made, a result and the end.
    b.run_event(
        &task,
        &run,
        tool_start(&edit, &format!("Edit {path}"), ToolKind::Edit, Some(path)),
    );
    b.run_event(&task, &run, tool_in_progress(&edit));
    b.run_event(
        &task,
        &run,
        ToolCallArgs {
            tool_call_id: edit.clone(),
            raw_input: json!({"path": path, "edits": 1}),
        },
    );
    b.run_event(
        &task,
        &run,
        Activity {
            activity_id: format!("{edit}:{path}"),
            activity_type: ActivityType::Diff,
            mode: ActivityMode::Snapshot,
            content: Some(content_of(&DiffContent {
                tool_call_id: Some(edit.clone()),
                path: path.to_owned(),
                old_text: Some("pub fn handle() {}\n".to_owned()),
                new_text: format!("pub fn handle() -> u32 {{ {n} }}\n"),
                chunk: None,
            })),
            patch: None,
        },
    );
    b.run_event(
        &task,
        &run,
        ToolCallResult {
            tool_call_id: edit.clone(),
            content: vec![text(format!("Applied 1 edit to {path}"))],
            raw_output: None,
        },
    );
    b.run_event(&task, &run, tool_end(&edit, ToolCallEndStatus::Completed));

    // Execute: a failing run every fifth task.
    let failed = n.is_multiple_of(5);
    b.run_event(
        &task,
        &run,
        tool_start(&exec, command, ToolKind::Execute, None),
    );
    b.run_event(
        &task,
        &run,
        ToolCallArgs {
            tool_call_id: exec.clone(),
            raw_input: json!({"command": command}),
        },
    );
    let summary = if failed {
        "test result: FAILED. 0 passed; 1 failed"
    } else {
        "test result: ok. 1 passed; 0 failed"
    };
    b.run_event(
        &task,
        &run,
        ToolCallResult {
            tool_call_id: exec.clone(),
            content: vec![text(summary)],
            raw_output: None,
        },
    );
    let status = if failed {
        ToolCallEndStatus::Failed
    } else {
        ToolCallEndStatus::Completed
    };
    b.run_event(&task, &run, tool_end(&exec, status));

    b.thread_event(StateDelta {
        scope: StateScope::Usage,
        patch: vec![
            replace("/context_used", json!(1000 + n * 350)),
            replace("/cost/amount", json!(n as f64 * 0.0021)),
        ],
    });
    b.run_event(
        &task,
        &run,
        Checkpoint {
            checkpoint_id: format!("cp_{n:05}"),
            kind: CheckpointKind::ShadowGit,
            git_ref: Some(format!("refs/sm/checkpoints/{thread_id}/{n}")),
            backup_handle: None,
            label: Some(format!("turn {n}")),
            files_changed: Some(2),
        },
    );
    b.run_success(&task, &run);
}

/// The `diff-5k` scenario: one task writes a 5,000-line file as an `Activity{diff}` split into
/// consecutive chunks (`chunk: {index, last}`), the first a snapshot and the rest appends.
pub fn diff(thread_id: &str) -> Script {
    let mut b = Builder::new(thread_id);
    let task = "tk_00001";
    let run = "run_00001";
    let call = "tc_00001_write";
    let path = "src/generated.rs";

    b.task_event(
        task,
        user_message("msg_user_00001", "Generate the bindings module."),
    );
    b.run_started(task, run, RunReason::Prompt);
    b.run_event(
        task,
        run,
        tool_start(call, &format!("Write {path}"), ToolKind::Edit, Some(path)),
    );
    b.run_event(task, run, tool_in_progress(call));
    b.run_event(
        task,
        run,
        ToolCallArgs {
            tool_call_id: call.to_owned(),
            raw_input: json!({"path": path}),
        },
    );

    let lines: Vec<String> = (1..=DIFF_LINES)
        .map(|i| {
            format!(
                "// generated line {i:04}: lorem ipsum dolor sit amet, consectetur adipiscing elit"
            )
        })
        .collect();
    let chunks = chunk_lines(&lines, DIFF_CHUNK_BYTES);
    let last_index = chunks.len() - 1;
    let activity_id = format!("{call}:{path}");
    for (index, new_text) in chunks.into_iter().enumerate() {
        let mode = if index == 0 {
            ActivityMode::Snapshot
        } else {
            ActivityMode::Append
        };
        let content = DiffContent {
            tool_call_id: Some(call.to_owned()),
            path: path.to_owned(),
            old_text: None,
            new_text,
            chunk: Some(Chunk {
                index: index as u64,
                last: index == last_index,
            }),
        };
        b.run_event(
            task,
            run,
            Activity {
                activity_id: activity_id.clone(),
                activity_type: ActivityType::Diff,
                mode,
                content: Some(content_of(&content)),
                patch: None,
            },
        );
    }

    b.run_event(
        task,
        run,
        ToolCallResult {
            tool_call_id: call.to_owned(),
            content: vec![text(format!("Wrote {DIFF_LINES} lines to {path}"))],
            raw_output: None,
        },
    );
    b.run_event(task, run, tool_end(call, ToolCallEndStatus::Completed));
    text_message(
        &mut b,
        task,
        run,
        "msg_text_00001",
        &["Wrote the bindings module."],
    );
    b.run_success(task, run);
    b.into_script()
}

/// The `terminal` scenario: a `cargo test` terminal. The `snapshot` opens it, `append` events
/// stream the output bytes, and a final `snapshot` carries the exit status. A `snapshot`
/// replaces the stored content, so the final one repeats the whole output.
pub fn terminal(thread_id: &str) -> Script {
    let mut b = Builder::new(thread_id);
    let task = "tk_00001";
    let run = "run_00001";
    let call = "tc_00001_test";
    let terminal_id = "term_00001";
    let command = "cargo test -p agui-fixture";
    let cwd = "/workspace/agui-fixture";

    b.task_event(task, user_message("msg_user_00001", "Run the test suite."));
    b.run_started(task, run, RunReason::Prompt);
    b.run_event(
        task,
        run,
        ToolCallStart {
            tool_call_id: call.to_owned(),
            title: Some(command.to_owned()),
            kind: Some(ToolKind::Execute),
            status: Some(ToolCallOpenStatus::InProgress),
            locations: None,
            parent_message_id: None,
        },
    );
    b.run_event(
        task,
        run,
        ToolCallArgs {
            tool_call_id: call.to_owned(),
            raw_input: json!({"command": command}),
        },
    );
    // Opening snapshot and append events carry only the fields that change; the closing
    // snapshot carries command, cwd, the whole output and the exit.
    let term = |output: Option<String>, exit: Option<TerminalExit>, opened: bool| TerminalContent {
        terminal_id: terminal_id.to_owned(),
        tool_call_id: Some(call.to_owned()),
        command: opened.then(|| command.to_owned()),
        cwd: opened.then(|| cwd.to_owned()),
        output,
        exit,
    };
    b.run_event(
        task,
        run,
        Activity {
            activity_id: terminal_id.to_owned(),
            activity_type: ActivityType::Terminal,
            mode: ActivityMode::Snapshot,
            content: Some(content_of(&term(None, None, true))),
            patch: None,
        },
    );

    let output: String = (1..=TERMINAL_LINES)
        .map(|i| format!("\u{1b}[32mtest case_{i:03} ... ok\u{1b}[0m \u{2713} ({i} ms)\n"))
        .collect();
    for piece in chunk_text(&output, TERMINAL_CHUNK_BYTES) {
        b.run_event(
            task,
            run,
            Activity {
                activity_id: terminal_id.to_owned(),
                activity_type: ActivityType::Terminal,
                mode: ActivityMode::Append,
                content: Some(content_of(&term(Some(piece.to_owned()), None, false))),
                patch: None,
            },
        );
    }
    b.run_event(
        task,
        run,
        Activity {
            activity_id: terminal_id.to_owned(),
            activity_type: ActivityType::Terminal,
            mode: ActivityMode::Snapshot,
            content: Some(content_of(&term(
                Some(output),
                Some(TerminalExit {
                    exit_code: Some(0),
                    signal: None,
                }),
                true,
            ))),
            patch: None,
        },
    );

    b.run_event(
        task,
        run,
        ToolCallResult {
            tool_call_id: call.to_owned(),
            content: vec![text("exit code 0")],
            raw_output: None,
        },
    );
    b.run_event(task, run, tool_end(call, ToolCallEndStatus::Completed));
    text_message(&mut b, task, run, "msg_text_00001", &["All tests passed."]);
    b.run_success(task, run);
    b.into_script()
}

/// The `approval` scenario: the run edits a file after asking; it ends
/// `RunFinished{interrupt}` and waits. The answer to `int_00001` appends `InterruptResolved`,
/// then the resume run (`RunStarted{resume, parent_run_id}`) finishes the task.
pub fn approval(thread_id: &str) -> Script {
    let mut b = Builder::new(thread_id);
    let task = "tk_00001";
    let run_1 = "run_00001";
    let run_2 = "run_00002";
    let call = "tc_00001_route";
    let interrupt = "int_00001";
    let path = "src/routes.rs";

    b.task_event(
        task,
        user_message("msg_user_00001", "Add a /health route to the server."),
    );
    b.run_started(task, run_1, RunReason::Prompt);
    text_message(
        &mut b,
        task,
        run_1,
        "msg_text_00001",
        &["I will add the route and ask before editing."],
    );
    b.run_event(
        task,
        run_1,
        tool_start(call, &format!("Edit {path}"), ToolKind::Edit, Some(path)),
    );
    b.run_event(
        task,
        run_1,
        ToolCallArgs {
            tool_call_id: call.to_owned(),
            raw_input: json!({"path": path, "change": "add /health route"}),
        },
    );
    b.run_event(
        task,
        run_1,
        Activity {
            activity_id: format!("{call}:{path}"),
            activity_type: ActivityType::Diff,
            mode: ActivityMode::Snapshot,
            content: Some(content_of(&DiffContent {
                tool_call_id: Some(call.to_owned()),
                path: path.to_owned(),
                old_text: Some("pub fn routes() -> Router {\n    Router::new()\n}\n".to_owned()),
                new_text: "pub fn routes() -> Router {\n    Router::new().route(\"/health\", get(health))\n}\n"
                    .to_owned(),
                chunk: None,
            })),
            patch: None,
        },
    );
    b.run_event(
        task,
        run_1,
        Interrupt {
            interrupt_id: interrupt.to_owned(),
            kind: InterruptKind::Approval,
            title: format!("Edit {path}"),
            message: Some(format!("Allow the fixture agent to edit {path}?")),
            expires_at: b.timestamp_after(APPROVAL_TTL_MS),
            tool_call_id: Some(call.to_owned()),
            options: Some(vec![
                InterruptOption {
                    option_id: "allow".to_owned(),
                    name: "Allow once".to_owned(),
                    kind: PermissionOptionKind::AllowOnce,
                },
                InterruptOption {
                    option_id: "reject".to_owned(),
                    name: "Reject".to_owned(),
                    kind: PermissionOptionKind::RejectOnce,
                },
            ]),
            response_schema: None,
            url: None,
        },
    );
    b.run_event(
        task,
        run_1,
        RunFinished {
            outcome: RunOutcome::Interrupt {
                interrupt_ids: vec![interrupt.to_owned()],
            },
        },
    );
    b.await_answer(task, interrupt);

    b.run_event(
        task,
        run_2,
        RunStarted {
            reason: RunReason::Resume,
            parent_run_id: Some(run_1.to_owned()),
            agent_id: Some("agt_fixture".to_owned()),
            machine_id: Some("mch_fixture".to_owned()),
        },
    );
    b.run_event(
        task,
        run_2,
        ToolCallResult {
            tool_call_id: call.to_owned(),
            content: vec![text(format!("Edited {path}"))],
            raw_output: None,
        },
    );
    b.run_event(task, run_2, tool_end(call, ToolCallEndStatus::Completed));
    text_message(
        &mut b,
        task,
        run_2,
        "msg_text_00002",
        &["Added GET /health; it returns 200 OK."],
    );
    b.run_event(
        task,
        run_2,
        Checkpoint {
            checkpoint_id: "cp_00001".to_owned(),
            kind: CheckpointKind::ShadowGit,
            git_ref: Some(format!("refs/sm/checkpoints/{thread_id}/1")),
            backup_handle: None,
            label: Some("add /health".to_owned()),
            files_changed: Some(1),
        },
    );
    b.run_success(task, run_2);
    b.into_script()
}
