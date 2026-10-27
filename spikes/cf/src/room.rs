//! `Room`: a SQLite-backed Durable Object with a hibernatable WebSocket.
//!
//! - `POST /counter/{name}` increments a counter row in the DO's own SQLite database.
//! - `GET /counter/{name}` reads it.
//! - `GET /ws/{name}` (Upgrade: websocket) accepts the socket with the hibernation API
//!   (`state.accept_web_socket`), so the DO can be evicted from memory while the socket
//!   stays open. Text frames are echoed as `echo:<text>` by `websocket_message`.
//!   The literal frame `ping` is answered with `pong` by the runtime itself
//!   (`setWebSocketAutoResponse`), without waking the DO (tunnel spec §2/§3.7).

use serde::Deserialize;
// `#[durable_object]` expands to wasm-bindgen code that expects `wasm_bindgen` in scope.
use worker::wasm_bindgen;
use worker::{
    durable_object, DurableObject, Env, Method, Request, Response, Result, SqlStorage, State,
    WebSocket, WebSocketIncomingMessage, WebSocketPair, WebSocketRequestResponsePair,
};

#[durable_object]
pub struct Room {
    state: State,
    sql: SqlStorage,
}

#[derive(Deserialize)]
struct Counter {
    value: i64,
}

impl DurableObject for Room {
    fn new(state: State, _env: Env) -> Self {
        let sql = state.storage().sql();
        // `exec` is synchronous; schema setup in the constructor is the documented pattern.
        sql.exec(
            "CREATE TABLE IF NOT EXISTS counter (
               id    INTEGER PRIMARY KEY CHECK (id = 1),
               value INTEGER NOT NULL
             )",
            None,
        )
        .expect("create counter table");
        Self { state, sql }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        let path = req.path();
        if path.starts_with("/ws/") {
            return self.accept_ws(&req);
        }
        if path.starts_with("/counter/") {
            return match req.method() {
                Method::Post => self.counter(
                    "INSERT INTO counter (id, value) VALUES (1, 1)
                     ON CONFLICT (id) DO UPDATE SET value = value + 1
                     RETURNING value",
                ),
                Method::Get => self.counter("SELECT value FROM counter WHERE id = 1"),
                _ => Response::error("method not allowed", 405),
            };
        }
        Response::error("not found", 404)
    }

    async fn websocket_message(&self, ws: WebSocket, msg: WebSocketIncomingMessage) -> Result<()> {
        match msg {
            WebSocketIncomingMessage::String(text) => ws.send_with_str(format!("echo:{text}")),
            // The tunnel protocol is text-only (tunnel spec §2): binary is a protocol error.
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
    ) -> Result<()> {
        // Complete the close handshake. 1005/1006 cannot be sent, so always answer 1000.
        let _ = ws.close(Some(1000), Some("bye"));
        Ok(())
    }

    async fn websocket_error(&self, _ws: WebSocket, _error: worker::Error) -> Result<()> {
        Ok(())
    }
}

impl Room {
    fn counter(&self, query: &str) -> Result<Response> {
        let rows: Vec<Counter> = self.sql.exec(query, None)?.to_array()?;
        let value = rows.first().map_or(0, |c| c.value);
        Response::ok(value.to_string())
    }

    fn accept_ws(&self, req: &Request) -> Result<Response> {
        let upgrade = req.headers().get("Upgrade")?.unwrap_or_default();
        if !upgrade.eq_ignore_ascii_case("websocket") {
            return Response::error("expected Upgrade: websocket", 426);
        }
        let pair = WebSocketPair::new()?;
        // Hibernation API: the runtime owns the socket; events arrive via websocket_* methods.
        self.state.accept_web_socket(&pair.server);
        // Answered by the runtime without waking the DO. Setting it per accept is idempotent.
        self.state
            .set_websocket_auto_response(&WebSocketRequestResponsePair::new("ping", "pong")?);
        Response::from_websocket(pair.client)
    }
}
