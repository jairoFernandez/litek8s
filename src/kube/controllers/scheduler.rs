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

/// Requests of extended resources (anything but cpu, memory, storage and
/// hugepages), in milli-units: (name, amount).
pub(crate) fn extended_requests(spec: &Value) -> Vec<(String, i128)> {
    let mut out: Vec<(String, i128)> = Vec::new();
    for c in spec.get("containers").and_then(Value::as_array).into_iter().flatten() {
        let reqs = c.pointer("/resources/requests").or_else(|| c.pointer("/resources/limits")).and_then(Value::as_object);
        for (k, v) in reqs.into_iter().flatten() {
            if matches!(k.as_str(), "cpu" | "memory" | "ephemeral-storage") || k.starts_with("hugepages-") {
                continue;
            }
            let q = quantity::milli_of(v).unwrap_or(0);
            match out.iter_mut().find(|(n, _)| n == k) {
                Some((_, total)) => *total += q,
                None => out.push((k.clone(), q)),
            }
        }
    }
    out
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

/// NoExecute taints: evict pods on `node` that do not tolerate one (now, or
/// once their `tolerationSeconds` since the taint was added have passed).
pub(crate) fn evict_for_taints(api: &ApiState, node_name: &str) -> Result<super::Requeue, ApiError> {
    let Some(node) = api.get_object("", "nodes", "", node_name) else { return Ok(None) };
    let taints: Vec<Value> = node
        .pointer("/spec/taints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|t| s(t, "/effect") == "NoExecute")
        .cloned()
        .collect();
    if taints.is_empty() {
        return Ok(None);
    }
    let now = util::now();
    let mut requeue: Option<std::time::Duration> = None;
    let (pods, _) = api.store().list("", "pods", None);
    for pod in pods.iter().filter(|p| s(p, "/spec/nodeName") == node_name && util::is_active(p)) {
        let spec = pod.get("spec").cloned().unwrap_or_default();
        let mut evict_in: Option<i64> = None;
        for taint in &taints {
            // The shortest limit among the matching tolerations; none set:
            // tolerated forever.
            let matching: Vec<&Value> = spec.get("tolerations").and_then(Value::as_array).into_iter().flatten().filter(|t| toleration_matches(t, taint)).collect();
            let wait = if matching.is_empty() {
                0
            } else {
                // Kubernetes's getMinTolerationTime: the minimum of the limits set.
                match matching.iter().filter_map(|t| t.get("tolerationSeconds").and_then(Value::as_i64)).min() {
                    None => continue,
                    Some(secs) => {
                        let added = s(taint, "/timeAdded");
                        let since = util::parse_time(added).map(|t| (now - t).num_seconds()).unwrap_or(0);
                        (secs - since).max(0)
                    }
                }
            };
            evict_in = Some(evict_in.map_or(wait, |w| w.min(wait)));
        }
        match evict_in {
            Some(0) => {
                let mut status = pod.get("status").cloned().unwrap_or_else(|| json!({}));
                let mut conditions = util::conditions_of(pod);
                util::set_condition(&mut conditions, "DisruptionTarget", "True", "DeletionByTaintManager", "Taint manager: deleting due to NoExecute taint");
                status["conditions"] = json!(conditions);
                let _ = util::write_status(api, "", "pods", pod, status);
                util::record_event(api, pod, "Normal", "TaintManagerEviction", &format!("Marking for deletion Pod {}", util::name(pod)), "taint-eviction-controller");
                let (ns, name) = util::ns_name(pod);
                let _ = api.delete_named("", "pods", &ns, &name);
            }
            Some(secs) => {
                let d = std::time::Duration::from_secs(secs as u64);
                requeue = Some(requeue.map_or(d, |r| r.min(d)));
            }
            None => {}
        }
    }
    Ok(requeue)
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
    let want_ext = extended_requests(&spec);
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
        let alloc = |res: &str| node.pointer("/status/allocatable").and_then(|m| m.get(res)).and_then(quantity::milli_of);
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
        // Extended resources (device plugins, fake test resources): what the
        // node advertises, absent meaning none.
        let short = want_ext.iter().find(|(res, want)| {
            let used: i128 = on_node.iter().flat_map(|p| extended_requests(p.get("spec").unwrap_or(&Value::Null))).filter(|(n, _)| n == res).map(|(_, q)| q).sum();
            used + want > alloc(res).unwrap_or(0)
        });
        if let Some((res, _)) = short {
            reasons.push(format!("Insufficient {res}"));
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
        preempt(api, &pod, &nodes, &pods, pinned.as_deref())?;
        return unschedulable(api, &pod, &message);
    };

    for pvc in unbound {
        provision(api, &pvc, &node)?;
    }
    let mut bound = pod.clone();
    bound["spec"]["nodeName"] = json!(node);
    let bound = api.update_object("", "pods", bound)?;
    util::record_event(api, &bound, "Normal", "Scheduled", &format!("Successfully assigned {ns}/{name} to {node}"), "default-scheduler");
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
    let first = util::condition(pod, "PodScheduled").is_none_or(|c| s(c, "/message") != message);
    set_scheduled(api, pod.clone(), "False", "Unschedulable", message)?;
    if first {
        util::record_event(api, pod, "Warning", "FailedScheduling", message, "default-scheduler");
    }
    Ok(Outcome::Unschedulable)
}

fn priority(pod: &Value) -> i64 {
    util::i(pod, "/spec/priority").unwrap_or(0)
}

/// Preemption: on a node where evicting lower-priority pods makes room,
/// evict the fewest (lowest priority first) and nominate the node.
fn preempt(api: &ApiState, pod: &Value, nodes: &[Value], pods: &[Value], pinned: Option<&str>) -> Result<(), ApiError> {
    if s(pod, "/spec/preemptionPolicy") == "Never" || !s(pod, "/status/nominatedNodeName").is_empty() {
        return Ok(());
    }
    let spec = pod.get("spec").cloned().unwrap_or_else(|| json!({}));
    let (want_cpu, want_mem) = pod_requests(&spec);
    let mine = priority(pod);
    let mut best: Option<(String, Vec<&Value>)> = None;
    for node in nodes {
        let node_name = util::name(node);
        if pinned.is_some_and(|p| p != node_name) || node_fits_constraints(&spec, node, false).is_err() {
            continue;
        }
        let alloc = |res: &str| node.pointer("/status/allocatable").and_then(|m| m.get(res)).and_then(quantity::milli_of);
        let mut on_node: Vec<&Value> = pods.iter().filter(|p| s(p, "/spec/nodeName") == node_name && util::is_active(p)).collect();
        let want_ext = extended_requests(&spec);
        let fits = |on: &[&Value]| {
            let (cpu, mem) = on.iter().map(|p| pod_requests(p.get("spec").unwrap_or(&Value::Null))).fold((0, 0), |(a, b), (c, d)| (a + c, b + d));
            alloc("pods").is_none_or(|m| (on.len() as i128 + 1) * 1000 <= m)
                && alloc("cpu").is_none_or(|c| cpu + want_cpu <= c)
                && alloc("memory").is_none_or(|m| (mem + want_mem) * 1000 <= m)
                && want_ext.iter().all(|(res, want)| {
                    let used: i128 = on.iter().flat_map(|p| extended_requests(p.get("spec").unwrap_or(&Value::Null))).filter(|(n, _)| n == res).map(|(_, q)| q).sum();
                    used + want <= alloc(res).unwrap_or(0)
                })
        };
        on_node.sort_by_key(|p| priority(p));
        let mut victims = Vec::new();
        let mut kept = on_node.clone();
        while !fits(&kept) {
            let Some(pos) = kept.iter().position(|p| priority(p) < mine) else { break };
            victims.push(kept.remove(pos));
        }
        if fits(&kept) && !victims.is_empty() && best.as_ref().is_none_or(|(_, v)| victims.len() < v.len()) {
            best = Some((node_name, victims));
        }
    }
    let Some((node, victims)) = best else { return Ok(()) };
    for victim in victims {
        let mut status = victim.get("status").cloned().unwrap_or_else(|| json!({}));
        let mut conditions = util::conditions_of(victim);
        util::set_condition(&mut conditions, "DisruptionTarget", "True", "PreemptionByScheduler", &format!("{}: preempting to accommodate a higher priority pod", "default-scheduler"));
        status["conditions"] = json!(conditions);
        let _ = util::write_status(api, "", "pods", victim, status);
        util::record_event(api, victim, "Normal", "Preempted", &format!("Preempted by pod {}", util::uid(pod)), "default-scheduler");
        let (vns, vname) = util::ns_name(victim);
        let _ = api.delete_named("", "pods", &vns, &vname);
    }
    if let Some(current) = api.get_object("", "pods", &util::ns_name(pod).0, &util::name(pod)) {
        let mut status = current.get("status").cloned().unwrap_or_else(|| json!({}));
        status["nominatedNodeName"] = json!(node);
        let _ = util::write_status(api, "", "pods", &current, status);
    }
    Ok(())
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

    fn new_api() -> ApiState {
        ApiState::open(None, "127.0.0.1:0".into(), false).unwrap()
    }

    fn add_node(api: &ApiState, n: Value) {
        let created = api.create_object("", "nodes", None, n.clone()).unwrap();
        util::write_status(api, "", "nodes", &created, n["status"].clone()).unwrap();
    }

    fn add_pod(api: &ApiState, name: &str, spec: Value) {
        api.create_object("", "pods", Some("default"), json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name}, "spec": spec})).unwrap();
    }

    fn container(cpu: &str, mem: &str) -> Value {
        json!({"name": "c", "image": "x", "resources": {"requests": {"cpu": cpu, "memory": mem}}})
    }

    fn get_pod(api: &ApiState, name: &str) -> Option<Value> {
        api.get_object("", "pods", "default", name)
    }

    fn node_of(api: &ApiState, name: &str) -> String {
        s(&get_pod(api, name).unwrap(), "/spec/nodeName").to_string()
    }

    fn scheduled_message(api: &ApiState, name: &str) -> String {
        let pod = get_pod(api, name).unwrap();
        let c = util::condition(&pod, "PodScheduled").unwrap();
        assert_eq!(s(c, "/status"), "False");
        assert_eq!(s(c, "/reason"), "Unschedulable");
        s(c, "/message").to_string()
    }

    fn events(api: &ApiState, reason: &str) -> Vec<Value> {
        let (evs, _) = api.store().list("", "events", None);
        evs.into_iter().filter(|e| s(e, "/reason") == reason).collect()
    }

    fn evicted(api: &ApiState, name: &str) -> bool {
        get_pod(api, name).is_none_or(|p| util::is_deleting(&p))
    }

    #[test]
    fn pod_requests_sum_containers_or_take_the_largest_init_container() {
        let spec = json!({"containers": [container("100m", "64Mi"), {"name": "l", "resources": {"limits": {"cpu": "200m"}}}, {"name": "none"}]});
        assert_eq!(pod_requests(&spec), (300, 64 * 1024 * 1024));
        let spec = json!({"containers": [container("100m", "64Mi")],
            "initContainers": [container("1", "1Mi"), container("500m", "1Mi")]});
        assert_eq!(pod_requests(&spec), (1000, 64 * 1024 * 1024));
        assert_eq!(pod_requests(&json!({})), (0, 0));
    }

    #[test]
    fn extended_requests_skip_standard_resources_and_sum_per_name() {
        let spec = json!({"containers": [
            {"name": "a", "resources": {"requests": {"cpu": "1", "memory": "1Gi", "ephemeral-storage": "1Gi", "hugepages-2Mi": "2Mi", "example.com/gpu": "1"}}},
            {"name": "b", "resources": {"limits": {"example.com/gpu": "2", "example.com/fpga": "1"}}},
            {"name": "c"},
        ]});
        assert_eq!(extended_requests(&spec), vec![("example.com/gpu".to_string(), 3000), ("example.com/fpga".to_string(), 1000)]);
        assert!(extended_requests(&json!({})).is_empty());
    }

    #[test]
    fn toleration_matches_by_operator_key_value_and_effect() {
        let taint = json!({"key": "k", "value": "v", "effect": "NoSchedule"});
        assert!(toleration_matches(&json!({"operator": "Exists"}), &taint));
        assert!(toleration_matches(&json!({"key": "k", "operator": "Exists"}), &taint));
        assert!(!toleration_matches(&json!({"key": "other", "operator": "Exists"}), &taint));
        assert!(toleration_matches(&json!({"key": "k", "operator": "Equal", "value": "v"}), &taint));
        assert!(toleration_matches(&json!({"key": "k", "value": "v"}), &taint));
        assert!(!toleration_matches(&json!({"key": "k", "operator": "Equal", "value": "w"}), &taint));
        assert!(toleration_matches(&json!({"key": "k", "value": "v", "effect": "NoSchedule"}), &taint));
        assert!(!toleration_matches(&json!({"key": "k", "value": "v", "effect": "NoExecute"}), &taint));
        assert!(!toleration_matches(&json!({"operator": "Exists", "effect": "NoExecute"}), &taint));
    }

    fn labelled(labels: Value) -> Value {
        let mut n = node("n1");
        n["metadata"]["labels"] = labels;
        n
    }

    fn affinity(term: Value) -> Value {
        json!({"affinity": {"nodeAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": {"nodeSelectorTerms": [term]}}}})
    }

    #[test]
    fn required_node_affinity_operators() {
        let n = labelled(json!({"zone": "a", "cores": "8"}));
        let ok = |expr: Value| node_fits_constraints(&affinity(json!({"matchExpressions": [expr]})), &n, false).is_ok();
        assert!(ok(json!({"key": "zone", "operator": "In", "values": ["a", "b"]})));
        assert!(!ok(json!({"key": "zone", "operator": "In", "values": ["b"]})));
        assert!(!ok(json!({"key": "missing", "operator": "In", "values": ["a"]})));
        assert!(ok(json!({"key": "zone", "operator": "NotIn", "values": ["b"]})));
        assert!(!ok(json!({"key": "zone", "operator": "NotIn", "values": ["a"]})));
        assert!(ok(json!({"key": "missing", "operator": "NotIn", "values": ["a"]})));
        assert!(ok(json!({"key": "zone", "operator": "Exists"})));
        assert!(!ok(json!({"key": "missing", "operator": "Exists"})));
        assert!(ok(json!({"key": "missing", "operator": "DoesNotExist"})));
        assert!(!ok(json!({"key": "zone", "operator": "DoesNotExist"})));
        assert!(ok(json!({"key": "cores", "operator": "Gt", "values": ["4"]})));
        assert!(!ok(json!({"key": "cores", "operator": "Gt", "values": ["8"]})));
        assert!(!ok(json!({"key": "zone", "operator": "Gt", "values": ["1"]})));
        assert!(ok(json!({"key": "cores", "operator": "Lt", "values": ["16"]})));
        assert!(!ok(json!({"key": "cores", "operator": "Lt", "values": ["8"]})));
        assert!(!ok(json!({"key": "zone", "operator": "Bogus", "values": ["a"]})));
    }

    #[test]
    fn node_affinity_terms_are_ored_and_match_fields_by_name() {
        let n = labelled(json!({"zone": "a"}));
        let fields = |op: &str, name: &str| affinity(json!({"matchFields": [{"key": "metadata.name", "operator": op, "values": [name]}]}));
        assert!(node_fits_constraints(&fields("In", "n1"), &n, false).is_ok());
        assert!(node_fits_constraints(&fields("In", "n2"), &n, false).is_err());
        assert!(node_fits_constraints(&fields("NotIn", "n2"), &n, false).is_ok());
        let other_key = affinity(json!({"matchFields": [{"key": "metadata.uid", "operator": "In", "values": ["n1"]}]}));
        assert!(node_fits_constraints(&other_key, &n, false).is_err());
        let both = affinity(json!({"matchExpressions": [{"key": "zone", "operator": "In", "values": ["a"]}],
            "matchFields": [{"key": "metadata.name", "operator": "In", "values": ["n2"]}]}));
        assert!(node_fits_constraints(&both, &n, false).is_err());
        assert!(node_fits_constraints(&affinity(json!({})), &n, false).is_err());
        let ored = json!({"affinity": {"nodeAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": {"nodeSelectorTerms": [
            {"matchExpressions": [{"key": "zone", "operator": "In", "values": ["b"]}]},
            {"matchExpressions": [{"key": "zone", "operator": "In", "values": ["a"]}]},
        ]}}}});
        assert!(node_fits_constraints(&ored, &n, false).is_ok());
    }

    #[test]
    fn node_fits_constraints_checks_readiness_cordon_taints_and_selector() {
        let mut n = labelled(json!({"disk": "ssd"}));
        assert!(node_fits_constraints(&json!({"nodeSelector": {"disk": "ssd"}}), &n, false).is_ok());
        assert_eq!(node_fits_constraints(&json!({"nodeSelector": {"disk": "hdd"}}), &n, false).unwrap_err(), "node didn't match Pod's node affinity/selector");
        n["spec"] = json!({"unschedulable": true});
        assert_eq!(node_fits_constraints(&json!({}), &n, false).unwrap_err(), "node is unschedulable");
        assert!(node_fits_constraints(&json!({}), &n, true).is_ok());
        n["status"]["conditions"] = json!([{"type": "Ready", "status": "False"}]);
        assert_eq!(node_fits_constraints(&json!({}), &n, false).unwrap_err(), "node is not ready");
        assert!(node_fits_constraints(&json!({}), &n, true).is_ok());
        n["spec"] = json!({"taints": [{"key": "dedicated", "value": "db", "effect": "NoSchedule"}, {"key": "soft", "effect": "PreferNoSchedule"}]});
        assert_eq!(node_fits_constraints(&json!({}), &n, true).unwrap_err(), "node had untolerated taint {dedicated: db}");
        let tolerating = json!({"tolerations": [{"key": "dedicated", "operator": "Equal", "value": "db", "effect": "NoSchedule"}]});
        assert!(node_fits_constraints(&tolerating, &n, true).is_ok());
    }

    #[test]
    fn running_daemon_pods_stay_unless_a_no_execute_taint_is_untolerated() {
        let mut n = node("n1");
        n["spec"] = json!({"unschedulable": true, "taints": [{"key": "node.kubernetes.io/unreachable", "effect": "NoSchedule"}]});
        n["status"]["conditions"] = json!([{"type": "Ready", "status": "Unknown"}]);
        assert!(node_keeps_daemon_pod(&json!({}), &n));
        assert!(node_fits_constraints(&json!({}), &n, true).is_err());
        n["spec"]["taints"] = json!([{"key": "node.kubernetes.io/unreachable", "effect": "NoExecute"}]);
        assert!(!node_keeps_daemon_pod(&json!({}), &n));
        assert!(node_keeps_daemon_pod(&json!({"tolerations": [{"key": "node.kubernetes.io/unreachable", "operator": "Exists"}]}), &n));
    }

    #[test]
    fn schedule_skips_missing_and_bound_pods() {
        let api = new_api();
        add_node(&api, node("a"));
        assert!(matches!(schedule(&api, "default", "nope").unwrap(), Outcome::Done));
        add_pod(&api, "bound", json!({"nodeName": "elsewhere", "containers": [{"name": "c", "image": "x"}]}));
        assert!(matches!(schedule(&api, "default", "bound").unwrap(), Outcome::Done));
        assert_eq!(node_of(&api, "bound"), "elsewhere");
        assert!(events(&api, "Scheduled").is_empty());
    }

    #[test]
    fn schedule_binds_and_records_the_scheduled_condition_and_event() {
        let api = new_api();
        add_node(&api, node("a"));
        add_pod(&api, "p", json!({"containers": [{"name": "c", "image": "x"}]}));
        assert!(matches!(schedule(&api, "default", "p").unwrap(), Outcome::Done));
        assert_eq!(node_of(&api, "p"), "a");
        let pod = get_pod(&api, "p").unwrap();
        assert!(util::condition_true(&pod, "PodScheduled"));
        let evs = events(&api, "Scheduled");
        assert_eq!(evs.len(), 1);
        assert_eq!(s(&evs[0], "/message"), "Successfully assigned default/p to a");
    }

    #[test]
    fn schedule_filters_unready_cordoned_tainted_and_unselected_nodes() {
        let api = new_api();
        let mut unready = node("unready");
        unready["status"]["conditions"] = json!([{"type": "Ready", "status": "False"}]);
        add_node(&api, unready);
        let mut cordoned = node("cordoned");
        cordoned["spec"] = json!({"unschedulable": true});
        add_node(&api, cordoned);
        let mut tainted = node("tainted");
        tainted["spec"] = json!({"taints": [{"key": "gpu", "value": "true", "effect": "NoSchedule"}]});
        tainted["metadata"]["labels"] = json!({"kind": "gpu"});
        add_node(&api, tainted);
        let mut plain = node("plain");
        plain["metadata"]["labels"] = json!({"kind": "cpu"});
        add_node(&api, plain);

        add_pod(&api, "any", json!({"containers": [{"name": "c", "image": "x"}]}));
        schedule(&api, "default", "any").unwrap();
        assert_eq!(node_of(&api, "any"), "plain");

        add_pod(&api, "gpu", json!({"nodeSelector": {"kind": "gpu"}, "containers": [{"name": "c", "image": "x"}]}));
        assert!(matches!(schedule(&api, "default", "gpu").unwrap(), Outcome::Unschedulable));
        assert_eq!(
            scheduled_message(&api, "gpu"),
            "0/4 nodes are available: node didn't match Pod's node affinity/selector, node had untolerated taint {gpu: true}, node is not ready, node is unschedulable."
        );

        add_pod(&api, "gpu-ok", json!({"nodeSelector": {"kind": "gpu"}, "tolerations": [{"key": "gpu", "operator": "Exists", "effect": "NoSchedule"}],
            "containers": [{"name": "c", "image": "x"}]}));
        schedule(&api, "default", "gpu-ok").unwrap();
        assert_eq!(node_of(&api, "gpu-ok"), "tainted");
    }

    #[test]
    fn unschedulable_records_one_event_per_message() {
        let api = new_api();
        add_node(&api, node("a"));
        add_pod(&api, "big", json!({"containers": [container("4", "1Mi")]}));
        assert!(matches!(schedule(&api, "default", "big").unwrap(), Outcome::Unschedulable));
        assert!(matches!(schedule(&api, "default", "big").unwrap(), Outcome::Unschedulable));
        assert_eq!(scheduled_message(&api, "big"), "0/1 nodes are available: Insufficient cpu.");
        let evs = events(&api, "FailedScheduling");
        assert_eq!(evs.len(), 1);
        assert_eq!(s(&evs[0], "/type"), "Warning");
        assert_eq!(node_of(&api, "big"), "");
    }

    #[test]
    fn schedule_checks_cpu_memory_pods_and_extended_resources() {
        let api = new_api();
        let mut small = node("small");
        small["status"]["allocatable"] = json!({"cpu": "1", "memory": "256Mi", "pods": "2", "example.com/gpu": "1"});
        add_node(&api, small);
        add_pod(&api, "running", json!({"nodeName": "small", "containers": [container("500m", "128Mi")]}));

        add_pod(&api, "cpu", json!({"containers": [container("600m", "1Mi")]}));
        schedule(&api, "default", "cpu").unwrap();
        assert_eq!(scheduled_message(&api, "cpu"), "0/1 nodes are available: Insufficient cpu.");

        add_pod(&api, "mem", json!({"containers": [container("100m", "200Mi")]}));
        schedule(&api, "default", "mem").unwrap();
        assert_eq!(scheduled_message(&api, "mem"), "0/1 nodes are available: Insufficient memory.");

        add_pod(&api, "init", json!({"initContainers": [container("800m", "1Mi")], "containers": [container("100m", "1Mi")]}));
        schedule(&api, "default", "init").unwrap();
        assert_eq!(scheduled_message(&api, "init"), "0/1 nodes are available: Insufficient cpu.");

        let gpu = |n: &str| json!({"name": "c", "image": "x", "resources": {"limits": {"example.com/gpu": n}}});
        add_pod(&api, "gpu2", json!({"containers": [gpu("2")]}));
        schedule(&api, "default", "gpu2").unwrap();
        assert_eq!(scheduled_message(&api, "gpu2"), "0/1 nodes are available: Insufficient example.com/gpu.");
        add_pod(&api, "fpga", json!({"containers": [{"name": "c", "image": "x", "resources": {"limits": {"example.com/fpga": "1"}}}]}));
        schedule(&api, "default", "fpga").unwrap();
        assert_eq!(scheduled_message(&api, "fpga"), "0/1 nodes are available: Insufficient example.com/fpga.");

        add_pod(&api, "gpu1", json!({"containers": [gpu("1")]}));
        schedule(&api, "default", "gpu1").unwrap();
        assert_eq!(node_of(&api, "gpu1"), "small");

        add_pod(&api, "third", json!({"containers": [{"name": "c", "image": "x"}]}));
        schedule(&api, "default", "third").unwrap();
        assert_eq!(scheduled_message(&api, "third"), "0/1 nodes are available: Too many pods.");
    }

    #[test]
    fn finished_pods_do_not_count_against_a_node() {
        let api = new_api();
        let mut n = node("a");
        n["status"]["allocatable"]["cpu"] = json!("1");
        add_node(&api, n);
        add_pod(&api, "done", json!({"nodeName": "a", "containers": [container("1", "1Mi")]}));
        let done = get_pod(&api, "done").unwrap();
        util::write_status(&api, "", "pods", &done, json!({"phase": "Succeeded"})).unwrap();
        add_pod(&api, "p", json!({"containers": [container("1", "1Mi")]}));
        schedule(&api, "default", "p").unwrap();
        assert_eq!(node_of(&api, "p"), "a");
    }

    #[test]
    fn picks_the_least_loaded_node_then_the_one_with_fewer_pods() {
        let api = new_api();
        for n in ["a", "b"] {
            add_node(&api, node(n));
        }
        add_pod(&api, "heavy", json!({"nodeName": "a", "containers": [container("1", "1Mi")]}));
        add_pod(&api, "light1", json!({"nodeName": "b", "containers": [container("100m", "1Mi")]}));
        add_pod(&api, "light2", json!({"nodeName": "b", "containers": [container("100m", "1Mi")]}));
        add_pod(&api, "p", json!({"containers": [container("100m", "1Mi")]}));
        schedule(&api, "default", "p").unwrap();
        assert_eq!(node_of(&api, "p"), "b");

        let api = new_api();
        for n in ["a", "b"] {
            add_node(&api, node(n));
        }
        add_pod(&api, "x1", json!({"nodeName": "a", "containers": [{"name": "c", "image": "x"}]}));
        add_pod(&api, "x2", json!({"nodeName": "a", "containers": [{"name": "c", "image": "x"}]}));
        add_pod(&api, "y1", json!({"nodeName": "b", "containers": [{"name": "c", "image": "x"}]}));
        add_pod(&api, "p", json!({"containers": [{"name": "c", "image": "x"}]}));
        schedule(&api, "default", "p").unwrap();
        assert_eq!(node_of(&api, "p"), "b");
    }

    #[test]
    fn missing_claim_keeps_the_pod_pending() {
        let api = new_api();
        add_node(&api, node("a"));
        add_pod(&api, "p", json!({"containers": [{"name": "c", "image": "x"}],
            "volumes": [{"name": "empty", "emptyDir": {}}, {"name": "data", "persistentVolumeClaim": {"claimName": "data"}}]}));
        assert!(matches!(schedule(&api, "default", "p").unwrap(), Outcome::Unschedulable));
        assert_eq!(scheduled_message(&api, "p"), "persistentvolumeclaim \"data\" not found");
    }

    fn pvc(name: &str, class: &str, volume: &str) -> Value {
        let mut spec = json!({"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "2Gi"}}});
        if !class.is_empty() {
            spec["storageClassName"] = json!(class);
        }
        if !volume.is_empty() {
            spec["volumeName"] = json!(volume);
        }
        json!({"apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": {"name": name}, "spec": spec})
    }

    fn claim_pod(claim: &str) -> Value {
        json!({"containers": [{"name": "c", "image": "x"}], "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": claim}}]})
    }

    #[test]
    fn a_claim_bound_to_a_local_volume_pins_the_node() {
        let api = new_api();
        for n in ["a", "b"] {
            add_node(&api, node(n));
        }
        let pv = json!({"apiVersion": "v1", "kind": "PersistentVolume", "metadata": {"name": "pv-b"},
            "spec": {"capacity": {"storage": "2Gi"}, "accessModes": ["ReadWriteOnce"], "local": {"path": "/x"},
                "nodeAffinity": {"required": {"nodeSelectorTerms": [{"matchExpressions": [
                    {"key": "zone", "operator": "In", "values": ["z"]},
                    {"key": "kubernetes.io/hostname", "operator": "In", "values": ["b"]},
                ]}]}}}});
        api.create_object("", "persistentvolumes", None, pv).unwrap();
        api.create_object("", "persistentvolumeclaims", Some("default"), pvc("data", "", "pv-b")).unwrap();
        add_pod(&api, "p", claim_pod("data"));
        schedule(&api, "default", "p").unwrap();
        assert_eq!(node_of(&api, "p"), "b");

        let mut cordoned = api.get_object("", "nodes", "", "b").unwrap();
        cordoned["spec"] = json!({"unschedulable": true});
        api.update_object("", "nodes", cordoned).unwrap();
        add_pod(&api, "q", claim_pod("data"));
        schedule(&api, "default", "q").unwrap();
        assert_eq!(scheduled_message(&api, "q"), "0/2 nodes are available: node is unschedulable, node(s) had volume node affinity conflict.");
    }

    #[test]
    fn a_claim_bound_to_a_volume_without_node_affinity_does_not_pin() {
        let api = new_api();
        add_node(&api, node("a"));
        let pv = json!({"apiVersion": "v1", "kind": "PersistentVolume", "metadata": {"name": "nfs"},
            "spec": {"capacity": {"storage": "2Gi"}, "accessModes": ["ReadWriteMany"], "nfs": {"server": "s", "path": "/"}}});
        api.create_object("", "persistentvolumes", None, pv).unwrap();
        api.create_object("", "persistentvolumeclaims", Some("default"), pvc("data", "", "nfs")).unwrap();
        add_pod(&api, "p", claim_pod("data"));
        schedule(&api, "default", "p").unwrap();
        assert_eq!(node_of(&api, "p"), "a");
    }

    #[test]
    fn wait_for_first_consumer_claims_are_provisioned_on_the_chosen_node() {
        let api = new_api();
        let mut n = node("a");
        n["status"]["conditions"] = json!([{"type": "Ready", "status": "False"}]);
        add_node(&api, n);
        add_node(&api, node("b"));
        let sc = json!({"apiVersion": "storage.k8s.io/v1", "kind": "StorageClass", "metadata": {"name": "local"},
            "provisioner": "litek8s.dev/local-path", "reclaimPolicy": "Retain", "volumeBindingMode": "WaitForFirstConsumer"});
        api.create_object("storage.k8s.io", "storageclasses", None, sc).unwrap();
        api.create_object("", "persistentvolumeclaims", Some("default"), pvc("data", "local", "")).unwrap();
        add_pod(&api, "p", claim_pod("data"));
        schedule(&api, "default", "p").unwrap();
        assert_eq!(node_of(&api, "p"), "b");

        let claim = api.get_object("", "persistentvolumeclaims", "default", "data").unwrap();
        let pv_name = format!("pvc-{}", util::uid(&claim));
        assert_eq!(s(&claim, "/spec/volumeName"), pv_name);
        assert_eq!(s(&claim, "/status/phase"), "Bound");
        assert_eq!(s(&claim, "/status/capacity/storage"), "2Gi");
        assert_eq!(claim.pointer("/metadata/annotations/volume.kubernetes.io~1selected-node"), Some(&json!("b")));
        let pv = api.get_object("", "persistentvolumes", "", &pv_name).unwrap();
        assert_eq!(volume_node(&pv).as_deref(), Some("b"));
        assert_eq!(s(&pv, "/spec/persistentVolumeReclaimPolicy"), "Retain");
        assert_eq!(s(&pv, "/spec/claimRef/name"), "data");
        assert_eq!(s(&pv, "/spec/volumeMode"), "Filesystem");
        assert_eq!(s(&pv, "/status/phase"), "Bound");

        provision(&api, &claim, "a").unwrap();
        assert_eq!(api.store().list("", "persistentvolumes", None).0.len(), 1);
        assert_eq!(volume_node(&api.get_object("", "persistentvolumes", "", &pv_name).unwrap()).as_deref(), Some("b"));
    }

    #[test]
    fn claims_without_a_known_storage_class_wait_for_a_static_volume() {
        let api = new_api();
        add_node(&api, node("a"));
        let mut none = pvc("none", "", "");
        none["spec"]["storageClassName"] = json!("");
        api.create_object("", "persistentvolumeclaims", Some("default"), none).unwrap();
        api.create_object("", "persistentvolumeclaims", Some("default"), pvc("unknown", "missing", "")).unwrap();
        add_pod(&api, "p", json!({"containers": [{"name": "c", "image": "x"}], "volumes": [
            {"name": "a", "persistentVolumeClaim": {"claimName": "none"}},
            {"name": "b", "persistentVolumeClaim": {"claimName": "unknown"}},
        ]}));
        schedule(&api, "default", "p").unwrap();
        assert_eq!(node_of(&api, "p"), "a");
        for c in ["none", "unknown"] {
            assert_eq!(s(&api.get_object("", "persistentvolumeclaims", "default", c).unwrap(), "/spec/volumeName"), "");
        }
        assert!(api.store().list("", "persistentvolumes", None).0.is_empty());
    }

    #[test]
    fn a_higher_priority_pod_preempts_the_lowest_priority_pods() {
        let api = new_api();
        let mut n = node("a");
        n["status"]["allocatable"]["cpu"] = json!("1");
        add_node(&api, n);
        add_pod(&api, "low", json!({"nodeName": "a", "priority": 1, "containers": [container("500m", "1Mi")]}));
        add_pod(&api, "lower", json!({"nodeName": "a", "priority": 0, "containers": [container("500m", "1Mi")]}));
        add_pod(&api, "high", json!({"priority": 10, "containers": [container("500m", "1Mi")]}));
        assert!(matches!(schedule(&api, "default", "high").unwrap(), Outcome::Unschedulable));
        assert!(evicted(&api, "lower"));
        assert!(!evicted(&api, "low"));
        let lower = get_pod(&api, "lower").unwrap();
        assert_eq!(s(util::condition(&lower, "DisruptionTarget").unwrap(), "/reason"), "PreemptionByScheduler");
        assert_eq!(events(&api, "Preempted").len(), 1);
        assert_eq!(s(&get_pod(&api, "high").unwrap(), "/status/nominatedNodeName"), "a");
        schedule(&api, "default", "high").unwrap();
        assert!(!evicted(&api, "low"));
        assert_eq!(events(&api, "Preempted").len(), 1);
    }

    #[test]
    fn no_preemption_when_never_or_nothing_lower_frees_enough() {
        let api = new_api();
        let mut n = node("a");
        n["status"]["allocatable"]["cpu"] = json!("1");
        add_node(&api, n);
        add_pod(&api, "low", json!({"nodeName": "a", "priority": 1, "containers": [container("600m", "1Mi")]}));
        add_pod(&api, "peer", json!({"nodeName": "a", "priority": 10, "containers": [container("400m", "1Mi")]}));
        add_pod(&api, "never", json!({"priority": 10, "preemptionPolicy": "Never", "containers": [container("500m", "1Mi")]}));
        schedule(&api, "default", "never").unwrap();
        assert!(!evicted(&api, "low"));
        add_pod(&api, "huge", json!({"priority": 10, "containers": [container("700m", "1Mi")]}));
        schedule(&api, "default", "huge").unwrap();
        assert!(!evicted(&api, "low"));
        assert_eq!(s(&get_pod(&api, "huge").unwrap(), "/status/nominatedNodeName"), "");
        assert!(events(&api, "Preempted").is_empty());
    }

    fn taint_node(api: &ApiState, taints: Value) {
        let mut n = node("a");
        n["spec"] = json!({"taints": taints});
        add_node(api, n);
    }

    #[test]
    fn evict_for_taints_ignores_missing_nodes_and_other_effects() {
        let api = new_api();
        assert_eq!(evict_for_taints(&api, "nope").unwrap(), None);
        taint_node(&api, json!([{"key": "k", "effect": "NoSchedule"}]));
        add_pod(&api, "p", json!({"nodeName": "a", "containers": [{"name": "c", "image": "x"}]}));
        assert_eq!(evict_for_taints(&api, "a").unwrap(), None);
        assert!(!evicted(&api, "p"));
    }

    #[test]
    fn no_execute_taints_evict_untolerated_pods_now() {
        let api = new_api();
        taint_node(&api, json!([{"key": "k", "value": "v", "effect": "NoExecute"}]));
        add_pod(&api, "plain", json!({"nodeName": "a", "containers": [{"name": "c", "image": "x"}]}));
        add_pod(&api, "forever", json!({"nodeName": "a", "tolerations": [{"key": "k", "operator": "Exists"}], "containers": [{"name": "c", "image": "x"}]}));
        add_pod(&api, "elsewhere", json!({"nodeName": "b", "containers": [{"name": "c", "image": "x"}]}));
        assert_eq!(evict_for_taints(&api, "a").unwrap(), None);
        assert!(evicted(&api, "plain"));
        let plain = get_pod(&api, "plain").unwrap();
        assert_eq!(s(util::condition(&plain, "DisruptionTarget").unwrap(), "/reason"), "DeletionByTaintManager");
        assert_eq!(events(&api, "TaintManagerEviction").len(), 1);
        assert!(!evicted(&api, "forever"));
        assert!(!evicted(&api, "elsewhere"));
    }

    #[test]
    fn toleration_seconds_delay_eviction_from_when_the_taint_was_added() {
        let api = new_api();
        let long_ago = util::rfc3339(util::now() - chrono::Duration::seconds(600));
        let just_now = util::rfc3339(util::now());
        taint_node(&api, json!([
            {"key": "old", "effect": "NoExecute", "timeAdded": long_ago},
            {"key": "new", "effect": "NoExecute", "timeAdded": just_now},
        ]));
        let tol = |key: &str, secs: i64| json!({"key": key, "operator": "Exists", "effect": "NoExecute", "tolerationSeconds": secs});
        add_pod(&api, "expired", json!({"nodeName": "a", "tolerations": [tol("old", 300), tol("new", 3600)], "containers": [{"name": "c", "image": "x"}]}));
        add_pod(&api, "waiting", json!({"nodeName": "a", "tolerations": [tol("old", 3600), tol("new", 300), {"key": "new", "operator": "Exists"}],
            "containers": [{"name": "c", "image": "x"}]}));
        add_pod(&api, "later", json!({"nodeName": "a", "tolerations": [tol("old", 7200), {"key": "new", "operator": "Exists"}],
            "containers": [{"name": "c", "image": "x"}]}));
        let requeue = evict_for_taints(&api, "a").unwrap().expect("a requeue for the waiting pod");
        assert!(evicted(&api, "expired"));
        assert!(!evicted(&api, "waiting"));
        assert!(!evicted(&api, "later"));
        assert!(requeue <= std::time::Duration::from_secs(300) && requeue >= std::time::Duration::from_secs(298), "{requeue:?}");
    }
}
