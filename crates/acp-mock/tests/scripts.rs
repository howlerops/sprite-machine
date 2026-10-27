//! Drives `acp-mock` as an ACP client would: over an in-memory duplex pipe, and once over
//! the real binary's stdio. Every `session/update` is parsed into the protocol types.

use std::time::Duration;

use acp_mock::{FAKE_API_KEY, Script};
use protocol::acp::{
    ContentBlock, CreateElicitationRequest, CreateTerminalRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, StopReason,
    TerminalOutputRequest, ToolCallContent, ToolCallStatus, WaitForTerminalExitRequest,
};
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};
use tokio::time::timeout;

type ClientLines = Lines<BufReader<ReadHalf<DuplexStream>>>;

const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// A minimal ACP client over a line-oriented pipe.
struct Client {
    lines: ClientLines,
    writer: WriteHalf<DuplexStream>,
    next_id: u64,
}

/// Everything the agent sent while a request was pending.
struct Exchange {
    /// The JSON-RPC response to the request.
    response: Value,
    /// Every `session/update`, parsed into the protocol type.
    updates: Vec<SessionNotification>,
    /// Every agent-to-client request, as `(method, params)`, in arrival order.
    requests: Vec<(String, Value)>,
}

impl Exchange {
    fn result(&self) -> &Value {
        self.response
            .get("result")
            .unwrap_or_else(|| panic!("expected a result, got {}", self.response))
    }

    fn error(&self) -> &Value {
        self.response
            .get("error")
            .unwrap_or_else(|| panic!("expected an error, got {}", self.response))
    }

    fn prompt(&self) -> PromptResponse {
        serde_json::from_value(self.result().clone()).expect("typed prompt response")
    }

    fn request_methods(&self) -> Vec<&str> {
        self.requests
            .iter()
            .map(|(method, _)| method.as_str())
            .collect()
    }
}

/// Starts the mock in-process on the far end of a duplex pipe.
fn connect(script: Value) -> Client {
    let script = Script::from_value(&script).expect("valid script");
    let (client_io, agent_io) = tokio::io::duplex(1 << 16);
    let (agent_reader, agent_writer) = tokio::io::split(agent_io);
    tokio::spawn(async move { acp_mock::run(script, agent_reader, agent_writer).await });
    let (reader, writer) = tokio::io::split(client_io);
    Client {
        lines: BufReader::new(reader).lines(),
        writer,
        next_id: 1,
    }
}

impl Client {
    async fn send(&mut self, message: &Value) {
        let mut bytes = serde_json::to_vec(message).expect("serialize");
        bytes.push(b'\n');
        self.writer.write_all(&bytes).await.expect("write to agent");
        self.writer.flush().await.expect("flush to agent");
    }

    async fn recv(&mut self) -> Value {
        let line = timeout(IO_TIMEOUT, self.lines.next_line())
            .await
            .expect("agent went quiet")
            .expect("read from agent")
            .expect("agent closed its stdout");
        serde_json::from_str(&line).expect("agent wrote JSON")
    }

    /// Sends a request and reads until its response, answering agent requests with
    /// `answer(method, params)`.
    async fn call(
        &mut self,
        method: &str,
        params: Value,
        answer: &mut dyn FnMut(&str, &Value) -> Value,
    ) -> Exchange {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        self.read_until_response(id, answer).await
    }

    async fn read_until_response(
        &mut self,
        id: u64,
        answer: &mut dyn FnMut(&str, &Value) -> Value,
    ) -> Exchange {
        let mut exchange = Exchange {
            response: Value::Null,
            updates: Vec::new(),
            requests: Vec::new(),
        };
        loop {
            let message = self.recv().await;
            match message.get("method").and_then(Value::as_str) {
                Some("session/update") => {
                    let update = serde_json::from_value(message["params"].clone())
                        .expect("session/update parses as SessionNotification");
                    exchange.updates.push(update);
                }
                Some(method) => {
                    let method = method.to_owned();
                    let params = message["params"].clone();
                    let request_id = message["id"].clone();
                    let result = answer(&method, &params);
                    self.send(&json!({ "jsonrpc": "2.0", "id": request_id, "result": result }))
                        .await;
                    exchange.requests.push((method, params));
                }
                None if message.get("id") == Some(&json!(id)) => {
                    exchange.response = message;
                    return exchange;
                }
                None => panic!("unexpected message {message}"),
            }
        }
    }

    /// Sends `session/prompt` with the default answers to agent requests.
    async fn prompt(&mut self, session: &str, text: &str) -> Exchange {
        self.call(
            "session/prompt",
            json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] }),
            &mut default_answer,
        )
        .await
    }

    async fn new_session(&mut self) -> String {
        let exchange = self
            .call(
                "session/new",
                json!({ "cwd": "/work", "mcpServers": [] }),
                &mut default_answer,
            )
            .await;
        exchange.result()["sessionId"]
            .as_str()
            .expect("session id")
            .to_owned()
    }
}

/// Answers for the client requests the scripts use. Each answer is checked against the
/// protocol type it should parse as.
fn default_answer(method: &str, params: &Value) -> Value {
    match method {
        "session/request_permission" => answer_permission(params, "allow_once"),
        "elicitation/create" => {
            if params["mode"] == "form" {
                json!({ "action": "accept", "content": { "branch": "main" } })
            } else {
                json!({ "action": "accept" })
            }
        }
        "terminal/create" => {
            let _: CreateTerminalRequest =
                serde_json::from_value(params.clone()).expect("typed terminal/create");
            json!({ "terminalId": "term_1" })
        }
        "terminal/wait_for_exit" => json!({ "exitCode": 0 }),
        "terminal/output" => json!({ "output": "test result: ok\n", "truncated": false }),
        "terminal/release" => Value::Null,
        other => panic!("no default answer for {other}"),
    }
}

/// Selects the first option whose kind is `kind` (for example `allow_once`).
fn answer_permission(params: &Value, kind: &str) -> Value {
    let request: RequestPermissionRequest =
        serde_json::from_value(params.clone()).expect("typed session/request_permission");
    let option = request
        .options
        .iter()
        .find(|option| serde_json::to_value(option.kind).ok() == Some(json!(kind)))
        .unwrap_or_else(|| panic!("no {kind} option in {params}"));
    let outcome = RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
        option.option_id.clone(),
    ));
    serde_json::to_value(RequestPermissionResponse::new(outcome)).expect("serialize")
}

fn text_of(block: &ContentBlock) -> &str {
    match block {
        ContentBlock::Text(text) => &text.text,
        other => panic!("expected text content, got {other:?}"),
    }
}

/// The text of every agent message chunk, in order.
fn message_texts(exchange: &Exchange) -> Vec<String> {
    exchange
        .updates
        .iter()
        .filter_map(|notification| match &notification.update {
            SessionUpdate::AgentMessageChunk(chunk) => Some(text_of(&chunk.content).to_owned()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn chunks_text_thought_and_end_turn() {
    let mut client = connect(json!({
        "turns": [{ "steps": [
            { "thought": "Thinking first." },
            { "text": "Hello" },
            { "text": " world" },
            { "stop": "end_turn" }
        ] }]
    }));
    let session = client.new_session().await;
    let exchange = client.prompt(&session, "hi").await;

    assert_eq!(exchange.prompt().stop_reason, StopReason::EndTurn);
    assert_eq!(message_texts(&exchange), ["Hello", " world"]);
    match &exchange.updates[0].update {
        SessionUpdate::AgentThoughtChunk(chunk) => {
            assert_eq!(text_of(&chunk.content), "Thinking first.");
        }
        other => panic!("expected a thought chunk first, got {other:?}"),
    }
    assert_eq!(exchange.updates[0].session_id.0.as_ref(), session);
}

#[tokio::test]
async fn tool_call_with_diff_content() {
    let mut client = connect(json!({
        "turns": [{ "steps": [
            { "tool_call": {
                "toolCallId": "call_1",
                "title": "Edit parser.rs",
                "kind": "edit",
                "status": "in_progress",
                "locations": [{ "path": "/work/parser.rs", "line": 4 }]
            } },
            { "tool_call_update": {
                "toolCallId": "call_1",
                "status": "completed",
                "content": [{ "type": "diff", "path": "/work/parser.rs",
                              "oldText": "old line", "newText": "new line" }]
            } }
        ] }]
    }));
    let session = client.new_session().await;
    let exchange = client.prompt(&session, "edit").await;

    assert_eq!(exchange.prompt().stop_reason, StopReason::EndTurn);
    let SessionUpdate::ToolCall(call) = &exchange.updates[0].update else {
        panic!("expected tool_call, got {:?}", exchange.updates[0].update);
    };
    assert_eq!(call.tool_call_id.0.as_ref(), "call_1");
    let SessionUpdate::ToolCallUpdate(update) = &exchange.updates[1].update else {
        panic!(
            "expected tool_call_update, got {:?}",
            exchange.updates[1].update
        );
    };
    assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
    let Some(contents) = &update.fields.content else {
        panic!("tool_call_update has no content");
    };
    match contents.as_slice() {
        [ToolCallContent::Diff(diff)] => {
            assert_eq!(diff.path.to_str(), Some("/work/parser.rs"));
            assert_eq!(diff.old_text.as_deref(), Some("old line"));
            assert_eq!(diff.new_text, "new line");
        }
        other => panic!("expected one diff, got {other:?}"),
    }
}

#[tokio::test]
async fn plan_usage_commands_and_mode() {
    let mut client = connect(json!({
        "turns": [{ "steps": [
            { "plan": [
                { "content": "Read the test", "priority": "high", "status": "completed" },
                { "content": "Fix it", "priority": "high", "status": "in_progress" }
            ] },
            { "usage": { "used": 5300, "size": 200000, "cost": { "amount": 0.04, "currency": "USD" } } },
            { "available_commands": [{ "name": "review", "description": "Review the diff" }] },
            { "current_mode": "code" }
        ] }]
    }));
    let session = client.new_session().await;
    let exchange = client.prompt(&session, "plan").await;

    assert_eq!(exchange.prompt().stop_reason, StopReason::EndTurn);
    assert_eq!(exchange.updates.len(), 4);
    match &exchange.updates[0].update {
        SessionUpdate::Plan(plan) => {
            assert_eq!(plan.entries.len(), 2);
            assert_eq!(plan.entries[1].content, "Fix it");
        }
        other => panic!("expected plan, got {other:?}"),
    }
    match &exchange.updates[1].update {
        SessionUpdate::UsageUpdate(usage) => {
            assert_eq!(usage.used, 5300);
            assert_eq!(usage.size, 200000);
        }
        other => panic!("expected usage_update, got {other:?}"),
    }
    match &exchange.updates[2].update {
        SessionUpdate::AvailableCommandsUpdate(commands) => {
            assert_eq!(commands.available_commands[0].name, "review");
        }
        other => panic!("expected available_commands_update, got {other:?}"),
    }
    match &exchange.updates[3].update {
        SessionUpdate::CurrentModeUpdate(mode) => {
            assert_eq!(mode.current_mode_id.0.as_ref(), "code");
        }
        other => panic!("expected current_mode_update, got {other:?}"),
    }
}

#[tokio::test]
async fn permission_allow_reject_and_mismatch() {
    let permission = |expect: &str| {
        json!({ "request_permission": {
            "toolCall": { "toolCallId": "call_2", "title": "Run cargo test", "kind": "execute" },
            "options": [
                { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" }
            ]
        }, "expect": expect })
    };
    let mut client = connect(json!({
        "turns": [
            { "steps": [ permission("allow"), { "text": "ran it" }, { "stop": "end_turn" } ] },
            { "steps": [ permission("reject"), { "text": "skipped" } ] },
            { "steps": [ permission("allow") ] }
        ]
    }));
    let session = client.new_session().await;

    // Allowed: the client picks allow_once and the turn continues.
    let allowed = client.prompt(&session, "run").await;
    assert_eq!(allowed.prompt().stop_reason, StopReason::EndTurn);
    assert_eq!(allowed.request_methods(), ["session/request_permission"]);
    let (_, params) = &allowed.requests[0];
    let request: RequestPermissionRequest =
        serde_json::from_value(params.clone()).expect("typed permission request");
    assert_eq!(request.session_id.0.as_ref(), session);
    assert_eq!(message_texts(&allowed), ["ran it"]);

    // Rejected: the client picks reject_once, which the turn expects, so the turn carries
    // on to its later steps.
    let mut reject = |method: &str, params: &Value| match method {
        "session/request_permission" => answer_permission(params, "reject_once"),
        other => default_answer(other, params),
    };
    let rejected = client
        .call(
            "session/prompt",
            json!({ "sessionId": session, "prompt": [] }),
            &mut reject,
        )
        .await;
    assert_eq!(rejected.prompt().stop_reason, StopReason::EndTurn);
    assert_eq!(message_texts(&rejected), ["skipped"]);

    // Mismatch: the script expects allow, the client rejects, so the prompt fails.
    let mismatch = client
        .call(
            "session/prompt",
            json!({ "sessionId": session, "prompt": [] }),
            &mut reject,
        )
        .await;
    assert!(
        mismatch.error()["message"]
            .as_str()
            .is_some_and(|message| message.contains("permission expected")),
        "unexpected error {}",
        mismatch.error()
    );
}

#[tokio::test]
async fn elicitation_form_and_url() {
    let mut client = connect(json!({
        "turns": [{ "steps": [
            { "elicitation": {
                "mode": "form",
                "message": "Which branch should the fix target?",
                "requestedSchema": {
                    "type": "object",
                    "properties": { "branch": { "type": "string", "title": "Branch name" } },
                    "required": ["branch"]
                }
            }, "expect": "accept" },
            { "elicitation": {
                "mode": "url",
                "message": "Sign in to continue",
                "elicitationId": "login_1",
                "url": "https://auth.example.test/start"
            }, "expect": "accept" },
            { "text": "both answered" }
        ] }]
    }));
    let session = client.new_session().await;
    let exchange = client.prompt(&session, "login").await;

    assert_eq!(exchange.prompt().stop_reason, StopReason::EndTurn);
    assert_eq!(
        exchange.request_methods(),
        ["elicitation/create", "elicitation/create"]
    );
    let form: CreateElicitationRequest =
        serde_json::from_value(exchange.requests[0].1.clone()).expect("typed form elicitation");
    assert_eq!(
        serde_json::to_value(&form).expect("serialize")["mode"],
        "form"
    );
    let url: CreateElicitationRequest =
        serde_json::from_value(exchange.requests[1].1.clone()).expect("typed url elicitation");
    assert_eq!(
        serde_json::to_value(&url).expect("serialize")["mode"],
        "url"
    );
    assert_eq!(message_texts(&exchange), ["both answered"]);
}

#[tokio::test]
async fn terminal_via_client() {
    let mut client = connect(json!({
        "turns": [{ "steps": [
            { "terminal": { "command": "cargo test -p parser", "toolCallId": "call_t" } },
            { "stop": "end_turn" }
        ] }]
    }));
    let session = client.new_session().await;
    let exchange = client.prompt(&session, "test").await;

    assert_eq!(exchange.prompt().stop_reason, StopReason::EndTurn);
    assert_eq!(
        exchange.request_methods(),
        [
            "terminal/create",
            "terminal/wait_for_exit",
            "terminal/output",
            "terminal/release"
        ]
    );
    let create: CreateTerminalRequest =
        serde_json::from_value(exchange.requests[0].1.clone()).expect("typed create");
    assert_eq!(
        serde_json::to_value(&create).expect("serialize")["command"],
        "cargo test -p parser"
    );
    let _: WaitForTerminalExitRequest =
        serde_json::from_value(exchange.requests[1].1.clone()).expect("typed wait");
    let _: TerminalOutputRequest =
        serde_json::from_value(exchange.requests[2].1.clone()).expect("typed output");

    // The terminal is reported as a tool call that carries the terminal id, then the
    // captured output once it has exited.
    let SessionUpdate::ToolCall(call) = &exchange.updates[0].update else {
        panic!(
            "expected tool_call first, got {:?}",
            exchange.updates[0].update
        );
    };
    assert_eq!(call.tool_call_id.0.as_ref(), "call_t");
    let SessionUpdate::ToolCallUpdate(attached) = &exchange.updates[1].update else {
        panic!(
            "expected tool_call_update, got {:?}",
            exchange.updates[1].update
        );
    };
    match attached.fields.content.as_deref() {
        Some([ToolCallContent::Terminal(terminal)]) => {
            assert_eq!(terminal.terminal_id.0.as_ref(), "term_1");
        }
        other => panic!("expected a terminal content block, got {other:?}"),
    }
    let SessionUpdate::ToolCallUpdate(finished) = &exchange.updates[2].update else {
        panic!(
            "expected final tool_call_update, got {:?}",
            exchange.updates[2].update
        );
    };
    assert_eq!(finished.fields.status, Some(ToolCallStatus::Completed));
    match finished.fields.content.as_deref() {
        Some(
            [
                ToolCallContent::Content(output),
                ToolCallContent::Terminal(_),
            ],
        ) => {
            assert_eq!(text_of(&output.content), "test result: ok\n");
        }
        other => panic!("expected output then terminal, got {other:?}"),
    }
}

#[tokio::test]
async fn multi_session_and_resume() {
    let mut client = connect(json!({
        "turns": [
            { "steps": [ { "text": "one" } ] },
            { "steps": [ { "text": "two" } ] },
            { "steps": [ { "text": "three" } ] }
        ]
    }));
    let first = client.new_session().await;
    let second = client.new_session().await;
    assert_ne!(first, second);

    // Each session counts its own prompts, so both start at turn 1.
    assert_eq!(message_texts(&client.prompt(&first, "a").await), ["one"]);
    assert_eq!(message_texts(&client.prompt(&second, "b").await), ["one"]);
    assert_eq!(message_texts(&client.prompt(&first, "a").await), ["two"]);

    // Resuming keeps the prompt count, so the next prompt runs turn 3.
    let resumed = client
        .call(
            "session/resume",
            json!({ "sessionId": first, "cwd": "/work", "mcpServers": [] }),
            &mut default_answer,
        )
        .await;
    assert!(
        resumed.response.get("error").is_none(),
        "resume failed: {}",
        resumed.response
    );
    assert_eq!(message_texts(&client.prompt(&first, "a").await), ["three"]);

    // Past the end of the script, the prompt fails rather than inventing a turn.
    let past_end = client.prompt(&first, "a").await;
    assert!(
        past_end.response.get("error").is_some(),
        "expected an error past the script"
    );
    // The other session still has its own turn 2.
    assert_eq!(message_texts(&client.prompt(&second, "b").await), ["two"]);
}

#[tokio::test]
async fn cancel_during_permission_ends_cancelled() {
    let mut client = connect(json!({
        "turns": [{ "steps": [
            { "text": "before" },
            { "request_permission": {
                "toolCall": { "toolCallId": "call_3", "title": "Delete files", "kind": "delete" },
                "options": [{ "optionId": "allow_once", "name": "Allow", "kind": "allow_once" }]
            }, "expect": "cancelled" },
            { "text": "after" },
            { "stop": "end_turn" }
        ] }, { "steps": [ { "text": "next" } ] }]
    }));
    let session = client.new_session().await;

    let prompt_id = client.next_id;
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": prompt_id,
            "method": "session/prompt",
            "params": { "sessionId": session, "prompt": [] },
        }))
        .await;
    client.next_id += 1;

    // Wait for the permission request, then cancel while the turn is blocked on it.
    let mut updates = Vec::new();
    let permission_id = loop {
        let message = client.recv().await;
        match message.get("method").and_then(Value::as_str) {
            Some("session/update") => updates.push(
                serde_json::from_value::<SessionNotification>(message["params"].clone())
                    .expect("typed update"),
            ),
            Some("session/request_permission") => {
                let _: RequestPermissionRequest =
                    serde_json::from_value(message["params"].clone()).expect("typed request");
                break message["id"].clone();
            }
            other => panic!("unexpected message before permission: {other:?} {message}"),
        }
    };
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": session },
        }))
        .await;

    // A client must answer a pending permission with cancelled.
    let cancelled = json!({ "outcome": { "outcome": "cancelled" } });
    client
        .send(&json!({ "jsonrpc": "2.0", "id": permission_id, "result": cancelled }))
        .await;

    let response = loop {
        let message = client.recv().await;
        match message.get("method").and_then(Value::as_str) {
            Some("session/update") => updates.push(
                serde_json::from_value::<SessionNotification>(message["params"].clone())
                    .expect("typed update"),
            ),
            Some(method) => panic!("unexpected request {method} after cancel"),
            None => break message,
        }
    };
    assert_eq!(response["id"], json!(prompt_id));
    let prompt: PromptResponse =
        serde_json::from_value(response["result"].clone()).expect("typed prompt response");
    assert_eq!(prompt.stop_reason, StopReason::Cancelled);
    let texts: Vec<String> = updates
        .iter()
        .filter_map(|notification| match &notification.update {
            SessionUpdate::AgentMessageChunk(chunk) => Some(text_of(&chunk.content).to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["before"], "steps after the permission must not run");

    // The session is usable again: the next prompt runs its own turn.
    assert_eq!(
        message_texts(&client.prompt(&session, "again").await),
        ["next"]
    );
}

#[tokio::test]
async fn leak_secret_emits_fake_key() {
    let mut client = connect(json!({
        "turns": [{ "steps": [ { "leak_secret": true }, { "stop": "end_turn" } ] }]
    }));
    let session = client.new_session().await;
    let exchange = client.prompt(&session, "debug").await;

    assert_eq!(exchange.prompt().stop_reason, StopReason::EndTurn);
    let texts = message_texts(&exchange);
    assert_eq!(texts.len(), 1);
    assert!(
        texts[0].contains(FAKE_API_KEY),
        "missing fake key in {texts:?}"
    );
    assert!(FAKE_API_KEY.starts_with("sk-ant-api03-FAKE"));
}

#[test]
fn invalid_scripts_are_rejected_at_load() {
    let cases = [
        // A tool call needs its id.
        json!({ "turns": [{ "steps": [ { "tool_call": { "title": "t" } } ] }] }),
        // Two step keys in one step.
        json!({ "turns": [{ "steps": [ { "text": "a", "thought": "b" } ] }] }),
        // Stop must be last.
        json!({ "turns": [{ "steps": [ { "stop": "end_turn" }, { "text": "late" } ] }] }),
        // The expect value must be an option id or a known category.
        json!({ "turns": [{ "steps": [ { "request_permission": {
            "toolCall": { "toolCallId": "c", "title": "t" },
            "options": [{ "optionId": "allow_once", "name": "Allow", "kind": "allow_once" }]
        }, "expect": "maybe" } ] }] }),
    ];
    for case in cases {
        assert!(
            Script::from_value(&case).is_err(),
            "accepted invalid script {case}"
        );
    }
}

#[tokio::test]
async fn binary_completes_prompt_over_stdio() {
    use std::process::Stdio;

    let script_path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("stdio-script.json");
    std::fs::write(
        &script_path,
        json!({ "turns": [{ "steps": [ { "text": "over stdio" }, { "stop": "end_turn" } ] }] })
            .to_string(),
    )
    .expect("write script");

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_acp-mock"))
        .arg("--script")
        .arg(&script_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn acp-mock");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();

    let messages = [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": 1 } }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new",
                "params": { "cwd": "/work", "mcpServers": [] } }),
    ];
    for message in messages {
        stdin
            .write_all(format!("{message}\n").as_bytes())
            .await
            .expect("write");
    }
    let session = read_result(&mut lines, 2).await["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    let prompt = json!({ "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                         "params": { "sessionId": session, "prompt": [] } });
    stdin
        .write_all(format!("{prompt}\n").as_bytes())
        .await
        .expect("write");

    let mut texts = Vec::new();
    loop {
        let line = timeout(IO_TIMEOUT, lines.next_line())
            .await
            .expect("binary went quiet")
            .expect("read")
            .expect("binary closed stdout");
        let message: Value = serde_json::from_str(&line).expect("JSON line");
        if message.get("method") == Some(&json!("session/update")) {
            let notification: SessionNotification =
                serde_json::from_value(message["params"].clone()).expect("typed update");
            if let SessionUpdate::AgentMessageChunk(chunk) = notification.update {
                texts.push(text_of(&chunk.content).to_owned());
            }
        } else if message["id"] == json!(3) {
            let prompt: PromptResponse =
                serde_json::from_value(message["result"].clone()).expect("typed prompt");
            assert_eq!(prompt.stop_reason, StopReason::EndTurn);
            break;
        }
    }
    assert_eq!(texts, ["over stdio"]);

    // Closing stdin ends the agent cleanly.
    drop(stdin);
    let status = timeout(IO_TIMEOUT, child.wait())
        .await
        .expect("binary did not exit")
        .expect("wait");
    assert!(status.success(), "binary exited with {status}");
}

/// Reads stdout lines until the response with `id` and returns its result.
async fn read_result(lines: &mut Lines<BufReader<tokio::process::ChildStdout>>, id: u64) -> Value {
    loop {
        let line = timeout(IO_TIMEOUT, lines.next_line())
            .await
            .expect("binary went quiet")
            .expect("read")
            .expect("binary closed stdout");
        let message: Value = serde_json::from_str(&line).expect("JSON line");
        if message["id"] == json!(id) {
            return message["result"].clone();
        }
    }
}
