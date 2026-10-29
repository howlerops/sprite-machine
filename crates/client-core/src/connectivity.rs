//! Connectivity as the UI shows it: a banner while the client is not live.
//!
//! [`Connection`] is the one owner of the two links: the thread subscription and the outbox.
//! It publishes the combined state on a watch channel after each call that can change either
//! link, and the channel only sees a change when the banner state changes.

use tokio::sync::watch;

use crate::cache::Cache;
use crate::outbox::{Clock, FlushReport, Intent, Outbox, OutboxError, Random, Transport};
use crate::sync::{Applied, Subscription, SubscriptionError};

/// The client's link to the hub.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Connectivity {
    /// Opening a subscription, or waiting for `live` after one.
    #[default]
    Connecting,
    /// Caught up and live.
    Online,
    /// The last connection failed or closed. Writes wait in the outbox.
    Offline,
}

impl Connectivity {
    /// One state for the banner from two links: the subscription and the outbox. Offline if
    /// either is offline, else connecting if either is connecting, else online.
    pub fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Offline, _) | (_, Self::Offline) => Self::Offline,
            (Self::Connecting, _) | (_, Self::Connecting) => Self::Connecting,
            (Self::Online, Self::Online) => Self::Online,
        }
    }

    /// The banner text for this state, or `None` when no banner is needed.
    pub fn banner(self) -> Option<&'static str> {
        match self {
            Self::Connecting => Some("Connecting..."),
            Self::Online => None,
            Self::Offline => Some("Offline: changes will be sent when you reconnect"),
        }
    }
}

/// The owner of the hub link for the UI: the subscription and the outbox, one banner state.
pub struct Connection {
    subscription: Subscription,
    outbox: Outbox,
    combined: watch::Sender<Connectivity>,
}

impl Connection {
    /// Takes both links. The published state starts as their combination.
    pub fn new(subscription: Subscription, outbox: Outbox) -> Self {
        let state = Self::state_of(&subscription, &outbox);
        Self {
            subscription,
            outbox,
            combined: watch::channel(state).0,
        }
    }

    pub fn subscription(&self) -> &Subscription {
        &self.subscription
    }

    pub fn outbox(&self) -> &Outbox {
        &self.outbox
    }

    /// The combined state, as of the last call that changed a link.
    pub fn connectivity(&self) -> Connectivity {
        *self.combined.borrow()
    }

    /// A receiver for the combined state. It sees each change of the banner state, never a
    /// repeat of the same state.
    pub fn watch_connectivity(&self) -> watch::Receiver<Connectivity> {
        self.combined.subscribe()
    }

    /// [`Subscription::next_persisted`], then the combined state is published.
    pub async fn next_persisted(
        &mut self,
        cache: &mut Cache,
    ) -> Result<Applied, SubscriptionError> {
        let applied = self.subscription.next_persisted(cache).await;
        self.publish();
        applied
    }

    /// [`Subscription::reconnect`], then the combined state is published.
    pub async fn reconnect(&mut self) -> Result<(), SubscriptionError> {
        let result = self.subscription.reconnect().await;
        self.publish();
        result
    }

    /// The combined banner state of the two links: [`banner_state`].
    fn state_of(subscription: &Subscription, outbox: &Outbox) -> Connectivity {
        banner_state(Some(subscription.connectivity()), outbox)
    }

    /// Queues an intent. Queuing leaves both links as they are, but an unanswered intent
    /// gives a still-`Connecting` outbox an opinion again, so the combined state is published.
    pub fn enqueue(
        &mut self,
        cache: &Cache,
        intent: Intent,
        now_ms: u64,
    ) -> Result<i64, OutboxError> {
        let id = self.outbox.enqueue(cache, intent, now_ms)?;
        self.publish();
        Ok(id)
    }

    /// [`Outbox::flush`], then the combined state is published.
    pub async fn flush<T: Transport>(
        &mut self,
        cache: &Cache,
        transport: &T,
        base_url: &str,
        clock: &dyn Clock,
        rng: &mut dyn Random,
    ) -> Result<FlushReport, OutboxError> {
        let report = self
            .outbox
            .flush(cache, transport, base_url, clock, rng)
            .await;
        self.publish();
        report
    }

    fn publish(&self) {
        let state = Self::state_of(&self.subscription, &self.outbox);
        self.combined.send_if_modified(|current| {
            let changed = *current != state;
            *current = state;
            changed
        });
    }
}

/// The one banner state for a subscription's link (`None` while no thread is followed) and an
/// outbox. Every client that shows a banner uses this, so the rule lives in one place.
///
/// Rule: an outbox that is still `Connecting` (no HTTP answer or failure since it was loaded)
/// and has nothing queued has no opinion, because it has nothing to be unsure about; the state
/// then follows the subscription alone, or is `Online` when no thread is followed (nothing is
/// pending and nothing has failed). Once something is queued and unanswered the outbox counts
/// as `Connecting` again, and any answer or failure (the outbox's own `Online` / `Offline`)
/// always counts.
pub fn banner_state(subscription: Option<Connectivity>, outbox: &Outbox) -> Connectivity {
    let link = outbox.connectivity();
    let idle = link == Connectivity::Connecting && outbox.entries().next().is_none();
    match (subscription, idle) {
        (Some(sub), true) => sub,
        (None, true) => Connectivity::Online,
        (Some(sub), false) => sub.combine(link),
        (None, false) => link,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::sync::Mutex;

    use super::Connectivity;
    use crate::cache::Cache;
    use crate::outbox::{
        Clock, FixedRandom, Intent, ManualClock, Outbox, PostRequest, PostResponse, RetryPolicy,
        Transport, TransportError,
    };

    /// Answers each POST with the next scripted result; a POST with no script left fails.
    struct Scripted(Mutex<VecDeque<Result<PostResponse, TransportError>>>);

    impl Scripted {
        fn new(replies: Vec<Result<PostResponse, TransportError>>) -> Self {
            Self(Mutex::new(replies.into()))
        }
    }

    impl Transport for Scripted {
        fn post(
            &self,
            _request: PostRequest,
        ) -> impl Future<Output = Result<PostResponse, TransportError>> + Send {
            let next = self
                .0
                .lock()
                .expect("script lock")
                .pop_front()
                .unwrap_or_else(|| Err(TransportError("script exhausted".to_owned())));
            async move { next }
        }
    }

    fn answered(status: u16) -> Result<PostResponse, TransportError> {
        Ok(PostResponse {
            status,
            body: serde_json::json!({}),
        })
    }

    fn cancel() -> Intent {
        Intent::Cancel {
            thread_id: "th_1".to_owned(),
            task_id: "tk_1".to_owned(),
        }
    }

    #[tokio::test]
    async fn transport_failure_goes_offline_and_the_next_answer_brings_it_back() {
        let cache = Cache::open_in_memory().expect("cache");
        let clock = ManualClock::new(1_000);
        // Full jitter pinned to the top of the window: the first retry is 500 ms out.
        let mut rng = FixedRandom(u64::MAX);
        let mut outbox = Outbox::load(&cache, RetryPolicy::default()).expect("loads");
        assert_eq!(outbox.connectivity(), Connectivity::Connecting);
        outbox
            .enqueue(&cache, cancel(), clock.now_ms())
            .expect("enqueues");

        let down = Scripted::new(vec![Err(TransportError("connection refused".to_owned()))]);
        let report = outbox
            .flush(&cache, &down, "http://hub", &clock, &mut rng)
            .await
            .expect("flushes");
        assert_eq!(report.transport_failures, 1);
        assert_eq!(outbox.connectivity(), Connectivity::Offline);
        assert!(outbox.connectivity().banner().is_some());

        // Not due yet: no POST is made, and the link stays offline.
        clock.advance(499);
        let idle = outbox
            .flush(&cache, &down, "http://hub", &clock, &mut rng)
            .await
            .expect("flushes");
        assert_eq!(idle.sent, 0);
        assert_eq!(outbox.connectivity(), Connectivity::Offline);

        // Due: the hub answers, so the link is online and the banner goes away.
        clock.advance(1);
        let up = Scripted::new(vec![answered(200)]);
        let report = outbox
            .flush(&cache, &up, "http://hub", &clock, &mut rng)
            .await
            .expect("flushes");
        assert_eq!(report.done, 1);
        assert_eq!(outbox.connectivity(), Connectivity::Online);
        assert_eq!(outbox.connectivity().banner(), None);
    }

    #[tokio::test]
    async fn an_error_status_from_a_reachable_hub_is_online() {
        let cache = Cache::open_in_memory().expect("cache");
        let clock = ManualClock::new(0);
        let mut rng = FixedRandom(u64::MAX);
        let mut outbox = Outbox::load(&cache, RetryPolicy::default()).expect("loads");
        outbox
            .enqueue(&cache, cancel(), clock.now_ms())
            .expect("enqueues");

        let down = Scripted::new(vec![Err(TransportError("timeout".to_owned()))]);
        outbox
            .flush(&cache, &down, "http://hub", &clock, &mut rng)
            .await
            .expect("flushes");
        assert_eq!(outbox.connectivity(), Connectivity::Offline);

        // A 503 is an answer: the hub is reachable, the request is retried later.
        clock.advance(1_000);
        let answer = Scripted::new(vec![answered(503)]);
        let report = outbox
            .flush(&cache, &answer, "http://hub", &clock, &mut rng)
            .await
            .expect("flushes");
        assert_eq!(report.retrying, 1);
        assert_eq!(outbox.connectivity(), Connectivity::Online);
    }
}
