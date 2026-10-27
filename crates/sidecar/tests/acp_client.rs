//! Tests for the ACP client: capabilities, streaming, the session table and its pending
//! tasks, cancellation, routing of the agent's requests, and agent subprocesses.
//!
//! Most tests run `acp-mock` in process over a duplex pipe. The cancel test plays the
//! agent by hand, so it can read the answer the client sends to a permission request.
//! The subprocess tests run `acp-mock` over real stdio: the test binary re-runs itself as
//! the agent (see [`subprocess_agent_child`]).

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acp_mock::Script;
use protocol::acp::{
    ContentBlock, CreateElicitationRequest, CreateElicitationResponse, CreateTerminalRequest,
    CreateTerminalResponse, KillTerminalRequest, KillTerminalResponse, PermissionOptionKind,
    ReadTextFileRequest, ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionId, SessionNotification, SessionUpdate, StopReason,
    TerminalOutputRequest, TerminalOutputResponse, TextContent, WaitForTerminalExitRequest,
    WaitForTerminalExitResponse, WriteTextFileRequest, WriteTextFileResponse,
};
use serde_json::{Value, json};
use sidecar::acp::client::{AcpClient, ClientError, ClientHandler, client_capabilities};
use sidecar::acp::sessions::TaskId;
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::timeout;

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const CHILD_SCRIPT_ENV: &str = "SIDECAR_TEST_ACP_SCRIPT";

/// A client handler that records what the agent asked for. Permission requests wait until
/// `gate` is `true`, so a test decides when the agent's turn can go on.
#[derive(Clone)]
struct Handler {
    gate: watch::Receiver<bool>,
    /// Notified each time a permission request arrives.
    entered: Arc<Notify>,
    /// The methods the agent called, in arrival order.
    log: Arc<Mutex<Vec<String>>>,
    /// Every permission request, in arrival order.
    permissions: Arc<Mutex<Vec<RequestPermissionRequest>>>,
}

impl Handler {
    /// A handler that answers permission requests at once.
    fn open() -> Self {
        let (_, gate) = watch::channel(true);
        Self::with_gate(gate)
    }

    fn with_gate(gate: watch::Receiver<bool>) -> Self {
        Self {
            gate,
            entered: Arc::new(Notify::new()),
            log: Arc::default(),
            permissions: Arc::default(),
        }
    }

    fn record(&self, method: &str) {
        self.log.lock().expect("log").push(method.to_owned());
    }

    fn methods(&self) -> Vec<String> {
        self.log.lock().expect("log").clone()
    }
}

/// Picks the first allow option, as a user would.
fn allow_once(request: &RequestPermissionRequest) -> RequestPermissionResponse {
    let option = request
        .options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::AllowOnce)
        .expect("an allow_once option");
    RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
        SelectedPermissionOutcome::new(option.option_id.clone()),
    ))
}

impl ClientHandler for Handler {
    async fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> anyhow::Result<RequestPermissionResponse> {
        self.record("session/request_permission");
        let answer = allow_once(&request);
        self.permissions.lock().expect("permissions").push(request);
        self.entered.notify_one();
        let mut gate = self.gate.clone();
        gate.wait_for(|open| *open).await?;
        Ok(answer)
    }

    async fn create_elicitation(
        &self,
        _request: CreateElicitationRequest,
    ) -> anyhow::Result<CreateElicitationResponse> {
        self.record("elicitation/create");
        Ok(serde_json::from_value(json!({
            "action": "accept",
            "content": { "branch": "main" }
        }))?)
    }

    async fn read_text_file(
        &self,
        request: ReadTextFileRequest,
    ) -> anyhow::Result<ReadTextFileResponse> {
        self.record("fs/read_text_file");
        Ok(ReadTextFileResponse::new(format!(
            "contents of {}",
            request.path.display()
        )))
    }

    async fn write_text_file(
        &self,
        _request: WriteTextFileRequest,
    ) -> anyhow::Result<WriteTextFileResponse> {
        self.record("fs/write_text_file");
        Ok(WriteTextFileResponse::new())
    }

    async fn create_terminal(
        &self,
        _request: CreateTerminalRequest,
    ) -> anyhow::Result<CreateTerminalResponse> {
        self.record("terminal/create");
        Ok(CreateTerminalResponse::new("term_1"))
    }

    async fn terminal_output(
        &self,
        _request: TerminalOutputRequest,
    ) -> anyhow::Result<TerminalOutputResponse> {
        self.record("terminal/output");
        Ok(TerminalOutputResponse::new("test result: ok\n", false))
    }

    async fn wait_for_terminal_exit(
        &self,
        _request: WaitForTerminalExitRequest,
    ) -> anyhow::Result<WaitForTerminalExitResponse> {
        self.record("terminal/wait_for_exit");
        Ok(serde_json::from_value(json!({ "exitCode": 0 }))?)
    }

    async fn kill_terminal(
        &self,
        _request: KillTerminalRequest,
    ) -> anyhow::Result<KillTerminalResponse> {
        self.record("terminal/kill");
        Ok(KillTerminalResponse::new())
    }

    async fn release_terminal(
        &self,
        _request: ReleaseTerminalRequest,
    ) -> anyhow::Result<ReleaseTerminalResponse> {
        self.record("terminal/release");
        Ok(ReleaseTerminalResponse::new())
    }
}

type UpdateRx = mpsc::UnboundedReceiver<SessionNotification>;

/// Starts `acp-mock` in process with `script`, connects a client to it and initializes.
async fn start(script: Value, handler: Handler) -> (AcpClient<Handler>, UpdateRx) {
    let script = Script::from_value(&script).expect("valid script");
    let (client_io, agent_io) = tokio::io::duplex(1 << 16);
    let (agent_reader, agent_writer) = tokio::io::split(agent_io);
    tokio::spawn(async move { acp_mock::run(script, agent_reader, agent_writer).await });
    let (reader, writer) = tokio::io::split(client_io);
    let (updates, rx) = mpsc::unbounded_channel();
    let client = AcpClient::connect(reader, writer, handler, updates);
    client.initialize().await.expect("initialize");
    (client, rx)
}

async fn new_session(client: &AcpClient<Handler>) -> SessionId {
    client
        .new_session("/work", Vec::new())
        .await
        .expect("session/new")
}

fn text(content: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(TextContent::new(content))]
}

/// Every update received so far.
fn drain(updates: &mut UpdateRx) -> Vec<SessionNotification> {
    let mut received = Vec::new();
    while let Ok(update) = updates.try_recv() {
        received.push(update);
    }
    received
}

/// The text of every agent message chunk in `updates`, in order.
fn message_texts(updates: &[SessionNotification]) -> Vec<String> {
    updates
        .iter()
        .filter_map(|notification| match &notification.update {
            SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// A line reader and a line writer on the agent's side of a duplex pipe, for tests that
/// play the agent by hand.
struct FakeAgent {
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    writer: WriteHalf<DuplexStream>,
}

impl FakeAgent {
    async fn read(&mut self) -> Value {
        let line = timeout(IO_TIMEOUT, self.lines.next_line())
            .await
            .expect("the client went quiet")
            .expect("read from the client")
            .expect("the client closed the connection");
        serde_json::from_str(&line).expect("the client wrote JSON")
    }

    async fn write(&mut self, message: Value) {
        let mut bytes = serde_json::to_vec(&message).expect("serialize");
        bytes.push(b'\n');
        self.writer
            .write_all(&bytes)
            .await
            .expect("write to client");
        self.writer.flush().await.expect("flush to client");
    }
}

/// A client connected to a [`FakeAgent`], with the agent's end returned to the test.
fn connect_fake(handler: Handler) -> (AcpClient<Handler>, UpdateRx, FakeAgent) {
    let (client_io, agent_io) = tokio::io::duplex(1 << 16);
    let (client_reader, client_writer) = tokio::io::split(client_io);
    let (agent_reader, agent_writer) = tokio::io::split(agent_io);
    let (updates, rx) = mpsc::unbounded_channel();
    let client = AcpClient::connect(client_reader, client_writer, handler, updates);
    let agent = FakeAgent {
        lines: BufReader::new(agent_reader).lines(),
        writer: agent_writer,
    };
    (client, rx, agent)
}

#[tokio::test]
async fn initialize_advertises_fs_terminal_and_elicitation() {
    let (client_io, agent_io) = tokio::io::duplex(1 << 16);
    let (client_reader, client_writer) = tokio::io::split(client_io);
    let (agent_reader, agent_writer) = tokio::io::split(agent_io);
    let (updates, _rx) = mpsc::unbounded_channel();
    let client = AcpClient::connect(client_reader, client_writer, Handler::open(), updates);
    let mut agent = FakeAgent {
        lines: BufReader::new(agent_reader).lines(),
        writer: agent_writer,
    };

    let (response, request) = tokio::join!(client.initialize(), async {
        let request = agent.read().await;
        agent
            .write(json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": { "protocolVersion": 1 }
            }))
            .await;
        request
    });
    response.expect("initialize");

    assert_eq!(request["method"], "initialize");
    let capabilities = &request["params"]["clientCapabilities"];
    assert_eq!(capabilities["fs"]["readTextFile"], true);
    assert_eq!(capabilities["fs"]["writeTextFile"], true);
    assert_eq!(capabilities["terminal"], true);
    assert!(capabilities["elicitation"]["form"].is_object());
    assert!(capabilities["elicitation"]["url"].is_object());
    // The same capabilities the runner documents.
    assert_eq!(
        serde_json::to_value(client_capabilities()).expect("serialize"),
        *capabilities
    );
}

#[tokio::test]
async fn prompt_streams_updates_and_returns_stop_reason() {
    let (client, mut updates) = start(
        json!({ "turns": [{ "steps": [
            { "thought": "Thinking first." },
            { "text": "Hello" },
            { "text": " world" },
            { "stop": "end_turn" }
        ] }] }),
        Handler::open(),
    )
    .await;
    let session = new_session(&client).await;

    let stop = client.prompt(&session, text("hi")).await.expect("prompt");

    assert_eq!(stop, StopReason::EndTurn);
    let received = drain(&mut updates);
    assert_eq!(message_texts(&received), ["Hello", " world"]);
    assert!(matches!(
        received[0].update,
        SessionUpdate::AgentThoughtChunk(_)
    ));
    assert!(received.iter().all(|note| note.session_id.0 == session.0));
}

#[tokio::test]
async fn concurrent_sessions_on_one_agent_stay_separate() {
    let (client, mut updates) = start(
        json!({ "turns": [
            { "steps": [{ "text": "first" }] },
            { "steps": [{ "text": "second" }] }
        ] }),
        Handler::open(),
    )
    .await;
    let a = new_session(&client).await;
    let b = new_session(&client).await;
    let c = new_session(&client).await;
    assert_ne!(a.0, b.0);
    assert_ne!(b.0, c.0);

    for round in 0..2 {
        let (ra, rb, rc) = tokio::join!(
            client.prompt(&a, text("go")),
            client.prompt(&b, text("go")),
            client.prompt(&c, text("go")),
        );
        assert_eq!(ra.expect("a"), StopReason::EndTurn, "round {round}");
        assert_eq!(rb.expect("b"), StopReason::EndTurn, "round {round}");
        assert_eq!(rc.expect("c"), StopReason::EndTurn, "round {round}");
    }

    // Each session saw its own turns in order, tagged with its own id.
    let received = drain(&mut updates);
    for session in [&a, &b, &c] {
        let mine: Vec<SessionNotification> = received
            .iter()
            .filter(|note| note.session_id.0 == session.0)
            .cloned()
            .collect();
        assert_eq!(message_texts(&mine), ["first", "second"]);
    }
    assert_eq!(received.len(), 6);
}

#[tokio::test]
async fn second_prompt_queues_as_pending_task_and_runs_after() {
    let (gate_tx, gate) = watch::channel(false);
    let handler = Handler::with_gate(gate);
    let entered = Arc::clone(&handler.entered);
    let (client, mut updates) = start(
        json!({ "turns": [
            { "steps": [
                { "request_permission": {
                    "toolCall": { "toolCallId": "call_1", "title": "Run tests", "kind": "execute" },
                    "options": [
                        { "optionId": "allow_once", "name": "Allow", "kind": "allow_once" },
                        { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" }
                    ]
                }, "expect": "allow" },
                { "text": "A" }
            ] },
            { "steps": [{ "text": "B" }] },
            { "steps": [{ "text": "D" }] }
        ] }),
        handler,
    )
    .await;
    let session = new_session(&client).await;

    // A is blocked on its permission request until the gate opens.
    let a = client.submit(&session, text("a")).expect("submit a");
    assert!(!a.queued);
    entered.notified().await;
    assert_eq!(
        client.running_task(&session).expect("running"),
        Some(a.task)
    );

    let b = client.submit(&session, text("b")).expect("submit b");
    let c = client.submit(&session, text("c")).expect("submit c");
    let d = client.submit(&session, text("d")).expect("submit d");
    assert!(b.queued && c.queued && d.queued);

    let listed: Vec<TaskId> = client
        .pending_tasks(&session)
        .expect("list")
        .iter()
        .map(|task| task.id)
        .collect();
    assert_eq!(listed, [b.task, c.task, d.task]);

    // Removing C takes it out of the queue and fails its result.
    let removed = client
        .remove_pending(&session, c.task)
        .expect("remove")
        .expect("c was pending");
    assert_eq!(removed.id, c.task);
    assert_eq!(removed.prompt.len(), 1);
    let listed: Vec<TaskId> = client
        .pending_tasks(&session)
        .expect("list")
        .iter()
        .map(|task| task.id)
        .collect();
    assert_eq!(listed, [b.task, d.task]);
    assert!(matches!(c.done.await, Ok(Err(ClientError::Removed))));
    assert!(
        client
            .remove_pending(&session, c.task)
            .expect("remove again")
            .is_none()
    );

    // While A is in flight, B has not reached the agent: no B text has arrived.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(message_texts(&drain(&mut updates)).is_empty());

    gate_tx.send(true).expect("open gate");
    assert_eq!(
        a.done.await.expect("a result").expect("a ok"),
        StopReason::EndTurn
    );
    assert_eq!(
        b.done.await.expect("b result").expect("b ok"),
        StopReason::EndTurn
    );
    assert_eq!(
        d.done.await.expect("d result").expect("d ok"),
        StopReason::EndTurn
    );

    let received = drain(&mut updates);
    assert_eq!(message_texts(&received), ["A", "B", "D"]);
    assert!(client.pending_tasks(&session).expect("list").is_empty());
    assert_eq!(client.running_task(&session).expect("running"), None);
}

#[tokio::test]
async fn cancel_ends_prompt_and_answers_pending_permission_cancelled() {
    // The gate never opens, so the client's own answer is the only one the agent can get.
    let (_gate_tx, gate) = watch::channel(false);
    let handler = Handler::with_gate(gate);
    let (client, _updates, mut agent) = connect_fake(handler.clone());

    let (session, ()) = tokio::join!(client.new_session("/work", Vec::new()), async {
        let request = agent.read().await;
        assert_eq!(request["method"], "session/new");
        agent
            .write(
                json!({ "jsonrpc": "2.0", "id": request["id"], "result": { "sessionId": "s1" } }),
            )
            .await;
    });
    let session = session.expect("session/new");

    let running = client.submit(&session, text("edit")).expect("submit");
    let prompt = agent.read().await;
    assert_eq!(prompt["method"], "session/prompt");
    let queued = client.submit(&session, text("next")).expect("submit next");
    assert!(queued.queued);

    // The agent asks for permission and waits for an answer.
    agent
        .write(json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "session/request_permission",
            "params": {
                "sessionId": "s1",
                "toolCall": { "toolCallId": "call_1", "title": "Run tests" },
                "options": [
                    { "optionId": "allow_once", "name": "Allow", "kind": "allow_once" },
                    { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" }
                ]
            }
        }))
        .await;
    handler.entered.notified().await;

    client.cancel(&session).await.expect("cancel");

    // The agent receives the cancel and the client's answer to its permission request.
    let mut cancel_seen = false;
    let mut answer = None;
    while !(cancel_seen && answer.is_some()) {
        let message = agent.read().await;
        match message["method"].as_str() {
            Some("session/cancel") => {
                assert_eq!(message["params"]["sessionId"], "s1");
                cancel_seen = true;
            }
            Some(other) => panic!("unexpected {other} from the client"),
            None => {
                assert_eq!(message["id"], 100);
                answer = Some(message["result"]["outcome"].clone());
            }
        }
    }
    assert_eq!(answer, Some(json!({ "outcome": "cancelled" })));

    // The agent ends the cancelled prompt. The queued task then runs.
    agent
        .write(json!({
            "jsonrpc": "2.0",
            "id": prompt["id"],
            "result": { "stopReason": "cancelled" }
        }))
        .await;
    assert_eq!(
        running
            .done
            .await
            .expect("running result")
            .expect("running ok"),
        StopReason::Cancelled
    );

    let next = agent.read().await;
    assert_eq!(next["method"], "session/prompt");
    agent
        .write(
            json!({ "jsonrpc": "2.0", "id": next["id"], "result": { "stopReason": "end_turn" } }),
        )
        .await;
    assert_eq!(
        queued
            .done
            .await
            .expect("queued result")
            .expect("queued ok"),
        StopReason::EndTurn
    );
    assert_eq!(handler.methods(), ["session/request_permission"]);
}

#[tokio::test]
async fn agent_requests_route_to_the_handler() {
    let handler = Handler::open();
    let (client, mut updates) = start(
        json!({ "turns": [{ "steps": [
            { "request_permission": {
                "toolCall": { "toolCallId": "call_2", "title": "Run cargo test", "kind": "execute" },
                "options": [
                    { "optionId": "allow_once", "name": "Allow", "kind": "allow_once" },
                    { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" }
                ]
            }, "expect": "allow" },
            { "elicitation": { "mode": "form", "message": "Which branch?",
                "requestedSchema": { "type": "object", "properties": {} } },
              "expect": "accept" },
            { "terminal": { "command": "cargo test", "toolCallId": "call_3" } },
            { "text": "done" },
            { "stop": "end_turn" }
        ] }] }),
        handler.clone(),
    )
    .await;
    let session = new_session(&client).await;

    let stop = client.prompt(&session, text("run")).await.expect("prompt");

    assert_eq!(stop, StopReason::EndTurn);
    assert_eq!(
        handler.methods(),
        [
            "session/request_permission",
            "elicitation/create",
            "terminal/create",
            "terminal/wait_for_exit",
            "terminal/output",
            "terminal/release",
        ]
    );
    let permissions = handler.permissions.lock().expect("permissions").clone();
    assert_eq!(permissions.len(), 1);
    assert_eq!(permissions[0].session_id.0, session.0);
    assert_eq!(message_texts(&drain(&mut updates)), ["done"]);
}

#[tokio::test]
async fn subprocess_is_reaped_on_drop() {
    let mut command = tokio::process::Command::new("sleep");
    // A null stderr keeps a leaked child from holding the test's output pipe open.
    command.arg("600").stderr(Stdio::null());
    let (updates, _rx) = mpsc::unbounded_channel();
    let client = AcpClient::spawn(command, Handler::open(), updates).expect("spawn");
    let pid = client.child_id().expect("a spawned child has a pid");
    let proc_entry = format!("/proc/{pid}");
    assert!(Path::new(&proc_entry).exists(), "the child runs");

    drop(client);

    // kill_on_drop kills the child, and tokio reaps it, so its /proc entry goes away.
    let started = std::time::Instant::now();
    while Path::new(&proc_entry).exists() {
        assert!(
            started.elapsed() < IO_TIMEOUT,
            "child {pid} was not killed and reaped after the client was dropped"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn subprocess_agent_serves_acp_over_stdio() {
    let script =
        json!({ "turns": [{ "steps": [{ "text": "over stdio" }, { "stop": "end_turn" }] }] });
    let mut command = tokio::process::Command::new(std::env::current_exe().expect("test binary"));
    command
        .args([
            "--ignored",
            "--exact",
            "subprocess_agent_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_SCRIPT_ENV, script.to_string())
        .stderr(Stdio::null());
    let (updates, mut rx) = mpsc::unbounded_channel();
    let client = AcpClient::spawn(command, Handler::open(), updates).expect("spawn");

    client.initialize().await.expect("initialize");
    let session = new_session(&client).await;
    let stop = client.prompt(&session, text("hi")).await.expect("prompt");

    assert_eq!(stop, StopReason::EndTurn);
    assert_eq!(message_texts(&drain(&mut rx)), ["over stdio"]);
}

/// The agent for [`subprocess_agent_serves_acp_over_stdio`]. It is not a test: it runs only
/// when the parent test re-runs this binary with the script in `SIDECAR_TEST_ACP_SCRIPT`.
#[tokio::test]
#[ignore = "run by subprocess_agent_serves_acp_over_stdio"]
async fn subprocess_agent_child() {
    let Ok(script) = std::env::var(CHILD_SCRIPT_ENV) else {
        return;
    };
    let script = Script::from_json(&script).expect("valid script");
    // The test harness has printed a line without its newline. End that line so the first
    // JSON message starts on its own line.
    println!();
    acp_mock::run(script, tokio::io::stdin(), tokio::io::stdout())
        .await
        .expect("agent serves until stdin closes");
}
