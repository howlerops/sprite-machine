//! `diff`: the diff viewer (plan §3 "Task view: diffs").
//!
//! - [`model`]: [`DiffModel`], plain data built from a [`DiffInput`] (path, old text, new text):
//!   numbered lines, hunks, collapsible gaps, and the rows of the unified and split layouts.
//!   Binary, oversized and identical inputs become a [`Notice`].
//! - [`view`]: [`DiffView`], a virtualized pane (only the visible rows are laid out) that emits
//!   [`DiffEvent::OpenFile`] from a hunk's "Open file" action when a workspace root is known.
//!
//! Entry point: `cx.new(|_| DiffView::new(DiffInput::from(&diff_content), Some(root)))`, then
//! `cx.subscribe(&view, ...)` for [`DiffEvent`]. The task view wires it in a later item.
//!
//! Strings live in `locales/en/diff.ftl`.

pub mod model;
pub mod view;

use std::sync::OnceLock;

use crate::shell::i18n::Catalog;

pub use model::{
    Block, DiffInput, DiffModel, Gap, Hunk, Line, LineKind, Mode, Notice, Row, resolve_path,
};
pub use view::{DiffEvent, DiffView};

/// The viewer's English strings (`locales/en/diff.ftl`).
pub const EN_FTL: &str = include_str!("../../locales/en/diff.ftl");

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| Catalog::from_source("en", EN_FTL).expect("locales/en/diff.ftl parses"))
}

pub(crate) fn tr(id: &str) -> String {
    catalog().tr(id, &[])
}

pub(crate) fn tr_args(id: &str, args: &[(&str, &str)]) -> String {
    catalog().tr(id, args)
}
