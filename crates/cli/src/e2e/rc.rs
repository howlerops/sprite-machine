//! `cli e2e rc`: start on the computer, continue from the phone, survive a dropped tunnel
//! (M2B-019, PLAN-v2-ade.md §5 P2 and §4.10 "Connection, Sleep and disconnects").
//!
//! The stack of `e2e tracer` ([`super::tracer`]): the hub bundle built with worker-build, the D1
//! migrations, and `wrangler dev --local` on a free port, here with an `ATTACHMENT_SIGNING_KEY`.
//!
//! **The computer** is the interactive `sm --rc` session ([`runner::tui`]), wired as
//! `runner::tui::host` wires it, minus the real terminal: the production
//! [`runner::tui::session::Session`] over the real bridge and an in-process acp-mock agent, the
//! runner's real tunnel client to the hub, and the TUI's own state machine ([`runner::tui::App`])
//! fed with keys. Its tunnel goes through a TCP proxy that this check can cut and restore, which
//! is how a dropped tunnel is made (the WebSocket dies; the hub and the phone stay up).
//!
//! **The phone** is a headless client-core: it lists the hub's threads to find the session,
//! follows it with a [`client_core::Subscription`], and sends its prompts (with an image) and its
//! approval answers through a [`client_core::Outbox`] on the real `/v1` routes.
//!
//! The run:
//! 1. The terminal types a message; the phone finds the session in the hub's list and sees it.
//! 2. The phone sends a message and an image (`POST /v1/attachments` and a signed PUT first).
//!    The agent asks for approval; the terminal types a message while the phone answers.
//! 3. The phone sends another message; the agent asks for approval. The tunnel is cut: the
//!    machine leaves `online`. The phone answers and the terminal types while it is down, and
//!    nothing moves on the phone. The tunnel is restored; the turn finishes and the queued
//!    terminal message runs.
//!
//! It passes when the transcript the phone follows live and a fresh replay of it from the hub are
//! the same, with `seq` 1..N without a gap, every message and every agent reply exactly once, both
//! interrupts resolved exactly once, the agent got each prompt exactly once (the second with the
//! image bytes), and the terminal's own log holds the same messages and replies.
//!
//! `cli e2e rc-runner` is not a check: it is the computer alone, against a hub that is already
//! running (`web/tests/rc.spec.js` starts it), driven by lines on stdin (see [`runner_main`]).

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::tracer::{self, TempDir, Wrangler};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use client_core::sync::Incoming;
use client_core::{
    Applied, Cache, Clock, HttpTransport, Intent, IntentState, Outbox, RetryPolicy, Subscription,
    SubscriptionError, SystemClock, SystemRandom, ThreadSync,
};
use protocol::tunnel::{
    AnswerStatus, Capabilities, Command, CommandErrorCode, ContentPart, Outcome, Workspace,
};
use protocol::turn_event::{Envelope, Event, RunOutcome};
use runner::settings::Settings;
use runner::tui::driver::execute;
use runner::tui::session::{
    Handle, HubAnswerFuture, HubVerdict, Identity, Processed, Registrar, Registration, Session,
    Upstream, UpstreamFuture,
};
use runner::tui::{App, Backend, Feed, Key};
use runner::tunnel::{
    AnswerReply, StaticCredential, Timing, TunnelCommands, TunnelConfig, TunnelSink, tunnel,
};
use serde_json::{Value, json};
use sidecar::bridge::files::FilePolicy;
use sidecar::bridge::{
    Bridge, CommandFailure, CommandSource, Config, EventSink, IdGenerator, UlidIds,
};
use sidecar::store::Store;
use tokio::io::AsyncBufReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

const DEV_TOKEN: &str = "dev";
/// The harness name the session advertises and the thread's `agent`.
const AGENT_ID: &str = "opencode";
const SESSION_TITLE: &str = "rc-e2e-session";
const WAIT: Duration = Duration::from_secs(60);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// The attachment cap of the bridge's loopback policy (bytes).
const ATTACH_CAP: u64 = 64 * 1024;
/// How long an answer typed here waits for the hub's verdict (as `tui::host`).
const ANSWER_WAIT: Duration = Duration::from_secs(10);
const MAX_RECONNECTS: u32 = 5;

/// What the people type, and what the agent says back, in turn order. `web/tests/rc.spec.js`
/// repeats these strings.
const USER: [&str; 5] = [
    "start on the computer",
    "look at this photo from the phone",
    "typed on the terminal while the phone approves",
    "phone message before the tunnel drops",
    "typed on the terminal while the tunnel is down",
];
const AGENT: [&str; 6] = [
    "Computer reply: terminal prompt received.",
    "Phone reply: image received.",
    "Phone reply: approval applied.",
    "Computer reply: typed during approval.",
    "Cut reply: before the drop.",
    "Cut reply: after the drop.",
];
const AGENT_LAST: &str = "Computer reply: typed while offline.";

/// The PNG magic and an IHDR header: the bridge checks the magic bytes of an image.
const PNG: &[u8] = &[
    0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0, 0, 0, 0x0d, b'I', b'H', b'D', b'R', 0, 0,
    0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89,
];

static KEYS: AtomicU64 = AtomicU64::new(0);

/// The agent: five turns, in the order the prompts reach it.
fn script() -> Value {
    let plain = |text: &str| json!({ "steps": [{ "text": text }, { "stop": "end_turn" }] });
    let approval = |first: &str, call: &str, title: &str, last: &str| {
        json!({ "steps": [
            { "text": first },
            { "request_permission": {
                "toolCall": { "toolCallId": call, "title": title, "kind": "execute" },
                "options": [
                    { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                    { "optionId": "reject", "name": "Deny", "kind": "reject_once" }
                ] },
              "expect": "allow" },
            { "text": last },
            { "stop": "end_turn" }
        ] })
    };
    json!({ "turns": [
        plain(AGENT[0]),
        approval(AGENT[1], "call_phone", "Run the tests", AGENT[2]),
        plain(AGENT[3]),
        approval(AGENT[4], "call_cut", "Deploy the change", AGENT[5]),
        plain(AGENT_LAST),
    ] })
}

// ---- entry points -----------------------------------------------------------------------------

/// Entry point for `cli e2e rc`.
pub async fn run() -> Result<()> {
    let started = Instant::now();
    let work = TempDir::create()?;
    let result = tokio::select! {
        result = rc(&work) => result,
        () = tracer::interrupted() => Err(anyhow!("interrupted; stopping wrangler")),
    };
    match &result {
        Ok(()) => println!("PASS e2e rc in {:.1}s", started.elapsed().as_secs_f64()),
        Err(_) => eprintln!("FAIL e2e rc after {:.1}s", started.elapsed().as_secs_f64()),
    }
    result
}

async fn rc(work: &TempDir) -> Result<()> {
    let hub_dir = tracer::hub_dir()?;
    tracer::build_hub(&hub_dir, work).await?;

    println!("== d1 migrations (local)");
    tracer::migrate(&hub_dir, work)?;

    let port = tracer::free_port()?;
    let base = format!("http://127.0.0.1:{port}");
    println!("== wrangler dev --local on port {port} (ATTACHMENT_SIGNING_KEY for this run)");
    let wrangler = Wrangler::start(
        &hub_dir,
        port,
        work,
        &[format!("ATTACHMENT_SIGNING_KEY:{}", signing_key()?)],
    )?;
    wrangler.wait_ready(&base).await?;
    let checked = tokio::time::timeout(Duration::from_secs(300), exercise(&base))
        .await
        .map_err(|_| anyhow!("the rc check did not finish in 300 s"))
        .and_then(|result| result);
    let stopped = wrangler.stop();
    checked?;
    stopped?;
    println!("wrangler stopped");
    Ok(())
}

/// `cli e2e rc-runner`: the computer alone, against a running hub. Not a check.
///
/// Environment: `SM_RC_BASE` (the hub, `http://127.0.0.1:<port>`), `SM_RC_MACHINE` and
/// `SM_RC_CREDENTIAL` (a paired machine), optional `SM_RC_TITLE`. It prints `ready <json>` once the
/// session is shared (`thread_id`, `title`), then answers one line of stdin with one line:
///
/// - `type <text>`: the terminal types `<text>` and presses Enter.
/// - `cut` / `restore`: the tunnel is dropped / allowed back.
/// - `prompts`: the prompts the agent received, as JSON.
/// - `seen`: the events the terminal's own log holds, as JSON.
/// - `quit` (or the end of stdin): the session ends.
///
/// Each answer is `ok <json>`, or `error <text>`.
pub async fn runner_main() -> Result<()> {
    let var = |name: &str| std::env::var(name).with_context(|| format!("{name} is not set"));
    let config = RunnerConfig {
        base: var("SM_RC_BASE")?,
        machine_id: var("SM_RC_MACHINE")?,
        credential: var("SM_RC_CREDENTIAL")?,
        title: std::env::var("SM_RC_TITLE").unwrap_or_else(|_| SESSION_TITLE.to_owned()),
    };
    let runner = Runner::start(&config).await?;
    runner.wait_shared().await?;
    println!(
        "ready {}",
        json!({ "thread_id": runner.thread_id, "title": config.title })
    );

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let (command, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        let answer: Result<Value> = match command {
            "type" => runner.type_line(rest).await.map(|()| json!(true)),
            "cut" => {
                runner.proxy.cut();
                Ok(json!(true))
            }
            "restore" => {
                runner.proxy.restore();
                Ok(json!(true))
            }
            "prompts" => Ok(Value::Array(prompts_of(&runner.recorder))),
            "seen" => serde_json::to_value(runner.snapshot().events).map_err(Into::into),
            "quit" => break,
            other => Err(anyhow!("unknown command {other:?}")),
        };
        match answer {
            Ok(value) => println!("ok {value}"),
            Err(error) => println!("error {error:#}"),
        }
    }
    runner.stop().await;
    Ok(())
}

// ---- the check --------------------------------------------------------------------------------

async fn exercise(base: &str) -> Result<()> {
    println!("== machine and credential (dev routes)");
    let machine_id = tracer::create_machine(base).await?;
    let credential = mint_credential(base, &machine_id).await?;

    println!(
        "== the computer: sm --rc session (TUI session, bridge, acp-mock, tunnel via a proxy)"
    );
    let runner = Runner::start(&RunnerConfig {
        base: base.to_owned(),
        machine_id: machine_id.clone(),
        credential,
        title: SESSION_TITLE.to_owned(),
    })
    .await?;
    let checked = steps(base, &machine_id, &runner).await;
    let tui = runner.snapshot().events;
    runner.stop().await;
    let phone = checked?;
    verify_terminal_log(&phone, &tui)?;
    println!("the terminal's own log holds the same messages and replies, once each");
    Ok(())
}

async fn steps(base: &str, machine_id: &str, runner: &Runner) -> Result<Vec<Envelope>> {
    let api = format!("{base}/v1");
    runner.wait_shared().await?;

    println!("== 1. the terminal types; the phone finds the session and sees it");
    runner.type_line(USER[0]).await?;
    let thread = find_session(&api, SESSION_TITLE).await?;
    ensure!(
        thread == runner.thread_id,
        "the hub lists thread {thread}, the terminal's session is {}",
        runner.thread_id
    );
    println!("the phone found {thread} in GET /v1/threads");
    let mut phone = Phone::connect(&api, &thread).await?;
    phone
        .until("the first turn", |seen| finished(seen) == 1)
        .await?;
    ensure!(
        phone
            .user_messages()
            .first()
            .map(|(_, text, _)| text.as_str())
            == Some(USER[0]),
        "the phone does not see the terminal's message first: {:?}",
        phone.user_messages()
    );

    println!(
        "== 2. the phone sends a message and an image; approval answered while the terminal types"
    );
    let part = upload(base, "photo.png", "image/png", PNG).await?;
    let task = phone
        .prompt(vec![
            ContentPart::Text {
                text: USER[1].to_owned(),
            },
            serde_json::from_value(part).context("the attachment part")?,
        ])
        .await?;
    let interrupt = phone.until_interrupt(&task).await?;
    let (typed, answered) =
        tokio::join!(runner.type_line(USER[2]), phone.answer(&interrupt, "allow"));
    typed?;
    let status = answered?;
    println!("approval answered from the phone while the terminal typed: HTTP {status}");
    phone
        .until("the approval turn", |seen| finished(seen) == 2)
        .await?;
    // The line typed while the phone's turn ran is queued behind it and runs after it.
    phone
        .until("the typed turn", |seen| finished(seen) == 3)
        .await?;

    println!("== 3. the tunnel is cut mid-turn");
    let task = phone.prompt(vec![text(USER[3])]).await?;
    let interrupt = phone.until_interrupt(&task).await?;
    ensure!(
        phone.agent_text().contains(AGENT[4]),
        "the turn is not mid-flight: {:?}",
        phone.agent_text()
    );
    runner.proxy.cut();
    let state = wait_presence(base, machine_id, |state| state != "online").await?;
    println!("tunnel cut; the hub shows the machine as {state}");
    runner.type_line(USER[4]).await?;
    let status = phone.answer(&interrupt, "allow").await?;
    println!("phone answered the approval while the tunnel is down: HTTP {status}");
    phone.quiet(Duration::from_millis(2500)).await?;
    ensure!(
        !phone.agent_text().contains(AGENT[5]) && finished(&phone.seen) == 3,
        "the turn moved while the tunnel was down: {:?}",
        phone.agent_text()
    );
    runner.proxy.restore();
    wait_presence(base, machine_id, |state| state == "online").await?;
    println!("tunnel restored; the machine is online again");
    phone
        .until("the cut turn", |seen| finished(seen) == 4)
        .await?;
    // The line typed while the tunnel was down was queued behind the cut turn.
    phone
        .until("the offline turn", |seen| finished(seen) == 5)
        .await?;

    println!("== 4. the transcript: phone live vs a fresh replay from the hub");
    let seen = phone.seen.clone();
    let replay = replay(&api, &thread, seen.len()).await?;
    ensure!(
        serde_json::to_value(&seen)? == serde_json::to_value(&replay)?,
        "the live transcript and the hub's replay differ"
    );
    for envelope in &seen {
        println!("{:>3} {}", envelope.seq, tracer::describe(envelope));
    }
    verify_phone(&seen)?;
    let prompts = prompts_of(&runner.recorder);
    verify_agent_prompts(&prompts)?;
    println!(
        "seq 1..{} without a gap; {} messages and {} replies once each; both approvals resolved once; the agent got {} prompts, the second with the image",
        seen.len(),
        USER.len(),
        AGENT.len() + 1,
        prompts.len()
    );
    Ok(seen)
}

// ---- verification -----------------------------------------------------------------------------

/// The number of `RunFinished{success}` events.
fn finished(seen: &[Envelope]) -> usize {
    seen.iter()
        .filter(|e| {
            matches!(&e.event, Event::RunFinished(f) if matches!(f.outcome, RunOutcome::Success { .. }))
        })
        .count()
}

/// `(message_id, text, author name)` of each user message, in order.
fn user_messages_of(seen: &[Envelope]) -> Vec<(String, String, String)> {
    seen.iter()
        .filter_map(|e| match &e.event {
            Event::UserMessage(m) => Some((
                m.message_id.clone(),
                m.parts
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.as_str()),
                        ContentPart::File { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
                m.author.name.clone().unwrap_or_else(|| m.author.id.clone()),
            )),
            _ => None,
        })
        .collect()
}

/// All the agent's text, in order.
fn agent_text_of(seen: &[Envelope]) -> String {
    seen.iter()
        .filter_map(|e| match &e.event {
            Event::TextMessageContent(t) => Some(t.delta.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The transcript the phone ends with: gap-free, every message and reply once, in order.
fn verify_phone(seen: &[Envelope]) -> Result<()> {
    for (index, envelope) in seen.iter().enumerate() {
        ensure!(
            envelope.seq == index as u64 + 1,
            "seq is {} at position {}: the transcript has a gap or a repeat",
            envelope.seq,
            index + 1
        );
    }
    let users = user_messages_of(seen);
    let texts: Vec<&str> = users.iter().map(|(_, text, _)| text.as_str()).collect();
    ensure!(
        texts == USER,
        "the user messages are {texts:?}, expected {USER:?}"
    );
    let ids: HashSet<&String> = users.iter().map(|(id, _, _)| id).collect();
    ensure!(ids.len() == USER.len(), "a message id repeats: {users:?}");
    let agent = agent_text_of(seen);
    for reply in AGENT.iter().chain([&AGENT_LAST]) {
        let count = agent.matches(reply).count();
        ensure!(count == 1, "{reply:?} appears {count} times in {agent:?}");
    }
    let mut interrupts = 0;
    let mut resolved = HashSet::new();
    for envelope in seen {
        match &envelope.event {
            Event::Interrupt(_) => interrupts += 1,
            Event::InterruptResolved(r) => {
                ensure!(
                    resolved.insert(r.interrupt_id.clone()),
                    "interrupt {} is resolved twice",
                    r.interrupt_id
                );
            }
            Event::RunError(error) => bail!("a run failed: {error:?}"),
            _ => {}
        }
    }
    ensure!(
        interrupts == 2 && resolved.len() == 2,
        "{interrupts} interrupts and {} resolutions, expected 2 and 2",
        resolved.len()
    );
    ensure!(finished(seen) == USER.len(), "not every turn finished");
    Ok(())
}

/// What the terminal's log holds is what the phone saw: the same messages and replies, once each.
fn verify_terminal_log(phone: &[Envelope], tui: &[Envelope]) -> Result<()> {
    let ids = |seen: &[Envelope]| {
        let mut ids: Vec<String> = user_messages_of(seen).into_iter().map(|u| u.0).collect();
        ids.sort();
        ids
    };
    ensure!(
        ids(phone) == ids(tui),
        "the terminal's messages {:?} differ from the phone's {:?}",
        user_messages_of(tui),
        user_messages_of(phone)
    );
    let agent = agent_text_of(tui);
    for reply in AGENT.iter().chain([&AGENT_LAST]) {
        let count = agent.matches(reply).count();
        ensure!(
            count == 1,
            "the terminal log has {reply:?} {count} times: {agent:?}"
        );
    }
    ensure!(
        finished(tui) == finished(phone),
        "the terminal log has {} finished runs, the phone {}",
        finished(tui),
        finished(phone)
    );
    Ok(())
}

/// The agent received each prompt once; the second has the image.
fn verify_agent_prompts(prompts: &[Value]) -> Result<()> {
    ensure!(
        prompts.len() == USER.len(),
        "the agent received {} prompts, expected {}",
        prompts.len(),
        USER.len()
    );
    let blocks = prompts[1]
        .as_array()
        .ok_or_else(|| anyhow!("prompt 2 has no blocks"))?;
    ensure!(
        blocks
            .iter()
            .any(|b| b["type"] == "text" && b["text"] == USER[1]),
        "prompt 2 has no text block: {blocks:?}"
    );
    ensure!(
        blocks.iter().any(|b| b["type"] == "image"
            && b["mimeType"] == "image/png"
            && b["data"] == STANDARD.encode(PNG)),
        "prompt 2 has no image block with the PNG bytes: {blocks:?}"
    );
    for (index, prompt) in prompts.iter().enumerate() {
        if index == 1 {
            continue;
        }
        let first = prompt
            .as_array()
            .and_then(|blocks| blocks.first())
            .and_then(|block| block["text"].as_str());
        ensure!(
            first == Some(USER[index]),
            "prompt {} is {first:?}, expected {:?}",
            index + 1,
            USER[index]
        );
    }
    Ok(())
}

/// The `prompt` blocks of each `session/prompt` the agent received.
fn prompts_of(recorder: &acp_mock::Recorder) -> Vec<Value> {
    recorder
        .messages()
        .into_iter()
        .filter(|m| m["method"] == "session/prompt")
        .map(|m| m["params"]["prompt"].clone())
        .collect()
}

// ---- the phone --------------------------------------------------------------------------------

fn text(text: &str) -> ContentPart {
    ContentPart::Text {
        text: text.to_owned(),
    }
}

/// A headless phone on the real `/v1` routes: the thread's subscription, and prompts and answers
/// through the outbox.
struct Phone {
    api: String,
    thread: String,
    cache: Cache,
    outbox: Outbox,
    transport: HttpTransport,
    rng: SystemRandom,
    subscription: Subscription,
    seen: Vec<Envelope>,
    counter: u32,
}

impl Phone {
    async fn connect(api: &str, thread: &str) -> Result<Self> {
        let cache = Cache::open_in_memory()?;
        let outbox = Outbox::load(&cache, RetryPolicy::default())?;
        let subscription = Subscription::connect(
            api,
            ThreadSync::new(thread.to_owned()),
            Some(DEV_TOKEN.to_owned()),
        )
        .await
        .context("subscribe to the thread")?;
        Ok(Self {
            api: api.to_owned(),
            thread: thread.to_owned(),
            cache,
            outbox,
            transport: HttpTransport::new(Some(DEV_TOKEN.to_owned())),
            rng: SystemRandom,
            subscription,
            seen: Vec::new(),
            counter: 0,
        })
    }

    fn user_messages(&self) -> Vec<(String, String, String)> {
        user_messages_of(&self.seen)
    }

    fn agent_text(&self) -> String {
        agent_text_of(&self.seen)
    }

    /// Reads the thread until `done` holds for what has been seen.
    async fn until(&mut self, what: &str, done: impl Fn(&[Envelope]) -> bool) -> Result<()> {
        let deadline = Instant::now() + WAIT;
        while !done(&self.seen) {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| anyhow!("timed out waiting for {what}: {:?}", self.agent_text()))?;
            match tokio::time::timeout(remaining, next_applied(&mut self.subscription)).await {
                Ok(applied) => self.record(applied?),
                Err(_) => bail!("timed out waiting for {what}: {:?}", self.agent_text()),
            }
        }
        Ok(())
    }

    /// Reads whatever arrives for `span`.
    async fn quiet(&mut self, span: Duration) -> Result<()> {
        let deadline = Instant::now() + span;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if let Ok(applied) =
                tokio::time::timeout(remaining, next_applied(&mut self.subscription)).await
            {
                self.record(applied?);
            }
        }
        Ok(())
    }

    fn record(&mut self, applied: Applied) {
        if let Applied::Event(envelope) = applied {
            self.seen.push(*envelope);
        }
    }

    /// The id of the first interrupt of `task`, once the phone sees it.
    async fn until_interrupt(&mut self, task: &str) -> Result<String> {
        let find = |seen: &[Envelope]| interrupt_of(seen, task);
        self.until("the approval", |seen| find(seen).is_some())
            .await?;
        find(&self.seen).ok_or_else(|| anyhow!("no interrupt for {task}"))
    }

    /// Sends a prompt through the outbox; returns its task id.
    async fn prompt(&mut self, parts: Vec<ContentPart>) -> Result<String> {
        self.counter += 1;
        let n = self.counter;
        let seed = format!("{:x}", SystemClock.now_ms());
        let task_id = format!("tk_phone_{seed}_{n}");
        let status = self
            .send(Intent::Prompt {
                thread_id: self.thread.clone(),
                task_id: task_id.clone(),
                message_id: format!("msg_phone_{seed}_{n}"),
                parts,
                machine_id: None,
                author: None,
            })
            .await?;
        ensure!(status == 202, "the prompt got HTTP {status}, expected 202");
        Ok(task_id)
    }

    /// Answers an approval with `option_id`; returns the HTTP status (200).
    async fn answer(&mut self, interrupt_id: &str, option_id: &str) -> Result<u16> {
        let status = self
            .send(Intent::Approval {
                thread_id: self.thread.clone(),
                interrupt_id: interrupt_id.to_owned(),
                outcome: Outcome::Selected {
                    option_id: option_id.to_owned(),
                },
                resolved_by: None,
            })
            .await?;
        ensure!(status == 200, "the answer got HTTP {status}, expected 200");
        Ok(status)
    }

    async fn send(&mut self, intent: Intent) -> Result<u16> {
        let local_id = self
            .outbox
            .enqueue(&self.cache, intent, SystemClock.now_ms())?;
        self.outbox
            .flush(
                &self.cache,
                &self.transport,
                &self.api,
                &SystemClock,
                &mut self.rng,
            )
            .await?;
        let entry = self
            .outbox
            .get(local_id)
            .ok_or_else(|| anyhow!("outbox entry {local_id} vanished"))?;
        match &entry.state {
            IntentState::Done { status, .. } | IntentState::Conflict { status, .. } => Ok(*status),
            IntentState::Rejected { status, error } => {
                Err(anyhow!("request refused with HTTP {status}: {error}"))
            }
            other => Err(anyhow!("request not answered: {other:?}")),
        }
    }
}

/// The first interrupt of `task`.
fn interrupt_of(seen: &[Envelope], task: &str) -> Option<String> {
    seen.iter().find_map(|e| match &e.event {
        Event::Interrupt(i) if e.task_id.as_deref() == Some(task) => Some(i.interrupt_id.clone()),
        _ => None,
    })
}

/// The next applied message. A dropped socket is reconnected from the last applied seq.
async fn next_applied(subscription: &mut Subscription) -> Result<Applied> {
    let mut reconnects = 0;
    loop {
        match subscription.next().await {
            Ok(applied) => return Ok(applied),
            Err(SubscriptionError::Closed | SubscriptionError::Read(_))
                if reconnects < MAX_RECONNECTS =>
            {
                reconnects += 1;
                tokio::time::sleep(Duration::from_millis(500 * u64::from(reconnects))).await;
                subscription.reconnect().await?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// A fresh subscription from the start, read until it holds `count` events.
async fn replay(api: &str, thread: &str, count: usize) -> Result<Vec<Envelope>> {
    let mut subscription = Subscription::connect(
        api,
        ThreadSync::new(thread.to_owned()),
        Some(DEV_TOKEN.to_owned()),
    )
    .await
    .context("subscribe for the replay")?;
    let mut events = Vec::new();
    let deadline = Instant::now() + WAIT;
    while events.len() < count {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow!("the replay stopped at {} of {count} events", events.len()))?;
        let applied = tokio::time::timeout(remaining, next_applied(&mut subscription))
            .await
            .map_err(|_| anyhow!("the replay stopped at {} of {count} events", events.len()))??;
        if let Applied::Event(envelope) = applied {
            events.push(*envelope);
        }
    }
    // Nothing more than what the live phone saw: a short wait for a stray event.
    if let Ok(Ok(Applied::Event(extra))) =
        tokio::time::timeout(Duration::from_millis(500), next_applied(&mut subscription)).await
    {
        bail!(
            "the hub has an event after the phone's last: seq {}",
            extra.seq
        );
    }
    Ok(events)
}

// ---- the hub's HTTP routes ----------------------------------------------------------------------

/// `GET /v1/threads` until a thread titled `title` is listed.
async fn find_session(api: &str, title: &str) -> Result<String> {
    let http = reqwest::Client::new();
    let deadline = Instant::now() + WAIT;
    loop {
        let body: Value = http
            .get(format!("{api}/threads"))
            .bearer_auth(DEV_TOKEN)
            .send()
            .await
            .context("GET /v1/threads")?
            .json()
            .await
            .context("threads answer")?;
        let found = body["threads"].as_array().and_then(|threads| {
            threads
                .iter()
                .find(|t| t["title"] == title)
                .and_then(|t| t["thread_id"].as_str())
        });
        if let Some(id) = found {
            return Ok(id.to_owned());
        }
        ensure!(
            Instant::now() < deadline,
            "no thread titled {title:?} in GET /v1/threads"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Polls `GET /v1/machines` until the machine's presence satisfies `want`; returns the state.
async fn wait_presence(
    base: &str,
    machine_id: &str,
    want: impl Fn(&str) -> bool,
) -> Result<String> {
    let http = reqwest::Client::new();
    let deadline = Instant::now() + WAIT;
    let mut last = String::new();
    loop {
        let body: Value = http
            .get(format!("{base}/v1/machines"))
            .bearer_auth(DEV_TOKEN)
            .send()
            .await
            .context("GET /v1/machines")?
            .json()
            .await
            .context("machines answer")?;
        if let Some(state) = body["machines"]
            .as_array()
            .and_then(|machines| machines.iter().find(|m| m["machine_id"] == machine_id))
            .and_then(|m| m["presence"]["state"].as_str())
        {
            last = state.to_owned();
            if want(state) {
                return Ok(last);
            }
        }
        ensure!(
            Instant::now() < deadline,
            "the machine's presence is {last:?}, not what the step needs"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// `POST /v1/attachments` then the PUT of `bytes` to the signed URL; returns the FilePart.
async fn upload(base: &str, name: &str, mime: &str, bytes: &[u8]) -> Result<Value> {
    let http = reqwest::Client::new();
    let response = http
        .post(format!("{base}/v1/attachments"))
        .bearer_auth(DEV_TOKEN)
        .header("Idempotency-Key", new_key("att"))
        .json(&json!({ "name": name, "mime_type": mime, "size": bytes.len() }))
        .send()
        .await
        .context("POST /v1/attachments")?;
    let status = response.status();
    let answer: Value = response.json().await.context("attachment answer")?;
    ensure!(
        status.as_u16() == 201,
        "POST /v1/attachments answered HTTP {status}: {answer}"
    );
    let upload_url = answer["upload_url"]
        .as_str()
        .ok_or_else(|| anyhow!("no upload_url in {answer}"))?;
    let put = http
        .put(upload_url)
        .body(bytes.to_vec())
        .send()
        .await
        .context("PUT the attachment bytes")?;
    ensure!(
        put.status().as_u16() == 204,
        "PUT of {name} answered HTTP {}",
        put.status()
    );
    answer
        .get("part")
        .cloned()
        .ok_or_else(|| anyhow!("the attachment answer has no part: {answer}"))
}

/// The dev machine credential (`POST /v1/dev/machines/{id}/credential`, DEV_FIXTURES only).
async fn mint_credential(base: &str, machine_id: &str) -> Result<String> {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/dev/machines/{machine_id}/credential"))
        .bearer_auth(DEV_TOKEN)
        .send()
        .await
        .context("POST the dev machine credential")?;
    let status = response.status();
    let answer: Value = response.json().await.context("credential answer")?;
    ensure!(
        status.as_u16() == 201,
        "dev credential answered HTTP {status}: {answer}"
    );
    answer["credential"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("no credential in {answer}"))
}

/// 32 random bytes from the kernel, as hex.
fn signing_key() -> Result<String> {
    let mut bytes = [0u8; 32];
    fs::File::open("/dev/urandom")
        .context("open /dev/urandom")?
        .read_exact(&mut bytes)
        .context("read /dev/urandom")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// A unique `Idempotency-Key` value.
fn new_key(what: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "idk_rc_{what}_{nanos}_{}",
        KEYS.fetch_add(1, Ordering::Relaxed)
    )
}

// ---- a TCP proxy that can be cut ----------------------------------------------------------------

/// Forwards TCP to the hub. [`Proxy::cut`] drops every open connection and refuses new ones until
/// [`Proxy::restore`]: for the tunnel that is a dead WebSocket and a hub that is not reachable.
struct Proxy {
    port: u16,
    state: Arc<ProxyState>,
    accept: JoinHandle<()>,
}

struct ProxyState {
    cut: AtomicBool,
    connections: Mutex<Vec<JoinHandle<()>>>,
}

impl Proxy {
    async fn start(upstream: SocketAddr) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind the proxy")?;
        let port = listener.local_addr()?.port();
        let state = Arc::new(ProxyState {
            cut: AtomicBool::new(false),
            connections: Mutex::new(Vec::new()),
        });
        let accept = {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                while let Ok((mut inbound, _)) = listener.accept().await {
                    if state.cut.load(Ordering::SeqCst) {
                        drop(inbound);
                        continue;
                    }
                    let connection = tokio::spawn(async move {
                        if let Ok(mut outbound) = TcpStream::connect(upstream).await {
                            let _ =
                                tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                        }
                    });
                    let mut connections = state
                        .connections
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    connections.retain(|c| !c.is_finished());
                    connections.push(connection);
                }
            })
        };
        Ok(Self {
            port,
            state,
            accept,
        })
    }

    fn cut(&self) {
        self.state.cut.store(true, Ordering::SeqCst);
        let mut connections = self
            .state
            .connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for connection in connections.drain(..) {
            // Dropping the task drops both sockets: the peers see the connection close.
            connection.abort();
        }
    }

    fn restore(&self) {
        self.state.cut.store(false, Ordering::SeqCst);
    }

    fn stop(&self) {
        self.cut();
        self.accept.abort();
    }
}

// ---- the computer -------------------------------------------------------------------------------

struct RunnerConfig {
    base: String,
    machine_id: String,
    credential: String,
    title: String,
}

/// What the terminal session shows, for the checks.
#[derive(Clone, Default)]
struct Snapshot {
    /// The session is shared with the hub (`Remote::On`).
    shared: bool,
    /// Every event of the terminal's own log, as the UI saw it.
    events: Vec<Envelope>,
}

/// The `sm --rc` session without a terminal.
struct Runner {
    thread_id: String,
    proxy: Proxy,
    keys: mpsc::Sender<Key>,
    snapshot: watch::Receiver<Snapshot>,
    recorder: acp_mock::Recorder,
    parts: Parts,
    _work: TempDir,
}

/// The tunnel and the session link, once the tunnel is built.
type Slot = Arc<Mutex<Option<(Arc<dyn Upstream>, String)>>>;

struct Parts {
    ui: JoinHandle<()>,
    bridge: JoinHandle<()>,
    agent: JoinHandle<Result<()>>,
    forward: JoinHandle<()>,
    tunnel: JoinHandle<Result<()>>,
    stop_tunnel: oneshot::Sender<()>,
}

impl Runner {
    /// Wires `tui::host`'s pieces together: the session, the bridge over an in-process acp-mock,
    /// the tunnel (through the proxy) and the UI loop, registered at start (`autoConnect`).
    async fn start(config: &RunnerConfig) -> Result<Self> {
        let work = TempDir::create()?;
        let project = work.path.join("project");
        for dir in ["project", "session", "tunnel"] {
            fs::create_dir_all(work.path.join(dir)).with_context(|| format!("create {dir}"))?;
        }
        let hub: SocketAddr = config
            .base
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .parse()
            .with_context(|| format!("the hub address of {}", config.base))?;
        let proxy = Proxy::start(hub).await?;

        let script = acp_mock::Script::from_value(&script()).context("rc acp-mock script")?;
        let recorder = acp_mock::Recorder::new();
        let (client_io, agent_io) = tokio::io::duplex(1 << 16);
        let (agent_reader, agent_writer) = tokio::io::split(agent_io);
        let agent = tokio::spawn(acp_mock::run_recorded(
            script,
            recorder.clone(),
            agent_reader,
            agent_writer,
        ));
        let (reader, writer) = tokio::io::split(client_io);

        let ids = Arc::new(UlidIds::new());
        let thread_id = ids.next_id("th_");

        // The tunnel is built after the session (its command task needs the session's handle);
        // the registrar hands `/rc` the connection once it exists.
        let slot: Slot = Arc::new(Mutex::new(None));
        let registrar: Registrar = {
            let slot = Arc::clone(&slot);
            Box::new(move |_handle| {
                let registration = slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                    .map(|(upstream, url)| Registration {
                        upstream: Arc::clone(upstream),
                        url: url.clone(),
                    })
                    .ok_or_else(|| "the tunnel is not ready".to_owned());
                Box::pin(async move { registration })
            })
        };
        let (mut session, sink, commands) = Session::new(
            Identity {
                thread_id: thread_id.clone(),
                title: config.title.clone(),
                agent: AGENT_ID.to_owned(),
                workspace: Workspace {
                    path: project.to_string_lossy().into_owned(),
                    branch: None,
                    spawn: None,
                },
            },
            Arc::new(now_ms),
            registrar,
        );

        let mut bridge_config = Config::new(config.machine_id.clone(), AGENT_ID, project.clone());
        bridge_config.file_policy = FilePolicy::loopback_for_tests(ATTACH_CAP);
        let session_store =
            Arc::new(Store::open(&work.path.join("session")).context("open the session store")?);
        let bridge = Bridge::connect(
            bridge_config,
            reader,
            writer,
            session_store,
            ids,
            sink,
            commands,
        );
        let bridge = tokio::spawn(async move {
            if let Err(error) = bridge.run().await {
                tracing::warn!("the session's bridge stopped: {error}");
            }
        });

        let tunnel_store =
            Arc::new(Store::open(&work.path.join("tunnel")).context("open the tunnel store")?);
        let (client, tunnel_sink, tunnel_commands) = tunnel(
            TunnelConfig {
                hub_ws_base: format!("ws://127.0.0.1:{}", proxy.port),
                machine_id: config.machine_id.clone(),
                runner_version: env!("CARGO_PKG_VERSION").to_owned(),
                capabilities: Capabilities {
                    host_mode: "local".to_owned(),
                    harnesses: vec![AGENT_ID.to_owned()],
                    max_sessions: 1,
                    spawn: vec!["same_dir".to_owned()],
                    features: Vec::new(),
                },
                timing: Timing {
                    backoff_base: Duration::from_millis(50),
                    backoff_cap: Duration::from_millis(400),
                    stable_after: Duration::from_secs(3600),
                    welcome_timeout: Duration::from_secs(5),
                },
            },
            tunnel_store,
            StaticCredential::new(config.credential.clone()),
        );
        let (stop_tunnel, stop_rx) = oneshot::channel::<()>();
        let tunnel_task = tokio::spawn(async move {
            client
                .run(async {
                    let _ = stop_rx.await;
                })
                .await
                .map(|_| ())
        });
        let handle = session.handle();
        let processed = handle
            .take_processed()
            .ok_or_else(|| anyhow!("the session's acks were taken already"))?;
        let forward = tokio::spawn(forward(
            tunnel_commands,
            tunnel_sink.clone(),
            processed,
            handle,
            thread_id.clone(),
        ));
        let url = format!("{}/t/{thread_id}", config.base.trim_end_matches('/'));
        *slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((Arc::new(TunnelUpstream(tunnel_sink)), url));

        let (keys, mut key_rx) = mpsc::channel::<Key>(256);
        let (snap_tx, snapshot) = watch::channel(Snapshot::default());
        let mut app = App::new(
            &thread_id,
            Settings { auto_connect: true },
            &thread_id.trim_start_matches("th_").to_ascii_lowercase(),
        );
        let ui = tokio::spawn(async move {
            ui_loop(&mut app, &mut session, &mut key_rx, &snap_tx).await;
        });

        Ok(Self {
            thread_id,
            proxy,
            keys,
            snapshot,
            recorder,
            parts: Parts {
                ui,
                bridge,
                agent,
                forward,
                tunnel: tunnel_task,
                stop_tunnel,
            },
            _work: work,
        })
    }

    fn snapshot(&self) -> Snapshot {
        self.snapshot.borrow().clone()
    }

    /// Waits until the session is registered with the hub (`remote: on`).
    async fn wait_shared(&self) -> Result<()> {
        let mut snapshot = self.snapshot.clone();
        tokio::time::timeout(WAIT, snapshot.wait_for(|s| s.shared))
            .await
            .map_err(|_| anyhow!("the session was not shared with the hub within {WAIT:?}"))?
            .map_err(|_| anyhow!("the terminal session ended"))?;
        Ok(())
    }

    /// The terminal types `line` and presses Enter.
    async fn type_line(&self, line: &str) -> Result<()> {
        for c in line.chars() {
            self.keys
                .send(Key::Char(c))
                .await
                .map_err(|_| anyhow!("the terminal session ended"))?;
        }
        self.keys
            .send(Key::Enter)
            .await
            .map_err(|_| anyhow!("the terminal session ended"))
    }

    /// Ends the session and its tunnel.
    async fn stop(self) {
        let Parts {
            ui,
            bridge,
            agent,
            forward,
            mut tunnel,
            stop_tunnel,
        } = self.parts;
        drop(self.keys);
        let _ = tokio::time::timeout(STOP_TIMEOUT, ui).await;
        let _ = stop_tunnel.send(());
        if tokio::time::timeout(STOP_TIMEOUT, &mut tunnel)
            .await
            .is_err()
        {
            tunnel.abort();
        }
        forward.abort();
        bridge.abort();
        agent.abort();
        self.proxy.stop();
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// `runner::tui::driver::run` without a terminal (this crate has no ratatui): keys and the
/// session's feed go to the [`App`], its actions are carried out on the session, and what the UI
/// saw is published.
async fn ui_loop(
    app: &mut App,
    session: &mut Session,
    keys: &mut mpsc::Receiver<Key>,
    snapshot: &watch::Sender<Snapshot>,
) {
    let publish = |app: &App, event: Option<Envelope>| {
        snapshot.send_modify(|s| {
            s.shared = matches!(app.remote(), runner::tui::Remote::On { .. });
            s.events.extend(event);
        });
    };
    let start = app.start();
    if !execute(start, app, session).await {
        return;
    }
    loop {
        publish(app, None);
        tokio::select! {
            biased;
            key = keys.recv() => {
                let Some(key) = key else { return };
                let actions = app.key(key);
                if !execute(actions, app, session).await {
                    return;
                }
            }
            feed = session.next() => {
                let Some(feed) = feed else { return };
                match feed {
                    Feed::Thread(incoming) => {
                        let event = match &incoming {
                            Incoming::Event(envelope) => Some((**envelope).clone()),
                            _ => None,
                        };
                        let actions = app.incoming(incoming);
                        publish(app, event);
                        if !execute(actions, app, session).await {
                            return;
                        }
                    }
                    Feed::Reset => {}
                    Feed::Connectivity(state) => app.set_connectivity(state),
                    Feed::Refused { task_id, message } => app.refused(&task_id, &message),
                }
            }
        }
    }
}

/// The tunnel as the session's hub (as `tui::host`'s).
struct TunnelUpstream(TunnelSink);

impl Upstream for TunnelUpstream {
    fn open(
        &self,
        thread_id: &str,
        title: &str,
        agent: &str,
        workspace: &Workspace,
    ) -> Result<(), String> {
        self.0.mark_local_thread(thread_id);
        self.0
            .thread_open(thread_id, title, agent, workspace)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn send<'a>(&'a self, envelopes: Vec<Envelope>) -> UpstreamFuture<'a> {
        Box::pin(async move {
            self.0
                .send(envelopes)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn prompt(
        &self,
        thread_id: &str,
        task_id: &str,
        message_id: &str,
        parts: &[ContentPart],
    ) -> Result<(), String> {
        self.0
            .prompt_submit(thread_id, task_id, message_id, parts)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn answer<'a>(
        &'a self,
        thread_id: &'a str,
        interrupt_id: &'a str,
        outcome: &'a Outcome,
    ) -> HubAnswerFuture<'a> {
        Box::pin(async move {
            self.0
                .interrupt_answer(thread_id, interrupt_id, outcome, ANSWER_WAIT)
                .await
                .map(verdict)
        })
    }
}

fn verdict(reply: AnswerReply) -> HubVerdict {
    match reply.status {
        AnswerStatus::Accepted => HubVerdict::Accepted,
        AnswerStatus::AlreadyResolved => HubVerdict::AlreadyAnswered {
            by: reply.resolved_by,
        },
        AnswerStatus::Rejected => HubVerdict::Rejected(reply.message),
    }
}

/// Feeds the hub's commands for this thread to the session and acks what the bridge processed
/// (as `tui::host`'s). A command for another thread is refused.
async fn forward(
    mut commands: TunnelCommands,
    sink: TunnelSink,
    mut processed: Processed,
    handle: Handle,
    thread_id: String,
) {
    loop {
        tokio::select! {
            incoming = commands.next() => {
                let Some(incoming) = incoming else { return };
                if command_thread(&incoming.command) == thread_id {
                    handle.hub_command(incoming).await;
                    continue;
                }
                let failure = CommandFailure {
                    idempotency_key: incoming.idempotency_key.clone(),
                    code: CommandErrorCode::Capacity,
                    message: "this machine is running an interactive session".to_owned(),
                };
                let _ = sink.command_result(failure).await;
                commands.processed(incoming.seq, &incoming.idempotency_key).await;
            }
            Some((seq, key)) = processed.recv() => commands.processed(seq, &key).await,
        }
    }
}

fn command_thread(command: &Command) -> &str {
    match command {
        Command::Prompt { thread_id, .. }
        | Command::Approval { thread_id, .. }
        | Command::Dispatch { thread_id, .. }
        | Command::Cancel { thread_id, .. }
        | Command::SessionConfig { thread_id, .. } => thread_id,
    }
}
