//! `litek8s tree` on a Kubernetes cluster: nodes, then each namespace's
//! workloads down their ownerReferences (Deployment → ReplicaSet → Pod,
//! StatefulSet/DaemonSet/Job → Pod, CronJob → Job → Pod) and its Services
//! with the pods they select. The legacy daemon's tree is in cli.rs.

use std::collections::{BTreeMap, HashMap};

use crate::ui::theme::Health;

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

/// The kinds a tree has, in the order a namespace lists them (by name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum Kind {
    CronJob,
    DaemonSet,
    #[default]
    Deployment,
    Job,
    Node,
    Pod,
    ReplicaSet,
    Service,
    StatefulSet,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::CronJob => "CronJob",
            Kind::DaemonSet => "DaemonSet",
            Kind::Deployment => "Deployment",
            Kind::Job => "Job",
            Kind::Node => "Node",
            Kind::Pod => "Pod",
            Kind::ReplicaSet => "ReplicaSet",
            Kind::Service => "Service",
            Kind::StatefulSet => "StatefulSet",
        }
    }

    /// The API's plural (the UI's resource name).
    pub fn plural(self) -> &'static str {
        match self {
            Kind::CronJob => "cronjobs",
            Kind::DaemonSet => "daemonsets",
            Kind::Deployment => "deployments",
            Kind::Job => "jobs",
            Kind::Node => "nodes",
            Kind::Pod => "pods",
            Kind::ReplicaSet => "replicasets",
            Kind::Service => "services",
            Kind::StatefulSet => "statefulsets",
        }
    }

    pub const ALL: [Kind; 9] = [Kind::Node, Kind::Pod, Kind::ReplicaSet, Kind::Deployment, Kind::StatefulSet, Kind::DaemonSet, Kind::Job, Kind::CronJob, Kind::Service];
}

/// One object as a tree needs it: from the API's objects (`litek8s tree`)
/// or from a UI's table rows (`litek8s ui`'s `:tree`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Item {
    pub kind: Kind,
    pub ns: String,
    pub name: String,
    pub uid: String,
    /// The controlling owner's uid.
    pub owner: Option<String>,
    /// What follows the name: `1/2 ready`, `Running on n1, 3 restarts`; a
    /// service's type, IP and ports; a node's Ready condition.
    pub detail: String,
    pub health: Health,
    /// A pod's labels (services pick pods by them).
    pub labels: BTreeMap<String, String>,
    /// A service's selector (empty: it selects nothing itself).
    pub selector: BTreeMap<String, String>,
    /// A pod's node.
    pub node: String,
    /// A pod that counts as ready for a service.
    pub running: bool,
    /// An old ReplicaSet scaled to zero (history, not a workload).
    pub history: bool,
    /// A node's pod CIDR.
    pub cidr: String,
}

/// A line of a tree, in order: `depth` 0 for the `nodes` and `namespace
/// <ns>` groups, 1 for what is under them, and so on.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub depth: usize,
    /// `None` for a group line.
    pub kind: Option<Kind>,
    pub ns: String,
    pub name: String,
    pub detail: String,
    pub health: Health,
}

impl Row {
    /// What the line says: `Deployment api  1/2 ready`, `namespace web`.
    pub fn text(&self) -> String {
        match self.kind {
            Some(k) => format!("{} {}  {}", k.name(), self.name, self.detail).trim_end().to_string(),
            None => self.name.clone(),
        }
    }

    /// A stable id (folds and the selection follow it).
    pub fn key(&self) -> String {
        match self.kind {
            Some(k) => format!("{}/{}/{}", k.name(), self.ns, self.name),
            None => format!("group/{}", self.name),
        }
    }
}

fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

impl Item {
    /// From an API object of `kind`.
    pub fn from_object(kind: Kind, o: &Value) -> Item {
        let strings = |p: &str| -> BTreeMap<String, String> { o.pointer(p).and_then(Value::as_object).into_iter().flatten().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect() };
        let mut it = Item {
            kind,
            ns: s(o, "/metadata/namespace").into(),
            name: s(o, "/metadata/name").into(),
            uid: s(o, "/metadata/uid").into(),
            owner: o.pointer("/metadata/ownerReferences").and_then(Value::as_array).into_iter().flatten().find(|r| r["controller"].as_bool() == Some(true)).and_then(|r| r["uid"].as_str()).map(str::to_string),
            ..Default::default()
        };
        match kind {
            Kind::Deployment | Kind::ReplicaSet | Kind::StatefulSet => {
                let want = o.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or(1);
                let ready = n(o, "/status/readyReplicas");
                it.detail = format!("{ready}/{want} ready");
                it.health = if ready >= want { Health::Ok } else { Health::Warn };
                it.history = kind == Kind::ReplicaSet && want == 0 && n(o, "/status/replicas") == 0;
            }
            Kind::DaemonSet => {
                let (ready, want) = (n(o, "/status/numberReady"), n(o, "/status/desiredNumberScheduled"));
                it.detail = format!("{ready}/{want} ready");
                it.health = if ready >= want { Health::Ok } else { Health::Warn };
            }
            Kind::Job => {
                let want = o.pointer("/spec/completions").and_then(Value::as_i64).unwrap_or(1);
                let (done, failed) = (n(o, "/status/succeeded"), n(o, "/status/failed"));
                it.detail = format!("{done}/{want} done");
                if failed > 0 {
                    it.detail.push_str(&format!(", {failed} failed"));
                }
                it.health = if failed > 0 && done < want { Health::Bad } else if done >= want { Health::Done } else { Health::Warn };
            }
            Kind::CronJob => {
                it.detail = format!("schedule {}", s(o, "/spec/schedule"));
                if o.pointer("/spec/suspend").and_then(Value::as_bool) == Some(true) {
                    it.detail.push_str(", suspended");
                    it.health = Health::Done;
                }
            }
            Kind::Pod => {
                let status = pod_status(o);
                let ready = o.pointer("/status/containerStatuses").and_then(Value::as_array).map(|c| (c.iter().filter(|x| x["ready"].as_bool() == Some(true)).count(), c.len())).unwrap_or((0, 0));
                it.health = Health::of_pod(&status, &format!("{}/{}", ready.0, ready.1));
                it.running = status == "Running";
                it.node = s(o, "/spec/nodeName").into();
                it.labels = strings("/metadata/labels");
                it.detail = pod_detail(&status, &it.node, restarts(o));
            }
            Kind::Service => {
                let ports: Vec<String> = o
                    .pointer("/spec/ports")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|p| match p["nodePort"].as_i64() {
                        Some(np) => format!("{}:{np}", p["port"]),
                        None => p["port"].to_string(),
                    })
                    .collect();
                it.detail = service_detail(s(o, "/spec/type"), s(o, "/spec/clusterIP"), &ports.join(","));
                it.selector = strings("/spec/selector");
            }
            Kind::Node => {
                let ready = o
                    .pointer("/status/conditions")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .find(|c| c["type"] == "Ready")
                    .map(|c| if c["status"] == "True" { "Ready" } else { "NotReady" })
                    .unwrap_or("Unknown");
                it.detail = ready.into();
                it.health = if ready == "Ready" { Health::Ok } else { Health::Bad };
                it.cidr = s(o, "/spec/podCIDR").into();
            }
        }
        it
    }
}

/// `Running on n1, 3 restarts`.
pub fn pod_detail(status: &str, node: &str, restarts: i64) -> String {
    let mut d = status.to_string();
    if !node.is_empty() && node != "<none>" {
        d.push_str(&format!(" on {node}"));
    }
    if restarts > 0 {
        d.push_str(&format!(", {}", plural(restarts as usize, "restart")));
    }
    d
}

/// `NodePort 10.43.0.9 80:30080`.
pub fn service_detail(kind: &str, ip: &str, ports: &str) -> String {
    let mut d = format!("{kind} {ip}");
    if !ports.is_empty() {
        d.push_str(&format!(" {ports}"));
    }
    d
}

/// Whether a selector picks a pod's labels.
fn selects(sel: &BTreeMap<String, String>, labels: &BTreeMap<String, String>) -> bool {
    !sel.is_empty() && sel.iter().all(|(k, v)| labels.get(k) == Some(v))
}

/// The tree's lines: nodes (with their pods, counted in `namespace` when
/// one is given), then each namespace's workloads down their controllers'
/// ownerReferences and its services with the pods they select. Old
/// ReplicaSets at zero only with `all`.
pub fn build(items: &[Item], all: bool, namespace: Option<&str>) -> Vec<Row> {
    let by_uid: HashMap<&str, &Item> = items.iter().filter(|i| !i.uid.is_empty() && !matches!(i.kind, Kind::Node | Kind::Service)).map(|i| (i.uid.as_str(), i)).collect();
    let mut children: HashMap<&str, Vec<&Item>> = HashMap::new();
    let mut roots: BTreeMap<&str, Vec<&Item>> = BTreeMap::new();
    for it in items.iter().filter(|i| !matches!(i.kind, Kind::Node | Kind::Service)) {
        if it.history && !all {
            continue;
        }
        match it.owner.as_deref().filter(|u| by_uid.contains_key(u)) {
            Some(uid) => children.entry(uid).or_default().push(it),
            None => roots.entry(it.ns.as_str()).or_default().push(it),
        }
    }
    let row = |it: &Item, depth: usize| Row { depth, kind: Some(it.kind), ns: it.ns.clone(), name: it.name.clone(), detail: it.detail.clone(), health: it.health };
    fn walk<'a>(it: &'a Item, depth: usize, children: &HashMap<&'a str, Vec<&'a Item>>, out: &mut Vec<Row>, row: &dyn Fn(&Item, usize) -> Row) {
        out.push(row(it, depth));
        let mut kids: Vec<&&Item> = children.get(it.uid.as_str()).map(|v| v.iter().collect()).unwrap_or_default();
        kids.sort_by_key(|k| (k.kind, k.name.as_str()));
        for k in kids {
            walk(k, depth + 1, children, out, row);
        }
    }
    let pods: Vec<&Item> = items.iter().filter(|i| i.kind == Kind::Pod).collect();
    let mut out = Vec::new();
    let mut nodes: Vec<&Item> = items.iter().filter(|i| i.kind == Kind::Node).collect();
    nodes.sort_by_key(|n| n.name.as_str());
    if !nodes.is_empty() {
        out.push(Row { depth: 0, kind: None, ns: String::new(), name: "nodes".into(), detail: String::new(), health: Health::Ok });
        for n in nodes {
            let count = pods.iter().filter(|p| p.node == n.name).count();
            let scope = namespace.map(|ns| format!(" in {ns}")).unwrap_or_default();
            let mut detail = format!("{}, {}{scope}", n.detail, plural(count, "pod"));
            if !n.cidr.is_empty() {
                detail.push_str(&format!(", pods {}", n.cidr));
            }
            out.push(Row { detail, ..row(n, 1) });
        }
    }
    let mut services: BTreeMap<&str, Vec<&Item>> = BTreeMap::new();
    for svc in items.iter().filter(|i| i.kind == Kind::Service) {
        services.entry(svc.ns.as_str()).or_default().push(svc);
    }
    let mut namespaces: Vec<&str> = roots.keys().chain(services.keys()).copied().collect();
    namespaces.sort();
    namespaces.dedup();
    for ns in namespaces {
        out.push(Row { depth: 0, kind: None, ns: ns.to_string(), name: format!("namespace {ns}"), detail: String::new(), health: Health::Ok });
        let mut list: Vec<&&Item> = roots.get(ns).map(|v| v.iter().collect()).unwrap_or_default();
        list.sort_by_key(|i| (i.kind, i.name.as_str()));
        for it in list {
            walk(it, 1, &children, &mut out, &row);
        }
        let mut svcs = services.get(ns).cloned().unwrap_or_default();
        svcs.sort_by_key(|s| s.name.as_str());
        for svc in svcs {
            let mut r = row(svc, 1);
            if !svc.selector.is_empty() {
                let picked: Vec<&&Item> = pods.iter().filter(|p| p.ns == ns && selects(&svc.selector, &p.labels)).collect();
                let ready = picked.iter().filter(|p| p.running).count();
                r.detail.push_str(&format!(" → {ready}/{} pods", picked.len()));
                r.health = if picked.is_empty() || ready == 0 { Health::Warn } else if ready < picked.len() { Health::Warn } else { Health::Ok };
            }
            out.push(r);
        }
    }
    out
}

/// For each line, whether it is the last of its siblings.
pub fn last_flags(rows: &[Row]) -> Vec<bool> {
    let mut last = vec![true; rows.len()];
    for i in 0..rows.len() {
        let d = rows[i].depth;
        if let Some(next) = rows[i + 1..].iter().find(|r| r.depth <= d) {
            last[i] = next.depth < d;
        }
    }
    last
}

/// The connectors in front of each line (`│  ├─ `), from depths alone.
pub fn prefixes(rows: &[Row]) -> Vec<String> {
    let last = last_flags(rows);
    let mut open: Vec<bool> = Vec::new();
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            open.truncate(r.depth);
            let mut p: String = open.iter().map(|o| if *o { "│  " } else { "   " }).collect();
            p.push_str(if last[i] { "└─ " } else { "├─ " });
            open.push(!last[i]);
            p
        })
        .collect()
}

pub fn render(server: &str, o: &Objects, all: bool, namespace: Option<&str>) -> String {
    let lists: [(Kind, &Vec<Value>); 9] = [
        (Kind::Node, &o.nodes),
        (Kind::Pod, &o.pods),
        (Kind::ReplicaSet, &o.replicasets),
        (Kind::Deployment, &o.deployments),
        (Kind::StatefulSet, &o.statefulsets),
        (Kind::DaemonSet, &o.daemonsets),
        (Kind::Job, &o.jobs),
        (Kind::CronJob, &o.cronjobs),
        (Kind::Service, &o.services),
    ];
    let items: Vec<Item> = lists.iter().flat_map(|(k, l)| l.iter().map(move |v| Item::from_object(*k, v))).collect();
    let rows = build(&items, all, namespace);
    let mut out = format!("cluster {server}\n");
    for (p, r) in prefixes(&rows).iter().zip(&rows) {
        out.push_str(p);
        out.push_str(&r.text());
        out.push('\n');
    }
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
