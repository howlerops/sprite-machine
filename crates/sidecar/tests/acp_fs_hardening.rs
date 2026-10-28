//! Hardening tests for the sidecar's client-side fs and terminals: `terminal/kill` escalates
//! to SIGKILL, file I/O cannot be raced out of the workspace, and a failed `terminal/create`
//! leaves no child or thread behind.
//!
//! Unix only (process groups, symlinks); `kill -0` is used for liveness so the tests run on
//! Linux and macOS alike.
#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use protocol::acp::{
    CreateTerminalRequest, KillTerminalRequest, ReadTextFileRequest, ReleaseTerminalRequest,
    SessionId, TerminalId, TerminalOutputRequest, WaitForTerminalExitRequest, WriteTextFileRequest,
};
use sidecar::acp::fs::Workspace;
use sidecar::acp::terminal::{
    ActivitySink, SpawnHooks, SpawnStep, TerminalError, TerminalScope, Terminals,
};
use tempfile::TempDir;
use tokio::time::timeout;

const OUTSIDE_SECRET: &str = "OUTSIDE-SECRET";

fn session() -> SessionId {
    SessionId::from("s1")
}

fn scope() -> TerminalScope {
    TerminalScope {
        thread_id: "thread_1".to_owned(),
        task_id: None,
        run_id: None,
    }
}

fn sink() -> ActivitySink {
    Arc::new(|_| {})
}

fn workspace() -> (TempDir, Workspace) {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("workspace");
    fs::create_dir_all(&root).expect("create workspace root");
    let workspace = Workspace::new(&root).expect("open workspace");
    (dir, workspace)
}

fn shell(script: &str) -> CreateTerminalRequest {
    CreateTerminalRequest::new(session(), "sh").args(vec!["-c".to_owned(), script.to_owned()])
}

/// True while `kill -0 pid` finds the process (a zombie counts as alive).
fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

async fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn output_of(terminals: &Terminals, id: &TerminalId) -> String {
    let request = TerminalOutputRequest::new(session(), id.clone());
    terminals.output(request).await.expect("output").output
}

/// Waits until the script printed `ready`, which it does after its setup (here: the trap).
async fn wait_ready(terminals: &Terminals, id: &TerminalId) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !output_of(terminals, id).await.contains("ready") {
        assert!(Instant::now() < deadline, "the script never printed ready");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---- terminal/kill escalation -------------------------------------------------------------

#[tokio::test]
async fn kill_escalates_to_sigkill_when_sighup_is_ignored() {
    let (_dir, workspace) = workspace();
    let terminals = Terminals::new(workspace, scope(), sink());
    let id = terminals
        .create(&session(), shell("trap '' HUP; echo ready; sleep 30"))
        .await
        .expect("created")
        .terminal_id;
    wait_ready(&terminals, &id).await;

    let started = Instant::now();
    terminals
        .kill(KillTerminalRequest::new(session(), id.clone()))
        .await
        .expect("kill");
    let status = timeout(
        Duration::from_secs(3),
        terminals.wait_for_exit(WaitForTerminalExitRequest::new(session(), id.clone())),
    )
    .await
    .expect("the child is gone within 3 s of kill")
    .expect("wait_for_exit")
    .exit_status;

    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "SIGHUP is ignored, so only the SIGKILL after the grace ended it: {:?}",
        started.elapsed()
    );
    assert_eq!(status.exit_code, None, "ended by a signal: {status:?}");
    assert!(status.signal.is_some(), "ended by a signal: {status:?}");
}

#[tokio::test]
async fn kill_reaches_children_in_the_process_group() {
    let (_dir, workspace) = workspace();
    let root = workspace.root().to_path_buf();
    let terminals = Terminals::new(workspace, scope(), sink());
    // A background grandchild that ignores SIGHUP, and a foreground wait.
    let id = terminals
        .create(
            &session(),
            shell("(trap '' HUP; exec sleep 30) & echo $! > grandchild.pid; echo ready; wait"),
        )
        .await
        .expect("created")
        .terminal_id;
    wait_ready(&terminals, &id).await;
    let grandchild: u32 = fs::read_to_string(root.join("grandchild.pid"))
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid");
    assert!(process_alive(grandchild));

    terminals
        .kill(KillTerminalRequest::new(session(), id.clone()))
        .await
        .expect("kill");
    wait_until("the grandchild to die", Duration::from_secs(4), || {
        !process_alive(grandchild)
    })
    .await;
}

#[tokio::test]
async fn release_after_kill_still_frees_the_terminal() {
    let (_dir, workspace) = workspace();
    let terminals = Terminals::new(workspace, scope(), sink());
    let id = terminals
        .create(&session(), shell("trap '' HUP; echo ready; sleep 30"))
        .await
        .expect("created")
        .terminal_id;
    wait_ready(&terminals, &id).await;
    terminals
        .kill(KillTerminalRequest::new(session(), id.clone()))
        .await
        .expect("kill");

    terminals
        .release(ReleaseTerminalRequest::new(session(), id.clone()))
        .await
        .expect("release after kill");

    let later = terminals
        .output(TerminalOutputRequest::new(session(), id.clone()))
        .await;
    assert!(
        matches!(later, Err(TerminalError::UnknownTerminal(_))),
        "the id is invalid after release: {later:?}"
    );
    // The reader and waiter threads end once the child is gone.
    wait_until("terminal threads to end", Duration::from_secs(5), || {
        terminals.live_threads() == 0
    })
    .await;
}

// ---- fs: no escape by racing a symlink ----------------------------------------------------

/// Atomically re-points the symlink `root/sub` between `inside` (a directory in the root) and
/// `outside`, until told to stop. Each flip is a `rename` over the old link, so `sub` always
/// exists and the main thread's lookups see either target at any step.
fn flip_sub_link(root: PathBuf, inside: PathBuf, outside: PathBuf, stop: Arc<AtomicBool>) {
    let sub = root.join("sub");
    let staging = root.join("sub.tmp");
    let mut to_outside = true;
    while !stop.load(Ordering::Relaxed) {
        let target = if to_outside { &outside } else { &inside };
        let _ = fs::remove_file(&staging);
        if std::os::unix::fs::symlink(target, &staging).is_ok() {
            let _ = fs::rename(&staging, &sub);
        }
        to_outside = !to_outside;
    }
}

#[tokio::test]
async fn fs_race_flipping_a_directory_symlink_never_escapes() {
    let (dir, workspace) = workspace();
    let root = workspace.root().to_path_buf();
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).expect("outside dir");
    fs::write(outside.join("secret.txt"), OUTSIDE_SECRET).expect("outside secret");
    let inside = root.join("real");
    fs::create_dir_all(&inside).expect("inside dir");
    fs::write(inside.join("secret.txt"), "inside").expect("inside file");
    std::os::unix::fs::symlink(&inside, root.join("sub")).expect("initial link");

    let stop = Arc::new(AtomicBool::new(false));
    let swapper = {
        let (root, inside, outside) = (root.clone(), inside.clone(), outside.clone());
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || flip_sub_link(root, inside, outside, stop))
    };

    let mut reads_ok = 0u32;
    for round in 0..4000u32 {
        let read = workspace
            .read_text_file(ReadTextFileRequest::new(session(), "sub/secret.txt"))
            .await;
        if let Ok(response) = read {
            assert_ne!(
                response.content, OUTSIDE_SECRET,
                "round {round}: read the outside file"
            );
            reads_ok += 1;
        }
        // Errors are fine: the path was a symlink out of the root at that moment.
        let _ = workspace
            .write_text_file(WriteTextFileRequest::new(
                session(),
                "sub/written.txt",
                "payload",
            ))
            .await;
        let _ = workspace
            .write_text_file(WriteTextFileRequest::new(
                session(),
                "sub/nested/deeper/written.txt",
                "payload",
            ))
            .await;
    }
    stop.store(true, Ordering::Relaxed);
    swapper.join().expect("swapper thread");

    let mut left: Vec<String> = fs::read_dir(&outside)
        .expect("outside listing")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    left.sort();
    assert_eq!(left, ["secret.txt"], "nothing was written outside the root");
    assert_eq!(
        fs::read_to_string(outside.join("secret.txt")).expect("secret"),
        OUTSIDE_SECRET,
        "the outside file is untouched"
    );
    assert!(
        reads_ok > 0,
        "some reads landed while the link pointed inside"
    );
}

#[tokio::test]
async fn fs_write_into_a_symlinked_directory_that_points_out_is_refused() {
    let (dir, workspace) = workspace();
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).expect("outside dir");
    std::os::unix::fs::symlink(&outside, workspace.root().join("link")).expect("symlink");

    let write = workspace
        .write_text_file(WriteTextFileRequest::new(session(), "link/new.txt", "x"))
        .await;

    assert!(
        matches!(write, Err(sidecar::acp::fs::FsError::Outside(_))),
        "write: {write:?}"
    );
    assert!(!outside.join("new.txt").exists());
}

// ---- terminal/create cleanup --------------------------------------------------------------

/// Fails one spawn step and records the child's pid.
struct FailAt {
    step: SpawnStep,
    pid: Mutex<Option<u32>>,
}

impl SpawnHooks for FailAt {
    fn before(&self, step: SpawnStep, pid: Option<u32>) -> std::io::Result<()> {
        if step == self.step {
            *self.pid.lock().unwrap_or_else(PoisonError::into_inner) = pid;
            return Err(std::io::Error::other("injected spawn failure"));
        }
        Ok(())
    }
}

async fn assert_failed_create_leaves_nothing(step: SpawnStep) {
    let (_dir, workspace) = workspace();
    let hooks = Arc::new(FailAt {
        step,
        pid: Mutex::new(None),
    });
    let terminals = Terminals::with_hooks(workspace, scope(), sink(), hooks.clone());

    let result = terminals
        .create(&session(), shell("trap '' HUP; sleep 30"))
        .await;

    assert!(result.is_err(), "{step:?}: create fails: {result:?}");
    let pid = hooks
        .pid
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .expect("the child had started when the step failed");
    assert!(
        !process_alive(pid),
        "{step:?}: child {pid} is gone as soon as create returns"
    );
    wait_until("the reader/waiter threads", Duration::from_secs(5), || {
        terminals.live_threads() == 0
    })
    .await;
}

#[tokio::test]
async fn failed_create_at_clone_reader_leaves_nothing_running() {
    assert_failed_create_leaves_nothing(SpawnStep::CloneReader).await;
}

#[tokio::test]
async fn failed_create_at_reader_thread_leaves_nothing_running() {
    assert_failed_create_leaves_nothing(SpawnStep::ReaderThread).await;
}

#[tokio::test]
async fn failed_create_at_waiter_thread_leaves_nothing_running() {
    assert_failed_create_leaves_nothing(SpawnStep::WaiterThread).await;
}
