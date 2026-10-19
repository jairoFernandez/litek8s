//! Helpers shared by the controllers: owner references, pod states,
//! conditions, template hashing and status writes.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};

use crate::kube::server::{ApiError, ApiState};

pub(crate) fn s<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or_default()
}

pub(crate) fn i(v: &Value, ptr: &str) -> Option<i64> {
    v.pointer(ptr).and_then(Value::as_i64)
}

pub(crate) fn name(v: &Value) -> String {
    s(v, "/metadata/name").to_string()
}

pub(crate) fn uid(v: &Value) -> String {
    s(v, "/metadata/uid").to_string()
}

pub(crate) fn ns_name(v: &Value) -> (String, String) {
    (s(v, "/metadata/namespace").to_string(), name(v))
}

pub(crate) fn now() -> DateTime<Utc> {
    Utc::now()
}

pub(crate) fn rfc3339(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub(crate) fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw).ok().map(|t| t.with_timezone(&Utc))
}

pub(crate) fn is_deleting(obj: &Value) -> bool {
    obj.pointer("/metadata/deletionTimestamp").is_some_and(|v| !v.is_null())
}

/// The `controller: true` owner reference, as (kind, name).
pub(crate) fn controller_of(obj: &Value) -> Option<(String, String)> {
    obj.pointer("/metadata/ownerReferences")?
        .as_array()?
        .iter()
        .find(|r| r.get("controller").and_then(Value::as_bool) == Some(true))
        .map(|r| (s(r, "/kind").to_string(), s(r, "/name").to_string()))
}

pub(crate) fn controlled_by(obj: &Value, owner_uid: &str) -> bool {
    obj.pointer("/metadata/ownerReferences")
        .and_then(Value::as_array)
        .is_some_and(|refs| {
            refs.iter().any(|r| {
                r.get("controller").and_then(Value::as_bool) == Some(true)
                    && r.get("uid").and_then(Value::as_str) == Some(owner_uid)
            })
        })
}

pub(crate) fn owner_ref(owner: &Value) -> Value {
    json!({
        "apiVersion": s(owner, "/apiVersion"),
        "kind": s(owner, "/kind"),
        "name": name(owner),
        "uid": uid(owner),
        "controller": true,
        "blockOwnerDeletion": true,
    })
}

/// Objects of a resource in `ns` controlled by `owner`.
pub(crate) fn owned(api: &ApiState, group: &str, plural: &str, ns: &str, owner: &Value) -> Vec<Value> {
    let owner_uid = uid(owner);
    let (items, _) = api.store().list(group, plural, Some(ns));
    items.into_iter().filter(|o| controlled_by(o, &owner_uid)).collect()
}

// ---------------------------------------------------------------------------
// Pods

pub(crate) fn phase(pod: &Value) -> &str {
    s(pod, "/status/phase")
}

pub(crate) fn is_terminal(pod: &Value) -> bool {
    matches!(phase(pod), "Succeeded" | "Failed")
}

/// Counts toward a controller's replicas: not finished, not being deleted.
pub(crate) fn is_active(pod: &Value) -> bool {
    !is_terminal(pod) && !is_deleting(pod)
}

pub(crate) fn needs_scheduling(pod: &Value) -> bool {
    s(pod, "/spec/nodeName").is_empty() && is_active(pod)
}

pub(crate) fn condition<'a>(obj: &'a Value, type_: &str) -> Option<&'a Value> {
    obj.pointer("/status/conditions")?
        .as_array()?
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some(type_))
}

pub(crate) fn condition_true(obj: &Value, type_: &str) -> bool {
    condition(obj, type_).is_some_and(|c| c.get("status").and_then(Value::as_str) == Some("True"))
}

pub(crate) fn is_ready(pod: &Value) -> bool {
    is_active(pod) && condition_true(pod, "Ready")
}

/// Ready for at least `min_ready` (so "available"); otherwise how long until
/// it will be, if it is ready at all.
pub(crate) fn availability(pod: &Value, min_ready: i64) -> (bool, Option<Duration>) {
    if !is_ready(pod) {
        return (false, None);
    }
    if min_ready <= 0 {
        return (true, None);
    }
    let since = condition(pod, "Ready")
        .and_then(|c| c.get("lastTransitionTime"))
        .and_then(Value::as_str)
        .and_then(parse_time);
    match since {
        Some(t) => {
            let ready_for = (now() - t).num_seconds();
            if ready_for >= min_ready {
                (true, None)
            } else {
                (false, Some(Duration::from_secs((min_ready - ready_for) as u64 + 1)))
            }
        }
        None => (true, None),
    }
}

/// Order pods the way Kubernetes picks victims when scaling down: not yet
/// scheduled, then pending, then not ready, then the newest.
pub(crate) fn deletion_order(pods: &mut [Value]) {
    let rank = |p: &Value| {
        let unscheduled = s(p, "/spec/nodeName").is_empty();
        let pending = phase(p) != "Running";
        let not_ready = !is_ready(p);
        (!unscheduled, !pending, !not_ready)
    };
    pods.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| s(b, "/metadata/creationTimestamp").cmp(s(a, "/metadata/creationTimestamp")))
    });
}

/// A pod built from a workload's template.
pub(crate) fn pod_from_template(template: &Value, owner: &Value, extra_labels: &[(&str, String)]) -> Value {
    let mut pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": template.get("metadata").cloned().unwrap_or_else(|| json!({})),
        "spec": template.get("spec").cloned().unwrap_or_else(|| json!({})),
    });
    let meta = pod["metadata"].as_object_mut().unwrap();
    meta.remove("name");
    meta.remove("namespace");
    meta.remove("creationTimestamp");
    let labels = meta.entry("labels").or_insert_with(|| json!({}));
    if !labels.is_object() {
        *labels = json!({});
    }
    for (k, v) in extra_labels {
        labels[*k] = json!(v);
    }
    meta.insert("ownerReferences".into(), json!([owner_ref(owner)]));
    pod
}

// ---------------------------------------------------------------------------
// Status and conditions

/// Set a condition, keeping `lastTransitionTime` when the status is unchanged.
pub(crate) fn set_condition(conditions: &mut Vec<Value>, type_: &str, status: &str, reason: &str, message: &str) {
    let now = rfc3339(now());
    match conditions.iter_mut().find(|c| c.get("type").and_then(Value::as_str) == Some(type_)) {
        Some(c) => {
            if c.get("status").and_then(Value::as_str) != Some(status) {
                c["lastTransitionTime"] = json!(now);
            }
            if c.get("reason").and_then(Value::as_str) != Some(reason) || c.get("status").and_then(Value::as_str) != Some(status) {
                c["lastUpdateTime"] = json!(now);
            }
            c["status"] = json!(status);
            c["reason"] = json!(reason);
            c["message"] = json!(message);
        }
        None => conditions.push(json!({
            "type": type_,
            "status": status,
            "reason": reason,
            "message": message,
            "lastTransitionTime": now,
            "lastUpdateTime": now,
        })),
    }
}

pub(crate) fn conditions_of(obj: &Value) -> Vec<Value> {
    obj.pointer("/status/conditions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Write `status` if it differs from what the object has.
pub(crate) fn write_status(api: &ApiState, group: &str, plural: &str, obj: &Value, status: Value) -> Result<(), ApiError> {
    if obj.get("status") == Some(&status) {
        return Ok(());
    }
    let mut updated = obj.clone();
    updated["status"] = status;
    api.update_status(group, plural, updated).map(|_| ())
}

/// Set `metadata.annotations[key]` if it is not already `value`.
pub(crate) fn ensure_annotation(api: &ApiState, group: &str, plural: &str, obj: &Value, key: &str, value: &str) -> Result<Value, ApiError> {
    let current = obj.pointer("/metadata/annotations").and_then(|a| a.get(key)).and_then(Value::as_str);
    if current == Some(value) {
        return Ok(obj.clone());
    }
    let mut updated = obj.clone();
    let meta = updated["metadata"].as_object_mut().unwrap();
    let annotations = meta.entry("annotations").or_insert_with(|| json!({}));
    if !annotations.is_object() {
        *annotations = json!({});
    }
    annotations[key] = json!(value);
    api.update_object(group, plural, updated)
}

// ---------------------------------------------------------------------------
// Hashing (pod-template-hash, controller-revision-hash)

const SAFE_ALPHABET: &[u8] = b"bcdfghjklmnpqrstvwxz2456789";

/// FNV-1a over the canonical JSON of `value`, encoded the way Kubernetes
/// encodes pod-template-hash (decimal digits mapped onto a vowel-free alphabet).
pub(crate) fn hash_of(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let mut h: u32 = 0x811c9dc5;
    for b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h.to_string()
        .bytes()
        .map(|b| SAFE_ALPHABET[(b as usize) % SAFE_ALPHABET.len()] as char)
        .collect()
}

/// A pod template without the label a controller adds to it.
pub(crate) fn template_without_label(template: &Value, label: &str) -> Value {
    let mut t = template.clone();
    if let Some(labels) = t.pointer_mut("/metadata/labels").and_then(Value::as_object_mut) {
        labels.remove(label);
        if labels.is_empty() {
            t["metadata"].as_object_mut().unwrap().remove("labels");
        }
    }
    t
}

/// Resolve an int-or-percent (`1`, `"25%"`) against `total`.
pub(crate) fn int_or_percent(value: Option<&Value>, total: i64, round_up: bool, default: i64) -> i64 {
    match value {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(default),
        Some(Value::String(s)) => match s.strip_suffix('%').and_then(|p| p.parse::<f64>().ok()) {
            Some(p) => {
                let v = p * total as f64 / 100.0;
                if round_up { v.ceil() as i64 } else { v.floor() as i64 }
            }
            None => s.parse().unwrap_or(default),
        },
        _ => default,
    }
}

pub(crate) fn labels_of(obj: &Value) -> Map<String, Value> {
    obj.pointer("/metadata/labels")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_or_percent_rounding() {
        assert_eq!(int_or_percent(Some(&json!("25%")), 3, true, 0), 1);
        assert_eq!(int_or_percent(Some(&json!("25%")), 3, false, 0), 0);
        assert_eq!(int_or_percent(Some(&json!(2)), 3, false, 0), 2);
        assert_eq!(int_or_percent(None, 3, false, 7), 7);
    }

    #[test]
    fn hashes_are_stable_and_safe() {
        let a = hash_of(&json!({"spec": {"containers": [{"image": "nginx"}]}}));
        assert_eq!(a, hash_of(&json!({"spec": {"containers": [{"image": "nginx"}]}})));
        assert!(a.bytes().all(|b| SAFE_ALPHABET.contains(&b)));
        assert_ne!(a, hash_of(&json!({"spec": {"containers": [{"image": "httpd"}]}})));
    }
}

/// Record a core/v1 Event about `obj` (like client-go's event recorder).
pub(crate) fn record_event(api: &ApiState, obj: &Value, type_: &str, reason: &str, message: &str, component: &str) {
    let (ns, name) = ns_name(obj);
    let ns = if ns.is_empty() { "default".to_string() } else { ns };
    let now = rfc3339(now());
    let mut suffix = [0u8; 8];
    use rand_core::RngCore;
    rand_core::OsRng.fill_bytes(&mut suffix);
    let event = json!({
        "apiVersion": "v1",
        "kind": "Event",
        "metadata": {"name": format!("{name}.{}", hex::encode(suffix))},
        "involvedObject": {
            "apiVersion": s(obj, "/apiVersion"), "kind": s(obj, "/kind"), "name": name,
            "namespace": s(obj, "/metadata/namespace"), "uid": uid(obj),
            "resourceVersion": s(obj, "/metadata/resourceVersion"),
        },
        "reason": reason,
        "message": message,
        "type": type_,
        "source": {"component": component},
        "reportingComponent": component,
        "reportingInstance": "",
        "firstTimestamp": now,
        "lastTimestamp": now,
        "count": 1,
    });
    let _ = api.create_object("", "events", Some(&ns), event);
}
