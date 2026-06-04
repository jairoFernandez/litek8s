use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::config;
use crate::manifest::DeploymentManifest;
use crate::node::{ClusterNode, LOCAL_NODE_NAME};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum DeploymentStatus {
    #[default]
    Pending,
    Running,
    Restarting,
    Failed,
    Deleted,
}

const HEALTHY_RESET_AFTER_SECS: u64 = 10;
const MAX_EVENT_HISTORY: usize = 250;

/// State of one replica (one ordinal) of a deployment. The reconciler drives
/// these; the flat fields on `StoredDeployment` are an aggregate mirror derived
/// from this list (so display code that predates multi-replica still works).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplicaState {
    pub ordinal: u32,
    #[serde(default = "default_assigned_node")]
    pub assigned_node: String,
    #[serde(default)]
    pub status: DeploymentStatus,
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub container_id: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub restart_count: u64,
    #[serde(default)]
    pub last_exit: Option<String>,
    #[serde(default)]
    pub restart_streak: u32,
    #[serde(default)]
    pub next_retry_at: Option<u64>,
    #[serde(default)]
    pub healthy_since: Option<u64>,
    #[serde(default)]
    pub container_started_at: Option<u64>,
    #[serde(default)]
    pub ready_since: Option<u64>,
    #[serde(default)]
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredDeployment {
    pub manifest: DeploymentManifest,
    pub spec_hash: String,
    #[serde(default = "default_assigned_node")]
    pub assigned_node: String,
    pub status: DeploymentStatus,
    #[serde(default)]
    pub ready: bool,
    pub container_id: Option<String>,
    pub message: Option<String>,
    #[serde(default)]
    pub restart_count: u64,
    #[serde(default)]
    pub last_exit: Option<String>,
    #[serde(default)]
    pub restart_streak: u32,
    #[serde(default)]
    pub next_retry_at: Option<u64>,
    #[serde(default)]
    pub healthy_since: Option<u64>,
    #[serde(default)]
    pub container_started_at: Option<u64>,
    #[serde(default)]
    pub ready_since: Option<u64>,
    pub updated_at: u64,
    /// Per-replica state, ordinal 0..desired. Empty on records written before
    /// multi-replica support; `Store::load` synthesizes ordinal 0 from the flat
    /// fields above so the existing container is adopted, not recreated.
    #[serde(default)]
    pub replicas: Vec<ReplicaState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredService {
    pub name: String,
    #[serde(default)]
    pub deployment_name: String,
    #[serde(default)]
    pub selector: BTreeMap<String, String>,
    pub listen_addr: String,
    pub target_port: Option<u16>,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StateFile {
    pub deployments: BTreeMap<String, StoredDeployment>,
    #[serde(default)]
    pub services: BTreeMap<String, StoredService>,
    #[serde(default)]
    pub nodes: BTreeMap<String, ClusterNode>,
    #[serde(default)]
    pub models: BTreeMap<String, crate::model::ModelManifest>,
    #[serde(default)]
    pub events: Vec<StoredEvent>,
    #[serde(default)]
    pub next_event_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Created,
    Configured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeApplyOutcome {
    Created,
    Configured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceApplyOutcome {
    Created,
    Configured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventLevel {
    Normal,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvent {
    pub level: EventLevel,
    pub reason: String,
    pub object_kind: String,
    pub object_name: String,
    pub node_name: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredEvent {
    pub id: u64,
    pub timestamp_secs: u64,
    pub level: EventLevel,
    pub reason: String,
    pub object_kind: String,
    pub object_name: String,
    pub node_name: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventQuery {
    pub limit: usize,
    pub since_id: Option<u64>,
    pub deployment_name: Option<String>,
    pub node_name: Option<String>,
}

pub struct Store {
    path: PathBuf,
    state: RwLock<StateFile>,
    persist_lock: Mutex<()>,
}

impl Store {
    pub async fn load(path: PathBuf) -> Result<Self> {
        let state = if tokio::fs::try_exists(&path)
            .await
            .with_context(|| format!("failed to check if {} exists", path.display()))?
        {
            let bytes = tokio::fs::read(&path)
                .await
                .with_context(|| format!("failed to read {}", path.display()))?;
            serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to deserialize {}", path.display()))?
        } else {
            StateFile::default()
        };
        let mut state = state;
        for entry in state.deployments.values_mut() {
            if entry.assigned_node.is_empty() {
                entry.assigned_node = entry
                    .manifest
                    .explicit_node_name()
                    .unwrap_or(LOCAL_NODE_NAME)
                    .to_string();
            } else if entry.assigned_node == LOCAL_NODE_NAME {
                if let Some(explicit_node) = entry.manifest.explicit_node_name() {
                    entry.assigned_node = explicit_node.to_string();
                }
            }
            // Migrate pre-multi-replica records: synthesize ordinal 0 from the
            // flat fields so the existing container is adopted, not recreated.
            if entry.replicas.is_empty() {
                entry.replicas.push(ReplicaState {
                    ordinal: 0,
                    assigned_node: entry.assigned_node.clone(),
                    status: entry.status.clone(),
                    ready: entry.ready,
                    container_id: entry.container_id.clone(),
                    message: entry.message.clone(),
                    restart_count: entry.restart_count,
                    last_exit: entry.last_exit.clone(),
                    restart_streak: entry.restart_streak,
                    next_retry_at: entry.next_retry_at,
                    healthy_since: entry.healthy_since,
                    container_started_at: entry.container_started_at,
                    ready_since: entry.ready_since,
                    updated_at: entry.updated_at,
                });
            }
        }
        state
            .nodes
            .entry(LOCAL_NODE_NAME.to_string())
            .or_insert_with(ClusterNode::local);
        if let Some(local) = state.nodes.get_mut(LOCAL_NODE_NAME) {
            if local.labels.is_empty() {
                *local = ClusterNode::local();
            }
        }

        Ok(Self {
            path,
            state: RwLock::new(state),
            persist_lock: Mutex::new(()),
        })
    }

    pub async fn load_default() -> Result<Self> {
        let path = config::default_state_file()?;
        Self::load(path).await
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub async fn upsert(&self, manifest: DeploymentManifest) -> Result<ApplyOutcome> {
        let name = manifest.deployment_name().to_string();
        // Identity hash is the pod template only — scaling the replica count
        // must not invalidate running containers.
        let spec_hash = manifest.pod_template_hash()?;

        let mut guard = self.state.write().await;
        let existing = guard.deployments.get(&name).cloned();
        let outcome = if existing.is_some() {
            ApplyOutcome::Configured
        } else {
            ApplyOutcome::Created
        };

        // Preserve live per-replica state across re-apply; the reconciler scales
        // the Vec up/down to match the new desired replica count.
        let replicas = existing
            .as_ref()
            .map(|entry| entry.replicas.clone())
            .unwrap_or_default();

        guard.deployments.insert(
            name,
            StoredDeployment {
                manifest,
                spec_hash,
                assigned_node: existing
                    .as_ref()
                    .map(|entry| entry.assigned_node.clone())
                    .unwrap_or_else(|| LOCAL_NODE_NAME.to_string()),
                status: DeploymentStatus::Pending,
                ready: false,
                container_id: None,
                message: Some("reconciliation scheduled".to_string()),
                restart_count: existing
                    .as_ref()
                    .map(|entry| entry.restart_count)
                    .unwrap_or(0),
                last_exit: None,
                restart_streak: 0,
                next_retry_at: None,
                healthy_since: None,
                container_started_at: None,
                ready_since: None,
                updated_at: now_unix(),
                replicas,
            },
        );

        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot).await?;
        Ok(outcome)
    }

    pub async fn remove(&self, name: &str) -> Result<Option<StoredDeployment>> {
        let mut guard = self.state.write().await;
        let removed = guard.deployments.remove(name);
        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot).await?;
        Ok(removed)
    }

    pub async fn get(&self, name: &str) -> Option<StoredDeployment> {
        let guard = self.state.read().await;
        guard.deployments.get(name).cloned()
    }

    pub async fn list(&self) -> Vec<StoredDeployment> {
        let guard = self.state.read().await;
        guard.deployments.values().cloned().collect()
    }

    pub async fn names(&self) -> Vec<String> {
        let guard = self.state.read().await;
        guard.deployments.keys().cloned().collect()
    }

    pub async fn upsert_service(
        &self,
        name: String,
        deployment_name: String,
        selector: BTreeMap<String, String>,
        listen_addr: String,
        target_port: Option<u16>,
    ) -> Result<ServiceApplyOutcome> {
        let mut guard = self.state.write().await;
        let outcome = if guard.services.contains_key(&name) {
            ServiceApplyOutcome::Configured
        } else {
            ServiceApplyOutcome::Created
        };
        guard.services.insert(
            name.clone(),
            StoredService {
                name,
                deployment_name,
                selector,
                listen_addr,
                target_port,
                updated_at: now_unix(),
            },
        );
        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot).await?;
        Ok(outcome)
    }

    pub async fn remove_service(&self, name: &str) -> Result<Option<StoredService>> {
        let mut guard = self.state.write().await;
        let removed = guard.services.remove(name);
        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot).await?;
        Ok(removed)
    }

    pub async fn get_service(&self, name: &str) -> Option<StoredService> {
        let guard = self.state.read().await;
        guard.services.get(name).cloned()
    }

    pub async fn list_services(&self) -> Vec<StoredService> {
        let guard = self.state.read().await;
        guard.services.values().cloned().collect()
    }

    pub async fn upsert_node(&self, node: ClusterNode) -> Result<NodeApplyOutcome> {
        let mut guard = self.state.write().await;
        let name = node.name.clone();
        let outcome = if guard.nodes.contains_key(&name) {
            NodeApplyOutcome::Configured
        } else {
            NodeApplyOutcome::Created
        };
        let mut node = node;
        if let Some(existing) = guard.nodes.get(&name) {
            node.unschedulable = existing.unschedulable;
        }
        guard.nodes.insert(name, node);
        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot).await?;
        Ok(outcome)
    }

    pub async fn get_node(&self, name: &str) -> Option<ClusterNode> {
        let guard = self.state.read().await;
        guard.nodes.get(name).cloned()
    }

    pub async fn list_nodes(&self) -> Vec<ClusterNode> {
        let guard = self.state.read().await;
        guard.nodes.values().cloned().collect()
    }

    pub async fn upsert_model(&self, model: crate::model::ModelManifest) -> Result<()> {
        let mut guard = self.state.write().await;
        guard.models.insert(model.name.clone(), model);
        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot).await
    }

    pub async fn get_model(&self, name: &str) -> Option<crate::model::ModelManifest> {
        let guard = self.state.read().await;
        guard.models.get(name).cloned()
    }

    pub async fn list_models(&self) -> Vec<crate::model::ModelManifest> {
        let guard = self.state.read().await;
        guard.models.values().cloned().collect()
    }

    pub async fn delete_model(&self, name: &str) -> Result<bool> {
        let mut guard = self.state.write().await;
        if guard.models.remove(name).is_none() {
            return Ok(false);
        }
        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot).await?;
        Ok(true)
    }

    pub async fn set_node_vpn(
        &self,
        name: &str,
        membership: Option<crate::vpn::VpnMembership>,
    ) -> Result<bool> {
        let mut guard = self.state.write().await;
        let mut changed = false;
        if let Some(node) = guard.nodes.get_mut(name) {
            if node.vpn != membership {
                node.vpn = membership;
                changed = true;
            }
        }
        if !changed {
            return Ok(false);
        }
        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot).await?;
        Ok(true)
    }

    pub async fn set_node_unschedulable(&self, name: &str, unschedulable: bool) -> Result<bool> {
        let mut guard = self.state.write().await;
        let mut changed = false;
        if let Some(node) = guard.nodes.get_mut(name) {
            if node.unschedulable != unschedulable {
                node.unschedulable = unschedulable;
                changed = true;
            }
        }
        if !changed {
            return Ok(false);
        }

        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot).await?;
        Ok(true)
    }

    pub async fn set_assigned_node(&self, name: &str, assigned_node: String) -> Result<bool> {
        let mut guard = self.state.write().await;
        let mut changed = false;
        if let Some(entry) = guard.deployments.get_mut(name) {
            if entry.assigned_node != assigned_node {
                entry.assigned_node = assigned_node;
                entry.updated_at = now_unix();
                changed = true;
            }
        }
        if !changed {
            return Ok(false);
        }

        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot).await?;
        Ok(true)
    }

    pub async fn record_event(&self, event: NewEvent) -> Result<StoredEvent> {
        let mut guard = self.state.write().await;
        guard.next_event_id = guard.next_event_id.saturating_add(1);
        let stored = StoredEvent {
            id: guard.next_event_id,
            timestamp_secs: now_unix(),
            level: event.level,
            reason: event.reason,
            object_kind: event.object_kind,
            object_name: event.object_name,
            node_name: event.node_name,
            message: event.message,
        };
        guard.events.push(stored.clone());
        if guard.events.len() > MAX_EVENT_HISTORY {
            let overflow = guard.events.len() - MAX_EVENT_HISTORY;
            guard.events.drain(0..overflow);
        }

        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot).await?;
        Ok(stored)
    }

    pub async fn list_events(&self, query: EventQuery) -> Vec<StoredEvent> {
        let guard = self.state.read().await;
        let mut items = guard
            .events
            .iter()
            .filter(|event| query.since_id.map(|value| event.id > value).unwrap_or(true))
            .filter(|event| {
                query
                    .deployment_name
                    .as_deref()
                    .map(|name| event.object_kind == "deployment" && event.object_name == name)
                    .unwrap_or(true)
            })
            .filter(|event| {
                query
                    .node_name
                    .as_deref()
                    .map(|name| {
                        event.node_name.as_deref() == Some(name)
                            || (event.object_kind == "node" && event.object_name == name)
                    })
                    .unwrap_or(true)
            })
            .cloned()
            .collect::<Vec<_>>();

        if query.since_id.is_none() && query.limit > 0 && items.len() > query.limit {
            items = items[items.len() - query.limit..].to_vec();
        }

        items
    }

    pub async fn set_status(
        &self,
        name: &str,
        status: DeploymentStatus,
        ready: bool,
        container_id: Option<String>,
        message: Option<String>,
        last_exit: Option<String>,
        increment_restart_count: bool,
        increment_restart_streak: bool,
        next_retry_at: Option<u64>,
    ) -> Result<bool> {
        let update = StatusUpdate {
            status,
            ready,
            container_id,
            message,
            last_exit,
            increment_restart_count,
            increment_restart_streak,
            next_retry_at,
        };
        let mut guard = self.state.write().await;
        let mut changed = false;
        if let Some(entry) = guard.deployments.get_mut(name) {
            let probe = entry.manifest.container().readiness_probe().is_some();
            let mut fields = StatusFields::from_deployment(entry);
            if apply_status_transition(&mut fields, update, probe) {
                fields.write_back_deployment(entry);
                // Mirror onto ordinal 0 so the per-replica view stays consistent
                // when the legacy aggregate path is used (single-replica path).
                mirror_aggregate_to_replica0(entry);
                changed = true;
            }
        }
        if !changed {
            return Ok(false);
        }

        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot).await?;
        Ok(true)
    }

    /// Assign a single replica (ordinal) of a deployment to a node, creating the
    /// replica record if absent.
    pub async fn set_replica_assignment(
        &self,
        name: &str,
        ordinal: u32,
        assigned_node: String,
    ) -> Result<bool> {
        let mut guard = self.state.write().await;
        let mut changed = false;
        if let Some(entry) = guard.deployments.get_mut(name) {
            let replica = ensure_replica(entry, ordinal);
            if replica.assigned_node != assigned_node {
                replica.assigned_node = assigned_node;
                replica.updated_at = now_unix();
                recompute_aggregate(entry);
                changed = true;
            }
        }
        if !changed {
            return Ok(false);
        }
        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot).await?;
        Ok(true)
    }

    /// Apply a status transition to one replica (ordinal), creating it if absent.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_replica_status(
        &self,
        name: &str,
        ordinal: u32,
        status: DeploymentStatus,
        ready: bool,
        container_id: Option<String>,
        message: Option<String>,
        last_exit: Option<String>,
        increment_restart_count: bool,
        increment_restart_streak: bool,
        next_retry_at: Option<u64>,
    ) -> Result<bool> {
        let update = StatusUpdate {
            status,
            ready,
            container_id,
            message,
            last_exit,
            increment_restart_count,
            increment_restart_streak,
            next_retry_at,
        };
        let mut guard = self.state.write().await;
        let mut changed = false;
        if let Some(entry) = guard.deployments.get_mut(name) {
            let probe = entry.manifest.container().readiness_probe().is_some();
            let replica = ensure_replica(entry, ordinal);
            let mut fields = StatusFields::from_replica(replica);
            if apply_status_transition(&mut fields, update, probe) {
                fields.write_back_replica(replica);
                recompute_aggregate(entry);
                changed = true;
            }
        }
        if !changed {
            return Ok(false);
        }
        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot).await?;
        Ok(true)
    }

    /// Remove a single replica (used for scale-down). Returns true if removed.
    pub async fn remove_replica(&self, name: &str, ordinal: u32) -> Result<bool> {
        let mut guard = self.state.write().await;
        let mut changed = false;
        if let Some(entry) = guard.deployments.get_mut(name) {
            let before = entry.replicas.len();
            entry.replicas.retain(|replica| replica.ordinal != ordinal);
            if entry.replicas.len() != before {
                recompute_aggregate(entry);
                changed = true;
            }
        }
        if !changed {
            return Ok(false);
        }
        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot).await?;
        Ok(true)
    }

    pub async fn snapshot(&self) -> StateFile {
        self.state.read().await.clone()
    }

    pub async fn replace_state(&self, new_state: StateFile) {
        *self.state.write().await = new_state;
    }

    async fn persist(&self, snapshot: &StateFile) -> Result<()> {
        let _persist_guard = self.persist_lock.lock().await;
        config::ensure_parent_dir(&self.path)?;
        let bytes = serde_json::to_vec_pretty(snapshot).context("failed to serialize state")?;
        let tmp_path = self.path.with_extension("tmp");
        tokio::fs::write(&tmp_path, bytes)
            .await
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        tokio::fs::rename(&tmp_path, &self.path)
            .await
            .with_context(|| format!("failed to move state file into {}", self.path.display()))?;
        Ok(())
    }
}

impl Display for DeploymentStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Pending => "Pending",
            Self::Running => "Running",
            Self::Restarting => "Restarting",
            Self::Failed => "Failed",
            Self::Deleted => "Deleted",
        };
        write!(f, "{text}")
    }
}

impl Display for EventLevel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Normal => "Normal",
            Self::Warning => "Warning",
        };
        write!(f, "{text}")
    }
}

/// The inputs of a status transition, shared by the aggregate (`set_status`)
/// and per-replica (`set_replica_status`) paths.
struct StatusUpdate {
    status: DeploymentStatus,
    ready: bool,
    container_id: Option<String>,
    message: Option<String>,
    last_exit: Option<String>,
    increment_restart_count: bool,
    increment_restart_streak: bool,
    next_retry_at: Option<u64>,
}

/// The subset of fields a status transition reads/writes. Both `StoredDeployment`
/// (aggregate mirror) and `ReplicaState` carry these same fields; this lets the
/// transition logic live in one place.
struct StatusFields {
    status: DeploymentStatus,
    ready: bool,
    container_id: Option<String>,
    message: Option<String>,
    last_exit: Option<String>,
    restart_count: u64,
    restart_streak: u32,
    next_retry_at: Option<u64>,
    healthy_since: Option<u64>,
    container_started_at: Option<u64>,
    ready_since: Option<u64>,
    updated_at: u64,
}

impl StatusFields {
    fn from_deployment(dep: &StoredDeployment) -> Self {
        Self {
            status: dep.status.clone(),
            ready: dep.ready,
            container_id: dep.container_id.clone(),
            message: dep.message.clone(),
            last_exit: dep.last_exit.clone(),
            restart_count: dep.restart_count,
            restart_streak: dep.restart_streak,
            next_retry_at: dep.next_retry_at,
            healthy_since: dep.healthy_since,
            container_started_at: dep.container_started_at,
            ready_since: dep.ready_since,
            updated_at: dep.updated_at,
        }
    }

    fn write_back_deployment(self, dep: &mut StoredDeployment) {
        dep.status = self.status;
        dep.ready = self.ready;
        dep.container_id = self.container_id;
        dep.message = self.message;
        dep.last_exit = self.last_exit;
        dep.restart_count = self.restart_count;
        dep.restart_streak = self.restart_streak;
        dep.next_retry_at = self.next_retry_at;
        dep.healthy_since = self.healthy_since;
        dep.container_started_at = self.container_started_at;
        dep.ready_since = self.ready_since;
        dep.updated_at = self.updated_at;
    }

    fn from_replica(replica: &ReplicaState) -> Self {
        Self {
            status: replica.status.clone(),
            ready: replica.ready,
            container_id: replica.container_id.clone(),
            message: replica.message.clone(),
            last_exit: replica.last_exit.clone(),
            restart_count: replica.restart_count,
            restart_streak: replica.restart_streak,
            next_retry_at: replica.next_retry_at,
            healthy_since: replica.healthy_since,
            container_started_at: replica.container_started_at,
            ready_since: replica.ready_since,
            updated_at: replica.updated_at,
        }
    }

    fn write_back_replica(self, replica: &mut ReplicaState) {
        replica.status = self.status;
        replica.ready = self.ready;
        replica.container_id = self.container_id;
        replica.message = self.message;
        replica.last_exit = self.last_exit;
        replica.restart_count = self.restart_count;
        replica.restart_streak = self.restart_streak;
        replica.next_retry_at = self.next_retry_at;
        replica.healthy_since = self.healthy_since;
        replica.container_started_at = self.container_started_at;
        replica.ready_since = self.ready_since;
        replica.updated_at = self.updated_at;
    }
}

/// Apply a status transition to a set of fields. Returns true if anything
/// changed (the caller persists only then). `has_probe` is whether the
/// container declares a readiness probe (affects the healthy-without-probe path).
fn apply_status_transition(fields: &mut StatusFields, update: StatusUpdate, has_probe: bool) -> bool {
    let now = now_unix();
    let previous_container_id = fields.container_id.clone();
    let previous_ready = fields.ready;
    let snapshot = (
        fields.status.clone(),
        fields.ready,
        fields.container_id.clone(),
        fields.message.clone(),
        fields.last_exit.clone(),
        fields.restart_count,
        fields.restart_streak,
        fields.next_retry_at,
        fields.healthy_since,
        fields.container_started_at,
        fields.ready_since,
    );

    fields.status = update.status;
    fields.ready = update.ready;
    fields.container_id = update.container_id;
    fields.message = update.message;
    if let Some(last_exit) = update.last_exit {
        fields.last_exit = Some(last_exit);
    }
    if update.increment_restart_count {
        fields.restart_count = fields.restart_count.saturating_add(1);
    }
    if update.increment_restart_streak {
        fields.restart_streak = fields.restart_streak.saturating_add(1);
    }
    fields.next_retry_at = update.next_retry_at;

    if previous_container_id != fields.container_id {
        fields.container_started_at = fields.container_id.as_ref().map(|_| now);
        fields.ready_since = None;
    } else if fields.container_id.is_none() {
        fields.container_started_at = None;
        fields.ready_since = None;
    }

    if fields.ready {
        if !previous_ready {
            fields.ready_since = Some(now);
        }
    } else {
        fields.ready_since = None;
    }

    let counts_as_healthy = if fields.ready {
        true
    } else {
        fields.status == DeploymentStatus::Running && fields.container_id.is_some() && !has_probe
    };

    if counts_as_healthy {
        if fields.healthy_since.is_none() || previous_container_id != fields.container_id {
            fields.healthy_since = Some(now);
        }
        if fields
            .healthy_since
            .map(|healthy_since| now.saturating_sub(healthy_since) >= HEALTHY_RESET_AFTER_SECS)
            .unwrap_or(false)
        {
            fields.restart_streak = 0;
            fields.next_retry_at = None;
        }
    } else {
        fields.healthy_since = None;
    }

    let after = (
        fields.status.clone(),
        fields.ready,
        fields.container_id.clone(),
        fields.message.clone(),
        fields.last_exit.clone(),
        fields.restart_count,
        fields.restart_streak,
        fields.next_retry_at,
        fields.healthy_since,
        fields.container_started_at,
        fields.ready_since,
    );
    let changed = snapshot != after;
    if changed {
        fields.updated_at = now;
    }
    changed
}

/// Find a replica by ordinal, creating a fresh one if it doesn't exist yet.
fn ensure_replica(dep: &mut StoredDeployment, ordinal: u32) -> &mut ReplicaState {
    if let Some(index) = dep.replicas.iter().position(|r| r.ordinal == ordinal) {
        &mut dep.replicas[index]
    } else {
        dep.replicas.push(ReplicaState {
            ordinal,
            assigned_node: dep.assigned_node.clone(),
            ..ReplicaState::default()
        });
        dep.replicas.sort_by_key(|r| r.ordinal);
        let index = dep
            .replicas
            .iter()
            .position(|r| r.ordinal == ordinal)
            .expect("just inserted");
        &mut dep.replicas[index]
    }
}

/// Recompute the flat aggregate fields on `StoredDeployment` from its replicas,
/// so display code that reads the flat fields stays consistent.
fn recompute_aggregate(dep: &mut StoredDeployment) {
    if dep.replicas.is_empty() {
        return;
    }
    let all_ready = dep.replicas.iter().all(|r| r.ready);
    let any_failed = dep
        .replicas
        .iter()
        .any(|r| r.status == DeploymentStatus::Failed);
    let any_restarting = dep
        .replicas
        .iter()
        .any(|r| r.status == DeploymentStatus::Restarting);
    let any_pending = dep
        .replicas
        .iter()
        .any(|r| r.status == DeploymentStatus::Pending);
    // Worst-of aggregation: a single bad replica downgrades the whole deployment.
    dep.status = if any_failed {
        DeploymentStatus::Failed
    } else if any_restarting {
        DeploymentStatus::Restarting
    } else if any_pending {
        DeploymentStatus::Pending
    } else {
        DeploymentStatus::Running
    };
    dep.ready = all_ready;
    dep.restart_count = dep.replicas.iter().map(|r| r.restart_count).sum();
    let ordinal0 = dep.replicas.iter().find(|r| r.ordinal == 0);
    dep.container_id = ordinal0.and_then(|r| r.container_id.clone());
    dep.assigned_node = ordinal0
        .map(|r| r.assigned_node.clone())
        .unwrap_or_else(|| dep.assigned_node.clone());
    dep.updated_at = now_unix();
}

/// Mirror the aggregate (flat) fields onto the ordinal-0 replica. Used by the
/// legacy single-replica `set_status` path so the replica view doesn't diverge.
fn mirror_aggregate_to_replica0(dep: &mut StoredDeployment) {
    let fields = StatusFields::from_deployment(dep);
    let assigned_node = dep.assigned_node.clone();
    let replica = ensure_replica(dep, 0);
    replica.assigned_node = assigned_node;
    fields.write_back_replica(replica);
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn default_assigned_node() -> String {
    LOCAL_NODE_NAME.to_string()
}
