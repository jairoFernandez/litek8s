use std::fmt;
use std::future::Future;
use std::io;
use std::time::Duration;

use openraft::BasicNode;
use openraft::Snapshot;
use openraft::Vote;
use openraft::error::InstallSnapshotError;
use openraft::error::RPCError;
use openraft::error::RaftError;
use openraft::error::ReplicationClosed;
use openraft::error::StreamingError;
use openraft::network::RPCOption;
use openraft::network::RaftNetwork;
use openraft::network::RaftNetworkFactory;
use openraft::raft::AppendEntriesRequest;
use openraft::raft::AppendEntriesResponse;
use openraft::raft::InstallSnapshotRequest;
use openraft::raft::InstallSnapshotResponse;
use openraft::raft::SnapshotResponse;
use openraft::raft::VoteRequest;
use openraft::raft::VoteResponse;

use super::config::TypeConfig;

// ---------------------------------------------------------------------------
// Simple error type that implements std::error::Error (unlike anyhow::Error)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct RpcTransportError(String);

impl fmt::Display for RpcTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for RpcTransportError {}

// ---------------------------------------------------------------------------
// RPC envelope sent over TCP between peers
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum RaftRpc {
    AppendEntries(AppendEntriesRequest<TypeConfig>),
    Vote(VoteRequest<u64>),
    InstallSnapshot(InstallSnapshotRequest<TypeConfig>),
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum RaftRpcResponse {
    AppendEntries(AppendEntriesResponse<u64>),
    Vote(VoteResponse<u64>),
    InstallSnapshot(InstallSnapshotResponse<u64>),
}

// ---------------------------------------------------------------------------
// Network factory
// ---------------------------------------------------------------------------

pub struct NetworkFactory;

impl RaftNetworkFactory<TypeConfig> for NetworkFactory {
    type Network = NetworkClient;

    async fn new_client(&mut self, _target: u64, node: &BasicNode) -> Self::Network {
        NetworkClient {
            addr: node.addr.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Network client (one per peer)
// ---------------------------------------------------------------------------

pub struct NetworkClient {
    addr: String,
}

impl NetworkClient {
    async fn send_rpc(&self, rpc: RaftRpc) -> io::Result<RaftRpcResponse> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut stream = tokio::net::TcpStream::connect(&self.addr).await?;
        let req_bytes =
            serde_json::to_vec(&rpc).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // Length-prefixed JSON (same pattern as protocol.rs)
        let len = req_bytes.len() as u32;
        stream.write_all(&len.to_be_bytes()).await?;
        stream.write_all(&req_bytes).await?;
        stream.flush().await?;

        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let resp_len = u32::from_be_bytes(len_buf) as usize;

        let mut resp_buf = vec![0u8; resp_len];
        stream.read_exact(&mut resp_buf).await?;
        let resp: RaftRpcResponse = serde_json::from_slice(&resp_buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(resp)
    }
}

impl RaftNetwork<TypeConfig> for NetworkClient {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        match self.send_rpc(RaftRpc::AppendEntries(rpc)).await {
            Ok(RaftRpcResponse::AppendEntries(resp)) => Ok(resp),
            Ok(_) => Err(RPCError::Network(openraft::error::NetworkError::new(
                &RpcTransportError("unexpected response type".into()),
            ))),
            Err(e) => Err(RPCError::Network(openraft::error::NetworkError::new(&e))),
        }
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, BasicNode, RaftError<u64, InstallSnapshotError>>,
    > {
        match self.send_rpc(RaftRpc::InstallSnapshot(rpc)).await {
            Ok(RaftRpcResponse::InstallSnapshot(resp)) => Ok(resp),
            Ok(_) => Err(RPCError::Network(openraft::error::NetworkError::new(
                &RpcTransportError("unexpected response type".into()),
            ))),
            Err(e) => Err(RPCError::Network(openraft::error::NetworkError::new(&e))),
        }
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<u64>,
        _option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        match self.send_rpc(RaftRpc::Vote(rpc)).await {
            Ok(RaftRpcResponse::Vote(resp)) => Ok(resp),
            Ok(_) => Err(RPCError::Network(openraft::error::NetworkError::new(
                &RpcTransportError("unexpected response type".into()),
            ))),
            Err(e) => Err(RPCError::Network(openraft::error::NetworkError::new(&e))),
        }
    }

    async fn full_snapshot(
        &mut self,
        vote: Vote<u64>,
        snapshot: Snapshot<TypeConfig>,
        _cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        _option: RPCOption,
    ) -> Result<SnapshotResponse<u64>, StreamingError<TypeConfig, openraft::error::Fatal<u64>>>
    {
        use std::io::Read;
        let mut data = Vec::new();
        let mut cursor = *snapshot.snapshot;
        cursor
            .read_to_end(&mut data)
            .map_err(|e| StreamingError::Unreachable(openraft::error::Unreachable::new(&e)))?;

        let rpc = InstallSnapshotRequest {
            vote,
            meta: snapshot.meta,
            offset: 0,
            data,
            done: true,
        };

        match self.send_rpc(RaftRpc::InstallSnapshot(rpc)).await {
            Ok(RaftRpcResponse::InstallSnapshot(resp)) => Ok(SnapshotResponse::new(resp.vote)),
            Ok(_) => Err(StreamingError::Unreachable(
                openraft::error::Unreachable::new(&RpcTransportError(
                    "unexpected response type".into(),
                )),
            )),
            Err(e) => Err(StreamingError::Unreachable(
                openraft::error::Unreachable::new(&e),
            )),
        }
    }

    fn backoff(&self) -> openraft::network::Backoff {
        openraft::network::Backoff::new(std::iter::repeat(Duration::from_millis(500)))
    }
}
