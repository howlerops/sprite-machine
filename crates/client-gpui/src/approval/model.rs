//! The approval card as plain data: no GPUI, no I/O. Built from a `client-core`
//! [`InterruptView`] plus the transcript it sits in (for the pending diff or command) and the
//! current time from an injected clock, so every rule here is tested natively.

use client_core::view::{InterruptView, Item, LocalAnswer, Transcript};
use protocol::tunnel::{Actor, ActorKind};
use protocol::turn_event::{InterruptKind, PermissionOptionKind, ResolvedOutcome};

use super::{tr, tr_args};
use crate::task::transcript::{sanitize, wrap_lines};

/// Lines of a pending diff or command shown before "N more lines".
pub const MAX_PENDING_LINES: usize = 24;
/// Characters per wrapped line of the pending content.
pub const PENDING_COLS: usize = 100;

/// What the agent wants to do, taken from the tool call the approval is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// A file edit: `-` lines (old text) then `+` lines (new text).
    Diff {
        path: String,
        new_file: bool,
        lines: Vec<DiffLine>,
        hidden: usize,
        complete: bool,
    },
    /// A shell command.
    Command {
        command: String,
        cwd: Option<String>,
        hidden: usize,
    },
}

/// One line of a pending diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub sign: char,
    pub text: String,
}

/// One answer button: exactly one per ACP option, in the order the agent sent them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionButton {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
    /// Spoken label, e.g. `Always allow (allow always)`.
    pub a11y: String,
    /// `false` once the card can no longer be answered.
    pub enabled: bool,
}

/// The offer to save an `allow_always` option as the workspace's preset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetOffer {
    pub option_id: String,
    pub label: String,
    pub a11y: String,
    /// The user ticked the offer (or it was already saved).
    pub checked: bool,
    /// The preset was recorded for this interrupt.
    pub saved: bool,
}

/// Where the card stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardState {
    /// Waiting for an answer.
    Open,
    /// This device queued its answer; the hub has not confirmed it.
    Sending,
    /// The queue gave up on this device's answer.
    Failed { message: String },
    /// Time ran out before anyone answered (and the hub has not said so yet).
    Expired,
    /// The hub resolved it. `summary` reads "Allow by Jacob at 22:03:12 UTC".
    Resolved { summary: String, who: String },
    /// Another device answered first (HTTP 409).
    AlreadyResolved { summary: String, who: String },
}

/// Everything the card shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalCard {
    pub interrupt_id: String,
    pub title: String,
    pub message: Option<String>,
    pub options: Vec<OptionButton>,
    pub pending: Vec<Pending>,
    pub preset: Option<PresetOffer>,
    pub expires_at_ms: Option<u64>,
    /// `None` once answered or when `expires_at` does not parse.
    pub remaining_ms: Option<u64>,
    pub remaining_text: Option<String>,
    /// What the card says. A refused click shows as [`CardState::Failed`] here, while the
    /// buttons follow the state the request is really in (see [`Self::is_answerable`]).
    pub state: CardState,
    pub answerable: bool,
}

impl ApprovalCard {
    /// `true` while the buttons work: the request is still open, whatever the card says.
    pub fn is_answerable(&self) -> bool {
        self.answerable
    }
}

/// Inputs that are not in the interrupt itself.
#[derive(Debug, Clone, Default)]
pub struct CardContext {
    /// Injected current time (ms since the epoch).
    pub now_ms: u64,
    /// When this client first saw the hub's resolution (ms), if it did.
    pub resolved_at_ms: Option<u64>,
    /// The "save as workspace preset" box is ticked.
    pub remember_preset: bool,
    /// The preset was recorded locally for this interrupt.
    pub preset_saved: bool,
    /// Why this device's last answer to the card was refused, already worded for the user.
    /// It replaces the card's status until the next answer succeeds or the card closes.
    pub refused: Option<String>,
}

/// Builds the card for an approval interrupt; `None` for form and auth interrupts.
pub fn build_card(
    view: &InterruptView,
    transcript: &Transcript,
    context: CardContext,
) -> Option<ApprovalCard> {
    if view.kind() != InterruptKind::Approval {
        return None;
    }
    let interrupt = &view.interrupt;
    let expires_at_ms = parse_rfc3339_ms(&interrupt.expires_at);
    let expired = expires_at_ms.is_some_and(|at| context.now_ms >= at);
    let remaining_ms = match (expires_at_ms, view.is_open()) {
        (Some(at), true) => Some(at.saturating_sub(context.now_ms)),
        _ => None,
    };
    let underlying = card_state(view, expired, &context);
    let answerable = underlying == CardState::Open;
    let state = match &context.refused {
        Some(message) => CardState::Failed {
            message: sanitize(message),
        },
        None => underlying,
    };
    let options: Vec<OptionButton> = view
        .options()
        .iter()
        .map(|option| OptionButton {
            option_id: option.option_id.clone(),
            name: sanitize(&option.name),
            kind: option.kind,
            a11y: tr_args(
                "approval-option-a11y",
                &[
                    ("name", &sanitize(&option.name)),
                    ("kind", &tr(kind_id(option.kind))),
                ],
            ),
            enabled: answerable,
        })
        .collect();
    let preset = options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::AllowAlways)
        .map(|option| PresetOffer {
            option_id: option.option_id.clone(),
            label: tr_args("approval-preset-offer", &[("name", &option.name)]),
            a11y: tr_args("approval-preset-offer-a11y", &[("name", &option.name)]),
            checked: context.remember_preset || context.preset_saved,
            saved: context.preset_saved,
        });
    Some(ApprovalCard {
        interrupt_id: interrupt.interrupt_id.clone(),
        title: sanitize(&interrupt.title),
        message: interrupt.message.as_deref().map(sanitize),
        options,
        pending: interrupt
            .tool_call_id
            .as_deref()
            .map(|id| pending_for(transcript, id))
            .unwrap_or_default(),
        preset,
        expires_at_ms,
        remaining_ms,
        remaining_text: remaining_ms.map(format_remaining),
        state,
        answerable,
    })
}

fn card_state(view: &InterruptView, expired: bool, context: &CardContext) -> CardState {
    if let Some(resolution) = &view.resolution {
        let who = actor_text(&resolution.resolved_by);
        let choice = match &resolution.outcome {
            ResolvedOutcome::Selected { option_id } => view
                .options()
                .iter()
                .find(|option| &option.option_id == option_id)
                .map_or_else(|| sanitize(option_id), |option| sanitize(&option.name)),
            ResolvedOutcome::Submitted { .. } => tr("approval-resolved-submitted"),
            ResolvedOutcome::Cancelled => tr("approval-resolved-cancelled"),
            ResolvedOutcome::Declined => tr("approval-resolved-declined"),
            ResolvedOutcome::Expired => tr("approval-resolved-expired"),
        };
        return CardState::Resolved {
            summary: with_time(
                "approval-resolved",
                "approval-resolved-no-time",
                &[("choice", &choice), ("user", &who)],
                context.resolved_at_ms,
            ),
            who,
        };
    }
    if let Some(winner) = &view.conflict {
        let who = actor_text(winner);
        return CardState::AlreadyResolved {
            summary: with_time(
                "approval-conflict",
                "approval-conflict-no-time",
                &[("user", &who)],
                context.resolved_at_ms,
            ),
            who,
        };
    }
    match &view.local {
        Some(LocalAnswer::Pending) => CardState::Sending,
        Some(LocalAnswer::Failed { message }) => CardState::Failed {
            message: sanitize(message),
        },
        None if expired => CardState::Expired,
        None => CardState::Open,
    }
}

fn with_time(with: &str, without: &str, args: &[(&str, &str)], at_ms: Option<u64>) -> String {
    match at_ms {
        Some(at) => {
            let time = format_clock(at);
            let mut all = args.to_vec();
            all.push(("time", &time));
            tr_args(with, &all)
        }
        None => tr_args(without, args),
    }
}

/// Who, as text: the name (else the id), plus the device when the hub sent one.
pub fn actor_text(actor: &Actor) -> String {
    match actor.kind {
        ActorKind::System => tr("approval-actor-system"),
        ActorKind::Policy => tr("approval-actor-policy"),
        ActorKind::User | ActorKind::Agent => {
            let user = sanitize(actor.name.as_deref().unwrap_or(&actor.id));
            match &actor.device_id {
                Some(device) => tr_args(
                    "approval-actor-device",
                    &[("user", &user), ("device", &sanitize(device))],
                ),
                None => user,
            }
        }
    }
}

fn kind_id(kind: PermissionOptionKind) -> &'static str {
    match kind {
        PermissionOptionKind::AllowOnce => "approval-kind-allow_once",
        PermissionOptionKind::AllowAlways => "approval-kind-allow_always",
        PermissionOptionKind::RejectOnce => "approval-kind-reject_once",
        PermissionOptionKind::RejectAlways => "approval-kind-reject_always",
    }
}

/// The diffs and commands of the tool call `tool_call_id`, already in the log.
fn pending_for(transcript: &Transcript, tool_call_id: &str) -> Vec<Pending> {
    let mut out = Vec::new();
    let mut saw_command = false;
    for item in transcript.items() {
        let Item::Activity(activity) = item else {
            continue;
        };
        if let Some(diff) = activity.diff()
            && diff.tool_call_id.as_deref() == Some(tool_call_id)
        {
            out.push(diff_pending(&diff, activity.complete));
        } else if let Some(terminal) = activity.terminal()
            && terminal.tool_call_id.as_deref() == Some(tool_call_id)
            && let Some(command) = &terminal.command
        {
            saw_command = true;
            out.push(command_pending(command, terminal.cwd.as_deref()));
        }
    }
    if !saw_command
        && let Some(command) = transcript.items().iter().find_map(|item| match item {
            Item::Tool(tool) if tool.tool_call_id == tool_call_id => tool
                .raw_input
                .as_ref()
                .and_then(|input| input.get("command"))
                .and_then(|command| command.as_str()),
            _ => None,
        })
    {
        out.push(command_pending(command, None));
    }
    out
}

fn diff_pending(diff: &protocol::turn_event::DiffContent, complete: bool) -> Pending {
    let mut all = Vec::new();
    let sides = [
        ('-', diff.old_text.as_deref().unwrap_or("")),
        ('+', diff.new_text.as_str()),
    ];
    for (sign, text) in sides {
        // A final newline ends the last line; it does not start another.
        let text = text.strip_suffix('\n').unwrap_or(text);
        if text.is_empty() {
            continue;
        }
        for line in wrap_lines(text, PENDING_COLS) {
            all.push(DiffLine { sign, text: line });
        }
    }
    let hidden = all.len().saturating_sub(MAX_PENDING_LINES);
    all.truncate(MAX_PENDING_LINES);
    Pending::Diff {
        path: sanitize(&diff.path),
        new_file: diff.old_text.is_none(),
        lines: all,
        hidden,
        complete,
    }
}

fn command_pending(command: &str, cwd: Option<&str>) -> Pending {
    let lines = wrap_lines(command, PENDING_COLS);
    let hidden = lines.len().saturating_sub(MAX_PENDING_LINES);
    let shown: Vec<String> = lines.into_iter().take(MAX_PENDING_LINES).collect();
    Pending::Command {
        command: shown.join("\n"),
        cwd: cwd.map(sanitize),
        hidden,
    }
}

// ---- time ------------------------------------------------------------------------------------

/// "9:42 left" (or "1:00:00 left"), rounded up to the next second so "0:00" means over.
pub fn format_remaining(ms: u64) -> String {
    let total = ms.div_ceil(1000);
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    let detail = if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    };
    tr_args("approval-time-left", &[("detail", &detail)])
}

/// `22:03:12 UTC` for a time in ms since the epoch.
pub fn format_clock(ms: u64) -> String {
    let day_secs = ms / 1000 % 86_400;
    tr_args(
        "approval-time-format",
        &[
            ("hours", &format!("{:02}", day_secs / 3600)),
            ("minutes", &format!("{:02}", day_secs / 60 % 60)),
            ("seconds", &format!("{:02}", day_secs % 60)),
        ],
    )
}

/// Parses `2026-10-27T22:10:00.000Z` (or with a `+hh:mm` offset) to ms since the epoch.
pub fn parse_rfc3339_ms(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    let digits = |from: usize, len: usize| -> Option<i64> {
        let part = text.get(from..from + len)?;
        if part.bytes().all(|b| b.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let (year, month, day) = (digits(0, 4)?, digits(5, 2)?, digits(8, 2)?);
    let (hour, minute, second) = (digits(11, 2)?, digits(14, 2)?, digits(17, 2)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    let mut at = 19;
    let mut millis = 0;
    if bytes.get(at) == Some(&b'.') {
        let start = at + 1;
        let mut end = start;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        if end == start {
            return None;
        }
        let fraction = &text[start..end.min(start + 3)];
        let scale = 10_i64.pow(3 - u32::try_from(fraction.len()).ok()?);
        millis = fraction.parse::<i64>().ok()? * scale;
        at = end;
    }
    let offset_minutes = match bytes.get(at)? {
        b'Z' | b'z' if at + 1 == bytes.len() => 0,
        sign @ (b'+' | b'-') if at + 6 == bytes.len() && bytes[at + 3] == b':' => {
            let minutes = digits(at + 1, 2)? * 60 + digits(at + 4, 2)?;
            if *sign == b'+' { minutes } else { -minutes }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_minutes * 60;
    u64::try_from(secs * 1000 + millis).ok()
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}
