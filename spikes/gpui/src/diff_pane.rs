//! The diff pane: a read-only unified diff of one file, from the `diff-5k` fixture.
//!
//! The rows come from `gpui-kit`'s `Diff` component (`component::diff`), which is a GPUI
//! `list`: only the rows in the viewport (plus overdraw) are laid out, so a 5,000-line file costs
//! the same as a screenful. Syntax highlighting is on for the file's language, which the
//! component detects from the path (`src/generated.rs` is `rust`) through the highlighter that
//! the `syntect` feature of `gpui-kit` enables.

use gpui_kit::component::diff::{Diff, DiffFile, DiffMode, DiffState};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::diff_model::{reassemble_diffs, unified_patch};

/// Element id of the pane (scroll target in tests).
pub const PANE_ID: &str = "diff-pane";
/// Element id carried by every laid-out source row of the diff (`gpui-kit` sets it on the text
/// of each row; its aria label is the source line).
pub const LINE_ID: &str = "source";
/// Thread id the `diff-5k` fixture log is generated for.
pub const DIFF_THREAD: &str = "th_diff";

/// The `diff-5k` fixture reassembled and rendered as one unified patch.
pub fn diff5k_patch() -> Result<String, String> {
    let log = agui_fixture::complete_log(agui_fixture::Scenario::Diff5k, DIFF_THREAD)
        .map_err(|err| err.to_string())?;
    let diffs = reassemble_diffs(&log)?;
    let [diff] = diffs.as_slice() else {
        return Err(format!("expected one diff, found {}", diffs.len()));
    };
    Ok(unified_patch(
        &diff.path,
        diff.old_text.as_deref(),
        &diff.new_text,
    ))
}

/// The `diff-5k` fixture as the component's files (one file).
pub fn diff5k_files() -> Result<Vec<DiffFile>, String> {
    let patch = diff5k_patch()?;
    DiffFile::parse(&patch).map_err(|err| err.to_string())
}

pub struct DiffPane {
    state: Entity<DiffState>,
}

impl DiffPane {
    pub fn new(files: Vec<DiffFile>, cx: &mut Context<Self>) -> Self {
        let state = cx.new(|cx| DiffState::new(files, cx).with_mode(DiffMode::Unified));
        Self { state }
    }

    pub fn state(&self) -> &Entity<DiffState> {
        &self.state
    }
}

impl Render for DiffPane {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(PANE_ID)
            .test_support()
            .relative()
            .size_full()
            .child(
                Diff::new(&self.state)
                    .size_full()
                    .line_number(true)
                    .syntax_highlight(true)
                    .header_visible(true)
                    .change_background(true),
            )
    }
}

/// Opens the diff pane over `files` in a window (via `gpui_kit::open_window`, so it gets the
/// `Root` wrapper). Call `gpui_kit::init` first.
pub fn open_diff_pane(
    options: WindowOptions,
    files: Vec<DiffFile>,
    cx: &mut App,
) -> Result<(AnyWindowHandle, Entity<DiffPane>)> {
    gpui_kit::open_window(options, cx, |_, cx| cx.new(|cx| DiffPane::new(files, cx)))
}

#[cfg(test)]
mod tests {
    // Explicit imports: a glob would bring gpui's `test` attribute into scope and shadow `#[test]`.
    use super::{diff5k_files, diff5k_patch};

    #[test]
    fn diff_fixture_reassembles_to_5000_added_lines() {
        let patch = diff5k_patch().expect("diff-5k patch");
        assert!(patch.starts_with("--- /dev/null\n+++ b/src/generated.rs\n@@ -0,0 +1,5000 @@\n"));
        assert!(patch.contains("+// generated line 0001: lorem ipsum"));
        assert!(patch.contains("+// generated line 5000: lorem ipsum"));
    }

    #[test]
    fn diff_fixture_parses_into_one_rust_file_with_5000_additions() {
        let files = diff5k_files().expect("diff-5k files");
        let [file] = files.as_slice() else {
            panic!("expected one file, found {}", files.len());
        };
        assert_eq!(file.path().as_str(), "src/generated.rs");
        assert_eq!(file.additions(), 5_000);
        assert_eq!(file.deletions(), 0);
        assert_eq!(file.language().as_str(), "rust");
    }
}
