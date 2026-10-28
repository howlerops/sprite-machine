//! The composer: a focusable single-line input under the transcript. It has keyboard focus at
//! startup; every key it receives is recorded for the bench hooks (`key_to_glyph`).

use std::rc::Rc;
use std::time::SystemTime;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::bench::{BenchLog, PendingKey, unix_micros};

/// Element id of the composer (its aria label is the field's name).
pub const COMPOSER_ID: &str = "composer";
/// Shown while the composer is empty.
pub const PLACEHOLDER: &str = "Message the agent";

/// Applies one key press to `text`: backspace removes the last character, text the key types is
/// appended. Keys that type nothing (or are chords with control or alt) leave `text` as it is.
pub fn apply_key(text: &mut String, key: &str, key_char: Option<&str>, control: bool, alt: bool) {
    if key == "backspace" {
        text.pop();
        return;
    }
    if control || alt {
        return;
    }
    let typed = match key_char {
        Some(typed) => typed,
        None if key.chars().count() == 1 => key,
        None => return,
    };
    if typed.chars().all(|c| !c.is_control()) {
        text.push_str(typed);
    }
}

pub struct Composer {
    text: String,
    focus_handle: FocusHandle,
    bench: Option<Rc<BenchLog>>,
    next_key_id: u64,
    /// Keys received since the last render; the next frame paints them.
    pending: Vec<PendingKey>,
}

impl Composer {
    pub fn new(bench: Option<Rc<BenchLog>>, cx: &mut Context<Self>) -> Self {
        Self {
            text: String::new(),
            focus_handle: cx.focus_handle(),
            bench,
            next_key_id: 0,
            pending: Vec::new(),
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    fn key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key_us = unix_micros(SystemTime::now());
        self.next_key_id += 1;
        self.pending.push((self.next_key_id, key_us));
        let keystroke = &event.keystroke;
        apply_key(
            &mut self.text,
            &keystroke.key,
            keystroke.key_char.as_deref(),
            keystroke.modifiers.control,
            keystroke.modifiers.alt,
        );
        // Always repaint, even when the text did not change: the key's glyph frame must happen.
        cx.notify();
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(bench) = &self.bench {
            bench.note_render(window);
            bench.keys_painted(window, std::mem::take(&mut self.pending));
        } else {
            self.pending.clear();
        }
        let theme = cx.theme();
        let (foreground, muted, border, background) = (
            theme.foreground,
            theme.muted_foreground,
            theme.border,
            theme.background,
        );
        let (shown, color) = if self.text.is_empty() {
            (PLACEHOLDER.to_owned(), muted)
        } else {
            (self.text.clone(), foreground)
        };
        div()
            .id(COMPOSER_ID)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| this.key_down(event, cx)))
            .test_support()
            .aria_label(PLACEHOLDER)
            .w_full()
            .p_2()
            .rounded(px(6.))
            .border_1()
            .border_color(border)
            .bg(background)
            .text_color(color)
            .text_sm()
            .child(shown)
    }
}

#[cfg(test)]
mod tests {
    use super::apply_key;

    #[test]
    fn composer_typed_text_is_appended_and_backspace_removes_the_last_character() {
        let mut text = String::new();
        apply_key(&mut text, "h", Some("h"), false, false);
        apply_key(&mut text, "i", Some("i"), false, false);
        apply_key(&mut text, "A", Some("A"), false, false);
        assert_eq!(text, "hiA");
        apply_key(&mut text, "backspace", None, false, false);
        assert_eq!(text, "hi");
    }

    #[test]
    fn composer_chords_and_named_keys_type_nothing() {
        let mut text = "ok".to_owned();
        apply_key(&mut text, "c", Some("c"), true, false);
        apply_key(&mut text, "b", Some("b"), false, true);
        apply_key(&mut text, "enter", None, false, false);
        apply_key(&mut text, "up", None, false, false);
        assert_eq!(text, "ok");
    }

    #[test]
    fn composer_single_character_key_without_key_char_is_typed() {
        let mut text = String::new();
        apply_key(&mut text, "s", None, false, false);
        assert_eq!(text, "s");
    }
}
