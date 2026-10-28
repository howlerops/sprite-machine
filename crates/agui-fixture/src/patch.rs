//! A small RFC 6902 applier for replaying `StateDelta` patches when a subscriber's `snapshot`
//! is built (turn-event.md §6: the snapshot is the state as of `at_seq`).

use protocol::turn_event::{PatchOp, PatchOpKind};
use serde_json::Value;

/// Applies `ops` to `doc` in order. Stops at the first failing operation.
pub fn apply(doc: &mut Value, ops: &[PatchOp]) -> Result<(), String> {
    for op in ops {
        apply_one(doc, op)?;
    }
    Ok(())
}

fn apply_one(doc: &mut Value, op: &PatchOp) -> Result<(), String> {
    match op.op {
        PatchOpKind::Add => {
            let value = required_value(op)?;
            add(doc, &op.path, value)
        }
        PatchOpKind::Replace => {
            let value = required_value(op)?;
            let slot = doc
                .pointer_mut(&op.path)
                .ok_or_else(|| format!("replace: no value at {}", op.path))?;
            *slot = value;
            Ok(())
        }
        PatchOpKind::Remove => remove(doc, &op.path).map(|_| ()),
        PatchOpKind::Test => {
            let expected = required_value(op)?;
            if doc.pointer(&op.path) == Some(&expected) {
                Ok(())
            } else {
                Err(format!("test failed at {}", op.path))
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
        .ok_or_else(|| format!("{:?} at {} needs a value", op.op, op.path))
}

fn required_from(op: &PatchOp) -> Result<String, String> {
    op.from
        .clone()
        .ok_or_else(|| format!("{:?} at {} needs a from", op.op, op.path))
}

/// Splits a JSON pointer into (parent pointer, unescaped last token).
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
        .ok_or_else(|| format!("add: no parent for {path}"))?;
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
            let index: usize = token
                .parse()
                .map_err(|_| format!("add: bad array index in {path}"))?;
            if index > items.len() {
                return Err(format!("add: index out of range in {path}"));
            }
            items.insert(index, value);
            Ok(())
        }
        _ => Err(format!("add: parent of {path} is not a container")),
    }
}

fn remove(doc: &mut Value, path: &str) -> Result<Value, String> {
    if path.is_empty() {
        return Err("remove: cannot remove the whole document".to_owned());
    }
    let (parent_path, token) = split(path)?;
    let parent = doc
        .pointer_mut(parent_path)
        .ok_or_else(|| format!("remove: no parent for {path}"))?;
    match parent {
        Value::Object(map) => map
            .remove(&token)
            .ok_or_else(|| format!("remove: no member at {path}")),
        Value::Array(items) => {
            let index: usize = token
                .parse()
                .map_err(|_| format!("remove: bad array index in {path}"))?;
            if index >= items.len() {
                return Err(format!("remove: index out of range in {path}"));
            }
            Ok(items.remove(index))
        }
        _ => Err(format!("remove: parent of {path} is not a container")),
    }
}

#[cfg(test)]
mod tests {
    use protocol::turn_event::{PatchOp, PatchOpKind};
    use serde_json::json;

    use super::apply;

    fn op(kind: PatchOpKind, path: &str, value: Option<serde_json::Value>) -> PatchOp {
        PatchOp {
            op: kind,
            path: path.to_owned(),
            from: None,
            value,
        }
    }

    #[test]
    fn replace_add_and_remove_on_objects_and_arrays() {
        let mut doc = json!({"cost": {"amount": 1}, "list": [1, 2]});
        apply(
            &mut doc,
            &[
                op(PatchOpKind::Replace, "/cost/amount", Some(json!(2))),
                op(PatchOpKind::Add, "/list/1", Some(json!(9))),
                op(PatchOpKind::Add, "/list/-", Some(json!(7))),
                op(PatchOpKind::Remove, "/cost", None),
            ],
        )
        .expect("patch applies");
        assert_eq!(doc, json!({"list": [1, 9, 2, 7]}));
    }

    #[test]
    fn replace_of_missing_member_fails() {
        let mut doc = json!({"a": 1});
        let result = apply(&mut doc, &[op(PatchOpKind::Replace, "/b", Some(json!(2)))]);
        assert!(result.is_err());
    }
}
