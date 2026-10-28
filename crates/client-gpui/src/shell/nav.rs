//! Navigation: the sections of the app, the current one, and the accessibility tables
//! (labels and keyboard focus order) the view renders from.

use super::i18n;

/// A top-level page, in sidebar order (plan §3 surfaces).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Inbox,
    NewTask,
    Agents,
    Machines,
    Workspace,
    Flows,
    Loadouts,
    Usage,
    Settings,
}

impl Section {
    pub const ALL: [Section; 9] = [
        Self::Inbox,
        Self::NewTask,
        Self::Agents,
        Self::Machines,
        Self::Workspace,
        Self::Flows,
        Self::Loadouts,
        Self::Usage,
        Self::Settings,
    ];

    /// Stable element id suffix.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::NewTask => "new-task",
            Self::Agents => "agents",
            Self::Machines => "machines",
            Self::Workspace => "workspace",
            Self::Flows => "flows",
            Self::Loadouts => "loadouts",
            Self::Usage => "usage",
            Self::Settings => "settings",
        }
    }

    /// Fluent id of the visible name.
    pub fn message_id(self) -> &'static str {
        match self {
            Self::Inbox => "nav-inbox",
            Self::NewTask => "nav-new-task",
            Self::Agents => "nav-agents",
            Self::Machines => "nav-machines",
            Self::Workspace => "nav-workspace",
            Self::Flows => "nav-flows",
            Self::Loadouts => "nav-loadouts",
            Self::Usage => "nav-usage",
            Self::Settings => "nav-settings",
        }
    }

    pub fn title(self) -> String {
        i18n::tr(self.message_id())
    }

    /// The screen-reader label of this item in the sidebar.
    pub fn a11y_label(self, current: Section) -> String {
        let title = self.title();
        if self == current {
            i18n::tr_args("nav-current-a11y", &[("section", &title)])
        } else {
            i18n::tr_args("nav-item-a11y", &[("section", &title)])
        }
    }

    /// Element id of the sidebar button.
    pub fn nav_id(self) -> String {
        format!("nav-{}", self.slug())
    }
}

/// Which section is showing; next/previous wrap around (for the keyboard shortcuts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavModel {
    current: Section,
}

impl Default for NavModel {
    fn default() -> Self {
        Self {
            current: Section::Inbox,
        }
    }
}

impl NavModel {
    pub fn current(&self) -> Section {
        self.current
    }

    pub fn select(&mut self, section: Section) {
        self.current = section;
    }

    fn index(&self) -> usize {
        Section::ALL
            .iter()
            .position(|section| *section == self.current)
            .unwrap_or(0)
    }

    pub fn next(&mut self) {
        self.current = Section::ALL[(self.index() + 1) % Section::ALL.len()];
    }

    pub fn previous(&mut self) {
        let len = Section::ALL.len();
        self.current = Section::ALL[(self.index() + len - 1) % len];
    }
}
