//! Native tests of the machine core and driver (M2B-002). The runner side is simulated with
//! the protocol crate's own `Sender` / `Receiver` / `DedupeWindow`, the Thread DO with a fake
//! [`ThreadHost`]. The live tests in `tests/machine_do.rs` cover the Durable Object itself.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::future::{Future, ready};
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use protocol::tunnel::{
    Actor, ActorKind, AnswerStatus, Capabilities, CloseCode, Command, CommandErrorCode,
    ContentPart, DedupeWindow, Disposition, Frame, NackCode, PresenceState, Receiver, Sender,
    Verdict,
};
use protocol::turn_event::{Envelope, TextMessageContent, format_timestamp};
use rusqlite::params_from_iter;
use rusqlite::types::Value as SqlValue;
use serde_json::{Value, json};

use super::admin::{self, MachineControl};
use super::core::{Auth, Heartbeat, HelloInfo, MachineCore, Out, PresenceChange, PushBody};
use super::credential;
use super::driver::{
    self, AnswerVerdict, Core, HostError, MachineAnswer, MachinePrompt, ThreadHost, handle_text,
};
use super::store::{HostKind, MemoryStore};
use crate::auth::{Principal, authenticate};
use crate::db::{Db, DbError, MIGRATIONS, Param, Role, Row};
use crate::machines::core::PushCommand;
use crate::threads::ApiError;

const NOW: u64 = 1_793_138_403_000;
const DAY: u64 = 24 * 60 * 60 * 1000;
const MACHINE: &str = "mch_1";
const THREAD: &str = "th_1";

fn block_on<T>(future: impl Future<Output = T>) -> T {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("future pending: the fakes never yield"),
    }
}

// ---- the fake thread host ----------------------------------------------------------------

/// A local prompt as the fake host records it: thread, task id, message id, text, actor id.
type LocalPrompt = (String, String, String, String, String);

#[derive(Default)]
struct FakeHost {
    appended: RefCell<Vec<(HostKind, String, Value)>>,
    /// The `event_id` passed with each append (`None` for hub-made envelopes).
    appended_ids: RefCell<Vec<Option<String>>>,
    fail_next: RefCell<VecDeque<HostError>>,
    owners: RefCell<HashMap<String, String>>,
    inits: RefCell<Vec<(String, Value)>>,
    listed: RefCell<HashMap<String, Vec<PushCommand>>>,
    acked: RefCell<Vec<(String, Vec<String>)>>,
    presence: RefCell<Vec<String>>,
    /// The capabilities mirrored at each accepted hello.
    capabilities: RefCell<Vec<Value>>,
    /// Each interrupt answer passed to the thread host: thread, answer id, interrupt id, actor id.
    answers: RefCell<Vec<(String, String, String, String)>>,
    /// Each local prompt passed to the thread host: thread, task id, message id, text, actor id.
    prompts: RefCell<Vec<LocalPrompt>>,
    /// The verdicts the Thread DO gives, oldest first; `accepted` when empty.
    verdicts: RefCell<VecDeque<AnswerVerdict>>,
}

impl FakeHost {
    /// The `event.type` names appended so far, `Presence` excluded.
    fn types(&self) -> Vec<String> {
        self.appended
            .borrow()
            .iter()
            .map(|(_, _, e)| e["event"]["type"].as_str().unwrap_or_default().to_owned())
            .filter(|t| t != "Presence")
            .collect()
    }

    fn deltas(&self) -> Vec<String> {
        self.appended
            .borrow()
            .iter()
            .filter(|(_, _, e)| e["event"]["type"] == "TextMessageContent")
            .map(|(_, _, e)| e["event"]["delta"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    fn presence_events(&self) -> Vec<(String, String)> {
        self.appended
            .borrow()
            .iter()
            .filter(|(_, _, e)| e["event"]["type"] == "Presence")
            .map(|(_, thread, e)| {
                (
                    thread.clone(),
                    e["event"]["state"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect()
    }
}

impl ThreadHost for FakeHost {
    fn append(
        &self,
        host: HostKind,
        thread_id: &str,
        event_id: Option<&str>,
        envelope: &Value,
    ) -> impl Future<Output = Result<(), HostError>> {
        let failure = self.fail_next.borrow_mut().pop_front();
        ready(match failure {
            Some(error) => Err(error),
            None => {
                self.appended_ids
                    .borrow_mut()
                    .push(event_id.map(str::to_owned));
                self.appended
                    .borrow_mut()
                    .push((host, thread_id.to_owned(), envelope.clone()));
                Ok(())
            }
        })
    }

    fn answer_interrupt(
        &self,
        thread_id: &str,
        answer: &MachineAnswer<'_>,
    ) -> impl Future<Output = Result<AnswerVerdict, HostError>> {
        self.answers.borrow_mut().push((
            thread_id.to_owned(),
            answer.answer_id.to_owned(),
            answer.interrupt_id.to_owned(),
            answer.actor.id.clone(),
        ));
        let verdict = self.verdicts.borrow_mut().pop_front();
        ready(Ok(verdict.unwrap_or_else(|| AnswerVerdict {
            status: AnswerStatus::Accepted,
            resolved_by: Some(answer.actor.clone()),
            message: "accepted".to_owned(),
        })))
    }

    fn submit_prompt(
        &self,
        thread_id: &str,
        prompt: &MachinePrompt<'_>,
    ) -> impl Future<Output = Result<(), HostError>> {
        if let Some(error) = self.fail_next.borrow_mut().pop_front() {
            return ready(Err(error));
        }
        let text = prompt
            .parts
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => text.as_str(),
                ContentPart::File { .. } => "",
            })
            .collect::<String>();
        self.prompts.borrow_mut().push((
            thread_id.to_owned(),
            prompt.task_id.to_owned(),
            prompt.message_id.to_owned(),
            text,
            prompt.actor.id.clone(),
        ));
        ready(Ok(()))
    }

    fn thread_machine(
        &self,
        thread_id: &str,
    ) -> impl Future<Output = Result<Option<String>, HostError>> {
        ready(Ok(self.owners.borrow().get(thread_id).cloned()))
    }

    fn init_thread(
        &self,
        thread_id: &str,
        info: &Value,
    ) -> impl Future<Output = Result<(), HostError>> {
        self.inits
            .borrow_mut()
            .push((thread_id.to_owned(), info.clone()));
        ready(Ok(()))
    }

    fn list_commands(
        &self,
        _host: HostKind,
        thread_id: &str,
    ) -> impl Future<Output = Result<Vec<PushCommand>, HostError>> {
        ready(Ok(self
            .listed
            .borrow()
            .get(thread_id)
            .cloned()
            .unwrap_or_default()))
    }

    fn ack_commands(
        &self,
        _host: HostKind,
        thread_id: &str,
        source_ids: &[String],
    ) -> impl Future<Output = Result<(), HostError>> {
        self.acked
            .borrow_mut()
            .push((thread_id.to_owned(), source_ids.to_vec()));
        ready(Ok(()))
    }

    fn record_presence(
        &self,
        _machine_id: &str,
        change: &PresenceChange,
        _now_ms: u64,
    ) -> impl Future<Output = ()> {
        self.presence
            .borrow_mut()
            .push(serde_json::to_value(change.state).map_or_else(
                |_| String::new(),
                |v| v.as_str().unwrap_or_default().to_owned(),
            ));
        ready(())
    }

    fn record_capabilities(
        &self,
        _machine_id: &str,
        capabilities: &Value,
    ) -> impl Future<Output = ()> {
        self.capabilities.borrow_mut().push(capabilities.clone());
        ready(())
    }
}

// ---- the simulated runner ----------------------------------------------------------------

struct Runner {
    /// Up frames without their `seq` (JSON objects with `t`); `seq` is set when sent.
    up: Sender<Value>,
    down: Receiver,
    keys: DedupeWindow,
    token: Option<String>,
    got: Vec<Command>,
    closes: Vec<(CloseCode, String)>,
    nacks: Vec<(u64, NackCode)>,
    /// `interrupt_answer_result` frames received: answer id, status, winner.
    results: Vec<(String, AnswerStatus, Option<Actor>)>,
    welcome: Option<(bool, u64)>,
    seen_down: Vec<u64>,
    next_event: u32,
}

impl Runner {
    fn new() -> Self {
        Self {
            up: Sender::new(256),
            down: Receiver::fresh(),
            keys: DedupeWindow::idempotency_keys(),
            token: None,
            got: Vec::new(),
            closes: Vec::new(),
            nacks: Vec::new(),
            results: Vec::new(),
            welcome: None,
            seen_down: Vec::new(),
            next_event: 0,
        }
    }

    /// A runner that lost its state (no token, empty outbox) but keeps the command store.
    fn restarted(&self) -> Self {
        let mut runner = Self::new();
        runner.keys = self.keys.clone();
        runner.next_event = self.next_event;
        runner
    }

    fn hello(&self) -> Frame {
        Frame::Hello {
            proto: 1,
            machine_id: MACHINE.to_owned(),
            runner_version: "0.0.0".to_owned(),
            resume_token: self.token.clone(),
            last_down_seq: self.down.last(),
            capabilities: Capabilities {
                host_mode: "local".to_owned(),
                harnesses: vec!["claude".to_owned()],
                max_sessions: 4,
                spawn: vec!["same_dir".to_owned()],
                features: Vec::new(),
            },
        }
    }

    fn receive(&mut self, outs: Vec<Out>, now: u64) {
        for out in outs {
            match out {
                Out::Send(frame) => match *frame {
                    Frame::Welcome {
                        resumed,
                        resume_token,
                        last_up_seq,
                        ..
                    } => {
                        self.up
                            .on_welcome(resumed, last_up_seq)
                            .expect("welcome cursor");
                        if !resumed {
                            self.down = Receiver::fresh();
                        }
                        self.token = Some(resume_token);
                        self.welcome = Some((resumed, last_up_seq));
                    }
                    Frame::Command {
                        seq,
                        idempotency_key,
                        command,
                        ..
                    } => {
                        self.seen_down.push(seq);
                        if let Ok(Verdict::Process) = self.down.on_frame(seq)
                            && self.keys.observe(&idempotency_key, now) == Disposition::New
                        {
                            self.got.push(command);
                        }
                    }
                    Frame::Ack { seq } => self.up.on_ack(seq).expect("ack"),
                    Frame::Nack { seq, code, .. } => {
                        self.up.on_nack(seq).expect("nack");
                        self.nacks.push((seq, code));
                    }
                    Frame::InterruptAnswerResult {
                        answer_id,
                        status,
                        resolved_by,
                        ..
                    } => self.results.push((answer_id, status, resolved_by)),
                    _ => {}
                },
                Out::Close(code, message) => self.closes.push((code, message)),
            }
        }
    }

    fn event(&mut self, thread: &str, delta: &str) -> String {
        let envelope = Envelope::new(
            thread,
            Some("tk_1".to_owned()),
            Some("run_1".to_owned()),
            format_timestamp(NOW),
            TextMessageContent {
                message_id: "msg_a".to_owned(),
                delta: delta.to_owned(),
            },
        );
        self.event_value(serde_json::to_value(&envelope).expect("envelope"))
    }

    /// Queues an `event` frame with an arbitrary envelope; returns its `event_id`.
    fn event_value(&mut self, envelope: Value) -> String {
        self.next_event += 1;
        let event_id = format!("evt_{}", self.next_event);
        self.event_with_id(&event_id, envelope);
        event_id
    }

    fn event_with_id(&mut self, event_id: &str, envelope: Value) {
        self.up
            .enqueue(json!({"t": "event", "event_id": event_id, "envelope": envelope}));
    }

    fn up_frame(&mut self, frame: Value) {
        self.up.enqueue(frame);
    }
}

struct World {
    core: Core<MemoryStore>,
    host: FakeHost,
    credential: String,
}

fn new_world() -> World {
    let credential = credential::new_credential().expect("credential");
    let mut core = MachineCore::open(MemoryStore::new()).expect("open");
    core.provision(MACHINE, "org_1", &credential::hash(&credential), false)
        .expect("provision");
    core.host_thread(THREAD, HostKind::Thread, NOW)
        .expect("host thread");
    World {
        core: RefCell::new(core),
        host: FakeHost::default(),
        credential,
    }
}

impl World {
    /// Accepts a socket and handles one frame on it.
    fn deliver(&self, conn: u64, runner: &mut Runner, text: &str, now: u64) {
        let mut outs = Vec::new();
        block_on(handle_text(
            &self.core,
            &self.host,
            conn,
            text,
            now,
            &mut |o| outs.push(o),
        ));
        runner.receive(outs, now);
    }

    fn frame(&self, conn: u64, runner: &mut Runner, frame: &Frame, now: u64) {
        let text = serde_json::to_string(frame).expect("frame");
        self.deliver(conn, runner, &text, now);
    }

    fn accept(&self, now: u64) -> u64 {
        self.core
            .borrow_mut()
            .begin_connection(now)
            .expect("connection")
    }

    /// Accepts a socket and completes the hello / welcome exchange.
    fn connect(&self, runner: &mut Runner, now: u64) -> u64 {
        let conn = self.accept(now);
        let hello = runner.hello();
        self.frame(conn, runner, &hello, now);
        conn
    }

    /// Sends every up frame the window allows, then the batched ack.
    fn pump_up(&self, conn: u64, runner: &mut Runner, now: u64) {
        while let Some((seq, mut body)) = runner.up.next_to_send() {
            body["seq"] = json!(seq);
            self.deliver(conn, runner, &body.to_string(), now);
        }
        if let Some(ack) = driver::flush_ack(&self.core) {
            runner.receive(vec![ack], now);
        }
    }

    /// The runner acks every command it has received.
    fn ack_down(&self, conn: u64, runner: &mut Runner, now: u64) {
        if let Some(seq) = runner.down.take_ack() {
            self.frame(conn, runner, &Frame::Ack { seq }, now);
        }
    }

    fn push(&self, runner: &mut Runner, commands: Vec<PushCommand>, snapshot: bool, now: u64) {
        let mut outs = Vec::new();
        let body = PushBody {
            thread_id: THREAD.to_owned(),
            host: HostKind::Thread,
            snapshot,
            commands,
        };
        block_on(driver::push_commands(
            &self.core,
            &self.host,
            body,
            now,
            &mut |o| outs.push(o),
        ))
        .expect("store")
        .expect("push accepted");
        runner.receive(outs, now);
    }

    fn drop_socket(&self, conn: u64, heartbeat_timeout: bool, now: u64) {
        block_on(driver::connection_closed(
            &self.core,
            &self.host,
            conn,
            heartbeat_timeout,
            now,
        ))
        .expect("close");
    }

    fn presence(&self) -> String {
        self.core.borrow().state().presence.clone()
    }
}

fn author() -> Actor {
    Actor {
        kind: ActorKind::User,
        id: "usr_1".to_owned(),
        name: None,
        device_id: None,
    }
}

fn prompt(id: u32) -> PushCommand {
    PushCommand {
        source_id: id.to_string(),
        idempotency_key: format!("idk_{id}"),
        issued_at: format_timestamp(NOW),
        command: Command::Prompt {
            thread_id: THREAD.to_owned(),
            task_id: format!("tk_{id}"),
            message_id: format!("msg_{id}"),
            parts: vec![ContentPart::Text {
                text: format!("prompt {id}"),
            }],
            author: author(),
        },
    }
}

fn task_ids(runner: &Runner) -> Vec<String> {
    runner
        .got
        .iter()
        .map(|c| match c {
            Command::Prompt { task_id, .. } => task_id.clone(),
            other => panic!("unexpected command {other:?}"),
        })
        .collect()
}

// ---- auth, hello, presence -----------------------------------------------------------------

#[test]
fn credential_identifies_the_machine_and_revocation_ends_it() {
    let world = new_world();
    let core = &world.core;
    let bearer = |c: &str| format!("Bearer {c}");
    assert_eq!(core.borrow().authenticate(None), Auth::Missing);
    assert_eq!(core.borrow().authenticate(Some("Basic x")), Auth::Missing);
    let other = credential::new_credential().expect("credential");
    assert_eq!(
        core.borrow().authenticate(Some(&bearer(&other))),
        Auth::Invalid,
        "another machine's credential is not this machine's"
    );
    assert_eq!(
        core.borrow().authenticate(Some("Bearer smt_session")),
        Auth::Invalid
    );
    assert_eq!(
        core.borrow().authenticate(Some(&bearer(&world.credential))),
        Auth::Ok
    );
    block_on(driver::revoke(core, &world.host, NOW)).expect("revoke");
    assert_eq!(
        core.borrow().authenticate(Some(&bearer(&world.credential))),
        Auth::Revoked
    );
    assert_eq!(
        core.borrow().authenticate(Some(&bearer(&other))),
        Auth::Invalid
    );
    // An unprovisioned object authenticates nobody.
    let empty = MachineCore::open(MemoryStore::new()).expect("open");
    assert_eq!(
        empty.authenticate(Some(&bearer(&world.credential))),
        Auth::Invalid
    );
}

#[test]
fn hello_mirrors_the_runners_capabilities_into_d1() {
    let world = new_world();
    let mut runner = Runner::new();
    world.connect(&mut runner, NOW);
    let recorded = world.host.capabilities.borrow().clone();
    assert_eq!(recorded.len(), 1, "one accepted hello, one mirror");
    assert_eq!(recorded[0]["host_mode"], "local");
    assert_eq!(recorded[0]["harnesses"], json!(["claude"]));
    assert_eq!(recorded[0]["max_sessions"], 4);
    assert_eq!(recorded[0]["spawn"], json!(["same_dir"]));
}

#[test]
fn hello_gets_welcome_and_publishes_online_to_hosted_threads() {
    let world = new_world();
    let mut runner = Runner::new();
    world.connect(&mut runner, NOW);
    assert_eq!(runner.welcome, Some((false, 0)));
    assert!(
        runner
            .token
            .as_deref()
            .is_some_and(|t| t.starts_with("rt_"))
    );
    assert_eq!(world.presence(), "online");
    assert_eq!(
        world.host.presence_events(),
        vec![(THREAD.to_owned(), "online".to_owned())]
    );
    assert_eq!(*world.host.presence.borrow(), vec!["online".to_owned()]);
    // Only the hash of the token is stored.
    let stored = world
        .core
        .borrow()
        .state()
        .resume_hash
        .clone()
        .expect("hash");
    assert_ne!(Some(&stored), runner.token.as_ref());
    assert!(credential::matches(
        &stored,
        runner.token.as_deref().unwrap_or("")
    ));
}

#[test]
fn hello_is_checked_before_welcome() {
    let world = new_world();
    let mut runner = Runner::new();

    // A frame before hello.
    let conn = world.accept(NOW);
    world.frame(conn, &mut runner, &Frame::Ack { seq: 1 }, NOW);
    assert_eq!(
        runner.closes.pop().map(|c| c.0),
        Some(CloseCode::ProtocolError)
    );

    // The wrong protocol version.
    let conn = world.accept(NOW);
    let mut hello = runner.hello();
    if let Frame::Hello { proto, .. } = &mut hello {
        *proto = 2;
    }
    world.frame(conn, &mut runner, &hello, NOW);
    assert_eq!(
        runner.closes.pop().map(|c| c.0),
        Some(CloseCode::VersionUnsupported)
    );

    // A machine id that is not the credential's.
    let conn = world.accept(NOW);
    let mut hello = runner.hello();
    if let Frame::Hello { machine_id, .. } = &mut hello {
        *machine_id = "mch_other".to_owned();
    }
    world.frame(conn, &mut runner, &hello, NOW);
    assert_eq!(
        runner.closes.pop().map(|c| c.0),
        Some(CloseCode::AuthInvalid)
    );
    assert_eq!(world.presence(), "offline", "no hello was accepted");

    // A second hello on the same socket.
    let conn = world.connect(&mut runner, NOW);
    let hello = runner.hello();
    world.frame(conn, &mut runner, &hello, NOW);
    assert_eq!(
        runner.closes.pop().map(|c| c.0),
        Some(CloseCode::ProtocolError)
    );
}

#[test]
fn frames_are_parsed_strictly() {
    let world = new_world();
    let mut runner = Runner::new();
    let conn = world.connect(&mut runner, NOW);
    for bad in [
        "not json",
        r#"{"t":"nope"}"#,
        r#"{"t":"ack","seq":1,"extra":true}"#,
        r#"{"t":"ping","x":1}"#,
    ] {
        world.deliver(conn, &mut runner, bad, NOW);
        assert_eq!(
            runner.closes.pop().map(|c| c.0),
            Some(CloseCode::ProtocolError),
            "{bad}"
        );
    }
    let huge = format!(
        r#"{{"t":"event","seq":1,"event_id":"e","envelope":{{"pad":"{}"}}}}"#,
        "x".repeat(protocol::tunnel::MAX_FRAME_BYTES)
    );
    world.deliver(conn, &mut runner, &huge, NOW);
    assert_eq!(
        runner.closes.pop().map(|c| c.0),
        Some(CloseCode::ProtocolError)
    );
    // The heartbeat the runtime does not auto-answer still gets a pong.
    world.deliver(conn, &mut runner, r#"{"t":"ping"}"#, NOW);
    assert!(runner.closes.is_empty());
}

#[test]
fn presence_follows_the_socket_and_the_asleep_hint() {
    // Socket lost without a hint: offline.
    let world = new_world();
    let mut runner = Runner::new();
    let conn = world.connect(&mut runner, NOW);
    world.drop_socket(conn, false, NOW + 1_000);
    assert_eq!(world.presence(), "offline");

    // `presence{asleep}`, then the socket is lost: asleep.
    let world = world_with_runner(&mut runner);
    let conn = world.connect(&mut runner, NOW);
    let hint = Frame::Presence {
        state: PresenceState::Asleep,
        reason: "os_sleep".to_owned(),
    };
    world.frame(conn, &mut runner, &hint, NOW);
    assert_eq!(world.presence(), "online", "a hint alone changes nothing");
    world.drop_socket(conn, false, NOW + 2_000);
    assert_eq!(world.presence(), "asleep");
    assert_eq!(
        world.host.presence_events().last().map(|e| e.1.as_str()),
        Some("asleep")
    );

    // Heartbeat silence is offline even after a hint (tunnel.md §3.7).
    let world = world_with_runner(&mut runner);
    let conn = world.connect(&mut runner, NOW);
    world.frame(
        conn,
        &mut runner,
        &Frame::Close {
            code: CloseCode::GoingToSleep,
            message: "sleep".to_owned(),
        },
        NOW,
    );
    world.drop_socket(conn, true, NOW + 90_000);
    assert_eq!(world.presence(), "offline");

    // A `normal` close clears the hint: the runner is shutting down.
    let world = world_with_runner(&mut runner);
    let conn = world.connect(&mut runner, NOW);
    let hint = Frame::Presence {
        state: PresenceState::Asleep,
        reason: "x".to_owned(),
    };
    world.frame(conn, &mut runner, &hint, NOW);
    world.frame(
        conn,
        &mut runner,
        &Frame::Close {
            code: CloseCode::Normal,
            message: "bye".to_owned(),
        },
        NOW,
    );
    world.drop_socket(conn, false, NOW + 1_000);
    assert_eq!(world.presence(), "offline");
}

/// A fresh world for the same runner identity (the runner keeps nothing it must not).
fn world_with_runner(runner: &mut Runner) -> World {
    *runner = Runner::new();
    new_world()
}

#[test]
fn heartbeat_alarm_distinguishes_healthy_overdue_and_silent_sockets() {
    let world = new_world();
    let mut runner = Runner::new();
    assert_eq!(world.core.borrow().heartbeat(NOW, None), Heartbeat::Idle);
    let conn = world.accept(NOW);
    assert!(matches!(
        world.core.borrow().heartbeat(NOW + 1_000, None),
        Heartbeat::Healthy { .. }
    ));
    assert_eq!(
        world.core.borrow().heartbeat(NOW + 11_000, None),
        Heartbeat::HelloTimeout
    );
    let hello = runner.hello();
    world.frame(conn, &mut runner, &hello, NOW);
    let core = world.core.borrow();
    assert!(matches!(
        core.heartbeat(NOW + 59_000, None),
        Heartbeat::Healthy { .. }
    ));
    assert_eq!(core.heartbeat(NOW + 61_000, None), Heartbeat::Overdue);
    // A ping the runtime auto-answered counts as life.
    assert!(matches!(
        core.heartbeat(NOW + 61_000, Some(NOW + 50_000)),
        Heartbeat::Healthy { .. }
    ));
}

#[test]
fn a_superseded_socket_is_ignored_and_does_not_change_presence() {
    let world = new_world();
    let mut first = Runner::new();
    let old = world.connect(&mut first, NOW);
    assert_eq!(world.presence(), "online");

    let mut second = first.restarted();
    let new = world.connect(&mut second, NOW + 1_000);
    assert_ne!(old, new);
    // The old socket's late traffic and its close are not the machine's.
    world.frame(old, &mut first, &Frame::Ack { seq: 1 }, NOW + 1_001);
    assert!(first.closes.is_empty());
    world.drop_socket(old, false, NOW + 1_002);
    assert_eq!(world.presence(), "online");
    assert!(world.core.borrow().is_current(new));
    // Presence was published once: the new hello found the machine already online.
    assert_eq!(world.host.presence_events().len(), 1);
}

// ---- up stream -----------------------------------------------------------------------------

fn answer_frame(event_id: &str, thread: &str, answer_id: &str) -> Value {
    json!({
        "t": "interrupt_answer",
        "event_id": event_id,
        "thread_id": thread,
        "interrupt_id": "int_1",
        "answer_id": answer_id,
        "outcome": { "type": "selected", "option_id": "allow" },
    })
}

#[test]
fn an_interrupt_answer_goes_to_the_thread_host_as_the_local_user_and_the_verdict_comes_back() {
    let world = new_world();
    let phone = Actor {
        kind: ActorKind::User,
        id: "usr_1".to_owned(),
        name: None,
        device_id: Some("dev_phone".to_owned()),
    };
    world.host.verdicts.borrow_mut().extend([
        AnswerVerdict {
            status: AnswerStatus::Accepted,
            resolved_by: Some(driver::local_user(MACHINE)),
            message: "accepted".to_owned(),
        },
        AnswerVerdict {
            status: AnswerStatus::AlreadyResolved,
            resolved_by: Some(phone.clone()),
            message: "this interrupt was already answered".to_owned(),
        },
    ]);
    let mut runner = Runner::new();
    let conn = world.connect(&mut runner, NOW);
    runner.up_frame(answer_frame("evt_a", THREAD, "ans_1"));
    runner.up_frame(answer_frame("evt_b", THREAD, "ans_2"));
    world.pump_up(conn, &mut runner, NOW);

    assert_eq!(
        *world.host.answers.borrow(),
        vec![
            (
                THREAD.to_owned(),
                "ans_1".to_owned(),
                "int_1".to_owned(),
                format!("local:{MACHINE}")
            ),
            (
                THREAD.to_owned(),
                "ans_2".to_owned(),
                "int_1".to_owned(),
                format!("local:{MACHINE}")
            ),
        ],
        "the answer is resolved by the thread host, not appended as an event"
    );
    assert!(
        world.host.types().is_empty(),
        "no InterruptResolved event is appended by the machine"
    );
    assert_eq!(runner.results.len(), 2);
    assert_eq!(runner.results[0].0, "ans_1");
    assert_eq!(runner.results[0].1, AnswerStatus::Accepted);
    assert_eq!(runner.results[1].0, "ans_2");
    assert_eq!(runner.results[1].1, AnswerStatus::AlreadyResolved);
    assert_eq!(
        runner.results[1].2,
        Some(phone),
        "the loser is told who won"
    );
    assert!(runner.nacks.is_empty());
}

#[test]
fn an_interrupt_answer_for_a_thread_the_machine_does_not_host_is_nacked_and_a_retransmit_is_not_resolved_twice()
 {
    let world = new_world();
    let mut runner = Runner::new();
    let conn = world.connect(&mut runner, NOW);
    runner.up_frame(answer_frame("evt_a", "th_other", "ans_1"));
    world.pump_up(conn, &mut runner, NOW);
    assert_eq!(runner.nacks, vec![(1, NackCode::Forbidden)]);
    assert!(world.host.answers.borrow().is_empty());
    assert!(runner.results.is_empty());

    // The same frame again on a fresh stream (same event_id): acked, not resolved again.
    runner.up_frame(answer_frame("evt_b", THREAD, "ans_2"));
    world.pump_up(conn, &mut runner, NOW);
    let mut again = runner.restarted();
    let conn = world.connect(&mut again, NOW + 1);
    again.up_frame(answer_frame("evt_b", THREAD, "ans_2"));
    world.pump_up(conn, &mut again, NOW + 1);
    assert_eq!(world.host.answers.borrow().len(), 1, "evt_b resolved once");
}

fn prompt_frame(event_id: &str, thread: &str, task_id: &str, text: &str) -> Value {
    json!({
        "t": "prompt_submit",
        "event_id": event_id,
        "thread_id": thread,
        "task_id": task_id,
        "message_id": format!("msg_{task_id}"),
        "parts": [{ "type": "text", "text": text }],
    })
}

#[test]
fn a_runner_prompt_goes_to_the_thread_host_as_the_local_user_in_order_and_is_not_appended() {
    let world = new_world();
    let mut runner = Runner::new();
    let conn = world.connect(&mut runner, NOW);
    runner.up_frame(prompt_frame("evt_a", THREAD, "tk_1", "first"));
    runner.up_frame(prompt_frame("evt_b", THREAD, "tk_2", "second"));
    world.pump_up(conn, &mut runner, NOW);
    let local = format!("local:{MACHINE}");
    assert_eq!(
        *world.host.prompts.borrow(),
        vec![
            (
                THREAD.to_owned(),
                "tk_1".to_owned(),
                "msg_tk_1".to_owned(),
                "first".to_owned(),
                local.clone()
            ),
            (
                THREAD.to_owned(),
                "tk_2".to_owned(),
                "msg_tk_2".to_owned(),
                "second".to_owned(),
                local
            ),
        ],
        "the hub's Thread DO queues the prompts; the machine appends nothing itself"
    );
    assert!(world.host.types().is_empty());
    assert!(runner.nacks.is_empty());
}

#[test]
fn a_runner_prompt_for_a_foreign_thread_or_a_refused_one_is_nacked_and_a_retransmit_is_queued_once()
{
    let world = new_world();
    let mut runner = Runner::new();
    let conn = world.connect(&mut runner, NOW);
    runner.up_frame(prompt_frame("evt_a", "th_other", "tk_1", "x"));
    world.pump_up(conn, &mut runner, NOW);
    assert_eq!(runner.nacks, vec![(1, NackCode::Forbidden)]);
    assert!(world.host.prompts.borrow().is_empty());

    // The Thread DO refuses it (for example an empty prompt): a nack, nothing queued.
    world
        .host
        .fail_next
        .borrow_mut()
        .push_back(HostError::Rejected {
            code: NackCode::InvalidEnvelope,
            message: "parts must have at least one part".to_owned(),
        });
    runner.up_frame(prompt_frame("evt_b", THREAD, "tk_2", "x"));
    world.pump_up(conn, &mut runner, NOW);
    assert_eq!(runner.nacks[1], (2, NackCode::InvalidEnvelope));
    assert!(world.host.prompts.borrow().is_empty());

    // The same frame again on a fresh stream (same event_id): acked, queued once.
    runner.up_frame(prompt_frame("evt_c", THREAD, "tk_3", "again"));
    world.pump_up(conn, &mut runner, NOW);
    let mut again = runner.restarted();
    let conn = world.connect(&mut again, NOW + 1);
    again.up_frame(prompt_frame("evt_c", THREAD, "tk_3", "again"));
    world.pump_up(conn, &mut again, NOW + 1);
    assert_eq!(world.host.prompts.borrow().len(), 1, "evt_c queued once");
}

#[test]
fn runner_events_carry_their_event_id_to_the_thread_host() {
    let world = new_world();
    let mut runner = Runner::new();
    let conn = world.connect(&mut runner, NOW);
    for text in ["a", "b"] {
        runner.event(THREAD, text);
    }
    world.pump_up(conn, &mut runner, NOW);
    let ids: Vec<Option<String>> = world
        .host
        .appended
        .borrow()
        .iter()
        .zip(world.host.appended_ids.borrow().iter())
        .filter(|((_, _, e), _)| e["event"]["type"] != "Presence")
        .map(|(_, id)| id.clone())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(
        ids.iter()
            .all(|id| id.as_deref().is_some_and(|id| id.starts_with("evt"))),
        "every runner event is appended with its frame event_id: {ids:?}"
    );
}

#[test]
fn events_are_appended_in_order_acked_and_retransmits_are_not_appended_again() {
    let world = new_world();
    let mut runner = Runner::new();
    let conn = world.connect(&mut runner, NOW);
    for text in ["a", "b", "c"] {
        runner.event(THREAD, text);
    }
    world.pump_up(conn, &mut runner, NOW);
    assert_eq!(world.host.deltas(), ["a", "b", "c"]);
    assert_eq!(world.core.borrow().state().up_last, 3);
    assert!(
        runner.up.is_empty(),
        "the cumulative ack emptied the outbox"
    );

    // The same frame again (a retransmit): re-acked, not appended.
    let again = Frame::Event {
        seq: 2,
        event_id: "evt_2".to_owned(),
        envelope: world.host.appended.borrow()[1].2.clone(),
    };
    world.frame(conn, &mut runner, &again, NOW);
    assert_eq!(world.host.deltas().len(), 3);

    // A gap is a protocol error.
    let gap = Frame::Event {
        seq: 9,
        event_id: "evt_9".to_owned(),
        envelope: json!({}),
    };
    world.frame(conn, &mut runner, &gap, NOW);
    assert_eq!(
        runner.closes.pop().map(|c| c.0),
        Some(CloseCode::ProtocolError)
    );
}

#[test]
fn a_refused_frame_is_nacked_and_counts_as_acked() {
    let world = new_world();
    let mut runner = Runner::new();
    let conn = world.connect(&mut runner, NOW);

    // 1. Not a thread this machine hosts.
    runner.event("th_other", "x");
    // 2. Valid and hosted, but the Thread DO refuses it under turn-event.md §5.
    runner.event(THREAD, "y");
    world
        .host
        .fail_next
        .borrow_mut()
        .push_back(HostError::Rejected {
            code: NackCode::InvalidSequence,
            message: "TextMessageContent before TextMessageStart".to_owned(),
        });
    // 3. Hosted, but not an envelope.
    runner.event_with_id("evt_bad", json!({"thread_id": THREAD, "v": 1}));
    // 4. Too large.
    runner.event_with_id(
        "evt_big",
        json!({"thread_id": THREAD, "pad": "x".repeat(300 * 1024)}),
    );
    // 5. A good one after the bad ones: the stream is not wedged.
    runner.event(THREAD, "z");
    world.pump_up(conn, &mut runner, NOW);

    let codes: Vec<(u64, NackCode)> = runner.nacks.clone();
    assert_eq!(
        codes,
        vec![
            (1, NackCode::Forbidden),
            (2, NackCode::InvalidSequence),
            (3, NackCode::InvalidEnvelope),
            (4, NackCode::TooLarge),
        ]
    );
    assert_eq!(world.host.deltas(), ["z"]);
    assert!(runner.up.is_empty());
}

#[test]
fn an_unreachable_thread_host_closes_without_counting_the_frame() {
    let world = new_world();
    let mut runner = Runner::new();
    let conn = world.connect(&mut runner, NOW);
    runner.event(THREAD, "a");
    world
        .host
        .fail_next
        .borrow_mut()
        .push_back(HostError::Unavailable("thread DO down".to_owned()));
    world.pump_up(conn, &mut runner, NOW);
    assert_eq!(
        runner.closes.pop().map(|c| c.0),
        Some(CloseCode::ServerRestart)
    );
    assert_eq!(world.core.borrow().state().up_last, 0);
    assert!(world.host.deltas().is_empty());

    // The runner reconnects with its token and resends: appended exactly once.
    world.drop_socket(conn, false, NOW + 1);
    let conn = world.connect(&mut runner, NOW + 2);
    assert_eq!(runner.welcome, Some((true, 0)));
    world.pump_up(conn, &mut runner, NOW + 2);
    assert_eq!(world.host.deltas(), ["a"]);
}

#[test]
fn thread_open_registers_a_thread_unless_another_machine_hosts_it() {
    let world = new_world();
    let mut runner = Runner::new();
    let conn = world.connect(&mut runner, NOW);
    let open = |event_id: &str, thread: &str| {
        json!({
            "t": "thread_open",
            "event_id": event_id,
            "thread_id": thread,
            "title": "fix flaky test",
            "agent": "claude",
            "workspace": { "path": "/p", "branch": "main" },
        })
    };
    world
        .host
        .owners
        .borrow_mut()
        .insert("th_taken".to_owned(), "mch_other".to_owned());
    runner.up_frame(open("evt_a", "th_new"));
    runner.up_frame(open("evt_b", "th_taken"));
    // Events for the new thread are accepted once it is registered.
    runner.event("th_new", "hi");
    world.pump_up(conn, &mut runner, NOW);

    assert_eq!(runner.nacks, vec![(2, NackCode::Forbidden)]);
    assert_eq!(
        world.core.borrow().hosted("th_new").expect("hosted"),
        Some(HostKind::Thread)
    );
    assert_eq!(
        world.core.borrow().hosted("th_taken").expect("hosted"),
        None
    );
    let inits = world.host.inits.borrow();
    assert_eq!(inits.len(), 1);
    assert_eq!(inits[0].0, "th_new");
    assert_eq!(inits[0].1["machine_id"], MACHINE);
    assert_eq!(
        inits[0].1["org_id"], "org_1",
        "the machine's org travels with the init"
    );
    assert_eq!(inits[0].1["title"], "fix flaky test");
    drop(inits);
    assert_eq!(world.host.deltas(), ["hi"]);
    assert!(runner.up.is_empty());
}

#[test]
fn thread_open_passes_the_task_the_runner_is_already_running_to_the_thread() {
    let world = new_world();
    let mut runner = Runner::new();
    let conn = world.connect(&mut runner, NOW);
    let open = |event_id: &str, thread: &str, extra: Value| {
        let mut frame = json!({
            "t": "thread_open",
            "event_id": event_id,
            "thread_id": thread,
            "title": "typed offline",
            "agent": "claude",
            "workspace": { "path": "/p" },
        });
        frame
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().cloned().unwrap_or_default());
        frame
    };
    runner.up_frame(open("evt_a", "th_busy", json!({"running_task": "tk_off"})));
    runner.up_frame(open("evt_b", "th_idle", json!({})));
    world.pump_up(conn, &mut runner, NOW);
    assert!(runner.nacks.is_empty(), "{:?}", runner.nacks);
    let inits = world.host.inits.borrow();
    assert_eq!(inits.len(), 2);
    assert_eq!(inits[0].1["running_task"], "tk_off");
    assert!(inits[1].1.get("running_task").is_none());
}

#[test]
fn a_failed_command_fails_its_task() {
    let world = new_world();
    let mut runner = Runner::new();
    let conn = world.connect(&mut runner, NOW);
    world.push(&mut runner, vec![prompt(1)], false, NOW);
    world.ack_down(conn, &mut runner, NOW);
    let result = Frame::CommandResult {
        seq: 1,
        event_id: "evt_cr".to_owned(),
        idempotency_key: "idk_1".to_owned(),
        ok: false,
        code: CommandErrorCode::Capacity,
        message: "32 sessions already running".to_owned(),
    };
    world.frame(conn, &mut runner, &result, NOW);
    assert_eq!(world.host.types(), ["RunStarted", "RunError"]);
    let appended = world.host.appended.borrow();
    let error = &appended.last().expect("RunError").2;
    assert_eq!(error["task_id"], "tk_1");
    assert_eq!(error["event"]["code"], "capacity");
    assert_eq!(error["event"]["reason"], "error");
    assert_eq!(error["run_id"], appended[appended.len() - 2].2["run_id"]);
}

// ---- down stream -------------------------------------------------------------------------

#[test]
fn commands_queued_offline_are_delivered_in_order_after_welcome() {
    let world = new_world();
    let mut runner = Runner::new();
    // The machine has never connected: three commands wait in the outbox.
    world.push(&mut runner, vec![prompt(1), prompt(2)], false, NOW);
    world.push(
        &mut runner,
        vec![prompt(1), prompt(2), prompt(3)],
        false,
        NOW + 1,
    );
    assert!(
        runner.seen_down.is_empty(),
        "nothing goes out while offline"
    );
    assert_eq!(world.core.borrow().store().outbox_len(), 3);

    let conn = world.connect(&mut runner, NOW + 2);
    assert_eq!(runner.seen_down, [1, 2, 3]);
    assert_eq!(task_ids(&runner), ["tk_1", "tk_2", "tk_3"]);

    world.ack_down(conn, &mut runner, NOW + 3);
    assert_eq!(world.core.borrow().store().outbox_len(), 0);
    assert_eq!(
        *world.host.acked.borrow(),
        vec![(
            THREAD.to_owned(),
            vec!["1".to_owned(), "2".to_owned(), "3".to_owned()]
        )]
    );
}

#[test]
fn resume_with_the_token_continues_both_streams_without_duplicates() {
    let world = new_world();
    let mut runner = Runner::new();
    let conn = world.connect(&mut runner, NOW);

    // Up: two events delivered and acked, a third processed whose ack the network loses.
    runner.event(THREAD, "e1");
    runner.event(THREAD, "e2");
    world.pump_up(conn, &mut runner, NOW);
    runner.event(THREAD, "e3");
    let (seq, mut body) = runner.up.next_to_send().expect("e3");
    body["seq"] = json!(seq);
    world.deliver(conn, &mut runner, &body.to_string(), NOW);
    let _lost_ack = driver::flush_ack(&world.core).expect("ack e3");
    // ... a fourth event never reaches the hub.
    runner.event(THREAD, "e4");
    let _ = runner.up.next_to_send();

    // Down: three commands sent; the runner durably processes two before the socket drops.
    world.push(
        &mut runner,
        vec![prompt(1), prompt(2), prompt(3)],
        false,
        NOW,
    );
    assert_eq!(runner.seen_down, [1, 2, 3]);
    let first_token = runner.token.clone();
    let mut lagging = Runner::new();
    lagging.token = first_token.clone();
    lagging.up = runner.up;
    lagging.next_event = runner.next_event;
    for key in ["idk_1", "idk_2"] {
        lagging.keys.observe(key, NOW);
    }
    lagging.got = runner.got.iter().take(2).cloned().collect();
    lagging.down.on_frame(1).expect("down 1");
    lagging.down.on_frame(2).expect("down 2");
    let mut runner = lagging;
    world.drop_socket(conn, false, NOW + 10);

    let conn = world.connect(&mut runner, NOW + 20);
    assert_eq!(
        runner.welcome,
        Some((true, 3)),
        "resumed, hub processed up to e3"
    );
    assert_eq!(
        runner.seen_down,
        [3],
        "only the unprocessed command is resent"
    );
    assert_eq!(task_ids(&runner), ["tk_1", "tk_2", "tk_3"]);
    world.pump_up(conn, &mut runner, NOW + 20);
    assert_eq!(world.host.deltas(), ["e1", "e2", "e3", "e4"]);
    // The tokens rotate: each welcome carries a new one.
    assert_ne!(runner.token, first_token);
}

#[test]
fn a_fresh_stream_dedupes_events_by_event_id_and_resends_commands() {
    let world = new_world();
    let mut runner = Runner::new();
    let conn = world.connect(&mut runner, NOW);
    runner.event(THREAD, "e1");
    runner.event(THREAD, "e2");
    world.pump_up(conn, &mut runner, NOW);
    world.push(&mut runner, vec![prompt(1), prompt(2)], false, NOW);
    world.drop_socket(conn, false, NOW + 1);
    assert_eq!(world.host.deltas(), ["e1", "e2"]);

    // The runner restarts without its state: no token, its outbox renumbered from 1. It
    // resends e2 (same event_id; the hub already has it) and adds e3.
    let mut restarted = runner.restarted();
    restarted.next_event = 1;
    restarted.event(THREAD, "e2");
    restarted.event(THREAD, "e3");
    let conn = world.connect(&mut restarted, NOW + 5);
    assert_eq!(restarted.welcome, Some((false, 0)));
    world.pump_up(conn, &mut restarted, NOW + 5);
    assert_eq!(
        world.host.deltas(),
        ["e1", "e2", "e3"],
        "e2 was not appended twice"
    );
    assert!(restarted.up.is_empty());

    // The hub renumbers its unacked commands from 1; the runner's idempotency keys drop the
    // ones it already applied.
    assert_eq!(restarted.seen_down, [1, 2]);
    assert!(
        restarted.got.is_empty(),
        "idk_1 and idk_2 were applied before"
    );
    assert_eq!(world.core.borrow().state().stream_epoch, 2);
}

#[test]
fn a_resume_token_is_single_use_and_expires() {
    let world = new_world();
    let mut runner = Runner::new();
    let conn = world.connect(&mut runner, NOW);
    let first_token = runner.token.clone();
    world.drop_socket(conn, false, NOW + 1);

    // Reusing the token twice: the second hello gets a fresh stream.
    let mut a = runner.restarted();
    a.token = first_token.clone();
    let conn = world.connect(&mut a, NOW + 2);
    assert_eq!(a.welcome, Some((true, 0)));
    world.drop_socket(conn, false, NOW + 3);
    let mut b = runner.restarted();
    b.token = first_token;
    world.connect(&mut b, NOW + 4);
    assert_eq!(b.welcome, Some((false, 0)), "a used token is refused");

    // A token older than 7 days.
    let world = world_with_runner(&mut runner);
    let conn = world.connect(&mut runner, NOW);
    world.drop_socket(conn, false, NOW + 1);
    world.connect(&mut runner, NOW + 7 * DAY + 1);
    assert_eq!(runner.welcome, Some((false, 0)));

    // An unknown token.
    let world = new_world();
    let mut stranger = Runner::new();
    stranger.token = Some("rt_made_up".to_owned());
    world.connect(&mut stranger, NOW);
    assert_eq!(stranger.welcome, Some((false, 0)));
}

#[test]
fn a_resume_cursor_that_disagrees_falls_back_to_a_fresh_stream() {
    let world = new_world();
    let mut runner = Runner::new();
    let conn = world.connect(&mut runner, NOW);
    world.push(&mut runner, vec![prompt(1)], false, NOW);
    world.drop_socket(conn, false, NOW + 1);
    // The runner claims it processed command 5; the hub only ever sent 1.
    let mut liar = runner.restarted();
    liar.token = runner.token.clone();
    for seq in 1..=5 {
        let _ = liar.down.on_frame(seq);
    }
    world.connect(&mut liar, NOW + 2);
    assert_eq!(liar.welcome, Some((false, 0)));
}

#[test]
fn a_snapshot_push_withdraws_a_command_that_never_went_out() {
    let world = new_world();
    let mut runner = Runner::new();
    world.push(
        &mut runner,
        vec![prompt(1), prompt(2), prompt(3)],
        true,
        NOW,
    );
    // The user cancelled task 2 before the runner ever connected: the Thread DO retracted it.
    world.push(&mut runner, vec![prompt(1), prompt(3)], true, NOW + 1);
    assert_eq!(world.core.borrow().store().outbox_len(), 2);
    // A re-push of the withdrawn command (a late older snapshot) is not queued again.
    world.push(
        &mut runner,
        vec![prompt(1), prompt(2), prompt(3)],
        true,
        NOW + 2,
    );
    assert_eq!(world.core.borrow().store().outbox_len(), 2);

    // The remaining frames are numbered 1 and 2 (renumbered after the removal).
    let conn = world.connect(&mut runner, NOW + 3);
    assert_eq!(runner.seen_down, [1, 2]);
    assert_eq!(task_ids(&runner), ["tk_1", "tk_3"]);
    world.ack_down(conn, &mut runner, NOW + 4);

    // Once on the wire a command can no longer be withdrawn.
    world.push(&mut runner, vec![prompt(4)], false, NOW + 5);
    assert_eq!(runner.seen_down, [1, 2, 3]);
    world.push(&mut runner, vec![], true, NOW + 6);
    assert_eq!(runner.seen_down, [1, 2, 3]);
    assert_eq!(world.core.borrow().store().outbox_len(), 1);
}

#[test]
fn queued_commands_expire_after_seven_days_and_fail_their_task() {
    let world = new_world();
    let mut runner = Runner::new();
    world.push(&mut runner, vec![prompt(1)], false, NOW);
    world.push(&mut runner, vec![prompt(2)], false, NOW + 6 * DAY);
    world.connect(&mut runner, NOW + 7 * DAY + 1_000);
    assert_eq!(
        runner.seen_down,
        [1],
        "only the command younger than 7 days is sent"
    );
    assert_eq!(task_ids(&runner), ["tk_2"]);
    assert_eq!(world.host.types(), ["RunStarted", "RunError"]);
    let appended = world.host.appended.borrow();
    let error = appended
        .iter()
        .find(|a| a.2["event"]["type"] == "RunError")
        .expect("RunError");
    assert_eq!(error.2["task_id"], "tk_1");
    assert_eq!(error.2["event"]["code"], "expired");
    drop(appended);
    // The expired command is acked at the Thread DO so it is not listed again.
    assert!(
        world
            .host
            .acked
            .borrow()
            .iter()
            .any(|(_, ids)| ids == &["1".to_owned()])
    );
}

#[test]
fn the_thread_dos_unacked_commands_are_pulled_after_welcome() {
    let world = new_world();
    let mut runner = Runner::new();
    // A push got lost while the object was away: the command is only in the Thread DO.
    world
        .host
        .listed
        .borrow_mut()
        .insert(THREAD.to_owned(), vec![prompt(7)]);
    world.connect(&mut runner, NOW);
    assert_eq!(task_ids(&runner), ["tk_7"]);
}

#[test]
fn a_push_for_another_thread_or_a_revoked_machine_is_refused() {
    let world = new_world();
    let mut bad = prompt(1);
    if let Command::Prompt { thread_id, .. } = &mut bad.command {
        *thread_id = "th_elsewhere".to_owned();
    }
    let result = world
        .core
        .borrow_mut()
        .push(
            PushBody {
                thread_id: THREAD.to_owned(),
                host: HostKind::Thread,
                snapshot: false,
                commands: vec![bad],
            },
            NOW,
        )
        .expect("store");
    assert!(result.is_err());
    block_on(driver::revoke(&world.core, &world.host, NOW)).expect("revoke");
    let result = world
        .core
        .borrow_mut()
        .push(
            PushBody {
                thread_id: THREAD.to_owned(),
                host: HostKind::Thread,
                snapshot: false,
                commands: vec![prompt(1)],
            },
            NOW,
        )
        .expect("store");
    assert!(result.is_err());
}

// ---- the admin API and the schema ----------------------------------------------------------

struct Fake {
    conn: RefCell<rusqlite::Connection>,
}

impl Fake {
    fn new() -> Self {
        let conn = rusqlite::Connection::open_in_memory().expect("open sqlite");
        conn.pragma_update(None, "foreign_keys", true).expect("fk");
        for (name, sql) in MIGRATIONS {
            conn.execute_batch(sql)
                .unwrap_or_else(|error| panic!("migration {name}: {error}"));
        }
        Self {
            conn: RefCell::new(conn),
        }
    }
}

fn backend(error: rusqlite::Error) -> DbError {
    DbError::Backend(error.to_string())
}

fn bind(params: &[Param]) -> Vec<SqlValue> {
    params
        .iter()
        .map(|param| match param {
            Param::Null => SqlValue::Null,
            Param::Int(value) => SqlValue::Integer(*value),
            Param::Real(value) => SqlValue::Real(*value),
            Param::Text(text) => SqlValue::Text(text.clone()),
        })
        .collect()
}

fn run_query(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[Param],
) -> Result<Vec<Row>, DbError> {
    let mut statement = conn.prepare(sql).map_err(backend)?;
    let names: Vec<String> = statement.column_names().map(|n| n.to_string()).collect();
    let mut rows = statement
        .query(params_from_iter(bind(params)))
        .map_err(backend)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(backend)? {
        let mut object = Row::new();
        for (index, name) in names.iter().enumerate() {
            let value: SqlValue = row.get(index).map_err(backend)?;
            object.insert(
                name.clone(),
                match value {
                    SqlValue::Null | SqlValue::Blob(_) => Value::Null,
                    SqlValue::Integer(v) => json!(v),
                    SqlValue::Real(v) => json!(v),
                    SqlValue::Text(t) => Value::String(t),
                },
            );
        }
        out.push(object);
    }
    Ok(out)
}

impl Db for Fake {
    fn execute(&self, sql: &str, params: &[Param]) -> impl Future<Output = Result<u64, DbError>> {
        let conn = self.conn.borrow();
        ready(
            conn.prepare(sql)
                .and_then(|mut s| s.execute(params_from_iter(bind(params))))
                .map(|n| n as u64)
                .map_err(backend),
        )
    }

    fn query(
        &self,
        sql: &str,
        params: &[Param],
    ) -> impl Future<Output = Result<Vec<Row>, DbError>> {
        ready(run_query(&self.conn.borrow(), sql, params))
    }
}

#[derive(Default)]
struct FakeControl {
    provisioned: RefCell<Vec<(String, String, String)>>,
    revoked: RefCell<Vec<String>>,
    disconnected: RefCell<Vec<String>>,
    fail_provision: bool,
}

impl MachineControl for FakeControl {
    fn provision(
        &self,
        machine_id: &str,
        org_id: &str,
        credential_hash: &str,
    ) -> impl Future<Output = Result<(), ApiError>> {
        self.provisioned.borrow_mut().push((
            machine_id.to_owned(),
            org_id.to_owned(),
            credential_hash.to_owned(),
        ));
        ready(if self.fail_provision {
            Err(ApiError::internal("machine DO unreachable"))
        } else {
            Ok(())
        })
    }

    fn revoke(&self, machine_id: &str) -> impl Future<Output = Result<(), ApiError>> {
        self.revoked.borrow_mut().push(machine_id.to_owned());
        ready(Ok(()))
    }

    fn disconnect(&self, machine_id: &str) -> impl Future<Output = Result<(), ApiError>> {
        self.disconnected.borrow_mut().push(machine_id.to_owned());
        ready(Ok(()))
    }
}

fn dev_admin(db: &Fake) -> Principal {
    block_on(authenticate(db, Some("Bearer dev"), true)).expect("dev principal")
}

#[test]
fn the_admin_api_issues_a_credential_once_and_stores_only_its_hash() {
    let db = Fake::new();
    let control = FakeControl::default();
    let admin = dev_admin(&db);

    let created = block_on(admin::create_machine(
        &db,
        &control,
        &admin,
        &json!({"name": "laptop", "kind": "local"}),
        "req_1",
    ))
    .expect("create");
    let credential = created["credential"]
        .as_str()
        .expect("credential")
        .to_owned();
    assert!(credential.starts_with("smm_"));
    assert_eq!(credential.len(), 68);
    let id = created["machine"]["id"].as_str().expect("id").to_owned();
    assert_eq!(created["machine"]["presence"], "offline");
    assert!(created["machine"].get("credential_hash").is_none());

    // D1 and the Machine DO get the SHA-256, never the credential.
    let rows = block_on(db.query("SELECT credential_hash FROM machines", &[])).expect("rows");
    assert_eq!(rows[0]["credential_hash"], credential::hash(&credential));
    assert_ne!(rows[0]["credential_hash"], json!(credential));
    assert_eq!(
        *control.provisioned.borrow(),
        vec![(
            id.clone(),
            admin.org_id.clone(),
            credential::hash(&credential)
        )]
    );
    let audit = block_on(crate::audit::list(&db, &admin.org_id, 0, 10)).expect("audit");
    assert!(
        audit
            .iter()
            .any(|e| e.action == "machine.create" && e.target_id == id)
    );
    assert!(
        !audit
            .iter()
            .any(|e| e.detail.to_string().contains(&credential)),
        "the credential never reaches the audit log"
    );

    // The credential cannot be read again.
    let listed = block_on(admin::list_machines(&db, &admin)).expect("list");
    assert_eq!(listed["machines"][0]["machine_id"], id);
    assert!(!listed.to_string().contains(&credential));
}

#[test]
fn creating_and_revoking_need_the_admin_role_and_validate_input() {
    let db = Fake::new();
    let control = FakeControl::default();
    let mut member = dev_admin(&db);
    member.role = Role::Member;
    let forbidden = block_on(admin::create_machine(
        &db,
        &control,
        &member,
        &json!({"name": "x"}),
        "r",
    ))
    .expect_err("member");
    assert_eq!(forbidden.status, 403);
    let forbidden =
        block_on(admin::revoke_machine(&db, &control, &member, "mch_1", "r")).expect_err("member");
    assert_eq!(forbidden.status, 403);

    let admin = dev_admin(&db);
    for body in [
        json!({}),
        json!({"name": ""}),
        json!({"name": "x", "kind": "vm"}),
    ] {
        let error =
            block_on(admin::create_machine(&db, &control, &admin, &body, "r")).expect_err("bad");
        assert_eq!(error.status, 400, "{body}");
    }
    assert_eq!(
        block_on(admin::count(&db, &admin.org_id)).expect("count"),
        0
    );

    // A Machine DO that cannot be provisioned leaves no row behind.
    let broken = FakeControl {
        fail_provision: true,
        ..FakeControl::default()
    };
    let error = block_on(admin::create_machine(
        &db,
        &broken,
        &admin,
        &json!({"name": "x"}),
        "r",
    ))
    .expect_err("provision");
    assert_eq!(error.status, 500);
    assert_eq!(
        block_on(admin::count(&db, &admin.org_id)).expect("count"),
        0
    );
}

#[test]
fn revoking_marks_the_row_tells_the_object_and_is_idempotent() {
    let db = Fake::new();
    let control = FakeControl::default();
    let admin = dev_admin(&db);
    let created = block_on(admin::create_machine(
        &db,
        &control,
        &admin,
        &json!({"name": "box", "kind": "sandbox"}),
        "r",
    ))
    .expect("create");
    let id = created["machine"]["id"].as_str().expect("id").to_owned();

    let first = block_on(admin::revoke_machine(&db, &control, &admin, &id, "r1")).expect("revoke");
    assert!(first["revoked_at"].is_string());
    let second = block_on(admin::revoke_machine(&db, &control, &admin, &id, "r2")).expect("revoke");
    assert_eq!(
        first["revoked_at"], second["revoked_at"],
        "the first revocation time stays"
    );
    assert_eq!(
        control.revoked.borrow().len(),
        2,
        "the object is told every time"
    );

    let unknown = block_on(admin::revoke_machine(
        &db, &control, &admin, "mch_nope", "r",
    ))
    .expect_err("404");
    assert_eq!(unknown.status, 404);
    // Another org's admin cannot see or revoke it.
    let mut stranger = admin.clone();
    stranger.org_id = "org_other".to_owned();
    let hidden =
        block_on(admin::revoke_machine(&db, &control, &stranger, &id, "r")).expect_err("404");
    assert_eq!(hidden.status, 404);
    let audit = block_on(crate::audit::list(&db, &admin.org_id, 0, 10)).expect("audit");
    assert_eq!(
        audit
            .iter()
            .filter(|e| e.action == "machine.revoke")
            .count(),
        2
    );
}

#[test]
fn presence_is_mirrored_into_d1() {
    let db = Fake::new();
    let control = FakeControl::default();
    let admin = dev_admin(&db);
    let created = block_on(admin::create_machine(
        &db,
        &control,
        &admin,
        &json!({"name": "m"}),
        "r",
    ))
    .expect("create");
    let id = created["machine"]["id"].as_str().expect("id").to_owned();
    block_on(admin::record_presence(
        &db,
        &id,
        "online",
        "2026-10-28T00:00:00.000Z",
    ))
    .expect("presence");
    let listed = block_on(admin::list_machines(&db, &admin)).expect("list");
    assert_eq!(listed["machines"][0]["presence"]["state"], "online");
    assert_eq!(
        listed["machines"][0]["presence"]["since"],
        "2026-10-28T00:00:00.000Z"
    );
}

#[test]
fn the_list_relays_the_machines_local_permission_mode() {
    let db = Fake::new();
    let control = FakeControl::default();
    let admin = dev_admin(&db);
    let created = block_on(admin::create_machine(
        &db,
        &control,
        &admin,
        &json!({"name": "m"}),
        "r",
    ))
    .expect("create");
    let id = created["machine"]["id"].as_str().expect("id").to_owned();
    block_on(admin::record_capabilities(
        &db,
        &id,
        &json!({
            "host_mode": "local", "harnesses": ["claude"], "max_sessions": 2,
            "spawn": ["same_dir"], "features": ["terminals", "permission_mode:plan"],
        }),
    ))
    .expect("capabilities");
    let listed = block_on(admin::list_machines(&db, &admin)).expect("list");
    assert_eq!(
        listed["machines"][0]["capabilities"]["permission_mode"],
        "plan"
    );
}

#[test]
fn the_list_is_the_client_api_shape_scoped_to_the_org_and_leaves_out_revoked() {
    let db = Fake::new();
    let control = FakeControl::default();
    let admin = dev_admin(&db);
    block_on(db.execute(
        "INSERT INTO orgs (id, slug, name) VALUES ('org_other', 'other', 'Other')",
        &[],
    ))
    .expect("second org");
    let mut other = admin.clone();
    other.org_id = "org_other".to_owned();
    let mut viewer = admin.clone();
    viewer.role = Role::Viewer;

    let create = |principal: &Principal, name: &str| {
        let created = block_on(admin::create_machine(
            &db,
            &control,
            principal,
            &json!({"name": name, "kind": "sandbox"}),
            "r",
        ))
        .expect("create");
        created["machine"]["id"].as_str().expect("id").to_owned()
    };
    let mine = create(&admin, "mine");
    let theirs = create(&other, "theirs");
    let gone = create(&admin, "gone");
    block_on(admin::revoke_machine(&db, &control, &admin, &gone, "r")).expect("revoke");

    // A viewer of the org gets its live machines only, in §5.1 shape, capabilities not yet known.
    let listed = block_on(admin::list_machines(&db, &viewer)).expect("list");
    let rows = listed["machines"].as_array().expect("rows");
    assert_eq!(rows.len(), 1, "{listed}");
    let row = &rows[0];
    assert_eq!(row["machine_id"], mine.as_str());
    assert_eq!(row["name"], "mine");
    assert_eq!(row["kind"], "sandbox");
    assert_eq!(row["presence"]["state"], "offline");
    assert!(row["presence"]["since"].is_string(), "{row}");
    assert_eq!(row["capabilities"], Value::Null);
    let mut keys: Vec<&str> = row
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["capabilities", "kind", "machine_id", "name", "presence"]
    );
    assert!(!listed.to_string().contains(&theirs));
    assert!(!listed.to_string().contains(&gone));

    // Once the runner has said hello, its capabilities come as the three §5.1 fields.
    block_on(admin::record_capabilities(
        &db,
        &mine,
        &json!({
            "host_mode": "sandbox",
            "harnesses": ["claude", "opencode"],
            "max_sessions": 32,
            "spawn": ["same_dir", "worktree"],
            "features": ["x"],
        }),
    ))
    .expect("capabilities");
    block_on(admin::record_presence(
        &db,
        &mine,
        "online",
        "2026-10-28T00:00:00.000Z",
    ))
    .expect("presence");
    let listed = block_on(admin::list_machines(&db, &viewer)).expect("list");
    let row = &listed["machines"][0];
    assert_eq!(row["presence"]["state"], "online");
    assert_eq!(row["presence"]["since"], "2026-10-28T00:00:00.000Z");
    assert_eq!(
        row["capabilities"],
        json!({
            "harnesses": ["claude", "opencode"],
            "spawn": ["same_dir", "worktree"],
            "max_sessions": 32,
        })
    );

    // The other org sees only its own machine.
    let listed = block_on(admin::list_machines(&db, &other)).expect("list");
    assert_eq!(listed["machines"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed["machines"][0]["machine_id"], theirs.as_str());
}

#[test]
fn machines_table_checks_and_agents_kind_is_local_managed_or_byo() {
    let db = Fake::new();
    let admin = dev_admin(&db);
    let conn = db.conn.borrow();
    let org = admin.org_id.as_str();
    let machine = |kind: &str, hash: &str| {
        conn.execute(
            "INSERT INTO machines (id, org_id, name, kind, credential_hash) VALUES (?, ?, 'm', ?, ?)",
            rusqlite::params![format!("mch_{kind}_{}", hash.len()), org, kind, hash],
        )
    };
    assert!(machine("local", &"a".repeat(64)).is_ok());
    assert!(machine("sandbox", &"a".repeat(64)).is_ok());
    assert!(
        machine("vm", &"a".repeat(64)).is_err(),
        "kind is local or sandbox"
    );
    assert!(
        machine("local", "short").is_err(),
        "the hash is a SHA-256 hex"
    );
    let presence = conn.execute(
        "UPDATE machines SET presence = 'sleeping' WHERE kind = 'local'",
        [],
    );
    assert!(presence.is_err(), "presence is online, asleep or offline");

    // agents.kind exists since 0002 with exactly these three values.
    let agent = |kind: &str, url: Option<&str>| {
        conn.execute(
            "INSERT INTO agents (id, org_id, kind, name, url) VALUES (?, ?, ?, ?, ?)",
            rusqlite::params![format!("agt_{kind}"), org, kind, format!("a_{kind}"), url],
        )
    };
    assert!(agent("local", None).is_ok());
    assert!(agent("managed", Some("https://a.example")).is_ok());
    assert!(agent("byo", Some("https://b.example")).is_ok());
    assert!(agent("other", Some("https://c.example")).is_err());
}

#[test]
fn hello_info_is_what_the_core_checks() {
    // The core can be driven without the driver: hello on a never-connected socket.
    let credential = credential::new_credential().expect("credential");
    let mut core = MachineCore::open(MemoryStore::new()).expect("open");
    core.provision(MACHINE, "org_1", &credential::hash(&credential), false)
        .expect("provision");
    assert_eq!(
        core.provision(MACHINE, "org_1", &credential::hash(&credential), false)
            .expect("again"),
        super::core::Provisioned::Unchanged
    );
    assert_eq!(
        core.provision("mch_2", "org_1", "x", false).expect("other"),
        super::core::Provisioned::Conflict
    );
    core.begin_connection(NOW).expect("conn");
    let fx = core
        .on_hello(
            HelloInfo {
                proto: 1,
                machine_id: MACHINE.to_owned(),
                runner_version: "1".to_owned(),
                resume_token: None,
                last_down_seq: 0,
                capabilities: json!({"host_mode": "local"}),
            },
            NOW,
        )
        .expect("hello");
    assert!(matches!(
        fx.out.first(),
        Some(Out::Send(frame)) if matches!(**frame, Frame::Welcome { resumed: false, last_up_seq: 0, heartbeat_ms: 20_000, max_inflight: 256, .. })
    ));
    assert_eq!(fx.presence.map(|p| p.since), Some(format_timestamp(NOW)));
}
