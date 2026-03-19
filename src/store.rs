use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredDeployment {
    pub manifest: DeploymentManifest,
    pub spec_hash: String,
    pub status: DeploymentStatus,
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
    pub updated_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StateFile {
    pub deployments: BTreeMap<String, StoredDeployment>,
    #[serde(default)]
    pub nodes: BTreeMap<String, ClusterNode>,
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

pub struct Store {
    path: PathBuf,
    state: RwLock<StateFile>,
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
        state
            .nodes
            .entry(LOCAL_NODE_NAME.to_string())
            .or_insert_with(ClusterNode::local);

        Ok(Self {
            path,
            state: RwLock::new(state),
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
                status: DeploymentStatus::Pending,
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

    pub async fn upsert_node(&self, node: ClusterNode) -> Result<NodeApplyOutcome> {
        let mut guard = self.state.write().await;
        let name = node.name.clone();
        let outcome = if guard.nodes.contains_key(&name) {
            NodeApplyOutcome::Configured
        } else {
            NodeApplyOutcome::Created
        };
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

    pub async fn set_status(
        &self,
        name: &str,
        status: DeploymentStatus,
        container_id: Option<String>,
        message: Option<String>,
        last_exit: Option<String>,
        increment_restart_count: bool,
        increment_restart_streak: bool,
        next_retry_at: Option<u64>,
    ) -> Result<()> {
        let mut guard = self.state.write().await;
        let mut changed = false;
        if let Some(entry) = guard.deployments.get_mut(name) {
            let now = now_unix();
            let before = entry.clone();

            let previous_container_id = entry.container_id.clone();

            entry.status = status;
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

            if entry.status == DeploymentStatus::Running {
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
            return Ok(());
        }

        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot).await
    }

    async fn persist(&self, snapshot: &StateFile) -> Result<()> {
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

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
