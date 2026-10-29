//! The desktop shell: window, sign-in (device-code pairing with a QR code, or GitHub OAuth PKCE
//! through the system browser), navigation, settings, theming and the Fluent string catalog.
//!
//! Logic is plain Rust and tested without a window:
//! [`model::ShellModel`] (screens, focus order, preferences), [`login`] (the sign-in state machine
//! and its background driver), [`pairing`] (RFC 8628 against `/v1/auth/device/*`), [`pkce`]
//! (RFC 7636 + loopback redirect against `/v1/auth/github/*`), [`i18n`] (Fluent) and [`nav`].
//! [`view::AppShell`] is the thin GPUI view over them.

pub mod auth;
pub mod browser;
pub mod i18n;
pub mod link;
pub mod login;
pub mod machines;
pub mod model;
pub mod nav;
pub mod pairing;
pub mod pkce;
pub mod prefs;
pub mod session;
pub mod view;

pub use view::{AppConfig, AppShell, init};

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::{WindowOptions, prelude::*};

/// `$XDG_CONFIG_HOME/sprite-machine` (or `~/.config/sprite-machine`).
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sprite-machine")
}

/// The configuration a real launch uses: the system browser, files under [`config_dir`], and the
/// hub from `SM_HUB_URL` (default: the local `wrangler dev` address).
pub fn default_config() -> AppConfig {
    let dir = config_dir();
    let prefs_path = dir.join("preferences.json");
    AppConfig {
        hub: std::env::var("SM_HUB_URL").unwrap_or_else(|_| "http://127.0.0.1:8787".to_string()),
        browser: Arc::new(browser::SystemBrowser),
        store: Arc::new(session::FileStore::new(dir.join("session.json"))),
        prefs: prefs::Preferences::load(&prefs_path),
        prefs_path: Some(prefs_path),
        poll_speedup: 1,
        link_thread: String::new(),
        clock: Arc::new(client_core::outbox::SystemClock),
    }
}

/// Open the main window and run the app until it closes.
pub fn run(config: AppConfig) {
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        init(cx);
        gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
            cx.new(|cx| AppShell::new(config, window, cx))
        })
        .expect("open main window");
    });
}
