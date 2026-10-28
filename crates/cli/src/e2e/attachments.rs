//! `cli e2e attachments`: user file parts, end to end (M2B-012).
//!
//! The stack of `e2e tracer` ([`super::tracer`]): the hub bundle built with worker-build, the D1
//! migrations, and `wrangler dev --local` on a free port, here with an `ATTACHMENT_SIGNING_KEY`
//! for the run. The runner is `sm rc` server mode, in-process
//! (`runner::server::run_with_file_policy`). Its harness is a stub `opencode` on a fake PATH that
//! runs this binary as the agent (`e2e attachments-agent`: acp-mock with its Recorder, which dumps
//! what the runner sent to `recorded-1.json`).
//!
//! Attachments go through the real routes: `POST /v1/attachments`, a PUT of the bytes to the
//! signed `upload_url`, then the FileParts of the answer in a prompt (client-api.md §5.3, §5.6).
//!
//! **Loopback.** The hub is `http://127.0.0.1`, which the production fetch policy refuses. This
//! check runs the runner under `FilePolicy::loopback_for_tests`, which exists only with the
//! `test-support` feature. That feature is enabled by this crate's `runner` dependency, and no
//! flag or environment variable selects it, so `sm` never gets the loopback policy. The cap is
//! 1 KiB, so the oversize case stays small.
//!
//! Checks:
//! 1. A PNG and a text file in one prompt. The agent receives an `image` block with the PNG bytes
//!    and an `@.sm/attachments/notes.txt` text block. The file in the workspace has the sha256
//!    that was uploaded.
//! 2. A FilePart URL with another org in its path, a tampered signature, and an oversize file
//!    each end their task with `RunError{code:"attachment"}`. The agent never receives their
//!    prompts, and `.sm/attachments/` holds only `notes.txt`.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use client_core::{Applied, Subscription, ThreadSync};
use protocol::turn_event::Event;
use runner::login::{CredentialStore, Login};
use runner::server::{self, Options, PermissionMode};
use runner::trust::TrustStore;
use runner::tunnel::Timing;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sidecar::bridge::files::FilePolicy;
use tokio::sync::oneshot;

use super::tracer::{self, TempDir, Wrangler};

const DEV_TOKEN: &str = "dev";
/// The harness name the runner advertises for the stub `opencode`; the thread's `agent`.
const AGENT_ID: &str = "opencode";
const AGENT_DIR_ENV: &str = "SM_ATTACHMENTS_E2E_DIR";
/// The runner's attachment cap for this check (bytes).
const CAP_BYTES: u64 = 1024;
const WAIT: Duration = Duration::from_secs(60);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// The agent's script: one turn, one text step, then `end_turn`.
const SCRIPT: &str = r#"{"turns":[{"steps":[{"text":"ok"}]}]}"#;

/// The PNG magic and an IHDR header. The runner checks the magic bytes of an image.
const PNG: &[u8] = &[
    0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0, 0, 0, 0x0d, b'I', b'H', b'D', b'R', 0, 0,
    0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89,
];
const TEXT: &[u8] = b"Shipping notes for the phone attachment path.\n";

static KEYS: AtomicU64 = AtomicU64::new(0);

/// Entry point for `cli e2e attachments`.
pub async fn run() -> Result<()> {
    let started = Instant::now();
    let work = TempDir::create()?;
    let result = tokio::select! {
        result = attachments(&work) => result,
        () = tracer::interrupted() => Err(anyhow!("interrupted; stopping wrangler")),
    };
    match &result {
        Ok(()) => println!(
            "PASS e2e attachments in {:.1}s",
            started.elapsed().as_secs_f64()
        ),
        Err(_) => eprintln!(
            "FAIL e2e attachments after {:.1}s",
            started.elapsed().as_secs_f64()
        ),
    }
    result
}

/// The agent process: `e2e attachments-agent`, started by the stub harness. Not a check.
pub async fn agent() -> Result<()> {
    let dir = PathBuf::from(
        std::env::var_os(AGENT_DIR_ENV).ok_or_else(|| anyhow!("{AGENT_DIR_ENV} is not set"))?,
    );
    let script = acp_mock::Script::from_json(SCRIPT).context("attachments acp-mock script")?;
    let recorder = acp_mock::Recorder::new();
    let dumper = {
        let recorder = recorder.clone();
        let dump = dir.join("recorded-1.json");
        tokio::spawn(async move {
            loop {
                let _ = fs::write(&dump, Value::Array(recorder.messages()).to_string());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };
    let result = acp_mock::run_recorded(script, recorder, tokio::io::stdin(), tokio::io::stdout())
        .await
        .context("acp-mock agent");
    dumper.abort();
    result
}

async fn attachments(work: &TempDir) -> Result<()> {
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
    let checked = exercise(&base, work).await;
    let stopped = wrangler.stop();
    checked?;
    stopped?;
    println!("wrangler stopped");
    Ok(())
}

/// Starts the machine, the runner and the checks, then stops the runner.
async fn exercise(base: &str, work: &TempDir) -> Result<()> {
    println!("== machine and credential (dev routes)");
    let machine_id = tracer::create_machine(base).await?;
    let credential = mint_credential(base, &machine_id).await?;

    let root = &work.path;
    let state = root.join("state");
    let home = root.join("home");
    let bin = root.join("bin");
    let mock = root.join("mock");
    let project = root.join("project");
    for dir in [&state, &home, &bin, &mock, &project] {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    fs::write(mock.join("script.json"), SCRIPT)?;
    // The agent runs a copy of this binary: the target directory is shared, and a rebuild of
    // `cli` elsewhere replaces the file at `current_exe()` mid-run. `/proc/self/exe` still opens
    // the running image after that.
    let agent_dir = root.join("agent");
    fs::create_dir_all(&agent_dir)?;
    let agent_exe = agent_dir.join("cli");
    let source = if Path::new("/proc/self/exe").exists() {
        PathBuf::from("/proc/self/exe")
    } else {
        std::env::current_exe().context("current exe")?
    };
    fs::copy(source, &agent_exe).context("copy the agent binary")?;
    write_stub(&bin, &mock, &root.join("agent.log"), &agent_exe)?;
    let credentials = root.join("credential.json");
    CredentialStore::file_only(credentials.clone()).save(&Login {
        hub: base.to_owned(),
        machine_id: machine_id.clone(),
        credential,
    })?;
    TrustStore::new(&state, &home).trust(&project)?;

    let options = Options {
        credentials: CredentialStore::file_only(credentials),
        state_dir: state,
        cwd: project.clone(),
        home,
        path_env: OsString::from(bin),
        spawn: None,
        capacity: 32,
        permission_mode: PermissionMode::Default,
        name_prefix: "e2e".to_owned(),
        continue_last: false,
        idle_timeout: Duration::from_secs(3600),
        timing: Timing {
            backoff_base: Duration::from_millis(20),
            backoff_cap: Duration::from_millis(200),
            stable_after: Duration::from_secs(3600),
            welcome_timeout: Duration::from_secs(5),
        },
        signin: None,
        asker: None,
        allow_remote_terminal: false,
    };
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let runner = tokio::spawn(server::run_with_file_policy(
        options,
        FilePolicy::loopback_for_tests(CAP_BYTES),
        async {
            let _ = stop_rx.await;
        },
    ));

    let checked = checks(base, &machine_id, &project, &mock).await;
    let _ = stop_tx.send(());
    let stopped = match tokio::time::timeout(STOP_TIMEOUT, runner).await {
        Ok(Ok(result)) => result,
        Ok(Err(join)) => Err(anyhow!("runner task: {join}")),
        Err(_) => Err(anyhow!("runner did not stop")),
    };
    checked?;
    stopped
}

async fn checks(base: &str, machine_id: &str, project: &Path, mock: &Path) -> Result<()> {
    let api = format!("{base}/v1");
    let attachments_dir = project.join(".sm/attachments");

    // Check 1: a PNG and a text file in one prompt.
    println!("== 1. PNG and text file in one prompt");
    let png = upload(base, "photo.png", "image/png", PNG).await?;
    let notes = upload(base, "notes.txt", "text/plain", TEXT).await?;
    let thread = create_thread(
        &api,
        machine_id,
        project,
        "tk_e2e_ok",
        vec![part(&png)?, part(&notes)?],
    )
    .await?;
    let mut subscription = Subscription::connect(
        &api,
        ThreadSync::new(thread.clone()),
        Some(DEV_TOKEN.to_owned()),
    )
    .await
    .context("subscribe to the thread")?;
    let outcome = wait_for_task(&mut subscription, "tk_e2e_ok").await?;
    ensure!(
        matches!(outcome, Outcome::Finished),
        "the PNG and text prompt did not finish: {outcome:?}"
    );

    let prompts = wait_for_prompts(mock, 1).await?;
    let blocks = prompts[0]["params"]["prompt"]
        .as_array()
        .context("the prompt has no content blocks")?;
    let image_ok = blocks.iter().any(|block| {
        block["type"] == "image"
            && block["mimeType"] == "image/png"
            && block["data"] == STANDARD.encode(PNG)
    });
    ensure!(
        image_ok,
        "the agent got no image block with the PNG: {blocks:?}"
    );
    let reference_ok = blocks
        .iter()
        .any(|block| block["type"] == "text" && block["text"] == "@.sm/attachments/notes.txt");
    ensure!(
        reference_ok,
        "the agent got no @.sm/attachments/notes.txt reference: {blocks:?}"
    );
    println!(
        "agent received: image/png block ({} bytes) and @.sm/attachments/notes.txt",
        PNG.len()
    );

    let written = fs::read(attachments_dir.join("notes.txt"))
        .context("the text file is not in the workspace")?;
    ensure!(
        sha256_hex(&written) == sha256_hex(TEXT),
        "the workspace file's sha256 differs from the uploaded one"
    );
    println!(
        "workspace .sm/attachments/notes.txt has the uploaded sha256 {}",
        sha256_hex(TEXT)
    );

    // Check 2: three attachments that must each fail the task before the agent sees it.
    println!("== 2. cross-org, tampered and oversize attachments");
    let cross = upload(base, "cross.txt", "text/plain", TEXT).await?;
    let tampered = upload(base, "tampered.txt", "text/plain", TEXT).await?;
    let oversize = upload(base, "big.txt", "text/plain", &vec![b'x'; 2048]).await?;

    let cross_url = with_other_org(string_of(&part(&cross)?, "url")?)?;
    let tampered_url = with_flipped_signature(string_of(&part(&tampered)?, "url")?)?;
    let cases = [
        ("tk_e2e_cross_org", cross_url, &cross),
        ("tk_e2e_tampered", tampered_url, &tampered),
        (
            "tk_e2e_oversize",
            string_of(&part(&oversize)?, "url")?.to_owned(),
            &oversize,
        ),
    ];
    for (task, url, answer) in cases {
        let mut file = part(answer)?;
        file["url"] = Value::String(url);
        send_prompt(&api, &thread, task, vec![file]).await?;
        match wait_for_task(&mut subscription, task).await? {
            Outcome::Failed { code, message } => {
                ensure!(
                    code.as_deref() == Some("attachment"),
                    "{task} failed with code {code:?}, expected attachment"
                );
                println!("{task}: RunError code attachment ({message})");
            }
            Outcome::Finished => bail_finished(task)?,
        }
    }

    // The agent got only the first prompt; the three failed tasks never reached it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let prompts = recorded_prompts(mock);
    ensure!(
        prompts.len() == 1,
        "the agent received {} prompts, expected 1",
        prompts.len()
    );
    let names = listing(&attachments_dir)?;
    ensure!(
        names == ["notes.txt"],
        ".sm/attachments holds {names:?}, expected only notes.txt"
    );
    println!("agent received 1 prompt; .sm/attachments holds only notes.txt");
    Ok(())
}

#[derive(Debug)]
enum Outcome {
    Finished,
    Failed {
        code: Option<String>,
        message: String,
    },
}

fn bail_finished(task: &str) -> Result<()> {
    Err(anyhow!(
        "{task} finished; its attachment should have failed the task"
    ))
}

/// Reads the thread's events until the task of `task` ends.
async fn wait_for_task(subscription: &mut Subscription, task: &str) -> Result<Outcome> {
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow!("{task} did not end"))?;
        let applied = tokio::time::timeout(remaining, subscription.next())
            .await
            .map_err(|_| anyhow!("{task} did not end"))?
            .context("subscription")?;
        let Applied::Event(envelope) = applied else {
            continue;
        };
        if envelope.task_id.as_deref() != Some(task) {
            continue;
        }
        match &envelope.event {
            Event::RunFinished(_) => return Ok(Outcome::Finished),
            Event::RunError(error) => {
                return Ok(Outcome::Failed {
                    code: error.code.clone(),
                    message: error.message.clone(),
                });
            }
            _ => {}
        }
    }
}

/// `POST /v1/attachments` then the PUT of `bytes` to the signed `upload_url`. The answer's
/// `part` is the FilePart a prompt carries.
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
    let upload_url = string_of(&answer, "upload_url")?;
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
    Ok(answer)
}

/// `POST /v1/threads` with the first prompt; the thread's id.
async fn create_thread(
    api: &str,
    machine_id: &str,
    project: &Path,
    task: &str,
    parts: Vec<Value>,
) -> Result<String> {
    let body = json!({
        "machine_id": machine_id,
        "agent": AGENT_ID,
        "workspace": { "path": project.to_string_lossy(), "spawn": "same_dir" },
        "permission_mode": "default",
        "title": "attachments e2e",
        "prompt": { "task_id": task, "message_id": format!("msg_{task}"), "parts": parts },
    });
    let response = reqwest::Client::new()
        .post(format!("{api}/threads"))
        .bearer_auth(DEV_TOKEN)
        .header("Idempotency-Key", new_key("thread"))
        .json(&body)
        .send()
        .await
        .context("POST /v1/threads")?;
    let status = response.status();
    let answer: Value = response.json().await.context("thread answer")?;
    ensure!(
        status.as_u16() == 201,
        "POST /v1/threads answered HTTP {status}: {answer}"
    );
    Ok(string_of(&answer, "thread_id")?.to_owned())
}

/// `POST /v1/threads/{id}/prompts` (client-api.md §5.3).
async fn send_prompt(api: &str, thread: &str, task: &str, parts: Vec<Value>) -> Result<()> {
    let response = reqwest::Client::new()
        .post(format!("{api}/threads/{thread}/prompts"))
        .bearer_auth(DEV_TOKEN)
        .header("Idempotency-Key", new_key("prompt"))
        .json(&json!({ "task_id": task, "message_id": format!("msg_{task}"), "parts": parts }))
        .send()
        .await
        .context("POST prompts")?;
    let status = response.status();
    ensure!(
        status.as_u16() == 202,
        "POST prompts for {task} answered HTTP {status}"
    );
    Ok(())
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
    Ok(string_of(&answer, "credential")?.to_owned())
}

/// The FilePart of an upload answer.
fn part(answer: &Value) -> Result<Value> {
    answer
        .get("part")
        .cloned()
        .ok_or_else(|| anyhow!("the attachment answer has no part: {answer}"))
}

fn string_of<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("no {key} in {value}"))
}

/// The URL with its org segment replaced by another org. The signature covers the org, so the
/// hub must refuse the URL.
fn with_other_org(url: &str) -> Result<String> {
    let marker = "/v1/attachments/";
    let rest = url
        .split_once(marker)
        .map(|(_, rest)| rest)
        .ok_or_else(|| anyhow!("not an attachment URL"))?;
    let org = rest.split('/').next().unwrap_or_default();
    Ok(url.replacen(
        &format!("{marker}{org}/"),
        &format!("{marker}org_someone_else/"),
        1,
    ))
}

/// The URL with its last signature character changed.
fn with_flipped_signature(url: &str) -> Result<String> {
    let mut chars: Vec<char> = url.chars().collect();
    let last = chars.last_mut().ok_or_else(|| anyhow!("empty URL"))?;
    *last = if *last == '0' { '1' } else { '0' };
    Ok(chars.into_iter().collect())
}

/// The prompts the agent received, from its recorder dump.
fn recorded_prompts(mock: &Path) -> Vec<Value> {
    fs::read_to_string(mock.join("recorded-1.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<Value>>(&text).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|message| message["method"] == "session/prompt")
        .collect()
}

/// Waits until the agent's dump holds `count` prompts.
async fn wait_for_prompts(mock: &Path, count: usize) -> Result<Vec<Value>> {
    let deadline = Instant::now() + WAIT;
    loop {
        let prompts = recorded_prompts(mock);
        if prompts.len() >= count {
            return Ok(prompts);
        }
        ensure!(
            Instant::now() < deadline,
            "the agent received {} prompts, expected {count}",
            prompts.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The names in `dir`, sorted.
fn listing(dir: &Path) -> Result<Vec<String>> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Ok(names)
}

/// The stub `opencode`: runs this binary as the agent, with the mock directory in the
/// environment and its stderr in `log`.
fn write_stub(bin: &Path, mock: &Path, log: &Path, exe: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let stub = bin.join("opencode");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\n{AGENT_DIR_ENV}='{}' exec '{}' e2e attachments-agent 2>>'{}'\n",
            mock.display(),
            exe.display(),
            log.display()
        ),
    )?;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755))?;
    Ok(())
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

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A unique `Idempotency-Key` value.
fn new_key(what: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "idk_e2e_{what}_{nanos}_{}",
        KEYS.fetch_add(1, Ordering::Relaxed)
    )
}
