//! Machine credentials (tunnel.md §2, plan §4.8): `smm_` plus 64 hex characters (256 random
//! bits). Only the SHA-256 (hex) is stored, in D1 `machines.credential_hash` and in the
//! Machine DO; the credential itself is shown once, when the machine is created.

use crate::auth::token::{ct_eq, hash_token, new_id, new_token};
use crate::threads::ApiError;

/// Every machine credential starts with this.
pub const CREDENTIAL_PREFIX: &str = "smm_";

/// Every resume token starts with this (tunnel.md §4.3).
pub const RESUME_PREFIX: &str = "rt_";

/// A fresh machine credential (`smm_` + 256 random bits).
pub fn new_credential() -> Result<String, ApiError> {
    let token = new_token()?;
    let random = token
        .strip_prefix(crate::auth::token::TOKEN_PREFIX)
        .unwrap_or(&token);
    Ok(format!("{CREDENTIAL_PREFIX}{random}"))
}

/// A fresh resume token: `rt_` + 256 random bits (opaque, 67 characters, tunnel.md §4.3).
pub fn new_resume_token() -> Result<String, ApiError> {
    let (a, b) = (new_id("x")?, new_id("x")?);
    Ok(format!(
        "{RESUME_PREFIX}{}{}",
        a.trim_start_matches("x_"),
        b.trim_start_matches("x_")
    ))
}

/// A fresh machine id, `mch_` + 128 random bits.
pub fn new_machine_id() -> Result<String, ApiError> {
    new_id("mch")
}

/// The SHA-256 (hex) of a credential or resume token: what is stored.
pub fn hash(secret: &str) -> String {
    hash_token(secret)
}

/// `true` when `presented` hashes to `stored_hash` (constant time).
pub fn matches(stored_hash: &str, presented: &str) -> bool {
    ct_eq(stored_hash.as_bytes(), hash(presented).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_prefixed_256_bit_and_distinct() {
        let a = new_credential().expect("credential");
        let b = new_credential().expect("credential");
        assert!(a.starts_with("smm_"));
        assert_eq!(a.len(), 4 + 64);
        assert!(a[4..].bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn resume_tokens_are_opaque_and_within_the_spec_limit() {
        let token = new_resume_token().expect("token");
        assert!(token.starts_with("rt_"));
        assert!(token.len() <= 128);
        assert_ne!(token, new_resume_token().expect("token"));
    }

    #[test]
    fn only_the_hash_matches() {
        let credential = new_credential().expect("credential");
        let stored = hash(&credential);
        assert_eq!(stored.len(), 64);
        assert!(matches(&stored, &credential));
        assert!(!matches(&stored, &format!("{credential}x")));
        assert!(!matches(&stored, &stored), "the hash is not a credential");
    }
}
