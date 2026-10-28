//! Pairing codes. The device code is a bearer secret (256 random bits, `sdc_` + 64 hex), shown
//! once to the runner that asked for it; only its SHA-256 is stored. The user code is typed by a
//! human: 8 characters from an alphabet without look-alikes (no 0/O, 1/I/L), shown as
//! `XXXX-XXXX`.

use crate::auth::token::{hash_token, hex};
use crate::threads::ApiError;

pub const DEVICE_CODE_PREFIX: &str = "sdc_";

/// 31 characters: no `0 O 1 I L`.
pub const USER_CODE_ALPHABET: &[u8; 31] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
pub const USER_CODE_LEN: usize = 8;

fn random_bytes(len: usize) -> Result<Vec<u8>, ApiError> {
    let mut bytes = vec![0u8; len];
    getrandom::getrandom(&mut bytes).map_err(|_| ApiError::internal("no randomness available"))?;
    Ok(bytes)
}

/// A fresh device code.
pub fn new_device_code() -> Result<String, ApiError> {
    Ok(format!("{DEVICE_CODE_PREFIX}{}", hex(&random_bytes(32)?)))
}

/// What is stored for a device code.
pub fn hash_device_code(code: &str) -> String {
    hash_token(code)
}

pub const LINK_CODE_PREFIX: &str = "sli_";

/// A fresh sign-in link code (256 random bits, `sli_` + 64 hex): a bearer secret that appears
/// only in the URL the runner prints; the hub stores its SHA-256.
pub fn new_link_code() -> Result<String, ApiError> {
    Ok(format!("{LINK_CODE_PREFIX}{}", hex(&random_bytes(32)?)))
}

/// What is stored for a link code.
pub fn hash_link_code(code: &str) -> String {
    hash_token(code)
}

/// Whether `code` has the shape of a link code (a cheap filter before the lookup).
pub fn looks_like_link_code(code: &str) -> bool {
    code.strip_prefix(LINK_CODE_PREFIX)
        .is_some_and(|rest| rest.len() == 64 && rest.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A fresh user code (8 characters, no dash), drawn without modulo bias.
pub fn new_user_code() -> Result<String, ApiError> {
    let n = USER_CODE_ALPHABET.len();
    let limit = 256 - 256 % n; // 248: bytes at or above are redrawn
    let mut out = String::with_capacity(USER_CODE_LEN);
    while out.len() < USER_CODE_LEN {
        for byte in random_bytes(16)? {
            if usize::from(byte) < limit && out.len() < USER_CODE_LEN {
                out.push(char::from(USER_CODE_ALPHABET[usize::from(byte) % n]));
            }
        }
    }
    Ok(out)
}

/// `ABCDEFGH` -> `ABCD-EFGH`.
pub fn display_user_code(code: &str) -> String {
    let (a, b) = code.split_at(code.len().min(4));
    format!("{a}-{b}")
}

/// Normalizes what a person typed (case, dashes, spaces) to the stored form, or `None` when it
/// cannot be a user code.
pub fn normalize_user_code(typed: &str) -> Option<String> {
    let code: String = typed
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let valid =
        code.len() == USER_CODE_LEN && code.bytes().all(|b| USER_CODE_ALPHABET.contains(&b));
    valid.then_some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_codes_use_the_unambiguous_alphabet() {
        for _ in 0..200 {
            let code = new_user_code().expect("code");
            assert_eq!(code.len(), 8);
            assert!(code.bytes().all(|b| USER_CODE_ALPHABET.contains(&b)));
            assert!(!code.contains(['0', 'O', '1', 'I', 'L']));
        }
    }

    #[test]
    fn typed_codes_normalize() {
        assert_eq!(
            normalize_user_code("abcd-efgh").as_deref(),
            Some("ABCDEFGH")
        );
        assert_eq!(
            normalize_user_code(" ab cd efgh ").as_deref(),
            Some("ABCDEFGH")
        );
        assert_eq!(normalize_user_code("ABCD-EFG"), None);
        assert_eq!(normalize_user_code("ABCD-EFG0"), None);
        assert_eq!(display_user_code("ABCDEFGH"), "ABCD-EFGH");
    }

    #[test]
    fn device_codes_are_secret_sized_and_hashed() {
        let a = new_device_code().expect("code");
        assert!(a.starts_with("sdc_") && a.len() == 4 + 64);
        assert_ne!(a, new_device_code().expect("code"));
        assert_eq!(hash_device_code(&a).len(), 64);
        assert_ne!(hash_device_code(&a), a);
    }
}
