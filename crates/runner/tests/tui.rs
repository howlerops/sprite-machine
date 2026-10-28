//! The interactive TUI (M2B-010, PLAN-v2-ade.md §4.10): the state machine over client-core, the
//! rendered buffer (snapshots), the event loop on a `TestBackend`, and the `autoConnect` setting.
//! No terminal, hub or agent: a fake hub numbers events and applies the first-answer-wins rule.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use client_core::sync::Incoming;
use client_core::{
    Cache, Connectivity, Intent, IntentState, ManualClock, Outbox, PostRequest, PostResponse,
    RetryPolicy, Transport, TransportError,
};
use protocol::tunnel::{Actor, ActorKind, Outcome};
use protocol::turn_event::{Envelope, Event, is_valid_id};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use runner::settings::{self, Settings};
use runner::tui::render::render;
use runner::tui::term::{map_key, panic_hook_with};
use runner::tui::{Action, AnswerResult, App, Backend, Feed, Key, Remote};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const THREAD: &str = "th_tui1";
const URL: &str = "https://hub.example/t/th_tui1";

fn actor(id: &str, name: &str) -> Actor {
    Actor {
        kind: ActorKind::User,
        id: id.to_owned(),
        name: Some(name.to_owned()),
        device_id: Some(format!("dev_{name}")),
    }
}

fn terminal_actor() -> Actor {
    actor("usr_1", "terminal")
}

fn phone_actor() -> Actor {
    actor("usr_1", "phone")
}

// ---- the fake hub ---------------------------------------------------------------------------

/// What the hub does with a thread: numbers events in arrival order and lets the first answer to
/// an interrupt win.
struct Hub {
    seq: u64,
    resolved: HashMap<String, Actor>,
}

impl Hub {
    fn new() -> Self {
        Self {
            seq: 0,
            resolved: HashMap::new(),
        }
    }

    fn emit(&mut self, task: Option<&str>, run: Option<&str>, event: Value) -> Incoming {
        self.seq += 1;
        let event: Event = serde_json::from_value(event).expect("a TurnEvent");
        let mut envelope = Envelope::new(
            THREAD,
            task.map(str::to_owned),
            run.map(str::to_owned),
            "2026-10-28T10:00:00.000Z",
            event,
        );
        envelope.seq = self.seq;
        Incoming::Event(Box::new(envelope))
    }

    /// A prompt from `author` (the terminal or the phone) and the run it starts.
    fn prompt(&mut self, task: &str, text: &str, author: &Actor) -> Vec<Incoming> {
        let run = format!("run_{task}");
        vec![
            self.emit(
                Some(task),
                None,
                json!({
                    "type": "UserMessage",
                    "message_id": format!("msg_{task}"),
                    "parts": [{"type": "text", "text": text}],
                    "author": author,
                }),
            ),
            self.emit(
                Some(task),
                Some(&run),
                json!({"type": "RunStarted", "reason": "prompt"}),
            ),
        ]
    }

    fn agent_says(&mut self, task: &str, text: &str) -> Vec<Incoming> {
        let run = format!("run_{task}");
        let message = format!("am_{task}");
        vec![
            self.emit(
                Some(task),
                Some(&run),
                json!({"type": "TextMessageStart", "message_id": message, "role": "assistant"}),
            ),
            self.emit(
                Some(task),
                Some(&run),
                json!({"type": "TextMessageContent", "message_id": message, "delta": text}),
            ),
            self.emit(
                Some(task),
                Some(&run),
                json!({"type": "TextMessageEnd", "message_id": message}),
            ),
        ]
    }

    fn finish(&mut self, task: &str) -> Incoming {
        let run = format!("run_{task}");
        self.emit(
            Some(task),
            Some(&run),
            json!({"type": "RunFinished", "outcome": {"type": "success", "stop_reason": "end_turn"}}),
        )
    }

    /// The agent asks to run something: the interrupt and the pause.
    fn ask(&mut self, task: &str, interrupt: &str, title: &str) -> Vec<Incoming> {
        let run = format!("run_{task}");
        vec![
            self.emit(
                Some(task),
                Some(&run),
                json!({
                    "type": "Interrupt",
                    "interrupt_id": interrupt,
                    "kind": "approval",
                    "title": title,
                    "expires_at": "2026-10-28T10:10:00.000Z",
                    "options": [
                        {"option_id": "allow", "name": "Allow once", "kind": "allow_once"},
                        {"option_id": "reject", "name": "Reject", "kind": "reject_once"},
                    ],
                }),
            ),
            self.emit(
                Some(task),
                Some(&run),
                json!({"type": "RunFinished", "outcome": {"type": "interrupt", "interrupt_ids": [interrupt]}}),
            ),
        ]
    }

    /// An answer reaching the hub. The first one is accepted and becomes an `InterruptResolved`
    /// event; a later one is the `409` of the loser, with the winner.
    fn answer(
        &mut self,
        task: &str,
        interrupt: &str,
        outcome: &Outcome,
        by: &Actor,
    ) -> Result<Incoming, Actor> {
        if let Some(winner) = self.resolved.get(interrupt) {
            return Err(winner.clone());
        }
        self.resolved.insert(interrupt.to_owned(), by.clone());
        let run = format!("run_{task}");
        let Outcome::Selected { option_id } = outcome else {
            panic!("the tests answer with options");
        };
        Ok(self.emit(
            Some(task),
            Some(&run),
            json!({
                "type": "InterruptResolved",
                "interrupt_id": interrupt,
                "outcome": {"type": "selected", "option_id": option_id},
                "resolved_by": by,
            }),
        ))
    }
}

fn feed(app: &mut App, messages: Vec<Incoming>) {
    for message in messages {
        app.incoming(message);
    }
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        assert!(app.key(Key::Char(c)).is_empty());
    }
}

/// Types `text` and presses Enter.
fn send_line(app: &mut App, text: &str) -> Vec<Action> {
    type_text(app, text);
    app.key(Key::Enter)
}

fn new_app(auto_connect: bool) -> App {
    App::new(THREAD, Settings { auto_connect }, "t1")
}

// ---- the rendered buffer --------------------------------------------------------------------

fn text_of(buf: &Buffer) -> String {
    let area = buf.area;
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn screen_sized(app: &App, width: u16, height: u16) -> String {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    render(app, area, &mut buf);
    text_of(&buf)
}

fn screen(app: &App) -> String {
    screen_sized(app, 60, 16)
}

#[track_caller]
fn assert_screen(actual: &str, expected: &str) {
    let expected = expected.strip_prefix('\n').unwrap_or(expected);
    assert_eq!(
        actual, expected,
        "\n--- actual ---\n{actual}\n--- expected ---\n{expected}\n"
    );
}

#[test]
fn an_empty_session_snapshot() {
    let app = new_app(false);
    assert_screen(
        &screen_sized(&app, 50, 6),
        r#"
 sm  th_tui1  [idle]                  remote: off




>"#,
    );
}

#[test]
fn messages_from_the_terminal_and_the_phone_appear_in_order() {
    let mut app = new_app(false);
    let mut hub = Hub::new();

    // Typed here.
    let actions = send_line(&mut app, "add a healthz route");
    let [Action::Prompt { task_id, text, .. }] = actions.as_slice() else {
        panic!("one prompt, got {actions:?}");
    };
    assert_eq!(text, "add a healthz route");
    // The typed line is not shown until the hub has it: the transcript is the hub's order.
    assert!(!screen(&app).contains("add a healthz"));
    feed(&mut app, hub.prompt(task_id, text, &terminal_actor()));
    feed(&mut app, hub.agent_says(task_id, "On it."));
    app.incoming(hub.finish(task_id));

    // Typed on the phone.
    feed(
        &mut app,
        hub.prompt("tk_phone1", "also add a test", &phone_actor()),
    );
    feed(&mut app, hub.agent_says("tk_phone1", "Test added."));
    app.incoming(hub.finish("tk_phone1"));

    // Typed here again.
    let actions = send_line(&mut app, "thanks");
    let [Action::Prompt { task_id, text, .. }] = actions.as_slice() else {
        panic!("one prompt, got {actions:?}");
    };
    feed(&mut app, hub.prompt(task_id, text, &terminal_actor()));

    let shown = screen(&app);
    let at = |needle: &str| {
        shown
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} in\n{shown}"))
    };
    assert!(at("terminal> add a healthz route") < at("agent> On it."));
    assert!(at("agent> On it.") < at("phone> also add a test"));
    assert!(at("phone> also add a test") < at("agent> Test added."));
    assert!(at("agent> Test added.") < at("terminal> thanks"));
    assert_screen(
        &screen_sized(&app, 50, 12),
        r#"
 sm  th_tui1  [working]               remote: off
terminal> add a healthz route
agent> On it.
phone> also add a test
agent> Test added.
terminal> thanks





>"#,
    );
}

#[test]
fn an_open_approval_snapshot() {
    let mut app = new_app(false);
    let mut hub = Hub::new();
    feed(
        &mut app,
        hub.prompt("tk_a", "run the tests", &terminal_actor()),
    );
    feed(&mut app, hub.ask("tk_a", "int_1", "Run cargo test?"));
    assert_screen(
        &screen_sized(&app, 56, 14),
        r#"
 sm  th_tui1  [waiting for you]             remote: off
terminal> run the tests
? Run cargo test?
    waiting for an answer, here or on another device



┌ Approval ────────────────────────────────────────────┐
│Run cargo test?                                       │
│> Allow once                                          │
│  Reject                                              │
│up/down choose, enter answers (any device may answer) │
└──────────────────────────────────────────────────────┘
>"#,
    );
}

#[test]
fn an_approval_is_answered_on_the_terminal() {
    let mut app = new_app(false);
    let mut hub = Hub::new();
    feed(
        &mut app,
        hub.prompt("tk_a", "run the tests", &terminal_actor()),
    );
    feed(&mut app, hub.ask("tk_a", "int_1", "Run cargo test?"));

    // Down chooses the second option, Up goes back, Enter answers with the highlighted one.
    assert!(app.key(Key::Down).is_empty());
    assert_eq!(app.selected(), 1);
    assert!(app.key(Key::Up).is_empty());
    assert!(app.key(Key::Down).is_empty());
    let actions = app.key(Key::Enter);
    let reject = Outcome::Selected {
        option_id: "reject".to_owned(),
    };
    assert_eq!(
        actions,
        vec![Action::Answer {
            interrupt_id: "int_1".to_owned(),
            outcome: reject.clone(),
        }]
    );
    // While the answer is on its way the panel is gone and the transcript says so; a second
    // Enter does not send it twice.
    assert!(app.pending_approval().is_none());
    assert!(app.key(Key::Enter).is_empty());
    let shown = screen(&app);
    assert!(shown.contains("sending your answer..."), "{shown}");

    let event = hub
        .answer("tk_a", "int_1", &reject, &terminal_actor())
        .expect("the first answer wins");
    app.incoming(event);
    app.answered("int_1", AnswerResult::Accepted);
    let shown = screen(&app);
    assert!(shown.contains("answered: Reject (by terminal)"), "{shown}");
    assert!(!shown.contains("sending your answer"), "{shown}");
}

#[test]
fn an_approval_answered_on_the_phone_first_closes_the_terminal_panel() {
    let mut app = new_app(false);
    let mut hub = Hub::new();
    feed(
        &mut app,
        hub.prompt("tk_a", "run the tests", &terminal_actor()),
    );
    feed(&mut app, hub.ask("tk_a", "int_1", "Run cargo test?"));
    assert!(app.pending_approval().is_some());

    let event = hub
        .answer(
            "tk_a",
            "int_1",
            &Outcome::Selected {
                option_id: "allow".to_owned(),
            },
            &phone_actor(),
        )
        .expect("first");
    app.incoming(event);

    assert!(app.pending_approval().is_none());
    // Enter on an empty line has nothing left to answer.
    assert!(app.key(Key::Enter).is_empty());
    assert_screen(
        &screen_sized(&app, 56, 12),
        r#"
 sm  th_tui1  [working]                     remote: off
terminal> run the tests
? Run cargo test?
    answered: Allow once (by phone)







>"#,
    );
}

#[test]
fn the_second_answer_is_a_409_and_the_terminal_says_already_answered() {
    let mut app = new_app(false);
    let mut hub = Hub::new();
    feed(
        &mut app,
        hub.prompt("tk_a", "run the tests", &terminal_actor()),
    );
    feed(&mut app, hub.ask("tk_a", "int_1", "Run cargo test?"));

    // The user presses Enter here; at the hub the phone's answer got there first, and its event
    // has not reached the terminal yet.
    let allow = Outcome::Selected {
        option_id: "allow".to_owned(),
    };
    let phone_event = hub
        .answer("tk_a", "int_1", &allow, &phone_actor())
        .expect("the phone wins");
    let actions = app.key(Key::Enter);
    let [Action::Answer { outcome, .. }] = actions.as_slice() else {
        panic!("one answer, got {actions:?}");
    };
    let winner = hub
        .answer("tk_a", "int_1", outcome, &terminal_actor())
        .expect_err("the second answer is a 409");
    assert_eq!(winner, phone_actor());
    app.answered("int_1", AnswerResult::AlreadyAnswered { by: Some(winner) });

    let shown = screen_sized(&app, 56, 12);
    assert!(shown.contains("already answered by phone"), "{shown}");
    assert!(app.pending_approval().is_none());
    assert_screen(
        &shown,
        r#"
 sm  th_tui1  [working]                     remote: off
terminal> run the tests
? Run cargo test?
    already answered by phone






already answered by phone
>"#,
    );

    // Then the hub's own event arrives and the transcript reads as the hub says.
    app.incoming(phone_event);
    let shown = screen(&app);
    assert!(shown.contains("answered: Allow once (by phone)"), "{shown}");
}

/// The 409 as client-core's outbox reports it, mapped to what the UI shows.
#[tokio::test]
async fn a_409_through_the_outbox_reads_already_answered() {
    struct Scripted(Mutex<VecDeque<PostResponse>>);
    impl Transport for Scripted {
        fn post(
            &self,
            _request: PostRequest,
        ) -> impl std::future::Future<Output = Result<PostResponse, TransportError>> + Send
        {
            let reply = self.0.lock().expect("lock").pop_front().expect("scripted");
            async move { Ok(reply) }
        }
    }

    let mut app = new_app(false);
    let mut hub = Hub::new();
    feed(
        &mut app,
        hub.prompt("tk_a", "run the tests", &terminal_actor()),
    );
    feed(&mut app, hub.ask("tk_a", "int_1", "Run cargo test?"));
    let Action::Answer {
        interrupt_id,
        outcome,
    } = app.key(Key::Enter).remove(0)
    else {
        panic!("an answer");
    };

    let transport = Scripted(Mutex::new(VecDeque::from([
        PostResponse {
            status: 409,
            body: json!({
                "error": "already_resolved",
                "message": "this approval was already answered",
                "resolved_by": phone_actor(),
            }),
        },
        PostResponse {
            status: 200,
            body: json!({}),
        },
        PostResponse {
            status: 410,
            body: json!({"error": "expired"}),
        },
    ])));
    let cache = Cache::open_in_memory().expect("cache");
    let mut outbox = Outbox::load(&cache, RetryPolicy::default()).expect("outbox");
    let clock = ManualClock::new(1_000);
    let mut rng = client_core::SystemRandom;
    let mut results = Vec::new();
    for _ in 0..3 {
        let id = outbox
            .enqueue(
                &cache,
                Intent::Approval {
                    thread_id: THREAD.to_owned(),
                    interrupt_id: interrupt_id.clone(),
                    outcome: outcome.clone(),
                    resolved_by: None,
                },
                1_000,
            )
            .expect("enqueue");
        outbox
            .flush(
                &cache,
                &transport,
                "https://hub.example/v1",
                &clock,
                &mut rng,
            )
            .await
            .expect("flush");
        let state: IntentState = outbox.get(id).expect("entry").state.clone();
        results.push(AnswerResult::from_intent_state(&state).expect("settled"));
    }

    assert_eq!(
        results[0],
        AnswerResult::AlreadyAnswered {
            by: Some(phone_actor())
        }
    );
    assert_eq!(results[1], AnswerResult::Accepted);
    assert!(matches!(&results[2], AnswerResult::Failed(m) if m.contains("410")));
    assert!(
        AnswerResult::from_intent_state(&IntentState::Queued).is_none(),
        "a queued answer has no result yet"
    );

    app.answered("int_1", results.remove(0));
    let shown = screen(&app);
    assert!(shown.contains("already answered by phone"), "{shown}");
}

#[test]
fn a_failed_answer_is_shown_and_can_be_sent_again() {
    let mut app = new_app(false);
    let mut hub = Hub::new();
    feed(
        &mut app,
        hub.prompt("tk_a", "run the tests", &terminal_actor()),
    );
    feed(&mut app, hub.ask("tk_a", "int_1", "Run cargo test?"));
    assert_eq!(app.key(Key::Enter).len(), 1);
    app.answered(
        "int_1",
        AnswerResult::Failed("HTTP 410: expired".to_owned()),
    );
    let shown = screen_sized(&app, 56, 14);
    assert!(
        shown.contains("your answer was not sent: HTTP 410: expired"),
        "{shown}"
    );
    assert_screen(
        &shown,
        r#"
 sm  th_tui1  [waiting for you]             remote: off
terminal> run the tests
? Run cargo test?
    your answer was not sent: HTTP 410: expired

┌ Approval ────────────────────────────────────────────┐
│Run cargo test?                                       │
│your answer was not sent: HTTP 410: expired           │
│> Allow once                                          │
│  Reject                                              │
│up/down choose, enter answers (any device may answer) │
└──────────────────────────────────────────────────────┘
answer not sent
>"#,
    );
    // The approval is open again for another try.
    assert_eq!(app.key(Key::Enter).len(), 1);
}

#[test]
fn the_first_answer_wins_at_the_hub_whichever_side_it_comes_from() {
    let mut hub = Hub::new();
    let _ = hub.prompt("tk_a", "x", &terminal_actor());
    let _ = hub.ask("tk_a", "int_1", "Run?");
    let allow = Outcome::Selected {
        option_id: "allow".to_owned(),
    };
    assert!(
        hub.answer("tk_a", "int_1", &allow, &terminal_actor())
            .is_ok()
    );
    assert_eq!(
        hub.answer("tk_a", "int_1", &allow, &phone_actor())
            .unwrap_err(),
        terminal_actor()
    );
}

// ---- /rc and autoConnect --------------------------------------------------------------------

#[test]
fn rc_toggles_remote_and_shows_the_link() {
    let mut app = new_app(false);
    assert_eq!(*app.remote(), Remote::Off);
    assert!(
        app.start().is_empty(),
        "autoConnect is off: nothing registers at start"
    );

    // /rc registers; until the hub answers the state is "connecting".
    assert_eq!(send_line(&mut app, "/rc"), vec![Action::Register]);
    assert_eq!(*app.remote(), Remote::Connecting);
    assert!(screen_sized(&app, 56, 6).contains("remote: connecting"));
    // A second /rc while connecting asks for nothing.
    assert!(send_line(&mut app, "/rc").is_empty());

    app.registered(Ok(URL.to_owned()));
    assert_eq!(
        *app.remote(),
        Remote::On {
            url: URL.to_owned()
        }
    );
    app.set_connectivity(Connectivity::Online);
    let shown = screen_sized(&app, 60, 6);
    assert!(shown.contains("remote: on"), "{shown}");
    assert!(shown.contains(&format!("link: {URL}")), "{shown}");
    assert_screen(
        &shown,
        r#"
 sm  th_tui1  [idle]                             remote: on
link: https://hub.example/t/th_tui1


remote on: https://hub.example/t/th_tui1
>"#,
    );

    // /rc again stops sharing: the link goes.
    assert_eq!(send_line(&mut app, "/rc"), vec![Action::Unregister]);
    app.unregistered();
    let shown = screen_sized(&app, 60, 6);
    assert!(shown.contains("remote: off"), "{shown}");
    assert!(!shown.contains("link:"), "{shown}");
}

#[test]
fn a_failed_registration_is_shown_and_rc_tries_again() {
    let mut app = new_app(false);
    assert_eq!(send_line(&mut app, "/rc"), vec![Action::Register]);
    app.registered(Err("not logged in: run `sm login --hub <url>`".to_owned()));
    let shown = screen_sized(&app, 70, 6);
    assert!(shown.contains("remote: failed"), "{shown}");
    assert!(shown.contains("remote failed: not logged in"), "{shown}");
    assert_eq!(send_line(&mut app, "/rc"), vec![Action::Register]);
}

#[test]
fn the_connection_state_is_in_the_header_while_shared() {
    let mut app = new_app(false);
    send_line(&mut app, "/rc");
    app.registered(Ok(URL.to_owned()));
    app.set_connectivity(Connectivity::Offline);
    assert!(screen_sized(&app, 60, 4).contains("remote: on (offline)"));
}

#[test]
fn auto_connect_registers_at_start_and_off_waits_for_rc() {
    let mut on = new_app(true);
    assert_eq!(on.start(), vec![Action::Register]);
    assert_eq!(*on.remote(), Remote::Connecting);

    let mut off = new_app(false);
    assert!(off.start().is_empty());
    assert_eq!(*off.remote(), Remote::Off);
}

#[test]
fn auto_connect_comes_from_the_settings_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    // No file: the default is off.
    let default = settings::load(dir.path());
    assert!(!default.auto_connect);
    assert!(App::new(THREAD, default, "s").start().is_empty());

    std::fs::write(dir.path().join(settings::FILE), r#"{"autoConnect": true}"#).expect("write");
    let loaded = settings::load(dir.path());
    assert!(loaded.auto_connect);
    assert_eq!(
        App::new(THREAD, loaded, "s").start(),
        vec![Action::Register]
    );
}

// ---- the commands and keys ------------------------------------------------------------------

#[test]
fn prompts_get_unique_valid_ids() {
    let mut app = new_app(false);
    let mut seen = Vec::new();
    for text in ["one", "two", "three"] {
        let actions = send_line(&mut app, text);
        let [
            Action::Prompt {
                task_id,
                message_id,
                text: sent,
            },
        ] = actions.as_slice()
        else {
            panic!("a prompt");
        };
        assert_eq!(sent, text);
        assert!(is_valid_id(task_id) && is_valid_id(message_id));
        assert!(!seen.contains(task_id) && !seen.contains(message_id));
        seen.push(task_id.clone());
        seen.push(message_id.clone());
    }
}

#[test]
fn an_empty_line_sends_nothing_and_unknown_commands_say_so() {
    let mut app = new_app(false);
    assert!(app.key(Key::Enter).is_empty());
    assert!(send_line(&mut app, "   ").is_empty());
    assert!(send_line(&mut app, "/nope").is_empty());
    assert!(
        app.notice()
            .expect("a notice")
            .contains("unknown command /nope")
    );
}

#[test]
fn cancel_targets_the_running_task() {
    let mut app = new_app(false);
    let mut hub = Hub::new();
    assert!(send_line(&mut app, "/cancel").is_empty());
    assert_eq!(app.notice(), Some("nothing is running"));

    feed(&mut app, hub.prompt("tk_a", "long job", &terminal_actor()));
    assert_eq!(
        send_line(&mut app, "/cancel"),
        vec![Action::Cancel {
            task_id: "tk_a".to_owned()
        }]
    );
    app.incoming(hub.finish("tk_a"));
    assert!(send_line(&mut app, "/cancel").is_empty(), "the task ended");
}

#[test]
fn quitting_and_editing_keys() {
    let mut app = new_app(false);
    type_text(&mut app, "helo");
    assert!(app.key(Key::Backspace).is_empty());
    type_text(&mut app, "lo");
    assert_eq!(app.input(), "hello");
    assert!(app.key(Key::Esc).is_empty());
    assert_eq!(app.input(), "");
    // Ctrl-D quits only on an empty line.
    type_text(&mut app, "x");
    assert!(app.key(Key::Ctrl('d')).is_empty());
    assert!(!app.should_quit());
    assert_eq!(app.key(Key::Ctrl('c')), vec![Action::Quit]);
    assert!(app.should_quit());
    assert_eq!(new_app(false).key(Key::Ctrl('d')), vec![Action::Quit]);
    assert_eq!(send_line(&mut new_app(false), "/quit"), vec![Action::Quit]);
}

#[test]
fn crossterm_keys_map_to_tui_keys() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    let key = |code, modifiers| map_key(KeyEvent::new(code, modifiers));
    assert_eq!(
        key(KeyCode::Char('a'), KeyModifiers::NONE),
        Some(Key::Char('a'))
    );
    assert_eq!(
        key(KeyCode::Char('C'), KeyModifiers::CONTROL),
        Some(Key::Ctrl('c'))
    );
    assert_eq!(key(KeyCode::Enter, KeyModifiers::NONE), Some(Key::Enter));
    assert_eq!(key(KeyCode::Down, KeyModifiers::NONE), Some(Key::Down));
    assert_eq!(key(KeyCode::F(5), KeyModifiers::NONE), None);
    let mut release = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    assert_eq!(map_key(release), None);
}

// ---- rendering ------------------------------------------------------------------------------

#[test]
fn long_messages_wrap_and_the_newest_lines_stay_in_view() {
    let mut app = new_app(false);
    let mut hub = Hub::new();
    feed(
        &mut app,
        hub.prompt(
            "tk_a",
            "please read the whole of the quarterly report and summarise it",
            &phone_actor(),
        ),
    );
    let words = (1..=40)
        .map(|n| format!("w{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    feed(&mut app, hub.agent_says("tk_a", &words));
    let shown = screen_sized(&app, 30, 8);
    // The newest lines are the bottom of the transcript; the oldest scrolled off.
    assert!(shown.contains("w40"), "{shown}");
    assert!(!shown.contains("quarterly"), "{shown}");
    assert_screen(
        &shown,
        r#"
 sm  th_tui1  [wo remote: off
       w9 w10 w11 w12 w13 w14
       w15 w16 w17 w18 w19 w20
       w21 w22 w23 w24 w25 w26
       w27 w28 w29 w30 w31 w32
       w33 w34 w35 w36 w37 w38
       w39 w40
>"#,
    );

    // PageUp goes back; PageDown returns.
    app.key(Key::PageUp);
    app.key(Key::PageUp);
    assert!(!screen_sized(&app, 30, 8).contains("w40"));
    app.key(Key::PageDown);
    app.key(Key::PageDown);
    assert!(screen_sized(&app, 30, 8).contains("w40"));
}

#[test]
fn tiny_terminals_do_not_panic() {
    let mut app = new_app(false);
    let mut hub = Hub::new();
    feed(&mut app, hub.prompt("tk_a", "hello", &terminal_actor()));
    feed(&mut app, hub.ask("tk_a", "int_1", "Run cargo test?"));
    send_line(&mut app, "/rc");
    app.registered(Ok(URL.to_owned()));
    for width in 0..12 {
        for height in 0..8 {
            let _ = screen_sized(&app, width, height);
        }
    }
}

#[test]
fn tools_reasoning_errors_and_checkpoints_have_lines() {
    let mut app = new_app(false);
    let mut hub = Hub::new();
    feed(&mut app, hub.prompt("tk_a", "fix it", &terminal_actor()));
    let run = "run_tk_a";
    let events = [
        json!({"type": "ReasoningMessageStart", "message_id": "rm1", "role": "reasoning"}),
        json!({"type": "ReasoningMessageContent", "message_id": "rm1", "delta": "check the route"}),
        json!({"type": "ReasoningMessageEnd", "message_id": "rm1"}),
        json!({"type": "ToolCallStart", "tool_call_id": "tc1", "title": "Edit src/lib.rs", "kind": "edit", "status": "in_progress"}),
        json!({"type": "ToolCallEnd", "tool_call_id": "tc1", "status": "completed"}),
        json!({"type": "RunError", "reason": "error", "code": "harness_exited", "message": "the agent exited"}),
    ];
    for event in events {
        let incoming = hub.emit(Some("tk_a"), Some(run), event);
        app.incoming(incoming);
    }
    let shown = screen_sized(&app, 50, 10);
    assert!(shown.contains("thinking: check the route"), "{shown}");
    assert!(shown.contains("[tool] Edit src/lib.rs (done)"), "{shown}");
    assert!(shown.contains("! the agent exited"), "{shown}");
    assert_screen(
        &shown,
        r#"
 sm  th_tui1  [idle]                  remote: off
terminal> fix it
  thinking: check the route
  [tool] Edit src/lib.rs (done)
! the agent exited




>"#,
    );
}

// ---- the event loop on a TestBackend --------------------------------------------------------

/// A host that records what it is asked and reacts like the hub does.
struct FakeBackend {
    calls: Arc<Mutex<Vec<String>>>,
    hub: Arc<Mutex<Hub>>,
    feeds: mpsc::UnboundedReceiver<Feed>,
    out: mpsc::UnboundedSender<Feed>,
    registration: Result<String, String>,
}

impl FakeBackend {
    fn new() -> Self {
        let (out, feeds) = mpsc::unbounded_channel();
        Self {
            calls: Arc::default(),
            hub: Arc::new(Mutex::new(Hub::new())),
            feeds,
            out,
            registration: Ok(URL.to_owned()),
        }
    }

    fn calls(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.calls)
    }

    fn record(&self, call: impl Into<String>) {
        self.calls.lock().expect("lock").push(call.into());
    }

    fn push(&self, messages: Vec<Incoming>) {
        for message in messages {
            let _ = self.out.send(Feed::Thread(message));
        }
    }
}

impl Backend for FakeBackend {
    async fn register(&mut self) -> Result<String, String> {
        self.record("register");
        self.registration.clone()
    }

    async fn unregister(&mut self) {
        self.record("unregister");
    }

    async fn prompt(
        &mut self,
        task_id: String,
        _message_id: String,
        text: String,
    ) -> Result<(), String> {
        self.record(format!("prompt:{text}"));
        let events = self
            .hub
            .lock()
            .expect("lock")
            .prompt(&task_id, &text, &terminal_actor());
        self.push(events);
        Ok(())
    }

    async fn answer(&mut self, interrupt_id: &str, outcome: Outcome) -> AnswerResult {
        self.record(format!("answer:{interrupt_id}"));
        let result = self.hub.lock().expect("lock").answer(
            "tk_a",
            interrupt_id,
            &outcome,
            &terminal_actor(),
        );
        match result {
            Ok(event) => {
                self.push(vec![event]);
                AnswerResult::Accepted
            }
            Err(winner) => AnswerResult::AlreadyAnswered { by: Some(winner) },
        }
    }

    async fn cancel(&mut self, task_id: &str) -> Result<(), String> {
        self.record(format!("cancel:{task_id}"));
        Ok(())
    }

    async fn next(&mut self) -> Option<Feed> {
        self.feeds.recv().await
    }
}

/// Lets the event loop run everything that is ready.
async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

async fn keys(tx: &mpsc::Sender<Key>, text: &str) {
    for c in text.chars() {
        tx.send(Key::Char(c)).await.expect("keys");
    }
    tx.send(Key::Enter).await.expect("keys");
    settle().await;
}

fn terminal(width: u16, height: u16) -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(width, height)).expect("terminal")
}

#[tokio::test]
async fn the_loop_shows_terminal_and_phone_messages_in_order() {
    let mut app = new_app(false);
    let mut backend = FakeBackend::new();
    let calls = backend.calls();
    let hub = Arc::clone(&backend.hub);
    let phone = backend.out.clone();
    let mut term = terminal(50, 12);
    let (keys_tx, mut keys_rx) = mpsc::channel(64);

    let script = async {
        keys(&keys_tx, "add a healthz route").await;
        // The agent answers and the phone speaks.
        let events = {
            let mut hub = hub.lock().expect("lock");
            let mut events = hub.agent_says("tk_t11", "On it.");
            events.push(hub.finish("tk_t11"));
            events.extend(hub.prompt("tk_phone", "and a test please", &phone_actor()));
            events
        };
        for event in events {
            phone.send(Feed::Thread(event)).expect("feed");
        }
        settle().await;
        keys(&keys_tx, "thanks").await;
        keys_tx.send(Key::Ctrl('c')).await.expect("keys");
    };
    let (result, ()) = tokio::join!(
        runner::tui::run(&mut app, &mut backend, &mut term, &mut keys_rx),
        script
    );
    result.expect("the loop ends cleanly");

    assert_eq!(
        *calls.lock().expect("lock"),
        vec!["prompt:add a healthz route", "prompt:thanks"]
    );
    assert_screen(
        &text_of(term.backend().buffer()),
        r#"
 sm  th_tui1  [working]               remote: off
terminal> add a healthz route
agent> On it.
phone> and a test please
terminal> thanks






>"#,
    );
}

#[tokio::test]
async fn the_loop_registers_at_start_only_with_auto_connect() {
    for (auto_connect, expected) in [(true, vec!["register"]), (false, vec![])] {
        let mut app = new_app(auto_connect);
        let mut backend = FakeBackend::new();
        let calls = backend.calls();
        let mut term = terminal(50, 5);
        let (keys_tx, mut keys_rx) = mpsc::channel(8);
        keys_tx.send(Key::Ctrl('c')).await.expect("keys");
        runner::tui::run(&mut app, &mut backend, &mut term, &mut keys_rx)
            .await
            .expect("loop");
        assert_eq!(
            *calls.lock().expect("lock"),
            expected,
            "autoConnect={auto_connect}"
        );
        let shown = text_of(term.backend().buffer());
        let remote = if auto_connect {
            "remote: on"
        } else {
            "remote: off"
        };
        assert!(shown.contains(remote), "{shown}");
    }
}

#[tokio::test]
async fn the_loop_registers_after_rc() {
    let mut app = new_app(false);
    let mut backend = FakeBackend::new();
    let calls = backend.calls();
    let mut term = terminal(70, 5);
    let (keys_tx, mut keys_rx) = mpsc::channel(64);
    let script = async {
        keys(&keys_tx, "hello").await;
        assert!(calls.lock().expect("lock").iter().all(|c| c != "register"));
        keys(&keys_tx, "/rc").await;
        keys_tx.send(Key::Ctrl('c')).await.expect("keys");
    };
    let (result, ()) = tokio::join!(
        runner::tui::run(&mut app, &mut backend, &mut term, &mut keys_rx),
        script
    );
    result.expect("loop");
    assert_eq!(
        *calls.lock().expect("lock"),
        vec!["prompt:hello", "register"]
    );
    let shown = text_of(term.backend().buffer());
    assert!(shown.contains(&format!("link: {URL}")), "{shown}");
}

#[tokio::test]
async fn the_loop_shows_a_failed_registration() {
    let mut app = new_app(false);
    let mut backend = FakeBackend::new();
    backend.registration = Err("the hub did not answer".to_owned());
    let mut term = terminal(60, 5);
    let (keys_tx, mut keys_rx) = mpsc::channel(64);
    let script = async {
        keys(&keys_tx, "/rc").await;
        keys_tx.send(Key::Ctrl('c')).await.expect("keys");
    };
    let (result, ()) = tokio::join!(
        runner::tui::run(&mut app, &mut backend, &mut term, &mut keys_rx),
        script
    );
    result.expect("loop");
    let shown = text_of(term.backend().buffer());
    assert!(shown.contains("remote: failed"), "{shown}");
}

#[tokio::test]
async fn the_loop_answers_an_approval_and_loses_the_race_to_the_phone() {
    let mut app = new_app(false);
    let mut backend = FakeBackend::new();
    let calls = backend.calls();
    let hub = Arc::clone(&backend.hub);
    let phone = backend.out.clone();
    let mut term = terminal(56, 14);
    let (keys_tx, mut keys_rx) = mpsc::channel(64);
    let script = async {
        {
            let mut hub = hub.lock().expect("lock");
            let mut events = hub.prompt("tk_a", "run the tests", &terminal_actor());
            events.extend(hub.ask("tk_a", "int_1", "Run cargo test?"));
            for event in events {
                phone.send(Feed::Thread(event)).expect("feed");
            }
            // The phone answers at the hub, and its event is still on the way.
            let _on_the_way = hub
                .answer(
                    "tk_a",
                    "int_1",
                    &Outcome::Selected {
                        option_id: "allow".to_owned(),
                    },
                    &phone_actor(),
                )
                .expect("first");
        }
        settle().await;
        // The terminal's Enter reaches the hub second.
        keys_tx.send(Key::Enter).await.expect("keys");
        settle().await;
        keys_tx.send(Key::Ctrl('c')).await.expect("keys");
    };
    let (result, ()) = tokio::join!(
        runner::tui::run(&mut app, &mut backend, &mut term, &mut keys_rx),
        script
    );
    result.expect("loop");
    assert_eq!(*calls.lock().expect("lock"), vec!["answer:int_1"]);
    let shown = text_of(term.backend().buffer());
    assert!(shown.contains("already answered by phone"), "{shown}");
    assert_screen(
        &shown,
        r#"
 sm  th_tui1  [working]                     remote: off
terminal> run the tests
? Run cargo test?
    already answered by phone








already answered by phone
>"#,
    );
}

#[tokio::test]
async fn a_reset_rebuilds_the_thread_from_the_new_source() {
    let mut app = new_app(false);
    let mut backend = FakeBackend::new();
    let hub_out = backend.out.clone();
    let mut term = terminal(50, 8);
    let (keys_tx, mut keys_rx) = mpsc::channel(8);
    let script = async {
        let mut local = Hub::new();
        for event in local.prompt("tk_l", "local only", &terminal_actor()) {
            hub_out.send(Feed::Thread(event)).expect("feed");
        }
        settle().await;
        // The session is shared: the hub's log replaces the local one.
        hub_out.send(Feed::Reset).expect("feed");
        let mut remote = Hub::new();
        for event in remote.prompt("tk_h", "from the hub", &phone_actor()) {
            hub_out.send(Feed::Thread(event)).expect("feed");
        }
        hub_out
            .send(Feed::Connectivity(Connectivity::Online))
            .expect("feed");
        settle().await;
        keys_tx.send(Key::Ctrl('c')).await.expect("keys");
    };
    let (result, ()) = tokio::join!(
        runner::tui::run(&mut app, &mut backend, &mut term, &mut keys_rx),
        script
    );
    result.expect("loop");
    let shown = text_of(term.backend().buffer());
    assert!(shown.contains("phone> from the hub"), "{shown}");
    assert!(!shown.contains("local only"), "{shown}");
}

#[tokio::test]
async fn the_loop_ends_when_the_keys_end() {
    let mut app = new_app(false);
    let mut backend = FakeBackend::new();
    let mut term = terminal(30, 4);
    let (keys_tx, mut keys_rx) = mpsc::channel(1);
    drop(keys_tx);
    runner::tui::run(&mut app, &mut backend, &mut term, &mut keys_rx)
        .await
        .expect("ends with the keys");
}

// ---- the real terminal ----------------------------------------------------------------------

/// A panic puts the terminal back before its message is printed.
#[test]
fn a_panic_restores_the_terminal() {
    // The hook is process-wide and the other tests run beside this one: count by thread.
    let restored = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&restored);
    let before = std::panic::take_hook();
    // Stands in for the hook that was there before.
    std::panic::set_hook(Box::new(|_| {}));
    panic_hook_with(move || seen.lock().expect("lock").push(std::thread::current().id()));
    let result = std::panic::catch_unwind(|| panic!("the UI panicked"));
    std::panic::set_hook(before);
    assert!(result.is_err());
    let here = std::thread::current().id();
    let restored = restored.lock().expect("lock");
    assert_eq!(restored.iter().filter(|id| **id == here).count(), 1);
}

#[test]
fn restoring_without_a_terminal_is_harmless() {
    runner::tui::term::restore();
    runner::tui::term::restore();
}

// ---- the session over a real bridge ---------------------------------------------------------

/// The production session (log, sharing, first answer wins) over the real bridge and an
/// in-process acp-mock. The hub is a recording stand-in; phone commands come in through the
/// handle the tunnel's command task uses.
mod session {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use protocol::tunnel::{Command, ContentPart, Workspace};
    use runner::tui::driver::execute;
    use runner::tui::session::{
        Handle, HubAnswerFuture, HubVerdict, Identity, Registrar, Registration, Session, Upstream,
        UpstreamFuture, local_actor,
    };
    use sidecar::bridge::{Bridge, Config, IncomingCommand, Origin, SequentialGenerator};
    use sidecar::store::Store;
    use tempfile::TempDir;
    use tokio::time::timeout;

    use super::*;

    const STEP: Duration = Duration::from_secs(20);

    #[derive(Default)]
    struct Recording {
        opened: Mutex<Vec<(String, String)>>,
        sent: Mutex<Vec<Envelope>>,
        /// The answers typed here that went up (interrupt id, outcome).
        answers: Mutex<Vec<(String, Outcome)>>,
        /// What the hub says to the next answer: `Ok(verdict)`, or `Err` for no verdict.
        verdict: Mutex<Option<Result<HubVerdict, String>>>,
        /// When set, the hub accepts and sends the `approval` command back, as the Thread DO
        /// does for the winner, with the command's `seq`.
        approve_with: Mutex<Option<(Handle, u64)>>,
    }

    impl Upstream for Recording {
        fn open(
            &self,
            thread_id: &str,
            title: &str,
            _agent: &str,
            _workspace: &Workspace,
        ) -> Result<(), String> {
            self.opened
                .lock()
                .expect("lock")
                .push((thread_id.to_owned(), title.to_owned()));
            Ok(())
        }

        fn send<'a>(&'a self, envelopes: Vec<Envelope>) -> UpstreamFuture<'a> {
            self.sent.lock().expect("lock").extend(envelopes);
            Box::pin(async { Ok(()) })
        }

        fn answer<'a>(
            &'a self,
            _thread_id: &'a str,
            interrupt_id: &'a str,
            outcome: &'a Outcome,
        ) -> HubAnswerFuture<'a> {
            self.answers
                .lock()
                .expect("lock")
                .push((interrupt_id.to_owned(), outcome.clone()));
            let verdict = self
                .verdict
                .lock()
                .expect("lock")
                .take()
                .unwrap_or_else(|| Err("no verdict scripted".to_owned()));
            let approve = self.approve_with.lock().expect("lock").clone();
            let interrupt = interrupt_id.to_owned();
            let outcome = outcome.clone();
            Box::pin(async move {
                if let (Ok(HubVerdict::Accepted), Some((handle, seq))) = (&verdict, approve) {
                    handle
                        .hub_command(IncomingCommand {
                            seq,
                            idempotency_key: format!("ans_{seq}"),
                            command: Command::Approval {
                                thread_id: THREAD.to_owned(),
                                task_id: "tk_t11".to_owned(),
                                interrupt_id: interrupt,
                                outcome,
                                resolved_by: local_actor(),
                            },
                            origin: Origin::Hub,
                        })
                        .await;
                }
                verdict
            })
        }
    }

    impl Recording {
        fn kinds(&self) -> Vec<String> {
            self.sent
                .lock()
                .expect("lock")
                .iter()
                .map(|envelope| {
                    serde_json::to_value(&envelope.event).expect("event")["type"]
                        .as_str()
                        .expect("type")
                        .to_owned()
                })
                .collect()
        }
    }

    struct Rig {
        session: Session,
        handle: Handle,
        hub: Arc<Recording>,
        registrations: Arc<AtomicUsize>,
        app: App,
        _bridge: tokio::task::JoinHandle<()>,
        _dirs: Vec<TempDir>,
    }

    fn permission_turn(first: &str, last: &str) -> Value {
        json!({ "steps": [
            { "text": first },
            { "request_permission": {
                "toolCall": { "toolCallId": "call_1", "title": "Run tests", "kind": "execute" },
                "options": [
                    { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                    { "optionId": "reject", "name": "Reject", "kind": "reject_once" }
                ] },
              "expect": "allow" },
            { "text": last },
            { "stop": "end_turn" }
        ] })
    }

    fn plain_turn(text: &str) -> Value {
        json!({ "steps": [{ "text": text }, { "stop": "end_turn" }] })
    }

    async fn rig(turns: Vec<Value>, auto_connect: bool) -> Rig {
        let script = acp_mock::Script::from_value(&json!({ "turns": turns })).expect("script");
        let (client_io, agent_io) = tokio::io::duplex(1 << 16);
        let (agent_r, agent_w) = tokio::io::split(agent_io);
        tokio::spawn(acp_mock::run(script, agent_r, agent_w));
        let (reader, writer) = tokio::io::split(client_io);

        let state = TempDir::new().expect("state dir");
        let work = TempDir::new().expect("work dir");
        let store = Arc::new(Store::open(state.path()).expect("store"));
        let config = Config::new("mch_1", "agt_mock", work.path());

        let hub = Arc::new(Recording::default());
        let registrations = Arc::new(AtomicUsize::new(0));
        let registrar: Registrar = {
            let hub = Arc::clone(&hub);
            let registrations = Arc::clone(&registrations);
            Box::new(move |_handle| {
                registrations.fetch_add(1, Ordering::SeqCst);
                let upstream: Arc<dyn Upstream> = hub.clone();
                Box::pin(async move {
                    Ok(Registration {
                        upstream,
                        url: URL.to_owned(),
                    })
                })
            })
        };
        let (session, sink, commands) = Session::new(
            Identity {
                thread_id: THREAD.to_owned(),
                title: "laptop-brave-otter".to_owned(),
                agent: "agt_mock".to_owned(),
                workspace: Workspace {
                    path: work.path().to_string_lossy().into_owned(),
                    branch: None,
                    spawn: None,
                },
            },
            Arc::new(|| 1_700_000_000_000),
            registrar,
        );
        let bridge = Bridge::connect(
            config,
            reader,
            writer,
            store,
            Arc::new(SequentialGenerator::new()),
            sink,
            commands,
        );
        let bridge = tokio::spawn(async move {
            let _ = bridge.run().await;
        });
        Rig {
            handle: session.handle(),
            session,
            hub,
            registrations,
            app: new_app(auto_connect),
            _bridge: bridge,
            _dirs: vec![state, work],
        }
    }

    impl Rig {
        /// Types a line and carries out what the UI asks for.
        async fn say(&mut self, text: &str) {
            let actions = send_line(&mut self.app, text);
            assert!(execute(actions, &mut self.app, &mut self.session).await);
        }

        async fn press(&mut self, key: Key) {
            let actions = self.app.key(key);
            assert!(execute(actions, &mut self.app, &mut self.session).await);
        }

        async fn start(&mut self) {
            let actions = self.app.start();
            assert!(execute(actions, &mut self.app, &mut self.session).await);
        }

        /// Shows what the session reports until `done` holds.
        async fn until(&mut self, what: &str, done: impl Fn(&App) -> bool) {
            while !done(&self.app) {
                let feed = timeout(STEP, self.session.next())
                    .await
                    .unwrap_or_else(|_| {
                        panic!("timed out waiting for {what}\n{}", screen(&self.app))
                    })
                    .expect("the session ended");
                match feed {
                    Feed::Thread(incoming) => self.app.incoming(incoming),
                    Feed::Connectivity(state) => self.app.set_connectivity(state),
                    Feed::Reset => panic!("the session never resets"),
                }
            }
        }

        fn task_succeeded(&self, task: &str) -> bool {
            self.app
                .view()
                .task(task)
                .is_some_and(|task| task.status() == client_core::TaskStatus::Succeeded)
        }
    }

    fn hub_prompt(seq: u64, task: &str, text: &str) -> IncomingCommand {
        IncomingCommand {
            seq,
            idempotency_key: format!("idk_phone_{seq}"),
            command: Command::Prompt {
                thread_id: THREAD.to_owned(),
                task_id: task.to_owned(),
                message_id: format!("msg_{task}"),
                parts: vec![ContentPart::Text {
                    text: text.to_owned(),
                }],
                author: phone_actor(),
            },
            origin: Origin::Hub,
        }
    }

    fn hub_approval(seq: u64, task: &str, interrupt: &str, option: &str) -> IncomingCommand {
        IncomingCommand {
            seq,
            idempotency_key: format!("idk_phone_{seq}"),
            command: Command::Approval {
                thread_id: THREAD.to_owned(),
                task_id: task.to_owned(),
                interrupt_id: interrupt.to_owned(),
                outcome: Outcome::Selected {
                    option_id: option.to_owned(),
                },
                resolved_by: phone_actor(),
            },
            origin: Origin::Hub,
        }
    }

    fn open_approval(app: &App) -> bool {
        app.pending_approval().is_some()
    }

    /// A prompt typed in the terminal runs through the bridge, the approval is answered with
    /// Enter, and the turn finishes: all on screen, with the hub never contacted.
    #[tokio::test]
    async fn a_local_turn_with_an_approval_runs_and_stays_local() {
        let mut rig = rig(vec![permission_turn("Running.", "All green.")], false).await;
        rig.start().await;
        rig.say("run the tests").await;
        rig.until("the approval", open_approval).await;

        let shown = screen_sized(&rig.app, 60, 16);
        assert!(shown.contains("terminal> run the tests"), "{shown}");
        assert!(shown.contains("agent> Running."), "{shown}");
        assert!(shown.contains("Approval"), "{shown}");

        rig.press(Key::Enter).await;
        let task = rig.app.view().tasks()[0].task_id.clone();
        rig.until("the turn to finish", |app| {
            app.view()
                .task(&task)
                .is_some_and(|t| t.status() == client_core::TaskStatus::Succeeded)
        })
        .await;
        let shown = screen_sized(&rig.app, 60, 16);
        assert!(shown.contains("answered: Allow (by terminal)"), "{shown}");
        assert!(shown.contains("agent> All green."), "{shown}");
        assert!(shown.contains("remote: off"), "{shown}");

        assert_eq!(rig.registrations.load(Ordering::SeqCst), 0);
        assert!(rig.hub.sent.lock().expect("lock").is_empty());
        assert!(rig.hub.opened.lock().expect("lock").is_empty());
    }

    /// A phone's prompt and its answer arrive as hub commands: the screen shows them once, in
    /// order, and the agent runs them.
    #[tokio::test]
    async fn the_phones_prompt_and_answer_appear_on_the_terminal() {
        let mut rig = rig(vec![permission_turn("On it.", "Done.")], false).await;
        rig.handle
            .hub_command(hub_prompt(1, "tk_phone", "run the tests"))
            .await;
        rig.until("the approval", open_approval).await;
        let interrupt = rig
            .app
            .pending_approval()
            .expect("approval")
            .id()
            .to_owned();

        // The phone answers; the terminal's panel closes and the screen says who.
        rig.handle
            .hub_command(hub_approval(2, "tk_phone", &interrupt, "allow"))
            .await;
        rig.until("the turn to finish", |app| {
            app.view()
                .task("tk_phone")
                .is_some_and(|t| t.status() == client_core::TaskStatus::Succeeded)
        })
        .await;
        let shown = screen_sized(&rig.app, 60, 16);
        assert!(shown.contains("phone> run the tests"), "{shown}");
        assert!(shown.contains("answered: Allow (by phone)"), "{shown}");
        // The bridge did not add a second copy of a prompt the hub already has.
        assert_eq!(shown.matches("run the tests").count(), 1, "{shown}");
        assert!(rig.app.pending_approval().is_none());
        assert!(rig.task_succeeded("tk_phone"));
    }

    /// Terminal and phone both speak, in the order this runner saw them.
    #[tokio::test]
    async fn terminal_and_phone_turns_interleave_in_arrival_order() {
        let mut rig = rig(vec![plain_turn("First."), plain_turn("Second.")], false).await;
        rig.say("one from the terminal").await;
        rig.until("the first turn", |app| {
            app.view()
                .tasks()
                .first()
                .is_some_and(|t| t.status() == client_core::TaskStatus::Succeeded)
        })
        .await;
        rig.handle
            .hub_command(hub_prompt(1, "tk_phone", "two from the phone"))
            .await;
        rig.until("the second turn", |app| rig_done(app, "tk_phone"))
            .await;

        let shown = screen_sized(&rig.app, 60, 16);
        let at = |needle: &str| {
            shown
                .find(needle)
                .unwrap_or_else(|| panic!("{needle}\n{shown}"))
        };
        assert!(at("terminal> one from the terminal") < at("agent> First."));
        assert!(at("agent> First.") < at("phone> two from the phone"));
        assert!(at("phone> two from the phone") < at("agent> Second."));
    }

    fn rig_done(app: &App, task: &str) -> bool {
        app.view()
            .task(task)
            .is_some_and(|t| t.status() == client_core::TaskStatus::Succeeded)
    }

    /// Whoever's answer reaches this runner first wins; the other is told it was too late.
    #[tokio::test]
    async fn the_first_answer_to_reach_the_runner_wins() {
        let mut rig = rig(vec![permission_turn("Running.", "All green.")], false).await;
        rig.say("run the tests").await;
        rig.until("the approval", open_approval).await;
        let interrupt = rig
            .app
            .pending_approval()
            .expect("approval")
            .id()
            .to_owned();
        let task = rig.app.view().tasks()[0].task_id.clone();

        // The phone's command gets in; the terminal still shows the approval as open.
        rig.handle
            .hub_command(hub_approval(1, &task, &interrupt, "allow"))
            .await;
        let result = rig
            .session
            .answer(
                &interrupt,
                Outcome::Selected {
                    option_id: "reject".to_owned(),
                },
            )
            .await;
        assert_eq!(
            result,
            AnswerResult::AlreadyAnswered {
                by: Some(phone_actor())
            }
        );
        rig.app.answered(&interrupt, result);

        rig.until("the turn to finish", |app| rig_done(app, &task))
            .await;
        let shown = screen_sized(&rig.app, 60, 16);
        assert!(shown.contains("answered: Allow (by phone)"), "{shown}");
        assert!(!shown.contains("(by terminal)"), "{shown}");
    }

    /// With `autoConnect` the session registers at start; the hub gets the thread, then its events.
    #[tokio::test]
    async fn auto_connect_registers_the_session_at_start() {
        let mut rig = rig(vec![plain_turn("Hello.")], true).await;
        rig.start().await;
        assert_eq!(rig.registrations.load(Ordering::SeqCst), 1);
        assert_eq!(
            *rig.hub.opened.lock().expect("lock"),
            vec![(THREAD.to_owned(), "laptop-brave-otter".to_owned())]
        );
        assert_eq!(
            *rig.app.remote(),
            Remote::On {
                url: URL.to_owned()
            }
        );

        rig.say("hi").await;
        rig.until("the turn", |app| {
            app.view()
                .tasks()
                .first()
                .is_some_and(|t| t.status() == client_core::TaskStatus::Succeeded)
        })
        .await;
        // The hub has the terminal's own prompt and the agent's turn, producer side (seq 0).
        let kinds = rig.hub.kinds();
        assert_eq!(
            kinds.first().map(String::as_str),
            Some("UserMessage"),
            "{kinds:?}"
        );
        assert_eq!(
            kinds.last().map(String::as_str),
            Some("RunFinished"),
            "{kinds:?}"
        );
        assert!(
            rig.hub
                .sent
                .lock()
                .expect("lock")
                .iter()
                .all(|envelope| envelope.seq == 0)
        );
        assert!(screen(&rig.app).contains("remote: on"));
    }

    /// Without it nothing registers until `/rc`, which then sends what happened before; a second
    /// `/rc` stops sending, and the next one catches up without a second `thread_open`.
    #[tokio::test]
    async fn without_auto_connect_the_session_registers_after_rc_and_catches_up() {
        let mut rig = rig(
            vec![plain_turn("One."), plain_turn("Two."), plain_turn("Three.")],
            false,
        )
        .await;
        rig.start().await;
        assert_eq!(rig.registrations.load(Ordering::SeqCst), 0);

        rig.say("first").await;
        rig.until("turn one", |app| rig_done(app, "tk_t11")).await;
        assert_eq!(rig.registrations.load(Ordering::SeqCst), 0, "no /rc yet");
        assert!(rig.hub.sent.lock().expect("lock").is_empty());

        rig.say("/rc").await;
        assert_eq!(rig.registrations.load(Ordering::SeqCst), 1);
        assert_eq!(rig.hub.opened.lock().expect("lock").len(), 1);
        let after_rc = rig.hub.kinds();
        assert_eq!(
            after_rc.first().map(String::as_str),
            Some("UserMessage"),
            "the history goes first: {after_rc:?}"
        );
        assert_eq!(after_rc.last().map(String::as_str), Some("RunFinished"));
        let screen_now = screen(&rig.app);
        assert!(screen_now.contains(&format!("link: {URL}")), "{screen_now}");

        // Stop sharing: the next turn stays here.
        rig.say("/rc").await;
        assert_eq!(*rig.app.remote(), Remote::Off);
        rig.say("second").await;
        rig.until("turn two", |app| rig_done(app, "tk_t12")).await;
        assert_eq!(rig.hub.kinds(), after_rc);

        // Share again: what was missed is sent, and the thread is not opened a second time.
        rig.say("/rc").await;
        assert_eq!(rig.registrations.load(Ordering::SeqCst), 2);
        assert_eq!(rig.hub.opened.lock().expect("lock").len(), 1);
        assert!(rig.hub.kinds().len() > after_rc.len());
        let sent = rig.hub.sent.lock().expect("lock");
        let user_messages = sent
            .iter()
            .filter(|e| matches!(e.event, Event::UserMessage(_)))
            .count();
        assert_eq!(user_messages, 2, "each prompt went up once");
    }

    /// What the phone said is the hub's already: it is shown here and never sent back up.
    #[tokio::test]
    async fn the_phones_own_messages_are_not_sent_back_to_the_hub() {
        let mut rig = rig(vec![plain_turn("Hi phone.")], true).await;
        rig.start().await;
        rig.handle
            .hub_command(hub_prompt(1, "tk_phone", "hello from the phone"))
            .await;
        rig.until("the turn", |app| rig_done(app, "tk_phone")).await;
        assert!(screen(&rig.app).contains("phone> hello from the phone"));
        let sent = rig.hub.sent.lock().expect("lock");
        assert!(
            !sent
                .iter()
                .any(|envelope| matches!(&envelope.event, Event::UserMessage(_))),
            "the hub numbered that message itself"
        );
        assert!(
            sent.iter()
                .any(|envelope| matches!(envelope.event, Event::RunFinished(_)))
        );
    }

    /// A resent hub command (after a reconnect) is not written to the log twice.
    #[tokio::test]
    async fn a_resent_hub_command_is_logged_once() {
        let mut rig = rig(vec![plain_turn("Hi.")], false).await;
        rig.handle
            .hub_command(hub_prompt(1, "tk_phone", "only once"))
            .await;
        rig.handle
            .hub_command(hub_prompt(1, "tk_phone", "only once"))
            .await;
        rig.until("the turn", |app| rig_done(app, "tk_phone")).await;
        assert_eq!(screen(&rig.app).matches("only once").count(), 1);
    }

    /// An answer typed for an interrupt that does not exist is refused, not logged.
    #[tokio::test]
    async fn an_answer_to_an_unknown_approval_fails() {
        let mut rig = rig(vec![plain_turn("Hi.")], false).await;
        let result = rig
            .session
            .answer(
                "int_nope",
                Outcome::Selected {
                    option_id: "allow".to_owned(),
                },
            )
            .await;
        assert!(matches!(result, AnswerResult::Failed(_)), "{result:?}");
    }

    /// Shared session: the answer goes to the hub, which accepts it and sends the `approval`
    /// command back; only that command reaches the agent, and the terminal's answer is not an
    /// event sent up.
    #[tokio::test]
    async fn a_shared_answer_is_applied_only_after_the_hub_accepts_it() {
        let mut rig = rig(vec![permission_turn("Running.", "All green.")], true).await;
        rig.start().await;
        *rig.hub.verdict.lock().expect("lock") = Some(Ok(HubVerdict::Accepted));
        *rig.hub.approve_with.lock().expect("lock") = Some((rig.handle.clone(), 7));
        rig.say("run the tests").await;
        rig.until("the approval", open_approval).await;
        let interrupt = rig
            .app
            .pending_approval()
            .expect("approval")
            .id()
            .to_owned();
        let task = rig.app.view().tasks()[0].task_id.clone();
        assert!(
            !rig.hub.kinds().contains(&"InterruptResolved".to_owned()),
            "nothing is applied before the hub decides"
        );

        rig.press(Key::Enter).await;
        rig.until("the turn to finish", |app| rig_done(app, &task))
            .await;
        assert_eq!(
            *rig.hub.answers.lock().expect("lock"),
            vec![(
                interrupt,
                Outcome::Selected {
                    option_id: "allow".to_owned()
                }
            )],
            "the answer went up to the hub"
        );
        let shown = screen_sized(&rig.app, 60, 16);
        assert!(shown.contains("answered: Allow (by terminal)"), "{shown}");
        assert!(shown.contains("agent> All green."), "{shown}");
        assert!(
            !rig.hub.kinds().contains(&"InterruptResolved".to_owned()),
            "the hub's log gets the answer from the hub, not as a runner event: {:?}",
            rig.hub.kinds()
        );
    }

    /// The phone answered first: the hub refuses the terminal's answer, the terminal says
    /// "already answered", nothing of the terminal's choice reaches the agent, and the phone's
    /// choice (its `approval` command) is what runs.
    #[tokio::test]
    async fn a_terminal_answer_that_loses_to_the_phone_at_the_hub_shows_already_answered() {
        let mut rig = rig(vec![permission_turn("Running.", "All green.")], true).await;
        rig.start().await;
        rig.say("run the tests").await;
        rig.until("the approval", open_approval).await;
        let interrupt = rig
            .app
            .pending_approval()
            .expect("approval")
            .id()
            .to_owned();
        let task = rig.app.view().tasks()[0].task_id.clone();

        // The terminal picks Reject; at the hub the phone's Allow got there first.
        *rig.hub.verdict.lock().expect("lock") = Some(Ok(HubVerdict::AlreadyAnswered {
            by: Some(phone_actor()),
        }));
        let reject = Outcome::Selected {
            option_id: "reject".to_owned(),
        };
        let result = rig.session.answer(&interrupt, reject.clone()).await;
        assert_eq!(
            result,
            AnswerResult::AlreadyAnswered {
                by: Some(phone_actor())
            }
        );
        rig.app.answered(&interrupt, result);
        assert!(screen(&rig.app).contains("already answered by phone"));
        assert_eq!(
            *rig.hub.answers.lock().expect("lock"),
            vec![(interrupt.clone(), reject)]
        );
        assert!(rig.app.pending_approval().is_none());

        // The phone's command arrives; the agent runs the phone's choice (the mock script
        // expects `allow`, so a `reject` anywhere would fail the turn).
        rig.handle
            .hub_command(hub_approval(1, &task, &interrupt, "allow"))
            .await;
        rig.until("the turn to finish", |app| rig_done(app, &task))
            .await;
        let shown = screen_sized(&rig.app, 60, 16);
        assert!(shown.contains("answered: Allow (by phone)"), "{shown}");
        assert!(!shown.contains("(by terminal)"), "{shown}");
        assert!(
            !rig.hub.kinds().contains(&"InterruptResolved".to_owned()),
            "{:?}",
            rig.hub.kinds()
        );
    }

    /// No verdict from the hub (tunnel down): the answer is not applied anywhere and the
    /// approval stays open to try again; a rejection reads the same.
    #[tokio::test]
    async fn without_a_verdict_from_the_hub_a_shared_answer_is_not_applied() {
        let mut rig = rig(vec![permission_turn("Running.", "All green.")], true).await;
        rig.start().await;
        rig.say("run the tests").await;
        rig.until("the approval", open_approval).await;
        let interrupt = rig
            .app
            .pending_approval()
            .expect("approval")
            .id()
            .to_owned();
        let task = rig.app.view().tasks()[0].task_id.clone();

        *rig.hub.verdict.lock().expect("lock") = Some(Err("the hub has not answered yet".into()));
        let allow = Outcome::Selected {
            option_id: "allow".to_owned(),
        };
        let result = rig.session.answer(&interrupt, allow.clone()).await;
        assert_eq!(
            result,
            AnswerResult::Failed("the hub has not answered yet".to_owned())
        );
        rig.app.answered(&interrupt, result);
        let shown = screen(&rig.app);
        assert!(shown.contains("your answer was not sent"), "{shown}");
        assert!(open_approval(&rig.app), "still open for another try");
        assert!(
            !rig.task_succeeded(&task),
            "the agent was not told anything"
        );

        *rig.hub.verdict.lock().expect("lock") =
            Some(Ok(HubVerdict::Rejected("expired: too late".to_owned())));
        let result = rig.session.answer(&interrupt, allow).await;
        assert_eq!(result, AnswerResult::Failed("expired: too late".to_owned()));
    }
}
