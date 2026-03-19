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
        labels: BTreeMap<String, String>,
    },
    SetNodeScheduling {
        name: String,
        unschedulable: bool,
    },
    DeleteManifest {
        manifest: String,
    },
    DeleteDeployment {
        name: String,
    },
    GetDeployments,
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
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ack {
        message: String,
    },
    Deployments {
        items: Vec<DeploymentSummary>,
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
    Top {
        daemon: DaemonTopSummary,
        items: Vec<DeploymentTopRow>,
    },
    Error {
        message: String,
    },
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
    pub image: String,
    pub container_name: String,
    pub container_id: Option<String>,
    pub message: Option<String>,
    pub command: Vec<String>,
    pub args: Vec<String>,
    pub ports: Vec<String>,
    pub cpu_limit: Option<String>,
    pub memory_limit: Option<String>,
    pub restart_count: u64,
    pub restart_streak: u32,
    pub last_exit: Option<String>,
    pub next_retry_in_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentTopRow {
    pub name: String,
    pub node_name: String,
    pub status: String,
    pub image: String,
    pub container_id: Option<String>,
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

pub async fn send_message<T>(stream: &mut UnixStream, message: &T) -> Result<()>
where
    T: Serialize,
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

pub async fn read_message<T>(stream: &mut UnixStream) -> Result<T>
where
    T: DeserializeOwned,
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
