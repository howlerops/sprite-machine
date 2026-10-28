//! Interrupts: opening one for an agent request, and answering it from an `approval` command.
//!
//! `session/request_permission` becomes `Interrupt{approval}`; the elicitation module does the
//! same for `elicitation/create`. [`Bridge::approve`] resolves either kind, using the outcome
//! table of tunnel.md §3.5. An answer that does not fit the interrupt's kind answers the agent
//! `cancelled` and is reported `invalid`. A second answer, or one after cancel or expiry, finds
//! no open interrupt and is reported `unknown_interrupt` (the hub maps it to 409).

use protocol::acp::{
    CreateElicitationResponse, ElicitationAction, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
};
use protocol::tunnel::{CommandErrorCode, Outcome};
use serde_json::Value;
use tokio::sync::oneshot;

use super::elicitation::elicitation_response;
use super::io::{CommandSource, EventSink};
use super::{Bridge, BridgeError, PendingApproval};
use crate::acp::reattach::rfc3339;
use crate::store::{InterruptKind, PendingInterrupt};

/// An interrupt just registered: its ids and deadline.
pub(super) struct Opened {
    pub thread_id: String,
    pub interrupt_id: String,
    pub expires_at_ms: u64,
}

/// What `outcome` means to a permission request: `None` when it does not fit.
fn permission_response(outcome: &Outcome) -> Option<RequestPermissionResponse> {
    let outcome = match outcome {
        Outcome::Selected { option_id } => {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id.clone()))
        }
        Outcome::Cancelled => RequestPermissionOutcome::Cancelled,
        Outcome::Submitted { .. } | Outcome::Declined => return None,
    };
    Some(RequestPermissionResponse::new(outcome))
}

impl<S: EventSink, C: CommandSource> Bridge<S, C> {
    /// Registers an interrupt for the running task of `session`: stored with its deadline and
    /// tracked until it is answered, cancelled or expires. `None` when no task is running (the
    /// caller then drops its reply, which answers the agent `cancelled`).
    pub(super) fn open_interrupt(
        &mut self,
        session: &str,
        kind: InterruptKind,
        request: Value,
        reply: oneshot::Sender<RequestPermissionResponse>,
    ) -> Result<Option<Opened>, BridgeError> {
        let thread_id = self.by_session.get(session).cloned();
        let task = thread_id
            .as_ref()
            .and_then(|id| self.threads.get(id))
            .and_then(|thread| thread.mapper.task_id().map(str::to_owned));
        let (Some(thread_id), Some(task_id)) = (thread_id, task) else {
            tracing::warn!(%session, "interrupt request outside a running task; cancelled");
            return Ok(None);
        };
        let interrupt_id = self.ids.next_id("int_");
        let expires_at_ms = self.now_ms().saturating_add(
            u64::try_from(self.config.approval_ttl.as_millis()).unwrap_or(u64::MAX),
        );
        self.store.save_interrupt(&PendingInterrupt {
            interrupt_id: interrupt_id.clone(),
            thread_id: thread_id.clone(),
            task_id: task_id.clone(),
            kind,
            request,
            expires_at_ms,
        })?;
        self.approvals.insert(
            interrupt_id.clone(),
            PendingApproval {
                thread_id: thread_id.clone(),
                task_id,
                reply,
                expires_at_ms,
            },
        );
        Ok(Some(Opened {
            thread_id,
            interrupt_id,
            expires_at_ms,
        }))
    }

    /// `session/request_permission`: `Interrupt{approval}` + `RunFinished{interrupt}`.
    pub(super) async fn on_permission(
        &mut self,
        request: RequestPermissionRequest,
        reply: oneshot::Sender<RequestPermissionResponse>,
    ) -> Result<(), BridgeError> {
        let session = request.session_id.0.to_string();
        let payload = serde_json::to_value(&request).unwrap_or(Value::Null);
        let Some(opened) =
            self.open_interrupt(&session, InterruptKind::Approval, payload, reply)?
        else {
            return Ok(());
        };
        let ts = self.ts();
        let events = match self.threads.get_mut(&opened.thread_id) {
            Some(thread) => thread.mapper.on_request_permission(
                &request,
                &opened.interrupt_id,
                &rfc3339(opened.expires_at_ms),
                &ts,
            ),
            None => Vec::new(),
        };
        self.emit(&opened.thread_id, events).await
    }

    /// The `approval` command: answers the agent's request and emits the resume run.
    pub(super) async fn approve(
        &mut self,
        key: &str,
        thread_id: &str,
        task_id: &str,
        interrupt_id: &str,
        outcome: Outcome,
    ) -> Result<(), BridgeError> {
        if !self.threads.contains_key(thread_id) {
            // A thread this process has not loaded. When the store knows it, the interrupt is one
            // an earlier process left (already ended at startup): not open, so `unknown_interrupt`.
            let (code, message) = if self.store.thread(thread_id)?.is_some() {
                (
                    CommandErrorCode::UnknownInterrupt,
                    format!(
                        "no open interrupt {interrupt_id} in thread {thread_id} (not loaded here)"
                    ),
                )
            } else {
                (
                    CommandErrorCode::UnknownThread,
                    format!("unknown thread {thread_id}"),
                )
            };
            return self.reject(key, code, message).await;
        }
        let open = self
            .approvals
            .get(interrupt_id)
            .is_some_and(|pending| pending.thread_id == thread_id && pending.task_id == task_id);
        let surfaced = open
            && self
                .threads
                .get_mut(thread_id)
                .is_some_and(|thread| thread.mapper.note_interrupt_resolved(interrupt_id));
        let pending = if surfaced {
            self.approvals.remove(interrupt_id)
        } else {
            None
        };
        let Some(pending) = pending else {
            return self
                .reject(
                    key,
                    CommandErrorCode::UnknownInterrupt,
                    format!(
                        "no open interrupt {interrupt_id} in thread {thread_id} \
                         (already resolved, cancelled or expired)"
                    ),
                )
                .await;
        };
        self.store.resolve_interrupt(interrupt_id)?;
        let ts = self.ts();
        let mut events = match self.threads.get_mut(thread_id) {
            Some(thread) => thread.mapper.resume(&ts),
            None => Vec::new(),
        };
        events.extend(self.release_held(thread_id));
        self.emit(thread_id, events).await?;

        // Answer the agent; an outcome that does not fit the kind is a `cancelled` answer.
        let fits = match self.handler.take_parked(interrupt_id) {
            Some(parked) => {
                let answer = elicitation_response(parked.kind, &outcome);
                let fits = answer.is_some();
                let answer = answer
                    .unwrap_or_else(|| CreateElicitationResponse::new(ElicitationAction::Cancel));
                if parked.reply.send(answer).is_err() {
                    tracing::debug!(%interrupt_id, "the agent stopped waiting for the form");
                }
                fits
            }
            None => {
                let answer = permission_response(&outcome);
                let fits = answer.is_some();
                let answer = answer.unwrap_or_else(|| {
                    RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
                });
                if pending.reply.send(answer).is_err() {
                    tracing::debug!(%interrupt_id, "the agent stopped waiting for the approval");
                }
                fits
            }
        };
        if fits {
            return Ok(());
        }
        self.reject(
            key,
            CommandErrorCode::Invalid,
            format!(
                "the outcome does not fit interrupt {interrupt_id}; the agent was answered cancelled"
            ),
        )
        .await
    }
}
