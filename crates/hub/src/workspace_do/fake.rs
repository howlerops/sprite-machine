//! In-memory [`WorkspaceControl`] for native tests (M0-008b).
//!
//! It mirrors `workers/workspace/src/core.ts` over `fake.ts` (`FakeSandbox`): the same
//! `stopped -> running -> stopped` machine, the same argument checks, and the same error code and
//! message for each invalid state (`409 invalid_state`, `404 not_found`, `400 invalid_argument`).
//! The sandbox is fake, so `exec` only knows `echo`, `false` and `env`, as `FakeSandbox` does.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use super::{
    BackupHandle, BackupRequest, BackupResponse, ErrorCode, ExecRequest, ExecResponse,
    InstanceType, PreviewRequest, PreviewResponse, RestoreBackupRequest, RestoreBackupResponse,
    RestoreRequest, RestoreResponse, SnapshotRequest, SnapshotResponse, StartRequest,
    StartResponse, StatusRequest, StatusResponse, StopRequest, StopResponse, WorkspaceControl,
    WorkspaceError, WorkspaceState,
};

/// The instance size used when a request does not name one (`DEFAULT_INSTANCE` in `core.ts`).
pub const DEFAULT_INSTANCE: InstanceType = InstanceType::Standard2;

/// The clock of the fake: the first start is at this many milliseconds, then 1 s per start.
const FAKE_EPOCH_MS: u64 = 1_700_000_000_000;

/// The size the fake reports for a snapshot (`FakeSandbox.snapshot`).
const SNAPSHOT_SIZE: u64 = 1024;

#[derive(Debug, Default)]
struct State {
    running: bool,
    image: Option<String>,
    instance: Option<InstanceType>,
    started_at: Option<u64>,
    last_snapshot_id: Option<String>,
    seq: u64,
    snapshots: BTreeSet<String>,
    backups: BTreeSet<String>,
}

/// One in-memory workspace. Methods take `&self` (interior mutability), as the trait requires.
#[derive(Debug, Default)]
pub struct FakeWorkspace {
    state: RefCell<State>,
}

impl FakeWorkspace {
    pub fn new() -> Self {
        Self::default()
    }

    /// The fake's own clock: 1 s later per start or restore, starting at [`FAKE_EPOCH_MS`].
    fn tick(state: &mut State) -> u64 {
        state.seq += 1;
        FAKE_EPOCH_MS + state.seq * 1_000
    }

    fn require_running(state: &State) -> Result<(), WorkspaceError> {
        if state.running {
            Ok(())
        } else {
            Err(invalid_state("workspace is stopped; call start first"))
        }
    }
}

fn fail(code: ErrorCode, message: impl Into<String>) -> WorkspaceError {
    WorkspaceError::new(code, message)
}

fn invalid_argument(message: impl Into<String>) -> WorkspaceError {
    fail(ErrorCode::InvalidArgument, message)
}

fn invalid_state(message: impl Into<String>) -> WorkspaceError {
    fail(ErrorCode::InvalidState, message)
}

fn not_found(message: impl Into<String>) -> WorkspaceError {
    fail(ErrorCode::NotFound, message)
}

/// A non-empty string of at most `max` characters with no NUL (`str` in `core.ts`).
fn check_str(value: &str, what: &str, max: usize) -> Result<(), WorkspaceError> {
    if value.is_empty() {
        return Err(invalid_argument(format!(
            "{what} must be a non-empty string"
        )));
    }
    if value.chars().count() > max {
        return Err(invalid_argument(format!(
            "{what} is longer than {max} characters"
        )));
    }
    if value.contains('\0') {
        return Err(invalid_argument(format!("{what} must not contain NUL")));
    }
    Ok(())
}

/// An absolute path with no `..` segment (`absDir` in `core.ts`).
fn check_abs_dir(value: &str, what: &str) -> Result<(), WorkspaceError> {
    check_str(value, what, 4096)?;
    if !value.starts_with('/') {
        return Err(invalid_argument(format!("{what} must be an absolute path")));
    }
    if value.split('/').any(|segment| segment == "..") {
        return Err(invalid_argument(format!("{what} must not contain ..")));
    }
    Ok(())
}

/// An env key as `core.ts` accepts it: `[A-Za-z_][A-Za-z0-9_]*`.
fn check_env_key(key: &str) -> Result<(), WorkspaceError> {
    let mut chars = key.chars();
    let first_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    if first_ok && chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(())
    } else {
        Err(invalid_argument(format!("env key {key:?} is invalid")))
    }
}

/// The fake's answer for an argv (`FakeSandbox.exec`).
fn fake_exec(cmd: &[String], env: Option<&BTreeMap<String, String>>) -> ExecResponse {
    let (bin, args) = match cmd.split_first() {
        Some(parts) => parts,
        None => {
            return ExecResponse {
                exit_code: 127,
                stdout: String::new(),
                stderr: "empty command\n".to_owned(),
            };
        }
    };
    match bin.as_str() {
        "echo" => ExecResponse {
            exit_code: 0,
            stdout: format!("{}\n", args.join(" ")),
            stderr: String::new(),
        },
        "false" => ExecResponse {
            exit_code: 1,
            stdout: String::new(),
            stderr: String::new(),
        },
        "env" => ExecResponse {
            exit_code: 0,
            stdout: env
                .map(|vars| vars.iter().map(|(k, v)| format!("{k}={v}\n")).collect())
                .unwrap_or_default(),
            stderr: String::new(),
        },
        other => ExecResponse {
            exit_code: 127,
            stdout: String::new(),
            stderr: format!("{other}: command not found\n"),
        },
    }
}

impl WorkspaceControl for FakeWorkspace {
    async fn start(&self, req: StartRequest) -> Result<StartResponse, WorkspaceError> {
        check_str(&req.image, "image", 512)?;
        let instance = req.instance.unwrap_or(DEFAULT_INSTANCE);
        let mut state = self.state.borrow_mut();
        if state.running {
            return Err(invalid_state("workspace is already running"));
        }
        state.running = true;
        state.image = Some(req.image.clone());
        state.instance = Some(instance);
        let now = Self::tick(&mut state);
        state.started_at = Some(now);
        Ok(StartResponse {
            state: WorkspaceState::Running,
            image: req.image,
            instance,
        })
    }

    async fn stop(&self, _req: StopRequest) -> Result<StopResponse, WorkspaceError> {
        let mut state = self.state.borrow_mut();
        let was_running = state.running;
        state.running = false;
        state.started_at = None;
        Ok(StopResponse {
            state: WorkspaceState::Stopped,
            was_running,
        })
    }

    async fn status(&self, _req: StatusRequest) -> Result<StatusResponse, WorkspaceError> {
        let state = self.state.borrow();
        Ok(StatusResponse {
            state: if state.running {
                WorkspaceState::Running
            } else {
                WorkspaceState::Stopped
            },
            image: state.image.clone(),
            instance: state.instance,
            // Only a running workspace has a start time (`status` in core.ts).
            started_at: if state.running {
                state.started_at
            } else {
                None
            },
            last_snapshot_id: state.last_snapshot_id.clone(),
        })
    }

    async fn snapshot(&self, req: SnapshotRequest) -> Result<SnapshotResponse, WorkspaceError> {
        if let Some(name) = &req.name {
            check_str(name, "name", 256)?;
        }
        let mut state = self.state.borrow_mut();
        Self::require_running(&state)?;
        state.seq += 1;
        let id = format!("snap-{}", state.seq);
        state.snapshots.insert(id.clone());
        state.last_snapshot_id = Some(id.clone());
        Ok(SnapshotResponse {
            id,
            size: SNAPSHOT_SIZE,
            name: req.name,
        })
    }

    async fn restore(&self, req: RestoreRequest) -> Result<RestoreResponse, WorkspaceError> {
        check_str(&req.id, "id", 128)?;
        let mut state = self.state.borrow_mut();
        if !state.snapshots.contains(&req.id) {
            return Err(not_found(format!("unknown snapshot {}", req.id)));
        }
        state.running = true;
        if state.instance.is_none() {
            state.instance = Some(DEFAULT_INSTANCE);
        }
        let now = Self::tick(&mut state);
        state.started_at = Some(now);
        Ok(RestoreResponse {
            state: WorkspaceState::Running,
            restored_from: req.id,
        })
    }

    async fn backup(&self, req: BackupRequest) -> Result<BackupResponse, WorkspaceError> {
        check_abs_dir(&req.dir, "dir")?;
        if let Some(name) = &req.name {
            check_str(name, "name", 256)?;
        }
        let mut state = self.state.borrow_mut();
        Self::require_running(&state)?;
        state.seq += 1;
        let id = format!("bak-{}", state.seq);
        state.backups.insert(id.clone());
        Ok(BackupResponse {
            handle: BackupHandle {
                id,
                dir: req.dir,
                size: 2048,
                sha256: "0".repeat(64),
                format: "tar+zstd/1".to_owned(),
                name: req.name,
            },
        })
    }

    async fn restore_backup(
        &self,
        req: RestoreBackupRequest,
    ) -> Result<RestoreBackupResponse, WorkspaceError> {
        let handle = &req.handle;
        check_str(&handle.id, "handle.id", 128)?;
        check_abs_dir(&handle.dir, "handle.dir")?;
        check_str(&handle.sha256, "handle.sha256", 128)?;
        check_str(&handle.format, "handle.format", 64)?;
        if let Some(name) = &handle.name {
            check_str(name, "handle.name", 256)?;
        }
        if let Some(dir) = &req.dir {
            check_abs_dir(dir, "dir")?;
        }
        let state = self.state.borrow();
        Self::require_running(&state)?;
        if !state.backups.contains(&handle.id) {
            return Err(not_found(format!("unknown backup {}", handle.id)));
        }
        Ok(RestoreBackupResponse {
            restored: true,
            dir: req.dir.unwrap_or_else(|| handle.dir.clone()),
        })
    }

    async fn exec(&self, req: ExecRequest) -> Result<ExecResponse, WorkspaceError> {
        if req.cmd.is_empty() || req.cmd.len() > 256 {
            return Err(invalid_argument(
                "cmd must be a non-empty array of at most 256 strings",
            ));
        }
        for (i, arg) in req.cmd.iter().enumerate() {
            check_str(arg, &format!("cmd[{i}]"), 65536)?;
        }
        if let Some(env) = &req.env {
            for (key, value) in env {
                check_env_key(key)?;
                if value.contains('\0') {
                    return Err(invalid_argument(format!(
                        "env value for {key} must be a string"
                    )));
                }
            }
        }
        if let Some(cwd) = &req.cwd {
            check_abs_dir(cwd, "cwd")?;
        }
        let state = self.state.borrow();
        Self::require_running(&state)?;
        Ok(fake_exec(&req.cmd, req.env.as_ref()))
    }

    async fn preview(&self, req: PreviewRequest) -> Result<PreviewResponse, WorkspaceError> {
        if req.port == 0 {
            return Err(invalid_argument("port must be an integer in 1..65535"));
        }
        let state = self.state.borrow();
        Self::require_running(&state)?;
        Ok(PreviewResponse {
            port: req.port,
            path: format!("/preview/{}/", req.port),
        })
    }
}
