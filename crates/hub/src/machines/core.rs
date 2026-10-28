//! The Machine DO's logic (M2B-002), runtime-agnostic: [`MachineCore`] over a [`MachineStore`].
//!
//! It owns the tunnel bookkeeping of tunnel.md §4 on top of the pure state machines in
//! `protocol::tunnel`:
//!
//! - up stream: [`Receiver`] rebuilt from the persisted `up_last` (§4.2 receiver rules);
//! - down stream: [`Sender`] rebuilt from the durable outbox. The outbox stores no `seq`: the
//!   wire `seq` is `down_base + position`, so removing a never-sent row renumbers the rest for
//!   free (§5 "removal/renumbering"), and a fresh stream (§4.4) restarts at `down_base = 0`;
//! - resume tokens (§4.3): single-use, 7 days, only the hash is stored;
//! - presence (§3.7): `online` on `hello`, `asleep` / `offline` when the socket is lost.
//!
//! Everything here is synchronous. The cross-object steps (append to the Thread DO, publish a
//! `Presence` event, ack a command at the Thread DO) are done by `driver.rs`, which calls the
//! methods below between its `await` points and never holds a borrow across one.

use protocol::tunnel::{
    CloseCode, Command, Frame, PROTO_VERSION, Receiver, Sender, TunnelError, Verdict,
};
use protocol::turn_event::{MachinePresence, format_timestamp};

use super::credential::{self, new_resume_token};
use super::store::{
    ForwardState, ForwardedRecord, HostKind, MachineState, MachineStore, NewOutbox, OutboxRow,
    StoreError, StoreResult,
};

/// Heartbeat interval announced in `welcome` (tunnel.md §3.2).
pub const HEARTBEAT_MS: u64 = 20_000;

/// Unacked down frames allowed on the wire (tunnel.md §4.2 default).
pub const MAX_INFLIGHT: u32 = 256;

/// The runner is offline after this many silent heartbeats (tunnel.md §3.7).
pub const HEARTBEAT_MISSES: u64 = 3;

/// A resume token is valid this long (tunnel.md §4.3).
pub const RESUME_TOKEN_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Queued commands expire after this (tunnel.md §5).
pub const COMMAND_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// A socket that has not said `hello` by then is closed (tunnel.md §3.1: the runner gives up
/// on `welcome` after 10 s).
pub const HELLO_TIMEOUT_MS: u64 = 10_000;

/// What the DO glue does on the socket after a step.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// Send this frame on the connection.
    Send(Box<Frame>),
    /// Send `close{code}` and close the WebSocket with `4000 + n`.
    Close(CloseCode, String),
}

impl Out {
    pub fn send(frame: Frame) -> Self {
        Self::Send(Box::new(frame))
    }

    pub fn close(code: CloseCode, message: impl Into<String>) -> Self {
        Self::Close(code, message.into())
    }
}

/// A presence transition to publish (D1 row, `Presence` TurnEvent in every hosted thread).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceChange {
    pub state: MachinePresence,
    pub since: String,
}

/// Command ids to acknowledge at one thread host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadAcks {
    pub host: HostKind,
    pub thread_id: String,
    pub source_ids: Vec<String>,
}

/// The result of one core step.
#[derive(Debug, Default, PartialEq)]
pub struct Effects {
    pub out: Vec<Out>,
    pub presence: Option<PresenceChange>,
    /// Commands the runner has acked: tell their Thread DOs.
    pub acks: Vec<ThreadAcks>,
    /// Commands dropped after 7 days: fail their tasks.
    pub expired: Vec<OutboxRow>,
}

impl Effects {
    fn close(code: CloseCode, message: impl Into<String>) -> Self {
        Self {
            out: vec![Out::close(code, message)],
            ..Self::default()
        }
    }

    fn add_acks(&mut self, rows: &[OutboxRow]) {
        for row in rows {
            let slot = self
                .acks
                .iter_mut()
                .find(|a| a.host == row.host && a.thread_id == row.thread_id);
            match slot {
                Some(slot) => slot.source_ids.push(row.source_id.clone()),
                None => self.acks.push(ThreadAcks {
                    host: row.host,
                    thread_id: row.thread_id.clone(),
                    source_ids: vec![row.source_id.clone()],
                }),
            }
        }
    }
}

/// What `hello` carries (tunnel.md §3.1).
#[derive(Debug, Clone, PartialEq)]
pub struct HelloInfo {
    pub proto: u32,
    pub machine_id: String,
    pub runner_version: String,
    pub resume_token: Option<String>,
    pub last_down_seq: u64,
    pub capabilities: serde_json::Value,
}

/// How an upgrade request authenticated (tunnel.md §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    Ok,
    /// No `Authorization: Bearer` header.
    Missing,
    /// A credential that does not belong to this machine.
    Invalid,
    /// The machine's own credential, but the machine was revoked.
    Revoked,
}

/// The up-stream verdict for a sequenced frame (tunnel.md §4.2 receiver rules).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpCheck {
    /// The next frame in order: apply it, then [`MachineCore::complete_up`].
    Process,
    /// Already processed: re-ack `ack`.
    Duplicate { ack: u64 },
    /// A gap or `seq` 0: `close{protocol_error}`.
    Violation(String),
}

/// A batch of commands pushed by a thread host.
#[derive(Debug, Clone, PartialEq)]
pub struct PushBody {
    pub thread_id: String,
    pub host: HostKind,
    /// `true`: `commands` is the thread's full set of unacked commands, so a queued command that
    /// is missing from it was withdrawn (the Thread DO retracts a prompt of a cancelled task).
    pub snapshot: bool,
    pub commands: Vec<PushCommand>,
}

/// One pushed command.
#[derive(Debug, Clone, PartialEq)]
pub struct PushCommand {
    pub source_id: String,
    pub idempotency_key: String,
    pub issued_at: String,
    pub command: Command,
}

/// Why a push was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushError(pub String);

/// Outcome of [`MachineCore::provision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provisioned {
    Created,
    /// Same machine, org and credential hash: nothing to do.
    Unchanged,
    /// The credential was replaced (dev route only); the machine is active again.
    Replaced,
    /// Already provisioned with another identity or credential.
    Conflict,
}

/// What the alarm should do about the heartbeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heartbeat {
    /// Nothing connected.
    Idle,
    /// Healthy; check again at this time (ms).
    Healthy { next_check_ms: u64 },
    /// The socket said nothing for 3 x `heartbeat_ms`: close it, presence `offline`.
    Overdue,
    /// The socket never sent `hello`.
    HelloTimeout,
}

fn store_error(error: impl std::fmt::Display) -> StoreError {
    StoreError::new(error)
}

fn protocol_error(error: &TunnelError) -> Out {
    Out::close(CloseCode::ProtocolError, error.to_string())
}

/// The Machine DO's logic over one store.
pub struct MachineCore<S: MachineStore> {
    store: S,
    state: MachineState,
}

impl<S: MachineStore> MachineCore<S> {
    /// Opens the machine from its store (a never-provisioned machine has an empty state).
    pub fn open(store: S) -> StoreResult<Self> {
        let state = store.load()?.unwrap_or_default();
        Ok(Self { store, state })
    }

    pub fn state(&self) -> &MachineState {
        &self.state
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn machine_id(&self) -> &str {
        &self.state.machine_id
    }

    pub fn is_provisioned(&self) -> bool {
        !self.state.machine_id.is_empty()
    }

    fn save(&mut self) -> StoreResult<()> {
        self.store.save(&self.state)
    }

    // ---- provisioning, auth, revocation ------------------------------------------------------

    /// Stores the machine's identity and credential hash (admin API or the dev route).
    /// `replace` swaps the credential of an already provisioned machine of the same identity
    /// (the DEV_FIXTURES credential route; the admin API never replaces).
    pub fn provision(
        &mut self,
        machine_id: &str,
        org_id: &str,
        credential_hash: &str,
        replace: bool,
    ) -> StoreResult<Provisioned> {
        if self.is_provisioned() {
            if replace && self.state.machine_id == machine_id && self.state.org_id == org_id {
                self.state.credential_hash = Some(credential_hash.to_owned());
                self.state.revoked = false;
                self.state.resume_hash = None;
                self.save()?;
                return Ok(Provisioned::Replaced);
            }
            let same = self.state.machine_id == machine_id
                && self.state.org_id == org_id
                && self.state.credential_hash.as_deref() == Some(credential_hash)
                && !self.state.revoked;
            return Ok(if same {
                Provisioned::Unchanged
            } else {
                Provisioned::Conflict
            });
        }
        self.state = MachineState {
            machine_id: machine_id.to_owned(),
            org_id: org_id.to_owned(),
            credential_hash: Some(credential_hash.to_owned()),
            presence: "offline".to_owned(),
            heartbeat_ms: HEARTBEAT_MS,
            ..MachineState::default()
        };
        self.save()?;
        Ok(Provisioned::Created)
    }

    /// Checks the `Authorization` header of a tunnel upgrade. The machine identity is the one
    /// the credential was issued for (tunnel.md §6): a credential of another machine never
    /// matches this object's hash.
    pub fn authenticate(&self, authorization: Option<&str>) -> Auth {
        let Some(presented) = crate::auth::bearer_token(authorization) else {
            return Auth::Missing;
        };
        if !presented.starts_with(credential::CREDENTIAL_PREFIX) {
            return Auth::Invalid;
        }
        match &self.state.credential_hash {
            Some(stored) if credential::matches(stored, presented) => {
                if self.state.revoked {
                    Auth::Revoked
                } else {
                    Auth::Ok
                }
            }
            _ => Auth::Invalid,
        }
    }

    /// Revokes the machine: the credential stops working, every resume token dies, presence is
    /// `offline` (tunnel.md §4.3 item 5). The caller closes the live socket with `auth_revoked`.
    pub fn revoke(&mut self, now_ms: u64) -> StoreResult<Option<PresenceChange>> {
        self.state.revoked = true;
        self.state.resume_hash = None;
        self.state.cur_conn = 0;
        self.state.hello_done = false;
        self.state.asleep_hint = false;
        let change = self.set_presence(MachinePresence::Offline, now_ms);
        self.save()?;
        Ok(change)
    }

    // ---- connections -------------------------------------------------------------------------

    /// A new socket was accepted (and authenticated): it becomes the current connection.
    pub fn begin_connection(&mut self, now_ms: u64) -> StoreResult<u64> {
        self.state.conn_counter += 1;
        self.state.cur_conn = self.state.conn_counter;
        self.state.hello_done = false;
        self.state.last_seen_ms = now_ms;
        self.save()?;
        Ok(self.state.cur_conn)
    }

    /// `true` when `conn` is the accepted connection.
    pub fn is_current(&self, conn: u64) -> bool {
        conn != 0 && self.state.cur_conn == conn
    }

    pub fn hello_done(&self) -> bool {
        self.state.hello_done
    }

    /// Records that a frame arrived on the current connection.
    pub fn touch(&mut self, now_ms: u64) -> StoreResult<()> {
        self.state.last_seen_ms = now_ms;
        self.save()
    }

    /// The runner announced it is going to sleep (`presence{asleep}`, `close{going_to_sleep}`).
    pub fn note_asleep_hint(&mut self) -> StoreResult<()> {
        self.state.asleep_hint = true;
        self.save()
    }

    /// The runner is shutting down for good (`close{normal}`): the next loss is `offline`.
    pub fn note_normal_close(&mut self) -> StoreResult<()> {
        self.state.asleep_hint = false;
        self.save()
    }

    /// The connection's socket is gone. Returns the presence change, if any. A socket that is
    /// no longer current (superseded) changes nothing.
    pub fn connection_closed(
        &mut self,
        conn: u64,
        heartbeat_timeout: bool,
        now_ms: u64,
    ) -> StoreResult<Option<PresenceChange>> {
        if !self.is_current(conn) {
            return Ok(None);
        }
        self.state.cur_conn = 0;
        self.state.hello_done = false;
        let target = if self.state.asleep_hint && !heartbeat_timeout {
            MachinePresence::Asleep
        } else {
            MachinePresence::Offline
        };
        let change = if self.state.presence == "online" {
            self.set_presence(target, now_ms)
        } else {
            None
        };
        self.save()?;
        Ok(change)
    }

    /// What the heartbeat alarm should do now. `auto_response_ms` is the time of the last ping
    /// the runtime answered without waking the DO, when the platform reports it.
    pub fn heartbeat(&self, now_ms: u64, auto_response_ms: Option<u64>) -> Heartbeat {
        if self.state.cur_conn == 0 {
            return Heartbeat::Idle;
        }
        if !self.state.hello_done {
            let waited = now_ms.saturating_sub(self.state.last_seen_ms);
            return if waited > HELLO_TIMEOUT_MS {
                Heartbeat::HelloTimeout
            } else {
                Heartbeat::Healthy {
                    next_check_ms: self.state.last_seen_ms + HELLO_TIMEOUT_MS + 1,
                }
            };
        }
        let heartbeat = self.state.heartbeat_ms.max(1_000);
        let last = self.state.last_seen_ms.max(auto_response_ms.unwrap_or(0));
        let deadline = last + HEARTBEAT_MISSES * heartbeat;
        if now_ms > deadline {
            Heartbeat::Overdue
        } else {
            Heartbeat::Healthy {
                next_check_ms: deadline + 1,
            }
        }
    }

    fn set_presence(&mut self, state: MachinePresence, now_ms: u64) -> Option<PresenceChange> {
        let name = presence_name(state);
        if self.state.presence == name {
            return None;
        }
        let since = format_timestamp(now_ms);
        self.state.presence = name.to_owned();
        self.state.presence_since = since.clone();
        Some(PresenceChange { state, since })
    }

    // ---- hello / welcome ---------------------------------------------------------------------

    /// Answers `hello` (tunnel.md §3.1, §3.2, §4.3, §4.4): validates, decides `resumed`, mints
    /// the next resume token, sets presence `online`, and sends `welcome` plus the queued
    /// commands the runner has not processed.
    pub fn on_hello(&mut self, hello: HelloInfo, now_ms: u64) -> StoreResult<Effects> {
        if self.state.hello_done {
            return Ok(Effects::close(CloseCode::ProtocolError, "second hello"));
        }
        if hello.proto != PROTO_VERSION {
            return Ok(Effects::close(
                CloseCode::VersionUnsupported,
                format!("tunnel proto {} is not supported", hello.proto),
            ));
        }
        // The identity is the credential's; `hello.machine_id` must agree with it (§6).
        if hello.machine_id != self.state.machine_id {
            return Ok(Effects::close(
                CloseCode::AuthInvalid,
                "hello.machine_id does not match the credential",
            ));
        }
        let mut fx = Effects::default();

        // The token is single-use: it is consumed whatever the outcome (§4.3 item 3).
        let token_valid = match (&hello.resume_token, &self.state.resume_hash) {
            (Some(token), Some(stored)) => {
                now_ms < self.state.resume_expires_ms && credential::matches(stored, token)
            }
            _ => false,
        };
        self.state.resume_hash = None;

        let rows = self.store.outbox()?;
        let base = self.state.down_base;
        let cursor = hello.last_down_seq;
        let in_range = cursor >= base && cursor <= base + rows.len() as u64;
        // A valid token with a cursor outside what we hold means the two sides disagree:
        // start a fresh stream, which is always safe (the runner dedupes by idempotency key).
        let resumed = token_valid && in_range;

        if resumed {
            let mut sender = self.sender(&rows);
            sender
                .on_welcome(true, cursor - base)
                .map_err(|e| store_error(format!("welcome cursor: {e}")))?;
            let processed = usize::try_from(cursor - base).unwrap_or(usize::MAX);
            let done: Vec<OutboxRow> = rows.iter().take(processed).cloned().collect();
            self.finish_rows(&done, ForwardState::Delivered, now_ms)?;
            fx.add_acks(&done);
            self.state.down_base = cursor;
            self.state.down_sent = cursor;
        } else {
            self.state.down_base = 0;
            self.state.down_sent = 0;
            self.state.up_last = 0;
            self.state.up_acked = 0;
            self.state.stream_epoch += 1;
        }

        let token = new_resume_token().map_err(|e| store_error(e.message))?;
        self.state.resume_hash = Some(credential::hash(&token));
        self.state.resume_expires_ms = now_ms + RESUME_TOKEN_TTL_MS;
        self.state.hello_done = true;
        self.state.heartbeat_ms = HEARTBEAT_MS;
        self.state.last_seen_ms = now_ms;
        self.state.runner_version = hello.runner_version;
        self.state.capabilities = Some(hello.capabilities);
        self.state.asleep_hint = false;
        fx.presence = self.set_presence(MachinePresence::Online, now_ms);
        self.save()?;

        self.expire_unsent(now_ms, &mut fx)?;
        self.store.prune(now_ms)?;

        fx.out.push(Out::send(Frame::Welcome {
            proto: PROTO_VERSION,
            resumed,
            resume_token: token,
            last_up_seq: self.state.up_last,
            heartbeat_ms: HEARTBEAT_MS,
            max_inflight: MAX_INFLIGHT,
        }));
        fx.out.extend(self.drain()?.into_iter().map(Out::send));
        Ok(fx)
    }

    // ---- down stream -------------------------------------------------------------------------

    /// A [`Sender`] over the outbox, positioned after the frames already on the wire.
    fn sender(&self, rows: &[OutboxRow]) -> Sender<OutboxRow> {
        let mut sender = Sender::new(u64::from(MAX_INFLIGHT));
        for row in rows {
            sender.enqueue(row.clone());
        }
        for _ in 0..self.state.down_sent.saturating_sub(self.state.down_base) {
            let _ = sender.next_to_send();
        }
        sender
    }

    /// Every command frame the window allows, once the runner has said `hello`.
    fn drain(&mut self) -> StoreResult<Vec<Frame>> {
        if !self.state.hello_done || self.state.cur_conn == 0 {
            return Ok(Vec::new());
        }
        let rows = self.store.outbox()?;
        let mut sender = self.sender(&rows);
        let mut frames = Vec::new();
        while let Some((position, row)) = sender.next_to_send() {
            let seq = self.state.down_base + position;
            frames.push(Frame::Command {
                seq,
                idempotency_key: row.idempotency_key,
                issued_at: row.issued_at,
                command: row.command,
            });
            self.state.down_sent = seq;
        }
        if !frames.is_empty() {
            self.save()?;
        }
        Ok(frames)
    }

    /// Cumulative `ack` of the down stream (tunnel.md §3.7).
    pub fn on_ack(&mut self, seq: u64, now_ms: u64) -> StoreResult<Effects> {
        let mut fx = Effects::default();
        let base = self.state.down_base;
        if seq <= base {
            return Ok(fx);
        }
        let rows = self.store.outbox()?;
        let mut sender = self.sender(&rows);
        if let Err(error) = sender.on_ack(seq - base) {
            fx.out.push(protocol_error(&error));
            return Ok(fx);
        }
        let count = usize::try_from(seq - base).unwrap_or(usize::MAX);
        let done: Vec<OutboxRow> = rows.into_iter().take(count).collect();
        self.finish_rows(&done, ForwardState::Delivered, now_ms)?;
        fx.add_acks(&done);
        self.state.down_base = seq;
        self.state.down_sent = self.state.down_sent.max(seq);
        self.save()?;
        fx.out.extend(self.drain()?.into_iter().map(Out::send));
        Ok(fx)
    }

    /// Removes `rows` from the outbox and records their new state.
    fn finish_rows(
        &mut self,
        rows: &[OutboxRow],
        state: ForwardState,
        now_ms: u64,
    ) -> StoreResult<()> {
        let ids: Vec<u64> = rows.iter().map(|row| row.id).collect();
        self.store.outbox_remove(&ids)?;
        for row in rows {
            self.mark(row, state, now_ms)?;
        }
        Ok(())
    }

    fn mark(&mut self, row: &OutboxRow, state: ForwardState, now_ms: u64) -> StoreResult<()> {
        self.store.put_forwarded(&ForwardedRecord {
            thread_id: row.thread_id.clone(),
            source_id: row.source_id.clone(),
            host: row.host,
            idempotency_key: row.idempotency_key.clone(),
            kind: command_kind(&row.command).to_owned(),
            task_id: command_task(&row.command),
            state,
            updated_ms: now_ms,
        })
    }

    /// Drops rows older than 7 days that never went on the wire (tunnel.md §5).
    fn expire_unsent(&mut self, now_ms: u64, fx: &mut Effects) -> StoreResult<()> {
        let rows = self.store.outbox()?;
        let on_wire = usize::try_from(self.state.down_sent.saturating_sub(self.state.down_base))
            .unwrap_or(usize::MAX);
        let expired: Vec<OutboxRow> = rows
            .into_iter()
            .skip(on_wire)
            .filter(|row| now_ms.saturating_sub(row.enqueued_ms) >= COMMAND_TTL_MS)
            .collect();
        if expired.is_empty() {
            return Ok(());
        }
        self.finish_rows(&expired, ForwardState::Expired, now_ms)?;
        fx.add_acks(&expired);
        fx.expired.extend(expired);
        Ok(())
    }

    /// Commands pushed by a Thread DO: queue the new ones, withdraw the
    /// ones a snapshot no longer lists, and send what the connection allows. Registers the
    /// thread as hosted by this machine (a queued command proves the hub put it here).
    pub fn push(&mut self, body: PushBody, now_ms: u64) -> StoreResult<Result<Effects, PushError>> {
        if !self.is_provisioned() || self.state.revoked {
            return Ok(Err(PushError("machine is not active".to_owned())));
        }
        if let Some(bad) = body
            .commands
            .iter()
            .find(|c| command_thread(&c.command) != body.thread_id)
        {
            return Ok(Err(PushError(format!(
                "command {} is for thread {}, not {}",
                bad.source_id,
                command_thread(&bad.command),
                body.thread_id
            ))));
        }
        let mut fx = Effects::default();
        self.store.host_thread(&body.thread_id, body.host, now_ms)?;

        let mut reack = Vec::new();
        for pushed in &body.commands {
            match self.store.forwarded(&body.thread_id, &pushed.source_id)? {
                None => {
                    self.store.outbox_push(NewOutbox {
                        thread_id: body.thread_id.clone(),
                        source_id: pushed.source_id.clone(),
                        host: body.host,
                        idempotency_key: pushed.idempotency_key.clone(),
                        issued_at: pushed.issued_at.clone(),
                        enqueued_ms: now_ms,
                        command: pushed.command.clone(),
                    })?;
                    self.store.put_forwarded(&ForwardedRecord {
                        thread_id: body.thread_id.clone(),
                        source_id: pushed.source_id.clone(),
                        host: body.host,
                        idempotency_key: pushed.idempotency_key.clone(),
                        kind: command_kind(&pushed.command).to_owned(),
                        task_id: command_task(&pushed.command),
                        state: ForwardState::Queued,
                        updated_ms: now_ms,
                    })?;
                }
                // The runner has it but the Thread DO has not heard: say so again.
                Some(record)
                    if matches!(
                        record.state,
                        ForwardState::Delivered | ForwardState::Expired
                    ) =>
                {
                    reack.push(pushed.source_id.clone());
                }
                Some(_) => {}
            }
        }
        if !reack.is_empty() {
            fx.acks.push(ThreadAcks {
                host: body.host,
                thread_id: body.thread_id.clone(),
                source_ids: reack,
            });
        }

        if body.snapshot {
            self.withdraw_missing(&body, now_ms)?;
        }
        self.expire_unsent(now_ms, &mut fx)?;
        fx.out.extend(self.drain()?.into_iter().map(Out::send));
        Ok(Ok(fx))
    }

    /// Removes never-sent rows of the snapshot's thread that the snapshot does not list.
    fn withdraw_missing(&mut self, body: &PushBody, now_ms: u64) -> StoreResult<()> {
        let rows = self.store.outbox()?;
        let on_wire = usize::try_from(self.state.down_sent.saturating_sub(self.state.down_base))
            .unwrap_or(usize::MAX);
        let withdrawn: Vec<OutboxRow> = rows
            .into_iter()
            .skip(on_wire)
            .filter(|row| {
                row.thread_id == body.thread_id
                    && !body.commands.iter().any(|c| c.source_id == row.source_id)
            })
            .collect();
        if !withdrawn.is_empty() {
            self.finish_rows(&withdrawn, ForwardState::Retracted, now_ms)?;
        }
        Ok(())
    }

    // ---- up stream ---------------------------------------------------------------------------

    /// Applies the receiver rules to an incoming sequenced frame without changing anything.
    pub fn check_up(&self, seq: u64) -> UpCheck {
        let mut receiver = Receiver::resume(self.state.up_last);
        match receiver.on_frame(seq) {
            Ok(Verdict::Process) => UpCheck::Process,
            Ok(Verdict::Duplicate { ack }) => UpCheck::Duplicate { ack },
            Err(error) => UpCheck::Violation(error.to_string()),
        }
    }

    /// The stream epoch a frame is processed under (see [`MachineCore::complete_up`]).
    pub fn stream_epoch(&self) -> u64 {
        self.state.stream_epoch
    }

    pub fn event_seen(&self, event_id: &str, now_ms: u64) -> StoreResult<bool> {
        self.store.event_seen(event_id, now_ms)
    }

    /// The effect of up frame `seq` is durable: remember its `event_id` (24 h) and advance
    /// `last`. A late completion of an older stream (the socket was replaced while the append
    /// was in flight) still records the `event_id`, which is what keeps the retransmit from
    /// appending twice, but leaves the new stream's `last` alone.
    pub fn complete_up(
        &mut self,
        epoch: u64,
        seq: u64,
        event_id: Option<&str>,
        now_ms: u64,
    ) -> StoreResult<()> {
        if let Some(event_id) = event_id {
            self.store.record_event(event_id, now_ms)?;
        }
        if epoch == self.state.stream_epoch && seq == self.state.up_last + 1 {
            self.state.up_last = seq;
            self.save()?;
        }
        Ok(())
    }

    /// The cumulative ack to send now, if `last` moved since the last one.
    pub fn take_ack(&mut self) -> StoreResult<Option<u64>> {
        if self.state.up_last > self.state.up_acked {
            self.state.up_acked = self.state.up_last;
            self.save()?;
            return Ok(Some(self.state.up_last));
        }
        Ok(None)
    }

    // ---- hosted threads and sent commands ----------------------------------------------------

    pub fn hosted(&self, thread_id: &str) -> StoreResult<Option<HostKind>> {
        self.store.hosted(thread_id)
    }

    pub fn hosted_threads(&self) -> StoreResult<Vec<(String, HostKind)>> {
        self.store.hosted_threads()
    }

    pub fn host_thread(&mut self, thread_id: &str, host: HostKind, now_ms: u64) -> StoreResult<()> {
        self.store.host_thread(thread_id, host, now_ms)
    }

    /// The command that went down under this idempotency key (for `command_result`).
    pub fn sent_command(&self, idempotency_key: &str) -> StoreResult<Option<ForwardedRecord>> {
        self.store.forwarded_by_key(idempotency_key)
    }
}

fn presence_name(state: MachinePresence) -> &'static str {
    match state {
        MachinePresence::Online => "online",
        MachinePresence::Asleep => "asleep",
        MachinePresence::Offline => "offline",
    }
}

/// `prompt`, `approval`, `dispatch`, `cancel` or `session_config`.
pub fn command_kind(command: &Command) -> &'static str {
    match command {
        Command::Prompt { .. } => "prompt",
        Command::Approval { .. } => "approval",
        Command::Dispatch { .. } => "dispatch",
        Command::Cancel { .. } => "cancel",
        Command::SessionConfig { .. } => "session_config",
    }
}

/// The thread a command is for.
pub fn command_thread(command: &Command) -> &str {
    match command {
        Command::Prompt { thread_id, .. }
        | Command::Approval { thread_id, .. }
        | Command::Dispatch { thread_id, .. }
        | Command::Cancel { thread_id, .. }
        | Command::SessionConfig { thread_id, .. } => thread_id,
    }
}

/// The task a command is for; empty for `session_config`, which belongs to the thread.
pub fn command_task(command: &Command) -> String {
    match command {
        Command::Prompt { task_id, .. }
        | Command::Approval { task_id, .. }
        | Command::Dispatch { task_id, .. }
        | Command::Cancel { task_id, .. } => task_id.clone(),
        Command::SessionConfig { .. } => String::new(),
    }
}
