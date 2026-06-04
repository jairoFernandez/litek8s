use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::Result;

use crate::control_plane::raft::ControlPlaneNode;
use crate::control_plane::{ClusterCommand, CommandResult};
use crate::node::{ClusterNode, NodeClient, select_nodes_for_manifest};
use crate::runtime::LocalRuntime;
use crate::runtime::docker::{replica_container_name, summarize_exit_state};
use crate::store::{DeploymentStatus, EventLevel, ReplicaState, Store, StoredDeployment};

const RECREATE_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_RESTART_BACKOFF_SECS: u64 = 15;

pub async fn reconcile_all(
    store: &Store,
    ha_node: &ControlPlaneNode,
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

    if cleanup_orphans {
        for node in nodes {
            let runtime = NodeClient::for_node(node, local_runtime.clone());
            let desired_names = desired_names_by_node
                .get(&node.name)
                .cloned()
                .unwrap_or_default();
            if let Err(error) = runtime
                .remove_orphans(&desired_names, &known_deployments)
                .await
            {
                eprintln!(
                    "skipping orphan cleanup on node `{}` because it is unavailable: {error}",
                    node.name
                );
            }
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
                    let _ = ha_node
                        .propose(ClusterCommand::SetReplicaStatus {
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
                        })
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
            let _ = ha_node
                .propose(ClusterCommand::SetReplicaAssignment {
                    name: name.clone(),
                    ordinal,
                    assigned_node: target_node.clone(),
                })
                .await?;

            let Some(node) = nodes_by_name.get(target_node) else {
                continue;
            };
            let runtime = NodeClient::for_node(node, local_runtime.clone());
            if let Err(error) =
                reconcile_replica(ha_node, &runtime, &deployment, ordinal, replica.as_ref()).await
            {
                let result = ha_node
                    .propose(ClusterCommand::SetReplicaStatus {
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
                    })
                    .await?;
                if matches!(result, CommandResult::Changed { changed: true }) {
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

/// Reconcile a single replica (ordinal) on its assigned node's runtime.
pub async fn reconcile_replica(
    ha_node: &ControlPlaneNode,
    runtime: &NodeClient,
    deployment: &StoredDeployment,
    ordinal: u32,
    replica: Option<&ReplicaState>,
) -> Result<()> {
    let name = deployment.manifest.deployment_name();
    let cname = replica_container_name(name, ordinal);

    // Find this ordinal's container(s). Pre-upgrade containers (ordinal label
    // absent) are adopted as ordinal 0.
    let mut containers: Vec<_> = runtime
        .list_containers_for_deployment(name)
        .await?
        .into_iter()
        .filter(|c| c.ordinal == Some(ordinal) || (ordinal == 0 && c.ordinal.is_none()))
        .collect();

    // Collapse duplicates for the same ordinal down to one.
    if containers.len() > 1 {
        containers.sort_by(|a, b| a.id.cmp(&b.id));
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
                    } else {
                        match runtime
                            .probe_container_tcp(
                                &container.id,
                                probe.tcp_socket.port,
                                Duration::from_secs(probe.timeout_seconds()),
                            )
                            .await
                        {
                            Ok(()) => (
                                true,
                                format!("readiness probe succeeded on {probe_target}"),
                                Some("Ready"),
                            ),
                            Err(_) => (
                                false,
                                format!("readiness probe is still failing on {probe_target}"),
                                Some("NotReady"),
                            ),
                        }
                    }
                } else {
                    (true, "container is healthy".to_string(), Some("Running"))
                };
            let result = ha_node
                .propose(ClusterCommand::SetReplicaStatus {
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
                })
                .await?;
            if matches!(result, CommandResult::Changed { changed: true }) {
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
            runtime
                .inspect_exit_state(&container.id)
                .await
                .ok()
                .and_then(|state| summarize_exit_state(&state))
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
            let result = ha_node
                .propose(ClusterCommand::SetReplicaStatus {
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
                })
                .await?;
            if matches!(result, CommandResult::Changed { changed: true }) {
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
        let result = ha_node
            .propose(ClusterCommand::SetReplicaStatus {
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
            })
            .await?;
        if matches!(result, CommandResult::Changed { changed: true }) {
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

    let result = ha_node
        .propose(ClusterCommand::SetReplicaStatus {
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
        })
        .await?;
    if matches!(result, CommandResult::Changed { changed: true }) {
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
    use super::restart_backoff_for_streak;

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
