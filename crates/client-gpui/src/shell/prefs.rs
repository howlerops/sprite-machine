//! Desktop preferences (settings screen): theme, language, hub address. Plain data, JSON on disk.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The user's theme choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

/// Light or dark, after the choice meets the system appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Light,
    Dark,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn resolve(self, system_is_dark: bool) -> Appearance {
        match self {
            Self::Light => Appearance::Light,
            Self::Dark => Appearance::Dark,
            Self::System if system_is_dark => Appearance::Dark,
            Self::System => Appearance::Light,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// Fluent id of the visible name.
    pub fn message_id(self) -> &'static str {
        match self {
            Self::System => "settings-theme-system",
            Self::Light => "settings-theme-light",
            Self::Dark => "settings-theme-dark",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub theme: ThemeChoice,
    pub locale: String,
    pub hub_url: String,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            locale: "en".to_string(),
            hub_url: String::new(),
        }
    }
}

impl Preferences {
    /// Missing or unreadable file means defaults.
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
