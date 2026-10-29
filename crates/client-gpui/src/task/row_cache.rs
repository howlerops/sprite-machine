//! Incremental transcript rows.
//!
//! [`RowCache`] holds the flat row list of a thread and, per segment of it (the header, each
//! transcript item, each task's status line), the item it was built from and how many rows it
//! owns. [`RowCache::sync`] walks the thread, compares every item with the one its rows were built
//! from and rebuilds only the segments that differ, splicing the new rows in place. One streamed
//! event therefore rebuilds one item, not the thread. The result is always equal to
//! [`thread_rows_with_cards`](crate::task::transcript::thread_rows_with_cards); a test checks it.

use std::collections::BTreeSet;

use client_core::view::transcript::ActivityView;
use client_core::view::{Item, ThreadView};

use crate::approval::ApprovalCard;
use crate::task::tools::DiffStats;
use crate::task::transcript::{Row, header_rows, item_block, status_row};

/// What one update did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpdateStats {
    /// Transcript items looked at.
    pub items_visited: usize,
    /// Items whose rows were built again.
    pub items_rebuilt: usize,
    /// Rows produced by the segments (items, header, status lines) that were built again.
    pub rows_rebuilt: usize,
    /// `DiffModel`s built for diff summary counts.
    pub diff_builds: usize,
    /// Rows in the list afterwards.
    pub rows_total: usize,
}

/// What a segment of the row list is, independent of its position among the others.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Header,
    Item {
        task: usize,
        tag: &'static str,
        id: String,
        num: Option<u64>,
    },
    Status(usize),
}

/// The identity of an item, borrowed from it.
struct Ident<'a> {
    tag: &'static str,
    id: &'a str,
    /// A step's iteration, or the ordinal of an item that has no id (errors, raw events).
    num: Option<u64>,
}

fn ident(item: &Item, ordinal: usize) -> Ident<'_> {
    let ordinal = Some(ordinal as u64);
    let (tag, id, num) = match item {
        Item::User(v) => ("user", v.message_id.as_str(), None),
        Item::Text(v) => ("text", v.message_id.as_str(), None),
        Item::Reasoning(v) => ("reasoning", v.message_id.as_str(), None),
        Item::Tool(v) => ("tool", v.tool_call_id.as_str(), None),
        Item::Activity(v) => ("activity", v.activity_id.as_str(), None),
        Item::Interrupt(v) => ("interrupt", v.id(), None),
        Item::Checkpoint(v) => ("checkpoint", v.checkpoint_id.as_str(), None),
        Item::Step(v) => ("step", v.step_id.as_str(), v.iteration),
        Item::Subagent(v) => ("subagent", v.subagent_run_id.as_str(), None),
        Item::Error { .. } => ("error", "", ordinal),
        Item::Raw(_) => ("raw", "", ordinal),
    };
    Ident { tag, id, num }
}

impl Seg {
    fn is_item(&self, task: usize, ident: &Ident<'_>) -> bool {
        matches!(self, Seg::Item { task: t, tag, id, num }
            if *t == task && *tag == ident.tag && id == ident.id && *num == ident.num)
    }
}

#[derive(Debug)]
struct Entry {
    seg: Seg,
    /// The item the rows were built from (items only).
    item: Option<Item>,
    /// The approval card the rows were built with (interrupt items only).
    card: Option<ApprovalCard>,
    /// The item's number among the thread's items, and whether it was expanded, when the rows
    /// depend on them (`has_more`).
    key: usize,
    expanded: bool,
    has_more: bool,
    /// How many rows the segment owns.
    len: usize,
}

impl Entry {
    fn new(seg: Seg) -> Self {
        Self {
            seg,
            item: None,
            card: None,
            key: 0,
            expanded: false,
            has_more: false,
            len: 0,
        }
    }
}

/// The rows of a thread, kept up to date incrementally.
#[derive(Debug, Default)]
pub struct RowCache {
    rows: Vec<Row>,
    entries: Vec<Entry>,
    stats: DiffStats,
    last: UpdateStats,
}

impl RowCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// What the last [`sync`](Self::sync) or [`refresh`](Self::refresh) did.
    pub fn last_update(&self) -> UpdateStats {
        self.last
    }

    /// The activity `activity_id` of the thread last synced.
    pub fn activity(&self, activity_id: &str) -> Option<&ActivityView> {
        self.entries.iter().find_map(|entry| match &entry.item {
            Some(Item::Activity(activity)) if activity.activity_id == activity_id => Some(activity),
            _ => None,
        })
    }

    /// Brings the rows up to date with `thread`.
    pub fn sync(
        &mut self,
        thread: &ThreadView,
        expanded: &BTreeSet<usize>,
        cards: &[ApprovalCard],
    ) {
        self.begin();
        let (mut slot, mut pos, mut key) = (0, 0, 0);
        self.put_rows(&mut slot, &mut pos, Seg::Header, header_rows(thread));
        for (task_ix, task) in thread.tasks().iter().enumerate() {
            for (ordinal, item) in task.transcript().items().iter().enumerate() {
                let ident = ident(item, ordinal);
                if !self
                    .entries
                    .get(slot)
                    .is_some_and(|entry| entry.seg.is_item(task_ix, &ident))
                {
                    let seg = Seg::Item {
                        task: task_ix,
                        tag: ident.tag,
                        id: ident.id.to_owned(),
                        num: ident.num,
                    };
                    self.entries.insert(slot, Entry::new(seg));
                }
                self.last.items_visited += 1;
                let card = card_for(item, cards);
                let entry = &self.entries[slot];
                let is_expanded = expanded.contains(&key);
                let unchanged = entry.item.as_ref() == Some(item)
                    && entry.card.as_ref() == card
                    && (!entry.has_more || (entry.key == key && entry.expanded == is_expanded));
                if unchanged {
                    self.entries[slot].key = key;
                    self.entries[slot].expanded = is_expanded;
                } else {
                    self.rebuild_item(slot, pos, item, key, expanded, cards);
                    self.entries[slot].item = Some(item.clone());
                }
                pos += self.entries[slot].len;
                slot += 1;
                key += 1;
            }
            self.put_rows(
                &mut slot,
                &mut pos,
                Seg::Status(task_ix),
                vec![status_row(task.status())],
            );
        }
        self.truncate(slot, pos);
        self.end();
    }

    /// Applies a change of `expanded` or `cards` to the thread last synced, rebuilding only the
    /// items that it affects.
    pub fn refresh(&mut self, expanded: &BTreeSet<usize>, cards: &[ApprovalCard]) {
        self.begin();
        let (mut pos, mut key) = (0, 0);
        for slot in 0..self.entries.len() {
            if self.entries[slot].item.is_some() {
                self.last.items_visited += 1;
                let entry = &self.entries[slot];
                let is_expanded = expanded.contains(&key);
                let card = entry.item.as_ref().and_then(|item| card_for(item, cards));
                let unchanged = entry.card.as_ref() == card
                    && (!entry.has_more || (entry.key == key && entry.expanded == is_expanded));
                if unchanged {
                    self.entries[slot].key = key;
                    self.entries[slot].expanded = is_expanded;
                } else if let Some(item) = self.entries[slot].item.take() {
                    self.rebuild_item(slot, pos, &item, key, expanded, cards);
                    self.entries[slot].item = Some(item);
                }
                key += 1;
            }
            pos += self.entries[slot].len;
        }
        self.end();
    }

    fn begin(&mut self) {
        self.last = UpdateStats {
            diff_builds: self.stats.builds(),
            ..UpdateStats::default()
        };
    }

    fn end(&mut self) {
        self.last.diff_builds = self.stats.builds() - self.last.diff_builds;
        self.last.rows_total = self.rows.len();
    }

    /// Makes the segment at `slot` (inserting it when the slot holds another one) own `new`.
    fn put_rows(&mut self, slot: &mut usize, pos: &mut usize, seg: Seg, new: Vec<Row>) {
        if self.entries.get(*slot).is_none_or(|entry| entry.seg != seg) {
            self.entries.insert(*slot, Entry::new(seg));
        }
        let len = self.entries[*slot].len;
        if self.rows[*pos..*pos + len] != new[..] {
            self.last.rows_rebuilt += new.len();
            self.entries[*slot].len = new.len();
            self.rows.splice(*pos..*pos + len, new);
        }
        *pos += self.entries[*slot].len;
        *slot += 1;
    }

    fn rebuild_item(
        &mut self,
        slot: usize,
        pos: usize,
        item: &Item,
        key: usize,
        expanded: &BTreeSet<usize>,
        cards: &[ApprovalCard],
    ) {
        let mut out = Vec::new();
        let has_more = item_block(item, key, expanded, cards, &mut self.stats, &mut out);
        let entry = &mut self.entries[slot];
        self.last.items_rebuilt += 1;
        self.last.rows_rebuilt += out.len();
        let old = entry.len;
        entry.len = out.len();
        entry.has_more = has_more;
        entry.key = key;
        entry.expanded = expanded.contains(&key);
        entry.card = card_for(item, cards).cloned();
        self.rows.splice(pos..pos + old, out);
    }

    /// Drops the segments from `slot` on (the thread has fewer items than before).
    fn truncate(&mut self, slot: usize, pos: usize) {
        self.entries.truncate(slot);
        self.rows.truncate(pos);
    }
}

fn card_for<'a>(item: &Item, cards: &'a [ApprovalCard]) -> Option<&'a ApprovalCard> {
    match item {
        Item::Interrupt(interrupt) => cards
            .iter()
            .find(|card| card.interrupt_id == interrupt.id()),
        _ => None,
    }
}
