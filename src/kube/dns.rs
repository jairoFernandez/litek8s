//! Cluster DNS, answered from the object store (what CoreDNS does in k3s).
//!
//! - `<service>.<ns>.svc.cluster.local`: the ClusterIP (each node's service
//!   proxy routes it), or the ready pod IPs for a headless Service.
//! - `<pod>.<service>.<ns>.svc.cluster.local` (StatefulSet pods, `hostname`
//!   plus `subdomain`) and `<a-b-c-d>.<ns>.pod.cluster.local`.
//! - Everything else is forwarded to the host's resolver.
//!
//! Pods reach it through the node agent's sandbox (`--dns`); Docker's
//! embedded resolver forwards to it whatever it cannot answer itself.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::net::UdpSocket;

use super::controllers::util;
use super::server::ApiState;

pub const DOMAIN: &str = "cluster.local";
const TTL: u32 = 5;
const TYPE_A: u16 = 1;
const CLASS_IN: u16 = 1;

pub async fn bind(addr: SocketAddr) -> Option<UdpSocket> {
    match UdpSocket::bind(addr).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("apiserver: cluster DNS disabled: cannot listen on udp {addr}: {e}");
            None
        }
    }
}

pub async fn serve(api: Arc<ApiState>, socket: UdpSocket) {
    let socket = Arc::new(socket);
    let upstream = upstream_resolver();
    let mut buf = vec![0u8; 1500];
    loop {
        let Ok((n, peer)) = socket.recv_from(&mut buf).await else { continue };
        let query = buf[..n].to_vec();
        let (api, socket) = (api.clone(), socket.clone());
        tokio::spawn(async move {
            let reply = match parse_question(&query) {
                Some((name, qtype, end)) if name.ends_with(DOMAIN) => {
                    let answers = resolve(&api, &name);
                    Some(build_reply(&query, end, qtype, answers))
                }
                Some(_) => forward(&query, upstream).await,
                None => None,
            };
            if let Some(reply) = reply {
                let _ = socket.send_to(&reply, peer).await;
            }
        });
    }
}

/// The first nameserver of the host, or a public resolver.
fn upstream_resolver() -> SocketAddr {
    std::fs::read_to_string("/etc/resolv.conf")
        .ok()
        .and_then(|conf| {
            conf.lines()
                .filter_map(|l| l.trim().strip_prefix("nameserver"))
                .filter_map(|ip| ip.trim().parse::<std::net::IpAddr>().ok())
                .find(|ip| !ip.is_loopback())
        })
        .map(|ip| SocketAddr::new(ip, 53))
        .unwrap_or_else(|| SocketAddr::from(([1, 1, 1, 1], 53)))
}

async fn forward(query: &[u8], upstream: SocketAddr) -> Option<Vec<u8>> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).await.ok()?;
    socket.send_to(query, upstream).await.ok()?;
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(3), socket.recv(&mut buf)).await.ok()?.ok()?;
    buf.truncate(n);
    Some(buf)
}

/// (lowercased name without the trailing dot, qtype, offset after the question)
fn parse_question(packet: &[u8]) -> Option<(String, u16, usize)> {
    if packet.len() < 12 || u16::from_be_bytes([packet[4], packet[5]]) != 1 {
        return None;
    }
    let mut pos = 12;
    let mut labels = Vec::new();
    loop {
        let len = *packet.get(pos)? as usize;
        pos += 1;
        if len == 0 {
            break;
        }
        if len & 0xc0 != 0 {
            return None;
        }
        labels.push(String::from_utf8_lossy(packet.get(pos..pos + len)?).to_ascii_lowercase());
        pos += len;
    }
    let qtype = u16::from_be_bytes([*packet.get(pos)?, *packet.get(pos + 1)?]);
    Some((labels.join("."), qtype, pos + 4))
}

/// `None` is NXDOMAIN; `Some(vec![])` a name that exists with no A records.
fn resolve(api: &ApiState, name: &str) -> Option<Vec<Ipv4Addr>> {
    let rest = name.strip_suffix(DOMAIN)?.trim_end_matches('.');
    let parts: Vec<&str> = rest.split('.').collect();
    match parts.as_slice() {
        [svc, ns, "svc"] => {
            let service = api.get_object("", "services", ns, svc)?;
            // A ClusterIP (the node's service proxy routes it); headless
            // services resolve to their ready pods.
            match service.pointer("/spec/clusterIP").and_then(Value::as_str).and_then(|ip| ip.parse::<Ipv4Addr>().ok()) {
                Some(ip) => Some(vec![ip]),
                None => Some(endpoint_ips(api, ns, svc, None)),
            }
        }
        [host, svc, ns, "svc"] => {
            api.get_object("", "services", ns, svc)?;
            let ips = endpoint_ips(api, ns, svc, Some(host));
            if ips.is_empty() {
                // A pod with this hostname and subdomain, ready or not.
                let (pods, _) = api.store().list("", "pods", Some(ns));
                let ip = pods.iter().find(|p| {
                    let hostname = match util::s(p, "/spec/hostname") {
                        "" => util::name(p),
                        h => h.to_string(),
                    };
                    hostname == *host && util::s(p, "/spec/subdomain") == *svc
                });
                return ip.and_then(|p| util::s(p, "/status/podIP").parse().ok()).map(|ip| vec![ip]);
            }
            Some(ips)
        }
        [dashed, _ns, "pod"] => dashed.replace('-', ".").parse().ok().map(|ip| vec![ip]),
        [_, "svc"] | ["svc"] | [] => Some(Vec::new()),
        _ => None,
    }
}

/// Ready addresses of a Service's Endpoints, optionally for one hostname.
fn endpoint_ips(api: &ApiState, ns: &str, svc: &str, host: Option<&str>) -> Vec<Ipv4Addr> {
    let Some(ep) = api.get_object("", "endpoints", ns, svc) else { return Vec::new() };
    ep.get("subsets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|s| s.get("addresses").and_then(Value::as_array).cloned().unwrap_or_default())
        .filter(|a| {
            host.is_none_or(|h| {
                a.get("hostname").and_then(Value::as_str) == Some(h)
                    || a.pointer("/targetRef/name").and_then(Value::as_str) == Some(h)
            })
        })
        .filter_map(|a| a.get("ip").and_then(Value::as_str).and_then(|ip| ip.parse().ok()))
        .collect()
}

fn build_reply(query: &[u8], question_end: usize, qtype: u16, answers: Option<Vec<Ipv4Addr>>) -> Vec<u8> {
    let mut out = Vec::with_capacity(question_end + 64);
    out.extend_from_slice(&query[0..2]);
    let rd = query[2] & 0x01;
    let rcode = if answers.is_none() { 3 } else { 0 };
    out.push(0x84 | rd); // QR, AA, RD copied
    out.push(0x80 | rcode); // RA
    let records: Vec<Ipv4Addr> = match (&answers, qtype) {
        (Some(ips), TYPE_A) => ips.clone(),
        _ => Vec::new(),
    };
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&(records.len() as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&query[12..question_end]);
    for ip in records {
        out.extend_from_slice(&[0xc0, 0x0c]);
        out.extend_from_slice(&TYPE_A.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&TTL.to_be_bytes());
        out.extend_from_slice(&4u16.to_be_bytes());
        out.extend_from_slice(&ip.octets());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&CLASS_IN.to_be_bytes());
        q
    }

    #[test]
    fn parses_and_answers() {
        let q = query("Web.Default.svc.cluster.local", TYPE_A);
        let (name, qtype, end) = parse_question(&q).unwrap();
        assert_eq!((name.as_str(), qtype, end), ("web.default.svc.cluster.local", TYPE_A, q.len()));
        let reply = build_reply(&q, end, qtype, Some(vec![Ipv4Addr::new(10, 0, 0, 7)]));
        assert_eq!(&reply[0..2], &[0x12, 0x34]);
        assert_eq!(reply[3] & 0x0f, 0, "NOERROR");
        assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 1, "one answer");
        assert_eq!(&reply[reply.len() - 4..], &[10, 0, 0, 7]);
        let nx = build_reply(&q, end, qtype, None);
        assert_eq!(nx[3] & 0x0f, 3, "NXDOMAIN");
    }
}
