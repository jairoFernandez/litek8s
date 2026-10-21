//! `litek8s tree` on a Kubernetes cluster: nodes, then each namespace's
//! workloads down their ownerReferences (Deployment → ReplicaSet → Pod,
//! StatefulSet/DaemonSet/Job → Pod, CronJob → Job → Pod) and its Services
//! with the pods they select. The legacy daemon's tree is in cli.rs.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use hyper::Method;
use serde_json::Value;

use crate::ui::client::Endpoint;

#[derive(Debug, clap::Args)]
pub struct TreeArgs {
    #[arg(long, help = "kubeconfig context (tree of a Kubernetes cluster rather than the legacy daemon)")]
    pub context: Option<String>,
    #[arg(short = 'n', long, help = "Only this namespace (default: all)")]
    pub namespace: Option<String>,
    #[arg(long, help = "Also old ReplicaSets with no replicas")]
    pub all: bool,
    #[arg(long, help = "Print once, also on a terminal (piped output always prints once)")]
    pub once: bool,
}

/// The lists a tree is drawn from, as the API returns them (`items`).
#[derive(Debug, Default)]
pub struct Objects {
    pub nodes: Vec<Value>,
    pub pods: Vec<Value>,
    pub replicasets: Vec<Value>,
    pub deployments: Vec<Value>,
    pub statefulsets: Vec<Value>,
    pub daemonsets: Vec<Value>,
    pub jobs: Vec<Value>,
    pub cronjobs: Vec<Value>,
    pub services: Vec<Value>,
}

const LISTS: [(&str, &str); 9] = [
    ("nodes", "/api/v1/nodes"),
    ("pods", "/api/v1/pods"),
    ("replicasets", "/apis/apps/v1/replicasets"),
    ("deployments", "/apis/apps/v1/deployments"),
    ("statefulsets", "/apis/apps/v1/statefulsets"),
    ("daemonsets", "/apis/apps/v1/daemonsets"),
    ("jobs", "/apis/batch/v1/jobs"),
    ("cronjobs", "/apis/batch/v1/cronjobs"),
    ("services", "/api/v1/services"),
];

pub async fn fetch(ep: &Endpoint, namespace: Option<&str>) -> Result<Objects> {
    let mut o = Objects::default();
    for (what, path) in LISTS {
        let path = match namespace {
            Some(ns) if what != "nodes" => {
                let (group, kind) = path.rsplit_once('/').unwrap_or(("", path));
                format!("{group}/namespaces/{ns}/{kind}")
            }
            _ => path.to_string(),
        };
        let items = ep.json(Method::GET, &path, None).await?["items"].as_array().cloned().unwrap_or_default();
        *match what {
            "nodes" => &mut o.nodes,
            "pods" => &mut o.pods,
            "replicasets" => &mut o.replicasets,
            "deployments" => &mut o.deployments,
            "statefulsets" => &mut o.statefulsets,
            "daemonsets" => &mut o.daemonsets,
            "jobs" => &mut o.jobs,
            "cronjobs" => &mut o.cronjobs,
            _ => &mut o.services,
        } = items;
    }
    Ok(o)
}

pub async fn frame(ep: &Endpoint, a: &TreeArgs) -> Result<String> {
    let objects = fetch(ep, a.namespace.as_deref()).await?;
    Ok(render(&ep.display, &objects, a.all, a.namespace.as_deref()))
}

fn s<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or_default()
}

fn n(v: &Value, ptr: &str) -> i64 {
    v.pointer(ptr).and_then(Value::as_i64).unwrap_or(0)
}

/// A pod's state as `kubectl get pods` shows it: a waiting or terminated
/// container's reason wins over the phase.
fn pod_status(p: &Value) -> String {
    if p.pointer("/metadata/deletionTimestamp").is_some() {
        return "Terminating".into();
    }
    for c in p.pointer("/status/containerStatuses").and_then(Value::as_array).into_iter().flatten() {
        if let Some(r) = c.pointer("/state/waiting/reason").and_then(Value::as_str) {
            return r.into();
        }
        if let Some(r) = c.pointer("/state/terminated/reason").and_then(Value::as_str)
            && s(p, "/status/phase") == "Running"
        {
            return r.into();
        }
    }
    match s(p, "/status/phase") {
        "" => "Pending".into(),
        phase => phase.into(),
    }
}

fn restarts(p: &Value) -> i64 {
    p.pointer("/status/containerStatuses").and_then(Value::as_array).into_iter().flatten().map(|c| n(c, "/restartCount")).sum()
}

fn label(kind: &str, o: &Value) -> String {
    let name = s(o, "/metadata/name");
    let detail = match kind {
        "Deployment" | "ReplicaSet" | "StatefulSet" => {
            let want = o.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or(1);
            format!("{}/{} ready", n(o, "/status/readyReplicas"), want)
        }
        "DaemonSet" => format!("{}/{} ready", n(o, "/status/numberReady"), n(o, "/status/desiredNumberScheduled")),
        "Job" => {
            let want = o.pointer("/spec/completions").and_then(Value::as_i64).unwrap_or(1);
            let failed = n(o, "/status/failed");
            let mut d = format!("{}/{} done", n(o, "/status/succeeded"), want);
            if failed > 0 {
                d.push_str(&format!(", {failed} failed"));
            }
            d
        }
        "CronJob" => {
            let mut d = format!("schedule {}", s(o, "/spec/schedule"));
            if o.pointer("/spec/suspend").and_then(Value::as_bool) == Some(true) {
                d.push_str(", suspended");
            }
            d
        }
        "Pod" => {
            let mut d = pod_status(o);
            let node = s(o, "/spec/nodeName");
            if !node.is_empty() {
                d.push_str(&format!(" on {node}"));
            }
            let r = restarts(o);
            if r > 0 {
                d.push_str(&format!(", {r} restart{}", if r == 1 { "" } else { "s" }));
            }
            d
        }
        _ => String::new(),
    };
    format!("{kind} {name}  {detail}")
}

/// Whether a Service's selector picks a pod.
fn selects(svc: &Value, pod: &Value) -> bool {
    let Some(sel) = svc.pointer("/spec/selector").and_then(Value::as_object).filter(|m| !m.is_empty()) else {
        return false;
    };
    sel.iter().all(|(k, v)| pod.pointer("/metadata/labels").and_then(|l| l.get(k)) == Some(v))
}

struct Node {
    label: String,
    children: Vec<Node>,
}

fn draw(out: &mut String, nodes: &[Node], prefix: &str) {
    for (i, node) in nodes.iter().enumerate() {
        let last = i + 1 == nodes.len();
        out.push_str(prefix);
        out.push_str(if last { "└─ " } else { "├─ " });
        out.push_str(&node.label);
        out.push('\n');
        draw(out, &node.children, &format!("{prefix}{}", if last { "   " } else { "│  " }));
    }
}

pub fn render(server: &str, o: &Objects, all: bool, namespace: Option<&str>) -> String {
    // Objects by uid, and each owner's children (by kind, then name).
    let mut kinds: Vec<(&str, &Vec<Value>)> = vec![
        ("CronJob", &o.cronjobs),
        ("Deployment", &o.deployments),
        ("StatefulSet", &o.statefulsets),
        ("DaemonSet", &o.daemonsets),
        ("Job", &o.jobs),
        ("ReplicaSet", &o.replicasets),
        ("Pod", &o.pods),
    ];
    let mut by_uid: HashMap<&str, (&str, &Value)> = HashMap::new();
    for (kind, list) in &kinds {
        for obj in list.iter() {
            by_uid.insert(s(obj, "/metadata/uid"), (kind, obj));
        }
    }
    let mut children: HashMap<&str, Vec<(&str, &Value)>> = HashMap::new();
    let mut roots: BTreeMap<&str, Vec<(&str, &Value)>> = BTreeMap::new();
    for (kind, list) in kinds.drain(..) {
        for obj in list {
            // An old ReplicaSet scaled to zero is history, not a workload.
            if kind == "ReplicaSet" && !all && n(obj, "/spec/replicas") == 0 && n(obj, "/status/replicas") == 0 {
                continue;
            }
            let owner = obj
                .pointer("/metadata/ownerReferences")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|r| r["controller"].as_bool() == Some(true))
                .and_then(|r| r["uid"].as_str())
                .filter(|uid| by_uid.contains_key(uid));
            match owner {
                Some(uid) => children.entry(uid).or_default().push((kind, obj)),
                None => roots.entry(s(obj, "/metadata/namespace")).or_default().push((kind, obj)),
            }
        }
    }
    fn build<'a>(kind: &'a str, obj: &'a Value, children: &HashMap<&'a str, Vec<(&'a str, &'a Value)>>) -> Node {
        let mut kids: Vec<&(&str, &Value)> = children.get(s(obj, "/metadata/uid")).map(|v| v.iter().collect()).unwrap_or_default();
        kids.sort_by_key(|(k, o)| (*k, s(o, "/metadata/name")));
        Node { label: label(kind, obj), children: kids.into_iter().map(|(k, o)| build(k, o, children)).collect() }
    }

    let mut namespaces: BTreeMap<&str, Vec<Node>> = BTreeMap::new();
    for (ns, list) in &roots {
        let mut list: Vec<&(&str, &Value)> = list.iter().collect();
        list.sort_by_key(|(k, o)| (*k, s(o, "/metadata/name")));
        namespaces.entry(ns).or_default().extend(list.into_iter().map(|(k, obj)| build(k, obj, &children)));
    }
    for svc in &o.services {
        let ns = s(svc, "/metadata/namespace");
        let picked: Vec<&Value> = o.pods.iter().filter(|p| s(p, "/metadata/namespace") == ns && selects(svc, p)).collect();
        let ready = picked.iter().filter(|p| pod_status(p) == "Running").count();
        let ports: Vec<String> = svc
            .pointer("/spec/ports")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|p| match p["nodePort"].as_i64() {
                Some(np) => format!("{}:{np}", p["port"]),
                None => p["port"].to_string(),
            })
            .collect();
        let mut detail = format!("{} {}", s(svc, "/spec/type"), s(svc, "/spec/clusterIP"));
        if !ports.is_empty() {
            detail.push_str(&format!(" {}", ports.join(",")));
        }
        if svc.pointer("/spec/selector").and_then(Value::as_object).is_some_and(|m| !m.is_empty()) {
            detail.push_str(&format!(" → {ready}/{} pods", picked.len()));
        }
        namespaces.entry(ns).or_default().push(Node { label: format!("Service {}  {detail}", s(svc, "/metadata/name")), children: Vec::new() });
    }

    let mut nodes: Vec<Node> = Vec::new();
    let mut cluster_nodes: Vec<Node> = o
        .nodes
        .iter()
        .map(|node| {
            let name = s(node, "/metadata/name");
            let ready = node
                .pointer("/status/conditions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|c| c["type"] == "Ready")
                .map(|c| if c["status"] == "True" { "Ready" } else { "NotReady" })
                .unwrap_or("Unknown");
            let pods = o.pods.iter().filter(|p| s(p, "/spec/nodeName") == name).count();
            let cidr = s(node, "/spec/podCIDR");
            let scope = namespace.map(|ns| format!(" in {ns}")).unwrap_or_default();
            let mut d = format!("Node {name}  {ready}, {pods} pod{}{scope}", if pods == 1 { "" } else { "s" });
            if !cidr.is_empty() {
                d.push_str(&format!(", pods {cidr}"));
            }
            Node { label: d, children: Vec::new() }
        })
        .collect();
    cluster_nodes.sort_by(|a, b| a.label.cmp(&b.label));
    if !cluster_nodes.is_empty() {
        nodes.push(Node { label: "nodes".into(), children: cluster_nodes });
    }
    for (ns, children) in namespaces {
        nodes.push(Node { label: format!("namespace {ns}"), children });
    }
    let mut out = format!("cluster {server}\n");
    draw(&mut out, &nodes, "");
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn owned(kind: &str, name: &str, uid: &str, owner: Option<&str>, extra: Value) -> Value {
        let mut v = json!({"kind": kind, "metadata": {"name": name, "namespace": "web", "uid": uid}});
        if let Some(o) = owner {
            v["metadata"]["ownerReferences"] = json!([{"uid": o, "controller": true}]);
        }
        merge(&mut v, extra);
        v
    }

    fn merge(dst: &mut Value, src: Value) {
        match (dst, src) {
            (Value::Object(d), Value::Object(s)) => {
                for (k, x) in s {
                    merge(d.entry(k).or_insert(Value::Null), x);
                }
            }
            (d, s) => *d = s,
        }
    }

    #[test]
    fn draws_owner_chains_services_and_nodes() {
        let o = Objects {
            nodes: vec![json!({"metadata": {"name": "n1"}, "spec": {"podCIDR": "10.42.0.0/24"}, "status": {"conditions": [{"type": "Ready", "status": "True"}]}})],
            deployments: vec![owned("Deployment", "api", "d1", None, json!({"spec": {"replicas": 2}, "status": {"readyReplicas": 1}}))],
            replicasets: vec![
                owned("ReplicaSet", "api-abc", "r1", Some("d1"), json!({"spec": {"replicas": 2}, "status": {"replicas": 2, "readyReplicas": 1}})),
                owned("ReplicaSet", "api-old", "r0", Some("d1"), json!({"spec": {"replicas": 0}, "status": {"replicas": 0}})),
            ],
            pods: vec![
                owned("Pod", "api-abc-1", "p1", Some("r1"), json!({"metadata": {"labels": {"app": "api"}}, "spec": {"nodeName": "n1"}, "status": {"phase": "Running", "containerStatuses": [{"restartCount": 0, "state": {"running": {}}}]}})),
                owned("Pod", "api-abc-2", "p2", Some("r1"), json!({"metadata": {"labels": {"app": "api"}}, "spec": {"nodeName": "n1"}, "status": {"phase": "Running", "containerStatuses": [{"restartCount": 3, "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}})),
                owned("Pod", "debug", "p3", None, json!({"status": {"phase": "Pending"}})),
            ],
            services: vec![json!({"metadata": {"name": "api", "namespace": "web"}, "spec": {"type": "NodePort", "clusterIP": "10.43.0.9", "selector": {"app": "api"}, "ports": [{"port": 80, "nodePort": 30080}]}})],
            ..Default::default()
        };
        let objects = o;
        let out = render("http://127.0.0.1:6443", &objects, false, None);
        assert_eq!(
            out,
            "cluster http://127.0.0.1:6443
├─ nodes
│  └─ Node n1  Ready, 2 pods, pods 10.42.0.0/24
└─ namespace web
   ├─ Deployment api  1/2 ready
   │  └─ ReplicaSet api-abc  1/2 ready
   │     ├─ Pod api-abc-1  Running on n1
   │     └─ Pod api-abc-2  CrashLoopBackOff on n1, 3 restarts
   ├─ Pod debug  Pending
   └─ Service api  NodePort 10.43.0.9 80:30080 → 1/2 pods
",
            "{out}"
        );
        assert!(render("x", &objects, true, None).contains("ReplicaSet api-old  0/0 ready"));
        assert!(render("x", &objects, false, Some("web")).contains("Node n1  Ready, 2 pods in web"));
    }

    #[test]
    fn a_namespace_filter_goes_into_every_list_but_nodes() {
        // The paths fetch() builds for -n.
        for (what, path) in LISTS {
            let (group, kind) = path.rsplit_once('/').unwrap();
            let namespaced = format!("{group}/namespaces/ns/{kind}");
            if what == "pods" {
                assert_eq!(namespaced, "/api/v1/namespaces/ns/pods");
            }
            if what == "cronjobs" {
                assert_eq!(namespaced, "/apis/batch/v1/namespaces/ns/cronjobs");
            }
        }
    }
}
