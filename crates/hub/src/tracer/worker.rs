// TRACER STUB: replaced by M2-008 + M2B-002 (Worker routes, Thread DO and Machine DO with SQLite storage; NO AUTH on any tracer route)
//! Workers glue for the tracer hub (wasm32 only). Patterns copied from `spikes/cf`: an axum
//! router with `SendWrapper<Env>` state and `#[worker::send]` handlers; requests (including
//! WebSocket upgrades) forwarded unchanged to a Durable Object; hibernatable WebSockets with
//! `setWebSocketAutoResponse` for the literal tunnel heartbeat.
//!
//! Routes (NO AUTH in the tracer; the real hub authenticates every one of them):
//!
//! - `GET /health`
//! - `GET /v1/machines/{machine_id}/tunnel` (WebSocket, subprotocol `sm.tunnel.v1`)
//! - `GET /threads/{thread_id}/subscribe?after=<seq>` (WebSocket, turn-event.md §6)
//! - `POST /threads/{thread_id}/prompts` (`Idempotency-Key` header, [`PromptRequest`] body)
//! - `POST /threads/{thread_id}/interrupts/{interrupt_id}` (`Idempotency-Key`,
//!   [`AnswerRequest`] body; 409 when another answer won)
//!
//! Every route goes to ONE Durable Object instance (`TracerHub`, name `tracer`) that keeps
//! all machine and thread state in memory ([`HubState`]). An eviction loses it.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use protocol::tunnel::{CloseCode, Frame, SUBPROTOCOL};
use serde::Serialize;
use tower_service::Service;
// `#[durable_object]` expands to wasm-bindgen code that expects `wasm_bindgen` in scope.
use worker::wasm_bindgen;
use worker::{
    Context, DurableObject, Env, HttpRequest, Method, WebSocket, WebSocketIncomingMessage,
    WebSocketPair, WebSocketRequestResponsePair, durable_object, event, send::SendWrapper,
};

use super::state::{AnswerRequest, HttpError, HubState, Out, PromptRequest};

type AppState = SendWrapper<Env>;

/// Durable Object binding and the single instance name used by the tracer.
const HUB_BINDING: &str = "HUB";
const HUB_NAME: &str = "tracer";

/// Socket tags: `tunnel:<machine_id>` and `sub:<thread_id>`.
const TUNNEL_TAG: &str = "tunnel:";
const SUB_TAG: &str = "sub:";

fn router(env: Env) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/machines/{machine_id}/tunnel", get(to_hub))
        .route("/threads/{thread_id}/subscribe", get(to_hub))
        .route("/threads/{thread_id}/prompts", post(to_hub))
        .route(
            "/threads/{thread_id}/interrupts/{interrupt_id}",
            post(to_hub),
        )
        .with_state(SendWrapper::new(env))
}

#[event(fetch)]
async fn fetch(req: HttpRequest, env: Env, _ctx: Context) -> worker::Result<Response<Body>> {
    Ok(router(env).call(req).await?)
}

/// A worker error becomes a 500 with the message as body.
struct AppError(worker::Error);

impl From<worker::Error> for AppError {
    fn from(e: worker::Error) -> Self {
        Self(e)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
    }
}

/// Forwards the request unchanged (WebSocket upgrades included) to the tracer DO.
#[worker::send]
async fn to_hub(State(env): State<AppState>, req: Request) -> Result<Response, AppError> {
    let stub = env
        .durable_object(HUB_BINDING)?
        .id_from_name(HUB_NAME)?
        .get_stub()?;
    let req = worker::Request::try_from(req)?;
    let res = stub.fetch_with_request(req).await?;
    Ok(res.into())
}

/// The tracer hub Durable Object: in-memory [`HubState`] plus hibernatable sockets.
#[durable_object]
pub struct TracerHub {
    state: worker::State,
    hub: RefCell<HubState>,
    /// The current tunnel connection number per machine (also the socket's attachment).
    tunnel_conns: RefCell<HashMap<String, u64>>,
    next_conn: Cell<u64>,
}

impl DurableObject for TracerHub {
    fn new(state: worker::State, _env: Env) -> Self {
        Self {
            state,
            hub: RefCell::new(HubState::new()),
            tunnel_conns: RefCell::new(HashMap::new()),
            next_conn: Cell::new(1),
        }
    }

    async fn fetch(&self, mut req: worker::Request) -> worker::Result<worker::Response> {
        let path = req.path();
        let segments: Vec<String> = path
            .trim_matches('/')
            .split('/')
            .map(str::to_owned)
            .collect();
        let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
        match (req.method(), segments.as_slice()) {
            (Method::Get, ["v1", "machines", machine_id, "tunnel"]) => {
                self.accept_tunnel(&req, machine_id)
            }
            (Method::Get, ["threads", thread_id, "subscribe"]) => {
                let after = req
                    .url()?
                    .query_pairs()
                    .find(|(key, _)| key == "after")
                    .and_then(|(_, value)| value.parse::<u64>().ok())
                    .unwrap_or(0);
                self.accept_subscriber(&req, thread_id, after)
            }
            (Method::Post, ["threads", thread_id, "prompts"]) => {
                let Some(key) = idempotency_key(&req)? else {
                    return json_response(400, &error_body("Idempotency-Key header required"));
                };
                let body: PromptRequest = match req.json().await {
                    Ok(body) => body,
                    Err(error) => return json_response(400, &error_body(&error.to_string())),
                };
                let result = self
                    .hub
                    .borrow_mut()
                    .submit_prompt(thread_id, &key, body, now_ms());
                self.answer(result)
            }
            (Method::Post, ["threads", thread_id, "interrupts", interrupt_id]) => {
                let Some(key) = idempotency_key(&req)? else {
                    return json_response(400, &error_body("Idempotency-Key header required"));
                };
                let body: AnswerRequest = match req.json().await {
                    Ok(body) => body,
                    Err(error) => return json_response(400, &error_body(&error.to_string())),
                };
                let result = self.hub.borrow_mut().answer_interrupt(
                    thread_id,
                    interrupt_id,
                    &key,
                    body,
                    now_ms(),
                );
                self.answer(result)
            }
            _ => worker::Response::error("not found", 404),
        }
    }

    async fn websocket_message(
        &self,
        ws: WebSocket,
        msg: WebSocketIncomingMessage,
    ) -> worker::Result<()> {
        let text = match msg {
            WebSocketIncomingMessage::String(text) => text,
            // Both protocols are text-only (tunnel.md §2): binary is a protocol error.
            WebSocketIncomingMessage::Binary(_) => {
                return ws.close(Some(1003), Some("binary not supported"));
            }
        };
        let tags = self.state.get_tags(&ws);
        if let Some(machine_id) = tag_value(&tags, TUNNEL_TAG) {
            let out = self
                .hub
                .borrow_mut()
                .tunnel_message(&machine_id, &text, now_ms());
            self.deliver(out);
        } else if tag_value(&tags, SUB_TAG).is_some() {
            // Clients send nothing but `{"type":"ping"}` (turn-event.md §6).
            let is_ping = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .is_some_and(|v| v.get("type").and_then(|t| t.as_str()) == Some("ping"));
            if is_ping {
                ws.send_with_str(r#"{"type":"pong"}"#)?;
            }
        }
        Ok(())
    }

    async fn websocket_close(
        &self,
        ws: WebSocket,
        _code: usize,
        _reason: String,
        _clean: bool,
    ) -> worker::Result<()> {
        if let Some(machine_id) = tag_value(&self.state.get_tags(&ws), TUNNEL_TAG) {
            // Only the machine's current socket marks it offline; a superseded one does not.
            let conn: Option<u64> = ws.deserialize_attachment().ok().flatten();
            let current = self.tunnel_conns.borrow().get(&machine_id).copied();
            if conn.is_some() && conn == current {
                self.hub.borrow_mut().tunnel_closed(&machine_id);
            }
        }
        // Complete the close handshake. 1005/1006 cannot be sent, so always answer 1000.
        let _ = ws.close(Some(1000), Some("bye"));
        Ok(())
    }

    async fn websocket_error(&self, _ws: WebSocket, _error: worker::Error) -> worker::Result<()> {
        Ok(())
    }
}

impl TracerHub {
    fn accept_tunnel(
        &self,
        req: &worker::Request,
        machine_id: &str,
    ) -> worker::Result<worker::Response> {
        if !is_upgrade(req)? {
            return worker::Response::error("expected Upgrade: websocket", 426);
        }
        let protocols = req
            .headers()
            .get("Sec-WebSocket-Protocol")?
            .unwrap_or_default();
        if !protocols.split(',').any(|p| p.trim() == SUBPROTOCOL) {
            return worker::Response::error(format!("subprotocol {SUBPROTOCOL} required"), 426);
        }
        // One tunnel per machine: the new connection supersedes the old one (tunnel.md §2).
        let tag = format!("{TUNNEL_TAG}{machine_id}");
        for old in self.state.get_websockets_with_tag(&tag) {
            close_with(
                &old,
                CloseCode::Superseded,
                "a newer connection for this machine was accepted",
            );
        }
        self.hub.borrow_mut().tunnel_closed(machine_id);
        let pair = WebSocketPair::new()?;
        self.state.accept_websocket_with_tags(&pair.server, &[&tag]);
        let conn = self.next_conn.get();
        self.next_conn.set(conn + 1);
        pair.server.serialize_attachment(conn)?;
        self.tunnel_conns
            .borrow_mut()
            .insert(machine_id.to_owned(), conn);
        // Answered by the runtime without waking the DO (tunnel.md §3.7).
        self.state
            .set_websocket_auto_response(&WebSocketRequestResponsePair::new(
                r#"{"t":"ping"}"#,
                r#"{"t":"pong"}"#,
            )?);
        let mut response = worker::Response::from_websocket(pair.client)?;
        response
            .headers_mut()
            .set("Sec-WebSocket-Protocol", SUBPROTOCOL)?;
        Ok(response)
    }

    fn accept_subscriber(
        &self,
        req: &worker::Request,
        thread_id: &str,
        after: u64,
    ) -> worker::Result<worker::Response> {
        if !is_upgrade(req)? {
            return worker::Response::error("expected Upgrade: websocket", 426);
        }
        let pair = WebSocketPair::new()?;
        self.state
            .accept_websocket_with_tags(&pair.server, &[&format!("{SUB_TAG}{thread_id}")]);
        for message in self.hub.borrow().subscribe(thread_id, after) {
            pair.server.send_with_str(message.to_string())?;
        }
        worker::Response::from_websocket(pair.client)
    }

    fn answer(
        &self,
        result: Result<(u16, serde_json::Value, Vec<Out>), HttpError>,
    ) -> worker::Result<worker::Response> {
        match result {
            Ok((status, body, out)) => {
                self.deliver(out);
                json_response(status, &body)
            }
            Err(error) => json_response(error.status, &error.body),
        }
    }

    /// Sends every [`Out`] on the sockets it names. A socket that fails to send is closed by the
    /// runtime; the tracer does not retry (the tunnel outbox resends after a reconnect).
    fn deliver(&self, out: Vec<Out>) {
        for item in out {
            match item {
                Out::Tunnel { machine_id, frame } => {
                    let Ok(text) = serde_json::to_string(&frame) else {
                        continue;
                    };
                    for ws in self
                        .state
                        .get_websockets_with_tag(&format!("{TUNNEL_TAG}{machine_id}"))
                    {
                        let _ = ws.send_with_str(&text);
                    }
                }
                Out::CloseTunnel {
                    machine_id,
                    code,
                    message,
                } => {
                    for ws in self
                        .state
                        .get_websockets_with_tag(&format!("{TUNNEL_TAG}{machine_id}"))
                    {
                        close_with(&ws, code, &message);
                    }
                    self.hub.borrow_mut().tunnel_closed(&machine_id);
                }
                Out::Subscribers { thread_id, message } => {
                    let text = message.to_string();
                    for ws in self
                        .state
                        .get_websockets_with_tag(&format!("{SUB_TAG}{thread_id}"))
                    {
                        let _ = ws.send_with_str(&text);
                    }
                }
            }
        }
    }
}

/// Sends `close{code}` then closes the socket with `4000 + n` (tunnel.md §3.8).
fn close_with(ws: &WebSocket, code: CloseCode, message: &str) {
    let frame = Frame::Close {
        code,
        message: message.to_owned(),
    };
    if let Ok(text) = serde_json::to_string(&frame) {
        let _ = ws.send_with_str(&text);
    }
    let _ = ws.close(Some(code.ws_code()), Some(message));
}

fn tag_value(tags: &[String], prefix: &str) -> Option<String> {
    tags.iter()
        .find_map(|tag| tag.strip_prefix(prefix).map(str::to_owned))
}

fn is_upgrade(req: &worker::Request) -> worker::Result<bool> {
    Ok(req
        .headers()
        .get("Upgrade")?
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket")))
}

fn idempotency_key(req: &worker::Request) -> worker::Result<Option<String>> {
    Ok(req
        .headers()
        .get("Idempotency-Key")?
        .filter(|key| !key.is_empty()))
}

fn json_response<T: Serialize>(status: u16, body: &T) -> worker::Result<worker::Response> {
    Ok(worker::Response::from_json(body)?.with_status(status))
}

fn error_body(message: &str) -> serde_json::Value {
    serde_json::json!({ "error": message })
}

fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}
