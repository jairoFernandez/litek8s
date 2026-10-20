//! Server-to-server transport: HTTPS on the API port, pinned to the cluster
//! CA, authenticated with the cluster's join secret. Raft RPCs, forwarded
//! proposals and forwarded client requests all use it; connections are kept
//! open per peer (heartbeats go out every 100 ms).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use futures_util::TryStreamExt;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::client::conn::http1::SendRequest;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use openraft::error::{InstallSnapshotError, NetworkError, RPCError, RaftError, RemoteError, ReplicationClosed, StreamingError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse, SnapshotResponse, VoteRequest, VoteResponse};
use openraft::{Snapshot, Vote};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::types::{ServerNode, TypeConfig};

pub type ClientBody = BoxBody<Bytes, std::io::Error>;

pub const PREFIX: &str = "/apis/server.litek8s.dev/v1";
/// Header with the JSON (base64) of a snapshot's vote and metadata.
pub const SNAPSHOT_HEADER: &str = "x-litek8s-snapshot";
/// Set on requests one server forwards to another (loop guard).
pub const FORWARDED_HEADER: &str = "x-litek8s-forwarded";
/// The Raft log index a forwarded write's answer reflects: the forwarding
/// server waits until it has applied that far before answering.
pub const APPLIED_HEADER: &str = "x-litek8s-applied";

/// The request never reached the peer (it could not be connected to, or
/// the connection closed before the request went out): safe to retry
/// elsewhere.
#[derive(Debug)]
pub struct NotSent(pub String);

impl std::fmt::Display for NotSent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotSent {}

pub fn not_sent(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<NotSent>().is_some())
}

pub fn body(bytes: impl Into<Bytes>) -> ClientBody {
    Full::new(bytes.into()).map_err(|never| match never {}).boxed()
}

/// Which connections a request uses: API requests, or Raft's own (RPCs,
/// proposals, read indexes), which the other side serves on its Raft
/// runtime (TLS ALPN `litek8s-raft`), apart from API work that could delay
/// heartbeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    Api,
    Raft,
}

/// Connections to the other servers.
pub struct Peers {
    tls: Arc<rustls::ClientConfig>,
    raft_tls: Arc<rustls::ClientConfig>,
    secret: String,
    pool: Mutex<HashMap<(String, Lane), Vec<SendRequest<ClientBody>>>>,
    /// Peers this server cannot reach: a network partition, for tests.
    blocked: RwLock<HashSet<String>>,
    /// Raft-lane connections live on the Raft runtime.
    raft_runtime: std::sync::OnceLock<tokio::runtime::Handle>,
    /// Raft requests in flight per peer: a slow peer gets a few, not a new
    /// connection every heartbeat.
    raft_inflight: Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
}

/// Raft requests in flight to one peer at most.
const RAFT_INFLIGHT: usize = 4;

impl Peers {
    pub fn new(ca_pem: &str, secret: &str) -> Result<Self> {
        let tls = crate::kube::join::pinned_client_config(ca_pem)?;
        let mut raft = (*tls).clone();
        raft.alpn_protocols = vec![crate::kube::tls::RAFT_ALPN.to_vec()];
        Ok(Self {
            tls,
            raft_tls: Arc::new(raft),
            secret: secret.to_string(),
            pool: Mutex::default(),
            blocked: RwLock::default(),
            raft_runtime: std::sync::OnceLock::new(),
            raft_inflight: Mutex::default(),
        })
    }

    pub fn set_raft_runtime(&self, handle: tokio::runtime::Handle) {
        let _ = self.raft_runtime.set(handle);
    }

    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// Cut (or restore) the link to `addr` (tests).
    pub fn set_blocked(&self, addr: &str, blocked: bool) {
        let mut set = self.blocked.write().unwrap();
        if blocked {
            set.insert(addr.to_string());
            self.pool.lock().unwrap().retain(|(a, _), _| a != addr);
        } else {
            set.remove(addr);
        }
    }

    pub fn is_blocked(&self, addr: &str) -> bool {
        self.blocked.read().unwrap().contains(addr)
    }

    /// `addr` is the address to dial: a server's API address (Api lane) or
    /// its Raft address (Raft lane).
    async fn connect(&self, addr: &str, lane: Lane) -> Result<SendRequest<ClientBody>> {
        let dialed = match (lane, self.raft_runtime.get()) {
            (Lane::Raft, Some(rt)) => {
                let (addr, tls, blocked) = (addr.to_string(), self.raft_tls.clone(), self.is_blocked(addr));
                rt.spawn(async move { dial(&addr, tls, blocked).await }).await.map_err(|e| anyhow::anyhow!("{e}")).and_then(|r| r)
            }
            (Lane::Raft, None) => dial(addr, self.raft_tls.clone(), self.is_blocked(addr)).await,
            (Lane::Api, _) => dial(addr, self.tls.clone(), self.is_blocked(addr)).await,
        };
        dialed.map_err(|e| anyhow::Error::new(NotSent(format!("{e:#}"))))
    }

    fn take_pooled(&self, addr: &str, lane: Lane) -> Option<SendRequest<ClientBody>> {
        let mut pool = self.pool.lock().unwrap();
        let conns = pool.get_mut(&(addr.to_string(), lane))?;
        while let Some(c) = conns.pop() {
            if !c.is_closed() && c.is_ready() {
                return Some(c);
            }
        }
        None
    }

    fn put_back(&self, addr: &str, lane: Lane, conn: SendRequest<ClientBody>) {
        if conn.is_closed() || self.is_blocked(addr) {
            return;
        }
        let mut pool = self.pool.lock().unwrap();
        let conns = pool.entry((addr.to_string(), lane)).or_default();
        if conns.len() < 16 {
            conns.push(conn);
        }
    }

    fn build(&self, addr: &str, method: Method, path: &str, extra: &[(&str, String)], body: ClientBody) -> Result<Request<ClientBody>> {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(hyper::header::HOST, addr)
            .header(hyper::header::AUTHORIZATION, format!("Bearer {}", self.secret))
            .header(hyper::header::CONTENT_TYPE, "application/json");
        for (k, v) in extra {
            req = req.header(*k, v);
        }
        Ok(req.body(body)?)
    }

    /// One request with a small body, answered with a small body; reuses
    /// pooled connections.
    pub async fn call(&self, addr: &str, method: Method, path: &str, payload: Bytes, timeout: Duration) -> Result<(StatusCode, Bytes)> {
        self.call_api(addr, method, path, payload, timeout).await
    }

    fn call_api<'a>(&'a self, addr: &'a str, method: Method, path: &'a str, payload: Bytes, timeout: Duration) -> impl Future<Output = Result<(StatusCode, Bytes)>> + 'a {
        async move {
            let (parts, bytes) = self.send_on(Lane::Api, addr, timeout, || self.build(addr, method.clone(), path, &[], body(payload.clone()))).await?;
            Ok((parts.status, bytes))
        }
    }

    /// A request on `lane`. Raft requests run on the Raft runtime and are
    /// not cancelled when the caller gives up (openraft times RPCs out after
    /// one heartbeat): the connection then finishes the exchange and goes
    /// back to the pool instead of being torn down and dialed again.
    pub async fn call_on(self: &Arc<Self>, lane: Lane, addr: &str, method: Method, path: &str, payload: Bytes, timeout: Duration) -> Result<(StatusCode, Bytes)> {
        if lane == Lane::Api {
            return self.call_api(addr, method, path, payload, timeout).await;
        }
        let slots = self.raft_inflight.lock().unwrap().entry(addr.to_string()).or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(RAFT_INFLIGHT))).clone();
        let deadline = tokio::time::Instant::now() + timeout;
        let Ok(Ok(permit)) = tokio::time::timeout_at(deadline, slots.acquire_owned()).await else {
            bail!("request to {addr} timed out (too many in flight)");
        };
        let (me, addr_owned, path) = (self.clone(), addr.to_string(), path.to_string());
        let task = async move {
            let _permit = permit;
            let (parts, bytes) = me
                .send_on(Lane::Raft, &addr_owned, Duration::from_secs(30), || me.build(&addr_owned, method.clone(), &path, &[], body(payload.clone())))
                .await?;
            Ok::<_, anyhow::Error>((parts.status, bytes))
        };
        let handle = match self.raft_runtime.get() {
            Some(rt) => rt.spawn(task),
            None => tokio::spawn(task),
        };
        match tokio::time::timeout_at(deadline, handle).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => Err(anyhow::anyhow!("request to {addr} failed: {e}")),
            Err(_) => Err(anyhow::anyhow!("request to {addr} timed out")),
        }
    }

    /// Send the request `make` builds on a pooled connection (again on a
    /// fresh one if the pooled one turns out closed) and read the answer.
    pub async fn send(&self, addr: &str, timeout: Duration, make: impl Fn() -> Result<Request<ClientBody>>) -> Result<(hyper::http::response::Parts, Bytes)> {
        self.send_on(Lane::Api, addr, timeout, make).await
    }

    pub async fn send_on(&self, lane: Lane, addr: &str, timeout: Duration, make: impl Fn() -> Result<Request<ClientBody>>) -> Result<(hyper::http::response::Parts, Bytes)> {
        let attempt = async {
            if self.is_blocked(addr) {
                return Err(anyhow::Error::new(NotSent(format!("{addr} is unreachable (partitioned)"))));
            }
            for fresh in [false, true] {
                let pooled = if fresh { None } else { self.take_pooled(addr, lane) };
                let mut conn = match pooled {
                    Some(c) => c,
                    None => self.connect(addr, lane).await?,
                };
                let resp = match conn.send_request(make()?).await {
                    Ok(r) => r,
                    // The connection closed before the request went out.
                    Err(e) if e.is_canceled() && !fresh => continue,
                    Err(e) if e.is_canceled() => return Err(anyhow::Error::new(NotSent(format!("request to {addr} failed: {e}")))),
                    Err(e) => return Err(e).with_context(|| format!("request to {addr} failed")),
                };
                let (parts, body) = resp.into_parts();
                let bytes = body.collect().await?.to_bytes();
                self.put_back(addr, lane, conn);
                return Ok((parts, bytes));
            }
            bail!("request to {addr} failed")
        };
        tokio::time::timeout(timeout, attempt).await.with_context(|| format!("request to {addr} timed out"))?
    }

    /// A JSON request to a peer's `server.litek8s.dev` endpoint.
    pub async fn call_json<T: DeserializeOwned>(&self, addr: &str, method: Method, path: &str, payload: &impl Serialize, timeout: Duration) -> Result<T> {
        let (status, bytes) = self.call_api(addr, method, &format!("{PREFIX}{path}"), serde_json::to_vec(payload)?.into(), timeout).await?;
        json_answer(addr, status, &bytes)
    }

    pub async fn call_json_on<T: DeserializeOwned>(self: &Arc<Self>, lane: Lane, addr: &str, method: Method, path: &str, payload: &impl Serialize, timeout: Duration) -> Result<T> {
        let (status, bytes) = self.call_on(lane, addr, method, &format!("{PREFIX}{path}"), serde_json::to_vec(payload)?.into(), timeout).await?;
        json_answer(addr, status, &bytes)
    }

    /// A request on a connection of its own, its answer streamed (logs,
    /// upgrades, snapshots).
    pub async fn open(&self, addr: &str, req: Request<ClientBody>) -> Result<Response<Incoming>> {
        self.open_on(Lane::Api, addr, req).await
    }

    pub async fn open_on(&self, lane: Lane, addr: &str, req: Request<ClientBody>) -> Result<Response<Incoming>> {
        let mut conn = self.connect(addr, lane).await?;
        Ok(conn.send_request(req).await?)
    }

    pub fn request(&self, addr: &str, method: Method, path: &str, extra: &[(&str, String)], body: ClientBody) -> Result<Request<ClientBody>> {
        self.build(addr, method, path, extra, body)
    }
}

fn json_answer<T: DeserializeOwned>(addr: &str, status: StatusCode, bytes: &[u8]) -> Result<T> {
    if !status.is_success() {
        bail!("{addr} answered {status}: {}", String::from_utf8_lossy(bytes).trim());
    }
    serde_json::from_slice(bytes).with_context(|| format!("invalid answer from {addr}"))
}

/// A TLS connection to a peer, ready for HTTP/1.1 requests.
async fn dial(addr: &str, tls: Arc<rustls::ClientConfig>, blocked: bool) -> Result<SendRequest<ClientBody>> {
    if blocked {
        bail!("{addr} is unreachable (partitioned)");
    }
        let tcp = tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(addr))
            .await
            .with_context(|| format!("timed out connecting to {addr}"))?
            .with_context(|| format!("failed to connect to {addr}"))?;
        let _ = tcp.set_nodelay(true);
        let keepalive = socket2::TcpKeepalive::new().with_time(Duration::from_secs(10)).with_interval(Duration::from_secs(3));
        let _ = socket2::SockRef::from(&tcp).set_tcp_keepalive(&keepalive);
        let name = rustls::pki_types::ServerName::try_from(crate::kube::join::SERVER_NAME)?;
        let tls = tokio::time::timeout(Duration::from_secs(5), tokio_rustls::TlsConnector::from(tls).connect(name, tcp))
            .await
            .with_context(|| format!("TLS handshake with {addr} timed out"))??;
        let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls)).await?;
        tokio::spawn(async move {
            let _ = conn.with_upgrades().await;
        });
        Ok(sender)
    }

/// Raft's view of the network.
pub struct Network {
    pub peers: Arc<Peers>,
}

pub struct Conn {
    peers: Arc<Peers>,
    target: u64,
    /// The peer's Raft address.
    addr: String,
}

impl RaftNetworkFactory<TypeConfig> for Network {
    type Network = Conn;

    async fn new_client(&mut self, target: u64, node: &ServerNode) -> Conn {
        Conn { peers: self.peers.clone(), target, addr: node.raft_addr() }
    }
}

type RpcErr<E = openraft::error::Infallible> = RPCError<u64, ServerNode, RaftError<u64, E>>;

/// The largest AppendEntries request sent at once: openraft gives each one
/// a heartbeat interval to complete, so a batch of big objects (a chart's
/// CRDs) is split instead of timing out again and again.
const MAX_APPEND_BYTES: usize = 512 << 10;

impl Conn {
    async fn rpc<Req: Serialize, Resp: DeserializeOwned, E: std::error::Error + DeserializeOwned>(&self, path: &str, req: &Req, option: &RPCOption) -> Result<Resp, RpcErr<E>> {
        let bytes = serde_json::to_vec(req).map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        self.send_rpc(path, bytes, option).await
    }

    async fn send_rpc<Resp: DeserializeOwned, E: std::error::Error + DeserializeOwned>(&self, path: &str, bytes: Vec<u8>, option: &RPCOption) -> Result<Resp, RpcErr<E>> {
        let (status, body) = self
            .peers
            .call_on(Lane::Raft, &self.addr, Method::POST, &format!("{PREFIX}/raft/{path}"), bytes.into(), option.hard_ttl())
            .await
            .map_err(|e| {
                let io = std::io::Error::other(format!("{e:#}"));
                // Unreachable makes openraft back off (no heartbeats for a
                // while): only for a peer that cannot be connected to.
                if not_sent(&e) { RPCError::Unreachable(Unreachable::new(&io)) } else { RPCError::Network(NetworkError::new(&io)) }
            })?;
        if !status.is_success() {
            let e = std::io::Error::other(format!("{} answered {status}: {}", self.addr, String::from_utf8_lossy(&body)));
            return Err(RPCError::Network(NetworkError::new(&e)));
        }
        let result: Result<Resp, RaftError<u64, E>> = serde_json::from_slice(&body).map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        result.map_err(|e| RPCError::RemoteError(RemoteError::new(self.target, e)))
    }
}

impl RaftNetwork<TypeConfig> for Conn {
    async fn append_entries(&mut self, rpc: AppendEntriesRequest<TypeConfig>, option: RPCOption) -> Result<AppendEntriesResponse<u64>, RpcErr> {
        let bytes = serde_json::to_vec(&rpc).map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        if bytes.len() > MAX_APPEND_BYTES && rpc.entries.len() > 1 {
            let fit = (rpc.entries.len() * MAX_APPEND_BYTES / bytes.len()).max(1);
            return Err(RPCError::PayloadTooLarge(openraft::error::PayloadTooLarge::new_entries_hint(fit as u64)));
        }
        self.send_rpc("append", bytes, &option).await
    }

    async fn vote(&mut self, rpc: VoteRequest<u64>, option: RPCOption) -> Result<VoteResponse<u64>, RpcErr> {
        self.rpc("vote", &rpc, &option).await
    }

    async fn install_snapshot(&mut self, _rpc: InstallSnapshotRequest<TypeConfig>, _option: RPCOption) -> Result<InstallSnapshotResponse<u64>, RpcErr<InstallSnapshotError>> {
        // Snapshots go whole, through full_snapshot.
        Err(RPCError::Network(NetworkError::new(&std::io::Error::other("chunked snapshots are not supported"))))
    }

    async fn full_snapshot(
        &mut self,
        vote: Vote<u64>,
        snapshot: Snapshot<TypeConfig>,
        cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        option: RPCOption,
    ) -> Result<SnapshotResponse<u64>, StreamingError<TypeConfig, openraft::error::Fatal<u64>>> {
        let unreachable = |e: anyhow::Error| StreamingError::Unreachable(Unreachable::new(&std::io::Error::other(format!("{e:#}"))));
        let header = serde_json::to_vec(&(vote, &snapshot.meta)).map_err(|e| unreachable(e.into()))?;
        let header = base64::engine::general_purpose::STANDARD.encode(header);
        let file = *snapshot.snapshot;
        let stream = tokio_util_stream(file);
        let req = self
            .peers
            .request(&self.addr, Method::POST, &format!("{PREFIX}/raft/snapshot"), &[(SNAPSHOT_HEADER, header)], BodyExt::boxed(StreamBody::new(stream)))
            .map_err(unreachable)?;
        let send = async {
            let resp = self.peers.open_on(Lane::Raft, &self.addr, req).await?;
            let status = resp.status();
            let bytes = resp.into_body().collect().await?.to_bytes();
            if !status.is_success() {
                bail!("{} answered {status}: {}", self.addr, String::from_utf8_lossy(&bytes));
            }
            let result: Result<SnapshotResponse<u64>, RaftError<u64>> = serde_json::from_slice(&bytes)?;
            result.map_err(|e| anyhow::anyhow!("{e}"))
        };
        tokio::select! {
            r = tokio::time::timeout(option.hard_ttl().max(Duration::from_secs(600)), send) => match r {
                Ok(Ok(resp)) => Ok(resp),
                Ok(Err(e)) => Err(unreachable(e)),
                Err(_) => Err(unreachable(anyhow::anyhow!("sending the snapshot timed out"))),
            },
            closed = cancel => Err(StreamingError::Closed(closed)),
        }
    }

    fn backoff(&self) -> openraft::network::Backoff {
        openraft::network::Backoff::new(std::iter::repeat(Duration::from_millis(100)))
    }
}

/// A file as a stream of body frames.
fn tokio_util_stream(mut file: tokio::fs::File) -> impl futures_util::Stream<Item = Result<Frame<Bytes>, std::io::Error>> + Send + Sync + 'static {
    use tokio::io::AsyncReadExt;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    tokio::spawn(async move {
        let _ = tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(0)).await;
        loop {
            let mut buf = vec![0u8; 256 * 1024];
            match file.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    buf.truncate(n);
                    if tx.send(Ok(Bytes::from(buf))).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    break;
                }
            }
        }
    });
    futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) }).map_ok(Frame::data)
}
