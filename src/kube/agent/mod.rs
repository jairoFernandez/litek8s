//! The node agent: runs the pods the scheduler binds to its node.
//!
//! An agent holds ONE persistent connection to the API server (see
//! [`protocol`]): pods arrive as pushed events, status goes back on the same
//! connection, and log/exec/port-forward streams are multiplexed over it.
//! Nothing polls and nothing opens a connection per request, which is what
//! makes remote nodes cheap (an `ssh` exec per call costs ~7 ms and ~9 ms of
//! CPU, a resident socket ~0.2 ms; see bench/RESULTS.md).
//!
//! The local node runs in-process over an in-memory pipe; a remote node runs
//! `litek8s node-agent --server http://<apiserver>:6443`, which upgrades one
//! HTTP connection to the same protocol and reconnects if it drops. Running
//! pods survive reconnects: the server resends the node's pods and the agent
//! reconciles.
//!
//! The agent sends a heartbeat every [`HEARTBEAT_INTERVAL`] (the server
//! renews the node's Lease and answers), and drops a connection on which
//! the server has been silent for [`SERVER_SILENCE`], so a half-open
//! connection (after a laptop slept, or a NAT forgot it) is replaced quickly.

pub(crate) mod crun;
pub mod hub;
mod kubelet;
mod obs;
pub mod overlay;
mod pod;
pub(crate) mod probe;
pub mod protocol;
mod proxy;
pub mod runtime;
mod volumes;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use hyper::body::Bytes;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use protocol::{Frame, Message, read_frame, write_frame};
use runtime::{DockerPods, LogOptions, NodeRuntime, PodRuntime};

pub use hub::AgentHub;

/// How often an agent renews its node's Lease.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
/// A connection the server has been silent on for this long is dead.
pub const SERVER_SILENCE: Duration = Duration::from_secs(35);
/// Node annotation: this node's own heartbeat grace period, in seconds
/// (e.g. a laptop that sleeps), used when longer than the cluster's.
pub const ANNOTATION_GRACE_PERIOD: &str = "litek8s.dev/node-monitor-grace-period";

/// `litek8s crun-shim ...`: the per-container shim of the crun runtime.
pub fn crun_shim(args: &[String]) -> ! {
    crun::shim::main(args)
}

/// The crun binary the crun runtime would use, if installed.
pub fn crun_binary() -> Option<PathBuf> {
    crun::find_crun()
}

/// `litek8s crun-pause`: PID 1 of a pod's shared PID namespace.
pub fn crun_pause(args: &[String]) -> ! {
    crun::shim::pause_main(args)
}

/// Which container runtime a node runs pods with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum RuntimeChoice {
    /// crun on a Linux host where it is installed (and the agent is root),
    /// Docker otherwise.
    Auto,
    Crun,
    Docker,
}

pub struct AgentConfig {
    pub node_name: String,
    /// Where pod volumes (configMap, secret, projected) are materialized.
    pub root_dir: PathBuf,
    /// The Node object announced on connect.
    pub node: Value,
    /// This node's address (`status.hostIP`, hostNetwork pods' IP).
    pub node_ip: String,
}

/// Where pods reach the API server, as announced by the server.
#[derive(Clone, Debug)]
pub(crate) struct ApiEndpoint {
    pub host: String,
    pub port: u16,
    pub cluster_dns: bool,
}

/// The live connection to the server, while there is one.
pub(crate) struct Link {
    tx: mpsc::Sender<Frame>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Option<Value>>>>,
    next_id: AtomicU64,
}

impl Link {
    pub(crate) async fn get(&self, resource: &str, namespace: &str, name: &str) -> Result<Option<Value>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = Message::Get { id, resource: resource.into(), namespace: namespace.into(), name: name.into() };
        if self.tx.send(Frame::Control(msg)).await.is_err() {
            bail!("disconnected from the API server");
        }
        match tokio::time::timeout(Duration::from_secs(15), rx).await {
            Ok(Ok(obj)) => Ok(obj),
            _ => {
                self.pending.lock().unwrap().remove(&id);
                bail!("lookup of {resource} {namespace}/{name} timed out")
            }
        }
    }

    pub(crate) async fn send(&self, msg: Message) -> bool {
        self.tx.send(Frame::Control(msg)).await.is_ok()
    }
}

/// What stream handlers need to know about a pod, kept by its worker.
#[derive(Default, Clone, Debug)]
pub(crate) struct PodView {
    pub namespace: String,
    pub name: String,
    pub ip: Option<String>,
    /// The pod sandbox's runtime id; it owns the pod's network namespace.
    pub sandbox: Option<String>,
    /// container name → (current container id, previous container id)
    pub containers: HashMap<String, (Option<String>, Option<String>)>,
    /// The pod's labels (log search by selector).
    pub labels: std::collections::BTreeMap<String, String>,
    /// container name → image
    pub images: HashMap<String, String>,
}

enum StreamInput {
    Data(#[allow(dead_code)] u8, Bytes),
    Close,
    Resize(u16, u16),
}

pub struct Agent<R: PodRuntime> {
    rt: Arc<R>,
    config: AgentConfig,
    link: watch::Sender<Option<Arc<Link>>>,
    api: Mutex<Option<ApiEndpoint>>,
    workers: Mutex<HashMap<String, mpsc::UnboundedSender<pod::PodMsg>>>,
    views: Mutex<HashMap<String, PodView>>,
    /// container id → pod uid, to route exits.
    containers: Mutex<HashMap<String, String>>,
    streams: Mutex<HashMap<u32, (mpsc::UnboundedSender<StreamInput>, JoinHandle<()>)>>,
    images: Mutex<HashMap<String, ImageState>>,
    /// The address `host-gateway` stands for in pods (found once).
    gateway_ip: tokio::sync::OnceCell<Option<String>>,
    proxy: Arc<proxy::Proxy>,
    overlay: Option<Arc<overlay::Overlay>>,
    /// pod uid → its persistent volume claims (volume metrics).
    claims: Mutex<HashMap<String, Vec<crate::obs::node::ClaimRef>>>,
    /// Observability settings from the server, and the sampler task.
    obs: Mutex<(Option<crate::obs::ObsSettings>, Option<JoinHandle<()>>)>,
}

enum ImageState {
    Pulling(Vec<mpsc::UnboundedSender<pod::PodMsg>>),
    Ready,
    Failed(String, tokio::time::Instant),
}

impl<R: PodRuntime> Agent<R> {
    pub fn new(rt: R, config: AgentConfig) -> Arc<Self> {
        Self::with_overlay(rt, config, None)
    }

    pub fn with_overlay(rt: R, config: AgentConfig, overlay: Option<overlay::Overlay>) -> Arc<Self> {
        let (link, _) = watch::channel(None);
        let agent = Arc::new(Self {
            rt: Arc::new(rt),
            config,
            link,
            api: Mutex::new(None),
            workers: Mutex::new(HashMap::new()),
            views: Mutex::new(HashMap::new()),
            containers: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
            images: Mutex::new(HashMap::new()),
            gateway_ip: tokio::sync::OnceCell::new(),
            proxy: Arc::new(proxy::Proxy::default()),
            overlay: overlay.map(Arc::new),
            claims: Mutex::default(),
            obs: Mutex::default(),
        });
        let weak = Arc::downgrade(&agent);
        let in_pod: proxy::InPodDial = Arc::new(move |ip: String, port: u16| {
            let weak = weak.clone();
            Box::pin(async move {
                let agent = weak.upgrade()?;
                if agent.rt.reaches_pods_directly() {
                    return None;
                }
                let sandbox = agent.views.lock().unwrap().values().find(|v| v.ip.as_deref() == Some(ip.as_str()))?.sandbox.clone()?;
                agent.rt.exec(&sandbox, vec!["nc".into(), ip, port.to_string()], false, true).await.ok()
            })
        });
        let _ = agent.proxy.in_pod.set(in_pod);
        agent.proxy.direct.store(agent.rt.reaches_pods_directly(), Ordering::Relaxed);
        tokio::spawn(proxy::run(agent.proxy.clone(), agent.rt.clone()));
        let exits = agent.clone();
        tokio::spawn(async move {
            let mut stream = exits.rt.exits();
            while let Some(id) = stream.next().await {
                let uid = exits.containers.lock().unwrap().get(&id).cloned();
                if let Some(uid) = uid {
                    exits.tell(&uid, pod::PodMsg::Died(id));
                }
            }
        });
        agent
    }

    /// Whether pods here are on the cross-node WireGuard overlay.
    pub fn has_overlay(&self) -> bool {
        self.overlay.is_some()
    }

    pub(crate) fn link(&self) -> Option<Arc<Link>> {
        self.link.borrow().clone()
    }

    pub(crate) fn subscribe_link(&self) -> watch::Receiver<Option<Arc<Link>>> {
        self.link.subscribe()
    }

    pub(crate) fn api_endpoint(&self) -> Option<ApiEndpoint> {
        self.api.lock().unwrap().clone()
    }

    /// An IP address for `host` as pods see it (`host-gateway` included).
    pub(crate) async fn resolve_host(&self, host: &str) -> Option<String> {
        if host == "host-gateway" {
            return self
                .gateway_ip
                .get_or_init(|| async { self.rt.host_gateway_ip().await.ok() })
                .await
                .clone();
        }
        if host.parse::<std::net::IpAddr>().is_ok() {
            return Some(host.to_string());
        }
        tokio::net::lookup_host((host, 53)).await.ok()?.find(|a| a.is_ipv4()).map(|a| a.ip().to_string())
    }

    pub(crate) fn service_proxy(&self) -> &proxy::Proxy {
        &self.proxy
    }

    pub(crate) fn runtime(&self) -> &Arc<R> {
        &self.rt
    }

    pub(crate) fn config(&self) -> &AgentConfig {
        &self.config
    }

    fn tell(&self, uid: &str, msg: pod::PodMsg) {
        if let Some(tx) = self.workers.lock().unwrap().get(uid) {
            let _ = tx.send(msg);
        }
    }

    pub(crate) fn track_claim(&self, uid: &str, claim: crate::obs::node::ClaimRef) {
        let mut claims = self.claims.lock().unwrap();
        let list = claims.entry(uid.to_string()).or_default();
        list.retain(|c| c.claim != claim.claim || c.namespace != claim.namespace);
        list.push(claim);
    }

    pub(crate) fn set_view(&self, uid: &str, view: PodView) {
        self.views.lock().unwrap().insert(uid.to_string(), view);
    }

    pub(crate) fn track_container(&self, id: &str, uid: &str) {
        self.containers.lock().unwrap().insert(id.to_string(), uid.to_string());
    }

    pub(crate) fn untrack_container(&self, id: &str) {
        self.containers.lock().unwrap().remove(id);
    }

    /// Whether `image` is ready to run; if not, a pull is started (once per
    /// image, shared by every pod that needs it) and `notify` hears when it
    /// finishes. `Err` is the last pull failure, retried after a back-off.
    pub(crate) fn image_ready(
        self: &Arc<Self>,
        image: &str,
        always: bool,
        notify: &mpsc::UnboundedSender<pod::PodMsg>,
    ) -> Result<bool, String> {
        let mut images = self.images.lock().unwrap();
        match images.get_mut(image) {
            Some(ImageState::Ready) => return Ok(true),
            Some(ImageState::Pulling(waiters)) => {
                waiters.push(notify.clone());
                return Ok(false);
            }
            Some(ImageState::Failed(error, at)) if at.elapsed() < Duration::from_secs(10) => {
                return Err(error.clone());
            }
            _ => {}
        }
        images.insert(image.to_string(), ImageState::Pulling(vec![notify.clone()]));
        let agent = self.clone();
        let image = image.to_string();
        tokio::spawn(async move {
            let result = agent.rt.pull_image(&image, always).await;
            let state = match result {
                Ok(()) => ImageState::Ready,
                Err(e) => ImageState::Failed(format!("{e:#}"), tokio::time::Instant::now()),
            };
            let previous = agent.images.lock().unwrap().insert(image, state);
            if let Some(ImageState::Pulling(waiters)) = previous {
                for w in waiters {
                    let _ = w.send(pod::PodMsg::Wake);
                }
            }
        });
        Ok(false)
    }

    fn upsert(self: &Arc<Self>, pod_obj: Value) {
        let uid = pod_obj.pointer("/metadata/uid").and_then(Value::as_str).unwrap_or_default().to_string();
        if uid.is_empty() {
            return;
        }
        let mut workers = self.workers.lock().unwrap();
        if let Some(tx) = workers.get(&uid) {
            let _ = tx.send(pod::PodMsg::Update(pod_obj));
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        workers.insert(uid.clone(), tx.clone());
        drop(workers);
        let agent = self.clone();
        tokio::spawn(async move {
            pod::run(agent.clone(), pod_obj, tx, rx).await;
            agent.workers.lock().unwrap().remove(&uid);
            agent.views.lock().unwrap().remove(&uid);
            agent.claims.lock().unwrap().remove(&uid);
        });
    }

    /// Remove containers and volumes of pods the server no longer knows.
    async fn cleanup_orphans(self: &Arc<Self>) {
        // List first: a pod worker started after the listing is known below,
        // so its new containers are never taken for orphans.
        let Ok(containers) = self.rt.list().await else { return };
        let known: HashSet<String> = self.workers.lock().unwrap().keys().cloned().collect();
        let node = self.config.node_name.clone();
        for c in containers {
            // Another agent's container on a shared runtime is not ours.
            if c.labels.get(runtime::LABEL_NODE).is_some_and(|n| *n != node) {
                continue;
            }
            let uid = c.labels.get(runtime::LABEL_POD_UID).cloned().unwrap_or_default();
            if !known.contains(&uid) {
                let _ = self.rt.remove(&c.id).await;
            }
        }
        if let Ok(volumes) = self.rt.list_volumes(&format!("{}={node}", runtime::LABEL_NODE)).await {
            for v in volumes {
                if !known.iter().any(|uid| v.contains(&uid[..8.min(uid.len())])) {
                    let _ = self.rt.remove_volume(&v).await;
                }
            }
        }
        let pods_dir = self.config.root_dir.join("pods");
        if let Ok(entries) = std::fs::read_dir(&pods_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !known.contains(&name) {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
    }

    /// Run one session over an established connection, until it ends.
    pub async fn session<IO>(self: &Arc<Self>, io: IO) -> Result<()>
    where
        IO: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (reader, writer) = tokio::io::split(io);
        let mut reader = BufReader::new(reader);
        let (tx, mut rx) = mpsc::channel::<Frame>(1024);
        let writer_task = tokio::spawn(async move {
            let mut writer = BufWriter::new(writer);
            while let Some(frame) = rx.recv().await {
                if write_frame(&mut writer, &frame).await.is_err() {
                    break;
                }
                if rx.is_empty() && writer.flush().await.is_err() {
                    break;
                }
            }
        });
        let features = crate::obs::AGENT_FEATURES.iter().map(|f| f.to_string()).collect();
        tx.send(Frame::Control(Message::Hello { node: self.config.node.clone(), features }))
            .await
            .ok()
            .context("connection closed")?;
        let link = Arc::new(Link { tx: tx.clone(), pending: Mutex::new(HashMap::new()), next_id: AtomicU64::new(1) });
        let heartbeat_tx = tx.clone();
        let heartbeat = tokio::spawn(async move {
            let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if heartbeat_tx.send(Frame::Control(Message::Heartbeat)).await.is_err() {
                    return;
                }
            }
        });
        // Containers can outlive their pod when the runtime fails mid-way
        // (a create that succeeded behind a timed-out start): sweep them.
        let sweeper = {
            let agent = self.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
                tick.tick().await;
                loop {
                    tick.tick().await;
                    if agent.link().is_some() {
                        agent.cleanup_orphans().await;
                    }
                }
            })
        };

        let result = async {
            loop {
                let frame = match tokio::time::timeout(SERVER_SILENCE, read_frame(&mut reader)).await {
                    Ok(frame) => frame?,
                    Err(_) => bail!("no word from the server for {}s", SERVER_SILENCE.as_secs()),
                };
                let Some(frame) = frame else { break };
                match frame {
                    Frame::Control(msg) => self.handle(&link, msg).await,
                    Frame::Data { stream, channel, bytes } => self.stream_input(stream, StreamInput::Data(channel, bytes)),
                    Frame::CloseInput { stream } => self.stream_input(stream, StreamInput::Close),
                }
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        heartbeat.abort();
        self.link.send_replace(None);
        sweeper.abort();
        writer_task.abort();
        for (_, (_, task)) in self.streams.lock().unwrap().drain() {
            task.abort();
        }
        result
    }

    fn stream_input(&self, stream: u32, input: StreamInput) {
        if let Some((tx, _)) = self.streams.lock().unwrap().get(&stream) {
            let _ = tx.send(input);
        }
    }

    async fn handle(self: &Arc<Self>, link: &Arc<Link>, msg: Message) {
        match msg {
            Message::Welcome { api_host, api_port, cluster_dns, obs, .. } => {
                *self.api.lock().unwrap() = Some(ApiEndpoint { host: api_host, port: api_port, cluster_dns });
                self.link.send_replace(Some(link.clone()));
                obs::configure(self, obs);
            }
            Message::HeartbeatAck => {}
            Message::Resync => {
                self.link.send_replace(Some(link.clone()));
            }
            Message::Peers { peers } => {
                if let Some(overlay) = self.overlay.clone() {
                    tokio::spawn(async move {
                        if let Err(e) = overlay.apply(&peers).await {
                            eprintln!("node-agent: WireGuard peers not applied: {e:#}");
                        }
                    });
                }
            }
            Message::PodUpsert { pod } => self.upsert(pod),
            Message::PodDelete { uid } => self.tell(&uid, pod::PodMsg::Delete),
            Message::SyncDone => {
                let agent = self.clone();
                tokio::spawn(async move { agent.cleanup_orphans().await });
            }
            Message::VolumeDelete { name } => {
                let rt = self.rt.clone();
                tokio::spawn(async move {
                    // The pod using it may still be stopping; retry briefly.
                    for _ in 0..30 {
                        if rt.remove_volume(&volumes::pv_volume_name(&name)).await.is_ok() {
                            return;
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                });
            }
            Message::ServiceUpdate { namespace, name, service, endpoints } => {
                self.proxy.update(namespace, name, service, endpoints);
            }
            Message::ObjectChanged { resource, namespace, name } => {
                for tx in self.workers.lock().unwrap().values() {
                    let _ = tx.send(pod::PodMsg::ObjectChanged { resource: resource.clone(), namespace: namespace.clone(), name: name.clone() });
                }
            }
            Message::GetResult { id, object } => {
                if let Some(tx) = link.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(object);
                }
            }
            Message::Exec { stream, pod_uid, container, command, stdin, tty } => {
                let target = self.container_id(&pod_uid, &container, false);
                self.spawn_stream(link, stream, move |agent, link, input| {
                    exec_stream(agent, link, stream, target, command, stdin, tty, input)
                });
            }
            Message::Logs { stream, pod_uid, container, follow, previous, timestamps, tail_lines, since_seconds, limit_bytes } => {
                let target = self.container_id(&pod_uid, &container, previous);
                let options = LogOptions { follow, tail_lines, timestamps, since_seconds };
                self.spawn_stream(link, stream, move |agent, link, _input| {
                    logs_stream(agent, link, stream, target, options, limit_bytes)
                });
            }
            Message::LogQuery { stream, query } => {
                self.spawn_stream(link, stream, move |agent, link, _input| obs::log_query_stream(agent, link, stream, query));
            }
            Message::PortForward { stream, pod_uid, port } => {
                let (ip, sandbox) = self.views.lock().unwrap().get(&pod_uid).map(|v| (v.ip.clone(), v.sandbox.clone())).unwrap_or_default();
                let host_network = sandbox.is_some() && ip.as_deref() == Some(self.config.node_ip.as_str());
                // A hostNetwork pod's sandbox shares the node's (or Docker VM's) network.
                let ip = if host_network { Some("127.0.0.1".to_string()) } else { ip };
                self.spawn_stream(link, stream, move |agent, link, input| port_forward_stream(agent, link, stream, ip, sandbox, port, input));
            }
            Message::Resize { stream, width, height } => self.stream_input(stream, StreamInput::Resize(width, height)),
            Message::Cancel { stream } => {
                if let Some((_, task)) = self.streams.lock().unwrap().remove(&stream) {
                    task.abort();
                }
            }
            other => eprintln!("node-agent: unexpected message {other:?}"),
        }
    }

    /// Resolve a pod container to a runtime container id, or the reason
    /// there is none (as the API server words it).
    fn container_id(&self, uid: &str, container: &str, previous: bool) -> Result<String, String> {
        let views = self.views.lock().unwrap();
        let view = views.get(uid).ok_or_else(|| "pod is not running on this node".to_string())?;
        let name = if container.is_empty() {
            view.containers.keys().next().cloned().unwrap_or_default()
        } else {
            container.to_string()
        };
        let (current, prev) = view
            .containers
            .get(&name)
            .ok_or_else(|| format!("container {name} is not valid for pod"))?;
        let id = if previous { prev } else { current };
        id.clone().ok_or_else(|| {
            if previous {
                format!("previous terminated container \"{name}\" not found")
            } else {
                format!("container \"{name}\" is waiting to start: ContainerCreating")
            }
        })
    }

    fn spawn_stream<F, Fut>(self: &Arc<Self>, link: &Arc<Link>, stream: u32, body: F)
    where
        F: FnOnce(Arc<Self>, Arc<Link>, mpsc::UnboundedReceiver<StreamInput>) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let fut = body(self.clone(), link.clone(), rx);
        let agent = self.clone();
        // Hold the lock while spawning: a stream that ends at once must not
        // remove its entry before it is inserted (it would never go away).
        let mut streams = self.streams.lock().unwrap();
        let task = tokio::spawn(async move {
            fut.await;
            agent.streams.lock().unwrap().remove(&stream);
        });
        streams.insert(stream, (tx, task));
    }
}

async fn finish(link: &Link, stream: u32, exit_code: Option<i64>, error: Option<String>) {
    let _ = link.tx.send(Frame::CloseInput { stream }).await;
    let _ = link.send(Message::StreamDone { stream, exit_code, error }).await;
}

#[allow(clippy::too_many_arguments)]
async fn exec_stream<R: PodRuntime>(
    agent: Arc<Agent<R>>,
    link: Arc<Link>,
    stream: u32,
    target: Result<String, String>,
    command: Vec<String>,
    stdin: bool,
    tty: bool,
    mut input: mpsc::UnboundedReceiver<StreamInput>,
) {
    let id = match target {
        Ok(id) => id,
        Err(e) => return finish(&link, stream, None, Some(e)).await,
    };
    let session = match agent.rt.exec(&id, command, tty, stdin).await {
        Ok(s) => s,
        Err(e) => return finish(&link, stream, None, Some(format!("{e:#}"))).await,
    };
    let exec_id = session.id.clone();
    let mut output = session.output;
    let mut stdin_writer = Some(session.input);
    let mut input_open = true;
    loop {
        tokio::select! {
            chunk = output.next() => match chunk {
                Some(Ok((channel, bytes))) => {
                    if link.tx.send(Frame::Data { stream, channel: channel.max(1), bytes }).await.is_err() {
                        return;
                    }
                }
                _ => break,
            },
            msg = input.recv(), if input_open => match msg {
                Some(StreamInput::Data(_, bytes)) => {
                    if let Some(w) = stdin_writer.as_mut() {
                        if w.write_all(&bytes).await.is_err() {
                            stdin_writer = None;
                        }
                    }
                }
                Some(StreamInput::Close) => {
                    if let Some(mut w) = stdin_writer.take() {
                        let _ = w.shutdown().await;
                    }
                }
                Some(StreamInput::Resize(w, h)) => {
                    let _ = agent.rt.resize_exec(&exec_id, w, h).await;
                }
                None => input_open = false,
            },
        }
    }
    let code = agent.rt.exec_exit_code(&exec_id).await.ok().flatten();
    finish(&link, stream, code, None).await;
}

async fn logs_stream<R: PodRuntime>(
    agent: Arc<Agent<R>>,
    link: Arc<Link>,
    stream: u32,
    target: Result<String, String>,
    options: LogOptions,
    limit_bytes: Option<i64>,
) {
    let id = match target {
        Ok(id) => id,
        Err(e) => return finish(&link, stream, None, Some(e)).await,
    };
    let mut logs = agent.rt.logs(&id, options);
    let mut left = limit_bytes.filter(|l| *l > 0).map(|l| l as usize);
    while let Some(chunk) = logs.next().await {
        let Ok((_, mut bytes)) = chunk else { break };
        if let Some(l) = left.as_mut() {
            if bytes.len() >= *l {
                bytes.truncate(*l);
                let _ = link.tx.send(Frame::Data { stream, channel: 1, bytes }).await;
                break;
            }
            *l -= bytes.len();
        }
        if link.tx.send(Frame::Data { stream, channel: 1, bytes }).await.is_err() {
            return;
        }
    }
    finish(&link, stream, None, None).await;
}

async fn port_forward_stream<R: PodRuntime>(
    agent: Arc<Agent<R>>,
    link: Arc<Link>,
    stream: u32,
    ip: Option<String>,
    sandbox: Option<String>,
    port: u16,
    input: mpsc::UnboundedReceiver<StreamInput>,
) {
    let Some(ip) = ip else {
        return finish(&link, stream, None, Some("pod has no IP address yet".into())).await;
    };
    // Dial inside the pod's network namespace when the runtime can (that also
    // reaches listeners bound to the pod's loopback), else the pod's address.
    let dial = || {
        let (agent, sandbox, ip) = (agent.clone(), sandbox.clone(), ip.clone());
        async move {
            if let Some(sb) = &sandbox {
                if let Some(conn) = agent.rt.connect(sb, port).await? {
                    return Ok(conn);
                }
            }
            tokio::net::TcpStream::connect((ip.as_str(), port)).await
        }
    };
    // A container counts as started before its process listens: retry
    // refused connections briefly instead of failing the client's stream
    // (kubectl ends the whole port-forward on an error). The node's path to
    // a pod address can also be missing (Docker in a VM: Docker Desktop has
    // none, OrbStack's sometimes stays stale for a new address); then, and
    // for a process that only listens on loopback, connect from inside the
    // pod's network namespace instead, as the kubelet does.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let conn = loop {
        let attempt = tokio::time::timeout(Duration::from_secs(1), dial()).await;
        let retry = tokio::time::Instant::now() < deadline;
        match attempt {
            Ok(Ok(c)) => break Some(c),
            Ok(Err(e)) if retry && e.kind() == std::io::ErrorKind::ConnectionRefused => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            _ if sandbox.is_some() && !agent.rt.reaches_pods_directly() => break None,
            Ok(Err(e)) => return finish(&link, stream, None, Some(format!("failed to connect to {ip}:{port}: {e}"))).await,
            Err(_) => return finish(&link, stream, None, Some(format!("timed out connecting to {ip}:{port}"))).await,
        }
    };
    let Some(conn) = conn else {
        let command = vec!["nc".to_string(), "127.0.0.1".to_string(), port.to_string()];
        return exec_stream(agent, link, stream, Ok(sandbox.unwrap_or_default()), command, true, false, input).await;
    };
    let mut input = input;
    let _ = conn.set_nodelay(true);
    let (mut r, mut w) = conn.into_split();
    let tx = link.tx.clone();
    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match tokio::io::AsyncReadExt::read(&mut r, &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(Frame::Data { stream, channel: 1, bytes: Bytes::copy_from_slice(&buf[..n]) }).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut reader = reader;
    loop {
        tokio::select! {
            _ = &mut reader => break,
            msg = input.recv() => match msg {
                Some(StreamInput::Data(_, bytes)) => {
                    if w.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                Some(StreamInput::Close) => {
                    let _ = w.shutdown().await;
                }
                Some(StreamInput::Resize(..)) => {}
                None => break,
            },
        }
    }
    reader.abort();
    finish(&link, stream, None, None).await;
}

// ---------------------------------------------------------------------------
// Starting agents

/// Describe this machine as a Node object.
pub async fn describe_node<R: PodRuntime>(rt: &R, node_name: &str, node_ip: &str, pod_cidr: &str) -> Result<Value> {
    let runtime::MachineInfo { cpus, memory, arch, kernel, os, runtime_version } = rt.machine().await?;
    let mem_ki = memory / 1024;
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let condition = |t: &str, status: &str, reason: &str, message: &str| {
        json!({"type": t, "status": status, "reason": reason, "message": message, "lastHeartbeatTime": now, "lastTransitionTime": now})
    };
    Ok(json!({
        "apiVersion": "v1",
        "kind": "Node",
        "metadata": {
            "name": node_name,
            "labels": {
                "kubernetes.io/hostname": node_name,
                "kubernetes.io/os": "linux",
                "kubernetes.io/arch": arch,
                "beta.kubernetes.io/os": "linux",
                "beta.kubernetes.io/arch": arch,
                "node.kubernetes.io/instance-type": "litek8s",
            },
            "annotations": {"node.alpha.kubernetes.io/ttl": "0"},
        },
        "spec": {"podCIDR": pod_cidr, "podCIDRs": [pod_cidr]},
        "status": {
            "capacity": {"cpu": cpus.to_string(), "memory": format!("{mem_ki}Ki"), "pods": "110", "ephemeral-storage": "100Gi"},
            "allocatable": {"cpu": cpus.to_string(), "memory": format!("{mem_ki}Ki"), "pods": "110", "ephemeral-storage": "100Gi"},
            "conditions": [
                condition("MemoryPressure", "False", "KubeletHasSufficientMemory", "kubelet has sufficient memory available"),
                condition("DiskPressure", "False", "KubeletHasNoDiskPressure", "kubelet has no disk pressure"),
                condition("PIDPressure", "False", "KubeletHasSufficientPID", "kubelet has sufficient PID available"),
                condition("Ready", "True", "KubeletReady", "litek8s node agent is ready"),
            ],
            "addresses": [
                {"type": "InternalIP", "address": node_ip},
                {"type": "Hostname", "address": node_name},
            ],
            "daemonEndpoints": {"kubeletEndpoint": {"Port": 10250}},
            "nodeInfo": {
                "machineID": "",
                "systemUUID": "",
                "bootID": "",
                "kernelVersion": kernel,
                "osImage": os,
                "containerRuntimeVersion": runtime_version,
                "kubeletVersion": crate::kube::discovery::KUBE_GIT_VERSION,
                "kubeProxyVersion": "",
                "operatingSystem": "linux",
                "architecture": arch,
            },
        },
    }))
}

/// The address other machines reach this one at (the source address of the
/// default route), or loopback.
pub fn local_ip() -> String {
    // The dev VM pins it (its own address arrives by DHCP after startup).
    if let Ok(ip) = std::env::var("LITEK8S_NODE_IP") {
        if !ip.is_empty() {
            return ip;
        }
    }
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("10.255.255.255:9")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into())
}

/// A node name from the hostname, as Kubernetes would derive it.
pub fn default_node_name() -> String {
    let raw = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let name: String = raw
        .trim()
        .split('.')
        .next()
        .unwrap_or_default()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    let name = name.trim_matches('-').to_string();
    if name.is_empty() { "litek8s-node".into() } else { name }
}

pub fn default_root_dir() -> PathBuf {
    directories::ProjectDirs::from("dev", "litek8s", "litek8s")
        .map(|d| d.data_dir().join("node-agent"))
        .unwrap_or_else(|| std::env::temp_dir().join("litek8s-node-agent"))
}

/// What a node needs to know about its place in the cluster network.
#[derive(Debug, Clone)]
pub struct NodeNetwork {
    /// This node's block of the cluster CIDR (from joining).
    pub pod_cidr: String,
    pub cluster_cidr: String,
    /// The node's address (`InternalIP`, the WireGuard endpoint).
    pub node_ip: String,
    /// UDP port of the WireGuard overlay; `None` runs without one.
    pub wireguard_port: Option<u16>,
}

/// The pod CIDR this node had at its last join (kept in `join.json`).
pub fn saved_pod_cidr(node_dir: &std::path::Path) -> Option<String> {
    let saved: crate::kube::join::JoinResponse = serde_json::from_slice(&std::fs::read(node_dir.join("join.json")).ok()?).ok()?;
    Some(saved.pod_cidr)
}

/// Build an agent for this machine on the chosen runtime.
pub async fn node_agent(
    choice: RuntimeChoice,
    node_name: Option<String>,
    root_dir: Option<PathBuf>,
    network: NodeNetwork,
) -> Result<Arc<Agent<NodeRuntime>>> {
    node_agent_with(choice, node_name, root_dir, network, None).await
}

/// [`node_agent`], with this node's own heartbeat grace period.
pub async fn node_agent_with(
    choice: RuntimeChoice,
    node_name: Option<String>,
    root_dir: Option<PathBuf>,
    network: NodeNetwork,
    grace_period: Option<Duration>,
) -> Result<Arc<Agent<NodeRuntime>>> {
    let root_dir = root_dir.unwrap_or_else(default_root_dir);
    std::fs::create_dir_all(&root_dir).with_context(|| format!("failed to create {}", root_dir.display()))?;
    let use_crun = match choice {
        RuntimeChoice::Crun => {
            crun::available().map_err(|e| anyhow::anyhow!("cannot use the crun runtime: {e}"))?;
            true
        }
        RuntimeChoice::Docker => false,
        RuntimeChoice::Auto => crun::available().is_ok(),
    };
    // Only crun puts pods on a routed bridge the overlay can carry; Docker
    // pods stay on the node (single-node development, macOS).
    let wireguard = network.wireguard_port.filter(|_| use_crun);
    let rt = if use_crun {
        let mtu = if wireguard.is_some() { overlay::MTU } else { 1500 };
        NodeRuntime::Crun(crun::CrunPods::new(&root_dir, &network.pod_cidr, &network.cluster_cidr, mtu).await?)
    } else {
        let docker = crate::runtime::DockerRuntime::connect()?;
        docker.ping().await?;
        NodeRuntime::Docker(DockerPods::new(docker.client()))
    };
    let overlay = match wireguard {
        Some(port) => match overlay::Overlay::start(&root_dir, port, &network.pod_cidr, &network.cluster_cidr).await {
            Ok(o) => Some(o),
            Err(e) => {
                eprintln!("node-agent: running without the pod overlay: {e:#}");
                None
            }
        },
        None => None,
    };
    let node_name = node_name.unwrap_or_else(default_node_name);
    let node_ip = network.node_ip.clone();
    let mut node = describe_node(&rt, &node_name, &node_ip, &network.pod_cidr).await?;
    if let Some(o) = &overlay {
        node["metadata"]["annotations"][overlay::ANNOTATION_PUBLIC_KEY] = json!(o.public_key());
        node["metadata"]["annotations"][overlay::ANNOTATION_ENDPOINT] = json!(format!("{node_ip}:{}", o.port()));
    }
    if let Some(grace) = grace_period {
        node["metadata"]["annotations"][ANNOTATION_GRACE_PERIOD] = json!(grace.as_secs().to_string());
    }
    let saved = crate::kube::join::JoinResponse {
        pod_cidr: network.pod_cidr.clone(),
        cluster_cidr: network.cluster_cidr.clone(),
        service_cidr: crate::kube::join::SERVICE_CIDR.into(),
        servers: Vec::new(),
    };
    let _ = std::fs::write(root_dir.join("join.json"), serde_json::to_vec(&saved)?);
    eprintln!("node-agent: running pods with {}", node.pointer("/status/nodeInfo/containerRuntimeVersion").and_then(Value::as_str).unwrap_or("?"));
    let agent = Agent::with_overlay(rt, AgentConfig { node_name, root_dir, node, node_ip }, overlay);
    tokio::spawn(kubelet::serve(agent.clone(), kubelet::PORT));
    Ok(agent)
}

/// Run an agent inside the API server process, connected by a pipe.
pub async fn run_local<R: PodRuntime>(api: Arc<crate::kube::ApiState>, agent: Arc<Agent<R>>, api_port: u16) {
    loop {
        let (server_end, agent_end) = tokio::io::duplex(1 << 20);
        let server = tokio::spawn(AgentHub::serve_agent(api.clone(), server_end, "host-gateway".into(), api_port, None));
        if let Err(e) = agent.session(agent_end).await {
            eprintln!("node-agent: local session ended: {e:#}");
        }
        server.abort();
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Run an agent against a remote API server, reconnecting as needed. With
/// several servers (HA) it moves to another one when the one in use is
/// unreachable or reports it lost the cluster.
pub async fn run_remote<R: PodRuntime>(agent: Arc<Agent<R>>, mut client: crate::kube::join::Client) -> Result<()> {
    let mut backoff = Duration::from_millis(500);
    let mut failures = 0usize;
    loop {
        match client.connect().await {
            Ok(io) => {
                backoff = Duration::from_millis(500);
                failures = 0;
                eprintln!("node-agent: connected to {}", client.server_host());
                let session = agent.session(io);
                tokio::pin!(session);
                let mut probe = tokio::time::interval(Duration::from_secs(2));
                let mut unready = 0;
                loop {
                    tokio::select! {
                        r = &mut session => {
                            if let Err(e) = r {
                                eprintln!("node-agent: session ended: {e:#}");
                            }
                            break;
                        }
                        _ = probe.tick(), if client.server_count() > 1 => {
                            unready = if client.ready().await { 0 } else { unready + 1 };
                            if unready >= 3 {
                                eprintln!("node-agent: {} is not ready (cut off from the other servers?); switching", client.server_host());
                                client.next_server();
                                break;
                            }
                        }
                    }
                }
                continue;
            }
            Err(e) => {
                eprintln!("node-agent: failed to connect to {}: {e:#}", client.server_host());
                failures += 1;
                client.next_server();
            }
        }
        // Back off once every known server has been tried.
        if failures % client.server_count().max(1) == 0 {
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(15));
        }
    }
}
