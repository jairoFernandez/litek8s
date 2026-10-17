use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bollard::API_DEFAULT_VERSION;
use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, InspectContainerOptions, ListContainersOptions, LogOutput,
    LogsOptions, RemoveContainerOptions, StartContainerOptions, Stats, StatsOptions,
};
use bollard::errors::Error as BollardError;
use bollard::image::CreateImageOptions;
use bollard::models::{HostConfig, PortBinding};
use bollard::system::EventsOptions;
use futures_util::stream::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::Notify;

use crate::manifest::{
    ContainerPort, ContainerSpec, DeploymentManifest, NormalizedResourceLimits, Volume,
};
use crate::runtime::ContainerRuntime;

const LABEL_MANAGED: &str = "litek8s.managed";
const LABEL_DEPLOYMENT: &str = "litek8s.deployment";
const LABEL_HASH: &str = "litek8s.spec-hash";
const LABEL_ORDINAL: &str = "litek8s.ordinal";

#[derive(Clone)]
pub struct DockerRuntime {
    docker: Docker,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedContainer {
    pub id: String,
    pub name: String,
    pub deployment: String,
    pub state: Option<String>,
    pub status: Option<String>,
    pub running: bool,
    pub spec_hash: Option<String>,
    /// Replica ordinal from the `litek8s.ordinal` label. `None` for
    /// pre-multi-replica containers (treated as ordinal 0 by the reconciler).
    #[serde(default)]
    pub ordinal: Option<u32>,
    /// Bridge IP of the container. Not populated by list calls (would cost an
    /// inspect per container); filled on demand by the service-resolve path.
    #[serde(default)]
    pub container_ip: Option<String>,
    /// Creation time (unix seconds) as reported by the container list.
    #[serde(default)]
    pub created_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerMetrics {
    pub cpu_percent: Option<f64>,
    pub memory_usage_bytes: Option<u64>,
    pub memory_limit_bytes: Option<u64>,
    pub network_rx_bytes: Option<u64>,
    pub network_tx_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContainerStatsSnapshot {
    pub cpu_total_usage: u64,
    pub system_cpu_usage: Option<u64>,
    pub online_cpus: u64,
    pub fallback_cpu_percent: Option<f64>,
    pub memory_usage_bytes: Option<u64>,
    pub memory_limit_bytes: Option<u64>,
    pub network_rx_bytes: u64,
    pub network_tx_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContainerExitState {
    pub status: Option<String>,
    pub exit_code: Option<i64>,
    pub oom_killed: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredResourceLimits {
    pub nano_cpus: Option<i64>,
    pub memory_bytes: Option<i64>,
}

impl DockerRuntime {
    pub fn connect() -> Result<Self> {
        let docker = connect_docker()?;
        Ok(Self { docker })
    }

    /// The underlying Docker client, for runtimes layered on this one (the
    /// Kubernetes node agent's pod runtime).
    pub fn client(&self) -> Docker {
        self.docker.clone()
    }

    pub async fn ping(&self) -> Result<()> {
        self.docker
            .ping()
            .await
            .context("failed to ping the Docker daemon")?;
        Ok(())
    }

    pub fn spawn_event_listener(&self, notify: Arc<Notify>) -> tokio::task::JoinHandle<()> {
        let docker = self.docker.clone();
        tokio::spawn(async move {
            loop {
                let mut filters = HashMap::new();
                filters.insert("type".to_string(), vec!["container".to_string()]);
                filters.insert("label".to_string(), vec![format!("{LABEL_MANAGED}=true")]);

                let mut events = docker.events(Some(EventsOptions::<String> {
                    since: None,
                    until: None,
                    filters,
                }));

                while let Some(next_event) = events.next().await {
                    match next_event {
                        Ok(_) => notify.notify_one(),
                        Err(error) => {
                            eprintln!("docker event stream ended: {error}");
                            break;
                        }
                    }
                }

                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        })
    }

    pub async fn list_managed_containers(&self) -> Result<Vec<ManagedContainer>> {
        self.list_containers_with_filters(vec![format!("{LABEL_MANAGED}=true")])
            .await
    }

    pub async fn list_containers_for_deployment(
        &self,
        name: &str,
    ) -> Result<Vec<ManagedContainer>> {
        self.list_containers_with_filters(vec![
            format!("{LABEL_MANAGED}=true"),
            format!("{LABEL_DEPLOYMENT}={name}"),
        ])
        .await
    }

    pub async fn create_and_start(
        &self,
        manifest: &DeploymentManifest,
        spec_hash: &str,
        ordinal: u32,
    ) -> Result<String> {
        self.ensure_image(&manifest.container().image).await?;

        let container_name = replica_container_name(manifest.deployment_name(), ordinal);

        // Idempotency: if a container with this exact name already exists, adopt
        // it when it's still running with the right spec (a redundant create
        // triggered by the event-driven reconcile storm), else remove the stale
        // one so the name is free. Without this, an event flood can race two
        // creates into duplicate containers whose collapse churns the replica's
        // container ID.
        if let Some(existing) = self
            .list_containers_for_deployment(manifest.deployment_name())
            .await?
            .into_iter()
            .find(|c| c.name == container_name)
        {
            if existing.running && existing.spec_hash.as_deref() == Some(spec_hash) {
                return Ok(existing.id);
            }
            self.remove_container(&existing.id).await?;
        }

        // Replicas (>1) must not publish host ports — N containers can't share
        // one hostPort on a node. They are reachable only via the Service,
        // which connects to each container's IP:containerPort.
        let suppress_host_ports = manifest.desired_replicas() > 1;
        let config = build_container_config(manifest, spec_hash, ordinal, suppress_host_ports)?;

        let create_result = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: container_name,
                    platform: None,
                }),
                config,
            )
            .await
            .with_context(|| {
                format!(
                    "failed to create container for deployment {}",
                    manifest.deployment_name()
                )
            })?;

        self.docker
            .start_container(&create_result.id, None::<StartContainerOptions<String>>)
            .await
            .with_context(|| {
                format!(
                    "failed to start container for deployment {}",
                    manifest.deployment_name()
                )
            })?;

        Ok(create_result.id)
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

    /// Remove managed containers that belong to a KNOWN deployment but whose
    /// name is not desired (scaled-down ordinals). The set contains per-replica
    /// container names (`litek8s-<dep>-<ordinal>`); `known_deployments` scopes
    /// cleanup to deployments this daemon owns, so a managed container belonging
    /// to another deployment (e.g. a separate daemon sharing the Docker host) is
    /// left alone. Whole-deployment deletion is handled by the explicit delete
    /// path, not here.
    pub async fn remove_orphans(
        &self,
        desired_names: &HashSet<String>,
        known_deployments: &HashSet<String>,
    ) -> Result<()> {
        for container in self.list_managed_containers().await? {
            if known_deployments.contains(&container.deployment)
                && !desired_names.contains(&container.name)
            {
                self.remove_container(&container.id).await?;
            }
        }
        Ok(())
    }

    pub async fn remove_container(&self, container_id: &str) -> Result<()> {
        match self
            .docker
            .remove_container(
                container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(()),
            Err(BollardError::DockerResponseServerError {
                status_code: 409,
                message,
            }) if message.contains("already in progress") => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("failed to remove container {container_id}"))
            }
        }
    }

    pub async fn logs(&self, name: &str, tail: usize) -> Result<String> {
        let container = select_primary_container(self.list_containers_for_deployment(name).await?)
            .with_context(|| format!("deployment `{name}` has no managed container"))?;

        let mut stream = self.docker.logs(
            &container.id,
            Some(LogsOptions::<String> {
                follow: false,
                stdout: true,
                stderr: true,
                tail: tail.to_string(),
                ..Default::default()
            }),
        );

        let mut content = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(
                    LogOutput::StdOut { message }
                    | LogOutput::StdErr { message }
                    | LogOutput::Console { message }
                    | LogOutput::StdIn { message },
                ) => {
                    content.push_str(&String::from_utf8_lossy(&message));
                }
                Err(BollardError::DockerStreamError { .. }) => break,
                Err(_) if !content.is_empty() => return Ok(content),
                Err(error) => return Err(error).context("failed to fetch Docker logs"),
            }
        }

        Ok(content)
    }

    pub async fn configured_limits_for_deployment(
        &self,
        name: &str,
    ) -> Result<ConfiguredResourceLimits> {
        let container = select_primary_container(self.list_containers_for_deployment(name).await?)
            .with_context(|| format!("deployment `{name}` has no managed container"))?;
        let details = self
            .docker
            .inspect_container(&container.id, None::<InspectContainerOptions>)
            .await
            .with_context(|| format!("failed to inspect container {}", container.id))?;
        let host_config = details
            .host_config
            .context("inspected container did not include HostConfig")?;

        Ok(ConfiguredResourceLimits {
            nano_cpus: host_config.nano_cpus,
            memory_bytes: host_config.memory,
        })
    }

    pub async fn inspect_exit_state(&self, container_id: &str) -> Result<ContainerExitState> {
        let details = self
            .docker
            .inspect_container(container_id, None::<InspectContainerOptions>)
            .await
            .with_context(|| format!("failed to inspect container {container_id}"))?;
        let state = details
            .state
            .context("inspected container did not include state")?;

        Ok(ContainerExitState {
            status: state.status.map(|status| status.to_string()),
            exit_code: state.exit_code,
            oom_killed: state.oom_killed.unwrap_or(false),
            error: state.error.filter(|value| !value.trim().is_empty()),
        })
    }

    pub async fn metrics_for_container(&self, container_id: &str) -> Result<ContainerMetrics> {
        let snapshot = self.stats_snapshot_for_container(container_id).await?;

        Ok(ContainerMetrics {
            cpu_percent: snapshot.fallback_cpu_percent,
            memory_usage_bytes: snapshot.memory_usage_bytes,
            memory_limit_bytes: snapshot.memory_limit_bytes,
            network_rx_bytes: Some(snapshot.network_rx_bytes),
            network_tx_bytes: Some(snapshot.network_tx_bytes),
        })
    }

    pub async fn stats_snapshot_for_container(
        &self,
        container_id: &str,
    ) -> Result<ContainerStatsSnapshot> {
        let mut stats_stream = self.docker.stats(
            container_id,
            Some(StatsOptions {
                stream: false,
                one_shot: true,
            }),
        );

        let stats = stats_stream
            .next()
            .await
            .ok_or_else(|| anyhow!("docker stats stream ended before returning metrics"))?
            .with_context(|| format!("failed to collect metrics for container {container_id}"))?;

        Ok(snapshot_from_stats(&stats))
    }

    /// Inspect a container and return its bridge IPv4, if any. Shared by the
    /// readiness probe and the service-resolve path (which needs the IP to
    /// reach a replica that publishes no host port).
    pub async fn inspect_container_ip(&self, container_id: &str) -> Result<Option<String>> {
        let details = self
            .docker
            .inspect_container(container_id, None::<InspectContainerOptions>)
            .await
            .with_context(|| format!("failed to inspect container {container_id}"))?;
        Ok(details
            .network_settings
            .as_ref()
            .and_then(|settings| settings.networks.as_ref())
            .and_then(|networks| {
                networks
                    .values()
                    .find_map(|network| network.ip_address.as_deref())
            })
            .filter(|value| !value.is_empty())
            .map(str::to_string))
    }

    pub async fn probe_container_tcp(
        &self,
        container_id: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<()> {
        let ip_address = self
            .inspect_container_ip(container_id)
            .await?
            .context("container has no IPv4 address to probe")?;

        tokio::time::timeout(timeout, TcpStream::connect((ip_address.as_str(), port)))
            .await
            .context("tcp readiness probe timed out")?
            .with_context(|| format!("tcp readiness probe failed for {ip_address}:{port}"))?;
        Ok(())
    }

    async fn ensure_image(&self, image: &str) -> Result<()> {
        if self.docker.inspect_image(image).await.is_ok() {
            return Ok(());
        }

        let mut pull_stream = self.docker.create_image(
            Some(CreateImageOptions {
                from_image: image,
                ..Default::default()
            }),
            None,
            None,
        );

        while let Some(event) = pull_stream.next().await {
            event.with_context(|| format!("failed while pulling image `{image}`"))?;
        }

        Ok(())
    }

    async fn list_containers_with_filters(
        &self,
        labels: Vec<String>,
    ) -> Result<Vec<ManagedContainer>> {
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), labels);

        let containers = self
            .docker
            .list_containers(Some(ListContainersOptions::<String> {
                all: true,
                filters,
                ..Default::default()
            }))
            .await
            .context("failed to list Docker containers")?;

        let mut items = Vec::with_capacity(containers.len());
        for container in containers {
            let labels = container.labels.unwrap_or_default();
            let deployment = labels
                .get(LABEL_DEPLOYMENT)
                .cloned()
                .ok_or_else(|| anyhow!("managed container missing deployment label"))?;
            let name = container
                .names
                .unwrap_or_default()
                .first()
                .map(|value| value.trim_start_matches('/').to_string())
                .unwrap_or_else(|| deployment_container_name(&deployment));

            let ordinal = labels.get(LABEL_ORDINAL).and_then(|value| value.parse().ok());

            items.push(ManagedContainer {
                id: container.id.unwrap_or_default(),
                name,
                deployment: deployment.clone(),
                state: container.state.clone(),
                status: container.status.clone(),
                running: container.state.as_deref() == Some("running"),
                spec_hash: labels.get(LABEL_HASH).cloned(),
                ordinal,
                container_ip: None,
                created_at: container.created,
            });
        }

        items.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(items)
    }
}

pub(crate) fn build_container_config(
    manifest: &DeploymentManifest,
    spec_hash: &str,
    ordinal: u32,
    suppress_host_ports: bool,
) -> Result<Config<String>> {
    let container = manifest.container();
    let resource_limits = container.normalized_resource_limits()?;

    let exposed_ports = if container.ports.is_empty() {
        None
    } else {
        Some(
            container
                .ports
                .iter()
                .map(|port| (port.docker_port_key(), HashMap::new()))
                .collect(),
        )
    };

    // Replica mode publishes no host ports; the Service reaches each container
    // by its bridge IP instead.
    let port_bindings = if suppress_host_ports {
        HashMap::new()
    } else {
        build_port_bindings(&container.ports)
    };
    let binds = build_binds(container, manifest.volumes());
    let host_config = build_host_config(port_bindings, resource_limits, binds);

    let labels = HashMap::from([
        (LABEL_MANAGED.to_string(), "true".to_string()),
        (
            LABEL_DEPLOYMENT.to_string(),
            manifest.deployment_name().to_string(),
        ),
        (LABEL_HASH.to_string(), spec_hash.to_string()),
        (LABEL_ORDINAL.to_string(), ordinal.to_string()),
    ]);

    Ok(Config {
        image: Some(container.image.clone()),
        entrypoint: (!container.command.is_empty()).then_some(container.command.clone()),
        cmd: (!container.args.is_empty()).then_some(container.args.clone()),
        env: (!container.env.is_empty()).then_some(
            container
                .env
                .iter()
                .map(|entry| format!("{}={}", entry.name, entry.value))
                .collect(),
        ),
        exposed_ports,
        host_config,
        labels: Some(labels),
        ..Default::default()
    })
}

fn build_port_bindings(ports: &[ContainerPort]) -> HashMap<String, Option<Vec<PortBinding>>> {
    ports
        .iter()
        .filter_map(|port| {
            port.host_port.map(|host_port| {
                (
                    port.docker_port_key(),
                    Some(vec![PortBinding {
                        host_ip: Some("0.0.0.0".to_string()),
                        host_port: Some(host_port.to_string()),
                    }]),
                )
            })
        })
        .collect()
}

fn build_binds(container: &ContainerSpec, volumes: &[Volume]) -> Vec<String> {
    container
        .volume_mounts
        .iter()
        .filter_map(|mount| {
            let volume = volumes.iter().find(|volume| volume.name == mount.name)?;
            let mut bind = format!("{}:{}", volume.host_path, mount.mount_path);
            if mount.read_only {
                bind.push_str(":ro");
            }
            Some(bind)
        })
        .collect()
}

fn build_host_config(
    port_bindings: HashMap<String, Option<Vec<PortBinding>>>,
    resource_limits: Option<NormalizedResourceLimits>,
    binds: Vec<String>,
) -> Option<HostConfig> {
    let mut host_config = HostConfig::default();
    let mut configured = false;

    if !port_bindings.is_empty() {
        host_config.port_bindings = Some(port_bindings);
        configured = true;
    }

    if !binds.is_empty() {
        host_config.binds = Some(binds);
        configured = true;
    }

    if let Some(resource_limits) = resource_limits {
        if let Some(nano_cpus) = resource_limits.nano_cpus {
            host_config.nano_cpus = Some(nano_cpus);
            configured = true;
        }
        if let Some(memory_bytes) = resource_limits.memory_bytes {
            host_config.memory = Some(memory_bytes);
            configured = true;
        }
    }

    configured.then_some(host_config)
}

pub fn summarize_exit_state(exit_state: &ContainerExitState) -> Option<String> {
    if exit_state.oom_killed {
        return Some(match exit_state.exit_code {
            Some(code) => format!("oom({code})"),
            None => "oom".to_string(),
        });
    }

    if matches!(exit_state.status.as_deref(), Some("removing")) {
        return Some("removed".to_string());
    }

    if let Some(error) = exit_state.error.as_deref() {
        let compact = error.trim();
        if !compact.is_empty() {
            return Some(format!("error: {}", truncate_summary(compact, 28)));
        }
    }

    match (exit_state.status.as_deref(), exit_state.exit_code) {
        (Some(status), Some(code)) if !status.is_empty() => Some(format!("{status}({code})")),
        (Some(status), None) if !status.is_empty() => Some(status.to_string()),
        (None, Some(code)) => Some(format!("exit({code})")),
        _ => None,
    }
}

fn truncate_summary(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }

    value
        .chars()
        .take(max.saturating_sub(1))
        .collect::<String>()
        + "…"
}

fn select_primary_container(containers: Vec<ManagedContainer>) -> Option<ManagedContainer> {
    let mut first = None;
    for container in containers {
        if container.running {
            return Some(container);
        }
        if first.is_none() {
            first = Some(container);
        }
    }
    first
}

impl ContainerRuntime for DockerRuntime {
    async fn ping(&self) -> Result<()> {
        DockerRuntime::ping(self).await
    }

    async fn list_managed_containers(&self) -> Result<Vec<ManagedContainer>> {
        DockerRuntime::list_managed_containers(self).await
    }

    async fn list_containers_for_deployment(&self, name: &str) -> Result<Vec<ManagedContainer>> {
        DockerRuntime::list_containers_for_deployment(self, name).await
    }

    async fn create_and_start(
        &self,
        manifest: &DeploymentManifest,
        spec_hash: &str,
        ordinal: u32,
    ) -> Result<String> {
        DockerRuntime::create_and_start(self, manifest, spec_hash, ordinal).await
    }

    async fn remove_deployment(&self, name: &str) -> Result<()> {
        DockerRuntime::remove_deployment(self, name).await
    }

    async fn wait_until_deployment_cleared(&self, name: &str, timeout: Duration) -> Result<()> {
        DockerRuntime::wait_until_deployment_cleared(self, name, timeout).await
    }

    async fn remove_orphans(
        &self,
        desired_names: &HashSet<String>,
        known_deployments: &HashSet<String>,
    ) -> Result<()> {
        DockerRuntime::remove_orphans(self, desired_names, known_deployments).await
    }

    async fn remove_container(&self, container_id: &str) -> Result<()> {
        DockerRuntime::remove_container(self, container_id).await
    }

    async fn logs(&self, name: &str, tail: usize) -> Result<String> {
        DockerRuntime::logs(self, name, tail).await
    }

    async fn configured_limits_for_deployment(
        &self,
        name: &str,
    ) -> Result<ConfiguredResourceLimits> {
        DockerRuntime::configured_limits_for_deployment(self, name).await
    }

    async fn inspect_exit_state(&self, container_id: &str) -> Result<ContainerExitState> {
        DockerRuntime::inspect_exit_state(self, container_id).await
    }

    async fn metrics_for_container(&self, container_id: &str) -> Result<ContainerMetrics> {
        DockerRuntime::metrics_for_container(self, container_id).await
    }

    async fn stats_snapshot_for_container(
        &self,
        container_id: &str,
    ) -> Result<ContainerStatsSnapshot> {
        DockerRuntime::stats_snapshot_for_container(self, container_id).await
    }

    async fn probe_container_tcp(
        &self,
        container_id: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<()> {
        DockerRuntime::probe_container_tcp(self, container_id, port, timeout).await
    }

    async fn inspect_container_ip(&self, container_id: &str) -> Result<Option<String>> {
        DockerRuntime::inspect_container_ip(self, container_id).await
    }
}

fn connect_docker() -> Result<Docker> {
    if let Ok(host) = env::var("DOCKER_HOST") {
        return connect_from_host(&host);
    }

    for candidate in socket_candidates() {
        if candidate.exists() {
            return Docker::connect_with_unix(
                candidate.to_string_lossy().as_ref(),
                120,
                API_DEFAULT_VERSION,
            )
            .with_context(|| format!("failed to connect to Docker on {}", candidate.display()));
        }
    }

    bail!("failed to locate a usable Docker socket; set DOCKER_HOST or ensure Docker is running")
}

fn connect_from_host(host: &str) -> Result<Docker> {
    if let Some(path) = host.strip_prefix("unix://") {
        return Docker::connect_with_unix(path, 120, API_DEFAULT_VERSION)
            .with_context(|| format!("failed to connect to Docker on {host}"));
    }

    if host.starts_with("tcp://") || host.starts_with("http://") || host.starts_with("https://") {
        return Docker::connect_with_http(host, 120, API_DEFAULT_VERSION)
            .with_context(|| format!("failed to connect to Docker on {host}"));
    }

    bail!("unsupported DOCKER_HOST scheme in `{host}`")
}

fn socket_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        candidates.push(home.join(".orbstack/run/docker.sock"));
        candidates.push(home.join(".docker/run/docker.sock"));
    }
    candidates.push(Path::new("/var/run/docker.sock").to_path_buf());
    candidates
}

pub fn deployment_container_name(name: &str) -> String {
    let sanitized = name
        .chars()
        .map(|char| match char {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => char,
            _ => '-',
        })
        .collect::<String>();
    format!("litek8s-{sanitized}")
}

/// Container name for a specific replica ordinal, e.g. `litek8s-api-0`.
pub fn replica_container_name(name: &str, ordinal: u32) -> String {
    format!("{}-{ordinal}", deployment_container_name(name))
}

fn snapshot_from_stats(stats: &Stats) -> ContainerStatsSnapshot {
    ContainerStatsSnapshot {
        cpu_total_usage: stats.cpu_stats.cpu_usage.total_usage,
        system_cpu_usage: stats.cpu_stats.system_cpu_usage,
        online_cpus: online_cpus_for_stats(stats),
        fallback_cpu_percent: compute_cpu_percent_from_stats(stats),
        memory_usage_bytes: stats.memory_stats.usage,
        memory_limit_bytes: stats.memory_stats.limit,
        network_rx_bytes: total_network_rx_bytes(stats),
        network_tx_bytes: total_network_tx_bytes(stats),
    }
}

fn compute_cpu_percent_from_stats(stats: &Stats) -> Option<f64> {
    let cpu_delta = stats
        .cpu_stats
        .cpu_usage
        .total_usage
        .checked_sub(stats.precpu_stats.cpu_usage.total_usage)?;
    let system_cpu_delta = stats
        .cpu_stats
        .system_cpu_usage?
        .checked_sub(stats.precpu_stats.system_cpu_usage?)?;
    if cpu_delta == 0 || system_cpu_delta == 0 {
        return Some(0.0);
    }

    let online_cpus = online_cpus_for_stats(stats);

    Some((cpu_delta as f64 / system_cpu_delta as f64) * online_cpus as f64 * 100.0)
}

pub fn compute_cpu_percent_from_samples(
    previous: &ContainerStatsSnapshot,
    current: &ContainerStatsSnapshot,
) -> Option<f64> {
    let cpu_delta = current
        .cpu_total_usage
        .checked_sub(previous.cpu_total_usage)?;
    let system_cpu_delta = current
        .system_cpu_usage?
        .checked_sub(previous.system_cpu_usage?)?;
    if cpu_delta == 0 || system_cpu_delta == 0 {
        return Some(0.0);
    }

    Some((cpu_delta as f64 / system_cpu_delta as f64) * current.online_cpus as f64 * 100.0)
}

fn online_cpus_for_stats(stats: &Stats) -> u64 {
    stats
        .cpu_stats
        .online_cpus
        .or_else(|| {
            stats
                .cpu_stats
                .cpu_usage
                .percpu_usage
                .as_ref()
                .map(|usage| usage.len() as u64)
        })
        .unwrap_or(1)
}

fn total_network_rx_bytes(stats: &Stats) -> u64 {
    if let Some(networks) = &stats.networks {
        return networks.values().map(|network| network.rx_bytes).sum();
    }
    stats
        .network
        .map(|network| network.rx_bytes)
        .unwrap_or_default()
}

fn total_network_tx_bytes(stats: &Stats) -> u64 {
    if let Some(networks) = &stats.networks {
        return networks.values().map(|network| network.tx_bytes).sum();
    }
    stats
        .network
        .map(|network| network.tx_bytes)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::manifest::{API_VERSION, DeploymentManifest};

    use bollard::container::{
        BlkioStats, CPUStats, CPUUsage, MemoryStats, NetworkStats, PidsStats, Stats, StorageStats,
        ThrottlingData,
    };

    use super::{
        ContainerExitState, ContainerStatsSnapshot, LABEL_DEPLOYMENT, LABEL_HASH,
        build_container_config, compute_cpu_percent_from_samples, deployment_container_name,
        summarize_exit_state, total_network_rx_bytes, total_network_tx_bytes,
    };

    #[test]
    fn creates_expected_container_config() {
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
          command: ["nginx"]
          args: ["-g", "daemon off;"]
          env:
            - name: APP_ENV
              value: demo
          ports:
            - containerPort: 80
              hostPort: 8080
"#
        ))
        .expect("manifest should parse");

        let config =
            build_container_config(&manifest, "hash123", 0, false).expect("config should build");
        assert_eq!(config.image.as_deref(), Some("nginx:alpine"));
        assert_eq!(
            config.entrypoint.as_ref().expect("entrypoint missing"),
            &vec!["nginx".to_string()]
        );
        assert_eq!(
            config.cmd.as_ref().expect("cmd missing"),
            &vec!["-g".to_string(), "daemon off;".to_string()]
        );

        let labels = config.labels.expect("labels missing");
        assert_eq!(
            labels.get(LABEL_DEPLOYMENT).map(String::as_str),
            Some("demo")
        );
        assert_eq!(labels.get(LABEL_HASH).map(String::as_str), Some("hash123"));

        let bindings = config
            .host_config
            .expect("host config missing")
            .port_bindings
            .expect("port bindings missing");
        assert!(bindings.contains_key("80/tcp"));
    }

    #[test]
    fn maps_resource_limits_into_host_config() {
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
          ports:
            - containerPort: 80
              hostPort: 8080
          resources:
            limits:
              cpu: "500m"
              memory: "128Mi"
"#
        ))
        .expect("manifest should parse");

        let config =
            build_container_config(&manifest, "hash123", 0, false).expect("config should build");
        let host_config = config.host_config.expect("host config missing");
        assert_eq!(host_config.nano_cpus, Some(500_000_000));
        assert_eq!(host_config.memory, Some(134_217_728));
        let bindings = host_config.port_bindings.expect("port bindings missing");
        assert!(bindings.contains_key("80/tcp"));
    }

    #[test]
    fn maps_volume_mounts_into_host_config_binds() {
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
      volumes:
        - name: data
          hostPath: /host
      containers:
        - name: web
          image: nginx:alpine
          volumeMounts:
            - name: data
              mountPath: /container
"#
        ))
        .expect("manifest should parse");

        let config =
            build_container_config(&manifest, "hash123", 0, false).expect("config should build");
        let binds = config
            .host_config
            .expect("host config missing")
            .binds
            .expect("binds missing");
        assert!(binds.contains(&"/host:/container".to_string()));
    }

    #[test]
    fn appends_ro_suffix_for_read_only_volume_mounts() {
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
      volumes:
        - name: data
          hostPath: /host
      containers:
        - name: web
          image: nginx:alpine
          volumeMounts:
            - name: data
              mountPath: /container
              readOnly: true
"#
        ))
        .expect("manifest should parse");

        let config =
            build_container_config(&manifest, "hash123", 0, false).expect("config should build");
        let binds = config
            .host_config
            .expect("host config missing")
            .binds
            .expect("binds missing");
        assert!(binds.contains(&"/host:/container:ro".to_string()));
    }

    #[test]
    fn sanitizes_container_name() {
        assert_eq!(
            deployment_container_name("api/v1.demo"),
            "litek8s-api-v1.demo"
        );
    }

    #[test]
    fn aggregates_network_usage_from_all_interfaces() {
        let stats = Stats {
            read: String::new(),
            preread: String::new(),
            num_procs: 0,
            pids_stats: PidsStats {
                current: None,
                limit: None,
            },
            network: None,
            networks: Some(HashMap::from([
                (
                    "eth0".to_string(),
                    NetworkStats {
                        rx_dropped: 0,
                        rx_bytes: 15,
                        rx_errors: 0,
                        tx_packets: 0,
                        tx_dropped: 0,
                        rx_packets: 0,
                        tx_errors: 0,
                        tx_bytes: 25,
                    },
                ),
                (
                    "eth1".to_string(),
                    NetworkStats {
                        rx_dropped: 0,
                        rx_bytes: 10,
                        rx_errors: 0,
                        tx_packets: 0,
                        tx_dropped: 0,
                        rx_packets: 0,
                        tx_errors: 0,
                        tx_bytes: 20,
                    },
                ),
            ])),
            memory_stats: MemoryStats {
                stats: None,
                max_usage: None,
                usage: None,
                failcnt: None,
                limit: None,
                commit: None,
                commit_peak: None,
                commitbytes: None,
                commitpeakbytes: None,
                privateworkingset: None,
            },
            blkio_stats: BlkioStats {
                io_service_bytes_recursive: None,
                io_serviced_recursive: None,
                io_queue_recursive: None,
                io_service_time_recursive: None,
                io_wait_time_recursive: None,
                io_merged_recursive: None,
                io_time_recursive: None,
                sectors_recursive: None,
            },
            cpu_stats: CPUStats {
                cpu_usage: CPUUsage {
                    percpu_usage: None,
                    usage_in_usermode: 0,
                    total_usage: 0,
                    usage_in_kernelmode: 0,
                },
                system_cpu_usage: Some(0),
                online_cpus: Some(1),
                throttling_data: ThrottlingData {
                    periods: 0,
                    throttled_periods: 0,
                    throttled_time: 0,
                },
            },
            precpu_stats: CPUStats {
                cpu_usage: CPUUsage {
                    percpu_usage: None,
                    usage_in_usermode: 0,
                    total_usage: 0,
                    usage_in_kernelmode: 0,
                },
                system_cpu_usage: Some(0),
                online_cpus: Some(1),
                throttling_data: ThrottlingData {
                    periods: 0,
                    throttled_periods: 0,
                    throttled_time: 0,
                },
            },
            storage_stats: StorageStats {
                read_count_normalized: None,
                read_size_bytes: None,
                write_count_normalized: None,
                write_size_bytes: None,
            },
            name: String::new(),
            id: String::new(),
        };

        assert_eq!(total_network_rx_bytes(&stats), 25);
        assert_eq!(total_network_tx_bytes(&stats), 45);
    }

    #[test]
    fn summarizes_oom_exit_states() {
        let exit_state = ContainerExitState {
            status: Some("exited".to_string()),
            exit_code: Some(137),
            oom_killed: true,
            error: None,
        };

        assert_eq!(
            summarize_exit_state(&exit_state).as_deref(),
            Some("oom(137)")
        );
    }

    #[test]
    fn summarizes_regular_exit_states() {
        let exit_state = ContainerExitState {
            status: Some("exited".to_string()),
            exit_code: Some(1),
            oom_killed: false,
            error: None,
        };

        assert_eq!(
            summarize_exit_state(&exit_state).as_deref(),
            Some("exited(1)")
        );
    }

    #[test]
    fn summarizes_removing_as_removed() {
        let exit_state = ContainerExitState {
            status: Some("removing".to_string()),
            exit_code: Some(137),
            oom_killed: false,
            error: None,
        };

        assert_eq!(
            summarize_exit_state(&exit_state).as_deref(),
            Some("removed")
        );
    }

    #[test]
    fn computes_cpu_percent_from_cached_samples() {
        let previous = ContainerStatsSnapshot {
            cpu_total_usage: 2_000_000_000,
            system_cpu_usage: Some(20_000_000_000),
            online_cpus: 4,
            fallback_cpu_percent: None,
            memory_usage_bytes: Some(0),
            memory_limit_bytes: Some(0),
            network_rx_bytes: 0,
            network_tx_bytes: 0,
        };
        let current = ContainerStatsSnapshot {
            cpu_total_usage: 2_500_000_000,
            system_cpu_usage: Some(25_000_000_000),
            online_cpus: 4,
            fallback_cpu_percent: None,
            memory_usage_bytes: Some(0),
            memory_limit_bytes: Some(0),
            network_rx_bytes: 0,
            network_tx_bytes: 0,
        };

        let cpu = compute_cpu_percent_from_samples(&previous, &current).expect("cpu");
        assert!((cpu - 40.0).abs() < f64::EPSILON);
    }
}
