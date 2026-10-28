//! Draws the [`App`] into a ratatui [`Buffer`]. Pure: the same state and area give the same cells,
//! so the tests compare the buffer's text with a snapshot.
//!
//! Layout, top to bottom: the header (title, task status, remote state), the session link while
//! shared, the transcript (newest at the bottom), the open approval, the notice, the input line.

use client_core::TaskStatus;
use client_core::view::{InterruptView, Item, LocalAnswer, ToolStatus};
use client_core::{Connectivity, ThreadView};
use protocol::tunnel::Actor;
use protocol::turn_event::{ActivityType, ResolvedOutcome};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget};
use unicode_width::UnicodeWidthChar;

use super::app::{App, Remote};

const DIM: Style = Style::new().fg(Color::DarkGray);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);

/// Draws `app` over all of `area`.
pub fn render(app: &App, area: Rect, buf: &mut Buffer) {
    Clear.render(area, buf);
    if area.width < 4 || area.height < 3 {
        return;
    }
    let width = usize::from(area.width);
    let mut top = area.y;
    let mut bottom = area.y + area.height;

    draw_line(
        header(app, width),
        Rect::new(area.x, top, area.width, 1),
        buf,
    );
    top += 1;
    if let Remote::On { url } = app.remote() {
        let line = Line::from(vec![Span::styled("link: ", DIM), Span::raw(url.clone())]);
        draw_line(line, Rect::new(area.x, top, area.width, 1), buf);
        top += 1;
    }

    // The input is the last line; the notice sits above it.
    bottom -= 1;
    draw_line(
        input_line(app, width),
        Rect::new(area.x, bottom, area.width, 1),
        buf,
    );
    if let Some(notice) = app.notice() {
        bottom -= 1;
        let line = Line::styled(clip(notice, width), Style::new().fg(Color::Yellow));
        draw_line(line, Rect::new(area.x, bottom, area.width, 1), buf);
    }
    // Prompts typed while a task runs, oldest first, above the notice; they run after it.
    let pending: Vec<&str> = app.queued().map(|queued| queued.text.as_str()).collect();
    let room = usize::from(bottom.saturating_sub(top)).saturating_sub(2);
    let shown = &pending[pending.len().saturating_sub(room)..];
    let rows = u16::try_from(shown.len()).unwrap_or(u16::MAX);
    bottom -= rows;
    for (row, text) in shown.iter().enumerate() {
        let line = Line::styled(clip(&format!("pending: {text}"), width), DIM);
        let y = bottom + u16::try_from(row).unwrap_or(u16::MAX);
        draw_line(line, Rect::new(area.x, y, area.width, 1), buf);
    }

    // The approval panel takes what it needs, but leaves the transcript one line.
    if let Some(approval) = app.pending_approval() {
        let room = bottom.saturating_sub(top).saturating_sub(1);
        let lines = approval_lines(approval, app.selected(), width.saturating_sub(2));
        let height = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX).min(room);
        if height >= 3 {
            bottom -= height;
            let panel = Rect::new(area.x, bottom, area.width, height);
            let block = Block::bordered()
                .title(" Approval ")
                .border_style(Style::new().fg(Color::Yellow));
            let inner = block.inner(panel);
            block.render(panel, buf);
            Paragraph::new(lines).render(inner, buf);
        }
    }

    let body = Rect::new(area.x, top, area.width, bottom.saturating_sub(top));
    draw_transcript(app, body, buf);
}

fn draw_line(line: Line<'_>, area: Rect, buf: &mut Buffer) {
    Paragraph::new(line).render(area, buf);
}

fn header(app: &App, width: usize) -> Line<'static> {
    let view = app.view();
    let title = view.title().unwrap_or_else(|| view.thread_id()).to_owned();
    let work = match app.status() {
        Some(TaskStatus::Starting | TaskStatus::Running) => "working",
        Some(TaskStatus::NeedsYou) => "waiting for you",
        _ => "idle",
    };
    let remote = match app.remote() {
        Remote::Off => "remote: off".to_owned(),
        Remote::Connecting => "remote: connecting".to_owned(),
        Remote::On { .. } => match app.connectivity() {
            Connectivity::Online => "remote: on".to_owned(),
            Connectivity::Connecting => "remote: on (connecting)".to_owned(),
            Connectivity::Offline => "remote: on (offline)".to_owned(),
        },
        Remote::Failed { .. } => "remote: failed".to_owned(),
    };
    let left = format!(" sm  {title}  [{work}]");
    let right = format!("{remote} ");
    let left = clip(&left, width.saturating_sub(text_width(&right) + 1));
    let gap = width.saturating_sub(text_width(&left) + text_width(&right));
    Line::from(vec![
        Span::styled(left, BOLD),
        Span::raw(" ".repeat(gap)),
        Span::styled(right, DIM),
    ])
    .style(Style::new().add_modifier(Modifier::REVERSED))
}

fn input_line(app: &App, width: usize) -> Line<'static> {
    let prompt = "> ";
    let room = width.saturating_sub(prompt.len() + 1);
    // The tail of a long input stays visible, where the caret is.
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0;
    for c in app.input().chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > room {
            break;
        }
        used += w;
        tail.push(c);
    }
    let tail: String = tail.into_iter().rev().collect();
    Line::from(vec![
        Span::styled(prompt, BOLD),
        Span::raw(tail),
        Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)),
    ])
}

fn approval_lines(approval: &InterruptView, selected: usize, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    wrap_into(&mut lines, "", &approval.interrupt.title, width, BOLD);
    if let Some(message) = &approval.interrupt.message {
        wrap_into(&mut lines, "", message, width, Style::new());
    }
    if let Some(LocalAnswer::Failed { message }) = &approval.local {
        wrap_into(
            &mut lines,
            "",
            &format!("your answer was not sent: {message}"),
            width,
            Style::new().fg(Color::Red),
        );
    }
    for (index, option) in approval.options().iter().enumerate() {
        let marker = if index == selected { ">" } else { " " };
        let style = if index == selected {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new()
        };
        lines.push(Line::styled(
            clip(&format!("{marker} {}", option.name), width),
            style,
        ));
    }
    lines.push(Line::styled(
        clip(
            "up/down choose, enter answers (any device may answer)",
            width,
        ),
        DIM,
    ));
    lines
}

fn draw_transcript(app: &App, area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    let lines = transcript_lines(app.view(), usize::from(area.width));
    let height = usize::from(area.height);
    let newest = lines.len().saturating_sub(app.scroll().min(lines.len()));
    let first = newest.saturating_sub(height);
    let shown: Vec<Line<'static>> = lines[first..newest].to_vec();
    // A short transcript starts at the top; a full one ends at the bottom.
    Paragraph::new(shown).render(area, buf);
}

/// The transcript as wrapped lines, tasks and items in the order the hub gave them.
pub fn transcript_lines(view: &ThreadView, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for task in view.tasks() {
        for item in task.transcript().items() {
            item_lines(item, width, &mut lines);
        }
    }
    lines
}

fn item_lines(item: &Item, width: usize, lines: &mut Vec<Line<'static>>) {
    match item {
        Item::User(user) => {
            let name = user.author.name.as_deref().unwrap_or(&user.author.id);
            let prefix = format!("{name}> ");
            wrap_into(
                lines,
                &prefix,
                &user.text,
                width,
                Style::new().fg(Color::Cyan),
            );
        }
        Item::Text(text) => wrap_into(lines, "agent> ", &text.text, width, Style::new()),
        Item::Reasoning(text) => wrap_into(
            lines,
            "  thinking: ",
            &text.text,
            width,
            DIM.add_modifier(Modifier::ITALIC),
        ),
        Item::Tool(tool) => {
            let title = tool.title.as_deref().unwrap_or(&tool.tool_call_id);
            let status = match tool.status {
                ToolStatus::Pending => "pending",
                ToolStatus::InProgress => "running",
                ToolStatus::Completed => "done",
                ToolStatus::Failed => "failed",
            };
            wrap_into(
                lines,
                "  [tool] ",
                &format!("{title} ({status})"),
                width,
                DIM,
            );
        }
        Item::Activity(activity) => {
            let (kind, detail) = match activity.activity_type {
                ActivityType::Diff => ("diff", activity.diff().map(|d| d.path)),
                ActivityType::Terminal => ("terminal", activity.terminal().and_then(|t| t.command)),
                ActivityType::Plan => ("plan", None),
                ActivityType::Policy => ("policy", None),
            };
            let detail = detail.unwrap_or_else(|| activity.activity_id.clone());
            wrap_into(lines, &format!("  [{kind}] "), &detail, width, DIM);
        }
        Item::Interrupt(interrupt) => interrupt_lines(interrupt, width, lines),
        Item::Checkpoint(checkpoint) => wrap_into(
            lines,
            "  [checkpoint] ",
            checkpoint
                .label
                .as_deref()
                .unwrap_or(&checkpoint.checkpoint_id),
            width,
            DIM,
        ),
        Item::Error { message, .. } => {
            wrap_into(lines, "! ", message, width, Style::new().fg(Color::Red))
        }
    }
}

fn interrupt_lines(view: &InterruptView, width: usize, lines: &mut Vec<Line<'static>>) {
    let yellow = Style::new().fg(Color::Yellow);
    wrap_into(lines, "? ", &view.interrupt.title, width, yellow);
    let state = if let Some(resolution) = &view.resolution {
        let by = actor_name(&resolution.resolved_by);
        match &resolution.outcome {
            ResolvedOutcome::Selected { option_id } => {
                let name = view
                    .options()
                    .iter()
                    .find(|option| &option.option_id == option_id)
                    .map_or(option_id.as_str(), |option| option.name.as_str());
                format!("answered: {name} (by {by})")
            }
            ResolvedOutcome::Submitted { .. } => format!("answered: submitted (by {by})"),
            ResolvedOutcome::Declined => format!("answered: declined (by {by})"),
            ResolvedOutcome::Cancelled => "cancelled".to_owned(),
            ResolvedOutcome::Expired => "expired".to_owned(),
        }
    } else if let Some(winner) = &view.conflict {
        format!("already answered by {}", actor_name(winner))
    } else {
        match &view.local {
            Some(LocalAnswer::Pending) => "sending your answer...".to_owned(),
            Some(LocalAnswer::Failed { message }) => {
                format!("your answer was not sent: {message}")
            }
            None => "waiting for an answer, here or on another device".to_owned(),
        }
    };
    wrap_into(lines, "    ", &state, width, DIM);
}

fn actor_name(actor: &Actor) -> &str {
    actor.name.as_deref().unwrap_or(&actor.id)
}

// ---- text helpers ---------------------------------------------------------------------------

fn text_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// `text` cut to at most `width` cells.
fn clip(text: &str, width: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        used += w;
        out.push(c);
    }
    out
}

/// Appends `text` word-wrapped to `width`: the first line starts with `prefix`, the others are
/// indented by its width.
fn wrap_into(lines: &mut Vec<Line<'static>>, prefix: &str, text: &str, width: usize, style: Style) {
    let indent = text_width(prefix);
    // A very narrow terminal still gets one cell of text per line.
    let room = width.saturating_sub(indent).max(1);
    let mut first = true;
    let mut push = |row: String| {
        let lead = if first {
            prefix.to_owned()
        } else {
            " ".repeat(indent)
        };
        first = false;
        lines.push(Line::from(vec![Span::styled(lead + &row, style)]));
    };
    for paragraph in text.split('\n') {
        let mut row = String::new();
        let mut used = 0;
        for word in paragraph.split(' ') {
            let w = text_width(word);
            let sep = usize::from(!row.is_empty());
            if used + sep + w <= room {
                if sep == 1 {
                    row.push(' ');
                }
                row.push_str(word);
                used += sep + w;
                continue;
            }
            if !row.is_empty() {
                push(std::mem::take(&mut row));
            }
            // A word longer than a line is cut where the line ends.
            let mut rest = word;
            while text_width(rest) > room {
                let head = clip(rest, room);
                if head.is_empty() {
                    break;
                }
                rest = &rest[head.len()..];
                push(head);
            }
            row.push_str(rest);
            used = text_width(rest);
        }
        push(row);
    }
}
