//! Rows for the transcript items that are not plain messages: tool calls, activities (diff,
//! plan, terminal, policy), interrupts and checkpoints. Pure functions from `client-core` view
//! models to [`Row`]s, so they are tested without a window.

use client_core::view::transcript::{ActivityView, InterruptView, ToolStatus, ToolView};
use protocol::tunnel::ContentPart;
use protocol::turn_event::{
    ActivityType, Checkpoint, PlanStatus, PolicyDecision, ResolvedOutcome, ToolKind,
};

use crate::shell::i18n::{tr, tr_args};
use crate::task::transcript::{
    MAX_BODY_LINES, MAX_DIFF_LINES, MAX_TERMINAL_LINES, Row, RowKind, Tone, push_body, push_more,
    sanitize, wrap_lines,
};
use client_core::view::transcript::LocalAnswer;

fn kind_word(kind: Option<ToolKind>) -> String {
    let id = match kind.unwrap_or(ToolKind::Other) {
        ToolKind::Read => "tool-kind-read",
        ToolKind::Edit => "tool-kind-edit",
        ToolKind::Delete => "tool-kind-delete",
        ToolKind::Move => "tool-kind-move",
        ToolKind::Search => "tool-kind-search",
        ToolKind::Execute => "tool-kind-execute",
        ToolKind::Think => "tool-kind-think",
        ToolKind::Fetch => "tool-kind-fetch",
        ToolKind::SwitchMode => "tool-kind-switch-mode",
        ToolKind::Other => "tool-kind-other",
    };
    tr(id)
}

fn status_word(status: ToolStatus) -> (String, Tone) {
    match status {
        ToolStatus::Pending => (tr("tool-status-pending"), Tone::Muted),
        ToolStatus::InProgress => (tr("tool-status-in-progress"), Tone::Muted),
        ToolStatus::Completed => (tr("tool-status-completed"), Tone::Success),
        ToolStatus::Failed => (tr("tool-status-failed"), Tone::Danger),
    }
}

/// A tool call: a title row (`Edit src/main.rs (edit)`), its status, locations, input, output.
pub fn tool_rows(tool: &ToolView, out: &mut Vec<Row>) {
    let title = match &tool.title {
        Some(title) => sanitize(title),
        None => tr_args(
            "tool-untitled",
            &[("detail", &sanitize(&tool.tool_call_id))],
        ),
    };
    let (status, tone) = status_word(tool.status);
    let head = tr_args(
        "tool-title",
        &[("detail", &title), ("choice", &kind_word(tool.kind))],
    );
    out.push(Row::new(
        RowKind::ToolTitle,
        tone,
        1,
        format!("{head} - {status}"),
    ));
    for location in &tool.locations {
        let path = sanitize(&location.path);
        let text = match location.line {
            Some(line) => format!("{path}:{line}"),
            None => path,
        };
        out.push(Row::new(RowKind::ToolDetail, Tone::Muted, 2, text));
    }
    if let Some(input) = &tool.raw_input {
        out.push(Row::new(
            RowKind::ToolDetail,
            Tone::Muted,
            2,
            tr("tool-input"),
        ));
        let pretty = serde_json::to_string_pretty(input).unwrap_or_default();
        push_body(
            out,
            &pretty,
            MAX_BODY_LINES,
            RowKind::ToolDetail,
            Tone::Normal,
            3,
        );
    }
    let has_output = !tool.content.is_empty() || tool.raw_output.is_some();
    if has_output {
        out.push(Row::new(
            RowKind::ToolDetail,
            Tone::Muted,
            2,
            tr("tool-output"),
        ));
    }
    for part in &tool.content {
        match part {
            ContentPart::Text { text } => push_body(
                out,
                text,
                MAX_BODY_LINES,
                RowKind::ToolDetail,
                Tone::Normal,
                3,
            ),
            ContentPart::File { name, .. } => out.push(Row::new(
                RowKind::ToolDetail,
                Tone::Normal,
                3,
                tr_args("tool-file", &[("detail", &sanitize(name))]),
            )),
        }
    }
    if tool.content.is_empty()
        && let Some(raw) = &tool.raw_output
    {
        let pretty = serde_json::to_string_pretty(raw).unwrap_or_default();
        push_body(
            out,
            &pretty,
            MAX_BODY_LINES,
            RowKind::ToolDetail,
            Tone::Normal,
            3,
        );
    }
}

/// A diff, plan, terminal or policy activity.
pub fn activity_rows(activity: &ActivityView, out: &mut Vec<Row>) {
    match activity.activity_type {
        ActivityType::Diff => match activity.diff() {
            Some(diff) => {
                let id = if activity.complete {
                    "activity-diff"
                } else {
                    "activity-diff-partial"
                };
                out.push(Row::new(
                    RowKind::DiffTitle,
                    Tone::Normal,
                    1,
                    tr_args(id, &[("detail", &sanitize(&diff.path))]),
                ));
                diff_lines(diff.old_text.as_deref(), &diff.new_text, out);
            }
            None => generic(activity, out),
        },
        ActivityType::Plan => match activity.plan() {
            Some(plan) => {
                out.push(Row::new(
                    RowKind::Plan,
                    Tone::Normal,
                    1,
                    tr("activity-plan"),
                ));
                for entry in plan.entries {
                    let (id, tone) = match entry.status {
                        PlanStatus::Pending => ("activity-plan-todo", Tone::Normal),
                        PlanStatus::InProgress => ("activity-plan-doing", Tone::Warning),
                        PlanStatus::Completed => ("activity-plan-done", Tone::Success),
                    };
                    out.push(Row::new(
                        RowKind::Plan,
                        tone,
                        2,
                        tr_args(id, &[("detail", &sanitize(&entry.content))]),
                    ));
                }
            }
            None => generic(activity, out),
        },
        ActivityType::Terminal => match activity.terminal() {
            Some(terminal) => {
                let title = match &terminal.command {
                    Some(command) => tr_args(
                        "activity-terminal-command",
                        &[("detail", &sanitize(command))],
                    ),
                    None => tr_args(
                        "activity-terminal",
                        &[("detail", &sanitize(&terminal.terminal_id))],
                    ),
                };
                out.push(Row::new(RowKind::Terminal, Tone::Normal, 1, title));
                if let Some(output) = &terminal.output {
                    let lines = wrap_lines(output.trim_end_matches('\n'), 110);
                    let hidden = lines.len().saturating_sub(MAX_TERMINAL_LINES);
                    if hidden > 0 {
                        push_more(out, hidden, 2);
                    }
                    for line in lines.into_iter().skip(hidden) {
                        out.push(Row::new(RowKind::Terminal, Tone::Normal, 2, line));
                    }
                }
                let status = match &terminal.exit {
                    None => (tr("activity-terminal-running"), Tone::Muted),
                    Some(exit) => match (&exit.exit_code, &exit.signal) {
                        (Some(0), _) => (
                            tr_args("activity-terminal-exit-code", &[("detail", "0")]),
                            Tone::Success,
                        ),
                        (Some(code), _) => (
                            tr_args(
                                "activity-terminal-exit-code",
                                &[("detail", &code.to_string())],
                            ),
                            Tone::Danger,
                        ),
                        (None, Some(signal)) => (
                            tr_args(
                                "activity-terminal-exit-signal",
                                &[("detail", &sanitize(signal))],
                            ),
                            Tone::Danger,
                        ),
                        (None, None) => (tr("activity-terminal-running"), Tone::Muted),
                    },
                };
                out.push(Row::new(RowKind::Terminal, status.1, 2, status.0));
            }
            None => generic(activity, out),
        },
        ActivityType::Policy => match activity.policy() {
            Some(policy) => {
                let (id, tone) = match policy.decision {
                    PolicyDecision::Allow => ("activity-policy-allow", Tone::Success),
                    PolicyDecision::Deny => ("activity-policy-deny", Tone::Danger),
                };
                out.push(Row::new(
                    RowKind::Policy,
                    tone,
                    1,
                    tr_args(id, &[("detail", &sanitize(&policy.rule))]),
                ));
            }
            None => generic(activity, out),
        },
    }
}

fn generic(activity: &ActivityView, out: &mut Vec<Row>) {
    out.push(Row::new(
        RowKind::Policy,
        Tone::Muted,
        1,
        tr_args(
            "activity-generic",
            &[("detail", &sanitize(&activity.activity_id))],
        ),
    ));
}

/// A minimal diff: lines shared at the start and end are dropped, the rest is shown as removed
/// (`-`) and added (`+`). The full side-by-side diff is the diff view's job.
fn diff_lines(old: Option<&str>, new: &str, out: &mut Vec<Row>) {
    let old_lines: Vec<&str> = old.map(|old| old.lines().collect()).unwrap_or_default();
    let new_lines: Vec<&str> = new.lines().collect();
    let prefix = old_lines
        .iter()
        .zip(&new_lines)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = old_lines[prefix..]
        .iter()
        .rev()
        .zip(new_lines[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let removed = &old_lines[prefix..old_lines.len() - suffix];
    let added = &new_lines[prefix..new_lines.len() - suffix];
    let total = removed.len() + added.len();
    let signed = removed
        .iter()
        .map(|line| ('-', *line, Tone::Danger))
        .chain(added.iter().map(|line| ('+', *line, Tone::Success)));
    for (sign, line, tone) in signed.take(MAX_DIFF_LINES) {
        let mut text = format!("{sign} {}", sanitize(line));
        text.truncate(text.floor_char_boundary(crate::task::transcript::WRAP_COLS));
        out.push(Row::new(RowKind::DiffLine, tone, 2, text));
    }
    if total > MAX_DIFF_LINES {
        push_more(out, total - MAX_DIFF_LINES, 2);
    }
}

/// An interrupt (approval, form, auth): title, message, options, expiry, then its resolution.
pub fn interrupt_rows(view: &InterruptView, out: &mut Vec<Row>) {
    let interrupt = &view.interrupt;
    let id = match interrupt.kind {
        protocol::turn_event::InterruptKind::Approval => "interrupt-approval",
        protocol::turn_event::InterruptKind::Form => "interrupt-form",
        protocol::turn_event::InterruptKind::Auth => "interrupt-auth",
    };
    out.push(Row::new(
        RowKind::Interrupt,
        if view.is_open() {
            Tone::Warning
        } else {
            Tone::Muted
        },
        1,
        tr_args(id, &[("detail", &sanitize(&interrupt.title))]),
    ));
    if let Some(message) = &interrupt.message {
        push_body(
            out,
            message,
            MAX_BODY_LINES,
            RowKind::Interrupt,
            Tone::Normal,
            2,
        );
    }
    if !view.options().is_empty() {
        let names: Vec<String> = view.options().iter().map(|o| sanitize(&o.name)).collect();
        out.push(Row::new(
            RowKind::Interrupt,
            Tone::Muted,
            2,
            tr_args("interrupt-options", &[("detail", &names.join(" / "))]),
        ));
    }
    if view.is_open() {
        out.push(Row::new(
            RowKind::Interrupt,
            Tone::Muted,
            2,
            tr_args(
                "interrupt-expires",
                &[("detail", &sanitize(&interrupt.expires_at))],
            ),
        ));
    }
    if let Some(resolution) = &view.resolution {
        let who = sanitize(
            resolution
                .resolved_by
                .name
                .as_deref()
                .unwrap_or(&resolution.resolved_by.id),
        );
        let text = match &resolution.outcome {
            ResolvedOutcome::Selected { option_id } => {
                let name = view
                    .options()
                    .iter()
                    .find(|option| &option.option_id == option_id)
                    .map_or(option_id.as_str(), |option| option.name.as_str());
                tr_args(
                    "interrupt-resolved-selected",
                    &[("user", &who), ("choice", &sanitize(name))],
                )
            }
            ResolvedOutcome::Submitted { .. } => {
                tr_args("interrupt-resolved-submitted", &[("user", &who)])
            }
            ResolvedOutcome::Cancelled => {
                tr_args("interrupt-resolved-cancelled", &[("user", &who)])
            }
            ResolvedOutcome::Expired => tr("interrupt-resolved-expired"),
            ResolvedOutcome::Declined => tr_args("interrupt-resolved-declined", &[("user", &who)]),
        };
        out.push(Row::new(RowKind::Interrupt, Tone::Success, 2, text));
    } else if let Some(winner) = &view.conflict {
        let who = sanitize(winner.name.as_deref().unwrap_or(&winner.id));
        out.push(Row::new(
            RowKind::Interrupt,
            Tone::Warning,
            2,
            tr_args("interrupt-conflict", &[("user", &who)]),
        ));
    }
    match &view.local {
        Some(LocalAnswer::Pending) => out.push(Row::new(
            RowKind::Interrupt,
            Tone::Muted,
            2,
            tr("interrupt-local-pending"),
        )),
        Some(LocalAnswer::Failed { message }) => out.push(Row::new(
            RowKind::Interrupt,
            Tone::Danger,
            2,
            tr_args("interrupt-local-failed", &[("detail", &sanitize(message))]),
        )),
        None => {}
    }
}

/// A checkpoint marker.
pub fn checkpoint_rows(checkpoint: &Checkpoint, out: &mut Vec<Row>) {
    let name = sanitize(
        checkpoint
            .label
            .as_deref()
            .unwrap_or(&checkpoint.checkpoint_id),
    );
    let text = match checkpoint.files_changed {
        Some(files) => tr_args(
            "transcript-checkpoint-files",
            &[("detail", &name), ("choice", &files.to_string())],
        ),
        None => tr_args("transcript-checkpoint", &[("detail", &name)]),
    };
    out.push(Row::new(RowKind::Checkpoint, Tone::Muted, 0, text));
}
