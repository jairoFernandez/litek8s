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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DeploymentStatus {
    Pending,
    Running,
    Restarting,
    Failed,
    Deleted,
}

const HEALTHY_RESET_AFTER_SECS: u64 = 10;
const MAX_EVENT_HISTORY: usize = 250;

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
        let spec_hash = manifest.desired_hash()?;

        let mut guard = self.state.write().await;
        let existing = guard.deployments.get(&name).cloned();
        let outcome = if existing.is_some() {
            ApplyOutcome::Configured
        } else {
            ApplyOutcome::Created
        };

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
        let mut guard = self.state.write().await;
        let mut changed = false;
        if let Some(entry) = guard.deployments.get_mut(name) {
            let now = now_unix();
            let before = entry.clone();

            let previous_container_id = entry.container_id.clone();
            let previous_ready = entry.ready;

            entry.status = status;
            entry.ready = ready;
            entry.container_id = container_id;
            entry.message = message;
            if let Some(last_exit) = last_exit {
                entry.last_exit = Some(last_exit);
            }
            if increment_restart_count {
                entry.restart_count = entry.restart_count.saturating_add(1);
            }
            if increment_restart_streak {
                entry.restart_streak = entry.restart_streak.saturating_add(1);
            }
            entry.next_retry_at = next_retry_at;

            if previous_container_id != entry.container_id {
                entry.container_started_at = entry.container_id.as_ref().map(|_| now);
                entry.ready_since = None;
            } else if entry.container_id.is_none() {
                entry.container_started_at = None;
                entry.ready_since = None;
            }

            if entry.ready {
                if !previous_ready {
                    entry.ready_since = Some(now);
                }
            } else {
                entry.ready_since = None;
            }

            let counts_as_healthy = if entry.ready {
                true
            } else {
                entry.status == DeploymentStatus::Running
                    && entry.container_id.is_some()
                    && entry.manifest.container().readiness_probe().is_none()
            };

            if counts_as_healthy {
                if entry.healthy_since.is_none() || previous_container_id != entry.container_id {
                    entry.healthy_since = Some(now);
                }

                if entry
                    .healthy_since
                    .map(|healthy_since| {
                        now.saturating_sub(healthy_since) >= HEALTHY_RESET_AFTER_SECS
                    })
                    .unwrap_or(false)
                {
                    entry.restart_streak = 0;
                    entry.next_retry_at = None;
                }
            } else {
                entry.healthy_since = None;
            }

            changed = *entry != before;
            if changed {
                entry.updated_at = now;
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

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn default_assigned_node() -> String {
    LOCAL_NODE_NAME.to_string()
}
