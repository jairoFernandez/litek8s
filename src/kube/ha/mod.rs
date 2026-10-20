//! High availability: several API servers sharing one object store through
//! Raft (openraft), like k3s with embedded etcd.
//!
//! - The Raft log carries store [`Mutation`]s; every server applies them to
//!   its in-memory [`Store`] (`sm.rs`), so every server has every object, the
//!   same resourceVersions and the same watch history. The log lives on
//!   disk (`log.rs`); snapshots (`sm.rs`) replace the single-server WAL
//!   compaction.
//! - A write anywhere becomes a proposal ([`Cluster`] is the store's
//!   [`Replicator`]): on the leader it goes into the log, on a follower it is
//!   forwarded to the leader (`POST .../propose`) and the follower answers
//!   once it has applied the entry itself (read-your-writes). Proposals carry
//!   a (session, seq) id, so a retry after a leader died mid-write applies
//!   once. Client write requests are forwarded to the leader whole (see
//!   `server.rs`), so allocations kept in memory (ClusterIPs, pod CIDRs) stay
//!   on one server.
//! - Reads are served from the local store. A get/list/watch with
//!   `resourceVersion=""` (the default, "most recent") first passes a read
//!   barrier: the leader confirms it still leads (a heartbeat round) and
//!   names its commit index, and the local store waits until it has applied
//!   that far: a linearizable read, as kube-apiserver's quorum reads on etcd.
//! - Controllers, the scheduler, node lifecycle and other singletons run
//!   only on the leader ([`Cluster::run_while_leader`]); their writes are
//!   never forwarded, so a deposed leader's controllers cannot write.
//!
//! Servers talk HTTPS on the API port, pinned to the cluster CA and
//! authenticated with the join secret (`net.rs`).

pub mod lb;
pub mod lincheck;
pub mod log;
pub mod net;
pub mod sm;
pub mod types;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use openraft::error::{ClientWriteError, RaftError};
use openraft::{ChangeMembers, Config, Raft, ServerState, SnapshotPolicy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;

use self::net::{Network, PREFIX, Peers};
use self::types::{Command, ServerNode, TypeConfig};
use crate::kube::server::{ApiState, Body, full};
use crate::kube::store::{Mutation, Replicator, Store, StoreError};

/// How long a write waits for a leader and a commit before giving up (the
/// client gets a 504 and may retry; the write may still have happened).
const PROPOSE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a read waits for the read barrier.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Raft request bodies (log entries carry whole objects).
const MAX_RPC_BYTES: usize = 512 << 20;

tokio::task_local! {
    /// Set while the leader-only singletons run: their writes must not be
    /// forwarded if this server stops leading.
    static LEADER_ONLY: ();
}

/// How this server takes part in the cluster.
pub struct Options {
    pub data_dir: PathBuf,
    /// This server's name (its node name).
    pub name: String,
    /// `host:port` the other servers reach this one's API at.
    pub advertise: String,
    /// `host:port` they reach its Raft listener at.
    pub advertise_raft: String,
    /// Start a new cluster (the first server), moving a single server's
    /// objects into it.
    pub cluster_init: bool,
    /// Join the cluster of this server (`https://host:port`).
    pub join: Option<String>,
}

pub struct Cluster {
    pub id: u64,
    pub name: String,
    pub addr: String,
    raft_addr: String,
    pub raft: Raft<TypeConfig>,
    pub peers: Arc<Peers>,
    /// Where Raft and its connections run.
    runtime: tokio::runtime::Handle,
    data_dir: PathBuf,
    raft_dir: PathBuf,
    session: u64,
    seq: AtomicU64,
    inflight: Mutex<BTreeSet<u64>>,
    reads: ReadBatcher,
    leader: watch::Receiver<bool>,
    /// One membership change at a time.
    membership_lock: tokio::sync::Mutex<()>,
    /// When the leader last reached this server (ms since `started`).
    leader_contact: AtomicU64,
    started: std::time::Instant,
}

/// What a joining server needs to be one of the cluster's servers.
#[derive(Serialize, Deserialize)]
struct BootstrapData {
    tls: String,
    /// base64 of the PKCS#8 key.
    sa_key: String,
    token: String,
}

#[derive(Serialize, Deserialize)]
struct ProposeAnswer {
    result: Result<Value, StoreError>,
    index: u64,
}

#[derive(Serialize, Deserialize)]
struct MemberRequest {
    id: u64,
    name: String,
    addr: String,
    #[serde(default)]
    raft: String,
}

/// One server, as `litek8s server list` shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub id: u64,
    pub name: String,
    pub addr: String,
    pub voter: bool,
    pub leader: bool,
    /// The last log index the leader knows this server has (leader's view).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterStatus {
    /// "ok", or the error that stopped Raft on this server.
    #[serde(default)]
    pub raft: String,
    pub servers: Vec<ServerInfo>,
    pub term: u64,
    pub last_log_index: Option<u64>,
    pub applied: Option<u64>,
}

fn data_file(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

/// Whether `data_dir` already belongs to a cluster.
pub fn is_member(data_dir: &Path) -> bool {
    data_dir.join("raft").join("id").exists()
}

/// `https://host:port` (or `host[:port]`) → `host:port`.
pub fn authority(url: &str) -> String {
    let rest = url.trim_end_matches('/');
    let rest = rest.strip_prefix("https://").or_else(|| rest.strip_prefix("http://")).unwrap_or(rest);
    let rest = rest.split('/').next().unwrap_or(rest);
    if rest.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) && !rest.ends_with(']') {
        rest.to_string()
    } else {
        format!("{rest}:6443")
    }
}

/// Before a joining server opens its state: fetch the cluster's CA,
/// serving certificate and keys from `server` (checked against the token's
/// CA hash), so this server serves as the same cluster.
pub async fn fetch_bootstrap(data_dir: &Path, server: &str, token: &str) -> Result<()> {
    let token = crate::kube::join::Token::parse(token)?;
    let addr = authority(server);
    let ca = crate::kube::join::fetch_ca(&addr, &token).await.with_context(|| format!("cannot reach {server}"))?;
    let peers = Peers::new(&ca, &token.secret)?;
    let data: BootstrapData = peers
        .call_json(&addr, Method::POST, "/bootstrap", &json!({}), Duration::from_secs(30))
        .await
        .context("the server refused to bootstrap this one (wrong token?)")?;
    std::fs::create_dir_all(data_dir)?;
    std::fs::write(data_file(data_dir, "tls.json"), &data.tls)?;
    std::fs::write(data_file(data_dir, "sa.key"), base64::engine::general_purpose::STANDARD.decode(&data.sa_key)?)?;
    std::fs::write(data_file(data_dir, "join-token"), &data.token)?;
    #[cfg(unix)]
    for name in ["tls.json", "sa.key", "join-token"] {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(data_file(data_dir, name), std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// openraft's warnings and errors (or more: `LITEK8S_RAFT_LOG=info|debug`)
/// on stderr, prefixed `raft:`.
struct RaftLog(tracing::Level);

struct Fields(String);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        if field.name() == "message" {
            let _ = write!(self.0, " {value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

impl tracing::Subscriber for RaftLog {
    fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
        *m.level() <= self.0 && m.target().starts_with("openraft")
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        eprintln!("raft: {}{}", event.metadata().level(), fields.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn raft_config() -> Result<Config> {
    // A follower calls an election after the leader's lease (the max
    // election timeout) plus a random election timeout without a
    // heartbeat: a dead leader is replaced within ~1.5-2 s. openraft also
    // gives each AppendEntries one heartbeat interval: slow disks or a
    // lossy network (Wi-Fi) want more, e.g. LITEK8S_RAFT_HEARTBEAT_MS=300.
    let heartbeat: u64 = std::env::var("LITEK8S_RAFT_HEARTBEAT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(100).max(20);
    let config = Config {
        cluster_name: "litek8s".into(),
        heartbeat_interval: heartbeat,
        election_timeout_min: heartbeat * 5,
        election_timeout_max: heartbeat * 10,
        // A snapshot every 20k entries; keep some log after it so a
        // follower that was briefly away catches up from the log.
        snapshot_policy: SnapshotPolicy::LogsSinceLast(20_000),
        max_in_snapshot_log_to_keep: 2_000,
        install_snapshot_timeout: 60_000,
        max_payload_entries: 256,
        ..Default::default()
    };
    Ok(config.validate()?)
}

impl Cluster {
    /// Open this server's Raft state and start Raft. A fresh `--cluster-init`
    /// server becomes a one-member cluster (with the objects of a previous
    /// single-server WAL, if any); a fresh joining server waits to be added
    /// ([`Cluster::join`] once it serves).
    pub async fn start(opts: &Options, store: Arc<Store>, ca_pem: &str, secret: &str) -> Result<Arc<Self>> {
        let level = match std::env::var("LITEK8S_RAFT_LOG").as_deref() {
            Ok("debug") => tracing::Level::DEBUG,
            Ok("info") => tracing::Level::INFO,
            _ => tracing::Level::WARN,
        };
        let _ = tracing::subscriber::set_global_default(RaftLog(level));
        let raft_dir = opts.data_dir.join("raft");
        std::fs::create_dir_all(&raft_dir)?;
        let id_path = raft_dir.join("id");
        let existing: Option<u64> = std::fs::read_to_string(&id_path).ok().and_then(|s| s.trim().parse().ok());
        let fresh = existing.is_none();
        let id = existing.unwrap_or_else(|| sm::rand_u64() >> 1 | 1);
        let snap_dir = raft_dir.join("snapshots");
        let wal = opts.data_dir.join("objects.wal");
        let mut migrated = false;
        if fresh && opts.cluster_init && wal.exists() {
            let (objects, rv) = Store::read_wal(&wal)?;
            eprintln!("apiserver: moving {} objects (resourceVersion {rv}) from {} into the cluster", objects.len(), wal.display());
            store.restore(objects, rv);
            sm::StateMachine::seed(&snap_dir, &store, Default::default())?;
            std::fs::rename(&wal, opts.data_dir.join("objects.wal.pre-ha"))?;
            migrated = true;
        }
        let log = log::LogStore::open(&raft_dir.join("log"))?;
        let state_machine = sm::StateMachine::open(&snap_dir, store.clone())?;
        let peers = Arc::new(Peers::new(ca_pem, secret)?);
        // Raft runs on threads of its own: a burst of API work (a chart's
        // CRDs, admission) on a 2-core server must not delay heartbeats
        // into elections.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("raft")
            .enable_all()
            .build()
            .context("failed to start the raft runtime")?;
        let handle = runtime.handle().clone();
        peers.set_raft_runtime(handle.clone());
        // The runtime goes when the server's main runtime does.
        tokio::spawn(async move {
            struct Guard(Option<tokio::runtime::Runtime>);
            impl Drop for Guard {
                fn drop(&mut self) {
                    if let Some(rt) = self.0.take() {
                        rt.shutdown_background();
                    }
                }
            }
            let _guard = Guard(Some(runtime));
            std::future::pending::<()>().await
        });
        let config = Arc::new(raft_config()?);
        let network = Network { peers: peers.clone() };
        let raft = handle
            .spawn(async move { Raft::new(id, config, network, log, state_machine).await })
            .await
            .context("raft runtime")?
            .context("failed to start raft")?;
        let me = ServerNode { name: opts.name.clone(), addr: opts.advertise.clone(), raft: opts.advertise_raft.clone() };
        if fresh && opts.cluster_init {
            raft.initialize(BTreeMap::from([(id, me.clone())])).await.context("failed to initialize the cluster")?;
            raft.wait(Some(Duration::from_secs(30)))
                .current_leader(id, "new cluster elects its first server")
                .await
                .context("the new cluster did not elect this server")?;
        }
        if fresh {
            std::fs::write(&id_path, id.to_string())?;
        }
        let (leader_tx, leader_rx) = watch::channel(false);
        let mut metrics = raft.metrics();
        tokio::spawn(async move {
            loop {
                let leading = {
                    let m = metrics.borrow_and_update();
                    if let Err(fatal) = &m.running_state {
                        // A stopped Raft is a dead member: exit, so the
                        // service manager restarts it from its disk state.
                        eprintln!("apiserver: raft stopped: {fatal}");
                        if std::env::var_os("LITEK8S_HA_NO_EXIT").is_none() {
                            std::process::exit(70);
                        }
                        return;
                    }
                    m.state == ServerState::Leader && m.current_leader == Some(id)
                };
                leader_tx.send_if_modified(|v| std::mem::replace(v, leading) != leading);
                if metrics.changed().await.is_err() {
                    return;
                }
            }
        });
        let cluster = Arc::new(Self {
            id,
            name: opts.name.clone(),
            addr: opts.advertise.clone(),
            raft_addr: opts.advertise_raft.clone(),
            raft,
            peers,
            runtime: handle,
            data_dir: opts.data_dir.clone(),
            raft_dir,
            session: sm::rand_u64(),
            seq: AtomicU64::new(0),
            inflight: Mutex::default(),
            reads: ReadBatcher::default(),
            leader: leader_rx,
            membership_lock: Default::default(),
            leader_contact: AtomicU64::new(0),
            started: std::time::Instant::now(),
        });
        if migrated {
            // Members that join later must get the moved objects: they only
            // travel in a snapshot.
            let applied = cluster.raft.metrics().borrow().last_applied;
            cluster.raft.trigger().snapshot().await?;
            if let Some(applied) = applied {
                cluster.raft.wait(Some(Duration::from_secs(60))).snapshot(applied, "seed snapshot").await.ok();
                let _ = cluster.raft.trigger().purge_log(applied.index).await;
            }
        }
        cluster.watch_membership();
        if !(fresh && opts.cluster_init) {
            cluster.hold_elections_until_leader_heard();
        }
        Ok(cluster)
    }

    /// Ask the cluster at `server` to add this server (it must be serving
    /// Raft RPCs already). Retries until it is a voter.
    pub async fn join(&self, server: &str) -> Result<()> {
        let marker = self.raft_dir.join("joined");
        if marker.exists() {
            return Ok(());
        }
        let addr = authority(server);
        let req = MemberRequest { id: self.id, name: self.name.clone(), addr: self.addr.clone(), raft: self.raft_addr.clone() };
        let mut delay = Duration::from_secs(1);
        loop {
            match self.peers.call_json::<Value>(&addr, Method::POST, "/members", &req, Duration::from_secs(900)).await {
                Ok(_) => break,
                Err(e) => {
                    eprintln!("apiserver: joining the cluster at {server}: {e:#}; retrying");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(15));
                }
            }
        }
        std::fs::write(&marker, b"")?;
        eprintln!("apiserver: joined the cluster as a voter (id {:x})", self.id);
        Ok(())
    }

    /// Stop electing ourselves once removed from the cluster: a removed
    /// server's elections would only disrupt the others.
    fn watch_membership(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        let mut metrics = self.raft.metrics();
        tokio::spawn(async move {
            let mut was_member = false;
            loop {
                let Some(c) = me.upgrade() else { return };
                let member = metrics.borrow_and_update().membership_config.membership().voter_ids().any(|v| v == c.id);
                if was_member && !member {
                    eprintln!("apiserver: this server was removed from the cluster; stop it (its data dir no longer belongs to a cluster member)");
                    c.raft.runtime_config().elect(false);
                }
                was_member |= member;
                drop(c);
                if metrics.changed().await.is_err() {
                    return;
                }
            }
        });
    }

    /// A server that just (re)started does not call an election until it
    /// has heard from the leader or a few seconds passed without one: Raft
    /// without pre-vote would otherwise depose a healthy leader every time a
    /// follower restarts (its term goes up and the leader steps down when it
    /// sees it).
    fn hold_elections_until_leader_heard(self: &Arc<Self>) {
        self.raft.runtime_config().elect(false);
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
            while tokio::time::Instant::now() < deadline {
                let Some(c) = me.upgrade() else { return };
                if c.leader_contact.load(Ordering::Relaxed) > 0 {
                    break;
                }
                drop(c);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if let Some(c) = me.upgrade() {
                let removed = !c.raft.metrics().borrow().membership_config.membership().voter_ids().any(|v| v == c.id) && c.raft_dir.join("joined").exists();
                if !removed {
                    c.raft.runtime_config().elect(true);
                }
            }
        });
    }

    pub fn is_leader(&self) -> bool {
        *self.leader.borrow()
    }

    pub fn leader_id(&self) -> Option<u64> {
        self.raft.metrics().borrow().current_leader
    }

    fn raft_addr_of(&self, id: u64) -> Option<String> {
        self.raft.metrics().borrow().membership_config.membership().get_node(&id).map(|n| n.raft_addr())
    }

    fn addr_of(&self, id: u64) -> Option<String> {
        self.raft.metrics().borrow().membership_config.membership().get_node(&id).map(|n| n.addr.clone())
    }

    /// The leader's address, when there is one and it is not us.
    pub fn leader_addr(&self) -> Option<String> {
        let leader = self.leader_id()?;
        if leader == self.id {
            return None;
        }
        self.addr_of(leader)
    }

    /// The name of the server at `name`'s address.
    pub fn server_addr(&self, name: &str) -> Option<String> {
        let metrics = self.raft.metrics().borrow().clone();
        let membership = metrics.membership_config.membership();
        membership.nodes().find(|(_, n)| n.name == name).map(|(_, n)| n.addr.clone())
    }

    pub fn last_applied(&self) -> u64 {
        self.raft.metrics().borrow().last_applied.map(|l| l.index).unwrap_or(0)
    }

    /// Wait until this server has applied the log up to `index`.
    pub async fn wait_applied(&self, index: u64, timeout: Duration) -> bool {
        if self.last_applied() >= index {
            return true;
        }
        self.raft.wait(Some(timeout)).applied_index_at_least(Some(index), "catch up").await.is_ok()
    }

    /// Ready to serve: a leader is known and this server has applied
    /// everything it has heard is committed.
    pub fn ready(&self) -> bool {
        let m = self.raft.metrics().borrow().clone();
        match m.current_leader {
            // A leader that has not heard from a quorum lately is likely
            // cut off: let load balancers send clients elsewhere.
            Some(l) if l == self.id => m.millis_since_quorum_ack.is_some_and(|ms| ms < 3_000) || m.membership_config.membership().voter_ids().count() == 1,
            // A follower the leader has not reached lately: an election, or
            // this server is cut off.
            Some(_) => self.started.elapsed().as_millis() as u64 - self.leader_contact.load(Ordering::Relaxed) < 3_000,
            None => false,
        }
    }

    async fn propose(&self, mutation: Mutation, leader_only: bool) -> Result<Value, StoreError> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let acked = {
            let mut inflight = self.inflight.lock().unwrap();
            inflight.insert(seq);
            *inflight.first().unwrap()
        };
        let cmd = Command { session: self.session, seq, acked, mutation };
        let result = self.propose_command(cmd, leader_only).await;
        self.inflight.lock().unwrap().remove(&seq);
        result
    }

    async fn propose_command(&self, cmd: Command, leader_only: bool) -> Result<Value, StoreError> {
        let deadline = tokio::time::Instant::now() + PROPOSE_TIMEOUT;
        let mut last_error = String::from("no leader");
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(StoreError::Unavailable(format!("timed out: {last_error}")));
            }
            let remaining = deadline - now;
            let leader = self.leader_id();
            if leader == Some(self.id) {
                match tokio::time::timeout(remaining, self.raft.client_write(cmd.clone())).await {
                    Err(_) => return Err(StoreError::Unavailable("timed out waiting for the write to commit".into())),
                    Ok(Ok(resp)) => return resp.data.0.unwrap_or_else(|| Err(StoreError::Unavailable("no result".into()))),
                    Ok(Err(RaftError::APIError(ClientWriteError::ForwardToLeader(_)))) => {
                        if leader_only {
                            return Err(StoreError::Unavailable("this server is no longer the leader".into()));
                        }
                        last_error = "leadership changed".into();
                    }
                    Ok(Err(e)) => last_error = e.to_string(),
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
            if leader_only {
                return Err(StoreError::Unavailable("this server is not the leader".into()));
            }
            let Some(addr) = leader.and_then(|l| self.raft_addr_of(l)) else {
                // No leader (an election): wait for one.
                let mut m = self.raft.metrics();
                let _ = tokio::time::timeout(Duration::from_millis(200).min(remaining), m.changed()).await;
                continue;
            };
            match self.peers.call_json_on::<ProposeAnswer>(net::Lane::Raft, &addr, Method::POST, "/propose", &cmd, remaining).await {
                Ok(answer) => {
                    // Read-your-writes: answer once this server has it too.
                    let left = deadline.saturating_duration_since(tokio::time::Instant::now()).max(Duration::from_secs(1));
                    if !self.wait_applied(answer.index, left).await {
                        return Err(StoreError::Unavailable("committed, but this server has not caught up".into()));
                    }
                    return answer.result;
                }
                Err(e) => {
                    last_error = format!("{e:#}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// The linearizable read barrier: returns once this server's store
    /// reflects every write committed before the call.
    pub async fn read_barrier(&self) -> Result<(), String> {
        let index = tokio::time::timeout(READ_TIMEOUT, self.reads.get(|| self.read_index()))
            .await
            .map_err(|_| "timed out confirming the leader".to_string())??;
        if self.wait_applied(index, READ_TIMEOUT).await { Ok(()) } else { Err("timed out catching up with the leader".into()) }
    }

    async fn read_index(&self) -> Result<u64, String> {
        loop {
            match self.leader_id() {
                Some(l) if l == self.id => {
                    let (read, _) = self.raft.get_read_log_id().await.map_err(|e| e.to_string())?;
                    return Ok(read.map(|l| l.index).unwrap_or(0));
                }
                Some(l) => {
                    let addr = self.raft_addr_of(l).ok_or("unknown leader address")?;
                    let v: Value = self
                        .peers
                        .call_json_on(net::Lane::Raft, &addr, Method::POST, "/readindex", &json!({}), READ_TIMEOUT)
                        .await
                        .map_err(|e| format!("{e:#}"))?;
                    return v["index"].as_u64().ok_or_else(|| format!("leader answered {v}"));
                }
                None => {
                    let mut m = self.raft.metrics();
                    let _ = tokio::time::timeout(Duration::from_millis(200), m.changed()).await;
                }
            }
        }
    }

    /// Run the future `make` returns while this server leads, with leader-only
    /// writes; stop it (drop it) when leadership is lost, start it again on
    /// the next term this server wins.
    pub async fn run_while_leader<F, Fut>(self: Arc<Self>, make: F)
    where
        F: Fn() -> Fut,
        Fut: Future<Output = ()>,
    {
        let mut leader = self.leader.clone();
        loop {
            if leader.wait_for(|l| *l).await.is_err() {
                return;
            }
            // Apply everything earlier terms committed before acting on it.
            if let Err(e) = self.raft.ensure_linearizable().await {
                eprintln!("apiserver: leader could not confirm its term: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
            eprintln!("apiserver: this server is the leader; starting controllers");
            let work = LEADER_ONLY.scope((), make());
            tokio::select! {
                _ = work => {}
                _ = leader.wait_for(|l| !*l) => eprintln!("apiserver: no longer the leader; controllers stopped"),
            }
        }
    }

    pub fn status(&self) -> ClusterStatus {
        let m = self.raft.metrics().borrow().clone();
        let membership = m.membership_config.membership();
        let voters: BTreeSet<u64> = membership.voter_ids().collect();
        let servers = membership
            .nodes()
            .map(|(id, n)| ServerInfo {
                id: *id,
                name: n.name.clone(),
                addr: n.addr.clone(),
                voter: voters.contains(id),
                leader: m.current_leader == Some(*id),
                matched: m.replication.as_ref().and_then(|r| r.get(id).cloned().flatten()).map(|l| l.index).or_else(|| {
                    (*id == self.id && m.current_leader == Some(self.id)).then(|| m.last_log_index).flatten()
                }),
            })
            .collect();
        let raft = match &m.running_state {
            Ok(()) => "ok".to_string(),
            Err(e) => e.to_string(),
        };
        ClusterStatus { raft, servers, term: m.current_term, last_log_index: m.last_log_index, applied: m.last_applied.map(|l| l.index) }
    }

    async fn add_member(&self, req: MemberRequest) -> Result<()> {
        let _one_at_a_time = self.membership_lock.lock().await;
        let membership = self.raft.metrics().borrow().membership_config.membership().clone();
        if membership.voter_ids().any(|v| v == req.id) {
            return Ok(());
        }
        // The same server with a fresh data dir: drop its old identity.
        let stale: BTreeSet<u64> = membership.nodes().filter(|(id, n)| **id != req.id && (n.name == req.name || n.addr == req.addr)).map(|(id, _)| *id).collect();
        if !stale.is_empty() {
            eprintln!("apiserver: replacing the old member(s) {stale:x?} of server {}", req.name);
            self.raft.change_membership(ChangeMembers::RemoveVoters(stale.clone()), false).await?;
        }
        eprintln!("apiserver: adding server {} ({}) as a learner", req.name, req.addr);
        self.raft.add_learner(req.id, ServerNode { name: req.name.clone(), addr: req.addr.clone(), raft: req.raft.clone() }, true).await?;
        self.raft.change_membership(ChangeMembers::AddVoterIds(BTreeSet::from([req.id])), false).await?;
        eprintln!("apiserver: server {} is a voter", req.name);
        Ok(())
    }

    async fn remove_member(&self, name: &str) -> Result<()> {
        let _one_at_a_time = self.membership_lock.lock().await;
        let membership = self.raft.metrics().borrow().membership_config.membership().clone();
        let ids: BTreeSet<u64> = membership.nodes().filter(|(_, n)| n.name == name).map(|(id, _)| *id).collect();
        if ids.is_empty() {
            bail!("no server named {name}");
        }
        if ids.contains(&self.id) && membership.voter_ids().count() > 1 {
            // Hand leadership over first: a leader removing itself steps
            // down only after the change commits.
            eprintln!("apiserver: removing this server (the leader) from the cluster");
        }
        let voters: BTreeSet<u64> = membership.voter_ids().filter(|v| ids.contains(v)).collect();
        if !voters.is_empty() {
            self.raft.change_membership(ChangeMembers::RemoveVoters(voters), false).await?;
        }
        let learners: BTreeSet<u64> = self.raft.metrics().borrow().membership_config.membership().learner_ids().filter(|l| ids.contains(l)).collect();
        if !learners.is_empty() {
            self.raft.change_membership(ChangeMembers::RemoveNodes(learners), false).await?;
        }
        Ok(())
    }

    pub async fn shutdown(&self) {
        let _ = self.raft.shutdown().await;
    }

    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.runtime
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

impl Replicator for Cluster {
    fn replicate(&self, mutation: Mutation) -> Result<Value, StoreError> {
        let leader_only = LEADER_ONLY.try_with(|_| ()).is_ok();
        block_on(self.propose(mutation, leader_only))
    }
}

/// Run a future to completion from synchronous code inside the runtime
/// (the store's API is synchronous; a replicated write waits for Raft).
fn block_on<F: Future>(f: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread => {
            panic!("a replicated store needs the multi-threaded tokio runtime")
        }
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(f)),
        // Outside the runtime (a plain thread): drive it on a small one;
        // Raft itself keeps running on the main runtime.
        Err(_) => tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime").block_on(f),
    }
}

/// Concurrent readers share read-index requests: each waits for one that
/// started after it arrived.
#[derive(Default)]
struct ReadBatcher {
    started: AtomicU64,
    lock: tokio::sync::Mutex<(u64, u64)>,
}

impl ReadBatcher {
    async fn get<F, Fut>(&self, fetch: F) -> Result<u64, String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<u64, String>>,
    {
        let ticket = self.started.load(Ordering::SeqCst);
        let mut done = self.lock.lock().await;
        if done.0 > ticket {
            return Ok(done.1);
        }
        let mine = self.started.fetch_add(1, Ordering::SeqCst) + 1;
        let index = fetch().await?;
        *done = (mine, index);
        Ok(index)
    }
}

// ---------------------------------------------------------------------------
// HTTP side: `/apis/server.litek8s.dev/v1/...`

fn json_reply(code: StatusCode, value: &impl Serialize) -> Response<Body> {
    let mut resp = Response::new(full(serde_json::to_vec(value).unwrap_or_default()));
    *resp.status_mut() = code;
    resp.headers_mut().insert(hyper::header::CONTENT_TYPE, hyper::header::HeaderValue::from_static("application/json"));
    resp
}

fn error(code: StatusCode, message: impl Into<String>) -> Response<Body> {
    json_reply(code, &json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "message": message.into(), "code": code.as_u16()}))
}

fn not_leader(cluster: &Cluster) -> Response<Body> {
    json_reply(StatusCode::MISDIRECTED_REQUEST, &json!({"message": "not the leader", "leader": cluster.leader_id()}))
}

async fn read_body(body: Incoming, limit: usize) -> Result<hyper::body::Bytes, Response<Body>> {
    Limited::new(body, limit)
        .collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|_| error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"))
}

fn parse<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, Response<Body>> {
    serde_json::from_slice(bytes).map_err(|e| error(StatusCode::BAD_REQUEST, format!("invalid request: {e}")))
}

/// A byte stream to a pod's port through `addr`, the server its node's
/// agent is connected to.
pub async fn tunnel(cluster: &Cluster, addr: &str, node: &str, pod_uid: &str, port: u16) -> Result<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>> {
    let path = format!("{PREFIX}/tunnel?node={node}&pod={pod_uid}&port={port}");
    let req = cluster.peers.request(addr, Method::POST, &path, &[("connection", "Upgrade".into()), ("upgrade", TUNNEL_PROTOCOL.into())], net::body(Vec::new()))?;
    let mut resp = cluster.peers.open(addr, req).await?;
    if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        let status = resp.status();
        let body = resp.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
        bail!("{addr} refused the tunnel ({status}): {}", String::from_utf8_lossy(&body));
    }
    Ok(hyper_util::rt::TokioIo::new(hyper::upgrade::on(&mut resp).await?))
}

const TUNNEL_PROTOCOL: &str = "litek8s-tunnel";

/// Serve a `server.litek8s.dev` request.
pub async fn serve(state: &Arc<ApiState>, mut req: Request<Incoming>) -> Response<Body> {
    let upgrade = req.headers().contains_key(hyper::header::UPGRADE).then(|| hyper::upgrade::on(&mut req));
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().strip_prefix(PREFIX).unwrap_or_default().to_string();
    let method = parts.method.clone();
    let Some(cluster) = state.ha() else {
        return error(StatusCode::NOT_FOUND, "this server is not part of a cluster (start it with --cluster-init or --server)");
    };
    let public = method == Method::GET && path == "/servers";
    if !public {
        let auth = parts.headers.get(hyper::header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or_default();
        let presented = auth.strip_prefix("Bearer ").unwrap_or_default().trim();
        let presented = crate::kube::join::Token::parse(presented).map(|t| t.secret).unwrap_or_default();
        if !constant_time_eq(presented.as_bytes(), cluster.peers.secret().as_bytes()) {
            return error(StatusCode::UNAUTHORIZED, "invalid cluster token");
        }
    }
    match (method, path.as_str()) {
        (Method::POST, "/raft/append") => {
            let bytes = match read_body(body, MAX_RPC_BYTES).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let rpc: openraft::raft::AppendEntriesRequest<TypeConfig> = match parse(&bytes) {
                Ok(r) => r,
                Err(r) => return r,
            };
            let entries = rpc.entries.len();
            let started = std::time::Instant::now();
            let result = cluster.raft.append_entries(rpc).await;
            let took = started.elapsed();
            if took > Duration::from_millis(500) {
                eprintln!("apiserver: raft: slow append ({entries} entries, {} bytes) took {took:?}", bytes.len());
            }
            if result.as_ref().is_ok_and(|r| r.is_success()) {
                cluster.leader_contact.store((cluster.started.elapsed().as_millis() as u64).max(1), Ordering::Relaxed);
            }
            json_reply(StatusCode::OK, &result)
        }
        (Method::POST, "/raft/vote") => {
            let bytes = match read_body(body, 1 << 20).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let rpc = match parse(&bytes) {
                Ok(r) => r,
                Err(r) => return r,
            };
            json_reply(StatusCode::OK, &cluster.raft.vote(rpc).await)
        }
        (Method::POST, "/raft/snapshot") => receive_snapshot(&cluster, &parts.headers, body).await,
        (Method::POST, "/propose") => {
            let bytes = match read_body(body, MAX_RPC_BYTES).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let cmd: Command = match parse(&bytes) {
                Ok(c) => c,
                Err(r) => return r,
            };
            if !cluster.is_leader() {
                return not_leader(&cluster);
            }
            match cluster.raft.client_write(cmd).await {
                Ok(resp) => {
                    let result = resp.data.0.unwrap_or_else(|| Err(StoreError::Unavailable("no result".into())));
                    json_reply(StatusCode::OK, &ProposeAnswer { result, index: resp.log_id.index })
                }
                Err(RaftError::APIError(ClientWriteError::ForwardToLeader(_))) => not_leader(&cluster),
                Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, e.to_string()),
            }
        }
        (Method::POST, "/readindex") => {
            if !cluster.is_leader() {
                return not_leader(&cluster);
            }
            match cluster.raft.get_read_log_id().await {
                Ok((read, _)) => json_reply(StatusCode::OK, &json!({"index": read.map(|l| l.index).unwrap_or(0)})),
                Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, e.to_string()),
            }
        }
        (Method::POST, "/bootstrap") => {
            let dir = cluster.data_dir();
            match (std::fs::read_to_string(dir.join("tls.json")), std::fs::read(dir.join("sa.key"))) {
                (Ok(tls), Ok(sa_key)) => {
                    let sa_key = base64::engine::general_purpose::STANDARD.encode(sa_key);
                    json_reply(StatusCode::OK, &BootstrapData { tls, sa_key, token: state.join_token() })
                }
                _ => error(StatusCode::INTERNAL_SERVER_ERROR, "this server has no TLS or service account keys to share"),
            }
        }
        (Method::POST, "/members") => {
            let bytes = match read_body(body, 1 << 20).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let req: MemberRequest = match parse(&bytes) {
                Ok(r) => r,
                Err(r) => return r,
            };
            if let Some(addr) = cluster.leader_addr() {
                return relay(&cluster, &addr, Method::POST, "/members", bytes, Duration::from_secs(900)).await;
            }
            match cluster.add_member(req).await {
                Ok(()) => json_reply(StatusCode::OK, &cluster.status()),
                Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")),
            }
        }
        (Method::DELETE, p) if p.starts_with("/servers/") => {
            let name = p.trim_start_matches("/servers/").to_string();
            if let Some(addr) = cluster.leader_addr() {
                return relay(&cluster, &addr, Method::DELETE, p, Default::default(), Duration::from_secs(60)).await;
            }
            match cluster.remove_member(&name).await {
                Ok(()) => json_reply(StatusCode::OK, &cluster.status()),
                Err(e) => error(StatusCode::BAD_REQUEST, format!("{e:#}")),
            }
        }
        (Method::GET, "/servers") => {
            // The leader knows how far each server got.
            if let Some(addr) = cluster.leader_addr() {
                if let Ok(status) = cluster.peers.call_json::<ClusterStatus>(&addr, Method::POST, "/status", &json!({}), Duration::from_secs(3)).await {
                    return json_reply(StatusCode::OK, &status);
                }
            }
            json_reply(StatusCode::OK, &cluster.status())
        }
        (Method::POST, "/status") => json_reply(StatusCode::OK, &cluster.status()),
        (Method::POST, "/tunnel") => {
            let query: std::collections::HashMap<String, String> = parts
                .uri
                .query()
                .unwrap_or_default()
                .split('&')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let (node, pod, port) = (query.get("node").cloned().unwrap_or_default(), query.get("pod").cloned().unwrap_or_default(), query.get("port").and_then(|p| p.parse::<u16>().ok()).unwrap_or(0));
            if !state.hub.is_connected(&node) {
                return error(StatusCode::SERVICE_UNAVAILABLE, format!("node {node} is not connected to this server"));
            }
            let Some(upgrade) = upgrade else { return error(StatusCode::BAD_REQUEST, "a tunnel needs an upgrade") };
            let stream = match crate::kube::admission::pod_stream(state, &node, &pod, port).await {
                Ok(s) => s,
                Err(e) => return error(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")),
            };
            tokio::spawn(async move {
                if let Ok(up) = upgrade.await {
                    let mut up = hyper_util::rt::TokioIo::new(up);
                    let mut stream = stream;
                    let _ = tokio::io::copy_bidirectional(&mut up, &mut stream).await;
                }
            });
            let mut resp = Response::new(full(Vec::new()));
            *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
            resp.headers_mut().insert(hyper::header::UPGRADE, hyper::header::HeaderValue::from_static(TUNNEL_PROTOCOL));
            resp.headers_mut().insert(hyper::header::CONNECTION, hyper::header::HeaderValue::from_static("Upgrade"));
            resp
        }
        // Fault injection for tests (LITEK8S_HA_FAULTS=1): cut or restore
        // this server's links to the servers at `addrs`.
        (Method::POST, "/debug/partition") if std::env::var_os("LITEK8S_HA_FAULTS").is_some() => {
            let bytes = match read_body(body, 1 << 20).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let req: Value = match parse(&bytes) {
                Ok(r) => r,
                Err(r) => return r,
            };
            let blocked = req["blocked"].as_bool().unwrap_or(true);
            let membership = cluster.raft.metrics().borrow().membership_config.membership().clone();
            for addr in req["addrs"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                cluster.peers.set_blocked(addr, blocked);
                for (_, node) in membership.nodes().filter(|(_, n)| n.addr == addr) {
                    cluster.peers.set_blocked(&node.raft_addr(), blocked);
                }
            }
            json_reply(StatusCode::OK, &json!({}))
        }
        (Method::POST, "/podcidr") => {
            let bytes = match read_body(body, 1 << 20).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            if let Some(addr) = cluster.leader_addr() {
                return relay(&cluster, &addr, Method::POST, "/podcidr", bytes, Duration::from_secs(30)).await;
            }
            let req: Value = match parse(&bytes) {
                Ok(r) => r,
                Err(r) => return r,
            };
            let node = req["node"].as_str().unwrap_or_default();
            match crate::kube::join::allocate_pod_cidr(state, node, req["requested"].as_str()) {
                Ok(cidr) => json_reply(StatusCode::OK, &json!({"cidr": cidr})),
                Err(e) => error(StatusCode::CONFLICT, e.to_string()),
            }
        }
        _ => error(StatusCode::NOT_FOUND, "not found"),
    }
}

/// Pass a `server.litek8s.dev` request on to the leader.
async fn relay(cluster: &Cluster, addr: &str, method: Method, path: &str, body: hyper::body::Bytes, timeout: Duration) -> Response<Body> {
    match cluster.peers.call(addr, method, &format!("{PREFIX}{path}"), body, timeout).await {
        Ok((status, bytes)) => {
            let mut resp = Response::new(full(bytes));
            *resp.status_mut() = status;
            resp.headers_mut().insert(hyper::header::CONTENT_TYPE, hyper::header::HeaderValue::from_static("application/json"));
            resp
        }
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, format!("cannot reach the leader: {e:#}")),
    }
}

async fn receive_snapshot(cluster: &Cluster, headers: &hyper::HeaderMap, mut body: Incoming) -> Response<Body> {
    use tokio::io::AsyncWriteExt;
    let header = headers.get(net::SNAPSHOT_HEADER).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(header) else {
        return error(StatusCode::BAD_REQUEST, "missing snapshot header");
    };
    let (vote, meta): (openraft::Vote<u64>, openraft::SnapshotMeta<u64, ServerNode>) = match parse(&raw) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let mut data = match cluster.raft.begin_receiving_snapshot().await {
        Ok(d) => d,
        Err(e) => return error(StatusCode::SERVICE_UNAVAILABLE, e.to_string()),
    };
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(f) => f,
            Err(e) => return error(StatusCode::BAD_REQUEST, format!("snapshot transfer broke: {e}")),
        };
        if let Ok(chunk) = frame.into_data() {
            if let Err(e) = data.write_all(&chunk).await {
                return error(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot store the snapshot: {e}"));
            }
        }
    }
    if let Err(e) = data.flush().await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot store the snapshot: {e}"));
    }
    eprintln!("apiserver: installing a snapshot up to log index {:?}", meta.last_log_id.map(|l| l.index));
    let result: Result<openraft::raft::SnapshotResponse<u64>, RaftError<u64>> =
        cluster.raft.install_full_snapshot(vote, openraft::Snapshot { meta, snapshot: data }).await.map_err(RaftError::Fatal);
    json_reply(StatusCode::OK, &result)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorities() {
        assert_eq!(authority("https://10.0.0.1:6443/"), "10.0.0.1:6443");
        assert_eq!(authority("https://server"), "server:6443");
        assert_eq!(authority("10.0.0.2"), "10.0.0.2:6443");
        assert_eq!(authority("http://a:80"), "a:80");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_batcher_shares_fetches() {
        let batcher = Arc::new(ReadBatcher::default());
        let fetches = Arc::new(AtomicU64::new(0));
        let mut tasks = Vec::new();
        for _ in 0..50 {
            let (b, f) = (batcher.clone(), fetches.clone());
            tasks.push(tokio::spawn(async move {
                b.get(|| async {
                    let n = f.fetch_add(1, Ordering::SeqCst) + 1;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Ok(n)
                })
                .await
                .unwrap()
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert!(fetches.load(Ordering::SeqCst) <= 3, "{} fetches", fetches.load(Ordering::SeqCst));
    }
}
