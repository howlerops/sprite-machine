//! `GET /v1/threads` query parsing (client-api.md §5.2): `machine_id`, `status`, `agent`,
//! `workspace`, `q`, `archived` and `cursor`. Pure, so the rules are tested natively; unknown
//! parameters are ignored.

use protocol::turn_event::is_valid_id;

use crate::threads::ApiError;

/// The longest `agent` or `workspace` filter, in characters.
const MAX_FILTER_CHARS: usize = 512;
/// The longest `q`, in characters.
const MAX_QUERY_CHARS: usize = 200;

/// The status names of client-api.md §5.2, with the stored statuses each one covers.
const STATUS_NAMES: &[(&str, &[&str])] = &[
    ("idle", &["open"]),
    ("running", &["running"]),
    ("needs_you", &["waiting"]),
    ("done", &["done", "failed", "cancelled"]),
];

/// The client status of a stored task status (`open` is `idle`, `waiting` is `needs_you`).
pub fn api_status(stored: &str) -> &'static str {
    match stored {
        "open" => "idle",
        "running" => "running",
        "waiting" => "needs_you",
        _ => "done",
    }
}

/// A parsed `GET /v1/threads` query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListQuery {
    pub machine_id: Option<String>,
    pub agent: Option<String>,
    pub workspace_path: Option<String>,
    /// Stored statuses to match; empty means any.
    pub statuses: Vec<&'static str>,
    /// Free text for the FTS5 title search.
    pub q: Option<String>,
    /// `archived=true` includes archived threads; by default they are left out.
    pub include_archived: bool,
    pub cursor: Option<Cursor>,
}

/// The keyset cursor: the `(updated_at, seq)` of the last row of a page. Opaque to clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub updated_at: String,
    pub seq: i64,
}

impl Cursor {
    pub fn encode(&self) -> String {
        format!("{}|{}", self.updated_at, self.seq)
    }

    pub fn decode(text: &str) -> Option<Cursor> {
        let (updated_at, seq) = text.rsplit_once('|')?;
        let seq: i64 = seq.parse().ok()?;
        if updated_at.is_empty() || seq < 0 {
            return None;
        }
        Some(Cursor {
            updated_at: updated_at.to_owned(),
            seq,
        })
    }
}

/// Parses the raw query string (`a=1&b=2`, percent-encoded). `400 invalid_request` for a value
/// that cannot match anything: a bad id, an unknown status, a malformed cursor.
pub fn parse_list_query(raw: Option<&str>) -> Result<ListQuery, ApiError> {
    let mut query = ListQuery::default();
    for pair in raw.unwrap_or("").split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode(key)?;
        let value = decode(value)?;
        match key.as_str() {
            "machine_id" if !value.is_empty() => {
                if !is_valid_id(&value) {
                    return Err(ApiError::invalid_request(
                        "machine_id must be 1-128 characters of [A-Za-z0-9_:.-]",
                    ));
                }
                query.machine_id = Some(value);
            }
            "agent" if !value.is_empty() => query.agent = Some(filter(value, "agent")?),
            "workspace" if !value.is_empty() => {
                query.workspace_path = Some(filter(value, "workspace")?);
            }
            "status" if !value.is_empty() => {
                let statuses = STATUS_NAMES
                    .iter()
                    .find(|(name, _)| *name == value)
                    .map(|(_, stored)| stored.to_vec())
                    .ok_or_else(|| {
                        ApiError::invalid_request(
                            "status must be one of idle, running, needs_you, done",
                        )
                    })?;
                query.statuses = statuses;
            }
            "q" => {
                let text = value.trim();
                if text.chars().count() > MAX_QUERY_CHARS {
                    return Err(ApiError::invalid_request(format!(
                        "q must be at most {MAX_QUERY_CHARS} characters"
                    )));
                }
                query.q = (!text.is_empty()).then(|| text.to_owned());
            }
            "archived" => {
                query.include_archived = match value.as_str() {
                    "" | "false" => false,
                    "true" => true,
                    _ => {
                        return Err(ApiError::invalid_request("archived must be true or false"));
                    }
                };
            }
            "cursor" if !value.is_empty() => {
                query.cursor = Some(Cursor::decode(&value).ok_or_else(|| {
                    ApiError::invalid_request("cursor is not one this API returned")
                })?);
            }
            _ => {}
        }
    }
    Ok(query)
}

fn filter(value: String, name: &str) -> Result<String, ApiError> {
    if value.chars().count() > MAX_FILTER_CHARS {
        return Err(ApiError::invalid_request(format!(
            "{name} must be at most {MAX_FILTER_CHARS} characters"
        )));
    }
    Ok(value)
}

/// Percent-decodes one query component (`+` is a space). Malformed escapes are a `400`.
fn decode(text: &str) -> Result<String, ApiError> {
    let malformed = || ApiError::invalid_request("query string has a malformed percent escape");
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' => {
                let digits = bytes.get(index + 1..index + 3).ok_or_else(malformed)?;
                let digits = std::str::from_utf8(digits).map_err(|_| malformed())?;
                out.push(u8::from_str_radix(digits, 16).map_err(|_| malformed())?);
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| malformed())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_query_is_the_default_list() {
        assert_eq!(parse_list_query(None).expect("none"), ListQuery::default());
        assert_eq!(
            parse_list_query(Some("")).expect("empty"),
            ListQuery::default()
        );
    }

    #[test]
    fn filters_and_status_names_parse() {
        let query = parse_list_query(Some(
            "machine_id=mch_1&status=needs_you&agent=claude&workspace=%2Fsrc%2Fapp&q=fix+login&archived=true",
        ))
        .expect("parse");
        assert_eq!(query.machine_id.as_deref(), Some("mch_1"));
        assert_eq!(query.statuses, vec!["waiting"]);
        assert_eq!(query.agent.as_deref(), Some("claude"));
        assert_eq!(query.workspace_path.as_deref(), Some("/src/app"));
        assert_eq!(query.q.as_deref(), Some("fix login"));
        assert!(query.include_archived);
    }

    #[test]
    fn bad_values_are_400() {
        for raw in [
            "machine_id=bad%20id",
            "status=paused",
            "archived=yes",
            "cursor=nonsense",
            "q=%ZZ",
        ] {
            let error = parse_list_query(Some(raw)).expect_err(raw);
            assert_eq!(error.status, 400, "{raw}");
        }
    }

    #[test]
    fn cursors_round_trip() {
        let cursor = Cursor {
            updated_at: "2026-10-28T10:00:00.000Z".to_owned(),
            seq: 42,
        };
        assert_eq!(Cursor::decode(&cursor.encode()), Some(cursor));
        assert_eq!(Cursor::decode("|3"), None);
        assert_eq!(Cursor::decode("x|-1"), None);
    }

    #[test]
    fn api_status_maps_every_stored_status() {
        assert_eq!(api_status("open"), "idle");
        assert_eq!(api_status("waiting"), "needs_you");
        assert_eq!(api_status("cancelled"), "done");
    }
}
