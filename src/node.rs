use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::manifest::DeploymentManifest;
use crate::runtime::docker::{
    ContainerExitState, ContainerStatsSnapshot, DockerRuntime, ManagedContainer,
};
use crate::runtime::{ContainerRuntime, LocalRuntime};
use crate::vpn::{self, JoinToken, PublicKey, VpnMembership};

pub const LOCAL_NODE_NAME: &str = "local";
pub const DEFAULT_REMOTE_BINARY: &str = "~/.local/bin/litek8s";
const RUST_BUILD_IMAGE: &str = "rust:1.90";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClusterNode {
    pub name: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub unschedulable: bool,
    pub transport: NodeTransport,
    #[serde(default)]
    pub vpn: Option<VpnMembership>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum NodeTransport {
    Local,
    Ssh {
        target: String,
        remote_binary: String,
        #[serde(default)]
        identity_file: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentRequest {
    Ping,
    ListManagedContainers,
    ListContainersForDeployment {
        name: String,
    },
    CreateAndStart {
        manifest: DeploymentManifest,
        spec_hash: String,
    },
    RemoveContainer {
        container_id: String,
    },
    Logs {
        name: String,
        tail: usize,
    },
    InspectExitState {
        container_id: String,
    },
    StatsSnapshot {
        container_id: String,
    },
    ProbeContainerTcp {
        container_id: String,
        port: u16,
        timeout_ms: u64,
    },
    VpnAttach {
        node_name: String,
        spoke_pubkey: PublicKey,
    },
    HostResources,
    /// Multiply a row band of a matrix against the full right-hand matrix. The
    /// node computes the band with its own cores and returns the result rows.
    ComputeMatmulBand {
        left_band: Vec<Vec<f64>>,
        right: Vec<Vec<f64>>,
    },
    /// Ask whether the node already has a weight matrix cached on disk, so the
    /// caller can skip re-uploading it.
    HasWeights {
        weight_id: String,
    },
    /// Upload a weight matrix to the node's on-disk cache under `weight_id`.
    /// The matrix itself is NOT in this JSON — it follows the request line as a
    /// raw binary blob (the `write_matrix_bin` format), so a multi-MB weight
    /// ships at 8 bytes/f64 instead of ~18 chars/f64 as JSON text. `byte_len`
    /// lets the agent read exactly the blob.
    PutWeights {
        weight_id: String,
        byte_len: u64,
    },
    /// Multiply a row band against a weight matrix already cached on the node,
    /// avoiding re-transfer of the (large) right-hand matrix.
    ComputeMatmulBandCached {
        left_band: Vec<Vec<f64>>,
        weight_id: String,
    },
    /// Compute this node's column shard of a pipeline layer: multiply the full
    /// activation against the weight columns cached under `weight_id` (stored
    /// column-major), optionally applying ReLU. Returns the activation slice for
    /// the columns this node owns.
    ComputeLayerShardCached {
        activation: Vec<Vec<f64>>,
        weight_id: String,
        relu: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentResponse {
    Pong,
    ManagedContainers { items: Vec<ManagedContainer> },
    ContainerCreated { id: String },
    Ack,
    Logs { content: String },
    ExitState { state: ContainerExitState },
    StatsSnapshot { snapshot: ContainerStatsSnapshot },
    VpnAssignment {
        membership: VpnMembership,
        token: JoinToken,
    },
    HostResources { resources: crate::protocol::HostResources },
    MatmulBand { rows: Vec<Vec<f64>> },
    HasWeights { present: bool },
    Error { message: String },
}

#[derive(Clone)]
pub enum NodeClient {
    Local(LocalRuntime),
    Ssh(SshNodeClient),
}

#[derive(Clone)]
pub struct SshNodeClient {
    target: String,
    remote_binary: String,
    identity_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInstallSummary {
    pub platform_label: String,
    pub artifact_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteBuildSpec {
    platform_label: String,
    docker_platform: &'static str,
    artifact_tag: &'static str,
}

impl ClusterNode {
    pub fn local() -> Self {
        Self {
            name: LOCAL_NODE_NAME.to_string(),
            labels: default_local_labels(),
            unschedulable: false,
            transport: NodeTransport::Local,
            vpn: None,
        }
    }

    pub fn ssh(
        name: String,
        target: String,
        remote_binary: String,
        identity_file: Option<String>,
        labels: BTreeMap<String, String>,
    ) -> Result<Self> {
        validate_node_name(&name)?;
        ensure!(
            !target.trim().is_empty(),
            "ssh target must not be empty for node `{name}`"
        );
        ensure!(
            !remote_binary.trim().is_empty(),
            "remote binary path must not be empty for node `{name}`"
        );
        validate_labels(&labels)?;

        Ok(Self {
            name,
            labels,
            unschedulable: false,
            transport: NodeTransport::Ssh {
                target,
                remote_binary,
                identity_file,
            },
            vpn: None,
        })
    }

    pub fn transport_label(&self) -> &'static str {
        match self.transport {
            NodeTransport::Local => "local",
            NodeTransport::Ssh { .. } => "ssh",
        }
    }

    pub fn endpoint(&self) -> String {
        match &self.transport {
            NodeTransport::Local => "local Docker socket".to_string(),
            NodeTransport::Ssh { target, .. } => target.clone(),
        }
    }

    pub fn matches_selector(&self, selector: &BTreeMap<String, String>) -> bool {
        selector
            .iter()
            .all(|(key, value)| self.labels.get(key) == Some(value))
    }

    pub fn printable_labels(&self) -> Vec<String> {
        self.labels
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect()
    }

    pub fn scheduling_label(&self) -> &'static str {
        if self.unschedulable {
            "Cordoned"
        } else {
            "Schedulable"
        }
    }

    /// SSH target preferred for connecting to this node.
    ///
    /// When the node has a registered VPN membership, the target is rewritten
    /// to use the VPN IP so traffic flows over the mesh instead of the public
    /// cloud endpoint. Falls back to the original SSH target when no VPN IP
    /// is known.
    pub fn preferred_ssh_target(&self) -> Option<String> {
        let NodeTransport::Ssh { target, .. } = &self.transport else {
            return None;
        };
        let Some(vpn) = self.vpn.as_ref() else {
            return Some(target.clone());
        };
        let user = target.split_once('@').map(|(user, _)| user);
        Some(match user {
            Some(user) => format!("{user}@{}", vpn.ip),
            None => vpn.ip.to_string(),
        })
    }
}

pub fn select_node_for_manifest(
    manifest: &DeploymentManifest,
    nodes: &[ClusterNode],
    current_assigned: Option<&str>,
) -> Result<String> {
    if let Some(explicit) = manifest.explicit_node_name() {
        let node = nodes
            .iter()
            .find(|node| node.name == explicit)
            .with_context(|| format!("target node `{explicit}` is not registered"))?;
        ensure!(
            node.matches_selector(manifest.node_selector()),
            "target node `{explicit}` does not satisfy nodeSelector"
        );
        return Ok(explicit.to_string());
    }

    if let Some(current_assigned) = current_assigned {
        if let Some(node) = nodes.iter().find(|node| node.name == current_assigned) {
            if node.matches_selector(manifest.node_selector()) {
                return Ok(node.name.clone());
            }
        }
    }

    if manifest.node_selector().is_empty() {
        if let Some(local) = nodes.iter().find(|node| node.name == LOCAL_NODE_NAME) {
            if !local.unschedulable {
                return Ok(local.name.clone());
            }
        }

        return nodes
            .iter()
            .find(|node| !node.unschedulable)
            .map(|node| node.name.clone())
            .context("no schedulable node is available");
    }

    nodes
        .iter()
        .find(|node| !node.unschedulable && node.matches_selector(manifest.node_selector()))
        .map(|node| node.name.clone())
        .with_context(|| {
            format!(
                "no schedulable node matches nodeSelector `{}`",
                format_selector(manifest.node_selector())
            )
        })
}

impl NodeClient {
    pub fn for_node(node: &ClusterNode, local_runtime: impl Into<LocalRuntime>) -> Self {
        match &node.transport {
            NodeTransport::Local => Self::Local(local_runtime.into()),
            NodeTransport::Ssh {
                target,
                remote_binary,
                identity_file,
            } => {
                let effective = node
                    .preferred_ssh_target()
                    .unwrap_or_else(|| target.clone());
                Self::Ssh(SshNodeClient {
                    target: effective,
                    remote_binary: remote_binary.clone(),
                    identity_file: identity_file.clone(),
                })
            }
        }
    }

    pub async fn ping(&self) -> Result<()> {
        match self {
            Self::Local(runtime) => runtime.ping().await,
            Self::Ssh(client) => match client.request(AgentRequest::Ping).await? {
                AgentResponse::Pong => Ok(()),
                other => bail!("unexpected agent response to ping: {other:?}"),
            },
        }
    }

    pub async fn host_resources(&self) -> Result<crate::protocol::HostResources> {
        match self {
            Self::Local(_) => collect_host_resources().await,
            Self::Ssh(client) => match client.request(AgentRequest::HostResources).await? {
                AgentResponse::HostResources { resources } => Ok(resources),
                other => bail!("unexpected agent response for host resources: {other:?}"),
            },
        }
    }

    /// Compute one matmul row band on this node. The local node runs it in a
    /// blocking thread pool; an SSH node ships the band to its remote agent.
    pub async fn compute_matmul_band(
        &self,
        left_band: Vec<Vec<f64>>,
        right: Vec<Vec<f64>>,
    ) -> Result<Vec<Vec<f64>>> {
        match self {
            Self::Local(_) => tokio::task::spawn_blocking(move || {
                crate::matrix::multiply_band(&left_band, &right)
            })
            .await
            .context("local matmul worker panicked")?,
            Self::Ssh(client) => match client
                .request(AgentRequest::ComputeMatmulBand { left_band, right })
                .await?
            {
                AgentResponse::MatmulBand { rows } => Ok(rows),
                other => bail!("unexpected agent response while computing matmul band: {other:?}"),
            },
        }
    }

    /// Whether this node already has the given weight matrix cached on disk.
    pub async fn has_weights(&self, weight_id: &str) -> Result<bool> {
        match self {
            Self::Local(_) => Ok(local_weight_path(weight_id)?.exists()),
            Self::Ssh(client) => match client
                .request(AgentRequest::HasWeights {
                    weight_id: weight_id.to_string(),
                })
                .await?
            {
                AgentResponse::HasWeights { present } => Ok(present),
                other => bail!("unexpected agent response while checking weights: {other:?}"),
            },
        }
    }

    /// Upload a weight matrix to this node's on-disk cache under `weight_id`,
    /// stored at `dtype` (e.g. F16 to keep native weight precision and a quarter
    /// the bytes of f64).
    pub async fn put_weights(
        &self,
        weight_id: &str,
        matrix: Vec<Vec<f64>>,
        dtype: crate::matrix::BinDtype,
    ) -> Result<()> {
        let blob = crate::matrix::encode_matrix_bin_dtype(&matrix, dtype)?;
        match self {
            Self::Local(_) => {
                let path = local_weight_path(weight_id)?;
                tokio::task::spawn_blocking(move || crate::matrix::write_bin_blob(&path, &blob))
                    .await
                    .context("local weight write panicked")?
            }
            Self::Ssh(client) => {
                // Ship the encoded blob raw after the request — native bytes, not
                // JSON float text.
                match client
                    .request_blob(
                        AgentRequest::PutWeights {
                            weight_id: weight_id.to_string(),
                            byte_len: blob.len() as u64,
                        },
                        &blob,
                    )
                    .await?
                {
                    AgentResponse::Ack => Ok(()),
                    other => bail!("unexpected agent response while uploading weights: {other:?}"),
                }
            }
        }
    }

    /// Upload the weight matrix only if the node does not already have it, then
    /// return whether it was already cached (true = cache hit, no transfer).
    pub async fn ensure_weights(
        &self,
        weight_id: &str,
        matrix: &[Vec<f64>],
        dtype: crate::matrix::BinDtype,
    ) -> Result<bool> {
        if self.has_weights(weight_id).await? {
            return Ok(true);
        }
        self.put_weights(weight_id, matrix.to_vec(), dtype).await?;
        Ok(false)
    }

    /// Multiply a row band against a weight matrix already cached on this node.
    pub async fn compute_matmul_band_cached(
        &self,
        left_band: Vec<Vec<f64>>,
        weight_id: &str,
    ) -> Result<Vec<Vec<f64>>> {
        match self {
            Self::Local(_) => {
                let path = local_weight_path(weight_id)?;
                tokio::task::spawn_blocking(move || {
                    let right = crate::matrix::read_matrix_bin(&path)?;
                    crate::matrix::multiply_band(&left_band, &right)
                })
                .await
                .context("local cached matmul worker panicked")?
            }
            Self::Ssh(client) => match client
                .request(AgentRequest::ComputeMatmulBandCached {
                    left_band,
                    weight_id: weight_id.to_string(),
                })
                .await?
            {
                AgentResponse::MatmulBand { rows } => Ok(rows),
                other => {
                    bail!("unexpected agent response while computing cached matmul band: {other:?}")
                }
            },
        }
    }

    /// Compute this node's column shard of a pipeline layer against weight
    /// columns cached under `weight_id` (stored column-major: each cached row is
    /// one weight column). Returns this node's slice of the layer output.
    pub async fn compute_layer_shard_cached(
        &self,
        activation: Vec<Vec<f64>>,
        weight_id: &str,
        relu: bool,
    ) -> Result<Vec<Vec<f64>>> {
        match self {
            Self::Local(_) => {
                let path = local_weight_path(weight_id)?;
                tokio::task::spawn_blocking(move || {
                    let shard = crate::matrix::WeightShard::read(&path)?;
                    let mut out = crate::matrix::multiply_weight_shard(&activation, &shard)?;
                    if relu {
                        crate::matrix::apply_relu(&mut out);
                    }
                    Ok(out)
                })
                .await
                .context("local layer shard worker panicked")?
            }
            Self::Ssh(client) => match client
                .request(AgentRequest::ComputeLayerShardCached {
                    activation,
                    weight_id: weight_id.to_string(),
                    relu,
                })
                .await?
            {
                AgentResponse::MatmulBand { rows } => Ok(rows),
                other => {
                    bail!("unexpected agent response while computing layer shard: {other:?}")
                }
            },
        }
    }

    pub async fn list_managed_containers(&self) -> Result<Vec<ManagedContainer>> {
        match self {
            Self::Local(runtime) => runtime.list_managed_containers().await,
            Self::Ssh(client) => match client.request(AgentRequest::ListManagedContainers).await? {
                AgentResponse::ManagedContainers { items } => Ok(items),
                other => bail!("unexpected agent response while listing containers: {other:?}"),
            },
        }
    }

    pub async fn list_containers_for_deployment(
        &self,
        name: &str,
    ) -> Result<Vec<ManagedContainer>> {
        match self {
            Self::Local(runtime) => runtime.list_containers_for_deployment(name).await,
            Self::Ssh(client) => match client
                .request(AgentRequest::ListContainersForDeployment {
                    name: name.to_string(),
                })
                .await?
            {
                AgentResponse::ManagedContainers { items } => Ok(items),
                other => bail!(
                    "unexpected agent response while listing deployment containers: {other:?}"
                ),
            },
        }
    }

    pub async fn create_and_start(
        &self,
        manifest: &DeploymentManifest,
        spec_hash: &str,
    ) -> Result<String> {
        match self {
            Self::Local(runtime) => runtime.create_and_start(manifest, spec_hash).await,
            Self::Ssh(client) => match client
                .request(AgentRequest::CreateAndStart {
                    manifest: manifest.clone(),
                    spec_hash: spec_hash.to_string(),
                })
                .await?
            {
                AgentResponse::ContainerCreated { id } => Ok(id),
                other => bail!("unexpected agent response while creating a container: {other:?}"),
            },
        }
    }

    pub async fn remove_container(&self, container_id: &str) -> Result<()> {
        match self {
            Self::Local(runtime) => runtime.remove_container(container_id).await,
            Self::Ssh(client) => match client
                .request(AgentRequest::RemoveContainer {
                    container_id: container_id.to_string(),
                })
                .await?
            {
                AgentResponse::Ack => Ok(()),
                other => bail!("unexpected agent response while removing a container: {other:?}"),
            },
        }
    }

    pub async fn remove_deployment(&self, name: &str) -> Result<()> {
        let containers = self.list_containers_for_deployment(name).await?;
        for container in containers {
            self.remove_container(&container.id).await?;
        }
        Ok(())
    }

    pub async fn wait_until_deployment_cleared(&self, name: &str, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.list_containers_for_deployment(name).await?.is_empty() {
                return Ok(());
            }

            if tokio::time::Instant::now() > deadline {
                bail!("timed out waiting for deployment `{name}` containers to disappear");
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn remove_orphans(&self, desired_names: &HashSet<String>) -> Result<()> {
        for container in self.list_managed_containers().await? {
            if !desired_names.contains(&container.deployment) {
                self.remove_container(&container.id).await?;
            }
        }
        Ok(())
    }

    pub async fn logs(&self, name: &str, tail: usize) -> Result<String> {
        match self {
            Self::Local(runtime) => runtime.logs(name, tail).await,
            Self::Ssh(client) => match client
                .request(AgentRequest::Logs {
                    name: name.to_string(),
                    tail,
                })
                .await?
            {
                AgentResponse::Logs { content } => Ok(content),
                other => bail!("unexpected agent response while fetching logs: {other:?}"),
            },
        }
    }

    pub async fn inspect_exit_state(&self, container_id: &str) -> Result<ContainerExitState> {
        match self {
            Self::Local(runtime) => runtime.inspect_exit_state(container_id).await,
            Self::Ssh(client) => match client
                .request(AgentRequest::InspectExitState {
                    container_id: container_id.to_string(),
                })
                .await?
            {
                AgentResponse::ExitState { state } => Ok(state),
                other => bail!("unexpected agent response while inspecting exit state: {other:?}"),
            },
        }
    }

    pub async fn stats_snapshot_for_container(
        &self,
        container_id: &str,
    ) -> Result<ContainerStatsSnapshot> {
        match self {
            Self::Local(runtime) => runtime.stats_snapshot_for_container(container_id).await,
            Self::Ssh(client) => match client
                .request(AgentRequest::StatsSnapshot {
                    container_id: container_id.to_string(),
                })
                .await?
            {
                AgentResponse::StatsSnapshot { snapshot } => Ok(snapshot),
                other => bail!("unexpected agent response while reading stats: {other:?}"),
            },
        }
    }

    pub async fn probe_container_tcp(
        &self,
        container_id: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<()> {
        match self {
            Self::Local(runtime) => {
                runtime
                    .probe_container_tcp(container_id, port, timeout)
                    .await
            }
            Self::Ssh(client) => match client
                .request(AgentRequest::ProbeContainerTcp {
                    container_id: container_id.to_string(),
                    port,
                    timeout_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
                })
                .await?
            {
                AgentResponse::Ack => Ok(()),
                other => bail!("unexpected agent response while probing container TCP: {other:?}"),
            },
        }
    }
}

impl SshNodeClient {
    pub fn new(target: String, remote_binary: String, identity_file: Option<String>) -> Self {
        Self {
            target,
            remote_binary,
            identity_file,
        }
    }

    pub async fn send_agent_request(&self, request: AgentRequest) -> Result<AgentResponse> {
        self.request(request).await
    }

    async fn request(&self, request: AgentRequest) -> Result<AgentResponse> {
        self.request_blob(request, &[]).await
    }

    /// Like `request`, but appends a raw binary blob after the JSON request line
    /// on stdin — used to ship weight matrices at native byte size instead of as
    /// JSON float text.
    async fn request_blob(&self, request: AgentRequest, blob: &[u8]) -> Result<AgentResponse> {
        // Transient SSH transport failures warrant dropping the ControlMaster
        // and retrying with a short backoff:
        //
        // 1. A freshly-booted node can hand the ControlMaster a login session
        //    that predates the `docker` group landing in the user's session —
        //    every multiplexed call then fails the Docker ping with EACCES.
        // 2. A cold target races ControlMaster setup: ssh exits 0 having
        //    multiplexed onto a half-open master, leaving stdout empty (EOF).
        // 3. Under a burst of concurrent calls to the same host (a pipeline
        //    layer fans out many shards at once), ssh connects race the master
        //    and exit non-zero (status 255) before the socket is shared.
        //
        // All three clear by reopening the connection, so retry a few times,
        // backing off so the master has a moment to settle.
        const MAX_ATTEMPTS: u32 = 4;
        let mut last = self.request_once(&request, blob).await;
        let mut attempt = 1;
        while attempt < MAX_ATTEMPTS {
            let Err(error) = &last else { break };
            if !(is_docker_permission_error(error)
                || is_cold_transport_error(error)
                || is_transient_ssh_error(error))
            {
                break;
            }
            self.close_control_master().await;
            tokio::time::sleep(Duration::from_millis(150 * u64::from(attempt))).await;
            last = self.request_once(&request, blob).await;
            attempt += 1;
        }
        last
    }

    /// Issue a single agent request over the (possibly multiplexed) SSH
    /// connection. The wire is `<compact-json>\n<blob>`: serde_json emits no raw
    /// newlines, so the first `\n` cleanly delimits the request from the binary
    /// blob that follows (empty for most requests).
    async fn request_once(&self, request: &AgentRequest, blob: &[u8]) -> Result<AgentResponse> {
        let mut payload =
            serde_json::to_vec(request).context("failed to serialize remote agent request")?;
        payload.push(b'\n');
        payload.extend_from_slice(blob);

        // Reuse a persistent SSH connection per target via ControlMaster so that
        // only the first request pays the full handshake cost; subsequent pings
        // and agent calls multiplex over the existing socket in a few ms.
        let control_path = control_socket_path(&self.target);

        let mut command = Command::new("ssh");
        command
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ConnectTimeout=5");
        if let Some(identity) = self.identity_file.as_deref() {
            command
                .arg("-i")
                .arg(identity)
                .arg("-o")
                .arg("IdentitiesOnly=yes");
        }
        let mut child = command
            .arg("-o")
            .arg("ControlMaster=auto")
            .arg("-o")
            .arg(format!("ControlPath={control_path}"))
            .arg("-o")
            .arg("ControlPersist=60s")
            .arg(&self.target)
            .arg(&self.remote_binary)
            .arg("agent")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn ssh for remote node {}", self.target))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&payload).await.with_context(|| {
                format!("failed to send request to remote node {}", self.target)
            })?;
            stdin.shutdown().await.ok();
        }

        let output = child
            .wait_with_output()
            .await
            .with_context(|| format!("failed to wait for ssh on remote node {}", self.target))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let detail = if stderr.is_empty() {
                format!("ssh exited with status {}", output.status)
            } else {
                stderr
            };
            bail!("remote node {} failed: {detail}", self.target);
        }

        let response: AgentResponse = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("failed to decode agent response from {}", self.target))?;
        match response {
            AgentResponse::Error { message } => Err(anyhow!(message)),
            other => Ok(other),
        }
    }

    /// Tear down the persistent ControlMaster for this target so the next request
    /// opens a fresh login session. Best-effort: `ssh -O exit` fails harmlessly if
    /// no master is currently running.
    async fn close_control_master(&self) {
        let control_path = control_socket_path(&self.target);
        let _ = Command::new("ssh")
            .arg("-o")
            .arg(format!("ControlPath={control_path}"))
            .arg("-O")
            .arg("exit")
            .arg(&self.target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
}

/// True when an agent failure is the remote Docker daemon rejecting us for lack
/// of permission — the signature of a multiplexed SSH session that opened before
/// `docker` group membership activated on a freshly-provisioned node.
fn is_docker_permission_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("failed to ping the Docker daemon")
        && (message.contains("Permission denied") || message.contains("os error 13"))
}

/// True when an agent failure is the SSH transport coming up cold rather than a
/// real agent error: ssh exited 0 but stdout was empty (the response failed to
/// decode with an EOF), or the multiplexed channel dropped before the agent
/// answered. Reopening the ControlMaster and retrying clears it.
fn is_cold_transport_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("failed to decode agent response")
        && (message.contains("EOF while parsing")
            || message.contains("line 1 column 0"))
}

/// True when ssh itself failed to connect rather than the agent erroring —
/// status 255 (ssh's connect-failure code), connection refused/reset, or a
/// timeout. Common when many calls fan out to one host at once and race the
/// ControlMaster; reopening and retrying clears it.
fn is_transient_ssh_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("exit status: 255")
        || message.contains("Connection refused")
        || message.contains("Connection reset")
        || message.contains("Connection closed")
        || message.contains("Connection timed out")
        || message.contains("kex_exchange_identification")
        // ssh died mid-upload before reading all of stdin — common when many
        // large PutWeights payloads hit one host at once and it drops a session.
        || message.contains("Broken pipe")
}

/// Build a stable per-target SSH ControlPath. Unix domain sockets cap the path
/// at ~104 bytes (macOS) and ssh appends a ~17-char random suffix under
/// ControlPersist, so the base must stay short. A long `$TMPDIR` plus a
/// sanitized `user@long.ip.add.ress` target blew past the limit; instead hash
/// the target to a fixed 16 hex chars and root the socket at `/tmp` (short and
/// always present), keeping the whole path well under the cap regardless of how
/// long the target is.
fn control_socket_path(target: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in target.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("/tmp/lk8s-{hash:016x}.sock")
}

pub async fn install_remote_agent(
    ssh_target: &str,
    remote_binary: &str,
    identity_file: Option<&str>,
    project_root: &Path,
) -> Result<RemoteInstallSummary> {
    ensure_safe_remote_path(remote_binary)?;

    let build_spec = detect_remote_build_spec(ssh_target, identity_file).await?;
    let artifact_path = build_release_artifact(project_root, &build_spec).await?;
    ensure_remote_parent_dir(ssh_target, remote_binary, identity_file).await?;
    copy_artifact_to_remote(&artifact_path, ssh_target, remote_binary, identity_file).await?;
    mark_remote_binary_executable(ssh_target, remote_binary, identity_file).await?;

    let client = SshNodeClient {
        target: ssh_target.to_string(),
        remote_binary: remote_binary.to_string(),
        identity_file: identity_file.map(str::to_string),
    };

    // Verify with a few retries: right after the atomic rename, the first agent
    // spawn can race the SSH ControlMaster connection that the copy/chmod just
    // used and return EOF. The binary is in place, so a short retry settles it.
    let mut last_error = None;
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        match client.request(AgentRequest::Ping).await {
            Ok(AgentResponse::Pong) => {
                return Ok(RemoteInstallSummary {
                    platform_label: build_spec.platform_label,
                    artifact_path,
                });
            }
            Ok(other) => {
                bail!("unexpected remote agent response after install: {other:?}")
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("agent did not answer after install")))
}


/// Path of the persistent agent's Unix socket on the node.
fn agent_socket_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("litek8s").join("agent.sock")
}

/// Run the persistent agent: listen on a Unix socket, serve framed requests, and
/// keep weight shards resident in RAM across requests. Each connection carries
/// one `<json>\n<blob>` request and gets one JSON response, but the in-RAM
/// WeightCache persists for the daemon's lifetime — so a model is read from disk
/// once and later inferences hit memory, and the costly DockerRuntime::connect
/// is paid once instead of per request.
/// TCP port the persistent agent listens on (in addition to its Unix socket) so
/// other cluster nodes can reach it over the private subnet. The security group
/// restricts this to intra-cluster traffic.
pub const AGENT_TCP_PORT: u16 = 6444;

pub async fn run_agent_serve() -> Result<()> {
    use tokio::net::{TcpListener, UnixListener};

    let runtime = DockerRuntime::connect()?;
    runtime.ping().await?;

    let socket = agent_socket_path();
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if socket.exists() {
        std::fs::remove_file(&socket).ok();
    }
    let unix = UnixListener::bind(&socket)
        .with_context(|| format!("failed to bind agent socket {}", socket.display()))?;
    // Also listen on TCP so an in-cluster coordinator can drive this node over
    // the LAN. Binding may fail if another agent already holds the port; that's
    // fine (the Unix socket still works for local proxying).
    let tcp = TcpListener::bind(("0.0.0.0", AGENT_TCP_PORT)).await.ok();
    eprintln!(
        "litek8s agent serving on {} and tcp/{}",
        socket.display(),
        if tcp.is_some() { AGENT_TCP_PORT } else { 0 }
    );

    let cache = WeightCache::new();
    loop {
        tokio::select! {
            accepted = unix.accept() => {
                if let Ok((mut stream, _)) = accepted {
                    let (runtime, cache) = (runtime.clone(), cache.clone());
                    tokio::spawn(async move {
                        if let Err(error) = serve_one(&mut stream, runtime, &cache).await {
                            eprintln!("agent request failed: {error:#}");
                        }
                    });
                }
            }
            accepted = async { match &tcp { Some(l) => l.accept().await.map(|(s, _)| s), None => std::future::pending().await } } => {
                if let Ok(mut stream) = accepted {
                    let (runtime, cache) = (runtime.clone(), cache.clone());
                    tokio::spawn(async move {
                        if let Err(error) = serve_one(&mut stream, runtime, &cache).await {
                            eprintln!("agent request failed: {error:#}");
                        }
                    });
                }
            }
        }
    }
}

/// Handle one connection on the persistent agent's socket: read a framed
/// `<json>\n<blob>` request, dispatch it against the shared cache, write the
/// JSON response. Generic over the stream type so it serves both the local Unix
/// socket and TCP from other cluster nodes.
async fn serve_one<S>(stream: &mut S, runtime: DockerRuntime, cache: &WeightCache) -> Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let mut buffer = Vec::new();
    stream
        .read_to_end(&mut buffer)
        .await
        .context("failed to read agent request from socket")?;
    let split = buffer.iter().position(|&b| b == b'\n');
    let (json_bytes, blob): (&[u8], &[u8]) = match split {
        Some(idx) => (&buffer[..idx], &buffer[idx + 1..]),
        None => (&buffer[..], &[]),
    };
    let request: AgentRequest =
        serde_json::from_slice(json_bytes).context("failed to parse agent request JSON")?;
    let response = match handle_agent_request(runtime, request, blob, cache).await {
        Ok(response) => response,
        Err(error) => AgentResponse::Error {
            message: format!("{error:#}"),
        },
    };
    let payload =
        serde_json::to_vec(&response).context("failed to serialize agent response JSON")?;
    stream
        .write_all(&payload)
        .await
        .context("failed to write agent response to socket")?;
    stream.shutdown().await.ok();
    Ok(())
}

/// One-shot proxy mode (`litek8s agent`, default): forward the stdin request to
/// the node's persistent agent over its Unix socket, starting that daemon if it
/// is not already running, and relay the response to stdout. This keeps the SSH
/// contract (one request per invocation) while the heavy state lives in the
/// resident daemon.
pub async fn run_agent_proxy() -> Result<()> {
    let mut buffer = Vec::new();
    io::stdin()
        .read_to_end(&mut buffer)
        .await
        .context("failed to read agent request from stdin")?;

    let socket = agent_socket_path();
    let mut stream = match connect_or_start_daemon(&socket).await {
        Ok(stream) => stream,
        Err(error) => {
            // If the daemon can't be reached or started, fall back to handling
            // the request in-process so the node still works (no resident cache).
            eprintln!("persistent agent unavailable ({error:#}); handling in-process");
            return handle_buffer_in_process(&buffer).await;
        }
    };

    stream
        .write_all(&buffer)
        .await
        .context("failed to forward request to agent daemon")?;
    stream.shutdown().await.ok();
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .context("failed to read response from agent daemon")?;
    io::stdout()
        .write_all(&response)
        .await
        .context("failed to relay agent response to stdout")?;
    Ok(())
}

/// Connect to the daemon socket, launching `litek8s agent --serve` detached if
/// nothing is listening yet, then retrying the connect briefly.
async fn connect_or_start_daemon(socket: &Path) -> Result<tokio::net::UnixStream> {
    use tokio::net::UnixStream;

    if let Ok(stream) = UnixStream::connect(socket).await {
        return Ok(stream);
    }
    // Launch the daemon detached so it outlives this short-lived proxy process.
    let exe = std::env::current_exe().context("failed to find own binary to start agent daemon")?;
    Command::new("setsid")
        .arg(&exe)
        .arg("agent")
        .arg("--serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .or_else(|_| {
            // setsid may be absent; fall back to a bare detached spawn.
            Command::new(&exe)
                .arg("agent")
                .arg("--serve")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
        })
        .context("failed to start persistent agent daemon")?;

    for attempt in 0..20 {
        tokio::time::sleep(Duration::from_millis(100 * (attempt + 1).min(5))).await;
        if let Ok(stream) = UnixStream::connect(socket).await {
            return Ok(stream);
        }
    }
    bail!("agent daemon did not start listening on {}", socket.display());
}

/// Fallback: handle a buffered request in-process with an ephemeral cache.
async fn handle_buffer_in_process(buffer: &[u8]) -> Result<()> {
    let runtime = DockerRuntime::connect()?;
    runtime.ping().await?;
    let split = buffer.iter().position(|&b| b == b'\n');
    let (json_bytes, blob): (&[u8], &[u8]) = match split {
        Some(idx) => (&buffer[..idx], &buffer[idx + 1..]),
        None => (buffer, &[]),
    };
    let request: AgentRequest =
        serde_json::from_slice(json_bytes).context("failed to parse agent request JSON")?;
    let cache = WeightCache::new();
    let response = match handle_agent_request(runtime, request, blob, &cache).await {
        Ok(response) => response,
        Err(error) => AgentResponse::Error {
            message: format!("{error:#}"),
        },
    };
    let payload = serde_json::to_vec(&response)?;
    io::stdout().write_all(&payload).await?;
    Ok(())
}

/// In-RAM cache of weight shards, keyed by weight_id. A persistent agent keeps
/// shards resident across requests so a model is read from disk once and later
/// inferences hit memory. A one-shot agent gets a fresh (empty) cache that dies
/// with the process — correct, just not resident.
#[derive(Clone, Default)]
pub struct WeightCache {
    shards: std::sync::Arc<tokio::sync::Mutex<HashMap<String, std::sync::Arc<crate::matrix::WeightShard>>>>,
}

impl WeightCache {
    fn new() -> Self {
        Self::default()
    }

    /// Return the cached shard, loading it from `path` (native precision) on a
    /// miss and inserting it for next time.
    async fn get_or_load(
        &self,
        weight_id: &str,
        path: &Path,
    ) -> Result<std::sync::Arc<crate::matrix::WeightShard>> {
        let mut guard = self.shards.lock().await;
        if let Some(shard) = guard.get(weight_id) {
            return Ok(shard.clone());
        }
        let path = path.to_path_buf();
        let shard = tokio::task::spawn_blocking(move || crate::matrix::WeightShard::read(&path))
            .await
            .context("weight shard read panicked")??;
        let shard = std::sync::Arc::new(shard);
        guard.insert(weight_id.to_string(), shard.clone());
        Ok(shard)
    }

    /// Drop a cached shard (e.g. after its file is overwritten).
    async fn invalidate(&self, weight_id: &str) {
        self.shards.lock().await.remove(weight_id);
    }
}

async fn handle_agent_request(
    runtime: DockerRuntime,
    request: AgentRequest,
    blob: &[u8],
    cache: &WeightCache,
) -> Result<AgentResponse> {
    match request {
        AgentRequest::Ping => {
            runtime.ping().await?;
            Ok(AgentResponse::Pong)
        }
        AgentRequest::ListManagedContainers => Ok(AgentResponse::ManagedContainers {
            items: runtime.list_managed_containers().await?,
        }),
        AgentRequest::ListContainersForDeployment { name } => {
            Ok(AgentResponse::ManagedContainers {
                items: runtime.list_containers_for_deployment(&name).await?,
            })
        }
        AgentRequest::CreateAndStart {
            manifest,
            spec_hash,
        } => Ok(AgentResponse::ContainerCreated {
            id: runtime.create_and_start(&manifest, &spec_hash).await?,
        }),
        AgentRequest::RemoveContainer { container_id } => {
            runtime.remove_container(&container_id).await?;
            Ok(AgentResponse::Ack)
        }
        AgentRequest::Logs { name, tail } => Ok(AgentResponse::Logs {
            content: runtime.logs(&name, tail).await?,
        }),
        AgentRequest::InspectExitState { container_id } => Ok(AgentResponse::ExitState {
            state: runtime.inspect_exit_state(&container_id).await?,
        }),
        AgentRequest::StatsSnapshot { container_id } => Ok(AgentResponse::StatsSnapshot {
            snapshot: runtime.stats_snapshot_for_container(&container_id).await?,
        }),
        AgentRequest::ProbeContainerTcp {
            container_id,
            port,
            timeout_ms,
        } => {
            runtime
                .probe_container_tcp(&container_id, port, Duration::from_millis(timeout_ms))
                .await?;
            Ok(AgentResponse::Ack)
        }
        AgentRequest::VpnAttach {
            node_name,
            spoke_pubkey,
        } => {
            let membership = vpn::register_spoke(&node_name, spoke_pubkey)?;
            let hub_state = vpn::HubState::load()?;
            let network = hub_state
                .network
                .context("hub has no network — run `litek8s vpn init` first")?;
            let token = JoinToken {
                hub_pubkey: network.hub_pubkey.clone(),
                hub_endpoint: network.hub_endpoint.clone(),
                cidr_base: network.cidr_base,
                cidr_prefix: network.cidr_prefix,
            };
            Ok(AgentResponse::VpnAssignment { membership, token })
        }
        AgentRequest::HostResources => Ok(AgentResponse::HostResources {
            resources: collect_host_resources().await?,
        }),
        AgentRequest::ComputeMatmulBand { left_band, right } => {
            let rows = tokio::task::spawn_blocking(move || {
                crate::matrix::multiply_band(&left_band, &right)
            })
            .await
            .context("matmul worker panicked")??;
            Ok(AgentResponse::MatmulBand { rows })
        }
        AgentRequest::HasWeights { weight_id } => {
            let path = local_weight_path(&weight_id)?;
            Ok(AgentResponse::HasWeights {
                present: path.exists(),
            })
        }
        AgentRequest::PutWeights {
            weight_id,
            byte_len,
        } => {
            ensure!(
                blob.len() as u64 == byte_len,
                "weight blob length {} does not match declared {byte_len}",
                blob.len()
            );
            let path = local_weight_path(&weight_id)?;
            let bytes = blob.to_vec();
            // The blob is already the on-disk bin format — validate its header
            // and write it atomically without re-encoding.
            tokio::task::spawn_blocking(move || crate::matrix::write_bin_blob(&path, &bytes))
                .await
                .context("weight write panicked")??;
            // Drop any stale resident copy so the next compute reloads it.
            cache.invalidate(&weight_id).await;
            Ok(AgentResponse::Ack)
        }
        AgentRequest::ComputeMatmulBandCached {
            left_band,
            weight_id,
        } => {
            let path = local_weight_path(&weight_id)?;
            let rows = tokio::task::spawn_blocking(move || {
                let right = crate::matrix::read_matrix_bin(&path)?;
                crate::matrix::multiply_band(&left_band, &right)
            })
            .await
            .context("cached matmul worker panicked")??;
            Ok(AgentResponse::MatmulBand { rows })
        }
        AgentRequest::ComputeLayerShardCached {
            activation,
            weight_id,
            relu,
        } => {
            let path = local_weight_path(&weight_id)?;
            // Resolve the shard through the in-RAM cache: a persistent agent
            // reads it from disk once, then serves later inferences from memory.
            let shard = cache.get_or_load(&weight_id, &path).await?;
            let rows = tokio::task::spawn_blocking(move || {
                let mut out = crate::matrix::multiply_weight_shard(&activation, &shard)?;
                if relu {
                    crate::matrix::apply_relu(&mut out);
                }
                Ok::<_, anyhow::Error>(out)
            })
            .await
            .context("layer shard worker panicked")??;
            Ok(AgentResponse::MatmulBand { rows })
        }
    }
}

/// Resolve the on-disk path for a cached weight matrix, validating the id so it
/// can never escape the weights directory.
fn local_weight_path(weight_id: &str) -> Result<PathBuf> {
    crate::matrix::validate_weight_id(weight_id)?;
    Ok(crate::config::weights_dir()?.join(format!("{weight_id}.bin")))
}

/// Sample host-level resources via `sysinfo`. CPU usage needs two samples spaced
/// by sysinfo's minimum interval, so this briefly sleeps; callers cache the
/// result (default 5s) so the cost is paid at most once per refresh window.
pub async fn collect_host_resources() -> Result<crate::protocol::HostResources> {
    use sysinfo::{Disks, MINIMUM_CPU_UPDATE_INTERVAL, System};

    let mut system = System::new();
    // First CPU sample, wait the minimum interval, then re-sample for a real %.
    system.refresh_cpu_all();
    tokio::time::sleep(MINIMUM_CPU_UPDATE_INTERVAL).await;
    system.refresh_cpu_all();
    system.refresh_memory();

    let cpu_cores = system.cpus().len() as u32;
    let cpu_usage_percent = system.global_cpu_usage() as f64;
    let memory_total_bytes = system.total_memory();
    let memory_used_bytes = system.used_memory();

    // Pick the disk backing the root filesystem; fall back to the largest disk
    // so we report a real device rather than summing tmpfs/overlay mounts.
    let disks = Disks::new_with_refreshed_list();
    let root_disk = disks
        .iter()
        .find(|disk| disk.mount_point() == std::path::Path::new("/"))
        .or_else(|| disks.iter().max_by_key(|disk| disk.total_space()));
    let (disk_total_bytes, disk_free_bytes) = root_disk
        .map(|disk| (disk.total_space(), disk.available_space()))
        .unwrap_or((0, 0));

    let load = System::load_average();

    Ok(crate::protocol::HostResources {
        cpu_cores,
        cpu_usage_percent,
        memory_total_bytes,
        memory_used_bytes,
        disk_total_bytes,
        disk_free_bytes,
        load_avg_1m: load.one,
        load_avg_5m: load.five,
        load_avg_15m: load.fifteen,
        uptime_secs: System::uptime(),
    })
}

pub fn validate_node_name(name: &str) -> Result<()> {
    ensure!(!name.trim().is_empty(), "node name must not be empty");
    ensure!(
        name.chars()
            .all(|char| char.is_ascii_alphanumeric() || matches!(char, '-' | '_' | '.')),
        "node name `{name}` contains unsupported characters"
    );
    Ok(())
}

async fn detect_remote_build_spec(
    ssh_target: &str,
    identity_file: Option<&str>,
) -> Result<RemoteBuildSpec> {
    let uname = run_ssh_capture(ssh_target, "uname -s -m", identity_file).await?;
    parse_remote_build_spec(&uname)
}

fn parse_remote_build_spec(uname: &str) -> Result<RemoteBuildSpec> {
    let parts = uname.split_whitespace().collect::<Vec<_>>();
    ensure!(
        parts.len() >= 2,
        "failed to parse remote platform from `{uname}`"
    );

    match (parts[0], parts[1]) {
        ("Linux", "x86_64") | ("Linux", "amd64") => Ok(RemoteBuildSpec {
            platform_label: "linux/x86_64".to_string(),
            docker_platform: "linux/amd64",
            artifact_tag: "linux-amd64",
        }),
        ("Linux", "aarch64") | ("Linux", "arm64") => Ok(RemoteBuildSpec {
            platform_label: "linux/arm64".to_string(),
            docker_platform: "linux/arm64/v8",
            artifact_tag: "linux-arm64",
        }),
        (os, arch) => bail!(
            "automatic install currently supports Linux x86_64 and Linux arm64 nodes; remote host reported `{os} {arch}`"
        ),
    }
}

async fn build_release_artifact(
    project_root: &Path,
    build_spec: &RemoteBuildSpec,
) -> Result<PathBuf> {
    let uid = run_local_capture("id", &["-u"]).await?;
    let gid = run_local_capture("id", &["-g"]).await?;
    let target_dir = format!("/work/target/bootstrap/{}", build_spec.artifact_tag);

    let volume = format!("{}:/work", project_root.display());
    run_local_inherit(
        "docker",
        &[
            "run",
            "--rm",
            "--platform",
            build_spec.docker_platform,
            "-u",
            &format!("{uid}:{gid}"),
            "-e",
            &format!("CARGO_TARGET_DIR={target_dir}"),
            "-v",
            &volume,
            "-w",
            "/work",
            RUST_BUILD_IMAGE,
            "cargo",
            "build",
            "--release",
        ],
    )
    .await
    .with_context(|| {
        format!(
            "failed to build a {} litek8s binary via Docker",
            build_spec.platform_label
        )
    })?;

    let artifact_path = project_root
        .join("target")
        .join("bootstrap")
        .join(build_spec.artifact_tag)
        .join("release")
        .join("litek8s");
    ensure!(
        tokio::fs::try_exists(&artifact_path)
            .await
            .with_context(|| format!("failed to check {}", artifact_path.display()))?,
        "build finished but `{}` was not produced",
        artifact_path.display()
    );
    Ok(artifact_path)
}

async fn ensure_remote_parent_dir(
    ssh_target: &str,
    remote_binary: &str,
    identity_file: Option<&str>,
) -> Result<()> {
    let parent = remote_parent_dir(remote_binary);
    run_ssh_status(ssh_target, &format!("mkdir -p {parent}"), identity_file)
        .await
        .with_context(|| format!("failed to create remote directory `{parent}` on {ssh_target}"))
}

async fn copy_artifact_to_remote(
    artifact_path: &Path,
    ssh_target: &str,
    remote_binary: &str,
    identity_file: Option<&str>,
) -> Result<()> {
    // Copy to a sibling temp path, then atomically rename over the target. A
    // direct scp onto a running agent binary fails with "Text file busy"; the
    // rename swaps the inode so an in-flight agent keeps its old file.
    let staging = format!("{remote_binary}.new");
    let destination = format!("{ssh_target}:{staging}");
    let mut scp_args: Vec<String> = Vec::new();
    if let Some(identity) = identity_file {
        scp_args.push("-i".to_string());
        scp_args.push(identity.to_string());
        scp_args.push("-o".to_string());
        scp_args.push("IdentitiesOnly=yes".to_string());
    }
    scp_args.push(artifact_path.to_string_lossy().into_owned());
    scp_args.push(destination.clone());
    let scp_args_ref: Vec<&str> = scp_args.iter().map(String::as_str).collect();
    run_local_inherit("scp", &scp_args_ref)
        .await
        .with_context(|| {
            format!(
                "failed to copy `{}` to {destination}",
                artifact_path.display()
            )
        })?;

    run_ssh_status(
        ssh_target,
        &format!("mv -f {staging} {remote_binary}"),
        identity_file,
    )
    .await
    .with_context(|| {
        format!("failed to install `{remote_binary}` (atomic rename) on {ssh_target}")
    })
}

async fn mark_remote_binary_executable(
    ssh_target: &str,
    remote_binary: &str,
    identity_file: Option<&str>,
) -> Result<()> {
    run_ssh_status(ssh_target, &format!("chmod +x {remote_binary}"), identity_file)
        .await
        .with_context(|| format!("failed to mark `{remote_binary}` executable on {ssh_target}"))
}

fn ensure_safe_remote_path(path: &str) -> Result<()> {
    ensure!(
        !path.trim().is_empty(),
        "remote binary path must not be empty"
    );
    ensure!(
        path.chars().all(|char| {
            char.is_ascii_alphanumeric() || matches!(char, '/' | '.' | '_' | '-' | '~')
        }),
        "remote binary path `{path}` contains unsupported characters for automatic install"
    );
    Ok(())
}

fn remote_parent_dir(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(parent, _)| {
            if parent.is_empty() {
                ".".to_string()
            } else {
                parent.to_string()
            }
        })
        .unwrap_or_else(|| ".".to_string())
}

async fn run_local_capture(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to spawn `{program}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!("{program} exited with status {}", output.status)
        } else {
            stderr
        };
        bail!("{detail}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn run_local_inherit(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("failed to spawn `{program}`"))?;
    ensure!(status.success(), "`{program}` exited with status {status}");
    Ok(())
}

pub async fn run_ssh_capture_pub(
    ssh_target: &str,
    remote_command: &str,
    identity_file: Option<&str>,
) -> Result<String> {
    run_ssh_capture(ssh_target, remote_command, identity_file).await
}

async fn run_ssh_capture(
    ssh_target: &str,
    remote_command: &str,
    identity_file: Option<&str>,
) -> Result<String> {
    let mut command = Command::new("ssh");
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=5");
    if let Some(identity) = identity_file {
        command
            .arg("-i")
            .arg(identity)
            .arg("-o")
            .arg("IdentitiesOnly=yes");
    }
    let output = command
        .arg(ssh_target)
        .arg(remote_command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to spawn ssh for remote node {ssh_target}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!("ssh exited with status {}", output.status)
        } else {
            stderr
        };
        bail!("remote node {ssh_target} failed: {detail}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn run_ssh_status(
    ssh_target: &str,
    remote_command: &str,
    identity_file: Option<&str>,
) -> Result<()> {
    let mut command = Command::new("ssh");
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=5");
    if let Some(identity) = identity_file {
        command
            .arg("-i")
            .arg(identity)
            .arg("-o")
            .arg("IdentitiesOnly=yes");
    }
    let status = command
        .arg(ssh_target)
        .arg(remote_command)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("failed to spawn ssh for remote node {ssh_target}"))?;
    ensure!(
        status.success(),
        "remote node {ssh_target} command exited with status {status}"
    );
    Ok(())
}

pub fn validate_label_key(key: &str) -> Result<()> {
    ensure!(!key.trim().is_empty(), "label key must not be empty");
    ensure!(
        key.chars()
            .all(|char| { char.is_ascii_alphanumeric() || matches!(char, '.' | '-' | '_' | '/') }),
        "label key `{key}` contains unsupported characters"
    );
    Ok(())
}

pub fn validate_label_value(value: &str) -> Result<()> {
    ensure!(!value.trim().is_empty(), "label value must not be empty");
    ensure!(
        value
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || matches!(char, '.' | '-' | '_')),
        "label value `{value}` contains unsupported characters"
    );
    Ok(())
}

pub fn validate_labels(labels: &BTreeMap<String, String>) -> Result<()> {
    for (key, value) in labels {
        validate_label_key(key)?;
        validate_label_value(value)?;
    }
    Ok(())
}

fn default_local_labels() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "kubernetes.io/os".to_string(),
            std::env::consts::OS.to_string(),
        ),
        (
            "kubernetes.io/arch".to_string(),
            std::env::consts::ARCH.to_string(),
        ),
        ("litek8s.io/node".to_string(), LOCAL_NODE_NAME.to_string()),
    ])
}

fn format_selector(selector: &BTreeMap<String, String>) -> String {
    selector
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::{
        ClusterNode, DEFAULT_REMOTE_BINARY, LOCAL_NODE_NAME, parse_remote_build_spec,
        remote_parent_dir, select_node_for_manifest, validate_label_key, validate_label_value,
        validate_node_name,
    };
    use crate::manifest::{API_VERSION, DeploymentManifest};
    use std::collections::BTreeMap;

    #[test]
    fn prefers_vpn_ip_in_ssh_target_when_membership_present() {
        use crate::vpn::{KeyPair, VpnMembership};
        use std::net::Ipv4Addr;

        let mut node = ClusterNode::ssh(
            "civo-1".to_string(),
            "ec2-user@54.1.2.3".to_string(),
            DEFAULT_REMOTE_BINARY.to_string(),
            None,
            BTreeMap::new(),
        )
        .expect("ssh node should build");

        assert_eq!(
            node.preferred_ssh_target().as_deref(),
            Some("ec2-user@54.1.2.3")
        );

        let kp = KeyPair::generate();
        node.vpn = Some(VpnMembership::new_spoke(
            Ipv4Addr::new(10, 42, 0, 7),
            kp.public(),
        ));
        assert_eq!(
            node.preferred_ssh_target().as_deref(),
            Some("ec2-user@10.42.0.7")
        );
    }

    #[test]
    fn falls_back_to_bare_vpn_ip_when_target_has_no_user() {
        use crate::vpn::{KeyPair, VpnMembership};
        use std::net::Ipv4Addr;

        let mut node = ClusterNode::ssh(
            "civo-1".to_string(),
            "54.1.2.3".to_string(),
            DEFAULT_REMOTE_BINARY.to_string(),
            None,
            BTreeMap::new(),
        )
        .expect("ssh node should build");
        let kp = KeyPair::generate();
        node.vpn = Some(VpnMembership::new_spoke(
            Ipv4Addr::new(10, 42, 0, 9),
            kp.public(),
        ));
        assert_eq!(node.preferred_ssh_target().as_deref(), Some("10.42.0.9"));
    }

    #[tokio::test]
    async fn node_client_dispatches_to_fake_local_runtime() {
        use super::NodeClient;
        use crate::runtime::{FakeRuntime, LocalRuntime};
        use std::sync::Arc;

        let fake = Arc::new(FakeRuntime::new());
        let local = ClusterNode::local();
        let client = NodeClient::for_node(&local, LocalRuntime::Fake(fake.clone()));

        let manifest = DeploymentManifest::parse(&format!(
            r#"
apiVersion: {API_VERSION}
kind: Deployment
metadata:
  name: demo
spec:
  replicas: 1
  template:
    spec:
      restartPolicy: Always
      containers:
        - name: web
          image: nginx:alpine
"#
        ))
        .expect("manifest should parse");

        let id = client
            .create_and_start(&manifest, "hash1")
            .await
            .expect("create");
        assert_eq!(fake.create_calls(), 1);

        let containers = client
            .list_containers_for_deployment("demo")
            .await
            .expect("list");
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].id, id);
    }

    #[test]
    fn validates_node_names() {
        assert!(validate_node_name("server77").is_ok());
        assert!(validate_node_name("edge-1.demo").is_ok());
        assert!(validate_node_name("bad/name").is_err());
    }

    #[test]
    fn creates_local_and_ssh_nodes() {
        assert_eq!(ClusterNode::local().name, LOCAL_NODE_NAME);
        let node = ClusterNode::ssh(
            "server77".to_string(),
            "server77".to_string(),
            DEFAULT_REMOTE_BINARY.to_string(),
            None,
            BTreeMap::from([("role".to_string(), "edge".to_string())]),
        )
        .expect("ssh node should build");
        assert_eq!(node.endpoint(), "server77");
        assert_eq!(node.transport_label(), "ssh");
        assert!(node.matches_selector(&BTreeMap::from([("role".to_string(), "edge".to_string())])));
    }

    #[test]
    fn parses_supported_remote_build_specs() {
        let amd64 = parse_remote_build_spec("Linux x86_64").expect("linux amd64 should parse");
        assert_eq!(amd64.docker_platform, "linux/amd64");
        assert_eq!(amd64.artifact_tag, "linux-amd64");

        let arm64 = parse_remote_build_spec("Linux aarch64").expect("linux arm64 should parse");
        assert_eq!(arm64.docker_platform, "linux/arm64/v8");
        assert_eq!(arm64.artifact_tag, "linux-arm64");
    }

    #[test]
    fn rejects_unsupported_remote_build_specs() {
        assert!(parse_remote_build_spec("Darwin arm64").is_err());
        assert!(parse_remote_build_spec("garbage").is_err());
    }

    #[test]
    fn derives_remote_parent_dir() {
        assert_eq!(remote_parent_dir("~/.local/bin/litek8s"), "~/.local/bin");
        assert_eq!(remote_parent_dir("litek8s"), ".");
    }

    #[test]
    fn validates_label_parts() {
        assert!(validate_label_key("role").is_ok());
        assert!(validate_label_key("topology.k8s.io/zone").is_ok());
        assert!(validate_label_key("bad key").is_err());
        assert!(validate_label_value("edge").is_ok());
        assert!(validate_label_value("us-east-1a").is_ok());
        assert!(validate_label_value("bad value").is_err());
    }

    #[test]
    fn selects_node_from_selector_and_prefers_current_assignment() {
        let manifest = DeploymentManifest::parse(&format!(
            r#"
apiVersion: {API_VERSION}
kind: Deployment
metadata:
  name: demo
spec:
  replicas: 1
  template:
    spec:
      restartPolicy: Always
      nodeSelector:
        role: edge
      containers:
        - name: web
          image: nginx:alpine
"#
        ))
        .expect("manifest should parse");

        let nodes = vec![
            ClusterNode::local(),
            ClusterNode::ssh(
                "server77".to_string(),
                "server77".to_string(),
                DEFAULT_REMOTE_BINARY.to_string(),
                None,
                BTreeMap::from([("role".to_string(), "edge".to_string())]),
            )
            .expect("ssh node should build"),
        ];

        let selected =
            select_node_for_manifest(&manifest, &nodes, None).expect("selector should match");
        assert_eq!(selected, "server77");

        let preferred = select_node_for_manifest(&manifest, &nodes, Some("server77"))
            .expect("current node should be preserved");
        assert_eq!(preferred, "server77");
    }

    #[test]
    fn skips_cordoned_nodes_for_new_workloads_but_keeps_current_assignment() {
        let manifest = DeploymentManifest::parse(&format!(
            r#"
apiVersion: {API_VERSION}
kind: Deployment
metadata:
  name: demo
spec:
  replicas: 1
  template:
    spec:
      restartPolicy: Always
      nodeSelector:
        role: edge
      containers:
        - name: web
          image: nginx:alpine
"#
        ))
        .expect("manifest should parse");

        let mut server77 = ClusterNode::ssh(
            "server77".to_string(),
            "server77".to_string(),
            DEFAULT_REMOTE_BINARY.to_string(),
            None,
            BTreeMap::from([("role".to_string(), "edge".to_string())]),
        )
        .expect("ssh node should build");
        server77.unschedulable = true;

        let nodes = vec![ClusterNode::local(), server77];
        assert!(select_node_for_manifest(&manifest, &nodes, None).is_err());

        let preferred = select_node_for_manifest(&manifest, &nodes, Some("server77"))
            .expect("current assignment should be preserved on a cordoned node");
        assert_eq!(preferred, "server77");
    }
}
