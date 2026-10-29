//! `palette`: the command palette and the desktop's keyboard shortcuts (plan §3, surface 10).
//!
//! [`PaletteModel`] (in `model.rs`) holds every decision: the command list, fuzzy filtering and
//! ranking, the wrapping selection. [`PaletteView`] (in `view.rs`) renders it as an overlay and
//! emits [`PaletteEvent`]s; the shell executes the chosen [`CommandId`].
//!
//! # Shortcuts
//!
//! Every binding lives in [`SHORTCUTS`] (the source of truth for key strokes and for the labels
//! shown in the palette). "Primary" is Cmd on macOS and Ctrl elsewhere. GPUI's `secondary-`
//! alias is not available to `KeyBinding::new` at this pin (it only resolves in
//! `Keystroke::parse_with_os`), so the table spells out both strokes.
//!
//! | Shortcut (macOS / other)      | Where     | Does                                           |
//! |-------------------------------|-----------|------------------------------------------------|
//! | `cmd-k` / `ctrl-k`            | shell     | open the palette (again: close it)             |
//! | `escape`                      | palette   | close the palette                              |
//! | `enter`                       | palette   | run the selected command                       |
//! | `up` / `down`                 | palette   | select previous / next command (wraps)        |
//! | `cmd-n` / `ctrl-n`            | shell     | new task: show the composer, focus the prompt  |
//! | `cmd-enter` / `ctrl-enter`    | composer  | send the task (bound in `compose`)             |
//! | `alt-down` / `alt-up`         | shell     | open the next / previous thread in inbox order |
//! | `ctrl-tab` / `ctrl-shift-tab` | shell     | next / previous page                           |
//!
//! Thread navigation uses Alt+Down / Alt+Up on every platform: Cmd+] and Cmd+[ are the text
//! input's indent and outdent keys, so they would not reach the shell from a focused field.
//! It stops at the first and last thread instead of wrapping.

mod model;
mod view;

use std::sync::OnceLock;

use gpui_kit::*;

use crate::shell::i18n::Catalog;

pub use model::{
    Command, CommandId, PaletteModel, ThreadRef, adjacent, build_commands, fuzzy_score,
    thread_order,
};
pub use view::{PaletteEvent, PaletteView};

/// The palette's English strings (`locales/en/palette.ftl`).
pub const EN_FTL: &str = include_str!("../../locales/en/palette.ftl");

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG
        .get_or_init(|| Catalog::from_source("en", EN_FTL).expect("locales/en/palette.ftl parses"))
}

pub(crate) fn tr(id: &str) -> String {
    catalog().tr(id, &[])
}

pub(crate) fn tr_args(id: &str, args: &[(&str, &str)]) -> String {
    catalog().tr(id, args)
}

actions!(
    palette,
    [
        OpenPalette,
        ClosePalette,
        SelectNextCommand,
        SelectPreviousCommand,
        NextThread,
        PreviousThread
    ]
);

/// Key context of the palette overlay.
pub(crate) const CONTEXT: &str = "Palette";

/// The shell's key context (see `shell::view`), where the global shortcuts are bound.
const SHELL_CONTEXT: &str = "Shell";

/// A shortcut the desktop defines, by what it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShortcutId {
    OpenPalette,
    ClosePalette,
    RunCommand,
    SelectPrevious,
    SelectNext,
    NewTask,
    SendTask,
    NextThread,
    PreviousThread,
    NextSection,
    PreviousSection,
}

/// One row of the shortcut table.
#[derive(Debug, Clone, Copy)]
pub struct Shortcut {
    pub id: ShortcutId,
    /// GPUI key stroke on macOS.
    pub mac: &'static str,
    /// GPUI key stroke on Linux and Windows.
    pub other: &'static str,
}

impl Shortcut {
    /// The stroke to bind on the given platform.
    pub fn keystroke(&self, mac: bool) -> &'static str {
        if mac { self.mac } else { self.other }
    }

    /// The stroke as shown to the user: `⌘K` on macOS, `Ctrl+K` elsewhere.
    pub fn label(&self, mac: bool) -> String {
        display(self.keystroke(mac), mac)
    }
}

const fn row(id: ShortcutId, mac: &'static str, other: &'static str) -> Shortcut {
    Shortcut { id, mac, other }
}

/// Every shortcut, in one place. Bindings are made from this table; the palette shows its labels.
pub const SHORTCUTS: [Shortcut; 11] = [
    row(ShortcutId::OpenPalette, "cmd-k", "ctrl-k"),
    row(ShortcutId::ClosePalette, "escape", "escape"),
    row(ShortcutId::RunCommand, "enter", "enter"),
    row(ShortcutId::SelectPrevious, "up", "up"),
    row(ShortcutId::SelectNext, "down", "down"),
    row(ShortcutId::NewTask, "cmd-n", "ctrl-n"),
    row(ShortcutId::SendTask, "cmd-enter", "ctrl-enter"),
    row(ShortcutId::NextThread, "alt-down", "alt-down"),
    row(ShortcutId::PreviousThread, "alt-up", "alt-up"),
    row(ShortcutId::NextSection, "ctrl-tab", "ctrl-tab"),
    row(
        ShortcutId::PreviousSection,
        "ctrl-shift-tab",
        "ctrl-shift-tab",
    ),
];

/// The table row of `id`.
pub fn shortcut(id: ShortcutId) -> &'static Shortcut {
    SHORTCUTS
        .iter()
        .find(|shortcut| shortcut.id == id)
        .expect("every ShortcutId is in SHORTCUTS")
}

/// The stroke of `id` on this platform, for `KeyBinding::new`.
pub fn keystroke(id: ShortcutId) -> &'static str {
    shortcut(id).keystroke(cfg!(target_os = "macos"))
}

/// The label of `id` on this platform.
pub fn label(id: ShortcutId) -> String {
    shortcut(id).label(cfg!(target_os = "macos"))
}

/// A GPUI stroke (`cmd-shift-k`) as the user reads it: `⌘⇧K` on macOS, `Ctrl+Shift+K` elsewhere.
pub fn display(stroke: &str, mac: bool) -> String {
    let parts: Vec<String> = stroke
        .split('-')
        .map(|part| match (part, mac) {
            ("cmd", true) => "⌘".to_owned(),
            ("cmd", false) => "Win".to_owned(),
            ("ctrl", true) => "⌃".to_owned(),
            ("ctrl", false) => "Ctrl".to_owned(),
            ("alt", true) => "⌥".to_owned(),
            ("alt", false) => "Alt".to_owned(),
            ("shift", true) => "⇧".to_owned(),
            ("shift", false) => "Shift".to_owned(),
            ("enter", true) => "↵".to_owned(),
            ("enter", false) => "Enter".to_owned(),
            ("escape", _) => "Esc".to_owned(),
            ("up", _) => "↑".to_owned(),
            ("down", _) => "↓".to_owned(),
            ("tab", _) => "Tab".to_owned(),
            (key, _) => key.to_uppercase(),
        })
        .collect();
    parts.join(if mac { "" } else { "+" })
}

/// Register the palette's key bindings. Call once, after `gpui_kit::init`.
pub fn init(cx: &mut App) {
    for context in [SHELL_CONTEXT, "Shell > Input"] {
        cx.bind_keys([
            KeyBinding::new(
                keystroke(ShortcutId::OpenPalette),
                OpenPalette,
                Some(context),
            ),
            KeyBinding::new(keystroke(ShortcutId::NextThread), NextThread, Some(context)),
            KeyBinding::new(
                keystroke(ShortcutId::PreviousThread),
                PreviousThread,
                Some(context),
            ),
        ]);
    }
    // Inside the palette's field these must win over the field's own Escape / Up / Down.
    for context in [CONTEXT, "Palette > Input"] {
        cx.bind_keys([
            KeyBinding::new(
                keystroke(ShortcutId::ClosePalette),
                ClosePalette,
                Some(context),
            ),
            KeyBinding::new(
                keystroke(ShortcutId::SelectNext),
                SelectNextCommand,
                Some(context),
            ),
            KeyBinding::new(
                keystroke(ShortcutId::SelectPrevious),
                SelectPreviousCommand,
                Some(context),
            ),
        ]);
    }
}
