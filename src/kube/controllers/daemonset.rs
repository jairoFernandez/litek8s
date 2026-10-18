//! DaemonSet: one pod on every eligible node, bound directly to it.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};

use super::Requeue;
use super::scheduler;
use super::util::{self, i, s};
use crate::kube::server::{ApiError, ApiState};

const REVISION_LABEL: &str = "controller-revision-hash";

/// Tolerations every daemon pod gets, so node conditions do not evict it.
fn daemon_tolerations(spec: &mut Value) {
    if !spec.get("tolerations").is_some_and(Value::is_array) {
        spec["tolerations"] = json!([]);
    }
    let list = spec["tolerations"].as_array_mut().unwrap();
    for (key, effect) in [
        ("node.kubernetes.io/not-ready", "NoExecute"),
        ("node.kubernetes.io/unreachable", "NoExecute"),
        ("node.kubernetes.io/disk-pressure", "NoSchedule"),
        ("node.kubernetes.io/memory-pressure", "NoSchedule"),
        ("node.kubernetes.io/pid-pressure", "NoSchedule"),
        ("node.kubernetes.io/unschedulable", "NoSchedule"),
    ] {
        if !list.iter().any(|t| t.get("key").and_then(Value::as_str) == Some(key)) {
            list.push(json!({"key": key, "operator": "Exists", "effect": effect}));
        }
    }
}

pub(crate) fn reconcile(api: &ApiState, ns: &str, name: &str) -> Result<Requeue, ApiError> {
    let Some(ds) = api.get_object("apps", "daemonsets", ns, name) else {
        return Ok(None);
    };
    if util::is_deleting(&ds) {
        return Ok(None);
    }
    let template = ds.pointer("/spec/template").cloned().unwrap_or_else(|| json!({}));
    let hash = util::hash_of(&template);
    let min_ready = i(&ds, "/spec/minReadySeconds").unwrap_or(0);
    let mut probe_spec = template.get("spec").cloned().unwrap_or_else(|| json!({}));
    daemon_tolerations(&mut probe_spec);

    let (nodes, _) = api.store().list("", "nodes", None);
    let eligible: Vec<String> = nodes
        .iter()
        .filter(|n| !util::is_deleting(n) && scheduler::node_fits_constraints(&probe_spec, n, true).is_ok())
        .map(util::name)
        .collect();
    // Nodes a running daemon pod may stay on (NoSchedule taints aside).
    let keeps: Vec<String> = nodes
        .iter()
        .filter(|n| !util::is_deleting(n) && scheduler::node_keeps_daemon_pod(&probe_spec, n))
        .map(util::name)
        .collect();

    let mut by_node: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for pod in util::owned(api, "", "pods", ns, &ds) {
        if util::is_terminal(&pod) {
            // Failed daemon pods are replaced.
            let _ = api.delete_named("", "pods", ns, &util::name(&pod));
            continue;
        }
        if util::is_active(&pod) {
            by_node.entry(s(&pod, "/spec/nodeName").to_string()).or_default().push(pod);
        }
    }

    for (node, pods) in &by_node {
        let keep = if keeps.contains(node) { 1 } else { 0 };
        for pod in pods.iter().skip(keep) {
            let _ = api.delete_named("", "pods", ns, &util::name(pod));
        }
    }
    for node in &eligible {
        if by_node.get(node).is_none_or(|p| p.is_empty()) {
            let mut pod = util::pod_from_template(
                &template,
                &ds,
                &[
                    (REVISION_LABEL, hash.clone()),
                    ("pod-template-generation", i(&ds, "/metadata/generation").unwrap_or(1).to_string()),
                ],
            );
            pod["metadata"]["generateName"] = json!(format!("{name}-"));
            daemon_tolerations(&mut pod["spec"]);
            pod["spec"]["nodeName"] = json!(node);
            api.create_object("", "pods", Some(ns), pod)?;
        }
    }

    let mut requeue = None;
    let mut ready = 0;
    let mut available = 0;
    let mut updated = 0;
    let mut current = 0;
    let mut outdated = Vec::new();
    for node in eligible.iter().chain(keeps.iter().filter(|n| !eligible.contains(n) && by_node.contains_key(*n))) {
        let Some(pod) = by_node.get(node).and_then(|p| p.first()) else { continue };
        current += 1;
        if util::is_ready(pod) {
            ready += 1;
        }
        let (is_available, wait) = util::availability(pod, min_ready);
        if is_available {
            available += 1;
        }
        if let Some(wait) = wait {
            requeue = Some(requeue.map_or(wait, |r: Duration| r.min(wait)));
        }
        if util::labels_of(pod).get(REVISION_LABEL).and_then(Value::as_str) == Some(hash.as_str()) {
            updated += 1;
        } else {
            outdated.push((is_available, util::name(pod)));
        }
    }
    let desired = eligible.len() as i64;
    let misscheduled = by_node.keys().filter(|n| !keeps.contains(n)).count();

    // Rolling update within maxUnavailable, unavailable outdated pods first.
    if s(&ds, "/spec/updateStrategy/type") != "OnDelete" && !outdated.is_empty() {
        let max_unavailable = util::int_or_percent(ds.pointer("/spec/updateStrategy/rollingUpdate/maxUnavailable"), desired, true, 1).max(1);
        let unavailable = current - available;
        outdated.sort();
        let mut budget = max_unavailable - unavailable;
        for (is_available, pod) in outdated {
            if is_available && budget <= 0 {
                break;
            }
            let _ = api.delete_named("", "pods", ns, &pod);
            if is_available {
                budget -= 1;
            }
        }
    }

    let status = json!({
        "observedGeneration": i(&ds, "/metadata/generation").unwrap_or(1),
        "desiredNumberScheduled": desired,
        "currentNumberScheduled": current,
        "numberMisscheduled": misscheduled,
        "numberReady": ready,
        "numberAvailable": available,
        "numberUnavailable": (desired - available).max(0),
        "updatedNumberScheduled": updated,
    });
    util::write_status(api, "apps", "daemonsets", &ds, status)?;
    Ok(requeue)
}
