//! Small controllers: Service status and endpoints, per-namespace bootstrap
//! objects, and reclaiming the volumes of deleted claims.

use serde_json::{Value, json};

use super::Requeue;
use super::util::{self, s};
use crate::kube::defaults::ROOT_CA_CONFIGMAP;
use crate::kube::selector::LabelSelector;
use crate::kube::server::{ApiError, ApiState};

const SERVICE_NAME_LABEL: &str = "kubernetes.io/service-name";

/// The address of the first ready node, which LoadBalancer Services get as
/// their ingress (like k3s's servicelb).
fn node_address(api: &ApiState) -> Option<String> {
    // The dev cluster exposes LoadBalancers on the developer's localhost.
    if let Ok(ip) = std::env::var("LITEK8S_LOADBALANCER_IP") {
        if !ip.is_empty() {
            return Some(ip);
        }
    }
    let (nodes, _) = api.store().list("", "nodes", None);
    nodes
        .iter()
        .filter(|n| util::condition_true(n, "Ready"))
        .chain(nodes.iter())
        .find_map(|n| {
            n.pointer("/status/addresses")?
                .as_array()?
                .iter()
                .find(|a| s(a, "/type") == "InternalIP")
                .map(|a| s(a, "/address").to_string())
        })
}

pub(crate) fn reconcile_service(api: &ApiState, ns: &str, name: &str) -> Result<Requeue, ApiError> {
    let Some(svc) = api.get_object("", "services", ns, name) else {
        // Endpoints carry no owner reference; remove them with the Service.
        if api.get_object("", "endpoints", ns, name).is_some() {
            let _ = api.delete_named("", "endpoints", ns, name);
        }
        return Ok(None);
    };
    if s(&svc, "/spec/type") == "LoadBalancer" {
        if let Some(ip) = node_address(api) {
            let want = json!({"loadBalancer": {"ingress": [{"ip": ip, "ipMode": "VIP"}]}});
            let mut status = svc.get("status").cloned().unwrap_or_else(|| json!({}));
            if status.get("loadBalancer") != want.get("loadBalancer") {
                status["loadBalancer"] = want["loadBalancer"].clone();
                util::write_status(api, "", "services", &svc, status)?;
            }
        }
    }
    let selector = svc.pointer("/spec/selector").and_then(Value::as_object).filter(|m| !m.is_empty());
    let Some(selector) = selector else {
        return Ok(None);
    };
    let selector = LabelSelector::from_object(Some(&json!({"matchLabels": selector}))).unwrap_or_default();
    let (pods, _) = api.store().list("", "pods", Some(ns));
    let pods: Vec<&Value> = pods
        .iter()
        .filter(|p| selector.matches(p) && util::is_active(p) && !s(p, "/status/podIP").is_empty())
        .collect();
    sync_endpoints(api, &svc, &pods)?;
    Ok(None)
}

/// A Service port's target on one pod: a number, or a named container port.
fn target_port(port: &Value, pod: Option<&Value>) -> Option<i64> {
    match port.get("targetPort") {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(name)) => {
            if let Ok(n) = name.parse() {
                return Some(n);
            }
            pod?.pointer("/spec/containers")?
                .as_array()?
                .iter()
                .flat_map(|c| c.get("ports").and_then(Value::as_array).cloned().unwrap_or_default())
                .find(|p| s(p, "/name") == name)
                .and_then(|p| p.get("containerPort").and_then(Value::as_i64))
        }
        _ => port.get("port").and_then(Value::as_i64),
    }
}

fn sync_endpoints(api: &ApiState, svc: &Value, pods: &[&Value]) -> Result<(), ApiError> {
    let (ns, name) = util::ns_name(svc);
    let ports: Vec<&Value> = svc.pointer("/spec/ports").and_then(Value::as_array).map(|p| p.iter().collect()).unwrap_or_default();
    let endpoint_ports: Vec<Value> = ports
        .iter()
        .filter_map(|p| {
            let mut port = json!({
                "port": target_port(p, pods.first().copied())?,
                "protocol": p.get("protocol").cloned().unwrap_or_else(|| json!("TCP")),
            });
            if let Some(n) = p.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()) {
                port["name"] = json!(n);
            }
            if let Some(a) = p.get("appProtocol") {
                port["appProtocol"] = a.clone();
            }
            Some(port)
        })
        .collect();
    let address = |p: &Value| {
        let mut a = json!({
            "ip": s(p, "/status/podIP"),
            "nodeName": s(p, "/spec/nodeName"),
            "targetRef": {"kind": "Pod", "namespace": ns, "name": util::name(p), "uid": util::uid(p)},
        });
        // Pods that name this Service as their subdomain get DNS records.
        if s(p, "/spec/subdomain") == name && !s(p, "/spec/hostname").is_empty() {
            a["hostname"] = json!(s(p, "/spec/hostname"));
        }
        a
    };
    let ready: Vec<Value> = pods.iter().filter(|p| util::is_ready(p)).map(|p| address(p)).collect();
    let not_ready: Vec<Value> = pods.iter().filter(|p| !util::is_ready(p)).map(|p| address(p)).collect();
    let mut subset = json!({"ports": endpoint_ports});
    if !ready.is_empty() {
        subset["addresses"] = json!(ready);
    }
    if !not_ready.is_empty() {
        subset["notReadyAddresses"] = json!(not_ready);
    }
    let subsets = if pods.is_empty() { json!([]) } else { json!([subset]) };
    let labels = svc.pointer("/metadata/labels").cloned().unwrap_or_else(|| json!({}));
    match api.get_object("", "endpoints", &ns, &name) {
        Some(existing) => {
            if existing.get("subsets").cloned().unwrap_or_else(|| json!([])) != subsets {
                let mut updated = existing.clone();
                updated["subsets"] = subsets;
                api.update_object("", "endpoints", updated)?;
            }
        }
        None => {
            let ep = json!({"apiVersion": "v1", "kind": "Endpoints", "metadata": {"name": name, "labels": labels}, "subsets": subsets});
            api.create_object("", "endpoints", Some(&ns), ep)?;
        }
    }

    // The EndpointSlice view of the same data.
    let endpoints: Vec<Value> = pods
        .iter()
        .map(|p| {
            let ready = util::is_ready(p);
            json!({
                "addresses": [s(p, "/status/podIP")],
                "conditions": {"ready": ready, "serving": ready, "terminating": false},
                "nodeName": s(p, "/spec/nodeName"),
                "targetRef": {"kind": "Pod", "namespace": ns, "name": util::name(p), "uid": util::uid(p)},
            })
        })
        .collect();
    let (slices, _) = api.store().list("discovery.k8s.io", "endpointslices", Some(&ns));
    let existing = slices.into_iter().find(|sl| {
        util::labels_of(sl).get(SERVICE_NAME_LABEL).and_then(Value::as_str) == Some(name.as_str())
            && util::controlled_by(sl, &util::uid(svc))
    });
    match existing {
        Some(slice) => {
            if slice.get("endpoints") != Some(&json!(endpoints)) || slice.get("ports") != Some(&json!(endpoint_ports)) {
                let mut updated = slice.clone();
                updated["endpoints"] = json!(endpoints);
                updated["ports"] = json!(endpoint_ports);
                api.update_object("discovery.k8s.io", "endpointslices", updated)?;
            }
        }
        None => {
            let slice = json!({
                "apiVersion": "discovery.k8s.io/v1",
                "kind": "EndpointSlice",
                "metadata": {
                    "generateName": format!("{name}-"),
                    "labels": {SERVICE_NAME_LABEL: name, "endpointslice.kubernetes.io/managed-by": "endpointslice-controller.k8s.io"},
                    "ownerReferences": [util::owner_ref(svc)],
                },
                "addressType": "IPv4",
                "endpoints": endpoints,
                "ports": endpoint_ports,
            });
            api.create_object("discovery.k8s.io", "endpointslices", Some(&ns), slice)?;
        }
    }
    Ok(())
}

/// Objects every namespace has: the cluster CA bundle for in-cluster clients.
pub(crate) fn reconcile_namespace(api: &ApiState, ns: &str) -> Result<Requeue, ApiError> {
    let Some(namespace) = api.get_object("", "namespaces", "", ns) else {
        return Ok(None);
    };
    if util::is_deleting(&namespace) {
        return Ok(None);
    }
    let Some(tls) = api.tls() else {
        return Ok(None);
    };
    if ns == "kube-system" {
        // What aggregated API servers (metrics-server, ...) read to trust
        // the API server and its front proxy.
        let auth = json!({
            "client-ca-file": tls.ca_pem(),
            "requestheader-client-ca-file": tls.ca_pem(),
            "requestheader-allowed-names": "[\"front-proxy-client\"]",
            "requestheader-extra-headers-prefix": "[\"X-Remote-Extra-\"]",
            "requestheader-group-headers": "[\"X-Remote-Group\"]",
            "requestheader-username-headers": "[\"X-Remote-User\"]",
        });
        if api.get_object("", "configmaps", ns, "extension-apiserver-authentication").is_none() {
            let cm = json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": "extension-apiserver-authentication"},
                "data": auth,
            });
            api.create_object("", "configmaps", Some(ns), cm)?;
        }
    }
    let data = json!({"ca.crt": tls.ca_pem()});
    match api.get_object("", "configmaps", ns, ROOT_CA_CONFIGMAP) {
        Some(cm) if cm.get("data") == Some(&data) => {}
        Some(cm) => {
            let mut updated = cm.clone();
            updated["data"] = data;
            api.update_object("", "configmaps", updated)?;
        }
        None => {
            let cm = json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": ROOT_CA_CONFIGMAP, "annotations": {"kubernetes.io/description": "Contains a CA bundle that can be used to verify the kube-apiserver when using internal endpoints such as the internal service IP or kubernetes.default.svc. No other usage is guaranteed across distributions of Kubernetes clusters."}},
                "data": data,
            });
            api.create_object("", "configmaps", Some(ns), cm)?;
        }
    }
    Ok(None)
}

/// A claim was deleted: delete (or release) the volume that was bound to it.
pub(crate) fn reclaim_volumes(api: &ApiState, ns: &str, claim: &str) -> Result<Requeue, ApiError> {
    if api.get_object("", "persistentvolumeclaims", ns, claim).is_some() {
        return Ok(None);
    }
    let (pvs, _) = api.store().list("", "persistentvolumes", None);
    for pv in pvs {
        if s(&pv, "/spec/claimRef/namespace") != ns || s(&pv, "/spec/claimRef/name") != claim {
            continue;
        }
        if s(&pv, "/spec/persistentVolumeReclaimPolicy") == "Delete" {
            let _ = api.delete_named("", "persistentvolumes", "", &util::name(&pv));
        } else if s(&pv, "/status/phase") != "Released" {
            util::write_status(api, "", "persistentvolumes", &pv, json!({"phase": "Released"}))?;
        }
    }
    Ok(None)
}

/// How long Events are kept (kube-apiserver's `--event-ttl` default).
const EVENT_TTL: chrono::Duration = chrono::Duration::hours(1);

/// Delete Events (both API groups) older than the TTL. Without this, events
/// written by in-cluster controllers would pile up forever.
pub(crate) fn expire_events(api: &ApiState) -> Result<Requeue, ApiError> {
    let cutoff = util::now() - EVENT_TTL;
    for group in ["", "events.k8s.io"] {
        let (events, _) = api.store().list(group, "events", None);
        for ev in &events {
            let stamp = ["/lastTimestamp", "/eventTime", "/series/lastObservedTime", "/metadata/creationTimestamp"]
                .iter()
                .filter_map(|p| ev.pointer(p).and_then(Value::as_str).and_then(util::parse_time))
                .max();
            if stamp.is_some_and(|t| t < cutoff) {
                let (ns, name) = util::ns_name(ev);
                let _ = api.delete_named(group, "events", &ns, &name);
            }
        }
    }
    Ok(Some(std::time::Duration::from_secs(300)))
}
