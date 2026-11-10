//! Round-trip time between nodes, over the overlay.
//!
//! Every node answers pings on its overlay address (the network address of
//! its pod CIDR, UDP port [`PORT`]) and pings every other node's once per
//! overlay tick from that same socket, so a ping and its pong take the path
//! pods' traffic takes: straight to the peer, or through its relay. A pong
//! that has not come back by the next round counts as lost. Only packets
//! from the cluster CIDR are answered, and a pong is as big as its ping.

use std::collections::{BTreeMap, HashMap};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::net::UdpSocket;

/// UDP port of the echo socket (echo's well-known port, on the overlay
/// address only).
pub(crate) const PORT: u16 = 7;
const MAGIC: &[u8; 4] = b"lk8e";
const PING: u8 = 1;
const PONG: u8 = 2;
const LEN: usize = MAGIC.len() + 1 + 8;

fn encode(kind: u8, seq: u64) -> [u8; LEN] {
    let mut out = [0u8; LEN];
    out[..4].copy_from_slice(MAGIC);
    out[4] = kind;
    out[5..].copy_from_slice(&seq.to_be_bytes());
    out
}

fn decode(buf: &[u8]) -> Option<(u8, u64)> {
    if buf.len() != LEN || &buf[..4] != MAGIC || !matches!(buf[4], PING | PONG) {
        return None;
    }
    Some((buf[4], u64::from_be_bytes(buf[5..].try_into().ok()?)))
}

fn in_cidr(ip: Ipv4Addr, (net, prefix): (Ipv4Addr, u8)) -> bool {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    u32::from(ip) & mask == u32::from(net) & mask
}

/// Pings in flight and what came back, per peer name.
#[derive(Debug, Default)]
struct Rounds {
    next: u64,
    /// seq → (peer, its address, sent at)
    pending: HashMap<u64, (String, Ipv4Addr, Instant)>,
    /// Answered in the current round.
    rtt: HashMap<String, Duration>,
    lost: BTreeMap<String, u64>,
}

impl Rounds {
    /// A pong from `from`: the round trip of the ping it answers, if that
    /// ping is still waited for and went to that address.
    fn pong(&mut self, seq: u64, from: Ipv4Addr, now: Instant) {
        if self.pending.get(&seq).is_some_and(|(_, to, _)| *to == from) {
            let (peer, _, sent) = self.pending.remove(&seq).unwrap();
            self.rtt.insert(peer, now.duration_since(sent));
        }
    }

    /// End the round: the round trips measured in it, the unanswered pings
    /// counted lost. Returns those round trips and the pings to send now.
    fn round(&mut self, targets: &[(String, Ipv4Addr)], now: Instant) -> (HashMap<String, Duration>, Vec<(Ipv4Addr, u64)>) {
        for (_, (peer, _, _)) in self.pending.drain() {
            *self.lost.entry(peer).or_default() += 1;
        }
        self.lost.retain(|peer, _| targets.iter().any(|(p, _)| p == peer));
        let rtt = std::mem::take(&mut self.rtt);
        let pings = targets
            .iter()
            .map(|(peer, addr)| {
                self.next += 1;
                self.pending.insert(self.next, (peer.clone(), *addr, now));
                (*addr, self.next)
            })
            .collect();
        (rtt, pings)
    }
}

/// The echo socket of a node and its pings.
pub(crate) struct Echo {
    socket: Arc<UdpSocket>,
    rounds: Arc<Mutex<Rounds>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Echo {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Echo {
    /// Answer pings on `addr` (this node's overlay address) from nodes in
    /// `cluster`, and collect the pongs of this node's pings.
    pub async fn bind(addr: Ipv4Addr, cluster: (Ipv4Addr, u8)) -> Result<Self> {
        let socket = Arc::new(UdpSocket::bind(SocketAddrV4::new(addr, PORT)).await.with_context(|| format!("failed to bind {addr}:{PORT}/udp"))?);
        let rounds = Arc::new(Mutex::new(Rounds::default()));
        let task = tokio::spawn(serve(socket.clone(), rounds.clone(), cluster));
        Ok(Echo { socket, rounds, task })
    }

    /// End a round and ping `targets` (peer name, overlay address) for the
    /// next one. Returns the round trips measured since the last call.
    pub fn round(&self, targets: &[(String, Ipv4Addr)]) -> HashMap<String, Duration> {
        let (rtt, pings) = self.rounds.lock().unwrap().round(targets, Instant::now());
        for (addr, seq) in pings {
            let _ = self.socket.try_send_to(&encode(PING, seq), SocketAddr::V4(SocketAddrV4::new(addr, PORT)));
        }
        rtt
    }

    /// Pings to `peer` never answered.
    pub fn lost(&self, peer: &str) -> u64 {
        self.rounds.lock().unwrap().lost.get(peer).copied().unwrap_or(0)
    }
}

async fn serve(socket: Arc<UdpSocket>, rounds: Arc<Mutex<Rounds>>, cluster: (Ipv4Addr, u8)) {
    let mut buf = [0u8; 64];
    loop {
        let (n, from) = match socket.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let now = Instant::now();
        let SocketAddr::V4(from) = from else { continue };
        if !in_cidr(*from.ip(), cluster) {
            continue;
        }
        match decode(&buf[..n]) {
            Some((PING, seq)) => {
                let _ = socket.send_to(&encode(PONG, seq), from).await;
            }
            Some((PONG, seq)) => rounds.lock().unwrap().pong(seq, *from.ip(), now),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_round_trip_and_junk_is_ignored() {
        assert_eq!(decode(&encode(PING, 42)), Some((PING, 42)));
        assert_eq!(decode(&encode(PONG, u64::MAX)), Some((PONG, u64::MAX)));
        let mut bad = encode(PING, 1);
        bad[4] = 9;
        assert_eq!(decode(&bad), None);
        assert_eq!(decode(b"litek8s overlay probe"), None);
        assert_eq!(decode(&encode(PING, 1)[..LEN - 1]), None);
    }

    #[test]
    fn cluster_cidr_check() {
        let c = (Ipv4Addr::new(10, 42, 0, 0), 16);
        assert!(in_cidr(Ipv4Addr::new(10, 42, 7, 0), c));
        assert!(!in_cidr(Ipv4Addr::new(10, 43, 0, 1), c));
        assert!(in_cidr(Ipv4Addr::new(1, 2, 3, 4), (Ipv4Addr::UNSPECIFIED, 0)));
    }

    #[test]
    fn rounds_measure_and_count_lost() {
        let (a, b) = (Ipv4Addr::new(10, 42, 1, 0), Ipv4Addr::new(10, 42, 2, 0));
        let targets = vec![("a".to_string(), a), ("b".to_string(), b)];
        let t0 = Instant::now();
        let mut r = Rounds::default();
        let (rtt, pings) = r.round(&targets, t0);
        assert!(rtt.is_empty());
        assert_eq!(pings.len(), 2);
        let seq_a = pings.iter().find(|(addr, _)| *addr == a).unwrap().1;
        let seq_b = pings.iter().find(|(addr, _)| *addr == b).unwrap().1;
        // A pong from the wrong address, or for an unknown ping, counts for nothing.
        r.pong(seq_a, b, t0 + Duration::from_millis(1));
        r.pong(999, a, t0 + Duration::from_millis(1));
        r.pong(seq_a, a, t0 + Duration::from_millis(12));
        // Answered once only.
        r.pong(seq_a, a, t0 + Duration::from_millis(50));
        let (rtt, _) = r.round(&targets, t0 + Duration::from_secs(10));
        assert_eq!(rtt.get("a"), Some(&Duration::from_millis(12)));
        assert_eq!(rtt.get("b"), None);
        assert_eq!(r.lost.get("b"), Some(&1));
        assert_eq!(r.lost.get("a"), None);
        // A late pong (after its round ended) is not counted.
        r.pong(seq_b, b, t0 + Duration::from_secs(11));
        assert!(r.rtt.is_empty());
        // A peer that leaves is forgotten.
        let (_, pings) = r.round(&targets[..1], t0 + Duration::from_secs(20));
        assert_eq!(pings.len(), 1);
        assert_eq!(r.lost.get("b"), None);
        assert_eq!(r.lost.get("a"), Some(&1));
    }

    #[tokio::test]
    async fn echo_over_loopback() {
        let lo = Ipv4Addr::LOCALHOST;
        // Port 7 needs root: run the same exchange on an ephemeral port.
        let socket = Arc::new(UdpSocket::bind((lo, 0)).await.unwrap());
        let port = socket.local_addr().unwrap().port();
        let rounds = Arc::new(Mutex::new(Rounds::default()));
        let task = tokio::spawn(serve(socket.clone(), rounds.clone(), (Ipv4Addr::new(127, 0, 0, 0), 8)));
        let (_, pings) = rounds.lock().unwrap().round(&[("self".to_string(), lo)], Instant::now());
        for (addr, seq) in pings {
            socket.send_to(&encode(PING, seq), (addr, port)).await.unwrap();
        }
        // ping → serve answers pong → serve records it.
        let mut measured = None;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if let Some(d) = rounds.lock().unwrap().rtt.get("self") {
                measured = Some(*d);
                break;
            }
        }
        task.abort();
        assert!(measured.is_some_and(|d| d < Duration::from_secs(1)));
    }
}
