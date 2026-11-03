//! The two machine requests (client-api.md §5.1): `PATCH {api}/machines/{id}` `{ "name" }` and
//! `DELETE {api}/machines/{id}`. `api` is the `…/v1` base URL. Both run off the UI thread
//! ([`run_off_thread`]), like the shell's machines fetch.

use std::sync::mpsc::{Receiver, channel};

use client_core::outbox::new_idempotency_key;
use reqwest::Client;
use serde_json::{Value, json};

use super::model::{Action, Failure};
use crate::compose::Target;

async fn check(response: reqwest::Response) -> Result<(), Failure> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let detail = response
        .json::<Value>()
        .await
        .ok()
        .and_then(|body| body.get("message")?.as_str().map(str::to_owned))
        .unwrap_or_default();
    Err(Failure {
        status: Some(status.as_u16()),
        detail,
    })
}

fn transport(error: reqwest::Error) -> Failure {
    Failure {
        status: None,
        detail: error.to_string(),
    }
}

fn url(target: &Target, machine_id: &str) -> String {
    format!("{}/machines/{machine_id}", target.api.trim_end_matches('/'))
}

/// `PATCH /v1/machines/{id}`: rename.
pub async fn rename(target: &Target, machine_id: &str, name: &str) -> Result<(), Failure> {
    let mut request = Client::new()
        .patch(url(target, machine_id))
        .json(&json!({ "name": name }));
    if let Some(token) = &target.token {
        request = request.bearer_auth(token);
    }
    check(request.send().await.map_err(transport)?).await
}

/// `DELETE /v1/machines/{id}`: revoke (idempotent on the hub; the key is sent as on every DELETE).
pub async fn revoke(target: &Target, machine_id: &str) -> Result<(), Failure> {
    let mut request = Client::new()
        .delete(url(target, machine_id))
        .header("Idempotency-Key", new_idempotency_key());
    if let Some(token) = &target.token {
        request = request.bearer_auth(token);
    }
    check(request.send().await.map_err(transport)?).await
}

/// Performs `action` on its own thread and runtime; the answer is read with `try_recv` so no
/// foreign thread wakes a GPUI task.
pub fn run_off_thread(target: Target, action: Action) -> Receiver<Result<(), Failure>> {
    let (sender, receiver) = channel();
    std::thread::spawn(move || {
        if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            let answer = runtime.block_on(async {
                match &action {
                    Action::Rename { machine_id, name } => rename(&target, machine_id, name).await,
                    Action::Revoke { machine_id, .. } => revoke(&target, machine_id).await,
                }
            });
            let _ = sender.send(answer);
        }
    });
    receiver
}
