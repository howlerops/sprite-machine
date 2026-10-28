//! The tracer bullet's headless client, as a thin adapter over the real client: the
//! subscription ([`crate::sync::Subscription`]) and the outbound queue ([`crate::outbox`]).
//!
//! It subscribes to one thread (turn-event.md §6), enqueues one prompt, answers the approval
//! interrupt with an allow option, then enqueues a second answer under a new key, which the hub
//! refuses with HTTP 409 (first answer wins, client-api.md §4). It returns every envelope it
//! applied once the task ends with `RunFinished{success}`.
//!
//! Envelopes are deduplicated by `seq`. A gap resubscribes from the last contiguous seq (the
//! real client's behaviour) instead of failing. A dropped socket is reconnected with backoff.

use std::time::Duration;

use anyhow::{Result, anyhow, bail, ensure};
use protocol::tunnel::{Actor, ContentPart, Outcome};
use protocol::turn_event::{Envelope, Event, InterruptKind, PermissionOptionKind, RunOutcome};

use crate::cache::Cache;
use crate::outbox::{
    Clock, FlushReport, HttpTransport, Intent, IntentState, Outbox, RetryPolicy, SystemClock,
    SystemRandom,
};
use crate::sync::{Applied, Subscription, SubscriptionError, ThreadSync};

/// Client settings.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// `http://host:port` of the hub (the tracer hub serves its routes without a prefix).
    pub http_base: String,
    pub thread_id: String,
    /// The machine that should host the thread.
    pub machine_id: String,
    pub task_id: String,
    pub prompt: String,
    pub user: Actor,
    /// Upper bound for the whole exchange.
    pub timeout: Duration,
}

/// What the client saw.
#[derive(Debug, Clone)]
pub struct Observed {
    /// Every applied envelope, in `seq` order.
    pub envelopes: Vec<Envelope>,
    /// HTTP status of the deliberate second answer to the interrupt (expected 409).
    pub second_answer_status: u16,
}

/// Reconnect attempts after a dropped socket before the exchange fails.
const MAX_RECONNECTS: u32 = 5;

/// Runs the whole client exchange; see the module docs.
pub async fn run(config: ClientConfig) -> Result<Observed> {
    let timeout = config.timeout;
    tokio::time::timeout(timeout, exchange(config))
        .await
        .map_err(|_| anyhow!("client did not see RunFinished{{success}} within {timeout:?}"))?
}

async fn exchange(config: ClientConfig) -> Result<Observed> {
    let base = config.http_base.trim_end_matches('/').to_owned();
    let thread = config.thread_id.clone();
    let seed = format!("{:x}", SystemClock.now_ms());
    let cache = Cache::open_in_memory()?;
    let mut outbox = Outbox::load(&cache, RetryPolicy::default())?;
    let transport = HttpTransport::new(None);
    let mut rng = SystemRandom;

    let mut subscription =
        Subscription::connect(&base, ThreadSync::new(thread.clone()), None).await?;

    // Catch-up: hello, snapshot, replayed events, live (§6).
    let mut envelopes = Vec::new();
    loop {
        match next_applied(&mut subscription, &mut rng).await? {
            Applied::Live => break,
            Applied::Event(envelope) => envelopes.push(*envelope),
            _ => {}
        }
    }

    let prompt = Intent::Prompt {
        thread_id: thread.clone(),
        task_id: config.task_id.clone(),
        message_id: format!("msg_u{seed}"),
        parts: vec![ContentPart::Text {
            text: config.prompt.clone(),
        }],
        machine_id: Some(config.machine_id.clone()),
        author: Some(config.user.clone()),
    };
    let prompt_id = outbox.enqueue(&cache, prompt, SystemClock.now_ms())?;
    send(&mut outbox, &cache, &transport, &base, &mut rng).await?;
    let prompt_status = status_of(&outbox, prompt_id)?;
    ensure!(prompt_status == 202, "prompt answered HTTP {prompt_status}");

    let mut second_answer_status = 0;
    loop {
        let applied = next_applied(&mut subscription, &mut rng).await?;
        let Applied::Event(envelope) = applied else {
            continue;
        };
        let envelope = *envelope;
        envelopes.push(envelope.clone());
        match &envelope.event {
            Event::Interrupt(interrupt) if interrupt.kind == InterruptKind::Approval => {
                let option_id = interrupt
                    .options
                    .iter()
                    .flatten()
                    .find(|option| {
                        matches!(
                            option.kind,
                            PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
                        )
                    })
                    .map(|option| option.option_id.clone())
                    .ok_or_else(|| anyhow!("approval interrupt without an allow option"))?;
                let answer = |actor: &Actor| Intent::Approval {
                    thread_id: thread.clone(),
                    interrupt_id: interrupt.interrupt_id.clone(),
                    outcome: Outcome::Selected {
                        option_id: option_id.clone(),
                    },
                    resolved_by: Some(actor.clone()),
                };
                let first = outbox.enqueue(&cache, answer(&config.user), SystemClock.now_ms())?;
                send(&mut outbox, &cache, &transport, &base, &mut rng).await?;
                let status = status_of(&outbox, first)?;
                ensure!(status == 200, "answer answered HTTP {status}");

                // A second device answering later loses (first answer wins).
                let late = outbox.enqueue(&cache, answer(&config.user), SystemClock.now_ms())?;
                send(&mut outbox, &cache, &transport, &base, &mut rng).await?;
                second_answer_status = status_of(&outbox, late)?;
                for conflict in outbox.conflicts() {
                    if let Some(winner) = conflict.resolved_by {
                        subscription
                            .sync_mut()
                            .view_mut()
                            .note_conflict(&conflict.interrupt_id, winner);
                    }
                }
            }
            Event::RunFinished(finished)
                if matches!(finished.outcome, RunOutcome::Success { .. }) =>
            {
                return Ok(Observed {
                    envelopes,
                    second_answer_status,
                });
            }
            Event::RunError(error) => bail!("run failed: {error:?}"),
            _ => {}
        }
    }
}

/// Reads the next message. A dropped socket is reconnected from the last applied seq, with
/// backoff, up to [`MAX_RECONNECTS`] times.
async fn next_applied(subscription: &mut Subscription, rng: &mut SystemRandom) -> Result<Applied> {
    let policy = RetryPolicy::default();
    let mut reconnects = 0;
    loop {
        match subscription.next().await {
            Ok(applied) => return Ok(applied),
            Err(error @ (SubscriptionError::Closed | SubscriptionError::Read(_))) => {
                reconnects += 1;
                if reconnects > MAX_RECONNECTS {
                    return Err(anyhow::Error::new(error).context("subscription lost"));
                }
                let delay = policy.delay_ms(reconnects, rng);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                subscription.reconnect().await?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Flushes the outbox once; the tracer's hub never fails transiently, so a flush settles
/// every entry it was given.
async fn send(
    outbox: &mut Outbox,
    cache: &Cache,
    transport: &HttpTransport,
    base: &str,
    rng: &mut SystemRandom,
) -> Result<FlushReport> {
    let report = outbox
        .flush(cache, transport, base, &SystemClock, rng)
        .await?;
    Ok(report)
}

/// The HTTP status of a settled entry. A still-queued or failed entry is an error.
fn status_of(outbox: &Outbox, local_id: i64) -> Result<u16> {
    let entry = outbox
        .get(local_id)
        .ok_or_else(|| anyhow!("outbox entry {local_id} vanished"))?;
    match &entry.state {
        IntentState::Done { status, .. } | IntentState::Conflict { status, .. } => Ok(*status),
        IntentState::Rejected { status, error } => {
            Err(anyhow!("request refused with HTTP {status}: {error}"))
        }
        other => Err(anyhow!("request not answered: {other:?}")),
    }
}
