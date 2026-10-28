//! `elicitation/create` as an Interrupt: `form` mode becomes `Interrupt{form}` with the
//! request's schema, `url` mode `Interrupt{auth}` with the URL (turn-event.md §4.5). The answer
//! comes through [`Bridge::approve`](super::Bridge) and the outcome table of tunnel.md §3.5.

use protocol::acp::{
    CreateElicitationRequest, CreateElicitationResponse, ElicitationMode, ElicitationScope,
    RequestPermissionResponse,
};
use protocol::tunnel::Outcome;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::handler::{Parked, ParkedKind};
use super::io::{CommandSource, EventSink};
use super::{Bridge, BridgeError};
use crate::acp::reattach::rfc3339;
use crate::store::InterruptKind;

/// What `outcome` means to an elicitation of `kind`: `None` when it does not fit.
///
/// `submitted{data}` answers a form `accept(content)` (the data must be an object); `declined`
/// answers a form or auth `decline`; `cancelled` answers `cancel`.
pub(super) fn elicitation_response(
    kind: ParkedKind,
    outcome: &Outcome,
) -> Option<CreateElicitationResponse> {
    let action = match (kind, outcome) {
        (ParkedKind::Form, Outcome::Submitted { data }) => {
            let data = data.as_object()?;
            json!({ "action": "accept", "content": data })
        }
        (_, Outcome::Declined) => json!({ "action": "decline" }),
        (_, Outcome::Cancelled) => json!({ "action": "cancel" }),
        _ => return None,
    };
    serde_json::from_value(action).ok()
}

/// The ACP session of a session-scoped elicitation.
fn session_of(request: &CreateElicitationRequest) -> Option<String> {
    match request.scope() {
        ElicitationScope::Session(scope) => Some(scope.session_id.0.to_string()),
        _ => None,
    }
}

impl<S: EventSink, C: CommandSource> Bridge<S, C> {
    /// `elicitation/create`: `Interrupt{form|auth}` + `RunFinished{interrupt}`. A request outside
    /// a running task, outside a session or in an unknown mode is answered `cancel`.
    pub(super) async fn on_elicitation(
        &mut self,
        request: CreateElicitationRequest,
        reply: oneshot::Sender<CreateElicitationResponse>,
    ) -> Result<(), BridgeError> {
        let kind = match &request.mode {
            ElicitationMode::Form(_) => ParkedKind::Form,
            ElicitationMode::Url(_) => ParkedKind::Auth,
            _ => {
                tracing::warn!("elicitation in an unknown mode; cancelled");
                return Ok(());
            }
        };
        let Some(session) = session_of(&request) else {
            tracing::warn!("elicitation outside a session; cancelled");
            return Ok(());
        };
        let payload = serde_json::to_value(&request).unwrap_or(Value::Null);
        // The approvals table needs a permission-typed reply; the real one is parked on the
        // handler. This one has no receiver and is never used.
        let (placeholder, _) = oneshot::channel::<RequestPermissionResponse>();
        let Some(opened) =
            self.open_interrupt(&session, InterruptKind::Form, payload, placeholder)?
        else {
            return Ok(());
        };
        self.handler
            .park(&opened.interrupt_id, Parked { kind, reply });
        let ts = self.ts();
        let events = match self.threads.get_mut(&opened.thread_id) {
            Some(thread) => thread.mapper.on_elicitation(
                &request,
                &opened.interrupt_id,
                &rfc3339(opened.expires_at_ms),
                &ts,
            ),
            None => Vec::new(),
        };
        self.emit(&opened.thread_id, events).await
    }
}

#[cfg(test)]
mod tests {
    use protocol::acp::ElicitationAction;

    use super::*;

    #[test]
    fn outcomes_map_per_the_tunnel_table() {
        let form = ParkedKind::Form;
        let auth = ParkedKind::Auth;
        let data = Outcome::Submitted {
            data: json!({ "branch": "main" }),
        };
        let accept = elicitation_response(form, &data).expect("fits");
        assert!(matches!(accept.action, ElicitationAction::Accept(_)));
        assert!(elicitation_response(auth, &data).is_none());
        assert!(
            elicitation_response(
                form,
                &Outcome::Submitted {
                    data: json!("not an object")
                }
            )
            .is_none()
        );
        for kind in [form, auth] {
            let declined = elicitation_response(kind, &Outcome::Declined).expect("fits");
            assert!(matches!(declined.action, ElicitationAction::Decline));
            let cancelled = elicitation_response(kind, &Outcome::Cancelled).expect("fits");
            assert!(matches!(cancelled.action, ElicitationAction::Cancel));
            let selected = Outcome::Selected {
                option_id: "allow".into(),
            };
            assert!(elicitation_response(kind, &selected).is_none());
        }
    }
}
