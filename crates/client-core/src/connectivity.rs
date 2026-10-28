//! Connectivity as the UI shows it: a banner while the client is not live.

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
