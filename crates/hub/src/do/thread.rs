//! The Thread Durable Object (M2-008, wasm32 only): one per thread, SQLite-backed.
//!
//! - The event log, per-scope state, tasks, interrupts, the commands outbox and the idempotency
//!   records live in the DO's SQLite database (`threads::sqlite::SqlStore`); all rules are in
//!   the runtime-agnostic [`ThreadCore`], which this class only wires to HTTP and WebSockets.
//! - The subscription (`GET /v1/threads/{id}/subscribe?after=<seq>`, turn-event.md §6) uses the
//!   hibernation API: `accept_web_socket`, `websocket_message`, and
//!   `setWebSocketAutoResponse` for `{"type":"ping"}` -> `{"type":"pong"}` (answered by the
//!   runtime without waking the DO). There are no per-connection timers. After an eviction the
//!   next request rebuilds the core (and its §5 validator) from the stored log.
//! - Every request carries the thread id in its path (a DO cannot recover the name it was
//!   addressed by). The Worker has authenticated the caller and passes the derived actor in the
//!   `x-sm-actor` header, replacing anything the client sent. Internal routes
//!   (`/internal/threads/{id}/...`) are only reachable through a DO stub; see `threads::api`.
//!
//! Handlers never `.await` between storage calls (the body is read first), so one request's
//! writes are applied atomically and a subscriber's catch-up cannot interleave with an append.

use std::cell::RefCell;

use protocol::tunnel::Actor;
use serde_json::Value;
// `#[durable_object]` expands to wasm-bindgen code that expects `wasm_bindgen` in scope.
use worker::wasm_bindgen;
use worker::{
    DurableObject, Env, Method, Request, Response, State, WebSocket, WebSocketIncomingMessage,
    WebSocketPair, WebSocketRequestResponsePair, durable_object,
};

use crate::threads::api::{self, ApiRequest, ApiResponse, thread_id_of};
use crate::threads::auth::dev_fixtures_enabled;
use crate::threads::sqlite::SqlStore;
use crate::threads::{ApiError, ThreadCore, event_message};

/// Header the Worker uses to hand the authenticated actor (JSON) to the DO.
pub const ACTOR_HEADER: &str = "x-sm-actor";

/// The Thread DO (binding `THREAD`).
#[durable_object]
pub struct Thread {
    state: State,
    env: Env,
    /// Built on the first request after construction, from the stored log.
    core: RefCell<Option<ThreadCore<SqlStore>>>,
}

impl DurableObject for Thread {
    fn new(state: State, env: Env) -> Self {
        Self {
            state,
            env,
            core: RefCell::new(None),
        }
    }

    async fn fetch(&self, mut req: Request) -> worker::Result<Response> {
        let url = req.url()?;
        let path = url.path().to_owned();
        let Some(thread_id) = thread_id_of(&path) else {
            return json_error(&ApiError::not_found(format!("no route for {path}")));
        };
        let method = req.method();
        if method == Method::Get && path.ends_with("/subscribe") && path.starts_with("/v1/") {
            return self.subscribe(&req, &url, &thread_id);
        }

        let body = if matches!(method, Method::Get | Method::Head) {
            Value::Null
        } else {
            let text = req.text().await?;
            if text.trim().is_empty() {
                Value::Null
            } else {
                match serde_json::from_str(&text) {
                    Ok(value) => value,
                    Err(error) => {
                        return json_error(&ApiError::invalid_request(format!(
                            "body is not JSON: {error}"
                        )));
                    }
                }
            }
        };
        let actor = req
            .headers()
            .get(ACTOR_HEADER)?
            .and_then(|value| serde_json::from_str::<Actor>(&value).ok());
        let request = ApiRequest {
            method: method.to_string(),
            path,
            query: url
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect(),
            actor,
            idempotency_key: req.headers().get("Idempotency-Key")?,
            body,
            dev_fixtures: self.dev_fixtures(),
        };

        // No await from here to the response: the writes of this request commit together.
        let (response, appended) = match self.with_core(&thread_id, |core| {
            let response = api::handle(core, &request, now_ms());
            (response, core.take_appended())
        }) {
            Ok(done) => done,
            Err(error) => return json_error(&error),
        };
        for envelope in &appended {
            self.broadcast(&event_message(envelope).to_string());
        }
        respond(&response)
    }

    async fn websocket_message(
        &self,
        ws: WebSocket,
        msg: WebSocketIncomingMessage,
    ) -> worker::Result<()> {
        match msg {
            // Clients send nothing but `{"type":"ping"}` (turn-event.md §6). The exact string is
            // answered by the runtime's auto-response; this covers other spellings of it.
            WebSocketIncomingMessage::String(text) => {
                let is_ping = serde_json::from_str::<Value>(&text)
                    .ok()
                    .is_some_and(|value| value.get("type").and_then(Value::as_str) == Some("ping"));
                if is_ping {
                    ws.send_with_str(PONG)?;
                }
                Ok(())
            }
            WebSocketIncomingMessage::Binary(_) => {
                ws.close(Some(1003), Some("binary not supported"))
            }
        }
    }

    async fn websocket_close(
        &self,
        ws: WebSocket,
        _code: usize,
        _reason: String,
        _clean: bool,
    ) -> worker::Result<()> {
        // Complete the close handshake. 1005/1006 cannot be sent, so always answer 1000.
        let _ = ws.close(Some(1000), Some("bye"));
        Ok(())
    }

    async fn websocket_error(&self, _ws: WebSocket, _error: worker::Error) -> worker::Result<()> {
        Ok(())
    }
}

const PING: &str = r#"{"type":"ping"}"#;
const PONG: &str = r#"{"type":"pong"}"#;

impl Thread {
    fn dev_fixtures(&self) -> bool {
        let var = self
            .env
            .var("DEV_FIXTURES")
            .ok()
            .map(|value| value.to_string());
        dev_fixtures_enabled(var.as_deref())
    }

    /// Runs `f` on the core for `thread_id`, opening it from the stored log first if needed.
    fn with_core<T>(
        &self,
        thread_id: &str,
        f: impl FnOnce(&mut ThreadCore<SqlStore>) -> T,
    ) -> Result<T, ApiError> {
        let mut slot = self.core.borrow_mut();
        if slot.is_none() {
            let store = SqlStore::open(self.state.storage().sql())?;
            *slot = Some(ThreadCore::open(thread_id, store)?);
        }
        match slot.as_mut() {
            Some(core) if core.thread_id() == thread_id => Ok(f(core)),
            Some(core) => Err(ApiError::invalid_request(format!(
                "this object is thread {}, not {thread_id}",
                core.thread_id()
            ))),
            None => Err(ApiError::internal("thread core is not open")),
        }
    }

    /// `GET /v1/threads/{id}/subscribe?after=<seq>`: hello, snapshot, replay, live (§6).
    fn subscribe(
        &self,
        req: &Request,
        url: &worker::Url,
        thread_id: &str,
    ) -> worker::Result<Response> {
        let upgrade = req.headers().get("Upgrade")?.unwrap_or_default();
        if !upgrade.eq_ignore_ascii_case("websocket") {
            return Response::error("expected Upgrade: websocket", 426);
        }
        let after = match url.query_pairs().find(|(key, _)| key == "after") {
            None => 0,
            Some((_, value)) => match value.parse::<u64>() {
                Ok(after) => after,
                Err(_) => {
                    return json_error(&ApiError::invalid_request(
                        "after must be a non-negative integer",
                    ));
                }
            },
        };
        let messages = match self.with_core(thread_id, |core| core.catch_up(after)) {
            Ok(Ok(messages)) => messages,
            Ok(Err(error)) | Err(error) => return json_error(&error),
        };
        let pair = WebSocketPair::new()?;
        // Hibernation API: the runtime owns the socket; events arrive via websocket_* methods.
        self.state.accept_web_socket(&pair.server);
        // Answered by the runtime without waking the DO. Setting it per accept is idempotent.
        self.state
            .set_websocket_auto_response(&WebSocketRequestResponsePair::new(PING, PONG)?);
        // Synchronous from the log read above: no append can slip in before `live`.
        for message in messages {
            pair.server.send_with_str(message.to_string())?;
        }
        Response::from_websocket(pair.client)
    }

    /// Sends one text message to every subscriber. A socket that fails is closed by the
    /// runtime; the client resubscribes with `?after=` (turn-event.md §3).
    fn broadcast(&self, text: &str) {
        for ws in self.state.get_websockets() {
            let _ = ws.send_with_str(text);
        }
    }
}

fn respond(response: &ApiResponse) -> worker::Result<Response> {
    if response.status == 204 || response.body.is_null() {
        return Ok(Response::empty()?.with_status(response.status));
    }
    Ok(Response::from_json(&response.body)?.with_status(response.status))
}

fn json_error(error: &ApiError) -> worker::Result<Response> {
    Ok(Response::from_json(&error.body())?.with_status(error.status))
}

fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}
