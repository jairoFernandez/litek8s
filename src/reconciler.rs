use std::collections::HashSet;

use anyhow::Result;

use crate::runtime::docker::{DockerRuntime, summarize_exit_state};
use crate::store::{DeploymentStatus, Store, StoredDeployment};

const RECREATE_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
        let new_id = runtime
            .create_and_start(&deployment.manifest, &deployment.spec_hash)
            .await?;

        let message = if hash_matches {
            "container was restarted after drift was detected"
        } else {
            "container was recreated after the manifest changed"
        };

        store
            .set_status(
                name,
                DeploymentStatus::Restarting,
                Some(new_id),
                Some(message.to_string()),
                last_exit,
                true,
            )
            .await?;
        return Ok(());
    }

    let new_id = runtime
        .create_and_start(&deployment.manifest, &deployment.spec_hash)
        .await?;
    let status = if deployment.container_id.is_some() {
        DeploymentStatus::Restarting
    } else {
        DeploymentStatus::Running
    };
    let message = if deployment.container_id.is_some() {
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
            deployment
                .container_id
                .as_ref()
                .map(|_| "container disappeared".to_string()),
            deployment.container_id.is_some(),
        )
        .await?;
    Ok(())
}
