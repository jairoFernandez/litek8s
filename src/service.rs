use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::node::NodeTransport;
use crate::protocol::ProxyTarget;
use crate::proxy;
use crate::store::{Store, StoredDeployment, StoredService};

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

#[derive(Default)]
pub struct ServiceManager {
    running: HashMap<String, RunningService>,
}

impl ServiceManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn sync(&mut self, store: Arc<Store>) -> Result<()> {
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
            let task = spawn_service_task(service.clone(), store.clone(), status.clone());
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

        loop {
            match listener.accept().await {
                Ok((socket, _)) => {
                    let store = store.clone();
                    let service = service.clone();
                    let status = status.clone();
                    tokio::spawn(async move {
                        match resolve_service_target(&store, &service).await {
                            Ok(target) => {
                                *status.write().await = ServiceStatusSnapshot {
                                    status: "Listening".to_string(),
                                    message: None,
                                };
                                if let Err(error) = proxy::forward_connection(socket, target).await
                                {
                                    eprintln!(
                                        "service `{}` connection failed: {error}",
                                        service.name
                                    );
                                }
                            }
                            Err(error) => {
                                *status.write().await = ServiceStatusSnapshot {
                                    status: "Error".to_string(),
                                    message: Some(error.to_string()),
                                };
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

pub async fn resolve_service_target(store: &Store, service: &StoredService) -> Result<ProxyTarget> {
    let deployment = resolve_backing_deployment(store, service).await?;
    let selected_port = deployment.manifest.proxy_port(service.target_port)?;
    let host_port = selected_port
        .host_port
        .context("selected service target is missing hostPort")?;
    let node = store
        .get_node(&deployment.assigned_node)
        .await
        .with_context(|| format!("node `{}` was not found", deployment.assigned_node))?;
    let (transport, ssh_target) = match node.transport {
        NodeTransport::Local => ("local".to_string(), None),
        NodeTransport::Ssh { target, .. } => ("ssh".to_string(), Some(target)),
    };

    Ok(ProxyTarget {
        deployment_name: deployment.manifest.deployment_name().to_string(),
        node_name: deployment.assigned_node,
        transport,
        ssh_target,
        container_port: selected_port.container_port,
        host_port,
        protocol: selected_port.protocol.as_printable().to_string(),
    })
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
