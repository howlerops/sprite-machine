//! RFC 6902 JSON Patch over `serde_json` values, for `StateDelta` and `Activity{mode: patch}`
//! (turn-event.md §4.6, §4.4). A patch is atomic: it applies to a copy, and the document is
//! replaced only when every operation succeeded.

use protocol::turn_event::{PatchOp, PatchOpKind};
use serde_json::{Map, Value};

/// The members of `value` when it is an object, else an empty map.
pub fn as_object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// A patch that does not apply. The document is left unchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("json patch {op:?} at {path}: {reason}")]
pub struct PatchError {
    pub op: PatchOpKind,
    pub path: String,
    pub reason: String,
}

/// Applies `ops` to `doc` in order, all or nothing.
pub fn apply(doc: &mut Value, ops: &[PatchOp]) -> Result<(), PatchError> {
    let mut next = doc.clone();
    for op in ops {
        apply_one(&mut next, op).map_err(|reason| PatchError {
            op: op.op,
            path: op.path.clone(),
            reason,
        })?;
    }
    *doc = next;
    Ok(())
}

fn apply_one(doc: &mut Value, op: &PatchOp) -> Result<(), String> {
    match op.op {
        PatchOpKind::Add => add(doc, &op.path, required_value(op)?),
        PatchOpKind::Replace => {
            let value = required_value(op)?;
            let slot = doc
                .pointer_mut(&op.path)
                .ok_or_else(|| "no value at path".to_owned())?;
            *slot = value;
            Ok(())
        }
        PatchOpKind::Remove => remove(doc, &op.path).map(|_| ()),
        PatchOpKind::Test => {
            let expected = required_value(op)?;
            if doc.pointer(&op.path) == Some(&expected) {
                Ok(())
            } else {
                Err("test failed".to_owned())
            }
        }
        PatchOpKind::Copy => {
            let from = required_from(op)?;
            let value = doc
                .pointer(&from)
                .cloned()
                .ok_or_else(|| format!("copy: no value at {from}"))?;
            add(doc, &op.path, value)
        }
        PatchOpKind::Move => {
            let from = required_from(op)?;
            let value = remove(doc, &from)?;
            add(doc, &op.path, value)
        }
    }
}

fn required_value(op: &PatchOp) -> Result<Value, String> {
    op.value
        .clone()
        .ok_or_else(|| "operation needs a value".to_owned())
}

fn required_from(op: &PatchOp) -> Result<String, String> {
    op.from
        .clone()
        .ok_or_else(|| "operation needs a from".to_owned())
}

/// Splits a JSON pointer into its parent pointer and its unescaped last token.
fn split(path: &str) -> Result<(&str, String), String> {
    let index = path
        .rfind('/')
        .ok_or_else(|| format!("invalid pointer {path:?}"))?;
    let token = path[index + 1..].replace("~1", "/").replace("~0", "~");
    Ok((&path[..index], token))
}

fn add(doc: &mut Value, path: &str, value: Value) -> Result<(), String> {
    if path.is_empty() {
        *doc = value;
        return Ok(());
    }
    let (parent_path, token) = split(path)?;
    let parent = doc
        .pointer_mut(parent_path)
        .ok_or_else(|| format!("no parent at {parent_path:?}"))?;
    match parent {
        Value::Object(map) => {
            map.insert(token, value);
            Ok(())
        }
        Value::Array(items) => {
            if token == "-" {
                items.push(value);
                return Ok(());
            }
            let index = array_index(&token, items.len() + 1)?;
            items.insert(index, value);
            Ok(())
        }
        _ => Err("parent is not a container".to_owned()),
    }
}

fn remove(doc: &mut Value, path: &str) -> Result<Value, String> {
    if path.is_empty() {
        return Ok(std::mem::take(doc));
    }
    let (parent_path, token) = split(path)?;
    let parent = doc
        .pointer_mut(parent_path)
        .ok_or_else(|| format!("no parent at {parent_path:?}"))?;
    match parent {
        Value::Object(map) => map
            .remove(&token)
            .ok_or_else(|| format!("no member {token:?}")),
        Value::Array(items) => {
            let index = array_index(&token, items.len())?;
            Ok(items.remove(index))
        }
        _ => Err("parent is not a container".to_owned()),
    }
}

/// An array index strictly below `bound` (RFC 6901: no sign, no leading zeros).
fn array_index(token: &str, bound: usize) -> Result<usize, String> {
    let canonical = token == "0" || (!token.starts_with('0') && !token.is_empty());
    if !canonical || !token.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("bad array index {token:?}"));
    }
    let index: usize = token
        .parse()
        .map_err(|_| format!("bad array index {token:?}"))?;
    if index < bound {
        Ok(index)
    } else {
        Err(format!("array index {index} out of range"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn op(kind: PatchOpKind, path: &str, value: Option<Value>) -> PatchOp {
        PatchOp {
            op: kind,
            path: path.to_owned(),
            from: None,
            value,
        }
    }

    #[test]
    fn replace_add_remove_and_array_append() {
        let mut doc = json!({"usage": {"used": 1}, "list": [1]});
        apply(
            &mut doc,
            &[
                op(PatchOpKind::Replace, "/usage/used", Some(json!(2))),
                op(PatchOpKind::Add, "/list/-", Some(json!(3))),
                op(PatchOpKind::Add, "/cost", Some(json!(null))),
                op(PatchOpKind::Remove, "/usage", None),
            ],
        )
        .expect("patch applies");
        assert_eq!(doc, json!({"list": [1, 3], "cost": null}));
    }

    #[test]
    fn failed_patch_leaves_document_unchanged() {
        let mut doc = json!({"a": 1});
        let error = apply(
            &mut doc,
            &[
                op(PatchOpKind::Replace, "/a", Some(json!(2))),
                op(PatchOpKind::Replace, "/missing", Some(json!(3))),
            ],
        )
        .expect_err("second op fails");
        assert_eq!(error.path, "/missing");
        assert_eq!(doc, json!({"a": 1}));
    }

    #[test]
    fn escaped_tokens_and_test_op() {
        let mut doc = json!({"a/b": {"c~d": 1}});
        apply(
            &mut doc,
            &[
                op(PatchOpKind::Test, "/a~1b/c~0d", Some(json!(1))),
                op(PatchOpKind::Replace, "/a~1b/c~0d", Some(json!(9))),
            ],
        )
        .expect("escaped pointer applies");
        assert_eq!(doc, json!({"a/b": {"c~d": 9}}));
    }
}
