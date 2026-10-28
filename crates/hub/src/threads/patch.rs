//! RFC 6902 JSON Patch over `serde_json::Value`, for `StateDelta` (turn-event.md §4.6).

use protocol::turn_event::{PatchOp, PatchOpKind};
use serde_json::Value;

/// Applies `ops` to `doc`. All or nothing: on error `doc` is unchanged.
pub fn apply_patch(doc: &mut Value, ops: &[PatchOp]) -> Result<(), String> {
    let mut work = doc.clone();
    for (index, op) in ops.iter().enumerate() {
        apply_op(&mut work, op).map_err(|error| format!("patch op #{index}: {error}"))?;
    }
    *doc = work;
    Ok(())
}

fn apply_op(doc: &mut Value, op: &PatchOp) -> Result<(), String> {
    let path = tokens(&op.path)?;
    match op.op {
        PatchOpKind::Add => add(doc, &path, value_of(op)?),
        PatchOpKind::Remove => remove(doc, &path).map(drop),
        PatchOpKind::Replace => {
            if get(doc, &path).is_none() {
                return Err(format!("replace: {} does not exist", op.path));
            }
            if path.is_empty() {
                *doc = value_of(op)?;
                Ok(())
            } else {
                remove(doc, &path)?;
                add(doc, &path, value_of(op)?)
            }
        }
        PatchOpKind::Move => {
            let from = from_tokens(op)?;
            let value = remove(doc, &from)?;
            add(doc, &path, value)
        }
        PatchOpKind::Copy => {
            let from = from_tokens(op)?;
            let value = get(doc, &from)
                .ok_or_else(|| {
                    format!("copy: {} does not exist", op.from.as_deref().unwrap_or(""))
                })?
                .clone();
            add(doc, &path, value)
        }
        PatchOpKind::Test => {
            let expected = value_of(op)?;
            match get(doc, &path) {
                Some(found) if *found == expected => Ok(()),
                _ => Err(format!("test: {} does not match", op.path)),
            }
        }
    }
}

fn value_of(op: &PatchOp) -> Result<Value, String> {
    op.value
        .clone()
        .ok_or_else(|| format!("{:?} needs a value", op.op))
}

fn from_tokens(op: &PatchOp) -> Result<Vec<String>, String> {
    let from = op
        .from
        .as_deref()
        .ok_or_else(|| format!("{:?} needs from", op.op))?;
    tokens(from)
}

/// Splits a JSON Pointer into unescaped reference tokens.
fn tokens(pointer: &str) -> Result<Vec<String>, String> {
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    let rest = pointer
        .strip_prefix('/')
        .ok_or_else(|| format!("{pointer:?} is not a JSON pointer"))?;
    Ok(rest
        .split('/')
        .map(|token| token.replace("~1", "/").replace("~0", "~"))
        .collect())
}

fn get<'a>(doc: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut current = doc;
    for token in path {
        current = match current {
            Value::Object(map) => map.get(token)?,
            Value::Array(items) => items.get(array_index(token)?)?,
            _ => return None,
        };
    }
    Some(current)
}

/// `0` or a number without leading zeros.
fn array_index(token: &str) -> Option<usize> {
    if token.len() > 1 && token.starts_with('0') {
        return None;
    }
    token.parse().ok()
}

fn parent_mut<'a>(doc: &'a mut Value, parent: &[String]) -> Result<&'a mut Value, String> {
    let mut current = doc;
    for token in parent {
        current = match current {
            Value::Object(map) => map
                .get_mut(token)
                .ok_or_else(|| format!("no member {token:?}"))?,
            Value::Array(items) => {
                let index = array_index(token).ok_or_else(|| format!("bad index {token:?}"))?;
                items
                    .get_mut(index)
                    .ok_or_else(|| format!("no element {index}"))?
            }
            _ => return Err(format!("cannot descend into a scalar at {token:?}")),
        };
    }
    Ok(current)
}

fn add(doc: &mut Value, path: &[String], value: Value) -> Result<(), String> {
    let Some((last, parent)) = path.split_last() else {
        *doc = value;
        return Ok(());
    };
    match parent_mut(doc, parent)? {
        Value::Object(map) => {
            map.insert(last.clone(), value);
            Ok(())
        }
        Value::Array(items) => {
            if last == "-" {
                items.push(value);
                return Ok(());
            }
            let index = array_index(last).ok_or_else(|| format!("bad index {last:?}"))?;
            if index > items.len() {
                return Err(format!("index {index} is past the end"));
            }
            items.insert(index, value);
            Ok(())
        }
        _ => Err("add: the parent is a scalar".to_owned()),
    }
}

fn remove(doc: &mut Value, path: &[String]) -> Result<Value, String> {
    let Some((last, parent)) = path.split_last() else {
        return Err("cannot remove the document root".to_owned());
    };
    match parent_mut(doc, parent)? {
        Value::Object(map) => map
            .remove(last)
            .ok_or_else(|| format!("remove: no member {last:?}")),
        Value::Array(items) => {
            let index = array_index(last).ok_or_else(|| format!("bad index {last:?}"))?;
            if index >= items.len() {
                return Err(format!("remove: no element {index}"));
            }
            Ok(items.remove(index))
        }
        _ => Err("remove: the parent is a scalar".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn op(op: PatchOpKind, path: &str, value: Option<Value>, from: Option<&str>) -> PatchOp {
        PatchOp {
            op,
            path: path.to_owned(),
            from: from.map(str::to_owned),
            value,
        }
    }

    #[test]
    fn add_replace_remove_on_objects_and_arrays() {
        let mut doc = json!({"a": 1, "list": [1, 2]});
        apply_patch(
            &mut doc,
            &[
                op(PatchOpKind::Add, "/b", Some(json!("x")), None),
                op(PatchOpKind::Replace, "/a", Some(json!(2)), None),
                op(PatchOpKind::Add, "/list/-", Some(json!(3)), None),
                op(PatchOpKind::Add, "/list/0", Some(json!(0)), None),
                op(PatchOpKind::Remove, "/list/1", None, None),
            ],
        )
        .expect("applies");
        assert_eq!(doc, json!({"a": 2, "b": "x", "list": [0, 2, 3]}));
    }

    #[test]
    fn move_copy_test_and_escapes() {
        let mut doc = json!({"a/b": {"c": 1}, "d": []});
        apply_patch(
            &mut doc,
            &[
                op(PatchOpKind::Copy, "/d/-", None, Some("/a~1b/c")),
                op(PatchOpKind::Move, "/e", None, Some("/a~1b")),
                op(PatchOpKind::Test, "/d/0", Some(json!(1)), None),
            ],
        )
        .expect("applies");
        assert_eq!(doc, json!({"e": {"c": 1}, "d": [1]}));
    }

    #[test]
    fn a_failing_patch_leaves_the_document_unchanged() {
        let mut doc = json!({"a": 1});
        let before = doc.clone();
        let result = apply_patch(
            &mut doc,
            &[
                op(PatchOpKind::Add, "/b", Some(json!(2)), None),
                op(PatchOpKind::Remove, "/missing", None, None),
            ],
        );
        assert!(result.is_err());
        assert_eq!(doc, before);
        for bad in [
            op(PatchOpKind::Replace, "/zz", Some(json!(1)), None),
            op(PatchOpKind::Test, "/a", Some(json!(9)), None),
            op(PatchOpKind::Add, "/list/5", Some(json!(1)), None),
        ] {
            assert!(apply_patch(&mut doc, &[bad]).is_err());
        }
    }
}
