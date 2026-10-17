use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use crate::control_plane::raft::ControlPlaneNode;
use crate::control_plane::{ClusterCommand, CommandResult};
use crate::node::{ClusterNode, NodeClient, select_nodes_for_manifest};
use crate::runtime::LocalRuntime;
use crate::runtime::docker::{
    ContainerExitState, ManagedContainer, replica_container_name, summarize_exit_state,
};
use crate::store::{DeploymentStatus, EventLevel, ReplicaState, Store, StoredDeployment};

const RECREATE_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_RESTART_BACKOFF_SECS: u64 = 15;
/// How long after creation a `created`/`restarting` container is still
/// considered to be starting rather than stuck.
const STARTUP_GRACE: Duration = Duration::from_secs(10);
/// How long a Ready replica whose container is unchanged goes without a fresh
/// TCP readiness probe. A new container is always probed on the next pass.
const READY_PROBE_INTERVAL: Duration = Duration::from_secs(10);

/// Last successful readiness probe per replica, keyed by (deployment, ordinal)
/// and tied to the container it probed. Lets a steady Ready replica skip the
/// probe on most passes. In memory only: after a daemon restart every replica
/// is probed again on the first pass.
#[derive(Default)]
pub struct ReadinessCache {
    last_success: Mutex<HashMap<(String, u32), (String, Instant)>>,
}

impl ReadinessCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<(String, u32), (String, Instant)>> {
        self.last_success
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn is_fresh(&self, name: &str, ordinal: u32, container_id: &str) -> bool {
        self.entries()
            .get(&(name.to_string(), ordinal))
            .is_some_and(|(id, at)| id == container_id && at.elapsed() < READY_PROBE_INTERVAL)
    }

    fn record_success(&self, name: &str, ordinal: u32, container_id: &str) {
        self.entries().insert(
            (name.to_string(), ordinal),
            (container_id.to_string(), Instant::now()),
        );
    }

    fn forget(&self, name: &str, ordinal: u32) {
        self.entries().remove(&(name.to_string(), ordinal));
    }

    fn retain_deployments(&self, names: &HashSet<String>) {
        self.entries().retain(|(name, _), _| names.contains(name));
    }
}

pub async fn reconcile_all(
    store: &Store,
    ha_node: &ControlPlaneNode,
    readiness: &ReadinessCache,
    local_runtime: &LocalRuntime,
    nodes: &[ClusterNode],
    cleanup_orphans: bool,
) -> Result<()> {
    let deployments = store.list().await;
    let nodes_by_name: HashMap<String, ClusterNode> = nodes
        .iter()
        .cloned()
        .map(|node| (node.name.clone(), node))
        .collect();

    // Plan node placement for every replica ordinal of every deployment, and
    // record the desired CONTAINER names per node so orphan cleanup can remove
    // scaled-down ordinals (and whole deleted deployments).
    let mut planned: HashMap<String, Vec<String>> = HashMap::new();
    let mut desired_names_by_node: HashMap<String, HashSet<String>> = HashMap::new();
    for deployment in &deployments {
        let name = deployment.manifest.deployment_name();
        let desired = deployment.manifest.desired_replicas();
        let current = current_nodes_by_ordinal(deployment, desired);
        match select_nodes_for_manifest(&deployment.manifest, nodes, &current, desired) {
            Ok(target_nodes) => {
                for (ordinal, node) in target_nodes.iter().enumerate() {
                    let entry = desired_names_by_node.entry(node.clone()).or_default();
                    entry.insert(replica_container_name(name, ordinal as u32));
                    // Pre-multi-replica containers are named `litek8s-<dep>`
                    // (no ordinal). Keep the bare name desired for ordinal 0 so
                    // an upgraded daemon doesn't orphan the adopted container
                    // before it's recreated with the new name.
                    if ordinal == 0 {
                        entry.insert(crate::runtime::docker::deployment_container_name(name));
                    }
                }
                planned.insert(name.to_string(), target_nodes);
            }
            Err(_) => {
                // Scheduling failure is surfaced per-deployment in the loop below.
            }
        }
    }

    // Only deployments this daemon owns are eligible for orphan cleanup, so a
    // container belonging to another deployment (e.g. another daemon sharing the
    // Docker host) is never reaped here.
    let known_deployments: HashSet<String> = deployments
        .iter()
        .map(|d| d.manifest.deployment_name().to_string())
        .collect();
    readiness.retain_deployments(&known_deployments);

    // One container list per node per pass, shared by orphan cleanup and every
    // replica on that node (instead of one list per replica). A failed list is
    // kept as the error each replica on that node reports.
    let mut containers_by_node: HashMap<String, std::result::Result<Vec<ManagedContainer>, String>> =
        HashMap::new();
    for node in nodes {
        if !cleanup_orphans && !desired_names_by_node.contains_key(&node.name) {
            continue;
        }
        let runtime = NodeClient::for_node(node, local_runtime.clone());
        let listed = runtime
            .list_managed_containers()
            .await
            .map_err(|error| format!("{error:#}"));
        containers_by_node.insert(node.name.clone(), listed);
    }

    if cleanup_orphans {
        let no_names = HashSet::new();
        for node in nodes {
            let containers = match containers_by_node.get_mut(&node.name) {
                Some(Ok(containers)) => containers,
                Some(Err(error)) => {
                    eprintln!(
                        "skipping orphan cleanup on node `{}` because it is unavailable: {error}",
                        node.name
                    );
                    continue;
                }
                None => continue,
            };
            let runtime = NodeClient::for_node(node, local_runtime.clone());
            let desired_names = desired_names_by_node.get(&node.name).unwrap_or(&no_names);
            let orphans: Vec<String> = containers
                .iter()
                .filter(|c| {
                    known_deployments.contains(&c.deployment) && !desired_names.contains(&c.name)
                })
                .map(|c| c.id.clone())
                .collect();
            let mut removed = HashSet::new();
            for id in orphans {
                if let Err(error) = runtime.remove_container(&id).await {
                    eprintln!(
                        "skipping orphan cleanup on node `{}` because it is unavailable: {error}",
                        node.name
                    );
                    break;
                }
                removed.insert(id);
            }
            containers.retain(|c| !removed.contains(&c.id));
        }
    }

    for deployment in deployments {
        let name = deployment.manifest.deployment_name().to_string();
        let desired = deployment.manifest.desired_replicas();

        let target_nodes = match planned.get(&name) {
            Some(target_nodes) => target_nodes.clone(),
            None => {
                // Scheduling failed; mark every replica failed and stop.
                let current = current_nodes_by_ordinal(&deployment, desired);
                let error = select_nodes_for_manifest(&deployment.manifest, nodes, &current, desired)
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "no schedulable node".to_string());
                for ordinal in 0..desired {
                    propose_if_changed(
                        store,
                        ha_node,
                        ClusterCommand::SetReplicaStatus {
                            name: name.clone(),
                            ordinal,
                            status: DeploymentStatus::Failed,
                            ready: false,
                            container_id: None,
                            message: Some(error.clone()),
                            last_exit: None,
                            increment_restart_count: false,
                            increment_restart_streak: false,
                            next_retry_at: None,
                        },
                    )
                    .await?;
                }
                record_deployment_event(
                    ha_node,
                    EventLevel::Warning,
                    "FailedScheduling",
                    &name,
                    None,
                    error,
                )
                .await?;
                continue;
            }
        };

        // Scale down: remove any container/replica with ordinal >= desired,
        // highest first so survivors never get renumbered.
        scale_down_extras(ha_node, local_runtime, &nodes_by_name, &deployment, desired).await?;

        // Reconcile each desired ordinal on its target node.
        for ordinal in 0..desired {
            let target_node = &target_nodes[ordinal as usize];
            let replica = replica_for_ordinal(&deployment, ordinal);

            // Reschedule: if the replica moved nodes, clean it off the old one.
            if let Some(replica) = &replica {
                if !replica.assigned_node.is_empty() && &replica.assigned_node != target_node {
                    if let Some(previous) = nodes_by_name.get(&replica.assigned_node) {
                        let previous_runtime =
                            NodeClient::for_node(previous, local_runtime.clone());
                        let _ = previous_runtime
                            .remove_replica_container(&name, ordinal)
                            .await;
                    }
                }
            }
            propose_if_changed(
                store,
                ha_node,
                ClusterCommand::SetReplicaAssignment {
                    name: name.clone(),
                    ordinal,
                    assigned_node: target_node.clone(),
                },
            )
            .await?;

            let Some(node) = nodes_by_name.get(target_node) else {
                continue;
            };
            let runtime = NodeClient::for_node(node, local_runtime.clone());
            let outcome = match containers_by_node.get(target_node) {
                Some(Ok(containers)) => {
                    reconcile_replica(
                        store,
                        ha_node,
                        readiness,
                        &runtime,
                        &deployment,
                        ordinal,
                        replica.as_ref(),
                        containers,
                    )
                    .await
                }
                Some(Err(error)) => Err(anyhow!("{error}")),
                None => Err(anyhow!("containers on node `{target_node}` were not listed")),
            };
            if let Err(error) = outcome {
                readiness.forget(&name, ordinal);
                let changed = propose_if_changed(
                    store,
                    ha_node,
                    ClusterCommand::SetReplicaStatus {
                        name: name.clone(),
                        ordinal,
                        status: DeploymentStatus::Failed,
                        ready: false,
                        container_id: None,
                        message: Some(error.to_string()),
                        last_exit: None,
                        increment_restart_count: false,
                        increment_restart_streak: false,
                        next_retry_at: None,
                    },
                )
                .await?;
                if changed {
                    record_deployment_event(
                        ha_node,
                        EventLevel::Warning,
                        "Failed",
                        &name,
                        Some(target_node.clone()),
                        error.to_string(),
                    )
                    .await?;
                }
            }
        }
    }

    Ok(())
}

/// The node each ordinal currently runs on, as a Vec indexed by ordinal (for
/// scheduler stickiness). Missing ordinals get an empty string.
fn current_nodes_by_ordinal(deployment: &StoredDeployment, desired: u32) -> Vec<String> {
    (0..desired)
        .map(|ordinal| {
            deployment
                .replicas
                .iter()
                .find(|r| r.ordinal == ordinal)
                .map(|r| r.assigned_node.clone())
                .unwrap_or_default()
        })
        .collect()
}

fn replica_for_ordinal(deployment: &StoredDeployment, ordinal: u32) -> Option<ReplicaState> {
    deployment
        .replicas
        .iter()
        .find(|r| r.ordinal == ordinal)
        .cloned()
}

/// Remove containers and replica records whose ordinal is >= the desired count.
/// Highest ordinal first, so identities below `desired` are never disturbed.
async fn scale_down_extras(
    ha_node: &ControlPlaneNode,
    local_runtime: &LocalRuntime,
    nodes_by_name: &HashMap<String, ClusterNode>,
    deployment: &StoredDeployment,
    desired: u32,
) -> Result<()> {
    let name = deployment.manifest.deployment_name();
    let mut extras: Vec<&ReplicaState> = deployment
        .replicas
        .iter()
        .filter(|r| r.ordinal >= desired)
        .collect();
    extras.sort_by(|a, b| b.ordinal.cmp(&a.ordinal));
    for replica in extras {
        if let Some(node) = nodes_by_name.get(&replica.assigned_node) {
            let runtime = NodeClient::for_node(node, local_runtime.clone());
            let _ = runtime
                .remove_replica_container(name, replica.ordinal)
                .await;
        }
        let _ = ha_node
            .propose(ClusterCommand::RemoveReplica {
                name: name.to_string(),
                ordinal: replica.ordinal,
            })
            .await?;
        record_deployment_event(
            ha_node,
            EventLevel::Normal,
            "ScaledDown",
            name,
            Some(replica.assigned_node.clone()),
            format!("removed replica {}", replica.ordinal),
        )
        .await?;
    }
    Ok(())
}

/// Propose `command` unless the store says it would not change anything.
/// Every pass re-asserts each replica's assignment and status; skipping the
/// unchanged ones keeps steady-state passes out of the Raft log. Returns
/// whether the store changed.
async fn propose_if_changed(
    store: &Store,
    ha_node: &ControlPlaneNode,
    command: ClusterCommand,
) -> Result<bool> {
    let would_change = match &command {
        ClusterCommand::SetReplicaAssignment {
            name,
            ordinal,
            assigned_node,
        } => {
            store
                .replica_assignment_would_change(name, *ordinal, assigned_node)
                .await
        }
        ClusterCommand::SetReplicaStatus {
            name,
            ordinal,
            status,
            ready,
            container_id,
            message,
            last_exit,
            increment_restart_count,
            increment_restart_streak,
            next_retry_at,
        } => {
            store
                .replica_status_would_change(
                    name,
                    *ordinal,
                    status,
                    *ready,
                    container_id.as_deref(),
                    message.as_deref(),
                    last_exit.as_deref(),
                    *increment_restart_count,
                    *increment_restart_streak,
                    *next_retry_at,
                )
                .await
        }
        _ => true,
    };
    if !would_change {
        return Ok(false);
    }
    let result = ha_node.propose(command).await?;
    Ok(matches!(result, CommandResult::Changed { changed: true }))
}

/// Reconcile a single replica (ordinal) on its assigned node's runtime.
/// `node_containers` is this pass's list of managed containers on that node.
#[allow(clippy::too_many_arguments)]
pub async fn reconcile_replica(
    store: &Store,
    ha_node: &ControlPlaneNode,
    readiness: &ReadinessCache,
    runtime: &NodeClient,
    deployment: &StoredDeployment,
    ordinal: u32,
    replica: Option<&ReplicaState>,
    node_containers: &[ManagedContainer],
) -> Result<()> {
    let name = deployment.manifest.deployment_name();
    let cname = replica_container_name(name, ordinal);

    // Find this ordinal's container(s). Pre-upgrade containers (ordinal label
    // absent) are adopted as ordinal 0.
    let mut containers: Vec<_> = node_containers
        .iter()
        .filter(|c| c.deployment == name)
        .filter(|c| c.ordinal == Some(ordinal) || (ordinal == 0 && c.ordinal.is_none()))
        .cloned()
        .collect();

    // Collapse duplicates for the same ordinal down to one, keeping a healthy
    // container (preferring the one this replica already tracks) so a
    // duplicate never churns the replica's container ID.
    if containers.len() > 1 {
        let tracked_id = replica.and_then(|r| r.container_id.as_deref());
        containers.sort_by_key(|c| {
            (
                !(c.running && c.spec_hash.as_deref() == Some(deployment.spec_hash.as_str())),
                Some(c.id.as_str()) != tracked_id,
                c.id.clone(),
            )
        });
        for extra in &containers[1..] {
            runtime.remove_container(&extra.id).await?;
        }
        containers.truncate(1);
    }

    if let Some(container) = containers.into_iter().next() {
        let hash_matches = container.spec_hash.as_deref() == Some(deployment.spec_hash.as_str());
        if hash_matches && container.running {
            let (ready, message, event_reason) =
                if let Some(probe) = deployment.manifest.container().readiness_probe() {
                    let probe_target = format!("tcpSocket:{}", probe.tcp_socket.port);
                    let started_at = replica
                        .and_then(|r| r.container_started_at)
                        .unwrap_or_else(now_unix);
                    let initial_delay = probe.initial_delay_seconds();
                    if initial_delay > 0 && now_unix().saturating_sub(started_at) < initial_delay {
                        (
                            false,
                            format!(
                                "waiting {}s before readiness probe begins on {probe_target}",
                                initial_delay
                            ),
                            Some("NotReady"),
                        )
                    } else if replica.is_some_and(|r| {
                        r.ready && r.container_id.as_deref() == Some(container.id.as_str())
                    }) && readiness.is_fresh(name, ordinal, &container.id)
                    {
                        // Ready on this same container and probed recently:
                        // skip the probe until READY_PROBE_INTERVAL lapses.
                        (
                            true,
                            format!("readiness probe succeeded on {probe_target}"),
                            Some("Ready"),
                        )
                    } else {
                        match runtime
                            .probe_container_tcp(
                                &container.id,
                                probe.tcp_socket.port,
                                Duration::from_secs(probe.timeout_seconds()),
                            )
                            .await
                        {
                            Ok(()) => {
                                readiness.record_success(name, ordinal, &container.id);
                                (
                                    true,
                                    format!("readiness probe succeeded on {probe_target}"),
                                    Some("Ready"),
                                )
                            }
                            Err(_) => {
                                readiness.forget(name, ordinal);
                                (
                                    false,
                                    format!("readiness probe is still failing on {probe_target}"),
                                    Some("NotReady"),
                                )
                            }
                        }
                    }
                } else {
                    (true, "container is healthy".to_string(), Some("Running"))
                };
            let changed = propose_if_changed(
                store,
                ha_node,
                ClusterCommand::SetReplicaStatus {
                    name: name.to_string(),
                    ordinal,
                    status: DeploymentStatus::Running,
                    ready,
                    container_id: Some(container.id),
                    message: Some(message.clone()),
                    last_exit: None,
                    increment_restart_count: false,
                    increment_restart_streak: false,
                    next_retry_at: None,
                },
            )
            .await?;
            if changed {
                record_deployment_event(
                    ha_node,
                    EventLevel::Normal,
                    event_reason.unwrap_or("Running"),
                    name,
                    Some(deployment.assigned_node.clone()),
                    format!("replica {ordinal}: {message}"),
                )
                .await?;
            }
            return Ok(());
        }

        let last_exit = if !hash_matches {
            Some("spec changed".to_string())
        } else if container.running {
            Some("runtime drift".to_string())
        } else {
            let exit_state = runtime.inspect_exit_state(&container.id).await.ok();
            if exit_state
                .as_ref()
                .is_some_and(|state| is_transient_not_running(state, container.created_at))
            {
                // Not an exit: the list was stale or the container is still
                // starting. Leave it alone; a later pass will see it settle.
                return Ok(());
            }
            exit_state
                .as_ref()
                .and_then(summarize_exit_state)
                .or_else(|| Some("container exited".to_string()))
        };

        runtime.remove_container(&container.id).await?;
        wait_until_container_gone(runtime, name, &cname, ordinal).await?;

        if hash_matches {
            let streak = replica.map(|r| r.restart_streak).unwrap_or(0);
            let backoff = restart_backoff_for_streak(streak.saturating_add(1));
            let next_retry_at = now_unix().saturating_add(backoff.as_secs());
            let reason = last_exit
                .clone()
                .unwrap_or_else(|| "container exited".to_string());
            let changed = propose_if_changed(
                store,
                ha_node,
                ClusterCommand::SetReplicaStatus {
                    name: name.to_string(),
                    ordinal,
                    status: DeploymentStatus::Restarting,
                    ready: false,
                    container_id: None,
                    message: Some(format!(
                        "backing off {}s before retry after {reason}",
                        backoff.as_secs()
                    )),
                    last_exit,
                    increment_restart_count: true,
                    increment_restart_streak: true,
                    next_retry_at: Some(next_retry_at),
                },
            )
            .await?;
            if changed {
                record_deployment_event(
                    ha_node,
                    EventLevel::Warning,
                    "Backoff",
                    name,
                    Some(deployment.assigned_node.clone()),
                    format!("replica {ordinal}: backing off {}s after {reason}", backoff.as_secs()),
                )
                .await?;
            }
            return Ok(());
        }

        // Spec changed: recreate immediately.
        let new_id = runtime
            .create_and_start(&deployment.manifest, &deployment.spec_hash, ordinal)
            .await?;
        let changed = propose_if_changed(
            store,
            ha_node,
            ClusterCommand::SetReplicaStatus {
                name: name.to_string(),
                ordinal,
                status: DeploymentStatus::Restarting,
                ready: false,
                container_id: Some(new_id),
                message: Some("container was recreated after the manifest changed".to_string()),
                last_exit,
                increment_restart_count: true,
                increment_restart_streak: false,
                next_retry_at: None,
            },
        )
        .await?;
        if changed {
            record_deployment_event(
                ha_node,
                EventLevel::Normal,
                "Recreated",
                name,
                Some(deployment.assigned_node.clone()),
                format!("replica {ordinal} recreated after the manifest changed"),
            )
            .await?;
        }
        return Ok(());
    }

    // No container for this ordinal: respect this replica's backoff.
    if let Some(next_retry_at) = replica.and_then(|r| r.next_retry_at) {
        if now_unix() < next_retry_at {
            return Ok(());
        }
    }

    let had_container = replica.and_then(|r| r.container_id.clone()).is_some();
    let retried_after_backoff = replica.and_then(|r| r.next_retry_at).is_some();
    let new_id = runtime
        .create_and_start(&deployment.manifest, &deployment.spec_hash, ordinal)
        .await?;
    let status = if had_container || retried_after_backoff {
        DeploymentStatus::Restarting
    } else {
        DeploymentStatus::Running
    };
    let message = if retried_after_backoff {
        "container restarted after backoff"
    } else if had_container {
        "container was recreated after disappearing"
    } else {
        "container was created"
    };

    let changed = propose_if_changed(
        store,
        ha_node,
        ClusterCommand::SetReplicaStatus {
            name: name.to_string(),
            ordinal,
            status,
            ready: false,
            container_id: Some(new_id),
            message: Some(message.to_string()),
            last_exit: if had_container {
                Some("container disappeared".to_string())
            } else {
                None
            },
            increment_restart_count: had_container,
            increment_restart_streak: false,
            next_retry_at: None,
        },
    )
    .await?;
    if changed {
        record_deployment_event(
            ha_node,
            EventLevel::Normal,
            if retried_after_backoff {
                "Restarted"
            } else if had_container {
                "Recreated"
            } else {
                "Started"
            },
            name,
            Some(deployment.assigned_node.clone()),
            format!("replica {ordinal}: {message}"),
        )
        .await?;
    }
    Ok(())
}

/// Wait until no container with the given replica name remains on the node.
async fn wait_until_container_gone(
    runtime: &NodeClient,
    deployment: &str,
    cname: &str,
    ordinal: u32,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + RECREATE_WAIT_TIMEOUT;
    loop {
        let still_present = runtime
            .list_containers_for_deployment(deployment)
            .await?
            .into_iter()
            .any(|c| {
                c.name == cname || c.ordinal == Some(ordinal) || (ordinal == 0 && c.ordinal.is_none())
            });
        if !still_present {
            return Ok(());
        }
        if tokio::time::Instant::now() > deadline {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Whether a hash-matching container that the list reported as not running is
/// only transiently so, and must not be treated as an exit. Under load Docker
/// can list a healthy container before its state settles: inspect says it is
/// actually `running` (stale list), or it is still `created`/`restarting`
/// shortly after creation. Past the startup grace window those states count as
/// stuck, so a container whose start failed is still replaced.
fn is_transient_not_running(exit_state: &ContainerExitState, created_at: Option<i64>) -> bool {
    match exit_state.status.as_deref() {
        Some("running") => true,
        Some("created" | "restarting") => created_at.is_some_and(|created_at| {
            (now_unix() as i64).saturating_sub(created_at) < STARTUP_GRACE.as_secs() as i64
        }),
        _ => false,
    }
}

fn restart_backoff_for_streak(streak: u32) -> Duration {
    let streak = streak.max(1);
    let exponential = 1u64
        .checked_shl(streak.saturating_sub(1))
        .unwrap_or(u64::MAX);
    Duration::from_secs(exponential.min(MAX_RESTART_BACKOFF_SECS))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn record_deployment_event(
    ha_node: &ControlPlaneNode,
    level: EventLevel,
    reason: impl Into<String>,
    deployment_name: &str,
    node_name: Option<String>,
    message: impl Into<String>,
) -> Result<()> {
    ha_node
        .propose(ClusterCommand::AppendEvent {
            level,
            reason: reason.into(),
            object_kind: "deployment".to_string(),
            object_name: deployment_name.to_string(),
            node_name,
            message: message.into(),
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{ReadinessCache, reconcile_all, reconcile_replica, restart_backoff_for_streak};
    use crate::control_plane::ClusterCommand;
    use crate::control_plane::raft::ControlPlaneNode;
    use crate::node::{ClusterNode, NodeClient};
    use crate::runtime::{ContainerRuntime, FakeRuntime, LocalRuntime};
    use crate::store::{DeploymentStatus, ReplicaState, Store, StoredDeployment};

    struct Harness {
        store: Arc<Store>,
        ha_node: ControlPlaneNode,
        fake: Arc<FakeRuntime>,
        client: NodeClient,
        readiness: ReadinessCache,
        _tmp: tempfile::TempDir,
    }

    const WEB_MANIFEST: &str = r#"apiVersion: litek8s.dev/v1alpha1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 1
  template:
    spec:
      restartPolicy: Always
      containers:
        - name: app
          image: alpine:latest
"#;

    const PROBED_MANIFEST: &str = r#"apiVersion: litek8s.dev/v1alpha1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 3
  template:
    spec:
      restartPolicy: Always
      containers:
        - name: app
          image: alpine:latest
          ports:
            - containerPort: 80
          readinessProbe:
            tcpSocket:
              port: 80
            initialDelaySeconds: 0
            timeoutSeconds: 1
"#;

    impl Harness {
        async fn new() -> Self {
            Self::with_manifest(WEB_MANIFEST).await
        }

        async fn with_manifest(manifest_yaml: &str) -> Self {
            let tmp = tempfile::TempDir::new().unwrap();
            let store = Arc::new(Store::load(tmp.path().join("state.json")).await.unwrap());
            let ha_node =
                ControlPlaneNode::start(1, "127.0.0.1:0".into(), Arc::clone(&store), None)
                    .await
                    .unwrap();
            ha_node
                .propose(ClusterCommand::ApplyDeployment {
                    manifest_yaml: manifest_yaml.to_string(),
                })
                .await
                .unwrap();
            let fake = Arc::new(FakeRuntime::new());
            let client =
                NodeClient::for_node(&ClusterNode::local(), LocalRuntime::Fake(fake.clone()));
            Self {
                store,
                ha_node,
                fake,
                client,
                readiness: ReadinessCache::new(),
                _tmp: tmp,
            }
        }

        async fn deployment(&self) -> StoredDeployment {
            self.store.get("web").await.expect("deployment stored")
        }

        async fn create_replica(&self) -> String {
            let deployment = self.deployment().await;
            self.fake
                .create_and_start(&deployment.manifest, &deployment.spec_hash, 0)
                .await
                .unwrap()
        }

        async fn reconcile(&self, replica: Option<&ReplicaState>) {
            let deployment = self.deployment().await;
            let containers = self.fake.list_managed_containers().await.unwrap();
            reconcile_replica(
                &self.store,
                &self.ha_node,
                &self.readiness,
                &self.client,
                &deployment,
                0,
                replica,
                &containers,
            )
            .await
            .unwrap();
        }

        /// One full reconcile pass over the local node.
        async fn pass(&self) {
            let runtime = LocalRuntime::Fake(self.fake.clone());
            reconcile_all(
                &self.store,
                &self.ha_node,
                &self.readiness,
                &runtime,
                &[ClusterNode::local()],
                true,
            )
            .await
            .unwrap();
        }

        fn raft_log_index(&self) -> Option<u64> {
            self.ha_node.raft.metrics().borrow().last_log_index
        }

        async fn container_ids(&self) -> Vec<String> {
            self.fake
                .list_containers_for_deployment("web")
                .await
                .unwrap()
                .into_iter()
                .map(|c| c.id)
                .collect()
        }
    }

    fn mark_not_running(fake: &FakeRuntime, id: &str, inspected_status: &str) {
        fake.set_container_state(id, |state| {
            state.running = false;
            state.state = Some(inspected_status.to_string());
            state.exit.status = Some(inspected_status.to_string());
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn freshly_created_container_is_not_churned() {
        let h = Harness::new().await;
        let id = h.create_replica().await;
        mark_not_running(&h.fake, &id, "created");

        h.reconcile(None).await;

        assert_eq!(h.fake.remove_calls(), 0);
        assert_eq!(h.container_ids().await, vec![id]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_list_of_running_container_is_not_churned() {
        let h = Harness::new().await;
        let id = h.create_replica().await;
        // The list says not running but inspect says running.
        h.fake.set_container_state(&id, |state| {
            state.running = false;
            state.state = Some("exited".to_string());
        });

        h.reconcile(None).await;

        assert_eq!(h.fake.remove_calls(), 0);
        assert_eq!(h.container_ids().await, vec![id]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn container_stuck_in_created_is_replaced_after_grace() {
        let h = Harness::new().await;
        let id = h.create_replica().await;
        mark_not_running(&h.fake, &id, "created");
        h.fake.set_created_at(&id, 0);

        h.reconcile(None).await;

        assert_eq!(h.fake.remove_calls(), 1);
        assert!(h.container_ids().await.is_empty());
        let replica = h.deployment().await.replicas[0].clone();
        assert_eq!(replica.status, DeploymentStatus::Restarting);
        assert!(
            replica.next_retry_at.is_some(),
            "exit path keeps restart backoff"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exited_container_is_removed_with_backoff() {
        let h = Harness::new().await;
        let id = h.create_replica().await;
        h.fake.set_container_state(&id, |state| {
            state.running = false;
            state.state = Some("exited".to_string());
            state.exit.status = Some("exited".to_string());
            state.exit.exit_code = Some(1);
        });

        h.reconcile(None).await;

        assert_eq!(h.fake.remove_calls(), 1);
        let replica = h.deployment().await.replicas[0].clone();
        assert_eq!(replica.status, DeploymentStatus::Restarting);
        assert_eq!(replica.restart_streak, 1);
        assert_eq!(replica.last_exit.as_deref(), Some("exited(1)"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_collapse_keeps_tracked_container() {
        let h = Harness::new().await;
        let first = h.create_replica().await;
        let tracked = h.create_replica().await;
        let replica = ReplicaState {
            ordinal: 0,
            container_id: Some(tracked.clone()),
            ..ReplicaState::default()
        };

        h.reconcile(Some(&replica)).await;

        assert_eq!(h.container_ids().await, vec![tracked]);
        assert_ne!(first, h.container_ids().await[0]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn steady_state_pass_lists_once_and_proposes_nothing() {
        let h = Harness::with_manifest(PROBED_MANIFEST).await;
        // Converge: create, then probe Ready, then settle.
        for _ in 0..4 {
            h.pass().await;
        }
        let replicas = h.deployment().await.replicas;
        assert_eq!(replicas.len(), 3);
        assert!(replicas.iter().all(|r| r.ready), "{replicas:?}");

        let lists = h.fake.list_calls();
        let log_index = h.raft_log_index();
        h.pass().await;

        assert_eq!(
            h.fake.list_calls() - lists,
            1,
            "one container list per node per pass, shared by 3 replicas and orphan cleanup"
        );
        assert_eq!(
            h.raft_log_index(),
            log_index,
            "an unchanged pass proposes nothing"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ready_replica_probe_backs_off_until_container_changes() {
        let h = Harness::with_manifest(PROBED_MANIFEST).await;
        h.pass().await; // create
        h.pass().await; // first probe -> Ready
        assert_eq!(h.fake.probe_calls(), 3);
        assert!(h.deployment().await.replicas.iter().all(|r| r.ready));

        h.pass().await;
        h.pass().await;
        assert_eq!(
            h.fake.probe_calls(),
            3,
            "Ready replicas on unchanged containers are not re-probed"
        );

        // Replace replica 0's container: the new one is probed right away.
        let old = h.deployment().await.replicas[0].container_id.clone().unwrap();
        h.fake.remove_container(&old).await.unwrap();
        h.pass().await; // recreate
        h.pass().await; // probe the new container
        assert_eq!(h.fake.probe_calls(), 4);
        let replica = h.deployment().await.replicas[0].clone();
        assert!(replica.ready);
        assert_ne!(replica.container_id.as_deref(), Some(old.as_str()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failing_probe_is_retried_every_pass() {
        let h = Harness::with_manifest(PROBED_MANIFEST).await;
        h.pass().await;
        for replica in h.deployment().await.replicas {
            h.fake
                .set_container_state(replica.container_id.as_deref().unwrap(), |state| {
                    state.probe_succeeds = false;
                });
        }
        h.pass().await;
        h.pass().await;
        assert_eq!(h.fake.probe_calls(), 6);
        assert!(h.deployment().await.replicas.iter().all(|r| !r.ready));
    }

    #[test]
    fn caps_restart_backoff() {
        assert_eq!(restart_backoff_for_streak(1).as_secs(), 1);
        assert_eq!(restart_backoff_for_streak(2).as_secs(), 2);
        assert_eq!(restart_backoff_for_streak(3).as_secs(), 4);
        assert_eq!(restart_backoff_for_streak(4).as_secs(), 8);
        assert_eq!(restart_backoff_for_streak(5).as_secs(), 15);
        assert_eq!(restart_backoff_for_streak(6).as_secs(), 15);
    }
}
