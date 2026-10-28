//! Calls between Durable Objects (wasm32 only): stub fetches to the Thread DO, the Machine DO
//! and the tracer hub, and the D1 presence mirror. These implement [`ThreadHost`] and
//! [`MachineControl`] for the Worker; the logic that uses them is native-tested with fakes.
//!
//! The internal routes are not registered in the Worker router, so only code holding a stub
//! (another Durable Object or Worker code) can reach them.

use protocol::tunnel::NackCode;
use serde_json::{Value, json};
use worker::wasm_bindgen::JsValue;
use worker::{Env, Headers, Method, Request, RequestInit};

use super::admin::{self, MachineControl};
use super::core::{PresenceChange, PushCommand};
use super::driver::{HostError, ThreadHost, push_command_from_json};
use super::store::HostKind;
use crate::db::d1::D1;
use crate::threads::ApiError;

/// Durable Object bindings (wrangler.toml).
pub const THREAD_BINDING: &str = "THREAD";
pub const MACHINE_BINDING: &str = "MACHINE";
/// TRACER STUB: the tracer hub object and its single instance.
pub const TRACER_BINDING: &str = "HUB";
pub const TRACER_NAME: &str = "tracer";

/// The DO routes on the path only, so any origin works.
const ORIGIN: &str = "https://do.internal";

/// Most commands one `GET /internal/threads/{id}/commands` returns.
pub const COMMAND_LIST_LIMIT: usize = 500;

/// One request to the object `name` of namespace `binding`: `(status, body text)`.
pub async fn call(
    env: &Env,
    binding: &str,
    name: &str,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> worker::Result<(u16, String)> {
    let stub = env
        .durable_object(binding)?
        .id_from_name(name)?
        .get_stub()?;
    let mut init = RequestInit::new();
    init.with_method(method);
    if let Some(body) = body {
        let headers = Headers::new();
        headers.set("content-type", "application/json")?;
        init.with_headers(headers)
            .with_body(Some(JsValue::from_str(&body.to_string())));
    }
    let request = Request::new_with_init(&format!("{ORIGIN}{path}"), &init)?;
    let mut response = stub.fetch_with_request(request).await?;
    let status = response.status_code();
    let text = response.text().await?;
    Ok((status, text))
}

/// Pushes a batch of commands to a machine's Machine DO (Thread DO after it queued one, or the
/// tracer hub). `Err` carries a message for the log; the commands stay with the pusher.
pub async fn push_to_machine(env: &Env, machine_id: &str, body: &Value) -> Result<(), String> {
    let path = format!("/internal/machines/{machine_id}/commands");
    match call(
        env,
        MACHINE_BINDING,
        machine_id,
        Method::Post,
        &path,
        Some(body),
    )
    .await
    {
        Ok((200, _)) => Ok(()),
        Ok((status, text)) => Err(format!(
            "machine {machine_id} refused the push: {status} {text}"
        )),
        Err(error) => Err(format!("machine {machine_id} unreachable: {error}")),
    }
}

/// The push body for one thread: its unacked commands.
pub fn push_body(thread_id: &str, host: HostKind, snapshot: bool, commands: Vec<Value>) -> Value {
    json!({
        "thread_id": thread_id,
        "host": host.as_str(),
        "snapshot": snapshot,
        "commands": commands,
    })
}

/// Maps a thread host's HTTP refusal to a [`HostError`]. 4xx on the merits is a `nack`; other
/// statuses are an outage.
fn host_error(status: u16, body: &str) -> HostError {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| format!("HTTP {status}"));
    let code = match status {
        422 => NackCode::InvalidSequence,
        413 => NackCode::TooLarge,
        400 => NackCode::InvalidEnvelope,
        403 => NackCode::Forbidden,
        _ => return HostError::Unavailable(format!("HTTP {status}: {message}")),
    };
    HostError::Rejected { code, message }
}

fn unavailable(error: worker::Error) -> HostError {
    HostError::Unavailable(error.to_string())
}

/// The Machine DO's view of the other objects, over one `Env`.
pub struct WorkerHost<'a> {
    pub env: &'a Env,
}

impl WorkerHost<'_> {
    fn target(host: HostKind, thread_id: &str) -> (&'static str, String) {
        match host {
            HostKind::Thread => (THREAD_BINDING, thread_id.to_owned()),
            HostKind::Tracer => (TRACER_BINDING, TRACER_NAME.to_owned()),
        }
    }
}

impl ThreadHost for WorkerHost<'_> {
    async fn append(
        &self,
        host: HostKind,
        thread_id: &str,
        envelope: &Value,
    ) -> Result<(), HostError> {
        let (binding, name) = Self::target(host, thread_id);
        let path = format!("/internal/threads/{thread_id}/events");
        let (status, body) = call(
            self.env,
            binding,
            &name,
            Method::Post,
            &path,
            Some(envelope),
        )
        .await
        .map_err(unavailable)?;
        if status == 200 {
            Ok(())
        } else {
            Err(host_error(status, &body))
        }
    }

    async fn thread_machine(&self, thread_id: &str) -> Result<Option<String>, HostError> {
        let path = format!("/v1/threads/{thread_id}");
        let (status, body) = call(
            self.env,
            THREAD_BINDING,
            thread_id,
            Method::Get,
            &path,
            None,
        )
        .await
        .map_err(unavailable)?;
        match status {
            200 => Ok(serde_json::from_str::<Value>(&body).ok().and_then(|value| {
                value
                    .get("machine_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })),
            404 => Ok(None),
            other => Err(host_error(other, &body)),
        }
    }

    async fn init_thread(&self, thread_id: &str, info: &Value) -> Result<(), HostError> {
        let path = format!("/internal/threads/{thread_id}/init");
        let (status, body) = call(
            self.env,
            THREAD_BINDING,
            thread_id,
            Method::Post,
            &path,
            Some(info),
        )
        .await
        .map_err(unavailable)?;
        if status == 200 {
            Ok(())
        } else {
            Err(host_error(status, &body))
        }
    }

    async fn list_commands(
        &self,
        host: HostKind,
        thread_id: &str,
    ) -> Result<Vec<PushCommand>, HostError> {
        if host != HostKind::Thread {
            return Ok(Vec::new());
        }
        let path = format!("/internal/threads/{thread_id}/commands?limit={COMMAND_LIST_LIMIT}");
        let (status, body) = call(
            self.env,
            THREAD_BINDING,
            thread_id,
            Method::Get,
            &path,
            None,
        )
        .await
        .map_err(unavailable)?;
        if status != 200 {
            return Err(host_error(status, &body));
        }
        let value: Value = serde_json::from_str(&body)
            .map_err(|error| HostError::Unavailable(format!("commands list: {error}")))?;
        Ok(value
            .get("commands")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(push_command_from_json).collect())
            .unwrap_or_default())
    }

    async fn ack_commands(
        &self,
        host: HostKind,
        thread_id: &str,
        source_ids: &[String],
    ) -> Result<(), HostError> {
        if host != HostKind::Thread {
            return Ok(());
        }
        let ids: Vec<u64> = source_ids.iter().filter_map(|id| id.parse().ok()).collect();
        if ids.is_empty() {
            return Ok(());
        }
        let path = format!("/internal/threads/{thread_id}/commands/ack");
        let (status, body) = call(
            self.env,
            THREAD_BINDING,
            thread_id,
            Method::Post,
            &path,
            Some(&json!({ "ids": ids })),
        )
        .await
        .map_err(unavailable)?;
        if status == 200 {
            Ok(())
        } else {
            Err(host_error(status, &body))
        }
    }

    async fn record_presence(&self, machine_id: &str, change: &PresenceChange, _now_ms: u64) {
        // D1 is the org-wide read model; the DO is the source of truth. A missing binding or
        // table (the tracer runs without migrations) only loses the mirror.
        let Ok(database) = self.env.d1("DB") else {
            return;
        };
        let state = serde_json::to_value(change.state)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        if let Err(error) =
            admin::record_presence(&D1::new(&database), machine_id, &state, &change.since).await
        {
            worker::console_log!("machine {machine_id}: presence mirror failed: {error}");
        }
    }
}

/// The admin API's handle on the Machine DO.
pub struct DoControl<'a> {
    pub env: &'a Env,
}

impl DoControl<'_> {
    async fn post(&self, machine_id: &str, action: &str, body: &Value) -> Result<(), ApiError> {
        let path = format!("/internal/machines/{machine_id}/{action}");
        match call(
            self.env,
            MACHINE_BINDING,
            machine_id,
            Method::Post,
            &path,
            Some(body),
        )
        .await
        {
            Ok((200, _)) => Ok(()),
            Ok((409, text)) => Err(ApiError::new(409, "conflict", text)),
            Ok((status, text)) => Err(ApiError::internal(format!(
                "machine DO {action}: HTTP {status} {text}"
            ))),
            Err(error) => Err(ApiError::internal(format!("machine DO {action}: {error}"))),
        }
    }

    /// Dev route helper: provisions (or re-provisions) without D1.
    pub async fn provision_replacing(
        &self,
        machine_id: &str,
        org_id: &str,
        credential_hash: &str,
    ) -> Result<(), ApiError> {
        self.post(
            machine_id,
            "provision",
            &json!({ "org_id": org_id, "credential_hash": credential_hash, "replace": true }),
        )
        .await
    }
}

impl MachineControl for DoControl<'_> {
    async fn provision(
        &self,
        machine_id: &str,
        org_id: &str,
        credential_hash: &str,
    ) -> Result<(), ApiError> {
        self.post(
            machine_id,
            "provision",
            &json!({ "org_id": org_id, "credential_hash": credential_hash }),
        )
        .await
    }

    async fn revoke(&self, machine_id: &str) -> Result<(), ApiError> {
        self.post(machine_id, "revoke", &json!({})).await
    }
}
