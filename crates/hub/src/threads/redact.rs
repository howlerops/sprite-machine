//! Secret redaction before an envelope is persisted or broadcast (turn-event.md §2,
//! PLAN-v2-ade.md §4.2). Every string inside `event` and `raw` is scanned; a match is replaced
//! with `[redacted:<kind>]`.
//!
//! The patterns are matched by hand (no regex crate in the wasm build). A match needs a left
//! boundary (the byte before it is not a token character), so words such as `risk-` or
//! `Bearer authentication` do not match, and each rule has a minimum token length, so short
//! hex and ordinary words stay untouched. Hashes (hex) are not secrets here and are never matched.

use std::borrow::Cow;

use protocol::turn_event::Envelope;
use serde_json::Value;

use super::error::ApiError;

/// The key the acp-mock emits for its `leak_secret` step (`crates/acp-mock/src/script.rs`).
const FAKE_API_KEY: &str =
    "sk-ant-api03-FAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKE-FAKEFAKEFAKEFAKEFAKEFAKEFAKE";

/// Redacts every secret in `text`. Returns the input borrowed when nothing matched.
pub fn redact_text(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        match secret_at(text, i) {
            Some((end, kind)) => {
                let buffer = out.get_or_insert_with(|| String::with_capacity(text.len()));
                buffer.push_str(&text[copied..i]);
                buffer.push_str("[redacted:");
                buffer.push_str(kind);
                buffer.push(']');
                copied = end;
                i = end;
            }
            None => i += 1,
        }
    }
    match out {
        None => Cow::Borrowed(text),
        Some(mut buffer) => {
            buffer.push_str(&text[copied..]);
            Cow::Owned(buffer)
        }
    }
}

/// Redacts every string in a JSON value (object keys are not scanned). `true` when one changed.
pub fn redact_value(value: &mut Value) -> bool {
    match value {
        Value::String(text) => match redact_text(text) {
            Cow::Borrowed(_) => false,
            Cow::Owned(redacted) => {
                *text = redacted;
                true
            }
        },
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= redact_value(item);
            }
            changed
        }
        Value::Object(map) => {
            let mut changed = false;
            for item in map.values_mut() {
                changed |= redact_value(item);
            }
            changed
        }
        _ => false,
    }
}

/// Redacts `event` and `raw` of an envelope in place (turn-event.md §2). Only the changed parts
/// are rebuilt, so an envelope with no secret comes back unchanged.
pub fn redact_envelope(envelope: &mut Envelope) -> Result<(), ApiError> {
    let mut event = serde_json::to_value(&envelope.event)
        .map_err(|error| ApiError::internal(error.to_string()))?;
    if redact_value(&mut event) {
        envelope.event = serde_json::from_value(event)
            .map_err(|error| ApiError::internal(format!("redacted event: {error}")))?;
    }
    if let Some(raw) = envelope.raw.as_mut() {
        let mut value =
            serde_json::to_value(&*raw).map_err(|error| ApiError::internal(error.to_string()))?;
        if redact_value(&mut value) {
            *raw = serde_json::from_value(value)
                .map_err(|error| ApiError::internal(format!("redacted raw: {error}")))?;
        }
    }
    Ok(())
}

/// The secret that starts at byte `at` of `text`: its end and its kind.
fn secret_at(text: &str, at: usize) -> Option<(usize, &'static str)> {
    if !text.is_char_boundary(at) {
        return None;
    }
    let rest = &text[at..];
    if at > 0 && is_token_byte(text.as_bytes()[at - 1]) {
        return None;
    }
    if rest.starts_with(FAKE_API_KEY) {
        return Some((at + FAKE_API_KEY.len(), "fake_api_key"));
    }
    if let Some(body) = rest.strip_prefix("sk-ant-") {
        return token_end(body, 16, is_key_byte).map(|end| (at + 7 + end, "anthropic"));
    }
    if let Some(body) = rest.strip_prefix("sk-proj-") {
        return token_end(body, 16, is_key_byte).map(|end| (at + 8 + end, "openai"));
    }
    if let Some(body) = rest.strip_prefix("sk-") {
        return token_end(body, 20, is_alnum_or_underscore).map(|end| (at + 3 + end, "openai"));
    }
    for prefix in ["ghp_", "gho_", "ghs_", "ghu_", "ghr_"] {
        if let Some(body) = rest.strip_prefix(prefix) {
            return token_end(body, 20, |b| b.is_ascii_alphanumeric())
                .map(|end| (at + prefix.len() + end, "github"));
        }
    }
    if let Some(body) = rest.strip_prefix("github_pat_") {
        return token_end(body, 20, is_alnum_or_underscore).map(|end| (at + 11 + end, "github"));
    }
    if rest.starts_with("AKIA") {
        // An AWS access key id: `AKIA` and 16 upper-case letters or digits, then no more of them.
        let token = rest
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric())
            .count();
        let key = &rest.as_bytes()[..token];
        if token == 20
            && key
                .iter()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            return Some((at + token, "aws_access_key"));
        }
    }
    if rest.len() >= 21 && rest.as_bytes()[..21].eq_ignore_ascii_case(b"aws_secret_access_key") {
        return aws_secret_end(&rest[21..]).map(|end| (at + 21 + end, "aws_secret"));
    }
    if rest.len() >= 7 && rest.as_bytes()[..7].eq_ignore_ascii_case(b"bearer ") {
        // `Bearer <token>`: a token of 12+ characters that has a digit (`Bearer dev` is not one).
        let token: &str = &rest[7..];
        let len = token
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || b"._~+/=-".contains(b))
            .count();
        if len >= 12 && token.as_bytes()[..len].iter().any(u8::is_ascii_digit) {
            return Some((at + 7 + len, "bearer"));
        }
    }
    if let Some(body) = rest.strip_prefix("-----BEGIN ") {
        return private_key_end(body).map(|end| (at + 11 + end, "private_key"));
    }
    None
}

/// The length of the token at the start of `body` when it has at least `min` bytes that all
/// satisfy `allowed`.
fn token_end(body: &str, min: usize, allowed: impl Fn(u8) -> bool) -> Option<usize> {
    let len = body.bytes().take_while(|b| allowed(*b)).count();
    (len >= min).then_some(len)
}

fn is_key_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn is_alnum_or_underscore(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// A token character for the left boundary: a secret does not start in the middle of a word.
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// `aws_secret_access_key = <40 characters>` (also `:`, quotes and spaces): the length of the
/// text from just after the key name to the end of the value, if a 40-character value follows.
fn aws_secret_end(after_key: &str) -> Option<usize> {
    let bytes = after_key.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'"' || bytes[i] == b'\'') {
        i += 1;
    }
    if i >= bytes.len() || (bytes[i] != b'=' && bytes[i] != b':') {
        return None;
    }
    i += 1;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'"' || bytes[i] == b'\'') {
        i += 1;
    }
    let value = bytes[i..]
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || b"/+=".contains(b))
        .count();
    (value >= 40).then_some(i + value)
}

/// `-----BEGIN <kind> PRIVATE KEY-----` up to the matching `-----END ... PRIVATE KEY-----` (or to
/// the end of the text when the END line is missing). `body` starts after `-----BEGIN `.
fn private_key_end(body: &str) -> Option<usize> {
    let header_len = body.find("-----")?;
    let header = &body[..header_len];
    if header.contains('\n') || !header.ends_with("PRIVATE KEY") {
        return None;
    }
    let after_header = header_len + 5;
    let end = match body[after_header..].find("-----END ") {
        None => body.len(),
        Some(found) => {
            let after_end = after_header + found + 9;
            match body[after_end..].find("-----") {
                None => body.len(),
                Some(closing) => after_end + closing + 5,
            }
        }
    };
    Some(end)
}
