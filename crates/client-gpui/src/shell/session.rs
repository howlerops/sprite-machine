//! The signed-in session and where it is kept between launches.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::auth::SessionGrant;

/// A session this desktop holds: the hub it belongs to and the grant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub hub: String,
    pub grant: SessionGrant,
}

impl Session {
    pub fn user_label(&self) -> String {
        self.grant.user_label()
    }
    pub fn org_label(&self) -> String {
        self.grant.org_label()
    }
}

/// Persistence for the session. The token is a credential: implementations keep it private to the
/// user (the file store creates it `0600`); an OS keychain store can replace it behind this trait.
pub trait SessionStore: Send + Sync {
    fn load(&self) -> Option<Session>;
    fn save(&self, session: &Session) -> Result<(), String>;
    fn clear(&self) -> Result<(), String>;
}

#[derive(Debug, Default)]
pub struct MemoryStore {
    inner: Mutex<Option<Session>>,
}

impl SessionStore for MemoryStore {
    fn load(&self) -> Option<Session> {
        self.inner.lock().expect("lock").clone()
    }
    fn save(&self, session: &Session) -> Result<(), String> {
        *self.inner.lock().expect("lock") = Some(session.clone());
        Ok(())
    }
    fn clear(&self) -> Result<(), String> {
        *self.inner.lock().expect("lock") = None;
        Ok(())
    }
}

/// A JSON file (`session.json`), owner-only on Unix.
#[derive(Debug, Clone)]
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl SessionStore for FileStore {
    fn load(&self) -> Option<Session> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn save(&self, session: &Session) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let text = serde_json::to_string_pretty(session).map_err(|error| error.to_string())?;
        write_private(&self.path, text.as_bytes()).map_err(|error| error.to_string())
    }

    fn clear(&self) -> Result<(), String> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}
