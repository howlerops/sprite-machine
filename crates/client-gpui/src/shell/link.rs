//! [`SessionLink`]: the signed-in session's hub link. It owns the thread subscription, the
//! outbox, the cache they persist into and the approval controller, all on one thread (the cache
//! is not `Sync`), and publishes two things to the UI: the one banner state for
//! [`super::view::AppShell::follow_connectivity`], and a [`ThreadSnapshot`] (thread view plus
//! approval cards) of the thread the screens show.
//!
//! The shell creates a link when a session exists and drops it when the session ends, so a link
//! never outlives the grant it was opened with.
//!
//! **Which thread.** Subscriptions are per thread, so the link follows one thread: the one the
//! screens show. [`SessionLink::open_thread`] switches it (a fresh subscription resumed from the
//! cache, the same outbox, a fresh [`ApprovalController`]). With no thread yet, the link has no
//! subscription and the banner reflects the outbox alone.
//!
//! **Without moving ownership.** The thread view, the outbox and the cache never leave the link
//! thread. Code that needs them (the approval controller) runs there through
//! [`SessionLink::with_thread`], which hands a closure a [`ThreadAccess`] and returns its result
//! over a channel. After each such job the outbox is flushed and a new snapshot is published.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use client_core::outbox::{Clock, SystemRandom};
use client_core::{
    Applied, Cache, Connectivity, HttpTransport, Intent, Outbox, RetryPolicy, Subscription,
    SubscriptionError, ThreadSync, ThreadView,
};
use tokio::runtime::Builder;
use tokio::sync::{mpsc, watch};

use crate::approval::{AnswerError, ApprovalCard, ApprovalController, LocalPresets};

/// What a link needs to reach the hub as one signed-in session.
#[derive(Clone)]
pub struct LinkConfig {
    /// The hub the session belongs to (the API lives under `{hub}/v1`).
    pub hub: String,
    /// The session token, sent as a bearer credential. Never logged.
    pub token: String,
    /// The thread to follow from the start; empty for none until
    /// [`SessionLink::open_thread`].
    pub thread_id: String,
    /// Where the cache lives; `None` (or a file that cannot be opened) keeps it in memory.
    pub cache_path: Option<PathBuf>,
    /// Where the approval presets chosen on this device are kept; `None` keeps them in memory.
    pub presets_path: Option<PathBuf>,
    /// The wait before a closed or refused subscription is retried.
    pub retry: Duration,
    /// Time for the outbox and the approval cards (injected in tests).
    pub clock: Arc<dyn Clock>,
}

/// The thread the link follows, as the UI shows it: the view and its approval cards as of the
/// last change (an event, an answer, or a [`SessionLink::refresh`]).
#[derive(Debug, Clone)]
pub struct ThreadSnapshot {
    pub thread_id: String,
    pub view: ThreadView,
    pub cards: Vec<ApprovalCard>,
}

/// What a job run by [`SessionLink::with_thread`] can reach. Everything is borrowed from the
/// link thread; nothing can be kept.
pub struct ThreadAccess<'a> {
    pub thread_id: &'a str,
    pub view: &'a mut ThreadView,
    pub outbox: &'a mut Outbox,
    pub cache: &'a Cache,
    pub approvals: &'a mut ApprovalController,
}

type Job = Box<dyn FnOnce(&mut ThreadAccess<'_>) + Send>;

enum Command {
    Enqueue(Intent),
    OpenThread {
        thread_id: String,
        workspace: String,
    },
    With(Job),
    Refresh,
}

/// The running link. Dropping it stops the connection: its thread ends at the next wake-up.
pub struct SessionLink {
    commands: mpsc::UnboundedSender<Command>,
    feed: watch::Receiver<Connectivity>,
    snapshots: watch::Receiver<Option<ThreadSnapshot>>,
    hub: String,
}

impl SessionLink {
    /// Starts the link on its own thread. The first state is `Connecting`; the connection is
    /// opened there, so an unreachable hub never blocks the caller.
    pub fn open(config: LinkConfig) -> Result<Self, String> {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let (commands, inbox) = mpsc::unbounded_channel();
        let (publish, feed) = watch::channel(Connectivity::Connecting);
        let (snapshot_tx, snapshots) = watch::channel(None);
        let hub = config.hub.clone();
        std::thread::Builder::new()
            .name("session-link".to_owned())
            .spawn(move || runtime.block_on(run(config, inbox, publish, snapshot_tx)))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            commands,
            feed,
            snapshots,
            hub,
        })
    }

    /// The hub this link talks to.
    pub fn hub(&self) -> &str {
        &self.hub
    }

    /// A receiver of the combined connection state, for `AppShell::follow_connectivity`.
    pub fn connectivity_feed(&self) -> watch::Receiver<Connectivity> {
        self.feed.clone()
    }

    /// A receiver of the followed thread's snapshot; `None` until a thread is open.
    pub fn snapshot_feed(&self) -> watch::Receiver<Option<ThreadSnapshot>> {
        self.snapshots.clone()
    }

    /// Queues an intent on the outbox and sends it. Dropped silently once the link has stopped.
    pub fn enqueue(&self, intent: Intent) {
        let _ = self.commands.send(Command::Enqueue(intent));
    }

    /// Makes `thread_id` the thread the link follows (a no-op when it already is). `workspace`
    /// names the thread's workspace for `allow_always` presets; empty when unknown.
    pub fn open_thread(&self, thread_id: &str, workspace: &str) {
        let _ = self.commands.send(Command::OpenThread {
            thread_id: thread_id.to_owned(),
            workspace: workspace.to_owned(),
        });
    }

    /// Recomputes the cards (time left follows the clock), retries what is due and publishes a
    /// snapshot if the cards changed. Call it about once a second.
    pub fn refresh(&self) {
        let _ = self.commands.send(Command::Refresh);
    }

    /// Runs `job` on the link thread with the followed thread's view, outbox, cache and approval
    /// controller; the outbox is flushed afterwards and a snapshot published. The receiver
    /// yields the job's result, or nothing when no thread is open or the link has stopped.
    pub fn with_thread<R: Send + 'static>(
        &self,
        job: impl FnOnce(&mut ThreadAccess<'_>) -> R + Send + 'static,
    ) -> Receiver<R> {
        let (sender, receiver) = channel();
        let job: Job = Box::new(move |access| {
            let _ = sender.send(job(access));
        });
        let _ = self.commands.send(Command::With(job));
        receiver
    }

    /// Answers an approval with the option's own id, through the outbox, then flushes.
    pub fn answer(
        &self,
        interrupt_id: &str,
        option_id: &str,
    ) -> Receiver<Result<i64, AnswerError>> {
        let (interrupt_id, option_id) = (interrupt_id.to_owned(), option_id.to_owned());
        self.with_thread(move |access| {
            access.approvals.answer(
                access.view,
                access.outbox,
                access.cache,
                &interrupt_id,
                &option_id,
            )
        })
    }

    /// The followed thread's cards closed: forget the refusals they show.
    pub fn forget_refusals(&self) {
        drop(self.with_thread(|access| access.approvals.clear_refusals()));
    }

    /// Ticks or clears "save as this workspace's preset" on one card.
    pub fn set_remember_preset(&self, interrupt_id: &str, checked: bool) {
        let interrupt_id = interrupt_id.to_owned();
        drop(self.with_thread(move |access| {
            access.approvals.set_remember_preset(&interrupt_id, checked);
        }));
    }
}

struct Core {
    config: LinkConfig,
    api: String,
    cache: Cache,
    outbox: Outbox,
    sub: Option<Subscription>,
    approvals: Option<ApprovalController>,
    transport: HttpTransport,
    random: SystemRandom,
    connectivity: watch::Sender<Connectivity>,
    snapshots: watch::Sender<Option<ThreadSnapshot>>,
}

async fn run(
    config: LinkConfig,
    mut inbox: mpsc::UnboundedReceiver<Command>,
    connectivity: watch::Sender<Connectivity>,
    snapshots: watch::Sender<Option<ThreadSnapshot>>,
) {
    let api = format!("{}/v1", config.hub.trim_end_matches('/'));
    let cache = config
        .cache_path
        .as_deref()
        .and_then(|path| Cache::open(path).ok())
        .or_else(|| Cache::open_in_memory().ok());
    let Some(cache) = cache else {
        let _ = connectivity.send(Connectivity::Offline);
        return;
    };
    let Ok(outbox) = Outbox::load(&cache, RetryPolicy::default()) else {
        let _ = connectivity.send(Connectivity::Offline);
        return;
    };
    let initial = config.thread_id.clone();
    let mut core = Core {
        transport: HttpTransport::new(Some(config.token.clone())),
        config,
        api,
        cache,
        outbox,
        sub: None,
        approvals: None,
        random: SystemRandom,
        connectivity,
        snapshots,
    };
    if !initial.is_empty() {
        core.open_thread(&initial, "").await;
    }
    core.publish_connectivity();
    loop {
        tokio::select! {
            command = inbox.recv() => match command {
                None => break,
                Some(command) => core.handle(command).await,
            },
            read = next_read(&mut core.sub, &mut core.cache) => core.after_read(read).await,
        }
    }
}

/// The next message of the followed thread; never ready while no thread is followed.
async fn next_read(
    sub: &mut Option<Subscription>,
    cache: &mut Cache,
) -> Result<Applied, SubscriptionError> {
    match sub {
        Some(sub) => sub.next_persisted(cache).await,
        None => std::future::pending().await,
    }
}

impl Core {
    async fn handle(&mut self, command: Command) {
        match command {
            Command::Enqueue(intent) => {
                let now = self.config.clock.now_ms();
                if self.outbox.enqueue(&self.cache, intent, now).is_ok() {
                    self.flush().await;
                }
                self.publish_snapshot(true);
            }
            Command::OpenThread {
                thread_id,
                workspace,
            } => self.open_thread(&thread_id, &workspace).await,
            Command::With(job) => {
                if let (Some(sub), Some(approvals)) = (self.sub.as_mut(), self.approvals.as_mut()) {
                    let thread_id = sub.view().thread_id().to_owned();
                    let mut access = ThreadAccess {
                        thread_id: &thread_id,
                        view: sub.sync_mut().view_mut(),
                        outbox: &mut self.outbox,
                        cache: &self.cache,
                        approvals,
                    };
                    job(&mut access);
                    self.flush().await;
                    self.publish_snapshot(true);
                }
            }
            Command::Refresh => {
                self.flush().await;
                self.publish_snapshot(false);
            }
        }
    }

    async fn after_read(&mut self, read: Result<Applied, SubscriptionError>) {
        match read {
            Ok(Applied::Live) => {
                self.flush().await;
                self.publish_snapshot(true);
            }
            Ok(Applied::Ignored | Applied::Pong) => {}
            Ok(_) => self.publish_snapshot(true),
            Err(_) => {
                self.publish_connectivity();
                tokio::time::sleep(self.config.retry).await;
                if let Some(sub) = self.sub.as_mut() {
                    let _ = sub.reconnect().await;
                }
            }
        }
        self.publish_connectivity();
    }

    /// Follows `thread_id`: its stored events, then a fresh subscription from there. The outbox
    /// is the same one; the approval controller starts over for the new thread.
    async fn open_thread(&mut self, thread_id: &str, workspace: &str) {
        let same = self
            .sub
            .as_ref()
            .is_some_and(|sub| sub.view().thread_id() == thread_id);
        if same {
            return;
        }
        let presets = match self.approvals.take() {
            Some(old) => old.into_presets(),
            None => self
                .config
                .presets_path
                .as_deref()
                .and_then(|path| LocalPresets::open(path).ok())
                .unwrap_or_else(LocalPresets::in_memory),
        };
        let sync = self
            .cache
            .events(thread_id)
            .ok()
            .and_then(|events| ThreadSync::restore(thread_id, events).ok())
            .unwrap_or_else(|| ThreadSync::new(thread_id));
        self.sub = None;
        self.snapshots.send_replace(None);
        match Subscription::connect_or_offline(&self.api, sync, Some(self.config.token.clone()))
            .await
        {
            Ok(sub) => {
                self.sub = Some(sub);
                self.approvals = Some(ApprovalController::new(
                    thread_id,
                    workspace,
                    self.config.clock.clone(),
                    presets,
                ));
                self.publish_snapshot(true);
            }
            Err(_) => {
                let _ = self.connectivity.send(Connectivity::Offline);
            }
        }
        self.publish_connectivity();
    }

    async fn flush(&mut self) {
        let _ = self
            .outbox
            .flush(
                &self.cache,
                &self.transport,
                &self.api,
                self.config.clock.as_ref(),
                &mut self.random,
            )
            .await;
        self.publish_connectivity();
    }

    /// The banner state of the followed thread's subscription and the outbox
    /// ([`client_core::banner_state`]). Repeats are not published.
    fn publish_connectivity(&self) {
        let state = client_core::banner_state(
            self.sub.as_ref().map(|sub| sub.connectivity()),
            &self.outbox,
        );
        self.connectivity.send_if_modified(|current| {
            let changed = *current != state;
            *current = state;
            changed
        });
    }

    /// Copies the outbox's fate of this device's answers into the view, then publishes the view
    /// with its cards. Unless `force`, only when the cards differ from the last snapshot.
    fn publish_snapshot(&mut self, force: bool) {
        let (Some(sub), Some(approvals)) = (self.sub.as_mut(), self.approvals.as_mut()) else {
            return;
        };
        let view = sub.sync_mut().view_mut();
        approvals.sync(&self.outbox, view);
        let snapshot = ThreadSnapshot {
            thread_id: view.thread_id().to_owned(),
            cards: approvals.cards(view),
            view: view.clone(),
        };
        self.snapshots.send_if_modified(|current| {
            let same = !force
                && current.as_ref().is_some_and(|last| {
                    last.thread_id == snapshot.thread_id && last.cards == snapshot.cards
                });
            if !same {
                *current = Some(snapshot);
            }
            !same
        });
    }
}
