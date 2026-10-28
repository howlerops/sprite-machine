//! Local checkpoints as shadow git commits (PLAN-v2 §4.10, `docs/specs/turn-event.md` §4.8).
//!
//! A checkpoint is a commit under `refs/sm/checkpoints/<thread>/<n>` (n = 1, 2, ...) whose tree
//! holds the tracked and untracked, non-ignored files of the working tree. Each checkpoint is
//! parented on the previous one. The tree is built with plumbing against a temporary index
//! (`GIT_INDEX_FILE`), so taking a checkpoint never touches the user's index, HEAD, branches,
//! stash or working tree.
//!
//! Everything shells out to the `git` binary.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail, ensure};
use protocol::turn_event::{Checkpoint, CheckpointKind, is_valid_id};

const CHECKPOINT_NS: &str = "refs/sm/checkpoints";
const IDENTITY_NAME: &str = "sm checkpoint";
const IDENTITY_EMAIL: &str = "checkpoint@sm.invalid";

static TEMP_INDEX_SEQ: AtomicU64 = AtomicU64::new(0);

/// A checkpoint that was taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub thread: String,
    pub n: u64,
    pub git_ref: String,
    pub commit: String,
    pub tree: String,
    /// Paths that differ from the previous checkpoint (all files, for the first one).
    pub files_changed: u64,
    pub label: Option<String>,
}

/// What a diff compares a checkpoint against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Checkpoint(u64),
    WorkingTree,
}

/// A unified diff and the paths it touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub unified: String,
    pub files: Vec<String>,
}

/// Takes the next checkpoint (n = last + 1, or 1) of `thread` for the repository containing `dir`.
pub fn take(dir: &Path, thread: &str, label: Option<&str>) -> Result<Snapshot> {
    ensure!(is_valid_id(thread), "invalid thread id {thread:?}");
    let repo = Repo::open(dir)?;
    let n = repo
        .checkpoint_numbers(thread)?
        .into_iter()
        .max()
        .map_or(1, |last| last + 1);
    let git_ref = checkpoint_ref(thread, n);
    repo.git(&["check-ref-format", &git_ref], &[])
        .with_context(|| format!("checkpoint ref {git_ref} is not a valid git ref"))?;

    let tree = repo.working_tree()?;
    let parent = if n > 1 {
        Some(repo.resolve_commit(&checkpoint_ref(thread, n - 1))?)
    } else {
        None
    };
    let base_tree = match &parent {
        Some(commit) => repo.commit_tree(commit)?,
        None => repo.text(&["mktree"], &[])?,
    };
    let files_changed = repo.changed_paths(&base_tree, &tree)?.len() as u64;

    let message = match label {
        Some(label) => format!("sm checkpoint {n}: {label}"),
        None => format!("sm checkpoint {n}"),
    };
    let identity = identity_env();
    let mut args = vec!["commit-tree", tree.as_str(), "-m", message.as_str()];
    if let Some(parent) = parent.as_deref() {
        args.extend(["-p", parent]);
    }
    let commit = repo.text(&args, &identity)?;
    repo.git(&["update-ref", &git_ref, &commit], &[])?;

    Ok(Snapshot {
        thread: thread.to_owned(),
        n,
        git_ref,
        commit,
        tree,
        files_changed,
        label: label.map(str::to_owned),
    })
}

/// The checkpoint numbers that exist for `thread`, unordered.
pub fn list(dir: &Path, thread: &str) -> Result<Vec<u64>> {
    Repo::open(dir)?.checkpoint_numbers(thread)
}

/// Restores checkpoint `n` of `thread` into the working tree.
///
/// Tracked and untracked (non-ignored) files are written back to the snapshot. Files that
/// exist now but not in the snapshot are removed. Ignored files are not touched.
pub fn restore(dir: &Path, thread: &str, n: u64) -> Result<()> {
    let repo = Repo::open(dir)?;
    let target = repo.checkpoint_tree(thread, n)?;
    let current = repo.working_tree()?;

    for (status, path) in repo.changed_entries(&target, &current)? {
        if status == "A" {
            let full = repo.top.join(&path);
            fs::remove_file(&full)
                .with_context(|| format!("remove {} during restore", full.display()))?;
            remove_empty_parents(&repo.top, &full);
        }
    }

    let index = repo.temp_index()?;
    let env = [("GIT_INDEX_FILE", index.path.as_os_str())];
    repo.git(&["read-tree", &target], &env)?;
    repo.git(&["checkout-index", "-a", "-f"], &env)?;
    Ok(())
}

/// Diffs checkpoint `from` against checkpoint or working-tree state `to`.
pub fn diff(dir: &Path, thread: &str, from: u64, to: Target) -> Result<Diff> {
    let repo = Repo::open(dir)?;
    let from_tree = repo.checkpoint_tree(thread, from)?;
    let to_tree = match to {
        Target::Checkpoint(n) => repo.checkpoint_tree(thread, n)?,
        Target::WorkingTree => repo.working_tree()?,
    };
    let unified = repo.git(
        &[
            "diff-tree",
            "-r",
            "-p",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            &from_tree,
            &to_tree,
        ],
        &[],
    )?;
    Ok(Diff {
        unified: String::from_utf8(unified).context("git diff output is not UTF-8")?,
        files: repo.changed_paths(&from_tree, &to_tree)?,
    })
}

/// The TurnEvent `Checkpoint` (§4.8, kind `shadow_git`) for a snapshot taken by [`take`].
pub fn checkpoint_event(snapshot: &Snapshot, checkpoint_id: &str) -> Checkpoint {
    Checkpoint {
        checkpoint_id: checkpoint_id.to_owned(),
        kind: CheckpointKind::ShadowGit,
        git_ref: Some(snapshot.git_ref.clone()),
        backup_handle: None,
        label: snapshot.label.clone(),
        files_changed: Some(snapshot.files_changed),
    }
}

fn checkpoint_ref(thread: &str, n: u64) -> String {
    format!("{CHECKPOINT_NS}/{thread}/{n}")
}

fn identity_env() -> [(&'static str, &'static OsStr); 4] {
    [
        ("GIT_AUTHOR_NAME", OsStr::new(IDENTITY_NAME)),
        ("GIT_AUTHOR_EMAIL", OsStr::new(IDENTITY_EMAIL)),
        ("GIT_COMMITTER_NAME", OsStr::new(IDENTITY_NAME)),
        ("GIT_COMMITTER_EMAIL", OsStr::new(IDENTITY_EMAIL)),
    ]
}

/// Runs `git` in `dir`, with the caller's repository environment variables cleared.
fn git_run(dir: &Path, args: &[&str], envs: &[(&str, &OsStr)]) -> Result<Output> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null());
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_owned()
}

/// A repository, identified by its top-level work tree.
struct Repo {
    top: PathBuf,
}

impl Repo {
    fn open(dir: &Path) -> Result<Self> {
        let output = git_run(dir, &["rev-parse", "--show-toplevel"], &[])?;
        if !output.status.success() {
            bail!(
                "{} is not inside a git work tree: {}",
                dir.display(),
                stderr_text(&output)
            );
        }
        let top = String::from_utf8(output.stdout).context("git toplevel is not UTF-8")?;
        Ok(Self {
            top: PathBuf::from(top.trim_end()),
        })
    }

    /// Runs git at the top of the work tree and returns its stdout.
    fn git(&self, args: &[&str], envs: &[(&str, &OsStr)]) -> Result<Vec<u8>> {
        let output = git_run(&self.top, args, envs)?;
        if !output.status.success() {
            bail!("git {} failed: {}", args.join(" "), stderr_text(&output));
        }
        Ok(output.stdout)
    }

    /// Like [`Repo::git`], with trailing newlines removed.
    fn text(&self, args: &[&str], envs: &[(&str, &OsStr)]) -> Result<String> {
        let stdout = self.git(args, envs)?;
        let text = String::from_utf8(stdout).context("git output is not UTF-8")?;
        Ok(text.trim_end().to_owned())
    }

    /// A temporary index file in the git directory, removed on drop.
    fn temp_index(&self) -> Result<TempIndex> {
        let name = format!(
            "sm-checkpoint-index-{}-{}",
            std::process::id(),
            TEMP_INDEX_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let relative = self.text(&["rev-parse", "--git-path", &name], &[])?;
        Ok(TempIndex {
            path: self.top.join(relative),
        })
    }

    /// The tree of the working tree's tracked and untracked, non-ignored files.
    fn working_tree(&self) -> Result<String> {
        let index = self.temp_index()?;
        let env = [("GIT_INDEX_FILE", index.path.as_os_str())];
        self.git(&["add", "-A"], &env)?;
        self.text(&["write-tree"], &env)
    }

    fn checkpoint_numbers(&self, thread: &str) -> Result<Vec<u64>> {
        let prefix = format!("{CHECKPOINT_NS}/{thread}/");
        let refs = self.text(&["for-each-ref", "--format=%(refname)", CHECKPOINT_NS], &[])?;
        Ok(refs
            .lines()
            .filter_map(|name| name.strip_prefix(prefix.as_str()))
            .filter_map(|n| n.parse().ok())
            .collect())
    }

    fn resolve_commit(&self, git_ref: &str) -> Result<String> {
        self.text(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{git_ref}^{{commit}}"),
            ],
            &[],
        )
        .with_context(|| format!("no such checkpoint ref {git_ref}"))
    }

    fn commit_tree(&self, commit: &str) -> Result<String> {
        self.text(&["rev-parse", &format!("{commit}^{{tree}}")], &[])
    }

    fn checkpoint_tree(&self, thread: &str, n: u64) -> Result<String> {
        let commit = self.resolve_commit(&checkpoint_ref(thread, n))?;
        self.commit_tree(&commit)
    }

    /// Paths that differ between two trees, as `(status, path)`.
    fn changed_entries(&self, from: &str, to: &str) -> Result<Vec<(String, String)>> {
        let out = self.git(
            &[
                "diff-tree",
                "-r",
                "-z",
                "--name-status",
                "--no-renames",
                from,
                to,
            ],
            &[],
        )?;
        let mut fields = out.split(|b| *b == 0).filter(|field| !field.is_empty());
        let mut entries = Vec::new();
        while let Some(status) = fields.next() {
            let path = fields.next().context("diff-tree status without a path")?;
            entries.push((
                String::from_utf8(status.to_vec()).context("status is not UTF-8")?,
                String::from_utf8(path.to_vec()).context("path is not UTF-8")?,
            ));
        }
        Ok(entries)
    }

    fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let out = self.git(
            &[
                "diff-tree",
                "-r",
                "-z",
                "--name-only",
                "--no-renames",
                from,
                to,
            ],
            &[],
        )?;
        out.split(|b| *b == 0)
            .filter(|field| !field.is_empty())
            .map(|field| String::from_utf8(field.to_vec()).context("path is not UTF-8"))
            .collect()
    }
}

/// Removes `file`'s parent directories up to `top`, stopping at the first non-empty one.
fn remove_empty_parents(top: &Path, file: &Path) {
    let mut dir = file.parent();
    while let Some(current) = dir {
        if current == top || !current.starts_with(top) || fs::remove_dir(current).is_err() {
            break;
        }
        dir = current.parent();
    }
}

struct TempIndex {
    path: PathBuf,
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
