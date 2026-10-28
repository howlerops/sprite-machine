//! `cli e2e tracer`: the MVP-000 tracer bullet, end to end.
//!
//! acp-mock (in-process, over `tokio::io::duplex`) → runner core (`sidecar::tracer` bridge +
//! `runner::tracer` tunnel client, in-process) → tunnel → hub (`crates/hub`, built with
//! `worker-build`, served by `wrangler dev --local` on a free port, D1 migrations applied) →
//! headless client (`client_core::tracer`) on the real `/v1` routes as the dev user
//! (`DEV_FIXTURES=1`, client-api.md §2).
//!
//! The machine is registered with `POST /v1/machines`; the client creates the thread with
//! `POST /v1/threads`, subscribes with `GET /v1/threads/{id}/subscribe` and answers the
//! interrupt with `POST /v1/threads/{id}/interrupts/{id}`. The run passes when the client
//! observes the turn-event.md §5 rule 6 stream for one prompt with one approval (the hub emits
//! the `UserMessage`), answers it through the hub, and sees `RunFinished{success}`, with
//! envelope `seq` strictly increasing by one.
//!
//! `wrangler dev` runs in its own process group; the group is killed when this command ends,
//! on success, on error, on panic (unwinding), and on Ctrl-C / SIGTERM. Local only: this never
//! runs `wrangler deploy`, `wrangler login` or anything `--remote`.

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::wasm;
use anyhow::{Context, Result, anyhow, bail, ensure};
use protocol::turn_event::{Envelope, Event, InterruptKind, RunOutcome, RunReason};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};

const AGENT_ID: &str = "agt_acp_mock";
const READY_TIMEOUT: Duration = Duration::from_secs(240);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(60);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Cargo profile for the hub bundle; the same name is in `crates/hub/wrangler.toml` `[build]`.
const WASM_PROFILE: &str = "wasm-release";
/// The gzip'd hub bundle must stay under this (MVP-000a acceptance: gzip < 1 MiB).
const BUNDLE_LIMIT_GZIP: u64 = 1024 * 1024;

/// The acp-mock script: one prompt turn with text, a tool call that needs permission (the
/// client must answer an allow option), the tool call completing, more text, `end_turn`.
pub(super) fn script() -> serde_json::Value {
    json!({
        "turns": [{
            "steps": [
                { "text": "I'll add the /healthz route." },
                { "tool_call": { "toolCallId": "call_1", "title": "Edit src/main.rs",
                                 "kind": "edit", "status": "pending",
                                 "locations": [{ "path": "src/main.rs", "line": 12 }] } },
                { "request_permission": {
                    "toolCall": { "toolCallId": "call_1", "title": "Edit src/main.rs",
                                  "kind": "edit" },
                    "options": [
                        { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                        { "optionId": "reject", "name": "Deny", "kind": "reject_once" }
                    ] },
                  "expect": "allow" },
                { "tool_call_update": { "toolCallId": "call_1", "status": "completed" } },
                { "text": "Done: the route is in." },
                { "stop": "end_turn" }
            ]
        }]
    })
}

/// Entry point for `cli e2e tracer`.
pub async fn run() -> Result<()> {
    let started = Instant::now();
    let work = TempDir::create()?;
    let result = tokio::select! {
        result = tracer(&work) => result,
        () = interrupted() => Err(anyhow!("interrupted; stopping wrangler")),
    };
    // `tracer` owns the wrangler guard, so wrangler is already stopped here (also when the
    // select above dropped the future on a signal).
    match &result {
        Ok(()) => println!("PASS e2e tracer in {:.1}s", started.elapsed().as_secs_f64()),
        Err(_) => eprintln!(
            "FAIL e2e tracer after {:.1}s",
            started.elapsed().as_secs_f64()
        ),
    }
    result
}

/// Resolves on Ctrl-C or SIGTERM.
pub(super) async fn interrupted() {
    let ctrl_c = tokio::signal::ctrl_c();
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = ctrl_c => {}
                _ = term.recv() => {}
            }
        }
        Err(_) => {
            let _ = ctrl_c.await;
        }
    }
}

async fn tracer(work: &TempDir) -> Result<()> {
    let hub_dir = hub_dir()?;
    build_hub(&hub_dir, work).await?;

    println!("== d1 migrations (local)");
    migrate(&hub_dir, work)?;

    let port = free_port()?;
    let base = format!("http://127.0.0.1:{port}");
    println!("== wrangler dev --local on port {port}");
    let wrangler = Wrangler::start(&hub_dir, port, work, &[])?;
    wrangler.wait_ready(&base).await?;

    println!("== runner core (acp-mock + bridge + tunnel) and headless client");
    let observed = match create_machine(&base).await {
        Ok(machine_id) => run_loop(&base, &machine_id).await,
        Err(error) => Err(error),
    };
    let stop = wrangler.stop();
    let observed = observed?;
    stop?;
    println!("wrangler stopped");

    println!("== event sequence (seq · type · detail)");
    for envelope in &observed.envelopes {
        println!("{:>3} {}", envelope.seq, describe(envelope));
    }
    check_sequence(&observed.envelopes)?;
    ensure!(
        observed.second_answer_status == 409,
        "a second answer to the interrupt got HTTP {}, expected 409 (first answer wins)",
        observed.second_answer_status
    );
    println!("second answer to the interrupt: HTTP 409 (first answer wins)");
    Ok(())
}

/// Builds the hub bundle with worker-build (`WASM_PROFILE`) and strips its names. Shared with
/// the other hub-backed e2e checks.
pub(super) async fn build_hub(hub_dir: &Path, work: &TempDir) -> Result<()> {
    let wasm_dir = wasm_target_dir(hub_dir)?;
    println!(
        "== worker-build --profile {WASM_PROFILE} ({}, wasm target {})",
        hub_dir.display(),
        wasm_dir.display()
    );
    let build_log = work.path.join("worker-build.log");
    let build_started = Instant::now();
    let status = wasm_command("worker-build", hub_dir)
        .env("CARGO_TARGET_DIR", &wasm_dir)
        // Cargo names the hub package path for fresh builds too ("Fresh hub v0.1.0 (path)").
        .env("CARGO_TERM_VERBOSE", "true")
        .args(["--profile", WASM_PROFILE])
        .stdout(log_file(&build_log)?)
        .stderr(log_file_append(&build_log)?)
        .kill_on_drop(true)
        .status()
        .await
        .context("run worker-build (is worker-build 0.8.7 installed?)")?;
    if !status.success() {
        print_tail(&build_log, 40);
        bail!("worker-build failed: {status}");
    }
    println!("built in {:.1}s", build_started.elapsed().as_secs_f64());
    // CI-002: the bundle must come from this tree, so the log has to name this checkout's crates/hub.
    let log =
        fs::read_to_string(&build_log).with_context(|| format!("read {}", build_log.display()))?;
    let package = format!("({})", hub_dir.display());
    if !log.contains(&package) {
        print_tail(&build_log, 20);
        bail!("worker-build log does not name {package}: the bundle was not built from this tree");
    }
    // The same step as the second command of the wrangler [build] command (MVP-000f).
    let (before, after) = wasm::strip_file(&hub_dir.join("build/index_bg.wasm"))?;
    println!("strip-names: index_bg.wasm {before} -> {after} bytes");
    report_bundle(hub_dir)
}

/// Wires acp-mock → bridge → tunnel client in-process and runs the headless client against the
/// hub. Shuts the runner side down afterwards.
async fn run_loop(base: &str, machine_id: &str) -> Result<client_core::tracer::Observed> {
    let script = acp_mock::Script::from_value(&script()).context("tracer acp-mock script")?;
    let (client_io, agent_io) = tokio::io::duplex(1 << 16);
    let (agent_reader, agent_writer) = tokio::io::split(agent_io);
    let agent = tokio::spawn(acp_mock::run(script, agent_reader, agent_writer));

    let (command_tx, command_rx) = mpsc::channel(64);
    let (event_tx, event_rx) = mpsc::channel(256);
    let (client_reader, client_writer) = tokio::io::split(client_io);
    let cwd = std::env::current_dir().context("current dir")?;
    let cwd_text = cwd.to_string_lossy().into_owned();
    let mut bridge = tokio::spawn(sidecar::tracer::run(
        sidecar::tracer::BridgeConfig {
            machine_id: machine_id.to_owned(),
            agent_id: AGENT_ID.to_owned(),
            cwd,
        },
        client_reader,
        client_writer,
        command_rx,
        event_tx,
    ));

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let ws_base = base.replacen("http", "ws", 1);
    let mut tunnel = tokio::spawn(runner::tracer::run(
        runner::tracer::TunnelConfig {
            hub_ws_base: ws_base,
            machine_id: machine_id.to_owned(),
            runner_version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        event_rx,
        command_tx,
        async {
            let _ = shutdown_rx.await;
        },
    ));

    let client = client_core::tracer::run(client_core::tracer::ClientConfig {
        http_base: base.to_owned(),
        machine_id: machine_id.to_owned(),
        agent: AGENT_ID.to_owned(),
        workspace: cwd_text,
        prompt: "Add a /healthz route".to_owned(),
        timeout: CLIENT_TIMEOUT,
    });

    let observed = tokio::select! {
        observed = client => observed,
        result = &mut bridge => Err(task_ended("bridge", result)),
        result = &mut tunnel => Err(task_ended("tunnel client", result)),
    };

    // Orderly shutdown: the tunnel closes with `normal`, which drops the command channel and
    // ends the bridge; the agent ends when the bridge drops its pipe.
    let _ = shutdown_tx.send(());
    if !tunnel.is_finished() {
        match tokio::time::timeout(STOP_TIMEOUT, &mut tunnel).await {
            Ok(Ok(Err(error))) => tracing::warn!("tunnel client: {error:#}"),
            Ok(_) => {}
            Err(_) => tunnel.abort(),
        }
    }
    if !bridge.is_finished()
        && tokio::time::timeout(STOP_TIMEOUT, &mut bridge)
            .await
            .is_err()
    {
        bridge.abort();
    }
    agent.abort();
    observed
}

/// Registers the runner's machine as the dev user: `POST /v1/machines` (client-api.md §5.1).
pub(super) async fn create_machine(base: &str) -> Result<String> {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/machines"))
        .bearer_auth("dev")
        .json(&json!({ "name": "tracer" }))
        .send()
        .await
        .context("POST /v1/machines")?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.context("machine answer")?;
    ensure!(
        status.as_u16() == 201,
        "POST /v1/machines answered HTTP {status}: {body}"
    );
    body.pointer("/machine/id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("POST /v1/machines answered without machine.id: {body}"))
}

/// Applies the hub's D1 migrations to the local database wrangler dev will persist in `work`.
pub(super) fn migrate(hub_dir: &Path, work: &TempDir) -> Result<()> {
    let log = work.path.join("d1-migrations.log");
    let status = Command::new("wrangler")
        .current_dir(hub_dir)
        .args(["d1", "migrations", "apply", "DB", "--local", "--persist-to"])
        .arg(work.path.join("persist"))
        .env_remove("RUSTFLAGS")
        .env_remove("RUSTC_WRAPPER")
        .env("WRANGLER_SEND_METRICS", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::from(File::create(&log).context("d1 log")?))
        .stderr(Stdio::from(log_file_append_std(&log)?))
        .status()
        .context("run wrangler d1 migrations apply (is wrangler installed?)")?;
    if !status.success() {
        print_tail(&log, 40);
        bail!("wrangler d1 migrations apply failed: {status}");
    }
    Ok(())
}

pub(super) fn task_ended(
    name: &str,
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> anyhow::Error {
    match result {
        Ok(Ok(())) => anyhow!("{name} ended before the client finished"),
        Ok(Err(error)) => error.context(format!("{name} failed")),
        Err(error) => anyhow!("{name} panicked or was cancelled: {error}"),
    }
}

/// One-line description of an envelope for the printed sequence.
pub(super) fn describe(envelope: &Envelope) -> String {
    let detail = match &envelope.event {
        Event::UserMessage(m) => format!("{} parts", m.parts.len()),
        Event::RunStarted(r) => format!(
            "{:?}{}",
            r.reason,
            r.parent_run_id
                .as_deref()
                .map(|p| format!(" parent={p}"))
                .unwrap_or_default()
        ),
        Event::RunFinished(r) => match &r.outcome {
            RunOutcome::Success { stop_reason } => format!("success {stop_reason:?}"),
            RunOutcome::Interrupt { interrupt_ids } => format!("interrupt {interrupt_ids:?}"),
        },
        Event::RunError(e) => format!("{:?} {}", e.reason, e.message),
        Event::TextMessageContent(t) => format!("{:?}", t.delta),
        Event::ToolCallStart(t) => format!("{} {:?}", t.tool_call_id, t.title),
        Event::ToolCallEnd(t) => format!("{} {:?}", t.tool_call_id, t.status),
        Event::Interrupt(i) => format!("{:?} {} {:?}", i.kind, i.interrupt_id, i.title),
        Event::InterruptResolved(r) => format!("{} {:?}", r.interrupt_id, r.outcome),
        _ => String::new(),
    };
    format!(
        "{:<20} {:<24} {}",
        envelope.event.type_name(),
        envelope.run_id.as_deref().unwrap_or("-"),
        detail
    )
}

/// Checks the observed stream against turn-event.md §5 (rule 6 for the interrupt round trip)
/// for the tracer script.
pub(super) fn check_sequence(envelopes: &[Envelope]) -> Result<()> {
    for pair in envelopes.windows(2) {
        ensure!(
            pair[1].seq == pair[0].seq + 1,
            "seq not strictly increasing by one: {} then {}",
            pair[0].seq,
            pair[1].seq
        );
    }
    // The hub also logs state (`StateSnapshot`/`StateDelta`) and the machine's `Presence` in the
    // thread's stream; they are not part of the turn shape rule 6 describes.
    let shape: Vec<String> = envelopes
        .iter()
        .filter(|e| {
            !matches!(
                e.event,
                Event::StateSnapshot(_) | Event::StateDelta(_) | Event::Presence(_)
            )
        })
        .map(shape_of)
        .collect();
    let expected = [
        "UserMessage",
        "RunStarted{prompt}",
        "TextMessageStart",
        "TextMessageContent",
        "TextMessageEnd",
        "ToolCallStart",
        "Interrupt{approval}",
        "RunFinished{interrupt}",
        "InterruptResolved{selected}",
        "RunStarted{resume}",
        "ToolCallEnd",
        "TextMessageStart",
        "TextMessageContent",
        "TextMessageEnd",
        "RunFinished{success}",
    ];
    ensure!(
        shape == expected,
        "unexpected event stream:\n  got      {shape:?}\n  expected {expected:?}"
    );
    // The resume run names the run that ended with the interrupt.
    let paused_run = envelopes
        .iter()
        .find(|e| matches!(&e.event, Event::RunFinished(f) if matches!(f.outcome, RunOutcome::Interrupt { .. })))
        .and_then(|e| e.run_id.clone());
    let resumed_parent = envelopes.iter().find_map(|e| match &e.event {
        Event::RunStarted(r) if r.reason == RunReason::Resume => r.parent_run_id.clone(),
        _ => None,
    });
    ensure!(
        paused_run.is_some() && paused_run == resumed_parent,
        "resume parent_run_id {resumed_parent:?} does not name the paused run {paused_run:?}"
    );
    println!(
        "stream matches turn-event.md §5 rule 6; seq 1..={} strictly increasing",
        envelopes.len()
    );
    Ok(())
}

fn shape_of(envelope: &Envelope) -> String {
    match &envelope.event {
        Event::RunStarted(r) => match r.reason {
            RunReason::Prompt => "RunStarted{prompt}".to_owned(),
            RunReason::Resume => "RunStarted{resume}".to_owned(),
            RunReason::Retry => "RunStarted{retry}".to_owned(),
        },
        Event::RunFinished(r) => match r.outcome {
            RunOutcome::Success { .. } => "RunFinished{success}".to_owned(),
            RunOutcome::Interrupt { .. } => "RunFinished{interrupt}".to_owned(),
        },
        Event::Interrupt(i) => match i.kind {
            InterruptKind::Approval => "Interrupt{approval}".to_owned(),
            InterruptKind::Form => "Interrupt{form}".to_owned(),
            InterruptKind::Auth => "Interrupt{auth}".to_owned(),
        },
        Event::InterruptResolved(r) => {
            let outcome = serde_json::to_value(&r.outcome)
                .ok()
                .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
                .unwrap_or_default();
            format!("InterruptResolved{{{outcome}}}")
        }
        other => other.type_name().to_owned(),
    }
}

// ---- processes ----------------------------------------------------------------------------

/// The hub crate of the checkout the cli runs in, found at run time. Not from
/// `CARGO_MANIFEST_DIR`: with a shared `CARGO_TARGET_DIR` a cached cli binary built in another
/// (since deleted) checkout would point at a missing tree.
pub(super) fn hub_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("current dir")?;
    let dir = find_hub_dir(&cwd).ok_or_else(|| {
        anyhow!(
            "no crates/hub/wrangler.toml above {}; run `cargo run -p cli -- e2e tracer` inside the repository",
            cwd.display()
        )
    })?;
    dir.canonicalize()
        .with_context(|| format!("hub crate at {}", dir.display()))
}

/// CI-002: the wasm32 target dir of the hub built from this checkout: `wasm-<hash>` under the
/// shared `CARGO_TARGET_DIR` (default `<root>/target`), keyed by the checkout root so two worktrees
/// never share hub wasm artifacts. Native builds keep the shared dir. Matches scripts/hub-test.sh.
pub(super) fn wasm_target_dir(hub_dir: &Path) -> Result<PathBuf> {
    let root = hub_dir
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| anyhow!("hub crate {} is not under crates/", hub_dir.display()))?;
    let shared = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    let key: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(shared.join(format!("wasm-{key}")))
}

/// Walks up from `start` to the first directory that holds the workspace `Cargo.toml` and
/// `crates/hub/wrangler.toml`, and returns `crates/hub` under it.
fn find_hub_dir(start: &Path) -> Option<PathBuf> {
    start.ancestors().find_map(|dir| {
        (dir.join("Cargo.toml").is_file() && dir.join("crates/hub/wrangler.toml").is_file())
            .then(|| dir.join("crates/hub"))
    })
}

/// Prints the hub bundle size as the Worker ships it: `gzip -9` of `build/index.js` and
/// `build/index_bg.wasm`, each compressed on its own and summed. Fails over the limit.
fn report_bundle(hub_dir: &Path) -> Result<()> {
    let build = hub_dir.join("build");
    let mut total = 0_u64;
    for name in ["index.js", "index_bg.wasm"] {
        let path = build.join(name);
        let raw = fs::metadata(&path)
            .with_context(|| format!("bundle file {}", path.display()))?
            .len();
        let gzipped = gzip_size(&path)?;
        println!("{name}: {raw} bytes, gzip -9 {gzipped} bytes");
        total += gzipped;
    }
    println!(
        "bundle gzip -9: {total} bytes ({:.1} KiB), limit {} bytes",
        total as f64 / 1024.0,
        BUNDLE_LIMIT_GZIP
    );
    ensure!(
        total < BUNDLE_LIMIT_GZIP,
        "hub bundle is {total} bytes gzip'd, over the {BUNDLE_LIMIT_GZIP} byte limit"
    );
    Ok(())
}

/// Size of `path` after `gzip -9 -n` (no name or timestamp in the header). Shells out to
/// `gzip`: the cli has no compression crate.
fn gzip_size(path: &Path) -> Result<u64> {
    let input = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let output = Command::new("gzip")
        .args(["-9", "-n", "-c"])
        .stdin(input)
        .stderr(Stdio::inherit())
        .output()
        .context("run gzip (is gzip installed?)")?;
    ensure!(
        output.status.success(),
        "gzip {} failed: {}",
        path.display(),
        output.status
    );
    u64::try_from(output.stdout.len()).context("gzip size")
}

/// A command for the wasm toolchain: the native lld `RUSTFLAGS` must not reach the wasm build.
/// The size settings live in the `wasm-release` profile (root Cargo.toml).
fn wasm_command(program: &str, dir: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command
        .current_dir(dir)
        .env_remove("RUSTFLAGS")
        .env_remove("RUSTC_WRAPPER")
        .env("WRANGLER_SEND_METRICS", "false")
        .stdin(Stdio::null());
    command
}

pub(super) fn free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("bind a free port")?;
    Ok(listener.local_addr().context("local addr")?.port())
}

/// `wrangler dev --local`, in its own process group; the whole group (workerd included) is
/// killed by [`Wrangler::stop`] or, failing that, on drop.
pub(super) struct Wrangler {
    child: Option<Child>,
    pgid: i32,
    log: PathBuf,
}

impl Wrangler {
    /// wrangler's bundling scratch `crates/hub/.wrangler` stays in the checkout; it is ignored by
    /// `crates/hub/.gitignore`. `extra_vars` are passed as `--var NAME:VALUE`.
    pub(super) fn start(
        hub_dir: &Path,
        port: u16,
        work: &TempDir,
        extra_vars: &[String],
    ) -> Result<Self> {
        let log = work.path.join("wrangler.log");
        let persist = work.path.join("persist");
        // wrangler dev reruns the [build] command: it must use this worktree's wasm target dir.
        let wasm_dir = wasm_target_dir(hub_dir)?;
        let mut command = Command::new("wrangler");
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        command
            .current_dir(hub_dir)
            // `--inspector-port 0`: the default 9229 is shared by every wrangler on the machine.
            .args([
                "dev",
                "--local",
                "--inspector-port",
                "0",
                "--ip",
                "127.0.0.1",
                "--port",
            ])
            .arg(port.to_string())
            .arg("--persist-to")
            .arg(&persist)
            // The dev-only routes and the placeholder `Bearer dev` token (DEV_FIXTURES=1).
            .args(["--var", "DEV_FIXTURES:1"])
            .args(extra_vars.iter().flat_map(|var| ["--var", var.as_str()]))
            .arg("--show-interactive-dev-session=false")
            .env_remove("RUSTFLAGS")
            .env_remove("RUSTC_WRAPPER")
            .env("WRANGLER_SEND_METRICS", "false")
            .env("HUB_WASM_TARGET_DIR", &wasm_dir)
            .stdin(Stdio::null())
            .stdout(std::process::Stdio::from(
                File::create(&log).context("wrangler log")?,
            ))
            .stderr(std::process::Stdio::from(log_file_append_std(&log)?));
        let child = command
            .spawn()
            .context("start wrangler dev (is wrangler installed?)")?;
        let pgid = i32::try_from(child.id()).context("wrangler pid")?;
        Ok(Self {
            child: Some(child),
            pgid,
            log,
        })
    }

    pub(super) async fn wait_ready(&self, base: &str) -> Result<()> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .context("http client")?;
        let started = Instant::now();
        loop {
            if !group_alive(self.pgid) {
                print_tail(&self.log, 60);
                bail!("wrangler dev exited early");
            }
            let ready = http
                .get(format!("{base}/healthz"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success());
            if ready {
                println!("ready after {:.1}s", started.elapsed().as_secs_f64());
                return Ok(());
            }
            if started.elapsed() > READY_TIMEOUT {
                print_tail(&self.log, 60);
                bail!("wrangler dev not ready after {READY_TIMEOUT:?}");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Kills the process group and reaps wrangler. Errors if the group survives SIGKILL.
    pub(super) fn stop(mut self) -> Result<()> {
        self.kill_group();
        ensure!(
            !group_alive(self.pgid),
            "wrangler process group {} still alive",
            self.pgid
        );
        Ok(())
    }

    fn kill_group(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let group = format!("-{}", self.pgid);
        let _ = Command::new("kill")
            .args(["-TERM", "--", &group])
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + STOP_TIMEOUT;
        while group_alive(self.pgid) && Instant::now() < deadline {
            // Reap wrangler as soon as it exits so the group check below is accurate.
            let _ = child.try_wait();
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = Command::new("kill")
            .args(["-KILL", "--", &group])
            .stderr(Stdio::null())
            .status();
        let _ = child.wait();
        let deadline = Instant::now() + STOP_TIMEOUT;
        while group_alive(self.pgid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Wrangler {
    fn drop(&mut self) {
        self.kill_group();
    }
}

/// `true` while any process of the group is alive (`kill -0 -- -pgid`).
fn group_alive(pgid: i32) -> bool {
    Command::new("kill")
        .args(["-0", "--", &format!("-{pgid}")])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

// ---- files --------------------------------------------------------------------------------

/// A per-run scratch directory (logs, wrangler `--persist-to`), removed on drop.
pub(super) struct TempDir {
    pub(super) path: PathBuf,
}

impl TempDir {
    pub(super) fn create() -> Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("sm-e2e-tracer-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn log_file(path: &Path) -> Result<Stdio> {
    Ok(Stdio::from(
        File::create(path).with_context(|| format!("create {}", path.display()))?,
    ))
}

fn log_file_append(path: &Path) -> Result<Stdio> {
    Ok(Stdio::from(log_file_append_std(path)?))
}

fn log_file_append_std(path: &Path) -> Result<File> {
    File::options()
        .append(true)
        .create(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

fn print_tail(path: &Path, lines: usize) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let all: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();
    eprintln!("---- {} (tail) ----", path.display());
    for line in &all[all.len().saturating_sub(lines)..] {
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake checkout: workspace `Cargo.toml`, `crates/hub/wrangler.toml`, `crates/cli/src/e2e`.
    fn fake_checkout(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("sm-hub-lookup-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("crates/hub")).unwrap();
        fs::create_dir_all(root.join("crates/cli/src/e2e")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        fs::write(root.join("crates/hub/wrangler.toml"), "name = \"sm-hub\"\n").unwrap();
        root
    }

    #[test]
    fn hub_dir_is_found_from_a_nested_directory() {
        let root = fake_checkout("nested");
        let found = find_hub_dir(&root.join("crates/cli/src/e2e"));
        assert_eq!(found, Some(root.join("crates/hub")));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn hub_dir_is_found_from_the_checkout_root() {
        let root = fake_checkout("root");
        assert_eq!(find_hub_dir(&root), Some(root.join("crates/hub")));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn wasm_target_dir_is_keyed_by_the_checkout_root() {
        let a = fake_checkout("wasm-a");
        let b = fake_checkout("wasm-b");
        let dir_a = wasm_target_dir(&a.join("crates/hub")).unwrap();
        let dir_b = wasm_target_dir(&b.join("crates/hub")).unwrap();
        assert_ne!(
            dir_a, dir_b,
            "two checkouts must not share a wasm target dir"
        );
        assert_eq!(wasm_target_dir(&a.join("crates/hub")).unwrap(), dir_a);
        assert!(
            dir_a
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("wasm-")
        );
        fs::remove_dir_all(&a).unwrap();
        fs::remove_dir_all(&b).unwrap();
    }

    #[test]
    fn hub_dir_lookup_fails_without_a_hub_above() {
        let root =
            std::env::temp_dir().join(format!("sm-hub-lookup-{}-no-hub", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        assert_eq!(find_hub_dir(&root.join("a/b")), None);
        fs::remove_dir_all(&root).unwrap();
    }
}
