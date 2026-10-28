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

    /// Removes the worktree. One with uncommitted changes is kept (and reported): the agent's
    /// work is not thrown away.
    pub async fn remove(&self) {
        let path = self.path.to_string_lossy().into_owned();
        if let Err(error) = git(&self.repo, &["worktree", "remove", &path]).await {
            tracing::warn!(worktree = %self.path.display(), "worktree kept: {error:#}");
        }
    }
}
