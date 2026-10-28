//! Per-project state in `<state dir>/projects.json`: the spawn mode chosen for a directory and
//! the last session started there (`-c`). A small JSON object keyed by directory:
//!
//! ```json
//! { "/src/app": { "spawn": "worktree",
//!                 "last": { "thread_id": "th_1", "name": "host-calm-otter", "harness": "claude",
//!                           "workspace": "/src/app", "cwd": "/runner/worktrees/host-calm-otter" } } }
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use super::SpawnMode;

/// The last session started for a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastSession {
    pub thread_id: String,
    pub name: String,
    pub harness: String,
    /// The directory the hub asked for (the project).
    pub workspace: PathBuf,
    /// Where the session ran (a worktree, or `workspace`).
    pub cwd: PathBuf,
}

pub struct Projects {
    file: PathBuf,
}

impl Projects {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            file: state_dir.join("projects.json"),
        }
    }

    fn read(&self) -> Map<String, Value> {
        std::fs::read(&self.file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default()
    }

    fn update(&self, dir: &Path, edit: impl FnOnce(&mut Map<String, Value>)) -> Result<()> {
        let mut all = self.read();
        let entry = all
            .entry(dir.to_string_lossy().into_owned())
            .or_insert_with(|| json!({}));
        if let Some(object) = entry.as_object_mut() {
            edit(object);
        }
        let tmp = self.file.with_extension("json.tmp");
        std::fs::write(&tmp, Value::Object(all).to_string())
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.file)
            .with_context(|| format!("write {}", self.file.display()))?;
        Ok(())
    }

    fn entry(&self, dir: &Path) -> Option<Value> {
        self.read().remove(dir.to_string_lossy().as_ref())
    }

    /// The spawn mode saved for `dir`.
    pub fn spawn_mode(&self, dir: &Path) -> Option<SpawnMode> {
        self.entry(dir)?
            .get("spawn")?
            .as_str()
            .and_then(SpawnMode::parse)
    }

    pub fn set_spawn_mode(&self, dir: &Path, mode: SpawnMode) -> Result<()> {
        self.update(dir, |entry| {
            entry.insert("spawn".to_owned(), json!(mode.as_str()));
        })
    }

    pub fn last_session(&self, dir: &Path) -> Option<LastSession> {
        let entry = self.entry(dir)?;
        let last = entry.get("last")?;
        let text = |key: &str| last.get(key)?.as_str().map(str::to_owned);
        Some(LastSession {
            thread_id: text("thread_id")?,
            name: text("name")?,
            harness: text("harness")?,
            workspace: PathBuf::from(text("workspace")?),
            cwd: PathBuf::from(text("cwd")?),
        })
    }

    pub fn set_last_session(&self, dir: &Path, last: &LastSession) -> Result<()> {
        self.update(dir, |entry| {
            entry.insert(
                "last".to_owned(),
                json!({
                    "thread_id": last.thread_id,
                    "name": last.name,
                    "harness": last.harness,
                    "workspace": last.workspace,
                    "cwd": last.cwd,
                }),
            );
        })
    }
}
