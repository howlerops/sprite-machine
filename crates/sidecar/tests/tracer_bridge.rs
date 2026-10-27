//! The tracer bridge against acp-mock, without the hub: one prompt, one approval, end_turn.
//! Checks the producer-side stream (seq 0) for turn-event.md §5 rule 6.

use std::time::Duration;

use protocol::tunnel::{Actor, ActorKind, Command, ContentPart, Outcome};
use protocol::turn_event::{Envelope, Event, RunOutcome, RunReason, ThreadSeq};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(10);

fn user() -> Actor {
    Actor {
        kind: ActorKind::User,
        id: "usr_1".into(),
        name: None,
        device_id: None,
    }
}

async fn until(
    events: &mut mpsc::Receiver<Envelope>,
    stop: impl Fn(&Event) -> bool,
) -> Vec<Envelope> {
    let mut seen = Vec::new();
    loop {
        let envelope = timeout(STEP, events.recv())
            .await
            .expect("bridge produced an event in time")
            .expect("bridge still running");
        let done = stop(&envelope.event);
        seen.push(envelope);
        if done {
            return seen;
        }
    }
}

#[tokio::test]
async fn prompt_permission_resume_end_turn() {
    let script = acp_mock::Script::from_value(&json!({ "turns": [{ "steps": [
        { "text": "Working." },
        { "request_permission": {
            "toolCall": { "toolCallId": "call_1", "title": "Run tests", "kind": "execute" },
            "options": [{ "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                        { "optionId": "reject", "name": "Deny", "kind": "reject_once" }] },
          "expect": "allow" },
        { "tool_call_update": { "toolCallId": "call_1", "status": "completed" } },
        { "text": "Done." },
        { "stop": "end_turn" }
    ]}]}))
    .expect("script");
    let (client_io, agent_io) = tokio::io::duplex(1 << 16);
    let (agent_r, agent_w) = tokio::io::split(agent_io);
    tokio::spawn(acp_mock::run(script, agent_r, agent_w));
    let (client_r, client_w) = tokio::io::split(client_io);
    let (command_tx, command_rx) = mpsc::channel(8);
    let (event_tx, mut events) = mpsc::channel(64);
    let bridge = tokio::spawn(sidecar::tracer::run(
        sidecar::tracer::BridgeConfig {
            machine_id: "mch_1".into(),
            agent_id: "agt_mock".into(),
            cwd: std::env::temp_dir(),
        },
        client_r,
        client_w,
        command_rx,
        event_tx,
    ));

    command_tx
        .send(Command::Prompt {
            thread_id: "th_1".into(),
            task_id: "tk_1".into(),
            message_id: "msg_u1".into(),
            parts: vec![ContentPart::Text { text: "go".into() }],
            author: user(),
        })
        .await
        .expect("send prompt");
    let mut stream = until(&mut events, |e| matches!(e, Event::RunFinished(_))).await;
    let interrupt_id = stream
        .iter()
        .find_map(|e| match &e.event {
            Event::Interrupt(i) => Some(i.interrupt_id.clone()),
            _ => None,
        })
        .expect("an interrupt");

    command_tx
        .send(Command::Approval {
            thread_id: "th_1".into(),
            task_id: "tk_1".into(),
            interrupt_id,
            outcome: Outcome::Selected {
                option_id: "allow".into(),
            },
            resolved_by: user(),
        })
        .await
        .expect("send approval");
    stream.extend(until(&mut events, |e| matches!(e, Event::RunFinished(_))).await);

    let types: Vec<&str> = stream.iter().map(|e| e.event.type_name()).collect();
    assert_eq!(
        types,
        [
            "UserMessage",
            "RunStarted",
            "TextMessageStart",
            "TextMessageContent",
            "TextMessageEnd",
            "ToolCallStart",
            "Interrupt",
            "RunFinished",
            // InterruptResolved is the hub's; the runner resumes after the approval command.
            "RunStarted",
            "ToolCallEnd",
            "TextMessageStart",
            "TextMessageContent",
            "TextMessageEnd",
            "RunFinished",
        ]
    );
    assert!(stream.iter().all(|e| e.seq == 0), "producers send seq 0");
    assert!(
        matches!(&stream[7].event, Event::RunFinished(f) if matches!(f.outcome, RunOutcome::Interrupt { .. }))
    );
    assert!(
        matches!(&stream[13].event, Event::RunFinished(f) if matches!(f.outcome, RunOutcome::Success { .. }))
    );
    let Event::RunStarted(resume) = &stream[8].event else {
        panic!("resume RunStarted expected");
    };
    assert_eq!(resume.reason, RunReason::Resume);
    assert_eq!(resume.parent_run_id, stream[1].run_id);
    assert_ne!(stream[8].run_id, stream[1].run_id);
    assert_eq!(stream[0].run_id, None, "UserMessage is task-scoped");

    // The Thread DO's stamper turns them into a gap-free log.
    let mut seq = ThreadSeq::new();
    let stamped: Vec<u64> = stream
        .into_iter()
        .map(|e| seq.stamp(e).expect("seq 0").seq)
        .collect();
    assert_eq!(stamped, (1..=14).collect::<Vec<u64>>());

    drop(command_tx);
    timeout(STEP, bridge)
        .await
        .expect("bridge stops when commands close")
        .expect("bridge task")
        .expect("bridge ok");
}
