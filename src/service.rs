use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::node::{ClusterNode, NodeClient, NodeTransport};
use crate::protocol::ProxyTarget;
use crate::proxy;
use crate::runtime::docker::DockerRuntime;
use crate::store::{DeploymentStatus, ReplicaState, Store, StoredDeployment, StoredService};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatusSnapshot {
    pub status: String,
    pub message: Option<String>,
}

struct RunningService {
    spec_key: String,
    status: Arc<RwLock<ServiceStatusSnapshot>>,
    task: JoinHandle<()>,
}

/// Round-robin cursor over a service's backend replicas. Re-resolved per
/// connection, so it just indexes into whatever the current backend set is.
type RoundRobin = Arc<AtomicUsize>;

#[derive(Default)]
pub struct ServiceManager {
    running: HashMap<String, RunningService>,
}

impl ServiceManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn sync(&mut self, store: Arc<Store>, runtime: DockerRuntime) -> Result<()> {
        let services = store.list_services().await;
        let desired_names: HashSet<String> = services
            .iter()
            .map(|service| service.name.clone())
            .collect();

        let existing_names: Vec<String> = self.running.keys().cloned().collect();
        for name in existing_names {
            if !desired_names.contains(&name) {
                if let Some(running) = self.running.remove(&name) {
                    running.task.abort();
                }
            }
        }

        for service in services {
            let spec_key = service_spec_key(&service);
            let needs_restart = self
                .running
                .get(&service.name)
                .map(|running| running.spec_key != spec_key)
                .unwrap_or(true);
            if !needs_restart {
                continue;
            }

            if let Some(existing) = self.running.remove(&service.name) {
                existing.task.abort();
            }

            let status = Arc::new(RwLock::new(ServiceStatusSnapshot {
                status: "Starting".to_string(),
                message: Some("binding service listener".to_string()),
            }));
            let task = spawn_service_task(
                service.clone(),
                store.clone(),
                runtime.clone(),
                status.clone(),
            );
            self.running.insert(
                service.name.clone(),
                RunningService {
                    spec_key,
                    status,
                    task,
                },
            );
        }

        Ok(())
    }

    pub async fn status_for(&self, name: &str) -> Option<ServiceStatusSnapshot> {
        let running = self.running.get(name)?;
        Some(running.status.read().await.clone())
    }

    pub async fn statuses(&self) -> HashMap<String, ServiceStatusSnapshot> {
        let mut items = HashMap::new();
        for (name, running) in &self.running {
            items.insert(name.clone(), running.status.read().await.clone());
        }
        items
    }
}

fn spawn_service_task(
    service: StoredService,
    store: Arc<Store>,
    runtime: DockerRuntime,
    status: Arc<RwLock<ServiceStatusSnapshot>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let listener = match TcpListener::bind(&service.listen_addr).await {
            Ok(listener) => {
                *status.write().await = ServiceStatusSnapshot {
                    status: "Listening".to_string(),
                    message: None,
                };
                listener
            }
            Err(error) => {
                *status.write().await = ServiceStatusSnapshot {
                    status: "Error".to_string(),
                    message: Some(format!("failed to bind {}: {error}", service.listen_addr)),
                };
                return;
            }
        };

        let round_robin: RoundRobin = Arc::new(AtomicUsize::new(0));

        loop {
            match listener.accept().await {
                Ok((socket, _)) => {
                    let store = store.clone();
                    let runtime = runtime.clone();
                    let service = service.clone();
                    let round_robin = round_robin.clone();
                    tokio::spawn(async move {
                        match resolve_service_targets(&store, &runtime, &service).await {
                            Ok(targets) if !targets.is_empty() => {
                                // Round-robin across the ready replicas. Re-resolved
                                // per connection, so a changing backend set is fine.
                                let idx =
                                    round_robin.fetch_add(1, Ordering::Relaxed) % targets.len();
                                let target = targets[idx].clone();
                                if let Err(error) = proxy::forward_connection(socket, target).await
                                {
                                    eprintln!(
                                        "service `{}` connection failed: {error}",
                                        service.name
                                    );
                                }
                            }
                            Ok(_) => {
                                eprintln!(
                                    "service `{}` has no ready backend for this connection",
                                    service.name
                                );
                            }
                            Err(error) => {
                                eprintln!(
                                    "service `{}` has no routable backend for this connection: {error}",
                                    service.name
                                );
                            }
                        }
                    });
                }
                Err(error) => {
                    *status.write().await = ServiceStatusSnapshot {
                        status: "Error".to_string(),
                        message: Some(format!("listener accept failed: {error}")),
                    };
                    return;
                }
            }
        }
    })
}

/// Resolve all ready replica backends for a service into proxy targets.
/// Single-replica deployments (publishing a host port) keep the legacy
/// host-port path; multi-replica deployments resolve each ready replica's
/// container IP and are load-balanced across.
pub async fn resolve_service_targets(
    store: &Store,
    runtime: &DockerRuntime,
    service: &StoredService,
) -> Result<Vec<ProxyTarget>> {
    let deployment = resolve_backing_deployment(store, service).await?;
    let has_probe = deployment.manifest.container().readiness_probe().is_some();
    let selected_port = deployment.manifest.proxy_port(service.target_port)?;
    let container_port = selected_port.container_port;
    let protocol = selected_port.protocol.as_printable().to_string();
    let deployment_name = deployment.manifest.deployment_name().to_string();

    let mut targets = Vec::new();
    for replica in &deployment.replicas {
        if !replica_ready(replica, has_probe) {
            continue;
        }
        let Some(container_id) = replica.container_id.as_deref() else {
            continue;
        };
        let Some(node) = store.get_node(&replica.assigned_node).await else {
            continue;
        };
        let (transport, ssh_target) = transport_of(&node);
        // Reach the replica by its container IP (replicas publish no host port).
        let client = NodeClient::for_node(&node, runtime.clone());
        let container_ip = client.container_ip(container_id).await.ok().flatten();
        targets.push(ProxyTarget {
            deployment_name: deployment_name.clone(),
            node_name: replica.assigned_node.clone(),
            transport,
            ssh_target,
            container_port,
            host_port: selected_port.host_port,
            container_ip,
            protocol: protocol.clone(),
        });
    }

    if targets.is_empty() {
        bail!("backend deployment `{deployment_name}` has no ready replica");
    }
    Ok(targets)
}

fn replica_ready(replica: &ReplicaState, has_probe: bool) -> bool {
    if has_probe {
        replica.ready
    } else {
        replica.status == DeploymentStatus::Running && replica.container_id.is_some()
    }
}

fn transport_of(node: &ClusterNode) -> (String, Option<String>) {
    match &node.transport {
        NodeTransport::Local => ("local".to_string(), None),
        NodeTransport::Ssh { target, .. } => ("ssh".to_string(), Some(target.clone())),
    }
}

pub async fn resolve_backing_deployment(
    store: &Store,
    service: &StoredService,
) -> Result<StoredDeployment> {
    if !service.deployment_name.is_empty() {
        return store
            .get(&service.deployment_name)
            .await
            .with_context(|| format!("deployment `{}` was not found", service.deployment_name));
    }

    let matches: Vec<StoredDeployment> = store
        .list()
        .await
        .into_iter()
        .filter(|deployment| deployment.manifest.matches_labels(&service.selector))
        .collect();

    match matches.as_slice() {
        [] => Err(anyhow::anyhow!(
            "no deployment matches selector `{}`",
            format_selector(&service.selector)
        )),
        [deployment] => Ok(deployment.clone()),
        _ => Err(anyhow::anyhow!(
            "selector `{}` matches multiple deployments; service load-balancing is not implemented in v1",
            format_selector(&service.selector)
        )),
    }
}

fn service_spec_key(service: &StoredService) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        service.name,
        service.deployment_name,
        format_selector(&service.selector),
        service.listen_addr,
        service
            .target_port
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string())
    )
}

fn format_selector(selector: &BTreeMap<String, String>) -> String {
    selector
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}
