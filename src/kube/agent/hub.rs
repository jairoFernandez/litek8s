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
use hyper::body::Bytes;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, BufReader, BufWriter};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use super::netpol::{self, PodRules};
use super::overlay::{ANNOTATION_ENDPOINT, ANNOTATION_PUBLIC_KEY, NodePeer};
use super::relay::{self, MeshNode, Route};
use super::protocol::{Frame, Message, read_frame, write_frame};
use crate::kube::certificates;
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
    /// What the agent announced it can do beyond the base protocol.
    features: Vec<String>,
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

    /// A stream that answers every chunk of input upper-cased (tests of
    /// what relays streams).
    #[cfg(test)]
    pub fn echo_upper_for_tests() -> StreamHandle {
        let (tx, mut frames) = mpsc::channel::<Frame>(16);
        let (events_tx, events) = mpsc::channel(16);
        tokio::spawn(async move {
            while let Some(frame) = frames.recv().await {
                match frame {
                    Frame::Data { bytes, .. } => {
                        let _ = events_tx.send(StreamEvent::Data { channel: 1, bytes: Bytes::from(bytes.to_ascii_uppercase()) }).await;
                    }
                    Frame::CloseInput { .. } => {
                        let _ = events_tx.send(StreamEvent::Done { exit_code: None, error: None }).await;
                    }
                    Frame::Control(_) => break,
                }
            }
        });
        StreamHandle { id: 1, tx, events, streams: Arc::default() }
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

    /// Connected nodes whose agent announced `feature`.
    pub fn nodes_with_feature(&self, feature: &str) -> Vec<String> {
        let mut v: Vec<String> = self.nodes.lock().unwrap().iter().filter(|(_, c)| c.features.iter().any(|f| f == feature)).map(|(n, _)| n.clone()).collect();
        v.sort();
        v
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
            Ok(Ok(Some(Frame::Control(Message::Hello { node, features })))) => (node, features),
            _ => bail!("agent did not say hello"),
        };
        let (node, features) = node;
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
            NodeConn { id: conn_id, features: features.clone(), tx: tx.clone(), streams: streams.clone() },
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
                obs: api.obs.get().filter(|_| features.iter().any(|f| f == "metrics")).map(|o| o.settings()),
                overlay_status: features.iter().any(|f| f == super::OVERLAY_STATUS_FEATURE),
                network_policy: features.iter().any(|f| f == netpol::FEATURE),
            }))
            .await;
        let policies = features.iter().any(|f| f == netpol::FEATURE);
        let feed_task = tokio::spawn(feed_pods(api.clone(), node_name.clone(), tx.clone(), policies));

        let status = node.get("status").cloned().unwrap_or_else(|| json!({}));
        // This connection's metric series ids → the store's.
        let mut metric_ids: HashMap<u32, Option<u32>> = HashMap::new();
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
                    Frame::Control(Message::Metrics { ts, series, values }) => {
                        if let Some(obs) = api.obs.get() {
                            obs.ingest_node(&node_name, ts, series, values, &mut metric_ids);
                        }
                    }
                    Frame::Control(Message::OverlayStatus { peers }) => overlay_report(&api, &node_name, &peers),
                    Frame::Control(msg) => handle_message(&api, &node_name, &tx, &streams, msg).await,
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
        if let Some(obs) = api.obs.get() {
            obs.node_gone(&node_name);
        }
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
                if let Some(cluster) = api.ha() {
                    lease["metadata"]["annotations"][super::super::server::NODE_SERVER_ANNOTATION] = json!(cluster.name);
                }
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
                if let Some(cluster) = api.ha() {
                    lease["metadata"]["annotations"][super::super::server::NODE_SERVER_ANNOTATION] = json!(cluster.name);
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

/// The overlay as `node` should set it up: every other node (key,
/// endpoints, CIDR, and the relay to go through when there is no direct
/// path), and whether `node` relays for others.
fn overlay_peers(api: &ApiState, node: &str) -> (Vec<NodePeer>, bool) {
    let (nodes, _) = api.store().list("", "nodes", None);
    let members: Vec<&Value> = nodes.iter().filter(|n| !util::s(n, "/spec/podCIDR").is_empty()).collect();
    let mesh: Vec<MeshNode> = members.iter().filter_map(|n| MeshNode::from_node(n)).collect();
    let routes = relay::routes_for(&mesh, node);
    let relays = mesh.iter().find(|m| m.name == node).is_some_and(|m| m.relay);
    let peers = members
        .iter()
        .filter_map(|n| {
            let annotation = |k: &str| n.pointer("/metadata/annotations").and_then(|a| a.get(k)).and_then(Value::as_str).map(str::to_string);
            let name = util::name(n);
            let via = match routes.get(&name) {
                Some(Route::Via(r)) => Some(r.clone()),
                _ => None,
            };
            Some(NodePeer {
                public_key: annotation(ANNOTATION_PUBLIC_KEY)?,
                endpoint: annotation(ANNOTATION_ENDPOINT)?,
                pod_cidr: util::s(n, "/spec/podCIDR").to_string(),
                endpoints: relay::observed_endpoints(&mesh, &name),
                via,
                server: mesh.iter().any(|m| m.name == name && m.server),
                name,
            })
        })
        .collect();
    (peers, relays)
}

/// Set an annotation of a Node unless it already has that value.
fn annotate_node(api: &ApiState, node: &str, key: &str, value: &str) {
    for attempt in 0..5 {
        let Some(mut obj) = api.get_object("", "nodes", "", node) else { return };
        if obj.pointer("/metadata/annotations").and_then(|a| a.get(key)).and_then(Value::as_str) == Some(value) {
            return;
        }
        if !obj["metadata"]["annotations"].is_object() {
            obj["metadata"]["annotations"] = json!({});
        }
        obj["metadata"]["annotations"][key] = json!(value);
        match api.update_object("", "nodes", obj) {
            Ok(_) => return,
            Err(e) if e.is_conflict() && attempt < 4 => continue,
            Err(e) => return eprintln!("apiserver: could not annotate node {node}: {e}"),
        }
    }
}

/// An agent's report of its overlay peers: kept on its Node, and the
/// routes of every node written where they changed (agents get theirs from
/// the Node change).
fn overlay_report(api: &ApiState, node: &str, report: &relay::Reports) {
    let Some(current) = api.get_object("", "nodes", "", node) else { return };
    let merged = relay::merge_reports(&relay::reports_of(&current), report);
    annotate_node(api, node, relay::ANNOTATION_PEERS, &serde_json::to_string(&merged).unwrap_or_default());
    let (nodes, _) = api.store().list("", "nodes", None);
    let mesh: Vec<MeshNode> = nodes.iter().filter(|n| !util::s(n, "/spec/podCIDR").is_empty()).filter_map(MeshNode::from_node).collect();
    for (m, n) in mesh.iter().filter_map(|m| Some((m, nodes.iter().find(|n| util::name(n) == m.name)?))) {
        let routes = relay::routes_for(&mesh, &m.name);
        if relay::routes_of(n) != routes {
            annotate_node(api, &m.name, relay::ANNOTATION_ROUTES, &relay::routes_text(&routes));
        }
    }
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

/// The NetworkPolicy rules of a node's pods as last sent to its agent, and
/// what of every pod they were computed from.
#[derive(Default)]
struct PolicyFeed {
    /// pod uid → rules (isolated pods of this node only).
    sent: HashMap<String, PodRules>,
    /// pod uid → `crate::kube::netpol::fingerprint`, for every pod.
    seen: HashMap<String, Value>,
    /// Something changed: recompute every pod's rules at this time.
    due: Option<tokio::time::Instant>,
}

/// How long changes to policies, namespaces and pods are batched.
const POLICY_BATCH: std::time::Duration = std::time::Duration::from_millis(100);

/// The cluster as policies see it; `None` when there are no policies.
fn policy_snapshot(api: &ApiState) -> Option<(crate::kube::netpol::Snapshot, Vec<Value>)> {
    let (policies, _) = api.store().list(crate::kube::netpol::GROUP, crate::kube::netpol::RESOURCE, None);
    if policies.is_empty() {
        return None;
    }
    let (pods, _) = api.store().list("", "pods", None);
    let (namespaces, _) = api.store().list("", "namespaces", None);
    let snapshot = crate::kube::netpol::Snapshot::new(policies, namespaces, &pods);
    Some((snapshot, pods))
}

impl PolicyFeed {
    /// Rules of every pod of `node` now, and the changes from what was sent.
    fn recompute(&mut self, api: &ApiState, node: &str, full: bool) -> Option<Message> {
        self.due = None;
        let snapshot = policy_snapshot(api);
        let mut now: HashMap<String, PodRules> = HashMap::new();
        if let Some((snapshot, pods)) = &snapshot {
            for pod in pods.iter().filter(|p| util::s(p, "/spec/nodeName") == node) {
                if let Some(r) = snapshot.rules_for(pod) {
                    now.insert(util::uid(pod), r);
                }
            }
        }
        if full {
            let (pods, _) = api.store().list("", "pods", None);
            self.seen = pods.iter().map(|p| (util::uid(p), crate::kube::netpol::fingerprint(p))).collect();
        }
        let mut changes = std::collections::BTreeMap::new();
        for (uid, r) in &now {
            if full || self.sent.get(uid) != Some(r) {
                changes.insert(uid.clone(), Some(r.clone()));
            }
        }
        if !full {
            for uid in self.sent.keys().filter(|u| !now.contains_key(*u)) {
                changes.insert(uid.clone(), None);
            }
        }
        self.sent = now;
        (full || !changes.is_empty()).then_some(Message::NetPolicy { pods: changes, full })
    }

    /// Before a pod of this node is sent: its own rules, if they changed
    /// (so they are in place before it starts).
    fn before_upsert(&mut self, api: &ApiState, pod: &Value) -> Option<Message> {
        let uid = util::uid(pod);
        let rules = policy_snapshot(api).and_then(|(s, _)| s.rules_for(pod));
        if self.sent.get(&uid) == rules.as_ref() {
            return None;
        }
        match &rules {
            Some(r) => self.sent.insert(uid.clone(), r.clone()),
            None => self.sent.remove(&uid),
        };
        Some(Message::NetPolicy { pods: std::collections::BTreeMap::from([(uid, rules)]), full: false })
    }

    /// Note an event that may change some pod's rules.
    fn observe(&mut self, group: &str, resource: &str, event: EventType, object: &Value) {
        let changed = match (group, resource) {
            ("", "pods") => {
                let uid = util::uid(object);
                if event == EventType::Deleted {
                    self.seen.remove(&uid).is_some()
                } else {
                    let fp = crate::kube::netpol::fingerprint(object);
                    self.seen.insert(uid, fp.clone()) != Some(fp)
                }
            }
            ("", "namespaces") => true,
            (crate::kube::netpol::GROUP, crate::kube::netpol::RESOURCE) => true,
            _ => false,
        };
        if changed && self.due.is_none() {
            self.due = Some(tokio::time::Instant::now() + POLICY_BATCH);
        }
    }
}

/// Push the node's pods: a full sync, then every relevant change.
/// `policies`: the agent enforces NetworkPolicies, send their rules.
async fn feed_pods(api: Arc<ApiState>, node: String, tx: mpsc::Sender<Frame>, policies: bool) {
    loop {
        let Ok(sub) = api.store().subscribe(None) else { return };
        let mut live = sub.live;
        let mut sent: HashMap<String, Value> = HashMap::new();
        // Pods on this node per namespace, to forward only relevant changes.
        let mut namespaces: HashMap<String, usize> = HashMap::new();
        let mut peers = overlay_peers(&api, &node);
        if tx.send(Frame::Control(Message::Peers { peers: peers.0.clone(), relay: peers.1 })).await.is_err() {
            return;
        }
        let mut policy = policies.then(PolicyFeed::default);
        if let Some(msg) = policy.as_mut().and_then(|p| p.recompute(&api, &node, true)) {
            if tx.send(Frame::Control(msg)).await.is_err() {
                return;
            }
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
            let due = policy.as_ref().and_then(|p| p.due);
            let received = match due {
                Some(at) => tokio::select! {
                    e = live.recv() => Some(e),
                    _ = tokio::time::sleep_until(at) => None,
                },
                None => Some(live.recv().await),
            };
            let Some(received) = received else {
                if let Some(msg) = policy.as_mut().and_then(|p| p.recompute(&api, &node, false)) {
                    if tx.send(Frame::Control(msg)).await.is_err() {
                        return;
                    }
                }
                continue;
            };
            let event = match received {
                Ok(e) => e,
                Err(RecvError::Lagged(_)) => break,
                Err(RecvError::Closed) => return,
            };
            if let Some(p) = policy.as_mut() {
                p.observe(&event.key.group, &event.key.resource, event.event, &event.object);
            }
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
                        // Kubelet admission: a new pod whose hostPort another
                        // pod on this node holds is rejected.
                        // A pod that finished before this node ran it (rejected) is not run.
                        if !sent.contains_key(&uid) && util::is_terminal(&event.object) {
                            continue;
                        }
                        if !sent.contains_key(&uid) && host_port_conflict(&api, &node, &event.object) {
                            let mut status = event.object.get("status").cloned().unwrap_or_else(|| json!({}));
                            status["phase"] = json!("Failed");
                            status["reason"] = json!("NodePorts");
                            status["message"] = json!("Pod was rejected: node didn't have free ports for the requested pod ports");
                            let _ = util::write_status(&api, "", "pods", &event.object, status);
                            continue;
                        }
                        if sent.insert(uid, fp).is_none() {
                            *namespaces.entry(ns).or_default() += 1;
                        }
                        if let Some(rules) = policy.as_mut().and_then(|p| p.before_upsert(&api, &event.object)) {
                            if tx.send(Frame::Control(rules)).await.is_err() {
                                return;
                            }
                        }
                        Message::PodUpsert { pod: event.object.clone() }
                    }
                }
                ("", "services" | "endpoints") => service_update(&api, &event.key.namespace, &event.key.name),
                ("", "nodes") => {
                    let now = overlay_peers(&api, &node);
                    if now == peers {
                        continue;
                    }
                    peers = now;
                    Message::Peers { peers: peers.0.clone(), relay: peers.1 }
                }
                ("", resource @ ("configmaps" | "secrets")) if event.event != EventType::Deleted => {
                    let ns = &event.key.namespace;
                    if !namespaces.get(ns).is_some_and(|n| *n > 0) {
                        continue;
                    }
                    Message::ObjectChanged { resource: resource.to_string(), namespace: ns.clone(), name: event.key.name.clone() }
                }
                (certificates::GROUP, "clustertrustbundles") => {
                    Message::ObjectChanged { resource: "clustertrustbundles".into(), namespace: String::new(), name: event.key.name.clone() }
                }
                // A request of this node's pods was answered.
                (certificates::GROUP, "podcertificaterequests")
                    if event.event != EventType::Deleted
                        && util::s(&event.object, "/spec/nodeName") == node
                        && certificates::pcr_outcome(&event.object) != Ok(None) =>
                {
                    Message::ObjectChanged { resource: "podcertificaterequests".into(), namespace: event.key.namespace.clone(), name: event.key.name.clone() }
                }
                ("", "persistentvolumes") if event.event == EventType::Deleted => {
                    if !deletes_volume_data(&event.object, &node) {
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

/// Whether `pod` wants a (hostIP, hostPort, protocol) another active pod on
/// the node already holds.
fn host_port_conflict(api: &ApiState, node: &str, pod: &Value) -> bool {
    let ports = |p: &Value| -> Vec<(String, u64, String)> {
        p.pointer("/spec/containers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(|c| c.get("ports").and_then(Value::as_array).cloned().unwrap_or_default())
            .filter_map(|port| {
                let hp = port.get("hostPort").and_then(Value::as_u64).filter(|h| *h > 0)?;
                let ip = match util::s(&port, "/hostIP") {
                    "" => "0.0.0.0".to_string(),
                    ip => ip.to_string(),
                };
                let proto = match util::s(&port, "/protocol") {
                    "" => "TCP".to_string(),
                    p => p.to_string(),
                };
                Some((ip, hp, proto))
            })
            .collect()
    };
    let wanted = ports(pod);
    if wanted.is_empty() {
        return false;
    }
    let uid = util::uid(pod);
    let (pods, _) = api.store().list("", "pods", None);
    pods.iter()
        .filter(|p| util::s(p, "/spec/nodeName") == node && util::uid(p) != uid && util::is_active(p))
        .filter(|p| util::s(p, "/metadata/creationTimestamp") <= util::s(pod, "/metadata/creationTimestamp"))
        .flat_map(|p| ports(p))
        .any(|(ip, hp, proto)| {
            wanted.iter().any(|(wip, whp, wproto)| *whp == hp && *wproto == proto && (ip == "0.0.0.0" || *wip == "0.0.0.0" || ip == *wip))
        })
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
    node: &str,
    tx: &mpsc::Sender<Frame>,
    streams: &Arc<Mutex<HashMap<u32, mpsc::Sender<StreamEvent>>>>,
    msg: Message,
) {
    match msg {
        Message::PodStatus { namespace, name, uid, status } => {
            apply_pod_status(api, &namespace, &name, &uid, status);
        }
        Message::PodTerminated { namespace, name, uid } => api.finish_pod_deletion(&namespace, &name, &uid),
        Message::Get { id, resource, namespace, name } => {
            let object = lookup(api, node, &resource, &namespace, &name);
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
fn lookup(api: &ApiState, node: &str, resource: &str, namespace: &str, name: &str) -> Option<Value> {
    match resource {
        // `name`: a `clusterTrustBundle` projection (JSON).
        "clustertrustbundles" => Some(trust_bundles(api, &serde_json::from_str(name).ok()?)),
        "podcertificaterequests" => {
            let pcr = api.get_object(certificates::GROUP, resource, namespace, name)?;
            (util::s(&pcr, "/spec/nodeName") == node).then_some(pcr)
        }
        // `name`: JSON {podName, source, signerName, maxExpirationSeconds,
        // stubPKCS10Request, unverifiedUserAnnotations}.
        "podcertificaterequests/create" => Some(match create_pcr(api, node, namespace, name) {
            Ok(pcr) => pcr,
            Err(e) => json!({"error": e}),
        }),
        "configmaps" | "secrets" | "persistentvolumeclaims" | "serviceaccounts" => {
            api.get_object("", resource, namespace, name)
        }
        "persistentvolumes" => api.get_object("", resource, "", name),
        // `name` is the service account, or JSON {"sa", "pod", "audience", "expirationSeconds"}.
        "token" => {
            let (sa, pod, aud, exp) = match serde_json::from_str::<Value>(name) {
                Ok(v) if v.is_object() => (
                    util::s(&v, "/sa").to_string(),
                    util::s(&v, "/pod").to_string(),
                    util::s(&v, "/audience").to_string(),
                    v.get("expirationSeconds").and_then(Value::as_i64),
                ),
                _ => (name.to_string(), String::new(), String::new(), None),
            };
            let audiences = if aud.is_empty() { Vec::new() } else { vec![aud] };
            let bound = (!pod.is_empty()).then_some(("Pod", pod.as_str()));
            // Like --service-account-extend-token-expiration: the kubelet's
            // default-lifetime tokens stay valid for a year.
            let exp = match exp.unwrap_or(3607) {
                3607 => 365 * 24 * 3600,
                e => e.max(600),
            };
            api.mint_token(namespace, &sa, audiences, exp, bound).ok().map(|(token, _)| json!({"token": token}))
        }
        _ => None,
    }
}

/// The trust bundles a `clusterTrustBundle` projection selects:
/// `{"bundles": [trustBundle, ...]}`.
fn trust_bundles(api: &ApiState, projection: &Value) -> Value {
    let bundles: Vec<Value> = if let Some(name) = projection.get("name").and_then(Value::as_str) {
        api.get_object(certificates::GROUP, "clustertrustbundles", "", name).into_iter().collect()
    } else {
        let signer = util::s(projection, "/signerName");
        // No selector selects nothing; an empty one everything.
        let Some(selector) = crate::kube::selector::LabelSelector::from_object(projection.get("labelSelector")) else {
            return json!({"bundles": []});
        };
        let (all, _) = api.store().list(certificates::GROUP, "clustertrustbundles", None);
        all.into_iter().filter(|b| util::s(b, "/spec/signerName") == signer && selector.matches(b)).collect()
    };
    json!({"bundles": bundles.iter().map(|b| util::s(b, "/spec/trustBundle")).collect::<Vec<_>>()})
}

/// Label on the PodCertificateRequests the node agent makes: which
/// projection (`<volume>-<source index>`) of the pod it is for.
pub const PCR_SOURCE_LABEL: &str = "litek8s.dev/pod-certificate-source";

/// Create the PodCertificateRequest for a pod of `node`, filling in the
/// pod, service account and node identities from the store (an agent can
/// only ask for its own pods), owned by the pod. Earlier finished requests
/// of the same projection are removed.
fn create_pcr(api: &ApiState, node: &str, ns: &str, request: &str) -> Result<Value, String> {
    let req: Value = serde_json::from_str(request).map_err(|e| format!("bad request: {e}"))?;
    let pod_name = util::s(&req, "/podName");
    let pod = api.get_object("", "pods", ns, pod_name).ok_or_else(|| format!("pod {ns}/{pod_name} not found"))?;
    if util::s(&pod, "/spec/nodeName") != node {
        return Err(format!("pod {ns}/{pod_name} is not on node {node}"));
    }
    let sa_name = match util::s(&pod, "/spec/serviceAccountName") {
        "" => "default",
        sa => sa,
    };
    let sa = api.get_object("", "serviceaccounts", ns, sa_name).ok_or_else(|| format!("serviceaccount {ns}/{sa_name} not found"))?;
    let node_obj = api.get_object("", "nodes", "", node).ok_or_else(|| format!("node {node} not found"))?;
    let pod_uid = util::uid(&pod);
    let source = util::s(&req, "/source");
    let (existing, _) = api.store().list(certificates::GROUP, "podcertificaterequests", Some(ns));
    for old in existing.iter().filter(|p| {
        util::s(p, "/spec/podUID") == pod_uid && util::s(p, &format!("/metadata/labels/{}", PCR_SOURCE_LABEL.replace('/', "~1"))) == source
    }) {
        if certificates::pcr_outcome(old) != Ok(None) {
            let _ = api.delete_named(certificates::GROUP, "podcertificaterequests", ns, &util::s(old, "/metadata/name").to_string());
        }
    }
    let mut spec = json!({
        "signerName": req["signerName"],
        "podName": pod_name,
        "podUID": pod_uid,
        "serviceAccountName": sa_name,
        "serviceAccountUID": util::uid(&sa),
        "nodeName": node,
        "nodeUID": util::uid(&node_obj),
        "stubPKCS10Request": req["stubPKCS10Request"],
    });
    for field in ["maxExpirationSeconds", "unverifiedUserAnnotations"] {
        if let Some(v) = req.get(field).filter(|v| !v.is_null()) {
            spec[field] = v.clone();
        }
    }
    let pcr = json!({
        "apiVersion": "certificates.k8s.io/v1",
        "kind": "PodCertificateRequest",
        "metadata": {
            "generateName": format!("{pod_name}-"),
            "namespace": ns,
            "labels": {PCR_SOURCE_LABEL: source},
            "ownerReferences": [{"apiVersion": "v1", "kind": "Pod", "name": pod_name, "uid": pod_uid, "controller": false, "blockOwnerDeletion": false}],
        },
        "spec": spec,
    });
    api.create_object(certificates::GROUP, "podcertificaterequests", Some(ns), pcr).map_err(|e| e.message)
}

/// (name, image) of containers whose running instance is new in `status`.
fn newly_started(pod: &Value, status: &Value) -> Vec<(String, String)> {
    let started = |st: &Value, name: &str| -> Option<String> {
        ["containerStatuses", "initContainerStatuses", "ephemeralContainerStatuses"]
            .iter()
            .flat_map(|f| st.get(*f).and_then(Value::as_array).cloned().unwrap_or_default())
            .find(|c| util::s(c, "/name") == name)
            .and_then(|c| c.pointer("/state/running/startedAt").and_then(Value::as_str).map(str::to_string))
    };
    let old = pod.get("status").cloned().unwrap_or_default();
    ["containerStatuses", "initContainerStatuses", "ephemeralContainerStatuses"]
        .iter()
        .flat_map(|f| status.get(*f).and_then(Value::as_array).cloned().unwrap_or_default())
        .filter_map(|c| {
            let name = util::s(&c, "/name").to_string();
            let now = started(status, &name)?;
            (started(&old, &name).as_deref() != Some(now.as_str())).then(|| (name, util::s(&c, "/image").to_string()))
        })
        .collect()
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
        let started = newly_started(&pod, &status);
        match util::write_status(api, "", "pods", &pod, status.clone()) {
            Ok(()) => {
                // The kubelet's events for containers that just started.
                for (name, image) in started {
                    util::record_event(api, &pod, "Normal", "Pulled", &format!("Container image \"{image}\" already present on machine"), "kubelet");
                    util::record_event(api, &pod, "Normal", "Created", &format!("Created container: {name}"), "kubelet");
                    util::record_event(api, &pod, "Normal", "Started", &format!("Started container {name}"), "kubelet");
                }
                return;
            }
            Err(e) if e.is_conflict() => continue,
            Err(e) if !e.is_not_found() => {
                eprintln!("apiserver: pod status {ns}/{name}: {e}");
                return;
            }
            _ => return,
        }
    }
}

/// Whether a deleted PersistentVolume's data on `node` goes with it: the
/// volume lives on that node and its reclaim policy is `Delete`. `Retain`
/// (or no policy) keeps the data, as Kubernetes does, so a volume can be
/// re-created, here or on another node, over the same files.
fn deletes_volume_data(pv: &Value, node: &str) -> bool {
    let on_node = pv
        .pointer("/spec/nodeAffinity/required/nodeSelectorTerms")
        .is_some_and(|t| t.to_string().contains(&format!("\"{node}\"")));
    on_node && util::s(pv, "/spec/persistentVolumeReclaimPolicy") == "Delete"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_delete_policy_removes_a_volumes_data() {
        let pv = |node: &str, policy: Option<&str>| {
            let mut v = json!({"spec": {"nodeAffinity": {"required": {"nodeSelectorTerms": [
                {"matchExpressions": [{"key": "kubernetes.io/hostname", "operator": "In", "values": [node]}]}
            ]}}}});
            if let Some(p) = policy {
                v["spec"]["persistentVolumeReclaimPolicy"] = json!(p);
            }
            v
        };
        assert!(deletes_volume_data(&pv("n1", Some("Delete")), "n1"));
        assert!(!deletes_volume_data(&pv("n1", Some("Retain")), "n1"));
        assert!(!deletes_volume_data(&pv("n1", None), "n1"));
        assert!(!deletes_volume_data(&pv("n2", Some("Delete")), "n1"));
        // A node whose name is a prefix of another's is not that node.
        assert!(!deletes_volume_data(&pv("n10", Some("Delete")), "n1"));
    }
    use crate::kube::store::ObjectKey;

    fn pod(name: &str, node: &str, app: &str, ip: &str) -> Value {
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": name, "namespace": "shop", "labels": {"app": app}},
            "spec": {"nodeName": node, "containers": [{"name": "c", "image": "busybox"}]},
            "status": {"phase": "Running", "podIP": ip},
        })
    }

    /// Create a pod with its status (create drops it).
    fn put_pod(api: &ApiState, p: Value) -> Value {
        let created = api.create_object("", "pods", Some("shop"), p.clone()).unwrap();
        util::write_status(api, "", "pods", &created, p["status"].clone()).unwrap();
        api.get_object("", "pods", "shop", &util::name(&p)).unwrap()
    }

    fn set_ip(api: &ApiState, name: &str, ip: &str) {
        let cur = api.get_object("", "pods", "shop", name).unwrap();
        let mut status = cur["status"].clone();
        status["podIP"] = json!(ip);
        util::write_status(api, "", "pods", &cur, status).unwrap();
    }

    /// Control messages from the server until `stop` matches one, skipping
    /// heartbeats and services.
    async fn until<R: AsyncRead + Unpin>(r: &mut R, mut stop: impl FnMut(&Message) -> bool) -> Vec<Message> {
        let mut out = Vec::new();
        loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), read_frame(r)).await.expect("timed out").unwrap().unwrap();
            let Frame::Control(msg) = frame else { continue };
            if matches!(msg, Message::ServiceUpdate { .. } | Message::HeartbeatAck) {
                continue;
            }
            let done = stop(&msg);
            out.push(msg);
            if done {
                return out;
            }
        }
    }

    fn rules_of(msg: &Message) -> Option<(&std::collections::BTreeMap<String, Option<PodRules>>, bool)> {
        match msg {
            Message::NetPolicy { pods, full } => Some((pods, *full)),
            _ => None,
        }
    }

    #[tokio::test]
    async fn policy_rules_are_pushed_before_pods_and_follow_peers() {
        let api = Arc::new(ApiState::open(None, "127.0.0.1:0".into(), false).unwrap());
        api.create_object("", "namespaces", None, json!({"metadata": {"name": "shop"}})).unwrap();
        api.create_object(
            "networking.k8s.io",
            "networkpolicies",
            Some("shop"),
            json!({"metadata": {"name": "db"}, "spec": {"podSelector": {"matchLabels": {"app": "db"}}, "ingress": [{"from": [{"podSelector": {"matchLabels": {"app": "api"}}}], "ports": [{"port": 5432}]}]}}),
        )
        .unwrap();
        let db = put_pod(&api, pod("db", "n1", "db", "10.42.0.5"));
        let client = put_pod(&api, pod("api", "n2", "api", "10.42.1.7"));

        let (agent_end, server_end) = tokio::io::duplex(1 << 20);
        let server = tokio::spawn(AgentHub::serve_agent(api.clone(), server_end, "10.0.0.1".into(), 6443, None));
        let (mut r, mut w) = tokio::io::split(agent_end);
        let hello = Message::Hello { node: json!({"metadata": {"name": "n1"}, "status": {}}), features: vec![netpol::FEATURE.into()] };
        write_frame(&mut w, &Frame::Control(hello)).await.unwrap();

        // Welcome says so, and the full set comes before the node's pods.
        let msgs = until(&mut r, |m| matches!(m, Message::SyncDone)).await;
        assert!(matches!(msgs[0], Message::Welcome { network_policy: true, .. }));
        let full = msgs.iter().position(|m| rules_of(m).is_some_and(|(_, f)| f)).expect("full set");
        let upsert = msgs.iter().position(|m| matches!(m, Message::PodUpsert { .. })).unwrap();
        assert!(full < upsert);
        let (pods, _) = rules_of(&msgs[full]).unwrap();
        let r_db = pods[&util::uid(&db)].clone().unwrap();
        assert_eq!(r_db.ingress.as_ref().unwrap()[0].peers.as_deref().unwrap(), ["10.42.1.7/32"]);
        assert!(!pods.contains_key(&util::uid(&client)));

        // A new selected pod: its rules, then the pod.
        let db2 = api.create_object("", "pods", Some("shop"), pod("db2", "n1", "db", "")).unwrap();
        let msgs = until(&mut r, |m| matches!(m, Message::PodUpsert { .. })).await;
        let (pods, full) = rules_of(&msgs[msgs.len() - 2]).expect("rules first");
        assert!(!full && pods[&util::uid(&db2)].is_some());

        // A peer's address changes: both pods' rules follow.
        set_ip(&api, "api", "10.42.1.9");
        let msgs = until(&mut r, |m| rules_of(m).is_some()).await;
        let (pods, _) = rules_of(msgs.last().unwrap()).unwrap();
        assert_eq!(pods.len(), 2);
        for r in pods.values() {
            assert_eq!(r.as_ref().unwrap().ingress.as_ref().unwrap()[0].peers.as_deref().unwrap(), ["10.42.1.9/32"]);
        }

        // The policy goes: nothing is isolated any more.
        api.store().delete(&ObjectKey::new("networking.k8s.io", "networkpolicies", "shop", "db"), None).unwrap();
        let msgs = until(&mut r, |m| rules_of(m).is_some()).await;
        let (pods, _) = rules_of(msgs.last().unwrap()).unwrap();
        assert_eq!(pods.len(), 2);
        assert!(pods.values().all(Option::is_none));
        server.abort();
    }

    #[tokio::test]
    async fn agents_without_the_feature_get_no_rules() {
        let api = Arc::new(ApiState::open(None, "127.0.0.1:0".into(), false).unwrap());
        let (agent_end, server_end) = tokio::io::duplex(1 << 20);
        let server = tokio::spawn(AgentHub::serve_agent(api.clone(), server_end, "10.0.0.1".into(), 6443, None));
        let (mut r, mut w) = tokio::io::split(agent_end);
        write_frame(&mut w, &Frame::Control(Message::Hello { node: json!({"metadata": {"name": "n1"}, "status": {}}), features: vec![] })).await.unwrap();
        let msgs = until(&mut r, |m| matches!(m, Message::SyncDone)).await;
        assert!(matches!(msgs[0], Message::Welcome { network_policy: false, .. }));
        assert!(!msgs.iter().any(|m| rules_of(m).is_some()));
        server.abort();
    }
}
