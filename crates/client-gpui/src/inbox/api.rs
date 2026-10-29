//! The inbox's requests, built on the typed shapes of `model.rs`: `GET /v1/threads`,
//! `PATCH /v1/threads/{id}` and `POST /v1/threads/{id}/interrupts/{interrupt_id}`
//! (client-api.md §5.2, §5.4). `api` is the `…/v1` base URL.

use client_core::outbox::new_idempotency_key;
use reqwest::{Client, RequestBuilder};
use serde_json::Value;

use super::model::{AnswerRequest, Failure, ListRequest, PatchRequest, ThreadRow, parse_threads};

/// Pages followed for one list; the hub orders by newest change, so the rest is older.
const MAX_PAGES: usize = 5;

fn authed(request: RequestBuilder, token: Option<&str>) -> RequestBuilder {
    match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    }
}

async fn check(response: reqwest::Response, what: &str) -> Result<reqwest::Response, Failure> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let detail = response
        .json::<Value>()
        .await
        .ok()
        .and_then(|body| body.get("message")?.as_str().map(str::to_owned));
    Err(Failure {
        status: Some(status.as_u16()),
        message: match detail {
            Some(detail) => format!("{what} answered HTTP {}: {detail}", status.as_u16()),
            None => format!("{what} answered HTTP {}", status.as_u16()),
        },
    })
}

fn transport(error: reqwest::Error) -> Failure {
    Failure {
        status: None,
        message: error.to_string(),
    }
}

/// `GET {api}/threads` with the request's query, following `next_cursor`.
pub async fn fetch_threads(
    api: &str,
    token: Option<&str>,
    request: &ListRequest,
) -> Result<Vec<ThreadRow>, Failure> {
    let url = format!("{}/threads", api.trim_end_matches('/'));
    let client = Client::new();
    let mut rows = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut query = request.query.clone();
        if let Some(cursor) = &cursor {
            query.push(("cursor".to_owned(), cursor.clone()));
        }
        let response = authed(client.get(&url).query(&query), token)
            .send()
            .await
            .map_err(transport)?;
        let body: Value = check(response, "GET /v1/threads")
            .await?
            .json()
            .await
            .map_err(transport)?;
        let (page, next) = parse_threads(&body);
        rows.extend(page);
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    Ok(rows)
}

/// `PATCH {api}/threads/{id}`.
pub async fn patch_thread(
    api: &str,
    token: Option<&str>,
    request: &PatchRequest,
) -> Result<(), Failure> {
    let url = format!(
        "{}/threads/{}",
        api.trim_end_matches('/'),
        request.thread_id
    );
    let response = authed(
        Client::new()
            .patch(url)
            .header("Idempotency-Key", new_idempotency_key())
            .json(&request.body),
        token,
    )
    .send()
    .await
    .map_err(transport)?;
    check(response, "PATCH /v1/threads/{id}").await.map(|_| ())
}

/// `POST {api}/threads/{id}/interrupts/{interrupt_id}`, under the request's own key.
pub async fn answer_interrupt(
    api: &str,
    token: Option<&str>,
    request: &AnswerRequest,
) -> Result<(), Failure> {
    let url = format!(
        "{}/threads/{}/interrupts/{}",
        api.trim_end_matches('/'),
        request.thread_id,
        request.interrupt_id
    );
    let response = authed(
        Client::new()
            .post(url)
            .header("Idempotency-Key", &request.idempotency_key)
            .json(&request.body),
        token,
    )
    .send()
    .await
    .map_err(transport)?;
    check(response, "POST /v1/threads/{id}/interrupts/{id}")
        .await
        .map(|_| ())
}
