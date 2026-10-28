//! Client terminals for the ACP `terminal/*` requests: a command run on a PTY, with its output
//! kept for `terminal/output` and teed into `Activity{terminal}` envelopes (turn-event §4.4).
//!
//! [`Terminals`] owns the table of terminals for one runner. Each terminal runs on a PTY in
//! the workspace root, or in a directory under it when the request gives a `cwd`. Two std
//! threads serve each terminal: one reads the PTY, the other waits for the child to exit.
//!
//! The Activity tee goes to an injected [`ActivitySink`]. For each terminal it sends:
//!
//! 1. a `snapshot` on create, with `terminal_id`, `command` and `cwd`;
//! 2. `append` chunks with the output, each at most 64 KiB and split on UTF-8 boundaries;
//! 3. a closing `snapshot` once the child has exited and its output has been read. It has
//!    `exit` and repeats the last 128 KiB of output, so it fits the envelope limit (§4.9).
//!
//! The sink is called while the terminal's output lock is held, so the order of envelopes
//! matches the order of output. The sink must not block. An unbounded channel is the usual
//! choice.
//!
//! The ACP `output_byte_limit` truncates only what `terminal/output` returns. The tee sees
//! every byte. When the request gives no limit, [`DEFAULT_OUTPUT_LIMIT`] applies.
//!
//! A terminal's working directory must be inside the workspace root, the same rule as file
//! access ([`super::fs`]).

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use protocol::acp::{
    CreateTerminalRequest, CreateTerminalResponse, KillTerminalRequest, KillTerminalResponse,
    ReleaseTerminalRequest, ReleaseTerminalResponse, SessionId, TerminalExitStatus, TerminalId,
    TerminalOutputRequest, TerminalOutputResponse, WaitForTerminalExitRequest,
    WaitForTerminalExitResponse,
};
use protocol::turn_event::{
    Activity, ActivityMode, ActivityType, Envelope, Event, TerminalContent, TerminalExit,
    format_timestamp,
};
use serde_json::{Map, Value};
use tokio::sync::watch;

use super::fs::{FsError, Workspace};

/// Output bytes kept for `terminal/output` when the request sets no `output_byte_limit`.
pub const DEFAULT_OUTPUT_LIMIT: usize = 1024 * 1024;
/// The largest `output_byte_limit` honoured. A larger request is clamped to this.
pub const MAX_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
/// Largest `append` chunk, in bytes (§4.9).
const CHUNK_MAX: usize = 64 * 1024;
/// Output repeated in the closing snapshot, in bytes (§4.9).
const CLOSING_TAIL: usize = 128 * 1024;
/// Bytes read from the PTY at a time.
const READ_SIZE: usize = 8 * 1024;
/// How long the waiter waits for the reader to drain the PTY after the child exits. A
/// grandchild that keeps the PTY open must not hold the terminal open forever.
const READER_GRACE: Duration = Duration::from_secs(2);

/// Where the Activity envelopes go.
pub type ActivitySink = Arc<dyn Fn(Envelope) + Send + Sync>;

/// The identity an Activity envelope carries: the thread, and the task and run it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalScope {
    /// `thread_id` of the envelope.
    pub thread_id: String,
    /// `task_id` of the envelope, if the terminal belongs to a task.
    pub task_id: Option<String>,
    /// `run_id` of the envelope, if the terminal belongs to a run.
    pub run_id: Option<String>,
}

/// Errors from the terminal requests.
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    /// No terminal has this id, or it was released.
    #[error("unknown terminal {0}")]
    UnknownTerminal(String),
    /// The terminal exists but belongs to another session.
    #[error("terminal {0} belongs to another session")]
    WrongSession(String),
    /// The working directory is not inside the workspace.
    #[error("working directory: {0}")]
    Cwd(#[from] FsError),
    /// The command could not be started on a PTY.
    #[error("could not start the command: {0}")]
    Spawn(String),
    /// A thread or PTY call failed.
    #[error("terminal I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// The terminals of one runner.
pub struct Terminals {
    workspace: Workspace,
    scope: TerminalScope,
    sink: ActivitySink,
    table: Mutex<HashMap<String, Arc<Terminal>>>,
    next: AtomicU64,
}

impl Terminals {
    /// A terminal table that runs commands in `workspace` and tees their output to `sink`
    /// with the identity in `scope`.
    pub fn new(workspace: Workspace, scope: TerminalScope, sink: ActivitySink) -> Self {
        Self {
            workspace,
            scope,
            sink,
            table: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }
    }

    /// Answers `terminal/create`: starts the command on a PTY and returns its id.
    pub async fn create(
        &self,
        session: &SessionId,
        request: CreateTerminalRequest,
    ) -> Result<CreateTerminalResponse, TerminalError> {
        let cwd = match &request.cwd {
            Some(cwd) => self.workspace.resolve(cwd)?,
            None => self.workspace.root().to_path_buf(),
        };
        let limit = request
            .output_byte_limit
            .and_then(|limit| usize::try_from(limit).ok())
            .unwrap_or(DEFAULT_OUTPUT_LIMIT)
            .min(MAX_OUTPUT_LIMIT);

        let id = format!(
            "term_{}_{}",
            std::process::id(),
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        let mut command = CommandBuilder::new(&request.command);
        command.args(&request.args);
        command.cwd(&cwd);
        for variable in &request.env {
            command.env(&variable.name, &variable.value);
        }

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(spawn_error)?;
        let child = pair.slave.spawn_command(command).map_err(spawn_error)?;
        // The parent must not hold the slave: the reader only sees end-of-file once the
        // child, and anything it started, has closed it.
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().map_err(spawn_error)?;
        let killer = child.clone_killer();

        let emit = Emit {
            sink: Arc::clone(&self.sink),
            scope: self.scope.clone(),
            terminal_id: id.clone(),
            command: request.command.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
        };
        let shared = Arc::new(Shared {
            output: Mutex::new(Output::new(limit)),
            emit,
        });
        let (exit_tx, _) = watch::channel(None);
        let exit_tx = Arc::new(exit_tx);
        // The snapshot goes out before any thread can produce output.
        shared.emit.snapshot();

        let reader_shared = Arc::clone(&shared);
        let reader_thread = std::thread::Builder::new()
            .name(format!("pty-read-{id}"))
            .spawn(move || read_output(&reader_shared, reader))?;
        let waiter_shared = Arc::clone(&shared);
        let waiter_exit = Arc::clone(&exit_tx);
        std::thread::Builder::new()
            .name(format!("pty-wait-{id}"))
            .spawn(move || wait_for_child(&waiter_shared, &waiter_exit, child, reader_thread))?;

        let terminal = Arc::new(Terminal {
            session: session.clone(),
            shared,
            exit: exit_tx,
            killer: Mutex::new(killer),
            master: Mutex::new(Some(pair.master)),
        });
        lock(&self.table).insert(id.clone(), terminal);
        Ok(CreateTerminalResponse::new(TerminalId::from(id)))
    }

    /// Answers `terminal/output`: the retained output, cut from the start to the byte limit.
    pub async fn output(
        &self,
        request: TerminalOutputRequest,
    ) -> Result<TerminalOutputResponse, TerminalError> {
        let terminal = self.get(&request.session_id, &request.terminal_id)?;
        let (text, truncated) = {
            let output = lock(&terminal.shared.output);
            (
                String::from_utf8_lossy(&output.retained).into_owned(),
                output.truncated,
            )
        };
        let exit = terminal.exit.borrow().clone();
        let mut response = TerminalOutputResponse::new(text, truncated);
        response.exit_status = exit.as_ref().map(Exit::to_status);
        Ok(response)
    }

    /// Answers `terminal/wait_for_exit`: waits until the child has exited and its output has
    /// been teed.
    pub async fn wait_for_exit(
        &self,
        request: WaitForTerminalExitRequest,
    ) -> Result<WaitForTerminalExitResponse, TerminalError> {
        let terminal = self.get(&request.session_id, &request.terminal_id)?;
        let mut exit_rx = terminal.exit.subscribe();
        let exit = exit_rx
            .wait_for(Option::is_some)
            .await
            .map_err(|_| TerminalError::UnknownTerminal(request.terminal_id.0.to_string()))?
            .clone();
        let exit = exit.unwrap_or_default();
        Ok(WaitForTerminalExitResponse::new(exit.to_status()))
    }

    /// Answers `terminal/kill`: stops the child. The terminal stays until it is released, so
    /// its output can still be read. A child that has already exited is left alone.
    pub async fn kill(
        &self,
        request: KillTerminalRequest,
    ) -> Result<KillTerminalResponse, TerminalError> {
        let terminal = self.get(&request.session_id, &request.terminal_id)?;
        terminal.kill()?;
        Ok(KillTerminalResponse::new())
    }

    /// Answers `terminal/release`: kills the child if it runs, frees the PTY, and makes the
    /// id invalid for later calls.
    pub async fn release(
        &self,
        request: ReleaseTerminalRequest,
    ) -> Result<ReleaseTerminalResponse, TerminalError> {
        // Check the session before the terminal leaves the table.
        self.get(&request.session_id, &request.terminal_id)?;
        let removed = lock(&self.table).remove(request.terminal_id.0.as_ref());
        if let Some(terminal) = removed {
            terminal.kill()?;
            lock(&terminal.master).take();
        }
        Ok(ReleaseTerminalResponse::new())
    }

    fn get(
        &self,
        session: &SessionId,
        terminal_id: &TerminalId,
    ) -> Result<Arc<Terminal>, TerminalError> {
        let terminal = lock(&self.table)
            .get(terminal_id.0.as_ref())
            .cloned()
            .ok_or_else(|| TerminalError::UnknownTerminal(terminal_id.0.to_string()))?;
        if terminal.session.0 != session.0 {
            return Err(TerminalError::WrongSession(terminal_id.0.to_string()));
        }
        Ok(terminal)
    }
}

impl Drop for Terminals {
    fn drop(&mut self) {
        // Stop the children that are still running, as the runner shuts down.
        for terminal in lock(&self.table).values() {
            let _ = terminal.kill();
        }
    }
}

/// One terminal: its child's killer, its PTY master, and its output.
struct Terminal {
    session: SessionId,
    shared: Arc<Shared>,
    /// `None` until the child has exited; then the exit status.
    exit: Arc<watch::Sender<Option<Exit>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// Dropped on release, which frees the PTY.
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
}

impl Terminal {
    /// Kills the child, unless it has already exited.
    fn kill(&self) -> Result<(), TerminalError> {
        if self.exit.borrow().is_some() {
            return Ok(());
        }
        lock(&self.killer).kill()?;
        Ok(())
    }
}

/// State the reader and waiter threads share with the request handlers.
struct Shared {
    output: Mutex<Output>,
    emit: Emit,
}

impl Shared {
    fn push(&self, bytes: &[u8]) {
        lock(&self.output).append(&self.emit, bytes);
    }

    /// Closes the activity: emits any bytes still held back, then the closing snapshot.
    fn close(&self, exit: &Exit) {
        let mut output = lock(&self.output);
        if output.closed {
            return;
        }
        output.emit_ready(&self.emit, true);
        output.closed = true;
        let tail = String::from_utf8_lossy(&output.tail).into_owned();
        self.emit.closing(tail, exit);
    }
}

/// The retained output of one terminal, and the bytes not yet teed.
struct Output {
    /// The last `limit` bytes, at a character boundary. What `terminal/output` returns.
    retained: Vec<u8>,
    limit: usize,
    truncated: bool,
    /// The last `CLOSING_TAIL` bytes. The closing snapshot repeats them.
    tail: Vec<u8>,
    /// Bytes read but not yet teed. Only an incomplete UTF-8 sequence waits here.
    pending: Vec<u8>,
    closed: bool,
}

impl Output {
    fn new(limit: usize) -> Self {
        Self {
            retained: Vec::new(),
            limit,
            truncated: false,
            tail: Vec::new(),
            pending: Vec::new(),
            closed: false,
        }
    }

    fn append(&mut self, emit: &Emit, bytes: &[u8]) {
        self.retained.extend_from_slice(bytes);
        if self.retained.len() > self.limit {
            let excess = self.retained.len() - self.limit;
            let start = boundary_at_or_after(&self.retained, excess);
            self.retained.drain(..start);
            self.truncated = true;
        }
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > CLOSING_TAIL {
            let excess = self.tail.len() - CLOSING_TAIL;
            let start = boundary_at_or_after(&self.tail, excess);
            self.tail.drain(..start);
        }
        if self.closed {
            return;
        }
        self.pending.extend_from_slice(bytes);
        self.emit_ready(emit, false);
    }

    /// Tees the pending bytes in chunks of at most `CHUNK_MAX`. Unless `all`, an incomplete
    /// UTF-8 sequence at the end stays pending for the next read.
    fn emit_ready(&mut self, emit: &Emit, all: bool) {
        let complete = if all {
            self.pending.len()
        } else {
            complete_prefix(&self.pending)
        };
        let mut pos = 0;
        while pos < complete {
            let mut end = (pos + CHUNK_MAX).min(complete);
            if end < complete {
                let mut boundary = end;
                while boundary > pos && is_continuation(self.pending[boundary]) {
                    boundary -= 1;
                }
                if boundary > pos {
                    end = boundary;
                }
            }
            let text = String::from_utf8_lossy(&self.pending[pos..end]).into_owned();
            emit.append(text);
            pos = end;
        }
        self.pending.drain(..complete);
    }
}

/// Builds the Activity envelopes of one terminal.
struct Emit {
    sink: ActivitySink,
    scope: TerminalScope,
    terminal_id: String,
    command: String,
    cwd: String,
}

impl Emit {
    fn snapshot(&self) {
        let content = TerminalContent {
            terminal_id: self.terminal_id.clone(),
            tool_call_id: None,
            command: Some(self.command.clone()),
            cwd: Some(self.cwd.clone()),
            output: None,
            exit: None,
        };
        self.send(ActivityMode::Snapshot, content);
    }

    fn append(&self, output: String) {
        let content = TerminalContent {
            terminal_id: self.terminal_id.clone(),
            tool_call_id: None,
            command: None,
            cwd: None,
            output: Some(output),
            exit: None,
        };
        self.send(ActivityMode::Append, content);
    }

    fn closing(&self, output: String, exit: &Exit) {
        let content = TerminalContent {
            terminal_id: self.terminal_id.clone(),
            tool_call_id: None,
            command: Some(self.command.clone()),
            cwd: Some(self.cwd.clone()),
            output: Some(output),
            exit: Some(TerminalExit {
                exit_code: exit.code.map(i64::from),
                signal: exit.signal.clone(),
            }),
        };
        self.send(ActivityMode::Snapshot, content);
    }

    fn send(&self, mode: ActivityMode, content: TerminalContent) {
        let activity = Activity {
            activity_id: self.terminal_id.clone(),
            activity_type: ActivityType::Terminal,
            mode,
            content: Some(to_map(&content)),
            patch: None,
        };
        let envelope = Envelope::new(
            self.scope.thread_id.clone(),
            self.scope.task_id.clone(),
            self.scope.run_id.clone(),
            format_timestamp(now_millis()),
            Event::Activity(activity),
        );
        (self.sink)(envelope);
    }
}

/// How a child ended. `code` is `None` when a signal ended it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Exit {
    code: Option<u32>,
    signal: Option<String>,
}

impl Exit {
    fn from_status(status: &portable_pty::ExitStatus) -> Self {
        match status.signal() {
            Some(signal) => Self {
                code: None,
                signal: Some(signal.to_owned()),
            },
            None => Self {
                code: Some(status.exit_code()),
                signal: None,
            },
        }
    }

    fn to_status(&self) -> TerminalExitStatus {
        let mut status = TerminalExitStatus::default();
        status.exit_code = self.code;
        status.signal = self.signal.clone();
        status
    }
}

/// Reads the PTY until it ends, appending what it yields.
fn read_output(shared: &Shared, mut reader: Box<dyn Read + Send>) {
    let mut buffer = vec![0u8; READ_SIZE];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => shared.push(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            // On Linux the PTY reports EIO once the child side is closed. That is the normal end.
            Err(_) => break,
        }
    }
}

/// Waits for the child, lets the reader drain for a bounded time, then closes the activity
/// and publishes the exit status.
fn wait_for_child(
    shared: &Shared,
    exit_tx: &watch::Sender<Option<Exit>>,
    mut child: Box<dyn Child + Send + Sync>,
    reader: JoinHandle<()>,
) {
    let exit = match child.wait() {
        Ok(status) => Exit::from_status(&status),
        Err(error) => {
            tracing::warn!(%error, "waiting for a terminal child failed");
            Exit::default()
        }
    };
    join_reader(reader);
    shared.close(&exit);
    exit_tx.send_replace(Some(exit));
}

/// Waits up to `READER_GRACE` for the reader to finish. A reader still running after that is
/// left to finish on its own.
fn join_reader(reader: JoinHandle<()>) {
    let deadline = Instant::now() + READER_GRACE;
    while !reader.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    if reader.is_finished() {
        let _ = reader.join();
    }
}

/// The byte offset at or after `index` that starts a character.
fn boundary_at_or_after(bytes: &[u8], index: usize) -> usize {
    let mut at = index;
    while at < bytes.len() && is_continuation(bytes[at]) {
        at += 1;
    }
    at
}

fn is_continuation(byte: u8) -> bool {
    byte & 0xC0 == 0x80
}

/// The length of the prefix of `bytes` that is complete UTF-8. Invalid bytes count as
/// complete, since lossy decoding replaces them. Only a sequence cut off at the end is held
/// back.
fn complete_prefix(bytes: &[u8]) -> usize {
    let mut done = 0;
    loop {
        match std::str::from_utf8(&bytes[done..]) {
            Ok(_) => return bytes.len(),
            Err(error) => match error.error_len() {
                None => return done + error.valid_up_to(),
                Some(invalid) => done += error.valid_up_to() + invalid,
            },
        }
    }
}

fn to_map(content: &TerminalContent) -> Map<String, Value> {
    match serde_json::to_value(content) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

fn spawn_error(error: impl std::fmt::Display) -> TerminalError {
    TerminalError::Spawn(error.to_string())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
