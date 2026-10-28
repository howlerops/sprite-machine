// TRACER STUB: replaced by M2B-002 follow-ups (Worker routes onto the Thread DO; NO AUTH on any tracer route)
//! Workers glue for the tracer hub (wasm32 only). Patterns copied from `spikes/cf`: requests
//! (including WebSocket upgrades) forwarded unchanged to a Durable Object; hibernatable
//! WebSockets with `setWebSocketAutoResponse`.
//!
//! The routes (NO AUTH in the tracer; the real hub authenticates every one of them) are
//! mounted by `crate::worker` and all forward to ONE Durable Object instance (`TracerHub`,
//! name `tracer`) that keeps the tracer's threads in memory ([`HubState`]). An eviction loses
//! them:
//!
//! - `GET /threads/{thread_id}/subscribe?after=<seq>` (WebSocket, turn-event.md §6)
//! - `POST /threads/{thread_id}/prompts` (`Idempotency-Key` header, [`PromptRequest`] body)
//! - `POST /threads/{thread_id}/interrupts/{interrupt_id}` (`Idempotency-Key`,
//!   [`AnswerRequest`] body; 409 when another answer won)
//! - `POST /internal/threads/{thread_id}/events` (only through a stub: the Machine DO appends
//!   the runner's events here, the same route the Thread DO has)
//!
//! The runner's tunnel is not here: `GET /v1/machines/{id}/tunnel` ends at the Machine DO
//! (M2B-002). Commands reach it by `push_to_machine`.

use std::cell::RefCell;

use serde::Serialize;
// `#[durable_object]` expands to wasm-bindgen code that expects `wasm_bindgen` in scope.
use worker::wasm_bindgen;
use worker::{
    DurableObject, Env, Method, WebSocket, WebSocketIncomingMessage, WebSocketPair, durable_object,
};

use super::state::{AnswerRequest, HttpError, HubState, Out, PromptRequest};
use crate::machines::internal::{push_body, push_to_machine};
use crate::machines::store::HostKind;

/// Socket tag: `sub:<thread_id>`.
const SUB_TAG: &str = "sub:";

/// The tracer hub Durable Object: in-memory [`HubState`] plus hibernatable subscriber sockets.
#[durable_object]
pub struct TracerHub {
    state: worker::State,
    env: Env,
    hub: RefCell<HubState>,
}

impl DurableObject for TracerHub {
    fn new(state: worker::State, env: Env) -> Self {
        Self {
            state,
            env,
            hub: RefCell::new(HubState::new()),
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
                self.answer(result).await
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
                self.answer(result).await
            }
            (Method::Post, ["internal", "threads", thread_id, "events"]) => {
                let body: serde_json::Value = match req.json().await {
                    Ok(body) => body,
                    Err(error) => return json_response(400, &error_body(&error.to_string())),
                };
                let result = self
                    .hub
                    .borrow_mut()
                    .append_event(thread_id, body, now_ms());
                self.answer(result).await
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
            // The subscription protocol is text-only.
            WebSocketIncomingMessage::Binary(_) => {
                return ws.close(Some(1003), Some("binary not supported"));
            }
        };
        if tag_value(&self.state.get_tags(&ws), SUB_TAG).is_some() {
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
        // Complete the close handshake. 1005/1006 cannot be sent, so always answer 1000.
        let _ = ws.close(Some(1000), Some("bye"));
        Ok(())
    }

    async fn websocket_error(&self, _ws: WebSocket, _error: worker::Error) -> worker::Result<()> {
        Ok(())
    }
}

impl TracerHub {
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

    async fn answer(
        &self,
        result: Result<(u16, serde_json::Value, Vec<Out>), HttpError>,
    ) -> worker::Result<worker::Response> {
        match result {
            Ok((status, body, out)) => {
                self.deliver(out).await;
                json_response(status, &body)
            }
            Err(error) => json_response(error.status, &error.body),
        }
    }

    /// Sends every [`Out`]: subscription messages on their sockets, commands to the Machine DO.
    /// A socket that fails to send is closed by the runtime; a command that cannot be pushed is
    /// logged and lost (a stub limit, see `state.rs`).
    async fn deliver(&self, out: Vec<Out>) {
        for item in out {
            match item {
                Out::Subscribers { thread_id, message } => {
                    let text = message.to_string();
                    for ws in self
                        .state
                        .get_websockets_with_tag(&format!("{SUB_TAG}{thread_id}"))
                    {
                        let _ = ws.send_with_str(&text);
                    }
                }
                Out::Command {
                    machine_id,
                    thread_id,
                    queued,
                } => {
                    // The idempotency key is the command's id here: it is unique per command.
                    let command = serde_json::json!({
                        "source_id": queued.idempotency_key,
                        "idempotency_key": queued.idempotency_key,
                        "issued_at": queued.issued_at,
                        "command": queued.command,
                    });
                    let body = push_body(&thread_id, HostKind::Tracer, false, vec![command]);
                    if let Err(message) = push_to_machine(&self.env, &machine_id, &body).await {
                        worker::console_log!("tracer: {message}");
                    }
                }
            }
        }
    }
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
