//! Node health: what Kubernetes's node lifecycle controller and taint
//! manager do.
//!
//! - A node whose Lease (renewed by its agent's heartbeats) has not changed
//!   for the grace period (`--node-monitor-grace-period`, or the node's own
//!   `litek8s.dev/node-monitor-grace-period` when longer: a laptop that
//!   sleeps) gets its conditions set to Unknown, the
//!   `node.kubernetes.io/unreachable` NoSchedule and NoExecute taints, and
//!   its pods' Ready condition set to False (so Services stop routing there).
//!   Time is measured from when this controller last saw the Lease change,
//!   on its own clock, so neither clock skew nor an API server restart
//!   expires anyone.
//! - Pods on a node with NoExecute taints are evicted (deleted) once their
//!   tolerations run out: at once without a matching toleration, after
//!   `tolerationSeconds` from the taint's `timeAdded` with one, never with
//!   one that has no limit. Every pod gets 300 s for not-ready/unreachable by
//!   default (DefaultTolerationSeconds), so a node that comes back within the
//!   grace period plus that keeps its pods.
//! - When every node is unhealthy at once (the control plane lost its
//!   network, not the nodes), nothing is evicted, like Kubernetes's full
//!   disruption mode.
//! - A node that heartbeats again is set Ready by the agent hub; then its
//!   taints are removed here.
//! - Pods bound to a node that was deleted are deleted (pod GC).

use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use super::Requeue;
use super::scheduler::toleration_matches;
use super::util::{self, s};
use crate::kube::server::{ApiError, ApiState};

pub(crate) const TAINT_UNREACHABLE: &str = "node.kubernetes.io/unreachable";
pub(crate) const TAINT_NOT_READY: &str = "node.kubernetes.io/not-ready";

/// Heartbeat grace and the default eviction delays.
#[derive(Debug, Clone)]
pub struct NodeLifecycle {
    /// A node whose Lease has not been renewed for this long is Unknown.
    pub grace: Duration,
    /// tolerationSeconds added to every pod for the not-ready taint.
    pub not_ready_toleration_secs: u64,
    /// tolerationSeconds added to every pod for the unreachable taint.
    pub unreachable_toleration_secs: u64,
}

impl Default for NodeLifecycle {
    fn default() -> Self {
        Self { grace: Duration::from_secs(40), not_ready_toleration_secs: 300, unreachable_toleration_secs: 300 }
    }
}

/// What the controller remembers about a node's heartbeats.
#[derive(Debug, Clone)]
pub(crate) struct Health {
    pub renew_time: String,
    pub seen: Instant,
}

fn grace_of(api: &ApiState, node: &Value) -> Duration {
    let own = node
        .pointer("/metadata/annotations")
        .and_then(|a| a.get(crate::kube::agent::ANNOTATION_GRACE_PERIOD))
        .and_then(Value::as_str)
        .and_then(|v| v.trim_end_matches('s').parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_default();
    api.lifecycle.grace.max(own)
}

fn ready_status(node: &Value) -> String {
    util::condition(node, "Ready").map(|c| s(c, "/status").to_string()).unwrap_or_default()
}

fn healthy(node: &Value) -> bool {
    ready_status(node) == "True"
}

fn pods_on(api: &ApiState, node: &str) -> Vec<Value> {
    let (pods, _) = api.store().list("", "pods", None);
    pods.into_iter().filter(|p| s(p, "/spec/nodeName") == node).collect()
}

/// The taints the node should carry for its Ready condition (only ours are
/// touched; `timeAdded` is kept while a taint stays).
fn wanted_taints(node: &Value, evict: bool) -> Vec<Value> {
    let key = match ready_status(node).as_str() {
        "True" => return Vec::new(),
        "False" => TAINT_NOT_READY,
        // Unknown, or never reported: not reachable.
        _ => TAINT_UNREACHABLE,
    };
    let existing = node.pointer("/spec/taints").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = vec![json!({"key": key, "effect": "NoSchedule"})];
    if evict {
        let added = existing
            .iter()
            .find(|t| s(t, "/key") == key && s(t, "/effect") == "NoExecute")
            .and_then(|t| t.get("timeAdded").cloned())
            .unwrap_or_else(|| json!(util::rfc3339(util::now())));
        out.push(json!({"key": key, "effect": "NoExecute", "timeAdded": added}));
    }
    out
}

/// Reconcile one node. `health` is the controller's memory of its Lease
/// (`None`: the node is gone). Returns when to look again.
pub(crate) fn reconcile(api: &ApiState, name: &str, health: Option<&Health>) -> Result<Requeue, ApiError> {
    let Some(node) = api.get_object("", "nodes", "", name) else {
        for pod in pods_on(api, name) {
            let (ns, pod_name) = util::ns_name(&pod);
            eprintln!("controllers: deleting pod {ns}/{pod_name} of deleted node {name}");
            match api.delete_named("", "pods", &ns, &pod_name) {
                Err(e) if !e.is_not_found() => return Err(e),
                _ => {}
            }
        }
        return Ok(None);
    };
    let mut requeue: Requeue = None;
    let mut node = node;

    // 1. Heartbeats: expire a silent node.
    if let Some(health) = health {
        let grace = grace_of(api, &node);
        let silent = health.seen.elapsed();
        if silent >= grace && ready_status(&node) != "Unknown" && !util::condition(&node, "Ready").is_none() {
            eprintln!("controllers: node {name} stopped heartbeating {}s ago: marking it Unknown", silent.as_secs());
            let mut status = node.get("status").cloned().unwrap_or_else(|| json!({}));
            let mut conditions = util::conditions_of(&node);
            for c in conditions.iter_mut() {
                if s(c, "/status") != "Unknown" {
                    c["status"] = json!("Unknown");
                    c["reason"] = json!("NodeStatusUnknown");
                    c["message"] = json!("Kubelet stopped posting node status.");
                    c["lastTransitionTime"] = json!(util::rfc3339(util::now()));
                }
            }
            status["conditions"] = json!(conditions);
            util::write_status(api, "", "nodes", &node, status)?;
            node = api.get_object("", "nodes", "", name).unwrap_or(node);
            mark_pods_not_ready(api, name)?;
        } else if silent < grace {
            requeue = Some(grace - silent + Duration::from_millis(500));
        }
    }

    // 2. Taints for the Ready condition. NoExecute only while some node is
    // healthy (no evictions when the whole cluster looks down).
    let (nodes, _) = api.store().list("", "nodes", None);
    let any_healthy = nodes.iter().any(healthy);
    let wanted = wanted_taints(&node, any_healthy);
    let current = node.pointer("/spec/taints").and_then(Value::as_array).cloned().unwrap_or_default();
    let ours = |t: &Value| matches!(s(t, "/key"), TAINT_UNREACHABLE | TAINT_NOT_READY);
    let mut taints: Vec<Value> = current.iter().filter(|t| !ours(t)).cloned().collect();
    taints.extend(wanted.iter().cloned());
    let same = taints.len() == current.len() && taints.iter().all(|t| current.contains(t));
    if !same {
        let mut updated = node.clone();
        if taints.is_empty() {
            if let Some(spec) = updated.get_mut("spec").and_then(Value::as_object_mut) {
                spec.remove("taints");
            }
        } else {
            updated["spec"]["taints"] = json!(taints);
        }
        node = api.update_object("", "nodes", updated)?;
    }

    // 3. Evict pods whose tolerations ran out.
    let no_execute: Vec<Value> = taints.iter().filter(|t| s(t, "/effect") == "NoExecute").cloned().collect();
    if !no_execute.is_empty() {
        let now = util::now();
        for pod in pods_on(api, name) {
            if !util::is_active(&pod) {
                continue;
            }
            let Some(at) = eviction_time(&pod, &no_execute) else { continue };
            let (ns, pod_name) = util::ns_name(&pod);
            if at <= now {
                eprintln!("controllers: evicting pod {ns}/{pod_name} from node {name} (taint {})", s(&no_execute[0], "/key"));
                match api.delete_named("", "pods", &ns, &pod_name) {
                    Err(e) if !e.is_not_found() => return Err(e),
                    _ => {}
                }
            } else {
                let wait = (at - now).to_std().unwrap_or_default() + Duration::from_millis(500);
                requeue = Some(requeue.map_or(wait, |r| r.min(wait)));
            }
        }
    }
    let _ = node;
    Ok(requeue)
}

/// When a pod must leave a node with these NoExecute taints: now when one
/// is not tolerated, `None` when all are tolerated forever.
fn eviction_time(pod: &Value, taints: &[Value]) -> Option<chrono::DateTime<chrono::Utc>> {
    let tolerations = pod.pointer("/spec/tolerations").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut earliest: Option<chrono::DateTime<chrono::Utc>> = None;
    for taint in taints {
        let matching: Vec<&Value> = tolerations.iter().filter(|t| toleration_matches(t, taint)).collect();
        if matching.is_empty() {
            return Some(util::now());
        }
        // The shortest limit of the matching tolerations; none: forever.
        let Some(secs) = matching.iter().filter_map(|t| t.get("tolerationSeconds").and_then(Value::as_i64)).min() else { continue };
        let added = taint.get("timeAdded").and_then(Value::as_str).and_then(util::parse_time).unwrap_or_else(util::now);
        let at = added + chrono::Duration::seconds(secs.max(0));
        earliest = Some(earliest.map_or(at, |e| e.min(at)));
    }
    earliest
}

/// Set Ready=False on the node's pods, so Endpoints drop them.
fn mark_pods_not_ready(api: &ApiState, node: &str) -> Result<(), ApiError> {
    for pod in pods_on(api, node) {
        if !util::condition_true(&pod, "Ready") {
            continue;
        }
        let mut status = pod.get("status").cloned().unwrap_or_else(|| json!({}));
        let mut conditions = util::conditions_of(&pod);
        util::set_condition(&mut conditions, "Ready", "False", "NodeNotReady", "the node of this pod stopped heartbeating");
        for c in conditions.iter_mut() {
            if let Some(m) = c.as_object_mut() {
                m.remove("lastUpdateTime");
            }
        }
        status["conditions"] = json!(conditions);
        match util::write_status(api, "", "pods", &pod, status) {
            Err(e) if !e.is_not_found() && !e.is_conflict() => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn taint(key: &str, added: &str) -> Value {
        json!({"key": key, "effect": "NoExecute", "timeAdded": added})
    }

    #[test]
    fn eviction_follows_toleration_seconds() {
        let added = "2026-01-01T00:00:00Z";
        let t0 = util::parse_time(added).unwrap();
        let defaults = json!({"spec": {"tolerations": [
            {"key": TAINT_NOT_READY, "operator": "Exists", "effect": "NoExecute", "tolerationSeconds": 300},
            {"key": TAINT_UNREACHABLE, "operator": "Exists", "effect": "NoExecute", "tolerationSeconds": 300},
        ]}});
        assert_eq!(eviction_time(&defaults, &[taint(TAINT_UNREACHABLE, added)]), Some(t0 + chrono::Duration::seconds(300)));
        let forever = json!({"spec": {"tolerations": [{"operator": "Exists"}]}});
        assert_eq!(eviction_time(&forever, &[taint(TAINT_UNREACHABLE, added)]), None);
        let none = json!({"spec": {}});
        assert!(eviction_time(&none, &[taint(TAINT_UNREACHABLE, added)]).unwrap() > t0);
        let short = json!({"spec": {"tolerations": [
            {"key": TAINT_UNREACHABLE, "operator": "Exists", "effect": "NoExecute", "tolerationSeconds": 300},
            {"key": TAINT_UNREACHABLE, "operator": "Exists", "tolerationSeconds": 10},
        ]}});
        assert_eq!(eviction_time(&short, &[taint(TAINT_UNREACHABLE, added)]), Some(t0 + chrono::Duration::seconds(10)));
    }

    #[test]
    fn taints_follow_ready() {
        let ready = json!({"status": {"conditions": [{"type": "Ready", "status": "True"}]}});
        assert!(wanted_taints(&ready, true).is_empty());
        let unknown = json!({"spec": {"taints": [{"key": TAINT_UNREACHABLE, "effect": "NoExecute", "timeAdded": "2026-01-01T00:00:00Z"}]},
            "status": {"conditions": [{"type": "Ready", "status": "Unknown"}]}});
        let t = wanted_taints(&unknown, true);
        assert_eq!(t.len(), 2);
        assert_eq!(t[1]["timeAdded"], "2026-01-01T00:00:00Z");
        assert_eq!(wanted_taints(&unknown, false).len(), 1);
        let not_ready = json!({"status": {"conditions": [{"type": "Ready", "status": "False"}]}});
        assert_eq!(s(&wanted_taints(&not_ready, true)[0], "/key"), TAINT_NOT_READY);
    }
}
