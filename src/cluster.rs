//! `litek8s cluster apply|status|delete -f cluster.yaml`: a whole cluster of
//! machines reached over SSH, declared in one file.
//!
//! ```yaml
//! apiVersion: litek8s.dev/v1
//! kind: Cluster
//! metadata: {name: home}
//! servers: [{ssh: pi@192.168.1.10}]          # 1, or 3/5 for HA
//! agents:
//!   - {ssh: me@old-laptop, labels: {disk: ssd}}
//!   - {ssh: ubuntu@ec2-host, nodeIp: 10.0.0.7, taints: ["gpu=true:NoSchedule"]}
//! registries: registries.yaml                # optional, for every machine
//! ```
//!
//! `apply` installs or updates the servers (the first with --cluster-init
//! when there are several, the others joining it), then the agents, with
//! `litek8s install --ssh` (`remote.rs`); a machine whose binary and flags
//! did not change is left alone. It adds the kubeconfig context `<name>`,
//! then sets the declared labels and taints on each Node through the API.
//! Nodes carry annotations naming their cluster, role and SSH destination,
//! so `apply` finds the agents that left the file (drained, uninstalled and
//! deleted with --prune) and only removes labels and taints it set itself.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hyper::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::install::{InstallResult, Role};
use crate::remote::{LocalBinary, Remote, RemoteInstall, Ssh};
use crate::ui::client::Endpoint;

pub const API_VERSION: &str = "litek8s.dev/v1";
pub const ANNOTATION_CLUSTER: &str = "cluster.litek8s.dev/name";
pub const ANNOTATION_ROLE: &str = "cluster.litek8s.dev/role";
pub const ANNOTATION_SSH: &str = "cluster.litek8s.dev/ssh";
/// The label keys `apply` set (comma separated): the ones it may remove.
pub const ANNOTATION_LABELS: &str = "cluster.litek8s.dev/labels";
/// The taints `apply` set, as `key:Effect` (comma separated).
pub const ANNOTATION_TAINTS: &str = "cluster.litek8s.dev/taints";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub name: String,
}

/// One machine of the file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostSpec {
    /// `user@host` (or a Host of ~/.ssh/config).
    pub ssh: String,
    /// The Node's name (default: the machine's hostname).
    #[serde(default)]
    pub name: Option<String>,
    /// The address other nodes reach it at (default: detected).
    #[serde(default)]
    pub node_ip: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub taints: Vec<TaintSpec>,
    /// More flags for `litek8s apiserver` / `litek8s agent`.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub ssh_port: Option<u16>,
    #[serde(default)]
    pub identity_file: Option<PathBuf>,
    #[serde(default)]
    pub ssh_options: Vec<String>,
}

/// `key=value:Effect`, `key:Effect` or `{key, value, effect}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TaintSpec {
    Text(String),
    Fields {
        key: String,
        #[serde(default)]
        value: Option<String>,
        effect: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Taint {
    pub key: String,
    pub value: Option<String>,
    pub effect: String,
}

impl Taint {
    fn id(&self) -> String {
        format!("{}:{}", self.key, self.effect)
    }

    fn to_json(&self) -> Value {
        let mut t = json!({"key": self.key, "effect": self.effect});
        if let Some(v) = &self.value {
            t["value"] = json!(v);
        }
        t
    }

    fn from_json(v: &Value) -> Option<Taint> {
        Some(Taint {
            key: v.get("key")?.as_str()?.to_string(),
            value: v.get("value").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string),
            effect: v.get("effect")?.as_str()?.to_string(),
        })
    }
}

impl std::fmt::Display for Taint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.value {
            Some(v) => write!(f, "{}={v}:{}", self.key, self.effect),
            None => write!(f, "{}:{}", self.key, self.effect),
        }
    }
}

impl TaintSpec {
    pub fn parse(&self) -> Result<Taint> {
        let (key, value, effect) = match self {
            TaintSpec::Text(t) => {
                let (kv, effect) = t.rsplit_once(':').with_context(|| format!("taint {t:?} is not key[=value]:Effect"))?;
                let (k, v) = match kv.split_once('=') {
                    Some((k, v)) => (k, Some(v.to_string())),
                    None => (kv, None),
                };
                (k.to_string(), v, effect.to_string())
            }
            TaintSpec::Fields { key, value, effect } => (key.clone(), value.clone(), effect.clone()),
        };
        if !valid_label_key(&key) {
            bail!("taint key {key:?} is not a valid label key");
        }
        if value.as_deref().is_some_and(|v| !valid_label_value(v)) {
            bail!("taint value {:?} is not a valid label value", value.unwrap_or_default());
        }
        if !["NoSchedule", "PreferNoSchedule", "NoExecute"].contains(&effect.as_str()) {
            bail!("taint effect {effect:?} is not NoSchedule, PreferNoSchedule or NoExecute");
        }
        Ok(Taint { key, value: value.filter(|v| !v.is_empty()), effect })
    }
}

/// The cluster file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClusterFile {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    #[serde(default)]
    pub servers: Vec<HostSpec>,
    #[serde(default)]
    pub agents: Vec<HostSpec>,
    /// Where this machine reaches the API (host or host:port; default: the
    /// first server's SSH host).
    #[serde(default)]
    pub api_address: Option<String>,
    /// The URL agents and other servers join (default: the first server's
    /// node IP).
    #[serde(default)]
    pub join_url: Option<String>,
    /// Flags every server / agent gets (before its own `args`).
    #[serde(default)]
    pub server_args: Vec<String>,
    #[serde(default)]
    pub agent_args: Vec<String>,
    /// SSH settings for every machine (a machine's own win).
    #[serde(default)]
    pub ssh_port: Option<u16>,
    #[serde(default)]
    pub identity_file: Option<PathBuf>,
    #[serde(default)]
    pub ssh_options: Vec<String>,
    /// A Linux litek8s binary to install (else this version's release).
    #[serde(default)]
    pub binary: Option<PathBuf>,
    /// A release tag to install (`v0.2.0`).
    #[serde(default)]
    pub release: Option<String>,
    /// A registry configuration (k3s registries.yaml format) every machine
    /// gets as /etc/litek8s/registries.yaml; relative to the cluster file.
    #[serde(default)]
    pub registries: Option<PathBuf>,
}

/// A machine with everything resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub role: Role,
    /// Position among the servers or the agents.
    pub index: usize,
    pub ssh: Ssh,
    pub spec: HostSpec,
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Server => "server",
        Role::Agent => "agent",
    }
}

fn valid_label_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(c))
}

fn valid_dns_label(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty() && b.len() <= 63 && b[0].is_ascii_alphanumeric() && b[b.len() - 1].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn valid_dns_subdomain(n: &str) -> bool {
    n.len() <= 253 && n.split('.').all(valid_dns_label)
}

pub fn valid_label_key(k: &str) -> bool {
    match k.rsplit_once('/') {
        Some((prefix, name)) => valid_dns_subdomain(prefix) && valid_label_name(name),
        None => valid_label_name(k),
    }
}

pub fn valid_label_value(v: &str) -> bool {
    v.is_empty() || valid_label_name(v)
}

/// Labels the agent sets itself, and this module's own.
fn reserved_label(k: &str) -> bool {
    k.starts_with("cluster.litek8s.dev/") || ["kubernetes.io/hostname", "kubernetes.io/os", "kubernetes.io/arch", "beta.kubernetes.io/os", "beta.kubernetes.io/arch"].contains(&k)
}

/// Flags the file has fields for, or that `apply` sets itself.
const MANAGED_FLAGS: &[(&str, &str)] = &[
    ("--token", "apply passes the first server's token"),
    ("--join", "apply joins agents to the first server (joinUrl to change where)"),
    ("--server", "apply joins servers to the first one"),
    ("--cluster-init", "apply starts the first of several servers with it"),
    ("--node-name", "use the machine's `name`"),
    ("--node-ip", "use the machine's `nodeIp`"),
    ("--registries", "use the file's `registries`"),
];

impl ClusterFile {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        let mut file = Self::parse(&text).with_context(|| format!("{} is not a valid cluster file", path.display()))?;
        if let (Some(r), Some(dir)) = (&mut file.registries, path.parent()) {
            *r = dir.join(&*r);
        }
        Ok(file)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let file: ClusterFile = serde_yaml::from_str(text)?;
        file.validate()?;
        Ok(file)
    }

    /// Every problem with the file, at once.
    pub fn validate(&self) -> Result<()> {
        let mut problems = Vec::new();
        if self.api_version != API_VERSION {
            problems.push(format!("apiVersion must be {API_VERSION} (got {:?})", self.api_version));
        }
        if self.kind != "Cluster" {
            problems.push(format!("kind must be Cluster (got {:?})", self.kind));
        }
        if !valid_dns_label(&self.metadata.name) {
            problems.push(format!("metadata.name {:?} must be a DNS label (lowercase letters, digits and '-', at most 63)", self.metadata.name));
        }
        if ![1, 3, 5].contains(&self.servers.len()) {
            problems.push(format!("servers: 1 server, or 3 or 5 for a highly available control plane (got {})", self.servers.len()));
        }
        let mut seen_ssh = BTreeSet::new();
        let mut seen_names = BTreeSet::new();
        for host in self.hosts() {
            let at = format!("{}s[{}] ({})", role_name(host.role), host.index, host.spec.ssh);
            if let Err(e) = host.ssh.validate() {
                problems.push(format!("{at}: {e}"));
            }
            if !seen_ssh.insert((host.ssh.dest.clone(), host.ssh.port)) {
                problems.push(format!("{at}: the same machine is listed twice"));
            }
            if let Some(name) = &host.spec.name {
                if !valid_dns_subdomain(name) {
                    problems.push(format!("{at}: name {name:?} is not a valid node name"));
                }
                if !seen_names.insert(name.clone()) {
                    problems.push(format!("{at}: two machines are named {name}"));
                }
            }
            if let Some(ip) = &host.spec.node_ip
                && ip.parse::<std::net::IpAddr>().is_err()
            {
                problems.push(format!("{at}: nodeIp {ip:?} is not an IP address"));
            }
            for (k, v) in &host.spec.labels {
                if !valid_label_key(k) {
                    problems.push(format!("{at}: label key {k:?} is not valid"));
                } else if reserved_label(k) {
                    problems.push(format!("{at}: label {k} is set by litek8s itself"));
                }
                if !valid_label_value(v) {
                    problems.push(format!("{at}: label value {v:?} (of {k}) is not valid"));
                }
            }
            let mut taints = BTreeSet::new();
            for t in &host.spec.taints {
                match t.parse() {
                    Ok(t) if !taints.insert(t.id()) => problems.push(format!("{at}: taint {} is listed twice", t.id())),
                    Ok(_) => {}
                    Err(e) => problems.push(format!("{at}: {e}")),
                }
            }
            let defaults = if host.role == Role::Server { &self.server_args } else { &self.agent_args };
            for arg in defaults.iter().chain(&host.spec.args) {
                if let Some((flag, why)) = MANAGED_FLAGS.iter().find(|(f, _)| arg == f || arg.strip_prefix(f).is_some_and(|r| r.starts_with('='))) {
                    problems.push(format!("{at}: no {flag} in args: {why}"));
                }
            }
        }
        if let Some(url) = &self.join_url
            && !url.starts_with("https://")
        {
            problems.push(format!("joinUrl {url:?} must be https://host:port"));
        }
        if self.binary.is_some() && self.release.is_some() {
            problems.push("binary and release: pick one".into());
        }
        if !problems.is_empty() {
            bail!("{}", problems.join("\n"));
        }
        Ok(())
    }

    /// Servers, then agents.
    pub fn hosts(&self) -> Vec<Host> {
        let one = |role: Role, index: usize, spec: &HostSpec| Host {
            role,
            index,
            ssh: Ssh {
                dest: spec.ssh.clone(),
                port: spec.ssh_port.or(self.ssh_port),
                identity_file: spec.identity_file.clone().or_else(|| self.identity_file.clone()),
                options: if spec.ssh_options.is_empty() { self.ssh_options.clone() } else { spec.ssh_options.clone() },
            },
            spec: spec.clone(),
        };
        let servers = self.servers.iter().enumerate().map(|(i, s)| one(Role::Server, i, s));
        let agents = self.agents.iter().enumerate().map(|(i, s)| one(Role::Agent, i, s));
        servers.chain(agents).collect()
    }

    /// The service flags for `host`; `join` is the first server's URL (for
    /// the other servers and the agents).
    pub fn service_args(&self, host: &Host, join: Option<&str>) -> Vec<String> {
        let mut args = Vec::new();
        match host.role {
            Role::Server if self.servers.len() > 1 && host.index == 0 => args.push("--cluster-init".into()),
            Role::Server if host.index > 0 => args.extend(["--server".to_string(), join.unwrap_or_default().to_string()]),
            Role::Server => {}
            Role::Agent => args.extend(["--join".to_string(), join.unwrap_or_default().to_string()]),
        }
        if let Some(n) = &host.spec.name {
            args.extend(["--node-name".to_string(), n.clone()]);
        }
        if let Some(ip) = &host.spec.node_ip {
            args.extend(["--node-ip".to_string(), ip.clone()]);
        }
        args.extend(if host.role == Role::Server { self.server_args.iter() } else { self.agent_args.iter() }.cloned());
        args.extend(host.spec.args.iter().cloned());
        args
    }
}

/// What `apply` needs of a Node.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeState {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub taints: Vec<Value>,
    pub ready: bool,
    pub unschedulable: bool,
}

impl NodeState {
    pub fn from_json(node: &Value) -> Self {
        let map = |v: Option<&Value>| -> BTreeMap<String, String> {
            v.and_then(Value::as_object).into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect()
        };
        NodeState {
            name: node.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string(),
            labels: map(node.pointer("/metadata/labels")),
            annotations: map(node.pointer("/metadata/annotations")),
            taints: node.pointer("/spec/taints").and_then(Value::as_array).cloned().unwrap_or_default(),
            ready: node
                .pointer("/status/conditions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|c| c["type"] == "Ready" && c["status"] == "True"),
            unschedulable: node.pointer("/spec/unschedulable").and_then(Value::as_bool).unwrap_or(false),
        }
    }

    fn cluster(&self) -> Option<&str> {
        self.annotations.get(ANNOTATION_CLUSTER).map(String::as_str)
    }

    fn role(&self) -> Option<Role> {
        match self.annotations.get(ANNOTATION_ROLE).map(String::as_str) {
            Some("server") => Some(Role::Server),
            Some("agent") => Some(Role::Agent),
            _ => None,
        }
    }

    fn ssh(&self) -> Option<Ssh> {
        serde_json::from_str(self.annotations.get(ANNOTATION_SSH)?).ok()
    }

    fn list(&self, key: &str) -> BTreeSet<String> {
        self.annotations.get(key).map(|v| v.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect()).unwrap_or_default()
    }
}

/// What a Node should have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub node: String,
    pub role: Role,
    pub ssh: Ssh,
    pub labels: BTreeMap<String, String>,
    pub taints: Vec<Taint>,
}

/// One merge patch for a Node, and what it changes, for people.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeUpdate {
    pub node: String,
    pub patch: Value,
    pub changes: Vec<String>,
}

pub fn node_update(cluster: &str, d: &Desired, n: &NodeState) -> Option<NodeUpdate> {
    let mut changes = Vec::new();
    let mut labels = serde_json::Map::new();
    for (k, v) in &d.labels {
        if n.labels.get(k) != Some(v) {
            labels.insert(k.clone(), json!(v));
            changes.push(format!("label {k}={v}"));
        }
    }
    for k in n.list(ANNOTATION_LABELS) {
        if !d.labels.contains_key(&k) && n.labels.contains_key(&k) {
            labels.insert(k.clone(), Value::Null);
            changes.push(format!("label {k} removed"));
        }
    }
    // Taints: ours as declared, everybody else's as they are.
    let managed = n.list(ANNOTATION_TAINTS);
    let wanted: BTreeSet<String> = d.taints.iter().map(Taint::id).collect();
    let mut taints: Vec<Value> = Vec::new();
    for t in &n.taints {
        let Some(parsed) = Taint::from_json(t) else {
            taints.push(t.clone());
            continue;
        };
        if wanted.contains(&parsed.id()) {
            continue;
        }
        if managed.contains(&parsed.id()) {
            changes.push(format!("taint {parsed} removed"));
            continue;
        }
        taints.push(t.clone());
    }
    for t in &d.taints {
        let current = n.taints.iter().filter_map(Taint::from_json).find(|c| c.id() == t.id());
        if current.as_ref() != Some(t) {
            changes.push(format!("taint {t}"));
        }
        taints.push(t.to_json());
    }
    let taints_changed = taints != n.taints;
    let mut annotations = serde_json::Map::new();
    let want = [
        (ANNOTATION_CLUSTER, cluster.to_string()),
        (ANNOTATION_ROLE, role_name(d.role).to_string()),
        (ANNOTATION_SSH, serde_json::to_string(&d.ssh).unwrap_or_default()),
        (ANNOTATION_LABELS, d.labels.keys().cloned().collect::<Vec<_>>().join(",")),
        (ANNOTATION_TAINTS, wanted.iter().cloned().collect::<Vec<_>>().join(",")),
    ];
    for (k, v) in want {
        if n.annotations.get(k).map(String::as_str).unwrap_or_default() != v {
            if k == ANNOTATION_CLUSTER || k == ANNOTATION_SSH {
                changes.push(format!("{} of cluster {cluster} ({})", role_name(d.role), d.ssh.display()));
            }
            annotations.insert(k.to_string(), if v.is_empty() { Value::Null } else { json!(v) });
        }
    }
    if labels.is_empty() && annotations.is_empty() && !taints_changed {
        return None;
    }
    let mut patch = json!({"metadata": {}});
    if !labels.is_empty() {
        patch["metadata"]["labels"] = Value::Object(labels);
    }
    if !annotations.is_empty() {
        patch["metadata"]["annotations"] = Value::Object(annotations);
    }
    if taints_changed {
        patch["spec"] = json!({"taints": taints});
    }
    changes.dedup();
    Some(NodeUpdate { node: d.node.clone(), patch, changes })
}

/// Nodes of `cluster` that `apply` put there and that the file no longer
/// has: (node, its role, how to reach it).
pub fn stale_nodes(cluster: &str, desired: &BTreeSet<String>, nodes: &[NodeState]) -> Vec<(String, Role, Option<Ssh>)> {
    nodes
        .iter()
        .filter(|n| n.cluster() == Some(cluster) && !desired.contains(&n.name))
        .filter_map(|n| Some((n.name.clone(), n.role()?, n.ssh())))
        .collect()
}

/// The Node of `host`: by its name, else by the SSH destination `apply`
/// recorded on it.
pub fn find_node<'a>(cluster: &str, host: &Host, nodes: &'a [NodeState]) -> Option<&'a NodeState> {
    if let Some(name) = &host.spec.name {
        return nodes.iter().find(|n| &n.name == name);
    }
    nodes.iter().find(|n| n.cluster() == Some(cluster) && n.ssh().is_some_and(|s| s.dest == host.ssh.dest && s.port == host.ssh.port))
}

#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    /// Drain, uninstall and delete agents that left the file.
    pub prune: bool,
    /// With prune: also delete their data.
    pub purge: bool,
    pub binary: Option<PathBuf>,
    pub release: Option<String>,
    pub skip_checks: bool,
    pub timeout: Option<Duration>,
}

fn kubeconfig_text() -> String {
    std::fs::read_to_string(crate::config::paths().kubeconfig).unwrap_or_default()
}

async fn api(name: &str) -> Result<Endpoint> {
    let (ep, _) = crate::ui::resolve_kube(Some(name)).with_context(|| format!("no kubeconfig context {name}"))?;
    Ok(ep)
}

async fn wait_api(ep: &Endpoint, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        match ep.json(Method::GET, "/api/v1/namespaces/default", None).await {
            Ok(_) => return Ok(()),
            Err(e) if start.elapsed() > timeout => {
                return Err(e).with_context(|| format!("the API at {} does not answer from here (set apiAddress in the cluster file?)", ep.display));
            }
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

async fn nodes(ep: &Endpoint) -> Result<Vec<NodeState>> {
    let list = ep.json(Method::GET, "/api/v1/nodes", None).await?;
    Ok(list["items"].as_array().into_iter().flatten().map(NodeState::from_json).collect())
}

async fn wait_node(ep: &Endpoint, name: &str, timeout: Duration) -> Result<NodeState> {
    let start = Instant::now();
    loop {
        if let Ok(n) = ep.json(Method::GET, &format!("/api/v1/nodes/{name}"), None).await {
            return Ok(NodeState::from_json(&n));
        }
        if start.elapsed() > timeout {
            bail!("node {name} is not registered");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn evictable(pod: &Value) -> bool {
    let daemon = pod.pointer("/metadata/ownerReferences").and_then(Value::as_array).into_iter().flatten().any(|o| o["kind"] == "DaemonSet");
    let mirror = pod.pointer("/metadata/annotations/kubernetes.io~1config.mirror").is_some();
    let done = matches!(pod.pointer("/status/phase").and_then(Value::as_str), Some("Succeeded" | "Failed"));
    !daemon && !mirror && !done
}

async fn pods_on(ep: &Endpoint, node: &str) -> Result<Vec<Value>> {
    let list = ep.json(Method::GET, &format!("/api/v1/pods?fieldSelector=spec.nodeName%3D{node}"), None).await?;
    Ok(list["items"].as_array().into_iter().flatten().filter(|p| p.pointer("/spec/nodeName").and_then(Value::as_str) == Some(node)).cloned().collect())
}

/// Cordon `node` and delete its pods (but DaemonSets'), as `kubectl drain`.
async fn drain(ep: &Endpoint, node: &str, timeout: Duration) -> Result<usize> {
    ep.json_typed(Method::PATCH, &format!("/api/v1/nodes/{node}"), "application/merge-patch+json", Some(&json!({"spec": {"unschedulable": true}}))).await?;
    let pods: Vec<Value> = pods_on(ep, node).await?.into_iter().filter(evictable).collect();
    for p in &pods {
        let ns = p.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or("default");
        let name = p.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default();
        let _ = ep.json(Method::DELETE, &format!("/api/v1/namespaces/{ns}/pods/{name}"), None).await;
    }
    let start = Instant::now();
    while pods_on(ep, node).await?.iter().any(evictable) {
        if start.elapsed() > timeout {
            bail!("pods on {node} did not stop within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(pods.len())
}

fn label(host: &Host) -> String {
    host.ssh.display()
}

/// `litek8s cluster apply -f FILE`.
pub async fn apply(file: &ClusterFile, o: &ApplyOptions) -> Result<()> {
    file.validate()?;
    let name = &file.metadata.name;
    let hosts = file.hosts();
    let local = LocalBinary::detect(o.binary.clone().or_else(|| file.binary.clone()), o.release.clone().or_else(|| file.release.clone()))?;
    let timeout = o.timeout.unwrap_or(Duration::from_secs(180));
    let registries = file.registries.as_deref().map(crate::registries::bundle).transpose()?;
    println!("cluster {name}: {} server(s), {} agent(s)", file.servers.len(), file.agents.len());
    // Probe every machine first (and ask for sudo passwords one at a time).
    let mut remotes = Vec::new();
    for h in &hosts {
        remotes.push(Remote::connect(&h.ssh, Some(label(h)), true).await?);
    }
    let mut changed: Vec<String> = Vec::new();
    let mut results: Vec<Option<InstallResult>> = vec![None; hosts.len()];
    let install = |i: usize, join: Option<String>, token: Option<String>| {
        let h = &hosts[i];
        let o = RemoteInstall {
            ssh: h.ssh.clone(),
            role: Some(h.role),
            args: file.service_args(h, join.as_deref()),
            token,
            skip_checks: o.skip_checks,
            timeout: Some(timeout),
            label: Some(label(h)),
            registries: registries.clone(),
            ..Default::default()
        };
        let remote = &remotes[i];
        let local = &local;
        async move { crate::remote::install_on(remote, &o, local).await }
    };
    // The servers, one after the other: the first makes the token.
    let mut join: Option<String> = file.join_url.clone();
    let mut token: Option<String> = None;
    for i in 0..file.servers.len() {
        let r = install(i, join.clone(), token.clone()).await.with_context(|| format!("server {}", label(&hosts[i])))?;
        if i == 0 {
            token = r.token.clone();
            if join.is_none() {
                join = r.url.clone();
            }
            if token.is_none() || join.is_none() {
                bail!("the first server reported no join token or URL");
            }
        }
        if r.changed {
            changed.push(format!("server {} ({}) installed", r.node, label(&hosts[i])));
        }
        results[i] = Some(r);
    }
    let first = results[0].clone().expect("installed");
    let ssh_host = hosts[0].ssh.resolved_host().await.unwrap_or_else(|| hosts[0].ssh.host().to_string());
    let url = crate::remote::api_url(file.api_address.as_deref(), &file.service_args(&hosts[0], None), &ssh_host);
    let before = kubeconfig_text();
    let path = crate::remote::write_context(name, &url, &first)?;
    if kubeconfig_text() != before {
        changed.push(format!("kubeconfig context {name} ({url}) in {}", path.display()));
    }
    // The agents, all at once.
    let agents: Vec<usize> = (file.servers.len()..hosts.len()).collect();
    let installs = futures_util::future::join_all(agents.iter().map(|&i| install(i, join.clone(), token.clone()))).await;
    let mut failures = Vec::new();
    for (&i, r) in agents.iter().zip(installs) {
        match r {
            Ok(r) => {
                if r.changed {
                    changed.push(format!("agent {} ({}) installed", r.node, label(&hosts[i])));
                }
                results[i] = Some(r);
            }
            Err(e) => failures.push(format!("agent {}: {e:#}", label(&hosts[i]))),
        }
    }
    // Labels, taints and the annotations that tie each Node to this file.
    let ep = api(name).await?;
    wait_api(&ep, Duration::from_secs(60)).await?;
    let mut desired_names = BTreeSet::new();
    for (h, r) in hosts.iter().zip(&results) {
        let Some(r) = r else { continue };
        if r.node.is_empty() {
            continue;
        }
        desired_names.insert(r.node.clone());
        let d = Desired {
            node: r.node.clone(),
            role: h.role,
            ssh: h.ssh.clone(),
            labels: h.spec.labels.clone(),
            taints: h.spec.taints.iter().map(|t| t.parse()).collect::<Result<_>>()?,
        };
        let state = wait_node(&ep, &r.node, Duration::from_secs(60)).await?;
        if let Some(update) = node_update(name, &d, &state) {
            ep.json_typed(Method::PATCH, &format!("/api/v1/nodes/{}", update.node), "application/merge-patch+json", Some(&update.patch))
                .await
                .with_context(|| format!("cannot update node {}", update.node))?;
            changed.push(format!("node {}: {}", update.node, update.changes.join(", ")));
        }
    }
    // Machines that left the file.
    let all = nodes(&ep).await?;
    for (node, role, ssh) in stale_nodes(name, &desired_names, &all) {
        if role == Role::Server {
            eprintln!("warning: server {node} is no longer in the file; apply does not remove servers (`litek8s server remove {node}`, then `litek8s uninstall --ssh ...`)");
            continue;
        }
        if !o.prune {
            eprintln!("agent {node} is no longer in the file: `litek8s cluster apply --prune` drains, uninstalls and deletes it");
            continue;
        }
        match remove_agent(&ep, &node, ssh.as_ref(), o.purge, timeout).await {
            Ok(()) => changed.push(format!("agent {node} drained, uninstalled and deleted")),
            Err(e) => failures.push(format!("removing agent {node}: {e:#}")),
        }
    }
    println!();
    if changed.is_empty() {
        println!("cluster {name} is up to date: nothing changed");
    } else {
        println!("cluster {name}: {} change(s)", changed.len());
        for c in &changed {
            println!("  {c}");
        }
    }
    if !failures.is_empty() {
        bail!("{}", failures.join("\n"));
    }
    println!("\nkubectl --context {name} get nodes");
    Ok(())
}

async fn remove_agent(ep: &Endpoint, node: &str, ssh: Option<&Ssh>, purge: bool, timeout: Duration) -> Result<()> {
    let ssh = ssh.context("the node does not say how to reach it over ssh")?;
    let n = drain(ep, node, timeout).await?;
    println!("[{}] drained {node} ({n} pod(s))", ssh.display());
    let remote = Remote::connect(ssh, Some(ssh.display()), true).await?;
    crate::remote::uninstall_on(&remote, purge).await?;
    ep.json(Method::DELETE, &format!("/api/v1/nodes/{node}"), None).await?;
    Ok(())
}

/// `litek8s cluster status -f FILE`.
pub async fn status(file: &ClusterFile) -> Result<()> {
    file.validate()?;
    let name = &file.metadata.name;
    let hosts = file.hosts();
    let api = match api(name).await {
        Ok(ep) => match nodes(&ep).await {
            Ok(n) => Ok((ep, n)),
            Err(e) => Err(format!("{e:#}")),
        },
        Err(e) => Err(format!("{e:#}")),
    };
    let (nodes, pods): (Vec<NodeState>, BTreeMap<String, usize>) = match &api {
        Ok((ep, nodes)) => {
            let mut pods = BTreeMap::new();
            if let Ok(list) = ep.json(Method::GET, "/api/v1/pods", None).await {
                for p in list["items"].as_array().into_iter().flatten() {
                    if let Some(n) = p.pointer("/spec/nodeName").and_then(Value::as_str) {
                        *pods.entry(n.to_string()).or_default() += 1;
                    }
                }
            }
            (nodes.clone(), pods)
        }
        Err(_) => (Vec::new(), BTreeMap::new()),
    };
    let probes = futures_util::future::join_all(hosts.iter().map(|h| Remote::connect(&h.ssh, Some(label(h)), false))).await;
    let mut rows = vec![["ROLE", "SSH", "NODE", "SERVICE", "VERSION", "READY", "PODS"].map(String::from).to_vec()];
    let mut matched = BTreeSet::new();
    for (h, probe) in hosts.iter().zip(&probes) {
        let node = find_node(name, h, &nodes);
        if let Some(n) = node {
            matched.insert(n.name.clone());
        }
        let (service, version) = match probe {
            Ok(r) => {
                let (installed, active) = match h.role {
                    Role::Server => (r.probe.server_installed, r.probe.server_active),
                    Role::Agent => (r.probe.agent_installed, r.probe.agent_active),
                };
                (
                    match (installed, active) {
                        (_, true) => "active",
                        (true, false) => "inactive",
                        _ => "not installed",
                    }
                    .to_string(),
                    r.probe.version.clone().unwrap_or_else(|| "-".into()),
                )
            }
            Err(e) => (format!("unreachable ({})", e.root_cause()), "-".into()),
        };
        rows.push(vec![
            role_name(h.role).to_string(),
            h.ssh.display(),
            node.map_or("-".into(), |n| n.name.clone()),
            service,
            version,
            node.map_or("-".into(), |n| if n.ready { if n.unschedulable { "Ready,SchedulingDisabled".into() } else { "Ready".into() } } else { "NotReady".into() }),
            node.map_or("-".into(), |n| pods.get(&n.name).copied().unwrap_or(0).to_string()),
        ]);
    }
    for n in nodes.iter().filter(|n| !matched.contains(&n.name)) {
        let in_file = n.cluster() == Some(name.as_str());
        rows.push(vec![
            n.role().map_or("node", role_name).to_string(),
            n.ssh().map_or("-".into(), |s| s.display()),
            n.name.clone(),
            if in_file { "not in the file".into() } else { "not managed by the file".into() },
            "-".into(),
            if n.ready { "Ready".into() } else { "NotReady".into() },
            pods.get(&n.name).copied().unwrap_or(0).to_string(),
        ]);
    }
    println!("cluster {name}");
    match &api {
        Ok((ep, _)) => println!("API: {} (context {name})\n", ep.display),
        Err(e) => println!("API: {e}\n"),
    }
    print_table(&rows);
    Ok(())
}

fn print_table(rows: &[Vec<String>]) {
    let widths: Vec<usize> = (0..rows[0].len()).map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0)).collect();
    for r in rows {
        let line: Vec<String> = r.iter().zip(&widths).map(|(c, w)| format!("{c:<w$}")).collect();
        println!("{}", line.join("  ").trim_end());
    }
}

/// `litek8s cluster delete -f FILE`.
pub async fn delete(file: &ClusterFile, yes: bool, purge: bool) -> Result<()> {
    file.validate()?;
    let name = &file.metadata.name;
    let hosts = file.hosts();
    let mut agents: Vec<Ssh> = hosts.iter().filter(|h| h.role == Role::Agent).map(|h| h.ssh.clone()).collect();
    // Agents that left the file but are still in the cluster go too.
    if let Ok(ep) = api(name).await
        && let Ok(all) = nodes(&ep).await
    {
        let declared: BTreeSet<String> = hosts.iter().filter_map(|h| find_node(name, h, &all)).map(|n| n.name.clone()).collect();
        for (_, role, ssh) in stale_nodes(name, &declared, &all) {
            if let (Role::Agent, Some(ssh)) = (role, ssh)
                && !agents.contains(&ssh)
            {
                agents.push(ssh);
            }
        }
    }
    let servers: Vec<Ssh> = hosts.iter().filter(|h| h.role == Role::Server).rev().map(|h| h.ssh.clone()).collect();
    println!("cluster {name}: uninstalling litek8s{} from", if purge { " and deleting its data" } else { "" });
    for s in &agents {
        println!("  agent  {}", s.display());
    }
    for s in &servers {
        println!("  server {}", s.display());
    }
    if !yes {
        bail!("this stops every pod of the cluster; run again with --yes to go ahead");
    }
    let mut failures = Vec::new();
    let one = |ssh: Ssh| async move {
        let remote = Remote::connect(&ssh, Some(ssh.display()), true).await?;
        crate::remote::uninstall_on(&remote, purge).await
    };
    let results = futures_util::future::join_all(agents.iter().cloned().map(one)).await;
    for (s, r) in agents.iter().zip(results) {
        if let Err(e) = r {
            failures.push(format!("agent {}: {e:#}", s.display()));
        }
    }
    for s in servers {
        if let Err(e) = one(s.clone()).await {
            failures.push(format!("server {}: {e:#}", s.display()));
        }
    }
    crate::dev::kubeconfig::uninstall(&crate::config::paths().kubeconfig, name)?;
    println!("removed kubeconfig context {name}");
    if !failures.is_empty() {
        bail!("{}", failures.join("\n"));
    }
    println!("cluster {name} deleted");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "\
apiVersion: litek8s.dev/v1
kind: Cluster
metadata: {name: home}
servers: [{ssh: pi@192.168.1.10}]
agents:
  - {ssh: me@old-laptop, labels: {disk: ssd}}
  - {ssh: ubuntu@ec2-host, nodeIp: 10.0.0.7, name: ec2, taints: ['gpu=true:NoSchedule', {key: spot, effect: PreferNoSchedule}]}
identityFile: ~/.ssh/home
";

    #[test]
    fn parses_the_example() {
        let f = ClusterFile::parse(FILE).unwrap();
        let hosts = f.hosts();
        assert_eq!(hosts.len(), 3);
        assert_eq!(hosts[0].role, Role::Server);
        assert_eq!(hosts[2].ssh.identity_file.as_deref(), Some(Path::new("~/.ssh/home")));
        assert_eq!(f.service_args(&hosts[0], None), Vec::<String>::new());
        assert_eq!(f.service_args(&hosts[2], Some("https://192.168.1.10:6443")), ["--join", "https://192.168.1.10:6443", "--node-name", "ec2", "--node-ip", "10.0.0.7"]);
        let t: Vec<Taint> = hosts[2].spec.taints.iter().map(|t| t.parse().unwrap()).collect();
        assert_eq!(t[0].to_string(), "gpu=true:NoSchedule");
        assert_eq!(t[1].to_string(), "spot:PreferNoSchedule");
    }

    #[test]
    fn registries_are_relative_to_the_cluster_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cluster.yaml");
        std::fs::write(&path, format!("{FILE}registries: lan/registries.yaml\n")).unwrap();
        assert_eq!(ClusterFile::load(&path).unwrap().registries, Some(dir.path().join("lan/registries.yaml")));
        let e = ClusterFile::parse(&format!("{FILE}serverArgs: [--registries, x]\n")).unwrap_err();
        assert!(format!("{e:#}").contains("use the file's `registries`"), "{e:#}");
    }

    #[test]
    fn ha_servers_init_then_join() {
        let f = ClusterFile::parse(
            "apiVersion: litek8s.dev/v1\nkind: Cluster\nmetadata: {name: ha}\nservers: [{ssh: a}, {ssh: b}, {ssh: c, args: [--no-node]}]\nserverArgs: [--no-wireguard]\n",
        )
        .unwrap();
        let h = f.hosts();
        assert_eq!(f.service_args(&h[0], None), ["--cluster-init", "--no-wireguard"]);
        assert_eq!(f.service_args(&h[2], Some("https://a:6443")), ["--server", "https://a:6443", "--no-wireguard", "--no-node"]);
    }

    #[test]
    fn validation_lists_every_problem() {
        let bad = "\
apiVersion: litek8s.dev/v1alpha1
kind: Cluster
metadata: {name: Home}
servers: [{ssh: a}, {ssh: b}]
agents:
  - {ssh: a}
  - {ssh: '-oProxyCommand=x'}
  - {ssh: c, name: n, labels: {'bad key!': x, kubernetes.io/hostname: y, ok: 'bad value!'}}
  - {ssh: d, name: n, nodeIp: nope, taints: ['k=v:Sometimes', 'nokey'], args: [--token, t, --node-ip=1.2.3.4]}
";
        let e = ClusterFile::parse(bad).unwrap_err().to_string();
        for want in [
            "apiVersion must be litek8s.dev/v1",
            "metadata.name \"Home\"",
            "1 server, or 3 or 5",
            "agents[0] (a): the same machine is listed twice",
            "invalid ssh destination",
            "label key \"bad key!\"",
            "kubernetes.io/hostname is set by litek8s",
            "label value \"bad value!\"",
            "two machines are named n",
            "nodeIp \"nope\"",
            "taint effect \"Sometimes\"",
            "taint \"nokey\"",
            "no --token in args",
            "no --node-ip in args",
        ] {
            assert!(e.contains(want), "missing {want:?} in:\n{e}");
        }
        // Typos in field names are errors, not ignored.
        assert!(ClusterFile::parse("apiVersion: litek8s.dev/v1\nkind: Cluster\nmetadata: {name: a}\nservers: [{ssh: a, nodeIP: 1.2.3.4}]\n").is_err());
        // The same host on another port is another machine.
        assert!(ClusterFile::parse("apiVersion: litek8s.dev/v1\nkind: Cluster\nmetadata: {name: a}\nservers: [{ssh: me@127.0.0.1, sshPort: 2201}]\nagents: [{ssh: me@127.0.0.1, sshPort: 2202}]\n").is_ok());
    }

    fn state(name: &str, labels: &[(&str, &str)], annotations: &[(&str, &str)], taints: Value) -> NodeState {
        NodeState {
            name: name.into(),
            labels: labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            annotations: annotations.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            taints: taints.as_array().cloned().unwrap_or_default(),
            ready: true,
            unschedulable: false,
        }
    }

    fn desired(labels: &[(&str, &str)], taints: &[&str]) -> Desired {
        Desired {
            node: "n1".into(),
            role: Role::Agent,
            ssh: Ssh::new("me@laptop"),
            labels: labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            taints: taints.iter().map(|t| TaintSpec::Text(t.to_string()).parse().unwrap()).collect(),
        }
    }

    fn adopted(labels: &str, taints: &str) -> Vec<(&'static str, String)> {
        vec![
            (ANNOTATION_CLUSTER, "home".into()),
            (ANNOTATION_ROLE, "agent".into()),
            (ANNOTATION_SSH, r#"{"dest":"me@laptop"}"#.into()),
            (ANNOTATION_LABELS, labels.into()),
            (ANNOTATION_TAINTS, taints.into()),
        ]
    }

    #[test]
    fn first_apply_adopts_and_labels_the_node() {
        let n = state("n1", &[("kubernetes.io/hostname", "n1")], &[], json!([]));
        let u = node_update("home", &desired(&[("disk", "ssd")], &["gpu=true:NoSchedule"]), &n).unwrap();
        assert_eq!(u.patch["metadata"]["labels"], json!({"disk": "ssd"}));
        assert_eq!(u.patch["metadata"]["annotations"][ANNOTATION_CLUSTER], "home");
        assert_eq!(u.patch["metadata"]["annotations"][ANNOTATION_LABELS], "disk");
        assert_eq!(u.patch["metadata"]["annotations"][ANNOTATION_TAINTS], "gpu:NoSchedule");
        assert_eq!(u.patch["spec"]["taints"], json!([{"key": "gpu", "value": "true", "effect": "NoSchedule"}]));
        assert_eq!(u.changes, ["label disk=ssd", "taint gpu=true:NoSchedule", "agent of cluster home (me@laptop)"]);
    }

    #[test]
    fn reapply_is_a_no_op() {
        let a = adopted("disk", "gpu:NoSchedule");
        let ann: Vec<(&str, &str)> = a.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let n = state("n1", &[("disk", "ssd")], &ann, json!([{"key": "gpu", "value": "true", "effect": "NoSchedule"}]));
        assert_eq!(node_update("home", &desired(&[("disk", "ssd")], &["gpu=true:NoSchedule"]), &n), None);
    }

    #[test]
    fn removes_only_what_it_set() {
        let a = adopted("disk,zone", "gpu:NoSchedule");
        let ann: Vec<(&str, &str)> = a.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let taints = json!([
            {"key": "gpu", "value": "true", "effect": "NoSchedule"},
            {"key": "node.kubernetes.io/unreachable", "effect": "NoExecute", "timeAdded": "2026-10-20T00:00:00Z"},
            {"key": "mine", "effect": "NoSchedule"}
        ]);
        let n = state("n1", &[("disk", "ssd"), ("zone", "a"), ("hand-set", "x")], &ann, taints);
        let u = node_update("home", &desired(&[("disk", "hdd")], &["spot:PreferNoSchedule"]), &n).unwrap();
        assert_eq!(u.patch["metadata"]["labels"], json!({"disk": "hdd", "zone": null}));
        assert_eq!(
            u.patch["spec"]["taints"],
            json!([
                {"key": "node.kubernetes.io/unreachable", "effect": "NoExecute", "timeAdded": "2026-10-20T00:00:00Z"},
                {"key": "mine", "effect": "NoSchedule"},
                {"key": "spot", "effect": "PreferNoSchedule"}
            ])
        );
        assert_eq!(u.patch["metadata"]["annotations"], json!({ANNOTATION_LABELS: "disk", ANNOTATION_TAINTS: "spot:PreferNoSchedule"}));
        assert_eq!(u.changes, ["label disk=hdd", "label zone removed", "taint gpu=true:NoSchedule removed", "taint spot:PreferNoSchedule"]);
        // Everything gone: the bookkeeping annotations go too.
        let u = node_update("home", &desired(&[], &[]), &n).unwrap();
        assert_eq!(u.patch["metadata"]["annotations"], json!({ANNOTATION_LABELS: null, ANNOTATION_TAINTS: null}));
    }

    #[test]
    fn a_changed_taint_value_is_replaced() {
        let a = adopted("", "gpu:NoSchedule");
        let ann: Vec<(&str, &str)> = a.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let n = state("n1", &[], &ann, json!([{"key": "gpu", "value": "a100", "effect": "NoSchedule"}]));
        let u = node_update("home", &desired(&[], &["gpu=h100:NoSchedule"]), &n).unwrap();
        assert_eq!(u.patch["spec"]["taints"], json!([{"key": "gpu", "value": "h100", "effect": "NoSchedule"}]));
        assert_eq!(u.changes, ["taint gpu=h100:NoSchedule"]);
    }

    #[test]
    fn stale_and_found_nodes() {
        let ssh = |d: &str, p: Option<u16>| serde_json::to_string(&Ssh { dest: d.into(), port: p, ..Default::default() }).unwrap();
        let s1 = ssh("me@laptop", None);
        let s2 = ssh("u@ec2", Some(2222));
        let nodes = vec![
            state("laptop", &[], &[(ANNOTATION_CLUSTER, "home"), (ANNOTATION_ROLE, "agent"), (ANNOTATION_SSH, &s1)], json!([])),
            state("ec2", &[], &[(ANNOTATION_CLUSTER, "home"), (ANNOTATION_ROLE, "agent"), (ANNOTATION_SSH, &s2)], json!([])),
            state("pi", &[], &[(ANNOTATION_CLUSTER, "home"), (ANNOTATION_ROLE, "server")], json!([])),
            state("other", &[], &[(ANNOTATION_CLUSTER, "work"), (ANNOTATION_ROLE, "agent")], json!([])),
            state("manual", &[], &[], json!([])),
        ];
        let desired: BTreeSet<String> = ["pi".to_string(), "laptop".to_string()].into();
        let stale = stale_nodes("home", &desired, &nodes);
        assert_eq!(stale, vec![("ec2".to_string(), Role::Agent, Some(Ssh { dest: "u@ec2".into(), port: Some(2222), ..Default::default() }))]);
        let f = ClusterFile::parse("apiVersion: litek8s.dev/v1\nkind: Cluster\nmetadata: {name: home}\nservers: [{ssh: pi, name: pi}]\nagents: [{ssh: u@ec2, sshPort: 2222}, {ssh: u@ec2}]\n").unwrap();
        let h = f.hosts();
        assert_eq!(find_node("home", &h[0], &nodes).map(|n| n.name.as_str()), Some("pi"));
        assert_eq!(find_node("home", &h[1], &nodes).map(|n| n.name.as_str()), Some("ec2"));
        assert_eq!(find_node("home", &h[2], &nodes), None);
    }

    #[test]
    fn node_state_from_json() {
        let n = NodeState::from_json(&json!({
            "metadata": {"name": "a", "labels": {"x": "1"}, "annotations": {ANNOTATION_CLUSTER: "home"}},
            "spec": {"unschedulable": true, "taints": [{"key": "k", "effect": "NoSchedule"}]},
            "status": {"conditions": [{"type": "Ready", "status": "True"}]}
        }));
        assert!(n.ready && n.unschedulable);
        assert_eq!(n.cluster(), Some("home"));
        assert_eq!(n.labels["x"], "1");
        assert_eq!(n.taints.len(), 1);
    }

    #[test]
    fn evictable_pods() {
        assert!(evictable(&json!({"metadata": {}, "status": {"phase": "Running"}})));
        assert!(!evictable(&json!({"metadata": {"ownerReferences": [{"kind": "DaemonSet"}]}})));
        assert!(!evictable(&json!({"metadata": {"annotations": {"kubernetes.io/config.mirror": "x"}}})));
        assert!(!evictable(&json!({"metadata": {}, "status": {"phase": "Succeeded"}})));
    }
}
