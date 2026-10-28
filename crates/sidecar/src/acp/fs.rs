//! Client-side file access for the ACP `fs/read_text_file` and `fs/write_text_file` requests,
//! confined to the workspace root.
//!
//! [`Workspace`] holds an opened [capability directory](cap_std::fs::Dir) for its root. Every
//! read and write resolves its path one component at a time relative to that directory
//! (`openat` with no-follow semantics, symlinks emulated inside the sandbox), so a path can
//! never leave the root, even when a component is swapped for a symlink while the operation
//! runs. There is no check-then-use gap: the lookup is the check.
//!
//! - a `..` component is refused up front ([`FsError::Traversal`]);
//! - an absolute path must lie lexically under the root, else [`FsError::Outside`];
//! - a symlink (file or directory) whose target leaves the root is refused
//!   ([`FsError::Outside`]), and so is a write through a symlink whose target is missing and
//!   outside the root ([`FsError::DanglingSymlink`]). The error kind is classified after the
//!   fact for the caller's benefit; the refusal itself comes from the capability lookup.
//!
//! The capability lookup refuses every absolute symlink, so symlinks are first expanded by a
//! left-to-right walk (see `Workspace::confined`): an absolute target under the root is
//! re-based onto it, anything else is refused. The walk is not the guard; the I/O that follows
//! is relative to the root handle again and refuses any escape however the tree changed.
//!
//! Changes from the path-based version: an absolute path must be lexically under the canonical
//! root (an absolute alias of the root that goes through a symlink is `Outside`), and a
//! dangling symlink that points inside the root is followed and its target created, where it
//! used to be `DanglingSymlink`. All other error kinds are unchanged.
//!
//! Relative paths are taken as relative to the root. Symlinks that stay inside the root are
//! followed, including a dangling one inside the root, whose target a write then creates
//! (inside the root). A write creates missing parent directories, inside the root.
//!
//! [`Workspace::resolve`] is an advisory, path-based resolution used for a terminal's `cwd`.
//! File I/O does not rely on it.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use protocol::acp::{
    ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest, WriteTextFileResponse,
};

/// Symlinks followed in one path before giving up, as the kernel does.
const MAX_LINK_HOPS: usize = 40;
/// `ENOTDIR`, which `read_link` reports for a component below a regular file.
const NOT_A_DIRECTORY: i32 = 20;

/// The components of `path` as owned names. `..` and `.` are kept; root and prefix are not.
fn components_of(path: &Path) -> VecDeque<OsString> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_os_string()),
            Component::ParentDir => Some(OsString::from("..")),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => None,
        })
        .collect()
}

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
    dir: Arc<Dir>,
}

impl Workspace {
    /// Opens the workspace at `root`. The root is canonicalized and opened as a directory
    /// handle that all later file I/O is relative to. It must be an existing directory.
    pub fn new(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = std::fs::canonicalize(root)?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "workspace root is not a directory",
            ));
        }
        let dir = Dir::open_ambient_dir(&root, ambient_authority())?;
        Ok(Self {
            root,
            dir: Arc::new(dir),
        })
    }

    /// The canonical root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves `path` to its real location inside the root, by path. The path does not
    /// have to exist. Missing components are returned as they are, under the deepest
    /// existing ancestor's real path.
    ///
    /// This is a snapshot: it is fine for choosing a terminal's working directory, but file
    /// I/O does not rely on it.
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

    /// The path relative to the root, with `.` removed. Lexical only; the capability lookup
    /// does the real confinement.
    fn relative(&self, path: &Path) -> Result<PathBuf, FsError> {
        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(FsError::Traversal(path.to_path_buf()));
        }
        let stripped = if path.is_absolute() {
            path.strip_prefix(&self.root)
                .map_err(|_| FsError::Outside(path.to_path_buf()))?
        } else {
            path
        };
        let mut relative = PathBuf::new();
        for component in stripped.components() {
            if let Component::Normal(name) = component {
                relative.push(name);
            }
        }
        if relative.as_os_str().is_empty() {
            relative.push(".");
        }
        Ok(relative)
    }

    /// The symlink-free path of `path` relative to the root.
    ///
    /// The capability lookup refuses every absolute symlink, even one that points back into
    /// the root. So symlinks are expanded here, left to right: an absolute target must lie
    /// under the root and is re-based onto it, a relative one is spliced in place. Each
    /// component before the one being looked at is a real directory, so `..` in a target is
    /// exact. This walk is advisory, not the guard: the I/O that follows goes through the
    /// capability directory again, which refuses any escape however the tree changed in
    /// between.
    fn confined(&self, path: &Path) -> Result<PathBuf, FsError> {
        let relative = self.relative(path)?;
        let result = self.expand_links(&relative);
        result.map_err(|error| match error {
            FsError::Io(error) => self.classify(path, error),
            // A refused link: say whether it was dangling, as the path-based walk can tell.
            FsError::Outside(_) => match self.resolve(path) {
                Err(dangling @ FsError::DanglingSymlink(_)) => dangling,
                _ => FsError::Outside(path.to_path_buf()),
            },
            other => other,
        })
    }

    fn expand_links(&self, relative: &Path) -> Result<PathBuf, FsError> {
        let mut pending: VecDeque<OsString> = components_of(relative);
        let mut done = PathBuf::new();
        let mut hops = 0usize;
        while let Some(name) = pending.pop_front() {
            if name == "." {
                continue;
            }
            if name == ".." {
                if !done.pop() {
                    return Err(FsError::Outside(relative.to_path_buf()));
                }
                continue;
            }
            let candidate = done.join(&name);
            match self.dir.read_link_contents(&candidate) {
                Ok(target) => {
                    hops += 1;
                    if hops > MAX_LINK_HOPS {
                        return Err(FsError::Io(io::Error::other("too many levels of symlinks")));
                    }
                    let rebased = if target.is_absolute() {
                        done = PathBuf::new();
                        target
                            .strip_prefix(&self.root)
                            .map_err(|_| FsError::Outside(relative.to_path_buf()))?
                            .to_path_buf()
                    } else {
                        target
                    };
                    let mut spliced = components_of(&rebased);
                    spliced.extend(pending);
                    pending = spliced;
                }
                // Not a symlink, or nothing there yet: an ordinary component.
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::InvalidInput | io::ErrorKind::NotFound
                    ) || error.raw_os_error() == Some(NOT_A_DIRECTORY) =>
                {
                    done.push(name);
                }
                Err(error) => return Err(FsError::Io(error)),
            }
        }
        if done.as_os_str().is_empty() {
            done.push(".");
        }
        Ok(done)
    }

    /// Names the reason for a failed lookup. The capability layer reports an escape as
    /// `PermissionDenied` (or, for a directory it could not create, `AlreadyExists`); the
    /// advisory resolution then says whether it was an escape or a dangling link. Anything
    /// else stays an I/O error, except an escape that resolution cannot see any more (the
    /// link changed since), which is still `Outside`. This only picks the error kind: the
    /// refusal itself has already happened.
    fn classify(&self, path: &Path, error: io::Error) -> FsError {
        if let Err(refusal @ (FsError::Outside(_) | FsError::DanglingSymlink(_))) =
            self.resolve(path)
        {
            return refusal;
        }
        if error.kind() == io::ErrorKind::PermissionDenied && error.to_string().contains("outside")
        {
            return FsError::Outside(path.to_path_buf());
        }
        FsError::Io(error)
    }

    fn read_blocking(&self, path: &Path) -> Result<String, FsError> {
        let relative = self.confined(path)?;
        let mut text = String::new();
        let result = self
            .dir
            .open(&relative)
            .and_then(|mut file| file.read_to_string(&mut text));
        match result {
            Ok(_) => Ok(text),
            Err(error) => Err(self.classify(path, error)),
        }
    }

    fn write_blocking(&self, path: &Path, content: &str) -> Result<(), FsError> {
        let relative = self.confined(path)?;
        let result = (|| {
            if let Some(parent) = relative.parent().filter(|p| !p.as_os_str().is_empty()) {
                self.dir.create_dir_all(parent)?;
            }
            let mut options = OpenOptions::new();
            options.write(true).create(true).truncate(true);
            let mut file = self.dir.open_with(&relative, &options)?;
            file.write_all(content.as_bytes())
        })();
        result.map_err(|error| self.classify(path, error))
    }

    /// Answers `fs/read_text_file`. `line` is 1-based and `limit` counts lines. A `line` of
    /// 0 reads from the first line, and a `line` past the end gives an empty string.
    pub async fn read_text_file(
        &self,
        request: ReadTextFileRequest,
    ) -> Result<ReadTextFileResponse, FsError> {
        let workspace = self.clone();
        let (path, line, limit) = (request.path, request.line, request.limit);
        let text = run_blocking(move || workspace.read_blocking(&path)).await?;
        Ok(ReadTextFileResponse::new(slice_lines(&text, line, limit)))
    }

    /// Answers `fs/write_text_file`, creating missing parent directories inside the root.
    pub async fn write_text_file(
        &self,
        request: WriteTextFileRequest,
    ) -> Result<WriteTextFileResponse, FsError> {
        let workspace = self.clone();
        run_blocking(move || workspace.write_blocking(&request.path, &request.content)).await?;
        Ok(WriteTextFileResponse::new())
    }
}

/// Runs blocking file I/O off the async threads. A panicked or cancelled task is an I/O error.
async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, FsError> + Send + 'static,
) -> Result<T, FsError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(error) => Err(FsError::Io(io::Error::other(error))),
    }
}

/// The lines `line..line + limit` of `text`, each keeping its line ending.
fn slice_lines(text: &str, line: Option<u32>, limit: Option<u32>) -> String {
    let skip = line.unwrap_or(1).saturating_sub(1) as usize;
    let take = limit.map_or(usize::MAX, |limit| limit as usize);
    text.split_inclusive('\n').skip(skip).take(take).collect()
}
