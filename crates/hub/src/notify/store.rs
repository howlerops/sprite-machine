//! D1 access for subscriptions and prefs (migrations 0002 `notification_prefs` and 0010).

use std::collections::BTreeSet;

use super::job::Kind;
use super::prefs::{Prefs, QuietHours};
use super::provider::{ProviderKind, Subscription};
use crate::db::{Db, DbError, Param, Role, Row, int, opt_text, text};

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

/// A subscription to store.
#[derive(Debug, Clone, Copy)]
pub struct NewSubscription<'a> {
    pub id: &'a str,
    pub org_id: &'a str,
    pub user_id: &'a str,
    pub device_id: &'a str,
    pub provider: ProviderKind,
    pub endpoint: &'a str,
    pub p256dh: Option<&'a str>,
    pub auth: Option<&'a str>,
}

/// Stores a subscription, idempotent per endpoint: an endpoint that is already known keeps its
/// id and gets the new owner, device and keys. Returns the id and whether the row is new.
pub async fn upsert_subscription(
    db: &impl Db,
    new: NewSubscription<'_>,
) -> Result<(String, bool), DbError> {
    let existing = db
        .query(
            "SELECT id FROM push_subscriptions WHERE endpoint = ?",
            &[new.endpoint.into()],
        )
        .await?;
    if let Some(row) = existing.first() {
        let id = text(row, "id")?;
        let sql = format!(
            "UPDATE push_subscriptions SET org_id = ?, user_id = ?, device_id = ?, provider = ?, \
             p256dh = ?, auth = ?, updated_at = {NOW} WHERE id = ?"
        );
        db.execute(
            &sql,
            &[
                new.org_id.into(),
                new.user_id.into(),
                new.device_id.into(),
                new.provider.as_str().into(),
                Param::opt(new.p256dh),
                Param::opt(new.auth),
                id.as_str().into(),
            ],
        )
        .await?;
        return Ok((id, false));
    }
    let sql = format!(
        "INSERT INTO push_subscriptions \
         (id, org_id, user_id, device_id, provider, endpoint, p256dh, auth, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, {NOW})"
    );
    db.execute(
        &sql,
        &[
            new.id.into(),
            new.org_id.into(),
            new.user_id.into(),
            new.device_id.into(),
            new.provider.as_str().into(),
            new.endpoint.into(),
            Param::opt(new.p256dh),
            Param::opt(new.auth),
        ],
    )
    .await?;
    Ok((new.id.to_owned(), true))
}

/// Deletes the caller's own subscription with this endpoint. Returns whether one was removed.
pub async fn delete_own_subscription(
    db: &impl Db,
    user_id: &str,
    endpoint: &str,
) -> Result<bool, DbError> {
    let changed = db
        .execute(
            "DELETE FROM push_subscriptions WHERE user_id = ? AND endpoint = ?",
            &[user_id.into(), endpoint.into()],
        )
        .await?;
    Ok(changed > 0)
}

/// Deletes a subscription the push service said is gone (404, 410).
pub async fn delete_subscription(db: &impl Db, id: &str) -> Result<(), DbError> {
    db.execute("DELETE FROM push_subscriptions WHERE id = ?", &[id.into()])
        .await?;
    Ok(())
}

/// The org that owns a thread (`thread_hosts`, 0006), or `None` for an unmapped thread.
pub async fn org_of_thread(db: &impl Db, thread_id: &str) -> Result<Option<String>, DbError> {
    let rows = db
        .query(
            "SELECT org_id FROM thread_hosts WHERE thread_id = ?",
            &[thread_id.into()],
        )
        .await?;
    rows.first().map(|row| text(row, "org_id")).transpose()
}

/// The name of a machine of the org (`machines`, 0004), or `None` for an unknown one (a dev
/// machine has no row).
pub async fn machine_name(
    db: &impl Db,
    org_id: &str,
    machine_id: &str,
) -> Result<Option<String>, DbError> {
    let rows = db
        .query(
            "SELECT name FROM machines WHERE id = ? AND org_id = ?",
            &[machine_id.into(), org_id.into()],
        )
        .await?;
    rows.first().map(|row| text(row, "name")).transpose()
}

/// A subscription and the role its user has in the org now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub subscription: Subscription,
    pub role: Role,
}

/// The deliverable subscriptions of an org: the user is still a member and the registering
/// device is not revoked.
pub async fn targets(db: &impl Db, org_id: &str) -> Result<Vec<Target>, DbError> {
    let rows = db
        .query(
            "SELECT s.id, s.user_id, s.provider, s.endpoint, s.p256dh, s.auth, m.role \
             FROM push_subscriptions s \
             JOIN members m ON m.org_id = s.org_id AND m.user_id = s.user_id \
             JOIN devices d ON d.id = s.device_id AND d.revoked_at IS NULL \
             WHERE s.org_id = ? ORDER BY s.created_at, s.id",
            &[org_id.into()],
        )
        .await?;
    rows.iter().map(target_from_row).collect()
}

fn target_from_row(row: &Row) -> Result<Target, DbError> {
    let provider = text(row, "provider")?;
    let role = text(row, "role")?;
    Ok(Target {
        subscription: Subscription {
            id: text(row, "id")?,
            user_id: text(row, "user_id")?,
            provider: ProviderKind::parse(&provider)
                .ok_or_else(|| DbError::Invalid(format!("provider {provider:?}")))?,
            endpoint: text(row, "endpoint")?,
            p256dh: opt_text(row, "p256dh")?,
            auth: opt_text(row, "auth")?,
        },
        role: role
            .parse()
            .map_err(|error: crate::db::UnknownRole| DbError::Invalid(error.to_string()))?,
    })
}

/// A user's prefs in one org; the defaults if nothing is stored.
pub async fn load_prefs(db: &impl Db, user_id: &str, org_id: &str) -> Result<Prefs, DbError> {
    let mut prefs = Prefs::default();
    let rows = db
        .query(
            "SELECT event_class, muted, quiet_start_min, quiet_end_min, quiet_tz \
             FROM notification_prefs WHERE user_id = ? AND org_id = ?",
            &[user_id.into(), org_id.into()],
        )
        .await?;
    for row in &rows {
        if let Some(kind) = Kind::parse(&text(row, "event_class")?) {
            prefs.enabled.insert(kind, int(row, "muted")? == 0);
        }
        // Every class row repeats the quiet hours; any row that has them will do.
        let start = row
            .get("quiet_start_min")
            .and_then(serde_json::Value::as_i64);
        let end = row.get("quiet_end_min").and_then(serde_json::Value::as_i64);
        if let (Some(start), Some(end)) = (start, end) {
            prefs.quiet_hours = Some(QuietHours {
                start_min: u16::try_from(start).map_err(|e| DbError::Decode(e.to_string()))?,
                end_min: u16::try_from(end).map_err(|e| DbError::Decode(e.to_string()))?,
                tz: text(row, "quiet_tz")?,
            });
        }
    }
    let mutes = db
        .query(
            "SELECT thread_id FROM notification_thread_mutes WHERE user_id = ? AND org_id = ?",
            &[user_id.into(), org_id.into()],
        )
        .await?;
    prefs.muted_threads = mutes
        .iter()
        .map(|row| text(row, "thread_id"))
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(prefs)
}

/// Replaces a user's prefs in one org.
pub async fn save_prefs(
    db: &impl Db,
    user_id: &str,
    org_id: &str,
    prefs: &Prefs,
) -> Result<(), DbError> {
    let (start, end, zone) = match &prefs.quiet_hours {
        Some(quiet) => (
            Param::Int(i64::from(quiet.start_min)),
            Param::Int(i64::from(quiet.end_min)),
            quiet.tz.clone(),
        ),
        None => (Param::Null, Param::Null, "UTC".to_owned()),
    };
    let sql = format!(
        "INSERT INTO notification_prefs \
         (user_id, org_id, event_class, muted, quiet_start_min, quiet_end_min, quiet_tz, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, {NOW}) \
         ON CONFLICT (user_id, org_id, event_class) DO UPDATE SET \
         muted = excluded.muted, quiet_start_min = excluded.quiet_start_min, \
         quiet_end_min = excluded.quiet_end_min, quiet_tz = excluded.quiet_tz, \
         updated_at = excluded.updated_at"
    );
    for kind in Kind::ALL {
        db.execute(
            &sql,
            &[
                user_id.into(),
                org_id.into(),
                kind.as_str().into(),
                Param::from(!prefs.is_enabled(kind)),
                start.clone(),
                end.clone(),
                zone.as_str().into(),
            ],
        )
        .await?;
    }
    db.execute(
        "DELETE FROM notification_thread_mutes WHERE user_id = ? AND org_id = ?",
        &[user_id.into(), org_id.into()],
    )
    .await?;
    for thread_id in &prefs.muted_threads {
        db.execute(
            "INSERT OR IGNORE INTO notification_thread_mutes (user_id, org_id, thread_id) \
             VALUES (?, ?, ?)",
            &[user_id.into(), org_id.into(), thread_id.as_str().into()],
        )
        .await?;
    }
    Ok(())
}
