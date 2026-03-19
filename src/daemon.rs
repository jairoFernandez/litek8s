use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};

use crate::config;
use crate::manifest::DeploymentManifest;
use crate::node::{
    ClusterNode, DEFAULT_REMOTE_BINARY, NodeClient, NodeTransport, select_node_for_manifest,
};
use crate::protocol::{
    DaemonTopSummary, DeploymentDescription, DeploymentSummary, DeploymentTopRow, EventItem,
    NodeDescription, NodeSummary, Request, Response, read_message, send_message,
};
use crate::reconciler;
use crate::runtime::docker::{
    ContainerMetrics, ContainerStatsSnapshot, DockerRuntime, compute_cpu_percent_from_samples,
    deployment_container_name,
};
use crate::store::{ApplyOutcome, EventLevel, NewEvent, NodeApplyOutcome, Store};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub socket_path: PathBuf,
    pub state_file: PathBuf,
    pub poll_interval: Duration,
    pub cleanup_orphans: bool,
}

impl ServerConfig {
    pub fn from_defaults() -> Result<Self> {
        Ok(Self {
            socket_path: config::default_socket_path(),
            state_file: config::default_state_file()?,
            poll_interval: Duration::from_secs(2),
            cleanup_orphans: true,
        })
    }
}

#[derive(Clone)]
struct AppState {
    store: Arc<Store>,
    runtime: DockerRuntime,
    reconcile_guard: Arc<Mutex<()>>,
    cleanup_orphans: bool,
    daemon_metrics: Arc<Mutex<DaemonMetricsSampler>>,
    top_metrics: Arc<Mutex<TopMetricsSampler>>,
}

pub async fn serve(config: ServerConfig) -> Result<()> {
    config::ensure_parent_dir(&config.socket_path)?;
    config::ensure_parent_dir(&config.state_file)?;

    if config.socket_path.exists() {
        std::fs::remove_file(&config.socket_path).with_context(|| {
            format!(
                "failed to remove stale Unix socket {}",
                config.socket_path.display()
            )
        })?;
    }

    let store = Arc::new(Store::load(config.state_file.clone()).await?);
    let runtime = DockerRuntime::connect()?;
    runtime.ping().await?;

    let app_state = AppState {
        store,
        runtime,
        reconcile_guard: Arc::new(Mutex::new(())),
        cleanup_orphans: config.cleanup_orphans,
        daemon_metrics: Arc::new(Mutex::new(DaemonMetricsSampler::new())),
        top_metrics: Arc::new(Mutex::new(TopMetricsSampler::new())),
    };
    let app_state = Arc::new(app_state);
    app_state.reconcile_all().await?;

    let listener = UnixListener::bind(&config.socket_path).with_context(|| {
        format!(
            "failed to bind daemon Unix socket {}",
            config.socket_path.display()
        )
    })?;

    let notify = Arc::new(Notify::new());
    let reconcile_task =
        spawn_reconcile_loop(app_state.clone(), notify.clone(), config.poll_interval);
    let event_listener = app_state.runtime.spawn_event_listener(notify);

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (stream, _) = accept_result.context("failed to accept daemon connection")?;
                let state = app_state.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(state, stream).await {
                        eprintln!("failed to serve client request: {error}");
                    }
                });
            }
            signal_result = tokio::signal::ctrl_c() => {
                signal_result.context("failed to listen for ctrl-c")?;
                break;
            }
        }
    }

    reconcile_task.abort();
    event_listener.abort();
    if config.socket_path.exists() {
        std::fs::remove_file(&config.socket_path)
            .with_context(|| format!("failed to remove socket {}", config.socket_path.display()))?;
    }
    Ok(())
}

async fn handle_connection(state: Arc<AppState>, mut stream: UnixStream) -> Result<()> {
    let request = read_message::<Request>(&mut stream).await?;
    let response = match process_request(state, request).await {
        Ok(response) => response,
        Err(error) => Response::Error {
            message: format!("{error:#}"),
        },
    };
    send_message(&mut stream, &response).await
}

async fn process_request(state: Arc<AppState>, request: Request) -> Result<Response> {
    match request {
        Request::Apply { manifest } => {
            let manifest = DeploymentManifest::parse(&manifest)?;
            let name = manifest.deployment_name().to_string();
            let current_assignment = state
                .store
                .get(&name)
                .await
                .map(|entry| entry.assigned_node);
            let assigned_node = select_node_for_manifest(
                &manifest,
                &state.store.list_nodes().await,
                current_assignment.as_deref(),
            )?;
            let outcome = state.store.upsert(manifest).await?;
            let _ = state
                .store
                .set_assigned_node(&name, assigned_node.clone())
                .await?;
            state
                .record_event(NewEvent {
                    level: EventLevel::Normal,
                    reason: match outcome {
                        ApplyOutcome::Created => "Applied".to_string(),
                        ApplyOutcome::Configured => "Configured".to_string(),
                    },
                    object_kind: "deployment".to_string(),
                    object_name: name.clone(),
                    node_name: Some(assigned_node),
                    message: "deployment manifest accepted by the control plane".to_string(),
                })
                .await?;
            state.reconcile_all().await?;

            let message = match outcome {
                ApplyOutcome::Created => format!("deployment.apps/{name} created"),
                ApplyOutcome::Configured => format!("deployment.apps/{name} configured"),
            };
            Ok(Response::Ack { message })
        }
        Request::AddNode {
            name,
            ssh_target,
            remote_binary,
            labels,
        } => {
            if matches!(name.as_str(), "local") {
                bail!("`local` is reserved for the built-in node");
            }
            let node = ClusterNode::ssh(
                name.clone(),
                ssh_target,
                remote_binary.unwrap_or_else(|| DEFAULT_REMOTE_BINARY.to_string()),
                labels,
            )?;
            let client = NodeClient::for_node(&node, state.runtime.clone());
            client
                .ping()
                .await
                .with_context(|| format!("node `{name}` did not answer over ssh"))?;

            let outcome = state.store.upsert_node(node).await?;
            state
                .record_event(NewEvent {
                    level: EventLevel::Normal,
                    reason: match outcome {
                        NodeApplyOutcome::Created => "NodeAdded".to_string(),
                        NodeApplyOutcome::Configured => "NodeUpdated".to_string(),
                    },
                    object_kind: "node".to_string(),
                    object_name: name.clone(),
                    node_name: Some(name.clone()),
                    message: "node registered and answered over ssh".to_string(),
                })
                .await?;
            let message = match outcome {
                NodeApplyOutcome::Created => format!("node/{name} added"),
                NodeApplyOutcome::Configured => format!("node/{name} updated"),
            };
            Ok(Response::Ack { message })
        }
        Request::DeleteManifest { manifest } => {
            let manifest = DeploymentManifest::parse(&manifest)?;
            delete_deployment(state, manifest.deployment_name()).await
        }
        Request::DeleteDeployment { name } => delete_deployment(state, &name).await,
        Request::GetNodes => {
            let nodes = state.store.list_nodes().await;
            let deployments = state.store.list().await;
            let mut items = Vec::with_capacity(nodes.len());
            for node in nodes {
                let deployment_count = deployments
                    .iter()
                    .filter(|entry| entry.assigned_node == node.name)
                    .count();
                let (status, _) = state.node_health(&node).await;
                items.push(NodeSummary {
                    name: node.name.clone(),
                    transport: node.transport_label().to_string(),
                    endpoint: node.endpoint(),
                    status,
                    deployment_count,
                });
            }
            Ok(Response::Nodes { items })
        }
        Request::GetEvents { limit, since_id } => {
            let items = state
                .store
                .list_events(limit, since_id)
                .await
                .into_iter()
                .map(|event| EventItem {
                    id: event.id,
                    timestamp_secs: event.timestamp_secs,
                    level: event.level.to_string(),
                    reason: event.reason,
                    object_kind: event.object_kind,
                    object_name: event.object_name,
                    node_name: event.node_name,
                    message: event.message,
                })
                .collect();
            Ok(Response::Events { items })
        }
        Request::GetDeployments => {
            state.reconcile_all().await?;
            let items = state
                .store
                .list()
                .await
                .into_iter()
                .map(|entry| DeploymentSummary {
                    name: entry.manifest.deployment_name().to_string(),
                    node_name: entry.assigned_node,
                    status: entry.status.to_string(),
                    image: entry.manifest.container().image.clone(),
                    container_id: entry.container_id,
                })
                .collect();
            Ok(Response::Deployments { items })
        }
        Request::DescribeDeployment { name } => {
            state.reconcile_all().await?;
            let entry = state
                .store
                .get(&name)
                .await
                .with_context(|| format!("deployment `{name}` was not found"))?;

            let item = DeploymentDescription {
                name: entry.manifest.deployment_name().to_string(),
                api_version: entry.manifest.api_version.clone(),
                node_name: entry.assigned_node,
                requested_node_name: entry.manifest.explicit_node_name().map(str::to_string),
                node_selector: entry
                    .manifest
                    .node_selector()
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect(),
                status: entry.status.to_string(),
                image: entry.manifest.container().image.clone(),
                container_name: deployment_container_name(entry.manifest.deployment_name()),
                container_id: entry.container_id,
                message: entry.message,
                command: entry.manifest.container().command.clone(),
                args: entry.manifest.container().args.clone(),
                ports: entry
                    .manifest
                    .container()
                    .ports
                    .iter()
                    .map(|port| port.printable())
                    .collect(),
                cpu_limit: entry.manifest.container().cpu_limit().map(str::to_string),
                memory_limit: entry
                    .manifest
                    .container()
                    .memory_limit()
                    .map(str::to_string),
                restart_count: entry.restart_count,
                restart_streak: entry.restart_streak,
                last_exit: entry.last_exit,
                next_retry_in_secs: retry_in_secs(entry.next_retry_at),
            };

            Ok(Response::Description { item })
        }
        Request::DescribeNode { name } => {
            let node = state
                .store
                .get_node(&name)
                .await
                .with_context(|| format!("node `{name}` was not found"))?;
            let (status, message) = state.node_health(&node).await;
            let deployment_count = state
                .store
                .list()
                .await
                .into_iter()
                .filter(|entry| entry.assigned_node == node.name)
                .count();
            let item = NodeDescription {
                name: node.name.clone(),
                transport: node.transport_label().to_string(),
                endpoint: node.endpoint(),
                remote_binary: match &node.transport {
                    NodeTransport::Local => None,
                    NodeTransport::Ssh { remote_binary, .. } => Some(remote_binary.clone()),
                },
                labels: node.printable_labels(),
                status,
                deployment_count,
                message,
            };
            Ok(Response::NodeDescription { item })
        }
        Request::Logs { name, tail } => {
            let entry = state
                .store
                .get(&name)
                .await
                .with_context(|| format!("deployment `{name}` was not found"))?;
            let runtime = state.client_for_node_name(&entry.assigned_node).await?;
            let content = runtime.logs(&name, tail).await?;
            Ok(Response::Logs { content })
        }
        Request::Top => {
            // Keep `top` observational only: the background reconcile loop and Docker event
            // listener remain responsible for state convergence.
            let deployments = state.store.list().await;
            let mut items = Vec::with_capacity(deployments.len());
            let mut active_container_ids = HashSet::new();
            let mut snapshots = HashMap::new();

            for entry in deployments {
                if let Some(container_id) = entry.container_id.as_ref() {
                    let sample_key = top_sample_key(&entry.assigned_node, container_id);
                    active_container_ids.insert(sample_key.clone());
                    if let Ok(runtime) = state.client_for_node_name(&entry.assigned_node).await {
                        if let Ok(snapshot) =
                            runtime.stats_snapshot_for_container(container_id).await
                        {
                            snapshots.insert(sample_key, snapshot);
                        }
                    }
                }

                items.push(DeploymentTopRow {
                    name: entry.manifest.deployment_name().to_string(),
                    node_name: entry.assigned_node,
                    status: entry.status.to_string(),
                    image: entry.manifest.container().image.clone(),
                    container_id: entry.container_id,
                    restart_count: entry.restart_count,
                    restart_streak: entry.restart_streak,
                    last_exit: entry.last_exit,
                    next_retry_in_secs: retry_in_secs(entry.next_retry_at),
                    cpu_percent: None,
                    memory_usage_bytes: None,
                    memory_limit_bytes: None,
                    network_rx_bytes: None,
                    network_tx_bytes: None,
                });
            }

            let mut top_metrics = state.top_metrics.lock().await;
            top_metrics.retain_active(&active_container_ids);
            for item in &mut items {
                if let Some(container_id) = item.container_id.as_ref() {
                    let sample_key = top_sample_key(&item.node_name, container_id);
                    if let Some(snapshot) = snapshots.get(&sample_key) {
                        let metrics = top_metrics.metrics_for(&sample_key, snapshot);
                        item.cpu_percent = metrics.cpu_percent;
                        item.memory_usage_bytes = metrics.memory_usage_bytes;
                        item.memory_limit_bytes = metrics.memory_limit_bytes;
                        item.network_rx_bytes = metrics.network_rx_bytes;
                        item.network_tx_bytes = metrics.network_tx_bytes;
                    }
                }
            }

            let daemon = state.daemon_metrics.lock().await.snapshot();

            Ok(Response::Top { daemon, items })
        }
    }
}

async fn delete_deployment(state: Arc<AppState>, name: &str) -> Result<Response> {
    let deployment = state
        .store
        .get(name)
        .await
        .with_context(|| format!("deployment `{name}` was not found"))?;
    let removed = state.store.remove(name).await?;
    if removed.is_none() {
        bail!("deployment `{name}` was not found");
    }
    state
        .client_for_node_name(&deployment.assigned_node)
        .await?
        .remove_deployment(name)
        .await?;
    state
        .record_event(NewEvent {
            level: EventLevel::Normal,
            reason: "Deleted".to_string(),
            object_kind: "deployment".to_string(),
            object_name: name.to_string(),
            node_name: Some(deployment.assigned_node),
            message: "deployment removed from the control plane".to_string(),
        })
        .await?;
    Ok(Response::Ack {
        message: format!("deployment.apps/{name} deleted"),
    })
}

fn spawn_reconcile_loop(
    state: Arc<AppState>,
    notify: Arc<Notify>,
    poll_interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(poll_interval);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = notify.notified() => {}
            }

            if let Err(error) = state.reconcile_all().await {
                eprintln!("reconciliation failed: {error}");
            }
        }
    })
}

impl AppState {
    async fn reconcile_all(&self) -> Result<()> {
        let _guard = self.reconcile_guard.lock().await;
        let nodes = self.store.list_nodes().await;
        reconciler::reconcile_all(&self.store, &self.runtime, &nodes, self.cleanup_orphans).await
    }

    async fn client_for_node_name(&self, name: &str) -> Result<NodeClient> {
        let node = self
            .store
            .get_node(name)
            .await
            .with_context(|| format!("node `{name}` was not found"))?;
        Ok(NodeClient::for_node(&node, self.runtime.clone()))
    }

    async fn node_health(&self, node: &ClusterNode) -> (String, Option<String>) {
        let client = NodeClient::for_node(node, self.runtime.clone());
        match client.ping().await {
            Ok(()) => ("Ready".to_string(), None),
            Err(error) => ("Offline".to_string(), Some(error.to_string())),
        }
    }

    async fn record_event(&self, event: NewEvent) -> Result<()> {
        self.store.record_event(event).await?;
        Ok(())
    }
}

fn retry_in_secs(next_retry_at: Option<u64>) -> Option<u64> {
    let next_retry_at = next_retry_at?;
    Some(next_retry_at.saturating_sub(now_unix()))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn top_sample_key(node_name: &str, container_id: &str) -> String {
    format!("{node_name}:{container_id}")
}

struct DaemonMetricsSampler {
    pid: Pid,
    started_at: Instant,
    system: System,
}

impl DaemonMetricsSampler {
    fn new() -> Self {
        let pid = Pid::from_u32(std::process::id());
        let mut system = System::new_with_specifics(
            RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing()),
        );
        system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);

        Self {
            pid,
            started_at: Instant::now(),
            system,
        }
    }

    fn snapshot(&mut self) -> DaemonTopSummary {
        self.system
            .refresh_processes(ProcessesToUpdate::Some(&[self.pid]), true);

        let process = self.system.process(self.pid);
        DaemonTopSummary {
            pid: self.pid.as_u32(),
            cpu_percent: process.map(|process| process.cpu_usage() as f64),
            memory_bytes: process.map(|process| process.memory()).unwrap_or_default(),
            uptime_secs: self.started_at.elapsed().as_secs(),
        }
    }
}

#[derive(Default)]
struct TopMetricsSampler {
    previous_by_container: HashMap<String, ContainerStatsSnapshot>,
}

impl TopMetricsSampler {
    fn new() -> Self {
        Self::default()
    }

    fn retain_active(&mut self, active_ids: &HashSet<String>) {
        self.previous_by_container
            .retain(|container_id, _| active_ids.contains(container_id));
    }

    fn metrics_for(
        &mut self,
        container_id: &str,
        snapshot: &ContainerStatsSnapshot,
    ) -> ContainerMetrics {
        let cpu_percent = self
            .previous_by_container
            .get(container_id)
            .and_then(|previous| compute_cpu_percent_from_samples(previous, snapshot))
            .or(snapshot.fallback_cpu_percent);

        self.previous_by_container
            .insert(container_id.to_string(), snapshot.clone());

        ContainerMetrics {
            cpu_percent,
            memory_usage_bytes: snapshot.memory_usage_bytes,
            memory_limit_bytes: snapshot.memory_limit_bytes,
            network_rx_bytes: Some(snapshot.network_rx_bytes),
            network_tx_bytes: Some(snapshot.network_tx_bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::TopMetricsSampler;
    use crate::runtime::docker::ContainerStatsSnapshot;

    fn snapshot(cpu_total_usage: u64, system_cpu_usage: u64) -> ContainerStatsSnapshot {
        ContainerStatsSnapshot {
            cpu_total_usage,
            system_cpu_usage: Some(system_cpu_usage),
            online_cpus: 4,
            fallback_cpu_percent: None,
            memory_usage_bytes: Some(12),
            memory_limit_bytes: Some(64),
            network_rx_bytes: 20,
            network_tx_bytes: 10,
        }
    }

    #[test]
    fn top_metrics_sampler_uses_cached_samples_for_cpu() {
        let mut sampler = TopMetricsSampler::new();

        let first = sampler.metrics_for("abc", &snapshot(2_000_000_000, 20_000_000_000));
        assert!(first.cpu_percent.is_none());

        let second = sampler.metrics_for("abc", &snapshot(2_500_000_000, 25_000_000_000));
        let cpu = second.cpu_percent.expect("cpu should be available");
        assert!((cpu - 40.0).abs() < f64::EPSILON);
    }

    #[test]
    fn top_metrics_sampler_drops_inactive_containers() {
        let mut sampler = TopMetricsSampler::new();
        let _ = sampler.metrics_for("abc", &snapshot(2_000_000_000, 20_000_000_000));
        sampler.retain_active(&HashSet::new());

        let next = sampler.metrics_for("abc", &snapshot(2_500_000_000, 25_000_000_000));
        assert!(next.cpu_percent.is_none());
    }
}
