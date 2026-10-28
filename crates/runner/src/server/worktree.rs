//! `--spawn worktree`: one `git worktree add` per session under a runner-owned directory, on a
//! branch of its own (`sm/<session name>`). The worktree is removed when the session ends; the
//! branch stays, so the session's commits are kept. A prompt for the thread later adds the
//! worktree back on that branch ([`Worktree::restore`]).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::process::Command;

/// A worktree this runner created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// The repository's top-level directory.
    pub repo: PathBuf,
    /// The worktree's root.
    pub path: PathBuf,
    /// The branch the worktree is on.
    pub branch: String,
}

/// The record of a session's worktree, in the session's directory.
const RECORD: &str = "worktree.json";

async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .context("run git")?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Removes the worktree at `path` of `repo`; one with uncommitted changes is refused.
async fn git_remove(repo: &Path, path: &Path) -> Result<()> {
    let path = path.to_string_lossy().into_owned();
    git(repo, &["worktree", "remove", &path]).await.map(|_| ())
}

impl Worktree {
    /// Adds a worktree named `name` under `root`, on a new branch `sm/<name>`, for the repository
    /// that holds `dir`. Returns it with the session's working directory in it (the same
    /// sub-directory of the repository as `dir`).
    pub async fn add(dir: &Path, root: &Path, name: &str) -> Result<(Self, PathBuf)> {
        let top = git(dir, &["rev-parse", "--show-toplevel"])
            .await
            .with_context(|| format!("{} is not in a git repository", dir.display()))?;
        let prefix = git(dir, &["rev-parse", "--show-prefix"]).await?;
        std::fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
        let worktree = Self {
            repo: PathBuf::from(top),
            path: root.join(name),
            branch: format!("sm/{name}"),
        };
        let path = worktree.path.to_string_lossy().into_owned();
        git(
            &worktree.repo,
            &["worktree", "add", "-b", &worktree.branch, &path, "HEAD"],
        )
        .await?;
        let cwd = if prefix.is_empty() {
            worktree.path.clone()
        } else {
            worktree.path.join(prefix)
        };
        Ok((worktree, cwd))
    }

    /// Adds the worktree back at its path, on its existing branch: the session's commits are
    /// there as they were when the session ended.
    pub async fn restore(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let path = self.path.to_string_lossy().into_owned();
        git(&self.repo, &["worktree", "add", &path, &self.branch])
            .await
            .map(|_| ())
    }

    /// Records the worktree in `session_dir`, so a later prompt for the session can add it back.
    pub fn save(&self, session_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(session_dir)
            .with_context(|| format!("create {}", session_dir.display()))?;
        let record = json!({
            "repo": self.repo,
            "path": self.path,
            "branch": self.branch,
        });
        let file = session_dir.join(RECORD);
        std::fs::write(&file, record.to_string())
            .with_context(|| format!("write {}", file.display()))
    }

    /// The worktree recorded in `session_dir`, if the session had one.
    pub fn load(session_dir: &Path) -> Option<Self> {
        let bytes = std::fs::read(session_dir.join(RECORD)).ok()?;
        let record: Value = serde_json::from_slice(&bytes).ok()?;
        Some(Self {
            repo: PathBuf::from(record["repo"].as_str()?),
            path: PathBuf::from(record["path"].as_str()?),
            branch: record["branch"].as_str()?.to_owned(),
        })
    }

    /// Removes the worktree. One with uncommitted changes is kept (and reported): the agent's
    /// work is not thrown away.
    pub async fn remove(&self) {
        if let Err(error) = git_remove(&self.repo, &self.path).await {
            tracing::warn!(worktree = %self.path.display(), "worktree kept: {error:#}");
        }
    }
}

/// The repository a worktree at `path` was added from.
async fn repo_of(path: &Path) -> Option<PathBuf> {
    let common = git(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await
    .ok()?;
    PathBuf::from(common).parent().map(Path::to_path_buf)
}

/// Removes the worktrees under `root` that no session owns: clean ones go, the rest (changes, or
/// not removable) stay and are listed in one warning. `keep` are worktrees still needed.
pub async fn sweep(root: &Path, keep: &[PathBuf]) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut kept = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || keep.iter().any(|k| k.starts_with(&path)) {
            continue;
        }
        let removed = match repo_of(&path).await {
            Some(repo) => git_remove(&repo, &path).await.is_ok(),
            None => false,
        };
        if !removed {
            kept.push(path);
        }
    }
    if !kept.is_empty() {
        kept.sort();
        let list: Vec<_> = kept.iter().map(|p| p.display().to_string()).collect();
        tracing::warn!(
            "{} worktree(s) of earlier sessions have changes and were kept: {}",
            list.len(),
            list.join(", ")
        );
    }
}
