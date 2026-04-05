use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use openraft::BasicNode;
use openraft::Config;
use openraft::Raft;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::config::TypeConfig;
use super::network::{NetworkFactory, RaftRpc, RaftRpcResponse};
use super::storage::{LogStore, StateMachineStore};
use super::{ClusterCommand, CommandResult};
use crate::store::Store;

pub type ControlPlaneRaft = Raft<TypeConfig>;

/// A running control-plane Raft node.
pub struct ControlPlaneNode {
    pub raft: ControlPlaneRaft,
    pub node_id: u64,
    pub store: Arc<Store>,
}

impl ControlPlaneNode {
    /// Start a Raft node.
    ///
    /// If `join_addr` is `None`, the node initializes itself as a single-node
    /// cluster (becomes leader immediately).  If `join_addr` is `Some`, the
    /// node waits to be added by an existing leader — it does NOT self-initialize.
    pub async fn start(
        node_id: u64,
        peer_addr: String,
        store: Arc<Store>,
        join_addr: Option<String>,
    ) -> Result<Self> {
        let config = Config {
            cluster_name: "litek8s".into(),
            heartbeat_interval: 500,
            election_timeout_min: 1500,
            election_timeout_max: 3000,
            ..Default::default()
        };

        let log_store = LogStore::new();
        let state_machine = StateMachineStore::new(Arc::clone(&store));
        let network = NetworkFactory;

        let raft = Raft::new(
            node_id,
            Arc::new(config),
            network,
            log_store,
            state_machine,
        )
        .await
        .context("failed to create raft node")?;

        let node = Self {
            raft,
            node_id,
            store,
        };

        if join_addr.is_none() && !node.raft.is_initialized().await? {
            let mut members = BTreeMap::new();
            members.insert(node_id, BasicNode { addr: peer_addr });
            node.raft
                .initialize(members)
                .await
                .context("failed to initialize single-node raft cluster")?;
        }

        Ok(node)
    }

    /// Add a new member to the Raft cluster (leader only).
    pub async fn add_member(&self, new_node_id: u64, addr: String) -> Result<()> {
        use openraft::ChangeMembers;

        let mut new_voters = BTreeMap::new();
        new_voters.insert(new_node_id, BasicNode { addr });

        self.raft
            .change_membership(ChangeMembers::AddVoters(new_voters), false)
            .await
            .context("failed to add member to raft cluster")?;

        Ok(())
    }

    /// Propose a command through the Raft log (leader only).
    pub async fn propose(&self, cmd: ClusterCommand) -> Result<CommandResult> {
        let resp = self
            .raft
            .client_write(cmd)
            .await
            .context("raft client_write failed")?;
        Ok(resp.data)
    }

    /// Spawn a TCP listener that handles incoming Raft RPCs from peers.
    pub async fn spawn_rpc_listener(
        &self,
        listen_addr: &str,
    ) -> Result<tokio::task::JoinHandle<()>> {
        let listener = TcpListener::bind(listen_addr)
            .await
            .with_context(|| format!("failed to bind raft listener on {listen_addr}"))?;

        let raft = self.raft.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let raft = raft.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_rpc_connection(stream, raft).await {
                        eprintln!("raft rpc error: {e}");
                    }
                });
            }
        });

        Ok(handle)
    }
}

async fn handle_rpc_connection(
    mut stream: tokio::net::TcpStream,
    raft: ControlPlaneRaft,
) -> Result<()> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;

    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    let rpc: RaftRpc = serde_json::from_slice(&buf)?;

    let response = match rpc {
        RaftRpc::AppendEntries(req) => {
            let resp = raft
                .append_entries(req)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            RaftRpcResponse::AppendEntries(resp)
        }
        RaftRpc::Vote(req) => {
            let resp = raft
                .vote(req)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            RaftRpcResponse::Vote(resp)
        }
        RaftRpc::InstallSnapshot(req) => {
            let resp = raft
                .install_snapshot(req)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            RaftRpcResponse::InstallSnapshot(resp)
        }
    };

    let resp_bytes = serde_json::to_vec(&response)?;
    let resp_len = resp_bytes.len() as u32;
    stream.write_all(&resp_len.to_be_bytes()).await?;
    stream.write_all(&resp_bytes).await?;
    stream.flush().await?;

    Ok(())
}
