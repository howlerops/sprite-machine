//! Push providers behind one trait.
//!
//! [`PushProvider`] sends one [`Notification`] to one stored [`Subscription`] and says what
//! happened ([`Delivery`]). [`WebPush`] is the real one (RFC 8291 + 8292 over an [`HttpSender`]);
//! [`Apns`] and [`Fcm`] are stubs that answer [`Delivery::NotConfigured`] until the native apps
//! exist (P3 desktop/iOS, P7). Tests give providers and senders fakes; the Worker's sender is
//! `queues_notify::FetchSender`.

use std::future::Future;

use super::job::{Notification, TTL_SECS};
use super::webpush::{self, Vapid, WebPushError};

/// A stored push subscription (one row of `push_subscriptions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub id: String,
    pub user_id: String,
    pub provider: ProviderKind,
    /// The push service URL (Web Push) or the device token (APNs, FCM).
    pub endpoint: String,
    pub p256dh: Option<String>,
    pub auth: Option<String>,
}

/// Which provider delivers a subscription (`push_subscriptions.provider`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    WebPush,
    Apns,
    Fcm,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::WebPush => "webpush",
            ProviderKind::Apns => "apns",
            ProviderKind::Fcm => "fcm",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [Self::WebPush, Self::Apns, Self::Fcm]
            .into_iter()
            .find(|kind| kind.as_str() == text)
    }
}

/// The result of one send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// The push service accepted the message (2xx).
    Delivered,
    /// The subscription is gone (404 or 410): delete it.
    Gone,
    /// A temporary failure (5xx, 429, a network error): let the queue retry.
    Retry(String),
    /// A permanent failure for this message (other 4xx, bad keys): drop it, keep the subscription.
    Failed(String),
    /// The provider has no credentials or is not built yet.
    NotConfigured,
}

/// Sends one notification to one subscription. No `Send` bound: Workers are single-threaded.
pub trait PushProvider {
    fn send(
        &self,
        subscription: &Subscription,
        notification: &Notification,
        now_secs: u64,
    ) -> impl Future<Output = Delivery>;
}

/// One outgoing HTTP request to a push service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// POSTs a [`PushRequest`] and returns the status code. `Err` is a network failure.
pub trait HttpSender {
    fn post(&self, request: PushRequest) -> impl Future<Output = Result<u16, String>>;
}

/// Web Push (RFC 8030 + 8291 + 8292).
pub struct WebPush<H> {
    pub http: H,
    pub vapid: Option<Vapid>,
}

impl<H: HttpSender> WebPush<H> {
    /// The request for `subscription`: encrypted body and the headers a push service expects.
    pub fn build_request(
        &self,
        subscription: &Subscription,
        notification: &Notification,
        now_secs: u64,
    ) -> Result<PushRequest, WebPushError> {
        let vapid = self.vapid.as_ref().ok_or(WebPushError::BadVapidKey)?;
        let key = subscription
            .p256dh
            .as_deref()
            .and_then(webpush::b64_decode)
            .ok_or(WebPushError::BadPublicKey)?;
        let auth = subscription
            .auth
            .as_deref()
            .and_then(webpush::b64_decode)
            .ok_or(WebPushError::BadAuthSecret)?;
        let plaintext = notification.payload().to_string();
        let body = webpush::encrypt(plaintext.as_bytes(), &key, &auth)?;
        let authorization = vapid.authorization(&subscription.endpoint, now_secs)?;
        Ok(PushRequest {
            url: subscription.endpoint.clone(),
            headers: vec![
                ("Authorization".to_owned(), authorization),
                ("Content-Encoding".to_owned(), "aes128gcm".to_owned()),
                (
                    "Content-Type".to_owned(),
                    "application/octet-stream".to_owned(),
                ),
                ("TTL".to_owned(), TTL_SECS.to_string()),
                (
                    "Urgency".to_owned(),
                    notification.urgency().as_str().to_owned(),
                ),
                ("Topic".to_owned(), notification.topic()),
            ],
            body,
        })
    }
}

impl<H: HttpSender> PushProvider for WebPush<H> {
    async fn send(
        &self,
        subscription: &Subscription,
        notification: &Notification,
        now_secs: u64,
    ) -> Delivery {
        if self.vapid.is_none() {
            return Delivery::NotConfigured;
        }
        let request = match self.build_request(subscription, notification, now_secs) {
            Ok(request) => request,
            Err(error) => return Delivery::Failed(error.to_string()),
        };
        match self.http.post(request).await {
            Ok(status) => classify_status(status),
            Err(error) => Delivery::Retry(error),
        }
    }
}

/// RFC 8030 §5: 2xx accepted, 404 and 410 gone, 429 and 5xx retry, any other status fails.
pub fn classify_status(status: u16) -> Delivery {
    match status {
        200..=299 => Delivery::Delivered,
        404 | 410 => Delivery::Gone,
        429 | 500..=599 => Delivery::Retry(format!("push service answered {status}")),
        other => Delivery::Failed(format!("push service answered {other}")),
    }
}

/// Apple Push Notification service: not built yet (P3).
#[derive(Debug, Clone, Copy, Default)]
pub struct Apns;

impl PushProvider for Apns {
    async fn send(&self, _: &Subscription, _: &Notification, _: u64) -> Delivery {
        Delivery::NotConfigured
    }
}

/// Firebase Cloud Messaging: not built yet (P7).
#[derive(Debug, Clone, Copy, Default)]
pub struct Fcm;

impl PushProvider for Fcm {
    async fn send(&self, _: &Subscription, _: &Notification, _: u64) -> Delivery {
        Delivery::NotConfigured
    }
}

/// The three providers, picked by a subscription's kind.
pub struct Providers<W, A, F> {
    pub web: W,
    pub apns: A,
    pub fcm: F,
}

impl<W: PushProvider, A: PushProvider, F: PushProvider> Providers<W, A, F> {
    pub async fn send(
        &self,
        subscription: &Subscription,
        notification: &Notification,
        now_secs: u64,
    ) -> Delivery {
        match subscription.provider {
            ProviderKind::WebPush => self.web.send(subscription, notification, now_secs).await,
            ProviderKind::Apns => self.apns.send(subscription, notification, now_secs).await,
            ProviderKind::Fcm => self.fcm.send(subscription, notification, now_secs).await,
        }
    }
}
