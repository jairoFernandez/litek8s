//! Small controllers: Service status and endpoints, per-namespace bootstrap
//! objects, and reclaiming the volumes of deleted claims.

use serde_json::{Value, json};

use super::Requeue;
use super::util::{self, s};
use crate::kube::defaults::ROOT_CA_CONFIGMAP;
use crate::kube::selector::LabelSelector;
use crate::kube::server::{ApiError, ApiState};

const SERVICE_NAME_LABEL: &str = "kubernetes.io/service-name";
const MANAGED_BY: &str = "endpoints.kubernetes.io/managed-by";
const ENDPOINTS_MANAGER: &str = "endpoint-controller";

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
        let managed = |e: &Value| util::labels_of(e).get(MANAGED_BY).and_then(Value::as_str) == Some(ENDPOINTS_MANAGER);
        if api.get_object("", "endpoints", ns, name).is_some_and(|e| managed(&e)) {
            let _ = api.delete_named("", "endpoints", ns, name);
        }
        return Ok(None);
    };
    if util::is_deleting(&svc) {
        // Being deleted in the foreground, the garbage collector removes its
        // EndpointSlices first: recreating them would block it forever.
        return Ok(None);
    }
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
        mirror_endpoints(api, &svc)?;
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
    // Named target ports resolve per pod, so pods are grouped by the ports
    // they serve: one Endpoints subset (and one EndpointSlice) per group.
    let ports_of = |pod: &Value| -> Vec<Value> {
        ports
            .iter()
            .filter_map(|p| {
                let mut port = json!({
                    "port": target_port(p, Some(pod))?,
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
            .collect()
    };
    let mut groups: Vec<(Vec<Value>, Vec<&Value>)> = Vec::new();
    for pod in pods {
        let ps = ports_of(pod);
        if ps.is_empty() && !ports.is_empty() {
            continue;
        }
        match groups.iter_mut().find(|(g, _)| *g == ps) {
            Some((_, members)) => members.push(pod),
            None => groups.push((ps, vec![pod])),
        }
    }
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
    let subsets: Vec<Value> = groups
        .iter()
        .map(|(ps, members)| {
            let ready: Vec<Value> = members.iter().filter(|p| util::is_ready(p)).map(|p| address(p)).collect();
            let not_ready: Vec<Value> = members.iter().filter(|p| !util::is_ready(p)).map(|p| address(p)).collect();
            let mut subset = json!({"ports": ps});
            if !ready.is_empty() {
                subset["addresses"] = json!(ready);
            }
            if !not_ready.is_empty() {
                subset["notReadyAddresses"] = json!(not_ready);
            }
            subset
        })
        .collect();
    let subsets = json!(subsets);
    let mut labels = svc.pointer("/metadata/labels").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
    labels[MANAGED_BY] = json!(ENDPOINTS_MANAGER);
    match api.get_object("", "endpoints", &ns, &name) {
        Some(existing) => {
            let labeled = util::labels_of(&existing).get(MANAGED_BY).is_some();
            if existing.get("subsets").cloned().unwrap_or_else(|| json!([])) != subsets || !labeled {
                let mut updated = existing.clone();
                updated["subsets"] = subsets;
                if !updated["metadata"].get("labels").is_some_and(Value::is_object) {
                    updated["metadata"]["labels"] = json!({});
                }
                updated["metadata"]["labels"][MANAGED_BY] = json!(ENDPOINTS_MANAGER);
                api.update_object("", "endpoints", updated)?;
            }
        }
        None => {
            let ep = json!({"apiVersion": "v1", "kind": "Endpoints", "metadata": {"name": name, "labels": labels}, "subsets": subsets});
            api.create_object("", "endpoints", Some(&ns), ep)?;
        }
    }

    // The EndpointSlice view of the same data: one slice per port group
    // (one empty slice when there are no endpoints).
    if groups.is_empty() {
        groups.push((Vec::new(), Vec::new()));
    }
    let (slices, _) = api.store().list("discovery.k8s.io", "endpointslices", Some(&ns));
    let mut mine: Vec<Value> = slices
        .into_iter()
        .filter(|sl| {
            util::labels_of(sl).get(SERVICE_NAME_LABEL).and_then(Value::as_str) == Some(name.as_str())
                && util::controlled_by(sl, &util::uid(svc))
        })
        .collect();
    for (ps, members) in &groups {
        // EndpointSlice ports always carry a name (kubectl relies on it).
        let ps: Vec<Value> = ps
            .iter()
            .map(|p| {
                let mut p = p.clone();
                if p.get("name").is_none() {
                    p["name"] = json!("");
                }
                p
            })
            .collect();
        let ps = &ps;
        let endpoints: Vec<Value> = members
            .iter()
            .map(|p| {
                let ready = util::is_ready(p);
                let mut e = json!({
                    "addresses": [s(p, "/status/podIP")],
                    "conditions": {"ready": ready, "serving": ready, "terminating": false},
                    "nodeName": s(p, "/spec/nodeName"),
                    "targetRef": {"kind": "Pod", "namespace": ns, "name": util::name(p), "uid": util::uid(p)},
                });
                if s(p, "/spec/subdomain") == name && !s(p, "/spec/hostname").is_empty() {
                    e["hostname"] = json!(s(p, "/spec/hostname"));
                }
                e
            })
            .collect();
        // Reuse the slice with the same ports, else any spare one.
        let pos = mine.iter().position(|sl| sl.get("ports") == Some(&json!(ps))).or(if mine.is_empty() { None } else { Some(0) });
        match pos.map(|i| mine.remove(i)) {
            Some(slice) => {
                if slice.get("endpoints") != Some(&json!(endpoints)) || slice.get("ports") != Some(&json!(ps)) {
                    let mut updated = slice.clone();
                    updated["endpoints"] = json!(endpoints);
                    updated["ports"] = json!(ps);
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
                    "ports": ps,
                });
                api.create_object("discovery.k8s.io", "endpointslices", Some(&ns), slice)?;
            }
        }
    }
    for stale in mine {
        let _ = api.delete_named("discovery.k8s.io", "endpointslices", &ns, &util::name(&stale));
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

/// ResourceQuota: `status.hard` mirrors the spec, `status.used` is what the
/// namespace uses now.
pub(crate) fn reconcile_quota(api: &ApiState, ns: &str, name: &str) -> Result<Requeue, ApiError> {
    let Some(quota) = api.get_object("", "resourcequotas", ns, name) else { return Ok(None) };
    let used = crate::kube::quota::used(api.store(), &quota);
    let hard = quota.pointer("/spec/hard").cloned().unwrap_or_else(|| json!({}));
    util::write_status(api, "", "resourcequotas", &quota, json!({"hard": hard, "used": used}))?;
    Ok(None)
}

/// EndpointSlice mirroring: a selector-less Service's hand-written
/// Endpoints get matching EndpointSlices.
fn mirror_endpoints(api: &ApiState, svc: &Value) -> Result<(), ApiError> {
    const MANAGER: &str = "endpointslicemirroring-controller.k8s.io";
    let (ns, name) = util::ns_name(svc);
    let ep = api.get_object("", "endpoints", &ns, &name);
    let skip = ep.as_ref().is_some_and(|e| util::labels_of(e).get("endpointslice.kubernetes.io/skip-mirror").and_then(Value::as_str) == Some("true"));
    let mut wanted: Vec<(Value, Value)> = Vec::new();
    if !skip {
        for subset in ep.as_ref().and_then(|e| e.get("subsets")).and_then(Value::as_array).into_iter().flatten() {
            let mut endpoints = Vec::new();
            for (field, ready) in [("addresses", true), ("notReadyAddresses", false)] {
                for a in subset.get(field).and_then(Value::as_array).into_iter().flatten() {
                    let mut e = json!({"addresses": [s(a, "/ip")], "conditions": {"ready": ready}});
                    for k in ["hostname", "nodeName", "targetRef"] {
                        if let Some(v) = a.get(k) {
                            e[k] = v.clone();
                        }
                    }
                    endpoints.push(e);
                }
            }
            let ports = subset.get("ports").cloned().unwrap_or_else(|| json!([]));
            wanted.push((json!(endpoints), ports));
        }
    }
    let (slices, _) = api.store().list("discovery.k8s.io", "endpointslices", Some(&ns));
    let mut mine: Vec<Value> = slices
        .into_iter()
        .filter(|sl| {
            let labels = util::labels_of(sl);
            labels.get(SERVICE_NAME_LABEL).and_then(Value::as_str) == Some(name.as_str())
                && labels.get("endpointslice.kubernetes.io/managed-by").and_then(Value::as_str) == Some(MANAGER)
        })
        .collect();
    for (endpoints, ports) in wanted {
        match (!mine.is_empty()).then(|| mine.remove(0)) {
            Some(slice) => {
                if slice.get("endpoints") != Some(&endpoints) || slice.get("ports") != Some(&ports) {
                    let mut updated = slice.clone();
                    updated["endpoints"] = endpoints;
                    updated["ports"] = ports;
                    api.update_object("discovery.k8s.io", "endpointslices", updated)?;
                }
            }
            None => {
                let slice = json!({
                    "apiVersion": "discovery.k8s.io/v1",
                    "kind": "EndpointSlice",
                    "metadata": {
                        "generateName": format!("{name}-"),
                        "labels": {SERVICE_NAME_LABEL: name, "endpointslice.kubernetes.io/managed-by": MANAGER},
                        "ownerReferences": [util::owner_ref(svc)],
                    },
                    "addressType": "IPv4",
                    "endpoints": endpoints,
                    "ports": ports,
                });
                api.create_object("discovery.k8s.io", "endpointslices", Some(&ns), slice)?;
            }
        }
    }
    for stale in mine {
        let _ = api.delete_named("discovery.k8s.io", "endpointslices", &ns, &util::name(&stale));
    }
    Ok(())
}

/// PodDisruptionBudget status: how many matching pods are healthy and how
/// many may be disrupted now.
pub(crate) fn reconcile_pdb(api: &ApiState, ns: &str, name: &str) -> Result<Requeue, ApiError> {
    let Some(pdb) = api.get_object("policy", "poddisruptionbudgets", ns, name) else { return Ok(None) };
    let selector = LabelSelector::from_object(pdb.pointer("/spec/selector"));
    let (pods, _) = api.store().list("", "pods", Some(ns));
    let pods: Vec<&Value> = pods
        .iter()
        .filter(|p| selector.as_ref().is_some_and(|s| s.matches(p)) && !util::is_terminal(p) && !util::is_deleting(p))
        .collect();
    let expected = pods.len() as i64;
    let healthy = pods.iter().filter(|p| util::is_ready(p)).count() as i64;
    let desired = match (pdb.pointer("/spec/minAvailable"), pdb.pointer("/spec/maxUnavailable")) {
        (Some(min), _) => util::int_or_percent(Some(min), expected, true, 0),
        (None, Some(max)) => (expected - util::int_or_percent(Some(max), expected, true, 0)).max(0),
        (None, None) => expected,
    };
    let allowed = (healthy - desired).max(0);
    let mut conditions = util::conditions_of(&pdb);
    let (status, reason) = if allowed > 0 { ("True", "SufficientPods") } else { ("False", "InsufficientPods") };
    util::set_condition(&mut conditions, "DisruptionAllowed", status, reason, "");
    let mut new_status = json!({
        "currentHealthy": healthy,
        "desiredHealthy": desired,
        "expectedPods": expected,
        "disruptionsAllowed": allowed,
        "observedGeneration": util::i(&pdb, "/metadata/generation").unwrap_or(1),
        "conditions": conditions,
    });
    if let Some(d) = pdb.pointer("/status/disruptedPods") {
        new_status["disruptedPods"] = d.clone();
    }
    util::write_status(api, "policy", "poddisruptionbudgets", &pdb, new_status)?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube::store::ObjectKey;

    fn api() -> ApiState {
        ApiState::open(None, "127.0.0.1:0".into(), false).unwrap()
    }

    fn service(api: &ApiState, name: &str, spec: Value) -> Value {
        api.create_object("", "services", Some("default"), json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": name}, "spec": spec}))
            .unwrap()
    }

    fn web_service(api: &ApiState, target: Value) {
        service(api, "web", json!({"selector": {"app": "web"}, "ports": [{"name": "http", "port": 80, "targetPort": target}]}));
    }

    /// A pod with `labels` and container `ports`, given `ip` (when not empty)
    /// and Ready as `ready`.
    fn pod(api: &ApiState, name: &str, labels: Value, ports: Value, ip: &str, ready: bool) -> Value {
        let p = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name, "labels": labels},
            "spec": {"containers": [{"name": "c", "image": "nginx:1", "ports": ports}]}});
        let created = api.create_object("", "pods", Some("default"), p).unwrap();
        let mut status = json!({"phase": "Running", "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}]});
        if !ip.is_empty() {
            status["podIP"] = json!(ip);
        }
        util::write_status(api, "", "pods", &created, status).unwrap();
        api.get_object("", "pods", "default", name).unwrap()
    }

    fn web_pod(api: &ApiState, name: &str, ip: &str, ready: bool) -> Value {
        pod(api, name, json!({"app": "web"}), json!([]), ip, ready)
    }

    fn endpoints(api: &ApiState, name: &str) -> Option<Value> {
        api.get_object("", "endpoints", "default", name)
    }

    fn ips(addresses: &Value) -> Vec<String> {
        addresses.as_array().into_iter().flatten().map(|a| s(a, "/ip").to_string()).collect()
    }

    fn slices(api: &ApiState, service: &str) -> Vec<Value> {
        let (all, _) = api.store().list("discovery.k8s.io", "endpointslices", Some("default"));
        all.into_iter().filter(|sl| util::labels_of(sl).get(SERVICE_NAME_LABEL).and_then(Value::as_str) == Some(service)).collect()
    }

    fn slice_ips(slice: &Value) -> Vec<String> {
        slice["endpoints"].as_array().unwrap().iter().map(|e| s(e, "/addresses/0").to_string()).collect()
    }

    fn ready_node(api: &ApiState, name: &str, ip: &str, ready: bool) {
        let created = api.create_object("", "nodes", None, json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": name}})).unwrap();
        let status = json!({"addresses": [{"type": "Hostname", "address": name}, {"type": "InternalIP", "address": ip}],
            "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}]});
        util::write_status(api, "", "nodes", &created, status).unwrap();
    }

    #[test]
    fn endpoints_list_matching_pods_split_by_readiness() {
        let api = api();
        web_service(&api, json!(8080));
        web_pod(&api, "a", "10.42.0.1", true);
        web_pod(&api, "b", "10.42.0.2", false);
        pod(&api, "other", json!({"app": "db"}), json!([]), "10.42.0.3", true);
        web_pod(&api, "no-ip", "", true);
        let done = web_pod(&api, "done", "10.42.0.4", true);
        util::write_status(&api, "", "pods", &done, json!({"phase": "Succeeded", "podIP": "10.42.0.4"})).unwrap();
        reconcile_service(&api, "default", "web").unwrap();

        let ep = endpoints(&api, "web").unwrap();
        assert_eq!(util::labels_of(&ep).get(MANAGED_BY), Some(&json!(ENDPOINTS_MANAGER)));
        let subsets = ep["subsets"].as_array().unwrap();
        assert_eq!(subsets.len(), 1);
        assert_eq!(ips(&subsets[0]["addresses"]), ["10.42.0.1"]);
        assert_eq!(ips(&subsets[0]["notReadyAddresses"]), ["10.42.0.2"]);
        assert_eq!(subsets[0]["ports"], json!([{"name": "http", "port": 8080, "protocol": "TCP"}]));
        assert_eq!(subsets[0]["addresses"][0]["targetRef"]["name"], "a");
        assert_eq!(subsets[0]["addresses"][0]["targetRef"]["uid"], json!(util::uid(&api.get_object("", "pods", "default", "a").unwrap())));
    }

    #[test]
    fn endpoint_slices_mirror_the_endpoints_with_their_labels() {
        let api = api();
        service(&api, "web", json!({"selector": {"app": "web"}, "ports": [{"port": 80, "appProtocol": "http"}]}));
        web_pod(&api, "a", "10.42.0.1", true);
        web_pod(&api, "b", "10.42.0.2", false);
        reconcile_service(&api, "default", "web").unwrap();

        let svc = api.get_object("", "services", "default", "web").unwrap();
        let sl = slices(&api, "web");
        assert_eq!(sl.len(), 1);
        let slice = &sl[0];
        assert_eq!(util::labels_of(slice).get("endpointslice.kubernetes.io/managed-by"), Some(&json!("endpointslice-controller.k8s.io")));
        assert!(util::controlled_by(slice, &util::uid(&svc)));
        assert_eq!(slice["addressType"], "IPv4");
        // No targetPort: the port itself; slice ports always have a name.
        assert_eq!(slice["ports"], json!([{"name": "", "port": 80, "protocol": "TCP", "appProtocol": "http"}]));
        assert_eq!(slice_ips(slice), ["10.42.0.1", "10.42.0.2"]);
        assert_eq!(slice["endpoints"][0]["conditions"], json!({"ready": true, "serving": true, "terminating": false}));
        assert_eq!(slice["endpoints"][1]["conditions"], json!({"ready": false, "serving": false, "terminating": false}));
        assert_eq!(slice["endpoints"][0]["targetRef"]["name"], "a");
        let ep = endpoints(&api, "web").unwrap();
        assert!(ep["subsets"][0]["ports"][0].get("name").is_none());

        // Reconciling again changes nothing.
        let rv = s(slice, "/metadata/resourceVersion").to_string();
        let ep_rv = s(&ep, "/metadata/resourceVersion").to_string();
        reconcile_service(&api, "default", "web").unwrap();
        assert_eq!(s(&slices(&api, "web")[0], "/metadata/resourceVersion"), rv);
        assert_eq!(s(&endpoints(&api, "web").unwrap(), "/metadata/resourceVersion"), ep_rv);
    }

    #[test]
    fn named_target_ports_group_pods_by_the_port_they_serve() {
        let api = api();
        web_service(&api, json!("web"));
        pod(&api, "a", json!({"app": "web"}), json!([{"name": "web", "containerPort": 8080}]), "10.42.0.1", true);
        pod(&api, "b", json!({"app": "web"}), json!([{"name": "web", "containerPort": 9090}]), "10.42.0.2", true);
        pod(&api, "c", json!({"app": "web"}), json!([{"name": "web", "containerPort": 8080}]), "10.42.0.3", true);
        // Without the named port the pod cannot serve the Service.
        pod(&api, "d", json!({"app": "web"}), json!([{"name": "metrics", "containerPort": 9100}]), "10.42.0.4", true);
        reconcile_service(&api, "default", "web").unwrap();

        let ep = endpoints(&api, "web").unwrap();
        let mut by_port: Vec<(i64, Vec<String>)> =
            ep["subsets"].as_array().unwrap().iter().map(|ss| (ss["ports"][0]["port"].as_i64().unwrap(), ips(&ss["addresses"]))).collect();
        by_port.sort();
        assert_eq!(by_port, vec![(8080, vec!["10.42.0.1".to_string(), "10.42.0.3".to_string()]), (9090, vec!["10.42.0.2".to_string()])]);
        let mut slice_ports: Vec<(i64, Vec<String>)> =
            slices(&api, "web").iter().map(|sl| (sl["ports"][0]["port"].as_i64().unwrap(), slice_ips(sl))).collect();
        slice_ports.sort();
        assert_eq!(slice_ports, by_port);

        // One group gone: its slice is removed.
        api.delete_named("", "pods", "default", "b").unwrap();
        reconcile_service(&api, "default", "web").unwrap();
        let sl = slices(&api, "web");
        assert_eq!(sl.len(), 1);
        assert_eq!(sl[0]["ports"][0]["port"], 8080);
    }

    #[test]
    fn numeric_string_target_ports_are_ports() {
        let api = api();
        web_service(&api, json!("8443"));
        web_pod(&api, "a", "10.42.0.1", true);
        reconcile_service(&api, "default", "web").unwrap();
        assert_eq!(endpoints(&api, "web").unwrap()["subsets"][0]["ports"][0]["port"], 8443);
    }

    #[test]
    fn pods_in_the_service_subdomain_get_hostnames() {
        let api = api();
        web_service(&api, json!(80));
        let p = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "web-0", "labels": {"app": "web"}},
            "spec": {"hostname": "web-0", "subdomain": "web", "containers": [{"name": "c", "image": "nginx:1"}]}});
        let created = api.create_object("", "pods", Some("default"), p).unwrap();
        util::write_status(&api, "", "pods", &created, json!({"podIP": "10.42.0.1", "conditions": [{"type": "Ready", "status": "True"}]})).unwrap();
        let p = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "other", "labels": {"app": "web"}},
            "spec": {"hostname": "other", "subdomain": "elsewhere", "containers": [{"name": "c", "image": "nginx:1"}]}});
        let created = api.create_object("", "pods", Some("default"), p).unwrap();
        util::write_status(&api, "", "pods", &created, json!({"podIP": "10.42.0.2", "conditions": [{"type": "Ready", "status": "True"}]})).unwrap();
        reconcile_service(&api, "default", "web").unwrap();

        let ep = endpoints(&api, "web").unwrap();
        let addrs = ep["subsets"][0]["addresses"].as_array().unwrap();
        let hostname = |n: &str| addrs.iter().find(|a| a["targetRef"]["name"] == n).unwrap().get("hostname").cloned();
        assert_eq!(hostname("web-0"), Some(json!("web-0")));
        assert_eq!(hostname("other"), None);
        let sl = &slices(&api, "web")[0];
        let ends = sl["endpoints"].as_array().unwrap();
        assert_eq!(ends.iter().find(|e| e["targetRef"]["name"] == "web-0").unwrap()["hostname"], "web-0");
        assert!(ends.iter().find(|e| e["targetRef"]["name"] == "other").unwrap().get("hostname").is_none());
    }

    #[test]
    fn no_ready_endpoints_leave_one_empty_slice() {
        let api = api();
        web_service(&api, json!(80));
        web_pod(&api, "a", "10.42.0.1", true);
        reconcile_service(&api, "default", "web").unwrap();
        api.delete_named("", "pods", "default", "a").unwrap();
        reconcile_service(&api, "default", "web").unwrap();

        assert_eq!(endpoints(&api, "web").unwrap()["subsets"], json!([]));
        let sl = slices(&api, "web");
        assert_eq!(sl.len(), 1);
        assert_eq!(sl[0]["endpoints"], json!([]));
        assert_eq!(sl[0]["ports"], json!([]));
    }

    #[test]
    fn existing_endpoints_are_updated_and_labeled() {
        let api = api();
        api.create_object("", "endpoints", Some("default"), json!({"apiVersion": "v1", "kind": "Endpoints", "metadata": {"name": "web"}}))
            .unwrap();
        web_service(&api, json!(80));
        web_pod(&api, "a", "10.42.0.1", true);
        reconcile_service(&api, "default", "web").unwrap();

        let ep = endpoints(&api, "web").unwrap();
        assert_eq!(util::labels_of(&ep).get(MANAGED_BY), Some(&json!(ENDPOINTS_MANAGER)));
        assert_eq!(ips(&ep["subsets"][0]["addresses"]), ["10.42.0.1"]);

        web_pod(&api, "b", "10.42.0.2", true);
        reconcile_service(&api, "default", "web").unwrap();
        assert_eq!(ips(&endpoints(&api, "web").unwrap()["subsets"][0]["addresses"]), ["10.42.0.1", "10.42.0.2"]);
    }

    #[test]
    fn service_labels_are_copied_to_new_endpoints() {
        let api = api();
        api.create_object(
            "",
            "services",
            Some("default"),
            json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "web", "labels": {"team": "a"}},
                "spec": {"selector": {"app": "web"}, "ports": [{"port": 80}]}}),
        )
        .unwrap();
        reconcile_service(&api, "default", "web").unwrap();
        let labels = util::labels_of(&endpoints(&api, "web").unwrap()).clone();
        assert_eq!(labels.get("team"), Some(&json!("a")));
        assert_eq!(labels.get(MANAGED_BY), Some(&json!(ENDPOINTS_MANAGER)));
    }

    #[test]
    fn deleting_a_service_removes_only_managed_endpoints() {
        let api = api();
        web_service(&api, json!(80));
        reconcile_service(&api, "default", "web").unwrap();
        assert!(endpoints(&api, "web").is_some());
        api.delete_named("", "services", "default", "web").unwrap();
        reconcile_service(&api, "default", "web").unwrap();
        assert!(endpoints(&api, "web").is_none());

        // Hand-written Endpoints with no Service stay.
        api.create_object("", "endpoints", Some("default"), json!({"apiVersion": "v1", "kind": "Endpoints", "metadata": {"name": "ext"}}))
            .unwrap();
        reconcile_service(&api, "default", "ext").unwrap();
        assert!(endpoints(&api, "ext").is_some());
    }

    #[test]
    fn a_service_being_deleted_gets_no_endpoints() {
        let api = api();
        api.create_object(
            "",
            "services",
            Some("default"),
            json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "web", "finalizers": ["example.com/hold"]},
                "spec": {"selector": {"app": "web"}, "ports": [{"port": 80}]}}),
        )
        .unwrap();
        api.delete_named("", "services", "default", "web").unwrap();
        assert!(util::is_deleting(&api.get_object("", "services", "default", "web").unwrap()));
        reconcile_service(&api, "default", "web").unwrap();
        assert!(endpoints(&api, "web").is_none());
        assert!(slices(&api, "web").is_empty());
    }

    #[test]
    fn load_balancers_get_a_ready_node_address() {
        let api = api();
        ready_node(&api, "a", "192.168.1.10", false);
        ready_node(&api, "b", "192.168.1.11", true);
        service(&api, "lb", json!({"type": "LoadBalancer", "selector": {"app": "web"}, "ports": [{"port": 80}]}));
        service(&api, "plain", json!({"selector": {"app": "web"}, "ports": [{"port": 80}]}));
        reconcile_service(&api, "default", "lb").unwrap();
        reconcile_service(&api, "default", "plain").unwrap();

        let lb = api.get_object("", "services", "default", "lb").unwrap();
        assert_eq!(lb.pointer("/status/loadBalancer"), Some(&json!({"ingress": [{"ip": "192.168.1.11", "ipMode": "VIP"}]})));
        let plain = api.get_object("", "services", "default", "plain").unwrap();
        assert!(plain.pointer("/status/loadBalancer/ingress").is_none());

        let rv = s(&lb, "/metadata/resourceVersion").to_string();
        reconcile_service(&api, "default", "lb").unwrap();
        assert_eq!(s(&api.get_object("", "services", "default", "lb").unwrap(), "/metadata/resourceVersion"), rv);
    }

    #[test]
    fn load_balancers_fall_back_to_any_node_address() {
        let api = api();
        ready_node(&api, "a", "192.168.1.10", false);
        service(&api, "lb", json!({"type": "LoadBalancer", "selector": {"app": "web"}, "ports": [{"port": 80}]}));
        reconcile_service(&api, "default", "lb").unwrap();
        let lb = api.get_object("", "services", "default", "lb").unwrap();
        assert_eq!(lb.pointer("/status/loadBalancer/ingress/0/ip"), Some(&json!("192.168.1.10")));
    }

    #[test]
    fn selectorless_services_mirror_hand_written_endpoints() {
        let api = api();
        service(&api, "ext", json!({"ports": [{"port": 5432}]}));
        let ep = json!({"apiVersion": "v1", "kind": "Endpoints", "metadata": {"name": "ext"}, "subsets": [
            {"addresses": [{"ip": "10.0.0.1", "hostname": "db-0", "nodeName": "n1"}], "notReadyAddresses": [{"ip": "10.0.0.2"}],
             "ports": [{"name": "pg", "port": 5432, "protocol": "TCP"}]},
            {"addresses": [{"ip": "10.0.0.3"}], "ports": [{"name": "pg", "port": 6432, "protocol": "TCP"}]},
        ]});
        api.create_object("", "endpoints", Some("default"), ep).unwrap();
        reconcile_service(&api, "default", "ext").unwrap();

        let svc = api.get_object("", "services", "default", "ext").unwrap();
        let mut sl = slices(&api, "ext");
        assert_eq!(sl.len(), 2);
        sl.sort_by_key(|x| x["ports"][0]["port"].as_i64());
        assert_eq!(
            util::labels_of(&sl[0]).get("endpointslice.kubernetes.io/managed-by"),
            Some(&json!("endpointslicemirroring-controller.k8s.io"))
        );
        assert!(util::controlled_by(&sl[0], &util::uid(&svc)));
        assert_eq!(
            sl[0]["endpoints"],
            json!([
                {"addresses": ["10.0.0.1"], "conditions": {"ready": true}, "hostname": "db-0", "nodeName": "n1"},
                {"addresses": ["10.0.0.2"], "conditions": {"ready": false}},
            ])
        );
        assert_eq!(sl[0]["ports"], json!([{"name": "pg", "port": 5432, "protocol": "TCP"}]));
        assert_eq!(slice_ips(&sl[1]), ["10.0.0.3"]);
        // The Endpoints are the user's: not relabeled or rewritten.
        assert!(util::labels_of(&endpoints(&api, "ext").unwrap()).get(MANAGED_BY).is_none());

        // One subset left: one slice, updated in place.
        let mut ep = endpoints(&api, "ext").unwrap();
        ep["subsets"] = json!([{"addresses": [{"ip": "10.0.0.9"}], "ports": [{"name": "pg", "port": 5432, "protocol": "TCP"}]}]);
        api.update_object("", "endpoints", ep).unwrap();
        reconcile_service(&api, "default", "ext").unwrap();
        let sl = slices(&api, "ext");
        assert_eq!(sl.len(), 1);
        assert_eq!(slice_ips(&sl[0]), ["10.0.0.9"]);

        // skip-mirror: no slices.
        let mut ep = endpoints(&api, "ext").unwrap();
        ep["metadata"]["labels"] = json!({"endpointslice.kubernetes.io/skip-mirror": "true"});
        api.update_object("", "endpoints", ep).unwrap();
        reconcile_service(&api, "default", "ext").unwrap();
        assert!(slices(&api, "ext").is_empty());
    }

    #[test]
    fn selectorless_services_without_endpoints_have_no_slices() {
        let api = api();
        service(&api, "ext", json!({"selector": {}, "ports": [{"port": 5432}]}));
        reconcile_service(&api, "default", "ext").unwrap();
        assert!(slices(&api, "ext").is_empty());
        assert!(endpoints(&api, "ext").is_none());
    }

    #[test]
    fn namespaces_get_the_cluster_ca_bundle() {
        let api = api();
        let ca = api.tls().expect("in-memory TLS").ca_pem().to_string();
        api.create_object("", "namespaces", None, json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "team"}})).unwrap();
        reconcile_namespace(&api, "team").unwrap();
        let cm = api.get_object("", "configmaps", "team", ROOT_CA_CONFIGMAP).unwrap();
        assert_eq!(cm["data"], json!({"ca.crt": ca}));
        assert!(cm.pointer("/metadata/annotations/kubernetes.io~1description").is_some());
        assert!(api.get_object("", "configmaps", "team", "extension-apiserver-authentication").is_none());

        // Edited by hand: put back.
        let mut edited = cm.clone();
        edited["data"] = json!({"ca.crt": "tampered"});
        api.update_object("", "configmaps", edited).unwrap();
        reconcile_namespace(&api, "team").unwrap();
        assert_eq!(api.get_object("", "configmaps", "team", ROOT_CA_CONFIGMAP).unwrap()["data"], json!({"ca.crt": ca}));

        // Unchanged: no write.
        let rv = s(&api.get_object("", "configmaps", "team", ROOT_CA_CONFIGMAP).unwrap(), "/metadata/resourceVersion").to_string();
        reconcile_namespace(&api, "team").unwrap();
        assert_eq!(s(&api.get_object("", "configmaps", "team", ROOT_CA_CONFIGMAP).unwrap(), "/metadata/resourceVersion"), rv);

        assert_eq!(reconcile_namespace(&api, "missing").unwrap(), None);
        assert!(api.get_object("", "configmaps", "missing", ROOT_CA_CONFIGMAP).is_none());
    }

    #[test]
    fn kube_system_gets_the_extension_apiserver_authentication() {
        let api = api();
        let ca = api.tls().unwrap().ca_pem().to_string();
        let _ = api.delete_named("", "configmaps", "kube-system", "extension-apiserver-authentication");
        reconcile_namespace(&api, "kube-system").unwrap();
        let cm = api.get_object("", "configmaps", "kube-system", "extension-apiserver-authentication").unwrap();
        assert_eq!(cm["data"]["client-ca-file"], json!(ca));
        assert_eq!(cm["data"]["requestheader-client-ca-file"], json!(ca));
        assert_eq!(cm["data"]["requestheader-allowed-names"], "[\"front-proxy-client\"]");
        assert!(api.get_object("", "configmaps", "kube-system", ROOT_CA_CONFIGMAP).is_some());
    }

    #[test]
    fn terminating_namespaces_get_nothing() {
        let api = api();
        let mut ns = api.create_object("", "namespaces", None, json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "gone"}})).unwrap();
        ns["metadata"]["deletionTimestamp"] = json!(util::rfc3339(util::now()));
        api.store().update(ObjectKey::new("", "namespaces", "", "gone"), ns, None).unwrap();
        reconcile_namespace(&api, "gone").unwrap();
        assert!(api.get_object("", "configmaps", "gone", ROOT_CA_CONFIGMAP).is_none());
    }

    fn pv(api: &ApiState, name: &str, claim: &str, policy: &str) {
        let pv = json!({"apiVersion": "v1", "kind": "PersistentVolume", "metadata": {"name": name},
            "spec": {"capacity": {"storage": "1Gi"}, "accessModes": ["ReadWriteOnce"], "hostPath": {"path": "/tmp/x"},
                "persistentVolumeReclaimPolicy": policy, "claimRef": {"namespace": "default", "name": claim}}});
        let created = api.create_object("", "persistentvolumes", None, pv).unwrap();
        util::write_status(api, "", "persistentvolumes", &created, json!({"phase": "Bound"})).unwrap();
    }

    #[test]
    fn volumes_of_deleted_claims_are_deleted_or_released() {
        let api = api();
        pv(&api, "pv-delete", "data", "Delete");
        pv(&api, "pv-retain", "data", "Retain");
        pv(&api, "pv-other", "logs", "Delete");
        reclaim_volumes(&api, "default", "data").unwrap();

        assert!(api.get_object("", "persistentvolumes", "", "pv-delete").is_none());
        let retained = api.get_object("", "persistentvolumes", "", "pv-retain").unwrap();
        assert_eq!(retained.pointer("/status/phase"), Some(&json!("Released")));
        let other = api.get_object("", "persistentvolumes", "", "pv-other").unwrap();
        assert_eq!(other.pointer("/status/phase"), Some(&json!("Bound")));

        let rv = s(&retained, "/metadata/resourceVersion").to_string();
        reclaim_volumes(&api, "default", "data").unwrap();
        assert_eq!(s(&api.get_object("", "persistentvolumes", "", "pv-retain").unwrap(), "/metadata/resourceVersion"), rv);
    }

    #[test]
    fn volumes_of_existing_claims_are_kept() {
        let api = api();
        pv(&api, "pv", "data", "Delete");
        let pvc = json!({"apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": {"name": "data"},
            "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "1Gi"}}}});
        api.create_object("", "persistentvolumeclaims", Some("default"), pvc).unwrap();
        reclaim_volumes(&api, "default", "data").unwrap();
        assert!(api.get_object("", "persistentvolumes", "", "pv").is_some());
    }

    #[test]
    fn events_older_than_the_ttl_expire() {
        let api = api();
        let now = util::now();
        let stamp = |t: chrono::DateTime<chrono::Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let event = |name: &str, extra: Value| {
            let mut e = json!({"apiVersion": "v1", "kind": "Event", "metadata": {"name": name},
                "involvedObject": {"kind": "Pod", "name": "p"}, "reason": "Started"});
            for (k, v) in extra.as_object().unwrap() {
                e[k] = v.clone();
            }
            api.create_object("", "events", Some("default"), e).unwrap();
            // Recorded long ago: the server stamped creation just now.
            let mut stored = api.get_object("", "events", "default", name).unwrap();
            stored["metadata"]["creationTimestamp"] = json!(stamp(now - chrono::Duration::hours(3)));
            api.store().update(ObjectKey::new("", "events", "default", name), stored, None).unwrap();
        };
        event("old", json!({"lastTimestamp": stamp(now - chrono::Duration::hours(2))}));
        event("recent", json!({"lastTimestamp": stamp(now - chrono::Duration::minutes(10))}));
        // Old last timestamp, but the series was seen just now.
        event("series", json!({"lastTimestamp": stamp(now - chrono::Duration::hours(2)), "series": {"count": 3, "lastObservedTime": stamp(now)}}));

        assert_eq!(expire_events(&api).unwrap(), Some(std::time::Duration::from_secs(300)));
        assert!(api.get_object("", "events", "default", "old").is_none());
        assert!(api.get_object("", "events", "default", "recent").is_some());
        assert!(api.get_object("", "events", "default", "series").is_some());
    }

    #[test]
    fn quota_status_mirrors_hard_and_counts_usage() {
        let api = api();
        let quota = json!({"apiVersion": "v1", "kind": "ResourceQuota", "metadata": {"name": "q"},
            "spec": {"hard": {"pods": "10", "requests.cpu": "4", "services": "5"}}});
        api.create_object("", "resourcequotas", Some("default"), quota).unwrap();
        for (name, cpu) in [("a", "250m"), ("b", "500m")] {
            let p = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name},
                "spec": {"containers": [{"name": "c", "image": "nginx:1", "resources": {"requests": {"cpu": cpu}}}]}});
            api.create_object("", "pods", Some("default"), p).unwrap();
        }
        reconcile_quota(&api, "default", "q").unwrap();

        let status = api.get_object("", "resourcequotas", "default", "q").unwrap()["status"].clone();
        assert_eq!(status["hard"], json!({"pods": "10", "requests.cpu": "4", "services": "5"}));
        assert_eq!(status["used"]["pods"], "2");
        assert_eq!(status["used"]["requests.cpu"], "750m");
        // The bootstrap `kubernetes` Service is in default.
        assert_eq!(status["used"]["services"], "1");

        assert_eq!(reconcile_quota(&api, "default", "missing").unwrap(), None);
    }

    fn pdb(api: &ApiState, spec: Value) {
        let mut spec = spec;
        spec["selector"] = json!({"matchLabels": {"app": "web"}});
        let pdb = json!({"apiVersion": "policy/v1", "kind": "PodDisruptionBudget", "metadata": {"name": "web"}, "spec": spec});
        match api.get_object("policy", "poddisruptionbudgets", "default", "web") {
            Some(mut existing) => {
                existing["spec"] = pdb["spec"].clone();
                api.update_object("policy", "poddisruptionbudgets", existing).unwrap();
            }
            None => {
                api.create_object("policy", "poddisruptionbudgets", Some("default"), pdb).unwrap();
            }
        }
        reconcile_pdb(api, "default", "web").unwrap();
    }

    fn pdb_status(api: &ApiState) -> Value {
        api.get_object("policy", "poddisruptionbudgets", "default", "web").unwrap()["status"].clone()
    }

    /// Four matching pods, three of them ready, plus pods that do not count.
    fn pdb_pods(api: &ApiState) {
        for (name, ready) in [("a", true), ("b", true), ("c", true), ("d", false)] {
            web_pod(api, name, &format!("10.42.0.{}", name.len()), ready);
        }
        pod(api, "other", json!({"app": "db"}), json!([]), "10.42.1.1", true);
        let done = web_pod(api, "done", "10.42.1.2", true);
        util::write_status(api, "", "pods", &done, json!({"phase": "Failed"})).unwrap();
    }

    #[test]
    fn pdb_min_available_allows_the_surplus() {
        let api = api();
        pdb_pods(&api);
        pdb(&api, json!({"minAvailable": 2}));
        let st = pdb_status(&api);
        assert_eq!((st["expectedPods"].as_i64(), st["currentHealthy"].as_i64()), (Some(4), Some(3)));
        assert_eq!(st["desiredHealthy"], 2);
        assert_eq!(st["disruptionsAllowed"], 1);
        let cond = &st["conditions"][0];
        assert_eq!((cond["type"].as_str(), cond["status"].as_str(), cond["reason"].as_str()), (Some("DisruptionAllowed"), Some("True"), Some("SufficientPods")));
        assert_eq!(st["observedGeneration"], 1);

        // 75% of four, rounded up: three, all of the healthy ones.
        pdb(&api, json!({"minAvailable": "75%"}));
        let st = pdb_status(&api);
        assert_eq!(st["desiredHealthy"], 3);
        assert_eq!(st["disruptionsAllowed"], 0);
        assert_eq!(st["conditions"][0]["status"], "False");
        assert_eq!(st["conditions"][0]["reason"], "InsufficientPods");
    }

    #[test]
    fn pdb_max_unavailable_counts_from_the_expected_pods() {
        let api = api();
        pdb_pods(&api);
        pdb(&api, json!({"maxUnavailable": 2}));
        let st = pdb_status(&api);
        assert_eq!(st["desiredHealthy"], 2);
        assert_eq!(st["disruptionsAllowed"], 1);

        // 25% of four is one: three must stay, and one is already down.
        pdb(&api, json!({"maxUnavailable": "25%"}));
        let st = pdb_status(&api);
        assert_eq!(st["desiredHealthy"], 3);
        assert_eq!(st["disruptionsAllowed"], 0);

        // More unavailable than pods: nothing is desired.
        pdb(&api, json!({"maxUnavailable": 10}));
        assert_eq!(pdb_status(&api)["desiredHealthy"], 0);
        assert_eq!(pdb_status(&api)["disruptionsAllowed"], 3);
    }

    #[test]
    fn pdb_without_bounds_wants_every_pod() {
        let api = api();
        pdb_pods(&api);
        pdb(&api, json!({}));
        let st = pdb_status(&api);
        assert_eq!(st["desiredHealthy"], 4);
        assert_eq!(st["disruptionsAllowed"], 0);
    }

    #[test]
    fn pdb_keeps_disrupted_pods_and_ignores_deleting_ones() {
        let api = api();
        pdb_pods(&api);
        // Held by a finalizer: deleting, so not counted.
        let p = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "leaving", "labels": {"app": "web"}, "finalizers": ["example.com/hold"]},
            "spec": {"nodeName": "n1", "containers": [{"name": "c", "image": "nginx:1"}]}});
        let created = api.create_object("", "pods", Some("default"), p).unwrap();
        util::write_status(&api, "", "pods", &created, json!({"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}]})).unwrap();
        let _ = api.delete_named("", "pods", "default", "leaving");
        assert!(api.get_object("", "pods", "default", "leaving").is_none_or(|p| util::is_deleting(&p)));

        pdb(&api, json!({"minAvailable": 1}));
        let mut obj = api.get_object("policy", "poddisruptionbudgets", "default", "web").unwrap();
        obj["status"]["disruptedPods"] = json!({"a": "2026-01-01T00:00:00Z"});
        api.update_status("policy", "poddisruptionbudgets", obj).unwrap();
        reconcile_pdb(&api, "default", "web").unwrap();
        let st = pdb_status(&api);
        assert_eq!(st["expectedPods"], 4);
        assert_eq!(st["disruptedPods"], json!({"a": "2026-01-01T00:00:00Z"}));
        assert_eq!(st["conditions"].as_array().unwrap().len(), 1);

        assert_eq!(reconcile_pdb(&api, "default", "missing").unwrap(), None);
    }
}
