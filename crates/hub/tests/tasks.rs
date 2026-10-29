//! M2-010 live tests of threads as tasks against a running hub (`wrangler dev --local`): the
//! auto-title, `GET /v1/threads` (filters, `q` search, archive, cursor shape), `PATCH` (rename,
//! archive), `DELETE` (admin soft delete), the roles, the audit rows, and that a second org never
//! sees the first org's threads.
//!
//! Needs `HUB_URL`, a hub with `DEV_FIXTURES=1` (`crates/hub/.dev.vars`) and `BOOTSTRAP_OWNER_TOKEN`
//! in this process's environment. `bash scripts/hub-test.sh tasks` does all of that. Without them
//! every test fails with a message saying so. The `live` feature keeps this file out of a plain
//! `cargo test -p hub`; the native index and query tests are in `src/tasks/tests.rs`.
//!
//! Orgs: `Bearer dev` is the dev org. The second org is made by the one owner bootstrap of the
//! hub (`POST /v1/auth/bootstrap`), done once by [`second_org`] for all tests.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const READ_TIMEOUT: Duration = Duration::from_secs(20);

// ---- plain HTTP -------------------------------------------------------------------------

struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("body is not JSON ({error}): {:?}", self.body))
    }
}

fn authority() -> String {
    let url = match std::env::var("HUB_URL") {
        Ok(url) if !url.trim().is_empty() => url.trim().trim_end_matches('/').to_owned(),
        _ => panic!(
            "HUB_URL is not set: this test needs a running hub. \
             Run it with `bash scripts/hub-test.sh tasks`."
        ),
    };
    url.strip_prefix("http://")
        .unwrap_or_else(|| panic!("HUB_URL must look like http://host:port, got {url}"))
        .to_owned()
}

/// One request with `Connection: close`. `token` becomes `Authorization: Bearer <token>`.
fn http(
    method: &str,
    path: &str,
    token: Option<&str>,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> Reply {
    let authority = authority();
    let mut stream = TcpStream::connect(&authority)
        .unwrap_or_else(|error| panic!("cannot connect to HUB_URL {authority}: {error}"));
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("set read timeout");
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    let payload = body.map(Value::to_string).unwrap_or_default();
    if body.is_some() {
        request.push_str("Content-Type: application/json\r\n");
    }
    request.push_str(&format!(
        "Content-Length: {}\r\n\r\n{payload}",
        payload.len()
    ));
    stream.write_all(request.as_bytes()).expect("send request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    parse_reply(&String::from_utf8_lossy(&raw))
}

fn parse_reply(text: &str) -> Reply {
    let (head, body) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("malformed HTTP response: {text:?}"));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("bad status line in {head:?}"));
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(body)
    } else {
        body.to_owned()
    };
    Reply { status, body }
}

fn dechunk(mut rest: &str) -> String {
    let mut out = String::new();
    while let Some((size_line, after)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
            .unwrap_or(0);
        if size == 0 || after.len() < size {
            break;
        }
        out.push_str(&after[..size]);
        rest = after[size..].strip_prefix("\r\n").unwrap_or("");
    }
    out
}

// ---- fixtures ---------------------------------------------------------------------------

fn unique(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!(
        "{prefix}_{nanos:x}_{}",
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// A session for a new user with `role`, issued by the dev owner.
fn issue(role: &str) -> String {
    let body = json!({ "role": role, "display_name": unique("user") });
    let reply = http("POST", "/v1/auth/sessions", Some("dev"), &[], Some(&body));
    assert_eq!(reply.status, 201, "issue {role}: {}", reply.body);
    reply.json()["session"]["token"]
        .as_str()
        .expect("token")
        .to_owned()
}

/// The second org's owner token, from the hub's one bootstrap (shared by the tests).
fn second_org() -> &'static (String, String) {
    static ORG: OnceLock<(String, String)> = OnceLock::new();
    ORG.get_or_init(|| {
        let secret = match std::env::var("BOOTSTRAP_OWNER_TOKEN") {
            Ok(secret) if !secret.trim().is_empty() => secret,
            _ => panic!("BOOTSTRAP_OWNER_TOKEN is not set: run this with `bash scripts/hub-test.sh tasks`."),
        };
        let body = json!({ "org_name": "Tasks Test Org", "user_name": "Root", "user_email": "root@example.com" });
        let reply = http("POST", "/v1/auth/bootstrap", Some(&secret), &[], Some(&body));
        assert_eq!(reply.status, 201, "bootstrap: {}", reply.body);
        let out = reply.json();
        (
            out["session"]["token"].as_str().expect("token").to_owned(),
            out["org"]["id"].as_str().expect("org id").to_owned(),
        )
    })
}

/// A machine of the org `token` belongs to.
fn machine(token: &str) -> String {
    let body = json!({ "name": unique("mch"), "kind": "local" });
    let reply = http("POST", "/v1/machines", Some(token), &[], Some(&body));
    assert_eq!(reply.status, 201, "create machine: {}", reply.body);
    reply.json()["machine"]["id"]
        .as_str()
        .expect("machine id")
        .to_owned()
}

/// Creates a thread on `machine` with `token`. `title` and `prompt` are optional.
fn create_thread(token: &str, machine: &str, title: Option<&str>, prompt: Option<&str>) -> String {
    let mut body = json!({
        "machine_id": machine,
        "agent": "claude",
        "workspace": { "path": "/tmp/work" },
        "permission_mode": "default",
    });
    if let Some(title) = title {
        body["title"] = json!(title);
    }
    if let Some(text) = prompt {
        body["prompt"] = json!({
            "task_id": unique("tk"),
            "message_id": unique("msg"),
            "parts": [{ "type": "text", "text": text }],
        });
    }
    let key = unique("idk");
    let reply = http(
        "POST",
        "/v1/threads",
        Some(token),
        &[("Idempotency-Key", &key)],
        Some(&body),
    );
    assert_eq!(reply.status, 201, "create thread: {}", reply.body);
    reply.json()["thread_id"]
        .as_str()
        .expect("thread id")
        .to_owned()
}

/// `GET /v1/threads` with a raw query string; the answer's `threads` ids.
fn listed(token: &str, query: &str) -> Vec<String> {
    let reply = http(
        "GET",
        &format!("/v1/threads?{query}"),
        Some(token),
        &[],
        None,
    );
    assert_eq!(reply.status, 200, "list {query}: {}", reply.body);
    reply.json()["threads"]
        .as_array()
        .expect("threads")
        .iter()
        .map(|thread| thread["thread_id"].as_str().expect("id").to_owned())
        .collect()
}

fn rename(token: &str, thread: &str, body: &Value) -> Reply {
    http(
        "PATCH",
        &format!("/v1/threads/{thread}"),
        Some(token),
        &[],
        Some(body),
    )
}

fn delete(token: &str, thread: &str) -> Reply {
    let key = unique("idk");
    http(
        "DELETE",
        &format!("/v1/threads/{thread}"),
        Some(token),
        &[("Idempotency-Key", &key)],
        None,
    )
}

// ---- the tests --------------------------------------------------------------------------

#[test]
fn a_thread_is_listed_with_the_title_of_its_first_prompt() {
    let dev = "dev";
    let mch = machine(dev);
    let thread = create_thread(
        dev,
        &mch,
        None,
        Some("# Upgrade axum to 0.8\nthen run the tests"),
    );

    let reply = http(
        "GET",
        &format!("/v1/threads?machine_id={mch}"),
        Some(dev),
        &[],
        None,
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let body = reply.json();
    let row = body["threads"]
        .as_array()
        .expect("threads")
        .iter()
        .find(|row| row["thread_id"] == thread.as_str())
        .unwrap_or_else(|| panic!("thread not listed: {body}"))
        .clone();
    assert_eq!(row["title"], "Upgrade axum to 0.8");
    assert_eq!(row["machine_id"], mch.as_str());
    assert_eq!(row["agent"], "claude");
    assert!(
        row["updated_at"].is_string() && row["last_seq"].is_u64(),
        "{row}"
    );
    assert!(body["next_cursor"].is_null(), "one page: {body}");
}

#[test]
fn a_thread_without_a_prompt_is_idle_and_status_filters_it() {
    let dev = "dev";
    let mch = machine(dev);
    let thread = create_thread(dev, &mch, Some("Idle plan"), None);
    let reply = http(
        "GET",
        &format!("/v1/threads?machine_id={mch}"),
        Some(dev),
        &[],
        None,
    );
    let row = reply.json()["threads"][0].clone();
    assert_eq!(row["status"], "idle", "{row}");
    assert_eq!(
        listed(dev, &format!("machine_id={mch}&status=idle")),
        vec![thread.clone()]
    );
    assert!(listed(dev, &format!("machine_id={mch}&status=needs_you")).is_empty());
}

#[test]
fn q_searches_titles_and_a_rename_moves_the_thread_in_the_search() {
    let dev = "dev";
    let mch = machine(dev);
    let token = unique("zq");
    let thread = create_thread(dev, &mch, Some(&format!("{token} rollout")), None);

    assert_eq!(listed(dev, &format!("q={token}")), vec![thread.clone()]);
    let renamed = rename(dev, &thread, &json!({ "title": "Billing migration" }));
    assert_eq!(renamed.status, 200, "{}", renamed.body);
    assert_eq!(renamed.json()["title"], "Billing migration");
    assert!(
        listed(dev, &format!("q={token}")).is_empty(),
        "the old title no longer matches"
    );
    assert_eq!(listed(dev, "q=billing"), vec![thread]);
}

#[test]
fn archive_hides_a_thread_until_archived_is_asked_for() {
    let dev = "dev";
    let mch = machine(dev);
    let thread = create_thread(dev, &mch, Some("Archive me"), None);
    let by_machine = format!("machine_id={mch}");

    assert_eq!(
        rename(dev, &thread, &json!({ "archived": true })).status,
        200
    );
    assert!(
        !listed(dev, &by_machine).contains(&thread),
        "archived is left out by default"
    );
    assert!(listed(dev, &format!("{by_machine}&archived=true")).contains(&thread));

    assert_eq!(
        rename(dev, &thread, &json!({ "archived": false })).status,
        200
    );
    assert!(
        listed(dev, &by_machine).contains(&thread),
        "unarchived is listed again"
    );
}

#[test]
fn rename_needs_a_member_and_delete_needs_an_admin_and_both_are_audited() {
    let dev = "dev";
    let mch = machine(dev);
    let thread = create_thread(dev, &mch, Some("Roles"), None);
    let member = issue("member");
    let admin = issue("admin");
    let viewer = issue("viewer");

    // A viewer may list the thread but not rename or delete it.
    assert_eq!(
        listed(&viewer, &format!("machine_id={mch}")),
        vec![thread.clone()]
    );
    let denied = rename(&viewer, &thread, &json!({ "title": "nope" }));
    assert_eq!(denied.status, 403, "{}", denied.body);

    let renamed = rename(&member, &thread, &json!({ "title": "Member rename" }));
    assert_eq!(renamed.status, 200, "{}", renamed.body);

    // A member may not delete: admin is the role.
    let member_delete = delete(&member, &thread);
    assert_eq!(member_delete.status, 403, "{}", member_delete.body);

    let deleted = delete(&admin, &thread);
    assert_eq!(deleted.status, 204, "{}", deleted.body);
    assert!(!listed(dev, &format!("machine_id={mch}")).contains(&thread));
    assert!(
        !listed(dev, &format!("machine_id={mch}&archived=true")).contains(&thread),
        "a deleted thread stays out even when archived are included"
    );

    let audit = http("GET", "/v1/dev/audit?limit=1000", Some(dev), &[], None);
    assert_eq!(audit.status, 200, "{}", audit.body);
    let rows = audit.json()["entries"].as_array().expect("entries").clone();
    let for_thread = |action: &str| {
        rows.iter()
            .filter(|row| row["action"] == action && row["target_id"] == thread.as_str())
            .count()
    };
    assert!(for_thread("thread.update") >= 1, "rename is audited");
    assert!(for_thread("thread.delete") >= 1, "delete is audited");
}

#[test]
fn a_second_org_never_sees_the_first_orgs_threads() {
    let dev = "dev";
    let dev_machine = machine(dev);
    let token = unique("secretplan");
    let dev_thread = create_thread(dev, &dev_machine, Some(&format!("{token} for org A")), None);

    let (org_b_token, _) = second_org();
    let org_b_machine = machine(org_b_token);
    let org_b_thread = create_thread(org_b_token, &org_b_machine, Some("Org B thread"), None);

    let b_all = listed(org_b_token, "archived=true");
    assert!(b_all.contains(&org_b_thread), "org B sees its own thread");
    assert!(
        !b_all.contains(&dev_thread),
        "org B never sees org A's thread"
    );
    assert!(
        listed(org_b_token, &format!("q={token}")).is_empty(),
        "nor finds it by search"
    );
    assert!(listed(org_b_token, &format!("machine_id={dev_machine}")).is_empty());

    let a_all = listed(dev, "archived=true");
    assert!(a_all.contains(&dev_thread));
    assert!(
        !a_all.contains(&org_b_thread),
        "org A does not see org B's thread"
    );

    // Another org's thread is `404` on the thread routes, as before.
    let direct = http(
        "GET",
        &format!("/v1/threads/{dev_thread}"),
        Some(org_b_token),
        &[],
        None,
    );
    assert_eq!(direct.status, 404, "{}", direct.body);
}

#[test]
fn a_list_needs_a_session_and_a_valid_query() {
    let anonymous = http("GET", "/v1/threads", None, &[], None);
    assert_eq!(anonymous.status, 401, "{}", anonymous.body);
    let bad_status = http("GET", "/v1/threads?status=paused", Some("dev"), &[], None);
    assert_eq!(bad_status.status, 400, "{}", bad_status.body);
    assert_eq!(bad_status.json()["error"], "invalid_request");
}

#[test]
fn failed_and_cancelled_are_status_filters_that_never_match_a_done_thread() {
    let dev = "dev";
    let mch = machine(dev);
    let thread = create_thread(dev, &mch, Some("Finishes ok"), None);
    let task = unique("tk");
    assert_eq!(prompt(&thread, &task).status, 202);
    runner_event(
        &thread,
        &task,
        "run_a",
        json!({ "type": "RunStarted", "reason": "prompt" }),
    );
    runner_event(
        &thread,
        &task,
        "run_a",
        json!({ "type": "RunFinished", "outcome": { "type": "success", "stop_reason": "end_turn" } }),
    );
    let query = format!("machine_id={mch}");
    let has = |wanted: &str| listed(dev, &format!("{query}&status={wanted}")).contains(&thread);
    assert!(has("done"));
    assert!(!has("failed") && !has("cancelled"));
    for wanted in ["failed", "cancelled"] {
        let reply = http(
            "GET",
            &format!("/v1/threads?{query}&status={wanted}"),
            Some(dev),
            &[],
            None,
        );
        assert_eq!(reply.status, 200, "{wanted}: {}", reply.body);
    }
}

/// A runner event on `thread` through the dev append route (needs `DEV_FIXTURES=1`).
fn runner_event(thread: &str, task: &str, run: &str, event: Value) {
    let envelope = json!({
        "v": 1, "seq": 0, "thread_id": thread, "task_id": task, "run_id": run,
        "ts": "2026-10-28T10:00:00.000Z", "event": event,
    });
    let reply = http(
        "POST",
        &format!("/v1/dev/threads/{thread}/events"),
        Some("dev"),
        &[],
        Some(&envelope),
    );
    assert_eq!(reply.status, 200, "dev append: {}", reply.body);
}

fn prompt(thread: &str, task: &str) -> Reply {
    let key = unique("idk");
    http(
        "POST",
        &format!("/v1/threads/{thread}/prompts"),
        Some("dev"),
        &[("Idempotency-Key", &key)],
        Some(&json!({ "task_id": task, "parts": [{ "type": "text", "text": "go" }] })),
    )
}

#[test]
fn a_thread_whose_last_task_succeeded_is_done_until_the_next_prompt() {
    let dev = "dev";
    let mch = machine(dev);
    let thread = create_thread(dev, &mch, Some("Finishes"), None);
    let query = format!("machine_id={mch}");
    let status = |wanted: &str| listed(dev, &format!("{query}&status={wanted}")).contains(&thread);
    assert!(status("idle"));

    let task = unique("tk");
    assert_eq!(prompt(&thread, &task).status, 202);
    runner_event(
        &thread,
        &task,
        "run_a",
        json!({ "type": "RunStarted", "reason": "prompt" }),
    );
    assert!(status("running") && !status("done"));

    runner_event(
        &thread,
        &task,
        "run_a",
        json!({ "type": "RunFinished", "outcome": { "type": "success", "stop_reason": "end_turn" } }),
    );
    assert!(status("done"), "a finished last task is done");
    assert!(!status("running") && !status("idle"));

    // The next prompt starts the thread again.
    assert_eq!(prompt(&thread, &unique("tk")).status, 202);
    assert!(status("running") && !status("done"));
}

#[test]
fn a_deleted_thread_is_404_on_every_thread_route_like_an_unknown_one() {
    let dev = "dev";
    let admin = issue("admin");
    let mch = machine(dev);
    let thread = create_thread(dev, &mch, Some("To delete"), Some("hello"));
    let base = format!("/v1/threads/{thread}");

    // Alive: the routes answer.
    assert_eq!(http("GET", &base, Some(dev), &[], None).status, 200);
    let ticket = http(
        "POST",
        &format!("{base}/subscribe-ticket"),
        Some(dev),
        &[],
        None,
    );
    assert_eq!(ticket.status, 201, "{}", ticket.body);

    assert_eq!(delete(&admin, &thread).status, 204);

    let key = unique("idk");
    let keyed = [("Idempotency-Key", key.as_str())];
    let upgrade = [("Upgrade", "websocket"), ("Connection", "Upgrade")];
    let task_body = json!({ "task_id": "tk_x", "parts": [{ "type": "text", "text": "hi" }] });
    type Route<'a> = (&'a str, String, &'a [(&'a str, &'a str)], Option<Value>);
    let routes: Vec<Route> = vec![
        ("GET", base.clone(), &[], None),
        ("PATCH", base.clone(), &[], Some(json!({ "title": "x" }))),
        ("DELETE", base.clone(), &keyed, None),
        ("GET", format!("{base}/pending"), &[], None),
        ("DELETE", format!("{base}/pending/tk_x"), &keyed, None),
        ("POST", format!("{base}/prompts"), &keyed, Some(task_body)),
        (
            "POST",
            format!("{base}/interrupts/int_x"),
            &keyed,
            Some(json!({ "outcome": { "kind": "cancelled" } })),
        ),
        (
            "POST",
            format!("{base}/cancel"),
            &keyed,
            Some(json!({ "task_id": "tk_x" })),
        ),
        ("POST", format!("{base}/session"), &keyed, Some(json!({}))),
        ("POST", format!("{base}/subscribe-ticket"), &[], None),
        ("GET", format!("{base}/subscribe?after=0"), &upgrade, None),
    ];
    for (method, path, headers, body) in routes {
        let reply = http(method, &path, Some(dev), headers, body.as_ref());
        assert_eq!(reply.status, 404, "{method} {path}: {}", reply.body);
        assert_eq!(reply.json()["error"], "not_found", "{method} {path}");
    }
    // The same answer as a thread that never existed.
    let unknown = http("GET", "/v1/threads/th_never_existed", Some(dev), &[], None);
    assert_eq!(unknown.status, 404);
}

/// The listed row of `thread` (any status, archived included).
fn row_of(machine: &str, thread: &str) -> Value {
    let reply = http(
        "GET",
        &format!("/v1/threads?machine_id={machine}&archived=true"),
        Some("dev"),
        &[],
        None,
    );
    assert_eq!(reply.status, 200, "list: {}", reply.body);
    reply.json()["threads"]
        .as_array()
        .expect("threads")
        .iter()
        .find(|row| row["thread_id"] == thread)
        .unwrap_or_else(|| panic!("{thread} not listed"))
        .clone()
}

/// M2-010h: rows carry `workspace`, `archived` and, while an approval is open, its options.
#[test]
fn rows_carry_workspace_archived_and_the_open_approvals_with_their_options() {
    let dev = "dev";
    let mch = machine(dev);
    let thread = create_thread(dev, &mch, Some("Needs approval"), None);

    let idle = row_of(&mch, &thread);
    assert_eq!(idle["workspace"], "/tmp/work");
    assert_eq!(idle["archived"], false);
    assert_eq!(idle["pending_interrupts"], json!([]));

    let task = unique("tk");
    assert_eq!(prompt(&thread, &task).status, 202);
    runner_event(
        &thread,
        &task,
        "run_a",
        json!({ "type": "RunStarted", "reason": "prompt" }),
    );
    runner_event(
        &thread,
        &task,
        "run_a",
        json!({
            "type": "Interrupt",
            "interrupt_id": "int_list_1",
            "kind": "approval",
            "title": "Run cargo test?",
            "expires_at": "2099-01-01T00:00:00.000Z",
            "options": [
                { "option_id": "opt_yes", "name": "Yes, once", "kind": "allow_once" },
                { "option_id": "opt_never", "name": "Never", "kind": "reject_always" },
            ],
        }),
    );
    let waiting = row_of(&mch, &thread);
    assert_eq!(waiting["status"], "needs_you");
    assert_eq!(
        waiting["pending_interrupts"],
        json!([{
            "interrupt_id": "int_list_1",
            "title": "Run cargo test?",
            "kind": "approval",
            "options": [
                { "option_id": "opt_yes", "name": "Yes, once", "kind": "allow_once" },
                { "option_id": "opt_never", "name": "Never", "kind": "reject_always" },
            ],
        }])
    );

    let answer = http(
        "POST",
        &format!("/v1/threads/{thread}/interrupts/int_list_1"),
        Some(dev),
        &[("Idempotency-Key", &unique("idk"))],
        Some(&json!({ "outcome": { "type": "selected", "option_id": "opt_yes" } })),
    );
    assert_eq!(answer.status, 200, "answer: {}", answer.body);
    let resolved = row_of(&mch, &thread);
    assert_eq!(resolved["status"], "running");
    assert_eq!(resolved["pending_interrupts"], json!([]));

    assert_eq!(
        rename(dev, &thread, &json!({ "archived": true })).status,
        200
    );
    assert_eq!(row_of(&mch, &thread)["archived"], true);
    assert!(!listed(dev, &format!("machine_id={mch}")).contains(&thread));
}

/// M2-010i: a form interrupt makes the row `needs_you` and is listed with `kind: form` and no
/// options; answering it empties the list.
#[test]
fn a_form_interrupt_is_listed_with_kind_form_and_no_options_until_answered() {
    let dev = "dev";
    let mch = machine(dev);
    let thread = create_thread(dev, &mch, Some("Needs a form"), None);
    let task = unique("tk");
    assert_eq!(prompt(&thread, &task).status, 202);
    runner_event(
        &thread,
        &task,
        "run_f",
        json!({ "type": "RunStarted", "reason": "prompt" }),
    );
    runner_event(
        &thread,
        &task,
        "run_f",
        json!({
            "type": "Interrupt",
            "interrupt_id": "int_list_form",
            "kind": "form",
            "title": "Which branch?",
            "expires_at": "2099-01-01T00:00:00.000Z",
            "response_schema": { "type": "object" },
        }),
    );
    let waiting = row_of(&mch, &thread);
    assert_eq!(waiting["status"], "needs_you");
    assert_eq!(
        waiting["pending_interrupts"],
        json!([{
            "interrupt_id": "int_list_form",
            "title": "Which branch?",
            "kind": "form",
            "options": [],
        }])
    );

    let answer = http(
        "POST",
        &format!("/v1/threads/{thread}/interrupts/int_list_form"),
        Some(dev),
        &[("Idempotency-Key", &unique("idk"))],
        Some(&json!({ "outcome": { "type": "submitted", "data": { "branch": "main" } } })),
    );
    assert_eq!(answer.status, 200, "answer: {}", answer.body);
    let resolved = row_of(&mch, &thread);
    assert_eq!(resolved["status"], "running");
    assert_eq!(resolved["pending_interrupts"], json!([]));
}
