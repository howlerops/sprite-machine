//! [`ShellModel`]: everything the shell decides, with no GPUI in it. The view reads it and
//! forwards clicks; tests drive it directly.

use std::path::PathBuf;
use std::sync::Arc;

use client_core::Connectivity;

use super::auth::SessionGrant;
use super::i18n;
use super::login::{LoginEvent, LoginModel, LoginStage};
use super::nav::{NavModel, Section};
use super::prefs::{Appearance, Preferences, ThemeChoice};
use super::session::{Session, SessionStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Login,
    Main,
}

/// One keyboard stop: the element id and the label a screen reader announces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusSlot {
    pub id: String,
    pub label: String,
}

impl FocusSlot {
    fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

pub struct ShellModel {
    pub login: LoginModel,
    pub nav: NavModel,
    prefs: Preferences,
    prefs_path: Option<PathBuf>,
    session: Option<Session>,
    store: Arc<dyn SessionStore>,
    connectivity: Connectivity,
    hub: String,
}

impl ShellModel {
    /// Start signed in when the store holds a session, else on the login screen.
    pub fn new(
        store: Arc<dyn SessionStore>,
        prefs: Preferences,
        prefs_path: Option<PathBuf>,
        default_hub: &str,
    ) -> Self {
        let session = store.load();
        let hub = if !prefs.hub_url.is_empty() {
            prefs.hub_url.clone()
        } else {
            session
                .as_ref()
                .map(|session| session.hub.clone())
                .unwrap_or_else(|| default_hub.to_string())
        };
        Self {
            login: LoginModel::new(),
            nav: NavModel::default(),
            prefs,
            prefs_path,
            session,
            store,
            connectivity: Connectivity::Connecting,
            hub,
        }
    }

    pub fn screen(&self) -> Screen {
        if self.session.is_some() {
            Screen::Main
        } else {
            Screen::Login
        }
    }

    pub fn hub(&self) -> &str {
        &self.hub
    }

    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    pub fn prefs(&self) -> &Preferences {
        &self.prefs
    }

    /// Feed a login event; a granted session is stored and the shell moves to the main screen.
    pub fn apply_login(&mut self, event: LoginEvent) -> Result<(), String> {
        self.login.apply(event);
        if let LoginStage::SignedIn(grant) = self.login.stage().clone() {
            self.finish_sign_in(grant)?;
        }
        Ok(())
    }

    fn finish_sign_in(&mut self, grant: SessionGrant) -> Result<(), String> {
        let session = Session {
            hub: self.hub.clone(),
            grant,
        };
        self.store.save(&session)?;
        self.session = Some(session);
        self.nav = NavModel::default();
        Ok(())
    }

    /// Forget the session and return to the login screen.
    pub fn sign_out(&mut self) -> Result<(), String> {
        self.store.clear()?;
        self.session = None;
        // `Reset` is ignored after a finished sign-in, so start a fresh login model.
        self.login = LoginModel::new();
        Ok(())
    }

    pub fn set_theme(&mut self, choice: ThemeChoice) -> Result<(), String> {
        self.prefs.theme = choice;
        self.save_prefs()
    }

    pub fn set_hub(&mut self, hub: &str) -> Result<(), String> {
        self.hub = hub.to_string();
        self.prefs.hub_url = hub.to_string();
        self.save_prefs()
    }

    fn save_prefs(&self) -> Result<(), String> {
        match &self.prefs_path {
            Some(path) => self.prefs.save(path),
            None => Ok(()),
        }
    }

    pub fn appearance(&self, system_is_dark: bool) -> Appearance {
        self.prefs.theme.resolve(system_is_dark)
    }

    pub fn set_connectivity(&mut self, connectivity: Connectivity) {
        self.connectivity = connectivity;
    }

    /// The connectivity banner, localized (`None` when online).
    pub fn banner(&self) -> Option<String> {
        match self.connectivity {
            Connectivity::Online => None,
            Connectivity::Connecting => Some(i18n::tr("banner-connecting")),
            Connectivity::Offline => Some(i18n::tr("banner-offline")),
        }
    }

    /// Keyboard focus order of the current screen, first stop first. The view gives element `i`
    /// `tab_index(i + 1)`, so Tab walks exactly this list.
    pub fn focus_order(&self) -> Vec<FocusSlot> {
        match self.screen() {
            Screen::Login => self.login_focus_order(),
            Screen::Main => self.main_focus_order(),
        }
    }

    fn login_focus_order(&self) -> Vec<FocusSlot> {
        match self.login.stage() {
            LoginStage::Choose => vec![
                FocusSlot::new("login-pair", i18n::tr("login-pair-a11y")),
                FocusSlot::new("login-github", i18n::tr("login-github-a11y")),
            ],
            LoginStage::Failed(_) => {
                vec![FocusSlot::new("login-retry", i18n::tr("login-retry-a11y"))]
            }
            LoginStage::SignedIn(_) => Vec::new(),
            _ => vec![FocusSlot::new(
                "login-cancel",
                i18n::tr("login-cancel-a11y"),
            )],
        }
    }

    fn main_focus_order(&self) -> Vec<FocusSlot> {
        let current = self.nav.current();
        let mut slots: Vec<FocusSlot> = Section::ALL
            .iter()
            .map(|section| FocusSlot::new(section.nav_id(), section.a11y_label(current)))
            .collect();
        if current == Section::Settings {
            for choice in ThemeChoice::ALL {
                slots.push(FocusSlot::new(
                    format!("theme-{}", choice.slug()),
                    theme_a11y(choice, choice == self.prefs.theme),
                ));
            }
            slots.push(FocusSlot::new(
                "sign-out",
                i18n::tr("settings-sign-out-a11y"),
            ));
        }
        slots
    }
}

/// Screen-reader label of a theme choice button.
pub fn theme_a11y(choice: ThemeChoice, selected: bool) -> String {
    let name = i18n::tr(choice.message_id());
    let id = if selected {
        "settings-theme-selected-a11y"
    } else {
        "settings-theme-a11y"
    };
    i18n::tr_args(id, &[("choice", &name)])
}
