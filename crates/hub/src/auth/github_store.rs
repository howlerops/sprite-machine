//! The SQL behind GitHub sign-in (migration 0014): authorize requests, the hub's one-time codes,
//! the GitHub identity columns of `users`, and invite acceptance. Times are epoch milliseconds
//! passed in by the caller.

use super::github::{GithubUser, OpenInvite, Pending};
use crate::db::{Db, DbError, Param, Role, Row, opt_text, text};

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

pub struct NewRequest<'a> {
    pub id: &'a str,
    pub state_hash: &'a str,
    pub github_verifier: &'a str,
    pub redirect_uri: &'a str,
    pub challenge: &'a str,
    pub app_state: &'a str,
    pub org_id: Option<&'a str>,
    pub now_ms: i64,
    pub expires_at_ms: i64,
}

pub struct NewCode<'a> {
    pub id: &'a str,
    pub code_hash: &'a str,
    pub user_id: &'a str,
    pub org_id: &'a str,
    pub redirect_uri: &'a str,
    pub challenge: &'a str,
    pub invite_ids: &'a str,
    pub now_ms: i64,
    pub expires_at_ms: i64,
}

/// A stored hub code (read before the claim).
pub struct CodeRow {
    pub user_id: String,
    pub org_id: String,
    pub redirect_uri: String,
    pub challenge: String,
    pub invite_ids: String,
}

/// A user an admin created with a login and no GitHub id can be linked by that login only while
/// they are younger than this (a GitHub login can be renamed and re-registered by someone else).
pub const LINK_BY_LOGIN_MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Drops expired authorize requests, codes and invites. Returns the rows deleted
/// `(requests, codes, invites)`.
pub async fn purge_expired(db: &impl Db, now_ms: i64) -> Result<(u64, u64, u64), DbError> {
    let requests = db
        .execute(
            "DELETE FROM oauth_requests WHERE expires_at_ms < ?",
            &[now_ms.into()],
        )
        .await?;
    let codes = db
        .execute(
            "DELETE FROM oauth_codes WHERE expires_at_ms < ?",
            &[now_ms.into()],
        )
        .await?;
    let invites = db
        .execute(
            "DELETE FROM org_invites WHERE expires_at_ms < ?",
            &[now_ms.into()],
        )
        .await?;
    Ok((requests, codes, invites))
}

pub async fn insert_request(db: &impl Db, request: &NewRequest<'_>) -> Result<(), DbError> {
    db.execute(
        "INSERT INTO oauth_requests (id, state_hash, github_verifier, app_redirect_uri, \
         app_challenge, app_state, org_id, created_at_ms, expires_at_ms) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            request.id.into(),
            request.state_hash.into(),
            request.github_verifier.into(),
            request.redirect_uri.into(),
            request.challenge.into(),
            request.app_state.into(),
            Param::opt(request.org_id),
            request.now_ms.into(),
            request.expires_at_ms.into(),
        ],
    )
    .await
    .map(|_| ())
}

/// Single-use consume: only one caller flips `consumed_at_ms`, and only before expiry.
pub async fn consume_request(
    db: &impl Db,
    state_hash: &str,
    now_ms: i64,
) -> Result<Option<Pending>, DbError> {
    let changed = db
        .execute(
            "UPDATE oauth_requests SET consumed_at_ms = ? \
             WHERE state_hash = ? AND consumed_at_ms IS NULL AND expires_at_ms > ?",
            &[now_ms.into(), state_hash.into(), now_ms.into()],
        )
        .await?;
    if changed != 1 {
        return Ok(None);
    }
    let rows = db
        .query(
            "SELECT github_verifier, app_redirect_uri, app_challenge, app_state, org_id \
             FROM oauth_requests WHERE state_hash = ?",
            &[state_hash.into()],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    Ok(Some(Pending {
        github_verifier: text(row, "github_verifier")?,
        redirect_uri: text(row, "app_redirect_uri")?,
        challenge: text(row, "app_challenge")?,
        app_state: text(row, "app_state")?,
        org_id: opt_text(row, "org_id")?,
    }))
}

pub async fn user_by_github_id(db: &impl Db, github_id: i64) -> Result<Option<String>, DbError> {
    let rows = db
        .query(
            "SELECT id FROM users WHERE github_id = ?",
            &[github_id.into()],
        )
        .await?;
    rows.first().map(|row| text(row, "id")).transpose()
}

/// Links a user who has this login, no GitHub id yet, and was created less than
/// [`LINK_BY_LOGIN_MAX_AGE_MS`] before `now_ms`. `login` is lowercase.
pub async fn link_user_by_login(
    db: &impl Db,
    github_id: i64,
    login: &str,
    now_ms: i64,
) -> Result<Option<String>, DbError> {
    let oldest_secs = (now_ms - LINK_BY_LOGIN_MAX_AGE_MS).div_euclid(1000);
    let rows = db
        .query(
            "SELECT id FROM users WHERE github_id IS NULL AND lower(github_login) = ? \
             AND created_at >= strftime('%Y-%m-%dT%H:%M:%fZ', ?, 'unixepoch')",
            &[login.into(), oldest_secs.into()],
        )
        .await?;
    let Some(id) = rows.first().map(|row| text(row, "id")).transpose()? else {
        return Ok(None);
    };
    let sql = format!(
        "UPDATE users SET github_id = ?, updated_at = {NOW} WHERE id = ? AND github_id IS NULL"
    );
    let changed = db
        .execute(&sql, &[github_id.into(), id.as_str().into()])
        .await?;
    Ok((changed == 1).then_some(id))
}

/// A login moves with the account: whoever held it before (a rename, or a re-used name) loses it.
async fn release_login(db: &impl Db, login: &str, except_user: &str) -> Result<(), DbError> {
    db.execute(
        "UPDATE users SET github_login = NULL WHERE lower(github_login) = lower(?) AND id <> ?",
        &[login.into(), except_user.into()],
    )
    .await
    .map(|_| ())
}

/// Keeps the GitHub id and the current login of a known user up to date.
pub async fn refresh_user(
    db: &impl Db,
    id: &str,
    github_id: i64,
    login: &str,
) -> Result<(), DbError> {
    release_login(db, login, id).await?;
    let sql = format!(
        "UPDATE users SET github_id = ?, github_login = ?, updated_at = {NOW} WHERE id = ?"
    );
    db.execute(&sql, &[github_id.into(), login.into(), id.into()])
        .await
        .map(|_| ())
}

pub async fn create_user(
    db: &impl Db,
    id: &str,
    user: &GithubUser,
    display_name: &str,
) -> Result<(), DbError> {
    release_login(db, &user.login, id).await?;
    db.execute(
        "INSERT INTO users (id, github_id, github_login, email, display_name) VALUES (?, ?, ?, ?, ?)",
        &[
            id.into(),
            user.id.into(),
            user.login.as_str().into(),
            Param::opt(user.email.as_deref()),
            display_name.into(),
        ],
    )
    .await
    .map(|_| ())
}

/// The invites for this (lowercase) login that are neither accepted, revoked nor expired.
pub async fn open_invites(
    db: &impl Db,
    login: &str,
    now_ms: i64,
) -> Result<Vec<OpenInvite>, DbError> {
    let rows = db
        .query(
            "SELECT id, org_id, role FROM org_invites \
             WHERE github_login = ? AND accepted_at_ms IS NULL AND revoked_at_ms IS NULL \
               AND expires_at_ms > ? ORDER BY created_at_ms, id",
            &[login.into(), now_ms.into()],
        )
        .await?;
    rows.iter().map(invite_of).collect()
}

fn invite_of(row: &Row) -> Result<OpenInvite, DbError> {
    Ok(OpenInvite {
        id: text(row, "id")?,
        org_id: text(row, "org_id")?,
        role: text(row, "role")?
            .parse::<Role>()
            .map_err(|error| DbError::Invalid(error.to_string()))?,
    })
}

/// Claims the invite and makes the user a member with its role (an existing membership is kept
/// as it is). `false` when someone revoked or accepted it first.
pub async fn accept_invite(
    db: &impl Db,
    invite: &OpenInvite,
    user_id: &str,
    now_ms: i64,
) -> Result<bool, DbError> {
    let changed = db
        .execute(
            "UPDATE org_invites SET accepted_at_ms = ?, accepted_user_id = ? \
             WHERE id = ? AND accepted_at_ms IS NULL AND revoked_at_ms IS NULL AND expires_at_ms > ?",
            &[
                now_ms.into(),
                user_id.into(),
                invite.id.as_str().into(),
                now_ms.into(),
            ],
        )
        .await?;
    if changed != 1 {
        return Ok(false);
    }
    db.execute(
        "INSERT OR IGNORE INTO members (org_id, user_id, role) VALUES (?, ?, ?)",
        &[
            invite.org_id.as_str().into(),
            user_id.into(),
            invite.role.as_str().into(),
        ],
    )
    .await?;
    Ok(true)
}

/// The orgs the user belongs to, oldest membership first.
pub async fn member_orgs(db: &impl Db, user_id: &str) -> Result<Vec<String>, DbError> {
    let rows = db
        .query(
            "SELECT org_id FROM members WHERE user_id = ? ORDER BY created_at, org_id",
            &[user_id.into()],
        )
        .await?;
    rows.iter().map(|row| text(row, "org_id")).collect()
}

pub async fn insert_code(db: &impl Db, code: &NewCode<'_>) -> Result<(), DbError> {
    db.execute(
        "INSERT INTO oauth_codes (id, code_hash, user_id, org_id, redirect_uri, challenge, \
         invite_ids, created_at_ms, expires_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            code.id.into(),
            code.code_hash.into(),
            code.user_id.into(),
            code.org_id.into(),
            code.redirect_uri.into(),
            code.challenge.into(),
            code.invite_ids.into(),
            code.now_ms.into(),
            code.expires_at_ms.into(),
        ],
    )
    .await
    .map(|_| ())
}

pub async fn find_code(db: &impl Db, code_hash: &str) -> Result<Option<CodeRow>, DbError> {
    let rows = db
        .query(
            "SELECT user_id, org_id, redirect_uri, challenge, invite_ids FROM oauth_codes \
             WHERE code_hash = ?",
            &[code_hash.into()],
        )
        .await?;
    rows.first()
        .map(|row| {
            Ok(CodeRow {
                user_id: text(row, "user_id")?,
                org_id: text(row, "org_id")?,
                redirect_uri: text(row, "redirect_uri")?,
                challenge: text(row, "challenge")?,
                invite_ids: text(row, "invite_ids")?,
            })
        })
        .transpose()
}

/// Marks the code used; `true` for exactly one caller, and only before it expires.
pub async fn claim_code(db: &impl Db, code_hash: &str, now_ms: i64) -> Result<bool, DbError> {
    let changed = db
        .execute(
            "UPDATE oauth_codes SET used_at_ms = ? \
             WHERE code_hash = ? AND used_at_ms IS NULL AND expires_at_ms > ?",
            &[now_ms.into(), code_hash.into(), now_ms.into()],
        )
        .await?;
    Ok(changed == 1)
}

pub async fn github_login_of(db: &impl Db, user_id: &str) -> Result<String, DbError> {
    let rows = db
        .query(
            "SELECT github_login FROM users WHERE id = ?",
            &[user_id.into()],
        )
        .await?;
    Ok(rows
        .first()
        .map(|row| opt_text(row, "github_login"))
        .transpose()?
        .flatten()
        .unwrap_or_default())
}
