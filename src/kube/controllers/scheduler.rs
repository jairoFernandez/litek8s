//! The scheduler: bind each pending pod to a Node (`spec.nodeName`).
//!
//! Filters: node Ready and schedulable, taints tolerated, `nodeSelector` and
//! required node affinity, cpu/memory/pod-count requests fitting allocatable,
//! and claims bound to local volumes on another node. Among the nodes left it
//! picks the least loaded by requested cpu. Claims still unbound
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

fn tolerates(spec: &Value, taint: &Value) -> bool {
    let key = s(taint, "/key");
    let value = s(taint, "/value");
    let effect = s(taint, "/effect");
    spec.get("tolerations").and_then(Value::as_array).is_some_and(|ts| {
        ts.iter().any(|t| {
            let t_effect = s(t, "/effect");
            if !t_effect.is_empty() && t_effect != effect {
                return false;
            }
            let t_key = s(t, "/key");
            match s(t, "/operator") {
                "Exists" => t_key.is_empty() || t_key == key,
                _ => t_key == key && s(t, "/value") == value,
            }
        })
    })
}

/// The toleration of `spec` matching `taint`, if any.
fn toleration_for<'a>(spec: &'a Value, taint: &Value) -> Option<&'a Value> {
    let (key, value, effect) = (s(taint, "/key"), s(taint, "/value"), s(taint, "/effect"));
    spec.get("tolerations")?.as_array()?.iter().find(|t| {
        let t_effect = s(t, "/effect");
        if !t_effect.is_empty() && t_effect != effect {
            return false;
        }
        let t_key = s(t, "/key");
        match s(t, "/operator") {
            "Exists" => t_key.is_empty() || t_key == key,
            _ => t_key == key && s(t, "/value") == value,
        }
    })
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
            let wait = match toleration_for(&spec, taint) {
                None => 0,
                Some(t) => match t.get("tolerationSeconds").and_then(Value::as_i64) {
                    None => continue,
                    Some(secs) => {
                        let added = s(taint, "/timeAdded");
                        let since = util::parse_time(added).map(|t| (now - t).num_seconds()).unwrap_or(0);
                        (secs - since).max(0)
                    }
                },
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
        if (effect == "NoSchedule" || effect == "NoExecute") && !tolerates(spec, taint) {
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
    let mut best: Option<(f64, String)> = None;
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
        if best.as_ref().is_none_or(|(l, _)| load < *l) {
            best = Some((load, node_name));
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
