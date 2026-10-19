//! The Raft type configuration of the replicated object store.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::kube::store::{Mutation, StoreError};

/// A member of the control plane.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerNode {
    /// Its name (the hostname or `--node-name`), for `litek8s server list`.
    pub name: String,
    /// `host:port` of its API: Raft RPCs and forwarded requests go there.
    pub addr: String,
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
