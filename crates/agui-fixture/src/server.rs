//! The HTTP and WebSocket surface (turn-event.md §6, client-api.md §5.4) over one thread log.
//!
//! One task (the emitter) appends the script to the log, so `seq` is assigned in script order.
//! Subscribers replay the log under the same lock that they use to subscribe to the live
//! broadcast, so nothing is missed or duplicated between catch-up and live.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use axum::Json;
use axum::Router;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use protocol::tunnel::Actor;
use protocol::turn_event::{
    Envelope, Event, InterruptResolved, MAX_ENVELOPE_BYTES, ResolvedOutcome, StateScope, ThreadSeq,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::patch;
use crate::script::{Scenario, Script, Step, default_actor};

/// The thread a fixture serves unless configured otherwise.
pub const DEFAULT_THREAD_ID: &str = "th_fixture";

/// What to serve and how fast.
#[derive(Debug, Clone)]
pub struct FixtureConfig {
    pub scenario: Scenario,
    /// Events appended per second. `0` appends as fast as possible.
    pub rate: f64,
    /// Where to listen. Port `0` picks a free port; read it back with [`RunningFixture::addr`].
    pub addr: SocketAddr,
    /// The thread id the scenario is served on.
    pub thread_id: String,
}

impl FixtureConfig {
    /// `scenario` at unlimited rate on `127.0.0.1` with a free port and [`DEFAULT_THREAD_ID`].
    pub fn new(scenario: Scenario) -> Self {
        Self {
            scenario,
            rate: 0.0,
            addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            thread_id: DEFAULT_THREAD_ID.to_owned(),
        }
    }
}

/// A fixture being served. Dropping it stops the server and the emitter.
pub struct RunningFixture {
    addr: SocketAddr,
    thread_id: String,
    tasks: Vec<JoinHandle<()>>,
}

impl RunningFixture {
    /// The bound address (with the port picked by the OS when the config asked for port 0).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The thread the fixture serves.
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// `ws://` URL of the subscription for `after`.
    pub fn ws_url(&self, after: u64) -> String {
        format!(
            "ws://{}/threads/{}/subscribe?after={after}",
            self.addr, self.thread_id
        )
    }

    /// `http://` URL of `path` on this server.
    pub fn http_url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

impl Drop for RunningFixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Binds the listener and starts serving. The emitter starts when the first subscriber
/// connects, so that a subscriber sees the whole stream from `seq` 1 when it asks for it.
pub async fn start(config: FixtureConfig) -> Result<RunningFixture> {
    if !(config.rate.is_finite() && config.rate >= 0.0) {
        return Err(anyhow!(
            "rate must be a finite number >= 0, got {}",
            config.rate
        ));
    }
    let script = config.scenario.script(&config.thread_id);
    let shared = Arc::new(Shared::new(&config.thread_id, &script));
    let listener = tokio::net::TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("bind {}", config.addr))?;
    let addr = listener.local_addr().context("local address")?;

    let server = tokio::spawn(run_server(listener, router(Arc::clone(&shared))));
    let emitter = tokio::spawn(run_emitter(Arc::clone(&shared), script, config.rate));
    Ok(RunningFixture {
        addr,
        thread_id: config.thread_id,
        tasks: vec![server, emitter],
    })
}

async fn run_server(listener: tokio::net::TcpListener, app: Router) {
    // A failed serve only stops this fixture; the caller sees it as connection errors.
    let _ = axum::serve(listener, app).await;
}

/// One stored envelope with its serialized subscription message.
#[derive(Clone)]
struct Stored {
    envelope: Arc<Envelope>,
    message: Utf8Bytes,
}

/// An interrupt raised in the log, and its answer once one is accepted.
struct InterruptRecord {
    option_ids: Vec<String>,
    resolution: Option<Resolution>,
}

/// The first accepted answer (client-api.md §5.4).
#[derive(Clone)]
struct Resolution {
    idempotency_key: Option<String>,
    outcome: ResolvedOutcome,
    resolved_by: Actor,
}

/// The thread log, guarded by one mutex.
struct Log {
    seq: ThreadSeq,
    entries: Vec<Stored>,
    interrupts: BTreeMap<String, InterruptRecord>,
    live: broadcast::Sender<Stored>,
}

/// State shared by the HTTP handlers and the emitter.
struct Shared {
    thread_id: String,
    log: Mutex<Log>,
    /// Woken when an interrupt is answered.
    answered: Notify,
    /// Set to `true` by the first subscriber, which starts the emitter.
    started: watch::Sender<bool>,
}

impl Shared {
    fn new(thread_id: &str, script: &Script) -> Self {
        let emits = script
            .iter()
            .filter(|step| matches!(step, Step::Emit(_)))
            .count();
        // Sized to hold the whole script: a subscriber that is still replaying can never lag.
        let (live, _) = broadcast::channel(emits.max(1));
        Self {
            thread_id: thread_id.to_owned(),
            log: Mutex::new(Log {
                seq: ThreadSeq::new(),
                entries: Vec::with_capacity(emits),
                interrupts: BTreeMap::new(),
                live,
            }),
            answered: Notify::new(),
            started: watch::channel(false).0,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Log> {
        self.log.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stamps `envelope` with the next `seq`, stores it and sends it to live subscribers.
    fn append(&self, envelope: Envelope) -> Result<()> {
        let mut log = self.lock();
        let stamped = log.seq.stamp(envelope)?;
        let json = serde_json::to_string(&stamped)?;
        if json.len() > MAX_ENVELOPE_BYTES {
            return Err(anyhow!(
                "envelope seq {} is {} bytes, over the {MAX_ENVELOPE_BYTES}-byte limit",
                stamped.seq,
                json.len()
            ));
        }
        if let Event::Interrupt(interrupt) = &stamped.event {
            let option_ids = interrupt
                .options
                .iter()
                .flatten()
                .map(|option| option.option_id.clone())
                .collect();
            log.interrupts.insert(
                interrupt.interrupt_id.clone(),
                InterruptRecord {
                    option_ids,
                    resolution: None,
                },
            );
        }
        let message = Utf8Bytes::from(format!(r#"{{"type":"event","envelope":{json}}}"#));
        let stored = Stored {
            envelope: Arc::new(stamped),
            message,
        };
        log.entries.push(stored.clone());
        // No live subscribers is fine: the event is in the log.
        let _ = log.live.send(stored);
        Ok(())
    }

    /// Waits until `interrupt_id` has an accepted answer and returns it.
    async fn wait_answered(&self, interrupt_id: &str) -> Resolution {
        loop {
            // Created before the check, so an answer between check and wait is not missed.
            let answered = self.answered.notified();
            let resolution = self
                .lock()
                .interrupts
                .get(interrupt_id)
                .and_then(|record| record.resolution.clone());
            if let Some(resolution) = resolution {
                return resolution;
            }
            answered.await;
        }
    }

    /// The `hello`, `snapshot` and replayed events for `after`, and the live receiver. Both
    /// are taken under one lock, so the live receiver sees exactly the events after the
    /// replay.
    fn catch_up(&self, after: u64) -> (Vec<Utf8Bytes>, broadcast::Receiver<Stored>) {
        let log = self.lock();
        let head = log.entries.len();
        let from = replay_start(&log.entries, after).min(head as u64 + 1);
        let before = usize::try_from(from - 1).unwrap_or(head);
        let states = states_before(&log.entries[..before]);

        let mut messages = Vec::with_capacity(head - before + 2);
        messages.push(text(json!({
            "type": "hello",
            "head_seq": head,
            "replay_from": from,
        })));
        messages.push(text(json!({
            "type": "snapshot",
            "at_seq": from - 1,
            "states": states,
        })));
        messages.extend(log.entries[before..].iter().map(|s| s.message.clone()));
        (messages, log.live.subscribe())
    }
}

fn text(value: Value) -> Utf8Bytes {
    Utf8Bytes::from(value.to_string())
}

/// The first `seq` a subscriber that has applied `after` must be sent (turn-event.md §6, step 1):
/// the `RunStarted` of the run open at `after`, or `after + 1` when no run is open.
fn replay_start(entries: &[Stored], after: u64) -> u64 {
    let upto = usize::try_from(after).map_or(entries.len(), |n| n.min(entries.len()));
    let mut open: Option<(String, u64)> = None;
    for stored in &entries[..upto] {
        let envelope = &stored.envelope;
        match (&envelope.event, &envelope.run_id) {
            (Event::RunStarted(_), Some(run)) => open = Some((run.clone(), envelope.seq)),
            (Event::RunFinished(_) | Event::RunError(_), Some(run))
                if open.as_ref().is_some_and(|(open_run, _)| open_run == run) =>
            {
                open = None;
            }
            _ => {}
        }
    }
    match open {
        Some((_, started)) => started.min(after + 1),
        None => after + 1,
    }
}

/// The state of every scope as of the end of `entries` (snapshots, then deltas applied).
fn states_before(entries: &[Stored]) -> Map<String, Value> {
    let mut states = Map::new();
    for stored in entries {
        match &stored.envelope.event {
            Event::StateSnapshot(state) => {
                states.insert(
                    scope_key(state.scope).to_owned(),
                    Value::Object(state.snapshot.clone()),
                );
            }
            Event::StateDelta(delta) => {
                if let Some(document) = states.get_mut(scope_key(delta.scope)) {
                    // Scripts only emit patches that apply, so a failure is not expected.
                    let _ = patch::apply(document, &delta.patch);
                }
            }
            _ => {}
        }
    }
    states
}

fn scope_key(scope: StateScope) -> &'static str {
    match scope {
        StateScope::Session => "session",
        StateScope::Usage => "usage",
        StateScope::Thread => "thread",
        StateScope::Flow => "flow",
    }
}

fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/threads/{thread_id}/subscribe", get(subscribe))
        .route(
            "/threads/{thread_id}/interrupts/{interrupt_id}",
            post(answer_interrupt),
        )
        .with_state(shared)
}

#[derive(Deserialize)]
struct SubscribeQuery {
    #[serde(default)]
    after: u64,
}

async fn subscribe(
    ws: WebSocketUpgrade,
    Path(thread_id): Path<String>,
    Query(query): Query<SubscribeQuery>,
    State(shared): State<Arc<Shared>>,
) -> Response {
    if thread_id != shared.thread_id {
        return StatusCode::NOT_FOUND.into_response();
    }
    shared.started.send_replace(true);
    ws.on_upgrade(move |socket| stream(socket, shared, query.after))
        .into_response()
}

/// Serves one subscription: catch-up, then live events, answering pings, until the client
/// goes away or falls behind.
async fn stream(mut socket: WebSocket, shared: Arc<Shared>, after: u64) {
    let (catch_up, mut live) = shared.catch_up(after);
    for message in catch_up {
        if socket.send(Message::Text(message)).await.is_err() {
            return;
        }
    }
    if socket
        .send(Message::Text(text(json!({"type": "live"}))))
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(body))) if is_ping(body.as_str()) => {
                    let pong = Utf8Bytes::from_static(r#"{"type":"pong"}"#);
                    if socket.send(Message::Text(pong)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            event = live.recv() => match event {
                Ok(stored) => {
                    if socket.send(Message::Text(stored.message)).await.is_err() {
                        break;
                    }
                }
                // Closed, or lagged past the buffer: the client resubscribes with ?after.
                Err(_) => break,
            },
        }
    }
}

fn is_ping(body: &str) -> bool {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .map(|t| t == "ping")
        })
        .unwrap_or(false)
}

#[derive(Deserialize)]
struct AnswerBody {
    outcome: ResolvedOutcome,
    #[serde(default)]
    resolved_by: Option<Actor>,
}

/// `POST /threads/{thread_id}/interrupts/{interrupt_id}`. The first accepted answer wins
/// (turn-event.md §4.5). A repeat with the same `Idempotency-Key` gets the same 200; any other
/// later answer gets 409 `already_resolved` with `resolved_by`.
async fn answer_interrupt(
    State(shared): State<Arc<Shared>>,
    Path((thread_id, interrupt_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<AnswerBody>,
) -> Response {
    if thread_id != shared.thread_id {
        return error(StatusCode::NOT_FOUND, "unknown_thread", None);
    }
    let key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let mut log = shared.lock();
    let Some(record) = log.interrupts.get_mut(&interrupt_id) else {
        return error(StatusCode::NOT_FOUND, "unknown_interrupt", None);
    };
    if let Some(resolution) = &record.resolution {
        if key.is_some() && resolution.idempotency_key == key {
            return accepted(&interrupt_id, &resolution.resolved_by);
        }
        return error(
            StatusCode::CONFLICT,
            "already_resolved",
            Some((interrupt_id.as_str(), &resolution.resolved_by)),
        );
    }
    let ResolvedOutcome::Selected { option_id } = &body.outcome else {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_outcome", None);
    };
    if !record.option_ids.contains(option_id) {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_outcome", None);
    }
    let resolved_by = body.resolved_by.unwrap_or_else(default_actor);
    record.resolution = Some(Resolution {
        idempotency_key: key,
        outcome: body.outcome,
        resolved_by: resolved_by.clone(),
    });
    drop(log);
    shared.answered.notify_waiters();
    accepted(&interrupt_id, &resolved_by)
}

fn accepted(interrupt_id: &str, resolved_by: &Actor) -> Response {
    (
        StatusCode::OK,
        Json(json!({"interrupt_id": interrupt_id, "resolved_by": resolved_by})),
    )
        .into_response()
}

fn error(status: StatusCode, code: &str, conflict: Option<(&str, &Actor)>) -> Response {
    let body = match conflict {
        Some((interrupt_id, resolved_by)) => json!({
            "error": code,
            "interrupt_id": interrupt_id,
            "resolved_by": resolved_by,
        }),
        None => json!({"error": code}),
    };
    (status, Json(body)).into_response()
}

/// Appends the script to the log. Scripted waits (`AwaitAnswer`) block until the interrupt is
/// answered, then append its `InterruptResolved`. Pacing restarts after each wait.
async fn run_emitter(shared: Arc<Shared>, script: Script, rate: f64) {
    let mut started = shared.started.subscribe();
    if started.wait_for(|on| *on).await.is_err() {
        return;
    }
    let mut base = Instant::now();
    let mut emitted: u64 = 0;
    for step in script {
        match step {
            Step::Emit(envelope) => {
                if rate > 0.0 {
                    let due = base + Duration::from_secs_f64(emitted as f64 / rate);
                    tokio::time::sleep_until(due).await;
                }
                if shared.append(*envelope).is_err() {
                    return;
                }
                emitted += 1;
            }
            Step::AwaitAnswer {
                task_id,
                interrupt_id,
                ts,
            } => {
                let resolution = shared.wait_answered(&interrupt_id).await;
                let event = InterruptResolved {
                    interrupt_id,
                    outcome: resolution.outcome,
                    resolved_by: resolution.resolved_by,
                };
                let envelope =
                    Envelope::new(shared.thread_id.clone(), Some(task_id), None, ts, event);
                if shared.append(envelope).is_err() {
                    return;
                }
                base = Instant::now();
                emitted = 0;
            }
        }
    }
}
