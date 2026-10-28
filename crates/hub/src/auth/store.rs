//! The SQL behind auth: sessions, the bootstrap claim, and the identity rows a session needs.
//! Times are computed by SQLite (`strftime ... 'now'`), so no clock is needed in Rust and the
//! native fake and D1 agree.

use super::{DEV_DEVICE_ID, DEV_ORG_ID, DEV_USER_ID, Principal};
use crate::db::{Db, DbError, Param, Role, Row, opt_text, text};

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

fn role_of(row: &Row) -> Result<Role, DbError> {
    text(row, "role")?
        .parse::<Role>()
        .map_err(|error| DbError::Invalid(error.to_string()))
}

/// Creates the dev org, user, owner membership and device if they are missing.
pub async fn ensure_dev_principal(db: &impl Db) -> Result<(), DbError> {
    let rows = db
        .query(
            "SELECT role FROM members WHERE org_id = ? AND user_id = ?",
            &[DEV_ORG_ID.into(), DEV_USER_ID.into()],
        )
        .await?;
    if !rows.is_empty() {
        return Ok(());
    }
    db.execute(
        "INSERT OR IGNORE INTO orgs (id, slug, name) VALUES (?, 'dev-fixtures', 'Dev fixtures')",
        &[DEV_ORG_ID.into()],
    )
    .await?;
    db.execute(
        "INSERT OR IGNORE INTO users (id, display_name) VALUES (?, '')",
        &[DEV_USER_ID.into()],
    )
    .await?;
    db.execute(
        "INSERT OR IGNORE INTO members (org_id, user_id, role) VALUES (?, ?, 'owner')",
        &[DEV_ORG_ID.into(), DEV_USER_ID.into()],
    )
    .await?;
    db.execute(
        "INSERT OR IGNORE INTO devices (id, org_id, user_id, name, trusted) VALUES (?, ?, ?, 'dev', 1)",
        &[DEV_DEVICE_ID.into(), DEV_ORG_ID.into(), DEV_USER_ID.into()],
    )
    .await?;
    Ok(())
}

/// The live session with this token hash (not expired, not revoked, device not revoked, user
/// still a member), with the stored hash for a constant-time re-check.
pub async fn find_by_hash(
    db: &impl Db,
    hash: &str,
) -> Result<Option<(String, Principal)>, DbError> {
    find_live(db, "s.token_hash = ?", hash).await
}

/// The live session with this id (same conditions as [`find_by_hash`]).
pub async fn find_by_session_id(
    db: &impl Db,
    session_id: &str,
) -> Result<Option<Principal>, DbError> {
    Ok(find_live(db, "s.id = ?", session_id)
        .await?
        .map(|(_, principal)| principal))
}

async fn find_live(
    db: &impl Db,
    condition: &str,
    value: &str,
) -> Result<Option<(String, Principal)>, DbError> {
    let sql = format!(
        "SELECT s.id AS session_id, s.token_hash, s.user_id, s.device_id, s.org_id, \
                u.display_name, m.role, s.role_cap \
         FROM sessions s \
         JOIN users u ON u.id = s.user_id \
         JOIN members m ON m.org_id = s.org_id AND m.user_id = s.user_id \
         JOIN devices d ON d.id = s.device_id \
         WHERE {condition} AND s.revoked_at IS NULL AND s.expires_at > {NOW} \
           AND d.revoked_at IS NULL"
    );
    let rows = db.query(&sql, &[value.into()]).await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    // A session may be capped below the member's role (a browser sign-in); a demotion still
    // applies at once because the member's role is read on every request.
    let mut role = role_of(row)?;
    if let Some(cap) = opt_text(row, "role_cap")? {
        let cap = cap
            .parse::<Role>()
            .map_err(|error| DbError::Invalid(error.to_string()))?;
        role = role.min(cap);
    }
    Ok(Some((
        text(row, "token_hash")?,
        Principal {
            user_id: text(row, "user_id")?,
            device_id: text(row, "device_id")?,
            org_id: text(row, "org_id")?,
            role,
            display_name: text(row, "display_name")?,
            session_id: Some(text(row, "session_id")?),
        },
    )))
}

/// Whether the one-time bootstrap has been claimed.
pub async fn bootstrap_claimed(db: &impl Db) -> Result<bool, DbError> {
    Ok(!db
        .query("SELECT id FROM auth_bootstrap WHERE id = 1", &[])
        .await?
        .is_empty())
}

/// Claims the bootstrap. The primary key makes exactly one caller win; the loser gets an error
/// (check [`bootstrap_claimed`] to tell that from a database failure).
pub async fn claim_bootstrap(db: &impl Db) -> Result<(), DbError> {
    db.execute("INSERT INTO auth_bootstrap (id) VALUES (1)", &[])
        .await
        .map(|_| ())
}

pub async fn finish_bootstrap(db: &impl Db, org_id: &str, user_id: &str) -> Result<(), DbError> {
    db.execute(
        "UPDATE auth_bootstrap SET org_id = ?, user_id = ? WHERE id = 1",
        &[org_id.into(), user_id.into()],
    )
    .await
    .map(|_| ())
}

/// Undo of a failed bootstrap: the org (cascades members, devices, sessions), the user, the claim.
pub async fn release_bootstrap(db: &impl Db, org_id: &str, user_id: &str) -> Result<(), DbError> {
    db.execute("DELETE FROM users WHERE id = ?", &[user_id.into()])
        .await?;
    db.execute("DELETE FROM orgs WHERE id = ?", &[org_id.into()])
        .await?;
    db.execute("DELETE FROM auth_bootstrap WHERE id = 1", &[])
        .await
        .map(|_| ())
}

pub struct NewOrgRow<'a> {
    pub id: &'a str,
    pub slug: &'a str,
    pub name: &'a str,
}

pub async fn insert_org(db: &impl Db, org: &NewOrgRow<'_>) -> Result<(), DbError> {
    db.execute(
        "INSERT INTO orgs (id, slug, name) VALUES (?, ?, ?)",
        &[org.id.into(), org.slug.into(), org.name.into()],
    )
    .await
    .map(|_| ())
}

pub async fn insert_user(
    db: &impl Db,
    id: &str,
    display_name: &str,
    email: Option<&str>,
) -> Result<(), DbError> {
    db.execute(
        "INSERT INTO users (id, display_name, email) VALUES (?, ?, ?)",
        &[id.into(), display_name.into(), Param::opt(email)],
    )
    .await
    .map(|_| ())
}

pub async fn insert_member(
    db: &impl Db,
    org_id: &str,
    user_id: &str,
    role: Role,
) -> Result<(), DbError> {
    db.execute(
        "INSERT INTO members (org_id, user_id, role) VALUES (?, ?, ?)",
        &[org_id.into(), user_id.into(), role.as_str().into()],
    )
    .await
    .map(|_| ())
}

pub async fn insert_device(
    db: &impl Db,
    id: &str,
    org_id: &str,
    user_id: &str,
    name: &str,
) -> Result<(), DbError> {
    db.execute(
        "INSERT INTO devices (id, org_id, user_id, name) VALUES (?, ?, ?, ?)",
        &[id.into(), org_id.into(), user_id.into(), name.into()],
    )
    .await
    .map(|_| ())
}

/// Stores a session (the hash only) that expires `ttl_seconds` from now; returns `expires_at`.
pub async fn insert_session(
    db: &impl Db,
    id: &str,
    token_hash: &str,
    user_id: &str,
    device_id: &str,
    org_id: &str,
    ttl_seconds: i64,
) -> Result<String, DbError> {
    let sql = "INSERT INTO sessions (id, token_hash, user_id, device_id, org_id, expires_at) \
         VALUES (?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?))";
    db.execute(
        sql,
        &[
            id.into(),
            token_hash.into(),
            user_id.into(),
            device_id.into(),
            org_id.into(),
            format!("+{ttl_seconds} seconds").into(),
        ],
    )
    .await?;
    let rows = db
        .query("SELECT expires_at FROM sessions WHERE id = ?", &[id.into()])
        .await?;
    rows.first()
        .map(|row| text(row, "expires_at"))
        .transpose()?
        .ok_or_else(|| DbError::Decode("session vanished after insert".to_owned()))
}

/// Caps the effective role of a session (see [`find_by_hash`]).
pub async fn set_role_cap(db: &impl Db, id: &str, cap: Role) -> Result<(), DbError> {
    db.execute(
        "UPDATE sessions SET role_cap = ? WHERE id = ?",
        &[cap.as_str().into(), id.into()],
    )
    .await
    .map(|_| ())
}

/// Whether the org has a session with this id.
pub async fn session_in_org(db: &impl Db, id: &str, org_id: &str) -> Result<bool, DbError> {
    Ok(!db
        .query(
            "SELECT id FROM sessions WHERE id = ? AND org_id = ?",
            &[id.into(), org_id.into()],
        )
        .await?
        .is_empty())
}

/// Soft-revokes a session (idempotent: an already revoked one keeps its first time).
pub async fn revoke_session(db: &impl Db, id: &str) -> Result<(), DbError> {
    let sql = format!("UPDATE sessions SET revoked_at = {NOW} WHERE id = ? AND revoked_at IS NULL");
    db.execute(&sql, &[id.into()]).await.map(|_| ())
}

/// Hard delete of one device (undo of a failed browser sign-in).
pub async fn delete_device(db: &impl Db, id: &str) -> Result<(), DbError> {
    db.execute("DELETE FROM devices WHERE id = ?", &[id.into()])
        .await
        .map(|_| ())
}

/// Hard delete of a user and, by cascade, their membership, devices and sessions (undo of a
/// failed issue).
pub async fn delete_user(db: &impl Db, id: &str) -> Result<(), DbError> {
    db.execute("DELETE FROM users WHERE id = ?", &[id.into()])
        .await
        .map(|_| ())
}
