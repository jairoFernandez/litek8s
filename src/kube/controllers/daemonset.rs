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
    let in_use: Vec<String> = util::owned(api, "", "pods", ns, &ds)
        .iter()
        .filter_map(|p| util::labels_of(p).get(REVISION_LABEL).and_then(Value::as_str).map(|h| format!("{name}-{h}")))
        .collect();
    super::history::sync(api, &ds, &template, &hash, &in_use)?;
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
        // Nodes whose pod is being replaced count as unavailable too.
        let unavailable = (eligible.len() as i64 - available).max(current - available);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn api() -> ApiState {
        ApiState::open(None, "127.0.0.1:0".into(), false).unwrap()
    }

    fn add_node(api: &ApiState, name: &str, labels: Value, taints: Value) {
        let node = json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": name, "labels": labels}, "spec": {"taints": taints}});
        let created = api.create_object("", "nodes", None, node).unwrap();
        util::write_status(api, "", "nodes", &created, json!({"conditions": [{"type": "Ready", "status": "True"}]})).unwrap();
    }

    fn create_ds(api: &ApiState, spec: Value) {
        let mut spec = spec;
        spec["selector"] = json!({"matchLabels": {"app": "agent"}});
        if spec.get("template").is_none() {
            spec["template"] = json!({"metadata": {"labels": {"app": "agent"}}, "spec": {"containers": [{"name": "c", "image": "agent:1"}]}});
        }
        let ds = json!({"apiVersion": "apps/v1", "kind": "DaemonSet", "metadata": {"name": "agent"}, "spec": spec});
        api.create_object("apps", "daemonsets", Some("default"), ds).unwrap();
    }

    fn live_pods(api: &ApiState) -> Vec<Value> {
        api.store().list("", "pods", Some("default")).0.into_iter().filter(util::is_active).collect()
    }

    /// Node of each live pod, sorted.
    fn placement(api: &ApiState) -> Vec<String> {
        let mut nodes: Vec<String> = live_pods(api).iter().map(|p| s(p, "/spec/nodeName").to_string()).collect();
        nodes.sort();
        nodes
    }

    fn mark_all_ready(api: &ApiState) {
        for p in live_pods(api) {
            util::write_status(api, "", "pods", &p, json!({"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}]})).unwrap();
        }
    }

    fn status(api: &ApiState) -> Value {
        api.get_object("apps", "daemonsets", "default", "agent").unwrap()["status"].clone()
    }

    fn set_spec(api: &ApiState, f: impl FnOnce(&mut Value)) {
        let mut ds = api.get_object("apps", "daemonsets", "default", "agent").unwrap();
        f(&mut ds["spec"]);
        api.update_object("apps", "daemonsets", ds).unwrap();
    }

    #[test]
    fn tolerations_are_added_once() {
        let mut spec = json!({"tolerations": [{"key": "node.kubernetes.io/not-ready", "operator": "Exists", "effect": "NoExecute", "tolerationSeconds": 5}]});
        daemon_tolerations(&mut spec);
        daemon_tolerations(&mut spec);
        let list = spec["tolerations"].as_array().unwrap();
        assert_eq!(list.len(), 6);
        // The pod's own toleration is kept as it was.
        assert_eq!(list[0]["tolerationSeconds"], 5);
        assert!(list.iter().any(|t| t["key"] == "node.kubernetes.io/unschedulable" && t["effect"] == "NoSchedule"));
    }

    #[test]
    fn one_pod_bound_to_every_eligible_node() {
        let api = api();
        add_node(&api, "a", json!({"role": "edge"}), json!([]));
        add_node(&api, "b", json!({"role": "edge"}), json!([]));
        add_node(&api, "c", json!({"role": "core"}), json!([]));
        // Untolerated NoSchedule taint: no daemon pod either.
        add_node(&api, "d", json!({"role": "edge"}), json!([{"key": "dedicated", "value": "db", "effect": "NoSchedule"}]));
        // Unschedulable nodes still get one (the daemon tolerations).
        add_node(&api, "e", json!({"role": "edge"}), json!([{"key": "node.kubernetes.io/unschedulable", "effect": "NoSchedule"}]));
        create_ds(&api, json!({"template": {"metadata": {"labels": {"app": "agent"}},
            "spec": {"nodeSelector": {"role": "edge"}, "containers": [{"name": "c", "image": "agent:1"}]}}}));
        reconcile(&api, "default", "agent").unwrap();
        assert_eq!(placement(&api), ["a", "b", "e"]);
        let pod = &live_pods(&api)[0];
        assert!(util::name(pod).starts_with("agent-"));
        assert_eq!(s(pod, "/metadata/labels/pod-template-generation"), "1");
        assert!(pod["spec"]["tolerations"].as_array().unwrap().len() >= 6);

        // Idempotent: a second pass creates nothing.
        reconcile(&api, "default", "agent").unwrap();
        assert_eq!(placement(&api), ["a", "b", "e"]);
        mark_all_ready(&api);
        reconcile(&api, "default", "agent").unwrap();
        let st = status(&api);
        assert_eq!(st["desiredNumberScheduled"], 3);
        assert_eq!(st["currentNumberScheduled"], 3);
        assert_eq!(st["numberReady"], 3);
        assert_eq!(st["numberAvailable"], 3);
        assert_eq!(st["numberUnavailable"], 0);
        assert_eq!(st["updatedNumberScheduled"], 3);
        assert_eq!(st["numberMisscheduled"], 0);
    }

    #[test]
    fn follows_nodes_coming_and_going() {
        let api = api();
        add_node(&api, "a", json!({}), json!([]));
        create_ds(&api, json!({}));
        reconcile(&api, "default", "agent").unwrap();
        assert_eq!(placement(&api), ["a"]);
        add_node(&api, "b", json!({}), json!([]));
        reconcile(&api, "default", "agent").unwrap();
        assert_eq!(placement(&api), ["a", "b"]);

        // A NoExecute taint the pod does not tolerate removes it.
        let mut b = api.get_object("", "nodes", "", "b").unwrap();
        b["spec"]["taints"] = json!([{"key": "evict", "effect": "NoExecute"}]);
        api.update_object("", "nodes", b).unwrap();
        reconcile(&api, "default", "agent").unwrap();
        assert_eq!(placement(&api), ["a"]);
    }

    #[test]
    fn noschedule_taint_keeps_a_running_pod() {
        let api = api();
        add_node(&api, "a", json!({}), json!([]));
        create_ds(&api, json!({}));
        reconcile(&api, "default", "agent").unwrap();
        mark_all_ready(&api);
        let mut a = api.get_object("", "nodes", "", "a").unwrap();
        a["spec"]["taints"] = json!([{"key": "dedicated", "effect": "NoSchedule"}]);
        api.update_object("", "nodes", a).unwrap();
        reconcile(&api, "default", "agent").unwrap();
        reconcile(&api, "default", "agent").unwrap();
        assert_eq!(placement(&api), ["a"]);
        let st = status(&api);
        assert_eq!(st["desiredNumberScheduled"], 0);
        assert_eq!(st["currentNumberScheduled"], 1);
        assert_eq!(st["numberMisscheduled"], 0);
    }

    #[test]
    fn replaces_failed_pods_and_extra_pods() {
        let api = api();
        add_node(&api, "a", json!({}), json!([]));
        create_ds(&api, json!({}));
        reconcile(&api, "default", "agent").unwrap();
        let first = live_pods(&api)[0].clone();
        // A second pod on the same node goes away.
        let mut twin = first.clone();
        twin["metadata"] = json!({"name": "agent-twin", "labels": first["metadata"]["labels"], "ownerReferences": first["metadata"]["ownerReferences"]});
        api.create_object("", "pods", Some("default"), twin).unwrap();
        reconcile(&api, "default", "agent").unwrap();
        assert_eq!(placement(&api), ["a"]);

        let pod = live_pods(&api)[0].clone();
        util::write_status(&api, "", "pods", &pod, json!({"phase": "Failed"})).unwrap();
        reconcile(&api, "default", "agent").unwrap();
        let now = live_pods(&api);
        assert_eq!(now.len(), 1);
        assert_ne!(util::uid(&now[0]), util::uid(&pod));
        assert!(api.get_object("", "pods", "default", &util::name(&pod)).is_none());
    }

    #[test]
    fn rolling_update_within_max_unavailable() {
        let api = api();
        for n in ["a", "b", "c", "d"] {
            add_node(&api, n, json!({}), json!([]));
        }
        create_ds(&api, json!({"updateStrategy": {"type": "RollingUpdate", "rollingUpdate": {"maxUnavailable": 2}}}));
        reconcile(&api, "default", "agent").unwrap();
        mark_all_ready(&api);
        set_spec(&api, |s| s["template"]["spec"]["containers"][0]["image"] = json!("agent:2"));
        let image_count = |img: &str| live_pods(&api).iter().filter(|p| s(p, "/spec/containers/0/image") == img).count();

        reconcile(&api, "default", "agent").unwrap();
        // Two old pods are being replaced, two stay.
        assert_eq!(image_count("agent:1"), 2);
        reconcile(&api, "default", "agent").unwrap();
        assert_eq!(image_count("agent:2"), 2);
        // The new pods are not ready: nothing else may go.
        reconcile(&api, "default", "agent").unwrap();
        assert_eq!(image_count("agent:1"), 2);

        mark_all_ready(&api);
        reconcile(&api, "default", "agent").unwrap();
        reconcile(&api, "default", "agent").unwrap();
        mark_all_ready(&api);
        reconcile(&api, "default", "agent").unwrap();
        assert_eq!(image_count("agent:2"), 4);
        assert_eq!(status(&api)["updatedNumberScheduled"], 4);
        // Two templates, two revisions.
        assert_eq!(api.store().list("apps", "controllerrevisions", Some("default")).0.len(), 2);
    }

    #[test]
    fn on_delete_keeps_outdated_pods() {
        let api = api();
        add_node(&api, "a", json!({}), json!([]));
        create_ds(&api, json!({"updateStrategy": {"type": "OnDelete"}}));
        reconcile(&api, "default", "agent").unwrap();
        mark_all_ready(&api);
        set_spec(&api, |s| s["template"]["spec"]["containers"][0]["image"] = json!("agent:2"));
        reconcile(&api, "default", "agent").unwrap();
        reconcile(&api, "default", "agent").unwrap();
        assert_eq!(s(&live_pods(&api)[0], "/spec/containers/0/image"), "agent:1");
        assert_eq!(status(&api)["updatedNumberScheduled"], 0);
    }
}
