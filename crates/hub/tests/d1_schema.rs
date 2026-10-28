//! M2-016: the D1 schema (migrations 0002 and up) and the typed helpers in `hub::db`.
//!
//! Native and in-memory. The fake below is a `Db` over rusqlite that applies the same migration
//! files the Worker's D1 gets (`hub::db::MIGRATIONS`, embedded from `crates/hub/migrations/`),
//! with foreign keys on, as D1 has them. So these tests exercise the real schema, CHECKs,
//! triggers and FTS5 table; only the transport differs. `wrangler d1 migrations apply DB --local`
//! checks that the same files also apply on the local D1 (see docs/ledger/reports/M2-016.md).
//!
//! Run with `cargo test -p hub --test d1_schema`, or through `bash scripts/hub-test.sh d1_schema`.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::future::{Future, ready};
use std::path::Path;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use hub::db::queries::{self as q, NewAudit, NewOrg, NewUser, TaskInput};
use hub::db::{Db, DbError, MIGRATIONS, Param, Role, Row};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, params_from_iter};
use serde_json::{Value, json};

// ---- the native fake -----------------------------------------------------------------------

/// A [`Db`] over an in-memory SQLite with every migration applied in order.
struct Fake {
    conn: RefCell<Connection>,
}

impl Fake {
    fn new() -> Self {
        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        conn.pragma_update(None, "foreign_keys", true)
            .expect("enable foreign keys, as D1 does");
        for (name, sql) in MIGRATIONS {
            conn.execute_batch(sql)
                .unwrap_or_else(|error| panic!("migration {name} failed: {error}"));
        }
        Self {
            conn: RefCell::new(conn),
        }
    }

    /// Raw SQL for the tests that check constraints and triggers directly.
    fn raw(&self, sql: &str) -> Result<u64, DbError> {
        self.run_execute(sql, &[])
    }

    fn count(&self, sql: &str) -> i64 {
        let rows = self.run_query(sql, &[]).expect("count query");
        rows[0]["n"].as_i64().expect("count is an integer")
    }

    fn run_execute(&self, sql: &str, params: &[Param]) -> Result<u64, DbError> {
        let conn = self.conn.borrow();
        let mut statement = conn.prepare(sql).map_err(backend)?;
        let changed = statement
            .execute(params_from_iter(bind(params)))
            .map_err(backend)?;
        Ok(changed as u64)
    }

    fn run_query(&self, sql: &str, params: &[Param]) -> Result<Vec<Row>, DbError> {
        let conn = self.conn.borrow();
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

fn to_json(value: SqlValue) -> Value {
    match value {
        SqlValue::Null | SqlValue::Blob(_) => Value::Null,
        SqlValue::Integer(value) => json!(value),
        SqlValue::Real(value) => json!(value),
        SqlValue::Text(text) => Value::String(text),
    }
}

impl Db for Fake {
    fn execute(&self, sql: &str, params: &[Param]) -> impl Future<Output = Result<u64, DbError>> {
        ready(self.run_execute(sql, params))
    }

    fn query(
        &self,
        sql: &str,
        params: &[Param],
    ) -> impl Future<Output = Result<Vec<Row>, DbError>> {
        ready(self.run_query(sql, params))
    }
}

/// Drive a future that only ever returns `Ready` (the fake's). No runtime needed.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("the in-memory fake never returns Pending"),
    }
}

/// Two orgs, two users (one with no GitHub login) and their memberships.
fn seeded() -> Fake {
    let db = Fake::new();
    block_on(async {
        q::insert_org(
            &db,
            NewOrg {
                id: "org_acme",
                slug: "acme",
                name: "Acme",
            },
        )
        .await
        .unwrap();
        q::insert_org(
            &db,
            NewOrg {
                id: "org_beta",
                slug: "beta",
                name: "Beta",
            },
        )
        .await
        .unwrap();
        q::insert_user(
            &db,
            NewUser {
                id: "u_ann",
                github_login: Some("ann"),
                display_name: "Ann",
            },
        )
        .await
        .unwrap();
        q::insert_user(
            &db,
            NewUser {
                id: "u_bob",
                github_login: None,
                display_name: "Bob",
            },
        )
        .await
        .unwrap();
        q::set_member_role(&db, "org_acme", "u_ann", Role::Owner)
            .await
            .unwrap();
        q::set_member_role(&db, "org_acme", "u_bob", Role::Viewer)
            .await
            .unwrap();
    });
    db
}

fn labels(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

fn upsert(db: &Fake, id: &str, org: &str, thread: &str, title: &str, labels: &[String]) {
    block_on(q::upsert_task(
        db,
        &TaskInput {
            id,
            org_id: org,
            thread_id: thread,
            title,
            title_source: "user",
            status: "open",
            labels,
        },
    ))
    .expect("upsert task");
}

// ---- the schema ----------------------------------------------------------------------------

#[test]
fn embedded_migrations_are_the_files_in_the_migrations_directory() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut on_disk: Vec<(String, String)> = fs::read_dir(&dir)
        .expect("read migrations dir")
        .map(|entry| {
            let path = entry.expect("dir entry").path();
            let name = path
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .into_owned();
            let sql = fs::read_to_string(&path).expect("read migration");
            (name, sql)
        })
        .collect();
    on_disk.sort();
    let embedded: Vec<(String, String)> = MIGRATIONS
        .iter()
        .map(|(name, sql)| (name.to_string(), sql.to_string()))
        .collect();
    assert_eq!(
        embedded
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        on_disk
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        "MIGRATIONS in src/db/mod.rs must list every file in migrations/, in order"
    );
    assert_eq!(embedded, on_disk, "embedded migration text is stale");
}

#[test]
fn org_wide_tables_triggers_and_no_machines_table() {
    let db = Fake::new();
    let names: BTreeSet<String> = db
        .run_query(
            "SELECT name FROM sqlite_master WHERE type IN ('table', 'trigger')",
            &[],
        )
        .unwrap()
        .into_iter()
        .map(|row| row["name"].as_str().unwrap().to_owned())
        .collect();
    for expected in [
        "orgs",
        "users",
        "members",
        "devices",
        "agents",
        "workspaces",
        "tasks",
        "tasks_fts",
        "notification_prefs",
        "audit_log",
        "usage_rollups",
        "tasks_fts_insert",
        "tasks_fts_update",
        "tasks_fts_delete",
        "audit_log_no_update",
        "audit_log_no_delete",
    ] {
        assert!(names.contains(expected), "missing {expected}");
    }
    // The `machines` table is M2B-002's migration, not this one.
    assert!(!names.contains("machines"));
}

#[test]
fn foreign_keys_are_enforced() {
    let db = seeded();
    let error = block_on(q::set_member_role(
        &db,
        "org_missing",
        "u_ann",
        Role::Viewer,
    ));
    assert!(
        error.is_err(),
        "a member row for an unknown org must be refused"
    );
}

#[test]
fn members_role_is_one_of_the_four_roles() {
    let db = seeded();
    assert!(
        db.raw(
            "INSERT INTO members (org_id, user_id, role) VALUES ('org_beta', 'u_bob', 'superuser')"
        )
        .is_err()
    );
    assert!(
        db.raw("INSERT INTO members (org_id, user_id, role) VALUES ('org_beta', 'u_bob', 'admin')")
            .is_ok()
    );
}

#[test]
fn devices_agents_and_workspaces_carry_their_checks() {
    let db = seeded();
    db.raw(
        "INSERT INTO devices (id, org_id, user_id, name, trusted) \
         VALUES ('dev_1', 'org_acme', 'u_ann', 'laptop', 1)",
    )
    .expect("trusted device");
    assert!(db
        .raw("INSERT INTO devices (id, org_id, user_id, name, trusted) VALUES ('dev_2', 'org_acme', 'u_ann', 'x', 2)")
        .is_err(), "trusted is 0 or 1");

    // kind local: no URL, and it can name its device.
    db.raw(
        "INSERT INTO agents (id, org_id, kind, name, device_id) VALUES ('ag_local', 'org_acme', 'local', 'runner', 'dev_1')",
    )
    .expect("local agent without url");
    assert!(db
        .raw("INSERT INTO agents (id, org_id, kind, name, url) VALUES ('ag_bad', 'org_acme', 'local', 'x', 'https://a.example')")
        .is_err(), "a local agent has no URL");
    // kind byo: needs its URL.
    assert!(db
        .raw("INSERT INTO agents (id, org_id, kind, name) VALUES ('ag_nourl', 'org_acme', 'byo', 'x')")
        .is_err(), "a byo agent needs a URL");
    db.raw(
        "INSERT INTO agents (id, org_id, kind, name, url, card_json, card_fetched_at) \
         VALUES ('ag_byo', 'org_acme', 'byo', 'remote', 'https://agent.example', '{\"name\":\"remote\"}', '2026-10-28T09:00:00.000Z')",
    )
    .expect("byo agent with card cache");
    assert!(db
        .raw("INSERT INTO agents (id, org_id, kind, name, url) VALUES ('ag_kind', 'org_acme', 'robot', 'x', 'https://a.example')")
        .is_err(), "kind is local, managed or byo");
    assert!(db
        .raw("INSERT INTO agents (id, org_id, kind, name, url, card_json) VALUES ('ag_card', 'org_acme', 'byo', 'y', 'https://b.example', 'not json')")
        .is_err(), "card_json is JSON");

    db.raw("INSERT INTO workspaces (id, org_id, name, kind, owner_user_id) VALUES ('ws_1', 'org_acme', 'sandbox', 'cloud', 'u_ann')")
        .expect("cloud workspace");
    assert!(
        db.raw(
            "INSERT INTO workspaces (id, org_id, name, kind) VALUES ('ws_2', 'org_acme', 'x', 'vm')"
        )
        .is_err(),
        "workspace kind is local or cloud"
    );
}

// ---- orgs, users, members ------------------------------------------------------------------

#[test]
fn orgs_users_and_members_insert_and_list() {
    let db = seeded();
    block_on(async {
        let orgs = q::list_orgs(&db).await.unwrap();
        let slugs: Vec<&str> = orgs.iter().map(|org| org.slug.as_str()).collect();
        assert_eq!(slugs, ["acme", "beta"]);
        assert_eq!(orgs[0].name, "Acme");

        // Owners first, then by login (bob has none, so he sorts after ann's login but is a viewer).
        let members = q::list_members(&db, "org_acme").await.unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].user_id, "u_ann");
        assert_eq!(members[0].github_login.as_deref(), Some("ann"));
        assert_eq!(members[0].role, Role::Owner);
        assert_eq!(members[1].user_id, "u_bob");
        assert_eq!(members[1].github_login, None);
        assert_eq!(members[1].role, Role::Viewer);

        // Setting a role again changes it in place: still two members.
        q::set_member_role(&db, "org_acme", "u_bob", Role::Member)
            .await
            .unwrap();
        let members = q::list_members(&db, "org_acme").await.unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members[1].role, Role::Member);

        // Beta has no members yet.
        assert!(q::list_members(&db, "org_beta").await.unwrap().is_empty());
    });
}

#[test]
fn role_check_helper_follows_the_privilege_order() {
    let db = seeded();
    block_on(async {
        assert!(
            q::has_role_at_least(&db, "org_acme", "u_ann", Role::Admin)
                .await
                .unwrap()
        );
        assert!(
            q::has_role_at_least(&db, "org_acme", "u_ann", Role::Owner)
                .await
                .unwrap()
        );
        assert!(
            !q::has_role_at_least(&db, "org_acme", "u_bob", Role::Member)
                .await
                .unwrap()
        );
        assert!(
            q::has_role_at_least(&db, "org_acme", "u_bob", Role::Viewer)
                .await
                .unwrap()
        );
        // Not a member of this org at all: false, not an error.
        assert!(
            !q::has_role_at_least(&db, "org_beta", "u_ann", Role::Viewer)
                .await
                .unwrap()
        );
        assert_eq!(
            q::member_role(&db, "org_beta", "u_ann").await.unwrap(),
            None
        );
        assert_eq!(
            q::member_role(&db, "org_acme", "u_ann").await.unwrap(),
            Some(Role::Owner)
        );
    });
}

// ---- task index and FTS5 search ------------------------------------------------------------

#[test]
fn task_upsert_keeps_the_id_and_search_follows_the_title() {
    let db = seeded();
    upsert(
        &db,
        "task_1",
        "org_acme",
        "thread_1",
        "Fix login bug",
        &labels(&["backend", "urgent"]),
    );
    upsert(
        &db,
        "task_2",
        "org_acme",
        "thread_2",
        "Refactor billing",
        &labels(&[]),
    );

    block_on(async {
        let tasks = q::list_tasks(&db, "org_acme", 10).await.unwrap();
        assert_eq!(tasks.len(), 2);
        let login = tasks
            .iter()
            .find(|task| task.id == "task_1")
            .expect("task_1 listed");
        assert_eq!(login.labels, labels(&["backend", "urgent"]));
        assert_eq!(login.status, "open");
        assert_eq!(login.archived_at, None);

        // Prefix search over titles.
        let hits = q::search_tasks(&db, "org_acme", "logi", 10).await.unwrap();
        assert_eq!(
            hits.iter().map(|task| task.id.as_str()).collect::<Vec<_>>(),
            ["task_1"]
        );
        // Every word must match.
        assert!(
            q::search_tasks(&db, "org_acme", "login billing", 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            q::search_tasks(&db, "org_acme", "BILL", 10)
                .await
                .unwrap()
                .len(),
            1
        );

        // The same thread again: title and labels change in place, the row and its id stay.
        q::upsert_task(
            &db,
            &TaskInput {
                id: "task_ignored",
                org_id: "org_acme",
                thread_id: "thread_1",
                title: "Rename the auth flow",
                title_source: "agent",
                status: "done",
                labels: &labels(&["auth"]),
            },
        )
        .await
        .unwrap();
        let tasks = q::list_tasks(&db, "org_acme", 10).await.unwrap();
        assert_eq!(tasks.len(), 2, "upsert does not duplicate");
        let renamed = tasks
            .iter()
            .find(|task| task.thread_id == "thread_1")
            .unwrap();
        assert_eq!(renamed.id, "task_1", "the first id is kept");
        assert_eq!(renamed.title_source, "agent");
        assert_eq!(renamed.status, "done");
        assert_eq!(renamed.labels, labels(&["auth"]));

        assert!(
            q::search_tasks(&db, "org_acme", "login", 10)
                .await
                .unwrap()
                .is_empty()
        );
        let hits = q::search_tasks(&db, "org_acme", "auth", 10).await.unwrap();
        assert_eq!(
            hits.iter().map(|task| task.id.as_str()).collect::<Vec<_>>(),
            ["task_1"]
        );
    });
    assert_eq!(
        db.count("SELECT count(*) AS n FROM tasks_fts"),
        2,
        "one FTS row per task"
    );
}

#[test]
fn search_is_scoped_to_the_org_and_skips_archived_tasks() {
    let db = seeded();
    upsert(&db, "task_1", "org_acme", "thread_1", "login page", &[]);
    upsert(&db, "task_9", "org_beta", "thread_9", "login redirect", &[]);

    block_on(async {
        let acme = q::search_tasks(&db, "org_acme", "login", 10).await.unwrap();
        assert_eq!(
            acme.iter().map(|task| task.id.as_str()).collect::<Vec<_>>(),
            ["task_1"]
        );
        let beta = q::search_tasks(&db, "org_beta", "login", 10).await.unwrap();
        assert_eq!(
            beta.iter().map(|task| task.id.as_str()).collect::<Vec<_>>(),
            ["task_9"]
        );
    });

    db.raw("UPDATE tasks SET archived_at = '2026-10-27T10:00:00.000Z' WHERE id = 'task_1'")
        .expect("archive");
    block_on(async {
        assert!(
            q::search_tasks(&db, "org_acme", "login", 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(q::list_tasks(&db, "org_acme", 10).await.unwrap().is_empty());
    });
}

#[test]
fn deleting_a_task_removes_its_search_row() {
    let db = seeded();
    upsert(&db, "task_1", "org_acme", "thread_1", "kept title", &[]);
    upsert(&db, "task_2", "org_acme", "thread_2", "gone title", &[]);
    db.raw("DELETE FROM tasks WHERE id = 'task_2'")
        .expect("delete");
    assert_eq!(db.count("SELECT count(*) AS n FROM tasks_fts"), 1);
    block_on(async {
        assert!(
            q::search_tasks(&db, "org_acme", "gone", 10)
                .await
                .unwrap()
                .is_empty()
        );
    });
}

#[test]
fn free_text_search_cannot_inject_fts_syntax() {
    let db = seeded();
    upsert(&db, "task_1", "org_acme", "thread_1", "login page", &[]);
    block_on(async {
        for hostile in [
            "\"",
            "NEAR(login OR",
            "login AND",
            "a*b",
            "col:login",
            "   ",
        ] {
            q::search_tasks(&db, "org_acme", hostile, 10)
                .await
                .unwrap_or_else(|error| panic!("search {hostile:?} failed: {error}"));
        }
        let hits = q::search_tasks(&db, "org_acme", "\"login\"", 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "quotes in the query are literal, not syntax");
    });
}

// ---- audit log -----------------------------------------------------------------------------

#[test]
fn audit_appends_and_pages_by_seq() {
    let db = seeded();
    let granted = json!({ "role": "owner" });
    let renamed = json!({ "from": "a", "to": "b" });
    block_on(async {
        q::append_audit(
            &db,
            &NewAudit {
                id: "aud_1",
                org_id: "org_acme",
                actor_user_id: Some("u_ann"),
                actor_device_id: None,
                action: "member.role_set",
                target_kind: "user",
                target_id: "u_ann",
                detail: &granted,
            },
        )
        .await
        .unwrap();
        q::append_audit(
            &db,
            &NewAudit {
                id: "aud_2",
                org_id: "org_acme",
                actor_user_id: None,
                actor_device_id: None,
                action: "task.renamed",
                target_kind: "task",
                target_id: "task_1",
                detail: &renamed,
            },
        )
        .await
        .unwrap();

        let all = q::list_audit(&db, "org_acme", 0, 10).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "aud_1");
        assert_eq!(all[0].actor_user_id.as_deref(), Some("u_ann"));
        assert_eq!(all[0].detail, granted);
        assert_eq!(all[1].actor_user_id, None);
        assert!(all[0].seq < all[1].seq);

        let after_first = q::list_audit(&db, "org_acme", all[0].seq, 10)
            .await
            .unwrap();
        assert_eq!(after_first.len(), 1);
        assert_eq!(after_first[0].action, "task.renamed");
        assert!(
            q::list_audit(&db, "org_beta", 0, 10)
                .await
                .unwrap()
                .is_empty()
        );
    });
}

#[test]
fn audit_log_is_append_only_and_outlives_its_org() {
    let db = seeded();
    block_on(async {
        q::append_audit(
            &db,
            &NewAudit {
                id: "aud_1",
                org_id: "org_acme",
                actor_user_id: None,
                actor_device_id: None,
                action: "org.created",
                target_kind: "org",
                target_id: "org_acme",
                detail: &json!({}),
            },
        )
        .await
        .unwrap();
    });
    assert!(db.raw("UPDATE audit_log SET action = 'edited'").is_err());
    assert!(db.raw("DELETE FROM audit_log").is_err());
    assert!(
        db.raw("DELETE FROM orgs WHERE id = 'org_acme'").is_err(),
        "audit rows block the org delete"
    );
    assert_eq!(db.count("SELECT count(*) AS n FROM audit_log"), 1);
}

#[test]
fn audit_detail_must_be_json() {
    let db = seeded();
    assert!(db
        .raw("INSERT INTO audit_log (id, org_id, action, detail_json) VALUES ('aud_x', 'org_acme', 'x', 'not json')")
        .is_err());
}

// ---- notification prefs and usage rollups --------------------------------------------------

#[test]
fn notification_prefs_check_mute_and_quiet_hours() {
    let db = seeded();
    db.raw(
        "INSERT INTO notification_prefs (user_id, org_id, event_class, muted, quiet_start_min, quiet_end_min, quiet_tz) \
         VALUES ('u_ann', 'org_acme', 'task.done', 0, 1320, 420, 'Europe/Berlin')",
    )
    .expect("quiet hours 22:00 to 07:00");
    db.raw("INSERT INTO notification_prefs (user_id, org_id, event_class, muted) VALUES ('u_ann', 'org_acme', 'task.failed', 1)")
        .expect("muted, no quiet hours");
    assert!(db
        .raw("INSERT INTO notification_prefs (user_id, org_id, event_class, quiet_start_min) VALUES ('u_bob', 'org_acme', 'x', 600)")
        .is_err(), "quiet hours need both ends");
    assert!(db
        .raw("INSERT INTO notification_prefs (user_id, org_id, event_class, muted) VALUES ('u_bob', 'org_acme', 'x', 2)")
        .is_err(), "muted is 0 or 1");
    assert!(db
        .raw("INSERT INTO notification_prefs (user_id, org_id, event_class, muted) VALUES ('u_ann', 'org_acme', 'task.done', 1)")
        .is_err(), "one pref per user, org and class");
}

#[test]
fn usage_rollups_are_daily_per_metric() {
    let db = seeded();
    db.raw("INSERT INTO usage_rollups (org_id, day, metric, value) VALUES ('org_acme', '2026-10-28', 'tasks_started', 3)")
        .expect("rollup");
    db.raw("INSERT INTO usage_rollups (org_id, day, metric, value) VALUES ('org_acme', '2026-10-28', 'tokens', 0)")
        .expect("another metric, same day");
    assert!(db
        .raw("INSERT INTO usage_rollups (org_id, day, metric, value) VALUES ('org_acme', '28-10-2026', 'tasks_started', 1)")
        .is_err(), "day is YYYY-MM-DD");
    assert!(db
        .raw("INSERT INTO usage_rollups (org_id, day, metric, value) VALUES ('org_acme', '2026-10-29', 'tokens', -1)")
        .is_err(), "values are not negative");
    assert!(db
        .raw("INSERT INTO usage_rollups (org_id, day, metric, value) VALUES ('org_acme', '2026-10-28', 'tokens', 9)")
        .is_err(), "one row per org, day and metric");
}

#[test]
fn deleting_an_org_cascades_to_its_members_and_tasks() {
    let db = seeded();
    upsert(&db, "task_1", "org_beta", "thread_1", "beta task", &[]);
    db.raw("DELETE FROM orgs WHERE id = 'org_beta'")
        .expect("delete org with no audit rows");
    assert_eq!(db.count("SELECT count(*) AS n FROM tasks"), 0);
    assert_eq!(db.count("SELECT count(*) AS n FROM tasks_fts"), 0);
    assert_eq!(
        db.count("SELECT count(*) AS n FROM members WHERE org_id = 'org_beta'"),
        0
    );
    assert_eq!(
        db.count("SELECT count(*) AS n FROM members WHERE org_id = 'org_acme'"),
        2
    );
}
