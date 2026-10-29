//! [`SessionLink`]: the signed-in session's hub link. It owns the `client_core::Connection`
//! (thread subscription plus outbox), the cache it persists into, and the thread they run on,
//! and publishes the one banner state for [`super::view::AppShell::follow_connectivity`].
//!
//! The shell creates a link when a session exists and drops it when the session ends, so a
//! link never outlives the grant it was opened with. Later items hand the thread view, the
//! outbox and the cache to screens through this type (the `Connection` stays on its task; screens
//! talk to it through [`SessionLink::enqueue`] and the feed), which keeps one owner for each.

use std::path::PathBuf;
use std::time::Duration;

use client_core::outbox::{Clock as _, SystemClock, SystemRandom};
use client_core::{
    Applied, Cache, Connection, Connectivity, HttpTransport, Intent, Outbox, RetryPolicy,
    Subscription, ThreadSync,
};
use tokio::runtime::Builder;
use tokio::sync::{mpsc, watch};

/// What a link needs to reach the hub as one signed-in session.
#[derive(Clone)]
pub struct LinkConfig {
    /// The hub the session belongs to (the API lives under `{hub}/v1`).
    pub hub: String,
    /// The session token, sent as a bearer credential. Never logged.
    pub token: String,
    /// The thread whose subscription carries the banner state.
    pub thread_id: String,
    /// Where the cache lives; `None` (or a file that cannot be opened) keeps it in memory.
    pub cache_path: Option<PathBuf>,
    /// The wait before a closed or refused subscription is retried.
    pub retry: Duration,
}

enum Command {
    Enqueue(Intent),
}

/// The running link. Dropping it stops the connection: its thread ends at the next wake-up.
pub struct SessionLink {
    commands: mpsc::UnboundedSender<Command>,
    feed: watch::Receiver<Connectivity>,
    hub: String,
}

impl SessionLink {
    /// Starts the link on its own thread (the cache is not `Sync`, so the connection stays on
    /// one thread). The first state is `Connecting`; the connection is opened there, so an
    /// unreachable hub never blocks the caller.
    pub fn open(config: LinkConfig) -> Result<Self, String> {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let (commands, inbox) = mpsc::unbounded_channel();
        let (publish, feed) = watch::channel(Connectivity::Connecting);
        let hub = config.hub.clone();
        std::thread::Builder::new()
            .name("session-link".to_owned())
            .spawn(move || runtime.block_on(run(config, inbox, publish)))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            commands,
            feed,
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

    /// Queues an intent on the connection's outbox and sends it. Dropped silently once the link
    /// has stopped.
    pub fn enqueue(&self, intent: Intent) {
        let _ = self.commands.send(Command::Enqueue(intent));
    }
}

async fn run(
    config: LinkConfig,
    mut inbox: mpsc::UnboundedReceiver<Command>,
    publish: watch::Sender<Connectivity>,
) {
    let api = format!("{}/v1", config.hub.trim_end_matches('/'));
    let mut cache = config
        .cache_path
        .as_deref()
        .and_then(|path| Cache::open(path).ok())
        .or_else(|| Cache::open_in_memory().ok());
    let Some(cache) = cache.as_mut() else {
        let _ = publish.send(Connectivity::Offline);
        return;
    };
    let Ok(outbox) = Outbox::load(cache, RetryPolicy::default()) else {
        let _ = publish.send(Connectivity::Offline);
        return;
    };
    let subscription = match Subscription::connect_or_offline(
        &api,
        ThreadSync::new(config.thread_id.clone()),
        Some(config.token.clone()),
    )
    .await
    {
        Ok(subscription) => subscription,
        Err(_) => {
            let _ = publish.send(Connectivity::Offline);
            return;
        }
    };
    let mut connection = Connection::new(subscription, outbox);

    // The connection's own signal, forwarded as it changes.
    let mut inner = connection.watch_connectivity();
    let _ = publish.send(*inner.borrow_and_update());
    tokio::spawn(async move {
        while inner.changed().await.is_ok() {
            let state = *inner.borrow_and_update();
            if publish.send(state).is_err() {
                break;
            }
        }
    });

    let transport = HttpTransport::new(Some(config.token.clone()));
    let clock = SystemClock;
    let mut random = SystemRandom;
    loop {
        tokio::select! {
            command = inbox.recv() => match command {
                None => break,
                Some(Command::Enqueue(intent)) => {
                    if connection.enqueue(cache, intent, clock.now_ms()).is_ok() {
                        let _ = connection.flush(cache, &transport, &api, &clock, &mut random).await;
                    }
                }
            },
            read = connection.next_persisted(cache) => match read {
                Ok(Applied::Live) => {
                    let _ = connection.flush(cache, &transport, &api, &clock, &mut random).await;
                }
                Ok(_) => {}
                Err(_) => {
                    tokio::time::sleep(config.retry).await;
                    let _ = connection.reconnect().await;
                }
            },
        }
    }
}
