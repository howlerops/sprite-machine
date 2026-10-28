//! The runner's settings file, `<state dir>/settings.json` (M2B-009, PLAN-v2-ade.md §4.10).
//!
//! `"autoConnect": true` means every interactive local session registers with the hub. It is off
//! by default. A missing file, a file that does not parse, or a key with a non-boolean value is
//! the default.

use std::fs;
use std::path::Path;

use serde_json::Value;

/// The settings file name, in the state dir.
pub const FILE: &str = "settings.json";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Every interactive session registers automatically (`"autoConnect": true`).
    pub auto_connect: bool,
}

/// The settings in the bytes of a settings file.
pub fn parse(bytes: &[u8]) -> Settings {
    let auto_connect = serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|value| value.get("autoConnect")?.as_bool())
        .unwrap_or(false);
    Settings { auto_connect }
}

/// The settings of a state dir. A file that cannot be read is the defaults.
pub fn load(state_dir: &Path) -> Settings {
    fs::read(state_dir.join(FILE))
        .map(|bytes| parse(&bytes))
        .unwrap_or_default()
}
