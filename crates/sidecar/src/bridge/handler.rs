//! The bridge's [`ClientHandler`]: what the agent's requests turn into.
//!
//! - `fs/*` goes to the session's [`Workspace`], `terminal/*` to its [`Terminals`].
//! - `session/request_permission` becomes an [`Internal::Permission`] for the bridge loop, which
//!   emits the Interrupt and answers when the approval command arrives.
//! - `elicitation/create` (form or url) becomes an [`Internal::Elicitation`] the same way; the
//!   loop parks its reply here until the `approval` command arrives.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::anyhow;
use protocol::acp::{
    CreateElicitationRequest, CreateElicitationResponse, CreateTerminalRequest,
    CreateTerminalResponse, ElicitationAction, KillTerminalRequest, KillTerminalResponse,
    ReadTextFileRequest, ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, SessionId,
    TerminalOutputRequest, TerminalOutputResponse, WaitForTerminalExitRequest,
    WaitForTerminalExitResponse, WriteTextFileRequest, WriteTextFileResponse,
};
use protocol::turn_event::Envelope;
use tokio::sync::{mpsc, oneshot};

use crate::acp::client::ClientHandler;
use crate::acp::fs::Workspace;
use crate::acp::sessions::PromptResult;
use crate::acp::terminal::Terminals;

/// A message from a handler, a terminal thread or a prompt waiter to the bridge loop.
pub(crate) enum Internal {
    /// The agent asks for permission; the loop sends the answer on `reply`. Dropping `reply`
    /// answers the agent `cancelled`.
    Permission {
        request: RequestPermissionRequest,
        reply: oneshot::Sender<RequestPermissionResponse>,
    },
    /// The agent asks for input (form) or authorization (url); the loop answers on `reply`.
    /// Dropping `reply` answers the agent `cancel`.
    Elicitation {
        request: CreateElicitationRequest,
        reply: oneshot::Sender<CreateElicitationResponse>,
    },
    /// An `Activity{terminal}` envelope teed from a terminal of `thread_id`.
    Activity {
        thread_id: String,
        envelope: Envelope,
    },
    /// The prompt of `task_id` ended.
    PromptDone {
        thread_id: String,
        task_id: String,
        result: PromptResult,
    },
}

/// An open elicitation interrupt: its kind, and where its answer goes.
pub(crate) struct Parked {
    pub kind: ParkedKind,
    pub reply: oneshot::Sender<CreateElicitationResponse>,
}

/// The interrupt kind of a parked elicitation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParkedKind {
    Form,
    Auth,
}

/// What a session's `fs/*` and `terminal/*` requests run against.
pub(crate) struct Resources {
    pub workspace: Workspace,
    pub terminals: Terminals,
}

/// Shared between the bridge loop (which registers sessions) and the ACP client (which calls
/// the handler).
#[derive(Clone)]
pub(crate) struct BridgeHandler {
    tx: mpsc::UnboundedSender<Internal>,
    sessions: Arc<Mutex<HashMap<String, Arc<Resources>>>>,
    /// Open elicitation interrupts by interrupt id.
    parked: Arc<Mutex<HashMap<String, Parked>>>,
}

impl BridgeHandler {
    pub(crate) fn new(tx: mpsc::UnboundedSender<Internal>) -> Self {
        Self {
            tx,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            parked: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn table(&self) -> MutexGuard<'_, HashMap<String, Arc<Resources>>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keeps the reply of an open elicitation until its answer arrives.
    pub(crate) fn park(&self, interrupt_id: &str, parked: Parked) {
        self.parked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(interrupt_id.to_owned(), parked);
    }

    /// Takes the parked elicitation of `interrupt_id`; `None` for a permission or a closed one.
    pub(crate) fn take_parked(&self, interrupt_id: &str) -> Option<Parked> {
        self.parked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(interrupt_id)
    }

    /// Sets what `session`'s requests run against (replacing the previous task's terminals).
    pub(crate) fn register(&self, session: &str, resources: Resources) {
        self.table().insert(session.to_owned(), Arc::new(resources));
    }

    /// Forgets `session`'s resources; its terminals are killed once no request uses them.
    pub(crate) fn release(&self, session: &str) {
        self.table().remove(session);
    }

    fn resources(&self, session: &SessionId) -> anyhow::Result<Arc<Resources>> {
        self.table()
            .get(session.0.as_ref())
            .cloned()
            .ok_or_else(|| anyhow!("no workspace for session {}", session.0))
    }
}

impl ClientHandler for BridgeHandler {
    async fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> anyhow::Result<RequestPermissionResponse> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(Internal::Permission { request, reply })
            .map_err(|_| anyhow!("the bridge stopped"))?;
        // A dropped reply means the task ended (cancel, expiry) while the agent waited.
        Ok(answer.await.unwrap_or_else(|_| {
            RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
        }))
    }

    async fn create_elicitation(
        &self,
        request: CreateElicitationRequest,
    ) -> anyhow::Result<CreateElicitationResponse> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(Internal::Elicitation { request, reply })
            .map_err(|_| anyhow!("the bridge stopped"))?;
        Ok(answer
            .await
            .unwrap_or_else(|_| CreateElicitationResponse::new(ElicitationAction::Cancel)))
    }

    async fn read_text_file(
        &self,
        request: ReadTextFileRequest,
    ) -> anyhow::Result<ReadTextFileResponse> {
        let resources = self.resources(&request.session_id)?;
        Ok(resources.workspace.read_text_file(request).await?)
    }

    async fn write_text_file(
        &self,
        request: WriteTextFileRequest,
    ) -> anyhow::Result<WriteTextFileResponse> {
        let resources = self.resources(&request.session_id)?;
        Ok(resources.workspace.write_text_file(request).await?)
    }

    async fn create_terminal(
        &self,
        request: CreateTerminalRequest,
    ) -> anyhow::Result<CreateTerminalResponse> {
        let resources = self.resources(&request.session_id)?;
        let session = request.session_id.clone();
        Ok(resources.terminals.create(&session, request).await?)
    }

    async fn terminal_output(
        &self,
        request: TerminalOutputRequest,
    ) -> anyhow::Result<TerminalOutputResponse> {
        let resources = self.resources(&request.session_id)?;
        Ok(resources.terminals.output(request).await?)
    }

    async fn wait_for_terminal_exit(
        &self,
        request: WaitForTerminalExitRequest,
    ) -> anyhow::Result<WaitForTerminalExitResponse> {
        let resources = self.resources(&request.session_id)?;
        Ok(resources.terminals.wait_for_exit(request).await?)
    }

    async fn kill_terminal(
        &self,
        request: KillTerminalRequest,
    ) -> anyhow::Result<KillTerminalResponse> {
        let resources = self.resources(&request.session_id)?;
        Ok(resources.terminals.kill(request).await?)
    }

    async fn release_terminal(
        &self,
        request: ReleaseTerminalRequest,
    ) -> anyhow::Result<ReleaseTerminalResponse> {
        let resources = self.resources(&request.session_id)?;
        Ok(resources.terminals.release(request).await?)
    }
}
