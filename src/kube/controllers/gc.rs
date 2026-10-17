//! Garbage collection through ownerReferences (background propagation).
//!
//! When an object is deleted, every object that listed it as an owner loses
//! that reference; one left with no live owner is deleted in turn, which
//! cascades down (Deployment → ReplicaSet → Pod).

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

use super::util;
use crate::kube::server::ApiState;
use crate::kube::store::{ObjectKey, resource_version};

fn owner_uids(obj: &Value) -> Vec<String> {
    obj.pointer("/metadata/ownerReferences")
        .and_then(Value::as_array)
        .map(|refs| {
            refs.iter()
                .filter_map(|r| r.get("uid").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Process a batch of deletions, keyed by the namespace they happened in.
pub(crate) fn collect(api: &ApiState, deleted: &BTreeMap<String, HashSet<String>>) {
    let all_deleted: HashSet<&str> = deleted.values().flatten().map(String::as_str).collect();
    let mut namespaces: Vec<String> = deleted.keys().cloned().collect();
    if deleted.contains_key("") {
        // A cluster-scoped owner went away: dependents may live anywhere.
        namespaces.extend(api.store().keys_of_resource("", "namespaces").into_iter().map(|k| k.name));
    }
    for ns in &namespaces {
        sweep(api, ns, |uid| !all_deleted.contains(uid));
    }
}

/// Startup and resync: delete objects whose owners no longer exist at all.
pub(crate) fn collect_orphans(api: &ApiState) {
    let mut live: HashSet<String> = HashSet::new();
    let mut namespaces = vec![String::new()];
    for key in api.store().keys_of_resource("", "namespaces") {
        namespaces.push(key.name);
    }
    let mut objects = Vec::new();
    for ns in &namespaces {
        for (key, obj) in api.store().objects_in_namespace(ns) {
            live.insert(util::uid(&obj));
            objects.push((key, obj));
        }
    }
    for (key, obj) in objects {
        let owners = owner_uids(&obj);
        if !owners.is_empty() && owners.iter().all(|u| !live.contains(u)) {
            delete(api, &key, &obj);
        }
    }
}

/// Look at every object in `ns` that has owners; `alive(uid)` says whether an
/// owner still counts.
fn sweep(api: &ApiState, ns: &str, alive: impl Fn(&str) -> bool) {
    for (key, mut obj) in api.store().objects_in_namespace(ns) {
        let owners = owner_uids(&obj);
        if owners.is_empty() || owners.iter().all(|u| alive(u)) {
            continue;
        }
        if owners.iter().any(|u| alive(u)) {
            // Some owners remain: drop only the dangling references.
            if let Some(refs) = obj.pointer_mut("/metadata/ownerReferences").and_then(Value::as_array_mut) {
                refs.retain(|r| r.get("uid").and_then(Value::as_str).is_some_and(&alive));
            }
            let rv = resource_version(&obj);
            let _ = api.store().update(key, obj, rv);
            continue;
        }
        delete(api, &key, &obj);
    }
}

fn delete(api: &ApiState, key: &ObjectKey, obj: &Value) {
    let ns = obj.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or_default();
    let _ = api.delete_named(&key.group, &key.resource, ns, &key.name);
}
