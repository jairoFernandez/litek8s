//! Object state as metrics, with kube-state-metrics' names and labels
//! (`kube_pod_status_phase`, `kube_pod_container_status_restarts_total`,
//! `kube_node_status_condition`, `kube_deployment_status_replicas_available`,
//! ...), read straight from the object store every interval: no extra
//! component, no list/watch over HTTP.

use serde_json::Value;

use super::tsdb::Labels;
use crate::kube::store::Store;

fn s<'a>(v: &'a Value, p: &str) -> &'a str {
    v.pointer(p).and_then(Value::as_str).unwrap_or_default()
}

fn n(v: &Value, p: &str) -> f64 {
    v.pointer(p).and_then(Value::as_f64).unwrap_or(0.0)
}

fn quantity(raw: &Value) -> Option<f64> {
    crate::kube::quantity::milli_of(raw).map(|m| m as f64 / 1000.0)
}

struct Out(Vec<(Labels, f64)>);

impl Out {
    fn push(&mut self, name: &str, labels: &[(&str, &str)], v: f64) {
        let mut l: Labels = Vec::with_capacity(labels.len() + 2);
        l.push(("__name__".into(), name.into()));
        l.push(("job".into(), "kube-state-metrics".into()));
        l.extend(labels.iter().map(|(a, b)| (a.to_string(), b.to_string())));
        self.0.push((l, v));
    }

    fn flag(&mut self, name: &str, labels: &[(&str, &str)], on: bool) {
        self.push(name, labels, if on { 1.0 } else { 0.0 });
    }

    /// Three series (`true`, `false`, `unknown`) for a condition status.
    fn tri(&mut self, name: &str, labels: &[(&str, &str)], key: &str, status: &str) {
        for value in ["true", "false", "unknown"] {
            let mut l = labels.to_vec();
            l.push((key, value));
            self.flag(name, &l, status.eq_ignore_ascii_case(value));
        }
    }
}

pub fn collect(store: &Store) -> Vec<(Labels, f64)> {
    let mut out = Out(Vec::new());
    pods(store, &mut out);
    nodes(store, &mut out);
    workloads(store, &mut out);
    storage(store, &mut out);
    let (namespaces, _) = store.list("", "namespaces", None);
    for ns in &namespaces {
        let phase = s(ns, "/status/phase");
        let phase = if phase.is_empty() { "Active" } else { phase };
        for p in ["Active", "Terminating"] {
            out.flag("kube_namespace_status_phase", &[("namespace", s(ns, "/metadata/name")), ("phase", p)], phase == p);
        }
    }
    out.0
}

fn pods(store: &Store, out: &mut Out) {
    let (pods, _) = store.list("", "pods", None);
    for pod in &pods {
        let ns = s(pod, "/metadata/namespace");
        let name = s(pod, "/metadata/name");
        let uid = s(pod, "/metadata/uid");
        let node = s(pod, "/spec/nodeName");
        let base = [("namespace", ns), ("pod", name), ("uid", uid)];
        let owner = pod.pointer("/metadata/ownerReferences/0");
        let (owner_kind, owner_name) = owner.map(|o| (s(o, "/kind"), s(o, "/name"))).unwrap_or_default();
        let mut info = base.to_vec();
        info.extend([
            ("node", node),
            ("host_ip", s(pod, "/status/hostIP")),
            ("pod_ip", s(pod, "/status/podIP")),
            ("created_by_kind", owner_kind),
            ("created_by_name", owner_name),
            ("priority_class", s(pod, "/spec/priorityClassName")),
        ]);
        out.push("kube_pod_info", &info, 1.0);
        let phase = s(pod, "/status/phase");
        let deleting = pod.pointer("/metadata/deletionTimestamp").is_some();
        for p in ["Pending", "Running", "Succeeded", "Failed", "Unknown"] {
            let mut l = base.to_vec();
            l.push(("phase", p));
            out.flag("kube_pod_status_phase", &l, phase == p);
        }
        if deleting {
            out.push("kube_pod_deletion_timestamp", &base, 1.0);
        }
        let ready = pod
            .pointer("/status/conditions")
            .and_then(Value::as_array)
            .and_then(|c| c.iter().find(|c| s(c, "/type") == "Ready"))
            .map(|c| s(c, "/status"))
            .unwrap_or("Unknown");
        out.tri("kube_pod_status_ready", &base, "condition", ready);
        out.flag("kube_pod_status_scheduled", &base, !node.is_empty());

        let statuses: Vec<&Value> = pod.pointer("/status/containerStatuses").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
        for c in pod.pointer("/spec/containers").and_then(Value::as_array).into_iter().flatten() {
            let cname = s(c, "/name");
            let mut l = base.to_vec();
            l.push(("container", cname));
            for (kind, field) in [("requests", "/resources/requests"), ("limits", "/resources/limits")] {
                for (res, unit) in [("cpu", "core"), ("memory", "byte")] {
                    if let Some(q) = c.pointer(&format!("{field}/{res}")).and_then(quantity) {
                        let mut rl = l.clone();
                        rl.extend([("node", node), ("resource", res), ("unit", unit)]);
                        out.push(&format!("kube_pod_container_resource_{kind}"), &rl, q);
                    }
                }
            }
            let Some(st) = statuses.iter().find(|st| s(st, "/name") == cname) else { continue };
            let mut il = l.clone();
            il.extend([("image", s(st, "/image")), ("container_id", s(st, "/containerID"))]);
            out.push("kube_pod_container_info", &il, 1.0);
            out.push("kube_pod_container_status_restarts_total", &l, n(st, "/restartCount"));
            out.flag("kube_pod_container_status_ready", &l, st.get("ready").and_then(Value::as_bool) == Some(true));
            let state = st.get("state");
            let has = |k: &str| state.and_then(|s| s.get(k)).is_some();
            out.flag("kube_pod_container_status_running", &l, has("running"));
            out.flag("kube_pod_container_status_waiting", &l, has("waiting"));
            out.flag("kube_pod_container_status_terminated", &l, has("terminated"));
            if let Some(reason) = state.and_then(|s| s.pointer("/waiting/reason")).and_then(Value::as_str) {
                let mut rl = l.clone();
                rl.push(("reason", reason));
                out.push("kube_pod_container_status_waiting_reason", &rl, 1.0);
            }
            if let Some(reason) = state.and_then(|s| s.pointer("/terminated/reason")).and_then(Value::as_str) {
                let mut rl = l.clone();
                rl.push(("reason", reason));
                out.push("kube_pod_container_status_terminated_reason", &rl, 1.0);
            }
            if let Some(reason) = st.pointer("/lastState/terminated/reason").and_then(Value::as_str) {
                let mut rl = l.clone();
                rl.push(("reason", reason));
                out.push("kube_pod_container_status_last_terminated_reason", &rl, 1.0);
                out.push("kube_pod_container_status_last_terminated_exitcode", &l, n(st, "/lastState/terminated/exitCode"));
            }
        }
    }
}

fn nodes(store: &Store, out: &mut Out) {
    let (nodes, _) = store.list("", "nodes", None);
    for node in &nodes {
        let name = s(node, "/metadata/name");
        let base = [("node", name)];
        let ip = node
            .pointer("/status/addresses")
            .and_then(Value::as_array)
            .and_then(|a| a.iter().find(|x| s(x, "/type") == "InternalIP"))
            .map(|x| s(x, "/address"))
            .unwrap_or_default();
        out.push(
            "kube_node_info",
            &[
                ("node", name),
                ("kernel_version", s(node, "/status/nodeInfo/kernelVersion")),
                ("os_image", s(node, "/status/nodeInfo/osImage")),
                ("container_runtime_version", s(node, "/status/nodeInfo/containerRuntimeVersion")),
                ("kubelet_version", s(node, "/status/nodeInfo/kubeletVersion")),
                ("internal_ip", ip),
            ],
            1.0,
        );
        for c in node.pointer("/status/conditions").and_then(Value::as_array).into_iter().flatten() {
            let mut l = base.to_vec();
            l.push(("condition", s(c, "/type")));
            out.tri("kube_node_status_condition", &l, "status", s(c, "/status"));
        }
        out.flag("kube_node_spec_unschedulable", &base, node.pointer("/spec/unschedulable").and_then(Value::as_bool) == Some(true));
        for (kind, field) in [("capacity", "/status/capacity"), ("allocatable", "/status/allocatable")] {
            for (res, unit) in [("cpu", "core"), ("memory", "byte"), ("pods", "integer"), ("ephemeral_storage", "byte")] {
                let key = res.replace('_', "-");
                if let Some(q) = node.pointer(&format!("{field}/{key}")).and_then(quantity) {
                    out.push(&format!("kube_node_status_{kind}"), &[("node", name), ("resource", res), ("unit", unit)], q);
                }
            }
        }
    }
}

fn workloads(store: &Store, out: &mut Out) {
    let (deployments, _) = store.list("apps", "deployments", None);
    for d in &deployments {
        let l = [("namespace", s(d, "/metadata/namespace")), ("deployment", s(d, "/metadata/name"))];
        out.push("kube_deployment_spec_replicas", &l, d.pointer("/spec/replicas").and_then(Value::as_f64).unwrap_or(1.0));
        for (m, f) in [
            ("kube_deployment_status_replicas", "/status/replicas"),
            ("kube_deployment_status_replicas_available", "/status/availableReplicas"),
            ("kube_deployment_status_replicas_unavailable", "/status/unavailableReplicas"),
            ("kube_deployment_status_replicas_updated", "/status/updatedReplicas"),
            ("kube_deployment_status_replicas_ready", "/status/readyReplicas"),
            ("kube_deployment_status_observed_generation", "/status/observedGeneration"),
            ("kube_deployment_metadata_generation", "/metadata/generation"),
        ] {
            out.push(m, &l, n(d, f));
        }
    }
    let (sets, _) = store.list("apps", "statefulsets", None);
    for d in &sets {
        let l = [("namespace", s(d, "/metadata/namespace")), ("statefulset", s(d, "/metadata/name"))];
        out.push("kube_statefulset_replicas", &l, d.pointer("/spec/replicas").and_then(Value::as_f64).unwrap_or(1.0));
        for (m, f) in [
            ("kube_statefulset_status_replicas", "/status/replicas"),
            ("kube_statefulset_status_replicas_ready", "/status/readyReplicas"),
            ("kube_statefulset_status_replicas_current", "/status/currentReplicas"),
            ("kube_statefulset_status_replicas_updated", "/status/updatedReplicas"),
            ("kube_statefulset_status_observed_generation", "/status/observedGeneration"),
            ("kube_statefulset_metadata_generation", "/metadata/generation"),
        ] {
            out.push(m, &l, n(d, f));
        }
    }
    let (daemons, _) = store.list("apps", "daemonsets", None);
    for d in &daemons {
        let l = [("namespace", s(d, "/metadata/namespace")), ("daemonset", s(d, "/metadata/name"))];
        for (m, f) in [
            ("kube_daemonset_status_desired_number_scheduled", "/status/desiredNumberScheduled"),
            ("kube_daemonset_status_current_number_scheduled", "/status/currentNumberScheduled"),
            ("kube_daemonset_status_number_ready", "/status/numberReady"),
            ("kube_daemonset_status_number_available", "/status/numberAvailable"),
            ("kube_daemonset_status_number_misscheduled", "/status/numberMisscheduled"),
            ("kube_daemonset_status_updated_number_scheduled", "/status/updatedNumberScheduled"),
        ] {
            out.push(m, &l, n(d, f));
        }
    }
    let (jobs, _) = store.list("batch", "jobs", None);
    for j in &jobs {
        let l = [("namespace", s(j, "/metadata/namespace")), ("job_name", s(j, "/metadata/name"))];
        for (m, f) in [("kube_job_status_active", "/status/active"), ("kube_job_status_succeeded", "/status/succeeded"), ("kube_job_status_failed", "/status/failed")] {
            out.push(m, &l, n(j, f));
        }
        let failed = j.pointer("/status/conditions").and_then(Value::as_array).is_some_and(|c| c.iter().any(|c| s(c, "/type") == "Failed" && s(c, "/status") == "True"));
        let mut fl = l.to_vec();
        fl.push(("condition", "true"));
        out.flag("kube_job_failed", &fl, failed);
    }
    let (cronjobs, _) = store.list("batch", "cronjobs", None);
    for c in &cronjobs {
        let l = [("namespace", s(c, "/metadata/namespace")), ("cronjob", s(c, "/metadata/name"))];
        out.flag("kube_cronjob_spec_suspend", &l, c.pointer("/spec/suspend").and_then(Value::as_bool) == Some(true));
        out.push("kube_cronjob_status_active", &l, c.pointer("/status/active").and_then(Value::as_array).map_or(0, Vec::len) as f64);
    }
}

fn storage(store: &Store, out: &mut Out) {
    let (claims, _) = store.list("", "persistentvolumeclaims", None);
    for c in &claims {
        let l = [("namespace", s(c, "/metadata/namespace")), ("persistentvolumeclaim", s(c, "/metadata/name"))];
        let mut info = l.to_vec();
        info.extend([("storageclass", s(c, "/spec/storageClassName")), ("volumename", s(c, "/spec/volumeName"))]);
        out.push("kube_persistentvolumeclaim_info", &info, 1.0);
        let phase = s(c, "/status/phase");
        for p in ["Pending", "Bound", "Lost"] {
            let mut pl = l.to_vec();
            pl.push(("phase", p));
            out.flag("kube_persistentvolumeclaim_status_phase", &pl, phase == p);
        }
        if let Some(q) = c.pointer("/spec/resources/requests/storage").and_then(quantity) {
            out.push("kube_persistentvolumeclaim_resource_requests_storage_bytes", &l, q);
        }
    }
    let (volumes, _) = store.list("", "persistentvolumes", None);
    for v in &volumes {
        let l = [("persistentvolume", s(v, "/metadata/name"))];
        let phase = s(v, "/status/phase");
        for p in ["Pending", "Available", "Bound", "Released", "Failed"] {
            let mut pl = l.to_vec();
            pl.push(("phase", p));
            out.flag("kube_persistentvolume_status_phase", &pl, phase == p);
        }
        if let Some(q) = v.pointer("/spec/capacity/storage").and_then(quantity) {
            out.push("kube_persistentvolume_capacity_bytes", &l, q);
        }
    }
}
