//! The runner's local audit log: `<state dir>/audit.jsonl`, one JSON object per line with `at`
//! (UTC, ms), `event` and the event's fields. The protocol has no frame for runner audit events
//! yet, so trust decisions are kept here (see the M2B-008 report).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::turn_event::format_timestamp;
use serde_json::{Value, json};

const FILE: &str = "audit.jsonl";

#[derive(Debug, Clone)]
pub struct Audit {
    file: PathBuf,
}

impl Audit {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            file: state_dir.join(FILE),
        }
    }

    /// Appends `event` with `fields` (a JSON object). A failed write is logged, not returned: the
    /// decision it records has been made either way.
    pub fn record(&self, event: &str, fields: Value) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let mut line = match fields {
            Value::Object(map) => Value::Object(map),
            _ => json!({}),
        };
        if let Some(map) = line.as_object_mut() {
            map.insert("at".to_owned(), json!(format_timestamp(now)));
            map.insert("event".to_owned(), json!(event));
        }
        if let Err(error) = self.append(&format!("{line}\n")) {
            tracing::warn!("audit log {}: {error}", self.file.display());
        }
    }

    fn append(&self, text: &str) -> std::io::Result<()> {
        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut options = fs::OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&self.file)?.write_all(text.as_bytes())
    }
}
