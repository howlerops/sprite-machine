// TRACER STUB: replaced by M3-001 (client-core: subscription with resubscribe-on-gap, view models, cache, outbound queue)
//! The tracer bullet's headless client: subscribes to one thread over the hub's WebSocket
//! (turn-event.md §6), sends one prompt, answers the approval interrupt with an allow option
//! through the HTTP API (with idempotency keys), and returns every envelope it applied once the
//! task ends with `RunFinished{success}`.
//!
//! It asserts what a real client relies on: envelope `seq` strictly increases by one per thread
//! (duplicates are dropped, a gap is an error here instead of a resubscribe), and a second
//! answer to the same interrupt is refused with HTTP 409 (first answer wins, §4.5).
//!
//! Stub limits: no reconnect / `?after=` resubscribe, no auth, one thread, one task.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use futures_util::StreamExt;
use protocol::tunnel::{Actor, ContentPart, Outcome};
use protocol::turn_event::{Envelope, Event, InterruptKind, PermissionOptionKind, RunOutcome};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

/// Client settings.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// `http://host:port` of the hub.
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

/// Runs the whole client exchange; see the module docs.
pub async fn run(config: ClientConfig) -> Result<Observed> {
    let timeout = config.timeout;
    tokio::time::timeout(timeout, exchange(config))
        .await
        .map_err(|_| anyhow!("client did not see RunFinished{{success}} within {timeout:?}"))?
}

async fn exchange(config: ClientConfig) -> Result<Observed> {
    let base = config.http_base.trim_end_matches('/').to_owned();
    let ws_base = base
        .strip_prefix("http")
        .map(|rest| format!("ws{rest}"))
        .ok_or_else(|| anyhow!("http_base must start with http: {base}"))?;
    let thread = &config.thread_id;
    let seed = format!("{:x}", now_ms());
    let http = reqwest::Client::new();

    let subscribe = format!("{ws_base}/threads/{thread}/subscribe?after=0");
    let (mut socket, _) = tokio_tungstenite::connect_async(subscribe.as_str())
        .await
        .with_context(|| format!("subscribe {subscribe}"))?;

    // Catch-up: hello, snapshot, replayed events, live (§6).
    let hello = next_json(&mut socket).await?;
    ensure!(
        hello["type"] == "hello",
        "first message is not hello: {hello}"
    );
    let mut state = Applied::default();
    loop {
        let message = next_json(&mut socket).await?;
        match message["type"].as_str() {
            Some("snapshot") => {}
            Some("event") => {
                state.apply(&message)?;
            }
            Some("live") => break,
            _ => bail!("unexpected catch-up message {message}"),
        }
    }

    let prompt = json!({
        "machine_id": config.machine_id,
        "task_id": config.task_id,
        "message_id": format!("msg_u{seed}"),
        "parts": [ContentPart::Text { text: config.prompt.clone() }],
        "author": config.user,
    });
    let status = post(
        &http,
        &format!("{base}/threads/{thread}/prompts"),
        &format!("idk_{seed}_prompt"),
        &prompt,
    )
    .await?;
    ensure!(status == 202, "prompt answered HTTP {status}");

    let mut second_answer_status = 0;
    loop {
        let message = next_json(&mut socket).await?;
        match message["type"].as_str() {
            Some("event") => {}
            Some("pong") => continue,
            _ => bail!("unexpected live message {message}"),
        }
        let Some(envelope) = state.apply(&message)? else {
            continue;
        };
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
                let url = format!(
                    "{base}/threads/{thread}/interrupts/{}",
                    interrupt.interrupt_id
                );
                let answer = json!({
                    "outcome": Outcome::Selected { option_id },
                    "resolved_by": config.user,
                });
                let status = post(&http, &url, &format!("idk_{seed}_answer"), &answer).await?;
                ensure!(status == 200, "answer answered HTTP {status}");
                // A second device answering later loses (first answer wins).
                second_answer_status =
                    post(&http, &url, &format!("idk_{seed}_answer_late"), &answer).await?;
            }
            Event::RunFinished(finished)
                if matches!(finished.outcome, RunOutcome::Success { .. }) =>
            {
                return Ok(Observed {
                    envelopes: state.envelopes,
                    second_answer_status,
                });
            }
            Event::RunError(error) => bail!("run failed: {error:?}"),
            _ => {}
        }
    }
}

/// Envelopes applied so far, deduplicated by `seq`.
#[derive(Default)]
struct Applied {
    last_seq: u64,
    envelopes: Vec<Envelope>,
}

impl Applied {
    /// Applies one `{"type":"event"}` message. Returns the envelope if it was new.
    fn apply(&mut self, message: &Value) -> Result<Option<Envelope>> {
        let envelope: Envelope = serde_json::from_value(message["envelope"].clone())
            .with_context(|| format!("invalid envelope {message}"))?;
        if envelope.seq <= self.last_seq {
            return Ok(None);
        }
        ensure!(
            envelope.seq == self.last_seq + 1,
            "seq gap: after {} came {}",
            self.last_seq,
            envelope.seq
        );
        self.last_seq = envelope.seq;
        self.envelopes.push(envelope.clone());
        Ok(Some(envelope))
    }
}

async fn post(http: &reqwest::Client, url: &str, key: &str, body: &Value) -> Result<u16> {
    let response = http
        .post(url)
        .header("Idempotency-Key", key)
        .json(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    Ok(response.status().as_u16())
}

async fn next_json<S>(socket: &mut S) -> Result<Value>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let message = socket
            .next()
            .await
            .ok_or_else(|| anyhow!("subscription closed by the hub"))?
            .context("subscription read")?;
        match message {
            Message::Text(body) => {
                return serde_json::from_str(body.as_str())
                    .with_context(|| format!("invalid message {}", body.as_str()));
            }
            Message::Close(reason) => bail!("subscription closed by the hub: {reason:?}"),
            _ => {}
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
