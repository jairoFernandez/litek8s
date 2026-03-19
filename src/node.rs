use std::collections::{BTreeMap, HashSet};
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum NodeTransport {
    Local,
    Ssh {
        target: String,
        remote_binary: String,
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
    Error { message: String },
}

#[derive(Clone)]
pub enum NodeClient {
    Local(DockerRuntime),
    Ssh(SshNodeClient),
}

#[derive(Clone)]
pub struct SshNodeClient {
    target: String,
    remote_binary: String,
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
        }
    }

    pub fn ssh(
        name: String,
        target: String,
        remote_binary: String,
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
            },
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
    pub fn for_node(node: &ClusterNode, local_runtime: DockerRuntime) -> Self {
        match &node.transport {
            NodeTransport::Local => Self::Local(local_runtime),
            NodeTransport::Ssh {
                target,
                remote_binary,
            } => Self::Ssh(SshNodeClient {
                target: target.clone(),
                remote_binary: remote_binary.clone(),
            }),
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
}

impl SshNodeClient {
    async fn request(&self, request: AgentRequest) -> Result<AgentResponse> {
        let payload =
            serde_json::to_vec(&request).context("failed to serialize remote agent request")?;

        let mut child = Command::new("ssh")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ConnectTimeout=5")
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
}

pub async fn install_remote_agent(
    ssh_target: &str,
    remote_binary: &str,
    project_root: &Path,
) -> Result<RemoteInstallSummary> {
    ensure_safe_remote_path(remote_binary)?;

    let build_spec = detect_remote_build_spec(ssh_target).await?;
    let artifact_path = build_release_artifact(project_root, &build_spec).await?;
    ensure_remote_parent_dir(ssh_target, remote_binary).await?;
    copy_artifact_to_remote(&artifact_path, ssh_target, remote_binary).await?;
    mark_remote_binary_executable(ssh_target, remote_binary).await?;

    let client = SshNodeClient {
        target: ssh_target.to_string(),
        remote_binary: remote_binary.to_string(),
    };
    match client.request(AgentRequest::Ping).await? {
        AgentResponse::Pong => Ok(RemoteInstallSummary {
            platform_label: build_spec.platform_label,
            artifact_path,
        }),
        other => bail!("unexpected remote agent response after install: {other:?}"),
    }
}

pub async fn run_agent_stdio() -> Result<()> {
    let runtime = DockerRuntime::connect()?;
    runtime.ping().await?;

    let mut buffer = Vec::new();
    io::stdin()
        .read_to_end(&mut buffer)
        .await
        .context("failed to read agent request from stdin")?;

    let request: AgentRequest =
        serde_json::from_slice(&buffer).context("failed to parse agent request JSON")?;
    let response = match handle_agent_request(runtime, request).await {
        Ok(response) => response,
        Err(error) => AgentResponse::Error {
            message: format!("{error:#}"),
        },
    };

    let payload =
        serde_json::to_vec(&response).context("failed to serialize agent response JSON")?;
    io::stdout()
        .write_all(&payload)
        .await
        .context("failed to write agent response to stdout")?;
    Ok(())
}

async fn handle_agent_request(
    runtime: DockerRuntime,
    request: AgentRequest,
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
    }
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

async fn detect_remote_build_spec(ssh_target: &str) -> Result<RemoteBuildSpec> {
    let uname = run_ssh_capture(ssh_target, "uname -s -m").await?;
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

async fn ensure_remote_parent_dir(ssh_target: &str, remote_binary: &str) -> Result<()> {
    let parent = remote_parent_dir(remote_binary);
    run_ssh_status(ssh_target, &format!("mkdir -p {parent}"))
        .await
        .with_context(|| format!("failed to create remote directory `{parent}` on {ssh_target}"))
}

async fn copy_artifact_to_remote(
    artifact_path: &Path,
    ssh_target: &str,
    remote_binary: &str,
) -> Result<()> {
    let destination = format!("{ssh_target}:{remote_binary}");
    run_local_inherit(
        "scp",
        &[artifact_path.to_string_lossy().as_ref(), &destination],
    )
    .await
    .with_context(|| {
        format!(
            "failed to copy `{}` to {destination}",
            artifact_path.display()
        )
    })
}

async fn mark_remote_binary_executable(ssh_target: &str, remote_binary: &str) -> Result<()> {
    run_ssh_status(ssh_target, &format!("chmod +x {remote_binary}"))
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

async fn run_ssh_capture(ssh_target: &str, remote_command: &str) -> Result<String> {
    let output = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=5")
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

async fn run_ssh_status(ssh_target: &str, remote_command: &str) -> Result<()> {
    let status = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=5")
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
