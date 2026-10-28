//! Tests for the client-side ACP file access and terminals: `fs/*` confined to the workspace,
//! and `terminal/*` on a PTY with its output teed into `Activity{terminal}` envelopes.
//!
//! The terminal tests run real `sh` children on a PTY, so they need a POSIX shell and `sleep`,
//! and work the same on Linux and macOS. The Activity envelopes are validated against the
//! TurnEvent envelope schema in `crates/protocol/schemas/turn-event`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use jsonschema::{Retrieve, Uri};
use protocol::acp::{
    CreateTerminalRequest, EnvVariable, KillTerminalRequest, ReadTextFileRequest,
    ReleaseTerminalRequest, SessionId, TerminalId, TerminalOutputRequest,
    WaitForTerminalExitRequest, WriteTextFileRequest,
};
use protocol::turn_event::{ActivityMode, ActivityType, Envelope, Event, TerminalContent};
use serde_json::Value;
use sidecar::acp::fs::{FsError, Workspace};
use sidecar::acp::terminal::{ActivitySink, TerminalError, TerminalScope, Terminals};
use tempfile::TempDir;
use tokio::time::timeout;

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const SCHEMA_BASE: &str = "https://sprite-machine.dev/schemas/turn-event/v1/";
const CHUNK_MAX: usize = 64 * 1024;
const CLOSING_TAIL: usize = 128 * 1024;

// ---- helpers ------------------------------------------------------------------------------

/// A fresh workspace root inside a temporary directory. The `TempDir` is returned so it
/// lives as long as the test.
fn workspace() -> (TempDir, Workspace) {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("workspace");
    fs::create_dir_all(&root).expect("create workspace root");
    let workspace = Workspace::new(&root).expect("open workspace");
    (dir, workspace)
}

fn read_request(path: impl Into<PathBuf>) -> ReadTextFileRequest {
    ReadTextFileRequest::new(SessionId::from("s1"), path)
}

fn write_request(path: impl Into<PathBuf>, content: &str) -> WriteTextFileRequest {
    WriteTextFileRequest::new(SessionId::from("s1"), path, content)
}

/// Collects the envelopes the terminals emit.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<Envelope>>>);

impl Sink {
    fn activity(&self) -> ActivitySink {
        let envelopes = Arc::clone(&self.0);
        Arc::new(move |envelope| lock(&envelopes).push(envelope))
    }

    fn envelopes(&self) -> Vec<Envelope> {
        lock(&self.0).clone()
    }
}

fn terminals(workspace: Workspace) -> (Terminals, Sink) {
    let sink = Sink::default();
    let scope = TerminalScope {
        thread_id: "thread_1".to_owned(),
        task_id: Some("task_1".to_owned()),
        run_id: Some("run_1".to_owned()),
    };
    (Terminals::new(workspace, scope, sink.activity()), sink)
}

/// A `sh -c` command, the same in every test.
fn shell(script: &str) -> CreateTerminalRequest {
    CreateTerminalRequest::new(SessionId::from("s1"), "sh")
        .args(vec!["-c".to_owned(), script.to_owned()])
}

async fn create(terminals: &Terminals, request: CreateTerminalRequest) -> TerminalId {
    terminals
        .create(&SessionId::from("s1"), request)
        .await
        .expect("terminal created")
        .terminal_id
}

async fn wait_exit(terminals: &Terminals, id: &TerminalId) -> protocol::acp::TerminalExitStatus {
    let request = WaitForTerminalExitRequest::new(SessionId::from("s1"), id.clone());
    timeout(IO_TIMEOUT, terminals.wait_for_exit(request))
        .await
        .expect("the terminal exits in time")
        .expect("wait_for_exit succeeds")
        .exit_status
}

async fn output(terminals: &Terminals, id: &TerminalId) -> (String, bool) {
    let request = TerminalOutputRequest::new(SessionId::from("s1"), id.clone());
    let response = terminals.output(request).await.expect("output succeeds");
    (response.output, response.truncated)
}

/// Runs `script` to completion and returns its output.
async fn run_to_end(terminals: &Terminals, script: &str) -> String {
    let id = create(terminals, shell(script)).await;
    wait_exit(terminals, &id).await;
    output(terminals, &id).await.0
}

/// The TurnEvent envelope schema with every `$ref` resolved from the repository copy.
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
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../protocol/schemas/turn-event");
    let text = fs::read_to_string(dir.join("envelope.schema.json")).expect("envelope schema");
    let root: Value = serde_json::from_str(&text).expect("envelope schema is JSON");
    jsonschema::options()
        .with_retriever(SchemaFiles { dir })
        .build(&root)
        .expect("envelope schema compiles")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The terminal content of one Activity envelope, with its activity type and mode checked.
fn terminal_content(envelope: &Envelope) -> (ActivityMode, TerminalContent) {
    let Event::Activity(activity) = &envelope.event else {
        panic!("expected an Activity, got {:?}", envelope.event);
    };
    assert_eq!(activity.activity_type, ActivityType::Terminal);
    let content = activity
        .content_as::<TerminalContent>()
        .expect("content present")
        .expect("content is a terminal content");
    assert_eq!(activity.activity_id, content.terminal_id);
    (activity.mode, content)
}

// ---- fs/*: confinement to the workspace ---------------------------------------------------

#[tokio::test]
async fn fs_read_resolves_relative_path_against_root() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("notes.txt"), "hello\n").expect("seed file");

    let response = workspace
        .read_text_file(read_request("notes.txt"))
        .await
        .expect("read succeeds");

    assert_eq!(response.content, "hello\n");
}

#[tokio::test]
async fn fs_read_honours_line_and_limit() {
    let (_dir, workspace) = workspace();
    fs::write(
        workspace.root().join("lines.txt"),
        "one\ntwo\nthree\nfour\n",
    )
    .expect("seed file");

    let mut request = read_request("lines.txt");
    request.line = Some(2);
    request.limit = Some(2);
    let response = workspace
        .read_text_file(request)
        .await
        .expect("read succeeds");
    assert_eq!(response.content, "two\nthree\n");

    // A line past the end gives an empty string, not an error.
    let mut request = read_request("lines.txt");
    request.line = Some(10);
    let response = workspace
        .read_text_file(request)
        .await
        .expect("read succeeds");
    assert_eq!(response.content, "");
}

#[tokio::test]
async fn fs_write_creates_parent_dirs_inside_root() {
    let (_dir, workspace) = workspace();

    workspace
        .write_text_file(write_request("deep/er/file.txt", "written"))
        .await
        .expect("write succeeds");

    let on_disk =
        fs::read_to_string(workspace.root().join("deep/er/file.txt")).expect("file exists");
    assert_eq!(on_disk, "written");
    let read = workspace
        .read_text_file(read_request("deep/er/file.txt"))
        .await
        .expect("read back");
    assert_eq!(read.content, "written");
}

#[tokio::test]
async fn fs_rejects_dotdot_traversal() {
    let (dir, workspace) = workspace();
    fs::write(dir.path().join("secret.txt"), "outside").expect("seed outside file");

    let read = workspace
        .read_text_file(read_request("../secret.txt"))
        .await;
    assert!(matches!(read, Err(FsError::Traversal(_))), "read: {read:?}");

    let write = workspace
        .write_text_file(write_request("../planted.txt", "x"))
        .await;
    assert!(
        matches!(write, Err(FsError::Traversal(_))),
        "write: {write:?}"
    );
    assert!(
        !dir.path().join("planted.txt").exists(),
        "nothing was written outside the root"
    );
}

#[tokio::test]
async fn fs_rejects_absolute_path_outside_root() {
    let (dir, workspace) = workspace();
    let outside = dir.path().join("outside.txt");
    fs::write(&outside, "outside").expect("seed outside file");

    let read = workspace.read_text_file(read_request(&outside)).await;
    assert!(matches!(read, Err(FsError::Outside(_))), "read: {read:?}");

    let write = workspace
        .write_text_file(write_request(dir.path().join("new.txt"), "x"))
        .await;
    assert!(
        matches!(write, Err(FsError::Outside(_))),
        "write: {write:?}"
    );
    assert!(!dir.path().join("new.txt").exists());
}

#[tokio::test]
async fn fs_accepts_absolute_path_inside_root() {
    let (_dir, workspace) = workspace();
    let inside = workspace.root().join("inside.txt");
    fs::write(&inside, "inside").expect("seed file");

    let response = workspace
        .read_text_file(read_request(&inside))
        .await
        .expect("an absolute path inside the root reads");

    assert_eq!(response.content, "inside");
}

#[cfg(unix)]
#[tokio::test]
async fn fs_rejects_symlink_to_file_outside_root() {
    let (dir, workspace) = workspace();
    fs::write(dir.path().join("secret.txt"), "outside").expect("seed outside file");
    std::os::unix::fs::symlink(
        dir.path().join("secret.txt"),
        workspace.root().join("link.txt"),
    )
    .expect("symlink");

    let read = workspace.read_text_file(read_request("link.txt")).await;

    assert!(matches!(read, Err(FsError::Outside(_))), "read: {read:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn fs_rejects_symlinked_parent_escaping_root() {
    let (dir, workspace) = workspace();
    let outside_dir = dir.path().join("outside");
    fs::create_dir_all(&outside_dir).expect("outside dir");
    fs::write(outside_dir.join("secret.txt"), "outside").expect("seed outside file");
    std::os::unix::fs::symlink(&outside_dir, workspace.root().join("escape"))
        .expect("symlink to a directory");

    let read = workspace
        .read_text_file(read_request("escape/secret.txt"))
        .await;
    assert!(matches!(read, Err(FsError::Outside(_))), "read: {read:?}");

    let write = workspace
        .write_text_file(write_request("escape/planted.txt", "x"))
        .await;
    assert!(
        matches!(write, Err(FsError::Outside(_))),
        "write: {write:?}"
    );
    assert!(
        !outside_dir.join("planted.txt").exists(),
        "nothing was written outside the root"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn fs_rejects_write_through_dangling_symlink() {
    let (dir, workspace) = workspace();
    let target = dir.path().join("not-yet.txt");
    std::os::unix::fs::symlink(&target, workspace.root().join("dangling.txt")).expect("symlink");

    let write = workspace
        .write_text_file(write_request("dangling.txt", "payload"))
        .await;

    assert!(
        matches!(write, Err(FsError::DanglingSymlink(_))),
        "write: {write:?}"
    );
    assert!(
        !target.exists(),
        "the write did not follow the link outside the root"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn fs_follows_symlink_that_stays_inside_root() {
    let (_dir, workspace) = workspace();
    fs::create_dir_all(workspace.root().join("real")).expect("real dir");
    fs::write(workspace.root().join("real/file.txt"), "inside").expect("seed file");
    std::os::unix::fs::symlink(
        workspace.root().join("real"),
        workspace.root().join("alias"),
    )
    .expect("symlink inside the root");

    let response = workspace
        .read_text_file(read_request("alias/file.txt"))
        .await
        .expect("a link inside the root is followed");

    assert_eq!(response.content, "inside");
}

#[test]
fn fs_workspace_root_must_be_a_directory() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = dir.path().join("file.txt");
    fs::write(&file, "x").expect("seed file");

    assert!(Workspace::new(&file).is_err());
}

// ---- terminal/*: a PTY with its output ----------------------------------------------------

#[tokio::test]
async fn terminal_runs_command_and_reports_output_and_exit() {
    let (_dir, workspace) = workspace();
    let (terminals, _sink) = terminals(workspace);

    let id = create(&terminals, shell("printf hello; exit 3")).await;
    let status = wait_exit(&terminals, &id).await;

    assert_eq!(status.exit_code, Some(3));
    assert_eq!(status.signal, None);
    let (text, truncated) = output(&terminals, &id).await;
    assert_eq!(text, "hello");
    assert!(!truncated);
}

#[tokio::test]
async fn terminal_output_byte_limit_truncates_from_the_start() {
    let (_dir, workspace) = workspace();
    let (terminals, _sink) = terminals(workspace);
    let mut request = shell("printf abcdefghij");
    request.output_byte_limit = Some(4);

    let id = create(&terminals, request).await;
    wait_exit(&terminals, &id).await;
    let (text, truncated) = output(&terminals, &id).await;

    assert_eq!(text, "ghij", "the retained output is the tail");
    assert!(truncated);
}

#[tokio::test]
async fn terminal_output_limit_cuts_on_a_character_boundary() {
    let (_dir, workspace) = workspace();
    let (terminals, _sink) = terminals(workspace);
    let mut request = shell("printf 'ééé'");
    // Five bytes keep "éé" (four bytes) plus one byte of the first "é", which is not a
    // character. The cut moves forward to the next boundary.
    request.output_byte_limit = Some(5);

    let id = create(&terminals, request).await;
    wait_exit(&terminals, &id).await;
    let (text, truncated) = output(&terminals, &id).await;

    assert_eq!(text, "éé");
    assert!(truncated);
}

#[tokio::test]
async fn terminal_honours_cwd_and_env() {
    let (_dir, workspace) = workspace();
    let subdir = workspace.root().join("sub");
    fs::create_dir_all(&subdir).expect("subdir");
    let (terminals, _sink) = terminals(workspace);
    let mut request = shell(r#"printf '%s|%s' "$(basename "$(pwd -P)")" "$SM_TEST_VALUE""#);
    request.cwd = Some(subdir);
    request.env = vec![EnvVariable::new("SM_TEST_VALUE", "from-request")];

    let id = create(&terminals, request).await;
    let status = wait_exit(&terminals, &id).await;

    assert_eq!(status.exit_code, Some(0));
    let (text, _) = output(&terminals, &id).await;
    assert_eq!(text, "sub|from-request");
}

#[tokio::test]
async fn terminal_rejects_cwd_outside_workspace() {
    let (dir, workspace) = workspace();
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).expect("outside dir");
    let (terminals, _sink) = terminals(workspace);
    let mut request = shell("true");
    request.cwd = Some(outside);

    let result = terminals.create(&SessionId::from("s1"), request).await;

    assert!(
        matches!(result, Err(TerminalError::Cwd(FsError::Outside(_)))),
        "{result:?}"
    );
}

#[tokio::test]
async fn terminal_kill_stops_a_running_command() {
    let (_dir, workspace) = workspace();
    let (terminals, _sink) = terminals(workspace);

    let id = create(
        &terminals,
        CreateTerminalRequest::new(SessionId::from("s1"), "sleep").args(vec!["30".to_owned()]),
    )
    .await;
    let kill = KillTerminalRequest::new(SessionId::from("s1"), id.clone());
    terminals.kill(kill).await.expect("kill succeeds");
    let status = wait_exit(&terminals, &id).await;

    assert!(
        status.signal.is_some(),
        "a killed child reports the signal: {status:?}"
    );
    assert_eq!(status.exit_code, None);
}

#[tokio::test]
async fn terminal_release_frees_it_and_later_calls_error() {
    let (_dir, workspace) = workspace();
    let (terminals, _sink) = terminals(workspace);

    let id = create(
        &terminals,
        CreateTerminalRequest::new(SessionId::from("s1"), "sleep").args(vec!["30".to_owned()]),
    )
    .await;
    terminals
        .release(ReleaseTerminalRequest::new(
            SessionId::from("s1"),
            id.clone(),
        ))
        .await
        .expect("release succeeds");

    let after_output = terminals
        .output(TerminalOutputRequest::new(
            SessionId::from("s1"),
            id.clone(),
        ))
        .await;
    assert!(
        matches!(after_output, Err(TerminalError::UnknownTerminal(_))),
        "{after_output:?}"
    );
    let again = terminals
        .release(ReleaseTerminalRequest::new(
            SessionId::from("s1"),
            id.clone(),
        ))
        .await;
    assert!(
        matches!(again, Err(TerminalError::UnknownTerminal(_))),
        "{again:?}"
    );
    let waited = terminals
        .wait_for_exit(WaitForTerminalExitRequest::new(SessionId::from("s1"), id))
        .await;
    assert!(
        matches!(waited, Err(TerminalError::UnknownTerminal(_))),
        "{waited:?}"
    );
}

#[tokio::test]
async fn terminal_belongs_to_the_session_that_created_it() {
    let (_dir, workspace) = workspace();
    let (terminals, _sink) = terminals(workspace);
    let id = create(&terminals, shell("printf x")).await;

    let other = terminals
        .output(TerminalOutputRequest::new(
            SessionId::from("s2"),
            id.clone(),
        ))
        .await;

    assert!(
        matches!(other, Err(TerminalError::WrongSession(_))),
        "{other:?}"
    );
    wait_exit(&terminals, &id).await;
}

#[tokio::test]
async fn terminal_activity_is_snapshot_appends_then_closing_snapshot() {
    let (_dir, workspace) = workspace();
    let root = workspace.root().to_path_buf();
    let (terminals, sink) = terminals(workspace);

    let id = create(&terminals, shell("printf one; printf two; exit 0")).await;
    wait_exit(&terminals, &id).await;

    let envelopes = sink.envelopes();
    assert!(
        envelopes.len() >= 3,
        "snapshot, at least one append, closing snapshot: {envelopes:?}"
    );
    let validator = envelope_validator();
    for envelope in &envelopes {
        assert_eq!(envelope.seq, 0, "the producer never assigns seq");
        assert_eq!(envelope.thread_id, "thread_1");
        assert_eq!(envelope.task_id.as_deref(), Some("task_1"));
        assert_eq!(envelope.run_id.as_deref(), Some("run_1"));
        let value = serde_json::to_value(envelope).expect("envelope serializes");
        let errors: Vec<String> = validator
            .iter_errors(&value)
            .map(|e| e.to_string())
            .collect();
        assert!(
            errors.is_empty(),
            "{value}\nviolates the schema: {errors:?}"
        );
    }

    // Opening snapshot: command and cwd, no output, no exit.
    let (mode, first) = terminal_content(&envelopes[0]);
    assert_eq!(mode, ActivityMode::Snapshot);
    assert_eq!(first.command.as_deref(), Some("sh"));
    assert_eq!(first.cwd.as_deref(), root.to_str());
    assert_eq!(first.output, None);
    assert_eq!(first.exit, None);

    // Appends carry the output, in order.
    let last = envelopes.len() - 1;
    let mut streamed = String::new();
    for envelope in &envelopes[1..last] {
        let (mode, content) = terminal_content(envelope);
        assert_eq!(mode, ActivityMode::Append);
        streamed.push_str(content.output.as_deref().expect("append has output"));
    }
    assert_eq!(streamed, "onetwo");

    // Closing snapshot: the repeated command and cwd, the whole output, and the exit.
    let (mode, closing) = terminal_content(&envelopes[last]);
    assert_eq!(mode, ActivityMode::Snapshot);
    assert_eq!(closing.command.as_deref(), Some("sh"));
    assert_eq!(closing.output.as_deref(), Some("onetwo"));
    let exit = closing.exit.expect("closing snapshot carries the exit");
    assert_eq!(exit.exit_code, Some(0));
    assert_eq!(exit.signal, None);
}

#[tokio::test]
async fn terminal_activity_respects_chunk_and_closing_size_caps() {
    let (_dir, workspace) = workspace();
    let (terminals, sink) = terminals(workspace);
    let total = 200_000;

    let id = create(
        &terminals,
        shell(&format!("head -c {total} /dev/zero | tr '\\0' a")),
    )
    .await;
    let status = wait_exit(&terminals, &id).await;
    assert_eq!(status.exit_code, Some(0));

    let envelopes = sink.envelopes();
    let (_, closing) = terminal_content(envelopes.last().expect("closing snapshot"));
    let mut streamed = 0;
    for envelope in &envelopes[1..envelopes.len() - 1] {
        let (mode, content) = terminal_content(envelope);
        assert_eq!(mode, ActivityMode::Append);
        let chunk = content.output.expect("append has output");
        assert!(
            chunk.len() <= CHUNK_MAX,
            "an append is at most 64 KiB, got {}",
            chunk.len()
        );
        streamed += chunk.len();
    }
    assert_eq!(
        streamed, total,
        "every byte is teed, past the output limit too"
    );
    let closing_output = closing.output.expect("closing snapshot repeats the output");
    assert_eq!(
        closing_output.len(),
        CLOSING_TAIL,
        "the closing snapshot keeps the last 128 KiB"
    );
    assert!(closing_output.bytes().all(|b| b == b'a'));
    // The envelope stays inside the 256 KiB limit (§2).
    let size = serde_json::to_vec(envelopes.last().expect("closing snapshot"))
        .expect("serialize")
        .len();
    assert!(size <= 256 * 1024, "closing snapshot is {size} bytes");
}

#[tokio::test]
async fn terminal_activity_closing_snapshot_is_the_last_envelope_after_kill() {
    let (_dir, workspace) = workspace();
    let (terminals, sink) = terminals(workspace);
    let id = create(
        &terminals,
        CreateTerminalRequest::new(SessionId::from("s1"), "sleep").args(vec!["30".to_owned()]),
    )
    .await;
    terminals
        .kill(KillTerminalRequest::new(SessionId::from("s1"), id.clone()))
        .await
        .expect("kill succeeds");
    wait_exit(&terminals, &id).await;

    let envelopes = sink.envelopes();
    let (mode, closing) = terminal_content(envelopes.last().expect("closing snapshot"));
    assert_eq!(mode, ActivityMode::Snapshot);
    let exit = closing.exit.expect("exit present");
    assert_eq!(exit.exit_code, None);
    assert!(exit.signal.is_some());
}

#[tokio::test]
async fn terminal_output_reads_back_after_exit() {
    let (_dir, workspace) = workspace();
    let (terminals, _sink) = terminals(workspace);

    let text = run_to_end(&terminals, "printf done").await;

    assert_eq!(text, "done");
}

#[tokio::test]
async fn terminal_fs_and_terminal_share_one_root() {
    let (_dir, workspace) = workspace();
    let root = workspace.root().to_path_buf();
    workspace
        .write_text_file(write_request("made-by-fs.txt", "from fs"))
        .await
        .expect("write succeeds");
    let (terminals, _sink) = terminals(workspace);

    let text = run_to_end(&terminals, "cat made-by-fs.txt").await;

    assert_eq!(text, "from fs");
    assert!(root.join("made-by-fs.txt").exists());
}
