//! PATCH bodies: JSON merge patch (RFC 7386), JSON patch (RFC 6902), the
//! strategic merge patch kubectl sends by default, and the field-merge used for
//! server-side apply.

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value};

/// RFC 7386: objects merge recursively, `null` deletes, anything else replaces.
pub fn merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch_map) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let target_map = target.as_object_mut().unwrap();
    for (key, value) in patch_map {
        if value.is_null() {
            target_map.remove(key);
        } else {
            merge_patch(target_map.entry(key.clone()).or_insert(Value::Null), value);
        }
    }
}

/// RFC 6902 operations applied in order; the whole patch fails atomically.
pub fn json_patch(target: &mut Value, ops: &Value) -> Result<()> {
    let ops = ops.as_array().ok_or_else(|| anyhow!("a JSON patch must be an array"))?;
    let mut doc = target.clone();
    for op in ops {
        let name = op.get("op").and_then(Value::as_str).unwrap_or_default();
        let path = op
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("JSON patch operation is missing `path`"))?;
        match name {
            "add" => add(&mut doc, path, value_of(op)?)?,
            "remove" => {
                remove(&mut doc, path)?;
            }
            "replace" => {
                remove(&mut doc, path)?;
                add(&mut doc, path, value_of(op)?)?;
            }
            "move" | "copy" => {
                let from = op
                    .get("from")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("`{name}` needs `from`"))?;
                let value = if name == "move" {
                    remove(&mut doc, from)?
                } else {
                    doc.pointer(from)
                        .cloned()
                        .ok_or_else(|| anyhow!("path `{from}` does not exist"))?
                };
                add(&mut doc, path, value)?;
            }
            "test" => {
                if doc.pointer(path) != Some(value_of(op)?).as_ref() {
                    bail!("test operation failed at `{path}`");
                }
            }
            other => bail!("unsupported JSON patch operation `{other}`"),
        }
    }
    *target = doc;
    Ok(())
}

fn value_of(op: &Value) -> Result<Value> {
    op.get("value")
        .cloned()
        .ok_or_else(|| anyhow!("JSON patch operation is missing `value`"))
}

fn split_pointer(path: &str) -> Result<(String, String)> {
    let idx = path
        .rfind('/')
        .ok_or_else(|| anyhow!("invalid JSON pointer `{path}`"))?;
    let last = path[idx + 1..].replace("~1", "/").replace("~0", "~");
    Ok((path[..idx].to_string(), last))
}

fn add(doc: &mut Value, path: &str, value: Value) -> Result<()> {
    if path.is_empty() {
        *doc = value;
        return Ok(());
    }
    let (parent, last) = split_pointer(path)?;
    match doc.pointer_mut(&parent) {
        Some(Value::Object(map)) => {
            map.insert(last, value);
        }
        Some(Value::Array(items)) => {
            if last == "-" {
                items.push(value);
            } else {
                let index: usize = last.parse().map_err(|_| anyhow!("bad array index `{last}`"))?;
                if index > items.len() {
                    bail!("array index {index} out of bounds");
                }
                items.insert(index, value);
            }
        }
        _ => bail!("path `{parent}` does not exist"),
    }
    Ok(())
}

fn remove(doc: &mut Value, path: &str) -> Result<Value> {
    let (parent, last) = split_pointer(path)?;
    match doc.pointer_mut(&parent) {
        Some(Value::Object(map)) => map
            .remove(&last)
            .ok_or_else(|| anyhow!("path `{path}` does not exist")),
        Some(Value::Array(items)) => {
            let index: usize = last.parse().map_err(|_| anyhow!("bad array index `{last}`"))?;
            if index >= items.len() {
                bail!("array index {index} out of bounds");
            }
            Ok(items.remove(index))
        }
        _ => bail!("path `{path}` does not exist"),
    }
}

/// Merge key for a list field in a strategic merge patch, by field name. The
/// real apiserver reads this from Go struct tags; this table covers the lists
/// kubectl and Helm patch in the built-in types. Lists not listed here are
/// replaced wholesale, which is also what Kubernetes does for them.
fn merge_key(field: &str, items: &[Value]) -> Option<&'static str> {
    match field {
        "containers" | "initContainers" | "ephemeralContainers" | "volumes" | "env"
        | "imagePullSecrets" | "hostAliases" | "webhooks" => Some(if field == "hostAliases" { "ip" } else { "name" }),
        "volumeMounts" => Some("mountPath"),
        "volumeDevices" => Some("devicePath"),
        "ownerReferences" => Some("uid"),
        "conditions" => Some("type"),
        "topologySpreadConstraints" => Some("topologyKey"),
        "ports" => {
            let container_ports = items.iter().any(|i| i.get("containerPort").is_some());
            Some(if container_ports { "containerPort" } else { "port" })
        }
        _ => None,
    }
}

/// Primitive lists that merge as sets instead of being replaced.
fn merges_as_set(field: &str) -> bool {
    field == "finalizers"
}

/// A strategic merge patch: like a merge patch, but lists with a merge key are
/// merged element by element, and `$patch` / `$deleteFromPrimitiveList`
/// directives are honoured. `$setElementOrder` and `$retainKeys` are accepted
/// and ignored (order is kept from the target, then appended).
pub fn strategic_merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch_map) = patch else {
        *target = patch.clone();
        return;
    };
    if patch_map.get("$patch").and_then(Value::as_str) == Some("replace") {
        let mut replacement = patch.clone();
        strip_directives(&mut replacement);
        *target = replacement;
        return;
    }
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let target_map = target.as_object_mut().unwrap();
    for (key, value) in patch_map {
        if let Some(field) = key.strip_prefix("$deleteFromPrimitiveList/") {
            if let (Some(Value::Array(items)), Some(remove)) =
                (target_map.get_mut(field), value.as_array())
            {
                items.retain(|item| !remove.contains(item));
            }
            continue;
        }
        if key.starts_with('$') {
            continue;
        }
        if value.is_null() {
            target_map.remove(key);
            continue;
        }
        let existing = target_map.entry(key.clone()).or_insert(Value::Null);
        match (existing, value) {
            (Value::Array(current), Value::Array(incoming)) => {
                if let Some(mkey) = merge_key(key, incoming) {
                    let server_order: Vec<Value> = current.iter().filter_map(|e| e.get(mkey).cloned()).collect();
                    merge_keyed_list(current, incoming, mkey);
                    // Element order as the API server computes it: the patch's
                    // items in `$setElementOrder/<key>` (or patch) order, with
                    // server-only items merged in by their live positions.
                    let order: Vec<Value> = match patch_map.get(&format!("$setElementOrder/{key}")).and_then(Value::as_array) {
                        Some(o) => o.iter().filter_map(|e| e.get(mkey).cloned()).collect(),
                        None => incoming.iter().filter_map(|e| e.get(mkey).cloned()).collect(),
                    };
                    let rank = |v: &Value| v.get(mkey).and_then(|id| order.iter().position(|o| o == id));
                    let mut patched: Vec<(usize, Value)> = Vec::new();
                    let mut server_only = Vec::new();
                    for item in current.drain(..) {
                        match rank(&item) {
                            Some(r) => patched.push((r, item)),
                            None => server_only.push(item),
                        }
                    }
                    patched.sort_by_key(|(r, _)| *r);
                    let pos = |v: &Value| v.get(mkey).and_then(|id| server_order.iter().position(|o| o == id));
                    let (mut left, mut right) = (server_only.into_iter().peekable(), patched.into_iter().map(|(_, v)| v).peekable());
                    loop {
                        let take_left = match (left.peek(), right.peek()) {
                            (None, None) => break,
                            (Some(_), None) => true,
                            (None, Some(_)) => false,
                            (Some(l), Some(r)) => matches!((pos(l), pos(r)), (Some(a), Some(b)) if a < b),
                        };
                        current.push(if take_left { left.next().unwrap() } else { right.next().unwrap() });
                    }
                } else if merges_as_set(key) {
                    for item in incoming {
                        if !current.contains(item) {
                            current.push(item.clone());
                        }
                    }
                } else {
                    *current = incoming.clone();
                }
            }
            (slot, incoming) if incoming.is_object() => strategic_merge_patch(slot, incoming),
            (slot, incoming) => *slot = incoming.clone(),
        }
    }
}

fn merge_keyed_list(current: &mut Vec<Value>, incoming: &[Value], mkey: &str) {
    for item in incoming {
        let id = item.get(mkey);
        let delete = item.get("$patch").and_then(Value::as_str) == Some("delete");
        let pos = id.and_then(|id| current.iter().position(|c| c.get(mkey) == Some(id)));
        match (pos, delete) {
            (Some(i), true) => {
                current.remove(i);
            }
            (None, true) => {}
            (Some(i), false) => strategic_merge_patch(&mut current[i], item),
            (None, false) => {
                let mut fresh = item.clone();
                strip_directives(&mut fresh);
                current.push(fresh);
            }
        }
    }
}

fn strip_directives(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|k, _| !k.starts_with('$'));
            map.values_mut().for_each(strip_directives);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_directives),
        _ => {}
    }
}

/// Server-side apply without field ownership: the applied configuration is
/// merged into the live object (maps recursively, keyed lists by merge key,
/// other lists replaced). Fields dropped from the applied config are NOT
/// removed, because that needs `managedFields` tracking, which this server
/// does not do yet.
pub fn apply_merge(target: &mut Value, applied: &Value) {
    strategic_merge_patch(target, applied);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_patch_rfc7386() {
        let mut doc = json!({"a": 1, "b": {"c": 2, "d": 3}});
        merge_patch(&mut doc, &json!({"a": null, "b": {"c": 9}, "e": [1]}));
        assert_eq!(doc, json!({"b": {"c": 9, "d": 3}, "e": [1]}));
    }

    #[test]
    fn json_patch_ops() {
        let mut doc = json!({"data": {"a": "1"}, "list": [1, 2]});
        json_patch(
            &mut doc,
            &json!([
                {"op": "replace", "path": "/data/a", "value": "9"},
                {"op": "add", "path": "/list/-", "value": 3},
                {"op": "remove", "path": "/list/0"},
                {"op": "test", "path": "/data/a", "value": "9"},
                {"op": "copy", "from": "/data/a", "path": "/data/b"}
            ]),
        )
        .unwrap();
        assert_eq!(doc, json!({"data": {"a": "9", "b": "9"}, "list": [2, 3]}));
        assert!(json_patch(&mut doc, &json!([{"op": "test", "path": "/data/a", "value": "x"}])).is_err());
        assert_eq!(doc["data"]["a"], "9");
    }

    #[test]
    fn strategic_merges_containers_by_name() {
        let mut doc = json!({"spec": {"containers": [
            {"name": "app", "image": "a:1", "env": [{"name": "X", "value": "1"}]},
            {"name": "side", "image": "s:1"}
        ], "tolerations": [{"key": "a"}]}});
        strategic_merge_patch(
            &mut doc,
            &json!({"spec": {
                "$setElementOrder/containers": [{"name": "app"}, {"name": "side"}],
                "containers": [
                    {"name": "app", "image": "a:2", "env": [{"name": "Y", "value": "2"}]},
                    {"name": "side", "$patch": "delete"}
                ],
                "tolerations": [{"key": "b"}]
            }}),
        );
        assert_eq!(
            doc,
            json!({"spec": {"containers": [
                {"name": "app", "image": "a:2", "env": [{"name": "Y", "value": "2"}, {"name": "X", "value": "1"}]}
            ], "tolerations": [{"key": "b"}]}})
        );
    }

    #[test]
    fn strategic_directives() {
        let mut doc = json!({"metadata": {"finalizers": ["a"]}, "data": {"k": "v", "x": "y"}});
        strategic_merge_patch(
            &mut doc,
            &json!({"metadata": {"finalizers": ["b"], "$deleteFromPrimitiveList/finalizers": ["a"]},
                    "data": {"$patch": "replace", "only": "this"}}),
        );
        assert_eq!(doc, json!({"metadata": {"finalizers": ["b"]}, "data": {"only": "this"}}));
    }

    #[test]
    fn strategic_new_items_come_first() {
        let mut doc = json!({"containers": [{"name": "web", "image": "a"}]});
        strategic_merge_patch(&mut doc, &json!({"containers": [{"name": "new", "image": "b"}]}));
        assert_eq!(doc, json!({"containers": [{"name": "new", "image": "b"}, {"name": "web", "image": "a"}]}));
    }
}
