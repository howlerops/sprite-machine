//! Native tests of the attachment logic (M2-009): signing and verification, the size limits, the
//! key scoping, the idempotent create and the role gate. The blobs are [`MemoryBlobs`] and the
//! audit writes go to a counting [`Db`], so these run without a Worker (the live tests are in
//! `crates/hub/tests/attachments.rs`).

use std::cell::RefCell;
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use protocol::turn_event::format_timestamp;
use serde_json::{Value, json};

use super::MAX_BYTES;
use super::URL_TTL_MS;
use super::service::{Attachments, CreateRequest, UploadGrant, object_key, parse_declared};
use super::sign::{Claims, KeyTooShort, Op, Signer, VerifyError};
use super::store::MemoryBlobs;
use crate::auth::Principal;
use crate::db::{Db, DbError, Param, Role, Row};
use crate::threads::{ApiError, IDEMPOTENCY_TTL_MS};

const NOW: u64 = 1_800_000_000_000;
const ORIGIN: &str = "https://hub.test";
const KEY: &[u8] = b"unit-test-signing-key-0123456789abcdef";

/// Runs a future that never yields: the fakes complete at once.
fn block_on<T>(future: impl Future<Output = T>) -> T {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("future pending: the fakes never yield"),
    }
}

/// Counts the audit writes; there is no other table in these tests.
#[derive(Default)]
struct AuditSink {
    writes: RefCell<usize>,
}

impl Db for AuditSink {
    async fn execute(&self, _sql: &str, _params: &[Param]) -> Result<u64, DbError> {
        *self.writes.borrow_mut() += 1;
        Ok(1)
    }

    async fn query(&self, _sql: &str, _params: &[Param]) -> Result<Vec<Row>, DbError> {
        Ok(Vec::new())
    }
}

fn signer() -> Signer {
    Signer::new(KEY).expect("the test key is long enough")
}

fn principal_as(user: &str, role: Role, org: &str) -> Principal {
    Principal {
        user_id: user.to_owned(),
        device_id: "dev_1".to_owned(),
        org_id: org.to_owned(),
        role,
        display_name: String::new(),
        session_id: None,
    }
}

fn member(org: &str) -> Principal {
    principal_as("usr_1", Role::Member, org)
}

fn body(size: u64) -> Value {
    json!({ "name": "notes.pdf", "mime_type": "application/pdf", "size": size })
}

fn create_at(
    svc: &Attachments<'_, MemoryBlobs>,
    db: &AuditSink,
    principal: &Principal,
    key: Option<&str>,
    body: &Value,
    now: u64,
) -> Result<Value, ApiError> {
    let request = CreateRequest {
        origin: ORIGIN,
        idempotency_key: key,
        body,
        request_id: "req_test",
    };
    block_on(svc.create(db, principal, &request, now))
}

/// A create by the member `usr_1` of `org`, with key `idk_1`, at [`NOW`].
fn create(
    svc: &Attachments<'_, MemoryBlobs>,
    db: &AuditSink,
    org: &str,
    size: u64,
) -> Result<Value, ApiError> {
    create_at(svc, db, &member(org), Some("idk_1"), &body(size), NOW)
}

/// Splits `https://hub.test/v1/...?query` into the path and the query.
fn split(url: &str) -> (&str, Option<&str>) {
    let rest = url
        .strip_prefix(ORIGIN)
        .expect("the URL starts with the origin");
    match rest.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (rest, None),
    }
}

/// The org and attachment ids of `/v1/attachments/{org}/{id}[/upload]`.
fn ids(path: &str) -> (String, String) {
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    (parts[2].to_owned(), parts[3].to_owned())
}

fn string<'a>(answer: &'a Value, key: &str) -> &'a str {
    answer[key].as_str().expect("a string field")
}

/// The `put` grant of an upload URL, checked at `now`.
fn grant_of(svc: &Attachments<'_, MemoryBlobs>, answer: &Value, now: u64) -> UploadGrant {
    let (path, query) = split(string(answer, "upload_url"));
    let (org, id) = ids(path);
    svc.check_upload(now, &org, &id, query)
        .expect("a fresh upload URL is accepted")
}

// ---- signing ---------------------------------------------------------------------------

fn put_claims(org_id: &'static str, attachment_id: &'static str) -> Claims<'static> {
    Claims {
        op: Op::Put,
        org_id,
        attachment_id,
        expires_ms: NOW + 1000,
        size: 10,
        mime: "text/plain",
    }
}

#[test]
fn a_fresh_signature_verifies_and_is_64_hex_characters() {
    let s = signer();
    let claims = put_claims("org_a", "att_1");
    let signature = s.sign(&claims);
    assert_eq!(signature.len(), 64);
    assert!(signature.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(s.verify(&claims, &signature, NOW), Ok(()));
}

#[test]
fn a_tampered_signature_is_refused() {
    let s = signer();
    let claims = put_claims("org_a", "att_1");
    let signature = s.sign(&claims);
    let mut bytes = signature.into_bytes();
    bytes[0] = if bytes[0] == b'0' { b'1' } else { b'0' };
    let tampered = String::from_utf8(bytes).expect("ASCII");
    assert_eq!(
        s.verify(&claims, &tampered, NOW),
        Err(VerifyError::BadSignature)
    );
}

#[test]
fn any_changed_claim_breaks_the_signature() {
    let s = signer();
    let base = put_claims("org_a", "att_1");
    let signature = s.sign(&base);
    let variants = [
        ("another org", put_claims("org_b", "att_1")),
        ("another attachment", put_claims("org_a", "att_2")),
        (
            "the other operation",
            Claims {
                op: Op::Get,
                ..base
            },
        ),
        ("a larger size", Claims { size: 11, ..base }),
        (
            "another type",
            Claims {
                mime: "image/png",
                ..base
            },
        ),
        (
            "a later expiry",
            Claims {
                expires_ms: NOW + 2000,
                ..base
            },
        ),
    ];
    for (what, claims) in variants {
        assert_eq!(
            s.verify(&claims, &signature, NOW),
            Err(VerifyError::BadSignature),
            "{what} must not verify with the original signature"
        );
    }
}

#[test]
fn an_expired_signature_is_refused_from_the_expiry_instant() {
    let s = signer();
    let claims = put_claims("org_a", "att_1");
    let signature = s.sign(&claims);
    assert_eq!(s.verify(&claims, &signature, NOW + 999), Ok(()));
    assert_eq!(
        s.verify(&claims, &signature, NOW + 1000),
        Err(VerifyError::Expired)
    );
}

#[test]
fn a_signature_from_another_key_is_refused() {
    let other = Signer::new(b"another-key-0123456789abcdef0123456").expect("long enough");
    let claims = put_claims("org_a", "att_1");
    let foreign = other.sign(&claims);
    assert_eq!(
        signer().verify(&claims, &foreign, NOW),
        Err(VerifyError::BadSignature)
    );
}

#[test]
fn malformed_signatures_are_refused_as_malformed() {
    let s = signer();
    let claims = put_claims("org_a", "att_1");
    assert_eq!(s.verify(&claims, "", NOW), Err(VerifyError::Malformed));
    assert_eq!(
        s.verify(&claims, &"a".repeat(63), NOW),
        Err(VerifyError::Malformed)
    );
    assert_eq!(
        s.verify(&claims, &"g".repeat(64), NOW),
        Err(VerifyError::Malformed)
    );
}

#[test]
fn a_key_shorter_than_256_bits_is_refused() {
    assert!(matches!(Signer::new(&[7u8; 31]), Err(KeyTooShort)));
    assert!(Signer::new(&[7u8; 32]).is_ok());
}

// ---- declared size and type -------------------------------------------------------------

#[test]
fn the_declared_size_is_one_byte_to_twenty_megabytes() {
    assert_eq!(parse_declared(&body(1)).expect("ok").size, 1);
    assert_eq!(
        parse_declared(&body(MAX_BYTES)).expect("ok").size,
        MAX_BYTES
    );

    let zero = parse_declared(&body(0)).expect_err("zero is refused");
    assert_eq!((zero.status, zero.code), (400, "invalid_request"));

    let too_big = parse_declared(&body(MAX_BYTES + 1)).expect_err("over 20 MB is refused");
    assert_eq!((too_big.status, too_big.code), (413, "invalid_request"));

    let text = json!({ "name": "a", "mime_type": "text/plain", "size": "12" });
    assert_eq!(
        parse_declared(&text).expect_err("a string size").status,
        400
    );
    let missing = json!({ "name": "a", "mime_type": "text/plain" });
    assert_eq!(parse_declared(&missing).expect_err("no size").status, 400);
}

#[test]
fn the_declared_type_must_be_type_slash_subtype() {
    for bad in [
        "pdf",
        "a/b/c",
        "text/",
        "/plain",
        "text/plain; charset=utf-8",
        "te xt/plain",
    ] {
        let body = json!({ "name": "a", "mime_type": bad, "size": 1 });
        assert_eq!(
            parse_declared(&body).expect_err(bad).status,
            400,
            "{bad} must be refused"
        );
    }
    let ok = json!({ "name": "a", "mime_type": " Image/SVG+XML ", "size": 1 });
    assert_eq!(parse_declared(&ok).expect("ok").mime_type, "image/svg+xml");
}

#[test]
fn an_oversized_declaration_is_refused_before_anything_is_written() {
    let db = AuditSink::default();
    let blobs = MemoryBlobs::new();
    let s = signer();
    let svc = Attachments::new(&blobs, &s);
    let refused = create(&svc, &db, "org_a", MAX_BYTES + 1).expect_err("over 20 MB");
    assert_eq!(refused.status, 413);
    assert_eq!(*db.writes.borrow(), 0);
    assert!(blobs.keys().is_empty());
}

#[test]
fn a_body_of_another_size_than_declared_is_refused_and_not_stored() {
    let db = AuditSink::default();
    let blobs = MemoryBlobs::new();
    let s = signer();
    let svc = Attachments::new(&blobs, &s);
    let answer = create(&svc, &db, "org_a", 1024).expect("created");
    let grant = grant_of(&svc, &answer, NOW);
    let short = block_on(svc.store_upload(&grant, vec![1; 1023])).expect_err("short body");
    assert_eq!(short.status, 400);
    let long = block_on(svc.store_upload(&grant, vec![1; 1025])).expect_err("long body");
    assert_eq!(long.status, 400);
    assert!(blobs.keys().iter().all(|key| !key.contains("/blob/")));
}

#[test]
fn a_body_over_twenty_megabytes_is_refused_with_413_and_not_stored() {
    let db = AuditSink::default();
    let blobs = MemoryBlobs::new();
    let s = signer();
    let svc = Attachments::new(&blobs, &s);
    let answer = create(&svc, &db, "org_a", MAX_BYTES).expect("created at the limit");
    let grant = grant_of(&svc, &answer, NOW);
    let refused =
        block_on(svc.store_upload(&grant, vec![0; MAX_BYTES as usize + 1])).expect_err("413");
    assert_eq!(refused.status, 413);
    assert!(blobs.keys().iter().all(|key| !key.contains("/blob/")));

    // The limit itself is accepted, and lands under the attachment's own key.
    block_on(svc.store_upload(&grant, vec![0; MAX_BYTES as usize])).expect("exactly 20 MB");
    assert!(blobs.keys().contains(&grant.key));
}

// ---- create, upload, content ------------------------------------------------------------

#[test]
fn create_answers_two_signed_urls_and_the_file_part() {
    let db = AuditSink::default();
    let blobs = MemoryBlobs::new();
    let s = signer();
    let svc = Attachments::new(&blobs, &s);
    let answer = create(&svc, &db, "org_a", 1024).expect("created");

    let upload = string(&answer, "upload_url");
    let url = string(&answer, "url");
    assert!(upload.starts_with("https://hub.test/v1/attachments/org_a/att_"));
    assert!(upload.contains("/upload?size=1024&mime=application/pdf&exp="));
    assert!(url.starts_with("https://hub.test/v1/attachments/org_a/att_"));
    assert!(!url.contains("/upload"));
    assert_eq!(
        answer["expires_at"].as_str(),
        Some(format_timestamp(NOW + URL_TTL_MS).as_str())
    );
    assert_eq!(
        answer["part"],
        json!({
            "type": "file",
            "url": url,
            "name": "notes.pdf",
            "mime_type": "application/pdf",
            "size": 1024,
        })
    );
    assert_eq!(
        *db.writes.borrow(),
        1,
        "one audit row per created attachment"
    );
}

#[test]
fn upload_then_fetch_returns_the_bytes_with_the_declared_type() {
    let db = AuditSink::default();
    let blobs = MemoryBlobs::new();
    let s = signer();
    let svc = Attachments::new(&blobs, &s);
    let answer = create(&svc, &db, "org_a", 1024).expect("created");

    let bytes: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
    let grant = grant_of(&svc, &answer, NOW);
    block_on(svc.store_upload(&grant, bytes.clone())).expect("stored");

    let (path, query) = split(string(&answer, "url"));
    let (org, id) = ids(path);
    let stored = block_on(svc.content(NOW, &org, &id, query)).expect("fetched");
    assert_eq!(stored.bytes, bytes);
    assert_eq!(stored.content_type, "application/pdf");
    assert!(blobs.keys().contains(&object_key("org_a", &id)));
}

#[test]
fn content_before_any_upload_is_not_found() {
    let db = AuditSink::default();
    let blobs = MemoryBlobs::new();
    let s = signer();
    let svc = Attachments::new(&blobs, &s);
    let answer = create(&svc, &db, "org_a", 8).expect("created");
    let (path, query) = split(string(&answer, "url"));
    let (org, id) = ids(path);
    let missing = block_on(svc.content(NOW, &org, &id, query)).expect_err("nothing uploaded");
    assert_eq!(missing.status, 404);
}

#[test]
fn tampered_or_expired_urls_are_forbidden() {
    let db = AuditSink::default();
    let blobs = MemoryBlobs::new();
    let s = signer();
    let svc = Attachments::new(&blobs, &s);
    let answer = create(&svc, &db, "org_a", 1024).expect("created");
    let upload = string(&answer, "upload_url");
    let (path, query) = split(upload);
    let (org, id) = ids(path);
    let query = query.expect("query");

    let sig = query.rsplit("sig=").next().expect("sig");
    let exp = query
        .split('&')
        .find_map(|p| p.strip_prefix("exp="))
        .expect("exp");
    let forged = [
        query.replace(sig, &"0".repeat(64)),
        query.replace("size=1024", "size=1025"),
        query.replace(&format!("exp={exp}"), &format!("exp={}", NOW + 9_999_999)),
        query.replace("mime=application/pdf", "mime=text/html"),
        query.replace(&format!("&sig={sig}"), ""),
    ];
    for tampered in &forged {
        let refused = svc
            .check_upload(NOW, &org, &id, Some(tampered))
            .expect_err("a tampered upload URL");
        assert_eq!(refused.status, 403);
    }

    let expired = svc
        .check_upload(NOW + URL_TTL_MS, &org, &id, Some(query))
        .expect_err("an expired upload URL");
    assert_eq!(expired.status, 403);

    // A PUT signature is not a GET signature, even with the same expiry.
    let get_with_put_sig = format!("exp={exp}&sig={sig}");
    let refused = block_on(svc.content(NOW, &org, &id, Some(&get_with_put_sig)))
        .expect_err("a put signature does not read");
    assert_eq!(refused.status, 403);

    let content_query = split(string(&answer, "url")).1.expect("query");
    let expired_get = block_on(svc.content(NOW + URL_TTL_MS, &org, &id, Some(content_query)))
        .expect_err("an expired content URL");
    assert_eq!(expired_get.status, 403);
}

#[test]
fn a_url_from_one_org_does_not_reach_another_orgs_object() {
    let db = AuditSink::default();
    let blobs = MemoryBlobs::new();
    let s = signer();
    let svc = Attachments::new(&blobs, &s);
    let answer = create(&svc, &db, "org_a", 4).expect("created");
    let grant = grant_of(&svc, &answer, NOW);
    block_on(svc.store_upload(&grant, vec![9; 4])).expect("stored");

    let (path, query) = split(string(&answer, "url"));
    let (_, id) = ids(path);
    let moved = block_on(svc.content(NOW, "org_b", &id, query)).expect_err("another org");
    assert_eq!(moved.status, 403);

    let upload_query = split(string(&answer, "upload_url")).1;
    let moved_put = svc
        .check_upload(NOW, "org_b", &id, upload_query)
        .expect_err("another org's upload");
    assert_eq!(moved_put.status, 403);
    assert!(blobs.keys().contains(&object_key("org_a", &id)));
    assert!(!blobs.keys().contains(&object_key("org_b", &id)));
}

#[test]
fn a_url_does_not_reach_another_attachment() {
    let db = AuditSink::default();
    let blobs = MemoryBlobs::new();
    let s = signer();
    let svc = Attachments::new(&blobs, &s);
    let first = create_at(&svc, &db, &member("org_a"), Some("idk_a"), &body(4), NOW).expect("a");
    let second = create_at(&svc, &db, &member("org_a"), Some("idk_b"), &body(4), NOW).expect("b");
    let grant = grant_of(&svc, &second, NOW);
    block_on(svc.store_upload(&grant, vec![5; 4])).expect("second stored");

    let (first_path, first_query) = split(string(&first, "url"));
    let (org, first_id) = ids(first_path);
    let (second_path, _) = split(string(&second, "url"));
    let (_, second_id) = ids(second_path);
    assert_ne!(first_id, second_id);

    // The first URL's signature on the second attachment's path is refused.
    let moved =
        block_on(svc.content(NOW, &org, &second_id, first_query)).expect_err("another attachment");
    assert_eq!(moved.status, 403);
}

// ---- roles and idempotency ----------------------------------------------------------------

#[test]
fn a_viewer_cannot_create_and_nothing_is_written() {
    let db = AuditSink::default();
    let blobs = MemoryBlobs::new();
    let s = signer();
    let svc = Attachments::new(&blobs, &s);
    let viewer = principal_as("usr_v", Role::Viewer, "org_a");
    let refused = create_at(&svc, &db, &viewer, Some("idk_1"), &body(4), NOW).expect_err("403");
    assert_eq!((refused.status, refused.code), (403, "forbidden"));
    assert_eq!(*db.writes.borrow(), 0);
    assert!(blobs.keys().is_empty());
}

#[test]
fn create_needs_a_valid_idempotency_key() {
    let db = AuditSink::default();
    let blobs = MemoryBlobs::new();
    let s = signer();
    let svc = Attachments::new(&blobs, &s);
    let missing = create_at(&svc, &db, &member("org_a"), None, &body(4), NOW)
        .expect_err("the key is required");
    assert_eq!(
        (missing.status, missing.code),
        (400, "idempotency_key_required")
    );
    let bad = create_at(
        &svc,
        &db,
        &member("org_a"),
        Some("has space"),
        &body(4),
        NOW,
    )
    .expect_err("a key of token characters only");
    assert_eq!((bad.status, bad.code), (400, "invalid_request"));
}

#[test]
fn the_same_key_and_body_replays_the_same_answer_without_a_new_row() {
    let db = AuditSink::default();
    let blobs = MemoryBlobs::new();
    let s = signer();
    let svc = Attachments::new(&blobs, &s);
    let first = create_at(&svc, &db, &member("org_a"), Some("idk_1"), &body(4), NOW).expect("1");
    let writes = *db.writes.borrow();
    let second = create_at(
        &svc,
        &db,
        &member("org_a"),
        Some("idk_1"),
        &body(4),
        NOW + 5,
    )
    .expect("2");
    assert_eq!(first, second);
    assert_eq!(*db.writes.borrow(), writes, "a replay writes no audit row");
}

#[test]
fn the_same_key_with_another_body_is_422() {
    let db = AuditSink::default();
    let blobs = MemoryBlobs::new();
    let s = signer();
    let svc = Attachments::new(&blobs, &s);
    create_at(&svc, &db, &member("org_a"), Some("idk_1"), &body(4), NOW).expect("1");
    let clash = create_at(&svc, &db, &member("org_a"), Some("idk_1"), &body(5), NOW)
        .expect_err("another body");
    assert_eq!((clash.status, clash.code), (422, "idempotency_mismatch"));
}

#[test]
fn a_key_is_only_a_replay_for_the_user_who_sent_it() {
    let db = AuditSink::default();
    let blobs = MemoryBlobs::new();
    let s = signer();
    let svc = Attachments::new(&blobs, &s);
    let mine = create_at(&svc, &db, &member("org_a"), Some("idk_1"), &body(4), NOW).expect("1");
    let theirs = create_at(
        &svc,
        &db,
        &principal_as("usr_2", Role::Member, "org_a"),
        Some("idk_1"),
        &body(4),
        NOW,
    )
    .expect("another user's key is its own");
    assert_ne!(string(&mine, "upload_url"), string(&theirs, "upload_url"));
}

#[test]
fn a_key_is_forgotten_after_the_idempotency_window() {
    let db = AuditSink::default();
    let blobs = MemoryBlobs::new();
    let s = signer();
    let svc = Attachments::new(&blobs, &s);
    let first = create_at(&svc, &db, &member("org_a"), Some("idk_1"), &body(4), NOW).expect("1");
    let later = create_at(
        &svc,
        &db,
        &member("org_a"),
        Some("idk_1"),
        &body(5),
        NOW + IDEMPOTENCY_TTL_MS,
    )
    .expect("a new answer once the window has passed");
    assert_ne!(string(&first, "upload_url"), string(&later, "upload_url"));
    assert_eq!(*db.writes.borrow(), 2);
}
