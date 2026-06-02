use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Apply {
        manifest: String,
    },
    AddNode {
        name: String,
        ssh_target: String,
        remote_binary: Option<String>,
        #[serde(default)]
        identity_file: Option<String>,
        labels: BTreeMap<String, String>,
        #[serde(default)]
        vpn_ip: Option<String>,
    },
    SetNodeScheduling {
        name: String,
        unschedulable: bool,
    },
    DrainNode {
        name: String,
    },
    DeleteManifest {
        manifest: String,
    },
    DeleteDeployment {
        name: String,
    },
    AddService {
        name: String,
        deployment_name: String,
        listen_addr: String,
        target_port: Option<u16>,
    },
    DeleteService {
        name: String,
    },
    GetDeployments,
    GetServices,
    GetNodes,
    GetEvents {
        limit: usize,
        since_id: Option<u64>,
        deployment_name: Option<String>,
        node_name: Option<String>,
    },
    DescribeDeployment {
        name: String,
    },
    DescribeService {
        name: String,
    },
    DescribeNode {
        name: String,
    },
    Logs {
        name: String,
        tail: usize,
    },
    ResolveProxyTarget {
        name: String,
        port: Option<u16>,
    },
    Top,
    GetClusterStatus,
    JoinCluster {
        node_id: u64,
        addr: String,
    },
    /// Multiply `left · right`, fanning row bands across the cluster's
    /// schedulable nodes and stitching the partial results back together.
    DistributedMatmul {
        left: Vec<Vec<f64>>,
        right: Vec<Vec<f64>>,
        /// Cache `right` on each node by content hash and reference it on the
        /// compute call, so it transfers once per node instead of per band.
        #[serde(default)]
        use_cache: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ack {
        message: String,
    },
    Deployments {
        items: Vec<DeploymentSummary>,
    },
    Services {
        items: Vec<ServiceSummary>,
    },
    Nodes {
        items: Vec<NodeSummary>,
    },
    Events {
        items: Vec<EventItem>,
    },
    Description {
        item: DeploymentDescription,
    },
    NodeDescription {
        item: NodeDescription,
    },
    Logs {
        content: String,
    },
    ProxyTarget {
        item: ProxyTarget,
    },
    ServiceDescription {
        item: ServiceDescription,
    },
    Top {
        daemon: DaemonTopSummary,
        items: Vec<DeploymentTopRow>,
        #[serde(default)]
        nodes: Vec<NodeSummary>,
    },
    ClusterStatus {
        ha_enabled: bool,
        node_id: Option<u64>,
        leader_id: Option<u64>,
        state: String,
        members: Vec<ClusterMember>,
    },
    Matmul {
        result: Vec<Vec<f64>>,
        placements: Vec<MatmulPlacement>,
        wall_ms: u64,
    },
    Error {
        message: String,
    },
}

/// One node's slice of a distributed matmul: which rows it owned and how long
/// its band took (including the SSH round-trip for remote nodes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatmulPlacement {
    pub node_name: String,
    pub transport: String,
    pub start_row: usize,
    pub rows: usize,
    pub elapsed_ms: u64,
    /// True when the node already had the weight matrix cached (no transfer);
    /// false when this run uploaded it. None when caching was disabled.
    #[serde(default)]
    pub weight_cached: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentSummary {
    pub name: String,
    pub node_name: String,
    pub status: String,
    pub image: String,
    pub container_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentDescription {
    pub name: String,
    pub api_version: String,
    pub node_name: String,
    pub requested_node_name: Option<String>,
    pub node_selector: Vec<String>,
    pub status: String,
    pub ready: bool,
    pub image: String,
    pub container_name: String,
    pub container_id: Option<String>,
    pub message: Option<String>,
    pub command: Vec<String>,
    pub args: Vec<String>,
    pub ports: Vec<String>,
    pub readiness_probe: Option<String>,
    pub cpu_limit: Option<String>,
    pub memory_limit: Option<String>,
    pub restart_count: u64,
    pub restart_streak: u32,
    pub last_exit: Option<String>,
    pub next_retry_in_secs: Option<u64>,
    #[serde(default)]
    pub conditions: Vec<DeploymentCondition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentTopRow {
    pub name: String,
    pub node_name: String,
    pub status: String,
    pub ready: bool,
    pub image: String,
    pub container_id: Option<String>,
    #[serde(default)]
    pub ports: Vec<String>,
    pub restart_count: u64,
    pub restart_streak: u32,
    pub last_exit: Option<String>,
    pub next_retry_in_secs: Option<u64>,
    pub cpu_percent: Option<f64>,
    pub memory_usage_bytes: Option<u64>,
    pub memory_limit_bytes: Option<u64>,
    pub network_rx_bytes: Option<u64>,
    pub network_tx_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentCondition {
    pub type_name: String,
    pub status: String,
    pub reason: String,
    pub message: Option<String>,
    pub last_transition_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonTopSummary {
    pub pid: u32,
    pub cpu_percent: Option<f64>,
    pub memory_bytes: u64,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSummary {
    pub name: String,
    pub transport: String,
    pub endpoint: String,
    pub status: String,
    pub scheduling: String,
    pub deployment_count: usize,
    #[serde(default)]
    pub vpn_ip: Option<String>,
    #[serde(default)]
    pub vpn_role: Option<String>,
    #[serde(default)]
    pub host_resources: Option<HostResources>,
}

/// Real host-level resource metrics for a node, sampled by the node agent
/// (local node) or the remote `litek8s agent` (SSH nodes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostResources {
    pub cpu_cores: u32,
    pub cpu_usage_percent: f64,
    pub memory_total_bytes: u64,
    pub memory_used_bytes: u64,
    pub disk_total_bytes: u64,
    pub disk_free_bytes: u64,
    pub load_avg_1m: f64,
    pub load_avg_5m: f64,
    pub load_avg_15m: f64,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeDescription {
    pub name: String,
    pub transport: String,
    pub endpoint: String,
    pub remote_binary: Option<String>,
    pub labels: Vec<String>,
    pub status: String,
    pub scheduling: String,
    pub deployment_count: usize,
    pub message: Option<String>,
    #[serde(default)]
    pub vpn: Option<NodeVpnInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeVpnInfo {
    pub role: String,
    pub ip: String,
    pub pubkey: String,
    pub endpoint: Option<String>,
    pub status: String,
    pub last_handshake_unix: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceSummary {
    pub name: String,
    pub deployment_name: String,
    pub listen_addr: String,
    pub target_port: Option<u16>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceDescription {
    pub name: String,
    pub deployment_name: String,
    pub listen_addr: String,
    pub target_port: Option<u16>,
    pub status: String,
    pub message: Option<String>,
    pub current_node_name: Option<String>,
    pub current_upstream: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventItem {
    pub id: u64,
    pub timestamp_secs: u64,
    pub level: String,
    pub reason: String,
    pub object_kind: String,
    pub object_name: String,
    pub node_name: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyTarget {
    pub deployment_name: String,
    pub node_name: String,
    pub transport: String,
    pub ssh_target: Option<String>,
    pub container_port: u16,
    pub host_port: u16,
    pub protocol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterMember {
    pub node_id: u64,
    pub addr: String,
    pub is_leader: bool,
}

pub async fn send_message<S>(stream: &mut S, message: &impl Serialize) -> Result<()>
where
    S: AsyncWriteExt + Unpin,
{
    let payload = serde_json::to_vec(message).context("failed to serialize protocol message")?;
    stream
        .write_u32(payload.len() as u32)
        .await
        .context("failed to write message length")?;
    stream
        .write_all(&payload)
        .await
        .context("failed to write message body")?;
    Ok(())
}

pub async fn read_message<T, S>(stream: &mut S) -> Result<T>
where
    T: DeserializeOwned,
    S: AsyncReadExt + Unpin,
{
    let length = stream
        .read_u32()
        .await
        .context("failed to read message length")?;
    let mut buffer = vec![0; length as usize];
    stream
        .read_exact(&mut buffer)
        .await
        .context("failed to read message body")?;
    serde_json::from_slice(&buffer).context("failed to deserialize protocol message")
}

pub async fn request(socket_path: &Path, request: Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("failed to connect to {}", socket_path.display()))?;
    send_message(&mut stream, &request).await?;
    read_message(&mut stream).await
}

pub async fn request_tcp(addr: &str, req: Request) -> Result<Response> {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .with_context(|| format!("failed to connect to {addr}"))?;
    send_message(&mut stream, &req).await?;
    read_message(&mut stream).await
}
