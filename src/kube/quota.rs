//! ResourceQuota: what objects in a namespace use against a quota's `hard`
//! limits. The controller writes it to `status.used`; admission rejects
//! creates that would exceed it.

use serde_json::{Map, Value, json};

use super::quantity;
use super::store::Store;

fn s<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or_default()
}

/// The (group, plural) a quota resource name counts, for object counts.
fn counted(resource: &str) -> Option<(String, String)> {
    let plain = |p: &str| Some((String::new(), p.to_string()));
    match resource {
        "pods" | "count/pods" => plain("pods"),
        "services" | "count/services" => plain("services"),
        "configmaps" | "count/configmaps" => plain("configmaps"),
        "secrets" | "count/secrets" => plain("secrets"),
        "replicationcontrollers" | "count/replicationcontrollers" => plain("replicationcontrollers"),
        "resourcequotas" | "count/resourcequotas" => plain("resourcequotas"),
        "persistentvolumeclaims" | "count/persistentvolumeclaims" => plain("persistentvolumeclaims"),
        other => {
            let rest = other.strip_prefix("count/")?;
            match rest.split_once('.') {
                Some((plural, group)) => Some((group.to_string(), plural.to_string())),
                None => plain(rest),
            }
        }
    }
}

/// Whether a pod still uses quota: not finished.
fn pod_counts(pod: &Value) -> bool {
    !matches!(s(pod, "/status/phase"), "Succeeded" | "Failed")
}

fn best_effort(pod: &Value) -> bool {
    pod.pointer("/spec/containers").and_then(Value::as_array).into_iter().flatten().all(|c| {
        c.pointer("/resources/requests").and_then(Value::as_object).is_none_or(|m| m.is_empty())
            && c.pointer("/resources/limits").and_then(Value::as_object).is_none_or(|m| m.is_empty())
    })
}

/// Whether `pod` falls in the quota's scopes (`spec.scopes`, `scopeSelector`).
fn pod_in_scope(quota: &Value, pod: &Value) -> bool {
    let terminating = pod.pointer("/spec/activeDeadlineSeconds").and_then(Value::as_i64).is_some_and(|d| d >= 0);
    let scope_ok = |scope: &str, op: &str, values: &[&str]| -> bool {
        match scope {
            "Terminating" => terminating,
            "NotTerminating" => !terminating,
            "BestEffort" => best_effort(pod),
            "NotBestEffort" => !best_effort(pod),
            "PriorityClass" => {
                let class = s(pod, "/spec/priorityClassName");
                match op {
                    "In" => values.contains(&class),
                    "NotIn" => !values.contains(&class),
                    "Exists" => !class.is_empty(),
                    "DoesNotExist" => class.is_empty(),
                    _ => false,
                }
            }
            _ => true,
        }
    };
    for scope in quota.pointer("/spec/scopes").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        if !scope_ok(scope, "Exists", &[]) {
            return false;
        }
    }
    for expr in quota.pointer("/spec/scopeSelector/matchExpressions").and_then(Value::as_array).into_iter().flatten() {
        let values: Vec<&str> = expr.get("values").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        if !scope_ok(s(expr, "/scopeName"), s(expr, "/operator"), &values) {
            return false;
        }
    }
    true
}

fn scoped(quota: &Value) -> bool {
    quota.pointer("/spec/scopes").and_then(Value::as_array).is_some_and(|a| !a.is_empty())
        || quota.pointer("/spec/scopeSelector/matchExpressions").and_then(Value::as_array).is_some_and(|a| !a.is_empty())
}

/// Sum of a pod's container requests (or limits) of `res`, in milli-units.
fn pod_resource(pod: &Value, kind: &str, res: &str) -> i128 {
    // Not a JSON pointer: extended resource names contain '/'.
    let get = |c: &Value, kind: &str| c.get("resources").and_then(|r| r.get(kind)).and_then(|m| m.get(res)).and_then(quantity::milli_of);
    let sum: i128 = pod
        .pointer("/spec/containers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| get(c, kind).or_else(|| if kind == "requests" { get(c, "limits") } else { None }))
        .sum();
    let init_max = pod
        .pointer("/spec/initContainers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| get(c, kind))
        .max()
        .unwrap_or(0);
    sum.max(init_max)
}

/// What one object uses of quota resource `resource`, in milli-units
/// (counts are 1000 per object).
pub fn usage_of(quota: &Value, group: &str, plural: &str, obj: &Value, resource: &str) -> i128 {
    let is_pod = group.is_empty() && plural == "pods";
    if is_pod && (!pod_counts(obj) || !pod_in_scope(quota, obj)) {
        return 0;
    }
    if !is_pod && scoped(quota) {
        return 0;
    }
    if let Some((g, p)) = counted(resource) {
        return if g == group && p == plural { 1000 } else { 0 };
    }
    match (group, plural, resource) {
        ("", "pods", "cpu" | "memory" | "ephemeral-storage") => pod_resource(obj, "requests", resource),
        // requests.cpu, limits.memory, requests.example.com/dongle, ...
        ("", "pods", r) if r.starts_with("requests.") && r != "requests.storage" => pod_resource(obj, "requests", &r["requests.".len()..]),
        ("", "pods", r) if r.starts_with("limits.") => pod_resource(obj, "limits", &r["limits.".len()..]),
        ("", "services", "services.nodeports") => {
            if matches!(s(obj, "/spec/type"), "NodePort" | "LoadBalancer") {
                obj.pointer("/spec/ports").and_then(Value::as_array).map_or(0, |p| p.len() as i128 * 1000)
            } else {
                0
            }
        }
        ("", "services", "services.loadbalancers") => {
            if s(obj, "/spec/type") == "LoadBalancer" { 1000 } else { 0 }
        }
        ("", "persistentvolumeclaims", r) => {
            let storage = obj.pointer("/spec/resources/requests/storage").and_then(quantity::milli_of).unwrap_or(0);
            let class = s(obj, "/spec/storageClassName");
            if r == "requests.storage" {
                storage
            } else if r == format!("{class}.storageclass.storage.k8s.io/requests.storage") {
                storage
            } else if r == format!("{class}.storageclass.storage.k8s.io/persistentvolumeclaims") {
                1000
            } else {
                0
            }
        }
        _ => 0,
    }
}

/// The (group, plural) pairs a resource name can draw from.
fn sources(resource: &str) -> Vec<(String, String)> {
    if let Some(c) = counted(resource) {
        return vec![c];
    }
    let plain = |p: &str| vec![(String::new(), p.to_string())];
    if resource.starts_with("services.") {
        plain("services")
    } else if resource.contains(".storageclass.storage.k8s.io/") || resource == "requests.storage" {
        plain("persistentvolumeclaims")
    } else {
        plain("pods")
    }
}

/// Format milli-units like the API server would for this resource.
pub fn format(resource: &str, milli: i128) -> String {
    let cpu = resource == "cpu" || resource.ends_with(".cpu");
    if cpu {
        if milli % 1000 == 0 { (milli / 1000).to_string() } else { format!("{milli}m") }
    } else {
        let units = milli / 1000;
        if units != 0 && units % (1024 * 1024 * 1024) == 0 {
            format!("{}Gi", units / (1024 * 1024 * 1024))
        } else if units != 0 && units % (1024 * 1024) == 0 {
            format!("{}Mi", units / (1024 * 1024))
        } else if units != 0 && units % 1024 == 0 {
            format!("{}Ki", units / 1024)
        } else {
            units.to_string()
        }
    }
}

/// `status.used` for a quota: the namespace's current usage of every
/// resource in `spec.hard`.
pub fn used(store: &Store, quota: &Value) -> Map<String, Value> {
    let ns = s(quota, "/metadata/namespace");
    let mut out = Map::new();
    for resource in quota.pointer("/spec/hard").and_then(Value::as_object).into_iter().flatten().map(|(k, _)| k) {
        let mut total = 0;
        for (group, plural) in sources(resource) {
            let (items, _) = store.list(&group, &plural, Some(ns));
            total += items.iter().map(|o| usage_of(quota, &group, &plural, o, resource)).sum::<i128>();
        }
        out.insert(resource.clone(), json!(format(resource, total)));
    }
    out
}

/// Admission: the message when creating `obj` would exceed a quota.
pub fn exceeded(store: &Store, group: &str, plural: &str, ns: &str, obj: &Value) -> Option<String> {
    let (quotas, _) = store.list("", "resourcequotas", Some(ns));
    for quota in &quotas {
        let used_now = used(store, quota);
        let mut requested = Vec::new();
        let mut used_list = Vec::new();
        let mut limited = Vec::new();
        for (resource, hard) in quota.pointer("/spec/hard").and_then(Value::as_object).into_iter().flatten() {
            let delta = usage_of(quota, group, plural, obj, resource);
            if delta == 0 {
                continue;
            }
            let cur = used_now.get(resource).and_then(quantity::milli_of).unwrap_or(0);
            let max = quantity::milli_of(hard).unwrap_or(0);
            if cur + delta > max {
                requested.push(format!("{resource}={}", format(resource, delta)));
                used_list.push(format!("{resource}={}", format(resource, cur)));
                limited.push(format!("{resource}={}", hard.as_str().map(str::to_string).unwrap_or_else(|| hard.to_string())));
            }
        }
        if !requested.is_empty() {
            return Some(format!(
                "exceeded quota: {}, requested: {}, used: {}, limited: {}",
                s(quota, "/metadata/name"),
                requested.join(","),
                used_list.join(","),
                limited.join(",")
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_and_formatting() {
        let quota = json!({"spec": {"hard": {"pods": "2", "requests.cpu": "1", "services.nodeports": "1"}}});
        let pod = json!({"spec": {"containers": [{"resources": {"requests": {"cpu": "500m"}}}]}, "status": {"phase": "Running"}});
        assert_eq!(usage_of(&quota, "", "pods", &pod, "pods"), 1000);
        assert_eq!(usage_of(&quota, "", "pods", &pod, "requests.cpu"), 500);
        let dongle = json!({"spec": {"containers": [{"resources": {"requests": {"example.com/dongle": "2"}}}]}});
        assert_eq!(usage_of(&quota, "", "pods", &dongle, "requests.example.com/dongle"), 2000);
        let done = json!({"spec": {}, "status": {"phase": "Succeeded"}});
        assert_eq!(usage_of(&quota, "", "pods", &done, "pods"), 0);
        let svc = json!({"spec": {"type": "NodePort", "ports": [{"port": 80}]}});
        assert_eq!(usage_of(&quota, "", "services", &svc, "services.nodeports"), 1000);
        assert_eq!(format("requests.cpu", 500), "500m");
        assert_eq!(format("requests.memory", 1024 * 1024 * 1000), "1Mi");
        assert_eq!(format("pods", 2000), "2");
        let terminating = json!({"spec": {"scopes": ["Terminating"], "hard": {"pods": "1"}}});
        assert_eq!(usage_of(&terminating, "", "pods", &pod, "pods"), 0);
        let mut deadline = pod.clone();
        deadline["spec"]["activeDeadlineSeconds"] = json!(3600);
        assert_eq!(usage_of(&terminating, "", "pods", &deadline, "pods"), 1000);
    }
}
