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

use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::vpn::wireguard::{self, Peer};
use crate::vpn::{KeyPair, PublicKey};

pub(crate) const IFACE: &str = "litek8s-wg";
/// WireGuard adds 60 bytes over IPv4 (80 over IPv6); pods use the same MTU
/// so cross-node TCP never needs fragmentation.
pub(crate) const MTU: u32 = 1420;
pub const ANNOTATION_PUBLIC_KEY: &str = "litek8s.dev/wireguard-public-key";
pub const ANNOTATION_ENDPOINT: &str = "litek8s.dev/wireguard-endpoint";
/// Keepalive towards peers, so NAT mappings of nodes behind a home router
/// stay open and a peer that roamed (a laptop on another network) is found.
const KEEPALIVE_SECS: u16 = 25;

/// Another node of the mesh, as the server describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodePeer {
    pub name: String,
    pub public_key: String,
    /// host:port
    pub endpoint: String,
    pub pod_cidr: String,
}

pub struct Overlay {
    key: KeyPair,
    port: u16,
    pod_cidr: String,
    cluster_cidr: String,
    peers_file: PathBuf,
    applied: tokio::sync::Mutex<Option<Vec<Peer>>>,
}

fn parse_cidr(cidr: &str) -> Option<(Ipv4Addr, u8)> {
    let (addr, prefix) = cidr.split_once('/')?;
    let prefix: u8 = prefix.parse().ok().filter(|p| *p <= 32)?;
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    Some((Ipv4Addr::from(u32::from(addr.parse::<Ipv4Addr>().ok()?) & mask), prefix))
}

/// The WireGuard peers for a node list, skipping malformed entries.
pub(crate) fn to_peers(peers: &[NodePeer]) -> Vec<Peer> {
    let mut out: Vec<Peer> = peers
        .iter()
        .filter_map(|p| {
            Some(Peer {
                public_key: PublicKey::parse(&p.public_key).ok()?,
                endpoint: p.endpoint.parse::<SocketAddrV4>().ok(),
                allowed_ips: vec![parse_cidr(&p.pod_cidr)?],
                keepalive: KEEPALIVE_SECS,
            })
        })
        .collect();
    out.sort_by(|a, b| a.public_key.as_str().cmp(b.public_key.as_str()));
    out
}

async fn sh(script: &str) -> Result<()> {
    let out = tokio::process::Command::new("sh").arg("-c").arg(script).output().await.context("failed to run sh")?;
    if !out.status.success() {
        anyhow::bail!("{script}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
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
            cluster_cidr: cluster_cidr.to_string(),
            peers_file: node_dir.join("wireguard-peers.json"),
            applied: tokio::sync::Mutex::new(None),
        };
        let saved: Vec<NodePeer> = std::fs::read(&overlay.peers_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        overlay.apply(&saved).await?;
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

    /// Make the device's peers exactly `peers` (the other nodes).
    pub async fn apply(&self, peers: &[NodePeer]) -> Result<()> {
        let mine = self.public_key();
        let others: Vec<NodePeer> = peers.iter().filter(|p| p.public_key != mine).cloned().collect();
        let wanted = to_peers(&others);
        let mut applied = self.applied.lock().await;
        if applied.as_ref() == Some(&wanted) {
            return Ok(());
        }
        let (key, port, list) = (self.key.clone(), self.port, wanted.clone());
        tokio::task::spawn_blocking(move || wireguard::configure(IFACE, &key, port, &list)).await??;
        *applied = Some(wanted);
        let _ = std::fs::write(&self.peers_file, serde_json::to_vec(&others)?);
        Ok(())
    }

    /// Peers with their last handshake, for diagnostics.
    pub async fn status() -> Result<Vec<wireguard::PeerStatus>> {
        tokio::task::spawn_blocking(|| wireguard::peers(IFACE)).await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_from_nodes() {
        let key = KeyPair::generate().public().into_inner();
        let nodes = vec![
            NodePeer { name: "b".into(), public_key: key.clone(), endpoint: "192.168.77.12:51820".into(), pod_cidr: "10.42.1.0/24".into() },
            NodePeer { name: "bad".into(), public_key: "nope".into(), endpoint: "x".into(), pod_cidr: "10.42.2.0/24".into() },
        ];
        let peers = to_peers(&nodes);
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].allowed_ips, vec![(Ipv4Addr::new(10, 42, 1, 0), 24)]);
        assert_eq!(peers[0].endpoint, Some("192.168.77.12:51820".parse().unwrap()));
        assert_eq!(peers[0].keepalive, KEEPALIVE_SECS);
    }
}
