use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::config;
use crate::manifest::DeploymentManifest;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DeploymentStatus {
    Pending,
    Running,
    Restarting,
    Failed,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub updated_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StateFile {
    pub deployments: BTreeMap<String, StoredDeployment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
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
                last_exit: existing.and_then(|entry| entry.last_exit),
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

    pub async fn set_status(
        &self,
        name: &str,
        status: DeploymentStatus,
        container_id: Option<String>,
        message: Option<String>,
        last_exit: Option<String>,
        increment_restart_count: bool,
    ) -> Result<()> {
        let mut guard = self.state.write().await;
        if let Some(entry) = guard.deployments.get_mut(name) {
            entry.status = status;
            entry.container_id = container_id;
            entry.message = message;
            if let Some(last_exit) = last_exit {
                entry.last_exit = Some(last_exit);
            }
            if increment_restart_count {
                entry.restart_count = entry.restart_count.saturating_add(1);
            }
            entry.updated_at = now_unix();
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
