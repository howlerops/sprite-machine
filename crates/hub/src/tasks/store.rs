//! The D1 side of the task index (migrations 0002 and 0008): the upsert the Thread DO calls and
//! the org-scoped, filtered, paginated list `GET /v1/threads` reads. Every statement takes the
//! org as a bound parameter, so one org's query never sees another's rows.

use serde_json::{Value, json};

use super::index::{IndexRow, PendingInterrupt};
use super::query::{Cursor, ListQuery, api_status};
use crate::db::queries::fts_match_expression;
use crate::db::{Db, DbError, Param, Row, int, text};

/// Threads per `GET /v1/threads` page.
pub const PAGE_SIZE: usize = 50;

/// Inserts the thread's row, or updates it. `archived_at` keeps its first value while the thread
/// stays archived and clears when it is unarchived; `deleted_at` never clears.
const UPSERT: &str = "INSERT INTO tasks \
     (id, org_id, thread_id, title, title_source, status, machine_id, agent, workspace_path, \
      last_seq, archived_at, deleted_at, updated_at, pending_interrupts) \
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
     ON CONFLICT (org_id, thread_id) DO UPDATE SET \
       title = excluded.title, \
       title_source = excluded.title_source, \
       status = excluded.status, \
       machine_id = excluded.machine_id, \
       agent = excluded.agent, \
       workspace_path = excluded.workspace_path, \
       last_seq = excluded.last_seq, \
       archived_at = CASE WHEN excluded.archived_at IS NULL THEN NULL \
                          ELSE COALESCE(tasks.archived_at, excluded.archived_at) END, \
       deleted_at = COALESCE(tasks.deleted_at, excluded.deleted_at), \
       updated_at = excluded.updated_at, \
       pending_interrupts = excluded.pending_interrupts";

/// Writes the thread's index row for `org_id`. The task id is the thread id: both are unique.
pub async fn upsert(db: &impl Db, org_id: &str, row: &IndexRow) -> Result<(), DbError> {
    let archived_at = row.archived.then(|| row.updated_at.clone());
    let deleted_at = row.deleted.then(|| row.updated_at.clone());
    db.execute(
        UPSERT,
        &[
            row.thread_id.as_str().into(),
            org_id.into(),
            row.thread_id.as_str().into(),
            row.title.as_str().into(),
            row.title_source.into(),
            row.status.into(),
            row.machine_id.as_str().into(),
            row.agent.as_str().into(),
            row.workspace_path.as_str().into(),
            Param::Int(row.last_seq),
            Param::opt(archived_at),
            Param::opt(deleted_at),
            row.updated_at.as_str().into(),
            serde_json::to_string(&row.pending_interrupts)
                .unwrap_or_else(|_| "[]".to_owned())
                .into(),
        ],
    )
    .await?;
    Ok(())
}

/// The fields of a stored index row that the reconcile compares with the Thread DO's view.
/// `last_seq` and `updated_at` are not among them: they move only at index-changing events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredIndex {
    pub status: String,
    pub title: String,
    pub machine_id: String,
    pub agent: String,
    pub workspace_path: String,
    pub archived: bool,
    pub deleted: bool,
    pub pending_interrupts: Vec<PendingInterrupt>,
}

impl StoredIndex {
    /// `true` when the stored row already says what `row` says.
    pub fn is_current(&self, row: &IndexRow) -> bool {
        self.status == row.status
            && self.title == row.title
            && self.machine_id == row.machine_id
            && self.agent == row.agent
            && self.workspace_path == row.workspace_path
            && self.archived == row.archived
            && self.deleted == row.deleted
            && self.pending_interrupts == row.pending_interrupts
    }
}

/// The thread's stored index row in `org_id`, or `None` when it has none.
pub async fn stored(
    db: &impl Db,
    org_id: &str,
    thread_id: &str,
) -> Result<Option<StoredIndex>, DbError> {
    let rows = db
        .query(
            "SELECT status, title, machine_id, agent, workspace_path, \
             archived_at IS NOT NULL AS archived, deleted_at IS NOT NULL AS deleted, \
             pending_interrupts \
             FROM tasks WHERE org_id = ? AND thread_id = ?",
            &[org_id.into(), thread_id.into()],
        )
        .await?;
    rows.first()
        .map(|row| {
            Ok(StoredIndex {
                status: text(row, "status")?,
                title: text(row, "title")?,
                machine_id: text(row, "machine_id")?,
                agent: text(row, "agent")?,
                workspace_path: text(row, "workspace_path")?,
                archived: int(row, "archived")? != 0,
                deleted: int(row, "deleted")? != 0,
                pending_interrupts: parse_pending(row),
            })
        })
        .transpose()
}

/// Most threads one reconcile pass looks at.
pub const RECONCILE_LIMIT: usize = 200;

/// The hosted threads a reconcile checks, at most `limit`: threads with no index row first
/// (created before migration 0008, or a first upsert that failed), then the rest by most recent
/// index change. Threads already marked deleted are left out. Each id is paired with its org.
pub async fn reconcile_candidates(
    db: &impl Db,
    limit: usize,
) -> Result<Vec<(String, String)>, DbError> {
    let rows = db
        .query(
            "SELECT h.org_id AS org_id, h.thread_id AS thread_id FROM thread_hosts AS h \
             LEFT JOIN tasks AS t ON t.org_id = h.org_id AND t.thread_id = h.thread_id \
             WHERE t.thread_id IS NULL OR t.deleted_at IS NULL \
             ORDER BY (t.thread_id IS NULL) DESC, t.updated_at DESC, h.thread_id LIMIT ?",
            &[Param::Int(i64::try_from(limit).unwrap_or(i64::MAX))],
        )
        .await?;
    rows.iter()
        .map(|row| Ok((text(row, "org_id")?, text(row, "thread_id")?)))
        .collect()
}

/// One row of the thread list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub title: String,
    pub machine_id: String,
    pub agent: String,
    /// The stored status (`open`, `running`, ...); [`ThreadSummary::to_json`] names it.
    pub status: String,
    pub updated_at: String,
    pub last_seq: i64,
    /// The thread's workspace path; empty when the thread has none (listed as `null`).
    pub workspace_path: String,
    pub archived: bool,
    /// The open approval interrupts; only `needs_you` rows have any.
    pub pending_interrupts: Vec<PendingInterrupt>,
    /// The row's `seq`, the tie-breaker of the cursor.
    pub seq: i64,
}

impl ThreadSummary {
    /// The client shape of client-api.md §5.2.
    pub fn to_json(&self) -> Value {
        json!({
            "thread_id": self.thread_id,
            "title": self.title,
            "machine_id": self.machine_id,
            "agent": self.agent,
            "status": api_status(&self.status),
            "updated_at": self.updated_at,
            "last_seq": self.last_seq,
            "workspace": (!self.workspace_path.is_empty()).then_some(self.workspace_path.as_str()),
            "archived": self.archived,
            "pending_interrupts": self.pending_interrupts,
        })
    }

    /// The cursor that continues the list after this row.
    pub fn cursor(&self) -> Cursor {
        Cursor {
            updated_at: self.updated_at.clone(),
            seq: self.seq,
        }
    }
}

/// Splits `rows`, fetched with one more than a page, into the page and the next cursor.
pub fn paginate(mut rows: Vec<ThreadSummary>, page: usize) -> (Vec<ThreadSummary>, Option<Cursor>) {
    if rows.len() <= page {
        return (rows, None);
    }
    rows.truncate(page);
    let next = rows.last().map(ThreadSummary::cursor);
    (rows, next)
}

/// The org's threads matching `query`, newest first (`updated_at`, then `seq`), at most `limit`.
/// Deleted threads are never listed; archived ones only with `include_archived`.
pub async fn list_threads(
    db: &impl Db,
    org_id: &str,
    query: &ListQuery,
    limit: usize,
) -> Result<Vec<ThreadSummary>, DbError> {
    let mut sql = String::from(
        "SELECT t.seq, t.thread_id, t.title, t.machine_id, t.agent, t.status, t.updated_at, \
         t.last_seq, t.workspace_path, t.archived_at IS NOT NULL AS archived, \
         t.pending_interrupts FROM tasks AS t WHERE t.org_id = ? AND t.deleted_at IS NULL",
    );
    let mut params: Vec<Param> = vec![org_id.into()];
    if !query.include_archived {
        sql.push_str(" AND t.archived_at IS NULL");
    }
    if let Some(machine_id) = &query.machine_id {
        sql.push_str(" AND t.machine_id = ?");
        params.push(machine_id.as_str().into());
    }
    if let Some(agent) = &query.agent {
        sql.push_str(" AND t.agent = ?");
        params.push(agent.as_str().into());
    }
    if let Some(workspace) = &query.workspace_path {
        sql.push_str(" AND t.workspace_path = ?");
        params.push(workspace.as_str().into());
    }
    if !query.statuses.is_empty() {
        let marks = vec!["?"; query.statuses.len()].join(", ");
        sql.push_str(&format!(" AND t.status IN ({marks})"));
        params.extend(query.statuses.iter().map(|status| Param::from(*status)));
    }
    if let Some(expression) = query.q.as_deref().and_then(fts_match_expression) {
        sql.push_str(" AND t.seq IN (SELECT rowid FROM tasks_fts WHERE tasks_fts MATCH ?)");
        params.push(expression.into());
    }
    if let Some(cursor) = &query.cursor {
        sql.push_str(" AND (t.updated_at < ? OR (t.updated_at = ? AND t.seq < ?))");
        params.push(cursor.updated_at.as_str().into());
        params.push(cursor.updated_at.as_str().into());
        params.push(cursor.seq.into());
    }
    sql.push_str(" ORDER BY t.updated_at DESC, t.seq DESC LIMIT ?");
    params.push(Param::Int(i64::try_from(limit).unwrap_or(i64::MAX)));

    let rows = db.query(&sql, &params).await?;
    rows.iter().map(summary_from_row).collect()
}

/// The row's `pending_interrupts` JSON; a malformed or missing value reads as none.
fn parse_pending(row: &Row) -> Vec<PendingInterrupt> {
    text(row, "pending_interrupts")
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn summary_from_row(row: &Row) -> Result<ThreadSummary, DbError> {
    let status = text(row, "status")?;
    let pending_interrupts = if api_status(&status) == "needs_you" {
        parse_pending(row)
    } else {
        Vec::new()
    };
    Ok(ThreadSummary {
        thread_id: text(row, "thread_id")?,
        title: text(row, "title")?,
        machine_id: text(row, "machine_id")?,
        agent: text(row, "agent")?,
        status,
        updated_at: text(row, "updated_at")?,
        last_seq: int(row, "last_seq")?,
        workspace_path: text(row, "workspace_path")?,
        archived: int(row, "archived")? != 0,
        pending_interrupts,
        seq: int(row, "seq")?,
    })
}
