//! `cli e2e app`: a task driven from client-core against the hub and a fixture agent (M3-017).
//!
//! The stack of `e2e tracer` ([`super::tracer`]): the hub bundle built with worker-build, the D1
//! migrations, `wrangler dev --local` on a free port, and a runner (acp-mock, the sidecar bridge
//! and the tunnel client, in-process) connected to the hub over the tunnel.
//!
//! The difference is the headless client. It is not the tracer adapter
//! (`client_core::tracer`): it drives the public client-core API. [`client_core::Subscription`]
//! follows the thread, [`client_core::Outbox`] (over a [`client_core::Cache`]) sends the
//! approval answer, and the loop ends at `RunFinished{success}`. The thread is created with
//! [`client_core::create_thread`] (`POST /v1/threads` with the first prompt).
//!
//! The run passes when the client sees the turn-event.md §5 rule 6 stream for one prompt with
//! one approval, answers the approval through the outbox (HTTP 200), and sees
//! `RunFinished{success}`, with envelope `seq` strictly increasing by one.

use std::time::{Duration, Instant};

use super::tracer::{self, TempDir, Wrangler};
use anyhow::{Context, Result, anyhow, bail, ensure};
use client_core::outbox::new_idempotency_key;
use client_core::{
    Applied, Cache, Clock, CreateThread, FirstPrompt, HttpTransport, Intent, IntentState, Outbox,
    RetryPolicy, Subscription, SubscriptionError, SystemClock, SystemRandom, ThreadSync,
};
use protocol::tunnel::{ContentPart, Outcome, Workspace};
use protocol::turn_event::{Envelope, Event, InterruptKind, PermissionOptionKind, RunOutcome};
use tokio::sync::{mpsc, oneshot};

const AGENT_ID: &str = "agt_acp_mock";
/// The placeholder token of `DEV_FIXTURES=1` (client-api.md §2).
const DEV_TOKEN: &str = "dev";
const CLIENT_TIMEOUT: Duration = Duration::from_secs(60);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Reconnect attempts after a dropped socket before the exchange fails.
const MAX_RECONNECTS: u32 = 5;

/// Entry point for `cli e2e app`.
pub async fn run() -> Result<()> {
    let started = Instant::now();
    let work = TempDir::create()?;
    let result = tokio::select! {
        result = app(&work) => result,
        () = tracer::interrupted() => Err(anyhow!("interrupted; stopping wrangler")),
    };
    match &result {
        Ok(()) => println!("PASS e2e app in {:.1}s", started.elapsed().as_secs_f64()),
        Err(_) => eprintln!("FAIL e2e app after {:.1}s", started.elapsed().as_secs_f64()),
    }
    result
}

async fn app(work: &TempDir) -> Result<()> {
    let hub_dir = tracer::hub_dir()?;
    tracer::build_hub(&hub_dir, work).await?;

    println!("== d1 migrations (local)");
    tracer::migrate(&hub_dir, work)?;

    let port = tracer::free_port()?;
    let base = format!("http://127.0.0.1:{port}");
    println!("== wrangler dev --local on port {port}");
    let wrangler = Wrangler::start(&hub_dir, port, work, &[])?;
    wrangler.wait_ready(&base).await?;

    println!("== runner (acp-mock + bridge + tunnel) and headless client-core");
    let observed = match tracer::create_machine(&base).await {
        Ok(machine_id) => run_loop(&base, &machine_id).await,
        Err(error) => Err(error),
    };
    let stop = wrangler.stop();
    let envelopes = observed?;
    stop?;
    println!("wrangler stopped");

    println!("== event sequence (seq · type · detail)");
    for envelope in &envelopes {
        println!("{:>3} {}", envelope.seq, tracer::describe(envelope));
    }
    tracer::check_sequence(&envelopes)
}

/// Wires acp-mock → bridge → tunnel client in-process (as `tracer::run_loop` does), runs the
/// headless client against the hub, then shuts the runner side down.
async fn run_loop(base: &str, machine_id: &str) -> Result<Vec<Envelope>> {
    let script = acp_mock::Script::from_value(&tracer::script()).context("app acp-mock script")?;
    let (client_io, agent_io) = tokio::io::duplex(1 << 16);
    let (agent_reader, agent_writer) = tokio::io::split(agent_io);
    let agent = tokio::spawn(acp_mock::run(script, agent_reader, agent_writer));

    let (command_tx, command_rx) = mpsc::channel(64);
    let (event_tx, event_rx) = mpsc::channel(256);
    let (client_reader, client_writer) = tokio::io::split(client_io);
    let cwd = std::env::current_dir().context("current dir")?;
    let cwd_text = cwd.to_string_lossy().into_owned();
    let mut bridge = tokio::spawn(sidecar::tracer::run(
        sidecar::tracer::BridgeConfig {
            machine_id: machine_id.to_owned(),
            agent_id: AGENT_ID.to_owned(),
            cwd,
        },
        client_reader,
        client_writer,
        command_rx,
        event_tx,
    ));

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let ws_base = base.replacen("http", "ws", 1);
    let mut tunnel = tokio::spawn(runner::tracer::run(
        runner::tracer::TunnelConfig {
            hub_ws_base: ws_base,
            machine_id: machine_id.to_owned(),
            runner_version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        event_rx,
        command_tx,
        async {
            let _ = shutdown_rx.await;
        },
    ));

    let client = async {
        tokio::time::timeout(CLIENT_TIMEOUT, drive(base, machine_id, &cwd_text))
            .await
            .map_err(|_| {
                anyhow!("client did not see RunFinished{{success}} within {CLIENT_TIMEOUT:?}")
            })?
    };
    let observed = tokio::select! {
        observed = client => observed,
        result = &mut bridge => Err(tracer::task_ended("bridge", result)),
        result = &mut tunnel => Err(tracer::task_ended("tunnel client", result)),
    };

    // Orderly shutdown, as in `tracer::run_loop`.
    let _ = shutdown_tx.send(());
    if !tunnel.is_finished() {
        match tokio::time::timeout(STOP_TIMEOUT, &mut tunnel).await {
            Ok(Ok(Err(error))) => tracing::warn!("tunnel client: {error:#}"),
            Ok(_) => {}
            Err(_) => tunnel.abort(),
        }
    }
    if !bridge.is_finished()
        && tokio::time::timeout(STOP_TIMEOUT, &mut bridge)
            .await
            .is_err()
    {
        bridge.abort();
    }
    agent.abort();
    observed
}

/// The headless client: creates the task, subscribes to its thread, answers the approval
/// through the outbox, and returns every applied envelope once `RunFinished{success}` arrives.
async fn drive(base: &str, machine_id: &str, workspace: &str) -> Result<Vec<Envelope>> {
    let api = format!("{}/v1", base.trim_end_matches('/'));
    let seed = format!("{:x}", SystemClock.now_ms());
    let thread = create_thread(&api, machine_id, workspace, &seed).await?;

    let cache = Cache::open_in_memory()?;
    let mut outbox = Outbox::load(&cache, RetryPolicy::default())?;
    let transport = HttpTransport::new(Some(DEV_TOKEN.to_owned()));
    let mut rng = SystemRandom;
    let mut subscription = Subscription::connect(
        &api,
        ThreadSync::new(thread.clone()),
        Some(DEV_TOKEN.to_owned()),
    )
    .await?;

    let mut envelopes = Vec::new();
    let mut answered = false;
    loop {
        let Applied::Event(envelope) = next_applied(&mut subscription).await? else {
            continue;
        };
        let envelope = *envelope;
        envelopes.push(envelope.clone());
        match &envelope.event {
            Event::Interrupt(interrupt) if interrupt.kind == InterruptKind::Approval => {
                // The first approval is answered; a replay of it is not answered again.
                if answered {
                    continue;
                }
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
                let local_id = outbox.enqueue(
                    &cache,
                    Intent::Approval {
                        thread_id: thread.clone(),
                        interrupt_id: interrupt.interrupt_id.clone(),
                        outcome: Outcome::Selected { option_id },
                        resolved_by: None,
                    },
                    SystemClock.now_ms(),
                )?;
                outbox
                    .flush(&cache, &transport, &api, &SystemClock, &mut rng)
                    .await?;
                let status = status_of(&outbox, local_id)?;
                ensure!(
                    status == 200,
                    "approval answered HTTP {status}, expected 200"
                );
                answered = true;
            }
            Event::RunFinished(finished)
                if matches!(finished.outcome, RunOutcome::Success { .. }) =>
            {
                ensure!(
                    answered,
                    "RunFinished{{success}} before any approval was answered"
                );
                return Ok(envelopes);
            }
            Event::RunError(error) => bail!("run failed: {error:?}"),
            _ => {}
        }
    }
}

/// `POST /v1/threads` with the task's first prompt; returns the thread id (201).
async fn create_thread(api: &str, machine_id: &str, workspace: &str, seed: &str) -> Result<String> {
    let request = CreateThread {
        machine_id: machine_id.to_owned(),
        agent: AGENT_ID.to_owned(),
        workspace: Workspace {
            path: workspace.to_owned(),
            branch: None,
            spawn: Some("same_dir".to_owned()),
        },
        permission_mode: None,
        title: Some("App".to_owned()),
        prompt: Some(FirstPrompt {
            task_id: format!("tk_{seed}"),
            message_id: format!("msg_{seed}"),
            parts: vec![ContentPart::Text {
                text: "Add a /healthz route".to_owned(),
            }],
        }),
    };
    let created =
        client_core::create_thread(api, Some(DEV_TOKEN), &new_idempotency_key(), &request)
            .await
            .context("POST /v1/threads")?;
    Ok(created.thread_id)
}

/// The next applied message. A dropped socket is reconnected from the last applied seq, with
/// a growing delay, up to [`MAX_RECONNECTS`] times.
async fn next_applied(subscription: &mut Subscription) -> Result<Applied> {
    let mut reconnects = 0;
    loop {
        match subscription.next().await {
            Ok(applied) => return Ok(applied),
            Err(SubscriptionError::Closed | SubscriptionError::Read(_))
                if reconnects < MAX_RECONNECTS =>
            {
                reconnects += 1;
                tokio::time::sleep(Duration::from_millis(500 * u64::from(reconnects))).await;
                subscription.reconnect().await?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// The HTTP status of a settled outbox entry. A still-queued or failed entry is an error.
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
