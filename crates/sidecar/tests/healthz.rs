//! Tests for the sidecar `/healthz` server and env configuration.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use sidecar::config::{Config, ConfigError, MachineCredential, init_tracing};
use sidecar::server::serve;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// Minimal HTTP/1.1 GET over a raw socket. Returns (status code, body).
async fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read");
    let text = String::from_utf8(raw).expect("utf8 response");
    let (head, body) = text.split_once("\r\n\r\n").expect("header/body split");
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("status code");
    (status, body.to_owned())
}

#[tokio::test]
async fn healthz_returns_build_info() {
    let handle = serve("127.0.0.1:0".parse().unwrap()).await.expect("serve");
    let (status, body) = http_get(handle.local_addr(), "/healthz").await;
    assert_eq!(status, 200);

    let json: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(json["name"], "sidecar");
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        json["git_sha"],
        option_env!("SM_GIT_SHA").unwrap_or("unknown")
    );
    handle.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn shutdown_stops_listening() {
    let handle = serve("127.0.0.1:0".parse().unwrap()).await.expect("serve");
    let addr = handle.local_addr();
    assert_ne!(addr.port(), 0, "port 0 must resolve to a real port");
    handle.shutdown().await.expect("shutdown");
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "server still accepts connections after shutdown"
    );
}

#[test]
fn defaults_apply_when_env_is_minimal() {
    let cfg = Config::from_map(&vars(&[("HOME", "/home/u")])).expect("config");
    assert_eq!(cfg.hub_url, None);
    assert_eq!(cfg.machine_credential, None);
    assert_eq!(cfg.log_filter, "info");
    assert_eq!(
        cfg.healthz_addr,
        "127.0.0.1:0".parse::<SocketAddr>().unwrap()
    );
    assert_eq!(cfg.state_dir, PathBuf::from("/home/u/.local/share/sm"));
}

#[test]
fn all_variables_are_read() {
    let cfg = Config::from_map(&vars(&[
        ("SM_HUB_URL", "wss://hub.example.com"),
        ("SM_STATE_DIR", "/var/lib/sm-state"),
        ("SM_LOG", "debug,sidecar=trace"),
        ("SM_HEALTHZ_ADDR", "127.0.0.1:9911"),
    ]))
    .expect("config");
    assert_eq!(cfg.hub_url.as_deref(), Some("wss://hub.example.com"));
    assert_eq!(cfg.state_dir, PathBuf::from("/var/lib/sm-state"));
    assert_eq!(cfg.log_filter, "debug,sidecar=trace");
    assert_eq!(
        cfg.healthz_addr,
        "127.0.0.1:9911".parse::<SocketAddr>().unwrap()
    );
}

#[test]
fn state_dir_prefers_absolute_xdg_data_home() {
    let xdg = Config::from_map(&vars(&[("HOME", "/home/u"), ("XDG_DATA_HOME", "/data")]))
        .expect("config");
    assert_eq!(xdg.state_dir, PathBuf::from("/data/sm"));

    let relative =
        Config::from_map(&vars(&[("HOME", "/home/u"), ("XDG_DATA_HOME", "rel")])).expect("config");
    assert_eq!(relative.state_dir, PathBuf::from("/home/u/.local/share/sm"));
}

#[test]
fn no_state_dir_and_no_home_is_an_error() {
    assert_eq!(Config::from_map(&vars(&[])), Err(ConfigError::NoStateDir));
}

#[test]
fn bad_healthz_addr_is_an_error() {
    let err = Config::from_map(&vars(&[("HOME", "/h"), ("SM_HEALTHZ_ADDR", "localhost")]))
        .expect_err("hostname without port must be rejected");
    assert!(
        matches!(err, ConfigError::InvalidHealthzAddr { .. }),
        "{err:?}"
    );
}

#[test]
fn machine_credential_is_read_and_redacted_in_debug() {
    let secret = "tok-SECRET-123";
    let cfg = Config::from_map(&vars(&[("HOME", "/h"), ("SM_MACHINE_CREDENTIAL", secret)]))
        .expect("config");

    let cred = cfg.machine_credential.clone().expect("credential present");
    assert_eq!(cred.expose(), secret);
    assert_eq!(format!("{cred:?}"), "<redacted>");

    let debug = format!("{cfg:?}");
    assert!(debug.contains("<redacted>"), "{debug}");
    assert!(
        !debug.contains(secret),
        "credential leaked into Debug: {debug}"
    );
    assert_eq!(
        format!("{:?}", MachineCredential::new(secret)),
        "<redacted>"
    );
}

#[test]
fn empty_values_count_as_unset() {
    let cfg = Config::from_map(&vars(&[
        ("HOME", "/h"),
        ("SM_MACHINE_CREDENTIAL", ""),
        ("SM_HUB_URL", ""),
        ("SM_LOG", ""),
    ]))
    .expect("config");
    assert_eq!(cfg.machine_credential, None);
    assert_eq!(cfg.hub_url, None);
    assert_eq!(cfg.log_filter, "info");
}

#[test]
fn init_tracing_is_safe_to_call_twice() {
    let cfg = Config::from_map(&vars(&[("HOME", "/h")])).expect("config");
    init_tracing(&cfg).expect("first call");
    init_tracing(&cfg).expect("second call");
}

#[test]
fn init_tracing_rejects_invalid_filter() {
    let cfg = Config::from_map(&vars(&[("HOME", "/h"), ("SM_LOG", "sidecar=bogus_level")]))
        .expect("config");
    assert!(init_tracing(&cfg).is_err());
}
