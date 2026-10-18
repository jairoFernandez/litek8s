//! The scheduler: bind each pending pod to a Node (`spec.nodeName`).
//!
//! Filters: node Ready and schedulable, taints tolerated, `nodeSelector` and
//! required node affinity, cpu/memory/pod-count requests fitting allocatable,
//! and claims bound to local volumes on another node. Among the nodes left it
//! spreads replicas: fewest pods of the same owner first (Kubernetes's
//! default topology spread over hostnames), then the least loaded by
//! requested cpu, then the fewest pods. Claims still unbound
//! (WaitForFirstConsumer) are provisioned on the chosen node.

use serde_json::{Value, json};

use super::util::{self, s};
use crate::kube::quantity;
use crate::kube::server::{ApiError, ApiState};

pub(crate) enum Outcome {
    Done,
    Unschedulable,
}

/// Requested (milli-cpu, memory bytes) of a pod: the sum over containers,
/// or the largest init container if that is bigger.
pub(crate) fn pod_requests(spec: &Value) -> (i128, i128) {
    let req = |c: &Value, res: &str| {
        c.pointer(&format!("/resources/requests/{res}"))
            .or_else(|| c.pointer(&format!("/resources/limits/{res}")))
            .and_then(quantity::milli_of)
            .unwrap_or(0)
    };
    let sum = |field: &str, res: &str| -> i128 {
        spec.get(field).and_then(Value::as_array).map(|cs| cs.iter().map(|c| req(c, res)).sum()).unwrap_or(0)
    };
    let max_init = |res: &str| -> i128 {
        spec.get("initContainers")
            .and_then(Value::as_array)
            .map(|cs| cs.iter().map(|c| req(c, res)).max().unwrap_or(0))
            .unwrap_or(0)
    };
    let cpu = sum("containers", "cpu").max(max_init("cpu"));
    let mem = sum("containers", "memory").max(max_init("memory")) / 1000;
    (cpu, mem)
}

/// Whether toleration `t` tolerates `taint`.
pub(crate) fn toleration_matches(t: &Value, taint: &Value) -> bool {
    let t_effect = s(t, "/effect");
    if !t_effect.is_empty() && t_effect != s(taint, "/effect") {
        return false;
    }
    let t_key = s(t, "/key");
    match s(t, "/operator") {
        "Exists" => t_key.is_empty() || t_key == s(taint, "/key"),
        _ => t_key == s(taint, "/key") && s(t, "/value") == s(taint, "/value"),
    }
}

fn tolerates(spec: &Value, taint: &Value) -> bool {
    spec.get("tolerations").and_then(Value::as_array).is_some_and(|ts| ts.iter().any(|t| toleration_matches(t, taint)))
}

fn expression_matches(expr: &Value, value: Option<&str>) -> bool {
    let values: Vec<&str> = expr
        .get("values")
        .and_then(Value::as_array)
        .map(|vs| vs.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let num = |v: Option<&str>| v.and_then(|v| v.parse::<i64>().ok());
    match s(expr, "/operator") {
        "In" => value.is_some_and(|v| values.contains(&v)),
        "NotIn" => !value.is_some_and(|v| values.contains(&v)),
        "Exists" => value.is_some(),
        "DoesNotExist" => value.is_none(),
        "Gt" => num(value).zip(num(values.first().copied())).is_some_and(|(a, b)| a > b),
        "Lt" => num(value).zip(num(values.first().copied())).is_some_and(|(a, b)| a < b),
        _ => false,
    }
}

fn node_affinity_matches(spec: &Value, node: &Value) -> bool {
    let Some(terms) = spec
        .pointer("/affinity/nodeAffinity/requiredDuringSchedulingIgnoredDuringExecution/nodeSelectorTerms")
        .and_then(Value::as_array)
    else {
        return true;
    };
    let labels = util::labels_of(node);
    let node_name = util::name(node);
    // Terms are ORed; expressions within a term are ANDed.
    terms.iter().any(|term| {
        let exprs_ok = term
            .get("matchExpressions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .all(|e| expression_matches(e, labels.get(s(e, "/key")).and_then(Value::as_str)));
        let fields_ok = term
            .get("matchFields")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .all(|e| s(e, "/key") == "metadata.name" && expression_matches(e, Some(&node_name)));
        let empty = term.get("matchExpressions").is_none() && term.get("matchFields").is_none();
        !empty && exprs_ok && fields_ok
    })
}

/// Whether a pod spec may run on `node`, ignoring resources. With
/// `daemon`, readiness and cordoning do not matter (DaemonSet semantics).
pub(crate) fn node_fits_constraints(spec: &Value, node: &Value, daemon: bool) -> Result<(), String> {
    fits(spec, node, daemon, &["NoSchedule", "NoExecute"])
}

/// Whether a daemon pod already on `node` may stay: only NoExecute taints
/// count (a NoSchedule taint, e.g. unreachable, keeps new pods away but
/// leaves running ones alone).
pub(crate) fn node_keeps_daemon_pod(spec: &Value, node: &Value) -> bool {
    fits(spec, node, true, &["NoExecute"]).is_ok()
}

fn fits(spec: &Value, node: &Value, daemon: bool, effects: &[&str]) -> Result<(), String> {
    if !daemon {
        if !util::condition_true(node, "Ready") {
            return Err("node is not ready".into());
        }
        if node.pointer("/spec/unschedulable").and_then(Value::as_bool) == Some(true) {
            return Err("node is unschedulable".into());
        }
    }
    for taint in node.pointer("/spec/taints").and_then(Value::as_array).into_iter().flatten() {
        let effect = s(taint, "/effect");
        if effects.contains(&effect) && !tolerates(spec, taint) {
            return Err(format!("node had untolerated taint {{{}: {}}}", s(taint, "/key"), s(taint, "/value")));
        }
    }
    if let Some(selector) = spec.get("nodeSelector").and_then(Value::as_object) {
        let labels = util::labels_of(node);
        if !selector.iter().all(|(k, v)| labels.get(k) == Some(v)) {
            return Err("node didn't match Pod's node affinity/selector".into());
        }
    }
    if !node_affinity_matches(spec, node) {
        return Err("node didn't match Pod's node affinity/selector".into());
    }
    Ok(())
}

/// The node a local PersistentVolume is pinned to.
fn volume_node(pv: &Value) -> Option<String> {
    pv.pointer("/spec/nodeAffinity/required/nodeSelectorTerms/0/matchExpressions")?
        .as_array()?
        .iter()
        .find(|e| s(e, "/key") == "kubernetes.io/hostname")
        .and_then(|e| e.pointer("/values/0"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub(crate) fn schedule(api: &ApiState, ns: &str, name: &str) -> Result<Outcome, ApiError> {
    let Some(pod) = api.get_object("", "pods", ns, name) else {
        return Ok(Outcome::Done);
    };
    if !util::needs_scheduling(&pod) {
        return Ok(Outcome::Done);
    }
    let spec = pod.get("spec").cloned().unwrap_or_else(|| json!({}));

    // Claims: all must exist; bound ones pin the node.
    let mut pinned: Option<String> = None;
    let mut unbound = Vec::new();
    for volume in spec.get("volumes").and_then(Value::as_array).into_iter().flatten() {
        let claim = s(volume, "/persistentVolumeClaim/claimName");
        if claim.is_empty() {
            continue;
        }
        let Some(pvc) = api.get_object("", "persistentvolumeclaims", ns, claim) else {
            return unschedulable(api, &pod, &format!("persistentvolumeclaim \"{claim}\" not found"));
        };
        let pv_name = s(&pvc, "/spec/volumeName");
        if pv_name.is_empty() {
            unbound.push(pvc);
        } else if let Some(node) = api.get_object("", "persistentvolumes", "", pv_name).as_ref().and_then(volume_node) {
            pinned = Some(node);
        }
    }

    let (nodes, _) = api.store().list("", "nodes", None);
    let (pods, _) = api.store().list("", "pods", None);
    let (want_cpu, want_mem) = pod_requests(&spec);
    let mut reasons: Vec<String> = Vec::new();
    let owner = controller_uid(&pod);
    let mut best: Option<((usize, f64, usize), String)> = None;
    for node in &nodes {
        let node_name = util::name(node);
        if pinned.as_ref().is_some_and(|p| *p != node_name) {
            reasons.push("node(s) had volume node affinity conflict".into());
            continue;
        }
        if let Err(reason) = node_fits_constraints(&spec, node, false) {
            reasons.push(reason);
            continue;
        }
        let alloc = |res: &str| node.pointer(&format!("/status/allocatable/{res}")).and_then(quantity::milli_of);
        let on_node: Vec<&Value> = pods
            .iter()
            .filter(|p| s(p, "/spec/nodeName") == node_name && util::is_active(p))
            .collect();
        let (used_cpu, used_mem) = on_node
            .iter()
            .map(|p| pod_requests(p.get("spec").unwrap_or(&Value::Null)))
            .fold((0, 0), |(a, b), (c, d)| (a + c, b + d));
        if let Some(max_pods) = alloc("pods") {
            if (on_node.len() as i128 + 1) * 1000 > max_pods {
                reasons.push("Too many pods".into());
                continue;
            }
        }
        let cpu_cap = alloc("cpu");
        if cpu_cap.is_some_and(|cap| used_cpu + want_cpu > cap) {
            reasons.push("Insufficient cpu".into());
            continue;
        }
        if alloc("memory").is_some_and(|cap| (used_mem + want_mem) * 1000 > cap) {
            reasons.push("Insufficient memory".into());
            continue;
        }
        let load = cpu_cap.map(|cap| (used_cpu + want_cpu) as f64 / cap.max(1) as f64).unwrap_or(0.0);
        let siblings = owner.as_ref().map_or(0, |o| on_node.iter().filter(|p| controller_uid(p).as_ref() == Some(o)).count());
        let score = (siblings, load, on_node.len());
        let better = best.as_ref().is_none_or(|(b, _)| score.0 < b.0 || (score.0 == b.0 && (score.1 < b.1 || (score.1 == b.1 && score.2 < b.2))));
        if better {
            best = Some((score, node_name));
        }
    }
    let Some((_, node)) = best else {
        reasons.sort();
        reasons.dedup();
        let message = format!("0/{} nodes are available: {}.", nodes.len(), reasons.join(", "));
        return unschedulable(api, &pod, &message);
    };

    for pvc in unbound {
        provision(api, &pvc, &node)?;
    }
    let mut bound = pod.clone();
    bound["spec"]["nodeName"] = json!(node);
    let bound = api.update_object("", "pods", bound)?;
    set_scheduled(api, bound, "True", "", "")?;
    Ok(Outcome::Done)
}

/// The uid of the pod's controlling owner (its ReplicaSet, StatefulSet...).
fn controller_uid(pod: &Value) -> Option<String> {
    pod.pointer("/metadata/ownerReferences")?
        .as_array()?
        .iter()
        .find(|r| r.get("controller").and_then(Value::as_bool) == Some(true))
        .and_then(|r| r.get("uid").and_then(Value::as_str))
        .map(str::to_string)
}

fn unschedulable(api: &ApiState, pod: &Value, message: &str) -> Result<Outcome, ApiError> {
    set_scheduled(api, pod.clone(), "False", "Unschedulable", message)?;
    Ok(Outcome::Unschedulable)
}

fn set_scheduled(api: &ApiState, mut pod: Value, status: &str, reason: &str, message: &str) -> Result<(), ApiError> {
    for _ in 0..5 {
        let mut conditions = util::conditions_of(&pod);
        let existing = util::condition(&pod, "PodScheduled");
        if existing.is_some_and(|c| s(c, "/status") == status && s(c, "/message") == message) {
            return Ok(());
        }
        let now = util::rfc3339(util::now());
        conditions.retain(|c| c["type"] != "PodScheduled");
        let mut condition = json!({"type": "PodScheduled", "status": status, "lastProbeTime": null, "lastTransitionTime": now});
        if !reason.is_empty() {
            condition["reason"] = json!(reason);
            condition["message"] = json!(message);
        }
        conditions.insert(0, condition);
        let mut status_obj = pod.get("status").cloned().unwrap_or_else(|| json!({}));
        status_obj["conditions"] = json!(conditions);
        match util::write_status(api, "", "pods", &pod, status_obj) {
            Ok(()) => return Ok(()),
            Err(e) if e.is_conflict() => {
                let (ns, name) = util::ns_name(&pod);
                match api.get_object("", "pods", &ns, &name) {
                    Some(fresh) => pod = fresh,
                    None => return Ok(()),
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Create a node-local PersistentVolume for `pvc` on `node` and bind both.
pub(crate) fn provision(api: &ApiState, pvc: &Value, node: &str) -> Result<(), ApiError> {
    let (ns, claim) = util::ns_name(pvc);
    let class = s(pvc, "/spec/storageClassName");
    if class.is_empty() || api.get_object("storage.k8s.io", "storageclasses", "", class).is_none() {
        // No dynamic provisioning for this claim; it waits for a static PV.
        return Ok(());
    }
    let pv_name = format!("pvc-{}", util::uid(pvc));
    let storage = pvc.pointer("/spec/resources/requests/storage").cloned().unwrap_or_else(|| json!("1Gi"));
    let access_modes = pvc.pointer("/spec/accessModes").cloned().unwrap_or_else(|| json!(["ReadWriteOnce"]));
    let reclaim = api
        .get_object("storage.k8s.io", "storageclasses", "", class)
        .and_then(|sc| sc.get("reclaimPolicy").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "Delete".into());
    if api.get_object("", "persistentvolumes", "", &pv_name).is_none() {
        let pv = json!({
            "apiVersion": "v1",
            "kind": "PersistentVolume",
            "metadata": {
                "name": pv_name,
                "annotations": {"pv.kubernetes.io/provisioned-by": "litek8s.dev/local-path"},
            },
            "spec": {
                "capacity": {"storage": storage},
                "accessModes": access_modes,
                "persistentVolumeReclaimPolicy": reclaim,
                "storageClassName": class,
                "volumeMode": pvc.pointer("/spec/volumeMode").cloned().unwrap_or_else(|| json!("Filesystem")),
                "claimRef": {
                    "apiVersion": "v1",
                    "kind": "PersistentVolumeClaim",
                    "namespace": ns,
                    "name": claim,
                    "uid": util::uid(pvc),
                },
                "local": {"path": format!("/var/lib/litek8s/volumes/{pv_name}")},
                "nodeAffinity": {"required": {"nodeSelectorTerms": [{"matchExpressions": [
                    {"key": "kubernetes.io/hostname", "operator": "In", "values": [node]},
                ]}]}},
            },
        });
        let created = api.create_object("", "persistentvolumes", None, pv)?;
        util::write_status(api, "", "persistentvolumes", &created, json!({"phase": "Bound"}))?;
    }
    let mut bound = pvc.clone();
    bound["spec"]["volumeName"] = json!(pv_name);
    let annotations = bound["metadata"].as_object_mut().unwrap().entry("annotations").or_insert_with(|| json!({}));
    annotations["pv.kubernetes.io/bind-completed"] = json!("yes");
    annotations["pv.kubernetes.io/bound-by-controller"] = json!("yes");
    annotations["volume.kubernetes.io/selected-node"] = json!(node);
    annotations["volume.kubernetes.io/storage-provisioner"] = json!("litek8s.dev/local-path");
    let bound = api.update_object("", "persistentvolumeclaims", bound)?;
    util::write_status(
        api,
        "",
        "persistentvolumeclaims",
        &bound,
        json!({"phase": "Bound", "accessModes": access_modes, "capacity": {"storage": storage}}),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str) -> Value {
        json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": name},
            "status": {"allocatable": {"cpu": "2", "memory": "1Gi", "pods": "110"}, "conditions": [{"type": "Ready", "status": "True"}]}})
    }

    #[test]
    fn spreads_replicas_of_one_owner() {
        let api = ApiState::open(None, "127.0.0.1:0".into(), false).unwrap();
        for n in ["a", "b", "c"] {
            let created = api.create_object("", "nodes", None, node(n)).unwrap();
            util::write_status(&api, "", "nodes", &created, node(n)["status"].clone()).unwrap();
        }
        let owner = json!([{"apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "rs", "uid": "rs-uid", "controller": true}]);
        for i in 0..6 {
            let pod = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": format!("p{i}"), "ownerReferences": owner},
                "spec": {"containers": [{"name": "c", "image": "x"}]}});
            api.create_object("", "pods", Some("default"), pod).unwrap();
            schedule(&api, "default", &format!("p{i}")).unwrap();
        }
        let (pods, _) = api.store().list("", "pods", None);
        let mut per_node = std::collections::BTreeMap::new();
        for p in &pods {
            *per_node.entry(s(p, "/spec/nodeName").to_string()).or_insert(0) += 1;
        }
        assert_eq!(per_node.into_iter().collect::<Vec<_>>(), vec![("a".to_string(), 2), ("b".to_string(), 2), ("c".to_string(), 2)]);
    }
}

