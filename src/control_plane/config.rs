use std::io::Cursor;

use super::{ClusterCommand, CommandResult};

openraft::declare_raft_types!(
    pub TypeConfig:
        D            = ClusterCommand,
        R            = CommandResult,
        NodeId       = u64,
        Node         = openraft::BasicNode,
        Entry        = openraft::Entry<TypeConfig>,
        SnapshotData = Cursor<Vec<u8>>,
        Responder    = openraft::impls::OneshotResponder<TypeConfig>,
        AsyncRuntime = openraft::TokioRuntime,
);
