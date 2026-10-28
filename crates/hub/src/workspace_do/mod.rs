//! Rust side of the Workspace Durable Object (plan §4.2 and §4.3, M0-008b).
//!
//! The Workspace DO is TypeScript (`workers/workspace`, M0-008a). The hub never re-implements
//! it: it binds the `WORKSPACE` namespace and calls `POST /rpc/<method>` on the object named by
//! the workspace id. This module holds the pieces that do not depend on the Worker runtime:
//!
//! - the wire types, which round-trip the JSON of `workers/workspace/src/types.ts`;
//! - the [`WorkspaceControl`] trait, the surface the hub codes against;
//! - [`WorkspaceError`] and the decoding of `Result<T>` and HTTP statuses into it;
//! - [`fake::FakeWorkspace`], an in-memory implementation for native tests.
//!
//! The namespace binding (`binding`) and the dev-fixture route (`dev`) are wasm32 only.
//!
//! Every method takes one request object and returns `Result<T, WorkspaceError>`. The futures
//! carry no `Send` bound, so the trait works on the single-threaded Worker runtime and needs no
//! async runtime (plan §4.2).

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub mod fake;

/// The Worker-side call through the `WORKSPACE` namespace binding.
#[cfg(target_arch = "wasm32")]
pub mod binding;

/// The `DEV_FIXTURES=1` route that drives the Workspace DO from `curl` and the dev test.
#[cfg(target_arch = "wasm32")]
pub mod dev;

/// The RPC methods, in `RPC_METHODS` order of `types.ts`. Each one is a `POST /rpc/<method>`.
pub const RPC_METHODS: [&str; 9] = [
    "start",
    "stop",
    "status",
    "snapshot",
    "restore",
    "backup",
    "restoreBackup",
    "exec",
    "preview",
];

/// Whether `method` is one of [`RPC_METHODS`] (the path segment of `POST /rpc/<method>`).
pub fn is_rpc_method(method: &str) -> bool {
    RPC_METHODS.contains(&method)
}

/// A workspace id as the DO name: 1 to 128 characters of `[A-Za-z0-9_.-]`, the same set the
/// Workspace Worker accepts in `/w/<workspaceId>/...`.
pub fn valid_workspace_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

// ---- wire types (workers/workspace/src/types.ts) ----------------------------------------------

/// `InstanceType`: the sandbox sizes the Workspace DO accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstanceType {
    #[serde(rename = "lite")]
    Lite,
    #[serde(rename = "basic")]
    Basic,
    #[serde(rename = "standard-1")]
    Standard1,
    #[serde(rename = "standard-2")]
    Standard2,
    #[serde(rename = "standard-3")]
    Standard3,
    #[serde(rename = "standard-4")]
    Standard4,
}

/// `WorkspaceState`: the two states of the machine `stopped -> running -> stopped`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceState {
    Stopped,
    Running,
}

/// `ErrorCode`. The HTTP status of each code is in [`ErrorCode::http_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidArgument,
    InvalidState,
    NotFound,
    SandboxError,
    Internal,
}

impl ErrorCode {
    /// The wire string, as in `types.ts`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid_argument",
            Self::InvalidState => "invalid_state",
            Self::NotFound => "not_found",
            Self::SandboxError => "sandbox_error",
            Self::Internal => "internal",
        }
    }

    /// The HTTP status the Workspace DO answers with for this code (`index.ts`, `STATUS`).
    pub fn http_status(self) -> u16 {
        match self {
            Self::InvalidArgument => 400,
            Self::NotFound => 404,
            Self::InvalidState => 409,
            Self::SandboxError => 502,
            Self::Internal => 500,
        }
    }

    /// The code the DO answers with for an HTTP status, for a body that does not say which.
    pub fn from_http_status(status: u16) -> Self {
        match status {
            400 => Self::InvalidArgument,
            404 => Self::NotFound,
            409 => Self::InvalidState,
            502 => Self::SandboxError,
            _ => Self::Internal,
        }
    }
}

/// `StartRequest`. `instance` defaults to `standard-2` on the DO side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartRequest {
    /// Pinned workspace image reference.
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<InstanceType>,
}

/// `StartResponse`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartResponse {
    pub state: WorkspaceState,
    pub image: String,
    pub instance: InstanceType,
}

/// `StopRequest`: the empty object.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct StopRequest {}

/// `StopResponse`. `was_running` is false when the workspace was already stopped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopResponse {
    pub state: WorkspaceState,
    pub was_running: bool,
}

/// `StatusRequest`: the empty object.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct StatusRequest {}

/// `StatusResponse`. Absent values are `null`, not missing keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusResponse {
    pub state: WorkspaceState,
    pub image: Option<String>,
    pub instance: Option<InstanceType>,
    pub started_at: Option<u64>,
    pub last_snapshot_id: Option<String>,
}

/// `SnapshotRequest`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SnapshotRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `SnapshotResponse`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotResponse {
    pub id: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `RestoreRequest`: restarts the container from a snapshot id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestoreRequest {
    pub id: String,
}

/// `RestoreResponse`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreResponse {
    pub state: WorkspaceState,
    pub restored_from: String,
}

/// `BackupHandle`: an opaque record of one DirectoryBackup, passed back to `restoreBackup`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupHandle {
    pub id: String,
    pub dir: String,
    pub size: u64,
    pub sha256: String,
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `BackupRequest`: `dir` is an absolute directory inside the sandbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupRequest {
    pub dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `BackupResponse`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupResponse {
    pub handle: BackupHandle,
}

/// `RestoreBackupRequest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestoreBackupRequest {
    pub handle: BackupHandle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// `RestoreBackupResponse`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestoreBackupResponse {
    pub restored: bool,
    pub dir: String,
}

/// `ExecRequest`: `cmd` is argv, not a shell line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecRequest {
    pub cmd: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// `ExecResponse`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecResponse {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// `PreviewRequest`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PreviewRequest {
    pub port: u16,
}

/// `PreviewResponse`. `path` is on the DO's fetch handler, not a URL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreviewResponse {
    pub port: u16,
    pub path: String,
}

// ---- errors and the Result envelope ------------------------------------------------------------

/// A failed Workspace call: the `RpcError` of the DO, or a transport failure mapped to
/// `internal`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceError {
    pub code: ErrorCode,
    pub message: String,
}

impl WorkspaceError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for WorkspaceError {}

/// The `Result<T>` envelope of `types.ts`: `{ok:true,value}` or `{ok:false,error}`.
#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    value: Option<T>,
    error: Option<RpcError>,
}

/// The `RpcError` body: `{code, message}`.
#[derive(Deserialize)]
struct RpcError {
    code: ErrorCode,
    message: String,
}

/// Longest part of an unexpected body that goes into an error message.
const MAX_BODY_IN_MESSAGE: usize = 200;

/// Decodes one `POST /rpc/<method>` answer.
///
/// An `{ok:false,error}` body wins: its code is the answer. Any other body (not JSON, or not the
/// envelope) becomes an error whose code follows the HTTP status (`400` invalid_argument, `404`
/// not_found, `409` invalid_state, `502` sandbox_error, anything else internal).
pub fn decode_response<T: DeserializeOwned>(status: u16, body: &str) -> Result<T, WorkspaceError> {
    match serde_json::from_str::<Envelope<T>>(body) {
        Ok(Envelope {
            ok: true,
            value: Some(value),
            ..
        }) if (200..300).contains(&status) => Ok(value),
        Ok(Envelope {
            ok: false,
            error: Some(error),
            ..
        }) => Err(WorkspaceError::new(error.code, error.message)),
        _ => Err(error_from_status(status, body)),
    }
}

/// The error for an answer that is not a `Result` envelope.
pub fn error_from_status(status: u16, body: &str) -> WorkspaceError {
    let snippet: String = body.chars().take(MAX_BODY_IN_MESSAGE).collect();
    WorkspaceError::new(
        ErrorCode::from_http_status(status),
        format!("workspace DO answered HTTP {status}: {snippet}"),
    )
}

// ---- the trait ----------------------------------------------------------------------------------

/// The hub's view of one Workspace DO. The `WORKSPACE` binding implements it on wasm32 by calling
/// the object; [`fake::FakeWorkspace`] implements it in memory for native tests.
///
/// The methods return `impl Future` without a `Send` bound: the Worker runtime is
/// single-threaded, and the native fake is driven by any single-threaded executor.
pub trait WorkspaceControl {
    fn start(
        &self,
        req: StartRequest,
    ) -> impl Future<Output = Result<StartResponse, WorkspaceError>>;
    fn stop(&self, req: StopRequest) -> impl Future<Output = Result<StopResponse, WorkspaceError>>;
    fn status(
        &self,
        req: StatusRequest,
    ) -> impl Future<Output = Result<StatusResponse, WorkspaceError>>;
    fn snapshot(
        &self,
        req: SnapshotRequest,
    ) -> impl Future<Output = Result<SnapshotResponse, WorkspaceError>>;
    fn restore(
        &self,
        req: RestoreRequest,
    ) -> impl Future<Output = Result<RestoreResponse, WorkspaceError>>;
    fn backup(
        &self,
        req: BackupRequest,
    ) -> impl Future<Output = Result<BackupResponse, WorkspaceError>>;
    fn restore_backup(
        &self,
        req: RestoreBackupRequest,
    ) -> impl Future<Output = Result<RestoreBackupResponse, WorkspaceError>>;
    fn exec(&self, req: ExecRequest) -> impl Future<Output = Result<ExecResponse, WorkspaceError>>;
    fn preview(
        &self,
        req: PreviewRequest,
    ) -> impl Future<Output = Result<PreviewResponse, WorkspaceError>>;
}
