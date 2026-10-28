//! The notification logic over the [`Db`] trait, so it runs natively in the unit tests: the
//! subscription and prefs endpoints, and the queue consumer's per-job work ([`process_job`]).

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::job::{Kind, Notification, NotifyJob};
use super::prefs::Prefs;
use super::provider::{Delivery, ProviderKind, Providers, PushProvider};
use super::store::{self, NewSubscription};
use super::webpush::{endpoint_origin, validate_keys};
use crate::audit::{self, Event};
use crate::auth::Principal;
use crate::auth::token::new_id;
use crate::db::{Db, DbError, Role};
use crate::threads::ApiError;

/// Longest endpoint URL accepted (push services use a few hundred characters).
const MAX_ENDPOINT_LEN: usize = 2048;

fn db_failure(error: DbError) -> ApiError {
    ApiError::internal(format!("notify: {error}"))
}

/// Whether `endpoint` may receive pushes: an `https` URL with a public host name. With
/// `dev_fixtures` an `http` URL on a loopback address is allowed too, so the live test can serve
/// a fake push service. This keeps a member from pointing the hub at internal addresses.
pub fn validate_endpoint(endpoint: &str, dev_fixtures: bool) -> Result<(), String> {
    if endpoint.len() > MAX_ENDPOINT_LEN || endpoint.chars().any(|c| c.is_control() || c == ' ') {
        return Err("endpoint is not a valid URL".to_owned());
    }
    let origin = endpoint_origin(endpoint).ok_or("endpoint must be an absolute URL")?;
    let (scheme, authority) = origin.split_once("://").unwrap_or_default();
    if authority.contains('@') {
        return Err("endpoint must not contain credentials".to_owned());
    }
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    }
    .to_ascii_lowercase();
    let loopback = host == "localhost" || host == "::1" || host.starts_with("127.");
    match scheme {
        "https" => {}
        "http" if dev_fixtures && loopback => return Ok(()),
        _ => return Err("endpoint must be an https URL".to_owned()),
    }
    let ipv4 = host.split('.').count() == 4 && host.split('.').all(|p| p.parse::<u8>().is_ok());
    let internal = host.is_empty()
        || host.contains(':')
        || ipv4
        || loopback
        || !host.contains('.')
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".localhost");
    if internal {
        return Err("endpoint must name a public host".to_owned());
    }
    Ok(())
}

/// `POST /v1/push/subscriptions` (member): `{endpoint, keys: {p256dh, auth}}`. `201` for a new
/// endpoint, `200` when the endpoint was already registered (its owner, device and keys are
/// updated). The subscription belongs to the caller's user and device.
pub async fn subscribe(
    db: &impl Db,
    principal: &Principal,
    body: &Value,
    dev_fixtures: bool,
    request_id: &str,
) -> Result<(u16, Value), ApiError> {
    principal.require(Role::Member)?;
    let endpoint = body
        .get("endpoint")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::invalid_request("endpoint is required"))?;
    let keys = body.get("keys");
    let key = |name: &str| {
        keys.and_then(|keys| keys.get(name))
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::invalid_request(format!("keys.{name} is required")))
    };
    let (p256dh, auth) = (key("p256dh")?, key("auth")?);
    validate_endpoint(endpoint, dev_fixtures).map_err(ApiError::invalid_request)?;
    validate_keys(p256dh, auth)
        .map_err(|error| ApiError::invalid_request(format!("keys: {error}")))?;

    let id = new_id("psub")?;
    let (id, created) = store::upsert_subscription(
        db,
        NewSubscription {
            id: &id,
            org_id: &principal.org_id,
            user_id: &principal.user_id,
            device_id: &principal.device_id,
            provider: ProviderKind::WebPush,
            endpoint,
            p256dh: Some(p256dh),
            auth: Some(auth),
        },
    )
    .await
    .map_err(db_failure)?;
    // The endpoint is a capability URL: the audit row names the subscription, not the endpoint.
    audit::record(
        db,
        &Event::by(
            principal,
            "push.subscribe",
            "push_subscription",
            &id,
            request_id,
            json!({ "provider": "webpush", "created": created }),
        ),
    )
    .await?;
    Ok((
        if created { 201 } else { 200 },
        json!({ "id": id, "provider": "webpush", "device_id": principal.device_id }),
    ))
}

/// `DELETE /v1/push/subscriptions` (member): `{endpoint}`. Removes the caller's own subscription;
/// `204` also when there was none, so a retry is harmless.
pub async fn unsubscribe(
    db: &impl Db,
    principal: &Principal,
    body: &Value,
    request_id: &str,
) -> Result<(), ApiError> {
    principal.require(Role::Member)?;
    let endpoint = body
        .get("endpoint")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::invalid_request("endpoint is required"))?;
    let removed = store::delete_own_subscription(db, &principal.user_id, endpoint)
        .await
        .map_err(db_failure)?;
    audit::record(
        db,
        &Event::by(
            principal,
            "push.unsubscribe",
            "push_subscription",
            "",
            request_id,
            json!({ "removed": removed }),
        ),
    )
    .await?;
    Ok(())
}

/// `GET /v1/push/prefs` (member).
pub async fn get_prefs(db: &impl Db, principal: &Principal) -> Result<Value, ApiError> {
    principal.require(Role::Member)?;
    let prefs = store::load_prefs(db, &principal.user_id, &principal.org_id)
        .await
        .map_err(db_failure)?;
    Ok(prefs.to_json())
}

/// `PUT /v1/push/prefs` (member): replaces the caller's prefs (see [`Prefs`] for the shape).
pub async fn put_prefs(
    db: &impl Db,
    principal: &Principal,
    body: &Value,
    now_ms: u64,
    request_id: &str,
) -> Result<Value, ApiError> {
    principal.require(Role::Member)?;
    let prefs = Prefs::from_json(body, now_ms).map_err(ApiError::invalid_request)?;
    store::save_prefs(db, &principal.user_id, &principal.org_id, &prefs)
        .await
        .map_err(db_failure)?;
    audit::record(
        db,
        &Event::by(
            principal,
            "push.prefs.update",
            "user",
            &principal.user_id,
            request_id,
            json!({ "muted_threads": prefs.muted_threads.len() }),
        ),
    )
    .await?;
    Ok(prefs.to_json())
}

/// What one job did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobReport {
    pub delivered: usize,
    /// Skipped by prefs (class off, thread muted, quiet hours) or by role.
    pub skipped: usize,
    /// Subscriptions deleted because the push service said they are gone.
    pub removed: usize,
    /// Sends that failed for good, or went to a provider that is not configured.
    pub failed: usize,
    /// Subscriptions that failed temporarily (5xx, 429, network) and still need the notification:
    /// the queue re-sends the job with only these ([`NotifyJob::follow_up`]).
    pub pending: Vec<String>,
}

/// The consumer's work for one job: find the thread's org, then for every deliverable
/// subscription apply the user's prefs and send through the provider of its kind.
///
/// Reads come first, then the sends, so a database error (`Err`, retried by the queue as a whole)
/// happens before anything was sent. A delivered subscription is never sent to again: the
/// follow-up job carries only `report.pending`. A subscription whose deletion fails after a
/// `410` is kept pending as well, since a send to a gone endpoint reaches nobody.
pub async fn process_job<D, W, A, F>(
    db: &D,
    providers: &Providers<W, A, F>,
    job: &NotifyJob,
    now_ms: u64,
) -> Result<JobReport, DbError>
where
    D: Db,
    W: PushProvider,
    A: PushProvider,
    F: PushProvider,
{
    let mut report = JobReport::default();
    let Some(org_id) = store::org_of_thread(db, &job.thread_id).await? else {
        return Ok(report);
    };
    let mut to_send = Vec::new();
    let mut prefs_by_user: HashMap<String, Prefs> = HashMap::new();
    // A machine back is announced to each user once, whatever the number of their devices.
    let mut announced: HashSet<String> = HashSet::new();
    for target in store::targets(db, &org_id).await? {
        let subscription = &target.subscription;
        // A follow-up job sends to its pending subscriptions only.
        if job
            .subscriptions
            .as_ref()
            .is_some_and(|ids| !ids.contains(&subscription.id))
        {
            continue;
        }
        // Only someone who may answer an approval is asked to.
        if job.kind == Kind::ApprovalNeeded && !target.role.allows(Role::Member) {
            report.skipped += 1;
            continue;
        }
        if !prefs_by_user.contains_key(&subscription.user_id) {
            let prefs = store::load_prefs(db, &subscription.user_id, &org_id).await?;
            prefs_by_user.insert(subscription.user_id.clone(), prefs);
        }
        let prefs = &prefs_by_user[&subscription.user_id];
        if prefs.decide(job.kind, &job.thread_id, now_ms).is_err() {
            report.skipped += 1;
            continue;
        }
        if job.kind == Kind::MachineBack && !announced.insert(subscription.user_id.clone()) {
            report.skipped += 1;
            continue;
        }
        to_send.push(target.subscription);
    }

    let notification = Notification::new(job.clone());
    for subscription in to_send {
        match providers
            .send(&subscription, &notification, now_ms / 1000)
            .await
        {
            Delivery::Delivered => report.delivered += 1,
            Delivery::Gone => match store::delete_subscription(db, &subscription.id).await {
                Ok(()) => report.removed += 1,
                Err(_) => report.pending.push(subscription.id),
            },
            Delivery::Retry(_) => report.pending.push(subscription.id),
            Delivery::Failed(_) | Delivery::NotConfigured => report.failed += 1,
        }
    }
    Ok(report)
}

/// Which jobs of one queue batch to run: for each task (a thread without a task id counts as its
/// own), only the last job. A run that asked for approval and finished in the same batch needs
/// one notification, the latest. Everything else is acknowledged without sending.
pub fn collapse_batch(jobs: &[NotifyJob]) -> Vec<bool> {
    let key = |job: &NotifyJob| (job.thread_id.clone(), job.task_id.clone());
    let mut last: HashMap<(String, Option<String>), usize> = HashMap::new();
    for (index, job) in jobs.iter().enumerate() {
        last.insert(key(job), index);
    }
    jobs.iter()
        .enumerate()
        .map(|(index, job)| last.get(&key(job)) == Some(&index))
        .collect()
}
