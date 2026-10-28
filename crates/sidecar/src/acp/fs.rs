//! Client-side file access for the ACP `fs/read_text_file` and `fs/write_text_file` requests,
//! confined to the workspace root.
//!
//! [`Workspace`] resolves every path the agent sends against its root and refuses any path
//! that leaves it:
//!
//! - a `..` component ([`FsError::Traversal`]);
//! - an absolute path outside the root, or a path whose real location is outside it
//!   ([`FsError::Outside`]). The real location follows symlinks, so a symlinked file or a
//!   symlinked parent directory that points out of the root is caught, and so is a write
//!   into a path that does not exist yet;
//! - a dangling symlink, which a write would follow to an unchecked target
//!   ([`FsError::DanglingSymlink`]).
//!
//! Relative paths are taken as relative to the root. Symlinks that stay inside the root are
//! followed. A write creates missing parent directories, which are inside the root by
//! construction.
//!
//! The check and the I/O that follows are two steps. A process that swaps a path component for
//! a symlink in between can still escape. The agent writes through the runner, so this is the
//! limit of the check and is not guarded against further.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

use protocol::acp::{
    ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest, WriteTextFileResponse,
};

/// Errors from the workspace file access.
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    /// The path has a `..` component.
    #[error("path {0} contains '..'")]
    Traversal(PathBuf),
    /// The path resolves to a location outside the workspace root.
    #[error("path {0} is outside the workspace")]
    Outside(PathBuf),
    /// The path is a symlink whose target does not exist, so its target cannot be checked.
    #[error("path {0} is a dangling symlink")]
    DanglingSymlink(PathBuf),
    /// The file system call failed.
    #[error("file system error: {0}")]
    Io(#[from] io::Error),
}

/// The workspace root that file access is confined to.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Opens the workspace at `root`. The root is canonicalized, so later checks compare
    /// real paths. It must be an existing directory.
    pub fn new(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = std::fs::canonicalize(root)?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "workspace root is not a directory",
            ));
        }
        Ok(Self { root })
    }

    /// The canonical root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves `path` to its real location inside the root. The path does not have to
    /// exist. Missing components are returned as they are, under the deepest existing
    /// ancestor's real path.
    pub fn resolve(&self, path: &Path) -> Result<PathBuf, FsError> {
        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(FsError::Traversal(path.to_path_buf()));
        }
        let mut candidate = if path.is_absolute() {
            PathBuf::from(std::path::MAIN_SEPARATOR_STR)
        } else {
            self.root.clone()
        };
        for component in path.components() {
            if let Component::Normal(name) = component {
                candidate.push(name);
            }
        }

        // Walk up to the deepest entry that exists. `symlink_metadata` sees a dangling
        // symlink as existing, so it is not skipped over.
        let mut missing: Vec<OsString> = Vec::new();
        let mut probe = candidate;
        loop {
            match std::fs::symlink_metadata(&probe) {
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    match (probe.file_name(), probe.parent()) {
                        (Some(name), Some(parent)) => {
                            missing.push(name.to_os_string());
                            probe = parent.to_path_buf();
                        }
                        _ => return Err(FsError::Outside(path.to_path_buf())),
                    }
                }
                Err(error) => return Err(FsError::Io(error)),
            }
        }

        let base = match std::fs::canonicalize(&probe) {
            Ok(base) => base,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(FsError::DanglingSymlink(path.to_path_buf()));
            }
            Err(error) => return Err(FsError::Io(error)),
        };
        if !base.starts_with(&self.root) {
            return Err(FsError::Outside(path.to_path_buf()));
        }
        let mut resolved = base;
        for name in missing.iter().rev() {
            resolved.push(name);
        }
        Ok(resolved)
    }

    /// Answers `fs/read_text_file`. `line` is 1-based and `limit` counts lines. A `line` of
    /// 0 reads from the first line, and a `line` past the end gives an empty string.
    pub async fn read_text_file(
        &self,
        request: ReadTextFileRequest,
    ) -> Result<ReadTextFileResponse, FsError> {
        let path = self.resolve(&request.path)?;
        let text = tokio::fs::read_to_string(&path).await?;
        Ok(ReadTextFileResponse::new(slice_lines(
            &text,
            request.line,
            request.limit,
        )))
    }

    /// Answers `fs/write_text_file`, creating missing parent directories inside the root.
    pub async fn write_text_file(
        &self,
        request: WriteTextFileRequest,
    ) -> Result<WriteTextFileResponse, FsError> {
        let path = self.resolve(&request.path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, request.content).await?;
        Ok(WriteTextFileResponse::new())
    }
}

/// The lines `line..line + limit` of `text`, each keeping its line ending.
fn slice_lines(text: &str, line: Option<u32>, limit: Option<u32>) -> String {
    let skip = line.unwrap_or(1).saturating_sub(1) as usize;
    let take = limit.map_or(usize::MAX, |limit| limit as usize);
    text.split_inclusive('\n').skip(skip).take(take).collect()
}
