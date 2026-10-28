//! The session link and its QR code (M2B-009, PLAN-v2-ade.md §4.10). Pure functions, so they are
//! tested without a terminal:
//!
//! - [`session_url`] is `https://<hub>/t/<id>`, the hub's deep link (M2-010). It opens the mobile
//!   web page's thread, and the app once installed.
//! - [`mobile_url`] is the hub's mobile web page, the QR for `sm mobile` (install the app).
//! - [`QrToggle`] is the spacebar: it shows or hides the QR of the session URL.
//!
//! The QR is rendered by [`crate::login::flow::qr_text`] (the same renderer as `sm login`). The
//! keys that drive it, and the raw mode they need, are in [`keys`].

pub mod keys;

use anyhow::{Result, bail};
use protocol::turn_event::is_valid_id;

use crate::login::flow::normalize_hub;

pub use crate::login::flow::qr_text as render;
pub use keys::{Latest, Prompt};

/// The deep link of a session: `https://<hub>/t/<id>`. The hub is checked the way `sm login` checks
/// it (https, or http on loopback); the id must be a valid thread id.
pub fn session_url(hub: &str, thread_id: &str) -> Result<String> {
    let hub = normalize_hub(hub)?;
    if !is_valid_id(thread_id) {
        bail!("not a session id: {thread_id:?}");
    }
    Ok(format!("{hub}/t/{thread_id}"))
}

/// The hub's mobile web page, `https://<hub>/`, which is what the phone opens (and installs).
pub fn mobile_url(hub: &str) -> Result<String> {
    Ok(format!("{}/", normalize_hub(hub)?))
}

/// The terminal block for a link: the QR (when the link fits in one), then the link itself.
pub fn block(url: &str) -> String {
    let mut block = String::new();
    if let Some(qr) = render(url) {
        block.push_str(&qr);
        block.push('\n');
    }
    block.push_str(&format!("    {url}\n"));
    block
}

/// The spacebar QR toggle of `sm rc` and the interactive session: hidden at start, and each space
/// shows or hides the QR. Other keys change nothing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct QrToggle {
    shown: bool,
}

impl QrToggle {
    /// Handles one key. Returns `true` when the key toggled the QR (it was a space).
    pub fn on_key(&mut self, key: char) -> bool {
        if key != ' ' {
            return false;
        }
        self.shown = !self.shown;
        true
    }

    pub fn shown(&self) -> bool {
        self.shown
    }

    /// What to print for `url` in the current state: the block when shown, nothing when hidden.
    pub fn view(&self, url: &str) -> String {
        if self.shown {
            block(url)
        } else {
            String::new()
        }
    }
}
