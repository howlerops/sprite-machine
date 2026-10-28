//! The error type of the thread logic: an HTTP status plus the client-api.md §4 body.

use protocol::turn_event::Actor;
use serde_json::{Value, json};

use super::store::StoreError;

/// An error answer: `status` and the body `{error, message, resolved_by?}` (client-api.md §4).
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: u16,
    /// Stable machine-readable code (`already_resolved`, `invalid_sequence`, ...).
    pub code: &'static str,
    /// For humans.
    pub message: String,
    /// Set on `409 already_resolved`: who won.
    pub resolved_by: Option<Box<Actor>>,
}

impl ApiError {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            resolved_by: None,
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request", message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(401, "unauthorized", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, "not_found", message)
    }

    pub fn invalid_sequence(message: impl Into<String>) -> Self {
        Self::new(422, "invalid_sequence", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, "internal", message)
    }

    /// The JSON body of the error response.
    pub fn body(&self) -> Value {
        let mut body = json!({ "error": self.code, "message": self.message });
        if let (Some(actor), Some(map)) = (&self.resolved_by, body.as_object_mut()) {
            map.insert(
                "resolved_by".to_owned(),
                serde_json::to_value(actor).unwrap_or(Value::Null),
            );
        }
        body
    }
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        Self::internal(format!("storage: {}", error.0))
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.status, self.code, self.message)
    }
}

impl std::error::Error for ApiError {}
