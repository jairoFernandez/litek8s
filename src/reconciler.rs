use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::Result;

use crate::control_plane::raft::ControlPlaneNode;
use crate::control_plane::{ClusterCommand, CommandResult};
use crate::node::{ClusterNode, NodeClient, select_node_for_manifest};
use crate::runtime::LocalRuntime;
use crate::runtime::docker::summarize_exit_state;
use crate::store::{DeploymentStatus, EventLevel, Store, StoredDeployment};

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

    let mut desired_names_by_node: HashMap<String, HashSet<String>> = HashMap::new();
    for deployment in &deployments {
        if let Ok(target_node) =
            select_node_for_manifest(&deployment.manifest, nodes, Some(&deployment.assigned_node))
        {
            desired_names_by_node
                .entry(target_node)
                .or_default()
                .insert(deployment.manifest.deployment_name().to_string());
        }
    }

    if cleanup_orphans {
        for node in nodes {
            let runtime = NodeClient::for_node(node, local_runtime.clone());
            let desired_names = desired_names_by_node
                .get(&node.name)
                .cloned()
                .unwrap_or_default();
            if let Err(error) = runtime.remove_orphans(&desired_names).await {
                eprintln!(
                    "skipping orphan cleanup on node `{}` because it is unavailable: {error}",
                    node.name
                );
            }
        }
    }

    for mut deployment in deployments {
        let target_node = match select_node_for_manifest(
            &deployment.manifest,
            nodes,
            Some(&deployment.assigned_node),
        ) {
            Ok(target_node) => target_node,
            Err(error) => {
                let result = ha_node
                    .propose(ClusterCommand::SetDeploymentStatus {
                        name: deployment.manifest.deployment_name().to_string(),
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
                        "FailedScheduling",
                        deployment.manifest.deployment_name(),
                        Some(deployment.assigned_node.clone()),
                        error.to_string(),
                    )
                    .await?;
                }
                if let Some(previous_node) = nodes_by_name.get(&deployment.assigned_node) {
                    let runtime = NodeClient::for_node(previous_node, local_runtime.clone());
                    let _ = runtime
                        .remove_deployment(deployment.manifest.deployment_name())
                        .await;
                }
                continue;
            }
        };

        if deployment.assigned_node != target_node {
            if let Some(previous_node) = nodes_by_name.get(&deployment.assigned_node) {
                let previous_runtime = NodeClient::for_node(previous_node, local_runtime.clone());
                if let Err(error) = previous_runtime
                    .remove_deployment(deployment.manifest.deployment_name())
                    .await
                {
                    eprintln!(
                        "failed to clean deployment `{}` off previous node `{}` during reschedule: {error}",
                        deployment.manifest.deployment_name(),
                        deployment.assigned_node
                    );
                }
            }
            let assign_result = ha_node
                .propose(ClusterCommand::SetDeploymentAssignment {
                    name: deployment.manifest.deployment_name().to_string(),
                    assigned_node: target_node.clone(),
                })
                .await?;
            if matches!(assign_result, CommandResult::Changed { changed: true }) {
                record_deployment_event(
                    ha_node,
                    EventLevel::Normal,
                    "Scheduled",
                    deployment.manifest.deployment_name(),
                    Some(target_node.clone()),
                    format!("deployment assigned to node `{target_node}`"),
                )
                .await?;
            }
            let _ = ha_node
                .propose(ClusterCommand::SetDeploymentStatus {
                    name: deployment.manifest.deployment_name().to_string(),
                    status: DeploymentStatus::Pending,
                    ready: false,
                    container_id: None,
                    message: Some(format!("scheduled on node `{target_node}`")),
                    last_exit: Some("rescheduled".to_string()),
                    increment_restart_count: false,
                    increment_restart_streak: false,
                    next_retry_at: None,
                })
                .await?;
            deployment.assigned_node = target_node.clone();
            deployment.container_id = None;
        }

        let Some(node) = nodes_by_name.get(&target_node) else {
            continue;
        };

        let runtime = NodeClient::for_node(node, local_runtime.clone());
        if let Err(error) = reconcile_deployment(ha_node, &runtime, &deployment).await {
            let result = ha_node
                .propose(ClusterCommand::SetDeploymentStatus {
                    name: deployment.manifest.deployment_name().to_string(),
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
                    deployment.manifest.deployment_name(),
                    Some(deployment.assigned_node.clone()),
                    error.to_string(),
                )
                .await?;
            }
        }
    }

    Ok(())
}

pub async fn reconcile_deployment(
    ha_node: &ControlPlaneNode,
    runtime: &NodeClient,
    deployment: &StoredDeployment,
) -> Result<()> {
    let name = deployment.manifest.deployment_name();
    let containers = runtime.list_containers_for_deployment(name).await?;

    if containers.len() > 1 {
        runtime.remove_deployment(name).await?;
        runtime
            .wait_until_deployment_cleared(name, RECREATE_WAIT_TIMEOUT)
            .await?;
        let new_id = runtime
            .create_and_start(&deployment.manifest, &deployment.spec_hash)
            .await?;
        let result = ha_node
            .propose(ClusterCommand::SetDeploymentStatus {
                name: name.to_string(),
                status: DeploymentStatus::Restarting,
                ready: false,
                container_id: Some(new_id),
                message: Some(
                    "recreated after duplicate managed containers were found".to_string(),
                ),
                last_exit: Some("duplicate containers".to_string()),
                increment_restart_count: true,
                increment_restart_streak: false,
                next_retry_at: None,
            })
            .await?;
        if matches!(result, CommandResult::Changed { changed: true }) {
            record_deployment_event(
                ha_node,
                EventLevel::Warning,
                "Recreated",
                name,
                Some(deployment.assigned_node.clone()),
                "duplicate managed containers were removed and recreated".to_string(),
            )
            .await?;
        }
        return Ok(());
    }

    if let Some(container) = containers.into_iter().next() {
        let hash_matches = container.spec_hash.as_deref() == Some(deployment.spec_hash.as_str());
        if hash_matches && container.running {
            let (ready, message, event_reason) =
                if let Some(probe) = deployment.manifest.container().readiness_probe() {
                    let probe_target = format!("tcpSocket:{}", probe.tcp_socket.port);
                    let started_at = deployment.container_started_at.unwrap_or_else(now_unix);
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
                .propose(ClusterCommand::SetDeploymentStatus {
                    name: name.to_string(),
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
                    message,
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
        runtime
            .wait_until_deployment_cleared(name, RECREATE_WAIT_TIMEOUT)
            .await?;

        let message = if hash_matches {
            let backoff = restart_backoff_for_streak(deployment.restart_streak.saturating_add(1));
            let next_retry_at = now_unix().saturating_add(backoff.as_secs());
            let reason = last_exit
                .clone()
                .unwrap_or_else(|| "container exited".to_string());
            let result = ha_node
                .propose(ClusterCommand::SetDeploymentStatus {
                    name: name.to_string(),
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
                    format!("backing off {}s after {reason}", backoff.as_secs()),
                )
                .await?;
            }
            return Ok(());
        } else {
            "container was recreated after the manifest changed"
        };

        let new_id = runtime
            .create_and_start(&deployment.manifest, &deployment.spec_hash)
            .await?;

        let result = ha_node
            .propose(ClusterCommand::SetDeploymentStatus {
                name: name.to_string(),
                status: DeploymentStatus::Restarting,
                ready: false,
                container_id: Some(new_id),
                message: Some(message.to_string()),
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
                message.to_string(),
            )
            .await?;
        }
        return Ok(());
    }

    if let Some(next_retry_at) = deployment.next_retry_at {
        if now_unix() < next_retry_at {
            return Ok(());
        }
    }

    let new_id = runtime
        .create_and_start(&deployment.manifest, &deployment.spec_hash)
        .await?;
    let retried_after_backoff = deployment.next_retry_at.is_some();
    let status = if deployment.container_id.is_some() || retried_after_backoff {
        DeploymentStatus::Restarting
    } else {
        DeploymentStatus::Running
    };
    let message = if retried_after_backoff {
        "container restarted after backoff"
    } else if deployment.container_id.is_some() {
        "container was recreated after disappearing"
    } else {
        "container was created"
    };

    let result = ha_node
        .propose(ClusterCommand::SetDeploymentStatus {
            name: name.to_string(),
            status,
            ready: false,
            container_id: Some(new_id),
            message: Some(message.to_string()),
            last_exit: if deployment.status == DeploymentStatus::Restarting
                && deployment.last_exit.is_some()
            {
                deployment.last_exit.clone()
            } else {
                deployment
                    .container_id
                    .as_ref()
                    .map(|_| "container disappeared".to_string())
            },
            increment_restart_count: deployment.container_id.is_some(),
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
            } else if deployment.container_id.is_some() {
                "Recreated"
            } else {
                "Started"
            },
            name,
            Some(deployment.assigned_node.clone()),
            message.to_string(),
        )
        .await?;
    }
    Ok(())
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
