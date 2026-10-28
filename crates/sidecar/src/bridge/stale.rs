//! Interrupts an earlier process left open, found at startup (SPEC-004, turn-event.md §4.9).
//!
//! The ACP request each one waited on died with the agent that asked, so no answer can reach
//! it. Its task ends with `InterruptResolved{cancelled}` and then `RunError{error,
//! code:"harness_exited"}`. `expired` is only for an approval TTL that ran out
//! ([`crate::acp::reattach`]).

use protocol::acp::{CreateElicitationRequest, RequestPermissionRequest};
use protocol::mapping::acp_turn::{AcpTurnMapper, IdSource, SequentialIds, StartTask};
use protocol::turn_event::Envelope;

use crate::acp::reattach::rfc3339;
use crate::store::{InterruptKind, PendingInterrupt, Store, StoreError};

/// The `RunError` code for a task whose agent went away.
const HARNESS_EXITED: &str = "harness_exited";

/// The `RunError` message for a task ended by [`end_interrupts_of_earlier_process`].
const HARNESS_EXITED_MESSAGE: &str = "the agent exited before the interrupt was answered";

/// The events that end one task whose interrupts an earlier process left open.
pub(super) struct Ended {
    pub thread_id: String,
    pub events: Vec<Envelope>,
}

/// Resolves every open interrupt in `store` as lost with its agent, and returns the events that
/// end each task (one `Ended` per task, in stored order). The caller sends them.
///
/// A task with several open interrupts is ended once, with all of them `cancelled`. An
/// interrupt whose stored request does not decode is resolved in the store but not reported
/// `cancelled`: the task still ends with `harness_exited`.
pub(super) fn end_interrupts_of_earlier_process(
    store: &Store,
    now: u64,
) -> Result<Vec<Ended>, StoreError> {
    let ts = rfc3339(now);
    let mut tasks: Vec<(String, AcpTurnMapper, String)> = Vec::new();
    for interrupt in store.open_interrupts()? {
        store.resolve_interrupt(&interrupt.interrupt_id)?;
        let index = match tasks
            .iter()
            .position(|(task, _, _)| *task == interrupt.task_id)
        {
            Some(index) => index,
            None => {
                // The paused state is rebuilt from the stored request; its events were sent
                // before the restart.
                let mut mapper = stale_mapper(&interrupt, now);
                mapper.start_task(StartTask::new(interrupt.task_id.clone()), &ts);
                tasks.push((
                    interrupt.task_id.clone(),
                    mapper,
                    interrupt.thread_id.clone(),
                ));
                tasks.len() - 1
            }
        };
        if let Err(error) = pause_on(&mut tasks[index].1, &interrupt, &ts) {
            tracing::warn!(%error, interrupt_id = %interrupt.interrupt_id, "stored interrupt is malformed");
        }
    }
    Ok(tasks
        .into_iter()
        .map(|(_, mut mapper, thread_id)| Ended {
            thread_id,
            events: mapper.on_error(HARNESS_EXITED, HARNESS_EXITED_MESSAGE, &ts),
        })
        .collect())
}

/// The mapper that rebuilds a stored interrupt's paused state. The expiry functions want the
/// default id source, so skip ahead to make a run id unlikely to repeat one the thread already
/// used.
pub(super) fn stale_mapper(interrupt: &PendingInterrupt, now: u64) -> AcpTurnMapper {
    let mut ids = SequentialIds::default();
    for _ in 0..1_000 + (now / 1_000) % 100_000 {
        ids.next_id("run_");
    }
    AcpTurnMapper::new(interrupt.thread_id.clone(), ids)
}

/// Pauses `mapper` on the stored request of `interrupt`. The events are discarded.
fn pause_on(
    mapper: &mut AcpTurnMapper,
    interrupt: &PendingInterrupt,
    ts: &str,
) -> Result<(), serde_json::Error> {
    let expires_at = rfc3339(interrupt.expires_at_ms);
    match interrupt.kind {
        InterruptKind::Approval => {
            let request: RequestPermissionRequest =
                serde_json::from_value(interrupt.request.clone())?;
            mapper.on_request_permission(&request, &interrupt.interrupt_id, &expires_at, ts);
        }
        InterruptKind::Form => {
            let request: CreateElicitationRequest =
                serde_json::from_value(interrupt.request.clone())?;
            mapper.on_elicitation(&request, &interrupt.interrupt_id, &expires_at, ts);
        }
    }
    Ok(())
}
