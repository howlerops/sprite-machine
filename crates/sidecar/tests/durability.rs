//! Durability tests: the store survives a restart, the next message for a known thread
//! re-attaches its ACP session before the prompt, and an interrupt that waits past its
//! approval TTL fails the task with `expired`.
//!
//! The re-attach tests that use a real agent spawn the `acp-mock` binary (built on demand with
//! `cargo build -p acp-mock`) and kill it between turns by dropping the client.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::anyhow;
use protocol::acp::{
    ContentBlock, CreateElicitationRequest, CreateElicitationResponse, CreateTerminalRequest,
    CreateTerminalResponse, KillTerminalRequest, KillTerminalResponse, ReadTextFileRequest,
    ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
    RequestPermissionRequest, RequestPermissionResponse, StopReason, TerminalOutputRequest,
    TerminalOutputResponse, TextContent, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
    WriteTextFileRequest, WriteTextFileResponse,
};
use protocol::mapping::acp_turn::{AcpTurnMapper, StartTask};
use protocol::tunnel::{Disposition, IDEMPOTENCY_KEY_TTL_MS};
use serde_json::{Value, json};
use sidecar::acp::client::{AcpClient, ClientHandler};
use sidecar::acp::reattach::{
    Attach, ReattachError, Reattacher, expire_overdue_interrupts, rfc3339,
};
use sidecar::store::{
    CURRENT_VERSION, Clock, InterruptKind, ManualClock, PendingInterrupt, SessionState, Store,
    StoreError,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

const IO_TIMEOUT: Duration = Duration::from_secs(20);
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

// ---- helpers --------------------------------------------------------------------------

/// A scratch dir removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "sidecar-durability-{label}-{}-{n}",
            std::process::id()
        ));
        // Leftovers from a crashed earlier run with the same pid are removed first.
        std::fs::remove_dir_all(&path).ok();
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn open_store(dir: &TempDir, clock: &Arc<ManualClock>) -> Store {
    // Re-open inside a sub-dir that does not exist yet, so the 0700 creation is exercised.
    let clock: Arc<dyn Clock> = clock.clone();
    Store::open_with_clock(&dir.path().join("state"), clock).expect("open store")
}

/// A client handler for tests that never expect the agent to call back.
#[derive(Clone)]
struct NoCallbacks;

impl ClientHandler for NoCallbacks {
    async fn request_permission(
        &self,
        _: RequestPermissionRequest,
    ) -> anyhow::Result<RequestPermissionResponse> {
        Err(anyhow!("unexpected request_permission"))
    }
    async fn create_elicitation(
        &self,
        _: CreateElicitationRequest,
    ) -> anyhow::Result<CreateElicitationResponse> {
        Err(anyhow!("unexpected create_elicitation"))
    }
    async fn read_text_file(&self, _: ReadTextFileRequest) -> anyhow::Result<ReadTextFileResponse> {
        Err(anyhow!("unexpected read_text_file"))
    }
    async fn write_text_file(
        &self,
        _: WriteTextFileRequest,
    ) -> anyhow::Result<WriteTextFileResponse> {
        Err(anyhow!("unexpected write_text_file"))
    }
    async fn create_terminal(
        &self,
        _: CreateTerminalRequest,
    ) -> anyhow::Result<CreateTerminalResponse> {
        Err(anyhow!("unexpected create_terminal"))
    }
    async fn terminal_output(
        &self,
        _: TerminalOutputRequest,
    ) -> anyhow::Result<TerminalOutputResponse> {
        Err(anyhow!("unexpected terminal_output"))
    }
    async fn wait_for_terminal_exit(
        &self,
        _: WaitForTerminalExitRequest,
    ) -> anyhow::Result<WaitForTerminalExitResponse> {
        Err(anyhow!("unexpected wait_for_terminal_exit"))
    }
    async fn kill_terminal(&self, _: KillTerminalRequest) -> anyhow::Result<KillTerminalResponse> {
        Err(anyhow!("unexpected kill_terminal"))
    }
    async fn release_terminal(
        &self,
        _: ReleaseTerminalRequest,
    ) -> anyhow::Result<ReleaseTerminalResponse> {
        Err(anyhow!("unexpected release_terminal"))
    }
}

fn text(prompt: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(TextContent::new(prompt))]
}

/// Builds `acp-mock` once and returns the path of the binary.
fn acp_mock_binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = std::process::Command::new(cargo)
            .args(["build", "-p", "acp-mock", "--bin", "acp-mock"])
            .status()
            .expect("run cargo build -p acp-mock");
        assert!(status.success(), "cargo build -p acp-mock failed");
        // <target>/debug/deps/durability-<hash> -> <target>/debug
        let exe = std::env::current_exe().expect("test binary path");
        let profile_dir = exe
            .parent()
            .and_then(Path::parent)
            .expect("test binary sits in <target>/<profile>/deps");
        let name = if cfg!(windows) {
            "acp-mock.exe"
        } else {
            "acp-mock"
        };
        let binary = profile_dir.join(name);
        assert!(binary.exists(), "{} was not built", binary.display());
        binary
    })
}

/// Spawns a fresh `acp-mock` process serving `script`.
fn spawn_mock(script: &Path) -> AcpClient<NoCallbacks> {
    let mut command = tokio::process::Command::new(acp_mock_binary());
    command.arg("--script").arg(script);
    let (updates, rx) = mpsc::unbounded_channel();
    // Nothing reads the updates here; keep the receiver alive so sends do not fail.
    std::mem::forget(rx);
    AcpClient::spawn(command, NoCallbacks, updates).expect("spawn acp-mock")
}

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(IO_TIMEOUT, future)
        .await
        .expect("timed out")
}

// ---- store ----------------------------------------------------------------------------

#[test]
fn store_survives_restart() {
    let dir = TempDir::new("restart");
    let clock = Arc::new(ManualClock::new(1_000));

    let store = open_store(&dir, &clock);
    assert_eq!(store.schema_version().expect("version"), CURRENT_VERSION);
    store
        .upsert_thread("thr_1", "claude-code", "/work/a")
        .expect("thread");
    store
        .upsert_task("task_1", "thr_1", "running")
        .expect("task");
    store
        .upsert_session(
            "thr_1",
            "claude-code",
            "sess_9",
            "/work/a",
            SessionState::Active,
        )
        .expect("session");
    store.set_resume_token(Some("rt_abc")).expect("token");
    store.commit_down_frame(7, Some("idem_1")).expect("down");
    drop(store);

    clock.set(5_000);
    let store = open_store(&dir, &clock);
    let thread = store.thread("thr_1").expect("thread").expect("present");
    assert_eq!(thread.harness, "claude-code");
    assert_eq!(thread.cwd, "/work/a");
    assert_eq!(thread.created_ms, 1_000);
    let task = store.task("task_1").expect("task").expect("present");
    assert_eq!(task.state, "running");
    assert_eq!(store.tasks_for_thread("thr_1").expect("tasks").len(), 1);
    let session = store
        .session_for_thread("thr_1")
        .expect("session")
        .expect("present");
    assert_eq!(session.session_id, "sess_9");
    assert_eq!(session.state, SessionState::Active);
    assert_eq!(
        store.resume_token().expect("token").as_deref(),
        Some("rt_abc")
    );
    assert_eq!(store.last_down_seq().expect("down"), 7);
    assert_eq!(
        store.observe_idempotency_key("idem_1").expect("key"),
        Disposition::Duplicate
    );

    assert_eq!(store.detach_all_sessions().expect("detach"), 1);
    let session = store.session_for_thread("thr_1").expect("s").expect("p");
    assert_eq!(session.state, SessionState::Detached);
}

#[test]
fn store_uses_wal_and_private_permissions() {
    let dir = TempDir::new("perms");
    let clock = Arc::new(ManualClock::new(0));
    let store = open_store(&dir, &clock);
    store.set_resume_token(Some("x")).expect("write");
    let state_dir = dir.path().join("state");
    let db = state_dir.join(sidecar::store::DB_FILE);

    let conn = rusqlite_journal_mode(&db);
    assert_eq!(conn, "wal");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir_mode = std::fs::metadata(&state_dir)
            .expect("dir")
            .permissions()
            .mode();
        let file_mode = std::fs::metadata(&db).expect("db").permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        assert_eq!(file_mode & 0o777, 0o600);
    }
}

/// Reads the journal mode straight from the file with a second connection.
fn rusqlite_journal_mode(db: &Path) -> String {
    // The sidecar crate depends on rusqlite, but tests link only dev-dependencies and the
    // crate's own dependencies, so rusqlite is available here.
    let conn = rusqlite::Connection::open(db).expect("open db");
    conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
        .expect("journal_mode")
}

#[test]
fn newer_schema_is_refused() {
    let dir = TempDir::new("newer");
    let clock = Arc::new(ManualClock::new(0));
    let store = open_store(&dir, &clock);
    drop(store);
    let db = dir.path().join("state").join(sidecar::store::DB_FILE);
    {
        let conn = rusqlite::Connection::open(&db).expect("open db");
        conn.pragma_update(None, "user_version", CURRENT_VERSION + 1)
            .expect("bump version");
    }
    let clock: Arc<dyn Clock> = clock;
    let error = Store::open_with_clock(&dir.path().join("state"), clock)
        .err()
        .expect("a newer schema must be refused");
    assert!(matches!(error, StoreError::SchemaTooNew { .. }), "{error}");
}

#[test]
fn unacked_events_return_in_seq_order_and_acked_are_pruned() {
    let dir = TempDir::new("outbox");
    let clock = Arc::new(ManualClock::new(0));
    let store = open_store(&dir, &clock);

    for n in 1..=5u64 {
        let seq = store
            .enqueue_outbound(&format!("ev_{n}"), &json!({ "n": n }))
            .expect("enqueue");
        assert_eq!(seq, n);
    }
    // The same event_id is the same frame (stable across retries).
    assert_eq!(
        store
            .enqueue_outbound("ev_3", &json!({ "n": 3 }))
            .expect("retry"),
        3
    );
    store.ack_outbound(2).expect("ack");
    store.ack_outbound(1).expect("stale ack is a no-op");
    assert!(matches!(
        store.ack_outbound(9),
        Err(StoreError::AckAhead { seq: 9, last: 5 })
    ));
    drop(store);

    let store = open_store(&dir, &clock);
    let state = store.outbox_state().expect("state");
    assert_eq!(state.acked_upto, 2);
    assert_eq!(state.next_seq, 6);
    let seqs: Vec<u64> = state.unacked.iter().map(|row| row.seq).collect();
    assert_eq!(seqs, [3, 4, 5]);
    let ids: Vec<&str> = state
        .unacked
        .iter()
        .map(|row| row.event_id.as_str())
        .collect();
    assert_eq!(ids, ["ev_3", "ev_4", "ev_5"]);
    assert_eq!(state.unacked[0].body, json!({ "n": 3 }));

    // Seqs keep counting after everything is acked and pruned.
    store.ack_outbound(5).expect("ack all");
    assert!(store.unacked_outbound().expect("none").is_empty());
    assert_eq!(
        store.enqueue_outbound("ev_6", &json!({})).expect("enqueue"),
        6
    );
}

#[test]
fn fresh_stream_renumbers_the_outbox_from_one() {
    let dir = TempDir::new("renumber");
    let clock = Arc::new(ManualClock::new(0));
    let store = open_store(&dir, &clock);
    for n in 1..=4u64 {
        store
            .enqueue_outbound(&format!("ev_{n}"), &json!(n))
            .expect("enqueue");
    }
    store.ack_outbound(2).expect("ack");

    store
        .restart_outbound_stream()
        .expect("welcome resumed:false");

    let state = store.outbox_state().expect("state");
    assert_eq!(state.acked_upto, 0);
    assert_eq!(state.next_seq, 3);
    let pairs: Vec<(u64, &str)> = state
        .unacked
        .iter()
        .map(|row| (row.seq, row.event_id.as_str()))
        .collect();
    assert_eq!(pairs, [(1, "ev_3"), (2, "ev_4")]);
}

#[test]
fn idempotency_keys_are_remembered_for_seven_days() {
    let dir = TempDir::new("idem");
    let clock = Arc::new(ManualClock::new(10 * DAY_MS));
    let store = open_store(&dir, &clock);

    assert_eq!(
        store.commit_down_frame(1, Some("k")).expect("first"),
        Disposition::New
    );
    drop(store);

    clock.advance(IDEMPOTENCY_KEY_TTL_MS - 1);
    let store = open_store(&dir, &clock);
    assert_eq!(
        store
            .commit_down_frame(2, Some("k"))
            .expect("within window"),
        Disposition::Duplicate
    );
    assert_eq!(store.last_down_seq().expect("down"), 2);

    clock.advance(1);
    assert_eq!(
        store.commit_down_frame(3, Some("k")).expect("expired"),
        Disposition::New,
        "a key older than 7 days counts as new again"
    );
    assert_eq!(store.last_down_seq().expect("down"), 3);
    // A lower seq never moves the cursor back.
    store.commit_down_frame(1, None).expect("old frame");
    assert_eq!(store.last_down_seq().expect("down"), 3);
}

// ---- ACP re-attach --------------------------------------------------------------------

#[tokio::test]
async fn next_prompt_resumes_the_session_after_the_agent_is_killed() {
    let dir = TempDir::new("reattach");
    let script = dir.path().join("script.json");
    std::fs::write(
        &script,
        json!({ "turns": [
            { "steps": [{ "text": "hello" }, { "stop": "end_turn" }] },
            { "steps": [{ "text": "again" }, { "stop": "end_turn" }] }
        ] })
        .to_string(),
    )
    .expect("write script");
    let clock = Arc::new(ManualClock::new(0));
    let cwd = dir.path().to_path_buf();

    // Turn 1 on the first agent process.
    let store = Arc::new(open_store(&dir, &clock));
    let reattacher = Reattacher::new(Arc::clone(&store));
    let first = spawn_mock(&script);
    within(first.initialize()).await.expect("initialize");
    let (stop, how) = within(reattacher.prompt(&first, "thr_1", "mock", &cwd, text("one")))
        .await
        .expect("first prompt");
    assert_eq!((stop, how), (StopReason::EndTurn, Attach::New));
    let stored = store
        .session_for_thread("thr_1")
        .expect("row")
        .expect("present");
    let first_pid = first.child_id().expect("pid");

    // Kill the agent between the turns, and restart the whole runner state.
    drop(first);
    drop(reattacher);
    drop(store);

    let store = Arc::new(open_store(&dir, &clock));
    store.detach_all_sessions().expect("detach");
    let reattacher = Reattacher::new(Arc::clone(&store));
    let second = spawn_mock(&script);
    assert_ne!(second.child_id().expect("pid"), first_pid);
    within(second.initialize()).await.expect("initialize");

    // Without a resume the new process would answer "unknown session"; the prompt succeeds
    // only because the session was resumed first.
    let (stop, how) = within(reattacher.prompt(&second, "thr_1", "mock", &cwd, text("two")))
        .await
        .expect("second prompt");
    assert_eq!(stop, StopReason::EndTurn);
    assert_eq!(how, Attach::Resumed);
    let after = store.session_for_thread("thr_1").expect("row").expect("p");
    assert_eq!(after.session_id, stored.session_id, "same ACP session");
    assert_eq!(after.state, SessionState::Active);

    // A third prompt on the same connection sends no resume or load.
    let (_, how) = within(reattacher.prompt(&second, "thr_1", "mock", &cwd, text("three")))
        .await
        .expect("third prompt");
    assert_eq!(how, Attach::Live);
}

/// A hand-played agent: answers over a duplex pipe and logs the methods it receives.
/// `session/resume` fails with an error (`resume_error`); `session/load` and `session/prompt`
/// succeed; `session/load` fails too when `load_error` is set.
async fn fake_agent(
    resume_error: i64,
    load_error: bool,
) -> (AcpClient<NoCallbacks>, Arc<Mutex<Vec<String>>>) {
    let (client_end, agent_end) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_end);
    let (agent_read, mut agent_write) = tokio::io::split(agent_end);
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = Arc::clone(&log);
    tokio::spawn(async move {
        let mut lines = BufReader::new(agent_read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let (Some(id), Some(method)) = (message.get("id"), message["method"].as_str()) else {
                continue;
            };
            seen.lock().expect("log").push(method.to_owned());
            let reply = match method {
                "session/resume" => json!({ "jsonrpc": "2.0", "id": id,
                    "error": { "code": resume_error, "message": "resume not supported" } }),
                "session/load" if load_error => json!({ "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32000, "message": "no such session" } }),
                "session/prompt" => json!({ "jsonrpc": "2.0", "id": id,
                    "result": { "stopReason": "end_turn" } }),
                _ => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            };
            let mut bytes = reply.to_string().into_bytes();
            bytes.push(b'\n');
            if agent_write.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });
    let (updates, rx) = mpsc::unbounded_channel();
    std::mem::forget(rx);
    let client = AcpClient::connect(client_read, client_write, NoCallbacks, updates);
    (client, log)
}

fn stored_session(store: &Store) {
    store
        .upsert_session("thr_1", "mock", "sess_old", "/work", SessionState::Detached)
        .expect("session");
}

#[tokio::test]
async fn load_is_the_fallback_when_resume_errors() {
    let dir = TempDir::new("fallback");
    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(open_store(&dir, &clock));
    stored_session(&store);
    let reattacher = Reattacher::new(Arc::clone(&store));

    // -32601: the agent does not support session/resume at all.
    let (client, log) = fake_agent(-32601, false).await;
    let (stop, how) =
        within(reattacher.prompt(&client, "thr_1", "mock", Path::new("/work"), text("hi")))
            .await
            .expect("prompt");

    assert_eq!(stop, StopReason::EndTurn);
    assert_eq!(how, Attach::Loaded);
    assert_eq!(
        *log.lock().expect("log"),
        ["session/resume", "session/load", "session/prompt"],
        "resume is tried, then load, both before the prompt"
    );
}

#[tokio::test]
async fn load_is_the_fallback_when_resume_fails_with_an_agent_error() {
    let dir = TempDir::new("fallback2");
    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(open_store(&dir, &clock));
    stored_session(&store);
    let reattacher = Reattacher::new(Arc::clone(&store));

    let (client, log) = fake_agent(-32603, false).await;
    let (_, how) =
        within(reattacher.prompt(&client, "thr_1", "mock", Path::new("/work"), text("hi")))
            .await
            .expect("prompt");

    assert_eq!(how, Attach::Loaded);
    assert_eq!(
        log.lock().expect("log")[..2],
        ["session/resume", "session/load"]
    );
}

#[tokio::test]
async fn unrecoverable_session_is_an_error_and_the_prompt_is_not_sent() {
    let dir = TempDir::new("unrecoverable");
    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(open_store(&dir, &clock));
    stored_session(&store);
    let reattacher = Reattacher::new(Arc::clone(&store));

    let (client, log) = fake_agent(-32601, true).await;
    let error = within(reattacher.prompt(&client, "thr_1", "mock", Path::new("/work"), text("hi")))
        .await
        .expect_err("both resume and load fail");

    assert!(
        matches!(error, ReattachError::Unrecoverable { .. }),
        "{error}"
    );
    assert_eq!(
        *log.lock().expect("log"),
        ["session/resume", "session/load"]
    );
    // The stored session is kept.
    assert!(store.session_for_thread("thr_1").expect("row").is_some());
}

// ---- interrupt expiry -----------------------------------------------------------------

fn permission_request() -> Value {
    json!({
        "sessionId": "sess_1",
        "toolCall": { "toolCallId": "call_1", "title": "Run cargo test", "kind": "execute" },
        "options": [
            { "optionId": "allow_once", "name": "Allow", "kind": "allow_once" },
            { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" }
        ]
    })
}

/// The `RunError` events among `events`, as `(reason, code)`.
fn run_errors(events: &[protocol::turn_event::Envelope]) -> Vec<(String, Option<String>)> {
    events
        .iter()
        .filter_map(|envelope| {
            let value = serde_json::to_value(envelope).expect("envelope json");
            let event = &value["event"];
            (event["type"] == "RunError").then(|| {
                (
                    event["reason"].as_str().unwrap_or_default().to_owned(),
                    event["code"].as_str().map(str::to_owned),
                )
            })
        })
        .collect()
}

#[test]
fn interrupt_pending_across_restart_past_its_ttl_expires_the_task() {
    let dir = TempDir::new("expiry");
    let ttl_ms = 10 * 60 * 1000;
    let t0 = 1_800_000_000_000u64;
    let clock = Arc::new(ManualClock::new(t0));

    // Before the restart: the task pauses on an approval and the interrupt is stored.
    let store = open_store(&dir, &clock);
    store
        .upsert_thread("thr_1", "mock", "/work")
        .expect("thread");
    store
        .upsert_task("task_1", "thr_1", "paused")
        .expect("task");
    let request: protocol::acp::RequestPermissionRequest =
        serde_json::from_value(permission_request()).expect("request");
    let mut mapper = AcpTurnMapper::with_sequential_ids("thr_1");
    mapper.start_task(StartTask::new("task_1"), &rfc3339(t0));
    let sent = mapper.on_request_permission(&request, "int_1", &rfc3339(t0 + ttl_ms), &rfc3339(t0));
    assert!(!sent.is_empty());
    store
        .save_interrupt(&PendingInterrupt {
            interrupt_id: "int_1".to_owned(),
            thread_id: "thr_1".to_owned(),
            task_id: "task_1".to_owned(),
            kind: InterruptKind::Approval,
            request: permission_request(),
            expires_at_ms: t0 + ttl_ms,
        })
        .expect("save interrupt");
    drop(store);
    drop(mapper);

    // Restart inside the TTL: nothing expires, the interrupt is still open.
    clock.set(t0 + ttl_ms - 1);
    let store = open_store(&dir, &clock);
    let runs =
        expire_overdue_interrupts(&store, |i| AcpTurnMapper::with_sequential_ids(&i.thread_id))
            .expect("expire");
    assert!(runs.is_empty());
    assert_eq!(store.open_interrupts().expect("open").len(), 1);
    drop(store);

    // Restart after the TTL: the store reports it expired and the runner ends the task.
    clock.set(t0 + ttl_ms + 5);
    let store = open_store(&dir, &clock);
    let runs =
        expire_overdue_interrupts(&store, |i| AcpTurnMapper::with_sequential_ids(&i.thread_id))
            .expect("expire");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].interrupt.interrupt_id, "int_1");
    let types: Vec<String> = runs[0]
        .events
        .iter()
        .map(|e| {
            serde_json::to_value(e).expect("json")["event"]["type"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(types.first().map(String::as_str), Some("InterruptResolved"));
    let resolved = serde_json::to_value(&runs[0].events[0]).expect("json");
    assert_eq!(resolved["event"]["outcome"]["type"], "expired");
    assert_eq!(
        run_errors(&runs[0].events),
        [("error".to_owned(), Some("expired".to_owned()))]
    );
    assert_eq!(
        store.task("task_1").expect("task").expect("p").state,
        "failed"
    );

    // Reported once: a later scan finds nothing, and the interrupt is no longer open.
    let again =
        expire_overdue_interrupts(&store, |i| AcpTurnMapper::with_sequential_ids(&i.thread_id))
            .expect("expire");
    assert!(again.is_empty());
    assert!(store.open_interrupts().expect("open").is_empty());
}

#[test]
fn answered_interrupts_never_expire() {
    let dir = TempDir::new("answered");
    let clock = Arc::new(ManualClock::new(1_000));
    let store = open_store(&dir, &clock);
    store
        .save_interrupt(&PendingInterrupt {
            interrupt_id: "int_1".to_owned(),
            thread_id: "thr_1".to_owned(),
            task_id: "task_1".to_owned(),
            kind: InterruptKind::Approval,
            request: permission_request(),
            expires_at_ms: 2_000,
        })
        .expect("save");
    assert!(store.resolve_interrupt("int_1").expect("resolve"));
    clock.set(10_000);
    assert!(store.take_expired_interrupts().expect("expired").is_empty());
}
