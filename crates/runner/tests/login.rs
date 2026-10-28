//! `sm login` against an in-process fake hub (a tiny HTTP server on tokio): the device-code flow
//! (pending -> approved, slow_down backoff, expired, denied), the credential store (keychain,
//! `0600` file fallback, the forced file), `KeychainCredentials` as the tunnel's credential
//! source, and the `sm login` binary end to end.

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use runner::login::flow::normalize_hub;
use runner::login::{
    Backend, CredentialStore, Keychain, KeychainCredentials, Login, LoginError, Sleeper, login,
};
use runner::tunnel::CredentialSource;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CREDENTIAL: &str = "smm_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const DEVICE_CODE: &str = "sdc_secretdevicecode";

// ---- fake hub -------------------------------------------------------------------------------

#[derive(Default)]
struct HubState {
    /// Answers for `POST /v1/devices/token`, in order. The last one repeats.
    token_replies: VecDeque<(u16, Value)>,
    /// `(method, path, body)` of every request.
    requests: Vec<(String, String, Value)>,
    interval: u64,
}

struct FakeHub {
    url: String,
    state: Arc<Mutex<HubState>>,
}

impl FakeHub {
    async fn start(token_replies: Vec<(u16, Value)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let state = Arc::new(Mutex::new(HubState {
            token_replies: token_replies.into(),
            interval: 5,
            ..HubState::default()
        }));
        let shared = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve(stream, Arc::clone(&shared)));
            }
        });
        Self { url, state }
    }

    fn with_interval(self, seconds: u64) -> Self {
        self.state.lock().expect("state").interval = seconds;
        self
    }

    fn requests(&self) -> Vec<(String, String, Value)> {
        self.state.lock().expect("state").requests.clone()
    }
}

async fn serve(mut stream: tokio::net::TcpStream, state: Arc<Mutex<HubState>>) {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head_end, length) = loop {
        let n = stream.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        raw.extend_from_slice(&chunk[..n]);
        if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&raw[..at]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (at + 4, length);
        }
    };
    while raw.len() < head_end + length {
        let n = stream.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        raw.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let mut first = head.lines().next().unwrap_or_default().split_whitespace();
    let (method, path) = (
        first.next().unwrap_or_default().to_owned(),
        first.next().unwrap_or_default().to_owned(),
    );
    let body: Value =
        serde_json::from_slice(&raw[head_end..head_end + length]).unwrap_or(Value::Null);

    let (status, reply) = {
        let mut state = state.lock().expect("state");
        state
            .requests
            .push((method.clone(), path.clone(), body.clone()));
        match (method.as_str(), path.as_str()) {
            ("POST", "/v1/devices/code") => (
                200,
                json!({
                    "device_code": DEVICE_CODE,
                    "user_code": "WXYZ-2345",
                    "verification_uri": "https://hub.example/device",
                    "verification_uri_complete": "https://hub.example/device?user_code=WXYZ-2345",
                    "expires_in": 600,
                    "interval": state.interval,
                }),
            ),
            ("POST", "/v1/devices/token") => {
                if state.token_replies.len() > 1 {
                    state.token_replies.pop_front().expect("reply")
                } else {
                    state.token_replies.front().cloned().expect("a reply")
                }
            }
            _ => (404, json!({ "error": "not_found" })),
        }
    };
    let text = reply.to_string();
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{text}",
        text.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn pending() -> (u16, Value) {
    (400, json!({ "error": "authorization_pending" }))
}

fn slow_down(interval: Option<u64>) -> (u16, Value) {
    match interval {
        Some(interval) => (400, json!({ "error": "slow_down", "interval": interval })),
        None => (400, json!({ "error": "slow_down" })),
    }
}

fn granted() -> (u16, Value) {
    (
        200,
        json!({ "machine_id": "mch_abc", "credential": CREDENTIAL, "org_id": "org_1", "name": "laptop" }),
    )
}

// ---- helpers --------------------------------------------------------------------------------

/// A sleeper that records the requested durations and returns at once.
fn recording_sleeper() -> (Sleeper, Arc<Mutex<Vec<Duration>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&log);
    let sleeper: Sleeper = Arc::new(move |duration| {
        seen.lock().expect("log").push(duration);
        Box::pin(async {})
    });
    (sleeper, log)
}

fn secs(log: &Arc<Mutex<Vec<Duration>>>) -> Vec<u64> {
    log.lock()
        .expect("log")
        .iter()
        .map(Duration::as_secs)
        .collect()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

fn file_store(dir: &TempDir) -> CredentialStore {
    CredentialStore::file_only(dir.path().join("sm").join("credential.json"))
}

#[derive(Default)]
struct MemoryKeychain {
    secret: Mutex<Option<String>>,
    fail: bool,
}

/// The keychain handle the store owns; the test keeps the `Arc` to inspect it.
struct Shared(Arc<MemoryKeychain>);

impl Keychain for Shared {
    fn get(&self) -> Result<Option<String>> {
        if self.0.fail {
            return Err(anyhow!("keychain locked"));
        }
        Ok(self.0.secret.lock().expect("secret").clone())
    }

    fn set(&self, secret: &str) -> Result<()> {
        if self.0.fail {
            return Err(anyhow!("keychain locked"));
        }
        *self.0.secret.lock().expect("secret") = Some(secret.to_owned());
        Ok(())
    }

    fn delete(&self) -> Result<()> {
        if self.0.fail {
            return Err(anyhow!("keychain locked"));
        }
        *self.0.secret.lock().expect("secret") = None;
        Ok(())
    }
}

fn sample_login() -> Login {
    Login {
        hub: "https://hub.example".to_owned(),
        machine_id: "mch_abc".to_owned(),
        credential: CREDENTIAL.to_owned(),
    }
}

// ---- the flow -------------------------------------------------------------------------------

#[tokio::test]
async fn full_flow_pending_then_approved() {
    let hub = FakeHub::start(vec![pending(), pending(), granted()]).await;
    let dir = TempDir::new().expect("dir");
    let store = file_store(&dir);
    let (sleeper, log) = recording_sleeper();
    let mut out = Vec::new();

    let done = login(&format!("{}/", hub.url), &store, &mut out, &sleeper)
        .await
        .expect("login");

    assert_eq!(done.machine_id, "mch_abc");
    assert_eq!(done.credential, CREDENTIAL);
    assert_eq!(secs(&log), [5, 5, 5], "the server's interval is honoured");

    // The person sees the user code, the verification URL and a QR; never the credential.
    let shown = String::from_utf8(out).expect("utf8");
    assert!(shown.contains("WXYZ-2345"), "{shown}");
    assert!(shown.contains("https://hub.example/device"), "{shown}");
    assert!(
        shown.contains('\u{2588}') || shown.contains('\u{2580}') || shown.contains('\u{2584}'),
        "no terminal QR in:\n{shown}"
    );
    assert!(!shown.contains(CREDENTIAL) && !shown.contains(DEVICE_CODE));

    // The device code travels in the JSON body of the poll, never in a URL.
    let requests = hub.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].1, "/v1/devices/code");
    for (_, path, body) in &requests[1..] {
        assert_eq!(path, "/v1/devices/token");
        assert_eq!(body["device_code"], DEVICE_CODE);
    }
    assert!(requests.iter().all(|(_, path, _)| !path.contains("sdc_")));

    // Stored, and read back.
    assert_eq!(store.load().expect("load"), Some(done));
}

#[tokio::test]
async fn slow_down_backs_off_by_the_servers_interval_or_five_seconds() {
    let hub = FakeHub::start(vec![
        pending(),
        slow_down(Some(10)),
        slow_down(None),
        granted(),
    ])
    .await;
    let dir = TempDir::new().expect("dir");
    let (sleeper, log) = recording_sleeper();
    login(&hub.url, &file_store(&dir), &mut Vec::new(), &sleeper)
        .await
        .expect("login");
    // 5 (pending), 5 (-> slow_down{10}), 10 (-> slow_down, +5), 15 (granted).
    assert_eq!(secs(&log), [5, 5, 10, 15]);
}

#[tokio::test]
async fn expired_code_ends_the_login_and_stores_nothing() {
    let hub = FakeHub::start(vec![pending(), (400, json!({ "error": "expired_token" }))]).await;
    let dir = TempDir::new().expect("dir");
    let store = file_store(&dir);
    let (sleeper, _) = recording_sleeper();
    let error = login(&hub.url, &store, &mut Vec::new(), &sleeper)
        .await
        .expect_err("expired");
    assert_eq!(
        error.downcast_ref::<LoginError>(),
        Some(&LoginError::Expired)
    );
    assert_eq!(store.load().expect("load"), None);
    assert!(!store.file_path().exists());
}

#[tokio::test]
async fn a_hub_that_never_expires_the_code_is_given_up_on() {
    // Pending forever: the runner stops once the advertised lifetime is spent.
    let hub = FakeHub::start(vec![pending()]).await;
    let dir = TempDir::new().expect("dir");
    let (sleeper, log) = recording_sleeper();
    let error = login(&hub.url, &file_store(&dir), &mut Vec::new(), &sleeper)
        .await
        .expect_err("gives up");
    assert_eq!(
        error.downcast_ref::<LoginError>(),
        Some(&LoginError::Expired)
    );
    assert!(log.lock().expect("log").len() >= 120, "600 s at 5 s");
}

#[tokio::test]
async fn denied_pairing_ends_the_login_and_stores_nothing() {
    let hub = FakeHub::start(vec![(400, json!({ "error": "access_denied" }))]).await;
    let dir = TempDir::new().expect("dir");
    let store = file_store(&dir);
    let (sleeper, _) = recording_sleeper();
    let error = login(&hub.url, &store, &mut Vec::new(), &sleeper)
        .await
        .expect_err("denied");
    assert_eq!(
        error.downcast_ref::<LoginError>(),
        Some(&LoginError::Denied)
    );
    assert_eq!(store.load().expect("load"), None);
}

#[test]
fn plain_http_is_for_loopback_hubs_only() {
    assert_eq!(
        normalize_hub("https://hub.example/").expect("https"),
        "https://hub.example"
    );
    assert_eq!(
        normalize_hub("http://127.0.0.1:8787").expect("loopback"),
        "http://127.0.0.1:8787"
    );
    assert!(normalize_hub("http://localhost:8787").is_ok());
    assert!(normalize_hub("http://hub.example").is_err());
    assert!(normalize_hub("hub.example").is_err());
    assert!(normalize_hub("https://").is_err());
}

// ---- the store ------------------------------------------------------------------------------

#[test]
fn the_file_fallback_is_0600_and_used_when_the_keychain_fails() {
    let dir = TempDir::new().expect("dir");
    let path = dir.path().join("config").join("credential.json");
    let keychain = Arc::new(MemoryKeychain {
        fail: true,
        ..MemoryKeychain::default()
    });
    let store = CredentialStore::with_keychain(Box::new(Shared(keychain)), path.clone());

    let backend = store.save(&sample_login()).expect("save");
    assert_eq!(backend, Backend::File(path.clone()));
    assert_eq!(mode(&path), 0o600, "the credential file must be private");
    assert_eq!(
        mode(path.parent().expect("parent")),
        0o700,
        "and so must its directory"
    );
    assert_eq!(store.load().expect("load"), Some(sample_login()));

    // Saving again replaces the file and keeps the mode.
    let mut other = sample_login();
    other.machine_id = "mch_other".to_owned();
    store.save(&other).expect("save again");
    assert_eq!(mode(&path), 0o600);
    assert_eq!(store.load().expect("load"), Some(other));
    // No temp file is left behind.
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(leftovers.len(), 1, "{leftovers:?}");
}

#[test]
fn a_working_keychain_holds_the_secret_and_no_file_is_written() {
    let dir = TempDir::new().expect("dir");
    let path = dir.path().join("credential.json");
    // A stale file from an earlier fallback must not outlive a keychain save.
    std::fs::write(&path, "stale").expect("stale file");
    let keychain = Arc::new(MemoryKeychain::default());
    let store =
        CredentialStore::with_keychain(Box::new(Shared(Arc::clone(&keychain))), path.clone());

    assert_eq!(
        store.save(&sample_login()).expect("save"),
        Backend::Keychain
    );
    assert!(!path.exists());
    assert!(keychain.secret.lock().expect("secret").is_some());
    assert_eq!(store.load().expect("load"), Some(sample_login()));

    store.wipe().expect("wipe");
    assert_eq!(store.load().expect("load"), None);
    assert!(keychain.secret.lock().expect("secret").is_none());
}

// ---- the tunnel's credential source ----------------------------------------------------------

#[tokio::test]
async fn keychain_credentials_read_on_each_connect_and_wipe_on_revoke() {
    let dir = TempDir::new().expect("dir");
    let store = file_store(&dir);
    let path = store.file_path().to_path_buf();
    let credentials = KeychainCredentials::new(file_store(&dir));

    // Not logged in yet.
    let error = credentials.credential().await.expect_err("no login");
    assert!(error.to_string().contains("sm login"), "{error}");

    store.save(&sample_login()).expect("save");
    assert_eq!(credentials.credential().await.expect("cred"), CREDENTIAL);

    // A re-login (a rotated credential) is picked up by the next connect.
    let mut rotated = sample_login();
    rotated.credential = "smm_rotated".to_owned();
    store.save(&rotated).expect("save");
    assert_eq!(credentials.credential().await.expect("cred"), "smm_rotated");

    // auth_revoked: the client calls wipe(); the credential is gone from the store and the file.
    credentials.wipe();
    assert!(!path.exists(), "the file is removed");
    assert_eq!(store.load().expect("load"), None);
    assert!(credentials.credential().await.is_err());
    // Wiping twice is harmless.
    credentials.wipe();
}

#[tokio::test]
async fn wipe_clears_a_keychain_credential_too() {
    let dir = TempDir::new().expect("dir");
    let keychain = Arc::new(MemoryKeychain::default());
    let store = CredentialStore::with_keychain(
        Box::new(Shared(Arc::clone(&keychain))),
        dir.path().join("credential.json"),
    );
    store.save(&sample_login()).expect("save");
    let credentials = KeychainCredentials::new(store);
    assert_eq!(credentials.credential().await.expect("cred"), CREDENTIAL);
    credentials.wipe();
    assert!(keychain.secret.lock().expect("secret").is_none());
    assert!(credentials.credential().await.is_err());
}

// ---- the binary -----------------------------------------------------------------------------

#[tokio::test]
async fn sm_login_runs_end_to_end_with_the_forced_credential_file() {
    let hub = FakeHub::start(vec![granted()]).await.with_interval(1);
    let dir = TempDir::new().expect("dir");
    let file = dir.path().join("forced").join("cred.json");

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_sm"))
        .args(["login", "--hub", &hub.url])
        .env("SM_CREDENTIAL_FILE", &file)
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .await
        .expect("run sm");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "sm login failed: {stdout}\n{stderr}"
    );
    assert!(stdout.contains("WXYZ-2345"), "{stdout}");
    assert!(!stdout.contains(CREDENTIAL) && !stderr.contains(CREDENTIAL));

    // The env var forced the file: 0600, holding the credential, nothing in $HOME/.config.
    assert_eq!(mode(&file), 0o600);
    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(&file).expect("read")).expect("stored JSON");
    assert_eq!(stored["credential"], CREDENTIAL);
    assert_eq!(stored["machine_id"], "mch_abc");
    assert!(!dir.path().join(".config").exists());
}

#[tokio::test]
async fn sm_login_without_a_hub_prints_usage() {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_sm"))
        .args(["login"])
        .output()
        .await
        .expect("run sm");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage: sm login --hub"));
}
