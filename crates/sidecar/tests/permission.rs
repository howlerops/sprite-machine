//! Permission and elicitation interrupts against the runner bridge: the outcome table of
//! tunnel.md §3.5, double answers, and the cancel races of turn-event.md §4.9.
//!
//! An in-process acp-mock plays the agent. The mock checks what the bridge answered (`expect`),
//! so a wrong ACP answer fails the prompt and the test. The harness plays the hub: it appends
//! `UserMessage` and the accepted `InterruptResolved` to the log it validates.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use protocol::mapping::acp_turn::{SequenceValidator, validate_complete};
use protocol::tunnel::{Actor, ActorKind, Command, CommandErrorCode, ContentPart, Outcome};
use protocol::turn_event::{
    Envelope, Event, InterruptKind, InterruptResolved, ResolvedOutcome, RunErrorReason, RunOutcome,
    RunReason, ThreadSeq, UserMessage,
};
use serde_json::{Value, json};
use sidecar::bridge::{
    Bridge, BridgeError, CommandFailure, CommandFeed, Config, MemoryCommands, MemorySink,
    SequentialGenerator, SinkItem,
};
use sidecar::store::{ManualClock, Store};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(20);
const T0: u64 = 1_700_000_000_000;

fn user() -> Actor {
    Actor {
        kind: ActorKind::User,
        id: "usr_1".into(),
        name: None,
        device_id: Some("dev_1".into()),
    }
}

fn permission_step(call: &str, expect: &str) -> Value {
    json!({ "request_permission": {
        "toolCall": { "toolCallId": call, "title": "Run tests", "kind": "execute" },
        "options": [
            { "optionId": "allow_once", "name": "Allow", "kind": "allow_once" },
            { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" }
        ] },
      "expect": expect })
}

fn form_step(expect: &str) -> Value {
    json!({ "elicitation": { "mode": "form", "message": "Which branch?",
        "requestedSchema": { "type": "object",
            "properties": { "branch": { "type": "string" } } } },
      "expect": expect })
}

fn url_step(expect: &str) -> Value {
    json!({ "elicitation": { "mode": "url", "message": "Sign in to continue",
        "elicitationId": "e1", "url": "https://example.test/auth" },
      "expect": expect })
}

fn script(steps: Vec<Value>) -> Value {
    json!({ "turns": [{ "steps": steps }] })
}

// ---- harness ----------------------------------------------------------------------------------

struct Harness {
    feed: CommandFeed,
    events: mpsc::UnboundedReceiver<SinkItem>,
    pending: VecDeque<Envelope>,
    failures: VecDeque<CommandFailure>,
    log: Vec<Envelope>,
    bridge: JoinHandle<Result<(), BridgeError>>,
    _dirs: Vec<TempDir>,
}

impl Harness {
    async fn start(script: Value) -> Self {
        let script = acp_mock::Script::from_value(&script).expect("script");
        let (client_io, agent_io) = tokio::io::duplex(1 << 16);
        let (agent_r, agent_w) = tokio::io::split(agent_io);
        tokio::spawn(acp_mock::run(script, agent_r, agent_w));
        let (reader, writer) = tokio::io::split(client_io);

        let state = TempDir::new().expect("state dir");
        let work = TempDir::new().expect("work dir");
        let clock = Arc::new(ManualClock::new(T0));
        let store = Arc::new(Store::open_with_clock(state.path(), clock).expect("store"));
        let mut config = Config::new("mch_1", "agt_mock", work.path());
        config.expiry_check_interval = Duration::from_millis(20);
        let (sink, events) = MemorySink::new();
        let (commands, feed) = MemoryCommands::new();
        let bridge = Bridge::connect(
            config,
            reader,
            writer,
            store,
            Arc::new(SequentialGenerator::new()),
            sink,
            commands,
        );
        Self {
            feed,
            events,
            pending: VecDeque::new(),
            failures: VecDeque::new(),
            log: Vec::new(),
            bridge: tokio::spawn(bridge.run()),
            _dirs: vec![state, work],
        }
    }

    async fn pump(&mut self) {
        match timeout(STEP, self.events.recv()).await {
            Ok(Some(SinkItem::Events(envelopes))) => self.pending.extend(envelopes),
            Ok(Some(SinkItem::Failure(failure))) => self.failures.push_back(failure),
            Ok(None) => panic!("the bridge stopped; log so far: {:#?}", self.log),
            Err(_) => panic!("timed out; log so far: {:#?}", self.log),
        }
    }

    /// Consumes envelopes up to and including the first one `stop` accepts.
    async fn until(&mut self, stop: impl Fn(&Envelope) -> bool) -> Vec<Envelope> {
        let mut got = Vec::new();
        loop {
            while let Some(envelope) = self.pending.pop_front() {
                let done = stop(&envelope);
                self.log.push(envelope.clone());
                got.push(envelope);
                if done {
                    return got;
                }
            }
            self.pump().await;
        }
    }

    async fn failure(&mut self) -> CommandFailure {
        loop {
            if let Some(failure) = self.failures.pop_front() {
                return failure;
            }
            self.pump().await;
        }
    }

    fn hub_event(&mut self, event: impl Into<Event>) {
        self.log.push(Envelope::new(
            "th_1",
            Some("tk_1".to_owned()),
            None,
            "2023-11-14T22:13:20.000Z",
            event,
        ));
    }

    /// The hub appends `UserMessage`, then the prompt reaches the runner.
    fn prompt(&mut self) {
        self.hub_event(UserMessage {
            message_id: "msg_u".into(),
            parts: vec![ContentPart::Text { text: "go".into() }],
            author: user(),
        });
        self.feed.send(Command::Prompt {
            thread_id: "th_1".into(),
            task_id: "tk_1".into(),
            message_id: "msg_u".into(),
            parts: vec![ContentPart::Text { text: "go".into() }],
            author: user(),
        });
    }

    /// Waits for the run to pause; returns the events up to `RunFinished{interrupt}`.
    async fn paused(&mut self) -> Vec<Envelope> {
        self.until(|e| matches!(e.event, Event::RunFinished(_)))
            .await
    }

    /// The hub appends `InterruptResolved` for an accepted answer, then sends the command.
    fn answer(&mut self, interrupt: &str, outcome: Outcome, resolved: ResolvedOutcome) -> String {
        self.hub_event(InterruptResolved {
            interrupt_id: interrupt.into(),
            outcome: resolved,
            resolved_by: user(),
        });
        self.answer_unrecorded(interrupt, outcome)
    }

    /// An answer the hub would have refused (a second one): no `InterruptResolved` in the log.
    fn answer_unrecorded(&mut self, interrupt: &str, outcome: Outcome) -> String {
        self.feed.send(Command::Approval {
            thread_id: "th_1".into(),
            task_id: "tk_1".into(),
            interrupt_id: interrupt.into(),
            outcome,
            resolved_by: user(),
        })
    }

    fn cancel(&mut self) {
        self.feed.send(Command::Cancel {
            thread_id: "th_1".into(),
            task_id: "tk_1".into(),
        });
    }

    /// Ends the command stream, waits for the bridge and checks the whole log.
    async fn finish(self) -> Vec<Envelope> {
        let Self {
            feed,
            mut events,
            mut pending,
            mut log,
            bridge,
            ..
        } = self;
        drop(feed);
        timeout(STEP, bridge)
            .await
            .expect("the bridge stops when the commands end")
            .expect("bridge task")
            .expect("bridge ok");
        while let Ok(item) = events.try_recv() {
            if let SinkItem::Events(envelopes) = item {
                pending.extend(envelopes);
            }
        }
        log.extend(pending);
        assert_valid_log(&log);
        log
    }
}

fn interrupt_of(events: &[Envelope]) -> protocol::turn_event::Interrupt {
    events
        .iter()
        .find_map(|e| match &e.event {
            Event::Interrupt(interrupt) => Some((**interrupt).clone()),
            _ => None,
        })
        .expect("an Interrupt")
}

fn is_success(envelope: &Envelope) -> bool {
    matches!(&envelope.event, Event::RunFinished(f) if matches!(f.outcome, RunOutcome::Success { .. }))
}

fn is_run_error(envelope: &Envelope) -> bool {
    matches!(envelope.event, Event::RunError(_))
}

fn types(events: &[Envelope]) -> Vec<&'static str> {
    events.iter().map(|e| e.event.type_name()).collect()
}

/// The log as the Thread DO would store it: valid in order and gap-free once stamped.
fn assert_valid_log(log: &[Envelope]) {
    validate_complete(log).unwrap_or_else(|error| panic!("{error}\n{log:#?}"));
    let mut seq = ThreadSeq::new();
    let mut validator = SequenceValidator::new();
    for envelope in log {
        let stamped = seq.stamp(envelope.clone()).expect("producers send seq 0");
        validator
            .push(&stamped)
            .unwrap_or_else(|error| panic!("{error}"));
    }
    validator.finish().expect("the log ends between tasks");
}

fn selected(option: &str) -> Outcome {
    Outcome::Selected {
        option_id: option.into(),
    }
}

fn selected_resolved(option: &str) -> ResolvedOutcome {
    ResolvedOutcome::Selected {
        option_id: option.into(),
    }
}

/// After a cancel: `InterruptResolved{cancelled}`, the resume run, `RunError{cancelled}`.
fn assert_cancel_sequence(rest: &[Envelope], interrupt: &str) {
    let names = types(rest);
    assert_eq!(names.first(), Some(&"InterruptResolved"), "{names:?}");
    assert_eq!(names.get(1), Some(&"RunStarted"), "{names:?}");
    assert_eq!(names.last(), Some(&"RunError"), "{names:?}");
    let Event::InterruptResolved(resolved) = &rest[0].event else {
        panic!("InterruptResolved first");
    };
    assert_eq!(resolved.interrupt_id, interrupt);
    assert_eq!(resolved.outcome, ResolvedOutcome::Cancelled);
    let Event::RunStarted(resume) = &rest[1].event else {
        panic!("resume RunStarted");
    };
    assert_eq!(resume.reason, RunReason::Resume);
    let Event::RunError(error) = &rest[rest.len() - 1].event else {
        panic!("RunError last");
    };
    assert_eq!(error.reason, RunErrorReason::Cancelled);
}

// ---- tests ------------------------------------------------------------------------------------

#[tokio::test]
async fn permission_is_an_approval_interrupt_and_the_selected_option_answers_acp() {
    // The mock fails the prompt unless the bridge answered the `allow_once` option.
    let mut h = Harness::start(script(vec![
        json!({ "text": "Working." }),
        permission_step("call_1", "allow_once"),
        json!({ "text": "Done." }),
    ]))
    .await;
    h.prompt();
    let first = h.paused().await;
    let interrupt = interrupt_of(&first);
    assert_eq!(interrupt.kind, InterruptKind::Approval);
    let options = interrupt.options.as_ref().expect("options");
    assert_eq!(options.len(), 2);
    assert_eq!(options[0].option_id, "allow_once");

    h.answer(
        &interrupt.interrupt_id,
        selected("allow_once"),
        selected_resolved("allow_once"),
    );
    let rest = h.until(is_success).await;
    assert_eq!(types(&rest)[0], "RunStarted");
    let log = h.finish().await;
    assert!(log.iter().all(|e| !is_run_error(e)), "{:?}", types(&log));
}

#[tokio::test]
async fn a_second_answer_for_the_same_interrupt_is_rejected_as_unknown() {
    let mut h = Harness::start(script(vec![
        permission_step("call_1", "allow_once"),
        permission_step("call_2", "any"),
    ]))
    .await;
    h.prompt();
    let first = interrupt_of(&h.paused().await).interrupt_id;
    h.answer(
        &first,
        selected("allow_once"),
        selected_resolved("allow_once"),
    );
    // The agent asks again, so the task is still paused when the duplicate arrives.
    let second = interrupt_of(&h.paused().await).interrupt_id;
    assert_ne!(first, second);

    let key = h.answer_unrecorded(&first, selected("reject_once"));
    let failure = h.failure().await;
    assert_eq!(failure.code, CommandErrorCode::UnknownInterrupt);
    assert_eq!(failure.idempotency_key, key);

    // The open interrupt is untouched: it still takes its own answer.
    h.answer(
        &second,
        selected("allow_once"),
        selected_resolved("allow_once"),
    );
    h.until(is_success).await;
    h.finish().await;
}

#[tokio::test]
async fn a_form_interrupt_carries_the_schema_and_submitted_data_is_accepted() {
    let mut h = Harness::start(script(vec![form_step("accept"), json!({ "text": "ok" })])).await;
    h.prompt();
    let interrupt = interrupt_of(&h.paused().await);
    assert_eq!(interrupt.kind, InterruptKind::Form);
    let schema = interrupt.response_schema.as_ref().expect("response_schema");
    assert_eq!(schema["properties"]["branch"]["type"], "string");
    assert!(interrupt.url.is_none());

    let data = json!({ "branch": "main" });
    h.answer(
        &interrupt.interrupt_id,
        Outcome::Submitted { data: data.clone() },
        ResolvedOutcome::Submitted { data },
    );
    h.until(is_success).await;
    h.finish().await;
}

#[tokio::test]
async fn a_url_interrupt_is_auth_with_the_url_and_declined_answers_decline() {
    let mut h = Harness::start(script(vec![url_step("decline"), json!({ "text": "ok" })])).await;
    h.prompt();
    let interrupt = interrupt_of(&h.paused().await);
    assert_eq!(interrupt.kind, InterruptKind::Auth);
    assert_eq!(interrupt.url.as_deref(), Some("https://example.test/auth"));
    assert!(interrupt.response_schema.is_none());

    h.answer(
        &interrupt.interrupt_id,
        Outcome::Declined,
        ResolvedOutcome::Declined,
    );
    h.until(is_success).await;
    h.finish().await;
}

#[tokio::test]
async fn cancelled_outcomes_answer_acp_cancel_for_forms_and_permissions() {
    let mut h = Harness::start(script(vec![
        form_step("cancel"),
        permission_step("call_1", "cancelled"),
        json!({ "text": "ok" }),
    ]))
    .await;
    h.prompt();
    let form = interrupt_of(&h.paused().await).interrupt_id;
    h.answer(&form, Outcome::Cancelled, ResolvedOutcome::Cancelled);
    let permission = interrupt_of(&h.paused().await).interrupt_id;
    h.answer(&permission, Outcome::Cancelled, ResolvedOutcome::Cancelled);
    h.until(is_success).await;
    h.finish().await;
}

#[tokio::test]
async fn a_mismatched_outcome_answers_acp_cancelled_and_is_invalid() {
    // Each mock step expects the cancelled answer; the order of the interrupts is the script's.
    let mut h = Harness::start(script(vec![
        permission_step("call_1", "cancelled"),
        form_step("cancel"),
        url_step("cancel"),
        json!({ "text": "ok" }),
    ]))
    .await;
    h.prompt();

    // `submitted` for an approval.
    let approval = interrupt_of(&h.paused().await).interrupt_id;
    let data = json!({ "x": 1 });
    let key = h.answer(
        &approval,
        Outcome::Submitted { data: data.clone() },
        ResolvedOutcome::Submitted { data },
    );
    let failure = h.failure().await;
    assert_eq!(failure.code, CommandErrorCode::Invalid);
    assert_eq!(failure.idempotency_key, key);

    // `selected` for a form.
    let form = interrupt_of(&h.paused().await).interrupt_id;
    h.answer(
        &form,
        selected("allow_once"),
        selected_resolved("allow_once"),
    );
    assert_eq!(h.failure().await.code, CommandErrorCode::Invalid);

    // `submitted` for an auth interrupt (the table gives auth only declined and cancelled).
    let auth = interrupt_of(&h.paused().await).interrupt_id;
    let data = json!({});
    h.answer(
        &auth,
        Outcome::Submitted { data: data.clone() },
        ResolvedOutcome::Submitted { data },
    );
    assert_eq!(h.failure().await.code, CommandErrorCode::Invalid);

    // The mock only reaches its last step if every answer was the expected cancel.
    h.until(is_success).await;
    h.finish().await;
}

#[tokio::test]
async fn a_form_answered_with_non_object_data_is_cancelled_and_invalid() {
    let mut h = Harness::start(script(vec![form_step("cancel"), json!({ "text": "ok" })])).await;
    h.prompt();
    let form = interrupt_of(&h.paused().await).interrupt_id;
    let data = json!("main");
    h.answer(
        &form,
        Outcome::Submitted { data: data.clone() },
        ResolvedOutcome::Submitted { data },
    );
    assert_eq!(h.failure().await.code, CommandErrorCode::Invalid);
    h.until(is_success).await;
    h.finish().await;
}

#[tokio::test]
async fn cancel_then_answer_ends_cancelled_and_the_late_answer_is_rejected() {
    let mut h = Harness::start(script(vec![
        json!({ "text": "Working." }),
        permission_step("call_1", "cancelled"),
        json!({ "text": "never sent" }),
    ]))
    .await;
    h.prompt();
    let interrupt = interrupt_of(&h.paused().await).interrupt_id;

    h.cancel();
    let rest = h.until(is_run_error).await;
    assert_cancel_sequence(&rest, &interrupt);

    // The hub would have emitted nothing for this answer yet: it is told "resolved" only
    // after the runner accepts. The runner has no open interrupt, so it rejects.
    let key = h.answer_unrecorded(&interrupt, selected("allow_once"));
    let failure = h.failure().await;
    assert_eq!(failure.code, CommandErrorCode::UnknownInterrupt);
    assert_eq!(failure.idempotency_key, key);

    let log = h.finish().await;
    assert_eq!(log.iter().filter(|e| is_run_error(e)).count(), 1);
}

#[tokio::test]
async fn answer_then_cancel_resumes_first_and_the_next_interrupt_is_cancelled() {
    let mut h = Harness::start(script(vec![
        permission_step("call_1", "allow_once"),
        form_step("cancel"),
        json!({ "text": "never sent" }),
    ]))
    .await;
    h.prompt();
    let first = interrupt_of(&h.paused().await).interrupt_id;
    h.answer(
        &first,
        selected("allow_once"),
        selected_resolved("allow_once"),
    );
    // The answer resumed the run, and the agent paused again on a form.
    let second = interrupt_of(&h.paused().await).interrupt_id;
    assert_ne!(first, second);

    h.cancel();
    let rest = h.until(is_run_error).await;
    assert_cancel_sequence(&rest, &second);

    let key = h.answer_unrecorded(&second, Outcome::Declined);
    let failure = h.failure().await;
    assert_eq!(failure.code, CommandErrorCode::UnknownInterrupt);
    assert_eq!(failure.idempotency_key, key);
    h.finish().await;
}

#[tokio::test]
async fn cancel_while_a_url_interrupt_is_pending_answers_acp_cancel() {
    // The mock checks the bridge answered `cancel`; a wrong answer would fail its prompt.
    let mut h = Harness::start(script(vec![url_step("cancel"), json!({ "text": "never" })])).await;
    h.prompt();
    let interrupt = interrupt_of(&h.paused().await).interrupt_id;
    h.cancel();
    let rest = h.until(is_run_error).await;
    assert_cancel_sequence(&rest, &interrupt);
    h.answer_unrecorded(&interrupt, Outcome::Declined);
    assert_eq!(h.failure().await.code, CommandErrorCode::UnknownInterrupt);
    h.finish().await;
}
