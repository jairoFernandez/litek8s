use std::collections::HashSet;
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClusterNode {
    pub name: String,
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

impl ClusterNode {
    pub fn local() -> Self {
        Self {
            name: LOCAL_NODE_NAME.to_string(),
            transport: NodeTransport::Local,
        }
    }

    pub fn ssh(name: String, target: String, remote_binary: String) -> Result<Self> {
        validate_node_name(&name)?;
        ensure!(
            !target.trim().is_empty(),
            "ssh target must not be empty for node `{name}`"
        );
        ensure!(
            !remote_binary.trim().is_empty(),
            "remote binary path must not be empty for node `{name}`"
        );

        Ok(Self {
            name,
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

#[cfg(test)]
mod tests {
    use super::{ClusterNode, DEFAULT_REMOTE_BINARY, LOCAL_NODE_NAME, validate_node_name};

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
        )
        .expect("ssh node should build");
        assert_eq!(node.endpoint(), "server77");
        assert_eq!(node.transport_label(), "ssh");
    }
}
