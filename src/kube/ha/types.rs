//! The Raft type configuration of the replicated object store.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::kube::store::{Mutation, StoreError};

/// A member of the control plane.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerNode {
    /// Its name (the hostname or `--node-name`), for `litek8s server list`.
    pub name: String,
    /// `host:port` of its API: forwarded requests go there.
    pub addr: String,
    /// `host:port` of its Raft listener (default: the API port + 1).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub raft: String,
}

impl ServerNode {
    pub fn raft_addr(&self) -> String {
        if !self.raft.is_empty() {
            return self.raft.clone();
        }
        match self.addr.rsplit_once(':').and_then(|(h, p)| Some((h, p.parse::<u16>().ok()?))) {
            Some((host, port)) => format!("{host}:{}", port.wrapping_add(1)),
            None => self.addr.clone(),
        }
    }
}

/// One log entry's payload: a store mutation, tagged so a retried
/// proposal is applied once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command {
    /// The proposing process (random per start).
    pub session: u64,
    pub seq: u64,
    /// Every seq of this session below this one has been answered: the
    /// state machine forgets their outcomes.
    pub acked: u64,
    pub mutation: Mutation,
}

/// What applying an entry gave: `None` for blank and membership entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Applied(pub Option<Result<Value, StoreError>>);

openraft::declare_raft_types!(
    pub TypeConfig:
        D            = Command,
        R            = Applied,
        NodeId       = u64,
        Node         = ServerNode,
        Entry        = openraft::Entry<TypeConfig>,
        SnapshotData = tokio::fs::File,
        Responder    = openraft::impls::OneshotResponder<TypeConfig>,
        AsyncRuntime = openraft::TokioRuntime,
);
