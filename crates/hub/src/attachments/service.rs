//! The attachment logic (M2-009) over [`Db`] (audit rows) and [`Blobs`] (R2). Runtime-agnostic:
//! `routes.rs` reads the request, the env and the clock, and calls these.
//!
//! - [`Attachments::create`]: `POST /v1/attachments` (member, Idempotency-Key, audited). Answers
//!   the two signed URLs and the FilePart. The same key with the same body replays the stored
//!   answer for 24 hours; the same key with another body is `422 idempotency_mismatch`.
//! - [`Attachments::check_upload`] and [`Attachments::store_upload`]: the signed `PUT`. The
//!   signature names the declared size and MIME type; the bytes must match the size and be at
//!   most 20 MB.
//! - [`Attachments::content`]: the signed `GET`, answered with the bytes and the stored type.
//!
//! Object keys are org-scoped (`orgs/{org}/attachments/...`). Every signed URL names its org and
//! attachment, and the signature covers both, so a URL reaches only its own object.

use protocol::turn_event::{format_timestamp, is_valid_id};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::sign::{Claims, Op, Signer};
use super::store::{BlobError, Blobs, Stored};
use super::{MAX_BYTES, URL_TTL_MS};
use crate::audit::{self, Event};
use crate::auth::Principal;
use crate::auth::token::{hex, new_id};
use crate::db::{Db, Role};
use crate::threads::{ApiError, IDEMPOTENCY_TTL_MS, request_hash, valid_idempotency_key};

/// The path the create route is mounted at. It is part of the idempotency request hash.
pub const CREATE_PATH: &str = "/v1/attachments";

/// Longest attachment name, in characters.
const MAX_NAME_CHARS: usize = 255;

/// What a client declared in `POST /v1/attachments`, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    pub name: String,
    pub mime_type: String,
    pub size: u64,
}

/// What a valid signed upload URL grants: where the bytes go and what they must be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadGrant {
    pub key: String,
    pub size: u64,
    pub mime: String,
}

/// The `POST /v1/attachments` request as the route hands it over.
#[derive(Debug, Clone, Copy)]
pub struct CreateRequest<'a> {
    /// `https://host` of the hub, the origin of the signed URLs.
    pub origin: &'a str,
    /// The `Idempotency-Key` header, if sent.
    pub idempotency_key: Option<&'a str>,
    pub body: &'a Value,
    pub request_id: &'a str,
}

/// The R2 key of one attachment's bytes, org-scoped.
pub fn object_key(org_id: &str, attachment_id: &str) -> String {
    format!("orgs/{org_id}/attachments/blob/{attachment_id}")
}

/// The R2 key of the idempotency record of `(user, key)` within an org. The key is hashed, so
/// any client-chosen text is safe in an object name.
fn idempotency_object_key(org_id: &str, user_id: &str, idempotency_key: &str) -> String {
    let digest = Sha256::digest(format!("{user_id}\n{idempotency_key}").as_bytes());
    format!("orgs/{org_id}/attachments/idempotency/{}", hex(&digest))
}

/// Checks the body of `POST /v1/attachments`: a name, a `type/subtype` MIME type and a size of
/// 1 byte to 20 MB. A size above 20 MB is `413`.
pub fn parse_declared(body: &Value) -> Result<Declared, ApiError> {
    let name = match body.get("name") {
        Some(Value::String(name)) => name.trim().to_owned(),
        _ => return Err(ApiError::invalid_request("name is required")),
    };
    if name.is_empty()
        || name.chars().count() > MAX_NAME_CHARS
        || name.chars().any(char::is_control)
    {
        return Err(ApiError::invalid_request(format!(
            "name must be 1-{MAX_NAME_CHARS} characters without control characters"
        )));
    }
    let mime_type = match body.get("mime_type") {
        Some(Value::String(mime)) => mime.trim().to_ascii_lowercase(),
        _ => return Err(ApiError::invalid_request("mime_type is required")),
    };
    if !valid_mime(&mime_type) {
        return Err(ApiError::invalid_request(
            "mime_type must be type/subtype made of letters, digits, '.', '+' and '-'",
        ));
    }
    let size = body
        .get("size")
        .and_then(Value::as_u64)
        .ok_or_else(|| ApiError::invalid_request("size must be a whole number of bytes"))?;
    if size == 0 {
        return Err(ApiError::invalid_request("size must be at least 1 byte"));
    }
    if size > MAX_BYTES {
        return Err(ApiError::new(
            413,
            "invalid_request",
            format!("an attachment is at most {MAX_BYTES} bytes (20 MB)"),
        ));
    }
    Ok(Declared {
        name,
        mime_type,
        size,
    })
}

/// `type/subtype`, each part non-empty with letters, digits, `.`, `+` and `-` only. Such a value
/// needs no percent-encoding in a URL query.
fn valid_mime(mime: &str) -> bool {
    let Some((kind, subtype)) = mime.split_once('/') else {
        return false;
    };
    let token = |part: &str| {
        !part.is_empty()
            && part.len() <= 127
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
    };
    token(kind) && token(subtype)
}

/// The value of `name` in a raw query string (`a=1&b=2`). No percent-decoding: the values this
/// module signs are plain tokens and it writes them that way.
fn query_value<'a>(query: Option<&'a str>, name: &str) -> Option<&'a str> {
    query?.split('&').find_map(|pair| {
        pair.split_once('=')
            .filter(|(key, _)| *key == name)
            .map(|(_, value)| value)
    })
}

/// The answer for a refused signed URL: the same for every reason, so a probe learns nothing.
fn refused() -> ApiError {
    ApiError::new(403, "forbidden", "the URL is not valid or has expired")
}

/// `Some(response)` when the stored record answers this request (same body, not expired);
/// `None` when the key is unused or its record has expired; a mismatch is `422`.
fn replay(stored: &Stored, hash: &str, now_ms: u64) -> Result<Option<Value>, ApiError> {
    let record: Value = serde_json::from_slice(&stored.bytes)
        .map_err(|error| ApiError::internal(format!("attachments: idempotency record: {error}")))?;
    let created_ms = record
        .get("created_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if now_ms.saturating_sub(created_ms) >= IDEMPOTENCY_TTL_MS {
        return Ok(None);
    }
    if record.get("request_hash").and_then(Value::as_str) != Some(hash) {
        return Err(ApiError::new(
            422,
            "idempotency_mismatch",
            "this Idempotency-Key was already used with another body",
        ));
    }
    Ok(record.get("response").cloned())
}

fn blob_failure(error: BlobError) -> ApiError {
    ApiError::internal(format!("attachments: storage: {}", error.0))
}

/// The upload URL: `PUT` bytes of exactly `declared.size`, of `declared.mime_type`.
fn upload_url(
    signer: &Signer,
    origin: &str,
    org_id: &str,
    attachment_id: &str,
    expires_ms: u64,
    declared: &Declared,
) -> String {
    let claims = Claims {
        op: Op::Put,
        org_id,
        attachment_id,
        expires_ms,
        size: declared.size,
        mime: &declared.mime_type,
    };
    let signature = signer.sign(&claims);
    format!(
        "{origin}/v1/attachments/{org_id}/{attachment_id}/upload?size={}&mime={}&exp={expires_ms}&sig={signature}",
        declared.size, declared.mime_type
    )
}

/// The content URL: `GET` returns the bytes with the type they were uploaded with.
fn content_url(
    signer: &Signer,
    origin: &str,
    org_id: &str,
    attachment_id: &str,
    expires_ms: u64,
) -> String {
    let claims = Claims {
        op: Op::Get,
        org_id,
        attachment_id,
        expires_ms,
        size: 0,
        mime: "",
    };
    let signature = signer.sign(&claims);
    format!("{origin}/v1/attachments/{org_id}/{attachment_id}?exp={expires_ms}&sig={signature}")
}

/// The attachment service over one bucket and one signing key.
pub struct Attachments<'a, B> {
    blobs: &'a B,
    signer: &'a Signer,
}

impl<'a, B: Blobs> Attachments<'a, B> {
    pub fn new(blobs: &'a B, signer: &'a Signer) -> Self {
        Self { blobs, signer }
    }

    /// `POST /v1/attachments`: member role; `Idempotency-Key` required; audited in `db`. Answers
    /// the `upload_url`, the `url`, `expires_at` (both URLs expire then) and the FilePart `part`.
    pub async fn create<D: Db>(
        &self,
        db: &D,
        principal: &Principal,
        request: &CreateRequest<'_>,
        now_ms: u64,
    ) -> Result<Value, ApiError> {
        principal.require(Role::Member)?;
        let key = request.idempotency_key.ok_or_else(|| {
            ApiError::new(
                400,
                "idempotency_key_required",
                "the Idempotency-Key header is required",
            )
        })?;
        if !valid_idempotency_key(key) {
            return Err(ApiError::invalid_request(
                "Idempotency-Key must be 1-128 characters of [A-Za-z0-9_:.-], not starting with local:",
            ));
        }
        let declared = parse_declared(request.body)?;
        let hash = request_hash("POST", CREATE_PATH, request.body);
        let slot = idempotency_object_key(&principal.org_id, &principal.user_id, key);
        let replayed = match self.blobs.get(&slot).await.map_err(blob_failure)? {
            Some(stored) => replay(&stored, &hash, now_ms)?,
            None => None,
        };
        if let Some(response) = replayed {
            return Ok(response);
        }

        let org_id = principal.org_id.as_str();
        let attachment_id = new_id("att")?;
        let expires_ms = now_ms.saturating_add(URL_TTL_MS);
        let upload = upload_url(
            self.signer,
            request.origin,
            org_id,
            &attachment_id,
            expires_ms,
            &declared,
        );
        let url = content_url(
            self.signer,
            request.origin,
            org_id,
            &attachment_id,
            expires_ms,
        );
        let response = json!({
            "upload_url": upload,
            "url": url,
            "expires_at": format_timestamp(expires_ms),
            "part": {
                "type": "file",
                "url": url,
                "name": declared.name,
                "mime_type": declared.mime_type,
                "size": declared.size,
            },
        });

        audit::record(
            db,
            &Event::by(
                principal,
                "attachment.create",
                "attachment",
                &attachment_id,
                request.request_id,
                json!({ "mime_type": declared.mime_type, "size": declared.size }),
            ),
        )
        .await?;
        let record = json!({
            "request_hash": hash,
            "created_ms": now_ms,
            "response": response,
        });
        self.blobs
            .put(&slot, record.to_string().into_bytes(), "application/json")
            .await
            .map_err(blob_failure)?;
        Ok(response)
    }

    /// Checks an upload URL's path and query (`size`, `mime`, `exp`, `sig`). Any failure, or an
    /// expired URL, is `403`. Answers where the bytes go and what they must be.
    pub fn check_upload(
        &self,
        now_ms: u64,
        org_id: &str,
        attachment_id: &str,
        query: Option<&str>,
    ) -> Result<UploadGrant, ApiError> {
        if !is_valid_id(org_id) || !is_valid_id(attachment_id) {
            return Err(refused());
        }
        let number = |name: &str| query_value(query, name).and_then(|v| v.parse::<u64>().ok());
        let expires_ms = number("exp").ok_or_else(refused)?;
        let size = number("size").ok_or_else(refused)?;
        let mime = query_value(query, "mime").ok_or_else(refused)?;
        let signature = query_value(query, "sig").ok_or_else(refused)?;
        let claims = Claims {
            op: Op::Put,
            org_id,
            attachment_id,
            expires_ms,
            size,
            mime,
        };
        self.signer
            .verify(&claims, signature, now_ms)
            .map_err(|_| refused())?;
        Ok(UploadGrant {
            key: object_key(org_id, attachment_id),
            size,
            mime: mime.to_owned(),
        })
    }

    /// Stores the bytes of a checked upload. Above 20 MB is `413`; a size other than the declared
    /// one is `400`. Nothing is stored unless both hold.
    pub async fn store_upload(&self, grant: &UploadGrant, bytes: Vec<u8>) -> Result<(), ApiError> {
        let len = bytes.len() as u64;
        if len > MAX_BYTES {
            return Err(ApiError::new(
                413,
                "invalid_request",
                "an attachment is at most 20 MB",
            ));
        }
        if len != grant.size {
            return Err(ApiError::invalid_request(format!(
                "the body is {len} bytes; the declared size is {}",
                grant.size
            )));
        }
        self.blobs
            .put(&grant.key, bytes, &grant.mime)
            .await
            .map_err(blob_failure)
    }

    /// Serves a content URL: `403` for a bad or expired signature, `404` when nothing has been
    /// uploaded yet.
    pub async fn content(
        &self,
        now_ms: u64,
        org_id: &str,
        attachment_id: &str,
        query: Option<&str>,
    ) -> Result<Stored, ApiError> {
        if !is_valid_id(org_id) || !is_valid_id(attachment_id) {
            return Err(refused());
        }
        let expires_ms = query_value(query, "exp")
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(refused)?;
        let signature = query_value(query, "sig").ok_or_else(refused)?;
        let claims = Claims {
            op: Op::Get,
            org_id,
            attachment_id,
            expires_ms,
            size: 0,
            mime: "",
        };
        self.signer
            .verify(&claims, signature, now_ms)
            .map_err(|_| refused())?;
        self.blobs
            .get(&object_key(org_id, attachment_id))
            .await
            .map_err(blob_failure)?
            .ok_or_else(|| ApiError::not_found("no content has been uploaded for this attachment"))
    }
}
