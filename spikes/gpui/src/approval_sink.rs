//! Where an answer goes. The card talks to an [`AnswerSink`]; the app wires [`HttpAnswerSink`]
//! (the real `POST /threads/{id}/interrupts/{iid}` of client-api.md §5.4, served by agui-fixture
//! in this spike) and tests wire a fake.
//!
//! `answer` blocks on the network. The card calls it on the background executor, never on the
//! UI thread.

use std::time::Duration;

use protocol::turn_event::Actor;

use crate::approval_model::{AnswerError, AnswerRequest};

/// Records one answer. First accepted answer wins; the sink reports the winner on 409.
pub trait AnswerSink: Send + Sync {
    /// `Ok` carries the `resolved_by` of the accepted answer.
    fn answer(&self, request: &AnswerRequest) -> Result<Actor, AnswerError>;
}

/// The real sink: HTTP/1.1 (or TLS) POST to `base_url` plus the interrupt route.
pub struct HttpAnswerSink {
    base_url: String,
    client: reqwest::blocking::Client,
}

impl HttpAnswerSink {
    /// `base_url` is the server origin, with any API prefix (for the fixture: `http://127.0.0.1:PORT`).
    pub fn new(base_url: impl Into<String>) -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|err| err.to_string())?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client,
        })
    }

    /// The route for `request`.
    pub fn url(&self, request: &AnswerRequest) -> String {
        format!(
            "{}/threads/{}/interrupts/{}",
            self.base_url, request.thread_id, request.interrupt_id
        )
    }
}

impl AnswerSink for HttpAnswerSink {
    fn answer(&self, request: &AnswerRequest) -> Result<Actor, AnswerError> {
        let response = self
            .client
            .post(self.url(request))
            .header("Idempotency-Key", &request.idempotency_key)
            .json(&request.body)
            .send()
            .map_err(|err| AnswerError::Transport(err.to_string()))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .map_err(|err| AnswerError::Transport(err.to_string()))?;
        let resolved_by = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|json| json.get("resolved_by").cloned())
            .and_then(|value| serde_json::from_value::<Actor>(value).ok());
        match (status, resolved_by) {
            (200, Some(resolver)) => Ok(resolver),
            (409, Some(resolver)) => Err(AnswerError::AlreadyResolved {
                resolved_by: resolver,
            }),
            (410, _) => Err(AnswerError::Expired),
            _ => Err(AnswerError::Http { status, body: text }),
        }
    }
}
