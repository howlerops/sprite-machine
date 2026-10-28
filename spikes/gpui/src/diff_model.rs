//! Pure diff model for the diff pane: no GPUI here.
//!
//! - [`reassemble_diffs`] folds the chunked `Activity{diff}` envelopes of a run into one whole
//!   file per activity (a `snapshot` chunk starts it, `append` chunks extend it, `chunk.last`
//!   closes it).
//! - [`line_diff`] computes a line-level edit script (longest common subsequence, after trimming
//!   the common prefix and suffix).
//! - [`unified_patch`] renders that script as a unified diff with `context` lines around each
//!   change. The GPUI diff component parses that text with `DiffFile::parse`, so the pane never
//!   hand-builds its rows.

use protocol::turn_event::{ActivityMode, ActivityType, Chunk, DiffContent, Envelope, Event};

/// Unchanged lines shown around each change, as in `git diff`.
pub const CONTEXT_LINES: usize = 3;

/// Above this many LCS table cells the middle section is shown as one delete block followed by
/// one insert block (correct, but not minimal). Keeps memory bounded for huge rewrites.
const MAX_LCS_CELLS: usize = 1 << 24;

/// One file reassembled from its chunked diff activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReassembledDiff {
    pub activity_id: String,
    pub path: String,
    /// `None` for a new file.
    pub old_text: Option<String>,
    pub new_text: String,
    /// Number of chunks folded into `new_text`.
    pub chunks: usize,
}

/// One line of an edit script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineOp<'a> {
    Equal(&'a str),
    Delete(&'a str),
    Insert(&'a str),
}

/// Folds every `diff` activity in `envelopes` into whole files, in first-seen order.
///
/// Errors when a chunk arrives out of order, an `append` precedes its `snapshot`, a `patch`
/// mode is used, or an activity ends before its `last` chunk.
pub fn reassemble_diffs<'a>(
    envelopes: impl IntoIterator<Item = &'a Envelope>,
) -> Result<Vec<ReassembledDiff>, String> {
    struct Pending {
        diff: ReassembledDiff,
        next_index: u64,
        finished: bool,
    }

    let mut pending: Vec<Pending> = Vec::new();
    let mut slots: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for envelope in envelopes {
        let Event::Activity(activity) = &envelope.event else {
            continue;
        };
        if activity.activity_type != ActivityType::Diff {
            continue;
        }
        let id = activity.activity_id.as_str();
        let content: DiffContent = match activity.content_as::<DiffContent>() {
            Some(Ok(content)) => content,
            Some(Err(err)) => return Err(format!("diff activity {id}: {err}")),
            None => return Err(format!("diff activity {id} has no content")),
        };
        let index = content.chunk.map_or(0, |Chunk { index, .. }| index);
        let last = content.chunk.is_none_or(|Chunk { last, .. }| last);

        match activity.mode {
            ActivityMode::Snapshot => {
                if index != 0 {
                    return Err(format!("diff {id}: snapshot at chunk {index}, expected 0"));
                }
                let diff = ReassembledDiff {
                    activity_id: id.to_owned(),
                    path: content.path,
                    old_text: content.old_text,
                    new_text: content.new_text,
                    chunks: 1,
                };
                let entry = Pending {
                    diff,
                    next_index: 1,
                    finished: last,
                };
                match slots.get(id) {
                    Some(&slot) => pending[slot] = entry,
                    None => {
                        slots.insert(id.to_owned(), pending.len());
                        pending.push(entry);
                    }
                }
            }
            ActivityMode::Append => {
                let Some(&slot) = slots.get(id) else {
                    return Err(format!("diff {id}: append before any snapshot"));
                };
                let entry = &mut pending[slot];
                if entry.finished {
                    return Err(format!("diff {id}: append after the last chunk"));
                }
                if index != entry.next_index {
                    return Err(format!(
                        "diff {id}: chunk {index} follows chunk {}",
                        entry.next_index.saturating_sub(1)
                    ));
                }
                entry.diff.new_text.push_str(&content.new_text);
                entry.diff.chunks += 1;
                entry.next_index += 1;
                entry.finished = last;
            }
            ActivityMode::Patch => {
                return Err(format!("diff {id}: patch mode is not used for diffs"));
            }
        }
    }

    pending
        .into_iter()
        .map(|entry| {
            if entry.finished {
                Ok(entry.diff)
            } else {
                Err(format!(
                    "diff {} ended before its last chunk",
                    entry.diff.activity_id
                ))
            }
        })
        .collect()
}

/// Line-level edit script from `old` to `new`, in file order.
pub fn line_diff<'a>(old: &'a str, new: &'a str) -> Vec<LineOp<'a>> {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();

    let mut ops = Vec::with_capacity(old.len().max(new.len()));
    ops.extend(old[..prefix].iter().map(|line| LineOp::Equal(line)));
    ops.extend(middle_diff(
        &old[prefix..old.len() - suffix],
        &new[prefix..new.len() - suffix],
    ));
    ops.extend(
        old[old.len() - suffix..]
            .iter()
            .map(|line| LineOp::Equal(line)),
    );
    ops
}

/// LCS edit script of two line slices that share no common prefix or suffix.
fn middle_diff<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<LineOp<'a>> {
    let (m, n) = (old.len(), new.len());
    if m == 0 {
        return new.iter().map(|line| LineOp::Insert(line)).collect();
    }
    if n == 0 {
        return old.iter().map(|line| LineOp::Delete(line)).collect();
    }
    if m.saturating_mul(n) > MAX_LCS_CELLS {
        return old
            .iter()
            .map(|line| LineOp::Delete(line))
            .chain(new.iter().map(|line| LineOp::Insert(line)))
            .collect();
    }

    // lcs[i * w + j] = length of the LCS of old[i..] and new[j..].
    let w = n + 1;
    let mut lcs = vec![0u32; (m + 1) * w];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            lcs[i * w + j] = if old[i] == new[j] {
                lcs[(i + 1) * w + j + 1] + 1
            } else {
                lcs[(i + 1) * w + j].max(lcs[i * w + j + 1])
            };
        }
    }

    let mut ops = Vec::with_capacity(m + n);
    let (mut i, mut j) = (0, 0);
    while i < m && j < n {
        if old[i] == new[j] {
            ops.push(LineOp::Equal(old[i]));
            i += 1;
            j += 1;
        } else if lcs[(i + 1) * w + j] >= lcs[i * w + j + 1] {
            ops.push(LineOp::Delete(old[i]));
            i += 1;
        } else {
            ops.push(LineOp::Insert(new[j]));
            j += 1;
        }
    }
    ops.extend(old[i..].iter().map(|line| LineOp::Delete(line)));
    ops.extend(new[j..].iter().map(|line| LineOp::Insert(line)));
    ops
}

/// A unified diff of `path` from `old` (`None` for a new file) to `new`, with
/// [`CONTEXT_LINES`] of context around each change. Every hunk header carries both counts.
pub fn unified_patch(path: &str, old: Option<&str>, new: &str) -> String {
    let ops = line_diff(old.unwrap_or(""), new);
    let mut out = String::new();
    let old_name = if old.is_some() {
        format!("a/{path}")
    } else {
        "/dev/null".to_owned()
    };
    out.push_str(&format!("--- {old_name}\n+++ b/{path}\n"));

    // Changes padded by the context, merged where the padded windows touch or overlap.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (ix, op) in ops.iter().enumerate() {
        if matches!(op, LineOp::Equal(_)) {
            continue;
        }
        let start = ix.saturating_sub(CONTEXT_LINES);
        let end = (ix + CONTEXT_LINES + 1).min(ops.len());
        match ranges.last_mut() {
            Some((_, last_end)) if start <= *last_end => *last_end = (*last_end).max(end),
            _ => ranges.push((start, end)),
        }
    }

    // Line numbers consumed before the cursor, per side (1-based starts come from these).
    let (mut old_seen, mut new_seen) = (0usize, 0usize);
    let mut cursor = 0usize;
    for (start, end) in ranges {
        // Lines between the previous hunk and this one are unchanged and not printed.
        for op in &ops[cursor..start] {
            match op {
                LineOp::Equal(_) => {
                    old_seen += 1;
                    new_seen += 1;
                }
                LineOp::Delete(_) => old_seen += 1,
                LineOp::Insert(_) => new_seen += 1,
            }
        }
        cursor = end;
        let mut old_count = 0;
        let mut new_count = 0;
        let mut body = String::new();
        for op in &ops[start..end] {
            match op {
                LineOp::Equal(line) => {
                    old_count += 1;
                    new_count += 1;
                    body.push_str(&format!(" {line}\n"));
                }
                LineOp::Delete(line) => {
                    old_count += 1;
                    body.push_str(&format!("-{line}\n"));
                }
                LineOp::Insert(line) => {
                    new_count += 1;
                    body.push_str(&format!("+{line}\n"));
                }
            }
        }
        let old_start = if old_count == 0 {
            old_seen
        } else {
            old_seen + 1
        };
        let new_start = if new_count == 0 {
            new_seen
        } else {
            new_seen + 1
        };
        out.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        ));
        out.push_str(&body);
        old_seen += old_count;
        new_seen += new_count;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::turn_event::Activity;

    fn diff_envelope(activity_id: &str, mode: ActivityMode, content: DiffContent) -> Envelope {
        let value = serde_json::to_value(&content).expect("serialize diff content");
        let serde_json::Value::Object(map) = value else {
            panic!("diff content is an object");
        };
        let event = Event::Activity(Activity {
            activity_id: activity_id.to_owned(),
            activity_type: ActivityType::Diff,
            mode,
            content: Some(map),
            patch: None,
        });
        Envelope::new(
            "th_test",
            Some("tk_1".to_owned()),
            Some("run_1".to_owned()),
            "2026-10-28T00:00:00.000Z",
            event,
        )
    }

    fn chunk(path: &str, text: &str, index: u64, last: bool) -> DiffContent {
        DiffContent {
            tool_call_id: None,
            path: path.to_owned(),
            old_text: None,
            new_text: text.to_owned(),
            chunk: Some(Chunk { index, last }),
        }
    }

    #[test]
    fn diff_identical_texts_have_only_equal_lines() {
        let ops = line_diff("a\nb\nc\n", "a\nb\nc\n");
        assert_eq!(
            ops,
            vec![LineOp::Equal("a"), LineOp::Equal("b"), LineOp::Equal("c")]
        );
    }

    #[test]
    fn diff_changed_line_is_one_delete_then_one_insert() {
        let ops = line_diff("a\nb\nc", "a\nB\nc");
        assert_eq!(
            ops,
            vec![
                LineOp::Equal("a"),
                LineOp::Delete("b"),
                LineOp::Insert("B"),
                LineOp::Equal("c"),
            ]
        );
    }

    #[test]
    fn diff_new_file_is_all_inserts() {
        let ops = line_diff("", "x\ny");
        assert_eq!(ops, vec![LineOp::Insert("x"), LineOp::Insert("y")]);
    }

    #[test]
    fn diff_uses_lcs_not_positional_matching() {
        // Inserting one line in the middle must not mark the rest of the file as changed.
        let old: String = (0..50).map(|i| format!("line {i}\n")).collect();
        let new = old.replacen("line 20\n", "line 20\nadded\n", 1);
        let ops = line_diff(&old, &new);
        let changes = ops
            .iter()
            .filter(|op| !matches!(op, LineOp::Equal(_)))
            .count();
        assert_eq!(changes, 1, "{ops:?}");
    }

    #[test]
    fn patch_for_a_new_file_is_one_hunk_of_additions() {
        let patch = unified_patch("src/a.rs", None, "x\ny\n");
        assert_eq!(
            patch,
            "--- /dev/null\n+++ b/src/a.rs\n@@ -0,0 +1,2 @@\n+x\n+y\n"
        );
    }

    #[test]
    fn patch_has_context_and_splits_distant_changes_into_hunks() {
        let old: String = (1..=30).map(|i| format!("l{i}\n")).collect();
        let new = old
            .replacen("l2\n", "L2\n", 1)
            .replacen("l29\n", "L29\n", 1);
        let patch = unified_patch("f.txt", Some(&old), &new);
        let hunks: Vec<&str> = patch.lines().filter(|l| l.starts_with("@@")).collect();
        assert_eq!(
            hunks,
            vec!["@@ -1,5 +1,5 @@", "@@ -26,5 +26,5 @@"],
            "{patch}"
        );
        assert!(patch.contains("-l2\n+L2\n"));
        assert!(patch.contains("-l29\n+L29\n"));
    }

    #[test]
    fn patch_of_identical_texts_has_no_hunks() {
        let patch = unified_patch("f.txt", Some("a\nb\n"), "a\nb\n");
        assert!(!patch.contains("@@"), "{patch}");
    }

    #[test]
    fn reassemble_folds_snapshot_and_appends_in_order() {
        let envelopes = vec![
            diff_envelope(
                "act",
                ActivityMode::Snapshot,
                chunk("a.rs", "1\n", 0, false),
            ),
            diff_envelope("act", ActivityMode::Append, chunk("a.rs", "2\n", 1, false)),
            diff_envelope("act", ActivityMode::Append, chunk("a.rs", "3\n", 2, true)),
        ];
        let diffs = reassemble_diffs(&envelopes).expect("reassemble");
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].new_text, "1\n2\n3\n");
        assert_eq!(diffs[0].old_text, None);
        assert_eq!(diffs[0].chunks, 3);
        assert_eq!(diffs[0].path, "a.rs");
    }

    #[test]
    fn reassemble_rejects_a_gap_and_an_unfinished_diff() {
        let gap = vec![
            diff_envelope(
                "act",
                ActivityMode::Snapshot,
                chunk("a.rs", "1\n", 0, false),
            ),
            diff_envelope("act", ActivityMode::Append, chunk("a.rs", "3\n", 2, true)),
        ];
        assert!(reassemble_diffs(&gap).unwrap_err().contains("chunk 2"));

        let unfinished = vec![diff_envelope(
            "act",
            ActivityMode::Snapshot,
            chunk("a.rs", "1\n", 0, false),
        )];
        assert!(
            reassemble_diffs(&unfinished)
                .unwrap_err()
                .contains("before its last chunk")
        );
    }

    #[test]
    fn reassemble_rejects_append_before_snapshot() {
        let envelopes = vec![diff_envelope(
            "act",
            ActivityMode::Append,
            chunk("a.rs", "1\n", 1, true),
        )];
        assert!(
            reassemble_diffs(&envelopes)
                .unwrap_err()
                .contains("before any snapshot")
        );
    }
}
