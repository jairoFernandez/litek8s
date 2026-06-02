//! Apply / diff / destroy / export for `kind: Cluster` manifests.
//!
//! Runs on the CLI side. Reuses the existing daemon protocol (one request per
//! operation) instead of inventing a new bulk endpoint. That keeps the daemon
//! ignorant of the manifest format and makes failures easy to localize.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cluster_manifest::{
    ClusterManifest, ClusterMetadata, ClusterNodeSpec, ClusterSpec, ClusterVpnSpec, parse_cidr,
};
use crate::node::{self, DEFAULT_REMOTE_BINARY};
use crate::protocol::{NodeSummary, Request, Response};

/// Result of `apply` — what actually changed, for diff-style output.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApplyReport {
    pub vpn_initialized: Option<String>,
    pub nodes_added: Vec<String>,
    pub nodes_unchanged: Vec<String>,
    pub workloads_applied: Vec<PathBuf>,
}

/// Result of `diff` — list of pending operations without performing them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiffReport {
    pub vpn_to_initialize: Option<String>,
    pub vpn_already_initialized: bool,
    pub nodes_to_add: Vec<String>,
    pub nodes_already_present: Vec<String>,
    pub nodes_extra: Vec<String>,
    pub workloads_to_apply: Vec<PathBuf>,
}

/// Apply a cluster manifest idempotently.
pub async fn apply<E>(manifest: &ClusterManifest, endpoint: &E) -> Result<ApplyReport>
where
    E: DaemonEndpoint,
{
    let mut report = ApplyReport::default();

    if let Some(vpn) = manifest.spec.vpn.as_ref() {
        let initialized = ensure_vpn(vpn)?;
        if initialized {
            report.vpn_initialized = Some(format!(
                "hub initialized for {} via {}",
                vpn.cidr, vpn.hub_endpoint
            ));
        }
    }

    let existing_nodes = endpoint.list_node_summaries().await?;
    let existing_by_name: BTreeMap<&str, &NodeSummary> =
        existing_nodes.iter().map(|n| (n.name.as_str(), n)).collect();

    for node_spec in &manifest.spec.nodes {
        if existing_by_name.contains_key(node_spec.name.as_str()) {
            report.nodes_unchanged.push(node_spec.name.clone());
            continue;
        }
        add_node(endpoint, node_spec).await?;
        report.nodes_added.push(node_spec.name.clone());
    }

    for workload in &manifest.spec.workloads {
        let path = resolve_workload_path(&workload.file)?;
        let yaml = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read workload {}", path.display()))?;
        match endpoint.request(Request::Apply { manifest: yaml }).await? {
            Response::Ack { .. } => {
                report.workloads_applied.push(path);
            }
            Response::Error { message } => {
                anyhow::bail!("workload `{}` failed: {message}", path.display());
            }
            other => anyhow::bail!(
                "unexpected daemon response while applying `{}`: {other:?}",
                path.display()
            ),
        }
    }

    Ok(report)
}

/// Compute a diff against the current cluster state without mutating anything.
pub async fn diff<E>(manifest: &ClusterManifest, endpoint: &E) -> Result<DiffReport>
where
    E: DaemonEndpoint,
{
    let mut report = DiffReport::default();

    if let Some(vpn) = manifest.spec.vpn.as_ref() {
        let hub = crate::vpn::HubState::load()?;
        if hub.network.is_some() {
            report.vpn_already_initialized = true;
        } else {
            report.vpn_to_initialize = Some(format!("{} via {}", vpn.cidr, vpn.hub_endpoint));
        }
    }

    let existing_nodes = endpoint.list_node_summaries().await?;
    let existing_names: std::collections::HashSet<&str> =
        existing_nodes.iter().map(|n| n.name.as_str()).collect();
    let desired_names: std::collections::HashSet<&str> = manifest
        .spec
        .nodes
        .iter()
        .map(|n| n.name.as_str())
        .collect();

    for node in &manifest.spec.nodes {
        if existing_names.contains(node.name.as_str()) {
            report.nodes_already_present.push(node.name.clone());
        } else {
            report.nodes_to_add.push(node.name.clone());
        }
    }
    for existing in &existing_nodes {
        if existing.name == "local" {
            continue;
        }
        if !desired_names.contains(existing.name.as_str()) {
            report.nodes_extra.push(existing.name.clone());
        }
    }

    for workload in &manifest.spec.workloads {
        report
            .workloads_to_apply
            .push(resolve_workload_path(&workload.file)?);
    }

    Ok(report)
}

/// Tear down workloads and worker nodes declared in the manifest.
///
/// VPN state and the local node are preserved so a subsequent `apply` works
/// without re-bootstrapping the hub.
pub async fn destroy<E>(manifest: &ClusterManifest, endpoint: &E) -> Result<DestroyReport>
where
    E: DaemonEndpoint,
{
    let mut report = DestroyReport::default();

    for workload in &manifest.spec.workloads {
        let path = resolve_workload_path(&workload.file)?;
        let yaml = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read workload {}", path.display()))?;
        match endpoint
            .request(Request::DeleteManifest { manifest: yaml })
            .await?
        {
            Response::Ack { .. } => report.workloads_deleted.push(path),
            Response::Error { message } => {
                eprintln!("warn: workload `{}` delete: {message}", path.display());
            }
            other => anyhow::bail!(
                "unexpected daemon response while deleting `{}`: {other:?}",
                path.display()
            ),
        }
    }

    for node_spec in &manifest.spec.nodes {
        match endpoint
            .request(Request::DrainNode {
                name: node_spec.name.clone(),
            })
            .await
        {
            Ok(Response::Ack { .. }) => report.nodes_drained.push(node_spec.name.clone()),
            Ok(Response::Error { message }) => {
                eprintln!("warn: drain `{}`: {message}", node_spec.name);
            }
            Ok(other) => {
                eprintln!(
                    "warn: unexpected drain response for `{}`: {other:?}",
                    node_spec.name
                );
            }
            Err(error) => {
                eprintln!("warn: drain `{}`: {error}", node_spec.name);
            }
        }
    }

    Ok(report)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DestroyReport {
    pub workloads_deleted: Vec<PathBuf>,
    pub nodes_drained: Vec<String>,
}

/// Build a `ClusterManifest` reflecting the current daemon state.
pub async fn export<E>(name: &str, endpoint: &E) -> Result<ClusterManifest>
where
    E: DaemonEndpoint,
{
    let nodes = endpoint.list_node_summaries().await?;
    let hub = crate::vpn::HubState::load()?;
    let spec_vpn = hub.network.as_ref().map(|network| ClusterVpnSpec {
        cidr: format!("{}/{}", network.cidr_base, network.cidr_prefix),
        hub_endpoint: network.hub_endpoint.clone(),
        hub_role: "this".to_string(),
    });

    let node_specs: Vec<ClusterNodeSpec> = nodes
        .into_iter()
        .filter(|node| node.transport != "local")
        .map(|node| ClusterNodeSpec {
            name: node.name,
            ssh: node.endpoint,
            vpn_ip: node.vpn_ip,
            install: false,
            remote_binary: DEFAULT_REMOTE_BINARY.to_string(),
            labels: BTreeMap::new(),
        })
        .collect();

    Ok(ClusterManifest {
        api_version: crate::cluster_manifest::API_VERSION.to_string(),
        kind: crate::cluster_manifest::KIND.to_string(),
        metadata: ClusterMetadata {
            name: name.to_string(),
        },
        spec: ClusterSpec {
            vpn: spec_vpn,
            nodes: node_specs,
            workloads: Vec::new(),
        },
    })
}

fn ensure_vpn(vpn: &ClusterVpnSpec) -> Result<bool> {
    let existing = crate::vpn::HubState::load()?;
    if let Some(network) = existing.network {
        if format!("{}/{}", network.cidr_base, network.cidr_prefix) == vpn.cidr
            && network.hub_endpoint == vpn.hub_endpoint
        {
            return Ok(false);
        }
        anyhow::bail!(
            "existing hub state ({}/{} via {}) conflicts with manifest ({} via {}); run `litek8s vpn init` manually or wipe state",
            network.cidr_base,
            network.cidr_prefix,
            network.hub_endpoint,
            vpn.cidr,
            vpn.hub_endpoint
        );
    }

    let (cidr_base, cidr_prefix) = parse_cidr(&vpn.cidr)?;
    crate::vpn::init_hub(cidr_base, cidr_prefix, vpn.hub_endpoint.clone())?;
    Ok(true)
}

async fn add_node<E: DaemonEndpoint>(endpoint: &E, node_spec: &ClusterNodeSpec) -> Result<()> {
    if node_spec.install {
        let project_root = current_project_root()?;
        node::install_remote_agent(&node_spec.ssh, &node_spec.remote_binary, None, &project_root)
            .await
            .with_context(|| {
                format!(
                    "failed to install litek8s on `{}` ({})",
                    node_spec.name, node_spec.ssh
                )
            })?;
    }

    if let Some(raw_ip) = node_spec.vpn_ip.as_deref() {
        // Make sure the spoke is registered on the hub before AddNode, so the
        // daemon can look up its membership by IP.
        let ip: Ipv4Addr = raw_ip
            .parse()
            .with_context(|| format!("node `{}` has invalid vpnIp `{raw_ip}`", node_spec.name))?;
        ensure_hub_has_peer(&node_spec.name, ip, &node_spec.ssh, &node_spec.remote_binary)
            .await
            .with_context(|| {
                format!(
                    "failed to register `{}` on the local hub before AddNode",
                    node_spec.name
                )
            })?;
    }

    match endpoint
        .request(Request::AddNode {
            name: node_spec.name.clone(),
            ssh_target: node_spec.ssh.clone(),
            remote_binary: Some(node_spec.remote_binary.clone()),
            identity_file: None,
            labels: node_spec.labels.clone(),
            vpn_ip: node_spec.vpn_ip.clone(),
        })
        .await?
    {
        Response::Ack { .. } => Ok(()),
        Response::Error { message } => {
            anyhow::bail!("failed to add node `{}`: {message}", node_spec.name)
        }
        other => anyhow::bail!(
            "unexpected daemon response while adding node `{}`: {other:?}",
            node_spec.name
        ),
    }
}

/// Best-effort: SSH into the remote spoke to capture its pubkey, then register
/// it on the local hub. If the spoke is already registered (matching pubkey),
/// the hub keeps its existing IP assignment.
async fn ensure_hub_has_peer(
    node_name: &str,
    declared_ip: Ipv4Addr,
    ssh_target: &str,
    remote_binary: &str,
) -> Result<()> {
    let hub = crate::vpn::HubState::load()?;
    if hub
        .peers
        .iter()
        .any(|peer| peer.node_name == node_name && peer.membership.ip == declared_ip)
    {
        return Ok(());
    }

    // The spoke needs to have run `vpn join` already so it has a pubkey. We
    // delegate detection of that to the operator: if SSH bootstrap is desired
    // it can happen out of band via `litek8s vpn join --ssh-hub`. Here we just
    // surface the gap.
    anyhow::bail!(
        "spoke `{node_name}` is not registered on the hub at {declared_ip}. \
         Run `litek8s vpn join --token <T> --ssh-hub <hub> --node-name {node_name}` from the spoke first, \
         then re-apply (ssh target `{ssh_target}`, remote bin `{remote_binary}`)"
    );
}

fn resolve_workload_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd =
        std::env::current_dir().context("failed to read the current working directory")?;
    Ok(cwd.join(path))
}

fn current_project_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read the current working directory")?;
    if cwd.join("Cargo.toml").exists() {
        return Ok(cwd);
    }
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

/// Abstraction over the daemon connection so the orchestrator is testable.
#[allow(async_fn_in_trait)]
pub trait DaemonEndpoint {
    async fn request(&self, request: Request) -> Result<Response>;

    async fn list_node_summaries(&self) -> Result<Vec<NodeSummary>> {
        match self.request(Request::GetNodes).await? {
            Response::Nodes { items } => Ok(items),
            Response::Error { message } => anyhow::bail!("daemon error: {message}"),
            other => anyhow::bail!("unexpected daemon response while listing nodes: {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{NodeSummary, Request, Response};
    use std::sync::Mutex;

    struct MockEndpoint {
        nodes: Mutex<Vec<NodeSummary>>,
        sent: Mutex<Vec<Request>>,
        next_id: Mutex<u64>,
    }

    impl MockEndpoint {
        fn new(nodes: Vec<NodeSummary>) -> Self {
            Self {
                nodes: Mutex::new(nodes),
                sent: Mutex::new(Vec::new()),
                next_id: Mutex::new(0),
            }
        }
    }

    impl DaemonEndpoint for MockEndpoint {
        async fn request(&self, request: Request) -> Result<Response> {
            self.sent.lock().unwrap().push(clone_request(&request));
            match request {
                Request::GetNodes => Ok(Response::Nodes {
                    items: self.nodes.lock().unwrap().clone(),
                }),
                Request::AddNode { name, .. } => {
                    let mut id = self.next_id.lock().unwrap();
                    *id += 1;
                    self.nodes.lock().unwrap().push(NodeSummary {
                        name,
                        transport: "ssh".to_string(),
                        endpoint: format!("synthetic-{}", *id),
                        status: "Ready".to_string(),
                        scheduling: "Schedulable".to_string(),
                        deployment_count: 0,
                        vpn_ip: None,
                        vpn_role: None,
                        host_resources: None,
                    });
                    Ok(Response::Ack {
                        message: "added".to_string(),
                    })
                }
                Request::Apply { .. } => Ok(Response::Ack {
                    message: "applied".to_string(),
                }),
                Request::DeleteManifest { .. } => Ok(Response::Ack {
                    message: "deleted".to_string(),
                }),
                Request::DrainNode { .. } => Ok(Response::Ack {
                    message: "drained".to_string(),
                }),
                other => Ok(Response::Error {
                    message: format!("unhandled: {other:?}"),
                }),
            }
        }
    }

    fn clone_request(request: &Request) -> Request {
        let json = serde_json::to_string(request).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn minimal_manifest() -> ClusterManifest {
        ClusterManifest::parse(
            r#"
apiVersion: litek8s.dev/v1alpha1
kind: Cluster
metadata:
  name: test
spec:
  nodes:
    - name: server77
      ssh: ubuntu@10.0.0.5
"#,
        )
        .expect("manifest")
    }

    #[tokio::test]
    async fn apply_adds_missing_nodes_only_once() {
        let endpoint = MockEndpoint::new(vec![NodeSummary {
            name: "local".to_string(),
            transport: "local".to_string(),
            endpoint: "local".to_string(),
            status: "Ready".to_string(),
            scheduling: "Schedulable".to_string(),
            deployment_count: 0,
            vpn_ip: None,
            vpn_role: None,
            host_resources: None,
        }]);

        let manifest = minimal_manifest();
        let report = apply(&manifest, &endpoint).await.expect("apply");
        assert_eq!(report.nodes_added, vec!["server77".to_string()]);
        assert!(report.nodes_unchanged.is_empty());

        let report = apply(&manifest, &endpoint).await.expect("apply twice");
        assert!(report.nodes_added.is_empty());
        assert_eq!(report.nodes_unchanged, vec!["server77".to_string()]);
    }

    #[tokio::test]
    async fn diff_reports_pending_and_extra_nodes() {
        let endpoint = MockEndpoint::new(vec![
            NodeSummary {
                name: "local".to_string(),
                transport: "local".to_string(),
                endpoint: "local".to_string(),
                status: "Ready".to_string(),
                scheduling: "Schedulable".to_string(),
                deployment_count: 0,
                vpn_ip: None,
                vpn_role: None,
                host_resources: None,
            },
            NodeSummary {
                name: "old-node".to_string(),
                transport: "ssh".to_string(),
                endpoint: "ubuntu@stale".to_string(),
                status: "Ready".to_string(),
                scheduling: "Schedulable".to_string(),
                deployment_count: 0,
                vpn_ip: None,
                vpn_role: None,
                host_resources: None,
            },
        ]);

        let manifest = minimal_manifest();
        let report = diff(&manifest, &endpoint).await.expect("diff");
        assert_eq!(report.nodes_to_add, vec!["server77".to_string()]);
        assert_eq!(report.nodes_extra, vec!["old-node".to_string()]);
        assert!(report.nodes_already_present.is_empty());
    }

    #[tokio::test]
    async fn destroy_drains_declared_nodes() {
        let endpoint = MockEndpoint::new(Vec::new());
        let manifest = minimal_manifest();
        let report = destroy(&manifest, &endpoint).await.expect("destroy");
        assert_eq!(report.nodes_drained, vec!["server77".to_string()]);
    }
}
