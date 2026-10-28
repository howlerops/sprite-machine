//! Checkpoints: per-turn snapshots and undo (PLAN-v2 §4.1, §4.3).
//!
//! - `local`: [`ShadowGitCheckpointer`], shadow-git commits under `refs/sm/checkpoints/*`.
//!   A stub until M2B-011.
//! - `sandbox`: [`DirectoryBackupCheckpointer`], a `DirectoryBackup` of `/workspace` through the
//!   hub, plus a shadow-git commit. A stub until M1A-012.

use crate::host::BoxFuture;

/// A checkpoint handle, as returned by [`Checkpointer::snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// Mode-specific identifier (a shadow-git ref or a backup handle).
    pub id: String,
}

/// Checkpoint errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckpointError {
    /// The operation is not implemented in this mode yet.
    #[error("checkpoint {op} is not implemented yet: {item} fills it")]
    NotImplemented {
        op: &'static str,
        item: &'static str,
    },
}

/// Per-turn snapshots and restore.
pub trait Checkpointer: Send + Sync {
    /// Snapshots the workspace under `label` (the task or turn id).
    fn snapshot<'a>(&'a self, label: &'a str)
    -> BoxFuture<'a, Result<Checkpoint, CheckpointError>>;

    /// Restores the workspace to `checkpoint`.
    fn restore<'a>(
        &'a self,
        checkpoint: &'a Checkpoint,
    ) -> BoxFuture<'a, Result<(), CheckpointError>>;
}

/// The `local` checkpointer: shadow-git commits. Stub until M2B-011.
#[derive(Debug, Default, Clone, Copy)]
pub struct ShadowGitCheckpointer;

impl Checkpointer for ShadowGitCheckpointer {
    fn snapshot<'a>(
        &'a self,
        _label: &'a str,
    ) -> BoxFuture<'a, Result<Checkpoint, CheckpointError>> {
        Box::pin(std::future::ready(Err(CheckpointError::NotImplemented {
            op: "snapshot",
            item: "M2B-011",
        })))
    }

    fn restore<'a>(
        &'a self,
        _checkpoint: &'a Checkpoint,
    ) -> BoxFuture<'a, Result<(), CheckpointError>> {
        Box::pin(std::future::ready(Err(CheckpointError::NotImplemented {
            op: "restore",
            item: "M2B-011",
        })))
    }
}

/// The `sandbox` checkpointer: `DirectoryBackup` through the hub, plus shadow git. Stub until
/// M1A-012.
#[derive(Debug, Default, Clone, Copy)]
pub struct DirectoryBackupCheckpointer;

impl Checkpointer for DirectoryBackupCheckpointer {
    fn snapshot<'a>(
        &'a self,
        _label: &'a str,
    ) -> BoxFuture<'a, Result<Checkpoint, CheckpointError>> {
        Box::pin(std::future::ready(Err(CheckpointError::NotImplemented {
            op: "snapshot",
            item: "M1A-012",
        })))
    }

    fn restore<'a>(
        &'a self,
        _checkpoint: &'a Checkpoint,
    ) -> BoxFuture<'a, Result<(), CheckpointError>> {
        Box::pin(std::future::ready(Err(CheckpointError::NotImplemented {
            op: "restore",
            item: "M1A-012",
        })))
    }
}
