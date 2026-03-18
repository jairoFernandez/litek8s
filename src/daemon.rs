use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};

use crate::config;
use crate::manifest::DeploymentManifest;
use crate::protocol::{
    DaemonTopSummary, DeploymentDescription, DeploymentSummary, DeploymentTopRow, Request,
    Response, read_message, send_message,
};
use crate::reconciler;
use crate::runtime::docker::{DockerRuntime, deployment_container_name};
use crate::store::{ApplyOutcome, Store};

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
            message: error.to_string(),
        },
    };
    send_message(&mut stream, &response).await
}

async fn process_request(state: Arc<AppState>, request: Request) -> Result<Response> {
    match request {
        Request::Apply { manifest } => {
            let manifest = DeploymentManifest::parse(&manifest)?;
            let name = manifest.deployment_name().to_string();
            let outcome = state.store.upsert(manifest).await?;
            state.reconcile_all().await?;

            let message = match outcome {
                ApplyOutcome::Created => format!("deployment.apps/{name} created"),
                ApplyOutcome::Configured => format!("deployment.apps/{name} configured"),
            };
            Ok(Response::Ack { message })
        }
        Request::DeleteManifest { manifest } => {
            let manifest = DeploymentManifest::parse(&manifest)?;
            delete_deployment(state, manifest.deployment_name()).await
        }
        Request::DeleteDeployment { name } => delete_deployment(state, &name).await,
        Request::GetDeployments => {
            state.reconcile_all().await?;
            let items = state
                .store
                .list()
                .await
                .into_iter()
                .map(|entry| DeploymentSummary {
                    name: entry.manifest.deployment_name().to_string(),
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
            };

            Ok(Response::Description { item })
        }
        Request::Logs { name, tail } => {
            let content = state.runtime.logs(&name, tail).await?;
            Ok(Response::Logs { content })
        }
        Request::Top => {
            state.reconcile_all().await?;
            let deployments = state.store.list().await;
            let mut items = Vec::with_capacity(deployments.len());

            for entry in deployments {
                let metrics = match &entry.container_id {
                    Some(container_id) => {
                        state.runtime.metrics_for_container(container_id).await.ok()
                    }
                    None => None,
                };

                items.push(DeploymentTopRow {
                    name: entry.manifest.deployment_name().to_string(),
                    status: entry.status.to_string(),
                    image: entry.manifest.container().image.clone(),
                    container_id: entry.container_id,
                    cpu_percent: metrics.as_ref().and_then(|value| value.cpu_percent),
                    memory_usage_bytes: metrics.as_ref().and_then(|value| value.memory_usage_bytes),
                    memory_limit_bytes: metrics.as_ref().and_then(|value| value.memory_limit_bytes),
                    network_rx_bytes: metrics.as_ref().and_then(|value| value.network_rx_bytes),
                    network_tx_bytes: metrics.as_ref().and_then(|value| value.network_tx_bytes),
                });
            }

            let daemon = state.daemon_metrics.lock().await.snapshot();

            Ok(Response::Top { daemon, items })
        }
    }
}

async fn delete_deployment(state: Arc<AppState>, name: &str) -> Result<Response> {
    let removed = state.store.remove(name).await?;
    if removed.is_none() {
        bail!("deployment `{name}` was not found");
    }
    state.runtime.remove_deployment(name).await?;
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
        reconciler::reconcile_all(&self.store, &self.runtime, self.cleanup_orphans).await
    }
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
