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

const FOREGROUND: &str = "foregroundDeletion";

/// Foreground deletion of the object `group|resource|name` in `ns`: delete
/// its dependents (dropping only our reference from those with another live
/// owner), then let it go by removing the `foregroundDeletion` finalizer once
/// no blocking dependent is left.
pub(crate) fn foreground(api: &ApiState, ns: &str, encoded: &str) -> Result<super::Requeue, crate::kube::server::ApiError> {
    let mut parts = encoded.splitn(3, '|');
    let (group, resource, name) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
    let Some(owner) = api.get_object(group, resource, ns, name) else { return Ok(None) };
    let finalizers: Vec<String> = owner
        .pointer("/metadata/finalizers")
        .and_then(Value::as_array)
        .map(|f| f.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if !util::is_deleting(&owner) || !finalizers.iter().any(|f| f == FOREGROUND) {
        return Ok(None);
    }
    let uid = util::uid(&owner);
    let mut namespaces = vec![ns.to_string()];
    if ns.is_empty() {
        namespaces.extend(api.store().keys_of_resource("", "namespaces").into_iter().map(|k| k.name));
    }
    let mut blocking = 0;
    for dns in &namespaces {
        for (key, obj) in api.store().objects_in_namespace(dns) {
            let refs: Vec<Value> = obj.pointer("/metadata/ownerReferences").and_then(Value::as_array).cloned().unwrap_or_default();
            let Some(ours) = refs.iter().find(|r| r.get("uid").and_then(Value::as_str) == Some(uid.as_str())) else { continue };
            let blocks = ours.get("blockOwnerDeletion").and_then(Value::as_bool) == Some(true);
            let other_live = refs.iter().any(|r| {
                let other = r.get("uid").and_then(Value::as_str).unwrap_or_default();
                other != uid && owner_alive(api, dns, r)
            });
            if other_live {
                let mut obj = obj.clone();
                if let Some(refs) = obj.pointer_mut("/metadata/ownerReferences").and_then(Value::as_array_mut) {
                    refs.retain(|r| r.get("uid").and_then(Value::as_str) != Some(uid.as_str()));
                }
                let rv = resource_version(&obj);
                let _ = api.store().update(key, obj, rv);
                continue;
            }
            if blocks {
                blocking += 1;
            }
            if !util::is_deleting(&obj) {
                delete(api, &key, &obj);
            }
        }
    }
    if blocking > 0 {
        return Ok(Some(std::time::Duration::from_millis(500)));
    }
    let mut done = owner.clone();
    if let Some(f) = done.pointer_mut("/metadata/finalizers").and_then(Value::as_array_mut) {
        f.retain(|x| x.as_str() != Some(FOREGROUND));
    }
    match api.update_object(group, resource, done) {
        Ok(_) => Ok(None),
        Err(e) if e.is_not_found() => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether the owner an ownerReference names exists and is not being deleted.
fn owner_alive(api: &ApiState, ns: &str, r: &Value) -> bool {
    let api_version = r.get("apiVersion").and_then(Value::as_str).unwrap_or_default();
    let group = api_version.rsplit_once('/').map(|(g, _)| g).unwrap_or("");
    let kind = r.get("kind").and_then(Value::as_str).unwrap_or_default();
    let Some(rt) = api.resource_for_kind(group, kind) else { return false };
    let ons = if rt.namespaced { ns } else { "" };
    let name = r.get("name").and_then(Value::as_str).unwrap_or_default();
    api.get_object(&rt.group, &rt.plural, ons, name)
        .is_some_and(|o| util::uid(&o) == r.get("uid").and_then(Value::as_str).unwrap_or_default() && !util::is_deleting(&o))
}
