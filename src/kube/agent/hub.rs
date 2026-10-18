//! Server side of the node agent protocol.
//!
//! For each connected node the hub pushes the pods bound to it (an initial
//! sync, then every change, straight from the store's change feed), applies
//! the status the agent reports, answers its lookups, and relays log, exec
//! and port-forward streams opened by API clients.
//!
//! Heartbeats renew the node's Lease in `kube-node-lease`. A dropped
//! connection does not change the node's status by itself: the node
//! lifecycle controller decides when a node whose Lease stopped is Unknown
//! (after a grace period, so a laptop that sleeps or a node that restarts
//! quickly keeps its pods).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use base64::Engine;
use hyper::body::Bytes;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, BufReader, BufWriter};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use super::overlay::{ANNOTATION_ENDPOINT, ANNOTATION_PUBLIC_KEY, NodePeer};
use super::protocol::{Frame, Message, read_frame, write_frame};
use crate::kube::controllers::util;
use crate::kube::server::ApiState;
use crate::kube::store::EventType;

/// What a relayed stream delivers to the API handler.
#[derive(Debug)]
pub enum StreamEvent {
    Data { channel: u8, bytes: Bytes },
    Done { exit_code: Option<i64>, error: Option<String> },
}

struct NodeConn {
    id: u64,
    tx: mpsc::Sender<Frame>,
    streams: Arc<Mutex<HashMap<u32, mpsc::Sender<StreamEvent>>>>,
}

#[derive(Default)]
pub struct AgentHub {
    nodes: Mutex<HashMap<String, NodeConn>>,
    next_stream: AtomicU32,
    next_conn: AtomicU64,
}

/// An open relay to an agent. Dropping it cancels the stream on the node.
pub struct StreamHandle {
    pub id: u32,
    tx: mpsc::Sender<Frame>,
    pub events: mpsc::Receiver<StreamEvent>,
    streams: Arc<Mutex<HashMap<u32, mpsc::Sender<StreamEvent>>>>,
}

impl StreamHandle {
    pub async fn send(&self, channel: u8, bytes: Bytes) -> bool {
        self.tx.send(Frame::Data { stream: self.id, channel, bytes }).await.is_ok()
    }

    pub async fn close_input(&self) {
        let _ = self.tx.send(Frame::CloseInput { stream: self.id }).await;
    }

    pub async fn resize(&self, width: u16, height: u16) {
        let _ = self.tx.send(Frame::Control(Message::Resize { stream: self.id, width, height })).await;
    }

    /// A sender for input, usable from another task.
    pub fn input(&self) -> StreamInput {
        StreamInput { id: self.id, tx: self.tx.clone() }
    }
}

#[derive(Clone)]
pub struct StreamInput {
    id: u32,
    tx: mpsc::Sender<Frame>,
}

impl StreamInput {
    pub async fn send(&self, channel: u8, bytes: Bytes) -> bool {
        self.tx.send(Frame::Data { stream: self.id, channel, bytes }).await.is_ok()
    }

    pub async fn close(&self) {
        let _ = self.tx.send(Frame::CloseInput { stream: self.id }).await;
    }

    pub async fn resize(&self, width: u16, height: u16) {
        let _ = self.tx.send(Frame::Control(Message::Resize { stream: self.id, width, height })).await;
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.streams.lock().unwrap().remove(&self.id);
        let _ = self.tx.try_send(Frame::Control(Message::Cancel { stream: self.id }));
    }
}

/// The server closes a connection the agent has been silent on this long
/// (agents send a heartbeat every 10 s).
const AGENT_SILENCE: std::time::Duration = std::time::Duration::from_secs(35);
/// How long a node's Lease lasts without renewal (as the kubelet sets it).
pub const LEASE_DURATION_SECS: i64 = 40;

impl AgentHub {
    pub fn is_connected(&self, node: &str) -> bool {
        self.nodes.lock().unwrap().contains_key(node)
    }

    pub fn connected_count(&self) -> usize {
        self.nodes.lock().unwrap().len()
    }

    pub fn stream_count(&self) -> usize {
        self.nodes.lock().unwrap().values().map(|c| c.streams.lock().unwrap().len()).sum()
    }

    /// Open a stream on `node`. `request` builds the opening message from
    /// the stream id.
    pub async fn open_stream(&self, node: &str, request: impl FnOnce(u32) -> Message) -> Result<StreamHandle> {
        let (tx, streams) = {
            let nodes = self.nodes.lock().unwrap();
            let conn = nodes.get(node).with_context(|| format!("node \"{node}\" is not connected"))?;
            (conn.tx.clone(), conn.streams.clone())
        };
        let id = self.next_stream.fetch_add(1, Ordering::Relaxed) + 1;
        let (events_tx, events) = mpsc::channel(64);
        streams.lock().unwrap().insert(id, events_tx);
        let handle = StreamHandle { id, tx, events, streams };
        handle
            .tx
            .send(Frame::Control(request(id)))
            .await
            .map_err(|_| anyhow::anyhow!("node \"{node}\" disconnected"))?;
        Ok(handle)
    }

    /// Serve one agent connection until it ends. `authenticated` is the
    /// node the connection's credentials are for (`None`: the in-process
    /// local node).
    pub async fn serve_agent<IO>(api: Arc<ApiState>, io: IO, api_host: String, api_port: u16, authenticated: Option<String>) -> Result<()>
    where
        IO: AsyncRead + AsyncWrite + Send + 'static,
    {
        let hub = api.hub.clone();
        let (reader, writer) = tokio::io::split(io);
        let mut reader = BufReader::new(reader);
        let node = match tokio::time::timeout(AGENT_SILENCE, read_frame(&mut reader)).await {
            Ok(Ok(Some(Frame::Control(Message::Hello { node })))) => node,
            _ => bail!("agent did not say hello"),
        };
        let node_name = util::name(&node);
        if node_name.is_empty() {
            bail!("agent sent a Node without a name");
        }
        if authenticated.as_ref().is_some_and(|n| *n != node_name) {
            bail!("agent authenticated as {authenticated:?} but announced node {node_name}");
        }
        register_node(&api, &node)?;
        renew_lease(&api, &node_name);

        let (tx, mut rx) = mpsc::channel::<Frame>(1024);
        let streams: Arc<Mutex<HashMap<u32, mpsc::Sender<StreamEvent>>>> = Arc::default();
        let conn_id = hub.next_conn.fetch_add(1, Ordering::Relaxed);
        hub.nodes.lock().unwrap().insert(
            node_name.clone(),
            NodeConn { id: conn_id, tx: tx.clone(), streams: streams.clone() },
        );
        eprintln!("apiserver: node {node_name} connected");

        let writer_task = tokio::spawn(async move {
            let mut writer = BufWriter::new(writer);
            while let Some(frame) = rx.recv().await {
                if write_frame(&mut writer, &frame).await.is_err() {
                    break;
                }
                // Flush once the queue is drained, batching bursts.
                if rx.is_empty() && tokio::io::AsyncWriteExt::flush(&mut writer).await.is_err() {
                    break;
                }
            }
        });
        let _ = tx
            .send(Frame::Control(Message::Welcome {
                node: node_name.clone(),
                api_host,
                api_port,
                cluster_dns: api.cluster_dns.load(std::sync::atomic::Ordering::Relaxed),
            }))
            .await;
        let feed_task = tokio::spawn(feed_pods(api.clone(), node_name.clone(), tx.clone()));

        let status = node.get("status").cloned().unwrap_or_else(|| json!({}));
        let result = async {
            loop {
                let frame = match tokio::time::timeout(AGENT_SILENCE, read_frame(&mut reader)).await {
                    Ok(frame) => frame?,
                    Err(_) => bail!("no heartbeat from node {node_name} for {}s", AGENT_SILENCE.as_secs()),
                };
                let Some(frame) = frame else { break };
                match frame {
                    Frame::Control(Message::Heartbeat) => {
                        renew_lease(&api, &node_name);
                        if ensure_ready(&api, &node_name, &status) {
                            let _ = tx.send(Frame::Control(Message::Resync)).await;
                        }
                        let _ = tx.send(Frame::Control(Message::HeartbeatAck)).await;
                    }
                    Frame::Control(msg) => handle_message(&api, &tx, &streams, msg).await,
                    Frame::Data { stream, channel, bytes } => {
                        let sender = streams.lock().unwrap().get(&stream).cloned();
                        if let Some(sender) = sender {
                            let _ = sender.send(StreamEvent::Data { channel, bytes }).await;
                        }
                    }
                    // The agent's output ended; StreamDone follows.
                    Frame::CloseInput { .. } => {}
                }
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;

        feed_task.abort();
        writer_task.abort();
        {
            let mut nodes = hub.nodes.lock().unwrap();
            if nodes.get(&node_name).is_some_and(|c| c.id == conn_id) {
                nodes.remove(&node_name);
            }
        }
        streams.lock().unwrap().clear();
        eprintln!("apiserver: node {node_name} disconnected");
        result
    }
}

/// Create or refresh the Node object an agent describes, keeping labels,
/// taints and other fields users set on it.
fn register_node(api: &ApiState, node: &Value) -> Result<()> {
    let name = util::name(node);
    let status = node.get("status").cloned().unwrap_or_else(|| json!({}));
    let current = match api.get_object("", "nodes", "", &name) {
        Some(existing) => {
            let mut updated = existing.clone();
            for field in ["labels", "annotations"] {
                if let Some(values) = node.pointer(&format!("/metadata/{field}")).and_then(Value::as_object) {
                    if !updated["metadata"].get(field).is_some_and(Value::is_object) {
                        updated["metadata"][field] = json!({});
                    }
                    for (k, v) in values {
                        updated["metadata"][field][k] = v.clone();
                    }
                }
            }
            // A node's pod CIDR is the one it joined with.
            if util::s(&updated, "/spec/podCIDR").is_empty() {
                if let Some(cidr) = node.pointer("/spec/podCIDR") {
                    updated["spec"]["podCIDR"] = cidr.clone();
                    updated["spec"]["podCIDRs"] = json!([cidr]);
                }
            }
            if updated != existing {
                api.update_object("", "nodes", updated).map_err(|e| anyhow::anyhow!("{e}"))?
            } else {
                existing
            }
        }
        None => api
            .create_object("", "nodes", None, node.clone())
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    };
    util::write_status(api, "", "nodes", &current, status).map_err(|e| anyhow::anyhow!("{e}"))
}

fn micro_time() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

/// Renew (or create) the node's Lease, as the kubelet does. The server's
/// clock stamps it, so nodes with skewed clocks are judged fairly.
pub(crate) fn renew_lease(api: &ApiState, node: &str) {
    for _ in 0..3 {
        let now = micro_time();
        let result = match api.get_object("coordination.k8s.io", "leases", "kube-node-lease", node) {
            Some(mut lease) => {
                lease["spec"]["renewTime"] = json!(now);
                lease["spec"]["holderIdentity"] = json!(node);
                api.update_object("coordination.k8s.io", "leases", lease).map(|_| ())
            }
            None => {
                let owner = api.get_object("", "nodes", "", node).map(|n| {
                    json!([{"apiVersion": "v1", "kind": "Node", "name": node, "uid": util::uid(&n)}])
                });
                let mut lease = json!({
                    "apiVersion": "coordination.k8s.io/v1",
                    "kind": "Lease",
                    "metadata": {"name": node, "namespace": "kube-node-lease"},
                    "spec": {"holderIdentity": node, "leaseDurationSeconds": LEASE_DURATION_SECS, "renewTime": now, "acquireTime": now},
                });
                if let Some(owner) = owner {
                    lease["metadata"]["ownerReferences"] = owner;
                }
                api.create_object("coordination.k8s.io", "leases", Some("kube-node-lease"), lease).map(|_| ())
            }
        };
        match result {
            Err(e) if e.is_conflict() || e.is_already_exists() => continue,
            Err(e) => return eprintln!("apiserver: lease of node {node}: {e}"),
            Ok(()) => return,
        }
    }
}

/// A node that heartbeats is Ready: when the lifecycle controller had
/// marked it otherwise (its Lease expired while the connection stayed up,
/// e.g. the node slept), post the status it announced again. Returns
/// whether it did, so the agent re-reports its pods.
fn ensure_ready(api: &ApiState, node: &str, announced: &Value) -> bool {
    let Some(current) = api.get_object("", "nodes", "", node) else { return false };
    if util::condition_true(&current, "Ready") {
        return false;
    }
    let mut status = announced.clone();
    let now = util::rfc3339(util::now());
    for c in status.get_mut("conditions").and_then(Value::as_array_mut).into_iter().flatten() {
        c["lastHeartbeatTime"] = json!(now);
        c["lastTransitionTime"] = json!(now);
    }
    util::write_status(api, "", "nodes", &current, status).is_ok()
}

/// Every node of the WireGuard overlay (with its key, endpoint and CIDR).
fn overlay_peers(api: &ApiState) -> Vec<NodePeer> {
    let (nodes, _) = api.store().list("", "nodes", None);
    nodes
        .iter()
        .filter_map(|n| {
            let annotation = |k: &str| n.pointer("/metadata/annotations").and_then(|a| a.get(k)).and_then(Value::as_str).map(str::to_string);
            let pod_cidr = util::s(n, "/spec/podCIDR");
            if pod_cidr.is_empty() {
                return None;
            }
            Some(NodePeer {
                name: util::name(n),
                public_key: annotation(ANNOTATION_PUBLIC_KEY)?,
                endpoint: annotation(ANNOTATION_ENDPOINT)?,
                pod_cidr: pod_cidr.to_string(),
            })
        })
        .collect()
}

/// The part of a pod the agent acts on; status changes are not resent.
fn pod_fingerprint(pod: &Value) -> Value {
    json!({
        "spec": pod.get("spec"),
        "deletionTimestamp": pod.pointer("/metadata/deletionTimestamp"),
        "labels": pod.pointer("/metadata/labels"),
        "annotations": pod.pointer("/metadata/annotations"),
    })
}

/// Push the node's pods: a full sync, then every relevant change.
async fn feed_pods(api: Arc<ApiState>, node: String, tx: mpsc::Sender<Frame>) {
    loop {
        let Ok(sub) = api.store().subscribe(None) else { return };
        let mut live = sub.live;
        let mut sent: HashMap<String, Value> = HashMap::new();
        // Pods on this node per namespace, to forward only relevant changes.
        let mut namespaces: HashMap<String, usize> = HashMap::new();
        let mut peers = overlay_peers(&api);
        if tx.send(Frame::Control(Message::Peers { peers: peers.clone() })).await.is_err() {
            return;
        }
        let (pods, _) = api.store().list("", "pods", None);
        for pod in pods.iter().filter(|p| util::s(p, "/spec/nodeName") == node) {
            sent.insert(util::uid(pod), pod_fingerprint(pod));
            *namespaces.entry(util::s(pod, "/metadata/namespace").to_string()).or_default() += 1;
            if tx.send(Frame::Control(Message::PodUpsert { pod: pod.clone() })).await.is_err() {
                return;
            }
        }
        if tx.send(Frame::Control(Message::SyncDone)).await.is_err() {
            return;
        }
        let (services, _) = api.store().list("", "services", None);
        for svc in &services {
            let (ns, name) = util::ns_name(svc);
            if tx.send(Frame::Control(service_update(&api, &ns, &name))).await.is_err() {
                return;
            }
        }
        loop {
            let event = match live.recv().await {
                Ok(e) => e,
                Err(RecvError::Lagged(_)) => break,
                Err(RecvError::Closed) => return,
            };
            let msg = match (event.key.group.as_str(), event.key.resource.as_str()) {
                ("", "pods") if util::s(&event.object, "/spec/nodeName") == node => {
                    let uid = util::uid(&event.object);
                    let ns = event.key.namespace.clone();
                    if event.event == EventType::Deleted {
                        if sent.remove(&uid).is_some() {
                            if let Some(n) = namespaces.get_mut(&ns) {
                                *n = n.saturating_sub(1);
                            }
                        }
                        Message::PodDelete { uid }
                    } else {
                        let fp = pod_fingerprint(&event.object);
                        if sent.get(&uid) == Some(&fp) {
                            continue;
                        }
                        if sent.insert(uid, fp).is_none() {
                            *namespaces.entry(ns).or_default() += 1;
                        }
                        Message::PodUpsert { pod: event.object.clone() }
                    }
                }
                ("", "services" | "endpoints") => service_update(&api, &event.key.namespace, &event.key.name),
                ("", "nodes") => {
                    let now = overlay_peers(&api);
                    if now == peers {
                        continue;
                    }
                    peers = now;
                    Message::Peers { peers: peers.clone() }
                }
                ("", resource @ ("configmaps" | "secrets")) if event.event != EventType::Deleted => {
                    let ns = &event.key.namespace;
                    if !namespaces.get(ns).is_some_and(|n| *n > 0) {
                        continue;
                    }
                    Message::ObjectChanged { resource: resource.to_string(), namespace: ns.clone(), name: event.key.name.clone() }
                }
                ("", "persistentvolumes") if event.event == EventType::Deleted => {
                    let on_node = event
                        .object
                        .pointer("/spec/nodeAffinity/required/nodeSelectorTerms")
                        .is_some_and(|t| t.to_string().contains(&format!("\"{node}\"")));
                    if !on_node {
                        continue;
                    }
                    Message::VolumeDelete { name: event.key.name.clone() }
                }
                _ => continue,
            };
            if tx.send(Frame::Control(msg)).await.is_err() {
                return;
            }
        }
        // Lagged behind the feed: start over with a full sync.
    }
}

fn service_update(api: &ApiState, ns: &str, name: &str) -> Message {
    Message::ServiceUpdate {
        namespace: ns.to_string(),
        name: name.to_string(),
        service: api.get_object("", "services", ns, name),
        endpoints: api.get_object("", "endpoints", ns, name),
    }
}

async fn handle_message(
    api: &Arc<ApiState>,
    tx: &mpsc::Sender<Frame>,
    streams: &Arc<Mutex<HashMap<u32, mpsc::Sender<StreamEvent>>>>,
    msg: Message,
) {
    match msg {
        Message::PodStatus { namespace, name, uid, status } => {
            apply_pod_status(api, &namespace, &name, &uid, status);
        }
        Message::Get { id, resource, namespace, name } => {
            let object = lookup(api, &resource, &namespace, &name);
            let _ = tx.send(Frame::Control(Message::GetResult { id, object })).await;
        }
        Message::StreamDone { stream, exit_code, error } => {
            let sender = streams.lock().unwrap().remove(&stream);
            if let Some(sender) = sender {
                let _ = sender.send(StreamEvent::Done { exit_code, error }).await;
            }
        }
        Message::Hello { .. } => {}
        other => eprintln!("apiserver: unexpected message from agent: {other:?}"),
    }
}

/// Objects an agent may read: only what pods need.
fn lookup(api: &ApiState, resource: &str, namespace: &str, name: &str) -> Option<Value> {
    match resource {
        "configmaps" | "secrets" | "persistentvolumeclaims" | "serviceaccounts" => {
            api.get_object("", resource, namespace, name)
        }
        "persistentvolumes" => api.get_object("", resource, "", name),
        "token" => Some(json!({"token": service_account_token(namespace, name)})),
        _ => None,
    }
}

/// A JWT-shaped service account token. Nothing verifies it yet (there is no
/// authentication), but clients that decode tokens find the usual claims.
pub fn service_account_token(namespace: &str, service_account: &str) -> String {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let now = chrono::Utc::now().timestamp();
    let header = json!({"alg": "RS256", "kid": "litek8s"});
    let claims = json!({
        "aud": ["https://kubernetes.default.svc.cluster.local", "k3s"],
        "exp": now + 365 * 24 * 3600,
        "iat": now,
        "iss": "https://kubernetes.default.svc.cluster.local",
        "kubernetes.io": {"namespace": namespace, "serviceaccount": {"name": service_account}},
        "nbf": now,
        "sub": format!("system:serviceaccount:{namespace}:{service_account}"),
    });
    format!(
        "{}.{}.{}",
        b64.encode(header.to_string()),
        b64.encode(claims.to_string()),
        b64.encode(b"unsigned")
    )
}

/// Store a pod status reported by its node, keeping conditions owned by the
/// control plane (PodScheduled, DisruptionTarget).
fn apply_pod_status(api: &ApiState, ns: &str, name: &str, uid: &str, mut status: Value) {
    for _ in 0..8 {
        let Some(pod) = api.get_object("", "pods", ns, name) else { return };
        if util::uid(&pod) != uid {
            return;
        }
        let kept: Vec<Value> = util::conditions_of(&pod)
            .into_iter()
            .filter(|c| matches!(util::s(c, "/type"), "PodScheduled" | "DisruptionTarget"))
            .collect();
        let mut conditions = kept;
        for c in status.get("conditions").and_then(Value::as_array).cloned().unwrap_or_default() {
            if !conditions.iter().any(|k| k["type"] == c["type"]) {
                conditions.push(c);
            }
        }
        status["conditions"] = json!(conditions);
        if status.get("qosClass").is_none() {
            if let Some(q) = pod.pointer("/status/qosClass") {
                status["qosClass"] = q.clone();
            }
        }
        match util::write_status(api, "", "pods", &pod, status.clone()) {
            Err(e) if e.is_conflict() => continue,
            Err(e) if !e.is_not_found() => {
                eprintln!("apiserver: pod status {ns}/{name}: {e}");
                return;
            }
            _ => return,
        }
    }
}
