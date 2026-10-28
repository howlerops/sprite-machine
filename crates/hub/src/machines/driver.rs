//! The Machine DO's frame handling (M2B-002): the async part around [`MachineCore`].
//!
//! `handle_text` takes one text message of the tunnel and runs tunnel.md §6 on it: the frame
//! parses ([`parse_frame`], strict); the machine is the one the socket authenticated as; the
//! thread is hosted by this machine; the envelope fits the schema and 256 KiB; the thread host
//! (Thread DO) accepts it under turn-event.md §5. Only then is the frame counted as processed
//! (`last`, `event_id`), which is what the cumulative `ack` reports. A refusal at steps 2 to 4
//! is a `nack`, and a nack counts as an ack (§3.7).
//!
//! The cross-object calls go through [`ThreadHost`]; the core is borrowed only between `await`
//! points, never across one, so other entry points (a command push, a close, a revoke) can run
//! while an append is in flight. The Durable Object serialises `handle_text` calls itself.

use std::cell::RefCell;
use std::future::Future;

use protocol::tunnel::{
    CloseCode, Command, Frame, MAX_FRAME_BYTES, NackCode, PresenceState, parse_frame,
};
use protocol::turn_event::{
    Envelope, MAX_ENVELOPE_BYTES, Presence, RunError, RunErrorReason, RunReason, RunStarted,
    format_timestamp,
};
use serde_json::{Value, json};

use super::core::{
    Effects, HelloInfo, MachineCore, Out, PresenceChange, PushBody, PushCommand, PushError,
    ThreadAcks, UpCheck,
};
use super::store::{ForwardedRecord, HostKind, MachineStore, OutboxRow, StoreResult};

/// A call to a thread host failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    /// The host refused the request on its merits: this becomes a `nack`.
    Rejected { code: NackCode, message: String },
    /// The host could not be reached or failed: not the frame's fault, so no `nack`.
    Unavailable(String),
}

/// The objects a machine talks to: the thread hosts and the D1 mirror of its presence.
pub trait ThreadHost {
    /// Appends one envelope (`seq: 0`) to a thread (Thread DO `POST /internal/threads/{id}/events`).
    fn append(
        &self,
        host: HostKind,
        thread_id: &str,
        envelope: &Value,
    ) -> impl Future<Output = Result<(), HostError>>;

    /// The machine a Thread DO records as the thread's host, if the thread exists.
    fn thread_machine(
        &self,
        thread_id: &str,
    ) -> impl Future<Output = Result<Option<String>, HostError>>;

    /// Thread DO `POST /internal/threads/{id}/init`.
    fn init_thread(
        &self,
        thread_id: &str,
        info: &Value,
    ) -> impl Future<Output = Result<(), HostError>>;

    /// The thread's unacked commands (Thread DO `GET /internal/threads/{id}/commands`).
    fn list_commands(
        &self,
        host: HostKind,
        thread_id: &str,
    ) -> impl Future<Output = Result<Vec<PushCommand>, HostError>>;

    /// Thread DO `POST /internal/threads/{id}/commands/ack`.
    fn ack_commands(
        &self,
        host: HostKind,
        thread_id: &str,
        source_ids: &[String],
    ) -> impl Future<Output = Result<(), HostError>>;

    /// Mirrors presence into D1 (`machines.presence`, best effort).
    fn record_presence(
        &self,
        machine_id: &str,
        change: &PresenceChange,
        now_ms: u64,
    ) -> impl Future<Output = ()>;
}

/// The core behind a `RefCell`, shared by every entry point of the DO.
pub type Core<S> = RefCell<MachineCore<S>>;

fn storage_failure(error: &impl std::fmt::Display) -> Out {
    Out::close(CloseCode::ServerRestart, format!("storage error: {error}"))
}

/// Handles one text message from connection `conn`. Frames to send go through `emit` as soon
/// as they are decided, so `welcome` is not held up by the presence fan-out after it.
pub async fn handle_text<S, H>(
    core: &Core<S>,
    host: &H,
    conn: u64,
    text: &str,
    now_ms: u64,
    emit: &mut impl FnMut(Out),
) where
    S: MachineStore,
    H: ThreadHost,
{
    if let Err(error) = handle_inner(core, host, conn, text, now_ms, emit).await {
        emit(storage_failure(&error));
    }
}

async fn handle_inner<S, H>(
    core: &Core<S>,
    host: &H,
    conn: u64,
    text: &str,
    now_ms: u64,
    emit: &mut impl FnMut(Out),
) -> StoreResult<()>
where
    S: MachineStore,
    H: ThreadHost,
{
    // A superseded or closed socket may still deliver a message; it is not ours any more.
    if !core.borrow().is_current(conn) {
        return Ok(());
    }
    // 1. The frame parses and matches the spec (tunnel.md §2 size, §6 step 1).
    if text.len() > MAX_FRAME_BYTES {
        emit(Out::close(
            CloseCode::ProtocolError,
            "frame larger than 1 MiB",
        ));
        return Ok(());
    }
    let frame = match parse_frame(text) {
        Ok(frame) => frame,
        Err(error) => {
            emit(Out::close(
                CloseCode::ProtocolError,
                format!("invalid frame: {error}"),
            ));
            return Ok(());
        }
    };
    core.borrow_mut().touch(now_ms)?;

    match frame {
        Frame::Ping => emit(Out::send(Frame::Pong)),
        Frame::Pong => {}
        Frame::Hello {
            proto,
            machine_id,
            runner_version,
            resume_token,
            last_down_seq,
            capabilities,
        } => {
            let info = HelloInfo {
                proto,
                machine_id,
                runner_version,
                resume_token,
                last_down_seq,
                capabilities: serde_json::to_value(&capabilities).unwrap_or(Value::Null),
            };
            let fx = core.borrow_mut().on_hello(info, now_ms)?;
            apply(core, host, fx, now_ms, emit).await?;
            if core.borrow().hello_done() {
                reconcile_all(core, host, now_ms, emit).await?;
            }
        }
        _ if !core.borrow().hello_done() => {
            emit(Out::close(CloseCode::ProtocolError, "frame before hello"));
        }
        Frame::Ack { seq } => {
            let fx = core.borrow_mut().on_ack(seq, now_ms)?;
            apply(core, host, fx, now_ms, emit).await?;
        }
        Frame::Presence { state, .. } => {
            // A hint only (tunnel.md §3.7); `busy` changes nothing the hub publishes.
            if state == PresenceState::Asleep {
                core.borrow_mut().note_asleep_hint()?;
            }
        }
        Frame::Close { code, .. } => match code {
            CloseCode::GoingToSleep => core.borrow_mut().note_asleep_hint()?,
            CloseCode::Normal => core.borrow_mut().note_normal_close()?,
            _ => {}
        },
        Frame::Event {
            seq,
            event_id,
            envelope,
        } => event(core, host, seq, &event_id, &envelope, now_ms, emit).await?,
        Frame::ThreadOpen {
            seq,
            event_id,
            thread_id,
            title,
            agent,
            workspace,
        } => {
            let info = json!({
                "agent": agent,
                "title": title,
                "workspace": workspace,
            });
            thread_open(core, host, (seq, &event_id, &thread_id), info, now_ms, emit).await?;
        }
        Frame::CommandResult {
            seq,
            event_id,
            idempotency_key,
            ok,
            code,
            message,
        } => {
            let code = serde_json::to_value(code)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default();
            let result = CommandResult {
                seq,
                event_id: &event_id,
                idempotency_key: &idempotency_key,
                ok,
                code: &code,
                message: &message,
            };
            command_result(core, host, &result, now_ms, emit).await?;
        }
        Frame::Welcome { .. } | Frame::Command { .. } | Frame::Nack { .. } => {
            emit(Out::close(
                CloseCode::ProtocolError,
                "down-stream frame sent by the runner",
            ));
        }
    }
    Ok(())
}

/// The ack to send once the frames queued behind the current one are handled.
pub fn flush_ack<S: MachineStore>(core: &Core<S>) -> Option<Out> {
    match core.borrow_mut().take_ack() {
        Ok(Some(seq)) => Some(Out::send(Frame::Ack { seq })),
        Ok(None) => None,
        Err(error) => Some(storage_failure(&error)),
    }
}

/// The receiver rules for a sequenced frame; `Some(epoch)` means "process it".
fn up_preface<S: MachineStore>(
    core: &Core<S>,
    seq: u64,
    emit: &mut impl FnMut(Out),
) -> Option<u64> {
    let core = core.borrow();
    match core.check_up(seq) {
        UpCheck::Process => Some(core.stream_epoch()),
        UpCheck::Duplicate { ack } => {
            emit(Out::send(Frame::Ack { seq: ack }));
            None
        }
        UpCheck::Violation(message) => {
            emit(Out::close(CloseCode::ProtocolError, message));
            None
        }
    }
}

fn nack(seq: u64, code: NackCode, message: impl Into<String>) -> Out {
    Out::send(Frame::Nack {
        seq,
        code,
        message: message.into(),
    })
}

/// `event` (tunnel.md §3.3): dedupe by `event_id`, validate (§6 steps 2 to 4), append, count.
async fn event<S, H>(
    core: &Core<S>,
    host: &H,
    seq: u64,
    event_id: &str,
    envelope: &Value,
    now_ms: u64,
    emit: &mut impl FnMut(Out),
) -> StoreResult<()>
where
    S: MachineStore,
    H: ThreadHost,
{
    let Some(epoch) = up_preface(core, seq, emit) else {
        return Ok(());
    };
    let mut refusal: Option<Out> = None;
    // A fresh stream resends what the old one delivered: the `event_id` says so (§4.4).
    if !core.borrow().event_seen(event_id, now_ms)? {
        match validate_envelope(core, envelope)? {
            Err((code, message)) => refusal = Some(nack(seq, code, message)),
            Ok((host_kind, thread_id)) => {
                match host.append(host_kind, &thread_id, envelope).await {
                    Ok(()) => {}
                    Err(HostError::Rejected { code, message }) => {
                        refusal = Some(nack(seq, code, message));
                    }
                    Err(HostError::Unavailable(message)) => {
                        // Not processed, so not counted: the runner resends after reconnecting.
                        emit(Out::close(
                            CloseCode::ServerRestart,
                            format!("thread host unavailable: {message}"),
                        ));
                        return Ok(());
                    }
                }
            }
        }
    }
    core.borrow_mut()
        .complete_up(epoch, seq, Some(event_id), now_ms)?;
    if let Some(refusal) = refusal {
        emit(refusal);
    }
    Ok(())
}

/// §6 steps 2 and 3 for an envelope: hosted by this machine, at most 256 KiB, valid.
/// Returns the host kind and thread id, or the `nack` to send.
fn validate_envelope<S: MachineStore>(
    core: &Core<S>,
    envelope: &Value,
) -> StoreResult<Result<(HostKind, String), (NackCode, String)>> {
    let Some(thread_id) = envelope.get("thread_id").and_then(Value::as_str) else {
        return Ok(Err((
            NackCode::InvalidEnvelope,
            "envelope has no thread_id".to_owned(),
        )));
    };
    let Some(host_kind) = core.borrow().hosted(thread_id)? else {
        return Ok(Err((
            NackCode::Forbidden,
            format!("thread {thread_id} is not hosted by this machine"),
        )));
    };
    let size = serde_json::to_vec(envelope).map_or(usize::MAX, |bytes| bytes.len());
    if size > MAX_ENVELOPE_BYTES {
        return Ok(Err((
            NackCode::TooLarge,
            format!("envelope is {size} bytes, the limit is {MAX_ENVELOPE_BYTES}"),
        )));
    }
    if let Err(error) = serde_json::from_value::<Envelope>(envelope.clone()) {
        return Ok(Err((NackCode::InvalidEnvelope, error.to_string())));
    }
    Ok(Ok((host_kind, thread_id.to_owned())))
}

/// `thread_open` (tunnel.md §3.4): registers a thread the runner started. `info` carries the
/// frame's `agent`, `title` and `workspace`.
async fn thread_open<S, H>(
    core: &Core<S>,
    host: &H,
    (seq, event_id, thread_id): (u64, &str, &str),
    mut info: Value,
    now_ms: u64,
    emit: &mut impl FnMut(Out),
) -> StoreResult<()>
where
    S: MachineStore,
    H: ThreadHost,
{
    let Some(epoch) = up_preface(core, seq, emit) else {
        return Ok(());
    };
    let mut refusal: Option<Out> = None;
    if !core.borrow().event_seen(event_id, now_ms)? {
        let machine_id = core.borrow().machine_id().to_owned();
        if protocol::turn_event::is_valid_id(thread_id) {
            if core.borrow().hosted(thread_id)?.is_none() {
                match host.thread_machine(thread_id).await {
                    Ok(Some(owner)) if owner != machine_id => {
                        refusal = Some(nack(
                            seq,
                            NackCode::Forbidden,
                            format!("thread {thread_id} is hosted by another machine"),
                        ));
                    }
                    Ok(_) => {
                        if let Some(map) = info.as_object_mut() {
                            map.insert("machine_id".to_owned(), json!(machine_id));
                        }
                        match host.init_thread(thread_id, &info).await {
                            Ok(()) => {
                                core.borrow_mut().host_thread(
                                    thread_id,
                                    HostKind::Thread,
                                    now_ms,
                                )?;
                            }
                            Err(HostError::Rejected { code, message }) => {
                                refusal = Some(nack(seq, code, message));
                            }
                            Err(HostError::Unavailable(message)) => {
                                emit(Out::close(
                                    CloseCode::ServerRestart,
                                    format!("thread host unavailable: {message}"),
                                ));
                                return Ok(());
                            }
                        }
                    }
                    Err(HostError::Rejected { code, message }) => {
                        refusal = Some(nack(seq, code, message));
                    }
                    Err(HostError::Unavailable(message)) => {
                        emit(Out::close(
                            CloseCode::ServerRestart,
                            format!("thread host unavailable: {message}"),
                        ));
                        return Ok(());
                    }
                }
            }
        } else {
            refusal = Some(nack(
                seq,
                NackCode::InvalidEnvelope,
                "thread_id must be 1-128 characters of [A-Za-z0-9_:.-]",
            ));
        }
    }
    core.borrow_mut()
        .complete_up(epoch, seq, Some(event_id), now_ms)?;
    if let Some(refusal) = refusal {
        emit(refusal);
    }
    Ok(())
}

/// The fields of a `command_result` frame.
#[derive(Clone, Copy)]
struct CommandResult<'a> {
    seq: u64,
    event_id: &'a str,
    idempotency_key: &'a str,
    ok: bool,
    code: &'a str,
    message: &'a str,
}

/// `command_result` (tunnel.md §3.6): a command the runner could not apply. A failed `prompt`
/// or `dispatch` fails its task: `RunStarted` + `RunError{error, code}` through the thread host
/// (the same shape the Thread DO uses for a task cancelled before it started).
async fn command_result<S, H>(
    core: &Core<S>,
    host: &H,
    result: &CommandResult<'_>,
    now_ms: u64,
    emit: &mut impl FnMut(Out),
) -> StoreResult<()>
where
    S: MachineStore,
    H: ThreadHost,
{
    let CommandResult {
        seq,
        event_id,
        idempotency_key,
        ok,
        code,
        message,
    } = *result;
    let Some(epoch) = up_preface(core, seq, emit) else {
        return Ok(());
    };
    if !ok && !core.borrow().event_seen(event_id, now_ms)? {
        let record = core.borrow().sent_command(idempotency_key)?;
        if let Some(record) = record {
            let reason = format!("{code}: {message}");
            if let Err(HostError::Unavailable(message)) =
                fail_task(core, host, &record, code, &reason, now_ms).await
            {
                emit(Out::close(
                    CloseCode::ServerRestart,
                    format!("thread host unavailable: {message}"),
                ));
                return Ok(());
            }
        }
    }
    core.borrow_mut()
        .complete_up(epoch, seq, Some(event_id), now_ms)?;
    Ok(())
}

/// Ends the task of a `prompt` / `dispatch` command that never ran. A `RunError` is only valid
/// inside a run (turn-event.md §5), so the task gets a run that starts and errors at once.
/// `approval` and `cancel` have no task to fail.
async fn fail_task<S, H>(
    core: &Core<S>,
    host: &H,
    record: &ForwardedRecord,
    code: &str,
    message: &str,
    now_ms: u64,
) -> Result<(), HostError>
where
    S: MachineStore,
    H: ThreadHost,
{
    if !matches!(record.kind.as_str(), "prompt" | "dispatch") || record.task_id.is_empty() {
        return Ok(());
    }
    let machine_id = core.borrow().machine_id().to_owned();
    let ts = format_timestamp(now_ms);
    let run_id = format!("run_failed_{}", record.task_id);
    let started = Envelope::new(
        record.thread_id.clone(),
        Some(record.task_id.clone()),
        Some(run_id.clone()),
        ts.clone(),
        RunStarted {
            reason: RunReason::Prompt,
            parent_run_id: None,
            agent_id: None,
            machine_id: Some(machine_id),
        },
    );
    let errored = Envelope::new(
        record.thread_id.clone(),
        Some(record.task_id.clone()),
        Some(run_id),
        ts,
        RunError {
            reason: RunErrorReason::Error,
            code: Some(code.to_owned()),
            message: message.to_owned(),
        },
    );
    for envelope in [started, errored] {
        let value = serde_json::to_value(&envelope).unwrap_or(Value::Null);
        match host.append(record.host, &record.thread_id, &value).await {
            Ok(()) => {}
            // The thread cannot take it (task already ended, no such run state): nothing to do.
            Err(HostError::Rejected { .. }) => return Ok(()),
            Err(error @ HostError::Unavailable(_)) => return Err(error),
        }
    }
    Ok(())
}

/// Emits the frames of an [`Effects`], then does its cross-object work.
async fn apply<S, H>(
    core: &Core<S>,
    host: &H,
    fx: Effects,
    now_ms: u64,
    emit: &mut impl FnMut(Out),
) -> StoreResult<()>
where
    S: MachineStore,
    H: ThreadHost,
{
    for out in fx.out {
        emit(out);
    }
    if let Some(change) = fx.presence {
        publish_presence(core, host, &change, now_ms).await?;
    }
    for acks in fx.acks {
        ack_at_host(host, &acks).await;
    }
    for row in fx.expired {
        expire_task(core, host, &row, now_ms).await;
    }
    Ok(())
}

async fn expire_task<S, H>(core: &Core<S>, host: &H, row: &OutboxRow, now_ms: u64)
where
    S: MachineStore,
    H: ThreadHost,
{
    let record = ForwardedRecord {
        thread_id: row.thread_id.clone(),
        source_id: row.source_id.clone(),
        host: row.host,
        idempotency_key: row.idempotency_key.clone(),
        kind: super::core::command_kind(&row.command).to_owned(),
        task_id: super::core::command_task(&row.command),
        state: super::store::ForwardState::Expired,
        updated_ms: now_ms,
    };
    let _ = fail_task(
        core,
        host,
        &record,
        "expired",
        "the command waited 7 days for its machine and expired",
        now_ms,
    )
    .await;
}

async fn ack_at_host<H: ThreadHost>(host: &H, acks: &ThreadAcks) {
    // A failed ack leaves the command listed at the Thread DO; the next push or reconnect
    // reports it again (`push` re-acks what the runner already holds).
    let _ = host
        .ack_commands(acks.host, &acks.thread_id, &acks.source_ids)
        .await;
}

/// Publishes a presence change: D1 first, then a thread-scoped `Presence` TurnEvent into every
/// thread this machine hosts. TRACER STUB threads are skipped (the tracer's stream is asserted
/// event by event).
pub async fn publish_presence<S, H>(
    core: &Core<S>,
    host: &H,
    change: &PresenceChange,
    now_ms: u64,
) -> StoreResult<()>
where
    S: MachineStore,
    H: ThreadHost,
{
    let (machine_id, threads) = {
        let core = core.borrow();
        (core.machine_id().to_owned(), core.hosted_threads()?)
    };
    host.record_presence(&machine_id, change, now_ms).await;
    for (thread_id, kind) in threads {
        if kind != HostKind::Thread {
            continue;
        }
        let envelope = Envelope::new(
            thread_id.clone(),
            None,
            None,
            format_timestamp(now_ms),
            Presence {
                machine_id: machine_id.clone(),
                state: change.state,
                since: change.since.clone(),
            },
        );
        if let Ok(value) = serde_json::to_value(&envelope) {
            // Best effort: one unreachable thread must not hold up the others.
            let _ = host.append(kind, &thread_id, &value).await;
        }
    }
    Ok(())
}

/// After `welcome`: pulls the unacked commands of every hosted Thread DO, so a push that was
/// lost while this object was away is not (§5 "delivered in order after welcome").
async fn reconcile_all<S, H>(
    core: &Core<S>,
    host: &H,
    now_ms: u64,
    emit: &mut impl FnMut(Out),
) -> StoreResult<()>
where
    S: MachineStore,
    H: ThreadHost,
{
    let threads = core.borrow().hosted_threads()?;
    for (thread_id, kind) in threads {
        if kind != HostKind::Thread {
            continue;
        }
        let Ok(commands) = host.list_commands(kind, &thread_id).await else {
            continue;
        };
        if commands.is_empty() {
            continue;
        }
        // Additive only: a command pushed while the list was in flight must not be withdrawn.
        let result = core.borrow_mut().push(
            PushBody {
                thread_id: thread_id.clone(),
                host: kind,
                snapshot: false,
                commands,
            },
            now_ms,
        )?;
        if let Ok(fx) = result {
            apply(core, host, fx, now_ms, emit).await?;
        }
    }
    Ok(())
}

/// A thread host pushed commands (Thread DO after it queued one, or the tracer hub).
pub async fn push_commands<S, H>(
    core: &Core<S>,
    host: &H,
    body: PushBody,
    now_ms: u64,
    emit: &mut impl FnMut(Out),
) -> StoreResult<Result<(), PushError>>
where
    S: MachineStore,
    H: ThreadHost,
{
    let result = core.borrow_mut().push(body, now_ms)?;
    match result {
        Ok(fx) => {
            apply(core, host, fx, now_ms, emit).await?;
            Ok(Ok(()))
        }
        Err(error) => Ok(Err(error)),
    }
}

/// A socket closed. Publishes the presence change if this was the current connection.
pub async fn connection_closed<S, H>(
    core: &Core<S>,
    host: &H,
    conn: u64,
    heartbeat_timeout: bool,
    now_ms: u64,
) -> StoreResult<()>
where
    S: MachineStore,
    H: ThreadHost,
{
    let change = core
        .borrow_mut()
        .connection_closed(conn, heartbeat_timeout, now_ms)?;
    if let Some(change) = change {
        publish_presence(core, host, &change, now_ms).await?;
    }
    Ok(())
}

/// The machine was revoked: persist it and publish `offline`. The caller closes the socket.
pub async fn revoke<S, H>(core: &Core<S>, host: &H, now_ms: u64) -> StoreResult<()>
where
    S: MachineStore,
    H: ThreadHost,
{
    let change = core.borrow_mut().revoke(now_ms)?;
    if let Some(change) = change {
        publish_presence(core, host, &change, now_ms).await?;
    }
    Ok(())
}

/// A command the Thread DO listed, as the core takes it.
pub fn push_command_from_json(value: &Value) -> Option<PushCommand> {
    let source_id = match value.get("id")? {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => return None,
    };
    Some(PushCommand {
        source_id,
        idempotency_key: value.get("idempotency_key")?.as_str()?.to_owned(),
        issued_at: value.get("issued_at")?.as_str()?.to_owned(),
        command: serde_json::from_value::<Command>(value.get("command")?.clone()).ok()?,
    })
}
