//! The palette's decisions without GPUI: the command list, fuzzy filtering and ranking, and the
//! selection (which wraps). Also the thread order that "next / previous thread" walks.

use crate::inbox::{InboxModel, Item};
use crate::shell::nav::Section;

use super::{ShortcutId, tr, tr_args};

/// What a command does when it runs. The shell executes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandId {
    /// Show a page (Inbox, Agents, ...).
    Go(Section),
    /// Start a new task: show the composer and focus its prompt.
    NewTask,
    /// Open a thread from the inbox by id.
    OpenThread(String),
    NextThread,
    PreviousThread,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub id: CommandId,
    /// The visible, localized title; the text the query is matched against.
    pub title: String,
    /// Extra words that also match (the page's slug, the thread's id).
    pub keywords: String,
    /// The shortcut shown next to the command, if it has one.
    pub shortcut: Option<ShortcutId>,
}

/// A thread of the inbox, as the palette and the thread shortcuts see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRef {
    pub thread_id: String,
    pub title: String,
    pub workspace: Option<String>,
}

/// The threads in the order the inbox shows them (sections, then rows). Archived threads that
/// the inbox is hiding are not in its model, so they are not here.
pub fn thread_order(inbox: &InboxModel) -> Vec<ThreadRef> {
    inbox
        .items()
        .into_iter()
        .filter_map(|item| match item {
            Item::Row(ix) => inbox.rows().get(ix),
            Item::Header { .. } => None,
        })
        .map(|row| ThreadRef {
            thread_id: row.thread_id.clone(),
            title: row.title.clone(),
            workspace: row.workspace.clone(),
        })
        .collect()
}

/// Index in `ids` of the thread `delta` steps (1 next, -1 previous) from `current`. Stops at the
/// ends instead of wrapping. With no current thread (or one not in the list) next is the first
/// and previous the last. `None` for an empty list or when already at the end.
pub fn adjacent(ids: &[&str], current: Option<&str>, delta: isize) -> Option<usize> {
    if ids.is_empty() {
        return None;
    }
    let position = current.and_then(|current| ids.iter().position(|id| *id == current));
    match position {
        None if delta >= 0 => Some(0),
        None => Some(ids.len() - 1),
        Some(at) => {
            let to = at as isize + delta;
            (0..ids.len() as isize)
                .contains(&to)
                .then_some(to as usize)
                .filter(|to| *to != at)
        }
    }
}

/// The commands the palette offers: every page, new task, the thread shortcuts, then one
/// "open thread" command per inbox thread (in inbox order).
pub fn build_commands(threads: &[ThreadRef]) -> Vec<Command> {
    let mut commands = vec![Command {
        id: CommandId::NewTask,
        title: tr("palette-new-task"),
        keywords: Section::NewTask.slug().to_owned(),
        shortcut: Some(ShortcutId::NewTask),
    }];
    commands.extend(
        Section::ALL
            .iter()
            .copied()
            .filter(|section| *section != Section::NewTask)
            .map(|section| Command {
                id: CommandId::Go(section),
                title: tr_args("palette-go-to", &[("section", &section.title())]),
                keywords: section.slug().to_owned(),
                shortcut: None,
            }),
    );
    commands.push(Command {
        id: CommandId::NextThread,
        title: tr("palette-next-thread"),
        keywords: String::new(),
        shortcut: Some(ShortcutId::NextThread),
    });
    commands.push(Command {
        id: CommandId::PreviousThread,
        title: tr("palette-previous-thread"),
        keywords: String::new(),
        shortcut: Some(ShortcutId::PreviousThread),
    });
    commands.extend(threads.iter().map(|thread| {
        let title = if thread.title.trim().is_empty() {
            tr("palette-untitled-thread")
        } else {
            thread.title.clone()
        };
        Command {
            id: CommandId::OpenThread(thread.thread_id.clone()),
            title: tr_args("palette-open-thread", &[("title", &title)]),
            keywords: format!("{title} {}", thread.thread_id),
            shortcut: None,
        }
    }));
    commands
}

/// How well `query` matches `text`: `None` when the query's characters do not appear in order
/// (case-insensitive). Higher is better. Rewards a contiguous match, a match at the start, and
/// matches at word starts; penalises gaps. An empty query matches everything with score 0.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i64> {
    let query: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if query.is_empty() {
        return Some(0);
    }
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let word_start = |at: usize| at == 0 || !text[at - 1].is_alphanumeric();

    // A contiguous occurrence beats any scattered one.
    let contiguous = text
        .windows(query.len())
        .position(|window| window == query.as_slice());
    if let Some(at) = contiguous {
        let mut score = 100 + 10 * query.len() as i64 - at as i64;
        if at == 0 {
            score += 40;
        } else if word_start(at) {
            score += 20;
        }
        return Some(score);
    }

    let mut score = 0i64;
    let mut next = 0usize;
    let mut previous: Option<usize> = None;
    for wanted in &query {
        let at = (next..text.len()).find(|at| text[*at] == *wanted)?;
        score += 10;
        if word_start(at) {
            score += 8;
        }
        match previous {
            Some(last) if at == last + 1 => score += 12,
            Some(last) => score -= (at - last - 1).min(10) as i64,
            None => score -= at.min(10) as i64,
        }
        previous = Some(at);
        next = at + 1;
    }
    Some(score)
}

/// The command list, the query and the selection.
#[derive(Debug, Clone, Default)]
pub struct PaletteModel {
    commands: Vec<Command>,
    query: String,
    /// Indices into `commands`, best first.
    matches: Vec<usize>,
    selected: usize,
}

impl PaletteModel {
    pub fn new(commands: Vec<Command>) -> Self {
        let mut model = Self {
            commands,
            ..Self::default()
        };
        model.refilter();
        model
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Change the query: refilters, ranks, and selects the best match.
    pub fn set_query(&mut self, query: &str) {
        if query == self.query {
            return;
        }
        self.query = query.to_owned();
        self.refilter();
    }

    fn refilter(&mut self) {
        let query = self.query.trim();
        let mut scored: Vec<(usize, i64)> = self
            .commands
            .iter()
            .enumerate()
            .filter_map(|(ix, command)| {
                let title = fuzzy_score(query, &command.title);
                let keywords = fuzzy_score(query, &command.keywords).map(|score| score - 5);
                title.max(keywords).map(|score| (ix, score))
            })
            .collect();
        // Best first; equal scores keep the command order (stable sort).
        scored.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
        self.matches = scored.into_iter().map(|(ix, _)| ix).collect();
        self.selected = 0;
    }

    /// The matching commands, best first.
    pub fn matches(&self) -> Vec<&Command> {
        self.matches.iter().map(|ix| &self.commands[*ix]).collect()
    }

    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn selected(&self) -> Option<&Command> {
        self.matches
            .get(self.selected)
            .map(|ix| &self.commands[*ix])
    }

    /// Select `index` among the matches; out of range changes nothing.
    pub fn select(&mut self, index: usize) {
        if index < self.matches.len() {
            self.selected = index;
        }
    }

    /// Down: the next match, wrapping from the last to the first.
    pub fn select_next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    /// Up: the previous match, wrapping from the first to the last.
    pub fn select_previous(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + self.matches.len() - 1) % self.matches.len();
        }
    }
}
