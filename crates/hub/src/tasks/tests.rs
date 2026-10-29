//! Native tests of the task index (M2-010): the upsert, the org-scoped filtered list, the FTS
//! search, archive and delete, pagination, and which requests refresh the index. They run over
//! an in-memory SQLite that applies the real migration files (`crate::db::MIGRATIONS`), so the
//! CHECKs, triggers and FTS5 table are the production ones.

use std::cell::RefCell;
use std::future::{Future, ready};
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use protocol::tunnel::{Actor, ActorKind};
use protocol::turn_event::{Envelope, InterruptKind, StateDelta, StateScope, UserMessage};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, params_from_iter};
use serde_json::Value;

use super::index::{
    IndexRow, PendingInterrupt, STATUS_DONE, STATUS_OPEN, STATUS_RUNNING, STATUS_WAITING,
    changes_index,
};
use super::query::{ListQuery, parse_list_query};
use super::store::{ThreadSummary, list_threads, paginate, reconcile_candidates, stored, upsert};
use crate::db::{Db, DbError, MIGRATIONS, Param, Row};

// ---- the native fake -----------------------------------------------------------------------

struct Fake {
    conn: RefCell<Connection>,
}

impl Fake {
    fn new() -> Self {
        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.pragma_update(None, "foreign_keys", true)
            .expect("foreign keys");
        for (name, sql) in MIGRATIONS {
            conn.execute_batch(sql)
                .unwrap_or_else(|error| panic!("migration {name}: {error}"));
        }
        for (id, slug) in [("org_a", "a"), ("org_b", "b")] {
            conn.execute(
                "INSERT INTO orgs (id, slug, name) VALUES (?, ?, ?)",
                rusqlite::params![id, slug, slug],
            )
            .expect("org");
        }
        Self {
            conn: RefCell::new(conn),
        }
    }
}

fn backend(error: rusqlite::Error) -> DbError {
    DbError::Backend(error.to_string())
}

fn bind(params: &[Param]) -> Vec<SqlValue> {
    params
        .iter()
        .map(|param| match param {
            Param::Null => SqlValue::Null,
            Param::Int(value) => SqlValue::Integer(*value),
            Param::Real(value) => SqlValue::Real(*value),
            Param::Text(text) => SqlValue::Text(text.clone()),
        })
        .collect()
}

impl Db for Fake {
    fn execute(&self, sql: &str, params: &[Param]) -> impl Future<Output = Result<u64, DbError>> {
        let conn = self.conn.borrow();
        let result = conn
            .prepare(sql)
            .and_then(|mut statement| statement.execute(params_from_iter(bind(params))))
            .map(|changed| changed as u64)
            .map_err(backend);
        ready(result)
    }

    fn query(
        &self,
        sql: &str,
        params: &[Param],
    ) -> impl Future<Output = Result<Vec<Row>, DbError>> {
        let conn = self.conn.borrow();
        let result = (|| {
            let mut statement = conn.prepare(sql).map_err(backend)?;
            let names: Vec<String> = statement
                .column_names()
                .map(|name| name.to_string())
                .collect();
            let mut rows = statement
                .query(params_from_iter(bind(params)))
                .map_err(backend)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(backend)? {
                let mut object = Row::new();
                for (index, name) in names.iter().enumerate() {
                    let value: SqlValue = row.get(index).map_err(backend)?;
                    object.insert(name.clone(), to_json(value));
                }
                out.push(object);
            }
            Ok(out)
        })();
        ready(result)
    }
}

fn to_json(value: SqlValue) -> Value {
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(number) => Value::from(number),
        SqlValue::Real(number) => Value::from(number),
        SqlValue::Text(text) => Value::String(text),
        SqlValue::Blob(_) => Value::Null,
    }
}

/// Runs a future that the fake answers at once.
fn block_on<T>(future: impl Future<Output = T>) -> T {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("the fake database never waits"),
    }
}

// ---- helpers -------------------------------------------------------------------------------

fn row(thread: &str, title: &str, updated_at: &str) -> IndexRow {
    IndexRow {
        thread_id: thread.to_owned(),
        machine_id: "mch_laptop".to_owned(),
        agent: "claude".to_owned(),
        workspace_path: "/src/app".to_owned(),
        title: title.to_owned(),
        title_source: "derived",
        status: STATUS_OPEN,
        archived: false,
        deleted: false,
        pending_interrupts: Vec::new(),
        updated_at: updated_at.to_owned(),
        last_seq: 3,
    }
}

fn list(db: &Fake, org: &str, query: &ListQuery) -> Vec<String> {
    block_on(list_threads(db, org, query, 100))
        .expect("list")
        .into_iter()
        .map(|summary| summary.thread_id)
        .collect()
}

fn search(db: &Fake, org: &str, q: &str) -> Vec<String> {
    let query = parse_list_query(Some(&format!("q={}", q.replace(' ', "+")))).expect("q");
    list(db, org, &query)
}

fn ids(rows: &[ThreadSummary]) -> Vec<&str> {
    rows.iter().map(|row| row.thread_id.as_str()).collect()
}

// ---- the list and the upsert ---------------------------------------------------------------

#[test]
fn an_upsert_is_listed_with_the_client_shape() {
    let db = Fake::new();
    block_on(upsert(
        &db,
        "org_a",
        &row("th_1", "Upgrade axum", "2026-10-28T10:00:00.000Z"),
    ))
    .expect("upsert");
    let rows = block_on(list_threads(&db, "org_a", &ListQuery::default(), 10)).expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].to_json(),
        serde_json::json!({
            "thread_id": "th_1",
            "title": "Upgrade axum",
            "machine_id": "mch_laptop",
            "agent": "claude",
            "status": "idle",
            "updated_at": "2026-10-28T10:00:00.000Z",
            "last_seq": 3,
            "workspace": "/src/app",
            "archived": false,
            "pending_interrupts": [],
        })
    );
}

#[test]
fn a_second_upsert_updates_the_same_row() {
    let db = Fake::new();
    block_on(upsert(
        &db,
        "org_a",
        &row("th_1", "first", "2026-10-28T10:00:00.000Z"),
    ))
    .expect("1");
    let mut changed = row("th_1", "second", "2026-10-28T11:00:00.000Z");
    changed.status = STATUS_RUNNING;
    changed.last_seq = 9;
    block_on(upsert(&db, "org_a", &changed)).expect("2");
    let rows = block_on(list_threads(&db, "org_a", &ListQuery::default(), 10)).expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "second");
    assert_eq!(rows[0].status, "running");
    assert_eq!(rows[0].last_seq, 9);
}

#[test]
fn another_org_never_sees_the_threads() {
    let db = Fake::new();
    block_on(upsert(
        &db,
        "org_a",
        &row("th_1", "secret plan", "2026-10-28T10:00:00.000Z"),
    ))
    .expect("upsert");
    assert!(list(&db, "org_b", &ListQuery::default()).is_empty());
    assert!(search(&db, "org_b", "secret").is_empty());
    assert_eq!(list(&db, "org_a", &ListQuery::default()), vec!["th_1"]);
}

#[test]
fn filters_narrow_by_machine_agent_workspace_and_status() {
    let db = Fake::new();
    let at = "2026-10-28T10:00:00.000Z";
    block_on(upsert(&db, "org_a", &row("th_1", "a", at))).expect("1");
    let mut other = row("th_2", "b", at);
    other.machine_id = "mch_server".to_owned();
    other.agent = "codex".to_owned();
    other.workspace_path = "/srv/other".to_owned();
    other.status = STATUS_WAITING;
    block_on(upsert(&db, "org_a", &other)).expect("2");

    let by = |raw: &str| list(&db, "org_a", &parse_list_query(Some(raw)).expect(raw));
    assert_eq!(by("machine_id=mch_server"), vec!["th_2"]);
    assert_eq!(by("agent=claude"), vec!["th_1"]);
    assert_eq!(by("workspace=%2Fsrv%2Fother"), vec!["th_2"]);
    assert_eq!(by("status=needs_you"), vec!["th_2"]);
    assert_eq!(by("status=idle"), vec!["th_1"]);
    assert!(by("status=done").is_empty());
}

#[test]
fn archived_and_deleted_threads_are_left_out_unless_asked_for() {
    let db = Fake::new();
    let at = "2026-10-28T10:00:00.000Z";
    block_on(upsert(&db, "org_a", &row("th_live", "live", at))).expect("live");
    let mut archived = row("th_archived", "archived", at);
    archived.archived = true;
    block_on(upsert(&db, "org_a", &archived)).expect("archived");
    let mut deleted = row("th_deleted", "deleted", at);
    deleted.deleted = true;
    block_on(upsert(&db, "org_a", &deleted)).expect("deleted");

    assert_eq!(list(&db, "org_a", &ListQuery::default()), vec!["th_live"]);
    let with_archived = parse_list_query(Some("archived=true")).expect("parse");
    let mut both = list(&db, "org_a", &with_archived);
    both.sort();
    assert_eq!(both, vec!["th_archived", "th_live"], "deleted stays out");
}

#[test]
fn unarchiving_clears_archived_at_and_deleting_is_permanent() {
    let db = Fake::new();
    let at = "2026-10-28T10:00:00.000Z";
    let mut archived = row("th_1", "t", at);
    archived.archived = true;
    block_on(upsert(&db, "org_a", &archived)).expect("archive");
    let stamped = block_on(db.query(
        "SELECT archived_at FROM tasks WHERE thread_id = 'th_1'",
        &[],
    ))
    .expect("query");
    assert!(stamped[0]["archived_at"].is_string());

    block_on(upsert(
        &db,
        "org_a",
        &row("th_1", "t", "2026-10-28T12:00:00.000Z"),
    ))
    .expect("unarchive");
    assert!(list(&db, "org_a", &ListQuery::default()).contains(&"th_1".to_owned()));

    let mut deleted = row("th_1", "t", "2026-10-28T13:00:00.000Z");
    deleted.deleted = true;
    block_on(upsert(&db, "org_a", &deleted)).expect("delete");
    block_on(upsert(
        &db,
        "org_a",
        &row("th_1", "t", "2026-10-28T14:00:00.000Z"),
    ))
    .expect("sync");
    assert!(
        list(&db, "org_a", &ListQuery::default()).is_empty(),
        "a delete does not revert"
    );
}

// ---- search --------------------------------------------------------------------------------

#[test]
fn q_searches_the_titles_by_prefix_and_follows_renames() {
    let db = Fake::new();
    let at = "2026-10-28T10:00:00.000Z";
    block_on(upsert(
        &db,
        "org_a",
        &row("th_1", "Upgrade axum to 0.8", at),
    ))
    .expect("1");
    block_on(upsert(&db, "org_a", &row("th_2", "Fix login bug", at))).expect("2");

    assert_eq!(search(&db, "org_a", "upgr"), vec!["th_1"]);
    assert_eq!(search(&db, "org_a", "login bug"), vec!["th_2"]);
    assert!(
        search(&db, "org_a", "axum login").is_empty(),
        "every word must match"
    );

    block_on(upsert(
        &db,
        "org_a",
        &row("th_1", "Rename the billing job", at),
    ))
    .expect("rename");
    assert!(
        search(&db, "org_a", "axum").is_empty(),
        "the old title is gone from the index"
    );
    assert_eq!(search(&db, "org_a", "billing"), vec!["th_1"]);
}

#[test]
fn q_with_punctuation_is_text_not_fts_syntax() {
    let db = Fake::new();
    block_on(upsert(
        &db,
        "org_a",
        &row("th_1", "fix \"quoted\" bug", "2026-10-28T10:00:00.000Z"),
    ))
    .expect("upsert");
    assert_eq!(search(&db, "org_a", "\"quoted\""), vec!["th_1"]);
    assert!(search(&db, "org_a", "NOT OR").is_empty());
}

// ---- pagination ----------------------------------------------------------------------------

#[test]
fn pages_follow_the_cursor_newest_first_without_gaps() {
    let db = Fake::new();
    for (index, thread) in ["th_a", "th_b", "th_c", "th_d", "th_e"].iter().enumerate() {
        // Two threads share a timestamp: the seq breaks the tie.
        let at = format!("2026-10-28T10:00:0{}.000Z", index.min(3));
        block_on(upsert(&db, "org_a", &row(thread, "t", &at))).expect("upsert");
    }
    let mut seen = Vec::new();
    let mut query = ListQuery::default();
    loop {
        let fetched = block_on(list_threads(&db, "org_a", &query, 2 + 1)).expect("page");
        let (page, next) = paginate(fetched, 2);
        seen.extend(ids(&page).into_iter().map(str::to_owned));
        match next {
            Some(cursor) => query.cursor = Some(cursor),
            None => break,
        }
    }
    assert_eq!(seen, vec!["th_e", "th_d", "th_c", "th_b", "th_a"]);
}

// ---- which requests refresh the index ------------------------------------------------------

fn user_message(thread: &str) -> Envelope {
    Envelope::new(
        thread.to_owned(),
        Some("tk_1".to_owned()),
        None,
        "2026-10-28T10:00:00.000Z".to_owned(),
        UserMessage {
            message_id: "msg_1".to_owned(),
            parts: vec![protocol::tunnel::ContentPart::Text {
                text: "hello".to_owned(),
            }],
            author: Actor {
                kind: ActorKind::User,
                id: "usr_1".to_owned(),
                name: None,
                device_id: None,
            },
        },
    )
}

fn session_delta(thread: &str) -> Envelope {
    Envelope::new(
        thread.to_owned(),
        None,
        None,
        "2026-10-28T10:00:00.000Z".to_owned(),
        StateDelta {
            scope: StateScope::Session,
            patch: Vec::new(),
        },
    )
}

#[test]
fn only_title_status_and_lifecycle_changes_refresh_the_index() {
    let prompt = [user_message("th_1")];
    assert!(changes_index("POST", "/v1/threads/th_1/prompts", &prompt));
    assert!(changes_index("PATCH", "/v1/threads/th_1", &[]));
    assert!(changes_index("DELETE", "/v1/threads/th_1", &[]));
    assert!(changes_index("POST", "/internal/threads/th_1/init", &[]));
    assert!(changes_index("POST", "/internal/threads/th_1/create", &[]));
    // A session-state change (a mode switch) and a GET do not touch the index.
    assert!(!changes_index(
        "POST",
        "/internal/threads/th_1/events",
        &[session_delta("th_1")]
    ));
    assert!(!changes_index("GET", "/v1/threads/th_1", &[]));
}

// ---- the reconcile (M2-010a) ---------------------------------------------------------------

fn host(db: &Fake, org: &str, thread: &str) {
    db.conn
        .borrow()
        .execute(
            "INSERT INTO thread_hosts (thread_id, org_id, machine_id) VALUES (?, ?, 'mch_laptop')",
            rusqlite::params![thread, org],
        )
        .expect("host");
}

/// What the reconcile does for one thread, given the Thread DO's row: writes it when the thread
/// has no stored row or a different one. Returns `true` when it wrote.
fn reconcile_one(db: &Fake, org: &str, row: &IndexRow) -> bool {
    let current = block_on(stored(db, org, &row.thread_id)).expect("stored");
    if current.is_some_and(|current| current.is_current(row)) {
        return false;
    }
    block_on(upsert(db, org, row)).expect("upsert");
    true
}

#[test]
fn a_hosted_thread_without_an_index_row_is_a_reconcile_candidate_and_is_listed_after_it() {
    // A thread from before migration 0008 (or whose first upsert failed): hosted, no row.
    let db = Fake::new();
    host(&db, "org_a", "th_old");
    host(&db, "org_a", "th_new");
    block_on(upsert(
        &db,
        "org_a",
        &row("th_new", "known", "2026-10-28T10:00:00.000Z"),
    ))
    .expect("upsert");
    assert_eq!(list(&db, "org_a", &ListQuery::default()), ["th_new"]);

    let candidates = block_on(reconcile_candidates(&db, 10)).expect("candidates");
    assert_eq!(candidates[0], ("org_a".to_owned(), "th_old".to_owned()));
    assert!(candidates.contains(&("org_a".to_owned(), "th_new".to_owned())));

    let fresh = row("th_old", "from the thread", "2026-10-28T12:00:00.000Z");
    assert!(reconcile_one(&db, "org_a", &fresh));
    assert_eq!(
        list(&db, "org_a", &ListQuery::default()),
        ["th_old", "th_new"]
    );
}

#[test]
fn a_stale_row_from_a_failed_upsert_is_rewritten_and_a_current_one_is_left_alone() {
    let db = Fake::new();
    host(&db, "org_a", "th_1");
    // The row says running; the failed upsert of the run's end left it so.
    let mut stale = row("th_1", "title", "2026-10-28T10:00:00.000Z");
    stale.status = STATUS_RUNNING;
    block_on(upsert(&db, "org_a", &stale)).expect("upsert");

    let mut actual = row("th_1", "title", "2026-10-28T11:00:00.000Z");
    actual.status = STATUS_DONE;
    assert!(reconcile_one(&db, "org_a", &actual));
    let rows = block_on(list_threads(&db, "org_a", &ListQuery::default(), 10)).expect("list");
    assert_eq!(rows[0].status, "done");

    // In step now: a second pass writes nothing, so `updated_at` does not move.
    let mut later = actual.clone();
    later.updated_at = "2026-10-28T13:00:00.000Z".to_owned();
    later.last_seq = 99;
    assert!(!reconcile_one(&db, "org_a", &later));
    let rows = block_on(list_threads(&db, "org_a", &ListQuery::default(), 10)).expect("list");
    assert_eq!(rows[0].updated_at, "2026-10-28T11:00:00.000Z");
}

#[test]
fn deleted_threads_are_not_candidates_and_the_stored_row_is_per_org() {
    let db = Fake::new();
    host(&db, "org_a", "th_gone");
    host(&db, "org_b", "th_b");
    let mut gone = row("th_gone", "x", "2026-10-28T10:00:00.000Z");
    gone.deleted = true;
    block_on(upsert(&db, "org_a", &gone)).expect("upsert");
    let candidates = block_on(reconcile_candidates(&db, 10)).expect("candidates");
    assert_eq!(candidates, [("org_b".to_owned(), "th_b".to_owned())]);
    assert_eq!(
        block_on(stored(&db, "org_a", "th_b")).expect("stored"),
        None
    );
}

#[test]
fn a_done_status_is_listed_as_done_and_found_by_the_status_filter() {
    let db = Fake::new();
    let mut finished = row("th_1", "t", "2026-10-28T10:00:00.000Z");
    finished.status = STATUS_DONE;
    block_on(upsert(&db, "org_a", &finished)).expect("upsert");
    let query = parse_list_query(Some("status=done")).expect("query");
    assert_eq!(list(&db, "org_a", &query), ["th_1"]);
    let rows = block_on(list_threads(&db, "org_a", &ListQuery::default(), 10)).expect("list");
    assert_eq!(rows[0].to_json()["status"], "done");
}

#[test]
fn rows_carry_workspace_archived_and_the_pending_approvals_of_needs_you_rows() {
    use protocol::turn_event::{InterruptOption, PermissionOptionKind};
    let db = Fake::new();
    let at = "2026-10-28T10:00:00.000Z";
    let mut waiting = row("th_wait", "waits", at);
    waiting.status = STATUS_WAITING;
    waiting.pending_interrupts = vec![
        PendingInterrupt {
            interrupt_id: "int_1".to_owned(),
            title: "Run cargo test?".to_owned(),
            kind: InterruptKind::Approval,
            options: vec![InterruptOption {
                option_id: "opt_a".to_owned(),
                name: "Allow".to_owned(),
                kind: PermissionOptionKind::AllowOnce,
            }],
        },
        PendingInterrupt {
            interrupt_id: "int_2".to_owned(),
            title: "Which branch?".to_owned(),
            kind: InterruptKind::Form,
            options: Vec::new(),
        },
    ];
    block_on(upsert(&db, "org_a", &waiting)).expect("waiting");
    let mut bare = row("th_bare", "no workspace", "2026-10-28T09:00:00.000Z");
    bare.workspace_path = String::new();
    bare.archived = true;
    block_on(upsert(&db, "org_a", &bare)).expect("bare");

    let query = parse_list_query(Some("archived=true")).expect("query");
    let rows = block_on(list_threads(&db, "org_a", &query, 100)).expect("list");
    let json: Vec<Value> = rows.iter().map(ThreadSummary::to_json).collect();
    assert_eq!(json[0]["thread_id"], "th_wait");
    assert_eq!(json[0]["workspace"], "/src/app");
    assert_eq!(json[0]["archived"], false);
    assert_eq!(
        json[0]["pending_interrupts"],
        serde_json::json!([
            {
                "interrupt_id": "int_1",
                "title": "Run cargo test?",
                "kind": "approval",
                "options": [{ "option_id": "opt_a", "name": "Allow", "kind": "allow_once" }],
            },
            {
                "interrupt_id": "int_2",
                "title": "Which branch?",
                "kind": "form",
                "options": [],
            },
        ])
    );
    assert_eq!(json[1]["workspace"], Value::Null);
    assert_eq!(json[1]["archived"], true);
    assert_eq!(json[1]["pending_interrupts"], serde_json::json!([]));
    // The stored row round-trips for the reconcile.
    let stored = block_on(stored(&db, "org_a", "th_wait"))
        .expect("stored")
        .expect("row");
    assert!(stored.is_current(&waiting));
}
