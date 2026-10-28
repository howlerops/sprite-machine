//! The Machine Durable Object (M2B-002, wasm32 only): one per machine, SQLite-backed. It is the
//! hub end of the runner's tunnel (docs/specs/tunnel.md).
//!
//! - `GET /v1/machines/{id}/tunnel` (the Worker forwards it): subprotocol `sm.tunnel.v1`,
//!   `Authorization: Bearer <machine credential>` checked against the stored hash. Missing
//!   credential: HTTP 401. Wrong credential: the socket is accepted and closed with
//!   `close{auth_invalid}`; the revoked machine's own credential: `close{auth_revoked}`. A new
//!   authenticated connection closes the old one with `close{superseded}`.
//! - Hibernation API (`accept_websocket_with_tags`, `websocket_message`, auto-response for the
//!   literal ping, tunnel.md §3.7). Frames of one socket are handled strictly one after the
//!   other (a queue drained by one loop), because handling a frame awaits the Thread DO.
//! - `alarm()` is the heartbeat watchdog: no traffic for 3 x `heartbeat_ms` closes the socket
//!   with `heartbeat_timeout` and publishes presence `offline`.
//! - Internal routes (only reachable through a DO stub): `provision`, `revoke`, `disconnect`, `commands`
//!   (the push of a Thread DO) and `state`.
//!
//! All rules live in `machines::{core, driver}`, which are native-tested; this file only wires
//! them to sockets, storage and the other objects.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::VecDeque;

use protocol::tunnel::{CloseCode, Command, Frame, SUBPROTOCOL};
use serde::Deserialize;
use serde_json::{Value, json};
// `#[durable_object]` expands to wasm-bindgen code that expects `wasm_bindgen` in scope.
use worker::wasm_bindgen;
use worker::wasm_bindgen::JsValue;
use worker::{
    DurableObject, Env, Method, Request, Response, State, WebSocket, WebSocketIncomingMessage,
    WebSocketPair, WebSocketRequestResponsePair, durable_object,
};

use crate::machines::core::{HELLO_TIMEOUT_MS, Heartbeat, Provisioned, PushBody, PushCommand};
use crate::machines::driver::{self, Core};
use crate::machines::internal::WorkerHost;
use crate::machines::sqlite::SqlMachineStore;
use crate::machines::store::{HostKind, MachineStore};
use crate::machines::{Auth, MachineCore, Out};
use crate::notify::presence::{MachineBackDebounce, machine_back_job, machine_back_thread};
use crate::queues_notify;

const TUNNEL_TAG: &str = "tunnel";
/// The storage key of the machine-back debounce (`notify::presence`), one value per machine.
const BACK_DEBOUNCE_KEY: &str = "machine_back_debounce";
const PING: &str = r#"{"t":"ping"}"#;
const PONG: &str = r#"{"t":"pong"}"#;

/// WebSocket close reasons are limited to 123 bytes.
const MAX_REASON_BYTES: usize = 120;

/// One event waiting for its turn. Frames and socket losses share the queue, so a loss is
/// handled (and its presence published) before the frames of the connection that follows it.
enum Queued {
    Frame {
        ws: WebSocket,
        conn: u64,
        text: String,
    },
    Gone {
        conn: u64,
        heartbeat_timeout: bool,
    },
}

/// What the runtime knows about the pings it answered without waking the DO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoResponse {
    /// This runtime has no such record.
    Unsupported,
    /// No ping was auto-answered on the socket yet.
    Never,
    /// The last auto-answered ping, in ms since the epoch.
    At(u64),
}

impl AutoResponse {
    fn millis(self) -> Option<u64> {
        match self {
            Self::At(ms) => Some(ms),
            Self::Unsupported | Self::Never => None,
        }
    }
}

/// Resets the "a loop is draining the queue" flag, also when the loop ends early.
struct DrainGuard<'a>(&'a Cell<bool>);

impl Drop for DrainGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// The Machine DO (binding `MACHINE`).
#[durable_object]
pub struct Machine {
    state: State,
    env: Env,
    core: OnceCell<Core<SqlMachineStore>>,
    queue: RefCell<VecDeque<Queued>>,
    draining: Cell<bool>,
}

impl DurableObject for Machine {
    fn new(state: State, env: Env) -> Self {
        Self {
            state,
            env,
            core: OnceCell::new(),
            queue: RefCell::new(VecDeque::new()),
            draining: Cell::new(false),
        }
    }

    async fn fetch(&self, mut req: Request) -> worker::Result<Response> {
        let path = req.path();
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        match (req.method(), segments.as_slice()) {
            (Method::Get, ["v1", "machines", machine_id, "tunnel"]) => {
                self.accept_tunnel(&req, machine_id).await
            }
            (Method::Post, ["internal", "machines", machine_id, action]) => {
                let body = match req.text().await {
                    Ok(text) if text.trim().is_empty() => Value::Null,
                    Ok(text) => match serde_json::from_str(&text) {
                        Ok(value) => value,
                        Err(error) => {
                            return json_response(
                                400,
                                &error_body(&format!("body is not JSON: {error}")),
                            );
                        }
                    },
                    Err(error) => return json_response(400, &error_body(&error.to_string())),
                };
                match *action {
                    "provision" => self.provision(machine_id, &body),
                    "revoke" => self.revoke().await,
                    "disconnect" => self.disconnect().await,
                    "commands" => self.push(&body).await,
                    _ => json_response(404, &error_body("no such internal route")),
                }
            }
            (Method::Get, ["internal", "machines", _, "state"]) => self.dump(),
            _ => json_response(404, &error_body("not found")),
        }
    }

    async fn websocket_message(
        &self,
        ws: WebSocket,
        msg: WebSocketIncomingMessage,
    ) -> worker::Result<()> {
        let conn = ws
            .deserialize_attachment::<u64>()
            .ok()
            .flatten()
            .unwrap_or(0);
        let text = match msg {
            WebSocketIncomingMessage::String(text) => text,
            // Frames are UTF-8 JSON text; binary is a protocol error (tunnel.md §2).
            WebSocketIncomingMessage::Binary(_) => {
                self.send_out(
                    &ws,
                    Out::close(CloseCode::ProtocolError, "binary frames are not allowed"),
                );
                return Ok(());
            }
        };
        self.enqueue(Queued::Frame { ws, conn, text }).await
    }

    async fn websocket_close(
        &self,
        ws: WebSocket,
        _code: usize,
        _reason: String,
        _clean: bool,
    ) -> worker::Result<()> {
        let result = self.socket_gone(&ws).await;
        // Complete the close handshake. 1005/1006 cannot be sent, so always answer 1000.
        let _ = ws.close(Some(1000), Some("bye"));
        result
    }

    async fn websocket_error(&self, ws: WebSocket, _error: worker::Error) -> worker::Result<()> {
        self.socket_gone(&ws).await
    }

    async fn alarm(&self) -> worker::Result<Response> {
        let core = self.core()?;
        let now = now_ms();
        let conn = core.borrow().state().cur_conn;
        let auto = self
            .sockets_of(conn)
            .first()
            .map_or(AutoResponse::Never, |ws| self.auto_response(ws));
        let mut verdict = core.borrow().heartbeat(now, auto.millis());
        if verdict == Heartbeat::Overdue && auto == AutoResponse::Unsupported {
            // Without the runtime's record of auto-answered pings the hub cannot tell a quiet
            // healthy runner from a dead one: do not close a socket on that guess.
            verdict = Heartbeat::Healthy {
                next_check_ms: now + 60_000,
            };
        }
        match verdict {
            Heartbeat::Idle => {}
            Heartbeat::Healthy { next_check_ms } => {
                let delay = next_check_ms.saturating_sub(now).max(1_000);
                self.state
                    .storage()
                    .set_alarm(i64::try_from(delay).unwrap_or(i64::MAX))
                    .await?;
            }
            Heartbeat::Overdue => {
                for ws in self.sockets_of(conn) {
                    close_socket(&ws, CloseCode::HeartbeatTimeout, "no heartbeat");
                }
                self.enqueue(Queued::Gone {
                    conn,
                    heartbeat_timeout: true,
                })
                .await?;
            }
            Heartbeat::HelloTimeout => {
                for ws in self.sockets_of(conn) {
                    close_socket(&ws, CloseCode::ProtocolError, "no hello within 10 s");
                }
                self.enqueue(Queued::Gone {
                    conn,
                    heartbeat_timeout: false,
                })
                .await?;
            }
        }
        Response::ok("ok")
    }
}

impl Machine {
    /// The core, opened from SQLite on first use.
    fn core(&self) -> worker::Result<&Core<SqlMachineStore>> {
        if let Some(core) = self.core.get() {
            return Ok(core);
        }
        let store = SqlMachineStore::open(self.state.storage().sql()).map_err(storage_error)?;
        let opened = MachineCore::open(store).map_err(storage_error)?;
        let _ = self.core.set(RefCell::new(opened));
        self.core
            .get()
            .ok_or_else(|| worker::Error::RustError("the machine core did not open".to_owned()))
    }

    fn host(&self) -> WorkerHost<'_> {
        WorkerHost { env: &self.env }
    }

    /// The sockets tagged with connection number `conn`.
    fn sockets_of(&self, conn: u64) -> Vec<WebSocket> {
        if conn == 0 {
            return Vec::new();
        }
        self.state.get_websockets_with_tag(&format!("conn:{conn}"))
    }

    async fn accept_tunnel(&self, req: &Request, machine_id: &str) -> worker::Result<Response> {
        let upgrade = req.headers().get("Upgrade")?.unwrap_or_default();
        if !upgrade.eq_ignore_ascii_case("websocket") {
            return Response::error("expected Upgrade: websocket", 426);
        }
        let protocols = req
            .headers()
            .get("Sec-WebSocket-Protocol")?
            .unwrap_or_default();
        if !protocols.split(',').any(|p| p.trim() == SUBPROTOCOL) {
            return Response::error(format!("subprotocol {SUBPROTOCOL} required"), 426);
        }
        let core = self.core()?;
        let authorization = req.headers().get("Authorization")?;
        let mut auth = core.borrow().authenticate(authorization.as_deref());
        if core.borrow().machine_id() != machine_id {
            auth = Auth::Invalid;
        }
        match auth {
            Auth::Missing => json_response(401, &error_body("missing machine credential")),
            Auth::Invalid => self.refuse(CloseCode::AuthInvalid, "invalid machine credential"),
            Auth::Revoked => self.refuse(CloseCode::AuthRevoked, "the machine was revoked"),
            Auth::Ok => {
                // One tunnel per machine: the new connection supersedes the old one (§2).
                for old in self.state.get_websockets_with_tag(TUNNEL_TAG) {
                    close_socket(
                        &old,
                        CloseCode::Superseded,
                        "a newer connection for this machine was accepted",
                    );
                }
                let conn = core
                    .borrow_mut()
                    .begin_connection(now_ms())
                    .map_err(storage_error)?;
                let pair = WebSocketPair::new()?;
                self.state.accept_websocket_with_tags(
                    &pair.server,
                    &[TUNNEL_TAG, &format!("conn:{conn}")],
                );
                pair.server.serialize_attachment(conn)?;
                // Answered by the runtime without waking the DO (tunnel.md §3.7).
                self.state
                    .set_websocket_auto_response(&WebSocketRequestResponsePair::new(PING, PONG)?);
                self.state
                    .storage()
                    .set_alarm(i64::try_from(HELLO_TIMEOUT_MS + 1_000).unwrap_or(i64::MAX))
                    .await?;
                let mut response = Response::from_websocket(pair.client)?;
                response
                    .headers_mut()
                    .set("Sec-WebSocket-Protocol", SUBPROTOCOL)?;
                Ok(response)
            }
        }
    }

    /// Accepts the socket and closes it at once with `code`, so the runner learns why.
    fn refuse(&self, code: CloseCode, message: &str) -> worker::Result<Response> {
        let pair = WebSocketPair::new()?;
        self.state.accept_web_socket(&pair.server);
        close_socket(&pair.server, code, message);
        let mut response = Response::from_websocket(pair.client)?;
        response
            .headers_mut()
            .set("Sec-WebSocket-Protocol", SUBPROTOCOL)?;
        Ok(response)
    }

    /// Queues an event and, unless a loop is already draining the queue, drains it. One loop at
    /// a time keeps everything in order across the `await`s of a thread host call.
    async fn enqueue(&self, item: Queued) -> worker::Result<()> {
        self.queue.borrow_mut().push_back(item);
        if self.draining.replace(true) {
            return Ok(());
        }
        let _guard = DrainGuard(&self.draining);
        loop {
            let next = self.queue.borrow_mut().pop_front();
            let Some(item) = next else {
                return Ok(());
            };
            self.process(item).await?;
        }
    }

    /// Handles one queued event.
    async fn process(&self, item: Queued) -> worker::Result<()> {
        let core = self.core()?;
        let host = self.host();
        match item {
            Queued::Frame { ws, conn, text } => {
                let before = core.borrow().state().presence.clone();
                driver::handle_text(core, &host, conn, &text, now_ms(), &mut |out| {
                    self.send_out(&ws, out);
                })
                .await;
                let after = core.borrow().state().presence.clone();
                if before != after {
                    self.announce_machine_back(core, &before, &after).await;
                }
                // The batched ack goes out once the frames queued behind this one are handled.
                if self.queue.borrow().is_empty()
                    && let Some(out) = driver::flush_ack(core)
                {
                    self.send_out(&ws, out);
                }
            }
            Queued::Gone {
                conn,
                heartbeat_timeout,
            } => {
                if let Err(error) =
                    driver::connection_closed(core, &host, conn, heartbeat_timeout, now_ms()).await
                {
                    worker::console_log!("machine: closing connection {conn}: {error}");
                }
            }
        }
        Ok(())
    }

    fn send_out(&self, ws: &WebSocket, out: Out) {
        match out {
            Out::Send(frame) => {
                if let Ok(text) = serde_json::to_string(&frame) {
                    let _ = ws.send_with_str(&text);
                }
            }
            Out::Close(code, message) => close_socket(ws, code, &message),
        }
    }

    /// Sends on the current connection's socket (commands pushed from outside a frame).
    fn send_current(&self, out: Out) {
        let conn = self.core().map_or(0, |core| core.borrow().state().cur_conn);
        for ws in self.sockets_of(conn) {
            self.send_out(&ws, out.clone());
        }
    }

    /// A presence change to `online` with commands still queued for this machine (the outbox)
    /// sends one machine-back job to the NOTIFY queue, at most once per debounce window. The
    /// debounce is kept in the DO storage, so the window holds across evictions. A storage error
    /// sends nothing: a missed "back" push is better than a repeated one.
    async fn announce_machine_back(&self, core: &Core<SqlMachineStore>, from: &str, to: &str) {
        if to != "online" || from == "online" {
            return;
        }
        let (machine_id, queued) = {
            let core = core.borrow();
            let rows = match core.store().outbox() {
                Ok(rows) => rows,
                Err(error) => {
                    worker::console_log!("machine: outbox read failed: {error}");
                    return;
                }
            };
            let mut threads: Vec<String> = Vec::new();
            for row in rows {
                if !threads.contains(&row.thread_id) {
                    threads.push(row.thread_id);
                }
            }
            (core.machine_id().to_owned(), threads)
        };
        if queued.is_empty() {
            return;
        }
        let storage = self.state.storage();
        let mut debounce = match storage.get::<MachineBackDebounce>(BACK_DEBOUNCE_KEY).await {
            Ok(saved) => saved.unwrap_or_default(),
            Err(error) => {
                worker::console_log!("machine {machine_id}: debounce read failed: {error}");
                return;
            }
        };
        let Some(thread) =
            machine_back_thread(from, to, &queued, &mut debounce, now_ms()).map(str::to_owned)
        else {
            return;
        };
        if let Err(error) = storage.put(BACK_DEBOUNCE_KEY, &debounce).await {
            worker::console_log!("machine {machine_id}: debounce write failed: {error}");
            return;
        }
        queues_notify::enqueue(&self.env, &[machine_back_job(&thread, &machine_id)]).await;
    }

    /// A socket closed or failed: presence, if it was the current connection.
    async fn socket_gone(&self, ws: &WebSocket) -> worker::Result<()> {
        let conn = ws
            .deserialize_attachment::<u64>()
            .ok()
            .flatten()
            .unwrap_or(0);
        self.enqueue(Queued::Gone {
            conn,
            heartbeat_timeout: false,
        })
        .await
    }

    /// The runtime's record of the last ping it answered without waking the DO
    /// (`getWebSocketAutoResponseTimestamp`; the `worker` crate has no binding for it, so it is
    /// called through `Reflect`).
    fn auto_response(&self, ws: &WebSocket) -> AutoResponse {
        use worker::wasm_bindgen::JsCast;
        let raw: &JsValue = self.state.as_raw().as_ref();
        let Some(function) = worker::js_sys::Reflect::get(
            raw,
            &JsValue::from_str("getWebSocketAutoResponseTimestamp"),
        )
        .ok()
        .and_then(|method| method.dyn_into::<worker::js_sys::Function>().ok()) else {
            return AutoResponse::Unsupported;
        };
        let socket: &worker::web_sys::WebSocket = ws.as_ref();
        let Ok(value) = function.call1(raw, socket.as_ref()) else {
            return AutoResponse::Unsupported;
        };
        if value.is_null() || value.is_undefined() {
            return AutoResponse::Never;
        }
        match value.dyn_into::<worker::js_sys::Date>() {
            Ok(date) if date.get_time().is_finite() && date.get_time() >= 0.0 => {
                AutoResponse::At(date.get_time() as u64)
            }
            _ => AutoResponse::Unsupported,
        }
    }

    // ---- internal routes ---------------------------------------------------------------------

    fn provision(&self, machine_id: &str, body: &Value) -> worker::Result<Response> {
        let (Some(org_id), Some(hash)) = (
            body.get("org_id").and_then(Value::as_str),
            body.get("credential_hash").and_then(Value::as_str),
        ) else {
            return json_response(400, &error_body("org_id and credential_hash are required"));
        };
        let replace = body
            .get("replace")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let outcome = self
            .core()?
            .borrow_mut()
            .provision(machine_id, org_id, hash, replace)
            .map_err(storage_error)?;
        match outcome {
            Provisioned::Conflict => {
                json_response(409, &error_body("the machine is already provisioned"))
            }
            Provisioned::Created => json_response(200, &json!({ "status": "created" })),
            Provisioned::Unchanged => json_response(200, &json!({ "status": "unchanged" })),
            Provisioned::Replaced => json_response(200, &json!({ "status": "replaced" })),
        }
    }

    async fn revoke(&self) -> worker::Result<Response> {
        let core = self.core()?;
        if !core.borrow().is_provisioned() {
            return json_response(404, &error_body("no such machine"));
        }
        let live = self.state.get_websockets_with_tag(TUNNEL_TAG);
        driver::revoke(core, &self.host(), now_ms())
            .await
            .map_err(storage_error)?;
        for ws in live {
            close_socket(&ws, CloseCode::AuthRevoked, "the machine was revoked");
        }
        json_response(200, &json!({ "status": "revoked" }))
    }

    /// Closes the live tunnel with `device_untrusted` (SPEC-007) but leaves the machine usable: nothing is
    /// persisted as revoked, so its credential works again on the next connect. The socket's
    /// close takes presence offline like any other drop.
    async fn disconnect(&self) -> worker::Result<Response> {
        let core = self.core()?;
        if !core.borrow().is_provisioned() {
            return json_response(404, &error_body("no such machine"));
        }
        for ws in self.state.get_websockets_with_tag(TUNNEL_TAG) {
            close_socket(
                &ws,
                CloseCode::DeviceUntrusted,
                "the machine is not a trusted device",
            );
        }
        json_response(200, &json!({ "status": "disconnected" }))
    }

    async fn push(&self, body: &Value) -> worker::Result<Response> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct PushIn {
            thread_id: String,
            #[serde(default)]
            host: Option<String>,
            #[serde(default)]
            snapshot: bool,
            commands: Vec<CommandIn>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct CommandIn {
            source_id: String,
            idempotency_key: String,
            issued_at: String,
            command: Command,
        }
        let parsed: PushIn = match serde_json::from_value(body.clone()) {
            Ok(parsed) => parsed,
            Err(error) => {
                return json_response(400, &error_body(&format!("invalid push: {error}")));
            }
        };
        let host_kind = match parsed.host.as_deref() {
            None => HostKind::Thread,
            Some(name) => match HostKind::parse(name) {
                Some(kind) => kind,
                None => return json_response(400, &error_body("host must be thread")),
            },
        };
        let queued = parsed.commands.len();
        let push = PushBody {
            thread_id: parsed.thread_id,
            host: host_kind,
            snapshot: parsed.snapshot,
            commands: parsed
                .commands
                .into_iter()
                .map(|c| PushCommand {
                    source_id: c.source_id,
                    idempotency_key: c.idempotency_key,
                    issued_at: c.issued_at,
                    command: c.command,
                })
                .collect(),
        };
        let core = self.core()?;
        let result = driver::push_commands(core, &self.host(), push, now_ms(), &mut |out| {
            self.send_current(out);
        })
        .await
        .map_err(storage_error)?;
        match result {
            Ok(()) => json_response(200, &json!({ "queued": queued })),
            Err(error) => json_response(409, &error_body(&error.0)),
        }
    }

    fn dump(&self) -> worker::Result<Response> {
        let core = self.core()?.borrow();
        let state = core.state();
        let outbox = core.store().outbox().map_err(storage_error)?;
        let auto_response = match self
            .sockets_of(state.cur_conn)
            .first()
            .map(|ws| self.auto_response(ws))
        {
            Some(AutoResponse::At(ms)) => json!(ms),
            Some(AutoResponse::Unsupported) => json!("unsupported"),
            Some(AutoResponse::Never) | None => Value::Null,
        };
        let hosted: Vec<Value> = core
            .hosted_threads()
            .map_err(storage_error)?
            .into_iter()
            .map(|(thread, host)| json!({ "thread_id": thread, "host": host.as_str() }))
            .collect();
        json_response(
            200,
            &json!({
                "machine_id": state.machine_id,
                "revoked": state.revoked,
                "presence": state.presence,
                "presence_since": state.presence_since,
                "connected": state.cur_conn != 0 && state.hello_done,
                "stream_epoch": state.stream_epoch,
                "auto_response_ms": auto_response,
                "up_last": state.up_last,
                "down_base": state.down_base,
                "down_sent": state.down_sent,
                "outbox_len": outbox.len(),
                "outbox": outbox
                    .iter()
                    .map(|row| json!({
                        "thread_id": row.thread_id,
                        "source_id": row.source_id,
                        "idempotency_key": row.idempotency_key,
                    }))
                    .collect::<Vec<_>>(),
                "hosted": hosted,
                "runner_version": state.runner_version,
                "capabilities": state.capabilities,
            }),
        )
    }
}

/// Sends `close{code}` and closes the socket with `4000 + n` (tunnel.md §3.8).
fn close_socket(ws: &WebSocket, code: CloseCode, message: &str) {
    let frame = Frame::Close {
        code,
        message: message.to_owned(),
    };
    if let Ok(text) = serde_json::to_string(&frame) {
        let _ = ws.send_with_str(&text);
    }
    let _ = ws.close(Some(code.ws_code()), Some(short(message)));
}

/// `message` cut to what a WebSocket close reason can carry.
fn short(message: &str) -> &str {
    let mut end = message.len().min(MAX_REASON_BYTES);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    &message[..end]
}

fn storage_error(error: impl std::fmt::Display) -> worker::Error {
    worker::Error::RustError(format!("machine storage: {error}"))
}

fn json_response(status: u16, body: &Value) -> worker::Result<Response> {
    Ok(Response::from_json(body)?.with_status(status))
}

fn error_body(message: &str) -> Value {
    json!({ "error": "machine", "message": message })
}

fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}
