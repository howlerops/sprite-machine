//! [`LocalRunner`]: the handle the desktop holds on its embedded runner.
//!
//! The runner lives on a tokio runtime of its own (the GPUI executor is not tokio), started on
//! first use. Every method is an `async fn` that runs its work there, so it can be awaited from
//! any executor (the tests use `#[tokio::test]`); the view uses [`LocalRunner::spawn`] and reads
//! [`LocalRunner::snapshot`] on its next frame, so no foreign thread ever wakes a GPUI task.
//!
//! The machine credential is read by the runner from its own store (`sm login`); this module
//! never sees it.

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use runner::embed::{Config, Embedded, SessionSpec};
use runner::login::CredentialStore;
use runner::server::PermissionMode;
use runner::tunnel::Timing;

use super::model::{Action, PhoneLink, SessionRow};

/// How to build the runner's configuration. A closure so that a stop and a start read the
/// environment (and the stored login) again.
#[derive(Clone)]
pub struct LocalConfig {
    make: Arc<dyn Fn() -> Result<Config, String> + Send + Sync>,
}

impl LocalConfig {
    pub fn new(make: impl Fn() -> Result<Config, String> + Send + Sync + 'static) -> Self {
        Self {
            make: Arc::new(make),
        }
    }

    /// What `sm` uses: the stored login (`SM_CREDENTIAL_FILE`, else keychain, else its file), the
    /// state dir (`SM_STATE_DIR`, else `$XDG_STATE_HOME/sm`, else `~/.local/state/sm`), `HOME`
    /// and `PATH`.
    pub fn from_env() -> Self {
        Self::new(|| {
            let non_empty = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
            let state_dir = if let Some(dir) = non_empty("SM_STATE_DIR") {
                PathBuf::from(dir)
            } else if let Some(dir) = non_empty("XDG_STATE_HOME") {
                PathBuf::from(dir).join("sm")
            } else {
                let home = non_empty("HOME")
                    .ok_or_else(|| "no home directory: set SM_STATE_DIR".to_owned())?;
                PathBuf::from(home).join(".local/state/sm")
            };
            Ok(Config {
                credentials: CredentialStore::from_env().map_err(|error| format!("{error:#}"))?,
                state_dir,
                home: std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default(),
                path_env: std::env::var_os("PATH").unwrap_or_default(),
                permission_mode: PermissionMode::Default,
                timing: Timing::default(),
            })
        })
    }
}

/// Whether the embedded runner is up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerStatus {
    Stopped,
    Starting,
    Running,
    /// `sm login` was never run on this computer: there is no machine credential.
    NotEnrolled,
    /// It could not start, or its tunnel stopped for good (the message is for the user).
    Failed(String),
}

impl RunnerStatus {
    /// Fluent id of the status line, and its `reason` argument.
    pub fn message(&self) -> (&'static str, Option<&str>) {
        match self {
            Self::Stopped => ("phone-runner-stopped", None),
            Self::Starting => ("phone-runner-starting", None),
            Self::Running => ("phone-runner-running", None),
            Self::NotEnrolled => ("phone-runner-not-enrolled", None),
            Self::Failed(reason) => ("phone-runner-failed", Some(reason)),
        }
    }
}

/// Everything the view draws, copied out under one lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub status: RunnerStatus,
    pub auto: bool,
    pub rows: Vec<SessionRow>,
    /// Changes whenever anything above may have; the view repaints when it differs.
    pub version: u64,
}

struct Shared {
    config: LocalConfig,
    runtime: OnceLock<Runtime>,
    state: Mutex<State>,
    embedded: Mutex<Option<Arc<Embedded>>>,
    /// One start at a time.
    gate: tokio::sync::Mutex<()>,
    version: AtomicU64,
}

struct State {
    status: RunnerStatus,
    link: PhoneLink,
}

/// A runtime that is shut down without waiting when dropped, also inside another runtime.
struct Runtime {
    runtime: Option<tokio::runtime::Runtime>,
    handle: tokio::runtime::Handle,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// The embedded runner. Clones share it.
#[derive(Clone)]
pub struct LocalRunner {
    shared: Arc<Shared>,
}

impl LocalRunner {
    /// A runner that is not started. `auto` is the persisted "Connect to phone automatically".
    pub fn new(config: LocalConfig, auto: bool) -> Self {
        Self {
            shared: Arc::new(Shared {
                config,
                runtime: OnceLock::new(),
                state: Mutex::new(State {
                    status: RunnerStatus::Stopped,
                    link: PhoneLink::new(auto),
                }),
                embedded: Mutex::new(None),
                gate: tokio::sync::Mutex::new(()),
                version: AtomicU64::new(1),
            }),
        }
    }

    /// Runs `work` on the runner's runtime without waiting for it.
    pub fn spawn(&self, work: impl Future<Output = ()> + Send + 'static) {
        self.runtime().spawn(work);
    }

    /// The state to draw.
    pub fn snapshot(&self) -> Snapshot {
        let state = self.state();
        let mut status = state.status.clone();
        if status == RunnerStatus::Running
            && self
                .embedded()
                .is_some_and(|embedded| !embedded.is_running())
        {
            status = RunnerStatus::Failed("the connection to the hub stopped".to_owned());
        }
        Snapshot {
            status,
            auto: state.link.auto(),
            rows: state.link.rows().to_vec(),
            version: self.shared.version.load(Ordering::SeqCst),
        }
    }

    /// Starts the machine tunnel with the stored login. Starting a running runner does nothing.
    pub async fn start(&self) -> Result<(), String> {
        let this = self.clone();
        self.on_runtime(async move { this.start_inner().await })
            .await
    }

    /// Ends every session and says goodbye to the hub.
    pub async fn stop(&self) {
        let this = self.clone();
        self.on_runtime(async move {
            let embedded = this.take_embedded();
            if let Some(embedded) = embedded {
                embedded.stop().await;
            }
            {
                let mut state = this.state();
                state.link.clear();
                state.status = RunnerStatus::Stopped;
            }
            this.changed();
        })
        .await;
    }

    /// Stores `dir` as a trusted directory of this computer (what `sm trust` does).
    pub async fn trust(&self, dir: &Path) -> Result<(), String> {
        let (this, dir) = (self.clone(), dir.to_owned());
        self.on_runtime(async move {
            this.start_inner().await?;
            let embedded = this.embedded().ok_or("the runner is not running")?;
            embedded
                .trust(&dir)
                .map(|_| ())
                .map_err(|error| format!("{error:#}"))
        })
        .await
    }

    /// Starts a local session in `cwd` (a trusted directory). With auto-connect on, it is
    /// connected to the phone before this returns. Returns the session's thread id.
    pub async fn open_session(
        &self,
        cwd: PathBuf,
        title: Option<String>,
        harness: Option<String>,
    ) -> Result<String, String> {
        let this = self.clone();
        self.on_runtime(async move {
            this.start_inner().await?;
            let embedded = this.embedded().ok_or("the runner is not running")?;
            let info = embedded
                .open_session(SessionSpec {
                    cwd,
                    title,
                    harness,
                })
                .await
                .map_err(|error| format!("{error:#}"))?;
            let actions = this.state().link.add_session(&info.thread_id, &info.title);
            this.changed();
            this.run_actions(actions).await;
            Ok(info.thread_id)
        })
        .await
    }

    /// A prompt typed on this computer, for a session.
    pub async fn prompt(&self, thread_id: &str, text: &str) -> Result<(), String> {
        let (this, thread_id, text) = (self.clone(), thread_id.to_owned(), text.to_owned());
        self.on_runtime(async move {
            let embedded = this.embedded().ok_or("the runner is not running")?;
            embedded
                .prompt(&thread_id, &text)
                .await
                .map_err(|error| format!("{error:#}"))
        })
        .await
    }

    /// The global setting. Turning it on connects the sessions that are off.
    pub async fn set_auto_connect(&self, on: bool) {
        let this = self.clone();
        self.on_runtime(async move {
            let actions = this.state().link.set_auto(on);
            this.changed();
            this.run_actions(actions).await;
        })
        .await;
    }

    /// The per-session switch: connects a local session to the phone, or disconnects it.
    pub async fn toggle(&self, thread_id: &str) {
        let (this, thread_id) = (self.clone(), thread_id.to_owned());
        self.on_runtime(async move {
            let action = this.state().link.toggle(&thread_id);
            this.changed();
            this.run_actions(action.into_iter().collect()).await;
        })
        .await;
    }

    async fn start_inner(&self) -> Result<(), String> {
        let _one_at_a_time = self.shared.gate.lock().await;
        if self.embedded().is_some() {
            return Ok(());
        }
        self.set_status(RunnerStatus::Starting);
        let outcome = async {
            let config = (self.shared.config.make)()?;
            match config.credentials.load() {
                Ok(Some(_)) => {}
                Ok(None) => return Err(None),
                Err(error) => return Err(Some(format!("{error:#}"))),
            }
            Embedded::start(config)
                .await
                .map_err(|error| Some(format!("{error:#}")))
        }
        .await;
        match outcome {
            Ok(embedded) => {
                *self.embedded_slot() = Some(Arc::new(embedded));
                self.set_status(RunnerStatus::Running);
                Ok(())
            }
            Err(None) => {
                self.set_status(RunnerStatus::NotEnrolled);
                Err("this computer is not enrolled with the hub".to_owned())
            }
            Err(Some(reason)) => {
                self.set_status(RunnerStatus::Failed(reason.clone()));
                Err(reason)
            }
        }
    }

    /// Does what the model decided, in order. A connect that finishes after the user switched the
    /// session off asks for a disconnect, which is run here too.
    async fn run_actions(&self, actions: Vec<Action>) {
        let mut queue: VecDeque<Action> = actions.into();
        while let Some(action) = queue.pop_front() {
            let embedded = self.embedded();
            match action {
                Action::Connect(thread_id) => {
                    let result = match &embedded {
                        Some(embedded) => embedded
                            .connect(&thread_id)
                            .await
                            .map_err(|error| format!("{error:#}")),
                        None => Err("the runner is not running".to_owned()),
                    };
                    let mut state = self.state();
                    match result {
                        Ok(url) => queue.extend(state.link.connected(&thread_id, &url)),
                        Err(message) => state.link.failed(&thread_id, &message),
                    }
                }
                Action::Disconnect(thread_id) => {
                    // Only an unknown session fails, and then there is nothing to disconnect.
                    if let Some(embedded) = &embedded {
                        let _ = embedded.disconnect(&thread_id).await;
                    }
                }
            }
            self.changed();
        }
    }

    fn runtime(&self) -> &tokio::runtime::Handle {
        &self
            .shared
            .runtime
            .get_or_init(|| {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("local-runner")
                    .enable_all()
                    .build()
                    .expect("build the embedded runner's runtime");
                Runtime {
                    handle: runtime.handle().clone(),
                    runtime: Some(runtime),
                }
            })
            .handle
    }

    async fn on_runtime<T: Send + 'static>(
        &self,
        work: impl Future<Output = T> + Send + 'static,
    ) -> T {
        self.runtime()
            .spawn(work)
            .await
            .expect("the embedded runner's task panicked")
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set_status(&self, status: RunnerStatus) {
        self.state().status = status;
        self.changed();
    }

    fn changed(&self) {
        self.shared.version.fetch_add(1, Ordering::SeqCst);
    }

    fn embedded_slot(&self) -> MutexGuard<'_, Option<Arc<Embedded>>> {
        self.shared
            .embedded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn embedded(&self) -> Option<Arc<Embedded>> {
        self.embedded_slot().clone()
    }

    fn take_embedded(&self) -> Option<Arc<Embedded>> {
        self.embedded_slot().take()
    }
}
