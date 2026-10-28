//! Shared sign-in types: the granted session and why a sign-in failed.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::i18n;

/// What the hub returns once a sign-in succeeds (client-api.md §5.7 / §5.9):
/// `{ token, expires_at, user, org[, role] }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionGrant {
    pub token: String,
    #[serde(default)]
    pub expires_at: Value,
    #[serde(default)]
    pub user: Value,
    #[serde(default)]
    pub org: Value,
    #[serde(default)]
    pub role: Option<String>,
}

impl SessionGrant {
    /// Parse a hub answer. `None` when there is no non-empty `token`.
    pub fn from_json(body: &Value) -> Option<Self> {
        let grant: Self = serde_json::from_value(body.clone()).ok()?;
        (!grant.token.is_empty()).then_some(grant)
    }

    /// A short human name for the user (string, or the best field of an object).
    pub fn user_label(&self) -> String {
        label_of(
            &self.user,
            &["display_name", "github_login", "name", "email", "id"],
        )
    }

    /// A short human name for the org.
    pub fn org_label(&self) -> String {
        label_of(&self.org, &["name", "slug", "id"])
    }
}

fn label_of(value: &Value, keys: &[&str]) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(map) => keys
            .iter()
            .find_map(|key| map.get(*key).and_then(Value::as_str))
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// Why a sign-in ended without a session.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoginFailure {
    #[error("the code expired")]
    Expired,
    #[error("sign-in was declined")]
    Denied,
    #[error("network: {0}")]
    Network(String),
    #[error("browser: {0}")]
    Browser(String),
    #[error("timed out")]
    Timeout,
    #[error("hub: {0}")]
    Hub(String),
    #[error("cancelled")]
    Cancelled,
}

impl LoginFailure {
    /// The Fluent message id for this failure.
    pub fn message_id(&self) -> &'static str {
        match self {
            Self::Expired => "login-error-expired",
            Self::Denied => "login-error-denied",
            Self::Network(_) => "login-error-network",
            Self::Browser(_) => "login-error-browser",
            Self::Timeout => "login-error-timeout",
            Self::Hub(_) => "login-error-hub",
            Self::Cancelled => "login-error-cancelled",
        }
    }

    fn detail(&self) -> &str {
        match self {
            Self::Network(detail) | Self::Browser(detail) | Self::Hub(detail) => detail,
            _ => "",
        }
    }

    /// The localized message shown to the user.
    pub fn message(&self) -> String {
        i18n::tr_args(self.message_id(), &[("detail", self.detail())])
    }
}

/// `{hub}/v1/{path}` with exactly one slash between the parts.
pub fn api_url(hub: &str, path: &str) -> String {
    format!(
        "{}/v1/{}",
        hub.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}
