//! Home for a cluster that is not litek8s (k3s, kubeadm, a managed one):
//! no node reports of litek8s's own, so the figures come from the public
//! API. Capacity and allocatable from the Node objects, use from
//! `metrics.k8s.io` (the NodeMetrics metrics-server serves, what `kubectl
//! top nodes` shows) and each node's root filesystem from the kubelet's
//! `/stats/summary` through the API server's node proxy, when RBAC allows.
//! Parsing is here, pure; the requests are in `home.rs`.

use serde_json::Value;

use super::kube::table::quantity;

/// What Home knows of a cluster that is not litek8s.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cluster {
    /// `k3s`, `RKE2`, `EKS`, … or `Kubernetes`.
    pub distro: String,
    /// `/version`'s gitVersion (`v1.35.0+k3s1`).
    pub version: String,
    pub nodes: Vec<Node>,
    pub metrics: Source,
    pub disk: Source,
}

/// Whether an optional source of figures answers.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Source {
    /// Not asked yet.
    #[default]
    Pending,
    Served,
    /// Not there (`metrics.k8s.io` not served, no access to the node
    /// proxy): why, for the user.
    Missing(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Node {
    pub name: String,
    pub ready: bool,
    /// CPUs and bytes the scheduler may hand out (the share's whole).
    pub cpu_alloc: f64,
    pub mem_alloc: u64,
    pub cpu_capacity: f64,
    pub mem_capacity: u64,
    /// From its NodeMetrics: CPUs and working-set bytes.
    pub cpu_used: Option<f64>,
    pub mem_used: Option<u64>,
    /// Root filesystem: (used, capacity) bytes.
    pub fs: Option<(u64, u64)>,
}

impl Node {
    pub fn cpu_ratio(&self) -> Option<f64> {
        self.cpu_used.filter(|_| self.cpu_alloc > 0.0).map(|u| u / self.cpu_alloc)
    }

    pub fn mem_ratio(&self) -> Option<f64> {
        self.mem_used.filter(|_| self.mem_alloc > 0).map(|u| u as f64 / self.mem_alloc as f64)
    }
}

/// The nodes Home shows (all of them, or the one picked with `n`) added up.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Totals {
    pub nodes: usize,
    pub cpu_alloc: f64,
    pub mem_alloc: u64,
    /// `None` until some node has metrics.
    pub cpu_used: Option<f64>,
    pub mem_used: Option<u64>,
    pub fs: Option<(u64, u64)>,
}

impl Totals {
    pub fn cpu_ratio(&self) -> Option<f64> {
        self.cpu_used.filter(|_| self.cpu_alloc > 0.0).map(|u| u / self.cpu_alloc)
    }

    pub fn mem_ratio(&self) -> Option<f64> {
        self.mem_used.filter(|_| self.mem_alloc > 0).map(|u| u as f64 / self.mem_alloc as f64)
    }
}

impl Cluster {
    /// `None`: every node; else that node (unknown: nothing).
    pub fn totals(&self, scope: Option<&str>) -> Totals {
        let mut t = Totals::default();
        for n in self.nodes.iter().filter(|n| scope.is_none_or(|s| s == n.name)) {
            t.nodes += 1;
            t.cpu_alloc += n.cpu_alloc;
            t.mem_alloc += n.mem_alloc;
            if let Some(c) = n.cpu_used {
                t.cpu_used = Some(t.cpu_used.unwrap_or(0.0) + c);
            }
            if let Some(m) = n.mem_used {
                t.mem_used = Some(t.mem_used.unwrap_or(0) + m);
            }
            if let Some((u, c)) = n.fs {
                let (tu, tc) = t.fs.unwrap_or((0, 0));
                t.fs = Some((tu + u, tc + c));
            }
        }
        t
    }

    pub fn names(&self) -> Vec<String> {
        self.nodes.iter().map(|n| n.name.clone()).collect()
    }

    /// Each node's name as Home shows it: without the prefix every node
    /// shares (see [`short_names`]).
    pub fn short_name(&self, name: &str) -> String {
        let names = self.names();
        let short = short_names(&names);
        names.iter().position(|n| n == name).map(|i| short[i].clone()).unwrap_or_else(|| name.to_string())
    }

    /// `k3s v1.35.0+k3s1`, or `Kubernetes v1.33.1`.
    pub fn label(&self) -> String {
        if self.version.is_empty() { self.distro.clone() } else { format!("{} {}", self.distro, self.version) }
    }
}

/// Node names without the prefix they all share, cut at a `-` or `.`
/// (`pool-a-x7k2p`, `pool-a-m3n9q` → `…x7k2p`, `…m3n9q`): managed clusters
/// name nodes `<cluster>-<pool>-<hash>-<id>`, and only the end tells them
/// apart in a narrow column. Short prefixes (under 6) stay.
pub fn short_names(names: &[String]) -> Vec<String> {
    if names.len() < 2 {
        return names.to_vec();
    }
    let first = names[0].as_str();
    let mut common = first.len();
    for n in &names[1..] {
        common = common.min(first.bytes().zip(n.bytes()).take_while(|(a, b)| a == b).count());
    }
    // Back to just after a separator, and never a whole name.
    let cut = first[..common].rfind(['-', '.']).map(|i| i + 1).unwrap_or(0);
    if cut < 6 || names.iter().any(|n| n.len() <= cut) {
        return names.to_vec();
    }
    names.iter().map(|n| format!("…{}", &n[cut..])).collect()
}

/// The distribution a `/version` gitVersion names, by the marks
/// distributions put in it (`+k3s1`, `+rke2r1`, `-eks-…`, `-gke.…`).
pub fn distro(git_version: &str) -> &'static str {
    let g = git_version.to_ascii_lowercase();
    let meta: Vec<&str> = g.split('+').skip(1).collect();
    let has = |m: &str| meta.iter().any(|x| x.starts_with(m));
    if has("litek8s") {
        "litek8s"
    } else if has("k3s") {
        "k3s"
    } else if has("rke2") {
        "RKE2"
    } else if has("k0s") {
        "k0s"
    } else if g.contains("-eks-") {
        "EKS"
    } else if g.contains("-gke.") {
        "GKE"
    } else {
        "Kubernetes"
    }
}

/// The nodes of a `/api/v1/nodes` list, by name.
pub fn nodes(list: &Value) -> Vec<Node> {
    let q = |n: &Value, p: &str| n.pointer(p).and_then(Value::as_str).and_then(quantity).unwrap_or(0.0);
    let mut out: Vec<Node> = list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|n| Node {
            name: n.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string(),
            ready: n.pointer("/status/conditions").and_then(Value::as_array).into_iter().flatten().any(|c| c["type"] == "Ready" && c["status"] == "True"),
            cpu_alloc: q(n, "/status/allocatable/cpu"),
            mem_alloc: q(n, "/status/allocatable/memory") as u64,
            cpu_capacity: q(n, "/status/capacity/cpu"),
            mem_capacity: q(n, "/status/capacity/memory") as u64,
            ..Default::default()
        })
        .collect();
    out.sort_by(|a, b| super::kube::table::natural(&a.name, &b.name));
    out
}

/// Each node's use from a `metrics.k8s.io/v1beta1` NodeMetricsList;
/// nodes it leaves out (not scraped yet, NotReady) keep none.
pub fn apply_metrics(nodes: &mut [Node], list: &Value) {
    for n in nodes.iter_mut() {
        n.cpu_used = None;
        n.mem_used = None;
    }
    for m in list["items"].as_array().into_iter().flatten() {
        let name = m.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default();
        if let Some(n) = nodes.iter_mut().find(|n| n.name == name) {
            n.cpu_used = m.pointer("/usage/cpu").and_then(Value::as_str).and_then(quantity);
            n.mem_used = m.pointer("/usage/memory").and_then(Value::as_str).and_then(quantity).map(|b| b as u64);
        }
    }
}

/// The root filesystem of a kubelet `/stats/summary`: (used, capacity).
pub fn summary_fs(summary: &Value) -> Option<(u64, u64)> {
    let fs = summary.pointer("/node/fs")?;
    let cap = fs["capacityBytes"].as_u64().filter(|c| *c > 0)?;
    let used = fs["usedBytes"].as_u64().or_else(|| fs["availableBytes"].as_u64().map(|a| cap.saturating_sub(a)))?;
    Some((used, cap))
}

/// Why a source is missing, from its status: 404 (`metrics.k8s.io` not
/// served), 503 (its APIService is down), 403 (RBAC).
pub fn missing(what: &str, status: u16, message: &str) -> String {
    match status {
        404 => format!("{what} not served"),
        403 => format!("{what}: forbidden"),
        503 => format!("{what} unavailable"),
        _ if message.is_empty() => format!("{what}: HTTP {status}"),
        _ => format!("{what}: {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(name: &str, cpu: &str, mem: &str, ready: &str) -> Value {
        json!({
            "metadata": {"name": name},
            "status": {
                "capacity": {"cpu": cpu, "memory": mem},
                "allocatable": {"cpu": cpu, "memory": mem},
                "conditions": [{"type": "MemoryPressure", "status": "False"}, {"type": "Ready", "status": ready}],
                "images": [{"names": ["x"], "sizeBytes": 1}]
            }
        })
    }

    #[test]
    fn distro_from_git_version() {
        assert_eq!(distro("v1.35.0+k3s1"), "k3s");
        assert_eq!(distro("v1.33.0+litek8s"), "litek8s");
        assert_eq!(distro("v1.31.4+rke2r1"), "RKE2");
        assert_eq!(distro("v1.30.2+k0s"), "k0s");
        assert_eq!(distro("v1.30.9-eks-8cce635"), "EKS");
        assert_eq!(distro("v1.31.5-gke.1023000"), "GKE");
        assert_eq!(distro("v1.33.1"), "Kubernetes");
        assert_eq!(distro(""), "Kubernetes");
    }

    #[test]
    fn nodes_metrics_and_totals() {
        let list = json!({"items": [node("worker-10", "4", "8Gi", "True"), node("worker-2", "3500m", "4194304Ki", "False")]});
        let mut ns = nodes(&list);
        assert_eq!(ns.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), ["worker-2", "worker-10"]);
        assert!(!ns[0].ready && ns[1].ready);
        assert_eq!(ns[0].cpu_alloc, 3.5);
        assert_eq!(ns[0].mem_alloc, 4 << 30);
        let c = Cluster { nodes: ns.clone(), ..Default::default() };
        assert_eq!(c.totals(None).cpu_used, None, "no metrics yet");

        let metrics = json!({"items": [
            {"metadata": {"name": "worker-10"}, "usage": {"cpu": "1500000000n", "memory": "7Gi"}},
            {"metadata": {"name": "gone"}, "usage": {"cpu": "1", "memory": "1Gi"}}
        ]});
        apply_metrics(&mut ns, &metrics);
        assert_eq!(ns[1].cpu_used, Some(1.5));
        assert_eq!(ns[1].mem_ratio(), Some(7.0 / 8.0));
        assert_eq!(ns[0].cpu_used, None, "a node without metrics has none");
        let c = Cluster { nodes: ns, ..Default::default() };
        let all = c.totals(None);
        assert_eq!(all.nodes, 2);
        assert_eq!(all.cpu_alloc, 7.5);
        assert_eq!(all.cpu_ratio(), Some(1.5 / 7.5));
        assert_eq!(all.mem_used, Some(7 << 30));
        let one = c.totals(Some("worker-2"));
        assert_eq!((one.nodes, one.cpu_used), (1, None));
    }

    #[test]
    fn shared_prefix_dropped() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(short_names(&v(&["edge-lab-pool-a1b2-x7k2p", "edge-lab-pool-a1b2-m3n9q"])), v(&["…x7k2p", "…m3n9q"]));
        assert_eq!(short_names(&v(&["edge-lab-pool-a-x7k2p", "edge-lab-pool-b-m3n9q"])), v(&["…a-x7k2p", "…b-m3n9q"]));
        // Short or no shared prefix, a name that is the prefix, one node: unchanged.
        assert_eq!(short_names(&v(&["node-1", "node-2"])), v(&["node-1", "node-2"]));
        assert_eq!(short_names(&v(&["server", "agent"])), v(&["server", "agent"]));
        assert_eq!(short_names(&v(&["worker-pool-", "worker-pool-2"])), v(&["worker-pool-", "worker-pool-2"]));
        assert_eq!(short_names(&v(&["edge-lab-pool-a1b2-x7k2p"])), v(&["edge-lab-pool-a1b2-x7k2p"]));
    }

    #[test]
    fn summary_root_fs() {
        assert_eq!(summary_fs(&json!({"node": {"fs": {"capacityBytes": 100, "usedBytes": 40, "availableBytes": 60}}})), Some((40, 100)));
        assert_eq!(summary_fs(&json!({"node": {"fs": {"capacityBytes": 100, "availableBytes": 70}}})), Some((30, 100)));
        assert_eq!(summary_fs(&json!({"node": {}})), None);
    }

    #[test]
    fn missing_reasons() {
        assert_eq!(missing("metrics.k8s.io", 404, ""), "metrics.k8s.io not served");
        assert_eq!(missing("kubelet stats", 403, "nodes \"a\" is forbidden"), "kubelet stats: forbidden");
    }
}
