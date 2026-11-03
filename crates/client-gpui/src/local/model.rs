//! [`PhoneLink`]: which local sessions are connected to the phone, and the global auto-connect
//! setting. No runner, no GPUI: the runner and the view read it and report back.

/// Where a session is in its connection to the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    /// Local only; the hub does not know the session.
    Off,
    /// A connect was asked for and has not finished.
    Connecting,
    /// The hub knows the session.
    On,
    /// The last connect failed; the message is for the user.
    Failed(String),
}

/// One local session as the toggle shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub thread_id: String,
    pub title: String,
    pub state: LinkState,
    /// The session's link once connected.
    pub url: Option<String>,
}

impl SessionRow {
    /// Fluent id of the toggle button's label: the action a click does.
    pub fn toggle_label_id(&self) -> &'static str {
        match self.state {
            LinkState::Off => "phone-session-connect",
            LinkState::Connecting => "phone-session-connecting",
            LinkState::On => "phone-session-disconnect",
            LinkState::Failed(_) => "phone-session-retry",
        }
    }

    /// Fluent id of the state line.
    pub fn state_id(&self) -> &'static str {
        match self.state {
            LinkState::Off => "phone-state-off",
            LinkState::Connecting => "phone-state-connecting",
            LinkState::On => "phone-state-on",
            LinkState::Failed(_) => "phone-state-failed",
        }
    }

    /// Whether the switch reads as on (connected or connecting).
    pub fn is_on(&self) -> bool {
        matches!(self.state, LinkState::On | LinkState::Connecting)
    }
}

/// What the runner must do after a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Connect(String),
    Disconnect(String),
}

#[derive(Debug, Clone, Default)]
pub struct PhoneLink {
    auto: bool,
    rows: Vec<SessionRow>,
}

impl PhoneLink {
    pub fn new(auto: bool) -> Self {
        Self {
            auto,
            rows: Vec::new(),
        }
    }

    pub fn auto(&self) -> bool {
        self.auto
    }

    pub fn rows(&self) -> &[SessionRow] {
        &self.rows
    }

    pub fn row(&self, thread_id: &str) -> Option<&SessionRow> {
        self.rows.iter().find(|row| row.thread_id == thread_id)
    }

    fn row_mut(&mut self, thread_id: &str) -> Option<&mut SessionRow> {
        self.rows.iter_mut().find(|row| row.thread_id == thread_id)
    }

    /// Turning auto-connect on connects every session that is off now; turning it off leaves the
    /// connected ones as they are (it decides about sessions that start later).
    pub fn set_auto(&mut self, on: bool) -> Vec<Action> {
        self.auto = on;
        if !on {
            return Vec::new();
        }
        self.rows
            .iter_mut()
            .filter(|row| row.state == LinkState::Off)
            .map(|row| {
                row.state = LinkState::Connecting;
                Action::Connect(row.thread_id.clone())
            })
            .collect()
    }

    /// A new local session; with auto-connect it connects at once.
    pub fn add_session(&mut self, thread_id: &str, title: &str) -> Vec<Action> {
        if self.row(thread_id).is_some() {
            return Vec::new();
        }
        let state = if self.auto {
            LinkState::Connecting
        } else {
            LinkState::Off
        };
        let connect = state == LinkState::Connecting;
        self.rows.push(SessionRow {
            thread_id: thread_id.to_owned(),
            title: title.to_owned(),
            state,
            url: None,
        });
        if connect {
            vec![Action::Connect(thread_id.to_owned())]
        } else {
            Vec::new()
        }
    }

    /// Forgets every session (the runner stopped).
    pub fn clear(&mut self) {
        self.rows.clear();
    }

    pub fn remove_session(&mut self, thread_id: &str) {
        self.rows.retain(|row| row.thread_id != thread_id);
    }

    /// The per-session toggle: connects an off (or failed) session, disconnects a connected or
    /// connecting one. `None` for an unknown session.
    pub fn toggle(&mut self, thread_id: &str) -> Option<Action> {
        let row = self.row_mut(thread_id)?;
        if row.is_on() {
            row.state = LinkState::Off;
            row.url = None;
            Some(Action::Disconnect(thread_id.to_owned()))
        } else {
            row.state = LinkState::Connecting;
            Some(Action::Connect(thread_id.to_owned()))
        }
    }

    /// The connect finished. If the user switched the session off meanwhile, it is
    /// disconnected again.
    pub fn connected(&mut self, thread_id: &str, url: &str) -> Option<Action> {
        let row = self.row_mut(thread_id)?;
        if row.state == LinkState::Connecting {
            row.state = LinkState::On;
            row.url = Some(url.to_owned());
            None
        } else {
            Some(Action::Disconnect(thread_id.to_owned()))
        }
    }

    /// The connect failed. Ignored when the user switched the session off meanwhile.
    pub fn failed(&mut self, thread_id: &str, message: &str) {
        if let Some(row) = self.row_mut(thread_id)
            && row.state == LinkState::Connecting
        {
            row.state = LinkState::Failed(message.to_owned());
        }
    }
}
