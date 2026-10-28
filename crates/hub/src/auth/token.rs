//! Session tokens and ids: opaque random strings, hashed before they touch the database.
//!
//! A session token is `smt_` plus 64 hex characters (256 random bits). Only its SHA-256 (hex)
//! is stored; the token is shown once when issued and never logged.

use sha2::{Digest, Sha256};

use crate::threads::ApiError;

/// Every session token starts with this.
pub const TOKEN_PREFIX: &str = "smt_";

/// Bytes of entropy in a session token (256 bits).
const TOKEN_BYTES: usize = 32;

/// `len` random bytes from the platform CSPRNG (`crypto.getRandomValues` on Workers).
fn random_bytes(len: usize) -> Result<Vec<u8>, ApiError> {
    let mut bytes = vec![0u8; len];
    getrandom::getrandom(&mut bytes).map_err(|_| ApiError::internal("no randomness available"))?;
    Ok(bytes)
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// A fresh session token.
pub fn new_token() -> Result<String, ApiError> {
    Ok(format!(
        "{TOKEN_PREFIX}{}",
        hex(&random_bytes(TOKEN_BYTES)?)
    ))
}

/// A fresh id such as `usr_<32 hex>` (128 random bits).
pub fn new_id(prefix: &str) -> Result<String, ApiError> {
    Ok(format!("{prefix}_{}", hex(&random_bytes(16)?)))
}

/// The SHA-256 of a token, as 64 lowercase hex characters: what the database stores.
pub fn hash_token(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

/// Constant-time equality of two byte strings (the length is not secret here: both sides are
/// fixed-size digests when used for secrets).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Constant-time comparison of two secrets of any length: both are hashed first, so neither the
/// content nor the length leaks through timing.
pub fn secrets_match(a: &str, b: &str) -> bool {
    ct_eq(hash_token(a).as_bytes(), hash_token(b).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_prefixed_256_bit_and_distinct() {
        let a = new_token().expect("token");
        let b = new_token().expect("token");
        assert!(a.starts_with("smt_"));
        assert_eq!(a.len(), 4 + 64);
        assert_ne!(a, b);
    }

    #[test]
    fn hash_is_sha256_hex() {
        assert_eq!(
            hash_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(secrets_match("x", "x"));
        assert!(!secrets_match("x", "xy"));
    }

    #[test]
    fn ids_have_the_prefix() {
        assert!(new_id("usr").expect("id").starts_with("usr_"));
    }
}
