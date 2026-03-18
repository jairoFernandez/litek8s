use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;

use crate::runtime::docker::{DockerRuntime, summarize_exit_state};
use crate::store::{DeploymentStatus, Store, StoredDeployment};

const RECREATE_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_RESTART_BACKOFF_SECS: u64 = 15;

pub async fn reconcile_all(
    store: &Store,
    runtime: &DockerRuntime,
    cleanup_orphans: bool,
) -> Result<()> {
    let deployments = store.list().await;
    let desired_names: HashSet<String> = deployments
        .iter()
        .map(|entry| entry.manifest.deployment_name().to_string())
        .collect();

    if cleanup_orphans {
        runtime.remove_orphans(&desired_names).await?;
    }

    for deployment in deployments {
        if let Err(error) = reconcile_deployment(store, runtime, &deployment).await {
            store
                .set_status(
                    deployment.manifest.deployment_name(),
                    DeploymentStatus::Failed,
                    None,
                    Some(error.to_string()),
                    None,
                    false,
                    false,
                    None,
                )
                .await?;
        }
    }

    Ok(())
}

pub async fn reconcile_deployment(
    store: &Store,
    runtime: &DockerRuntime,
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
        store
            .set_status(
                name,
                DeploymentStatus::Restarting,
                Some(new_id),
                Some("recreated after duplicate managed containers were found".to_string()),
                Some("duplicate containers".to_string()),
                true,
                false,
                None,
            )
            .await?;
        return Ok(());
    }

    if let Some(container) = containers.into_iter().next() {
        let hash_matches = container.spec_hash.as_deref() == Some(deployment.spec_hash.as_str());
        if hash_matches && container.running {
            store
                .set_status(
                    name,
                    DeploymentStatus::Running,
                    Some(container.id),
                    Some("container is healthy".to_string()),
                    None,
                    false,
                    false,
                    None,
                )
                .await?;
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
            let reason = last_exit.as_deref().unwrap_or("container exited");
            store
                .set_status(
                    name,
                    DeploymentStatus::Restarting,
                    None,
                    Some(format!(
                        "backing off {}s before retry after {reason}",
                        backoff.as_secs()
                    )),
                    last_exit,
                    true,
                    true,
                    Some(next_retry_at),
                )
                .await?;
            return Ok(());
        } else {
            "container was recreated after the manifest changed"
        };

        let new_id = runtime
            .create_and_start(&deployment.manifest, &deployment.spec_hash)
            .await?;

        store
            .set_status(
                name,
                DeploymentStatus::Restarting,
                Some(new_id),
                Some(message.to_string()),
                last_exit,
                true,
                false,
                None,
            )
            .await?;
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

    store
        .set_status(
            name,
            status,
            Some(new_id),
            Some(message.to_string()),
            if deployment.status == DeploymentStatus::Restarting && deployment.last_exit.is_some() {
                deployment.last_exit.clone()
            } else {
                deployment
                    .container_id
                    .as_ref()
                    .map(|_| "container disappeared".to_string())
            },
            deployment.container_id.is_some(),
            false,
            None,
        )
        .await?;
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
