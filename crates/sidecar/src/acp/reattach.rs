//! Re-attaching to ACP sessions after a restart, and expiring interrupts that waited past their
//! approval TTL (PLAN-v2 §4.1, turn-event.md §4.9 and §5 rule 6).
//!
//! The session table lives in the [`Store`]. After a restart or cold wake the agent process is
//! new and knows no sessions, so the next message for a known thread calls `session/resume`
//! (fallback `session/load`) before the prompt. [`Reattacher`] does that on top of
//! [`AcpClient`].

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use protocol::acp::{
    ContentBlock, CreateElicitationRequest, McpServer, NewSessionResponse,
    RequestPermissionRequest, SessionId, StopReason,
};
use protocol::mapping::acp_turn::{AcpTurnMapper, StartTask};
use protocol::turn_event::Envelope;

use super::client::{AcpClient, ClientError, ClientHandler};
use crate::store::{InterruptKind, PendingInterrupt, SessionState, Store, StoreError};

/// How a thread's session was obtained for a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attach {
    /// First prompt of the thread: `session/new`.
    New,
    /// Already attached on this connection: nothing sent.
    Live,
    /// `session/resume` succeeded.
    Resumed,
    /// `session/resume` was unsupported or failed, and `session/load` succeeded.
    Loaded,
}

/// A thread's session on one connection.
#[derive(Debug, Clone)]
pub struct Attached {
    /// The ACP session.
    pub session: SessionId,
    /// How it was obtained.
    pub how: Attach,
    /// What the agent answered to `session/new`, `session/resume` or `session/load` (its modes
    /// and config options); `None` when the session was already attached.
    pub response: Option<NewSessionResponse>,
}

/// Errors from re-attaching.
#[derive(Debug, thiserror::Error)]
pub enum ReattachError {
    /// The agent connection failed.
    #[error(transparent)]
    Client(#[from] ClientError),
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Both `session/resume` and `session/load` were refused. The stored session is kept (the
    /// caller decides whether to start a new one), so no history is dropped silently.
    #[error("session {session_id} could not be re-attached: resume: {resume}; load: {load}")]
    Unrecoverable {
        /// The stored ACP session id.
        session_id: String,
        /// Why `session/resume` failed.
        resume: String,
        /// Why `session/load` failed.
        load: String,
    },
    /// A stored interrupt request could not be decoded.
    #[error("stored interrupt {interrupt_id} is malformed: {source}")]
    BadInterrupt {
        /// The interrupt id.
        interrupt_id: String,
        /// The decode error.
        #[source]
        source: serde_json::Error,
    },
}

/// Attaches threads to sessions of one agent connection.
///
/// Use one `Reattacher` per [`AcpClient`]; call [`Reattacher::connection_lost`] (or make a
/// new one) when the agent process is replaced.
pub struct Reattacher {
    store: Arc<Store>,
    attached: Mutex<HashSet<String>>,
}

impl Reattacher {
    /// A reattacher backed by `store`.
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            attached: Mutex::new(HashSet::new()),
        }
    }

    /// Forgets which sessions are attached and marks the stored active ones `detached`. Call
    /// when the agent process exits or is replaced.
    pub fn connection_lost(&self) -> Result<(), StoreError> {
        self.attached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.store.detach_all_sessions()?;
        Ok(())
    }

    /// Makes sure `thread_id` has an attached session on `client` and returns it. See
    /// [`Reattacher::attach`]; no MCP servers are passed.
    pub async fn ensure_session<H: ClientHandler>(
        &self,
        client: &AcpClient<H>,
        thread_id: &str,
        harness: &str,
        cwd: &Path,
    ) -> Result<(SessionId, Attach), ReattachError> {
        let attached = self.attach(client, thread_id, harness, cwd, &[]).await?;
        Ok((attached.session, attached.how))
    }

    /// Makes sure `thread_id` has an attached session on `client`, with `mcp_servers` passed to
    /// the agent.
    ///
    /// - no stored session (or a closed one): `session/new`, stored as active;
    /// - stored session not yet attached on this connection: `session/resume`, and if the
    ///   agent answers with an error (including "method not found"), `session/load`;
    /// - already attached: nothing is sent.
    pub async fn attach<H: ClientHandler>(
        &self,
        client: &AcpClient<H>,
        thread_id: &str,
        harness: &str,
        cwd: &Path,
        mcp_servers: &[McpServer],
    ) -> Result<Attached, ReattachError> {
        self.store
            .upsert_thread(thread_id, harness, &cwd.to_string_lossy())?;
        let stored = self.store.session_for_thread(thread_id)?;
        let Some(row) = stored.filter(|row| row.state != SessionState::Closed) else {
            let response = client
                .new_session_response(cwd, mcp_servers.to_vec())
                .await?;
            self.record(thread_id, harness, &response.session_id, cwd)?;
            return Ok(Attached {
                session: response.session_id.clone(),
                how: Attach::New,
                response: Some(response),
            });
        };
        let session = SessionId::new(row.session_id.clone());
        if self.is_attached(&row.session_id) {
            return Ok(Attached {
                session,
                how: Attach::Live,
                response: None,
            });
        }
        let cwd = Path::new(&row.cwd);
        let mut response = NewSessionResponse::new(session.clone());
        let how = match client
            .resume_session_with(&session, cwd, mcp_servers.to_vec())
            .await
        {
            Ok(resumed) => {
                response.modes = resumed.modes;
                response.config_options = resumed.config_options;
                Attach::Resumed
            }
            Err(ClientError::Rpc { code, message }) => {
                let resume = format!("agent error {code}: {message}");
                match client
                    .load_session_with(&session, cwd, mcp_servers.to_vec())
                    .await
                {
                    Ok(loaded) => {
                        response.modes = loaded.modes;
                        response.config_options = loaded.config_options;
                        Attach::Loaded
                    }
                    Err(ClientError::Rpc { code, message }) => {
                        return Err(ReattachError::Unrecoverable {
                            session_id: row.session_id,
                            resume,
                            load: format!("agent error {code}: {message}"),
                        });
                    }
                    Err(other) => return Err(other.into()),
                }
            }
            Err(other) => return Err(other.into()),
        };
        self.record(thread_id, harness, &session, cwd)?;
        Ok(Attached {
            session,
            how,
            response: Some(response),
        })
    }

    /// Re-attaches if needed (see [`Reattacher::ensure_session`]), then sends the prompt.
    pub async fn prompt<H: ClientHandler>(
        &self,
        client: &AcpClient<H>,
        thread_id: &str,
        harness: &str,
        cwd: &Path,
        prompt: Vec<ContentBlock>,
    ) -> Result<(StopReason, Attach), ReattachError> {
        let (session, how) = self.ensure_session(client, thread_id, harness, cwd).await?;
        let stop = client.prompt(&session, prompt).await?;
        Ok((stop, how))
    }

    fn is_attached(&self, session_id: &str) -> bool {
        self.attached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(session_id)
    }

    fn record(
        &self,
        thread_id: &str,
        harness: &str,
        session: &SessionId,
        cwd: &Path,
    ) -> Result<(), StoreError> {
        self.store.upsert_session(
            thread_id,
            harness,
            &session.0,
            &cwd.to_string_lossy(),
            SessionState::Active,
        )?;
        self.attached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(session.0.to_string());
        Ok(())
    }
}

/// An interrupt that outlived its approval TTL, with the events that end its task.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpiredRun {
    /// The interrupt, as stored.
    pub interrupt: PendingInterrupt,
    /// `InterruptResolved{expired}`, the short resume run, and `RunError{error, code:"expired"}`
    /// (turn-event.md §5 rule 6), ready for the tunnel outbox.
    pub events: Vec<Envelope>,
}

/// Expires every stored interrupt whose deadline has passed by the store's clock.
///
/// Call at startup (after the store is reopened) and on a timer. Each expired interrupt is
/// reported once. `mapper_for` supplies the turn mapper for the interrupt's thread; the mapper
/// is rebuilt into the paused state from the stored request, then
/// [`AcpTurnMapper::on_interrupt_expired`] ends the task. The task is stored as `failed`.
pub fn expire_overdue_interrupts(
    store: &Store,
    mut mapper_for: impl FnMut(&PendingInterrupt) -> AcpTurnMapper,
) -> Result<Vec<ExpiredRun>, ReattachError> {
    store
        .take_expired_interrupts()?
        .into_iter()
        .map(|interrupt| end_interrupt(store, interrupt, &mut mapper_for))
        .collect()
}

fn end_interrupt(
    store: &Store,
    interrupt: PendingInterrupt,
    mapper_for: &mut impl FnMut(&PendingInterrupt) -> AcpTurnMapper,
) -> Result<ExpiredRun, ReattachError> {
    let ts = rfc3339(store.now_ms());
    let expires_at = rfc3339(interrupt.expires_at_ms);
    let mut mapper = mapper_for(&interrupt);
    // Rebuild the paused state; the events were already sent before the restart.
    mapper.start_task(StartTask::new(interrupt.task_id.clone()), &ts);
    let bad = |source| ReattachError::BadInterrupt {
        interrupt_id: interrupt.interrupt_id.clone(),
        source,
    };
    match interrupt.kind {
        InterruptKind::Approval => {
            let request: RequestPermissionRequest =
                serde_json::from_value(interrupt.request.clone()).map_err(bad)?;
            mapper.on_request_permission(&request, &interrupt.interrupt_id, &expires_at, &ts);
        }
        InterruptKind::Form => {
            let request: CreateElicitationRequest =
                serde_json::from_value(interrupt.request.clone()).map_err(bad)?;
            mapper.on_elicitation(&request, &interrupt.interrupt_id, &expires_at, &ts);
        }
    }
    let events = mapper.on_interrupt_expired(&interrupt.interrupt_id, &ts);
    store.upsert_task(&interrupt.task_id, &interrupt.thread_id, "failed")?;
    Ok(ExpiredRun { interrupt, events })
}

/// Unix milliseconds as RFC 3339 UTC with milliseconds (`2026-01-02T03:04:05.678Z`).
pub fn rfc3339(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}
