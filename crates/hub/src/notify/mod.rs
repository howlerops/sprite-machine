//! Notifications (M2-011, plan §4.2 "Queues" and "Notifications"): Web Push through the NOTIFY
//! Queue, with prefs and approvals the phone can answer from the notification.
//!
//! # Flow
//!
//! ```text
//! Thread DO appends Interrupt / RunFinished{success|interrupt} / RunError
//!   -> jobs_for(appended)            ids only: {kind, thread_id, task_id, interrupt_id?, option ids?}
//!   -> NOTIFY queue                  (queues/notify.rs `enqueue`; a failed send is logged, never fatal)
//!   -> consumer: process_job         thread -> org (thread_hosts), subscriptions of the org's members
//!        per subscription: role (approvals need member), prefs (class off, thread muted,
//!        quiet hours -> skip), then the provider of the subscription's kind
//!   -> WebPush: RFC 8291 aes128gcm body + RFC 8292 VAPID header, POST to the push service
//!        2xx delivered | 404, 410 delete the subscription | 5xx, 429 pending | other 4xx dropped
//!   -> pending subscriptions only    the job is re-queued with `subscriptions` = the pending ids
//!        (`NotifyJob::follow_up`, at most `MAX_ATTEMPTS` sends); delivered ones are not sent to again
//! ```
//!
//! - [`webpush`]: the crypto (`encrypt`, checked against the RFC 8291 Appendix A vector) and
//!   [`webpush::Vapid`] (ES256 JWT; key from the Worker secret `VAPID_PRIVATE_KEY`, base64url of the
//!   32-byte scalar, never logged). `VAPID_PUBLIC_KEY` in wrangler.toml is a dev placeholder;
//!   the real public key is derived from the secret and served by `GET /v1/push/vapid-public-key`.
//! - [`provider`]: the [`provider::PushProvider`] trait with [`provider::WebPush`] and the APNs
//!   and FCM stubs, which answer `NotConfigured` (P3, P7).
//! - [`prefs`], [`store`], [`service`]: preferences, D1 access and the endpoint logic.
//! - `routes` and `env` (wasm32 only): the axum routes and the Worker configuration.
//!
//! # Collapsing
//!
//! At most one notification is pending per task: the `Topic` header is the task id (hashed to 32
//! base64url characters when it does not fit), so a push service replaces an undelivered message
//! with the next one for the same task, and within one queue batch only the last job of a task
//! is sent (`service::collapse_batch`).
//!
//! # What a push contains
//!
//! Ids and a generic title, nothing else (plan §4.2). Prompt text, tool input, file names and
//! interrupt titles never enter a job or a payload:
//!
//! ```json
//! { "kind": "approval_needed", "title": "Approval needed",
//!   "thread_id": "th_1", "task_id": "tk_1", "interrupt_id": "int_1",
//!   "actions": [ { "action": "allow", "title": "Allow", "option_id": "allow_once" },
//!                { "action": "deny",  "title": "Deny",  "option_id": "reject_once" } ] }
//! ```
//!
//! # Actionable approvals
//!
//! The payload of an approval carries the `interrupt_id` and two actions. The service worker
//! shows them as notification buttons; a tap on `allow` or `deny` makes the phone answer through
//! the route that already exists, with its own session token:
//!
//! ```text
//! POST /v1/threads/{thread_id}/interrupts/{interrupt_id}      (client-api.md §5.4)
//! Authorization: Bearer <session token>      Idempotency-Key: idk_...
//! { "outcome": { "type": "selected", "option_id": "<the action's option_id>" } }
//! ```
//!
//! `option_id` is the id of the interrupt's first allow (or reject) option, copied by the hook
//! from the `Interrupt` event; the ids are not secret and the title and message of the interrupt
//! stay out of the payload. A payload without `actions` (a form or auth interrupt, or a job
//! raised from `RunFinished` alone) is opened in the app instead. There is no new route and no
//! unauthenticated one: the hub checks the session, the org, the member role and
//! "first answer wins" exactly as for an answer typed in the app, and a push can be replayed
//! without effect because the interrupt resolves once.

pub mod job;
pub mod prefs;
pub mod provider;
pub mod service;
pub mod store;
pub mod webpush;

mod tz;

#[cfg(target_arch = "wasm32")]
pub mod env;
#[cfg(target_arch = "wasm32")]
pub mod routes;

#[cfg(test)]
mod tests;
