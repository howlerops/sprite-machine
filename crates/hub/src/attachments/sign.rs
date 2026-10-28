//! The URL signature (PLAN-v2 §4.2 "R2", signed URLs): HMAC-SHA256 over one canonical line that
//! names the operation, the org, the attachment, the expiry and, for an upload, the declared size
//! and MIME type. A changed field, the other operation, another object or another key fails.
//! The comparison is constant-time (`hmac`'s `verify_slice`).

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::auth::token::hex;

type HmacSha256 = Hmac<Sha256>;

/// The shortest signing key accepted: 256 bits.
pub const MIN_KEY_BYTES: usize = 32;

/// Length of a hex-encoded HMAC-SHA256.
const SIGNATURE_LEN: usize = 64;

/// First line of the canonical string. Changing it invalidates every outstanding URL.
const VERSION: &str = "sm-attachment-v1";

/// The two operations a signed URL allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `PUT` to `upload_url`.
    Put,
    /// `GET` of `url`.
    Get,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Op::Put => "put",
            Op::Get => "get",
        }
    }
}

/// What a signature covers. A `Get` has no declared size or MIME type (R2 holds them), so it
/// signs `0` and `""`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claims<'a> {
    pub op: Op,
    pub org_id: &'a str,
    pub attachment_id: &'a str,
    /// Unix milliseconds. The URL is refused from this instant on.
    pub expires_ms: u64,
    pub size: u64,
    pub mime: &'a str,
}

impl Claims<'_> {
    fn canonical(&self) -> String {
        format!(
            "{VERSION}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.op.as_str(),
            self.org_id,
            self.attachment_id,
            self.expires_ms,
            self.size,
            self.mime
        )
    }
}

/// Why a signed URL was refused. The route answers 403 for every case; the split is for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    /// The signature is not 64 hex characters.
    Malformed,
    /// The signature does not match the claims under this key.
    BadSignature,
    /// The signature is good, but the expiry has passed.
    Expired,
}

/// The key is shorter than [`MIN_KEY_BYTES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyTooShort;

/// Signs and verifies URLs with one key. It has no `Debug`, so the key cannot be logged by accident.
#[derive(Clone)]
pub struct Signer {
    key: Vec<u8>,
}

impl Signer {
    pub fn new(key: &[u8]) -> Result<Self, KeyTooShort> {
        if key.len() < MIN_KEY_BYTES {
            return Err(KeyTooShort);
        }
        Ok(Self { key: key.to_vec() })
    }

    fn mac(&self, claims: &Claims) -> HmacSha256 {
        // HMAC accepts a key of any length, so this cannot fail.
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.key)
            .unwrap_or_else(|_| unreachable!("HMAC takes a key of any length"));
        mac.update(claims.canonical().as_bytes());
        mac
    }

    /// The lowercase hex signature of `claims`.
    pub fn sign(&self, claims: &Claims) -> String {
        hex(&self.mac(claims).finalize().into_bytes())
    }

    /// Checks the signature (constant-time), then the expiry against `now_ms`.
    pub fn verify(&self, claims: &Claims, signature: &str, now_ms: u64) -> Result<(), VerifyError> {
        let expected = decode_hex(signature).ok_or(VerifyError::Malformed)?;
        self.mac(claims)
            .verify_slice(&expected)
            .map_err(|_| VerifyError::BadSignature)?;
        if now_ms >= claims.expires_ms {
            return Err(VerifyError::Expired);
        }
        Ok(())
    }
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() != SIGNATURE_LEN || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|digits| u8::from_str_radix(digits, 16).ok())
        })
        .collect()
}
