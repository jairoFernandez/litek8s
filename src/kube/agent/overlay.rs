//! The pod overlay between nodes: kernel WireGuard (see `crate::vpn`).
//!
//! Every node has one `litek8s-wg` interface holding the network address of
//! its pod CIDR (10.42.N.0/32) and a route for the whole cluster CIDR. Each
//! other node is a peer whose allowed IPs are that node's pod CIDR, so
//! WireGuard's cryptokey routing picks the node a pod address lives on.
//! Pod-to-pod traffic is not masqueraded: pods see each other's real
//! addresses, Services DNAT straight to remote pods, and replies come back
//! the same way. The server pushes the peer list (from the Node objects'
//! WireGuard annotations) on connect and whenever a node changes; the last
//! list is kept on disk so a restarted agent restores the mesh before it
//! reaches the server again.
//!
//! A peer the server says to reach through a relay (`NodePeer::via`, see
//! [`super::relay`]) keeps its entry with no allowed IPs, and its pod CIDR
//! goes to the relay's entry instead. Every entry keeps a keepalive, so the
//! direct path is tried all the time; [`Overlay::tick`] sends each directly
//! routed peer a probe, watches what comes back from each peer, tries the
//! other endpoints known for peers that stay silent, and returns what the
//! agent reports to the server. Peers are
//! changed in place, so a route change keeps the other sessions and the
//! endpoints WireGuard learned from peers behind NAT.

use std::collections::{BTreeMap, HashMap};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::relay::{PeerReport, Reach, Reports};
use crate::vpn::wireguard::{self, Peer, PeerUpdate};
use crate::vpn::{KeyPair, PublicKey};

pub(crate) const IFACE: &str = "litek8s-wg";
/// WireGuard adds 60 bytes over IPv4 (80 over IPv6); pods use the same MTU
/// so cross-node TCP never needs fragmentation.
pub(crate) const MTU: u32 = 1420;
pub const ANNOTATION_PUBLIC_KEY: &str = "litek8s.dev/wireguard-public-key";
pub const ANNOTATION_ENDPOINT: &str = "litek8s.dev/wireguard-endpoint";
/// Keepalive towards peers, so NAT mappings of nodes behind a home router
/// stay open and a peer that roamed (a laptop on another network) is found.
/// (Only one side of a pair ends up sending them: any packet received also
/// postpones the keepalive. Probes make the traffic two-way, see `tick`.)
const KEEPALIVE_SECS: u16 = 25;
/// How often [`Overlay::tick`] runs, and sends each directly routed peer a
/// probe (WireGuard answers a data packet with a keepalive within 10 s).
pub(crate) const TICK: Duration = Duration::from_secs(10);
/// A peer is direct when something arrived from it this recently (a few
/// probe rounds) over a session no older than WireGuard keeps one.
const RX_WINDOW: Duration = Duration::from_secs(40);
/// Probes go to this port of the peer's overlay address (discard: nothing
/// listens, the packet only has to get there).
const PROBE_PORT: u16 = 9;
const HANDSHAKE_FRESH_SECS: i64 = 180;
/// A silent peer's next endpoint is tried after this long: a few handshake
/// attempts, and less than the 30 s NATs keep an unanswered UDP mapping, so
/// two nodes trying each other's public address overlap.
const ROTATE_AFTER: Duration = Duration::from_secs(20);
/// The filter chain that lets relays forward between peers.
const CHAIN: &str = "LITEK8S-OVERLAY";

/// Another node of the mesh, as the server describes it to one node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodePeer {
    pub name: String,
    pub public_key: String,
    /// host:port
    pub endpoint: String,
    pub pod_cidr: String,
    /// More endpoints to try (where other nodes see this one, behind NAT).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<String>,
    /// Reach this node through that one (no direct path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// An API server's node: the address the agent dials the server at is
    /// also tried (a cloud server's own address is often private).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub server: bool,
}

/// What the overlay knows of one peer, for metrics.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PeerSnapshot {
    pub name: String,
    pub reach: Reach,
    pub via: Option<String>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    /// Seconds since the last handshake (`None`: never).
    pub handshake_age: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Snapshot {
    pub peers: Vec<PeerSnapshot>,
    /// Forwarded between peers (this node relays).
    pub relayed_bytes: Option<u64>,
}

/// What the kernel has of a peer, as last set.
#[derive(Debug, Clone, PartialEq)]
struct Applied {
    allowed: Vec<(Ipv4Addr, u8)>,
    /// The endpoint last given (the kernel may have roamed since).
    endpoint: Option<SocketAddrV4>,
}

/// Watching one peer.
#[derive(Debug, Clone)]
struct Probe {
    since: Instant,
    rx: u64,
    rx_at: Option<Instant>,
    /// Index into the peer's candidate endpoints.
    candidate: usize,
    switched: Instant,
    reach: Reach,
    /// Routed through a relay (its direct entry has no allowed IPs).
    relayed: bool,
}

impl Probe {
    fn new(now: Instant) -> Self {
        Probe { since: now, rx: 0, rx_at: None, candidate: 0, switched: now, reach: Reach::Unknown, relayed: false }
    }
}

#[derive(Default)]
struct State {
    /// The other nodes, as last pushed.
    peers: Vec<NodePeer>,
    /// Forwarding rules as loaded (`None`: not yet).
    relay: Option<bool>,
    applied: BTreeMap<String, Applied>,
    probes: HashMap<String, Probe>,
    server_host: Option<Ipv4Addr>,
    /// Relayed bytes: the rule's counter as last read, and the sum over
    /// reloads of the chain (which reset it).
    counter: u64,
    relayed: u64,
}

pub struct Overlay {
    key: KeyPair,
    port: u16,
    pod_cidr: String,
    cluster_cidr: String,
    peers_file: PathBuf,
    state: tokio::sync::Mutex<State>,
    snapshot: std::sync::Mutex<Snapshot>,
}

fn parse_cidr(cidr: &str) -> Option<(Ipv4Addr, u8)> {
    let (addr, prefix) = cidr.split_once('/')?;
    let prefix: u8 = prefix.parse().ok().filter(|p| *p <= 32)?;
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    Some((Ipv4Addr::from(u32::from(addr.parse::<Ipv4Addr>().ok()?) & mask), prefix))
}

/// Endpoints to try for a peer, in order: the one it announces, those other
/// nodes see it at, and for a server the address this agent dials it at.
fn candidates(peer: &NodePeer, server_host: Option<Ipv4Addr>) -> Vec<SocketAddrV4> {
    let mut out: Vec<SocketAddrV4> = Vec::new();
    let announced = peer.endpoint.parse::<SocketAddrV4>().ok();
    let mut add = |ep: Option<SocketAddrV4>| {
        if let Some(ep) = ep.filter(|e| !out.contains(e)) {
            out.push(ep);
        }
    };
    add(announced);
    for e in &peer.endpoints {
        add(e.parse().ok());
    }
    if peer.server {
        if let (Some(host), Some(a)) = (server_host, announced) {
            add(Some(SocketAddrV4::new(host, a.port())));
        }
    }
    out
}

/// The WireGuard peers for a node list, by public key: each with its pod
/// CIDR, or with none when reached through a relay present in the list,
/// whose entry then carries it. Malformed entries are skipped.
pub(crate) fn to_peers(peers: &[NodePeer], endpoint: impl Fn(&NodePeer) -> Option<SocketAddrV4>) -> BTreeMap<String, Peer> {
    let valid: Vec<(&NodePeer, PublicKey, (Ipv4Addr, u8))> =
        peers.iter().filter_map(|p| Some((p, PublicKey::parse(&p.public_key).ok()?, parse_cidr(&p.pod_cidr)?))).collect();
    let relayed = |p: &NodePeer| p.via.as_ref().is_some_and(|v| valid.iter().any(|(r, _, _)| r.name == *v && r.via.is_none()));
    valid
        .iter()
        .map(|(p, key, cidr)| {
            let mut allowed: Vec<(Ipv4Addr, u8)> = if relayed(p) { Vec::new() } else { vec![*cidr] };
            for (q, _, c) in &valid {
                if relayed(q) && q.via.as_deref() == Some(p.name.as_str()) {
                    allowed.push(*c);
                }
            }
            allowed.sort();
            (key.as_str().to_string(), Peer { public_key: key.clone(), endpoint: endpoint(p), allowed_ips: allowed, keepalive: KEEPALIVE_SECS })
        })
        .collect()
}

/// What to tell the kernel to go from `applied` to `wanted`. A peer's
/// endpoint is set when it is new, or when another one is to be tried for
/// a peer that is not getting through (a working one may have roamed).
fn diff(applied: &BTreeMap<String, Applied>, wanted: &BTreeMap<String, Peer>, direct: impl Fn(&str) -> bool) -> Vec<PeerUpdate> {
    let mut out = Vec::new();
    for (key, peer) in wanted {
        match applied.get(key) {
            None => out.push(PeerUpdate { peer: peer.clone(), remove: false }),
            Some(a) => {
                let new_endpoint = peer.endpoint.is_some() && a.endpoint != peer.endpoint && !direct(key);
                if a.allowed != peer.allowed_ips || new_endpoint {
                    let mut p = peer.clone();
                    if !new_endpoint {
                        p.endpoint = None;
                    }
                    out.push(PeerUpdate { peer: p, remove: false });
                }
            }
        }
    }
    for key in applied.keys().filter(|k| !wanted.contains_key(*k)) {
        if let Ok(public_key) = PublicKey::parse(key) {
            out.push(PeerUpdate { peer: Peer { public_key, endpoint: None, allowed_ips: Vec::new(), keepalive: 0 }, remove: true });
        }
    }
    out
}

/// Whether a peer gets through. Routed directly (probes go to it): something
/// arrived recently over a fresh session. Relayed (only keepalives go over
/// the direct entry, one way): a handshake completed since it was relayed.
/// Unreachable once watched a whole window (`since`: the last route change)
/// without that.
fn classify(now: Instant, since: Instant, rx_at: Option<Instant>, handshake_age: Option<i64>, relayed: bool) -> Reach {
    let fresh = handshake_age.is_some_and(|a| a < HANDSHAKE_FRESH_SECS);
    let ok = if relayed {
        fresh && handshake_age.is_some_and(|a| (a as u64) < now.duration_since(since).as_secs())
    } else {
        fresh && rx_at.is_some_and(|t| now.duration_since(t) < RX_WINDOW)
    };
    if ok {
        Reach::Direct
    } else if now.duration_since(if relayed { since } else { rx_at.unwrap_or(since).max(since) }) >= RX_WINDOW {
        Reach::Unreachable
    } else {
        Reach::Unknown
    }
}

/// The relay chain: forwarding between peers only on relays, and only
/// between pod addresses (its counter is what was relayed).
fn relay_rules(cluster_cidr: &str, relay: bool) -> String {
    let mut out = format!("*filter\n:{CHAIN} - [0:0]\n");
    if relay {
        out.push_str(&format!("-A {CHAIN} -i {IFACE} -o {IFACE} -s {cluster_cidr} -d {cluster_cidr} -j ACCEPT\n"));
    }
    out.push_str(&format!("-A {CHAIN} -i {IFACE} -o {IFACE} -j DROP\nCOMMIT\n"));
    out
}

/// Bytes of the chain's ACCEPT rule in `iptables -nvxL` output.
fn accepted_bytes(listing: &str) -> Option<u64> {
    listing.lines().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() > 2 && f[2] == "ACCEPT").then(|| f[1].parse().ok()).flatten()
    })
}

async fn sh(script: &str) -> Result<()> {
    let out = tokio::process::Command::new("sh").arg("-c").arg(script).output().await.context("failed to run sh")?;
    if !out.status.success() {
        anyhow::bail!("{script}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

async fn load_relay_rules(cluster_cidr: &str, relay: bool) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("iptables-restore")
        .arg("--noflush")
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to run iptables-restore")?;
    child.stdin.take().context("no stdin")?.write_all(relay_rules(cluster_cidr, relay).as_bytes()).await?;
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        anyhow::bail!("iptables-restore: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    sh(&format!("iptables -t filter -C FORWARD -j {CHAIN} 2>/dev/null || iptables -t filter -I FORWARD 1 -j {CHAIN}")).await
}

async fn relay_counter() -> Option<u64> {
    let out = tokio::process::Command::new("iptables").args(["-t", "filter", "-nvxL", CHAIN]).output().await.ok()?;
    accepted_bytes(&String::from_utf8_lossy(&out.stdout))
}

/// One datagram to each peer's overlay address, over its direct entry.
fn send_probes(targets: &[SocketAddrV4]) {
    let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") else { return };
    let _ = socket.set_nonblocking(true);
    for t in targets {
        let _ = socket.send_to(b"litek8s overlay probe", t);
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Overlay {
    /// Bring up the interface with the last known peers.
    pub async fn start(node_dir: &Path, port: u16, pod_cidr: &str, cluster_cidr: &str) -> Result<Self> {
        let key = crate::vpn::load_or_generate_keypair_at(&node_dir.join("wireguard.key"))?;
        let (net, _) = parse_cidr(pod_cidr).with_context(|| format!("invalid pod CIDR {pod_cidr}"))?;
        let (cluster, prefix) = parse_cidr(cluster_cidr).with_context(|| format!("invalid cluster CIDR {cluster_cidr}"))?;
        // Replies from sockets bound to the wildcard address (cluster DNS
        // on UDP) leave from the route's preferred source: the pod gateway,
        // the address remote pods sent to, not the interface's .0.
        let gateway = Ipv4Addr::from(u32::from(net) + 1);
        sh(&format!(
            "ip link show {IFACE} >/dev/null 2>&1 || ip link add {IFACE} type wireguard; \
             ip addr replace {net}/32 dev {IFACE}; ip link set {IFACE} mtu {MTU} up; \
             ip route replace {cluster}/{prefix} dev {IFACE} src {gateway} 2>/dev/null || ip route replace {cluster}/{prefix} dev {IFACE}"
        ))
        .await
        .context("failed to set up the WireGuard interface (is the wireguard kernel module available?)")?;
        let overlay = Self {
            key,
            port,
            pod_cidr: pod_cidr.to_string(),
            cluster_cidr: format!("{cluster}/{prefix}"),
            peers_file: node_dir.join("wireguard-peers.json"),
            state: tokio::sync::Mutex::default(),
            snapshot: std::sync::Mutex::default(),
        };
        let saved: Vec<NodePeer> = std::fs::read(&overlay.peers_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        // The whole list once (peers of an earlier run go), then in place.
        let wanted = to_peers(&saved, |p| candidates(p, None).first().copied());
        let (k, list) = (overlay.key.clone(), wanted.values().cloned().collect::<Vec<_>>());
        tokio::task::spawn_blocking(move || wireguard::configure(IFACE, &k, port, &list)).await??;
        {
            let mut state = overlay.state.lock().await;
            state.applied = wanted.into_iter().map(|(k, p)| (k, Applied { allowed: p.allowed_ips, endpoint: p.endpoint })).collect();
            state.peers = saved.clone();
        }
        overlay.set_relay(false).await;
        eprintln!("node-agent: WireGuard overlay up on {IFACE} (:{port}, pods {pod_cidr} of {cluster_cidr}, {} known peers)", saved.len());
        Ok(overlay)
    }

    pub fn public_key(&self) -> String {
        self.key.public().into_inner()
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn pod_cidr(&self) -> &str {
        &self.pod_cidr
    }

    pub fn cluster_cidr(&self) -> &str {
        &self.cluster_cidr
    }

    async fn set_relay(&self, relay: bool) {
        let mut state = self.state.lock().await;
        if state.relay == Some(relay) {
            return;
        }
        match load_relay_rules(&self.cluster_cidr, relay).await {
            Ok(()) => {
                state.relay = Some(relay);
                state.counter = 0;
                if relay {
                    eprintln!("node-agent: relaying overlay traffic between nodes with no direct path");
                }
            }
            Err(e) => eprintln!("node-agent: overlay forwarding rules not loaded: {e:#}"),
        }
    }

    /// The address this agent reaches its API server at (tried for server
    /// nodes whose own address is not reachable from here).
    pub async fn set_server_host(&self, host: &str) {
        let ip = match host.parse::<Ipv4Addr>() {
            Ok(ip) => Some(ip),
            Err(_) => tokio::net::lookup_host((host, 0)).await.ok().and_then(|mut a| {
                a.find_map(|a| match a.ip() {
                    std::net::IpAddr::V4(v4) => Some(v4),
                    _ => None,
                })
            }),
        };
        self.state.lock().await.server_host = ip;
    }

    /// Make the device's peers the other nodes of `peers`, routed as the
    /// server says; `relay`: this node forwards for others.
    pub async fn apply(&self, peers: &[NodePeer], relay: bool) -> Result<()> {
        self.set_relay(relay).await;
        let mine = self.public_key();
        let others: Vec<NodePeer> = peers.iter().filter(|p| p.public_key != mine).cloned().collect();
        let mut state = self.state.lock().await;
        if state.peers != others {
            let _ = std::fs::write(&self.peers_file, serde_json::to_vec(&others)?);
            state.peers = others;
        }
        self.sync(&mut state).await
    }

    async fn sync(&self, state: &mut State) -> Result<()> {
        let server_host = state.server_host;
        let wanted = to_peers(&state.peers, |p| {
            let c = candidates(p, server_host);
            let i = PublicKey::parse(&p.public_key).ok().and_then(|k| state.probes.get(k.as_str()).map(|pr| pr.candidate)).unwrap_or(0);
            c.get(i % c.len().max(1)).copied()
        });
        let updates = diff(&state.applied, &wanted, |k| state.probes.get(k).is_some_and(|p| p.reach == Reach::Direct));
        if updates.is_empty() {
            return Ok(());
        }
        let list = updates.clone();
        tokio::task::spawn_blocking(move || wireguard::update(IFACE, &list)).await??;
        for u in updates {
            let key = u.peer.public_key.as_str().to_string();
            if u.remove {
                state.applied.remove(&key);
                state.probes.remove(&key);
                continue;
            }
            let endpoint = u.peer.endpoint.or_else(|| state.applied.get(&key).and_then(|a| a.endpoint));
            state.applied.insert(key, Applied { allowed: u.peer.allowed_ips, endpoint });
        }
        Ok(())
    }

    /// Watch the peers once: what the kernel saw from each, the next
    /// endpoint for those that stay silent. Returns the report for the
    /// server (peer name → state).
    pub async fn tick(&self) -> Result<Reports> {
        let kernel = tokio::task::spawn_blocking(|| wireguard::peers(IFACE)).await??;
        let now = Instant::now();
        let unix = now_unix();
        let mut state = self.state.lock().await;
        let server_host = state.server_host;
        let peers = state.peers.clone();
        let mut reports = Reports::new();
        let mut snapshot = Snapshot::default();
        let routed = to_peers(&peers, |_| None);
        let mut targets = Vec::new();
        for p in &peers {
            let Ok(key) = PublicKey::parse(&p.public_key) else { continue };
            let Some(entry) = routed.get(key.as_str()) else { continue };
            let relayed = entry.allowed_ips.is_empty();
            if !relayed {
                if let Some((net, _)) = parse_cidr(&p.pod_cidr) {
                    targets.push(SocketAddrV4::new(net, PROBE_PORT));
                }
            }
            let status = kernel.iter().find(|s| s.public_key == key.as_str()).cloned();
            let probe = state.probes.entry(key.as_str().to_string()).or_insert_with(|| Probe::new(now));
            if probe.relayed != relayed {
                // A new route: watch it a whole window before judging.
                probe.relayed = relayed;
                probe.since = now;
                probe.rx_at = None;
            }
            if let Some(s) = &status {
                if s.rx_bytes > probe.rx {
                    probe.rx_at = Some(now);
                }
                // A peer re-added by the kernel starts its counters over.
                probe.rx = s.rx_bytes;
            }
            let handshake_age = status.as_ref().filter(|s| s.last_handshake > 0).map(|s| (unix - s.last_handshake).max(0));
            probe.reach = classify(now, probe.since, probe.rx_at, handshake_age, relayed);
            if probe.reach != Reach::Direct && now.duration_since(probe.switched) >= ROTATE_AFTER && candidates(p, server_host).len() > 1 {
                probe.candidate += 1;
                probe.switched = now;
            }
            let endpoint = status.as_ref().and_then(|s| s.endpoint).filter(|_| probe.reach == Reach::Direct).map(|e| e.to_string());
            reports.insert(p.name.clone(), PeerReport { state: probe.reach, endpoint });
            snapshot.peers.push(PeerSnapshot {
                name: p.name.clone(),
                reach: probe.reach,
                via: p.via.clone(),
                rx_bytes: status.as_ref().map(|s| s.rx_bytes).unwrap_or(0),
                tx_bytes: status.as_ref().map(|s| s.tx_bytes).unwrap_or(0),
                handshake_age,
            });
        }
        self.sync(&mut state).await?;
        send_probes(&targets);
        if state.relay == Some(true) {
            if let Some(c) = relay_counter().await {
                // The chain was reloaded: its counter started over.
                let delta = if c >= state.counter { c - state.counter } else { c };
                state.relayed += delta;
                state.counter = c;
            }
            snapshot.relayed_bytes = Some(state.relayed);
        }
        *self.snapshot.lock().unwrap() = snapshot;
        Ok(reports)
    }

    /// Overlay series for the node's metrics (`crate::obs::node`).
    pub(crate) fn metrics(&self, node: &str) -> Vec<(crate::obs::tsdb::Labels, f64)> {
        metric_samples(node, &self.snapshot.lock().unwrap())
    }

    /// Peers with their last handshake, for diagnostics.
    pub async fn status() -> Result<Vec<wireguard::PeerStatus>> {
        tokio::task::spawn_blocking(|| wireguard::peers(IFACE)).await?
    }
}

fn metric_samples(node: &str, s: &Snapshot) -> Vec<(crate::obs::tsdb::Labels, f64)> {
    let labels = |name: &str, extra: &[(&str, &str)]| -> crate::obs::tsdb::Labels {
        let mut l: crate::obs::tsdb::Labels = [("__name__", name), ("job", "litek8s"), ("instance", node), ("node", node)].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        l.extend(extra.iter().map(|(a, b)| (a.to_string(), b.to_string())));
        l
    };
    let mut out = Vec::new();
    for p in &s.peers {
        let peer = [("peer", p.name.as_str())];
        out.push((labels("litek8s_overlay_peer_receive_bytes_total", &peer), p.rx_bytes as f64));
        out.push((labels("litek8s_overlay_peer_transmit_bytes_total", &peer), p.tx_bytes as f64));
        if let Some(age) = p.handshake_age {
            out.push((labels("litek8s_overlay_peer_last_handshake_age_seconds", &peer), age as f64));
        }
        if p.reach != Reach::Unknown {
            out.push((labels("litek8s_overlay_peer_direct", &peer), if p.reach == Reach::Direct { 1.0 } else { 0.0 }));
        }
        let via = p.via.as_deref().unwrap_or("");
        out.push((labels("litek8s_overlay_peer_relayed", &[("peer", p.name.as_str()), ("via", via)]), if p.via.is_some() { 1.0 } else { 0.0 }));
    }
    if let Some(bytes) = s.relayed_bytes {
        out.push((labels("litek8s_overlay_relayed_bytes_total", &[]), bytes as f64));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(name: &str, key: &str, cidr: &str, via: Option<&str>) -> NodePeer {
        NodePeer {
            name: name.into(),
            public_key: key.into(),
            endpoint: format!("192.168.77.{}:51820", cidr.split('.').nth(2).unwrap()),
            pod_cidr: cidr.into(),
            endpoints: Vec::new(),
            via: via.map(str::to_string),
            server: false,
        }
    }

    fn key() -> String {
        KeyPair::generate().public().into_inner()
    }

    fn first(p: &NodePeer) -> Option<SocketAddrV4> {
        candidates(p, None).first().copied()
    }

    #[test]
    fn peers_from_nodes() {
        let k = key();
        let nodes = vec![peer("b", &k, "10.42.1.0/24", None), NodePeer { public_key: "nope".into(), ..peer("bad", &k, "10.42.2.0/24", None) }];
        let peers = to_peers(&nodes, first);
        assert_eq!(peers.len(), 1);
        let p = &peers[&k];
        assert_eq!(p.allowed_ips, vec![(Ipv4Addr::new(10, 42, 1, 0), 24)]);
        assert_eq!(p.endpoint, Some("192.168.77.1:51820".parse().unwrap()));
        assert_eq!(p.keepalive, KEEPALIVE_SECS);
    }

    #[test]
    fn a_relayed_peer_gives_its_cidr_to_the_relay() {
        let (s, b, c) = (key(), key(), key());
        let nodes = vec![peer("s", &s, "10.42.0.0/24", None), peer("b", &b, "10.42.1.0/24", Some("s")), peer("c", &c, "10.42.2.0/24", None)];
        let peers = to_peers(&nodes, first);
        assert_eq!(peers[&s].allowed_ips, vec![(Ipv4Addr::new(10, 42, 0, 0), 24), (Ipv4Addr::new(10, 42, 1, 0), 24)]);
        assert!(peers[&b].allowed_ips.is_empty(), "the direct entry stays, for probing");
        assert_eq!(peers[&b].keepalive, KEEPALIVE_SECS);
        assert_eq!(peers[&c].allowed_ips, vec![(Ipv4Addr::new(10, 42, 2, 0), 24)]);
        // A relay the node does not know (or that is itself relayed) is not used.
        let nodes = vec![peer("b", &b, "10.42.1.0/24", Some("gone"))];
        assert_eq!(to_peers(&nodes, first)[&b].allowed_ips.len(), 1);
        let nodes = vec![peer("s", &s, "10.42.0.0/24", Some("c")), peer("b", &b, "10.42.1.0/24", Some("s")), peer("c", &c, "10.42.2.0/24", None)];
        assert_eq!(to_peers(&nodes, first)[&b].allowed_ips.len(), 1);
    }

    #[test]
    fn candidates_in_order_without_duplicates() {
        let mut p = peer("s", &key(), "10.42.0.0/24", None);
        p.endpoints = vec!["203.0.113.5:40000".into(), "192.168.77.0:51820".into(), "junk".into()];
        p.server = true;
        let c = candidates(&p, Some(Ipv4Addr::new(198, 51, 100, 9)));
        let c: Vec<String> = c.iter().map(|e| e.to_string()).collect();
        assert_eq!(c, vec!["192.168.77.0:51820", "203.0.113.5:40000", "198.51.100.9:51820"]);
        p.server = false;
        assert_eq!(candidates(&p, Some(Ipv4Addr::new(198, 51, 100, 9))).len(), 2);
    }

    #[test]
    fn diff_changes_peers_in_place() {
        let (s, b, gone) = (key(), key(), key());
        let nodes = vec![peer("s", &s, "10.42.0.0/24", None), peer("b", &b, "10.42.1.0/24", None)];
        let wanted = to_peers(&nodes, first);
        let ups = diff(&BTreeMap::new(), &wanted, |_| false);
        assert_eq!(ups.len(), 2);
        assert!(ups.iter().all(|u| !u.remove && u.peer.endpoint.is_some()));
        let mut applied: BTreeMap<String, Applied> = wanted.iter().map(|(k, p)| (k.clone(), Applied { allowed: p.allowed_ips.clone(), endpoint: p.endpoint })).collect();
        applied.insert(gone.clone(), Applied { allowed: vec![], endpoint: None });
        // b now goes through s: both entries change, the endpoints stay.
        let relayed = vec![peer("s", &s, "10.42.0.0/24", None), peer("b", &b, "10.42.1.0/24", Some("s"))];
        let ups = diff(&applied, &to_peers(&relayed, first), |_| false);
        assert_eq!(ups.len(), 3);
        assert!(ups.iter().filter(|u| !u.remove).all(|u| u.peer.endpoint.is_none()));
        assert!(ups.iter().any(|u| u.remove && u.peer.public_key.as_str() == gone));
        // Another endpoint is tried for a silent peer, never for a working one.
        applied.remove(&gone);
        let other = to_peers(&nodes, |_| Some("10.0.0.1:1".parse().unwrap()));
        assert_eq!(diff(&applied, &other, |_| false).len(), 2);
        assert!(diff(&applied, &other, |_| true).is_empty());
        assert!(diff(&applied, &wanted, |_| false).is_empty());
    }

    #[test]
    fn classify_by_traffic_and_handshake() {
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        // Just configured, nothing yet: unknown, then unreachable.
        assert_eq!(classify(at(10), t0, None, None, false), Reach::Unknown);
        assert_eq!(classify(at(41), t0, None, None, false), Reach::Unreachable);
        // Traffic over a fresh session.
        assert_eq!(classify(at(70), t0, Some(at(65)), Some(5), false), Reach::Direct);
        // The session is old (no rekey went through): not direct.
        assert_eq!(classify(at(70), t0, Some(at(65)), Some(200), false), Reach::Unknown);
        // Silent for a window: unreachable, though a handshake is recent.
        assert_eq!(classify(at(110), t0, Some(at(65)), Some(100), false), Reach::Unreachable);
        assert_eq!(classify(at(100), t0, Some(at(65)), Some(100), false), Reach::Direct);
        // Relayed since t0: only a handshake after that counts, keepalives
        // arriving (one way) do not.
        assert_eq!(classify(at(50), t0, Some(at(49)), Some(60), true), Reach::Unreachable);
        assert_eq!(classify(at(20), t0, Some(at(19)), Some(60), true), Reach::Unknown);
        assert_eq!(classify(at(50), t0, None, Some(30), true), Reach::Direct);
    }

    #[test]
    fn relay_rules_and_counter() {
        let on = relay_rules("10.42.0.0/16", true);
        assert!(on.contains("-A LITEK8S-OVERLAY -i litek8s-wg -o litek8s-wg -s 10.42.0.0/16 -d 10.42.0.0/16 -j ACCEPT\n"));
        assert!(on.ends_with("-A LITEK8S-OVERLAY -i litek8s-wg -o litek8s-wg -j DROP\nCOMMIT\n"));
        assert!(!relay_rules("10.42.0.0/16", false).contains("ACCEPT"));
        let listing = "Chain LITEK8S-OVERLAY (1 references)\n    pkts      bytes target     prot opt in     out     source               destination\n     120    98765 ACCEPT     all  --  litek8s-wg litek8s-wg  10.42.0.0/16         10.42.0.0/16\n       0        0 DROP       all  --  litek8s-wg litek8s-wg  0.0.0.0/0            0.0.0.0/0\n";
        assert_eq!(accepted_bytes(listing), Some(98765));
        assert_eq!(accepted_bytes("Chain X\n pkts bytes target\n 0 0 DROP all"), None);
    }

    #[test]
    fn metrics_per_peer_and_relayed_bytes() {
        let s = Snapshot {
            peers: vec![
                PeerSnapshot { name: "a".into(), reach: Reach::Direct, via: None, rx_bytes: 10, tx_bytes: 20, handshake_age: Some(3) },
                PeerSnapshot { name: "b".into(), reach: Reach::Unreachable, via: Some("s".into()), rx_bytes: 0, tx_bytes: 0, handshake_age: None },
            ],
            relayed_bytes: Some(4096),
        };
        let m = metric_samples("n1", &s);
        let find = |name: &str, peer: Option<&str>| {
            m.iter().find(|(l, _)| l.iter().any(|(k, v)| k == "__name__" && v == name) && peer.is_none_or(|p| l.iter().any(|(k, v)| k == "peer" && v == p))).map(|(_, v)| *v)
        };
        assert_eq!(find("litek8s_overlay_peer_receive_bytes_total", Some("a")), Some(10.0));
        assert_eq!(find("litek8s_overlay_peer_direct", Some("b")), Some(0.0));
        assert_eq!(find("litek8s_overlay_peer_relayed", Some("b")), Some(1.0));
        assert_eq!(find("litek8s_overlay_peer_last_handshake_age_seconds", Some("b")), None);
        assert_eq!(find("litek8s_overlay_relayed_bytes_total", None), Some(4096.0));
    }
}
