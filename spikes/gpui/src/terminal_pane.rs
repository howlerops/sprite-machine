//! The terminal pane: the visible grid of a [`TerminalModel`] as styled text rows, plus keyboard
//! input forwarded to an injected sink.
//!
//! In the app the sink is the runner's terminal stdin; the tests read it from a channel. Rows
//! are laid out at a fixed cell size ([`CELL_WIDTH`] x [`CELL_HEIGHT`]) so a run's width is its
//! column count. The grid is at most [`SCREEN_LINES`] rows, so the pane lays out every row
//! without virtualizing.
//!
//! The fixture bytes come from the `terminal` scenario: the `append` chunks of the output, fed in
//! order. The closing snapshot repeats the whole output, so it is not fed again.

use std::sync::mpsc::Sender;

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use protocol::turn_event::{ActivityMode, ActivityType, Event, TerminalContent};

use crate::terminal_input::encode_key;
use crate::terminal_model::{COLUMNS, SCREEN_LINES, StyledRow, TerminalModel};

/// Element id of the pane (focus target and scroll target in tests).
pub const PANE_ID: &str = "terminal-pane";
/// Element id carried by every laid-out row's text (its aria label is the row's text).
pub const LINE_ID: &str = "terminal-line";
/// Width of one grid column, in pixels.
pub const CELL_WIDTH: f32 = 9.0;
/// Height of one grid row, in pixels.
pub const CELL_HEIGHT: f32 = 18.0;
/// Thread id the `terminal` fixture log is generated for.
pub const FIXTURE_THREAD: &str = "th_terminal";
/// Monospace family for the rows (cells must line up).
const MONO_FONT: &str = "monospace";

/// The `terminal` fixture's output: its `append` chunks, in order.
pub fn fixture_output_chunks() -> Result<Vec<Vec<u8>>, String> {
    let log = agui_fixture::complete_log(agui_fixture::Scenario::Terminal, FIXTURE_THREAD)
        .map_err(|err| err.to_string())?;
    let mut chunks = Vec::new();
    for envelope in &log {
        let Event::Activity(activity) = &envelope.event else {
            continue;
        };
        if activity.activity_type != ActivityType::Terminal || activity.mode != ActivityMode::Append
        {
            continue;
        }
        let content = match activity.content_as::<TerminalContent>() {
            Some(Ok(content)) => content,
            Some(Err(err)) => return Err(format!("terminal append: {err}")),
            None => return Err("terminal append has no content".to_owned()),
        };
        if let Some(output) = content.output {
            chunks.push(output.into_bytes());
        }
    }
    Ok(chunks)
}

/// Extra output appended after the fixture in tests: SGR red, bold, a `\r` overwrite, an
/// erase-to-end-of-line (`CSI K`), and a line too long for [`COLUMNS`] (so it wraps).
pub fn extra_test_output() -> Vec<u8> {
    let wrapped = format!("wrap: {}", "0123456789".repeat(12));
    format!(
        "\x1b[31merror: red span\x1b[0m\r\n\
         \x1b[1mbold span\x1b[0m done\r\n\
         progress 1/3\rprogress 3/3\x1b[K\r\n\
         {wrapped}\r\n"
    )
    .into_bytes()
}

pub struct TerminalPane {
    model: TerminalModel,
    input: Sender<Vec<u8>>,
    focus_handle: FocusHandle,
}

impl TerminalPane {
    /// A pane whose model has already consumed `output`, in order. Key input goes to `input`.
    pub fn new(
        output: impl IntoIterator<Item = Vec<u8>>,
        input: Sender<Vec<u8>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut model = TerminalModel::new(COLUMNS, SCREEN_LINES);
        for chunk in output {
            model.feed(&chunk);
        }
        Self {
            model,
            input,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn model(&self) -> &TerminalModel {
        &self.model
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// Output that arrives after the pane opened (a live terminal).
    pub fn feed(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        self.model.feed(bytes);
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent) {
        let keystroke = &event.keystroke;
        let bytes = encode_key(
            &keystroke.key,
            keystroke.key_char.as_deref(),
            keystroke.modifiers.control,
            keystroke.modifiers.alt,
        );
        if let Some(bytes) = bytes {
            // A closed sink means the child is gone; the pane keeps showing its last output.
            let _ = self.input.send(bytes);
        }
    }
}

impl Render for TerminalPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let default_fg = theme.foreground;
        let rows: Vec<_> = self
            .model
            .rows()
            .into_iter()
            .enumerate()
            .map(|(ix, row)| row_element(ix, row, default_fg))
            .collect();
        v_flex()
            .id(PANE_ID)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, _| this.key_down(event)))
            .test_support()
            .aria_label("Terminal")
            .size_full()
            .p_1()
            .bg(theme.background)
            .font_family(MONO_FONT)
            .children(rows)
    }
}

/// One grid row: a fixed-height wrapper, and inside it the row's runs as coloured text.
fn row_element(ix: usize, row: StyledRow, default_fg: Hsla) -> impl IntoElement {
    let runs: Vec<_> = row
        .runs
        .iter()
        .map(|run| {
            let fg = run
                .fg
                .map_or(default_fg, |colour| Hsla::from(rgb(colour.to_u32())));
            div()
                .w(px(run.columns as f32 * CELL_WIDTH))
                .h(px(CELL_HEIGHT))
                .overflow_hidden()
                .text_color(fg)
                .when(run.bold, |el| el.font_weight(FontWeight::BOLD))
                .child(run.text.clone())
        })
        .collect();
    div().id(("terminal-row", ix)).h(px(CELL_HEIGHT)).child(
        h_flex()
            .id(LINE_ID)
            .test_support()
            .aria_label(row.text())
            .h(px(CELL_HEIGHT))
            .whitespace_nowrap()
            .children(runs),
    )
}

/// Opens the terminal pane in a window (via `gpui_kit::open_window`, so it gets the `Root`
/// wrapper), with the pane focused. Call `gpui_kit::init` first.
pub fn open_terminal_pane(
    options: WindowOptions,
    output: Vec<Vec<u8>>,
    input: Sender<Vec<u8>>,
    cx: &mut App,
) -> Result<(AnyWindowHandle, Entity<TerminalPane>)> {
    gpui_kit::open_window(options, cx, |window, cx| {
        let pane = cx.new(|cx| TerminalPane::new(output, input, cx));
        let focus = pane.read(cx).focus_handle().clone();
        window.focus(&focus, cx);
        pane
    })
}

#[cfg(test)]
mod tests {
    // Explicit imports: a glob would bring gpui's `test` attribute into scope and shadow `#[test]`.
    use super::{extra_test_output, fixture_output_chunks};
    use crate::terminal_model::{ANSI_RED, COLUMNS, SCREEN_LINES, TerminalModel};

    #[test]
    fn terminal_pane_fixture_output_is_the_300_test_lines() {
        let chunks = fixture_output_chunks().expect("terminal fixture");
        assert!(chunks.len() > 1, "the output arrives in several chunks");
        let text: String = chunks
            .iter()
            .map(|chunk| String::from_utf8(chunk.clone()).expect("utf-8 chunk"))
            .collect();
        assert_eq!(text.matches("test case_").count(), 300);
        assert_eq!(text.matches("test case_001 ").count(), 1);
    }

    #[test]
    fn terminal_pane_fixture_and_extra_output_render_the_last_lines_and_red() {
        let mut model = TerminalModel::new(COLUMNS, SCREEN_LINES);
        for chunk in fixture_output_chunks().expect("terminal fixture") {
            model.feed(&chunk);
        }
        model.feed(&extra_test_output());
        let rows = model.rows();
        assert!(
            rows.iter()
                .any(|row| row.text() == "test case_300 ... ok ✓ (300 ms)")
        );
        assert!(rows.iter().any(|row| {
            row.runs
                .iter()
                .any(|run| run.text == "error: red span" && run.fg == Some(ANSI_RED))
        }));
    }
}
