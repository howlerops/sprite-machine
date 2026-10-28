//! Native unit tests for [`ThreadCore`] and the pure HTTP layer ([`api::handle`]) over a
//! [`MemoryStore`].

use protocol::tunnel::{Actor, ActorKind, Command, ContentPart, Outcome};
use protocol::turn_event::{
    Envelope, Event, Interrupt, InterruptKind, InterruptOption, PatchOp, PatchOpKind,
    PermissionOptionKind, ResolvedOutcome, RunError, RunErrorReason, RunFinished, RunOutcome,
    RunReason, RunStarted, StateDelta, StateScope, StateSnapshot, StopReason, TextMessageEnd,
    TextMessageStart, format_timestamp,
};
use serde_json::{Value, json};

use super::api::{self, ApiRequest};
use super::{
    EVENT_ID_TTL_MS, IDEMPOTENCY_TTL_MS, MemoryStore, PromptBody, ThreadCore, ThreadStore,
    event_message,
};

/// 2026-10-27T22:00:03Z.
const NOW: u64 = 1_793_138_403_000;

fn actor(id: &str) -> Actor {
    Actor {
        kind: ActorKind::User,
        id: id.to_owned(),
        name: Some(format!("name of {id}")),
        device_id: Some(format!("dev_{id}")),
    }
}

fn core() -> ThreadCore<MemoryStore> {
    ThreadCore::open("th_1", MemoryStore::new()).expect("open")
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

fn run_event(task: &str, run: &str, event: impl Into<Event>) -> Envelope {
    envelope(Some(task), Some(run), event)
}

fn started(task: &str, run: &str) -> Envelope {
    run_event(
        task,
        run,
        RunStarted {
            reason: RunReason::Prompt,
            parent_run_id: None,
            agent_id: None,
            machine_id: None,
        },
    )
}

fn resume(task: &str, run: &str, parent: &str) -> Envelope {
    run_event(
        task,
        run,
        RunStarted {
            reason: RunReason::Resume,
            parent_run_id: Some(parent.to_owned()),
            agent_id: None,
            machine_id: None,
        },
    )
}

fn success(task: &str, run: &str) -> Envelope {
    run_event(
        task,
        run,
        RunFinished {
            outcome: RunOutcome::Success {
                stop_reason: Some(StopReason::EndTurn),
            },
        },
    )
}

fn text_end(task: &str, run: &str) -> Envelope {
    run_event(
        task,
        run,
        TextMessageEnd {
            message_id: "m".to_owned(),
        },
    )
}

fn text_start(task: &str, run: &str) -> Envelope {
    run_event(
        task,
        run,
        TextMessageStart {
            message_id: "m".to_owned(),
            role: Default::default(),
        },
    )
}

fn interrupt(task: &str, run: &str, id: &str, expires_ms: u64) -> Envelope {
    run_event(
        task,
        run,
        Interrupt {
            interrupt_id: id.to_owned(),
            kind: InterruptKind::Approval,
            title: "Allow?".to_owned(),
            message: None,
            expires_at: format_timestamp(expires_ms),
            tool_call_id: None,
            options: Some(vec![InterruptOption {
                option_id: "allow".to_owned(),
                name: "Allow".to_owned(),
                kind: PermissionOptionKind::AllowOnce,
            }]),
            response_schema: None,
            url: None,
        },
    )
}

fn pause(task: &str, run: &str, ids: &[&str]) -> Envelope {
    run_event(
        task,
        run,
        RunFinished {
            outcome: RunOutcome::Interrupt {
                interrupt_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            },
        },
    )
}

fn prompt(task: &str, text: &str) -> PromptBody {
    PromptBody {
        task_id: Some(task.to_owned()),
        message_id: None,
        parts: vec![ContentPart::Text {
            text: text.to_owned(),
        }],
    }
}

fn append(core: &mut ThreadCore<MemoryStore>, envelope: Envelope) -> Envelope {
    core.append(envelope, NOW).expect("append accepted")
}

/// Starts `task` through the prompt route and the runner's `RunStarted`.
fn start_task(core: &mut ThreadCore<MemoryStore>, task: &str, run: &str) {
    let reply = core
        .submit_prompt(
            &actor("usr_1"),
            &format!("idk_{task}"),
            prompt(task, "go"),
            NOW,
        )
        .expect("prompt");
    assert_eq!(reply.body["status"], "started");
    append(core, started(task, run));
}

fn types(core: &mut ThreadCore<MemoryStore>) -> Vec<(u64, &'static str)> {
    core.take_appended()
        .iter()
        .map(|e| (e.seq, e.event.type_name()))
        .collect()
}

fn commands(core: &ThreadCore<MemoryStore>) -> Vec<Command> {
    core.store()
        .unacked_commands(100)
        .expect("commands")
        .into_iter()
        .map(|record| record.command)
        .collect()
}

// ---- seq and validation -----------------------------------------------------------------

#[test]
fn seq_is_assigned_by_the_core_and_gap_free() {
    let mut core = core();
    start_task(&mut core, "tk_1", "run_1");
    append(&mut core, text_start("tk_1", "run_1"));
    append(&mut core, text_end("tk_1", "run_1"));
    let seqs: Vec<u64> = core.take_appended().iter().map(|e| e.seq).collect();
    assert_eq!(seqs, [1, 2, 3, 4]);
    assert_eq!(core.head_seq(), 4);

    // A producer that sends its own seq is refused and the log is untouched.
    let mut stamped = success("tk_1", "run_1");
    stamped.seq = 9;
    let error = core.append(stamped, NOW).expect_err("seq must be 0");
    assert_eq!((error.status, error.code), (422, "invalid_sequence"));
    assert_eq!(core.head_seq(), 4);
    assert_eq!(append(&mut core, success("tk_1", "run_1")).seq, 5);
}

#[test]
fn section_5_violations_are_rejected_with_422_and_leave_the_log_unchanged() {
    let mut core = core();
    // RunStarted outside any task (a task opens with UserMessage).
    let error = core
        .append(started("tk_1", "run_1"), NOW)
        .expect_err("no task");
    assert_eq!((error.status, error.code), (422, "invalid_sequence"));
    assert_eq!(core.head_seq(), 0);

    start_task(&mut core, "tk_1", "run_1");
    // Content for a message that was never started.
    let content = run_event(
        "tk_1",
        "run_1",
        protocol::turn_event::TextMessageContent {
            message_id: "nope".into(),
            delta: "x".into(),
        },
    );
    assert_eq!(core.append(content, NOW).expect_err("rule 4").status, 422);
    // A second task while one runs (rule 2).
    let second = envelope(
        Some("tk_2"),
        None,
        protocol::turn_event::UserMessage {
            message_id: "msg".into(),
            parts: vec![ContentPart::Text { text: "x".into() }],
            author: actor("usr_1"),
        },
    );
    assert_eq!(core.append(second, NOW).expect_err("rule 2").status, 422);
    // A different thread id.
    let mut foreign = text_start("tk_1", "run_1");
    foreign.thread_id = "th_other".into();
    assert_eq!(core.append(foreign, NOW).expect_err("thread").status, 400);

    assert_eq!(core.head_seq(), 2);
    // The validator is intact after the rejections: the log keeps going.
    assert_eq!(append(&mut core, text_start("tk_1", "run_1")).seq, 3);
}

#[test]
fn reopening_rebuilds_the_validator_from_the_stored_log() {
    let mut first = core();
    start_task(&mut first, "tk_1", "run_1");
    append(&mut first, text_start("tk_1", "run_1"));
    let store = first.store().clone();

    let mut second = ThreadCore::open("th_1", store).expect("reopen");
    assert_eq!(second.head_seq(), 3);
    // The open message from before the "restart" is known: ending it is valid, starting it
    // again is not.
    assert_eq!(
        second
            .append(text_start("tk_1", "run_1"), NOW)
            .expect_err("duplicate")
            .status,
        422
    );
    assert_eq!(append(&mut second, text_end("tk_1", "run_1")).seq, 4);
}

// ---- state ------------------------------------------------------------------------------

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

#[test]
fn snapshot_and_delta_build_the_state_as_of_each_seq() {
    let mut core = core();
    append(&mut core, usage_snapshot(10)); // 1
    append(&mut core, usage_delta(20)); // 2
    append(&mut core, usage_delta(30)); // 3

    assert_eq!(core.states_at(0).expect("states"), json!({}));
    assert_eq!(
        core.states_at(1).expect("states")["usage"]["context_used"],
        10
    );
    assert_eq!(
        core.states_at(2).expect("states")["usage"]["context_used"],
        20
    );
    let head = core.states_at(3).expect("states");
    assert_eq!(head["usage"]["context_used"], 30);
    assert_eq!(head["usage"]["context_size"], 1000);

    // A delta that does not apply, and a delta before any snapshot, are 422 and change nothing.
    let bad = envelope(
        None,
        None,
        StateDelta {
            scope: StateScope::Usage,
            patch: vec![PatchOp {
                op: PatchOpKind::Remove,
                path: "/missing".into(),
                from: None,
                value: None,
            }],
        },
    );
    assert_eq!(core.append(bad, NOW).expect_err("bad patch").status, 422);
    let early = envelope(
        None,
        None,
        StateDelta {
            scope: StateScope::Session,
            patch: vec![],
        },
    );
    assert_eq!(core.append(early, NOW).expect_err("rule 8").status, 422);
    assert_eq!(core.head_seq(), 3);
}

// ---- catch-up (turn-event.md §6) --------------------------------------------------------

#[test]
fn catch_up_replays_from_the_open_run_boundary() {
    let mut core = core();
    append(&mut core, usage_snapshot(10)); // 1
    start_task(&mut core, "tk_1", "run_1"); // 2 UserMessage, 3 RunStarted
    append(&mut core, usage_delta(20)); // 4
    append(&mut core, text_start("tk_1", "run_1")); // 5
    append(&mut core, text_end("tk_1", "run_1")); // 6
    core.take_appended();

    // after = 5 is inside run_1 (RunStarted at 3): replay from 3, snapshot as of 2.
    let messages = core.catch_up(5).expect("catch up").collect::<Vec<_>>();
    assert_eq!(
        messages[0],
        json!({"type": "hello", "head_seq": 6, "replay_from": 3})
    );
    assert_eq!(messages[1]["type"], "snapshot");
    assert_eq!(messages[1]["at_seq"], 2);
    assert_eq!(messages[1]["states"]["usage"]["context_used"], 10);
    let replayed: Vec<u64> = messages[2..messages.len() - 1]
        .iter()
        .map(|m| m["envelope"]["seq"].as_u64().expect("seq"))
        .collect();
    assert_eq!(replayed, [3, 4, 5, 6]);
    assert_eq!(messages.last(), Some(&json!({"type": "live"})));

    // after = 0 replays everything; the snapshot is empty.
    let all = core.catch_up(0).expect("catch up").collect::<Vec<_>>();
    assert_eq!(all[0]["replay_from"], 1);
    assert_eq!(
        all[1],
        json!({"type": "snapshot", "at_seq": 0, "states": {}})
    );
    assert_eq!(all.len(), 2 + 6 + 1);

    // The run ends at 7: a client that has seen 7 replays nothing but the snapshot at head.
    append(&mut core, success("tk_1", "run_1"));
    let done = core.catch_up(7).expect("catch up").collect::<Vec<_>>();
    assert_eq!(done[0]["replay_from"], 8);
    assert_eq!(done[1]["at_seq"], 7);
    assert_eq!(done[1]["states"]["usage"]["context_used"], 20);
    assert_eq!(done.len(), 3);

    // after = 6 is still inside the (just ended) run: boundary again.
    assert_eq!(
        core.catch_up(6).expect("catch up").collect::<Vec<_>>()[0]["replay_from"],
        3
    );
    // after beyond the head is treated as the head.
    assert_eq!(
        core.catch_up(99).expect("catch up").collect::<Vec<_>>()[0]["replay_from"],
        8
    );
}

#[test]
fn catch_up_boundary_follows_the_resume_run_of_a_paused_task() {
    let mut core = core();
    start_task(&mut core, "tk_1", "run_1"); // 1, 2
    append(&mut core, interrupt("tk_1", "run_1", "int_1", NOW + 60_000)); // 3
    append(&mut core, pause("tk_1", "run_1", &["int_1"])); // 4
    // Between the runs no run is open: a client at 4 replays from 5.
    assert_eq!(
        core.catch_up(4).expect("catch up").collect::<Vec<_>>()[0]["replay_from"],
        5
    );
    core.resolve_interrupt(
        "int_1",
        ResolvedOutcome::Selected {
            option_id: "allow".into(),
        },
        actor("usr_1"),
        NOW,
    )
    .expect("resolve"); // 5
    append(&mut core, resume("tk_1", "run_2", "run_1")); // 6
    append(&mut core, text_start("tk_1", "run_2")); // 7
    assert_eq!(
        core.catch_up(7).expect("catch up").collect::<Vec<_>>()[0]["replay_from"],
        6
    );
}

// ---- interrupts (client-api.md §5.4) ----------------------------------------------------

fn paused_with_interrupt(core: &mut ThreadCore<MemoryStore>, expires_ms: u64) {
    start_task(core, "tk_1", "run_1");
    append(core, interrupt("tk_1", "run_1", "int_1", expires_ms));
    append(core, pause("tk_1", "run_1", &["int_1"]));
    core.take_appended();
}

fn allow() -> ResolvedOutcome {
    ResolvedOutcome::Selected {
        option_id: "allow".into(),
    }
}

#[test]
fn first_interrupt_answer_wins_and_later_answers_name_the_resolver() {
    let mut core = core();
    paused_with_interrupt(&mut core, NOW + 600_000);

    let reply = core
        .answer_interrupt(&actor("usr_1"), "idk_a1", "int_1", allow(), NOW)
        .expect("first answer");
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body["interrupt_id"], "int_1");
    assert_eq!(reply.body["resolved_by"]["id"], "usr_1");
    // The hub appended InterruptResolved (task-scoped) with the resolver.
    let appended = core.take_appended();
    assert_eq!(appended.len(), 1);
    assert_eq!(appended[0].event.type_name(), "InterruptResolved");
    assert_eq!(appended[0].task_id.as_deref(), Some("tk_1"));
    assert_eq!(appended[0].run_id, None);
    // ... and queued the approval command with the client's idempotency key.
    let queued = core.store().unacked_commands(10).expect("commands");
    assert_eq!(
        queued.last().map(|c| c.idempotency_key.as_str()),
        Some("idk_a1")
    );
    assert!(matches!(
        &queued.last().map(|c| &c.command),
        Some(Command::Approval { outcome: Outcome::Selected { option_id }, .. }) if option_id == "allow"
    ));

    // A second answer from someone else: 409 naming usr_1; nothing appended.
    let error = core
        .answer_interrupt(&actor("usr_2"), "idk_a2", "int_1", allow(), NOW)
        .expect_err("second answer");
    assert_eq!((error.status, error.code), (409, "already_resolved"));
    assert_eq!(
        error.resolved_by.as_ref().map(|a| a.id.as_str()),
        Some("usr_1")
    );
    assert_eq!(error.body()["resolved_by"]["device_id"], "dev_usr_1");
    assert!(core.take_appended().is_empty());
}

#[test]
fn interrupt_answers_check_existence_expiry_and_outcome() {
    let mut core = core();
    paused_with_interrupt(&mut core, NOW + 1_000);

    let missing = core
        .answer_interrupt(&actor("usr_1"), "k", "int_9", allow(), NOW)
        .expect_err("unknown");
    assert_eq!(missing.status, 404);

    let wrong_option = core
        .answer_interrupt(
            &actor("usr_1"),
            "k",
            "int_1",
            ResolvedOutcome::Selected {
                option_id: "nope".into(),
            },
            NOW,
        )
        .expect_err("option");
    assert_eq!(wrong_option.status, 400);
    let not_a_form = core
        .answer_interrupt(
            &actor("usr_1"),
            "k",
            "int_1",
            ResolvedOutcome::Submitted { data: json!({}) },
            NOW,
        )
        .expect_err("form outcome");
    assert_eq!(not_a_form.status, 400);

    // At and after expires_at the answer is 410 and nothing is appended.
    let expired = core
        .answer_interrupt(&actor("usr_1"), "k", "int_1", allow(), NOW + 1_000)
        .expect_err("expired");
    assert_eq!((expired.status, expired.code), (410, "expired"));
    assert!(core.take_appended().is_empty());
    assert_eq!(
        core.store().unacked_commands(10).expect("commands").len(),
        1
    );
}

// ---- prompts and the pending queue (client-api.md §5.3) ---------------------------------

#[test]
fn pending_tasks_queue_while_a_task_runs_and_start_in_order() {
    let mut core = core();
    let me = actor("usr_1");

    // Idle: the hub emits UserMessage (not the runner) and queues the prompt command.
    let first = core
        .submit_prompt(&me, "idk_1", prompt("tk_1", "first"), NOW)
        .expect("start");
    assert_eq!(first.status, 202);
    assert_eq!(first.body, json!({"task_id": "tk_1", "status": "started"}));
    assert_eq!(types(&mut core), [(1, "UserMessage")]);
    append(&mut core, started("tk_1", "run_1"));

    // Running: the next two become pending, first with a StateSnapshot{thread}, then a delta.
    let second = core
        .submit_prompt(&me, "idk_2", prompt("tk_2", "second"), NOW)
        .expect("queue");
    assert_eq!(
        second.body,
        json!({"task_id": "tk_2", "status": "pending", "position": 1})
    );
    let third = core
        .submit_prompt(&me, "idk_3", prompt("tk_3", "third"), NOW)
        .expect("queue");
    assert_eq!(third.body["position"], 2);
    let events = core.take_appended();
    assert_eq!(
        events
            .iter()
            .map(|e| e.event.type_name())
            .collect::<Vec<_>>(),
        ["RunStarted", "StateSnapshot", "StateDelta"]
    );

    // List.
    let listed = core.list_pending().expect("list").body;
    let ids: Vec<&str> = listed["pending"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|p| p["task_id"].as_str())
        .collect();
    assert_eq!(ids, ["tk_2", "tk_3"]);
    assert_eq!(listed["pending"][0]["preview"], "second");
    assert_eq!(listed["pending"][0]["author"]["id"], "usr_1");
    let thread_state = core.states_at(core.head_seq()).expect("states")["thread"].clone();
    assert_eq!(
        thread_state["pending_tasks"].as_array().map(Vec::len),
        Some(2)
    );

    // Remove tk_3: 204, a delta, gone from the list; removing again is 409 task_started? No:
    // it is finished (dropped), so it is no longer pending.
    let removed = core.remove_pending("tk_3", NOW).expect("remove");
    assert_eq!((removed.status, removed.body), (204, Value::Null));
    assert_eq!(types(&mut core).last().map(|t| t.1), Some("StateDelta"));
    assert_eq!(
        core.list_pending().expect("list").body["pending"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        core.remove_pending("tk_9", NOW)
            .expect_err("unknown")
            .status,
        404
    );

    // The running task ends: tk_2 starts by itself (UserMessage + delta + prompt command).
    append(&mut core, success("tk_1", "run_1"));
    assert_eq!(
        types(&mut core),
        [(6, "RunFinished"), (7, "UserMessage"), (8, "StateDelta")]
    );
    assert!(
        core.list_pending().expect("list").body["pending"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    assert_eq!(
        core.states_at(core.head_seq()).expect("states")["thread"]["pending_tasks"],
        json!([])
    );
    let prompts: Vec<String> = commands(&core)
        .into_iter()
        .filter_map(|c| match c {
            Command::Prompt { task_id, .. } => Some(task_id),
            _ => None,
        })
        .collect();
    assert_eq!(prompts, ["tk_1", "tk_2"]);
    // A started task cannot be deleted from the queue.
    let started = core.remove_pending("tk_2", NOW).expect_err("started");
    assert_eq!((started.status, started.code), (409, "task_started"));
}

#[test]
fn prompts_validate_ids_and_parts() {
    let mut core = core();
    let me = actor("usr_1");
    let empty = PromptBody {
        task_id: None,
        message_id: None,
        parts: vec![],
    };
    assert_eq!(
        core.submit_prompt(&me, "k", empty, NOW)
            .expect_err("empty")
            .status,
        400
    );
    let bad_id = prompt("has space", "x");
    assert_eq!(
        core.submit_prompt(&me, "k", bad_id, NOW)
            .expect_err("id")
            .status,
        400
    );
    // Generated ids are valid and distinct.
    let a = core
        .submit_prompt(
            &me,
            "k1",
            PromptBody {
                task_id: None,
                message_id: None,
                parts: vec![ContentPart::Text { text: "a".into() }],
            },
            NOW,
        )
        .expect("a");
    let id = a.body["task_id"].as_str().expect("task id").to_owned();
    assert!(protocol::turn_event::is_valid_id(&id));
    assert_eq!(
        core.submit_prompt(&me, "k2", prompt(&id, "again"), NOW)
            .expect_err("reused")
            .status,
        400
    );
}

// ---- cancel (client-api.md §5.5) --------------------------------------------------------

#[test]
fn cancel_of_a_running_task_abandons_open_interrupts_and_queues_the_command() {
    let mut core = core();
    start_task(&mut core, "tk_1", "run_1");
    append(
        &mut core,
        interrupt("tk_1", "run_1", "int_1", NOW + 600_000),
    );
    core.take_appended();

    let reply = core
        .cancel(&actor("usr_2"), "idk_c", "tk_1", NOW)
        .expect("cancel");
    assert_eq!(reply.status, 202);
    assert_eq!(reply.body["status"], "cancelling");
    // int_1 is resolved as cancelled by the canceller; the runner's RunError arrives later.
    let appended = core.take_appended();
    assert_eq!(appended.len(), 1);
    match &appended[0].event {
        Event::InterruptResolved(resolved) => {
            assert_eq!(resolved.outcome, ResolvedOutcome::Cancelled);
            assert_eq!(resolved.resolved_by.id, "usr_2");
        }
        other => panic!("expected InterruptResolved, got {other:?}"),
    }
    assert!(matches!(
        commands(&core).last(),
        Some(Command::Cancel { task_id, .. }) if task_id == "tk_1"
    ));
    // A late answer to the abandoned interrupt is 409.
    assert_eq!(
        core.answer_interrupt(&actor("usr_1"), "k", "int_1", allow(), NOW)
            .expect_err("late")
            .status,
        409
    );
    // The runner ends the task with RunError{cancelled}: accepted through append.
    append(
        &mut core,
        run_event(
            "tk_1",
            "run_1",
            RunError {
                reason: RunErrorReason::Cancelled,
                code: None,
                message: "cancelled".into(),
            },
        ),
    );
    // Cancelling a finished task changes nothing.
    let again = core
        .cancel(&actor("usr_2"), "idk_c2", "tk_1", NOW)
        .expect("again");
    assert_eq!(
        (again.status, again.body),
        (200, json!({"status": "already_finished"}))
    );
    assert_eq!(
        core.cancel(&actor("usr_2"), "k", "tk_9", NOW)
            .expect_err("unknown")
            .status,
        404
    );
}

#[test]
fn cancel_of_a_task_the_runner_never_started_ends_it_with_run_error_cancelled() {
    let mut core = core();
    let me = actor("usr_1");
    core.submit_prompt(&me, "idk_1", prompt("tk_1", "first"), NOW)
        .expect("start");
    core.submit_prompt(&me, "idk_2", prompt("tk_2", "second"), NOW)
        .expect("queue");
    core.take_appended();

    // The prompt command was never picked up: the hub ends tk_1 itself and retracts the command.
    let reply = core.cancel(&me, "idk_c", "tk_1", NOW).expect("cancel");
    assert_eq!(reply.body["status"], "cancelled");
    let events = core.take_appended();
    let names: Vec<&str> = events.iter().map(|e| e.event.type_name()).collect();
    assert_eq!(
        names,
        ["RunStarted", "RunError", "UserMessage", "StateDelta"],
        "synthetic run, then tk_2 starts"
    );
    match &events[1].event {
        Event::RunError(error) => assert_eq!(error.reason, RunErrorReason::Cancelled),
        other => panic!("expected RunError, got {other:?}"),
    }
    let queued = commands(&core);
    assert_eq!(
        queued.len(),
        1,
        "tk_1's prompt retracted, tk_2's queued: {queued:?}"
    );
    assert!(matches!(&queued[0], Command::Prompt { task_id, .. } if task_id == "tk_2"));

    // A pending task is simply dropped from the queue by cancel.
    core.submit_prompt(&me, "idk_3", prompt("tk_3", "third"), NOW)
        .expect("queue");
    let dropped = core
        .cancel(&me, "idk_c3", "tk_3", NOW)
        .expect("cancel pending");
    assert_eq!(dropped.body["status"], "cancelled");
    assert!(
        core.list_pending().expect("list").body["pending"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
}

#[test]
fn cancel_after_the_runner_took_the_prompt_goes_to_the_runner() {
    let mut core = core();
    let me = actor("usr_1");
    core.submit_prompt(&me, "idk_1", prompt("tk_1", "first"), NOW)
        .expect("start");
    let ids: Vec<u64> = core
        .store()
        .unacked_commands(10)
        .expect("commands")
        .iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(core.ack_commands(&ids).expect("ack"), 1);
    core.take_appended();

    let reply = core.cancel(&me, "idk_c", "tk_1", NOW).expect("cancel");
    assert_eq!(reply.body["status"], "cancelling");
    assert!(core.take_appended().is_empty(), "the runner ends the task");
    assert!(matches!(commands(&core)[..], [Command::Cancel { .. }]));
}

// ---- idempotency (client-api.md §3) -----------------------------------------------------

fn request(method: &str, path: &str, user: &str, key: Option<&str>, body: Value) -> ApiRequest {
    ApiRequest {
        method: method.to_owned(),
        path: path.to_owned(),
        query: Vec::new(),
        actor: Some(actor(user)),
        idempotency_key: key.map(str::to_owned),
        body,
        dev_fixtures: false,
    }
}

#[test]
fn idempotency_replays_the_same_answer_and_rejects_a_changed_body() {
    let mut core = core();
    let path = "/v1/threads/th_1/prompts";
    let body = json!({"task_id": "tk_1", "parts": [{"type": "text", "text": "hi"}]});

    let first = api::handle(
        &mut core,
        &request("POST", path, "usr_1", Some("idk_1"), body.clone()),
        NOW,
    );
    assert_eq!(
        (first.status, &first.body),
        (202, &json!({"task_id": "tk_1", "status": "started"}))
    );
    assert_eq!(core.take_appended().len(), 1);

    // Same key, same body: same answer, no new effect (no event, no command).
    let replay = api::handle(
        &mut core,
        &request("POST", path, "usr_1", Some("idk_1"), body.clone()),
        NOW + 5,
    );
    assert_eq!(replay, first);
    assert!(core.take_appended().is_empty());
    assert_eq!(commands(&core).len(), 1);
    assert_eq!(core.head_seq(), 1);

    // Same key, different body: 422 idempotency_mismatch.
    let other = json!({"task_id": "tk_2", "parts": [{"type": "text", "text": "other"}]});
    let mismatch = api::handle(
        &mut core,
        &request("POST", path, "usr_1", Some("idk_1"), other.clone()),
        NOW,
    );
    assert_eq!(mismatch.status, 422);
    assert_eq!(mismatch.body["error"], "idempotency_mismatch");

    // Keys are per user: another user's identical key is a fresh request.
    let theirs = api::handle(
        &mut core,
        &request("POST", path, "usr_2", Some("idk_1"), other),
        NOW,
    );
    assert_eq!(theirs.status, 202);
    assert_eq!(theirs.body["status"], "pending");

    // After 24 hours the key is free again.
    let later = NOW + IDEMPOTENCY_TTL_MS + 1;
    let stale = api::handle(
        &mut core,
        &request(
            "POST",
            path,
            "usr_1",
            Some("idk_1"),
            json!({"task_id": "tk_3", "parts": [{"type": "text", "text": "later"}]}),
        ),
        later,
    );
    assert_eq!(stale.status, 202);
}

#[test]
fn writes_need_a_valid_idempotency_key_and_errors_have_the_api_shape() {
    let mut core = core();
    let path = "/v1/threads/th_1/prompts";
    let body = json!({"parts": [{"type": "text", "text": "hi"}]});

    let missing = api::handle(
        &mut core,
        &request("POST", path, "usr_1", None, body.clone()),
        NOW,
    );
    assert_eq!(missing.status, 400);
    assert_eq!(missing.body["error"], "idempotency_key_required");
    let bad = api::handle(
        &mut core,
        &request("POST", path, "usr_1", Some("has space"), body.clone()),
        NOW,
    );
    assert_eq!(
        (bad.status, bad.body["error"].as_str()),
        (400, Some("invalid_request"))
    );
    // A client cannot send a key that reads as a locally typed prompt (client-api.md §6).
    let local = api::handle(
        &mut core,
        &request("POST", path, "usr_1", Some("local:1"), body.clone()),
        NOW,
    );
    assert_eq!(
        (local.status, local.body["error"].as_str()),
        (400, Some("invalid_request"))
    );
    let malformed = api::handle(
        &mut core,
        &request("POST", path, "usr_1", Some("k"), json!({"parts": "x"})),
        NOW,
    );
    assert_eq!(malformed.status, 400);
    assert!(malformed.body["message"].is_string());
    let unknown = api::handle(
        &mut core,
        &request("GET", "/v1/threads/th_1/nope", "usr_1", None, Value::Null),
        NOW,
    );
    assert_eq!(
        (unknown.status, unknown.body["error"].as_str()),
        (404, Some("not_found"))
    );
    // A failed attempt is not stored: the same key can be retried.
    let missing_task = request(
        "POST",
        "/v1/threads/th_1/cancel",
        "usr_1",
        Some("idk_x"),
        json!({"task_id": "tk_1"}),
    );
    assert_eq!(api::handle(&mut core, &missing_task, NOW).status, 404);
    api::handle(
        &mut core,
        &request(
            "POST",
            path,
            "usr_1",
            Some("idk_p"),
            json!({"task_id": "tk_1", "parts": [{"type": "text", "text": "x"}]}),
        ),
        NOW,
    );
    assert_eq!(api::handle(&mut core, &missing_task, NOW).status, 202);
}

#[test]
fn interrupt_route_replays_the_winner_and_409s_other_keys() {
    let mut core = core();
    paused_with_interrupt(&mut core, NOW + 600_000);
    let path = "/v1/threads/th_1/interrupts/int_1";
    let body = json!({"outcome": {"type": "selected", "option_id": "allow"}});

    let first = api::handle(
        &mut core,
        &request("POST", path, "usr_1", Some("idk_a"), body.clone()),
        NOW,
    );
    assert_eq!(first.status, 200);
    let replay = api::handle(
        &mut core,
        &request("POST", path, "usr_1", Some("idk_a"), body.clone()),
        NOW,
    );
    assert_eq!(replay, first, "the winner's retry gets its own 200 back");
    let loser = api::handle(
        &mut core,
        &request("POST", path, "usr_2", Some("idk_b"), body),
        NOW,
    );
    assert_eq!(loser.status, 409);
    assert_eq!(loser.body["error"], "already_resolved");
    assert_eq!(loser.body["resolved_by"]["id"], "usr_1");
}

fn machine_prompt(core: &mut ThreadCore<MemoryStore>, task: &str, text: &str) -> api::ApiResponse {
    let request = request(
        "POST",
        "/internal/threads/th_1/local-prompts",
        "ignored",
        None,
        json!({
            "task_id": task,
            "message_id": format!("msg_{task}"),
            "parts": [{"type": "text", "text": text}],
            "actor": actor("local:mch_1"),
        }),
    );
    api::handle(core, &request, NOW)
}

fn prompt_tasks(core: &ThreadCore<MemoryStore>) -> Vec<String> {
    commands(core)
        .into_iter()
        .filter_map(|c| match c {
            Command::Prompt { task_id, .. } => Some(task_id),
            _ => None,
        })
        .collect()
}

#[test]
fn a_terminal_prompt_is_queued_like_a_phone_prompt_and_comes_back_in_queue_order() {
    let mut core = core();
    let me = actor("usr_1");
    // Idle: the terminal's prompt starts at once, authored by the local user.
    let first = machine_prompt(&mut core, "tk_loc1", "typed here");
    assert_eq!(first.status, 202);
    assert_eq!(first.body["status"], "started");
    assert_eq!(prompt_tasks(&core), ["tk_loc1"]);
    let message = core.take_appended();
    assert_eq!(message[0].event.type_name(), "UserMessage");
    assert_eq!(message[0].task_id.as_deref(), Some("tk_loc1"));
    append(&mut core, started("tk_loc1", "run_1"));

    // Running: a phone prompt, a terminal prompt and another phone prompt queue in the order
    // they reached the hub, whoever sent them.
    core.submit_prompt(&me, "idk_p1", prompt("tk_ph1", "phone one"), NOW)
        .expect("phone");
    let local = machine_prompt(&mut core, "tk_loc2", "typed here too");
    assert_eq!(
        local.body,
        json!({"task_id": "tk_loc2", "status": "pending", "position": 2})
    );
    core.submit_prompt(&me, "idk_p2", prompt("tk_ph2", "phone two"), NOW)
        .expect("phone");
    let pending = core.list_pending().expect("list").body;
    let ids: Vec<&str> = pending["pending"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|p| p["task_id"].as_str())
        .collect();
    assert_eq!(ids, ["tk_ph1", "tk_loc2", "tk_ph2"]);
    assert_eq!(pending["pending"][1]["author"]["id"], "local:mch_1");

    // Each task starts only when the one before ended; the commands go down in that order.
    append(&mut core, success("tk_loc1", "run_1"));
    append(&mut core, started("tk_ph1", "run_2"));
    append(&mut core, success("tk_ph1", "run_2"));
    append(&mut core, started("tk_loc2", "run_3"));
    append(&mut core, success("tk_loc2", "run_3"));
    assert_eq!(
        prompt_tasks(&core),
        ["tk_loc1", "tk_ph1", "tk_loc2", "tk_ph2"]
    );
}

#[test]
fn a_repeated_terminal_prompt_queues_one_task_and_a_bad_one_is_refused() {
    let mut core = core();
    start_task(&mut core, "tk_1", "run_1");
    let first = machine_prompt(&mut core, "tk_loc", "once");
    assert_eq!(first.body["status"], "pending");
    // A retransmit replays the stored answer: no second task, no second pending entry.
    let again = machine_prompt(&mut core, "tk_loc", "once");
    assert_eq!((again.status, again.body), (first.status, first.body));
    assert_eq!(
        core.list_pending().expect("list").body["pending"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    // The same task id with another text is refused, an empty prompt too.
    assert_eq!(machine_prompt(&mut core, "tk_loc", "other").status, 422);
    let request = request(
        "POST",
        "/internal/threads/th_1/local-prompts",
        "ignored",
        None,
        json!({"task_id": "tk_e", "message_id": "msg_e", "parts": [], "actor": actor("local:mch_1")}),
    );
    assert_eq!(api::handle(&mut core, &request, NOW).status, 400);
}

fn approvals(core: &ThreadCore<MemoryStore>) -> Vec<Command> {
    commands(core)
        .into_iter()
        .filter(|command| matches!(command, Command::Approval { .. }))
        .collect()
}

fn machine_answer(
    core: &mut ThreadCore<MemoryStore>,
    answer_id: &str,
    interrupt_id: &str,
) -> Value {
    let request = request(
        "POST",
        "/internal/threads/th_1/interrupt-answers",
        "ignored",
        None,
        json!({
            "answer_id": answer_id,
            "interrupt_id": interrupt_id,
            "outcome": {"type": "selected", "option_id": "allow"},
            "actor": actor("local:mch_1"),
        }),
    );
    let response = api::handle(core, &request, NOW);
    assert_eq!(response.status, 200, "{:?}", response.body);
    response.body
}

#[test]
fn a_terminal_answer_that_wins_is_resolved_like_the_phone_route_and_the_phone_then_gets_409() {
    let mut core = core();
    paused_with_interrupt(&mut core, NOW + 600_000);

    let verdict = machine_answer(&mut core, "ans_1", "int_1");
    assert_eq!(verdict["status"], "accepted");
    assert_eq!(verdict["resolved_by"]["id"], "local:mch_1");
    let appended = core.take_appended();
    assert_eq!(appended.len(), 1);
    assert_eq!(appended[0].event.type_name(), "InterruptResolved");
    // The approval command goes to the runner like a phone's answer would.
    assert!(matches!(
        &approvals(&core)[..],
        [Command::Approval { interrupt_id, outcome: Outcome::Selected { option_id }, resolved_by, .. }]
            if interrupt_id == "int_1" && option_id == "allow" && resolved_by.id == "local:mch_1"
    ));

    // The phone's POST now loses, naming the terminal.
    let loser = api::handle(
        &mut core,
        &request(
            "POST",
            "/v1/threads/th_1/interrupts/int_1",
            "usr_1",
            Some("idk_p"),
            json!({"outcome": {"type": "selected", "option_id": "allow"}}),
        ),
        NOW,
    );
    assert_eq!(loser.status, 409);
    assert_eq!(loser.body["resolved_by"]["id"], "local:mch_1");
    assert_eq!(
        approvals(&core).len(),
        1,
        "only the winner's command is queued"
    );
}

#[test]
fn a_terminal_answer_that_loses_to_the_phone_is_refused_with_the_winner_and_changes_nothing() {
    let mut core = core();
    paused_with_interrupt(&mut core, NOW + 600_000);
    core.answer_interrupt(&actor("usr_1"), "idk_a1", "int_1", allow(), NOW)
        .expect("phone answer");
    core.take_appended();

    let verdict = machine_answer(&mut core, "ans_1", "int_1");
    assert_eq!(verdict["status"], "already_resolved");
    assert_eq!(verdict["resolved_by"]["id"], "usr_1");
    assert!(core.take_appended().is_empty());
    assert!(
        matches!(
            &approvals(&core)[..],
            [Command::Approval { outcome: Outcome::Selected { option_id }, resolved_by, .. }]
                if option_id == "allow" && resolved_by.id == "usr_1"
        ),
        "the agent is told the phone's choice only"
    );
}

#[test]
fn a_repeated_terminal_answer_gets_its_first_verdict_and_an_unknown_interrupt_is_rejected() {
    let mut core = core();
    paused_with_interrupt(&mut core, NOW + 600_000);
    let first = machine_answer(&mut core, "ans_1", "int_1");
    let again = machine_answer(&mut core, "ans_1", "int_1");
    assert_eq!(
        first, again,
        "a retransmit does not turn the winner into a loser"
    );
    assert_eq!(approvals(&core).len(), 1);

    let rejected = machine_answer(&mut core, "ans_2", "int_9");
    assert_eq!(rejected["status"], "rejected");
    assert!(rejected["resolved_by"].is_null());
    assert!(
        rejected["message"]
            .as_str()
            .is_some_and(|m| m.contains("int_9"))
    );
}

#[test]
fn dev_and_internal_routes() {
    let mut core = core();
    let envelope = serde_json::to_value(text_start("tk_1", "run_1")).expect("json");

    // The dev route is a 404 unless DEV_FIXTURES is on.
    let mut dev = request(
        "POST",
        "/v1/dev/threads/th_1/events",
        "usr_1",
        None,
        envelope.clone(),
    );
    assert_eq!(api::handle(&mut core, &dev, NOW).status, 404);
    dev.dev_fixtures = true;
    // Not valid yet (no task): 422.
    assert_eq!(api::handle(&mut core, &dev, NOW).status, 422);

    // Internal batch: UserMessage is hub-emitted via the prompt route; the runner's events come in.
    start_task(&mut core, "tk_1", "run_1");
    let batch = json!({"envelopes": [envelope, serde_json::to_value(text_end("tk_1", "run_1")).expect("json")]});
    let internal = request(
        "POST",
        "/internal/threads/th_1/events",
        "usr_1",
        None,
        batch,
    );
    let stored = api::handle(&mut core, &internal, NOW);
    assert_eq!(stored.status, 200);
    let seqs: Vec<u64> = stored.body["stored"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|e| e["seq"].as_u64())
        .collect();
    assert_eq!(seqs, [3, 4]);

    // Commands: list, ack, list again.
    let list = request(
        "GET",
        "/internal/threads/th_1/commands",
        "usr_1",
        None,
        Value::Null,
    );
    let commands = api::handle(&mut core, &list, NOW);
    let first = &commands.body["commands"][0];
    assert_eq!(first["command"]["kind"], "prompt");
    assert_eq!(first["idempotency_key"], "idk_tk_1");
    let ack = request(
        "POST",
        "/internal/threads/th_1/commands/ack",
        "usr_1",
        None,
        json!({"ids": [first["id"]]}),
    );
    assert_eq!(api::handle(&mut core, &ack, NOW).body["acked"], 1);
    assert_eq!(
        api::handle(&mut core, &list, NOW).body["commands"],
        json!([])
    );

    // Init publishes the thread state; GET /v1/threads/{id} reports it.
    let init = request(
        "POST",
        "/internal/threads/th_1/init",
        "usr_1",
        None,
        json!({"machine_id": "mch_1", "agent": "claude", "title": "Upgrade axum"}),
    );
    assert_eq!(api::handle(&mut core, &init, NOW).status, 200);
    let info = api::handle(
        &mut core,
        &request("GET", "/v1/threads/th_1", "usr_1", None, Value::Null),
        NOW,
    )
    .body;
    assert_eq!(info["title"], "Upgrade axum");
    assert_eq!(info["machine_id"], "mch_1");
    assert_eq!(info["head_seq"], core.head_seq());
    assert_eq!(info["states"]["thread"]["title"], "Upgrade axum");
}

#[test]
fn an_untouched_thread_is_a_404() {
    let core = core();
    assert_eq!(core.thread_info().expect_err("empty").status, 404);
}

#[test]
fn event_messages_wrap_the_envelope() {
    let mut core = core();
    let stored = append(&mut core, usage_snapshot(1));
    let message = event_message(&stored);
    assert_eq!(message["type"], "event");
    assert_eq!(message["envelope"]["seq"], 1);
    assert_eq!(message["envelope"]["event"]["type"], "StateSnapshot");
}

// ---- M2-008c: event_id dedupe (tunnel.md §4.2) and POST /v1/threads ------------------------

/// `{event_id, envelope}`: the frame shape the Machine DO sends to the internal events route.
fn framed(event_id: &str, envelope: &Envelope) -> Value {
    json!({ "event_id": event_id, "envelope": envelope })
}

#[test]
fn a_repeated_event_id_is_acknowledged_without_a_second_append() {
    let mut core = core();
    start_task(&mut core, "tk_1", "run_1");
    core.take_appended();

    let body = framed("evt_1", &text_start("tk_1", "run_1"));
    let first = api::handle(
        &mut core,
        &request(
            "POST",
            "/internal/threads/th_1/events",
            "usr_1",
            None,
            body.clone(),
        ),
        NOW,
    );
    assert_eq!(first.status, 200, "{:?}", first.body);
    assert_eq!(first.body["stored"][0]["seq"], 3);
    let head = core.head_seq();

    // A retransmit inside the 24 h window: 200, nothing appended, nothing broadcast.
    let again = api::handle(
        &mut core,
        &request(
            "POST",
            "/internal/threads/th_1/events",
            "usr_1",
            None,
            body.clone(),
        ),
        NOW + 60_000,
    );
    assert_eq!(again.status, 200);
    assert_eq!(again.body, json!({ "stored": [], "duplicate": true }));
    assert_eq!(core.head_seq(), head);
    assert_eq!(
        core.take_appended().len(),
        1,
        "only the first delivery appended"
    );

    // The dev route runs the same handler, so the same duplicate.
    let mut dev = request("POST", "/v1/dev/threads/th_1/events", "usr_1", None, body);
    dev.dev_fixtures = true;
    assert_eq!(
        api::handle(&mut core, &dev, NOW + 120_000).body["duplicate"],
        true
    );
    assert_eq!(core.head_seq(), head);
}

#[test]
fn event_ids_are_remembered_for_24_hours_and_a_failed_append_is_not() {
    let mut core = core();
    start_task(&mut core, "tk_1", "run_1");

    // A refused envelope (its run is unknown) leaves its event_id unrecorded.
    let bad = framed(
        "evt_bad",
        &Envelope::new(
            "th_1",
            Some("tk_1".to_owned()),
            Some("run_missing".to_owned()),
            format_timestamp(NOW),
            TextMessageStart {
                message_id: "m".to_owned(),
                role: Default::default(),
            },
        ),
    );
    let refused = api::handle(
        &mut core,
        &request("POST", "/internal/threads/th_1/events", "usr_1", None, bad),
        NOW,
    );
    assert_eq!(refused.status, 422);
    assert!(!core.store().event_seen("evt_bad", NOW).expect("seen"));

    let ok = framed("evt_ok", &text_start("tk_1", "run_1"));
    let stored = api::handle(
        &mut core,
        &request("POST", "/internal/threads/th_1/events", "usr_1", None, ok),
        NOW,
    );
    assert_eq!(stored.status, 200, "{:?}", stored.body);
    // Remembered inside the window, forgotten at its end.
    assert!(
        core.store()
            .event_seen("evt_ok", NOW + EVENT_ID_TTL_MS - 1)
            .expect("seen")
    );
    assert!(
        !core
            .store()
            .event_seen("evt_ok", NOW + EVENT_ID_TTL_MS)
            .expect("seen")
    );

    // A malformed event_id is a 400, not a dedupe key.
    let malformed = json!({ "event_id": "not valid!", "envelope": {} });
    let reply = api::handle(
        &mut core,
        &request(
            "POST",
            "/internal/threads/th_1/events",
            "usr_1",
            None,
            malformed,
        ),
        NOW,
    );
    assert_eq!(reply.status, 400);
}

fn create_body(prompt: bool) -> Value {
    let mut body = json!({
        "machine_id": "mch_1",
        "agent": "claude",
        "workspace": { "path": "/tmp/work", "spawn": "worktree" },
        "permission_mode": "default",
        "title": "Upgrade axum",
    });
    if prompt {
        body["prompt"] = json!({
            "task_id": "tk_1",
            "message_id": "msg_1",
            "parts": [{ "type": "text", "text": "Upgrade axum to 0.8" }],
        });
    }
    body
}

fn create(core: &mut ThreadCore<MemoryStore>, key: &str, body: Value) -> api::ApiResponse {
    api::handle(
        core,
        &request(
            "POST",
            "/internal/threads/th_1/create",
            "usr_1",
            Some(key),
            body,
        ),
        NOW,
    )
}

/// The Machine DO's view of the thread's unacked commands.
fn queued(core: &mut ThreadCore<MemoryStore>) -> Vec<Value> {
    api::handle(
        core,
        &request(
            "GET",
            "/internal/threads/th_1/commands",
            "usr_1",
            None,
            Value::Null,
        ),
        NOW,
    )
    .body["commands"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn creating_a_thread_records_the_host_and_queues_the_dispatch_of_its_first_task() {
    let mut core = core();
    let reply = create(&mut core, "idk_create_1", create_body(true));
    assert_eq!(reply.status, 201, "{:?}", reply.body);
    assert_eq!(
        reply.body,
        json!({ "thread_id": "th_1", "task_id": "tk_1", "status": "started" })
    );

    // Thread state (the init hook), then the hub's UserMessage.
    let info = api::handle(
        &mut core,
        &request("GET", "/v1/threads/th_1", "usr_1", None, Value::Null),
        NOW,
    )
    .body;
    assert_eq!(info["machine_id"], "mch_1");
    assert_eq!(info["title"], "Upgrade axum");
    assert_eq!(info["head_seq"], 2);
    assert_eq!(info["states"]["thread"]["machine_id"], "mch_1");
    let types: Vec<&str> = core
        .store()
        .read_range(1, 2)
        .expect("range")
        .iter()
        .map(|e| e.event.type_name())
        .collect();
    assert_eq!(types, ["StateSnapshot", "UserMessage"]);

    let commands = queued(&mut core);
    assert_eq!(commands.len(), 1);
    let command = &commands[0];
    assert_eq!(command["machine_id"], "mch_1");
    assert_eq!(command["idempotency_key"], "idk_create_1");
    assert_eq!(command["command"]["kind"], "dispatch");
    assert_eq!(command["command"]["thread_id"], "th_1");
    assert_eq!(command["command"]["task_id"], "tk_1");
    assert_eq!(command["command"]["agent"], "claude");
    assert_eq!(command["command"]["workspace"]["path"], "/tmp/work");
    assert_eq!(command["command"]["permission_mode"], "default");
    assert_eq!(command["command"]["message_id"], "msg_1");
    assert_eq!(command["command"]["author"]["id"], "usr_1");

    // Replay: the stored answer, no second thread, no second command.
    let replay = create(&mut core, "idk_create_1", create_body(true));
    assert_eq!(replay, reply);
    assert_eq!(core.head_seq(), 2);
    assert_eq!(queued(&mut core).len(), 1);

    // The same key with another body is refused.
    let mut other = create_body(true);
    other["title"] = json!("Something else");
    assert_eq!(create(&mut core, "idk_create_1", other).status, 422);
}

#[test]
fn a_thread_created_without_a_prompt_is_idle_and_queues_nothing() {
    let mut core = core();
    let reply = create(&mut core, "idk_idle", create_body(false));
    assert_eq!(reply.status, 201, "{:?}", reply.body);
    assert_eq!(
        reply.body,
        json!({ "thread_id": "th_1", "task_id": null, "status": "idle" })
    );
    assert_eq!(core.head_seq(), 1);
    assert!(queued(&mut core).is_empty());
    // A second create of the same thread is a conflict.
    assert_eq!(
        create(&mut core, "idk_again", create_body(false)).status,
        409
    );
}

#[test]
fn a_rejected_create_writes_nothing() {
    let mut core = core();
    let mut bad = create_body(true);
    bad["machine_id"] = json!("not valid!");
    assert_eq!(create(&mut core, "idk_bad", bad).status, 400);
    let mut no_parts = create_body(true);
    no_parts["prompt"]["parts"] = json!([]);
    assert_eq!(create(&mut core, "idk_bad_2", no_parts).status, 400);
    assert_eq!(core.head_seq(), 0);
    assert!(queued(&mut core).is_empty());
    assert_eq!(
        api::handle(
            &mut core,
            &request("GET", "/v1/threads/th_1", "usr_1", None, Value::Null),
            NOW,
        )
        .status,
        404
    );
}

#[test]
fn cancelling_a_dispatch_the_runner_never_received_retracts_it() {
    let mut core = core();
    assert_eq!(
        create(&mut core, "idk_create_x", create_body(true)).status,
        201
    );
    assert_eq!(queued(&mut core).len(), 1);
    let cancel = request(
        "POST",
        "/v1/threads/th_1/cancel",
        "usr_1",
        Some("idk_cancel_x"),
        json!({ "task_id": "tk_1" }),
    );
    let reply = api::handle(&mut core, &cancel, NOW);
    assert_eq!(reply.status, 202, "{:?}", reply.body);
    // The dispatch is gone; the hub ends the task itself (RunStarted + RunError cancelled).
    assert!(queued(&mut core).is_empty());
    let types: Vec<&str> = core
        .store()
        .read_range(3, 4)
        .expect("range")
        .iter()
        .map(|e| e.event.type_name())
        .collect();
    assert_eq!(types, ["RunStarted", "RunError"]);
}

// ---- session mode and config (client-api.md §5.5a) --------------------------------------

/// A thread with a `session` state that advertises two modes and two config options (a select
/// and a boolean), as a runner's `session/new` snapshot does.
fn core_with_session_state() -> ThreadCore<MemoryStore> {
    let mut core = core();
    assert_eq!(
        create(&mut core, "idk_create_session", create_body(false)).status,
        201
    );
    let snapshot = json!({
        "available_commands": [],
        "current_mode_id": "code",
        "modes": [
            { "id": "code", "name": "Code" },
            { "id": "plan", "name": "Plan" }
        ],
        "config_options": [
            {
                "id": "model", "name": "Model", "type": "select", "currentValue": "sonnet",
                "options": [
                    { "value": "sonnet", "name": "Sonnet" },
                    { "value": "opus", "name": "Opus" }
                ]
            },
            { "id": "auto_fix", "name": "Auto fix", "type": "boolean", "currentValue": false }
        ]
    });
    append(
        &mut core,
        envelope(
            None,
            None,
            StateSnapshot {
                scope: StateScope::Session,
                snapshot: snapshot.as_object().cloned().unwrap_or_default(),
            },
        ),
    );
    core
}

fn session_route(key: &str, body: Value) -> ApiRequest {
    request("POST", "/v1/threads/th_1/session", "usr_1", Some(key), body)
}

/// The `session_config` commands the Machine DO would see, as their wire form.
fn queued_session_configs(core: &mut ThreadCore<MemoryStore>) -> Vec<Value> {
    queued(core)
        .into_iter()
        .filter(|command| command["command"]["kind"] == "session_config")
        .collect()
}

#[test]
fn a_session_change_is_checked_against_the_advertised_state_and_queues_a_command() {
    let mut core = core_with_session_state();

    // Each of these is refused with 400 and queues nothing.
    let refused = [
        json!({}),
        json!({ "mode_id": "nope" }),
        json!({ "config": { "id": "nope", "value": "opus" } }),
        json!({ "config": { "id": "model", "value": "gpt" } }),
        json!({ "config": { "id": "auto_fix", "value": "yes" } }),
        json!({ "config": { "id": "model" } }),
    ];
    for (index, body) in refused.into_iter().enumerate() {
        let key = format!("idk_refused_{index}");
        let reply = api::handle(&mut core, &session_route(&key, body.clone()), NOW);
        assert_eq!(reply.status, 400, "{body} was accepted: {:?}", reply.body);
        assert_eq!(reply.body["error"], "invalid_request", "{body}");
    }
    assert!(
        queued_session_configs(&mut core).is_empty(),
        "a refused change queued"
    );

    // A valid mode and config option: 202, one command, nothing else.
    let body = json!({ "mode_id": "plan", "config": { "id": "model", "value": "opus" } });
    let reply = api::handle(
        &mut core,
        &session_route("idk_session_ok", body.clone()),
        NOW,
    );
    assert_eq!(reply.status, 202, "{:?}", reply.body);
    let commands = queued_session_configs(&mut core);
    assert_eq!(commands.len(), 1, "{commands:?}");
    let command = &commands[0]["command"];
    assert_eq!(command["thread_id"], "th_1");
    assert_eq!(command["mode_id"], "plan");
    assert_eq!(command["config"], json!({ "id": "model", "value": "opus" }));
    assert_eq!(commands[0]["idempotency_key"], "idk_session_ok");

    // A boolean option alone, with no mode.
    let flag = json!({ "config": { "id": "auto_fix", "value": true } });
    let reply = api::handle(&mut core, &session_route("idk_session_flag", flag), NOW);
    assert_eq!(reply.status, 202, "{:?}", reply.body);
    let commands = queued_session_configs(&mut core);
    assert_eq!(commands.len(), 2);
    assert!(
        commands[1]["command"]
            .get("mode_id")
            .is_none_or(Value::is_null)
    );
    assert_eq!(commands[1]["command"]["config"]["value"], true);
}

#[test]
fn a_session_change_replays_the_same_answer_and_queues_once() {
    let mut core = core_with_session_state();
    let body = json!({ "mode_id": "plan" });
    let first = api::handle(
        &mut core,
        &session_route("idk_session_replay", body.clone()),
        NOW,
    );
    assert_eq!(first.status, 202);
    let replay = api::handle(&mut core, &session_route("idk_session_replay", body), NOW);
    assert_eq!(replay, first, "the stored answer is replayed");
    assert_eq!(
        queued_session_configs(&mut core).len(),
        1,
        "no second command"
    );

    // The same key with another body is refused, and nothing is queued for it.
    let changed = json!({ "mode_id": "code" });
    let mismatch = api::handle(
        &mut core,
        &session_route("idk_session_replay", changed),
        NOW,
    );
    assert_eq!(mismatch.status, 422);
    assert_eq!(queued_session_configs(&mut core).len(), 1);
}

#[test]
fn a_session_change_needs_a_known_thread_and_a_key() {
    // No thread at all: 404, nothing queued.
    let mut empty = core();
    let reply = api::handle(
        &mut empty,
        &session_route("idk_session_none", json!({ "mode_id": "plan" })),
        NOW,
    );
    assert_eq!(reply.status, 404, "{:?}", reply.body);
    assert!(queued(&mut empty).is_empty());

    // A thread with no session state yet: nothing is advertised, so any change is refused.
    let mut bare = core();
    assert_eq!(
        create(&mut bare, "idk_create_bare", create_body(false)).status,
        201
    );
    let reply = api::handle(
        &mut bare,
        &session_route("idk_session_bare", json!({ "mode_id": "plan" })),
        NOW,
    );
    assert_eq!(reply.status, 400, "{:?}", reply.body);
    assert!(queued_session_configs(&mut bare).is_empty());

    // Without an Idempotency-Key the write is refused.
    let mut keyed = core_with_session_state();
    let no_key = request(
        "POST",
        "/v1/threads/th_1/session",
        "usr_1",
        None,
        json!({ "mode_id": "plan" }),
    );
    assert_eq!(api::handle(&mut keyed, &no_key, NOW).status, 400);
    assert!(queued_session_configs(&mut keyed).is_empty());
}

// ---- titles, archive, delete and the task index (client-api.md §5.2, M2-010) ---------------

fn created(core: &mut ThreadCore<MemoryStore>, body: Value) {
    let body: super::CreateThreadBody = serde_json::from_value(body).expect("create body");
    core.create_thread(&actor("usr_1"), "idk_create", body, NOW)
        .expect("create");
}

fn title_of(core: &ThreadCore<MemoryStore>) -> (String, &'static str) {
    let row = core.index_row(NOW).expect("index row");
    (row.title, row.title_source)
}

fn rename(title: Option<&str>, archived: Option<bool>) -> super::core::UpdateThreadBody {
    super::core::UpdateThreadBody {
        title: title.map(str::to_owned),
        archived,
    }
}

#[test]
fn the_auto_title_is_the_first_prompts_first_line_and_a_client_title_wins() {
    let mut core = core();
    let mut body = create_body(true);
    body.as_object_mut().expect("object").remove("title");
    body["prompt"]["parts"][0]["text"] = json!("# Upgrade axum to 0.8\nthen run the tests");
    created(&mut core, body);
    assert_eq!(
        title_of(&core),
        ("Upgrade axum to 0.8".to_owned(), "derived")
    );

    let reply = core
        .update_thread(rename(Some("  Axum upgrade  "), None), NOW)
        .expect("rename");
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body["title"], "Axum upgrade");
    assert_eq!(title_of(&core), ("Axum upgrade".to_owned(), "user"));
}

#[test]
fn a_title_given_at_creation_is_the_clients_and_is_never_replaced() {
    let mut core = core();
    created(&mut core, create_body(true));
    assert_eq!(title_of(&core), ("Upgrade axum".to_owned(), "user"));
}

#[test]
fn a_rename_is_a_thread_state_delta_for_subscribers() {
    let mut core = core();
    created(&mut core, create_body(false));
    core.take_appended();
    core.update_thread(rename(Some("New name"), None), NOW)
        .expect("rename");
    let appended = core.take_appended();
    assert!(
        appended.iter().any(|envelope| matches!(
            &envelope.event,
            Event::StateDelta(delta) if delta.scope == StateScope::Thread
        )),
        "a thread StateDelta: {appended:?}"
    );
}

#[test]
fn archive_and_delete_are_reflected_in_the_index_row() {
    let mut core = core();
    created(&mut core, create_body(false));
    core.update_thread(rename(None, Some(true)), NOW)
        .expect("archive");
    assert!(core.index_row(NOW).expect("row").archived);
    core.update_thread(rename(None, Some(false)), NOW)
        .expect("unarchive");
    assert!(!core.index_row(NOW).expect("row").archived);

    let deleted = core.delete_thread().expect("delete");
    assert_eq!(deleted.status, 204);
    assert!(core.index_row(NOW).expect("row").deleted);
}

#[test]
fn the_index_status_follows_the_run_and_its_approvals() {
    let mut core = core();
    created(&mut core, create_body(true));
    assert_eq!(core.index_row(NOW).expect("row").status, "running");
    append(&mut core, started("tk_1", "run_1"));
    append(&mut core, interrupt("tk_1", "run_1", "int_1", NOW + 60_000));
    assert_eq!(core.index_row(NOW).expect("row").status, "waiting");
    let answer = core
        .answer_interrupt(
            &actor("usr_2"),
            "idk_answer",
            "int_1",
            ResolvedOutcome::Selected {
                option_id: "allow".to_owned(),
            },
            NOW,
        )
        .expect("answer");
    assert_eq!(answer.status, 200);
    assert_eq!(core.index_row(NOW).expect("row").status, "running");
    append(&mut core, success("tk_1", "run_1"));
    assert_eq!(core.index_row(NOW).expect("row").status, "done");
}

#[test]
fn a_thread_is_done_after_its_last_task_succeeds_until_the_next_prompt() {
    let mut core = core();
    created(&mut core, create_body(false));
    assert_eq!(core.index_row(NOW).expect("row").status, "open");
    start_task(&mut core, "tk_1", "run_1");
    assert_eq!(core.index_row(NOW).expect("row").status, "running");
    // A second prompt queues behind the running task: the first task's end is not `done`
    // while a task is pending (the next one starts at once).
    let queued = core
        .submit_prompt(&actor("usr_1"), "idk_tk_2", prompt("tk_2", "more"), NOW)
        .expect("queue");
    assert_eq!(queued.body["status"], "pending");
    append(&mut core, success("tk_1", "run_1"));
    assert_eq!(core.index_row(NOW).expect("row").status, "running");
    append(&mut core, started("tk_2", "run_2"));
    append(&mut core, success("tk_2", "run_2"));
    assert_eq!(core.index_row(NOW).expect("row").status, "done");
    // The next prompt makes it running again, and a failed run leaves it idle, not done.
    start_task(&mut core, "tk_3", "run_3");
    assert_eq!(core.index_row(NOW).expect("row").status, "running");
    append(
        &mut core,
        run_event(
            "tk_3",
            "run_3",
            RunError {
                reason: RunErrorReason::Error,
                code: None,
                message: "boom".to_owned(),
            },
        ),
    );
    assert_eq!(core.index_row(NOW).expect("row").status, "open");
}

#[test]
fn a_deleted_thread_is_404_not_found_on_every_client_route() {
    let mut core = core();
    created(&mut core, create_body(true));
    let routes = [
        ("GET", "/v1/threads/th_1", Value::Null),
        ("PATCH", "/v1/threads/th_1", json!({"title": "x"})),
        ("GET", "/v1/threads/th_1/pending", Value::Null),
        (
            "POST",
            "/v1/threads/th_1/prompts",
            json!({"task_id": "tk_9", "parts": [{"type": "text", "text": "hi"}]}),
        ),
        ("DELETE", "/v1/threads/th_1/pending/tk_9", Value::Null),
        (
            "POST",
            "/v1/threads/th_1/interrupts/int_1",
            json!({"outcome": {"kind": "cancelled"}}),
        ),
        (
            "POST",
            "/v1/threads/th_1/cancel",
            json!({"task_id": "tk_1"}),
        ),
        ("POST", "/v1/threads/th_1/session", json!({})),
        ("DELETE", "/v1/threads/th_1", Value::Null),
    ];
    let before = api::handle(
        &mut core,
        &request("GET", "/v1/threads/th_1", "usr_1", None, Value::Null),
        NOW,
    );
    assert_eq!(before.status, 200);
    core.delete_thread().expect("delete");
    for (method, path, body) in &routes {
        let answer = api::handle(
            &mut core,
            &request(method, path, "usr_1", Some("idk_after"), body.clone()),
            NOW,
        );
        assert_eq!(answer.status, 404, "{method} {path} after the delete");
        assert_eq!(answer.body["error"], "not_found", "{method} {path}");
    }
    assert_eq!(
        core.catch_up(0).err().map(|error| error.status),
        Some(404),
        "subscribe"
    );
    // The runner's internal route still reaches it: events of a running task are not lost.
    let internal = api::handle(
        &mut core,
        &request(
            "POST",
            "/internal/threads/th_1/reindex",
            "usr_1",
            None,
            Value::Null,
        ),
        NOW,
    );
    assert_eq!(internal.status, 204);
}

#[test]
fn rename_and_delete_of_an_unknown_thread_are_404_and_a_bad_body_is_400() {
    let mut core = core();
    assert_eq!(
        core.update_thread(rename(Some("x"), None), NOW)
            .expect_err("unknown")
            .status,
        404
    );
    assert_eq!(core.delete_thread().expect_err("unknown").status, 404);

    created(&mut core, create_body(false));
    assert_eq!(
        core.update_thread(rename(None, None), NOW)
            .expect_err("empty")
            .status,
        400
    );
    assert_eq!(
        core.update_thread(rename(Some("   "), None), NOW)
            .expect_err("blank")
            .status,
        400
    );
    let long = "x".repeat(201);
    assert_eq!(
        core.update_thread(rename(Some(&long), None), NOW)
            .expect_err("long")
            .status,
        400
    );
}

#[test]
fn the_http_layer_routes_patch_and_delete_to_the_core() {
    let mut core = core();
    created(&mut core, create_body(false));
    let request_for = |method: &str, key: Option<&str>, body: Value| ApiRequest {
        method: method.to_owned(),
        path: "/v1/threads/th_1".to_owned(),
        query: Vec::new(),
        actor: Some(actor("usr_1")),
        idempotency_key: key.map(str::to_owned),
        body,
        dev_fixtures: false,
    };
    let patch = request_for(
        "PATCH",
        None,
        json!({ "title": "Via HTTP", "archived": false }),
    );
    let answer = api::handle(&mut core, &patch, NOW);
    assert_eq!(answer.status, 200, "{:?}", answer.body);
    assert_eq!(answer.body["title"], "Via HTTP");
    let typo = request_for("PATCH", None, json!({ "tittle": "typo" }));
    assert_eq!(api::handle(&mut core, &typo, NOW).status, 400);

    let delete_without_key = request_for("DELETE", None, Value::Null);
    assert_eq!(
        api::handle(&mut core, &delete_without_key, NOW).status,
        400,
        "key required"
    );
    let delete = request_for("DELETE", Some("idk_del"), Value::Null);
    assert_eq!(api::handle(&mut core, &delete, NOW).status, 204);
    assert!(core.index_row(NOW).expect("row").deleted);
}
