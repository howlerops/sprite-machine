//! `--spawn worktree`: one `git worktree add` per session under a runner-owned directory, removed
//! when the session ends.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// A worktree this runner created.
#[derive(Debug, Clone)]
pub struct Worktree {
    /// The repository's top-level directory.
    pub repo: PathBuf,
    /// The worktree's root.
    pub path: PathBuf,
}

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

impl Worktree {
    /// Adds a detached worktree named `name` under `root` for the repository that holds `dir`
    /// and returns it with the session's working directory in it (the same sub-directory of the
    /// repository as `dir`).
    pub async fn add(dir: &Path, root: &Path, name: &str) -> Result<(Self, PathBuf)> {
        let top = git(dir, &["rev-parse", "--show-toplevel"])
            .await
            .with_context(|| format!("{} is not in a git repository", dir.display()))?;
        let prefix = git(dir, &["rev-parse", "--show-prefix"]).await?;
        std::fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
        let path = root.join(name);
        let repo = PathBuf::from(top);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                &path.to_string_lossy(),
                "HEAD",
            ],
        )
        .await?;
        let cwd = if prefix.is_empty() {
            path.clone()
        } else {
            path.join(prefix)
        };
        Ok((Self { repo, path }, cwd))
    }

    /// The worktree at `path`, found through git (its repository is the one it was added from).
    async fn existing(path: &Path) -> Option<Self> {
        let common = git(
            path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await
        .ok()?;
        let repo = PathBuf::from(common).parent()?.to_path_buf();
        Some(Self {
            repo,
            path: path.to_path_buf(),
        })
    }

    /// Removes the worktree unless it has changes; whether it is gone.
    async fn try_remove(&self) -> bool {
        let path = self.path.to_string_lossy().into_owned();
        git(&self.repo, &["worktree", "remove", &path])
            .await
            .is_ok()
    }

    /// Removes the worktree. One with uncommitted changes is kept (and reported): the agent's
    /// work is not thrown away.
    pub async fn remove(&self) {
        let path = self.path.to_string_lossy().into_owned();
        if let Err(error) = git(&self.repo, &["worktree", "remove", &path]).await {
            tracing::warn!(worktree = %self.path.display(), "worktree kept: {error:#}");
        }
    }
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
        let removed = match Worktree::existing(&path).await {
            Some(worktree) => worktree.try_remove().await,
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
