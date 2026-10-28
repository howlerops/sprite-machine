//! The approval card's view model: a pure function of an `Interrupt{approval}`, a clock and the
//! answers the sink returns. No GPUI here, so every rule is unit-tested natively.
//!
//! Rules: options are shown allow-first (then reject), each kind in the order the run gave them;
//! time left is computed from `expires_at`; the first accepted answer wins, so a later answer
//! (HTTP 409) shows who resolved it; an expired approval (HTTP 410, or the clock passing
//! `expires_at`) takes no further answers.

use protocol::turn_event::{
    Actor, Event, Interrupt, InterruptKind, InterruptOption, PermissionOptionKind,
};
use serde_json::{Value, json};

/// Thread id the `approval` fixture log is generated for.
pub const FIXTURE_THREAD: &str = "th_approval";

/// Visual weight of an option's button: allow kinds read as go, reject kinds as stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonTone {
    Allow,
    Reject,
}

pub fn tone_of(kind: PermissionOptionKind) -> ButtonTone {
    match kind {
        PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways => ButtonTone::Allow,
        PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways => ButtonTone::Reject,
    }
}

/// Display order: allow once, allow always, reject once, reject always.
fn kind_rank(kind: PermissionOptionKind) -> u8 {
    match kind {
        PermissionOptionKind::AllowOnce => 0,
        PermissionOptionKind::AllowAlways => 1,
        PermissionOptionKind::RejectOnce => 2,
        PermissionOptionKind::RejectAlways => 3,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionView {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
    pub tone: ButtonTone,
}

/// The options in display order (a stable sort by kind, so the run's own order is kept within
/// a kind).
pub fn ordered_options(options: &[InterruptOption]) -> Vec<OptionView> {
    let mut views: Vec<OptionView> = options
        .iter()
        .map(|option| OptionView {
            option_id: option.option_id.clone(),
            name: option.name.clone(),
            kind: option.kind,
            tone: tone_of(option.kind),
        })
        .collect();
    views.sort_by_key(|view| kind_rank(view.kind));
    views
}

/// Time left as shown on the card: `5m 0s left`, `45s left`, or `Expired` at or past zero.
/// Partial seconds round up, so a card never reads `0s left` while still open.
pub fn format_time_left(remaining_ms: i64) -> String {
    if remaining_ms <= 0 {
        return "Expired".to_owned();
    }
    let secs = (remaining_ms + 999) / 1000;
    if secs >= 60 {
        format!("{}m {}s left", secs / 60, secs % 60)
    } else {
        format!("{secs}s left")
    }
}

/// Parses the `YYYY-MM-DDTHH:MM:SS[.fff]Z` form the protocol writes into epoch milliseconds.
pub fn parse_timestamp_ms(text: &str) -> Option<i64> {
    let (date, time) = text.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    let body = time.strip_suffix('Z')?;
    let (clock, fraction) = body.split_once('.').unwrap_or((body, "0"));
    let mut clock_parts = clock.split(':');
    let hour: i64 = clock_parts.next()?.parse().ok()?;
    let minute: i64 = clock_parts.next()?.parse().ok()?;
    let second: i64 = clock_parts.next()?.parse().ok()?;
    if clock_parts.next().is_some()
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let digits: String = fraction.chars().take(3).collect();
    let millis: i64 = format!("{digits:0<3}").parse().ok()?;
    let days = days_from_civil(year, month, day);
    Some((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000 + millis)
}

/// Days since 1970-01-01 (H. Hinnant's algorithm, proleptic Gregorian).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The name a person sees for an actor: its display name, or its id when it has none.
pub fn display_name(actor: &Actor) -> String {
    actor.name.clone().unwrap_or_else(|| actor.id.clone())
}

/// The body of `POST /threads/{thread}/interrupts/{interrupt}` for an approval option.
pub fn selected_body(option_id: &str) -> Value {
    json!({"outcome": {"type": "selected", "option_id": option_id}})
}

/// One answer to send. `idempotency_key` is new for every click, so a retry of one click can
/// reuse it while a second click is a different answer.
#[derive(Debug, Clone, PartialEq)]
pub struct AnswerRequest {
    pub thread_id: String,
    pub interrupt_id: String,
    pub idempotency_key: String,
    pub body: Value,
}

/// Why a sink could not record an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerError {
    /// 409 `already_resolved`: an earlier answer won; `resolved_by` says who.
    AlreadyResolved { resolved_by: Actor },
    /// 410 `expired`.
    Expired,
    /// Any other HTTP status, with the response body.
    Http { status: u16, body: String },
    /// No response at all (connection refused, timeout, ...).
    Transport(String),
}

impl std::fmt::Display for AnswerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyResolved { resolved_by } => {
                write!(f, "already resolved by {}", display_name(resolved_by))
            }
            Self::Expired => write!(f, "the approval expired"),
            Self::Http { status, body } => write!(f, "HTTP {status}: {body}"),
            Self::Transport(message) => write!(f, "{message}"),
        }
    }
}

/// Where the card is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardState {
    /// Waiting for an answer.
    Pending,
    /// An answer is in flight; no button is live.
    Sending,
    /// This card's answer won; `by` is the resolver's name.
    Accepted { by: String },
    /// Another answer won (409); `by` is the resolver's name.
    AlreadyResolved { by: String },
    /// The approval expired (410). No answer is accepted any more.
    Expired,
    /// The answer could not be sent; the buttons stay live so the person can retry.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalModel {
    thread_id: String,
    interrupt_id: String,
    title: String,
    message: Option<String>,
    tool_call_id: Option<String>,
    expires_at_ms: Option<i64>,
    options: Vec<OptionView>,
    state: CardState,
    attempts: u64,
}

impl ApprovalModel {
    /// The model for an approval interrupt of `thread_id`. Other kinds are not approvals.
    pub fn from_interrupt(thread_id: &str, interrupt: &Interrupt) -> Result<Self, String> {
        if interrupt.kind != InterruptKind::Approval {
            return Err(format!(
                "interrupt {} is not an approval",
                interrupt.interrupt_id
            ));
        }
        Ok(Self {
            thread_id: thread_id.to_owned(),
            interrupt_id: interrupt.interrupt_id.clone(),
            title: interrupt.title.clone(),
            message: interrupt.message.clone(),
            tool_call_id: interrupt.tool_call_id.clone(),
            expires_at_ms: parse_timestamp_ms(&interrupt.expires_at),
            options: interrupt
                .options
                .as_deref()
                .map(ordered_options)
                .unwrap_or_default(),
            state: CardState::Pending,
            attempts: 0,
        })
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn interrupt_id(&self) -> &str {
        &self.interrupt_id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// The line under the title: the interrupt's message, or the title when it has none.
    pub fn summary(&self) -> &str {
        self.message.as_deref().unwrap_or(&self.title)
    }

    pub fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
    }

    pub fn expires_at_ms(&self) -> Option<i64> {
        self.expires_at_ms
    }

    pub fn options(&self) -> &[OptionView] {
        &self.options
    }

    pub fn state(&self) -> &CardState {
        &self.state
    }

    fn past_expiry(&self, now_ms: i64) -> bool {
        self.expires_at_ms.is_some_and(|expires| now_ms >= expires)
    }

    /// Whether the option buttons take clicks at `now_ms`.
    pub fn buttons_enabled(&self, now_ms: i64) -> bool {
        matches!(self.state, CardState::Pending | CardState::Failed(_)) && !self.past_expiry(now_ms)
    }

    /// The request for clicking `option_id` at `now_ms`, or `None` when the card takes no answer
    /// (not pending, expired by the clock, or an unknown option). Moves the card to `Sending`.
    pub fn begin_answer(&mut self, option_id: &str, now_ms: i64) -> Option<AnswerRequest> {
        if !self.buttons_enabled(now_ms) || !self.options.iter().any(|o| o.option_id == option_id) {
            return None;
        }
        self.attempts += 1;
        self.state = CardState::Sending;
        Some(AnswerRequest {
            thread_id: self.thread_id.clone(),
            interrupt_id: self.interrupt_id.clone(),
            idempotency_key: format!("{}:{}", self.interrupt_id, self.attempts),
            body: selected_body(option_id),
        })
    }

    /// Folds the sink's reply into the state.
    pub fn apply_reply(&mut self, reply: Result<Actor, AnswerError>) {
        self.state = match reply {
            Ok(resolver) => CardState::Accepted {
                by: display_name(&resolver),
            },
            Err(AnswerError::AlreadyResolved { resolved_by }) => CardState::AlreadyResolved {
                by: display_name(&resolved_by),
            },
            Err(AnswerError::Expired) => CardState::Expired,
            Err(other) => CardState::Failed(other.to_string()),
        };
    }

    /// The status line at `now_ms`.
    pub fn status_text(&self, now_ms: i64) -> String {
        match &self.state {
            CardState::Pending => match self.expires_at_ms {
                Some(expires) => format_time_left(expires - now_ms),
                None => "Waiting for your answer".to_owned(),
            },
            CardState::Sending => "Sending answer".to_owned(),
            CardState::Accepted { by } => format!("Resolved by {by}"),
            CardState::AlreadyResolved { by } => format!("Already resolved by {by}"),
            CardState::Expired => "Expired".to_owned(),
            CardState::Failed(message) => format!("Answer not sent: {message}"),
        }
    }
}

/// The `Interrupt{approval}` of the `approval` fixture scenario.
pub fn fixture_interrupt() -> Result<Interrupt, String> {
    let log = agui_fixture::complete_log(agui_fixture::Scenario::Approval, FIXTURE_THREAD)
        .map_err(|err| err.to_string())?;
    log.into_iter()
        .find_map(|envelope| match envelope.event {
            Event::Interrupt(interrupt) => Some(*interrupt),
            _ => None,
        })
        .ok_or_else(|| "the approval fixture has no interrupt".to_owned())
}

#[cfg(test)]
mod tests {
    use protocol::turn_event::{
        Actor, ActorKind, Interrupt, InterruptKind, InterruptOption, PermissionOptionKind,
    };
    use serde_json::json;

    use super::*;

    fn option(id: &str, name: &str, kind: PermissionOptionKind) -> InterruptOption {
        InterruptOption {
            option_id: id.to_owned(),
            name: name.to_owned(),
            kind,
        }
    }

    fn actor(name: Option<&str>) -> Actor {
        Actor {
            kind: ActorKind::User,
            id: "usr_1".to_owned(),
            name: name.map(str::to_owned),
            device_id: None,
        }
    }

    /// An approval expiring at 2026-10-27T22:10:00Z, with the options in an order the card must
    /// change (rejects first).
    fn approval() -> Interrupt {
        Interrupt {
            interrupt_id: "int_1".to_owned(),
            kind: InterruptKind::Approval,
            title: "Edit src/routes.rs".to_owned(),
            message: Some("Allow the agent to edit src/routes.rs?".to_owned()),
            expires_at: "2026-10-27T22:10:00.000Z".to_owned(),
            tool_call_id: Some("tc_1".to_owned()),
            options: Some(vec![
                option("reject_always", "Never", PermissionOptionKind::RejectAlways),
                option("reject", "Reject", PermissionOptionKind::RejectOnce),
                option("allow_always", "Always", PermissionOptionKind::AllowAlways),
                option("allow", "Allow once", PermissionOptionKind::AllowOnce),
            ]),
            response_schema: None,
            url: None,
        }
    }

    /// 2026-10-27T22:10:00Z in epoch milliseconds.
    const EXPIRES_MS: i64 = 1_793_139_000_000;

    #[test]
    fn approval_options_are_ordered_allow_once_allow_always_then_rejects() {
        let model = ApprovalModel::from_interrupt("th_1", &approval()).expect("approval");
        let ids: Vec<&str> = model
            .options()
            .iter()
            .map(|o| o.option_id.as_str())
            .collect();
        assert_eq!(ids, ["allow", "allow_always", "reject", "reject_always"]);
        let tones: Vec<ButtonTone> = model.options().iter().map(|o| o.tone).collect();
        assert_eq!(
            tones,
            [
                ButtonTone::Allow,
                ButtonTone::Allow,
                ButtonTone::Reject,
                ButtonTone::Reject
            ]
        );
    }

    #[test]
    fn approval_options_keep_the_run_order_within_a_kind() {
        let options = vec![
            option("r2", "second", PermissionOptionKind::RejectOnce),
            option("a1", "first", PermissionOptionKind::AllowOnce),
            option("r1", "first reject", PermissionOptionKind::RejectOnce),
        ];
        let ids: Vec<String> = ordered_options(&options)
            .into_iter()
            .map(|o| o.option_id)
            .collect();
        assert_eq!(ids, ["a1", "r2", "r1"]);
    }

    #[test]
    fn approval_time_left_formats_minutes_seconds_and_expiry() {
        assert_eq!(format_time_left(300_000), "5m 0s left");
        assert_eq!(format_time_left(570_000), "9m 30s left");
        assert_eq!(format_time_left(45_000), "45s left");
        assert_eq!(format_time_left(1), "1s left");
        assert_eq!(format_time_left(1_500), "2s left");
        assert_eq!(format_time_left(0), "Expired");
        assert_eq!(format_time_left(-5), "Expired");
    }

    #[test]
    fn approval_timestamps_parse_the_protocol_format() {
        assert_eq!(parse_timestamp_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_timestamp_ms("2026-10-27T22:00:03.000Z"),
            Some(1_793_138_403_000)
        );
        assert_eq!(
            parse_timestamp_ms("2000-02-29T00:00:00.123Z"),
            Some(951_782_400_123)
        );
        assert_eq!(
            parse_timestamp_ms("2026-10-27T22:00:03Z"),
            Some(1_793_138_403_000)
        );
        assert_eq!(parse_timestamp_ms("not a time"), None);
        assert_eq!(parse_timestamp_ms("2026-13-01T00:00:00.000Z"), None);
    }

    #[test]
    fn approval_model_reads_expiry_and_time_left_from_the_clock() {
        let model = ApprovalModel::from_interrupt("th_1", &approval()).expect("approval");
        assert_eq!(model.expires_at_ms(), Some(EXPIRES_MS));
        assert_eq!(model.status_text(EXPIRES_MS - 300_000), "5m 0s left");
        assert!(model.buttons_enabled(EXPIRES_MS - 1));
        assert!(!model.buttons_enabled(EXPIRES_MS));
        assert_eq!(model.status_text(EXPIRES_MS + 1), "Expired");
    }

    #[test]
    fn approval_model_rejects_non_approval_interrupts() {
        let mut form = approval();
        form.kind = InterruptKind::Form;
        assert!(ApprovalModel::from_interrupt("th_1", &form).is_err());
    }

    #[test]
    fn approval_begin_answer_builds_the_selected_outcome_body() {
        let mut model = ApprovalModel::from_interrupt("th_1", &approval()).expect("approval");
        let request = model
            .begin_answer("allow", EXPIRES_MS - 1_000)
            .expect("allow is live");
        assert_eq!(request.thread_id, "th_1");
        assert_eq!(request.interrupt_id, "int_1");
        assert_eq!(
            request.body,
            json!({"outcome": {"type": "selected", "option_id": "allow"}})
        );
        assert_eq!(model.state(), &CardState::Sending);
        assert!(
            !model.buttons_enabled(EXPIRES_MS - 1_000),
            "no second click while sending"
        );
        assert_eq!(model.begin_answer("reject", EXPIRES_MS - 1_000), None);
    }

    #[test]
    fn approval_begin_answer_refuses_unknown_options_and_expired_cards() {
        let mut model = ApprovalModel::from_interrupt("th_1", &approval()).expect("approval");
        assert_eq!(model.begin_answer("nope", EXPIRES_MS - 1_000), None);
        assert_eq!(model.state(), &CardState::Pending);
        assert_eq!(model.begin_answer("allow", EXPIRES_MS), None);
        assert_eq!(model.state(), &CardState::Pending);
    }

    #[test]
    fn approval_idempotency_keys_differ_per_click() {
        let mut model = ApprovalModel::from_interrupt("th_1", &approval()).expect("approval");
        let first = model
            .begin_answer("allow", EXPIRES_MS - 1_000)
            .expect("first");
        model.apply_reply(Err(AnswerError::Transport("reset".to_owned())));
        let second = model
            .begin_answer("allow", EXPIRES_MS - 1_000)
            .expect("retry");
        assert_ne!(first.idempotency_key, second.idempotency_key);
    }

    #[test]
    fn approval_accepted_answer_names_the_resolver() {
        let mut model = ApprovalModel::from_interrupt("th_1", &approval()).expect("approval");
        model.begin_answer("allow", EXPIRES_MS - 1_000);
        model.apply_reply(Ok(actor(Some("Fixture user"))));
        assert_eq!(
            model.status_text(EXPIRES_MS - 1_000),
            "Resolved by Fixture user"
        );
        assert!(!model.buttons_enabled(EXPIRES_MS - 1_000));
    }

    #[test]
    fn approval_resolver_without_a_name_shows_its_id() {
        let mut model = ApprovalModel::from_interrupt("th_1", &approval()).expect("approval");
        model.begin_answer("allow", EXPIRES_MS - 1_000);
        model.apply_reply(Ok(actor(None)));
        assert_eq!(model.status_text(EXPIRES_MS - 1_000), "Resolved by usr_1");
    }

    #[test]
    fn approval_409_shows_already_resolved_by_the_winner() {
        let mut model = ApprovalModel::from_interrupt("th_1", &approval()).expect("approval");
        model.begin_answer("reject", EXPIRES_MS - 1_000);
        model.apply_reply(Err(AnswerError::AlreadyResolved {
            resolved_by: actor(Some("Dana (phone)")),
        }));
        assert_eq!(
            model.state(),
            &CardState::AlreadyResolved {
                by: "Dana (phone)".to_owned()
            }
        );
        assert_eq!(
            model.status_text(EXPIRES_MS - 1_000),
            "Already resolved by Dana (phone)"
        );
        assert!(!model.buttons_enabled(EXPIRES_MS - 1_000));
    }

    #[test]
    fn approval_410_shows_expired_and_takes_no_answer() {
        let mut model = ApprovalModel::from_interrupt("th_1", &approval()).expect("approval");
        model.begin_answer("allow", EXPIRES_MS - 1_000);
        model.apply_reply(Err(AnswerError::Expired));
        assert_eq!(model.status_text(EXPIRES_MS - 1_000), "Expired");
        assert_eq!(model.begin_answer("allow", EXPIRES_MS - 1_000), None);
    }

    #[test]
    fn approval_failed_send_keeps_the_buttons_live_for_a_retry() {
        let mut model = ApprovalModel::from_interrupt("th_1", &approval()).expect("approval");
        model.begin_answer("allow", EXPIRES_MS - 1_000);
        model.apply_reply(Err(AnswerError::Http {
            status: 500,
            body: "boom".to_owned(),
        }));
        assert_eq!(
            model.status_text(EXPIRES_MS - 1_000),
            "Answer not sent: HTTP 500: boom"
        );
        assert!(model.buttons_enabled(EXPIRES_MS - 1_000));
    }

    #[test]
    fn approval_fixture_interrupt_is_an_allow_or_reject_approval() {
        let interrupt = fixture_interrupt().expect("approval fixture");
        assert_eq!(interrupt.kind, InterruptKind::Approval);
        assert_eq!(interrupt.tool_call_id.as_deref(), Some("tc_00001_route"));
        let model = ApprovalModel::from_interrupt(FIXTURE_THREAD, &interrupt).expect("approval");
        let ids: Vec<&str> = model
            .options()
            .iter()
            .map(|o| o.option_id.as_str())
            .collect();
        assert_eq!(ids, ["allow", "reject"]);
        assert!(model.summary().contains("src/routes.rs"));
    }
}
