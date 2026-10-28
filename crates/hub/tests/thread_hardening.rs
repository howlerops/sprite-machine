//! Native tests for the Thread DO hardening (M2-008a): secret redaction before an envelope is
//! stored or broadcast, catch-up in bounded pages, and state-history compaction. They drive
//! [`ThreadCore`] over a [`MemoryStore`] (or a probe around it), so they need no wrangler.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use hub::threads::redact::redact_text;
use hub::threads::{
    CATCH_UP_PAGE, MemoryStore, PromptBody, StoreResult, ThreadCore, ThreadStore, event_message,
};
use protocol::tunnel::{Actor, ActorKind, ContentPart};
use protocol::turn_event::{
    Envelope, Event, MachinePresence, PatchOp, PatchOpKind, Presence, RawPayload, RawSource,
    RunFinished, RunOutcome, RunReason, RunStarted, StateDelta, StateScope, StateSnapshot,
    StopReason, TextMessageContent, TextMessageEnd, TextMessageStart, format_timestamp,
};
use serde_json::{Value, json};

/// 2026-10-27T22:00:03Z.
const NOW: u64 = 1_793_138_403_000;

/// Obviously fake, key-shaped strings (never a real credential).
const ANTHROPIC: &str = "sk-ant-api03-FAKEFAKEFAKEFAKEFAKE";
const ACP_MOCK_KEY: &str =
    "sk-ant-api03-FAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKE-FAKEFAKEFAKEFAKEFAKEFAKEFAKE";
const GITHUB: &str = "ghp_FAKEFAKEFAKEFAKEFAKEFAKE01";
const BEARER: &str = "Bearer FAKE1234567890abcdef";

fn actor() -> Actor {
    Actor {
        kind: ActorKind::User,
        id: "usr_1".to_owned(),
        name: None,
        device_id: None,
    }
}

fn envelope(task: Option<&str>, run: Option<&str>, event: impl Into<Event>) -> Envelope {
    Envelope::new(
        "th_1",
        task.map(str::to_owned),
        run.map(str::to_owned),
        format_timestamp(NOW),
        event,
    )
}

fn usage_snapshot(used: u64) -> Envelope {
    let snapshot = json!({ "context_used": used, "context_size": 1000, "cost": null });
    envelope(
        None,
        None,
        StateSnapshot {
            scope: StateScope::Usage,
            snapshot: snapshot.as_object().cloned().unwrap_or_default(),
        },
    )
}

fn usage_delta(used: u64) -> Envelope {
    envelope(
        None,
        None,
        StateDelta {
            scope: StateScope::Usage,
            patch: vec![PatchOp {
                op: PatchOpKind::Replace,
                path: "/context_used".into(),
                from: None,
                value: Some(json!(used)),
            }],
        },
    )
}

fn presence(i: u64) -> Envelope {
    envelope(
        None,
        None,
        Presence {
            machine_id: "mch_1".to_owned(),
            state: if i.is_multiple_of(2) {
                MachinePresence::Online
            } else {
                MachinePresence::Asleep
            },
            since: format_timestamp(NOW),
        },
    )
}

/// One complete run of task `tk_<n>`: the prompt, `RunStarted`, a usage change, a text message,
/// a second usage change, `RunFinished`.
fn run(core: &mut ThreadCore<MemoryStore>, n: u64) {
    let task = format!("tk_{n}");
    let run = format!("run_{n}");
    let reply = core
        .submit_prompt(
            &actor(),
            &format!("idk_{n}"),
            PromptBody {
                task_id: Some(task.clone()),
                message_id: Some(format!("msg_{n}")),
                parts: vec![ContentPart::Text {
                    text: format!("prompt {n}"),
                }],
            },
            NOW,
        )
        .expect("prompt");
    assert_eq!(reply.body["status"], "started");
    let steps = [
        envelope(
            Some(&task),
            Some(&run),
            RunStarted {
                reason: RunReason::Prompt,
                parent_run_id: None,
                agent_id: None,
                machine_id: None,
            },
        ),
        usage_delta(n * 10 + 1),
        envelope(
            Some(&task),
            Some(&run),
            TextMessageStart {
                message_id: format!("m_{n}"),
                role: Default::default(),
            },
        ),
        envelope(
            Some(&task),
            Some(&run),
            TextMessageEnd {
                message_id: format!("m_{n}"),
            },
        ),
        usage_delta(n * 10 + 2),
        envelope(
            Some(&task),
            Some(&run),
            RunFinished {
                outcome: RunOutcome::Success {
                    stop_reason: Some(StopReason::EndTurn),
                },
            },
        ),
    ];
    for step in steps {
        core.append(step, NOW).expect("append accepted");
    }
}

fn messages_text(envelopes: &[Envelope]) -> String {
    serde_json::to_string(envelopes).expect("json")
}

// ---- redaction (turn-event.md §2) ----------------------------------------------------------

#[test]
fn redact_text_redacts_each_secret_kind() {
    let cases = [
        (
            format!("use {ANTHROPIC} now"),
            "use [redacted:anthropic] now",
        ),
        (
            "key sk-proj-FAKEFAKEFAKEFAKEFAKE12 end".to_owned(),
            "key [redacted:openai] end",
        ),
        (
            "key sk-FAKEFAKEFAKEFAKEFAKEFA end".to_owned(),
            "key [redacted:openai] end",
        ),
        (format!("token {GITHUB}"), "token [redacted:github]"),
        (
            "gho_FAKEFAKEFAKEFAKEFAKEFAKE01 ghs_FAKEFAKEFAKEFAKEFAKEFAKE01".to_owned(),
            "[redacted:github] [redacted:github]",
        ),
        (
            "github_pat_FAKEFAKEFAKEFAKEFAKE_01".to_owned(),
            "[redacted:github]",
        ),
        (
            "aws AKIAFAKEFAKEFAKE1234 id".to_owned(),
            "aws [redacted:aws_access_key] id",
        ),
        (
            format!("aws_secret_access_key = {}", "FAKE".repeat(10)),
            "[redacted:aws_secret]",
        ),
        (
            format!("AWS_SECRET_ACCESS_KEY: \"{}\"", "FAKE".repeat(10)),
            "[redacted:aws_secret]\"",
        ),
        (
            format!("Authorization: {BEARER}"),
            "Authorization: [redacted:bearer]",
        ),
        (
            "-----BEGIN RSA PRIVATE KEY-----\nFAKEFAKE\n-----END RSA PRIVATE KEY-----\nafter"
                .to_owned(),
            "[redacted:private_key]\nafter",
        ),
        (
            "-----BEGIN OPENSSH PRIVATE KEY-----\nFAKE (no end line)".to_owned(),
            "[redacted:private_key]",
        ),
        (
            format!("debug: {ACP_MOCK_KEY}"),
            "debug: [redacted:fake_api_key]",
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(redact_text(&input), expected, "input: {input}");
    }
}

#[test]
fn redact_text_leaves_ordinary_words_and_short_hex_alone() {
    let ordinary = [
        "risk-free ask-me-anything, the task-list is sk-short",
        "Bearer authentication is required; Bearer dev is the dev token",
        "commit 0123456789abcdef0123456789abcdef01234567 and deadbeef",
        "akia1234 is lower case, AKIA and AKIAshort are not keys",
        "ghp_short and gho_ also stay",
        "-----BEGIN CERTIFICATE-----\nnot a key\n-----END CERTIFICATE-----",
        "unicode: héllo wörld — ok 🙂 sk-ant-",
    ];
    for text in ordinary {
        assert!(
            matches!(redact_text(text), std::borrow::Cow::Borrowed(_)),
            "changed: {text}"
        );
        assert_eq!(redact_text(text), text);
    }
}

#[test]
fn stored_and_broadcast_envelopes_are_redacted_in_event_and_raw() {
    let mut core = ThreadCore::open("th_1", MemoryStore::new()).expect("open");

    // The prompt (a UserMessage event) is broadcast and stored redacted.
    core.submit_prompt(
        &actor(),
        "idk_1",
        PromptBody {
            task_id: Some("tk_1".to_owned()),
            message_id: Some("msg_1".to_owned()),
            parts: vec![ContentPart::Text {
                text: format!("use {ANTHROPIC} please"),
            }],
        },
        NOW,
    )
    .expect("prompt");
    let broadcast = core.take_appended();
    assert_eq!(broadcast.len(), 1);
    let text = messages_text(&broadcast);
    assert!(!text.contains(ANTHROPIC), "broadcast leaks: {text}");
    assert!(text.contains("[redacted:anthropic]"));

    // A text chunk with a secret in `event` and one in `raw`.
    let started = envelope(
        Some("tk_1"),
        Some("run_1"),
        RunStarted {
            reason: RunReason::Prompt,
            parent_run_id: None,
            agent_id: None,
            machine_id: None,
        },
    );
    core.append(started, NOW).expect("run started");
    core.append(
        envelope(
            Some("tk_1"),
            Some("run_1"),
            TextMessageStart {
                message_id: "m_1".to_owned(),
                role: Default::default(),
            },
        ),
        NOW,
    )
    .expect("text start");
    let mut chunk = envelope(
        Some("tk_1"),
        Some("run_1"),
        TextMessageContent {
            message_id: "m_1".to_owned(),
            delta: format!("found {GITHUB} in the log"),
        },
    );
    chunk.raw = Some(RawPayload {
        source: RawSource::Acp,
        payload: json!({ "content": { "text": format!("auth {BEARER}") } }),
    });
    core.append(chunk, NOW).expect("chunk");

    let broadcast = core.take_appended();
    let chunk_broadcast = broadcast.last().expect("chunk broadcast").clone();
    let stored = core
        .store()
        .read_range(1, core.head_seq())
        .expect("read stored log");
    let stored_chunk = stored.last().expect("stored chunk").clone();

    // The broadcast and the stored envelope are the same, redacted.
    assert_eq!(chunk_broadcast, stored_chunk);
    for envelope in [&chunk_broadcast, &stored_chunk] {
        let text = messages_text(std::slice::from_ref(envelope));
        assert!(!text.contains("ghp_FAKE"), "secret kept: {text}");
        assert!(!text.contains("Bearer FAKE"), "secret kept: {text}");
        assert!(text.contains("[redacted:github]"));
        assert!(text.contains("[redacted:bearer]"));
        assert!(text.contains("found [redacted:github] in the log"));
    }
    assert!(!messages_text(&stored).contains(ANTHROPIC));
    assert!(!messages_text(&broadcast).contains("ghp_FAKE"));
}

#[test]
fn ordinary_text_is_stored_and_broadcast_unchanged() {
    let mut core = ThreadCore::open("th_1", MemoryStore::new()).expect("open");
    let delta = "risk-free ask-me-anything, commit 0123456789abcdef, Bearer authentication";
    let start = envelope(
        None,
        None,
        StateSnapshot {
            scope: StateScope::Usage,
            snapshot: Default::default(),
        },
    );
    core.append(start, NOW).expect("snapshot");
    core.take_appended();
    core.submit_prompt(
        &actor(),
        "idk_1",
        PromptBody {
            task_id: Some("tk_1".to_owned()),
            message_id: Some("msg_1".to_owned()),
            parts: vec![ContentPart::Text {
                text: delta.to_owned(),
            }],
        },
        NOW,
    )
    .expect("prompt");
    let broadcast = core.take_appended();
    let Event::UserMessage(message) = &broadcast[0].event else {
        panic!("expected UserMessage, got {:?}", broadcast[0].event);
    };
    assert_eq!(
        message.parts,
        vec![ContentPart::Text {
            text: delta.to_owned()
        }]
    );
    let stored = core.store().read_range(2, 2).expect("stored");
    assert_eq!(stored[0], broadcast[0]);
}

// ---- catch-up in bounded pages -------------------------------------------------------------

/// A store that delegates to [`MemoryStore`] and records the size of every `read_range`, so a
/// test can see how many rows one replay holds at a time.
#[derive(Clone)]
struct Probe {
    inner: MemoryStore,
    read_sizes: Rc<RefCell<Vec<usize>>>,
    fail_reads: Rc<Cell<bool>>,
}

impl Probe {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(),
            read_sizes: Rc::new(RefCell::new(Vec::new())),
            fail_reads: Rc::new(Cell::new(false)),
        }
    }
}

impl ThreadStore for Probe {
    fn head_seq(&self) -> StoreResult<u64> {
        self.inner.head_seq()
    }
    fn append_event(&mut self, envelope: &Envelope) -> StoreResult<()> {
        self.inner.append_event(envelope)
    }
    fn read_range(&self, from: u64, to: u64) -> StoreResult<Vec<Envelope>> {
        if self.fail_reads.get() {
            return Err(hub::threads::StoreError::new("disk on fire"));
        }
        let rows = self.inner.read_range(from, to)?;
        self.read_sizes.borrow_mut().push(rows.len());
        Ok(rows)
    }
    fn put_run(&mut self, run: &hub::threads::RunRecord) -> StoreResult<()> {
        self.inner.put_run(run)
    }
    fn get_run(&self, run_id: &str) -> StoreResult<Option<hub::threads::RunRecord>> {
        self.inner.get_run(run_id)
    }
    fn run_boundary_at(&self, after: u64) -> StoreResult<Option<u64>> {
        self.inner.run_boundary_at(after)
    }
    fn run_start_back(&self, back: u64) -> StoreResult<Option<u64>> {
        self.inner.run_start_back(back)
    }
    fn put_state(&mut self, scope: StateScope, seq: u64, state: &Value) -> StoreResult<()> {
        self.inner.put_state(scope, seq, state)
    }
    fn state_at(&self, scope: StateScope, seq: u64) -> StoreResult<Option<Value>> {
        self.inner.state_at(scope, seq)
    }
    fn compact_states(&mut self, scope: StateScope, floor: u64) -> StoreResult<()> {
        self.inner.compact_states(scope, floor)
    }
    fn put_task(&mut self, task: &hub::threads::TaskRecord) -> StoreResult<()> {
        self.inner.put_task(task)
    }
    fn get_task(&self, task_id: &str) -> StoreResult<Option<hub::threads::TaskRecord>> {
        self.inner.get_task(task_id)
    }
    fn tasks_with_status(
        &self,
        status: hub::threads::TaskStatus,
    ) -> StoreResult<Vec<hub::threads::TaskRecord>> {
        self.inner.tasks_with_status(status)
    }
    fn put_interrupt(&mut self, interrupt: &hub::threads::InterruptRecord) -> StoreResult<()> {
        self.inner.put_interrupt(interrupt)
    }
    fn get_interrupt(
        &self,
        interrupt_id: &str,
    ) -> StoreResult<Option<hub::threads::InterruptRecord>> {
        self.inner.get_interrupt(interrupt_id)
    }
    fn open_interrupts(&self, task_id: &str) -> StoreResult<Vec<hub::threads::InterruptRecord>> {
        self.inner.open_interrupts(task_id)
    }
    fn queue_command(
        &mut self,
        idempotency_key: &str,
        issued_at: &str,
        command: &protocol::tunnel::Command,
    ) -> StoreResult<u64> {
        self.inner
            .queue_command(idempotency_key, issued_at, command)
    }
    fn unacked_commands(&self, limit: usize) -> StoreResult<Vec<hub::threads::CommandRecord>> {
        self.inner.unacked_commands(limit)
    }
    fn ack_commands(&mut self, ids: &[u64]) -> StoreResult<usize> {
        self.inner.ack_commands(ids)
    }
    fn retract_prompt_command(&mut self, task_id: &str) -> StoreResult<bool> {
        self.inner.retract_prompt_command(task_id)
    }
    fn get_idempotency(
        &self,
        user_id: &str,
        key: &str,
    ) -> StoreResult<Option<hub::threads::IdempotencyRecord>> {
        self.inner.get_idempotency(user_id, key)
    }
    fn put_idempotency(&mut self, record: &hub::threads::IdempotencyRecord) -> StoreResult<()> {
        self.inner.put_idempotency(record)
    }
    fn prune_idempotency(&mut self, older_than_ms: u64) -> StoreResult<()> {
        self.inner.prune_idempotency(older_than_ms)
    }
    fn event_seen(&self, event_id: &str, now_ms: u64) -> StoreResult<bool> {
        self.inner.event_seen(event_id, now_ms)
    }
    fn record_event(&mut self, event_id: &str, now_ms: u64) -> StoreResult<()> {
        self.inner.record_event(event_id, now_ms)
    }
    fn get_meta(&self, key: &str) -> StoreResult<Option<Value>> {
        self.inner.get_meta(key)
    }
    fn put_meta(&mut self, key: &str, value: &Value) -> StoreResult<()> {
        self.inner.put_meta(key, value)
    }
    fn next_counter(&mut self) -> StoreResult<u64> {
        self.inner.next_counter()
    }
}

#[test]
fn catch_up_from_zero_over_20k_events_reads_pages_of_500_and_matches_the_log() {
    assert_eq!(CATCH_UP_PAGE, 500);
    let probe = Probe::new();
    let mut core = ThreadCore::open("th_1", probe.clone()).expect("open");
    for i in 0..20_000 {
        core.append(presence(i), NOW).expect("append");
    }
    let appended = core.take_appended();
    assert_eq!(appended.last().map(|e| e.seq), Some(20_000));
    probe.read_sizes.borrow_mut().clear();

    let messages: Vec<Value> = core.catch_up(0).expect("catch up").collect();

    // The same output as the whole log in one go: hello, snapshot (empty), 20k events, live.
    let mut expected = vec![
        json!({ "type": "hello", "head_seq": 20_000, "replay_from": 1 }),
        json!({ "type": "snapshot", "at_seq": 0, "states": {} }),
    ];
    expected.extend(appended.iter().map(event_message));
    expected.push(json!({ "type": "live" }));
    assert_eq!(messages, expected);

    // Every read is one page: at most 500 rows, 40 pages for 20k rows.
    let sizes = probe.read_sizes.borrow();
    assert_eq!(sizes.len(), 40);
    assert!(
        sizes.iter().all(|rows| *rows <= 500),
        "page too big: {sizes:?}"
    );
}

#[test]
fn catch_up_read_error_ends_the_replay_with_an_error_message() {
    let probe = Probe::new();
    let mut core = ThreadCore::open("th_1", probe.clone()).expect("open");
    for i in 0..10 {
        core.append(presence(i), NOW).expect("append");
    }
    probe.fail_reads.set(true);
    let messages: Vec<Value> = core.catch_up(0).expect("catch up").collect();
    assert_eq!(messages.len(), 3, "{messages:?}");
    assert_eq!(messages[0]["type"], "hello");
    assert_eq!(messages[1]["type"], "snapshot");
    assert_eq!(messages[2]["type"], "error");
    assert!(messages.iter().all(|m| m["type"] != "live"));
}

// ---- state-history compaction --------------------------------------------------------------

#[test]
fn state_history_compaction_keeps_catch_up_output_unchanged() {
    const RUNS: u64 = 12;
    const RETAINED: usize = 3;

    // The same thread twice: one keeps the whole state history, one compacts it.
    let mut reference = ThreadCore::open("th_1", MemoryStore::new())
        .expect("open")
        .with_state_retention(None);
    let mut compacted = ThreadCore::open("th_1", MemoryStore::new())
        .expect("open")
        .with_state_retention(Some(RETAINED));
    for core in [&mut reference, &mut compacted] {
        core.append(usage_snapshot(0), NOW).expect("snapshot");
    }
    for n in 0..RUNS {
        run(&mut reference, n);
        run(&mut compacted, n);
    }
    let head = compacted.head_seq();
    assert_eq!(reference.head_seq(), head);

    // The newest 3 runs keep their history: the floor is the oldest of them, minus one.
    let floor = compacted.state_floor().expect("floor");
    assert!(floor > 0, "nothing was compacted");
    assert_eq!(reference.state_floor().expect("floor"), 0);
    assert_eq!(
        compacted
            .store()
            .state_at(StateScope::Usage, 1)
            .expect("state"),
        None,
        "the first change should be compacted away"
    );
    assert!(
        reference
            .store()
            .state_at(StateScope::Usage, 1)
            .expect("state")
            .is_some()
    );

    // Catch-up output is the same for every `after` at or after the floor.
    for after in floor..=head {
        let kept: Vec<Value> = compacted.catch_up(after).expect("catch up").collect();
        let full: Vec<Value> = reference.catch_up(after).expect("catch up").collect();
        assert_eq!(kept, full, "after = {after}");
    }
    // Before the floor, a client is served from the floor (its snapshot would need the
    // compacted changes): the same output as after = floor.
    let from_floor: Vec<Value> = compacted.catch_up(floor).expect("catch up").collect();
    for after in 0..floor {
        let served: Vec<Value> = compacted.catch_up(after).expect("catch up").collect();
        assert_eq!(served, from_floor, "after = {after}");
    }
}
