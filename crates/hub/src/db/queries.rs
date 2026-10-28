//! Typed helpers over [`Db`]: the org-wide queries the hub's routes need. The SQL here is the
//! only SQL that touches these tables, so the schema (migrations 0002) and its tests stay in step.
//!
//! Every helper takes the database by reference and returns plain structs. Writes stamp
//! `updated_at` themselves (the schema's default only fills `created_at`).

use serde_json::Value;

use super::{Db, DbError, Param, Role, Row, int, opt_text, text};

/// The ISO-8601 UTC "now" the schema also uses as its default.
const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

// ---- orgs ----------------------------------------------------------------------------------

/// An org to insert.
#[derive(Debug, Clone, Copy)]
pub struct NewOrg<'a> {
    pub id: &'a str,
    pub slug: &'a str,
    pub name: &'a str,
}

/// An org row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Org {
    pub id: String,
    pub slug: String,
    pub name: String,
}

pub async fn insert_org(db: &impl Db, org: NewOrg<'_>) -> Result<(), DbError> {
    let sql = format!("INSERT INTO orgs (id, slug, name, updated_at) VALUES (?, ?, ?, {NOW})");
    db.execute(&sql, &[org.id.into(), org.slug.into(), org.name.into()])
        .await?;
    Ok(())
}

/// All orgs, by slug.
pub async fn list_orgs(db: &impl Db) -> Result<Vec<Org>, DbError> {
    let rows = db
        .query("SELECT id, slug, name FROM orgs ORDER BY slug", &[])
        .await?;
    rows.iter().map(org_from_row).collect()
}

fn org_from_row(row: &Row) -> Result<Org, DbError> {
    Ok(Org {
        id: text(row, "id")?,
        slug: text(row, "slug")?,
        name: text(row, "name")?,
    })
}

// ---- users and members ---------------------------------------------------------------------

/// A user to insert.
#[derive(Debug, Clone, Copy)]
pub struct NewUser<'a> {
    pub id: &'a str,
    pub github_login: Option<&'a str>,
    pub display_name: &'a str,
}

/// A user row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: String,
    pub github_login: Option<String>,
    pub display_name: String,
}

pub async fn insert_user(db: &impl Db, user: NewUser<'_>) -> Result<(), DbError> {
    let sql = format!(
        "INSERT INTO users (id, github_login, display_name, updated_at) VALUES (?, ?, ?, {NOW})"
    );
    db.execute(
        &sql,
        &[
            user.id.into(),
            Param::opt(user.github_login),
            user.display_name.into(),
        ],
    )
    .await?;
    Ok(())
}

/// One member of an org, joined with the user's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub user_id: String,
    pub github_login: Option<String>,
    pub display_name: String,
    pub role: Role,
}

/// Add a user to an org with a role, or change the role if they are already a member.
pub async fn set_member_role(
    db: &impl Db,
    org_id: &str,
    user_id: &str,
    role: Role,
) -> Result<(), DbError> {
    let sql = format!(
        "INSERT INTO members (org_id, user_id, role, updated_at) VALUES (?, ?, ?, {NOW}) \
         ON CONFLICT (org_id, user_id) DO UPDATE SET role = excluded.role, updated_at = excluded.updated_at"
    );
    db.execute(&sql, &[org_id.into(), user_id.into(), role.as_str().into()])
        .await?;
    Ok(())
}

/// The members of an org, owners first, then by login.
pub async fn list_members(db: &impl Db, org_id: &str) -> Result<Vec<Member>, DbError> {
    let rows = db
        .query(
            "SELECT m.user_id, u.github_login, u.display_name, m.role \
             FROM members AS m JOIN users AS u ON u.id = m.user_id \
             WHERE m.org_id = ? \
             ORDER BY CASE m.role WHEN 'owner' THEN 0 WHEN 'admin' THEN 1 WHEN 'member' THEN 2 ELSE 3 END, \
                      u.github_login",
            &[org_id.into()],
        )
        .await?;
    rows.iter().map(member_from_row).collect()
}

/// A user's role in an org, or `None` if they are not a member.
pub async fn member_role(
    db: &impl Db,
    org_id: &str,
    user_id: &str,
) -> Result<Option<Role>, DbError> {
    let rows = db
        .query(
            "SELECT role FROM members WHERE org_id = ? AND user_id = ?",
            &[org_id.into(), user_id.into()],
        )
        .await?;
    match rows.first() {
        None => Ok(None),
        Some(row) => text(row, "role")?
            .parse::<Role>()
            .map(Some)
            .map_err(|error| DbError::Invalid(error.to_string())),
    }
}

/// The role check the routes use: is `user_id` a member of `org_id` with at least `required`?
/// A non-member is `false`, never an error.
pub async fn has_role_at_least(
    db: &impl Db,
    org_id: &str,
    user_id: &str,
    required: Role,
) -> Result<bool, DbError> {
    Ok(member_role(db, org_id, user_id)
        .await?
        .is_some_and(|role| role.allows(required)))
}

fn member_from_row(row: &Row) -> Result<Member, DbError> {
    let role_text = text(row, "role")?;
    let role = role_text
        .parse::<Role>()
        .map_err(|error| DbError::Invalid(error.to_string()))?;
    Ok(Member {
        user_id: text(row, "user_id")?,
        github_login: opt_text(row, "github_login")?,
        display_name: text(row, "display_name")?,
        role,
    })
}

// ---- task index ----------------------------------------------------------------------------

/// Task statuses the schema accepts.
pub const TASK_STATUSES: &[&str] = &["open", "running", "waiting", "done", "failed", "cancelled"];

/// A task to upsert. A task is keyed by `(org_id, thread_id)`; `id` is set when it is first
/// inserted and kept after that.
#[derive(Debug, Clone)]
pub struct TaskInput<'a> {
    pub id: &'a str,
    pub org_id: &'a str,
    pub thread_id: &'a str,
    pub title: &'a str,
    /// `user`, `agent` or `derived`.
    pub title_source: &'a str,
    pub status: &'a str,
    pub labels: &'a [String],
}

/// A task index row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub seq: i64,
    pub id: String,
    pub org_id: String,
    pub thread_id: String,
    pub title: String,
    pub title_source: String,
    pub status: String,
    pub labels: Vec<String>,
    pub archived_at: Option<String>,
    pub updated_at: String,
}

/// Insert a task, or update its title, source, status and labels if its thread is already
/// indexed. The FTS5 rows follow through the triggers in migration 0002.
pub async fn upsert_task(db: &impl Db, task: &TaskInput<'_>) -> Result<(), DbError> {
    let labels = serde_json::to_string(task.labels)
        .map_err(|error| DbError::Invalid(format!("labels: {error}")))?;
    let sql = format!(
        "INSERT INTO tasks (id, org_id, thread_id, title, title_source, status, labels, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, {NOW}) \
         ON CONFLICT (org_id, thread_id) DO UPDATE SET \
           title = excluded.title, \
           title_source = excluded.title_source, \
           status = excluded.status, \
           labels = excluded.labels, \
           updated_at = excluded.updated_at"
    );
    db.execute(
        &sql,
        &[
            task.id.into(),
            task.org_id.into(),
            task.thread_id.into(),
            task.title.into(),
            task.title_source.into(),
            task.status.into(),
            labels.into(),
        ],
    )
    .await?;
    Ok(())
}

/// The org's unarchived tasks, most recently updated first, at most `limit`.
pub async fn list_tasks(db: &impl Db, org_id: &str, limit: i64) -> Result<Vec<Task>, DbError> {
    let rows = db
        .query(
            &format!(
                "SELECT {TASK_COLUMNS} FROM tasks \
                 WHERE org_id = ? AND archived_at IS NULL \
                 ORDER BY updated_at DESC, seq DESC LIMIT ?"
            ),
            &[org_id.into(), limit.into()],
        )
        .await?;
    rows.iter().map(task_from_row).collect()
}

/// Full-text search over the org's unarchived task titles, best match first, at most `limit`.
/// `query` is free text: every word must match (as a prefix). A blank query finds nothing.
pub async fn search_tasks(
    db: &impl Db,
    org_id: &str,
    query: &str,
    limit: i64,
) -> Result<Vec<Task>, DbError> {
    let Some(match_expr) = fts_match_expression(query) else {
        return Ok(Vec::new());
    };
    let rows = db
        .query(
            &format!(
                "SELECT {TASK_COLUMNS_T} FROM tasks_fts \
                 JOIN tasks AS t ON t.seq = tasks_fts.rowid \
                 WHERE tasks_fts MATCH ? AND t.org_id = ? AND t.archived_at IS NULL \
                 ORDER BY bm25(tasks_fts), t.updated_at DESC LIMIT ?"
            ),
            &[match_expr.into(), org_id.into(), limit.into()],
        )
        .await?;
    rows.iter().map(task_from_row).collect()
}

/// Turn free text into an FTS5 MATCH expression: each whitespace-separated word becomes a
/// quoted prefix term (`"word"*`), so punctuation in user input cannot be read as FTS syntax.
/// Terms are ANDed. Returns `None` when there is nothing to search for.
pub fn fts_match_expression(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|word| format!("\"{}\"*", word.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

const TASK_COLUMNS: &str =
    "seq, id, org_id, thread_id, title, title_source, status, labels, archived_at, updated_at";
const TASK_COLUMNS_T: &str = "t.seq, t.id, t.org_id, t.thread_id, t.title, t.title_source, t.status, t.labels, t.archived_at, t.updated_at";

fn task_from_row(row: &Row) -> Result<Task, DbError> {
    let labels_text = text(row, "labels")?;
    let labels: Vec<String> = serde_json::from_str(&labels_text)
        .map_err(|error| DbError::Invalid(format!("labels {labels_text:?}: {error}")))?;
    Ok(Task {
        seq: int(row, "seq")?,
        id: text(row, "id")?,
        org_id: text(row, "org_id")?,
        thread_id: text(row, "thread_id")?,
        title: text(row, "title")?,
        title_source: text(row, "title_source")?,
        status: text(row, "status")?,
        labels,
        archived_at: opt_text(row, "archived_at")?,
        updated_at: text(row, "updated_at")?,
    })
}

// ---- audit log -----------------------------------------------------------------------------

/// An audit event to append. `detail` is a JSON object (stored as text; the schema checks it).
#[derive(Debug, Clone)]
pub struct NewAudit<'a> {
    pub id: &'a str,
    pub org_id: &'a str,
    pub actor_user_id: Option<&'a str>,
    pub actor_device_id: Option<&'a str>,
    pub action: &'a str,
    pub target_kind: &'a str,
    pub target_id: &'a str,
    pub detail: &'a Value,
}

/// An audit row, in append order (`seq`).
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEntry {
    pub seq: i64,
    pub id: String,
    pub org_id: String,
    pub actor_user_id: Option<String>,
    pub action: String,
    pub target_kind: String,
    pub target_id: String,
    pub detail: Value,
    pub created_at: String,
}

/// Append one audit event. The log is append-only: the schema refuses UPDATE and DELETE.
pub async fn append_audit(db: &impl Db, event: &NewAudit<'_>) -> Result<(), DbError> {
    let detail = serde_json::to_string(event.detail)
        .map_err(|error| DbError::Invalid(format!("audit detail: {error}")))?;
    let sql = format!(
        "INSERT INTO audit_log (id, org_id, actor_user_id, actor_device_id, action, target_kind, target_id, detail_json, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, {NOW})"
    );
    db.execute(
        &sql,
        &[
            event.id.into(),
            event.org_id.into(),
            Param::opt(event.actor_user_id),
            Param::opt(event.actor_device_id),
            event.action.into(),
            event.target_kind.into(),
            event.target_id.into(),
            detail.into(),
        ],
    )
    .await?;
    Ok(())
}

/// An org's audit events after `after_seq` (0 for the start), in order, at most `limit`. This
/// is the cursor the audit export pages with.
pub async fn list_audit(
    db: &impl Db,
    org_id: &str,
    after_seq: i64,
    limit: i64,
) -> Result<Vec<AuditEntry>, DbError> {
    let rows = db
        .query(
            "SELECT seq, id, org_id, actor_user_id, action, target_kind, target_id, detail_json, created_at \
             FROM audit_log WHERE org_id = ? AND seq > ? ORDER BY seq LIMIT ?",
            &[org_id.into(), after_seq.into(), limit.into()],
        )
        .await?;
    rows.iter().map(audit_from_row).collect()
}

fn audit_from_row(row: &Row) -> Result<AuditEntry, DbError> {
    let detail_text = text(row, "detail_json")?;
    let detail: Value = serde_json::from_str(&detail_text)
        .map_err(|error| DbError::Invalid(format!("audit detail {detail_text:?}: {error}")))?;
    Ok(AuditEntry {
        seq: int(row, "seq")?,
        id: text(row, "id")?,
        org_id: text(row, "org_id")?,
        actor_user_id: opt_text(row, "actor_user_id")?,
        action: text(row, "action")?,
        target_kind: text(row, "target_kind")?,
        target_id: text(row, "target_id")?,
        detail,
        created_at: text(row, "created_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::fts_match_expression;

    #[test]
    fn fts_expression_quotes_every_word_as_a_prefix() {
        assert_eq!(
            fts_match_expression("fix  login \"bug\"").as_deref(),
            Some("\"fix\"* \"login\"* \"\"\"bug\"\"\"*")
        );
    }

    #[test]
    fn blank_search_has_no_expression() {
        assert_eq!(fts_match_expression("   "), None);
    }
}
