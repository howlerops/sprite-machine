//! Opening the system browser, behind a trait so tests never launch one.

use std::sync::Mutex;

/// Opens a URL in the user's default browser.
pub trait Browser: Send + Sync {
    fn open(&self, url: &str) -> Result<(), String>;
}

/// The real thing (`xdg-open`, `open`, `start`).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemBrowser;

impl Browser for SystemBrowser {
    fn open(&self, url: &str) -> Result<(), String> {
        open::that_detached(url).map_err(|error| error.to_string())
    }
}

/// Records the URLs it was asked to open, or fails on demand. For tests and headless runs.
#[derive(Debug, Default)]
pub struct RecordingBrowser {
    opened: Mutex<Vec<String>>,
    fail_with: Mutex<Option<String>>,
}

impl RecordingBrowser {
    pub fn new() -> Self {
        Self::default()
    }

    /// A browser whose launch always fails with `message`.
    pub fn failing(message: &str) -> Self {
        let browser = Self::default();
        *browser.fail_with.lock().expect("lock") = Some(message.to_string());
        browser
    }

    pub fn opened(&self) -> Vec<String> {
        self.opened.lock().expect("lock").clone()
    }
}

impl Browser for RecordingBrowser {
    fn open(&self, url: &str) -> Result<(), String> {
        if let Some(message) = self.fail_with.lock().expect("lock").clone() {
            return Err(message);
        }
        self.opened.lock().expect("lock").push(url.to_string());
        Ok(())
    }
}
