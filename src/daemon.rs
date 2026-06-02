use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};

use crate::config;
use crate::control_plane::raft::ControlPlaneNode;
use crate::control_plane::{ClusterCommand, CommandResult};
use crate::manifest::{
    API_VERSION, DeploymentManifest, KIND_SERVICE, Metadata, ParsedManifest, ServiceManifest,
    ServiceSpec,
};
use crate::node::{
    ClusterNode, DEFAULT_REMOTE_BINARY, NodeClient, NodeTransport, select_node_for_manifest,
};
use crate::protocol::{
    DaemonTopSummary, DeploymentCondition, DeploymentDescription, DeploymentSummary,
    DeploymentTopRow, EventItem, NodeDescription, NodeSummary, ProxyTarget as ProxyTargetInfo,
    Request, Response, ServiceDescription, ServiceSummary, read_message, send_message,
};
use crate::reconciler;
use crate::runtime::docker::{
    ContainerMetrics, ContainerStatsSnapshot, DockerRuntime, compute_cpu_percent_from_samples,
    deployment_container_name,
};
use crate::service::{ServiceManager, ServiceStatusSnapshot, resolve_backing_deployment};
use crate::store::{DeploymentStatus, EventLevel, EventQuery, NewEvent, Store};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub socket_path: PathBuf,
    pub state_file: PathBuf,
    pub poll_interval: Duration,
    pub cleanup_orphans: bool,
    pub peer_id: u64,
    pub peer_addr: String,
    pub join_addr: Option<String>,
    pub api_addr: Option<String>,
    /// How long per-node host resource samples are cached before re-querying.
    pub node_resource_interval: Duration,
}

const DEFAULT_API_PORT: u16 = 6443;

fn resolve_api_bind_addr_with(
    api_addr: Option<&str>,
    membership: Option<&crate::vpn::VpnMembership>,
) -> Option<String> {
    if let Some(addr) = api_addr {
        return Some(addr.to_string());
    }
    membership.map(|m| format!("{}:{}", m.ip, DEFAULT_API_PORT))
}

fn resolve_api_bind_addr(config: &ServerConfig) -> Option<String> {
    let membership = match crate::vpn::current_local_membership() {
        Ok(value) => value,
        Err(error) => {
            eprintln!("failed to read VPN membership for API bind: {error}");
            None
        }
    };
    resolve_api_bind_addr_with(config.api_addr.as_deref(), membership.as_ref())
}

impl ServerConfig {
    pub fn from_defaults() -> Result<Self> {
        Ok(Self {
            socket_path: config::default_socket_path(),
            state_file: config::default_state_file()?,
            poll_interval: Duration::from_secs(2),
            cleanup_orphans: true,
            peer_id: 1,
            peer_addr: "0.0.0.0:4001".into(),
            join_addr: None,
            api_addr: None,
            node_resource_interval: Duration::from_secs(5),
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
    node_resources: Arc<Mutex<NodeResourcesCache>>,
    node_resource_interval: Duration,
    service_manager: Arc<Mutex<ServiceManager>>,
    cluster_snapshot: Arc<Mutex<ClusterSnapshotCache>>,
    snapshot_refresh: Arc<Notify>,
    ha_node: Arc<ControlPlaneNode>,
}

const CLUSTER_SNAPSHOT_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
// Cold SSH connects to WAN nodes (e.g. EC2 across the internet) routinely take
// ~1.3s for the handshake plus remote agent spawn, so the old 1.2s budget marked
// healthy remote nodes Offline. Aligned with the ssh ConnectTimeout=5 used in node.rs.
const CLUSTER_REMOTE_QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const CLUSTER_SNAPSHOT_WARMING_UP: &str = "cluster snapshot is warming up";

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
    match crate::vpn::current_local_membership() {
        Ok(membership) => {
            if let Err(error) = store
                .set_node_vpn(crate::node::LOCAL_NODE_NAME, membership.clone())
                .await
            {
                eprintln!("failed to record local VPN membership: {error}");
            } else if let Some(m) = membership {
                eprintln!(
                    "[vpn] local node membership loaded: ip={} role={:?}",
                    m.ip, m.role
                );
            }
        }
        Err(error) => {
            eprintln!("failed to load local VPN membership: {error}");
        }
    }
    let runtime = DockerRuntime::connect()?;
    runtime.ping().await?;
    let snapshot_refresh = Arc::new(Notify::new());

    let cp_node = ControlPlaneNode::start(
        config.peer_id,
        config.peer_addr.clone(),
        Arc::clone(&store),
        config.join_addr.clone(),
    )
    .await?;

    cp_node.spawn_rpc_listener(&config.peer_addr).await?;

    if config.join_addr.is_some() {
        eprintln!(
            "raft: node {} listening on {} (joining existing cluster)",
            config.peer_id, config.peer_addr,
        );
    } else {
        eprintln!(
            "raft: node {} listening on {} (leader)",
            config.peer_id, config.peer_addr,
        );
    }
    let ha_node = Arc::new(cp_node);

    let app_state = AppState {
        store,
        runtime,
        reconcile_guard: Arc::new(Mutex::new(())),
        cleanup_orphans: config.cleanup_orphans,
        daemon_metrics: Arc::new(Mutex::new(DaemonMetricsSampler::new())),
        top_metrics: Arc::new(Mutex::new(TopMetricsSampler::new())),
        node_resources: Arc::new(Mutex::new(NodeResourcesCache::default())),
        node_resource_interval: config.node_resource_interval,
        service_manager: Arc::new(Mutex::new(ServiceManager::new())),
        cluster_snapshot: Arc::new(Mutex::new(ClusterSnapshotCache::default())),
        snapshot_refresh: snapshot_refresh.clone(),
        ha_node,
    };
    let app_state = Arc::new(app_state);
    app_state.sync_services().await?;

    let listener = UnixListener::bind(&config.socket_path).with_context(|| {
        format!(
            "failed to bind daemon Unix socket {}",
            config.socket_path.display()
        )
    })?;

    let effective_api_addr = resolve_api_bind_addr(&config);
    let tcp_api_task = if let Some(api_addr) = effective_api_addr.as_ref() {
        let tcp_listener = tokio::net::TcpListener::bind(api_addr)
            .await
            .with_context(|| format!("failed to bind TCP API listener on {api_addr}"))?;
        if config.api_addr.is_some() {
            eprintln!("api: listening on {api_addr}");
        } else {
            eprintln!("api: listening on {api_addr} (auto-rebound to VPN IP)");
        }
        let state = app_state.clone();
        Some(tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = tcp_listener.accept().await else {
                    continue;
                };
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_tcp_connection(state, stream).await {
                        eprintln!("failed to serve TCP API request: {error}");
                    }
                });
            }
        }))
    } else {
        None
    };

    let notify = Arc::new(Notify::new());
    let reconcile_task =
        spawn_reconcile_loop(app_state.clone(), notify.clone(), config.poll_interval);
    let snapshot_task = spawn_snapshot_loop(
        app_state.clone(),
        snapshot_refresh,
        CLUSTER_SNAPSHOT_REFRESH_INTERVAL,
    );
    let event_listener = app_state.runtime.spawn_event_listener(notify.clone());
    notify.notify_one();
    app_state.request_cluster_snapshot_refresh();

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
    snapshot_task.abort();
    event_listener.abort();
    if let Some(task) = tcp_api_task {
        task.abort();
    }
    if config.socket_path.exists() {
        std::fs::remove_file(&config.socket_path)
            .with_context(|| format!("failed to remove socket {}", config.socket_path.display()))?;
    }
    Ok(())
}

async fn handle_connection(state: Arc<AppState>, mut stream: UnixStream) -> Result<()> {
    let request = read_message::<Request, _>(&mut stream).await?;
    let response = match process_request(state, request).await {
        Ok(response) => response,
        Err(error) => Response::Error {
            message: format!("{error:#}"),
        },
    };
    send_message(&mut stream, &response).await
}

async fn handle_tcp_connection(
    state: Arc<AppState>,
    mut stream: tokio::net::TcpStream,
) -> Result<()> {
    let request = read_message::<Request, _>(&mut stream).await?;
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
        Request::Apply { manifest } => match ParsedManifest::parse(&manifest)? {
            ParsedManifest::Deployment(manifest) => {
                apply_deployment_manifest(state, manifest).await
            }
            ParsedManifest::Service(manifest) => apply_service_manifest(state, manifest).await,
        },
        Request::AddNode {
            name,
            ssh_target,
            remote_binary,
            identity_file,
            labels,
            vpn_ip,
        } => {
            if matches!(name.as_str(), "local") {
                bail!("`local` is reserved for the built-in node");
            }
            let mut node = ClusterNode::ssh(
                name.clone(),
                ssh_target,
                remote_binary.unwrap_or_else(|| DEFAULT_REMOTE_BINARY.to_string()),
                identity_file,
                labels,
            )?;
            if let Some(raw_ip) = vpn_ip.as_deref() {
                let ip: std::net::Ipv4Addr = raw_ip
                    .parse()
                    .with_context(|| format!("invalid VPN IPv4 `{raw_ip}`"))?;
                let hub = crate::vpn::HubState::load()
                    .context("failed to load hub VPN state — is this node the hub?")?;
                let peer = hub
                    .peers
                    .iter()
                    .find(|peer| peer.membership.ip == ip)
                    .with_context(|| {
                        format!(
                            "no peer registered at {ip} in hub state; run `litek8s vpn add-peer` or bootstrap via SSH first"
                        )
                    })?;
                node.vpn = Some(peer.membership.clone());
            }
            let client = NodeClient::for_node(&node, state.runtime.clone());
            client
                .ping()
                .await
                .with_context(|| format!("node `{name}` did not answer over ssh"))?;

            let (ssh_target, remote_binary) = match &node.transport {
                NodeTransport::Ssh {
                    target,
                    remote_binary,
                    ..
                } => (target.clone(), Some(remote_binary.clone())),
                NodeTransport::Local => bail!("cannot add local node via AddNode"),
            };
            let result = state
                .ha_node
                .propose(ClusterCommand::AddWorkerNode {
                    name: name.clone(),
                    ssh_target,
                    remote_binary,
                    labels: node.labels.clone(),
                })
                .await?;
            let created = matches!(result, CommandResult::Applied { created: true });
            if node.vpn.is_some() {
                state
                    .store
                    .set_node_vpn(&name, node.vpn.clone())
                    .await
                    .with_context(|| format!("failed to record VPN membership for `{name}`"))?;
            }
            state
                .record_event(NewEvent {
                    level: EventLevel::Normal,
                    reason: if created {
                        "NodeAdded".to_string()
                    } else {
                        "NodeUpdated".to_string()
                    },
                    object_kind: "node".to_string(),
                    object_name: name.clone(),
                    node_name: Some(name.clone()),
                    message: if node.vpn.is_some() {
                        "node registered and answered over ssh; VPN IP attached".to_string()
                    } else {
                        "node registered and answered over ssh".to_string()
                    },
                })
                .await?;
            state.refresh_cluster_snapshot().await?;
            let message = if created {
                format!("node/{name} added")
            } else {
                format!("node/{name} updated")
            };
            Ok(Response::Ack { message })
        }
        Request::SetNodeScheduling {
            name,
            unschedulable,
        } => {
            state
                .store
                .get_node(&name)
                .await
                .with_context(|| format!("node `{name}` was not found"))?;
            let result = state
                .ha_node
                .propose(ClusterCommand::SetWorkerNodeScheduling {
                    name: name.clone(),
                    unschedulable,
                })
                .await?;
            let changed = matches!(result, CommandResult::Changed { changed: true });
            if changed {
                state
                    .record_event(NewEvent {
                        level: EventLevel::Normal,
                        reason: if unschedulable {
                            "NodeCordoned".to_string()
                        } else {
                            "NodeUncordoned".to_string()
                        },
                        object_kind: "node".to_string(),
                        object_name: name.clone(),
                        node_name: Some(name.clone()),
                        message: if unschedulable {
                            "node marked unschedulable for new deployments".to_string()
                        } else {
                            "node reopened for scheduling".to_string()
                        },
                    })
                    .await?;
                state.reconcile_all().await?;
                state.refresh_cluster_snapshot().await?;
            }

            Ok(Response::Ack {
                message: if unschedulable {
                    format!("node/{name} cordoned")
                } else {
                    format!("node/{name} uncordoned")
                },
            })
        }
        Request::DrainNode { name } => drain_node(state, &name).await,
        Request::DeleteManifest { manifest } => match ParsedManifest::parse(&manifest)? {
            ParsedManifest::Deployment(manifest) => {
                delete_deployment(state, manifest.deployment_name()).await
            }
            ParsedManifest::Service(manifest) => {
                delete_service(state, manifest.service_name()).await
            }
        },
        Request::DeleteDeployment { name } => delete_deployment(state, &name).await,
        Request::AddService {
            name,
            deployment_name,
            listen_addr,
            target_port,
        } => {
            let manifest = ServiceManifest {
                api_version: API_VERSION.to_string(),
                kind: KIND_SERVICE.to_string(),
                metadata: Metadata {
                    name,
                    labels: Default::default(),
                },
                spec: ServiceSpec {
                    deployment_name: Some(deployment_name),
                    listen_addr: Some(listen_addr),
                    target_port,
                    selector: Default::default(),
                    ports: Vec::new(),
                },
            }
            .validate()?;
            apply_service_manifest(state, manifest).await
        }
        Request::DeleteService { name } => delete_service(state, &name).await,
        Request::GetServices => {
            let snapshot = state.latest_cluster_snapshot().await?;
            Ok(Response::Services {
                items: snapshot.services,
            })
        }
        Request::GetNodes => {
            let snapshot = state.latest_cluster_snapshot().await?;
            Ok(Response::Nodes {
                items: snapshot.nodes,
            })
        }
        Request::GetEvents {
            limit,
            since_id,
            deployment_name,
            node_name,
        } => {
            let items = state
                .store
                .list_events(EventQuery {
                    limit,
                    since_id,
                    deployment_name,
                    node_name,
                })
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
                    node_name: entry.assigned_node.clone(),
                    status: deployment_display_status(&entry),
                    image: entry.manifest.container().image.clone(),
                    container_id: entry.container_id.clone(),
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
                node_name: entry.assigned_node.clone(),
                requested_node_name: entry.manifest.explicit_node_name().map(str::to_string),
                node_selector: entry
                    .manifest
                    .node_selector()
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect(),
                status: deployment_display_status(&entry),
                ready: deployment_is_ready(&entry),
                image: entry.manifest.container().image.clone(),
                container_name: deployment_container_name(entry.manifest.deployment_name()),
                container_id: entry.container_id.clone(),
                message: entry.message.clone(),
                command: entry.manifest.container().command.clone(),
                args: entry.manifest.container().args.clone(),
                ports: entry
                    .manifest
                    .container()
                    .ports
                    .iter()
                    .map(|port| port.printable())
                    .collect(),
                readiness_probe: entry
                    .manifest
                    .container()
                    .readiness_probe()
                    .map(format_readiness_probe),
                cpu_limit: entry.manifest.container().cpu_limit().map(str::to_string),
                memory_limit: entry
                    .manifest
                    .container()
                    .memory_limit()
                    .map(str::to_string),
                restart_count: entry.restart_count,
                restart_streak: entry.restart_streak,
                last_exit: entry.last_exit.clone(),
                next_retry_in_secs: retry_in_secs(entry.next_retry_at),
                conditions: deployment_conditions(&entry),
            };

            Ok(Response::Description { item })
        }
        Request::DescribeService { name } => {
            let service = state
                .store
                .get_service(&name)
                .await
                .with_context(|| format!("service `{name}` was not found"))?;
            let runtime_status = state.service_manager.lock().await.status_for(&name).await;
            let view = state
                .inspect_service_state(&service, runtime_status, None)
                .await;

            Ok(Response::ServiceDescription {
                item: ServiceDescription {
                    name: service.name,
                    deployment_name: view.deployment_name,
                    listen_addr: service.listen_addr,
                    target_port: service.target_port,
                    status: view.status,
                    message: view.message,
                    current_node_name: view.current_node_name,
                    current_upstream: view.current_upstream,
                },
            })
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
            let vpn = node.vpn.as_ref().map(|vpn| crate::protocol::NodeVpnInfo {
                role: format!("{:?}", vpn.role).to_lowercase(),
                ip: vpn.ip.to_string(),
                pubkey: vpn.pubkey.as_str().to_string(),
                endpoint: vpn.endpoint.clone(),
                status: format!("{:?}", vpn.status).to_lowercase(),
                last_handshake_unix: vpn.last_handshake_unix,
            });
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
                scheduling: node.scheduling_label().to_string(),
                deployment_count,
                message,
                vpn,
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
        Request::ResolveProxyTarget { name, port } => {
            let entry = state
                .store
                .get(&name)
                .await
                .with_context(|| format!("deployment `{name}` was not found"))?;
            let selected_port = entry.manifest.proxy_port(port)?;
            let host_port = selected_port
                .host_port
                .context("selected port is missing hostPort")?;
            let node = state
                .store
                .get_node(&entry.assigned_node)
                .await
                .with_context(|| format!("node `{}` was not found", entry.assigned_node))?;
            let (transport, ssh_target) = match node.transport {
                NodeTransport::Local => ("local".to_string(), None),
                NodeTransport::Ssh { target, .. } => ("ssh".to_string(), Some(target)),
            };
            Ok(Response::ProxyTarget {
                item: ProxyTargetInfo {
                    deployment_name: name,
                    node_name: entry.assigned_node,
                    transport,
                    ssh_target,
                    container_port: selected_port.container_port,
                    host_port,
                    protocol: selected_port.protocol.as_printable().to_string(),
                },
            })
        }
        Request::Top => {
            let snapshot = state.latest_cluster_snapshot().await?;
            Ok(Response::Top {
                daemon: snapshot.daemon,
                items: snapshot.deployments,
                nodes: snapshot.nodes,
            })
        }
        Request::GetClusterStatus => {
            use crate::protocol::ClusterMember;

            let metrics = state.ha_node.raft.metrics().borrow().clone();
            let leader_id = metrics.current_leader;
            let node_id = metrics.id;
            let state_str = format!("{:?}", metrics.state);
            let members = metrics
                .membership_config
                .membership()
                .nodes()
                .map(|(id, node)| ClusterMember {
                    node_id: *id,
                    addr: node.addr.clone(),
                    is_leader: Some(*id) == leader_id,
                })
                .collect();
            Ok(Response::ClusterStatus {
                ha_enabled: true,
                node_id: Some(node_id),
                leader_id,
                state: state_str,
                members,
            })
        }
        Request::DistributedMatmul {
            left,
            right,
            use_cache,
        } => state.distributed_matmul(left, right, use_cache).await,
        Request::JoinCluster { node_id, addr } => {
            state.ha_node.add_member(node_id, addr).await?;
            Ok(Response::Ack {
                message: format!("control-plane node {node_id} joined the cluster"),
            })
        }
    }
}

async fn delete_deployment(state: Arc<AppState>, name: &str) -> Result<Response> {
    let deployment = state
        .store
        .get(name)
        .await
        .with_context(|| format!("deployment `{name}` was not found"))?;
    let result = state
        .ha_node
        .propose(ClusterCommand::DeleteDeployment {
            name: name.to_string(),
        })
        .await?;
    if !matches!(result, CommandResult::Changed { changed: true }) {
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
    state.refresh_cluster_snapshot().await?;
    Ok(Response::Ack {
        message: format!("deployment.apps/{name} deleted"),
    })
}

async fn apply_deployment_manifest(
    state: Arc<AppState>,
    manifest: DeploymentManifest,
) -> Result<Response> {
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
    let manifest_yaml = serde_yaml::to_string(&manifest).context("failed to serialize manifest")?;
    let result = state
        .ha_node
        .propose(ClusterCommand::ApplyDeployment { manifest_yaml })
        .await?;
    let created = matches!(result, CommandResult::Applied { created: true });
    let _ = state
        .ha_node
        .propose(ClusterCommand::SetDeploymentAssignment {
            name: name.clone(),
            assigned_node: assigned_node.clone(),
        })
        .await?;
    state
        .record_event(NewEvent {
            level: EventLevel::Normal,
            reason: if created {
                "Applied".to_string()
            } else {
                "Configured".to_string()
            },
            object_kind: "deployment".to_string(),
            object_name: name.clone(),
            node_name: Some(assigned_node),
            message: "deployment manifest accepted by the control plane".to_string(),
        })
        .await?;
    state.reconcile_all().await?;
    state.refresh_cluster_snapshot().await?;

    Ok(Response::Ack {
        message: if created {
            format!("deployment.apps/{name} created")
        } else {
            format!("deployment.apps/{name} configured")
        },
    })
}

async fn apply_service_manifest(
    state: Arc<AppState>,
    manifest: ServiceManifest,
) -> Result<Response> {
    let name = manifest.service_name().to_string();
    let deployment_name = manifest
        .configured_deployment_name()
        .unwrap_or_default()
        .to_string();
    let selector = manifest.selector().clone();
    let listen_addr = manifest.listen_addr()?;
    let target_port = manifest.resolved_target_port();

    let service_for_resolution = crate::store::StoredService {
        name: name.clone(),
        deployment_name: deployment_name.clone(),
        selector: selector.clone(),
        listen_addr: listen_addr.clone(),
        target_port,
        updated_at: 0,
    };
    let deployment = resolve_backing_deployment(&state.store, &service_for_resolution).await?;
    let _ = deployment.manifest.proxy_port(target_port)?;

    let existing = state.store.get_service(&name).await;
    let needs_bind_probe = existing
        .as_ref()
        .map(|service| service.listen_addr != listen_addr)
        .unwrap_or(true);
    if needs_bind_probe {
        let _probe = tokio::net::TcpListener::bind(&listen_addr)
            .await
            .with_context(|| format!("failed to bind service listen address {listen_addr}"))?;
    }

    let result = state
        .ha_node
        .propose(ClusterCommand::ApplyService {
            name: name.clone(),
            deployment_name: deployment_name.clone(),
            selector: selector.clone(),
            listen_addr: listen_addr.clone(),
            target_port,
        })
        .await?;
    let created = matches!(result, CommandResult::Applied { created: true });
    state.sync_services().await?;
    let target_text = if deployment_name.is_empty() {
        format!(
            "selector `{}`",
            selector
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(",")
        )
    } else {
        format!("deployment `{deployment_name}`")
    };
    state
        .record_event(NewEvent {
            level: EventLevel::Normal,
            reason: if created {
                "ServiceAdded".to_string()
            } else {
                "ServiceUpdated".to_string()
            },
            object_kind: "service".to_string(),
            object_name: name.clone(),
            node_name: None,
            message: format!("service listens on {listen_addr} and forwards to {target_text}"),
        })
        .await?;
    state.refresh_cluster_snapshot().await?;
    Ok(Response::Ack {
        message: if created {
            format!("service/{name} created")
        } else {
            format!("service/{name} configured")
        },
    })
}

async fn delete_service(state: Arc<AppState>, name: &str) -> Result<Response> {
    let result = state
        .ha_node
        .propose(ClusterCommand::DeleteService {
            name: name.to_string(),
        })
        .await?;
    if !matches!(result, CommandResult::Changed { changed: true }) {
        bail!("service `{name}` was not found");
    }
    state.sync_services().await?;
    state
        .record_event(NewEvent {
            level: EventLevel::Normal,
            reason: "ServiceDeleted".to_string(),
            object_kind: "service".to_string(),
            object_name: name.to_string(),
            node_name: None,
            message: "service listener removed from the control plane".to_string(),
        })
        .await?;
    state.refresh_cluster_snapshot().await?;
    Ok(Response::Ack {
        message: format!("service/{name} deleted"),
    })
}

async fn drain_node(state: Arc<AppState>, name: &str) -> Result<Response> {
    let _node = state
        .store
        .get_node(name)
        .await
        .with_context(|| format!("node `{name}` was not found"))?;
    let cordon_result = state
        .ha_node
        .propose(ClusterCommand::SetWorkerNodeScheduling {
            name: name.to_string(),
            unschedulable: true,
        })
        .await?;
    let cordoned = matches!(cordon_result, CommandResult::Changed { changed: true });
    let nodes = state.store.list_nodes().await;
    let deployments = state.store.list().await;
    if cordoned {
        state
            .record_event(NewEvent {
                level: EventLevel::Normal,
                reason: "NodeCordoned".to_string(),
                object_kind: "node".to_string(),
                object_name: name.to_string(),
                node_name: Some(name.to_string()),
                message: "node marked unschedulable for drain".to_string(),
            })
            .await?;
    }
    state
        .record_event(NewEvent {
            level: EventLevel::Normal,
            reason: "NodeDrainStarted".to_string(),
            object_kind: "node".to_string(),
            object_name: name.to_string(),
            node_name: Some(name.to_string()),
            message: "drain requested; evacuating movable deployments".to_string(),
        })
        .await?;

    let mut moved = 0usize;
    let mut blocked = 0usize;
    for deployment in deployments
        .into_iter()
        .filter(|deployment| deployment.assigned_node == name)
    {
        match drain_target_for_deployment(&deployment, &nodes, name) {
            DrainDecision::Move(target_node) => {
                let assign_result = state
                    .ha_node
                    .propose(ClusterCommand::SetDeploymentAssignment {
                        name: deployment.manifest.deployment_name().to_string(),
                        assigned_node: target_node.clone(),
                    })
                    .await?;
                if matches!(assign_result, CommandResult::Changed { changed: true }) {
                    let _ = state
                        .ha_node
                        .propose(ClusterCommand::SetDeploymentStatus {
                            name: deployment.manifest.deployment_name().to_string(),
                            status: DeploymentStatus::Pending,
                            ready: false,
                            container_id: None,
                            message: Some(format!(
                                "evicted from node `{name}`; rescheduling to `{target_node}`"
                            )),
                            last_exit: Some("drained".to_string()),
                            increment_restart_count: false,
                            increment_restart_streak: false,
                            next_retry_at: None,
                        })
                        .await?;
                    state
                        .record_event(NewEvent {
                            level: EventLevel::Normal,
                            reason: "Evicted".to_string(),
                            object_kind: "deployment".to_string(),
                            object_name: deployment.manifest.deployment_name().to_string(),
                            node_name: Some(name.to_string()),
                            message: format!(
                                "evicted from node `{name}`; target node `{target_node}` selected"
                            ),
                        })
                        .await?;
                    moved = moved.saturating_add(1);
                }
            }
            DrainDecision::Blocked(reason) => {
                blocked = blocked.saturating_add(1);
                state
                    .record_event(NewEvent {
                        level: EventLevel::Warning,
                        reason: "DrainBlocked".to_string(),
                        object_kind: "deployment".to_string(),
                        object_name: deployment.manifest.deployment_name().to_string(),
                        node_name: Some(name.to_string()),
                        message: reason,
                    })
                    .await?;
            }
            DrainDecision::Skip => {}
        }
    }
    state.reconcile_all().await?;
    state.refresh_cluster_snapshot().await?;

    Ok(Response::Ack {
        message: format!("node/{name} drained: {moved} moved, {blocked} blocked"),
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

fn spawn_snapshot_loop(
    state: Arc<AppState>,
    notify: Arc<Notify>,
    refresh_interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(refresh_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = notify.notified() => {}
            }

            if let Err(error) = state.refresh_cluster_snapshot().await {
                eprintln!("cluster snapshot refresh failed: {error}");
            }
        }
    })
}

impl AppState {
    async fn reconcile_all(&self) -> Result<()> {
        let _guard = self.reconcile_guard.lock().await;
        let nodes = self.store.list_nodes().await;
        let local_runtime = crate::runtime::LocalRuntime::Docker(self.runtime.clone());
        reconciler::reconcile_all(
            &self.store,
            &self.ha_node,
            &local_runtime,
            &nodes,
            self.cleanup_orphans,
        )
        .await?;
        self.request_cluster_snapshot_refresh();
        Ok(())
    }

    async fn client_for_node_name(&self, name: &str) -> Result<NodeClient> {
        let node = self
            .store
            .get_node(name)
            .await
            .with_context(|| format!("node `{name}` was not found"))?;
        Ok(NodeClient::for_node(&node, self.runtime.clone()))
    }

    /// Distribute a matrix multiply across the cluster's schedulable, reachable
    /// nodes. Splits `left` into contiguous row bands (one per node), ships each
    /// band with the full `right` to its node concurrently, then stitches the
    /// returned rows back in order. Mirrors how a weight matmul is sharded for
    /// tensor parallelism — each node holds a row slice and computes it locally.
    async fn distributed_matmul(
        &self,
        left: Vec<Vec<f64>>,
        right: Vec<Vec<f64>>,
        use_cache: bool,
    ) -> Result<Response> {
        ensure!(!left.is_empty(), "left matrix cannot be empty");
        ensure!(!right.is_empty(), "right matrix cannot be empty");
        let shared = left[0].len();
        ensure!(
            left.iter().all(|row| row.len() == shared),
            "left matrix must be rectangular"
        );
        ensure!(
            right.len() == shared,
            "dimension mismatch: left columns ({shared}) must match right rows ({})",
            right.len()
        );

        // Only fan out to nodes that are schedulable and answer a ping. A node
        // that's cordoned or offline is skipped so the job still completes on
        // whatever capacity is live.
        let all_nodes = self.store.list_nodes().await;
        let mut targets: Vec<ClusterNode> = Vec::new();
        for node in all_nodes {
            if node.unschedulable {
                continue;
            }
            let (status, _) = self.node_health(&node).await;
            if status == "Ready" {
                targets.push(node);
            }
        }
        ensure!(
            !targets.is_empty(),
            "no schedulable, reachable node is available to compute"
        );

        // Split rows as evenly as possible: the first `remainder` nodes get one
        // extra row. Nodes that would receive zero rows (more nodes than rows)
        // are dropped rather than handed empty bands.
        let total_rows = left.len();
        let node_count = targets.len().min(total_rows);
        let targets = &targets[..node_count];
        let base = total_rows / node_count;
        let remainder = total_rows % node_count;

        // When caching is on, content-address `right` once so every node stores
        // it under the same id and a repeat run with identical weights skips the
        // upload entirely.
        let weight_id = use_cache.then(|| crate::matrix::matrix_id(&right));

        let right = Arc::new(right);
        let weight_id = weight_id.map(Arc::new);
        let wall_started = Instant::now();
        let mut tasks = Vec::with_capacity(node_count);
        let mut cursor = 0;
        for (idx, node) in targets.iter().enumerate() {
            let rows_here = base + if idx < remainder { 1 } else { 0 };
            let start_row = cursor;
            cursor += rows_here;
            let band = left[start_row..cursor].to_vec();
            let client = NodeClient::for_node(node, self.runtime.clone());
            let right = Arc::clone(&right);
            let weight_id = weight_id.clone();
            let node_name = node.name.clone();
            let transport = node.transport_label().to_string();

            tasks.push(tokio::spawn(async move {
                let started = Instant::now();
                let (result, weight_cached) = match weight_id.as_deref() {
                    Some(id) => {
                        // Upload `right` only if the node lacks it, then compute
                        // against the cached copy — no per-band transfer.
                        match client.ensure_weights(id, &right).await {
                            Ok(cached) => {
                                let result = client
                                    .compute_matmul_band_cached(band, id)
                                    .await
                                    .with_context(|| {
                                        format!("node `{node_name}` failed its cached matmul band")
                                    });
                                (result, Some(cached))
                            }
                            Err(error) => (
                                Err(error.context(format!(
                                    "node `{node_name}` failed to cache weights"
                                ))),
                                None,
                            ),
                        }
                    }
                    None => {
                        let result = client
                            .compute_matmul_band(band, (*right).clone())
                            .await
                            .with_context(|| format!("node `{node_name}` failed its matmul band"));
                        (result, None)
                    }
                };
                let elapsed_ms = started.elapsed().as_millis() as u64;
                (
                    node_name,
                    transport,
                    start_row,
                    rows_here,
                    elapsed_ms,
                    weight_cached,
                    result,
                )
            }));
        }

        let mut bands: Vec<(usize, Vec<Vec<f64>>)> = Vec::with_capacity(node_count);
        let mut placements = Vec::with_capacity(node_count);
        for task in tasks {
            let (node_name, transport, start_row, rows, elapsed_ms, weight_cached, result) =
                task.await.context("matmul dispatch task panicked")?;
            let rows_out = result?;
            placements.push(crate::protocol::MatmulPlacement {
                node_name,
                transport,
                start_row,
                rows,
                elapsed_ms,
                weight_cached,
            });
            bands.push((start_row, rows_out));
        }
        let wall_ms = wall_started.elapsed().as_millis() as u64;

        // Stitch bands back in row order regardless of which node finished first.
        bands.sort_by_key(|(start, _)| *start);
        placements.sort_by_key(|p| p.start_row);
        let result: Vec<Vec<f64>> = bands.into_iter().flat_map(|(_, rows)| rows).collect();

        Ok(Response::Matmul {
            result,
            placements,
            wall_ms,
        })
    }

    async fn node_health(&self, node: &ClusterNode) -> (String, Option<String>) {
        let client = NodeClient::for_node(node, self.runtime.clone());
        match tokio::time::timeout(CLUSTER_REMOTE_QUERY_TIMEOUT, client.ping()).await {
            Ok(Ok(())) => ("Ready".to_string(), None),
            Ok(Err(error)) => ("Offline".to_string(), Some(error.to_string())),
            Err(_) => (
                "Offline".to_string(),
                Some(format!(
                    "health check timed out after {}ms",
                    CLUSTER_REMOTE_QUERY_TIMEOUT.as_millis()
                )),
            ),
        }
    }

    /// Fetch host resources for a node, returning None on timeout or error so the
    /// snapshot never blocks on a slow/old node (an old agent that doesn't know
    /// the HostResources op simply errors and degrades to None).
    async fn collect_node_resources(
        &self,
        node: &ClusterNode,
    ) -> Option<crate::protocol::HostResources> {
        let client = NodeClient::for_node(node, self.runtime.clone());
        match tokio::time::timeout(CLUSTER_REMOTE_QUERY_TIMEOUT, client.host_resources()).await {
            Ok(Ok(resources)) => Some(resources),
            _ => None,
        }
    }

    async fn record_event(&self, event: NewEvent) -> Result<()> {
        self.ha_node
            .propose(ClusterCommand::AppendEvent {
                level: event.level,
                reason: event.reason,
                object_kind: event.object_kind,
                object_name: event.object_name,
                node_name: event.node_name,
                message: event.message,
            })
            .await?;
        Ok(())
    }

    async fn sync_services(&self) -> Result<()> {
        self.service_manager
            .lock()
            .await
            .sync(self.store.clone())
            .await
    }

    async fn inspect_service_state(
        &self,
        service: &crate::store::StoredService,
        runtime_status: Option<ServiceStatusSnapshot>,
        node_statuses: Option<&HashMap<String, String>>,
    ) -> EffectiveServiceState {
        let backend = self.inspect_service_backend(service, node_statuses).await;
        effective_service_state(service, runtime_status.as_ref(), backend)
    }

    async fn inspect_service_backend(
        &self,
        service: &crate::store::StoredService,
        node_statuses: Option<&HashMap<String, String>>,
    ) -> ServiceBackendState {
        let deployment = match resolve_backing_deployment(&self.store, service).await {
            Ok(deployment) => deployment,
            Err(error) => {
                return ServiceBackendState {
                    deployment_name: service_deployment_hint(service),
                    current_node_name: None,
                    current_upstream: None,
                    health: ServiceBackendHealth::NoEndpoints,
                    message: Some(error.to_string()),
                };
            }
        };

        let deployment_name = deployment.manifest.deployment_name().to_string();
        let current_node_name = Some(deployment.assigned_node.clone());

        if !deployment_can_receive_traffic(&deployment) {
            return ServiceBackendState {
                deployment_name,
                current_node_name,
                current_upstream: None,
                health: ServiceBackendHealth::NoEndpoints,
                message: Some(format_deployment_backend_message(&deployment)),
            };
        }

        if deployment.container_id.is_none() {
            return ServiceBackendState {
                deployment_name,
                current_node_name,
                current_upstream: None,
                health: ServiceBackendHealth::NoEndpoints,
                message: Some(format!(
                    "deployment `{}` has no running container yet",
                    deployment.manifest.deployment_name()
                )),
            };
        }

        let selected_port = match deployment.manifest.proxy_port(service.target_port) {
            Ok(selected_port) => selected_port,
            Err(error) => {
                return ServiceBackendState {
                    deployment_name,
                    current_node_name,
                    current_upstream: None,
                    health: ServiceBackendHealth::Degraded,
                    message: Some(error.to_string()),
                };
            }
        };

        let current_upstream = match selected_port.host_port {
            Some(host_port) => format!("127.0.0.1:{host_port}"),
            None => {
                return ServiceBackendState {
                    deployment_name,
                    current_node_name,
                    current_upstream: None,
                    health: ServiceBackendHealth::Degraded,
                    message: Some("selected service target is missing hostPort".to_string()),
                };
            }
        };

        let node_status = match node_statuses.and_then(|items| items.get(&deployment.assigned_node))
        {
            Some(status) => status.clone(),
            None => match self.store.get_node(&deployment.assigned_node).await {
                Some(node) => self.node_health(&node).await.0,
                None => "Offline".to_string(),
            },
        };
        if node_status != "Ready" {
            return ServiceBackendState {
                deployment_name,
                current_node_name,
                current_upstream: Some(current_upstream),
                health: ServiceBackendHealth::Degraded,
                message: Some(format!(
                    "node `{}` is {}",
                    deployment.assigned_node, node_status
                )),
            };
        }

        ServiceBackendState {
            deployment_name,
            current_node_name,
            current_upstream: Some(current_upstream),
            health: ServiceBackendHealth::Ready,
            message: None,
        }
    }

    async fn latest_cluster_snapshot(&self) -> Result<ClusterSnapshot> {
        let cache = self.cluster_snapshot.lock().await;
        cache
            .snapshot
            .clone()
            .ok_or_else(|| anyhow::anyhow!(CLUSTER_SNAPSHOT_WARMING_UP))
    }

    fn request_cluster_snapshot_refresh(&self) {
        self.snapshot_refresh.notify_one();
    }

    async fn refresh_cluster_snapshot(&self) -> Result<()> {
        let snapshot = self.collect_cluster_snapshot().await?;
        let mut cache = self.cluster_snapshot.lock().await;
        cache.generated_at = Some(Instant::now());
        cache.snapshot = Some(snapshot);
        Ok(())
    }

    async fn collect_cluster_snapshot(&self) -> Result<ClusterSnapshot> {
        let deployments = self.store.list().await;
        let nodes = self.store.list_nodes().await;
        let services = self.store.list_services().await;
        let service_statuses = self.service_manager.lock().await.statuses().await;

        let mut deployment_items = Vec::with_capacity(deployments.len());
        let mut active_container_ids = HashSet::new();
        let mut stats_snapshots = HashMap::new();

        for entry in &deployments {
            if let Some(container_id) = entry.container_id.as_ref() {
                let sample_key = top_sample_key(&entry.assigned_node, container_id);
                active_container_ids.insert(sample_key.clone());
                if let Ok(runtime) = self.client_for_node_name(&entry.assigned_node).await {
                    if let Ok(Ok(snapshot)) = tokio::time::timeout(
                        CLUSTER_REMOTE_QUERY_TIMEOUT,
                        runtime.stats_snapshot_for_container(container_id),
                    )
                    .await
                    {
                        stats_snapshots.insert(sample_key, snapshot);
                    }
                }
            }

            deployment_items.push(DeploymentTopRow {
                name: entry.manifest.deployment_name().to_string(),
                node_name: entry.assigned_node.clone(),
                status: deployment_display_status(entry),
                ready: deployment_is_ready(entry),
                image: entry.manifest.container().image.clone(),
                container_id: entry.container_id.clone(),
                ports: entry
                    .manifest
                    .container()
                    .ports
                    .iter()
                    .map(|port| port.printable())
                    .collect(),
                restart_count: entry.restart_count,
                restart_streak: entry.restart_streak,
                last_exit: entry.last_exit.clone(),
                next_retry_in_secs: retry_in_secs(entry.next_retry_at),
                cpu_percent: None,
                memory_usage_bytes: None,
                memory_limit_bytes: None,
                network_rx_bytes: None,
                network_tx_bytes: None,
            });
        }

        let mut top_metrics = self.top_metrics.lock().await;
        top_metrics.retain_active(&active_container_ids);
        for item in &mut deployment_items {
            if let Some(container_id) = item.container_id.as_ref() {
                let sample_key = top_sample_key(&item.node_name, container_id);
                if let Some(snapshot) = stats_snapshots.get(&sample_key) {
                    let metrics = top_metrics.metrics_for(&sample_key, snapshot);
                    item.cpu_percent = metrics.cpu_percent;
                    item.memory_usage_bytes = metrics.memory_usage_bytes;
                    item.memory_limit_bytes = metrics.memory_limit_bytes;
                    item.network_rx_bytes = metrics.network_rx_bytes;
                    item.network_tx_bytes = metrics.network_tx_bytes;
                }
            }
        }
        drop(top_metrics);

        let live_node_names: HashSet<String> =
            nodes.iter().map(|node| node.name.clone()).collect();
        {
            let mut cache = self.node_resources.lock().await;
            cache.retain_nodes(&live_node_names);
        }

        let mut node_items = Vec::with_capacity(nodes.len());
        for node in nodes {
            let deployment_count = deployments
                .iter()
                .filter(|entry| entry.assigned_node == node.name)
                .count();
            let (status, _) = self.node_health(&node).await;

            // Host resources, served from cache unless the TTL has lapsed. Only
            // query Ready nodes; failures degrade to None ("-" in the UI).
            let host_resources = if status == "Ready" {
                let cached = self
                    .node_resources
                    .lock()
                    .await
                    .get_fresh(&node.name, self.node_resource_interval);
                match cached {
                    Some(resources) => Some(resources),
                    None => match self.collect_node_resources(&node).await {
                        Some(resources) => {
                            self.node_resources.lock().await.store(
                                &node.name,
                                Instant::now(),
                                resources.clone(),
                            );
                            Some(resources)
                        }
                        None => None,
                    },
                }
            } else {
                None
            };

            node_items.push(NodeSummary {
                name: node.name.clone(),
                transport: node.transport_label().to_string(),
                endpoint: node.endpoint(),
                status,
                scheduling: node.scheduling_label().to_string(),
                deployment_count,
                vpn_ip: node.vpn.as_ref().map(|vpn| vpn.ip.to_string()),
                vpn_role: node
                    .vpn
                    .as_ref()
                    .map(|vpn| format!("{:?}", vpn.role).to_lowercase()),
                host_resources,
            });
        }

        let node_statuses = node_items
            .iter()
            .map(|node| (node.name.clone(), node.status.clone()))
            .collect::<HashMap<_, _>>();

        let mut service_items = Vec::with_capacity(services.len());
        for service in services {
            let runtime_status = service_statuses.get(&service.name).cloned();
            let view = self
                .inspect_service_state(&service, runtime_status, Some(&node_statuses))
                .await;
            service_items.push(ServiceSummary {
                name: service.name.clone(),
                deployment_name: view.deployment_name,
                listen_addr: service.listen_addr.clone(),
                target_port: service.target_port,
                status: view.status,
            });
        }

        let daemon = self.daemon_metrics.lock().await.snapshot();

        Ok(ClusterSnapshot {
            daemon,
            deployments: deployment_items,
            nodes: node_items,
            services: service_items,
        })
    }
}

fn deployment_is_ready(entry: &crate::store::StoredDeployment) -> bool {
    if entry.manifest.container().readiness_probe().is_some() {
        entry.ready
    } else {
        entry.status == DeploymentStatus::Running && entry.container_id.is_some()
    }
}

fn deployment_can_receive_traffic(entry: &crate::store::StoredDeployment) -> bool {
    deployment_is_ready(entry)
}

fn deployment_display_status(entry: &crate::store::StoredDeployment) -> String {
    if entry.manifest.container().readiness_probe().is_some() && entry.ready {
        "Ready".to_string()
    } else {
        entry.status.to_string()
    }
}

fn format_readiness_probe(probe: &crate::manifest::ReadinessProbe) -> String {
    let mut text = format!("tcpSocket:{} ", probe.tcp_socket.port);
    text.push_str(&format!(
        "(initialDelay={}s timeout={}s)",
        probe.initial_delay_seconds(),
        probe.timeout_seconds()
    ));
    text
}

fn deployment_conditions(entry: &crate::store::StoredDeployment) -> Vec<DeploymentCondition> {
    let mut items = Vec::new();
    items.push(DeploymentCondition {
        type_name: "Scheduled".to_string(),
        status: if !entry.assigned_node.is_empty() {
            "True".to_string()
        } else {
            "False".to_string()
        },
        reason: if !entry.assigned_node.is_empty() {
            "AssignedNode".to_string()
        } else {
            "PendingScheduling".to_string()
        },
        message: (!entry.assigned_node.is_empty())
            .then(|| format!("assigned to node `{}`", entry.assigned_node)),
        last_transition_secs: entry.updated_at,
    });
    items.push(DeploymentCondition {
        type_name: "Started".to_string(),
        status: if entry.container_id.is_some() {
            "True".to_string()
        } else {
            "False".to_string()
        },
        reason: if entry.container_id.is_some() {
            "ContainerPresent".to_string()
        } else {
            "NoContainer".to_string()
        },
        message: entry
            .container_id
            .as_ref()
            .map(|id| format!("managed container `{}` is present", short_container_id(id))),
        last_transition_secs: entry
            .container_started_at
            .or(entry.container_id.as_ref().map(|_| entry.updated_at))
            .unwrap_or(entry.updated_at),
    });
    items.push(DeploymentCondition {
        type_name: "Ready".to_string(),
        status: if deployment_is_ready(entry) {
            "True".to_string()
        } else {
            "False".to_string()
        },
        reason: if deployment_is_ready(entry) {
            if entry.manifest.container().readiness_probe().is_some() {
                "ReadinessProbeSucceeded".to_string()
            } else {
                "ContainerRunning".to_string()
            }
        } else if entry.manifest.container().readiness_probe().is_some() {
            "ReadinessProbePending".to_string()
        } else {
            "ContainerNotRunning".to_string()
        },
        message: entry.message.clone(),
        last_transition_secs: entry
            .ready_since
            .or(entry.container_started_at)
            .unwrap_or(entry.updated_at),
    });
    items.push(DeploymentCondition {
        type_name: "Degraded".to_string(),
        status: if matches!(
            entry.status,
            DeploymentStatus::Failed | DeploymentStatus::Restarting
        ) {
            "True".to_string()
        } else {
            "False".to_string()
        },
        reason: match entry.status {
            DeploymentStatus::Failed => "Failed".to_string(),
            DeploymentStatus::Restarting => "Restarting".to_string(),
            _ => "Healthy".to_string(),
        },
        message: entry.message.clone(),
        last_transition_secs: entry.updated_at,
    });
    items
}

fn short_container_id(id: &str) -> String {
    id.chars().take(12).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EffectiveServiceState {
    deployment_name: String,
    status: String,
    message: Option<String>,
    current_node_name: Option<String>,
    current_upstream: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServiceBackendState {
    deployment_name: String,
    current_node_name: Option<String>,
    current_upstream: Option<String>,
    health: ServiceBackendHealth,
    message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceBackendHealth {
    Ready,
    NoEndpoints,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DrainDecision {
    Move(String),
    Blocked(String),
    Skip,
}

fn drain_target_for_deployment(
    deployment: &crate::store::StoredDeployment,
    nodes: &[ClusterNode],
    drained_node: &str,
) -> DrainDecision {
    if deployment.assigned_node != drained_node {
        return DrainDecision::Skip;
    }

    if deployment.manifest.explicit_node_name() == Some(drained_node) {
        return DrainDecision::Blocked(format!(
            "deployment `{}` is pinned to node `{drained_node}` via nodeName",
            deployment.manifest.deployment_name()
        ));
    }

    match select_node_for_manifest(&deployment.manifest, nodes, None) {
        Ok(target) if target != drained_node => DrainDecision::Move(target),
        Ok(_) => DrainDecision::Blocked(format!(
            "deployment `{}` has no alternate schedulable node",
            deployment.manifest.deployment_name()
        )),
        Err(error) => DrainDecision::Blocked(format!(
            "deployment `{}` cannot be evacuated: {error}",
            deployment.manifest.deployment_name()
        )),
    }
}

fn effective_service_state(
    service: &crate::store::StoredService,
    runtime_status: Option<&ServiceStatusSnapshot>,
    backend: ServiceBackendState,
) -> EffectiveServiceState {
    let (status, message) = match runtime_status {
        Some(runtime) if runtime.status == "Error" => {
            ("Error".to_string(), runtime.message.clone())
        }
        Some(runtime) if runtime.status == "Starting" => {
            ("Starting".to_string(), runtime.message.clone())
        }
        Some(_) => match backend.health {
            ServiceBackendHealth::Ready => ("Listening".to_string(), None),
            ServiceBackendHealth::NoEndpoints => {
                ("NoEndpoints".to_string(), backend.message.clone())
            }
            ServiceBackendHealth::Degraded => ("Degraded".to_string(), backend.message.clone()),
        },
        None => ("Stopped".to_string(), None),
    };

    EffectiveServiceState {
        deployment_name: backend.deployment_name,
        status,
        message: merge_service_message(message, selector_hint(service)),
        current_node_name: backend.current_node_name,
        current_upstream: backend.current_upstream,
    }
}

fn service_deployment_hint(service: &crate::store::StoredService) -> String {
    if !service.deployment_name.is_empty() {
        service.deployment_name.clone()
    } else if !service.selector.is_empty() {
        format!(
            "selector:{}",
            service
                .selector
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(",")
        )
    } else {
        "-".to_string()
    }
}

fn selector_hint(service: &crate::store::StoredService) -> Option<String> {
    if service.selector.is_empty() {
        None
    } else {
        Some(
            service
                .selector
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

fn merge_service_message(message: Option<String>, selector_hint: Option<String>) -> Option<String> {
    match (message, selector_hint) {
        (Some(message), Some(selector)) => Some(format!("{message}; selector {selector}")),
        (Some(message), None) => Some(message),
        (None, _) => None,
    }
}

fn format_deployment_backend_message(deployment: &crate::store::StoredDeployment) -> String {
    match deployment.message.as_deref() {
        Some(message) => format!(
            "deployment `{}` is {}: {message}",
            deployment.manifest.deployment_name(),
            deployment_display_status(deployment)
        ),
        None => format!(
            "deployment `{}` is {}",
            deployment.manifest.deployment_name(),
            deployment_display_status(deployment)
        ),
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

#[derive(Debug, Clone)]
struct ClusterSnapshot {
    daemon: DaemonTopSummary,
    deployments: Vec<DeploymentTopRow>,
    nodes: Vec<NodeSummary>,
    services: Vec<ServiceSummary>,
}

#[derive(Default)]
struct ClusterSnapshotCache {
    generated_at: Option<Instant>,
    snapshot: Option<ClusterSnapshot>,
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

/// Caches the last host-resource sample per node so the daemon does not query
/// every node over SSH on each 1s snapshot tick. Entries expire after the
/// configured `node_resource_interval` (default 5s).
#[derive(Default)]
struct NodeResourcesCache {
    entries: HashMap<String, (Instant, crate::protocol::HostResources)>,
}

impl NodeResourcesCache {
    fn get_fresh(&self, node: &str, ttl: Duration) -> Option<crate::protocol::HostResources> {
        self.entries.get(node).and_then(|(at, resources)| {
            (at.elapsed() < ttl).then(|| resources.clone())
        })
    }

    fn store(&mut self, node: &str, at: Instant, resources: crate::protocol::HostResources) {
        self.entries.insert(node.to_string(), (at, resources));
    }

    fn retain_nodes(&mut self, live: &HashSet<String>) {
        self.entries.retain(|name, _| live.contains(name));
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
    use std::collections::{BTreeMap, HashSet};

    use super::{
        DrainDecision, ServiceBackendHealth, ServiceBackendState, TopMetricsSampler,
        drain_target_for_deployment, effective_service_state, resolve_api_bind_addr_with,
    };
    use crate::manifest::{API_VERSION, DeploymentManifest};
    use crate::node::{ClusterNode, NodeTransport};
    use crate::runtime::docker::ContainerStatsSnapshot;
    use crate::service::ServiceStatusSnapshot;
    use crate::store::{DeploymentStatus, StoredDeployment, StoredService};
    use crate::vpn::{KeyPair, VpnMembership};
    use std::net::Ipv4Addr;

    #[test]
    fn api_bind_addr_uses_explicit_when_provided() {
        let kp = KeyPair::generate();
        let membership =
            VpnMembership::new_spoke(Ipv4Addr::new(10, 42, 0, 5), kp.public());
        let got = resolve_api_bind_addr_with(Some("0.0.0.0:7000"), Some(&membership));
        assert_eq!(got.as_deref(), Some("0.0.0.0:7000"));
    }

    #[test]
    fn api_bind_addr_falls_back_to_vpn_ip() {
        let kp = KeyPair::generate();
        let membership =
            VpnMembership::new_spoke(Ipv4Addr::new(10, 42, 0, 5), kp.public());
        let got = resolve_api_bind_addr_with(None, Some(&membership));
        assert_eq!(got.as_deref(), Some("10.42.0.5:6443"));
    }

    #[test]
    fn api_bind_addr_none_without_explicit_nor_vpn() {
        let got = resolve_api_bind_addr_with(None, None);
        assert!(got.is_none());
    }

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

    #[test]
    fn service_state_reports_no_endpoints_when_backend_is_missing() {
        let service = StoredService {
            name: "edge-svc".to_string(),
            deployment_name: "echo-edge".to_string(),
            selector: BTreeMap::new(),
            listen_addr: "127.0.0.1:9102".to_string(),
            target_port: Some(5678),
            updated_at: 0,
        };
        let runtime = ServiceStatusSnapshot {
            status: "Listening".to_string(),
            message: None,
        };
        let backend = ServiceBackendState {
            deployment_name: "echo-edge".to_string(),
            current_node_name: None,
            current_upstream: None,
            health: ServiceBackendHealth::NoEndpoints,
            message: Some("deployment `echo-edge` was not found".to_string()),
        };

        let state = effective_service_state(&service, Some(&runtime), backend);
        assert_eq!(state.status, "NoEndpoints");
        assert_eq!(
            state.message.as_deref(),
            Some("deployment `echo-edge` was not found")
        );
    }

    #[test]
    fn service_state_reports_degraded_when_node_is_offline() {
        let service = StoredService {
            name: "edge-svc".to_string(),
            deployment_name: String::new(),
            selector: BTreeMap::from([(String::from("role"), String::from("edge"))]),
            listen_addr: "127.0.0.1:9102".to_string(),
            target_port: Some(5678),
            updated_at: 0,
        };
        let runtime = ServiceStatusSnapshot {
            status: "Listening".to_string(),
            message: None,
        };
        let backend = ServiceBackendState {
            deployment_name: "echo-edge".to_string(),
            current_node_name: Some("server77".to_string()),
            current_upstream: Some("127.0.0.1:8092".to_string()),
            health: ServiceBackendHealth::Degraded,
            message: Some("node `server77` is Offline".to_string()),
        };

        let state = effective_service_state(&service, Some(&runtime), backend);
        assert_eq!(state.status, "Degraded");
        assert_eq!(
            state.message.as_deref(),
            Some("node `server77` is Offline; selector role=edge")
        );
    }

    #[test]
    fn drain_moves_workload_to_alternate_schedulable_node() {
        let deployment = stored_deployment("demo", "local", None, BTreeMap::new());
        let nodes = vec![
            local_node(true),
            ssh_node("server77", false, BTreeMap::new()),
        ];

        let decision = drain_target_for_deployment(&deployment, &nodes, "local");
        assert_eq!(decision, DrainDecision::Move("server77".to_string()));
    }

    #[test]
    fn drain_blocks_workload_pinned_by_nodename() {
        let deployment = stored_deployment("demo", "server77", Some("server77"), BTreeMap::new());
        let nodes = vec![
            local_node(false),
            ssh_node("server77", true, BTreeMap::new()),
        ];

        let decision = drain_target_for_deployment(&deployment, &nodes, "server77");
        assert!(matches!(decision, DrainDecision::Blocked(message) if message.contains("pinned")));
    }

    #[test]
    fn drain_skips_workloads_not_assigned_to_the_node() {
        let deployment = stored_deployment("demo", "server77", None, BTreeMap::new());
        let nodes = vec![
            local_node(false),
            ssh_node("server77", false, BTreeMap::new()),
        ];

        let decision = drain_target_for_deployment(&deployment, &nodes, "local");
        assert_eq!(decision, DrainDecision::Skip);
    }

    fn stored_deployment(
        name: &str,
        assigned_node: &str,
        explicit_node_name: Option<&str>,
        node_selector: BTreeMap<String, String>,
    ) -> StoredDeployment {
        let node_name_yaml = explicit_node_name
            .map(|value| format!("      nodeName: {value}\n"))
            .unwrap_or_default();
        let node_selector_yaml = if node_selector.is_empty() {
            String::new()
        } else {
            let selector_lines = node_selector
                .iter()
                .map(|(key, value)| format!("        {key}: {value}\n"))
                .collect::<String>();
            format!("      nodeSelector:\n{selector_lines}")
        };
        let manifest = DeploymentManifest::parse(&format!(
            r#"
apiVersion: {API_VERSION}
kind: Deployment
metadata:
  name: {name}
spec:
  replicas: 1
  template:
    spec:
{node_name_yaml}{node_selector_yaml}      restartPolicy: Always
      containers:
        - name: app
          image: alpine:3.20
          command: ["sh"]
          args: ["-c", "sleep 3600"]
"#
        ))
        .expect("manifest should parse");

        StoredDeployment {
            manifest,
            spec_hash: "hash".to_string(),
            assigned_node: assigned_node.to_string(),
            status: DeploymentStatus::Running,
            ready: true,
            container_id: Some("abc".to_string()),
            message: None,
            restart_count: 0,
            last_exit: None,
            restart_streak: 0,
            next_retry_at: None,
            healthy_since: None,
            container_started_at: None,
            ready_since: None,
            updated_at: 0,
        }
    }

    fn local_node(unschedulable: bool) -> ClusterNode {
        let mut node = ClusterNode::local();
        node.unschedulable = unschedulable;
        node
    }

    fn ssh_node(name: &str, unschedulable: bool, labels: BTreeMap<String, String>) -> ClusterNode {
        ClusterNode {
            name: name.to_string(),
            labels,
            unschedulable,
            transport: NodeTransport::Ssh {
                target: name.to_string(),
                remote_binary: "~/.local/bin/litek8s".to_string(),
                identity_file: None,
            },
            vpn: None,
        }
    }
}
