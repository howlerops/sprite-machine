//! View models built from TurnEvent fixtures (`crates/protocol/schemas/turn-event/examples`),
//! with no network: the inbox status, the task transcript, machines and presence, the 409
//! resolver on an approval, and state deltas.

use std::path::PathBuf;

use client_core::TaskStatus;
use client_core::view::inbox::{InboxStatus, inbox, thread_status};
use client_core::view::machines::machines;
use client_core::view::transcript::ToolStatus;
use client_core::view::{Item, LocalAnswer, ThreadView};
use protocol::tunnel::{Actor, ActorKind};
use protocol::turn_event::{
    Activity, ActivityMode, ActivityType, DiffContent, Envelope, Event, MachinePresence,
    PlanContent, RunStarted, StateScope, StateSnapshot, TerminalContent,
};
use serde_json::{Map, Value, json};

/// Loads one example envelope from the protocol crate's schema directory.
fn example(file: &str) -> Envelope {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "protocol",
        "schemas",
        "turn-event",
        "examples",
        file,
    ]
    .iter()
    .collect();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{file}: {error}"))
}

/// The example at a sequence position, on thread `thread`.
fn at(seq: u64, file: &str) -> Envelope {
    let mut envelope = example(file);
    envelope.seq = seq;
    envelope
}

/// The examples in `files`, numbered 1..n in order.
fn sequence(files: &[&str]) -> Vec<Envelope> {
    files
        .iter()
        .enumerate()
        .map(|(index, file)| at(index as u64 + 1, file))
        .collect()
}

fn on_thread(mut envelope: Envelope, thread: &str) -> Envelope {
    envelope.thread_id = thread.to_owned();
    envelope
}

fn apply_all(view: &mut ThreadView, envelopes: &[Envelope]) {
    for envelope in envelopes {
        view.apply(envelope).expect("applies");
    }
}

fn hub_actor(id: &str) -> Actor {
    Actor {
        kind: ActorKind::User,
        id: id.to_owned(),
        name: None,
        device_id: None,
    }
}

#[test]
fn inbox_status_follows_the_task_lifecycle() {
    let steps = sequence(&[
        "UserMessage.json",
        "RunStarted.json",
        "Interrupt.json",
        "RunFinished.interrupt.json",
        "InterruptResolved.json",
        "RunStarted.resume.json",
        "RunFinished.json",
    ]);
    let mut view = ThreadView::new("th_1");
    assert_eq!(thread_status(&view), InboxStatus::Idle);

    view.apply(&steps[0]).expect("applies");
    assert_eq!(
        thread_status(&view),
        InboxStatus::Running,
        "prompt accepted, not started yet"
    );
    assert_eq!(view.tasks()[0].status(), TaskStatus::Starting);

    view.apply(&steps[1]).expect("applies");
    assert_eq!(thread_status(&view), InboxStatus::Running);

    view.apply(&steps[2]).expect("applies");
    assert_eq!(
        thread_status(&view),
        InboxStatus::Running,
        "an open interrupt alone is not a pause"
    );

    view.apply(&steps[3]).expect("applies");
    assert_eq!(thread_status(&view), InboxStatus::NeedsYou);
    assert_eq!(view.open_interrupts().len(), 1);

    view.apply(&steps[4]).expect("applies");
    assert_eq!(
        thread_status(&view),
        InboxStatus::Running,
        "answered: the resume run follows"
    );
    assert!(view.open_interrupts().is_empty());

    apply_all(&mut view, &steps[5..]);
    assert_eq!(thread_status(&view), InboxStatus::Done);
    assert_eq!(view.tasks()[0].status(), TaskStatus::Succeeded);
    assert_eq!(view.last_seq(), 7);
}

#[test]
fn inbox_lists_threads_that_need_you_first() {
    let mut waiting = ThreadView::new("th_waiting");
    apply_all(
        &mut waiting,
        &sequence(&[
            "UserMessage.json",
            "RunStarted.json",
            "Interrupt.json",
            "RunFinished.interrupt.json",
        ])
        .into_iter()
        .map(|e| on_thread(e, "th_waiting"))
        .collect::<Vec<_>>(),
    );
    let mut finished = ThreadView::new("th_done");
    apply_all(
        &mut finished,
        &sequence(&["UserMessage.json", "RunStarted.json", "RunFinished.json"])
            .into_iter()
            .map(|e| on_thread(e, "th_done"))
            .collect::<Vec<_>>(),
    );
    let idle = ThreadView::new("th_idle");

    let rows = inbox([&finished, &idle, &waiting]);
    let order: Vec<(&str, InboxStatus)> = rows
        .iter()
        .map(|row| (row.thread_id.as_str(), row.status))
        .collect();
    assert_eq!(
        order,
        vec![
            ("th_waiting", InboxStatus::NeedsYou),
            ("th_idle", InboxStatus::Idle),
            ("th_done", InboxStatus::Done),
        ]
    );
    assert_eq!(rows[0].open_interrupts, 1);
}

#[test]
fn transcript_upserts_messages_tool_calls_activities_and_terminal() {
    let steps = sequence(&[
        "UserMessage.json",
        "RunStarted.json",
        "TextMessageStart.json",
        "TextMessageContent.json",
        "TextMessageEnd.json",
        "ReasoningMessageStart.json",
        "ReasoningMessageContent.json",
        "ReasoningMessageEnd.json",
        "ToolCallStart.json",
        "ToolCallArgs.json",
        "ToolCallResult.json",
        "ToolCallEnd.json",
        "Activity.json",
        "Activity.plan.json",
        "Activity.terminal.json",
        "Checkpoint.json",
    ]);
    let mut view = ThreadView::new("th_1");
    apply_all(&mut view, &steps);
    // Upserts: the same tool call id and message id again must not add items.
    apply_all(&mut view, &steps[8..9]);
    apply_all(&mut view, &steps[2..3]);

    let items = view.tasks()[0].transcript().items();
    let Event::TextMessageContent(text) = example("TextMessageContent.json").event else {
        panic!("fixture is a text delta");
    };
    let Event::ReasoningMessageContent(reasoning) = example("ReasoningMessageContent.json").event
    else {
        panic!("fixture is a reasoning delta");
    };
    let Event::UserMessage(prompt) = example("UserMessage.json").event else {
        panic!("fixture is a user message");
    };

    let user_items = items.iter().filter(|i| matches!(i, Item::User(_))).count();
    assert_eq!(user_items, 1);
    let Some(Item::User(user)) = items.iter().find(|i| matches!(i, Item::User(_))) else {
        panic!("user message in transcript");
    };
    assert!(!user.text.is_empty());
    assert_eq!(user.author, prompt.author);

    let assistant = items
        .iter()
        .filter_map(|item| match item {
            Item::Text(view) => Some(view),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(assistant.len(), 1, "one message per message_id");
    assert_eq!(assistant[0].text, text.delta);
    assert!(!assistant[0].open, "TextMessageEnd closed it");

    let reasoning_items: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            Item::Reasoning(view) => Some(view),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning_items.len(), 1);
    assert_eq!(reasoning_items[0].text, reasoning.delta);

    let tools: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            Item::Tool(tool) => Some(tool.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(tools.len(), 1, "one tool call per tool_call_id");
    assert_eq!(
        tools[0].status,
        ToolStatus::Completed,
        "ToolCallEnd closed it"
    );
    assert!(
        tools[0].raw_input.is_some(),
        "ToolCallArgs merged into the call"
    );
    assert!(
        !tools[0].content.is_empty(),
        "ToolCallResult merged into the call"
    );
    assert!(tools[0].title.is_some(), "ToolCallStart merged its title");

    let Event::Activity(diff) = example("Activity.json").event else {
        panic!("fixture is an activity");
    };
    let expected_diff: DiffContent = diff.content_as().expect("content").expect("diff content");
    let diff_view = view.tasks()[0]
        .transcript()
        .items()
        .iter()
        .find_map(|item| match item {
            Item::Activity(activity) if activity.activity_type == ActivityType::Diff => {
                Some(activity)
            }
            _ => None,
        })
        .expect("diff activity");
    assert_eq!(
        diff_view.diff().expect("typed diff").new_text,
        expected_diff.new_text
    );

    let plan = view.tasks()[0]
        .transcript()
        .items()
        .iter()
        .find_map(|item| match item {
            Item::Activity(activity) if activity.activity_type == ActivityType::Plan => {
                activity.plan()
            }
            _ => None,
        })
        .expect("plan activity");
    let Event::Activity(plan_event) = example("Activity.plan.json").event else {
        panic!("fixture is an activity");
    };
    let expected_plan: PlanContent = plan_event.content_as().expect("content").expect("plan");
    assert_eq!(plan.entries, expected_plan.entries);

    let terminal = view.tasks()[0]
        .transcript()
        .items()
        .iter()
        .find_map(|item| match item {
            Item::Activity(activity) if activity.activity_type == ActivityType::Terminal => {
                activity.terminal()
            }
            _ => None,
        })
        .expect("terminal activity");
    let Event::Activity(terminal_event) = example("Activity.terminal.json").event else {
        panic!("fixture is an activity");
    };
    let expected: TerminalContent = terminal_event
        .content_as()
        .expect("content")
        .expect("terminal");
    assert_eq!(
        terminal.output, expected.output,
        "append adds the chunk once"
    );
    assert!(terminal.exit.is_none());

    assert!(
        items.iter().any(|item| matches!(item, Item::Checkpoint(_))),
        "the checkpoint is in the transcript"
    );
}

#[test]
fn chunked_diffs_and_terminal_output_concatenate_in_order() {
    let mut view = ThreadView::new("th_1");
    let activity = |seq: u64, id: &str, kind: ActivityType, mode: ActivityMode, content: Value| {
        let content: Map<String, Value> = content.as_object().cloned().expect("object");
        let event = Activity {
            activity_id: id.to_owned(),
            activity_type: kind,
            mode,
            content: Some(content),
            patch: None,
        };
        let mut envelope = Envelope::new(
            "th_1",
            Some("tk_1".into()),
            Some("run_1".into()),
            "2026-10-27T22:00:00.000Z",
            event,
        );
        envelope.seq = seq;
        envelope
    };
    let steps = [
        activity(
            1,
            "tc_9:big.rs",
            ActivityType::Diff,
            ActivityMode::Snapshot,
            json!({
                "tool_call_id": "tc_9", "path": "big.rs", "old_text": null,
                "new_text": "fn a() {}\n", "chunk": {"index": 0, "last": false}
            }),
        ),
        activity(
            2,
            "tc_9:big.rs",
            ActivityType::Diff,
            ActivityMode::Append,
            json!({
                "new_text": "fn b() {}\n", "chunk": {"index": 1, "last": true}
            }),
        ),
        activity(
            3,
            "term_1",
            ActivityType::Terminal,
            ActivityMode::Snapshot,
            json!({
                "terminal_id": "term_1", "command": "cargo test", "cwd": "/w", "output": ""
            }),
        ),
        activity(
            4,
            "term_1",
            ActivityType::Terminal,
            ActivityMode::Append,
            json!({"output": "ok "}),
        ),
        activity(
            5,
            "term_1",
            ActivityType::Terminal,
            ActivityMode::Append,
            json!({"output": "done"}),
        ),
    ];
    apply_all(&mut view, &steps);

    let items = view.tasks()[0].transcript().items();
    let diff = items
        .iter()
        .find_map(|item| match item {
            Item::Activity(a) if a.activity_type == ActivityType::Diff => Some(a),
            _ => None,
        })
        .expect("diff");
    assert_eq!(
        diff.diff().expect("diff").new_text,
        "fn a() {}\nfn b() {}\n"
    );
    assert!(diff.complete, "the last chunk closed the diff");

    let terminal = items
        .iter()
        .find_map(|item| match item {
            Item::Activity(a) if a.activity_type == ActivityType::Terminal => a.terminal(),
            _ => None,
        })
        .expect("terminal");
    assert_eq!(terminal.output.as_deref(), Some("ok done"));
    assert_eq!(terminal.command.as_deref(), Some("cargo test"));
}

#[test]
fn interrupt_shows_the_409_winner_until_the_hub_confirms() {
    let mut view = ThreadView::new("th_1");
    apply_all(
        &mut view,
        &sequence(&[
            "UserMessage.json",
            "RunStarted.json",
            "Interrupt.json",
            "RunFinished.interrupt.json",
        ]),
    );
    assert!(view.set_local_answer("int_1", Some(LocalAnswer::Pending)));
    let local = view.tasks()[0]
        .transcript()
        .interrupt("int_1")
        .expect("interrupt");
    assert_eq!(local.local, Some(LocalAnswer::Pending));

    // This device lost the race: the hub's 409 names the winner.
    let winner = hub_actor("usr_winner");
    assert!(view.note_conflict("int_1", winner.clone()));
    let lost = view.tasks()[0]
        .transcript()
        .interrupt("int_1")
        .expect("interrupt");
    assert!(!lost.is_open(), "a 409 means it is answered");
    assert_eq!(lost.resolved_by(), Some(&winner));
    assert!(lost.local.is_none(), "the pending answer is gone");
    assert_eq!(
        view.tasks()[0].status(),
        TaskStatus::Running,
        "no longer waiting on this device"
    );

    // The hub's InterruptResolved (resolver usr_1) is the authority once it arrives.
    view.apply(&example("InterruptResolved.json"))
        .expect("applies");
    let confirmed = view.tasks()[0]
        .transcript()
        .interrupt("int_1")
        .expect("interrupt");
    assert_eq!(
        confirmed.resolved_by().map(|a| a.id.as_str()),
        Some("usr_1")
    );

    assert!(
        !view.note_conflict("int_missing", winner),
        "unknown interrupts are reported"
    );
}

#[test]
fn machines_show_the_latest_presence_per_machine() {
    let mut laptop_asleep = ThreadView::new("th_1");
    laptop_asleep
        .apply(&example("Presence.json"))
        .expect("applies");

    let mut laptop_online = ThreadView::new("th_2");
    let mut online = example("Presence.json");
    online.thread_id = "th_2".to_owned();
    online.seq = 4;
    if let Event::Presence(presence) = &mut online.event {
        presence.state = MachinePresence::Online;
        presence.since = "2026-10-27T22:10:00.000Z".to_owned();
    }
    laptop_online.apply(&online).expect("applies");

    let mut desk = ThreadView::new("th_3");
    desk.apply(&Envelope::new(
        "th_3",
        Some("tk_1".into()),
        Some("run_1".into()),
        "2026-10-27T22:00:00.000Z",
        RunStarted {
            reason: protocol::turn_event::RunReason::Prompt,
            parent_run_id: None,
            agent_id: None,
            machine_id: Some("mch_desk".to_owned()),
        },
    ))
    .expect("applies");

    let rows = machines([&laptop_asleep, &laptop_online, &desk]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].machine_id, "mch_desk");
    assert_eq!(rows[0].presence, None, "no presence reported yet");
    assert_eq!(rows[0].thread_ids, vec!["th_3".to_owned()]);

    assert_eq!(rows[1].machine_id, "mch_laptop");
    assert_eq!(
        rows[1].presence,
        Some(MachinePresence::Online),
        "the later since wins"
    );
    assert_eq!(rows[1].since.as_deref(), Some("2026-10-27T22:10:00.000Z"));
    assert_eq!(
        rows[1].thread_ids,
        vec!["th_1".to_owned(), "th_2".to_owned()]
    );
}

#[test]
fn state_deltas_patch_their_scope_and_unknown_types_only_take_a_seq() {
    let mut view = ThreadView::new("th_1");
    let delta = example("StateDelta.json");
    assert!(
        view.apply(&delta).is_err(),
        "a delta before its snapshot is refused"
    );

    let usage = json!({"context_used": 1, "context_size": 200000, "cost": null});
    let snapshot = Envelope::new(
        "th_1",
        None,
        None,
        "2026-10-27T22:00:00.000Z",
        StateSnapshot {
            scope: StateScope::Usage,
            snapshot: usage.as_object().cloned().expect("object"),
        },
    );
    let mut snapshot = snapshot;
    snapshot.seq = 1;
    view.apply(&snapshot).expect("snapshot applies");
    let mut delta = delta;
    delta.seq = 2;
    view.apply(&delta).expect("delta applies");
    assert_eq!(
        view.state("usage").expect("usage")["context_used"],
        json!(48211)
    );
    assert_eq!(
        view.state("usage").expect("usage")["cost"]["currency"],
        json!("USD")
    );

    // A type from a newer hub, on a task: it takes its seq and creates nothing.
    let future: Envelope = serde_json::from_value(json!({
        "v": 1, "seq": 3, "thread_id": "th_1", "task_id": "tk_future",
        "run_id": null, "ts": "2026-10-27T22:00:00.000Z",
        "event": {"type": "FromTheFuture", "anything": true}
    }))
    .expect("parses");
    assert_eq!(future.event, Event::Unknown);
    view.apply(&future).expect("ignored");
    assert_eq!(view.last_seq(), 3);
    assert!(view.task("tk_future").is_none());
}
