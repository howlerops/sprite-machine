//! The notification decisions (plan §4.2). No GPUI in here: [`Notifier`] sees thread statuses and
//! says which transitions notify; a [`NotificationSink`] shows them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::tr;

/// What a notification is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotifyKind {
    /// The thread waits for an approval or an answer (`needs_you`).
    NeedsYou,
    /// The thread finished (`done`).
    Done,
    /// The thread stopped with an error (`failed`).
    Failed,
}

impl NotifyKind {
    pub const ALL: [NotifyKind; 3] = [Self::NeedsYou, Self::Done, Self::Failed];

    /// The hub status that means this kind; any other status is not notifiable.
    pub fn from_status(status: &str) -> Option<Self> {
        match status {
            "needs_you" => Some(Self::NeedsYou),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::NeedsYou => "needs_you",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// Fluent id of the notification title (generic: no machine, prompt or output).
    pub fn title_id(self) -> String {
        format!("notify-title-{}", self.slug())
    }
}

/// Per-kind switches. All on by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyPrefs {
    pub needs_you: bool,
    pub done: bool,
    pub failed: bool,
}

impl Default for NotifyPrefs {
    fn default() -> Self {
        Self {
            needs_you: true,
            done: true,
            failed: true,
        }
    }
}

impl NotifyPrefs {
    pub fn enabled(&self, kind: NotifyKind) -> bool {
        match kind {
            NotifyKind::NeedsYou => self.needs_you,
            NotifyKind::Done => self.done,
            NotifyKind::Failed => self.failed,
        }
    }

    pub fn set(&mut self, kind: NotifyKind, on: bool) {
        match kind {
            NotifyKind::NeedsYou => self.needs_you = on,
            NotifyKind::Done => self.done = on,
            NotifyKind::Failed => self.failed = on,
        }
    }

    /// A missing or unreadable file means the defaults.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        std::fs::write(path, text).map_err(|error| error.to_string())
    }
}

/// Where the prefs live. The shell passes this in (a file next to the other preferences).
#[derive(Debug, Clone)]
pub struct NotifyConfig {
    pub prefs_path: PathBuf,
}

struct PrefsState {
    path: PathBuf,
    prefs: NotifyPrefs,
}

/// The prefs, loaded from and saved to the configured file. Cloning shares the state, so the
/// [`Notifier`] and the settings view always agree.
#[derive(Clone)]
pub struct PrefsHandle(Arc<Mutex<PrefsState>>);

impl PrefsHandle {
    pub fn load(config: &NotifyConfig) -> Self {
        Self(Arc::new(Mutex::new(PrefsState {
            prefs: NotifyPrefs::load(&config.prefs_path),
            path: config.prefs_path.clone(),
        })))
    }

    pub fn get(&self) -> NotifyPrefs {
        self.0.lock().expect("prefs lock").prefs
    }

    /// Switch a kind. The change applies even when saving fails (the error is returned).
    pub fn set(&self, kind: NotifyKind, on: bool) -> Result<(), String> {
        let mut state = self.0.lock().expect("prefs lock");
        state.prefs.set(kind, on);
        state.prefs.save(&state.path)
    }
}

/// One notification to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// `<thread_id>:<kind>`: the same thread and kind replace each other.
    pub id: String,
    pub thread_id: String,
    pub kind: NotifyKind,
    /// Generic title; never a machine name.
    pub title: String,
}

impl Notification {
    /// What clicking it does.
    pub fn click_event(&self) -> NotifyEvent {
        NotifyEvent::Open {
            thread_id: self.thread_id.clone(),
        }
    }
}

/// What the shell reacts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyEvent {
    /// Open this thread (`AppShell::open_thread`).
    Open { thread_id: String },
}

/// Shows notifications. The real one is [`super::ToastSink`]; tests use a fake.
pub trait NotificationSink {
    fn show(&mut self, notification: &Notification);
}

/// Decides which status changes notify. Feed it every thread status the app learns of.
pub struct Notifier {
    prefs: PrefsHandle,
    sink: Box<dyn NotificationSink>,
    /// The last status seen per thread: a notification needs a *change*.
    seen: HashMap<String, String>,
    focused: Option<String>,
    /// Shown notifications by id, for [`Notifier::clicked`].
    shown: HashMap<String, String>,
}

impl Notifier {
    pub fn new(config: &NotifyConfig, sink: Box<dyn NotificationSink>) -> Self {
        Self::with_prefs(PrefsHandle::load(config), sink)
    }

    pub fn with_prefs(prefs: PrefsHandle, sink: Box<dyn NotificationSink>) -> Self {
        Self {
            prefs,
            sink,
            seen: HashMap::new(),
            focused: None,
            shown: HashMap::new(),
        }
    }

    pub fn prefs(&self) -> &PrefsHandle {
        &self.prefs
    }

    /// The thread the user is looking at (`None` when none, or the window is in the background).
    /// A thread that changes while it is on screen never notifies.
    pub fn set_focused(&mut self, thread_id: Option<&str>) {
        self.focused = thread_id.map(str::to_owned);
    }

    /// Record statuses without notifying: the first list fetch after sign-in, where threads that
    /// already wait were announced in an earlier session.
    pub fn seed<'a>(&mut self, threads: impl IntoIterator<Item = (&'a str, &'a str)>) {
        for (thread_id, status) in threads {
            self.seen.insert(thread_id.to_owned(), status.to_owned());
        }
    }

    /// A thread's status (`needs_you`, `done`, `failed`, anything else is quiet). Notifies once
    /// when the status *changes* to a notifiable kind that is switched on and whose thread is not
    /// the focused one; the same status seen again (a refetch) does nothing. A change that is
    /// suppressed (focused, switched off) is not retried later.
    pub fn observe(&mut self, thread_id: &str, status: &str) -> Option<Notification> {
        let previous = self.seen.insert(thread_id.to_owned(), status.to_owned());
        if previous.as_deref() == Some(status) {
            return None;
        }
        let kind = NotifyKind::from_status(status)?;
        if !self.prefs.get().enabled(kind) || self.focused.as_deref() == Some(thread_id) {
            return None;
        }
        let notification = Notification {
            id: format!("{thread_id}:{}", kind.slug()),
            thread_id: thread_id.to_owned(),
            kind,
            title: tr(&kind.title_id()),
        };
        self.shown
            .insert(notification.id.clone(), thread_id.to_owned());
        self.sink.show(&notification);
        Some(notification)
    }

    /// [`Notifier::observe`] for a whole list.
    pub fn observe_all<'a>(
        &mut self,
        threads: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Vec<Notification> {
        threads
            .into_iter()
            .filter_map(|(thread_id, status)| self.observe(thread_id, status))
            .collect()
    }

    /// The user clicked notification `id`: the event for the shell. `None` for an unknown (or
    /// already clicked) id.
    pub fn clicked(&mut self, id: &str) -> Option<NotifyEvent> {
        self.shown
            .remove(id)
            .map(|thread_id| NotifyEvent::Open { thread_id })
    }

    /// Drop a thread's state (deleted / archived), so a reused id starts fresh.
    pub fn forget(&mut self, thread_id: &str) {
        self.seen.remove(thread_id);
    }
}
