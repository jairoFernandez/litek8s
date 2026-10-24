//! What the Kubernetes view asks of the API server: discovery, describe and
//! YAML, deletes, scaling, rollouts (restart, undo, history), cordon and
//! drain, a CronJob run, drill-downs, port lookups and xray trees. Each is
//! one async call that answers with text or the data the UI shows.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use base64::Engine;
use hyper::Method;
use serde_json::{Value, json};

use super::describe::{self, Related};
use super::resources::{self, Resource};
use super::state::{ContainerInfo, ObjRef, Propagation, XNode};
use super::table::Health;
use crate::ui::client::{Endpoint, enc};

const DISCOVERY_ACCEPT: &str = "application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList,application/json;g=apidiscovery.k8s.io;v=v2beta1;as=APIGroupDiscoveryList,application/json";

/// Every resource type the server has: aggregated discovery in two
/// requests, else the legacy documents (one per group, in parallel).
pub async fn discover(ep: &Endpoint) -> Result<Vec<Resource>> {
    let (core, apis) = tokio::join!(ep.json_accept("/api", DISCOVERY_ACCEPT), ep.json_accept("/apis", DISCOVERY_ACCEPT));
    let (core, apis) = (core?, apis?);
    if let (Some(mut a), Some(b)) = (resources::parse_aggregated(&core), resources::parse_aggregated(&apis)) {
        a.extend(b);
        return Ok(a);
    }
    let mut gvs: Vec<String> = core["versions"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
    if gvs.is_empty() {
        gvs.push("v1".into());
    }
    let mut paths: Vec<String> = gvs.iter().map(|v| format!("/api/{v}")).collect();
    paths.extend(resources::preferred_versions(&apis).into_iter().map(|gv| format!("/apis/{gv}")));
    let docs = futures_util::future::join_all(paths.iter().map(|p| ep.json_accept(p, "application/json"))).await;
    let mut out = Vec::new();
    // A broken aggregated API (metrics-server down) costs its group only.
    for d in docs.into_iter().flatten() {
        out.extend(resources::parse_resource_list(&d));
    }
    if out.is_empty() {
        bail!("discovery returned nothing");
    }
    Ok(out)
}

pub async fn get(ep: &Endpoint, obj: &ObjRef) -> Result<Value> {
    ep.json(Method::GET, &obj.res.object_path(&obj.ns, &obj.name), None).await
}

/// The events about an object (newest last).
pub async fn events(ep: &Endpoint, obj: &ObjRef, uid: &str) -> Vec<Value> {
    let mut fs = format!("involvedObject.name={}", obj.name);
    if !obj.ns.is_empty() {
        fs.push_str(&format!(",involvedObject.namespace={}", obj.ns));
    }
    let path = if obj.ns.is_empty() { format!("/api/v1/events?fieldSelector={}", enc(&fs)) } else { format!("/api/v1/namespaces/{}/events?fieldSelector={}", obj.ns, enc(&fs)) };
    let Ok(list) = ep.json(Method::GET, &path, None).await else { return Vec::new() };
    list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e.pointer("/involvedObject/kind").and_then(Value::as_str).is_none_or(|k| k == obj.res.kind))
        .filter(|e| uid.is_empty() || e.pointer("/involvedObject/uid").and_then(Value::as_str).is_none_or(|u| u.is_empty() || u == uid))
        .cloned()
        .collect()
}

pub async fn describe(ep: &Endpoint, obj: &ObjRef) -> Result<Vec<String>> {
    let o = get(ep, obj).await?;
    let uid = o.pointer("/metadata/uid").and_then(Value::as_str).unwrap_or_default().to_string();
    let mut rel = Related { events: events(ep, obj, &uid).await, ..Default::default() };
    match obj.res.kind.as_str() {
        "Deployment" => {
            let sel = selector_string(&o["spec"]["selector"]);
            let path = format!("/apis/apps/v1/namespaces/{}/replicasets?labelSelector={}", obj.ns, enc(&sel));
            if let Ok(l) = ep.json(Method::GET, &path, None).await {
                rel.children = l["items"].as_array().cloned().unwrap_or_default();
            }
        }
        "Node" => {
            let path = format!("/api/v1/pods?fieldSelector={}", enc(&format!("spec.nodeName={}", obj.name)));
            if let Ok(l) = ep.json(Method::GET, &path, None).await {
                rel.children = l["items"].as_array().cloned().unwrap_or_default();
            }
        }
        "Service" => rel.endpoints = ep.json(Method::GET, &format!("/api/v1/namespaces/{}/endpoints/{}", obj.ns, obj.name), None).await.ok(),
        _ => {}
    }
    Ok(describe::describe(&o, &rel, chrono::Utc::now().timestamp()))
}

pub async fn yaml(ep: &Endpoint, obj: &ObjRef, managed: bool) -> Result<Vec<String>> {
    let o = get(ep, obj).await?;
    Ok(super::yaml::to_yaml(&o, managed).lines().map(str::to_string).collect())
}

/// A Secret with `data` decoded (as `stringData` would read).
pub async fn decode(ep: &Endpoint, obj: &ObjRef) -> Result<Vec<String>> {
    let o = get(ep, obj).await?;
    let mut data = serde_json::Map::new();
    for (k, v) in o["data"].as_object().into_iter().flatten() {
        let raw = base64::engine::general_purpose::STANDARD.decode(v.as_str().unwrap_or_default()).unwrap_or_default();
        let text = match String::from_utf8(raw) {
            Ok(s) => s,
            Err(e) => format!("<{} bytes of binary data>", e.into_bytes().len()),
        };
        data.insert(k.clone(), Value::String(text));
    }
    let shown = json!({"name": obj.name, "namespace": obj.ns, "type": o["type"], "data": data});
    Ok(serde_yaml::to_string(&shown)?.lines().map(str::to_string).collect())
}

pub async fn delete(ep: &Endpoint, objs: &[ObjRef], propagation: Propagation, force: bool) -> Result<String> {
    let mut opts = json!({"kind": "DeleteOptions", "apiVersion": "v1", "propagationPolicy": propagation.as_str()});
    if force {
        opts["gracePeriodSeconds"] = json!(0);
    }
    let mut done = Vec::new();
    let mut failed = Vec::new();
    for o in objs {
        match ep.json(Method::DELETE, &o.res.object_path(&o.ns, &o.name), Some(&opts)).await {
            Ok(_) => done.push(o.name.clone()),
            Err(e) => failed.push(format!("{}: {e:#}", o.name)),
        }
    }
    if !failed.is_empty() {
        bail!("deleted {}; failed: {}", done.len(), failed.join("; "));
    }
    Ok(if done.len() == 1 { format!("{} deleted", objs[0].label()) } else { format!("{} objects deleted", done.len()) })
}

pub async fn scale(ep: &Endpoint, obj: &ObjRef, replicas: i64) -> Result<String> {
    let path = format!("{}/scale", obj.res.object_path(&obj.ns, &obj.name));
    let patch = json!({"spec": {"replicas": replicas}});
    if let Err(e) = ep.json_typed(Method::PATCH, &path, "application/merge-patch+json", Some(&patch)).await {
        // Servers without the scale subresource: the object itself.
        ep.json_typed(Method::PATCH, &obj.res.object_path(&obj.ns, &obj.name), "application/merge-patch+json", Some(&patch)).await.map_err(|e2| e2.context(format!("{e:#}")))?;
    }
    Ok(format!("{} scaled to {replicas}", obj.label()))
}

pub async fn restart(ep: &Endpoint, objs: &[ObjRef]) -> Result<String> {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let patch = json!({"spec": {"template": {"metadata": {"annotations": {"kubectl.kubernetes.io/restartedAt": now}}}}});
    for o in objs {
        ep.json_typed(Method::PATCH, &o.res.object_path(&o.ns, &o.name), "application/merge-patch+json", Some(&patch)).await.with_context(|| o.label())?;
    }
    Ok(if objs.len() == 1 { format!("{} restarting", objs[0].label()) } else { format!("{} workloads restarting", objs.len()) })
}

fn revision(o: &Value) -> i64 {
    o.pointer("/metadata/annotations/deployment.kubernetes.io~1revision").and_then(Value::as_str).and_then(|r| r.parse().ok()).unwrap_or(0)
}

/// A deployment's ReplicaSets, newest revision first.
async fn replicasets(ep: &Endpoint, obj: &ObjRef) -> Result<(Value, Vec<Value>)> {
    let d = get(ep, obj).await?;
    let uid = d.pointer("/metadata/uid").and_then(Value::as_str).unwrap_or_default().to_string();
    let sel = selector_string(&d["spec"]["selector"]);
    let list = ep.json(Method::GET, &format!("/apis/apps/v1/namespaces/{}/replicasets?labelSelector={}", obj.ns, enc(&sel)), None).await?;
    let mut rs: Vec<Value> = list["items"].as_array().into_iter().flatten().filter(|r| r.pointer("/metadata/ownerReferences").and_then(Value::as_array).is_some_and(|a| a.iter().any(|o| o["uid"].as_str() == Some(&uid)))).cloned().collect();
    rs.sort_by_key(|r| std::cmp::Reverse(revision(r)));
    Ok((d, rs))
}

/// `kubectl rollout undo`: the previous revision's pod template.
pub async fn undo(ep: &Endpoint, obj: &ObjRef) -> Result<String> {
    let (_, rs) = replicasets(ep, obj).await?;
    let prev = rs.get(1).context("there is no previous revision")?;
    let mut tpl = prev["spec"]["template"].clone();
    if let Some(l) = tpl.pointer_mut("/metadata/labels").and_then(Value::as_object_mut) {
        l.remove("pod-template-hash");
    }
    let patch = json!([{"op": "replace", "path": "/spec/template", "value": tpl}]);
    ep.json_typed(Method::PATCH, &obj.res.object_path(&obj.ns, &obj.name), "application/json-patch+json", Some(&patch)).await?;
    Ok(format!("{} rolled back to revision {}", obj.label(), revision(prev)))
}

/// `kubectl rollout history` and status, as lines.
pub async fn history(ep: &Endpoint, obj: &ObjRef) -> Result<Vec<String>> {
    let (d, rs) = replicasets(ep, obj).await?;
    let n = |p: &str| d.pointer(p).and_then(Value::as_i64).unwrap_or(0);
    let want = d.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or(1);
    let mut out = vec![format!("Status: {}", rollout_status(&d)), format!("Replicas: {want} desired, {} updated, {} ready, {} available", n("/status/updatedReplicas"), n("/status/readyReplicas"), n("/status/availableReplicas")), String::new()];
    let now = chrono::Utc::now().timestamp();
    let rows: Vec<Vec<String>> = rs
        .iter()
        .map(|r| {
            let images: Vec<String> = r.pointer("/spec/template/spec/containers").and_then(Value::as_array).into_iter().flatten().filter_map(|c| c["image"].as_str().map(str::to_string)).collect();
            let cause = r.pointer("/metadata/annotations/kubernetes.io~1change-cause").and_then(Value::as_str).unwrap_or("<none>").to_string();
            let created = r.pointer("/metadata/creationTimestamp").and_then(Value::as_str).unwrap_or_default();
            vec![
                revision(r).to_string(),
                r.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string(),
                format!("{}/{}", r.pointer("/status/readyReplicas").and_then(Value::as_i64).unwrap_or(0), r.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or(0)),
                describe::since(now, created),
                images.join(","),
                cause,
            ]
        })
        .collect();
    let header = ["REVISION", "REPLICASET", "READY", "AGE", "IMAGES", "CHANGE-CAUSE"];
    let mut w: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.len());
        }
    }
    let line = |cells: Vec<String>| cells.iter().enumerate().map(|(i, c)| format!("{c:<w$}", w = w[i] + 2)).collect::<String>().trim_end().to_string();
    out.push(line(header.iter().map(|s| s.to_string()).collect()));
    for (i, r) in rows.into_iter().enumerate() {
        let mut l = line(r);
        if i == 0 {
            l.push_str("  (current)");
        }
        out.push(l);
    }
    out.push(String::new());
    out.push("u on the deployment rolls back to the revision before the current one.".into());
    Ok(out)
}

/// What `kubectl rollout status` says once.
pub fn rollout_status(d: &Value) -> String {
    let n = |p: &str| d.pointer(p).and_then(Value::as_i64).unwrap_or(0);
    let want = d.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or(1);
    if n("/metadata/generation") > n("/status/observedGeneration") {
        return "waiting for the rollout to be observed".into();
    }
    if let Some(c) = d.pointer("/status/conditions").and_then(Value::as_array).and_then(|a| a.iter().find(|c| c["type"] == "Progressing")) {
        if c["reason"] == "ProgressDeadlineExceeded" {
            return format!("rollout exceeded its progress deadline: {}", c["message"].as_str().unwrap_or_default());
        }
    }
    let (upd, total, avail) = (n("/status/updatedReplicas"), n("/status/replicas"), n("/status/availableReplicas"));
    if upd < want {
        return format!("waiting for rollout: {upd} of {want} new replicas updated");
    }
    if total > upd {
        return format!("waiting for rollout: {} old replicas pending termination", total - upd);
    }
    if avail < upd {
        return format!("waiting for rollout: {avail} of {upd} updated replicas available");
    }
    "successfully rolled out".into()
}

pub async fn cordon(ep: &Endpoint, nodes: &[String], on: bool) -> Result<String> {
    for n in nodes {
        ep.json_typed(Method::PATCH, &format!("/api/v1/nodes/{n}"), "application/merge-patch+json", Some(&json!({"spec": {"unschedulable": on}}))).await.with_context(|| n.clone())?;
    }
    Ok(format!("{} {}", nodes.join(", "), if on { "cordoned" } else { "uncordoned" }))
}

/// `kubectl drain --ignore-daemonsets --delete-emptydir-data`: cordon, then
/// evict every pod but DaemonSet and mirror pods, retrying evictions a
/// PodDisruptionBudget refuses for up to two minutes.
pub async fn drain(ep: &Endpoint, nodes: &[String], progress: impl Fn(String)) -> Result<String> {
    cordon(ep, nodes, true).await?;
    let mut evicted = 0;
    for n in nodes {
        let list = ep.json(Method::GET, &format!("/api/v1/pods?fieldSelector={}", enc(&format!("spec.nodeName={n}"))), None).await?;
        let pods: Vec<(String, String)> = list["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| !p.pointer("/metadata/ownerReferences").and_then(Value::as_array).is_some_and(|a| a.iter().any(|o| o["kind"] == "DaemonSet")))
            .filter(|p| p.pointer("/metadata/annotations/kubernetes.io~1config.mirror").is_none())
            .filter(|p| !matches!(p.pointer("/status/phase").and_then(Value::as_str), Some("Succeeded" | "Failed")))
            .map(|p| (p.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or_default().to_string(), p.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string()))
            .collect();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        for (ns, name) in pods {
            progress(format!("draining {n}: evicting {ns}/{name}"));
            loop {
                let body = json!({"apiVersion": "policy/v1", "kind": "Eviction", "metadata": {"name": name, "namespace": ns}});
                let resp = ep.send(Method::POST, &format!("/api/v1/namespaces/{ns}/pods/{name}/eviction"), Some("application/json"), serde_json::to_vec(&body)?).await?;
                let status = resp.status();
                let data = http_body_util::BodyExt::collect(resp.into_body()).await?.to_bytes();
                match status.as_u16() {
                    200..=299 | 404 => break,
                    429 if tokio::time::Instant::now() < deadline => tokio::time::sleep(std::time::Duration::from_secs(5)).await,
                    405 => {
                        // No eviction API: delete.
                        ep.json(Method::DELETE, &format!("/api/v1/namespaces/{ns}/pods/{name}"), None).await?;
                        break;
                    }
                    _ => crate::ui::client::check(status, &data).with_context(|| format!("evicting {ns}/{name}"))?,
                }
            }
            evicted += 1;
        }
    }
    Ok(format!("{} drained: {evicted} pods evicted", nodes.join(", ")))
}

/// `kubectl create job --from=cronjob/<name>`.
pub async fn trigger(ep: &Endpoint, obj: &ObjRef) -> Result<String> {
    let cj = get(ep, obj).await?;
    let suffix = format!("{:x}", chrono::Utc::now().timestamp() % 0xffffff);
    let base: String = obj.name.chars().take(52).collect();
    let name = format!("{base}-manual-{suffix}");
    let job = json!({
        "apiVersion": "batch/v1", "kind": "Job",
        "metadata": {
            "name": name, "namespace": obj.ns,
            "labels": cj.pointer("/spec/jobTemplate/metadata/labels").cloned().unwrap_or(json!({})),
            "annotations": {"cronjob.kubernetes.io/instantiate": "manual"},
            "ownerReferences": [{"apiVersion": cj["apiVersion"], "kind": "CronJob", "name": obj.name, "uid": cj.pointer("/metadata/uid"), "controller": true, "blockOwnerDeletion": true}],
        },
        "spec": cj.pointer("/spec/jobTemplate/spec").cloned().unwrap_or(json!({})),
    });
    ep.json(Method::POST, &format!("/apis/batch/v1/namespaces/{}/jobs", obj.ns), Some(&job)).await?;
    Ok(format!("job {}/{name} created", obj.ns))
}

/// A label selector (`metav1.LabelSelector` or a plain map) as text.
pub fn selector_string(sel: &Value) -> String {
    let Some(m) = sel.as_object() else { return String::new() };
    if !m.contains_key("matchLabels") && !m.contains_key("matchExpressions") {
        // A service's or replication controller's map.
        return m.iter().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default())).collect::<Vec<_>>().join(",");
    }
    let mut parts: Vec<String> = sel["matchLabels"].as_object().into_iter().flatten().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default())).collect();
    for e in sel["matchExpressions"].as_array().into_iter().flatten() {
        let key = e["key"].as_str().unwrap_or_default();
        let vals: Vec<&str> = e["values"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        parts.push(match e["operator"].as_str().unwrap_or_default() {
            "In" => format!("{key} in ({})", vals.join(",")),
            "NotIn" => format!("{key} notin ({})", vals.join(",")),
            "Exists" => key.to_string(),
            _ => format!("!{key}"),
        });
    }
    parts.join(",")
}

/// A workload's or service's pod selector.
pub fn pod_selector(o: &Value) -> Option<String> {
    let sel = o.pointer("/spec/selector")?;
    let s = selector_string(sel);
    (!s.is_empty()).then_some(s)
}

/// What Enter on a workload shows: (pods' namespace, selector, field selector, title).
pub async fn drill(ep: &Endpoint, obj: &ObjRef) -> Result<(Option<String>, Option<String>, Option<String>, String)> {
    if obj.res.kind == "Node" {
        return Ok((None, None, Some(format!("spec.nodeName={}", obj.name)), format!("pods on {}", obj.name)));
    }
    let o = get(ep, obj).await?;
    let sel = pod_selector(&o).with_context(|| format!("{} selects no pods", obj.label()))?;
    Ok((Some(obj.ns.clone()), Some(sel), None, format!("pods of {}", obj.label())))
}

pub async fn pods_of(ep: &Endpoint, obj: &ObjRef) -> Result<Vec<Value>> {
    let o = get(ep, obj).await?;
    if obj.res.kind == "Pod" {
        return Ok(vec![o]);
    }
    let sel = pod_selector(&o).with_context(|| format!("{} selects no pods", obj.label()))?;
    let list = ep.json(Method::GET, &format!("/api/v1/namespaces/{}/pods?labelSelector={}", obj.ns, enc(&sel)), None).await?;
    let mut pods: Vec<Value> = list["items"].as_array().cloned().unwrap_or_default();
    pods.sort_by(|a, b| a.pointer("/metadata/name").and_then(Value::as_str).cmp(&b.pointer("/metadata/name").and_then(Value::as_str)));
    Ok(pods)
}

pub fn containers(pod: &Value) -> Vec<ContainerInfo> {
    let mut out = Vec::new();
    for (list, statuses, kind) in [("containers", "containerStatuses", ""), ("initContainers", "initContainerStatuses", "init"), ("ephemeralContainers", "ephemeralContainerStatuses", "ephemeral")] {
        for c in pod.pointer(&format!("/spec/{list}")).and_then(Value::as_array).into_iter().flatten() {
            let name = c["name"].as_str().unwrap_or_default().to_string();
            let running = pod.pointer(&format!("/status/{statuses}")).and_then(Value::as_array).and_then(|a| a.iter().find(|s| s["name"].as_str() == Some(&name))).is_some_and(|s| s.pointer("/state/running").is_some());
            out.push(ContainerInfo { name, kind: kind.into(), running });
        }
    }
    out
}

/// Ports a forward can use: (port, name).
pub async fn ports(ep: &Endpoint, obj: &ObjRef) -> Result<Vec<(u16, String)>> {
    let o = get(ep, obj).await?;
    let from_containers = |spec: &Value| -> Vec<(u16, String)> {
        spec["containers"].as_array().into_iter().flatten().flat_map(|c| c["ports"].as_array().cloned().unwrap_or_default()).filter_map(|p| Some((p["containerPort"].as_u64()? as u16, p["name"].as_str().unwrap_or_default().to_string()))).collect()
    };
    Ok(match obj.res.kind.as_str() {
        "Pod" => from_containers(&o["spec"]),
        "Service" => o["spec"]["ports"].as_array().into_iter().flatten().filter_map(|p| Some((p["port"].as_u64()? as u16, p["name"].as_str().unwrap_or_default().to_string()))).collect(),
        _ => from_containers(&o["spec"]["template"]["spec"]),
    })
}

fn ready(p: &Value) -> bool {
    p.pointer("/status/conditions").and_then(Value::as_array).is_some_and(|c| c.iter().any(|c| c["type"] == "Ready" && c["status"] == "True")) && p.pointer("/metadata/deletionTimestamp").is_none()
}

/// A container port by number or name.
fn container_port(pod: &Value, port: &str) -> Option<u16> {
    if let Ok(n) = port.parse::<u16>() {
        return Some(n);
    }
    pod["spec"]["containers"].as_array()?.iter().flat_map(|c| c["ports"].as_array().cloned().unwrap_or_default()).find(|p| p["name"].as_str() == Some(port)).and_then(|p| p["containerPort"].as_u64()).map(|n| n as u16)
}

/// Where a forward goes: (pod, its port) for a pod, a service's port or a
/// workload's.
pub async fn forward_target(ep: &Endpoint, obj: &ObjRef, remote: &str) -> Result<(String, u16)> {
    if obj.res.kind == "Pod" {
        let pod = get(ep, obj).await?;
        let port = container_port(&pod, remote).with_context(|| format!("{} has no port {remote}", obj.label()))?;
        return Ok((obj.name.clone(), port));
    }
    let o = get(ep, obj).await?;
    // A service's port maps to its target port on the pods.
    let mut want = remote.to_string();
    if obj.res.kind == "Service" {
        let p = o["spec"]["ports"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| p["port"].as_u64().map(|n| n.to_string()).as_deref() == Some(remote) || p["name"].as_str() == Some(remote))
            .with_context(|| format!("{} has no port {remote}", obj.label()))?;
        want = match &p["targetPort"] {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            _ => p["port"].to_string(),
        };
    }
    let pods = pods_of(ep, obj).await?;
    let pod = pods.iter().find(|p| ready(p)).or(pods.iter().find(|p| p.pointer("/status/phase").and_then(Value::as_str) == Some("Running"))).with_context(|| format!("{} has no running pod", obj.label()))?;
    let name = pod.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string();
    let port = container_port(pod, &want).with_context(|| format!("pod {name} has no port {want}"))?;
    Ok((name, port))
}

// ---- xray ----

async fn list_json(ep: &Endpoint, path: &str) -> Vec<Value> {
    ep.json(Method::GET, path, None).await.ok().and_then(|l| l["items"].as_array().cloned()).unwrap_or_default()
}

fn ns_path(group_version: &str, plural: &str, ns: Option<&str>) -> String {
    let prefix = if group_version == "v1" { "/api/v1".to_string() } else { format!("/apis/{group_version}") };
    match ns {
        Some(n) => format!("{prefix}/namespaces/{n}/{plural}"),
        None => format!("{prefix}/{plural}"),
    }
}

fn meta<'a>(o: &'a Value, k: &str) -> &'a str {
    o["metadata"][k].as_str().unwrap_or_default()
}

fn owned_by<'a>(items: &'a [Value], uid: &str) -> Vec<&'a Value> {
    items.iter().filter(|o| o.pointer("/metadata/ownerReferences").and_then(Value::as_array).is_some_and(|a| a.iter().any(|r| r["uid"].as_str() == Some(uid)))).collect()
}

fn pod_node(p: &Value, depth: u8, res: &Resource, out: &mut Vec<XNode>) {
    let m = crate::ui::model::pod(p);
    let health = match m.status.as_str() {
        "Running" if m.ready.split('/').next() == m.ready.split('/').nth(1) => Health::Ok,
        "Completed" | "Succeeded" => Health::Done,
        "Running" | "Pending" | "ContainerCreating" | "PodInitializing" | "Terminating" => Health::Warn,
        _ => Health::Bad,
    };
    let restarts = if m.restarts > 0 { format!(", {} restarts", m.restarts) } else { String::new() };
    out.push(XNode { depth, kind: "pod".into(), name: m.name.clone(), status: format!("{} {}{restarts}", m.status, m.ready), health, obj: Some(ObjRef { res: res.clone(), ns: m.namespace.clone(), name: m.name.clone() }), container_of: None });
    for (list, statuses) in [("initContainers", "initContainerStatuses"), ("containers", "containerStatuses")] {
        for c in p.pointer(&format!("/spec/{list}")).and_then(Value::as_array).into_iter().flatten() {
            let name = c["name"].as_str().unwrap_or_default();
            let st = p.pointer(&format!("/status/{statuses}")).and_then(Value::as_array).and_then(|a| a.iter().find(|s| s["name"].as_str() == Some(name))).cloned().unwrap_or(Value::Null);
            let state = st["state"].as_object().and_then(|m| m.iter().next()).map(|(k, v)| format!("{k}{}", v["reason"].as_str().map(|r| format!(" ({r})")).unwrap_or_default())).unwrap_or_else(|| "waiting".into());
            let ready = st["ready"].as_bool() == Some(true);
            let health = if state.starts_with("running") && ready {
                Health::Ok
            } else if state.contains("Completed") {
                Health::Done
            } else if state.starts_with("running") || state == "waiting" || state.contains("ContainerCreating") || state.contains("PodInitializing") {
                Health::Warn
            } else {
                Health::Bad
            };
            let kind = if list == "initContainers" { "init" } else { "container" };
            out.push(XNode {
                depth: depth + 1,
                kind: kind.into(),
                name: name.into(),
                status: format!("{state}, {}{}", if ready { "ready" } else { "not ready" }, c["image"].as_str().map(|i| format!(", {i}")).unwrap_or_default()),
                health,
                obj: None,
                container_of: Some((m.namespace.clone(), m.name.clone())),
            });
        }
    }
}

fn workload_health(o: &Value) -> (Health, String) {
    let n = |p: &str| o.pointer(p).and_then(Value::as_i64).unwrap_or(0);
    let want = o.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or_else(|| n("/status/desiredNumberScheduled"));
    let ready = o.pointer("/status/readyReplicas").and_then(Value::as_i64).unwrap_or_else(|| n("/status/numberReady"));
    (if ready >= want { Health::Ok } else { Health::Warn }, format!("{ready}/{want} ready"))
}

/// Owner trees: each `res` object, what it owns down to pods, and their
/// containers with their state.
pub async fn xray(ep: &Endpoint, catalog_pods: &Resource, find: &HashMap<String, Resource>, res: &Resource, ns: Option<&str>) -> Result<Vec<XNode>> {
    let gv = res.api_version();
    let roots = list_json(ep, &ns_path(&gv, &res.plural, ns.filter(|_| res.namespaced))).await;
    if super::gitops::is_app(res) {
        // Apps down their resources, child apps under their parent.
        let mut apps = roots;
        apps.sort_by(|a, b| (meta(a, "namespace"), meta(a, "name")).cmp(&(meta(b, "namespace"), meta(b, "name"))));
        let by_kind = |group: &str, kind: &str| find.get(&format!("{group}/{kind}")).cloned();
        for a in apps.iter_mut() {
            super::gitops::fill_health(ep, a, &by_kind).await;
        }
        return Ok(super::gitops::tree(&apps, res, &by_kind, ns.is_none()));
    }
    let need_pods = !matches!(res.kind.as_str(), "Namespace");
    let pods = if need_pods { list_json(ep, &ns_path("v1", "pods", ns)).await } else { Vec::new() };
    let rss = if res.kind == "Deployment" { list_json(ep, &ns_path("apps/v1", "replicasets", ns)).await } else { Vec::new() };
    let jobs = if res.kind == "CronJob" { list_json(ep, &ns_path("batch/v1", "jobs", ns)).await } else { Vec::new() };
    let mut out = Vec::new();
    let mut roots = roots;
    roots.sort_by(|a, b| (meta(a, "namespace"), meta(a, "name")).cmp(&(meta(b, "namespace"), meta(b, "name"))));
    let child = |kind: &str| find.get(kind).cloned();
    for r in &roots {
        let name = if ns.is_none() && !meta(r, "namespace").is_empty() { format!("{}/{}", meta(r, "namespace"), meta(r, "name")) } else { meta(r, "name").to_string() };
        let (health, status) = workload_health(r);
        let root_obj = ObjRef { res: res.clone(), ns: meta(r, "namespace").into(), name: meta(r, "name").into() };
        let kind_label = res.short.first().cloned().unwrap_or_else(|| res.singular.clone());
        match res.kind.as_str() {
            "Deployment" => {
                out.push(XNode { depth: 0, kind: kind_label, name, status, health, obj: Some(root_obj), container_of: None });
                let mut mine = owned_by(&rss, meta(r, "uid"));
                mine.sort_by_key(|x| std::cmp::Reverse(revision(x)));
                for rs in mine {
                    let rs_pods = owned_by(&pods, meta(rs, "uid"));
                    let want = rs.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or(0);
                    if want == 0 && rs_pods.is_empty() {
                        continue;
                    }
                    let (h, s) = workload_health(rs);
                    out.push(XNode { depth: 1, kind: "rs".into(), name: meta(rs, "name").into(), status: format!("{s}, revision {}", revision(rs)), health: h, obj: child("ReplicaSet").map(|res| ObjRef { res, ns: meta(rs, "namespace").into(), name: meta(rs, "name").into() }), container_of: None });
                    for p in rs_pods {
                        pod_node(p, 2, catalog_pods, &mut out);
                    }
                }
            }
            "CronJob" => {
                let last = r.pointer("/status/lastScheduleTime").and_then(Value::as_str).map(|t| format!("last run {} ago", describe::since(chrono::Utc::now().timestamp(), t))).unwrap_or_else(|| "never run".into());
                let suspended = r.pointer("/spec/suspend").and_then(Value::as_bool) == Some(true);
                out.push(XNode { depth: 0, kind: kind_label, name, status: format!("{} {last}{}", r.pointer("/spec/schedule").and_then(Value::as_str).unwrap_or_default(), if suspended { ", suspended" } else { "" }), health: if suspended { Health::Done } else { Health::Ok }, obj: Some(root_obj), container_of: None });
                for j in owned_by(&jobs, meta(r, "uid")) {
                    let failed = j.pointer("/status/failed").and_then(Value::as_i64).unwrap_or(0);
                    let ok = j.pointer("/status/succeeded").and_then(Value::as_i64).unwrap_or(0);
                    out.push(XNode { depth: 1, kind: "job".into(), name: meta(j, "name").into(), status: format!("{ok} succeeded, {failed} failed"), health: if failed > 0 { Health::Bad } else if ok > 0 { Health::Done } else { Health::Warn }, obj: child("Job").map(|res| ObjRef { res, ns: meta(j, "namespace").into(), name: meta(j, "name").into() }), container_of: None });
                    for p in owned_by(&pods, meta(j, "uid")) {
                        pod_node(p, 2, catalog_pods, &mut out);
                    }
                }
            }
            "Service" => {
                let sel = r.pointer("/spec/selector").map(selector_string).unwrap_or_default();
                out.push(XNode { depth: 0, kind: kind_label, name, status: format!("{} {}", r.pointer("/spec/type").and_then(Value::as_str).unwrap_or_default(), r.pointer("/spec/clusterIP").and_then(Value::as_str).unwrap_or_default()), health: Health::Ok, obj: Some(root_obj), container_of: None });
                if let Ok(s) = crate::kube::selector::LabelSelector::parse(&sel) {
                    if !sel.is_empty() {
                        for p in pods.iter().filter(|p| meta(p, "namespace") == meta(r, "namespace") && s.matches(p)) {
                            pod_node(p, 1, catalog_pods, &mut out);
                        }
                    }
                }
            }
            "Node" => {
                let ready = r.pointer("/status/conditions").and_then(Value::as_array).is_some_and(|c| c.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"));
                out.push(XNode { depth: 0, kind: kind_label, name, status: if ready { "Ready".into() } else { "NotReady".into() }, health: if ready { Health::Ok } else { Health::Bad }, obj: Some(root_obj), container_of: None });
                for p in pods.iter().filter(|p| p.pointer("/spec/nodeName").and_then(Value::as_str) == Some(meta(r, "name"))) {
                    pod_node(p, 1, catalog_pods, &mut out);
                }
            }
            _ => {
                out.push(XNode { depth: 0, kind: kind_label, name, status, health, obj: Some(root_obj), container_of: None });
                for p in owned_by(&pods, meta(r, "uid")) {
                    pod_node(p, 1, catalog_pods, &mut out);
                }
            }
        }
    }
    Ok(out)
}

/// `metrics.k8s.io` usage by row key: (cpu, memory) as `kubectl top`.
pub async fn metrics(ep: &Endpoint, nodes: bool, ns: Option<&str>) -> Result<(BTreeMap<String, String>, BTreeMap<String, String>)> {
    let path = match (nodes, ns) {
        (true, _) => "/apis/metrics.k8s.io/v1beta1/nodes".to_string(),
        (false, Some(n)) => format!("/apis/metrics.k8s.io/v1beta1/namespaces/{n}/pods"),
        (false, None) => "/apis/metrics.k8s.io/v1beta1/pods".to_string(),
    };
    let list = ep.json(Method::GET, &path, None).await?;
    let (mut cpu, mut mem) = (BTreeMap::new(), BTreeMap::new());
    for i in list["items"].as_array().into_iter().flatten() {
        let key = super::table::key(meta(i, "namespace"), meta(i, "name"));
        let usage: Vec<&Value> = if nodes { vec![&i["usage"]] } else { i["containers"].as_array().into_iter().flatten().map(|c| &c["usage"]).collect() };
        let c: f64 = usage.iter().filter_map(|u| u["cpu"].as_str().and_then(super::table::quantity)).sum();
        let m: f64 = usage.iter().filter_map(|u| u["memory"].as_str().and_then(super::table::quantity)).sum();
        cpu.insert(key.clone(), format!("{}m", (c * 1000.0).round()));
        mem.insert(key, format!("{}Mi", (m / 1048576.0).round()));
    }
    Ok((cpu, mem))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_as_text() {
        assert_eq!(selector_string(&json!({"matchLabels": {"app": "web"}, "matchExpressions": [{"key": "tier", "operator": "In", "values": ["a", "b"]}, {"key": "x", "operator": "DoesNotExist"}]})), "app=web,tier in (a,b),!x");
        assert_eq!(selector_string(&json!({"app": "web", "tier": "front"})), "app=web,tier=front");
        assert_eq!(pod_selector(&json!({"spec": {}})), None);
    }

    #[test]
    fn rollout_status_reads_like_kubectl() {
        let d = |upd: i64, total: i64, avail: i64| json!({"metadata": {"generation": 2}, "spec": {"replicas": 3}, "status": {"observedGeneration": 2, "updatedReplicas": upd, "replicas": total, "availableReplicas": avail}});
        assert_eq!(rollout_status(&d(1, 3, 3)), "waiting for rollout: 1 of 3 new replicas updated");
        assert_eq!(rollout_status(&d(3, 4, 3)), "waiting for rollout: 1 old replicas pending termination");
        assert_eq!(rollout_status(&d(3, 3, 2)), "waiting for rollout: 2 of 3 updated replicas available");
        assert_eq!(rollout_status(&d(3, 3, 3)), "successfully rolled out");
    }

    #[test]
    fn container_ports_by_name() {
        let pod = json!({"spec": {"containers": [{"name": "a", "ports": [{"name": "http", "containerPort": 8080}]}]}});
        assert_eq!(container_port(&pod, "http"), Some(8080));
        assert_eq!(container_port(&pod, "9000"), Some(9000));
        assert_eq!(container_port(&pod, "grpc"), None);
        let infos = containers(&json!({"spec": {"containers": [{"name": "a"}], "initContainers": [{"name": "i"}]}, "status": {"containerStatuses": [{"name": "a", "state": {"running": {}}}]}}));
        assert_eq!(infos, vec![ContainerInfo { name: "a".into(), kind: String::new(), running: true }, ContainerInfo { name: "i".into(), kind: "init".into(), running: false }]);
    }
}
