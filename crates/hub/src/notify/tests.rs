//! Native tests of the notifier over an in-memory SQLite that runs the real migrations (the same
//! pattern as `auth/tests.rs`): endpoints, prefs, the consumer's `process_job` with fake
//! providers, and Web Push delivery with a fake push service that decrypts what it receives.

use std::cell::RefCell;
use std::future::{Future, ready};
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use p256::SecretKey;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use protocol::turn_event::{Envelope, Event};
use rusqlite::params_from_iter;
use rusqlite::types::Value as SqlValue;
use serde_json::{Value, json};

use super::job::{Kind, Notification, NotifyJob, jobs_for};
use super::provider::{
    Apns, Delivery, Fcm, HttpSender, ProviderKind, Providers, PushProvider, PushRequest,
    Subscription, WebPush,
};
use super::service::{
    collapse_batch, get_prefs, process_job, put_prefs, subscribe, unsubscribe, validate_endpoint,
};
use super::webpush::{Vapid, b64_decode, b64_encode, decrypt};
use crate::auth::Principal;
use crate::db::{Db, DbError, MIGRATIONS, Param, Role, Row};

// ---- an in-memory D1 ----------------------------------------------------------------------

struct Fake {
    conn: RefCell<rusqlite::Connection>,
}

impl Fake {
    fn new() -> Self {
        let conn = rusqlite::Connection::open_in_memory().expect("open sqlite");
        conn.pragma_update(None, "foreign_keys", true)
            .expect("foreign keys");
        for (name, sql) in MIGRATIONS {
            conn.execute_batch(sql)
                .unwrap_or_else(|error| panic!("migration {name}: {error}"));
        }
        Self {
            conn: RefCell::new(conn),
        }
    }

    fn exec(&self, sql: &str) {
        self.conn.borrow().execute_batch(sql).expect("exec");
    }

    fn rows(&self, sql: &str) -> Vec<Row> {
        run_query(&self.conn.borrow(), sql, &[]).expect("query")
    }

    fn count(&self, table: &str) -> usize {
        self.rows(&format!("SELECT COUNT(*) AS n FROM {table}"))[0]["n"]
            .as_u64()
            .expect("count") as usize
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

fn run_query(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[Param],
) -> Result<Vec<Row>, DbError> {
    let mut statement = conn.prepare(sql).map_err(backend)?;
    let names: Vec<String> = statement.column_names().map(|n| n.to_string()).collect();
    let mut rows = statement
        .query(params_from_iter(bind(params)))
        .map_err(backend)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(backend)? {
        let mut object = Row::new();
        for (index, name) in names.iter().enumerate() {
            let value: SqlValue = row.get(index).map_err(backend)?;
            object.insert(
                name.clone(),
                match value {
                    SqlValue::Null | SqlValue::Blob(_) => Value::Null,
                    SqlValue::Integer(v) => json!(v),
                    SqlValue::Real(v) => json!(v),
                    SqlValue::Text(t) => Value::String(t),
                },
            );
        }
        out.push(object);
    }
    Ok(out)
}

impl Db for Fake {
    fn execute(&self, sql: &str, params: &[Param]) -> impl Future<Output = Result<u64, DbError>> {
        let conn = self.conn.borrow();
        let result = conn
            .prepare(sql)
            .and_then(|mut s| s.execute(params_from_iter(bind(params))))
            .map(|n| n as u64)
            .map_err(backend);
        ready(result)
    }

    fn query(
        &self,
        sql: &str,
        params: &[Param],
    ) -> impl Future<Output = Result<Vec<Row>, DbError>> {
        ready(run_query(&self.conn.borrow(), sql, params))
    }
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("future pending: the fakes never yield"),
    }
}

// ---- fixtures -----------------------------------------------------------------------------

const NOON: u64 = 12 * 3_600_000;
const THREE_AM: u64 = 3 * 3_600_000;

/// org_a with ada (owner), bob (member), vic (viewer), each with a device; org_b with eve.
fn world() -> Fake {
    let db = Fake::new();
    db.exec(
        "INSERT INTO orgs (id, slug, name) VALUES ('org_a', 'a', 'A'), ('org_b', 'b', 'B');
         INSERT INTO users (id, display_name) VALUES ('u_ada', 'Ada'), ('u_bob', 'Bob'),
           ('u_vic', 'Vic'), ('u_eve', 'Eve');
         INSERT INTO members (org_id, user_id, role) VALUES ('org_a', 'u_ada', 'owner'),
           ('org_a', 'u_bob', 'member'), ('org_a', 'u_vic', 'viewer'), ('org_b', 'u_eve', 'owner');
         INSERT INTO devices (id, org_id, user_id, name) VALUES ('d_ada', 'org_a', 'u_ada', 'phone'),
           ('d_bob', 'org_a', 'u_bob', 'phone'), ('d_vic', 'org_a', 'u_vic', 'phone'),
           ('d_eve', 'org_b', 'u_eve', 'phone');
         INSERT INTO thread_hosts (thread_id, org_id, machine_id) VALUES
           ('th_1', 'org_a', 'm_1'), ('th_2', 'org_a', 'm_1'), ('th_b', 'org_b', 'm_2');",
    );
    db
}

fn principal(user: &str, org: &str, role: Role) -> Principal {
    Principal {
        user_id: format!("u_{user}"),
        device_id: format!("d_{user}"),
        org_id: org.to_owned(),
        role,
        display_name: user.to_owned(),
        session_id: None,
    }
}

/// A browser's push subscription: its private key and auth secret stay with the test.
struct Browser {
    secret: SecretKey,
    auth: [u8; 16],
}

impl Browser {
    fn new(seed: u8) -> Self {
        let mut scalar = [seed; 32];
        scalar[0] = 1;
        Self {
            secret: SecretKey::from_slice(&scalar).expect("scalar"),
            auth: [seed; 16],
        }
    }

    fn body(&self, endpoint: &str) -> Value {
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        let public = self.secret.public_key().to_encoded_point(false);
        json!({
            "endpoint": endpoint,
            "keys": { "p256dh": b64_encode(public.as_bytes()), "auth": b64_encode(&self.auth) },
        })
    }
}

fn subscribe_as(
    db: &Fake,
    who: &Principal,
    browser: &Browser,
    endpoint: &str,
) -> Result<(u16, Value), crate::threads::ApiError> {
    block_on(subscribe(db, who, &browser.body(endpoint), false, "req_1"))
}

// ---- subscriptions and prefs --------------------------------------------------------------

#[test]
fn subscribe_is_idempotent_per_endpoint_and_belongs_to_user_and_device() {
    let db = world();
    let bob = principal("bob", "org_a", Role::Member);
    let browser = Browser::new(7);
    let endpoint = "https://push.example.net/wpush/abc";

    let (status, first) = subscribe_as(&db, &bob, &browser, endpoint).expect("first");
    assert_eq!(status, 201);
    let (status, again) = subscribe_as(&db, &bob, &browser, endpoint).expect("again");
    assert_eq!(status, 200);
    assert_eq!(first["id"], again["id"]);
    assert_eq!(db.count("push_subscriptions"), 1);
    let row = &db.rows("SELECT user_id, device_id, org_id, provider FROM push_subscriptions")[0];
    assert_eq!(row["user_id"], "u_bob");
    assert_eq!(row["device_id"], "d_bob");
    assert_eq!(row["org_id"], "org_a");
    assert_eq!(row["provider"], "webpush");

    // Another user signing in on the same browser takes the endpoint over; still one row.
    let ada = principal("ada", "org_a", Role::Owner);
    let (status, taken) = subscribe_as(&db, &ada, &browser, endpoint).expect("takeover");
    assert_eq!(status, 200);
    assert_eq!(taken["id"], first["id"]);
    assert_eq!(db.count("push_subscriptions"), 1);
    assert_eq!(
        db.rows("SELECT user_id FROM push_subscriptions")[0]["user_id"],
        "u_ada"
    );
}

#[test]
fn subscribe_needs_member_and_valid_input() {
    let db = world();
    let browser = Browser::new(7);
    let vic = principal("vic", "org_a", Role::Viewer);
    let error = subscribe_as(&db, &vic, &browser, "https://push.example.net/x").unwrap_err();
    assert_eq!(error.status, 403);

    let bob = principal("bob", "org_a", Role::Member);
    let bad_bodies = [
        json!({}),
        json!({"endpoint": "https://push.example.net/x"}),
        json!({"endpoint": "https://push.example.net/x", "keys": {"p256dh": "AAAA", "auth": "AAAA"}}),
        json!({"endpoint": 5, "keys": {}}),
    ];
    for body in bad_bodies {
        let error = block_on(subscribe(&db, &bob, &body, false, "req")).unwrap_err();
        assert_eq!(error.status, 400, "{body}");
    }
    for endpoint in [
        "http://push.example.net/x",
        "https://127.0.0.1/x",
        "https://localhost/x",
        "https://10.0.0.5/x",
        "https://intranet/x",
        "https://user:pw@push.example.net/x",
        "ftp://push.example.net/x",
        "not a url",
    ] {
        let error = subscribe_as(&db, &bob, &browser, endpoint).unwrap_err();
        assert_eq!(error.status, 400, "{endpoint}");
    }
    assert_eq!(db.count("push_subscriptions"), 0);
}

#[test]
fn endpoint_rules_allow_loopback_http_only_for_dev_fixtures() {
    assert!(validate_endpoint("http://127.0.0.1:9999/push", true).is_ok());
    assert!(validate_endpoint("http://127.0.0.1:9999/push", false).is_err());
    assert!(validate_endpoint("https://fcm.googleapis.com/fcm/send/abc", false).is_ok());
    assert!(validate_endpoint("http://push.example.net/x", true).is_err());
}

#[test]
fn unsubscribe_removes_only_the_callers_subscription() {
    let db = world();
    let (bob, ada) = (
        principal("bob", "org_a", Role::Member),
        principal("ada", "org_a", Role::Owner),
    );
    let endpoint = "https://push.example.net/wpush/abc";
    subscribe_as(&db, &bob, &Browser::new(7), endpoint).expect("subscribe");

    // Ada cannot delete Bob's subscription; the answer is the same, nothing changes.
    block_on(unsubscribe(&db, &ada, &json!({"endpoint": endpoint}), "r")).expect("ada");
    assert_eq!(db.count("push_subscriptions"), 1);
    block_on(unsubscribe(&db, &bob, &json!({"endpoint": endpoint}), "r")).expect("bob");
    assert_eq!(db.count("push_subscriptions"), 0);
    // A retry is harmless.
    block_on(unsubscribe(&db, &bob, &json!({"endpoint": endpoint}), "r")).expect("retry");
    assert!(block_on(unsubscribe(&db, &bob, &json!({}), "r")).is_err());
    // The endpoint is not audited, only the subscription and the fact.
    let audit = db.rows("SELECT detail_json FROM audit_log");
    assert!(!audit.is_empty());
    for row in audit {
        assert!(
            !row["detail_json"]
                .as_str()
                .unwrap()
                .contains("push.example.net")
        );
    }
}

#[test]
fn prefs_default_then_round_trip_through_d1() {
    let db = world();
    let bob = principal("bob", "org_a", Role::Member);
    let defaults = block_on(get_prefs(&db, &bob)).expect("defaults");
    assert_eq!(defaults["classes"]["approval_needed"], true);
    assert_eq!(defaults["quiet_hours"], Value::Null);
    assert_eq!(defaults["muted_threads"], json!([]));

    let body = json!({
        "classes": {"task_done": false},
        "muted_threads": ["th_2"],
        "quiet_hours": {"start": "22:00", "end": "07:00", "tz": "UTC"},
    });
    let saved = block_on(put_prefs(&db, &bob, &body, NOON, "r")).expect("put");
    assert_eq!(block_on(get_prefs(&db, &bob)).expect("get"), saved);
    assert_eq!(saved["classes"]["task_done"], false);
    assert_eq!(saved["muted_threads"], json!(["th_2"]));
    assert_eq!(saved["quiet_hours"]["tz"], "UTC");

    // PUT replaces: no mutes, no quiet hours, class back on.
    let cleared = block_on(put_prefs(&db, &bob, &json!({}), NOON, "r")).expect("clear");
    assert_eq!(cleared["classes"]["task_done"], true);
    assert_eq!(cleared["muted_threads"], json!([]));
    assert_eq!(cleared["quiet_hours"], Value::Null);
    assert_eq!(db.count("notification_thread_mutes"), 0);

    // Prefs are per user: Ada still has the defaults.
    let ada = principal("ada", "org_a", Role::Owner);
    assert_eq!(
        block_on(get_prefs(&db, &ada)).unwrap()["classes"]["task_done"],
        true
    );
    let error = block_on(put_prefs(
        &db,
        &bob,
        &json!({"classes": {"x": true}}),
        NOON,
        "r",
    ));
    assert_eq!(error.unwrap_err().status, 400);
}

// ---- jobs from events ---------------------------------------------------------------------

fn envelope(task: Option<&str>, event: Value) -> Envelope {
    serde_json::from_value(json!({
        "v": 1, "seq": 1, "thread_id": "th_1", "task_id": task, "run_id": "run_1",
        "ts": "2026-10-28T10:00:00.000Z", "event": event,
    }))
    .expect("envelope")
}

const SECRET_TITLE: &str = "Run rm -rf /home/ada/secret-plans.txt?";

fn interrupt_event() -> Value {
    json!({
        "type": "Interrupt", "interrupt_id": "int_1", "kind": "approval",
        "title": SECRET_TITLE, "message": "contents of secret-plans.txt",
        "expires_at": "2026-10-28T11:00:00.000Z",
        "options": [
            {"option_id": "allow_once", "name": "Allow", "kind": "allow_once"},
            {"option_id": "reject_once", "name": "Reject", "kind": "reject_once"},
        ],
    })
}

#[test]
fn events_map_to_jobs_with_ids_only() {
    let appended = vec![
        envelope(
            Some("tk_1"),
            json!({"type": "RunStarted", "reason": "prompt"}),
        ),
        envelope(Some("tk_1"), interrupt_event()),
        envelope(
            Some("tk_1"),
            json!({"type": "RunFinished", "outcome": {"type": "interrupt", "interrupt_ids": ["int_1"]}}),
        ),
        envelope(
            Some("tk_2"),
            json!({"type": "RunFinished", "outcome": {"type": "success", "stop_reason": "end_turn"}}),
        ),
        envelope(
            Some("tk_3"),
            json!({"type": "RunError", "reason": "error", "message": "boom secret-plans.txt"}),
        ),
        envelope(
            Some("tk_4"),
            json!({"type": "RunError", "reason": "cancelled", "message": "x"}),
        ),
        envelope(
            Some("tk_5"),
            json!({"type": "RunFinished", "outcome": {"type": "interrupt", "interrupt_ids": ["int_9"]}}),
        ),
    ];
    let jobs = jobs_for(&appended);
    let kinds: Vec<(Kind, Option<&str>, Option<&str>)> = jobs
        .iter()
        .map(|j| (j.kind, j.task_id.as_deref(), j.interrupt_id.as_deref()))
        .collect();
    assert_eq!(
        kinds,
        [
            (Kind::ApprovalNeeded, Some("tk_1"), Some("int_1")),
            (Kind::TaskDone, Some("tk_2"), None),
            (Kind::TaskFailed, Some("tk_3"), None),
            // The RunFinished{interrupt} of tk_1 is covered by its Interrupt; tk_4 was cancelled.
            (Kind::ApprovalNeeded, Some("tk_5"), Some("int_9")),
        ]
    );
    assert_eq!(jobs[0].allow_option_id.as_deref(), Some("allow_once"));
    assert_eq!(jobs[0].deny_option_id.as_deref(), Some("reject_once"));

    let text = serde_json::to_string(&jobs).unwrap();
    for leaked in ["rm -rf", "secret-plans", "boom", "contents"] {
        assert!(!text.contains(leaked), "job leaks {leaked}: {text}");
    }
    // Events that never notify.
    assert!(
        jobs_for(&[envelope(
            Some("tk_1"),
            json!({"type": "RunStarted", "reason": "prompt"})
        )])
        .is_empty()
    );
    assert!(matches!(appended[1].event, Event::Interrupt(_)));
}

#[test]
fn payload_is_ids_a_generic_title_and_approval_actions() {
    let jobs = jobs_for(&[envelope(Some("tk_1"), interrupt_event())]);
    let approval = Notification::new(jobs[0].clone()).payload();
    assert_eq!(
        approval,
        json!({
            "kind": "approval_needed", "title": "Approval needed",
            "thread_id": "th_1", "task_id": "tk_1", "interrupt_id": "int_1",
            "actions": [
                {"action": "allow", "title": "Allow", "option_id": "allow_once"},
                {"action": "deny", "title": "Deny", "option_id": "reject_once"},
            ],
        })
    );
    let done = NotifyJob {
        kind: Kind::TaskDone,
        thread_id: "th_1".into(),
        task_id: Some("tk_1".into()),
        interrupt_id: None,
        allow_option_id: None,
        deny_option_id: None,
    };
    assert_eq!(
        Notification::new(done).payload(),
        json!({"kind": "task_done", "title": "Task finished", "thread_id": "th_1", "task_id": "tk_1"})
    );
}

#[test]
fn topic_is_the_task_id_or_a_32_character_hash() {
    let job = |task: Option<&str>| NotifyJob {
        kind: Kind::TaskDone,
        thread_id: "th:1.x".into(),
        task_id: task.map(str::to_owned),
        interrupt_id: None,
        allow_option_id: None,
        deny_option_id: None,
    };
    assert_eq!(
        Notification::new(job(Some("tk_01JBX5"))).topic(),
        "tk_01JBX5"
    );
    let long = Notification::new(job(Some("tk_01JBX5ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ"))).topic();
    assert_eq!(long.len(), 32);
    let dotted = Notification::new(job(Some("tk:1.2"))).topic();
    assert_eq!(
        dotted.len(),
        32,
        "':' and '.' are not valid topic characters"
    );
    assert_eq!(dotted, Notification::new(job(Some("tk:1.2"))).topic());
    assert_ne!(dotted, Notification::new(job(Some("tk:1.3"))).topic());
    assert_eq!(Notification::new(job(None)).topic().len(), 32);
}

#[test]
fn a_batch_sends_only_the_last_job_per_task() {
    let job = |task: &str, kind| NotifyJob {
        kind,
        thread_id: "th_1".into(),
        task_id: Some(task.into()),
        interrupt_id: None,
        allow_option_id: None,
        deny_option_id: None,
    };
    let jobs = [
        job("tk_1", Kind::ApprovalNeeded),
        job("tk_2", Kind::TaskDone),
        job("tk_1", Kind::TaskDone),
    ];
    assert_eq!(collapse_batch(&jobs), [false, true, true]);
}

// ---- the consumer -------------------------------------------------------------------------

/// Records what it was asked to send and answers with a canned result.
struct Recording {
    sent: RefCell<Vec<(String, Value)>>,
    answer: RefCell<Delivery>,
}

impl Recording {
    fn new(answer: Delivery) -> Self {
        Self {
            sent: RefCell::new(Vec::new()),
            answer: RefCell::new(answer),
        }
    }
}

impl PushProvider for Recording {
    async fn send(&self, sub: &Subscription, note: &Notification, _now_secs: u64) -> Delivery {
        self.sent
            .borrow_mut()
            .push((sub.user_id.clone(), note.payload()));
        self.answer.borrow().clone()
    }
}

fn approval_job() -> NotifyJob {
    jobs_for(&[envelope(Some("tk_1"), interrupt_event())]).remove(0)
}

fn job_for(thread: &str, kind: Kind) -> NotifyJob {
    NotifyJob {
        kind,
        thread_id: thread.into(),
        task_id: Some("tk_1".into()),
        interrupt_id: None,
        allow_option_id: None,
        deny_option_id: None,
    }
}

/// Subscribes ada, bob, vic (org_a) and eve (org_b) with one endpoint each.
fn subscribed_world() -> Fake {
    let db = world();
    for (user, org, role, seed) in [
        ("ada", "org_a", Role::Owner, 1),
        ("bob", "org_a", Role::Member, 2),
        ("vic", "org_a", Role::Viewer, 3),
        ("eve", "org_b", Role::Owner, 4),
    ] {
        // Subscribing needs member: seed the viewer's row directly.
        let who = principal(user, org, Role::Member);
        let endpoint = format!("https://push.example.net/{user}");
        let _ = role;
        subscribe_as(&db, &who, &Browser::new(seed), &endpoint).expect("subscribe");
    }
    db
}

fn run(db: &Fake, web: &Recording, job: &NotifyJob, now_ms: u64) -> super::service::JobReport {
    let providers = Providers {
        web: Recording {
            sent: RefCell::new(Vec::new()),
            answer: RefCell::new(web.answer.borrow().clone()),
        },
        apns: Apns,
        fcm: Fcm,
    };
    let report = block_on(process_job(db, &providers, job, now_ms)).expect("process");
    *web.sent.borrow_mut() = providers.web.sent.into_inner();
    report
}

fn users(web: &Recording) -> Vec<String> {
    let mut users: Vec<String> = web.sent.borrow().iter().map(|(u, _)| u.clone()).collect();
    users.sort();
    users
}

#[test]
fn an_approval_goes_to_members_of_the_threads_org_only() {
    let db = subscribed_world();
    let web = Recording::new(Delivery::Delivered);
    let report = run(&db, &web, &approval_job(), NOON);
    // Ada (owner) and Bob (member) are asked; Vic (viewer) cannot answer; Eve is in another org.
    assert_eq!(users(&web), ["u_ada", "u_bob"]);
    assert_eq!((report.delivered, report.skipped), (2, 1));
    let payload = &web.sent.borrow()[0].1;
    assert_eq!(payload["interrupt_id"], "int_1");
    assert_eq!(payload["actions"][0]["action"], "allow");
    let text = payload.to_string();
    assert!(!text.contains("rm -rf") && !text.contains("secret-plans"));

    // A task finishing is sent to the viewer too.
    let web = Recording::new(Delivery::Delivered);
    run(&db, &web, &job_for("th_1", Kind::TaskDone), NOON);
    assert_eq!(users(&web), ["u_ada", "u_bob", "u_vic"]);

    // Org B's thread reaches org B only; an unmapped thread reaches nobody.
    let web = Recording::new(Delivery::Delivered);
    run(&db, &web, &job_for("th_b", Kind::TaskDone), NOON);
    assert_eq!(users(&web), ["u_eve"]);
    let web = Recording::new(Delivery::Delivered);
    run(&db, &web, &job_for("th_nowhere", Kind::TaskDone), NOON);
    assert!(users(&web).is_empty());
}

#[test]
fn prefs_decide_class_mute_and_quiet_hours() {
    let db = subscribed_world();
    let bob = principal("bob", "org_a", Role::Member);
    let ada = principal("ada", "org_a", Role::Owner);
    block_on(put_prefs(
        &db,
        &bob,
        &json!({"classes": {"task_done": false}, "muted_threads": ["th_2"]}),
        NOON,
        "r",
    ))
    .expect("bob prefs");
    block_on(put_prefs(
        &db,
        &ada,
        &json!({"quiet_hours": {"start": "22:00", "end": "07:00", "tz": "UTC"}}),
        NOON,
        "r",
    ))
    .expect("ada prefs");

    // Class off: Bob skips task_done, still gets task_failed.
    let web = Recording::new(Delivery::Delivered);
    run(&db, &web, &job_for("th_1", Kind::TaskDone), NOON);
    assert_eq!(users(&web), ["u_ada", "u_vic"]);
    let web = Recording::new(Delivery::Delivered);
    run(&db, &web, &job_for("th_1", Kind::TaskFailed), NOON);
    assert_eq!(users(&web), ["u_ada", "u_bob", "u_vic"]);

    // Muted thread: Bob skips everything from th_2.
    let web = Recording::new(Delivery::Delivered);
    run(&db, &web, &job_for("th_2", Kind::TaskFailed), NOON);
    assert_eq!(users(&web), ["u_ada", "u_vic"]);

    // Quiet hours: at 03:00 UTC Ada is skipped, at noon she is not.
    let web = Recording::new(Delivery::Delivered);
    let report = run(&db, &web, &job_for("th_1", Kind::TaskFailed), THREE_AM);
    assert_eq!(users(&web), ["u_bob", "u_vic"]);
    assert_eq!(report.skipped, 1);
}

#[test]
fn gone_subscriptions_are_deleted_and_temporary_failures_ask_for_a_retry() {
    let db = subscribed_world();
    let web = Recording::new(Delivery::Gone);
    let report = run(&db, &web, &job_for("th_1", Kind::TaskDone), NOON);
    assert_eq!((report.removed, report.retry), (3, false));
    // Org A's three subscriptions are gone, Eve's stays.
    assert_eq!(db.count("push_subscriptions"), 1);

    let db = subscribed_world();
    let web = Recording::new(Delivery::Retry("push service answered 503".into()));
    let report = run(&db, &web, &job_for("th_1", Kind::TaskDone), NOON);
    assert!(report.retry);
    assert_eq!(
        db.count("push_subscriptions"),
        4,
        "a 5xx keeps the subscription"
    );

    let web = Recording::new(Delivery::Failed("push service answered 400".into()));
    let report = run(&db, &web, &job_for("th_1", Kind::TaskDone), NOON);
    assert_eq!((report.failed, report.retry), (3, false));
}

#[test]
fn apns_and_fcm_are_stubs_that_say_not_configured() {
    let sub = Subscription {
        id: "s".into(),
        user_id: "u".into(),
        provider: ProviderKind::Apns,
        endpoint: "token".into(),
        p256dh: None,
        auth: None,
    };
    let note = Notification::new(job_for("th_1", Kind::TaskDone));
    assert_eq!(block_on(Apns.send(&sub, &note, 0)), Delivery::NotConfigured);
    assert_eq!(block_on(Fcm.send(&sub, &note, 0)), Delivery::NotConfigured);
    // Through the dispatcher, by the subscription's kind.
    let providers = Providers {
        web: Recording::new(Delivery::Delivered),
        apns: Apns,
        fcm: Fcm,
    };
    assert_eq!(
        block_on(providers.send(&sub, &note, 0)),
        Delivery::NotConfigured
    );
    let fcm = Subscription {
        provider: ProviderKind::Fcm,
        ..sub
    };
    assert_eq!(
        block_on(providers.send(&fcm, &note, 0)),
        Delivery::NotConfigured
    );
    assert!(providers.web.sent.borrow().is_empty());
}

// ---- Web Push over a fake push service ----------------------------------------------------

/// A fake push service: records requests and answers with a canned status.
struct FakeService {
    status: RefCell<Result<u16, String>>,
    seen: RefCell<Vec<PushRequest>>,
}

impl HttpSender for &FakeService {
    fn post(&self, request: PushRequest) -> impl Future<Output = Result<u16, String>> {
        self.seen.borrow_mut().push(request);
        ready(self.status.borrow().clone())
    }
}

fn header<'a>(request: &'a PushRequest, name: &str) -> &'a str {
    request
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("no {name} header"))
}

#[test]
fn web_push_sends_an_encrypted_vapid_signed_request_that_only_the_browser_can_read() {
    let service = FakeService {
        status: RefCell::new(Ok(201)),
        seen: RefCell::new(Vec::new()),
    };
    let vapid = Vapid::from_scalar(&[9; 32], "mailto:ops@example.com").expect("vapid");
    let public = vapid.public_key().to_owned();
    let provider = WebPush {
        http: &service,
        vapid: Some(vapid),
    };
    let browser = Browser::new(5);
    let body = browser.body("https://push.example.net/wpush/abc");
    let sub = Subscription {
        id: "s".into(),
        user_id: "u_bob".into(),
        provider: ProviderKind::WebPush,
        endpoint: "https://push.example.net/wpush/abc".into(),
        p256dh: body["keys"]["p256dh"].as_str().map(str::to_owned),
        auth: body["keys"]["auth"].as_str().map(str::to_owned),
    };
    let note = Notification::new(approval_job());

    assert_eq!(
        block_on(provider.send(&sub, &note, 1_700_000_000)),
        Delivery::Delivered
    );
    let request = &service.seen.borrow()[0].clone();
    assert_eq!(request.url, "https://push.example.net/wpush/abc");
    assert_eq!(header(request, "Content-Encoding"), "aes128gcm");
    assert_eq!(header(request, "Topic"), "tk_1");
    assert_eq!(header(request, "Urgency"), "high");
    assert_eq!(header(request, "TTL"), "86400");

    // The body is ciphertext: no id and no text in the clear.
    let raw = String::from_utf8_lossy(&request.body).into_owned();
    assert!(!raw.contains("th_1") && !raw.contains("tk_1") && !raw.contains("approval"));
    // The browser decrypts it with its own key and finds ids only.
    let plain = decrypt(&request.body, &browser.secret, &browser.auth).expect("decrypt");
    let payload: Value = serde_json::from_slice(&plain).expect("json");
    assert_eq!(payload["thread_id"], "th_1");
    assert_eq!(payload["task_id"], "tk_1");
    assert_eq!(payload["interrupt_id"], "int_1");
    assert_eq!(payload["kind"], "approval_needed");
    assert_eq!(payload["title"], "Approval needed");
    let text = String::from_utf8(plain).unwrap();
    assert!(!text.contains("rm -rf") && !text.contains("secret-plans"));
    // Another browser's key cannot.
    let other = Browser::new(6);
    assert!(decrypt(&request.body, &other.secret, &other.auth).is_err());

    // The VAPID header verifies against the hub's public key, for the endpoint's origin.
    let authorization = header(request, "Authorization");
    let rest = authorization
        .strip_prefix("vapid t=")
        .expect("vapid scheme");
    let (jwt, k) = rest.split_once(", k=").expect("k");
    assert_eq!(k, public);
    let parts: Vec<&str> = jwt.split('.').collect();
    let claims: Value = serde_json::from_slice(&b64_decode(parts[1]).unwrap()).unwrap();
    assert_eq!(claims["aud"], "https://push.example.net");
    let verifying = VerifyingKey::from_sec1_bytes(&b64_decode(k).unwrap()).unwrap();
    let signature = Signature::from_slice(&b64_decode(parts[2]).unwrap()).unwrap();
    verifying
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
        .expect("signature");

    // A task-done push is normal urgency.
    let done = Notification::new(job_for("th_1", Kind::TaskDone));
    block_on(provider.send(&sub, &done, 1_700_000_000));
    assert_eq!(header(&service.seen.borrow()[1], "Urgency"), "normal");
}

#[test]
fn web_push_maps_push_service_statuses() {
    let vapid = Vapid::from_scalar(&[9; 32], "mailto:ops@example.com").expect("vapid");
    let service = FakeService {
        status: RefCell::new(Ok(201)),
        seen: RefCell::new(Vec::new()),
    };
    let provider = WebPush {
        http: &service,
        vapid: Some(vapid),
    };
    let browser = Browser::new(5);
    let body = browser.body("https://push.example.net/wpush/abc");
    let sub = Subscription {
        id: "s".into(),
        user_id: "u".into(),
        provider: ProviderKind::WebPush,
        endpoint: "https://push.example.net/wpush/abc".into(),
        p256dh: body["keys"]["p256dh"].as_str().map(str::to_owned),
        auth: body["keys"]["auth"].as_str().map(str::to_owned),
    };
    let note = Notification::new(job_for("th_1", Kind::TaskDone));
    let send = |status: Result<u16, String>| {
        *service.status.borrow_mut() = status;
        block_on(provider.send(&sub, &note, 0))
    };
    assert_eq!(send(Ok(200)), Delivery::Delivered);
    assert_eq!(send(Ok(404)), Delivery::Gone);
    assert_eq!(send(Ok(410)), Delivery::Gone);
    assert!(matches!(send(Ok(429)), Delivery::Retry(_)));
    assert!(matches!(send(Ok(500)), Delivery::Retry(_)));
    assert!(matches!(send(Ok(503)), Delivery::Retry(_)));
    assert!(matches!(
        send(Err("connection reset".into())),
        Delivery::Retry(_)
    ));
    assert!(matches!(send(Ok(400)), Delivery::Failed(_)));
    assert!(matches!(send(Ok(413)), Delivery::Failed(_)));

    // Without a VAPID key nothing is sent.
    let unconfigured = WebPush {
        http: &service,
        vapid: None,
    };
    let before = service.seen.borrow().len();
    assert_eq!(
        block_on(unconfigured.send(&sub, &note, 0)),
        Delivery::NotConfigured
    );
    assert_eq!(service.seen.borrow().len(), before);
}
