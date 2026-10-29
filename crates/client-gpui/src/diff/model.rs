//! The diff as plain data: no GPUI, no I/O. An old/new text pair becomes numbered lines, hunks
//! with collapsible gaps between them, and the display rows of the unified and split layouts.
//!
//! `similar` (already in the lockfile through gpui-component) computes the line edit script.
//! Binary, oversized and identical inputs produce a [`Notice`] instead of lines.

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use protocol::turn_event::DiffContent;
use similar::{ChangeTag, TextDiff};

use super::tr_args;

/// Unchanged lines kept around each change, as in a command-line diff.
pub const CONTEXT_LINES: usize = 3;
/// A run of unchanged lines shorter than this is shown rather than collapsed.
pub const MIN_GAP_LINES: usize = 2;
/// Larger input (either side, in bytes) is not diffed.
pub const MAX_SIDE_BYTES: usize = 2 * 1024 * 1024;
/// More lines than this on either side is not diffed.
pub const MAX_SIDE_LINES: usize = 100_000;
/// Longest line text kept; the rest is replaced by an ellipsis.
pub const MAX_LINE_CHARS: usize = 2_000;
/// Upper bound on the time spent computing the edit script (the result stays correct, only less
/// minimal, when it is hit).
const DIFF_DEADLINE: Duration = Duration::from_secs(2);

/// One file's before and after text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffInput {
    pub path: String,
    /// `None` for a new file.
    pub old_text: Option<String>,
    pub new_text: String,
}

impl DiffInput {
    pub fn new(path: impl Into<String>, old_text: Option<String>, new_text: String) -> Self {
        Self {
            path: path.into(),
            old_text,
            new_text,
        }
    }
}

impl From<&DiffContent> for DiffInput {
    /// A `diff` activity or ACP tool-call `diff` content (path, oldText, newText).
    fn from(content: &DiffContent) -> Self {
        Self::new(
            content.path.clone(),
            content.old_text.clone(),
            content.new_text.clone(),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

/// One numbered line of the diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    /// 1-based line number in the old text (`None` for added lines).
    pub old_no: Option<u32>,
    /// 1-based line number in the new text (`None` for removed lines).
    pub new_no: Option<u32>,
    /// Sanitized text without the line ending.
    pub text: String,
    /// The line is the last of its file and has no line ending.
    pub no_newline: bool,
}

/// Why there are no lines to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Binary,
    TooLarge { bytes: usize },
    Identical,
}

impl Notice {
    /// The localized message.
    pub fn message(&self) -> String {
        match self {
            Notice::Binary => super::tr("diff-binary"),
            Notice::TooLarge { bytes } => tr_args(
                "diff-too-large",
                &[
                    ("size", &format_bytes(*bytes)),
                    ("limit", &format_bytes(MAX_SIDE_BYTES)),
                ],
            ),
            Notice::Identical => super::tr("diff-identical"),
        }
    }
}

/// `1.5 MiB`, `300 B`.
pub fn format_bytes(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = KIB * 1024;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// A visible run of lines (changes plus context) or a collapsed run of unchanged lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Hunk(Hunk),
    Gap(Gap),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// Index among the hunks (not the blocks).
    pub index: usize,
    pub lines: Range<usize>,
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    /// Line of the new file an "open file" action jumps to: the first changed line.
    pub open_line: u32,
}

impl Hunk {
    /// `@@ -3,7 +3,8 @@`
    pub fn header(&self) -> String {
        format!(
            "@@ -{},{} +{},{} @@",
            self.old_start, self.old_len, self.new_start, self.new_len
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    /// Identifies the gap across layout changes: the index of its first line.
    pub id: usize,
    pub lines: Range<usize>,
}

impl Gap {
    pub fn hidden(&self) -> usize {
        self.lines.len()
    }
}

/// One display row. Rows hold indices into [`DiffModel::lines`] so a layout is cheap to rebuild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// Header of hunk `usize` (by hunk index).
    Hunk(usize),
    /// A collapsed gap (by gap id).
    Gap(usize),
    /// One line of the unified layout.
    Unified(usize),
    /// One row of the split layout: removed on the left, added on the right, context on both.
    Split {
        left: Option<usize>,
        right: Option<usize>,
    },
    /// "No newline at end of file" after line `usize`.
    NoNewline(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Unified,
    Split,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffModel {
    pub path: String,
    pub new_file: bool,
    pub lines: Vec<Line>,
    pub blocks: Vec<Block>,
    pub added: usize,
    pub removed: usize,
    pub notice: Option<Notice>,
}

impl DiffModel {
    pub fn build(input: &DiffInput) -> Self {
        let new_file = input.old_text.is_none();
        let old = input.old_text.as_deref().unwrap_or("");
        let new = input.new_text.as_str();
        let mut model = Self {
            path: sanitize_inline(&input.path),
            new_file,
            lines: Vec::new(),
            blocks: Vec::new(),
            added: 0,
            removed: 0,
            notice: None,
        };
        if old.contains('\0') || new.contains('\0') {
            model.notice = Some(Notice::Binary);
            return model;
        }
        let biggest = old.len().max(new.len());
        if biggest > MAX_SIDE_BYTES
            || count_lines(old) > MAX_SIDE_LINES
            || count_lines(new) > MAX_SIDE_LINES
        {
            model.notice = Some(Notice::TooLarge { bytes: biggest });
            return model;
        }

        let diff = TextDiff::configure()
            .timeout(DIFF_DEADLINE)
            .diff_lines(old, new);
        for change in diff.iter_all_changes() {
            let kind = match change.tag() {
                ChangeTag::Equal => LineKind::Context,
                ChangeTag::Insert => LineKind::Added,
                ChangeTag::Delete => LineKind::Removed,
            };
            match kind {
                LineKind::Added => model.added += 1,
                LineKind::Removed => model.removed += 1,
                LineKind::Context => {}
            }
            let value = change.value();
            model.lines.push(Line {
                kind,
                old_no: change.old_index().map(|ix| ix as u32 + 1),
                new_no: change.new_index().map(|ix| ix as u32 + 1),
                text: display_text(value),
                no_newline: !value.ends_with('\n'),
            });
        }
        if model.added == 0 && model.removed == 0 {
            model.lines.clear();
            model.notice = Some(Notice::Identical);
            return model;
        }
        model.blocks = model.compute_blocks();
        model
    }

    fn compute_blocks(&self) -> Vec<Block> {
        let n = self.lines.len();
        let mut visible = vec![false; n];
        for (ix, line) in self.lines.iter().enumerate() {
            if line.kind != LineKind::Context {
                let from = ix.saturating_sub(CONTEXT_LINES);
                let to = (ix + CONTEXT_LINES + 1).min(n);
                visible[from..to].fill(true);
            }
        }
        // Short runs of hidden lines are not worth a gap.
        let mut ix = 0;
        while ix < n {
            if visible[ix] {
                ix += 1;
                continue;
            }
            let start = ix;
            while ix < n && !visible[ix] {
                ix += 1;
            }
            if ix - start < MIN_GAP_LINES {
                visible[start..ix].fill(true);
            }
        }

        let mut blocks = Vec::new();
        let mut hunks = 0;
        let mut ix = 0;
        while ix < n {
            let start = ix;
            let shown = visible[ix];
            while ix < n && visible[ix] == shown {
                ix += 1;
            }
            if shown {
                blocks.push(Block::Hunk(self.hunk(hunks, start..ix)));
                hunks += 1;
            } else {
                blocks.push(Block::Gap(Gap {
                    id: start,
                    lines: start..ix,
                }));
            }
        }
        blocks
    }

    fn hunk(&self, index: usize, lines: Range<usize>) -> Hunk {
        let slice = &self.lines[lines.clone()];
        let old_len = slice.iter().filter(|l| l.old_no.is_some()).count() as u32;
        let new_len = slice.iter().filter(|l| l.new_no.is_some()).count() as u32;
        let old_before = self.lines[..lines.start]
            .iter()
            .filter(|l| l.old_no.is_some())
            .count() as u32;
        let new_before = self.lines[..lines.start]
            .iter()
            .filter(|l| l.new_no.is_some())
            .count() as u32;
        let first_change = slice
            .iter()
            .position(|l| l.kind != LineKind::Context)
            .unwrap_or(0);
        let new_in_front = slice[..first_change]
            .iter()
            .filter(|l| l.new_no.is_some())
            .count() as u32;
        let total_new = self.lines.iter().filter(|l| l.new_no.is_some()).count() as u32;
        Hunk {
            index,
            lines,
            old_start: old_before + 1,
            old_len,
            new_start: new_before + 1,
            new_len,
            open_line: (new_before + new_in_front + 1).min(total_new.max(1)),
        }
    }

    pub fn hunks(&self) -> impl Iterator<Item = &Hunk> {
        self.blocks.iter().filter_map(|block| match block {
            Block::Hunk(hunk) => Some(hunk),
            Block::Gap(_) => None,
        })
    }

    pub fn gaps(&self) -> impl Iterator<Item = &Gap> {
        self.blocks.iter().filter_map(|block| match block {
            Block::Gap(gap) => Some(gap),
            Block::Hunk(_) => None,
        })
    }

    pub fn hunk_at(&self, index: usize) -> Option<&Hunk> {
        self.hunks().nth(index)
    }

    pub fn gap_by_id(&self, id: usize) -> Option<&Gap> {
        self.gaps().find(|gap| gap.id == id)
    }

    /// Digits needed for the widest line number.
    pub fn number_width(&self) -> usize {
        let max = self
            .lines
            .iter()
            .filter_map(|l| l.old_no.max(l.new_no))
            .max()
            .unwrap_or(1);
        max.to_string().len()
    }

    /// The display rows for `mode`, with the gaps in `expanded` opened.
    pub fn rows(&self, mode: Mode, expanded: &BTreeSet<usize>) -> Vec<Row> {
        let mut rows = Vec::new();
        for block in &self.blocks {
            match block {
                Block::Hunk(hunk) => {
                    rows.push(Row::Hunk(hunk.index));
                    self.push_lines(mode, hunk.lines.clone(), &mut rows);
                }
                Block::Gap(gap) if expanded.contains(&gap.id) => {
                    self.push_lines(mode, gap.lines.clone(), &mut rows);
                }
                Block::Gap(gap) => rows.push(Row::Gap(gap.id)),
            }
        }
        rows
    }

    fn push_lines(&self, mode: Mode, range: Range<usize>, rows: &mut Vec<Row>) {
        let marker = |ix: usize, rows: &mut Vec<Row>| {
            if self.lines[ix].no_newline {
                rows.push(Row::NoNewline(ix));
            }
        };
        match mode {
            Mode::Unified => {
                for ix in range {
                    rows.push(Row::Unified(ix));
                    marker(ix, rows);
                }
            }
            Mode::Split => {
                let mut ix = range.start;
                while ix < range.end {
                    if self.lines[ix].kind == LineKind::Context {
                        rows.push(Row::Split {
                            left: Some(ix),
                            right: Some(ix),
                        });
                        marker(ix, rows);
                        ix += 1;
                        continue;
                    }
                    let removed_start = ix;
                    while ix < range.end && self.lines[ix].kind == LineKind::Removed {
                        ix += 1;
                    }
                    let added_start = ix;
                    while ix < range.end && self.lines[ix].kind == LineKind::Added {
                        ix += 1;
                    }
                    let removed = removed_start..added_start;
                    let added = added_start..ix;
                    for pair in 0..removed.len().max(added.len()) {
                        let left = (pair < removed.len()).then(|| removed.start + pair);
                        let right = (pair < added.len()).then(|| added.start + pair);
                        rows.push(Row::Split { left, right });
                        for side in [left, right].into_iter().flatten() {
                            marker(side, rows);
                        }
                    }
                }
            }
        }
    }
}

/// The file to open for a diff `path`, or `None` when there is no workspace root or the path is
/// not inside it (absolute outside the root, or a relative path that climbs out with `..`).
pub fn resolve_path(root: Option<&Path>, path: &str) -> Option<PathBuf> {
    let root = root?;
    let path = Path::new(path);
    if path.as_os_str().is_empty() || path.components().any(|c| c == Component::ParentDir) {
        return None;
    }
    if path.is_absolute() {
        return path.starts_with(root).then(|| path.to_path_buf());
    }
    if path
        .components()
        .any(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
    {
        return None;
    }
    Some(root.join(path))
}

fn count_lines(text: &str) -> usize {
    text.bytes().filter(|b| *b == b'\n').count()
        + usize::from(!text.ends_with('\n') && !text.is_empty())
}

fn display_text(value: &str) -> String {
    let value = value.strip_suffix('\n').unwrap_or(value);
    let value = value.strip_suffix('\r').unwrap_or(value);
    let mut out = String::new();
    for (count, c) in value.chars().enumerate() {
        if count >= MAX_LINE_CHARS {
            out.push('\u{2026}');
            break;
        }
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn sanitize_inline(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    use super::{
        DiffInput, DiffModel, LineKind, MAX_LINE_CHARS, MAX_SIDE_BYTES, MAX_SIDE_LINES, Mode,
        Notice, Row, resolve_path,
    };

    fn input(old: Option<&str>, new: &str) -> DiffInput {
        DiffInput::new("src/a.rs", old.map(str::to_owned), new.to_owned())
    }

    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn simple_replace_has_numbers_and_one_hunk() {
        let model = DiffModel::build(&input(Some("a\nb\nc\n"), "a\nB\nc\n"));
        assert_eq!((model.added, model.removed), (1, 1));
        let kinds: Vec<_> = model
            .lines
            .iter()
            .map(|l| (l.kind, l.old_no, l.new_no, l.text.as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (LineKind::Context, Some(1), Some(1), "a"),
                (LineKind::Removed, Some(2), None, "b"),
                (LineKind::Added, None, Some(2), "B"),
                (LineKind::Context, Some(3), Some(3), "c"),
            ]
        );
        let hunks: Vec<_> = model.hunks().collect();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].header(), "@@ -1,3 +1,3 @@");
        assert_eq!(hunks[0].open_line, 2);
        assert_eq!(model.gaps().count(), 0);
    }

    #[test]
    fn new_file_is_all_added() {
        let model = DiffModel::build(&input(None, "x\ny\n"));
        assert!(model.new_file);
        assert_eq!((model.added, model.removed), (2, 0));
        assert!(model.lines.iter().all(|l| l.old_no.is_none()));
    }

    #[test]
    fn distant_changes_collapse_the_unchanged_middle() {
        let old = numbered(40);
        let new = old
            .replace("line 2\n", "two\n")
            .replace("line 38\n", "thirty-eight\n");
        let model = DiffModel::build(&input(Some(&old), &new));
        let hunks: Vec<_> = model.hunks().collect();
        assert_eq!(hunks.len(), 2);
        let gaps: Vec<_> = model.gaps().collect();
        assert_eq!(gaps.len(), 1, "{:?}", model.blocks);
        assert_eq!(model.lines[gaps[0].lines.start].old_no, Some(6));
        assert_eq!(hunks[1].open_line, 38);
        assert_eq!(hunks[0].old_start, 1);

        let collapsed = model.rows(Mode::Unified, &BTreeSet::new());
        let expanded = model.rows(Mode::Unified, &BTreeSet::from([gaps[0].id]));
        assert!(collapsed.contains(&Row::Gap(gaps[0].id)));
        assert_eq!(expanded.len(), collapsed.len() - 1 + gaps[0].hidden());
        assert!(!expanded.iter().any(|r| matches!(r, Row::Gap(_))));
    }

    #[test]
    fn a_one_line_gap_is_not_collapsed() {
        // Changes at lines 1 and 9 leave exactly 1 hidden line (line 5) with 3 lines of context.
        let old = numbered(9);
        let new = old
            .replace("line 1\n", "one\n")
            .replace("line 9\n", "nine\n");
        let model = DiffModel::build(&input(Some(&old), &new));
        assert_eq!(model.gaps().count(), 0);
        assert_eq!(model.hunks().count(), 1);
    }

    #[test]
    fn split_pairs_removed_with_added_and_pads_the_short_side() {
        let model = DiffModel::build(&input(Some("a\nb\nc\nd\n"), "a\nX\nd\n"));
        let rows = model.rows(Mode::Split, &BTreeSet::new());
        let text = |ix: Option<usize>| ix.map(|i| model.lines[i].text.clone());
        let cells: Vec<_> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Split { left, right } => Some((text(*left), text(*right))),
                _ => None,
            })
            .collect();
        assert_eq!(
            cells,
            vec![
                (Some("a".into()), Some("a".into())),
                (Some("b".into()), Some("X".into())),
                (Some("c".into()), None),
                (Some("d".into()), Some("d".into())),
            ]
        );
        // Unified has one row per line plus the hunk header.
        let unified = model.rows(Mode::Unified, &BTreeSet::new());
        assert_eq!(unified.len(), model.lines.len() + 1);
    }

    #[test]
    fn missing_newline_at_end_is_marked() {
        let model = DiffModel::build(&input(Some("a\nb"), "a\nb\n"));
        let marked: Vec<_> = model
            .lines
            .iter()
            .filter(|l| l.no_newline)
            .map(|l| (l.kind, l.text.as_str()))
            .collect();
        assert_eq!(marked, vec![(LineKind::Removed, "b")]);
        let rows = model.rows(Mode::Unified, &BTreeSet::new());
        assert!(rows.iter().any(|r| matches!(r, Row::NoNewline(_))));
        assert_eq!(model.added, 1);
        assert_eq!(model.removed, 1);
    }

    #[test]
    fn binary_too_large_and_identical_become_notices() {
        let binary = DiffModel::build(&input(Some("a\0b"), "c"));
        assert_eq!(binary.notice, Some(Notice::Binary));
        assert!(binary.lines.is_empty() && binary.blocks.is_empty());

        let big = "x\n".repeat(MAX_SIDE_BYTES / 2 + 1);
        let too_large = DiffModel::build(&input(Some(&big), "x"));
        assert!(matches!(too_large.notice, Some(Notice::TooLarge { .. })));

        let many = "\n".repeat(MAX_SIDE_LINES + 1);
        assert!(matches!(
            DiffModel::build(&input(None, &many)).notice,
            Some(Notice::TooLarge { .. })
        ));

        let same = DiffModel::build(&input(Some("a\n"), "a\n"));
        assert_eq!(same.notice, Some(Notice::Identical));
    }

    #[test]
    fn display_text_is_sanitized_and_capped() {
        let model = DiffModel::build(&input(None, "a\tb\u{1b}[31m\r\n"));
        assert_eq!(model.lines[0].text, "a    b[31m");
        let long = "y".repeat(MAX_LINE_CHARS * 2);
        let model = DiffModel::build(&input(None, &long));
        assert_eq!(model.lines[0].text.chars().count(), MAX_LINE_CHARS + 1);
    }

    #[test]
    fn paths_resolve_only_inside_the_workspace() {
        let root = Path::new("/work/repo");
        assert_eq!(resolve_path(None, "a.rs"), None);
        assert_eq!(
            resolve_path(Some(root), "src/a.rs"),
            Some(PathBuf::from("/work/repo/src/a.rs"))
        );
        assert_eq!(
            resolve_path(Some(root), "/work/repo/a.rs"),
            Some(PathBuf::from("/work/repo/a.rs"))
        );
        assert_eq!(resolve_path(Some(root), "/etc/passwd"), None);
        assert_eq!(resolve_path(Some(root), "../x"), None);
        assert_eq!(resolve_path(Some(root), "/work/repo/../x"), None);
        assert_eq!(resolve_path(Some(root), ""), None);
    }

    #[test]
    fn pure_deletion_opens_at_the_line_where_it_was() {
        let model = DiffModel::build(&input(Some("a\nb\nc\n"), "a\nc\n"));
        let hunk = model.hunks().next().unwrap();
        assert_eq!(hunk.open_line, 2);
        let tail = DiffModel::build(&input(Some("a\nb\n"), "a\n"));
        assert_eq!(tail.hunks().next().unwrap().open_line, 1);
    }
}
