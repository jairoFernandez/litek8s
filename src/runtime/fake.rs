//! In-memory `ContainerRuntime` for tests.
//!
//! No Docker required. State lives behind a `Mutex` for interior mutability
//! since trait methods take `&self`. Tests can configure failure injection
//! and inspect call counts.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use crate::manifest::DeploymentManifest;
use crate::runtime::{
    ConfiguredResourceLimits, ContainerExitState, ContainerMetrics, ContainerRuntime,
    ContainerStatsSnapshot, ManagedContainer,
};

#[derive(Debug, Default, Clone)]
pub struct FakeContainerState {
    pub running: bool,
    pub state: Option<String>,
    pub status: Option<String>,
    pub exit: ContainerExitState,
    pub limits: ConfiguredResourceLimits,
    pub metrics: ContainerMetrics,
    pub stats: ContainerStatsSnapshot,
    pub probe_succeeds: bool,
    pub logs: String,
}

impl FakeContainerState {
    fn running_default() -> Self {
        Self {
            running: true,
            state: Some("running".to_string()),
            status: Some("Up".to_string()),
            exit: ContainerExitState {
                status: Some("running".to_string()),
                exit_code: None,
                oom_killed: false,
                error: None,
            },
            limits: ConfiguredResourceLimits {
                nano_cpus: None,
                memory_bytes: None,
            },
            metrics: ContainerMetrics {
                cpu_percent: Some(0.0),
                memory_usage_bytes: Some(0),
                memory_limit_bytes: Some(0),
                network_rx_bytes: Some(0),
                network_tx_bytes: Some(0),
            },
            stats: ContainerStatsSnapshot {
                cpu_total_usage: 0,
                system_cpu_usage: Some(0),
                online_cpus: 1,
                fallback_cpu_percent: Some(0.0),
                memory_usage_bytes: Some(0),
                memory_limit_bytes: Some(0),
                network_rx_bytes: 0,
                network_tx_bytes: 0,
            },
            probe_succeeds: true,
            logs: String::new(),
        }
    }
}

#[derive(Debug, Default)]
struct FakeState {
    containers: HashMap<String, (ManagedContainer, FakeContainerState)>,
    next_id: u64,
    create_calls: u64,
    remove_calls: u64,
}

#[derive(Debug, Default)]
pub struct FakeRuntime {
    state: Mutex<FakeState>,
}

impl FakeRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_calls(&self) -> u64 {
        self.state.lock().unwrap().create_calls
    }

    pub fn remove_calls(&self) -> u64 {
        self.state.lock().unwrap().remove_calls
    }

    /// Override state for a container by id (e.g. mark exited, set exit code).
    pub fn set_container_state(&self, container_id: &str, mutate: impl FnOnce(&mut FakeContainerState)) {
        let mut state = self.state.lock().unwrap();
        if let Some((managed, fake)) = state.containers.get_mut(container_id) {
            mutate(fake);
            managed.running = fake.running;
            managed.state = fake.state.clone();
            managed.status = fake.status.clone();
        }
    }

    fn snapshot_managed(&self) -> Vec<ManagedContainer> {
        let state = self.state.lock().unwrap();
        let mut items: Vec<ManagedContainer> =
            state.containers.values().map(|(m, _)| m.clone()).collect();
        items.sort_by(|left, right| left.id.cmp(&right.id));
        items
    }
}

impl ContainerRuntime for FakeRuntime {
    async fn ping(&self) -> Result<()> {
        Ok(())
    }

    async fn list_managed_containers(&self) -> Result<Vec<ManagedContainer>> {
        Ok(self.snapshot_managed())
    }

    async fn list_containers_for_deployment(&self, name: &str) -> Result<Vec<ManagedContainer>> {
        Ok(self
            .snapshot_managed()
            .into_iter()
            .filter(|container| container.deployment == name)
            .collect())
    }

    async fn create_and_start(
        &self,
        manifest: &DeploymentManifest,
        spec_hash: &str,
        ordinal: u32,
    ) -> Result<String> {
        let mut state = self.state.lock().unwrap();
        state.next_id += 1;
        state.create_calls += 1;
        let id = format!("fake-{}", state.next_id);
        let deployment = manifest.deployment_name().to_string();
        let managed = ManagedContainer {
            id: id.clone(),
            name: format!("litek8s-{deployment}-{ordinal}"),
            deployment: deployment.clone(),
            state: Some("running".to_string()),
            status: Some("Up".to_string()),
            running: true,
            spec_hash: Some(spec_hash.to_string()),
            ordinal: Some(ordinal),
            container_ip: None,
        };
        state
            .containers
            .insert(id.clone(), (managed, FakeContainerState::running_default()));
        Ok(id)
    }

    async fn remove_deployment(&self, name: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let ids: Vec<String> = state
            .containers
            .iter()
            .filter(|(_, (managed, _))| managed.deployment == name)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            state.containers.remove(&id);
            state.remove_calls += 1;
        }
        Ok(())
    }

    async fn wait_until_deployment_cleared(&self, name: &str, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.list_containers_for_deployment(name).await?.is_empty() {
                return Ok(());
            }
            if tokio::time::Instant::now() > deadline {
                bail!("timed out waiting for deployment `{name}` containers to disappear");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn remove_orphans(&self, desired_names: &HashSet<String>) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let ids: Vec<String> = state
            .containers
            .iter()
            .filter(|(_, (managed, _))| !desired_names.contains(&managed.deployment))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            state.containers.remove(&id);
            state.remove_calls += 1;
        }
        Ok(())
    }

    async fn remove_container(&self, container_id: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.containers.remove(container_id).is_some() {
            state.remove_calls += 1;
        }
        Ok(())
    }

    async fn logs(&self, name: &str, _tail: usize) -> Result<String> {
        let state = self.state.lock().unwrap();
        state
            .containers
            .values()
            .find(|(managed, _)| managed.deployment == name)
            .map(|(_, fake)| fake.logs.clone())
            .ok_or_else(|| anyhow!("deployment `{name}` has no managed container"))
    }

    async fn configured_limits_for_deployment(
        &self,
        name: &str,
    ) -> Result<ConfiguredResourceLimits> {
        let state = self.state.lock().unwrap();
        state
            .containers
            .values()
            .find(|(managed, _)| managed.deployment == name)
            .map(|(_, fake)| fake.limits.clone())
            .ok_or_else(|| anyhow!("deployment `{name}` has no managed container"))
    }

    async fn inspect_exit_state(&self, container_id: &str) -> Result<ContainerExitState> {
        let state = self.state.lock().unwrap();
        state
            .containers
            .get(container_id)
            .map(|(_, fake)| fake.exit.clone())
            .ok_or_else(|| anyhow!("container `{container_id}` not found"))
    }

    async fn metrics_for_container(&self, container_id: &str) -> Result<ContainerMetrics> {
        let state = self.state.lock().unwrap();
        state
            .containers
            .get(container_id)
            .map(|(_, fake)| fake.metrics.clone())
            .ok_or_else(|| anyhow!("container `{container_id}` not found"))
    }

    async fn stats_snapshot_for_container(
        &self,
        container_id: &str,
    ) -> Result<ContainerStatsSnapshot> {
        let state = self.state.lock().unwrap();
        state
            .containers
            .get(container_id)
            .map(|(_, fake)| fake.stats.clone())
            .ok_or_else(|| anyhow!("container `{container_id}` not found"))
    }

    async fn probe_container_tcp(
        &self,
        container_id: &str,
        _port: u16,
        _timeout: Duration,
    ) -> Result<()> {
        let state = self.state.lock().unwrap();
        let fake = state
            .containers
            .get(container_id)
            .map(|(_, fake)| fake.clone())
            .ok_or_else(|| anyhow!("container `{container_id}` not found"))?;
        if fake.probe_succeeds {
            Ok(())
        } else {
            bail!("fake probe failure for container {container_id}")
        }
    }
}

impl Default for ContainerExitState {
    fn default() -> Self {
        Self {
            status: None,
            exit_code: None,
            oom_killed: false,
            error: None,
        }
    }
}

impl Default for ConfiguredResourceLimits {
    fn default() -> Self {
        Self {
            nano_cpus: None,
            memory_bytes: None,
        }
    }
}

impl Default for ContainerMetrics {
    fn default() -> Self {
        Self {
            cpu_percent: None,
            memory_usage_bytes: None,
            memory_limit_bytes: None,
            network_rx_bytes: None,
            network_tx_bytes: None,
        }
    }
}

impl Default for ContainerStatsSnapshot {
    fn default() -> Self {
        Self {
            cpu_total_usage: 0,
            system_cpu_usage: None,
            online_cpus: 1,
            fallback_cpu_percent: None,
            memory_usage_bytes: None,
            memory_limit_bytes: None,
            network_rx_bytes: 0,
            network_tx_bytes: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{API_VERSION, DeploymentManifest};

    fn sample_manifest(name: &str) -> DeploymentManifest {
        DeploymentManifest::parse(&format!(
            r#"
apiVersion: {API_VERSION}
kind: Deployment
metadata:
  name: {name}
spec:
  replicas: 1
  template:
    spec:
      restartPolicy: Always
      containers:
        - name: web
          image: nginx:alpine
"#
        ))
        .expect("manifest should parse")
    }

    #[tokio::test]
    async fn create_and_remove_roundtrip() {
        let runtime = FakeRuntime::new();
        let manifest = sample_manifest("demo");

        let id = runtime
            .create_and_start(&manifest, "hash1", 0)
            .await
            .expect("create");
        assert_eq!(runtime.create_calls(), 1);

        let containers = runtime
            .list_containers_for_deployment("demo")
            .await
            .expect("list");
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].id, id);
        assert!(containers[0].running);
        assert_eq!(containers[0].spec_hash.as_deref(), Some("hash1"));

        runtime.remove_deployment("demo").await.expect("remove");
        assert_eq!(runtime.remove_calls(), 1);

        let containers = runtime
            .list_containers_for_deployment("demo")
            .await
            .expect("list after remove");
        assert!(containers.is_empty());
    }

    #[tokio::test]
    async fn probe_failure_injection() {
        let runtime = FakeRuntime::new();
        let manifest = sample_manifest("api");
        let id = runtime
            .create_and_start(&manifest, "hash", 0)
            .await
            .expect("create");

        runtime.set_container_state(&id, |fake| fake.probe_succeeds = false);

        let result = runtime
            .probe_container_tcp(&id, 80, Duration::from_millis(10))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn remove_orphans_drops_undesired() {
        let runtime = FakeRuntime::new();
        runtime
            .create_and_start(&sample_manifest("keep"), "h", 0)
            .await
            .expect("create");
        runtime
            .create_and_start(&sample_manifest("drop"), "h", 0)
            .await
            .expect("create");

        let mut desired = HashSet::new();
        desired.insert("keep".to_string());
        runtime.remove_orphans(&desired).await.expect("orphans");

        let all = runtime.list_managed_containers().await.expect("list");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].deployment, "keep");
    }
}
