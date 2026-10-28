//! Connectivity as the UI shows it: a banner while the client is not live.

/// The client's link to the hub.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Connectivity {
    /// Opening a subscription, or waiting for `live` after one.
    #[default]
    Connecting,
    /// Caught up and live.
    Online,
    /// The last connection failed or closed. Writes wait in the outbox.
    Offline,
}

impl Connectivity {
    /// The banner text for this state, or `None` when no banner is needed.
    pub fn banner(self) -> Option<&'static str> {
        match self {
            Self::Connecting => Some("Connecting..."),
            Self::Online => None,
            Self::Offline => Some("Offline: changes will be sent when you reconnect"),
        }
    }
}
