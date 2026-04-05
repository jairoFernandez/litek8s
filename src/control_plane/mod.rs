pub mod config;
pub mod network;
pub mod raft;
pub mod storage;

use std::collections::BTreeMap;

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::store::{DeploymentStatus, EventLevel, StoredEvent};

pub type ControlPlaneId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlPlanePeer {
    pub id: ControlPlaneId,
    pub addr: String,
    #[serde(default)]
    pub control_plane_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClusterCommand {
    ApplyDeployment {
        manifest_yaml: String,
    },
    DeleteDeployment {
        name: String,
    },
    ApplyService {
        manifest_yaml: String,
    },
    DeleteService {
        name: String,
    },
    AddWorkerNode {
        name: String,
        ssh_target: String,
        remote_binary: Option<String>,
        labels: BTreeMap<String, String>,
    },
    SetWorkerNodeScheduling {
        name: String,
        unschedulable: bool,
    },
    RemoveWorkerNode {
        name: String,
    },
    SetDeploymentAssignment {
        name: String,
        assigned_node: String,
    },
    SetDeploymentStatus {
        name: String,
        status: DeploymentStatus,
        ready: bool,
        container_id: Option<String>,
        message: Option<String>,
        last_exit: Option<String>,
        increment_restart_count: bool,
        increment_restart_streak: bool,
        next_retry_at: Option<u64>,
    },
    AppendEvent {
        event: StoredEvent,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CommandResult {
    Ack {
        message: Option<String>,
    },
    EventRecorded {
        id: u64,
    },
    Noop,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NewReplicatedEvent {
    pub timestamp_secs: u64,
    pub level: EventLevel,
    pub reason: String,
    pub object_kind: String,
    pub object_name: String,
    pub node_name: Option<String>,
    pub message: String,
}

pub fn parse_peer_spec(raw: &str) -> Result<(String, String)> {
    let (id, addr) = raw
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("peer `{raw}` must use ID=HOST:PORT syntax"))?;
    ensure!(!id.trim().is_empty(), "peer id must not be empty");
    ensure!(!addr.trim().is_empty(), "peer address must not be empty");
    Ok((id.trim().to_string(), addr.trim().to_string()))
}

pub fn validate_peer_map(
    node_id: &str,
    _listen_addr: &str,
    peers: &BTreeMap<String, String>,
) -> Result<()> {
    ensure!(
        !node_id.trim().is_empty(),
        "control-plane id must not be empty in HA mode"
    );
    if peers.is_empty() {
        bail!("at least one --peer entry is required in HA mode");
    }
    if !peers.contains_key(node_id) {
        bail!("peer list must include this control-plane id `{node_id}`");
    }
    Ok(())
}
