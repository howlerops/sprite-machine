//! M0-008b: the hub's Rust side of the Workspace DO, native (no Worker, no wrangler).
//!
//! - Wire types round-trip the JSON of `workers/workspace/src/types.ts` (literals below are the
//!   shapes the TS tests and `core.ts` assert).
//! - `decode_response` maps the `Result<T>` envelope and HTTP statuses the way `index.ts` does.
//! - `FakeWorkspace` runs the same state machine and error codes as `core.ts` over `FakeSandbox`.
//!
//! The real `WORKSPACE` binding (wasm32) is checked by `scripts/workspace-dev-test.sh`, which runs
//! both Workers under `wrangler dev`.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use hub::workspace_do::fake::FakeWorkspace;
use hub::workspace_do::{
    BackupHandle, BackupRequest, ErrorCode, ExecRequest, InstanceType, PreviewRequest,
    RestoreBackupRequest, RestoreRequest, SnapshotRequest, StartRequest, StatusRequest,
    StopRequest, WorkspaceControl, WorkspaceError, WorkspaceState, decode_response,
    error_from_status, is_rpc_method, valid_workspace_id,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

const IMG: &str = "registry.cloudflare.com/acct/workspace:1";
const SHA: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Runs a future that never waits on anything (the fake never pends).
fn block_on<F: Future>(fut: F) -> F::Output {
    let mut cx = Context::from_waker(Waker::noop());
    let mut fut = pin!(fut);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("the fake workspace must not pend"),
    }
}

/// Parses `literal`, deserialises it as `T`, serialises it back and checks the JSON is the same.
fn round_trip<T: Serialize + DeserializeOwned>(literal: &str) -> T {
    let expected: Value = serde_json::from_str(literal).expect("literal is JSON");
    let typed: T = serde_json::from_value(expected.clone())
        .unwrap_or_else(|e| panic!("cannot decode {literal}: {e}"));
    let back = serde_json::to_value(&typed).expect("encode");
    assert_eq!(back, expected, "round trip changed {literal}");
    typed
}

fn backup_handle_json(name: Option<&str>) -> Value {
    let mut handle = json!({
        "id": "bak-1",
        "dir": "/workspace",
        "size": 2048,
        "sha256": SHA,
        "format": "tar+zstd/1",
    });
    if let Some(name) = name {
        handle["name"] = json!(name);
    }
    handle
}

// ---- wire types ---------------------------------------------------------------------------------

#[test]
fn request_and_response_types_round_trip_the_ts_json() {
    round_trip::<StartRequest>(&format!(r#"{{"image":"{IMG}","instance":"standard-2"}}"#));
    round_trip::<StartRequest>(&format!(r#"{{"image":"{IMG}"}}"#));
    round_trip::<hub::workspace_do::StartResponse>(&format!(
        r#"{{"state":"running","image":"{IMG}","instance":"standard-4"}}"#
    ));
    round_trip::<hub::workspace_do::StopResponse>(r#"{"state":"stopped","wasRunning":false}"#);
    round_trip::<StopRequest>("{}");
    round_trip::<StatusRequest>("{}");
    round_trip::<hub::workspace_do::StatusResponse>(
        r#"{"state":"stopped","image":null,"instance":null,"startedAt":null,"lastSnapshotId":null}"#,
    );
    round_trip::<hub::workspace_do::StatusResponse>(&format!(
        r#"{{"state":"running","image":"{IMG}","instance":"standard-2","startedAt":1700000000000,"lastSnapshotId":"snap-1"}}"#
    ));
    round_trip::<SnapshotRequest>("{}");
    round_trip::<SnapshotRequest>(r#"{"name":"before-build"}"#);
    round_trip::<hub::workspace_do::SnapshotResponse>(r#"{"id":"snap-1","size":1024}"#);
    round_trip::<hub::workspace_do::SnapshotResponse>(
        r#"{"id":"snap-1","size":1024,"name":"before-build"}"#,
    );
    round_trip::<RestoreRequest>(r#"{"id":"snap-1"}"#);
    round_trip::<hub::workspace_do::RestoreResponse>(
        r#"{"state":"running","restoredFrom":"snap-1"}"#,
    );
    round_trip::<BackupRequest>(r#"{"dir":"/workspace","name":"turn-3"}"#);
    round_trip::<hub::workspace_do::BackupResponse>(
        &json!({ "handle": backup_handle_json(Some("turn-3")) }).to_string(),
    );
    round_trip::<RestoreBackupRequest>(
        &json!({ "handle": backup_handle_json(None), "dir": "/tmp/restore" }).to_string(),
    );
    round_trip::<hub::workspace_do::RestoreBackupResponse>(
        r#"{"restored":true,"dir":"/workspace"}"#,
    );
    round_trip::<ExecRequest>(r#"{"cmd":["echo","hi"],"env":{"A":"1"},"cwd":"/workspace"}"#);
    round_trip::<hub::workspace_do::ExecResponse>(r#"{"exitCode":0,"stdout":"hi\n","stderr":""}"#);
    round_trip::<PreviewRequest>(r#"{"port":8080}"#);
    round_trip::<hub::workspace_do::PreviewResponse>(r#"{"port":8080,"path":"/preview/8080/"}"#);
}

#[test]
fn instance_types_are_the_ts_strings_and_nothing_else() {
    for name in [
        "lite",
        "basic",
        "standard-1",
        "standard-2",
        "standard-3",
        "standard-4",
    ] {
        let parsed: InstanceType = serde_json::from_value(json!(name)).expect(name);
        assert_eq!(serde_json::to_value(parsed).expect("encode"), json!(name));
    }
    assert!(serde_json::from_value::<InstanceType>(json!("huge")).is_err());
}

#[test]
fn error_codes_are_the_ts_strings() {
    let table = [
        (ErrorCode::InvalidArgument, "invalid_argument", 400),
        (ErrorCode::InvalidState, "invalid_state", 409),
        (ErrorCode::NotFound, "not_found", 404),
        (ErrorCode::SandboxError, "sandbox_error", 502),
        (ErrorCode::Internal, "internal", 500),
    ];
    for (code, text, status) in table {
        assert_eq!(code.as_str(), text);
        assert_eq!(code.http_status(), status);
        assert_eq!(serde_json::to_value(code).expect("encode"), json!(text));
    }
}

// ---- Result envelope and HTTP statuses ----------------------------------------------------------

#[test]
fn ok_envelope_decodes_to_the_value() {
    let body = format!(
        r#"{{"ok":true,"value":{{"state":"running","image":"{IMG}","instance":"standard-2"}}}}"#
    );
    let value: hub::workspace_do::StartResponse = decode_response(200, &body).expect("ok");
    assert_eq!(value.state, WorkspaceState::Running);
    assert_eq!(value.image, IMG);
    assert_eq!(value.instance, InstanceType::Standard2);
}

#[test]
fn error_envelope_wins_over_the_status() {
    let body = r#"{"ok":false,"error":{"code":"invalid_state","message":"workspace is stopped; call start first"}}"#;
    let err = decode_response::<hub::workspace_do::ExecResponse>(409, body).expect_err("error");
    assert_eq!(
        err,
        WorkspaceError::new(
            ErrorCode::InvalidState,
            "workspace is stopped; call start first"
        )
    );
    assert_eq!(
        err.to_string(),
        "invalid_state: workspace is stopped; call start first"
    );
}

#[test]
fn a_body_that_is_not_an_envelope_maps_by_http_status() {
    for (status, code) in [
        (400, ErrorCode::InvalidArgument),
        (404, ErrorCode::NotFound),
        (409, ErrorCode::InvalidState),
        (502, ErrorCode::SandboxError),
        (500, ErrorCode::Internal),
        (503, ErrorCode::Internal),
    ] {
        let err = decode_response::<hub::workspace_do::StopResponse>(status, "upstream went away")
            .expect_err("not an envelope");
        assert_eq!(err.code, code, "status {status}");
        assert!(
            err.message.contains("upstream went away"),
            "{}",
            err.message
        );
    }
    assert_eq!(error_from_status(404, "not json").code, ErrorCode::NotFound);
}

// ---- the fake, mirroring core.ts ----------------------------------------------------------------

fn start_req(instance: Option<InstanceType>) -> StartRequest {
    StartRequest {
        image: IMG.to_owned(),
        instance,
    }
}

fn expect_err<T: std::fmt::Debug>(result: Result<T, WorkspaceError>) -> WorkspaceError {
    result.expect_err("expected an error")
}

#[test]
fn lifecycle_is_stopped_running_stopped() {
    let ws = FakeWorkspace::new();
    let st = block_on(ws.status(StatusRequest {})).expect("status");
    assert_eq!(
        serde_json::to_value(&st).expect("encode"),
        json!({"state":"stopped","image":null,"instance":null,"startedAt":null,"lastSnapshotId":null})
    );

    let started = block_on(ws.start(start_req(Some(InstanceType::Standard4)))).expect("start");
    assert_eq!(started.state, WorkspaceState::Running);
    assert_eq!(started.instance, InstanceType::Standard4);

    let st = block_on(ws.status(StatusRequest {})).expect("status");
    assert_eq!(st.state, WorkspaceState::Running);
    assert_eq!(st.image.as_deref(), Some(IMG));
    assert!(
        st.started_at.is_some(),
        "a running workspace has a start time"
    );

    let stopped = block_on(ws.stop(StopRequest {})).expect("stop");
    assert_eq!(stopped.state, WorkspaceState::Stopped);
    assert!(stopped.was_running);

    let st = block_on(ws.status(StatusRequest {})).expect("status");
    assert_eq!(st.state, WorkspaceState::Stopped);
    assert_eq!(st.started_at, None, "a stopped workspace has no start time");
    assert_eq!(
        st.image.as_deref(),
        Some(IMG),
        "the image is kept after stop"
    );
}

#[test]
fn start_defaults_the_instance_and_rejects_a_second_start() {
    let ws = FakeWorkspace::new();
    let started = block_on(ws.start(start_req(None))).expect("start");
    assert_eq!(started.instance, InstanceType::Standard2);

    let err = expect_err(block_on(ws.start(start_req(None))));
    assert_eq!(err.code, ErrorCode::InvalidState);
    assert_eq!(err.message, "workspace is already running");

    let err = expect_err(block_on(ws.start(StartRequest {
        image: String::new(),
        instance: None,
    })));
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert_eq!(err.message, "image must be a non-empty string");
}

#[test]
fn stop_when_stopped_reports_was_running_false() {
    let ws = FakeWorkspace::new();
    let stopped = block_on(ws.stop(StopRequest {})).expect("stop");
    assert_eq!(
        serde_json::to_value(&stopped).expect("encode"),
        json!({"state":"stopped","wasRunning":false})
    );
}

#[test]
fn stopped_workspace_refuses_running_only_calls_with_invalid_state() {
    let ws = FakeWorkspace::new();
    let cases = [
        expect_err(block_on(ws.exec(ExecRequest {
            cmd: vec!["echo".to_owned(), "hi".to_owned()],
            env: None,
            cwd: None,
        }))),
        expect_err(block_on(ws.snapshot(SnapshotRequest::default()))),
        expect_err(block_on(ws.backup(BackupRequest {
            dir: "/workspace".to_owned(),
            name: None,
        }))),
        expect_err(block_on(ws.preview(PreviewRequest { port: 3000 }))),
    ];
    for err in cases {
        assert_eq!(err.code, ErrorCode::InvalidState, "{err}");
        assert_eq!(err.message, "workspace is stopped; call start first");
    }
}

#[test]
fn snapshot_restore_and_status_track_the_last_snapshot() {
    let ws = FakeWorkspace::new();
    block_on(ws.start(start_req(None))).expect("start");
    let snap = block_on(ws.snapshot(SnapshotRequest {
        name: Some("before-build".to_owned()),
    }))
    .expect("snapshot");
    assert_eq!(snap.size, 1024);
    assert_eq!(snap.name.as_deref(), Some("before-build"));

    let st = block_on(ws.status(StatusRequest {})).expect("status");
    assert_eq!(st.last_snapshot_id.as_deref(), Some(snap.id.as_str()));

    block_on(ws.stop(StopRequest {})).expect("stop");
    let restored = block_on(ws.restore(RestoreRequest {
        id: snap.id.clone(),
    }))
    .expect("restore");
    assert_eq!(restored.state, WorkspaceState::Running);
    assert_eq!(restored.restored_from, snap.id);

    let err = expect_err(block_on(ws.restore(RestoreRequest {
        id: "snap-nope".to_owned(),
    })));
    assert_eq!(err.code, ErrorCode::NotFound);
    assert_eq!(err.message, "unknown snapshot snap-nope");
}

#[test]
fn backup_and_restore_backup_round_trip_the_handle() {
    let ws = FakeWorkspace::new();
    block_on(ws.start(start_req(None))).expect("start");
    let backup = block_on(ws.backup(BackupRequest {
        dir: "/workspace".to_owned(),
        name: Some("turn-3".to_owned()),
    }))
    .expect("backup");
    let handle: BackupHandle = backup.handle;
    assert_eq!(handle.dir, "/workspace");
    assert_eq!(handle.format, "tar+zstd/1");

    let restored = block_on(ws.restore_backup(RestoreBackupRequest {
        handle: handle.clone(),
        dir: Some("/tmp/restore".to_owned()),
    }))
    .expect("restoreBackup");
    assert!(restored.restored);
    assert_eq!(restored.dir, "/tmp/restore");

    let mut unknown = handle;
    unknown.id = "bak-nope".to_owned();
    let err = expect_err(block_on(ws.restore_backup(RestoreBackupRequest {
        handle: unknown,
        dir: None,
    })));
    assert_eq!(err.code, ErrorCode::NotFound);
}

#[test]
fn argument_checks_match_core_ts() {
    let ws = FakeWorkspace::new();
    block_on(ws.start(start_req(None))).expect("start");

    let relative = expect_err(block_on(ws.backup(BackupRequest {
        dir: "workspace".to_owned(),
        name: None,
    })));
    assert_eq!(relative.code, ErrorCode::InvalidArgument);
    assert_eq!(relative.message, "dir must be an absolute path");

    let dotdot = expect_err(block_on(ws.exec(ExecRequest {
        cmd: vec!["echo".to_owned()],
        env: None,
        cwd: Some("/workspace/../etc".to_owned()),
    })));
    assert_eq!(dotdot.code, ErrorCode::InvalidArgument);
    assert_eq!(dotdot.message, "cwd must not contain ..");

    let empty_cmd = expect_err(block_on(ws.exec(ExecRequest {
        cmd: Vec::new(),
        env: None,
        cwd: None,
    })));
    assert_eq!(empty_cmd.code, ErrorCode::InvalidArgument);

    let port = expect_err(block_on(ws.preview(PreviewRequest { port: 0 })));
    assert_eq!(port.code, ErrorCode::InvalidArgument);
    assert_eq!(port.message, "port must be an integer in 1..65535");
}

#[test]
fn exec_runs_the_fake_commands_only() {
    let ws = FakeWorkspace::new();
    block_on(ws.start(start_req(None))).expect("start");

    let echoed = block_on(ws.exec(ExecRequest {
        cmd: vec!["echo".to_owned(), "hi".to_owned()],
        env: None,
        cwd: None,
    }))
    .expect("exec echo");
    assert_eq!(echoed.exit_code, 0);
    assert_eq!(echoed.stdout, "hi\n");

    let fails = block_on(ws.exec(ExecRequest {
        cmd: vec!["false".to_owned()],
        env: None,
        cwd: None,
    }))
    .expect("exec false");
    assert_eq!(fails.exit_code, 1);

    let unknown = block_on(ws.exec(ExecRequest {
        cmd: vec!["ls".to_owned()],
        env: None,
        cwd: None,
    }))
    .expect("exec ls");
    assert_eq!(unknown.exit_code, 127);
    assert_eq!(unknown.stderr, "ls: command not found\n");
}

#[test]
fn preview_returns_the_proxy_path() {
    let ws = FakeWorkspace::new();
    block_on(ws.start(start_req(None))).expect("start");
    let preview = block_on(ws.preview(PreviewRequest { port: 3000 })).expect("preview");
    assert_eq!(preview.path, "/preview/3000/");
}

// ---- paths and ids -------------------------------------------------------------------------------

#[test]
fn rpc_method_names_and_workspace_ids_are_the_ts_ones() {
    for method in [
        "start",
        "stop",
        "status",
        "snapshot",
        "restore",
        "backup",
        "restoreBackup",
        "exec",
        "preview",
    ] {
        assert!(is_rpc_method(method), "{method}");
    }
    assert!(!is_rpc_method("restore_backup"));
    assert!(!is_rpc_method("nosuchmethod"));

    assert!(valid_workspace_id("ws-1.prod_2"));
    assert!(valid_workspace_id(&"a".repeat(128)));
    assert!(!valid_workspace_id(""));
    assert!(!valid_workspace_id(&"a".repeat(129)));
    assert!(!valid_workspace_id("has/slash"));
}
