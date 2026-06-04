pub mod docker;
#[cfg(test)]
pub mod fake;

use std::collections::HashSet;
#[cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

pub use docker::{
    ConfiguredResourceLimits, ContainerExitState, ContainerMetrics, ContainerStatsSnapshot,
    DockerRuntime, ManagedContainer,
};
#[cfg(test)]
pub use fake::FakeRuntime;

use crate::manifest::DeploymentManifest;

/// Abstraction over a container runtime backend.
///
/// `DockerRuntime` is the default implementation. Future implementations
/// (containerd/CRI, OCI runc, in-memory fake for tests) implement this trait
/// so the reconciler/node layers stay backend-agnostic.
#[allow(async_fn_in_trait)]
pub trait ContainerRuntime {
    async fn ping(&self) -> Result<()>;

    async fn list_managed_containers(&self) -> Result<Vec<ManagedContainer>>;

    async fn list_containers_for_deployment(&self, name: &str) -> Result<Vec<ManagedContainer>>;

    async fn create_and_start(
        &self,
        manifest: &DeploymentManifest,
        spec_hash: &str,
        ordinal: u32,
    ) -> Result<String>;

    async fn remove_deployment(&self, name: &str) -> Result<()>;

    async fn wait_until_deployment_cleared(&self, name: &str, timeout: Duration) -> Result<()>;

    async fn remove_orphans(
        &self,
        desired_names: &HashSet<String>,
        known_deployments: &HashSet<String>,
    ) -> Result<()>;

    async fn remove_container(&self, container_id: &str) -> Result<()>;

    async fn logs(&self, name: &str, tail: usize) -> Result<String>;

    async fn configured_limits_for_deployment(
        &self,
        name: &str,
    ) -> Result<ConfiguredResourceLimits>;

    async fn inspect_exit_state(&self, container_id: &str) -> Result<ContainerExitState>;

    async fn metrics_for_container(&self, container_id: &str) -> Result<ContainerMetrics>;

    async fn stats_snapshot_for_container(
        &self,
        container_id: &str,
    ) -> Result<ContainerStatsSnapshot>;

    async fn probe_container_tcp(
        &self,
        container_id: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<()>;

    async fn inspect_container_ip(&self, container_id: &str) -> Result<Option<String>>;
}

/// Holder for the runtime backing a local node.
///
/// Production uses `Docker`. Tests can use `Fake` to drive reconciler logic
/// without a real Docker daemon. The enum implements `ContainerRuntime` so
/// callers stay backend-agnostic.
#[derive(Clone)]
pub enum LocalRuntime {
    Docker(DockerRuntime),
    #[cfg(test)]
    Fake(Arc<FakeRuntime>),
}

impl From<DockerRuntime> for LocalRuntime {
    fn from(runtime: DockerRuntime) -> Self {
        Self::Docker(runtime)
    }
}

#[cfg(test)]
impl From<Arc<FakeRuntime>> for LocalRuntime {
    fn from(runtime: Arc<FakeRuntime>) -> Self {
        Self::Fake(runtime)
    }
}

impl ContainerRuntime for LocalRuntime {
    async fn ping(&self) -> Result<()> {
        match self {
            Self::Docker(runtime) => runtime.ping().await,
            #[cfg(test)]
            Self::Fake(runtime) => runtime.ping().await,
        }
    }

    async fn list_managed_containers(&self) -> Result<Vec<ManagedContainer>> {
        match self {
            Self::Docker(runtime) => runtime.list_managed_containers().await,
            #[cfg(test)]
            Self::Fake(runtime) => runtime.list_managed_containers().await,
        }
    }

    async fn list_containers_for_deployment(&self, name: &str) -> Result<Vec<ManagedContainer>> {
        match self {
            Self::Docker(runtime) => runtime.list_containers_for_deployment(name).await,
            #[cfg(test)]
            Self::Fake(runtime) => runtime.list_containers_for_deployment(name).await,
        }
    }

    async fn create_and_start(
        &self,
        manifest: &DeploymentManifest,
        spec_hash: &str,
        ordinal: u32,
    ) -> Result<String> {
        match self {
            Self::Docker(runtime) => runtime.create_and_start(manifest, spec_hash, ordinal).await,
            #[cfg(test)]
            Self::Fake(runtime) => runtime.create_and_start(manifest, spec_hash, ordinal).await,
        }
    }

    async fn remove_deployment(&self, name: &str) -> Result<()> {
        match self {
            Self::Docker(runtime) => runtime.remove_deployment(name).await,
            #[cfg(test)]
            Self::Fake(runtime) => runtime.remove_deployment(name).await,
        }
    }

    async fn wait_until_deployment_cleared(&self, name: &str, timeout: Duration) -> Result<()> {
        match self {
            Self::Docker(runtime) => runtime.wait_until_deployment_cleared(name, timeout).await,
            #[cfg(test)]
            Self::Fake(runtime) => runtime.wait_until_deployment_cleared(name, timeout).await,
        }
    }

    async fn remove_orphans(
        &self,
        desired_names: &HashSet<String>,
        known_deployments: &HashSet<String>,
    ) -> Result<()> {
        match self {
            Self::Docker(runtime) => {
                runtime.remove_orphans(desired_names, known_deployments).await
            }
            #[cfg(test)]
            Self::Fake(runtime) => runtime.remove_orphans(desired_names, known_deployments).await,
        }
    }

    async fn remove_container(&self, container_id: &str) -> Result<()> {
        match self {
            Self::Docker(runtime) => runtime.remove_container(container_id).await,
            #[cfg(test)]
            Self::Fake(runtime) => runtime.remove_container(container_id).await,
        }
    }

    async fn logs(&self, name: &str, tail: usize) -> Result<String> {
        match self {
            Self::Docker(runtime) => runtime.logs(name, tail).await,
            #[cfg(test)]
            Self::Fake(runtime) => runtime.logs(name, tail).await,
        }
    }

    async fn configured_limits_for_deployment(
        &self,
        name: &str,
    ) -> Result<ConfiguredResourceLimits> {
        match self {
            Self::Docker(runtime) => runtime.configured_limits_for_deployment(name).await,
            #[cfg(test)]
            Self::Fake(runtime) => runtime.configured_limits_for_deployment(name).await,
        }
    }

    async fn inspect_exit_state(&self, container_id: &str) -> Result<ContainerExitState> {
        match self {
            Self::Docker(runtime) => runtime.inspect_exit_state(container_id).await,
            #[cfg(test)]
            Self::Fake(runtime) => runtime.inspect_exit_state(container_id).await,
        }
    }

    async fn metrics_for_container(&self, container_id: &str) -> Result<ContainerMetrics> {
        match self {
            Self::Docker(runtime) => runtime.metrics_for_container(container_id).await,
            #[cfg(test)]
            Self::Fake(runtime) => runtime.metrics_for_container(container_id).await,
        }
    }

    async fn stats_snapshot_for_container(
        &self,
        container_id: &str,
    ) -> Result<ContainerStatsSnapshot> {
        match self {
            Self::Docker(runtime) => runtime.stats_snapshot_for_container(container_id).await,
            #[cfg(test)]
            Self::Fake(runtime) => runtime.stats_snapshot_for_container(container_id).await,
        }
    }

    async fn probe_container_tcp(
        &self,
        container_id: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<()> {
        match self {
            Self::Docker(runtime) => {
                runtime.probe_container_tcp(container_id, port, timeout).await
            }
            #[cfg(test)]
            Self::Fake(runtime) => {
                runtime.probe_container_tcp(container_id, port, timeout).await
            }
        }
    }

    async fn inspect_container_ip(&self, container_id: &str) -> Result<Option<String>> {
        match self {
            Self::Docker(runtime) => runtime.inspect_container_ip(container_id).await,
            #[cfg(test)]
            Self::Fake(runtime) => runtime.inspect_container_ip(container_id).await,
        }
    }
}
