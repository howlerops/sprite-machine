//! The router: reads the tunnel's commands and hands each to the session of its thread, starting
//! sessions for `dispatch`, and answers the commands no session can take.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use protocol::tunnel::{Command, CommandErrorCode, Workspace};
use sidecar::bridge::{
    Bridge, CommandFailure, CommandSource, Config, EventSink, IncomingCommand, McpProvider, UlidIds,
};
use sidecar::store::Store;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::local;
use super::project::{LastSession, Projects};
use super::session::{SessionCommands, SessionSink, SessionState};
use super::worktree::Worktree;
use super::{PermissionMode, SpawnMode, names, session_dir};
use crate::harness::{self, Found};
use crate::tunnel::{Exit, TunnelCommands, TunnelSink};

/// How long a session gets to wind down at shutdown.
const WIND_DOWN: Duration = Duration::from_secs(10);

pub(super) struct Context {
    pub machine_id: String,
    pub state_dir: PathBuf,
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub found: Vec<Found>,
    pub spawn: SpawnMode,
    pub capacity: usize,
    pub permission_mode: PermissionMode,
    pub name_prefix: String,
    pub idle_timeout: Duration,
}

struct Session {
    tx: mpsc::UnboundedSender<IncomingCommand>,
    state: Arc<SessionState>,
    task: JoinHandle<()>,
    worktree: Option<Worktree>,
    /// Where the session runs.
    cwd: PathBuf,
}

type Reject = (CommandErrorCode, String);

pub(super) struct Router {
    ctx: Context,
    sink: TunnelSink,
    commands: TunnelCommands,
    projects: Projects,
    sessions: HashMap<String, Session>,
    processed_tx: mpsc::UnboundedSender<(u64, String)>,
    processed_rx: mpsc::UnboundedReceiver<(u64, String)>,
    ended_tx: mpsc::UnboundedSender<(String, Option<String>)>,
    ended_rx: mpsc::UnboundedReceiver<(String, Option<String>)>,
    /// `sm rc close` requests from the control socket.
    local: mpsc::UnboundedReceiver<local::Request>,
}

/// A thread id is a directory name: nothing that could leave the sessions directory.
fn valid_thread_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn thread_of(command: &Command) -> &str {
    match command {
        Command::Prompt { thread_id, .. }
        | Command::Approval { thread_id, .. }
        | Command::Dispatch { thread_id, .. }
        | Command::Cancel { thread_id, .. }
        | Command::SessionConfig { thread_id, .. } => thread_id,
    }
}

impl Router {
    pub fn new(
        ctx: Context,
        sink: TunnelSink,
        commands: TunnelCommands,
        projects: Projects,
        local: mpsc::UnboundedReceiver<local::Request>,
    ) -> Self {
        let (processed_tx, processed_rx) = mpsc::unbounded_channel();
        let (ended_tx, ended_rx) = mpsc::unbounded_channel();
        Self {
            ctx,
            sink,
            commands,
            projects,
            sessions: HashMap::new(),
            processed_tx,
            processed_rx,
            ended_tx,
            ended_rx,
            local,
        }
    }

    pub async fn run(
        mut self,
        continue_last: bool,
        shutdown: impl Future<Output = ()>,
        mut client_task: JoinHandle<Result<Exit>>,
        stop_tx: oneshot::Sender<()>,
    ) -> Result<()> {
        let mut client_result = None;
        let result = async {
            if continue_last {
                self.continue_last();
            }
            tokio::pin!(shutdown);
            let period = (self.ctx.idle_timeout / 4)
                .clamp(Duration::from_millis(10), Duration::from_secs(30));
            let mut idle_check = tokio::time::interval(period);
            idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = &mut shutdown => return Ok(()),
                    incoming = self.commands.next() => match incoming {
                        Some(incoming) => self.handle(incoming).await?,
                        None => return Ok(()),
                    },
                    Some((seq, key)) = self.processed_rx.recv() => {
                        self.commands.processed(seq, &key).await;
                    }
                    Some((thread_id, error)) = self.ended_rx.recv() => {
                        self.ended(&thread_id, error).await;
                    }
                    Some(request) = self.local.recv() => self.local_close(request).await,
                    _ = idle_check.tick() => self.end_idle_sessions().await,
                    joined = &mut client_task => {
                        client_result = Some(joined);
                        return Ok(());
                    }
                }
            }
        }
        .await;
        self.wind_down().await;
        match client_result {
            Some(joined) => {
                let exit = joined.map_err(|e| anyhow::anyhow!("tunnel task failed: {e}"))?;
                result?;
                exit.map(|_| ())
            }
            None => {
                let _ = stop_tx.send(());
                let _ = tokio::time::timeout(WIND_DOWN, client_task).await;
                result
            }
        }
    }

    // ---- commands ---------------------------------------------------------------------------

    async fn handle(&mut self, incoming: IncomingCommand) -> Result<()> {
        let thread_id = thread_of(&incoming.command).to_owned();
        if !valid_thread_id(&thread_id) {
            return self
                .reject(
                    &incoming,
                    CommandErrorCode::Invalid,
                    format!("invalid thread id {thread_id:?}"),
                )
                .await;
        }
        let outcome = match &incoming.command {
            Command::Dispatch { .. } => self.dispatch(&incoming, &thread_id).await,
            Command::Cancel { .. } if self.is_idle(&thread_id) => {
                // Nothing runs: the cancel is the hub closing the session.
                self.close(&thread_id).await;
                self.commands
                    .processed(incoming.seq, &incoming.idempotency_key)
                    .await;
                Ok(())
            }
            _ => self.forward_existing(&incoming, &thread_id).await,
        };
        match outcome {
            Ok(()) => Ok(()),
            Err((code, message)) => self.reject(&incoming, code, message).await,
        }
    }

    /// Answers a command the router refuses: `command_result`, then the ack.
    async fn reject(
        &mut self,
        incoming: &IncomingCommand,
        code: CommandErrorCode,
        message: String,
    ) -> Result<()> {
        tracing::warn!(key = %incoming.idempotency_key, ?code, %message, "command rejected");
        self.sink
            .command_result(CommandFailure {
                idempotency_key: incoming.idempotency_key.clone(),
                code,
                message,
            })
            .await?;
        self.commands
            .processed(incoming.seq, &incoming.idempotency_key)
            .await;
        Ok(())
    }

    fn running(&self) -> usize {
        self.sessions
            .values()
            .filter(|session| session.state.active())
            .count()
    }

    fn check_capacity(&self, thread_id: &str) -> Result<(), Reject> {
        let idle = self
            .sessions
            .get(thread_id)
            .is_none_or(|session| !session.state.active());
        if idle && self.running() >= self.ctx.capacity {
            return Err((
                CommandErrorCode::Capacity,
                format!("{} sessions already running", self.running()),
            ));
        }
        Ok(())
    }

    fn find(&self, harness: &str) -> Result<Found, Reject> {
        self.ctx
            .found
            .iter()
            .find(|found| found.harness == harness)
            .cloned()
            .ok_or_else(|| {
                (
                    CommandErrorCode::HarnessMissing,
                    format!("harness {harness:?} is not installed on this machine"),
                )
            })
    }

    async fn dispatch(
        &mut self,
        incoming: &IncomingCommand,
        thread_id: &str,
    ) -> Result<(), Reject> {
        let Command::Dispatch {
            agent, workspace, ..
        } = &incoming.command
        else {
            return Ok(());
        };
        let found = self.find(agent)?;
        self.check_capacity(thread_id)?;
        let mut command = incoming.command.clone();
        if !self.sessions.contains_key(thread_id) {
            let requested = PathBuf::from(&workspace.path);
            if !requested.is_dir() {
                return Err((
                    CommandErrorCode::Invalid,
                    format!("workspace {} is not a directory", requested.display()),
                ));
            }
            let mode = workspace
                .spawn
                .as_deref()
                .and_then(SpawnMode::parse)
                .unwrap_or(self.ctx.spawn);
            let name = names::session_name(&self.ctx.name_prefix);
            let (worktree, cwd) = match mode {
                SpawnMode::SameDir => (None, requested.clone()),
                SpawnMode::Worktree => {
                    let root = self.ctx.state_dir.join("worktrees");
                    match Worktree::add(&requested, &root, &name).await {
                        Ok((worktree, cwd)) => (Some(worktree), cwd),
                        Err(error) => {
                            return Err((
                                CommandErrorCode::Invalid,
                                format!("worktree for {}: {error:#}", requested.display()),
                            ));
                        }
                    }
                }
            };
            let store = open_store(&self.ctx.state_dir, thread_id)?;
            self.start_session(thread_id, &found, cwd.clone(), worktree, store)
                .await?;
            let last = LastSession {
                thread_id: thread_id.to_owned(),
                name,
                harness: found.harness.to_owned(),
                workspace: requested.clone(),
                cwd,
            };
            if let Err(error) = self.projects.set_last_session(&requested, &last) {
                tracing::warn!("could not save the last session: {error:#}");
            }
        }
        // The session may run in a worktree: it is the session's directory that counts.
        if let (Command::Dispatch { workspace, .. }, Some(session)) =
            (&mut command, self.sessions.get(thread_id))
        {
            *workspace = Workspace {
                path: session.cwd.to_string_lossy().into_owned(),
                ..workspace.clone()
            };
        }
        self.send(thread_id, incoming, command)
    }

    async fn forward_existing(
        &mut self,
        incoming: &IncomingCommand,
        thread_id: &str,
    ) -> Result<(), Reject> {
        if !self.sessions.contains_key(thread_id) {
            self.reopen(thread_id).await?;
        }
        if matches!(incoming.command, Command::Prompt { .. }) {
            self.check_capacity(thread_id)?;
        }
        self.send(thread_id, incoming, incoming.command.clone())
    }

    /// Hands `command` to the session. A command that starts a task counts against capacity
    /// from now on.
    fn send(
        &mut self,
        thread_id: &str,
        incoming: &IncomingCommand,
        command: Command,
    ) -> Result<(), Reject> {
        let Some(session) = self.sessions.get(thread_id) else {
            return Err((
                CommandErrorCode::UnknownThread,
                format!("unknown thread {thread_id}"),
            ));
        };
        if let Command::Prompt { task_id, .. } | Command::Dispatch { task_id, .. } = &command {
            session.state.begin(&incoming.idempotency_key, task_id);
        }
        let forwarded = IncomingCommand {
            seq: incoming.seq,
            idempotency_key: incoming.idempotency_key.clone(),
            command,
        };
        session.tx.send(forwarded).map_err(|_| {
            (
                CommandErrorCode::Invalid,
                format!("session {thread_id} has stopped"),
            )
        })
    }

    // ---- sessions ---------------------------------------------------------------------------

    /// Starts the session of a thread this runner has stored (after the session ended, or the
    /// runner restarted). A worktree session whose worktree was removed with it gets the worktree
    /// back on its branch; the bridge then resumes the agent's session.
    async fn reopen(&mut self, thread_id: &str) -> Result<(), Reject> {
        let unknown = || {
            (
                CommandErrorCode::UnknownThread,
                format!("unknown thread {thread_id}"),
            )
        };
        let dir = session_dir(&self.ctx.state_dir, thread_id);
        if !dir.is_dir() {
            return Err(unknown());
        }
        let store = open_store(&self.ctx.state_dir, thread_id)?;
        let row = store
            .thread(thread_id)
            .map_err(|e| (CommandErrorCode::Invalid, format!("session store: {e}")))?
            .ok_or_else(unknown)?;
        let found = self.find(&row.harness)?;
        let cwd = PathBuf::from(&row.cwd);
        let gone = || {
            (
                CommandErrorCode::Invalid,
                format!("workspace {} is gone", cwd.display()),
            )
        };
        let worktree = if cwd.is_dir() {
            None
        } else {
            let worktree = Worktree::load(&dir).ok_or_else(gone)?;
            worktree.restore().await.map_err(|error| {
                (
                    CommandErrorCode::Invalid,
                    format!(
                        "workspace {} is gone and its worktree could not be added back: {error:#}",
                        cwd.display()
                    ),
                )
            })?;
            if !cwd.is_dir() {
                worktree.remove().await;
                return Err(gone());
            }
            Some(worktree)
        };
        self.start_session(thread_id, &found, cwd, worktree, store)
            .await
    }

    /// `-c`: the last session of the current directory is started again and registered with the
    /// hub as a session started on this runner.
    fn continue_last(&mut self) {
        let Some(last) = self.projects.last_session(&self.ctx.cwd) else {
            tracing::warn!("no earlier session for {}", self.ctx.cwd.display());
            return;
        };
        let started = (|| -> Result<(), String> {
            let found = self.find(&last.harness).map_err(|(_, m)| m)?;
            // A worktree that was removed with its session: carry on in the project itself.
            let cwd = if last.cwd.is_dir() {
                last.cwd.clone()
            } else {
                last.workspace.clone()
            };
            let store = open_store(&self.ctx.state_dir, &last.thread_id).map_err(|(_, m)| m)?;
            store
                .upsert_thread(&last.thread_id, &last.harness, &cwd.to_string_lossy())
                .map_err(|e| e.to_string())?;
            // The hub learns of the thread before its first event (tunnel.md §3.4).
            self.sink.mark_local_thread(&last.thread_id);
            self.sink
                .thread_open(
                    &last.thread_id,
                    &last.name,
                    &last.harness,
                    &Workspace {
                        path: last.workspace.to_string_lossy().into_owned(),
                        branch: None,
                        spawn: None,
                    },
                )
                .map_err(|e| e.to_string())?;
            self.spawn_session(&last.thread_id, &found, cwd, None, store)
                .map_err(|(_, m)| m)
        })();
        if let Err(message) = started {
            tracing::warn!("could not continue the last session: {message}");
        }
    }

    async fn start_session(
        &mut self,
        thread_id: &str,
        found: &Found,
        cwd: PathBuf,
        worktree: Option<Worktree>,
        store: Arc<Store>,
    ) -> Result<(), Reject> {
        // The record lets a prompt after the session ends add the worktree back.
        let started = match &worktree {
            Some(worktree) => worktree
                .save(&session_dir(&self.ctx.state_dir, thread_id))
                .map_err(|e| {
                    (
                        CommandErrorCode::Invalid,
                        format!("save the worktree: {e:#}"),
                    )
                }),
            None => Ok(()),
        }
        .and_then(|()| self.spawn_session(thread_id, found, cwd, worktree.clone(), store));
        match started {
            Ok(()) => Ok(()),
            Err(error) => {
                if let Some(worktree) = worktree {
                    worktree.remove().await;
                }
                Err(error)
            }
        }
    }

    fn spawn_session(
        &mut self,
        thread_id: &str,
        found: &Found,
        cwd: PathBuf,
        worktree: Option<Worktree>,
        store: Arc<Store>,
    ) -> Result<(), Reject> {
        let mut config = Config::new(self.ctx.machine_id.clone(), found.harness, cwd.clone());
        // The runner's mode is in force before the first prompt (the bridge fails the task if
        // the agent does not take it). `default` is what an agent starts in.
        if self.ctx.permission_mode != PermissionMode::Default {
            config.initial_mode = Some(self.ctx.permission_mode.as_str().to_owned());
        }
        // http / sse servers need the agent's `mcp_capabilities`, known after `initialize`.
        let home = self.ctx.home.clone();
        let mcp_cwd = cwd.clone();
        config.mcp_provider =
            Some(McpProvider::new(
                move |_thread, caps| match harness::local_mcp_servers(
                    &home,
                    &mcp_cwd,
                    caps,
                    &harness::mcp::process_env(),
                ) {
                    Ok(servers) => servers,
                    Err(error) => {
                        tracing::warn!("no local MCP servers for the session: {error:#}");
                        Vec::new()
                    }
                },
            ));
        let state = Arc::new(SessionState::default());
        let (tx, rx) = mpsc::unbounded_channel();
        let sink = SessionSink {
            inner: self.sink.clone(),
            state: Arc::clone(&state),
        };
        let commands = SessionCommands {
            rx,
            processed: self.processed_tx.clone(),
        };
        let spawn_found = found.clone();
        let spawn_cwd = cwd.clone();
        let bridge = Bridge::spawn_with(
            config,
            move || tokio::process::Command::from(harness::spawn_command(&spawn_found, &spawn_cwd)),
            store,
            Arc::new(UlidIds::new()),
            sink,
            commands,
        )
        .map_err(|error| {
            (
                CommandErrorCode::HarnessMissing,
                format!("could not start {}: {error}", found.harness),
            )
        })?;
        let ended = self.ended_tx.clone();
        let id = thread_id.to_owned();
        let task = tokio::spawn(async move {
            let error = bridge.run().await.err().map(|e| e.to_string());
            let _ = ended.send((id, error));
        });
        self.sessions.insert(
            thread_id.to_owned(),
            Session {
                tx,
                state,
                task,
                worktree,
                cwd,
            },
        );
        Ok(())
    }

    /// A bridge stopped by itself: its session is over.
    async fn ended(&mut self, thread_id: &str, error: Option<String>) {
        if let Some(error) = error {
            tracing::warn!(%thread_id, "session stopped: {error}");
        }
        // A session started again under the same id is not the one that ended.
        if self
            .sessions
            .get(thread_id)
            .is_some_and(|session| session.task.is_finished())
            && let Some(session) = self.sessions.remove(thread_id)
            && let Some(worktree) = session.worktree
        {
            worktree.remove().await;
        }
    }

    /// The thread has a session and no task.
    fn is_idle(&self, thread_id: &str) -> bool {
        self.sessions
            .get(thread_id)
            .is_some_and(|session| !session.state.active())
    }

    /// Ends the session of `thread_id`: the bridge stops (and its agent with it), the clean
    /// worktree goes, and the slot is free. The thread stays stored: a later prompt reopens it.
    async fn close(&mut self, thread_id: &str) {
        let Some(Session {
            tx, task, worktree, ..
        }) = self.sessions.remove(thread_id)
        else {
            return;
        };
        drop(tx);
        let mut task = task;
        if tokio::time::timeout(WIND_DOWN, &mut task).await.is_err() {
            tracing::warn!(%thread_id, "session did not stop in time");
            task.abort();
            let _ = task.await;
        }
        if let Some(worktree) = worktree {
            worktree.remove().await;
        }
    }

    /// `sm rc close`: ends the idle session of the thread, as an idle close does. A session with
    /// a task running is left alone.
    async fn local_close(&mut self, request: local::Request) {
        let thread_id = request.thread_id;
        let outcome = if !valid_thread_id(&thread_id) {
            Err(format!("invalid thread id {thread_id:?}"))
        } else if !self.sessions.contains_key(&thread_id) {
            Err(format!("no open session for {thread_id}"))
        } else if !self.is_idle(&thread_id) {
            Err(format!("{thread_id} has a task running"))
        } else {
            tracing::info!(%thread_id, "session closed by `sm rc close`");
            self.close(&thread_id).await;
            Ok(())
        };
        let _ = request.reply.send(outcome);
    }

    /// Ends the sessions that have had no task for `--idle-timeout`.
    async fn end_idle_sessions(&mut self) {
        let timeout = self.ctx.idle_timeout;
        let idle: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.state.idle_for(timeout))
            .map(|(id, _)| id.clone())
            .collect();
        for thread_id in idle {
            tracing::info!(%thread_id, "session ended after the idle timeout");
            self.close(&thread_id).await;
        }
    }

    /// Ends every session: the bridges stop (and their agents with them), then the worktrees go.
    async fn wind_down(&mut self) {
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        for thread_id in ids {
            self.close(&thread_id).await;
        }
    }
}

fn open_store(state_dir: &std::path::Path, thread_id: &str) -> Result<Arc<Store>, Reject> {
    Store::open(&session_dir(state_dir, thread_id))
        .map(Arc::new)
        .map_err(|e| (CommandErrorCode::Invalid, format!("session store: {e}")))
}
