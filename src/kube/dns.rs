//! Cluster DNS, answered from the object store (what CoreDNS does in k3s).
//!
//! - `<service>.<ns>.svc.cluster.local`: the ClusterIP (each node's service
//!   proxy routes it), or the ready pod IPs for a headless Service.
//! - `<pod>.<service>.<ns>.svc.cluster.local` (StatefulSet pods, `hostname`
//!   plus `subdomain`) and `<a-b-c-d>.<ns>.pod.cluster.local`.
//! - SRV `_<port>._<proto>.<service>.<ns>.svc.cluster.local`, PTR for service
//!   and pod addresses, a CNAME for ExternalName Services; UDP and TCP.
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

/// Answers some clients get before cluster names and the upstream
/// resolver (Docker containers' names on their networks, see
/// `crate::docker::network::DnsTable`): `None` passes.
pub type ExtraResolver = Box<dyn Fn(std::net::IpAddr, &str) -> Option<Vec<Ipv4Addr>> + Send + Sync>;

static EXTRA_RESOLVER: std::sync::OnceLock<ExtraResolver> = std::sync::OnceLock::new();

pub fn set_extra_resolver(r: ExtraResolver) {
    let _ = EXTRA_RESOLVER.set(r);
}

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
    let mut buf = vec![0u8; 1500];
    loop {
        let Ok((n, peer)) = socket.recv_from(&mut buf).await else { continue };
        let query = buf[..n].to_vec();
        let (api, socket) = (api.clone(), socket.clone());
        tokio::spawn(async move {
            if let Some(reply) = answer(&api, &query, peer.ip()).await {
                let _ = socket.send_to(&reply, peer).await;
            }
        });
    }
}

/// DNS over TCP (length-prefixed messages) on the same address.
pub async fn serve_tcp(api: Arc<ApiState>, listener: tokio::net::TcpListener) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let Ok((mut conn, peer)) = listener.accept().await else { continue };
        let api = api.clone();
        tokio::spawn(async move {
            loop {
                let mut len = [0u8; 2];
                if conn.read_exact(&mut len).await.is_err() {
                    return;
                }
                let mut query = vec![0u8; u16::from_be_bytes(len) as usize];
                if conn.read_exact(&mut query).await.is_err() {
                    return;
                }
                let Some(reply) = answer(&api, &query, peer.ip()).await else { return };
                let mut out = (reply.len() as u16).to_be_bytes().to_vec();
                out.extend_from_slice(&reply);
                if conn.write_all(&out).await.is_err() {
                    return;
                }
            }
        });
    }
}

async fn answer(api: &ApiState, query: &[u8], client: std::net::IpAddr) -> Option<Vec<u8>> {
    let (name, qtype, end) = parse_question(query)?;
    if let Some(answers) = EXTRA_RESOLVER.get().and_then(|r| r(client, &name)) {
        // A name known here: its addresses for A (and ANY), no records for
        // other types (AAAA), never NXDOMAIN.
        let records = if matches!(qtype, TYPE_A | TYPE_ANY) { answers.into_iter().map(Record::A).collect() } else { Vec::new() };
        return Some(build_reply(query, end, qtype, Some(records)));
    }
    if name.ends_with(DOMAIN) || reverse_ours(&name) {
        let records = resolve(api, &name, qtype);
        Some(build_reply(query, end, qtype, records))
    } else {
        // Read per query: the host's resolver can change (DHCP in the dev
        // VM finishes after the API server starts, VPNs).
        forward(query, upstream_resolver()).await
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

#[derive(Debug, Clone, PartialEq)]
enum Record {
    A(Ipv4Addr),
    Cname(String),
    Srv { port: u16, target: String },
    Ptr(String),
}

const TYPE_CNAME: u16 = 5;
const TYPE_PTR: u16 = 12;
const TYPE_SRV: u16 = 33;
const TYPE_ANY: u16 = 255;

/// Reverse lookups this server answers: service and pod addresses.
fn reverse_ours(name: &str) -> bool {
    name.strip_suffix(".in-addr.arpa").is_some_and(|ip| ip.ends_with(".42.10") || ip.ends_with(".43.10"))
}

fn fqdn(parts: &[&str]) -> String {
    format!("{}.{DOMAIN}", parts.join("."))
}

/// The records of `name` for `qtype`; `None` is NXDOMAIN, an empty list a
/// name that exists without records of that type.
fn resolve(api: &ApiState, name: &str, qtype: u16) -> Option<Vec<Record>> {
    if let Some(rev) = name.strip_suffix(".in-addr.arpa") {
        let mut octets: Vec<&str> = rev.split('.').collect();
        octets.reverse();
        let ip = octets.join(".");
        return reverse(api, &ip).map(|n| if matches!(qtype, TYPE_PTR | TYPE_ANY) { vec![Record::Ptr(n)] } else { Vec::new() });
    }
    let all = records(api, name)?;
    // A CNAME answers every type; otherwise only matching records.
    if let Some(c) = all.iter().find(|r| matches!(r, Record::Cname(_))) {
        return Some(vec![c.clone()]);
    }
    Some(
        all.into_iter()
            .filter(|r| {
                qtype == TYPE_ANY
                    || matches!((r, qtype), (Record::A(_), TYPE_A) | (Record::Srv { .. }, TYPE_SRV) | (Record::Ptr(_), TYPE_PTR))
            })
            .collect(),
    )
}

/// The name a service or pod address has.
fn reverse(api: &ApiState, ip: &str) -> Option<String> {
    let (services, _) = api.store().list("", "services", None);
    if let Some(svc) = services.iter().find(|s| util::s(s, "/spec/clusterIP") == ip) {
        let (ns, name) = util::ns_name(svc);
        return Some(fqdn(&[&name, &ns, "svc"]));
    }
    let (eps, _) = api.store().list("", "endpoints", None);
    for ep in &eps {
        let (ns, svc) = util::ns_name(ep);
        for a in ep.get("subsets").and_then(Value::as_array).into_iter().flatten().flat_map(|s| s.get("addresses").and_then(Value::as_array).cloned().unwrap_or_default()) {
            if a.get("ip").and_then(Value::as_str) == Some(ip) {
                if let Some(h) = a.get("hostname").and_then(Value::as_str) {
                    return Some(fqdn(&[h, &svc, &ns, "svc"]));
                }
            }
        }
    }
    let (pods, _) = api.store().list("", "pods", None);
    pods.iter()
        .find(|p| util::s(p, "/status/podIP") == ip)
        .map(|p| fqdn(&[&ip.replace('.', "-"), util::s(p, "/metadata/namespace"), "pod"]))
}

/// Every record of a cluster name.
fn records(api: &ApiState, name: &str) -> Option<Vec<Record>> {
    let rest = name.strip_suffix(DOMAIN)?.trim_end_matches('.');
    let parts: Vec<&str> = rest.split('.').collect();
    match parts.as_slice() {
        [port, proto, svc, ns, "svc"] if port.starts_with('_') && proto.starts_with('_') => {
            let service = api.get_object("", "services", ns, svc)?;
            let all = srv_records(api, &service);
            let (port, proto) = (&port[1..], &proto[1..]);
            let named: Vec<Record> = all.into_iter().filter(|(n, p, _)| n == port && p.eq_ignore_ascii_case(proto)).map(|(_, _, r)| r).collect();
            if named.is_empty() { None } else { Some(named) }
        }
        [svc, ns, "svc"] => {
            let service = api.get_object("", "services", ns, svc)?;
            if util::s(&service, "/spec/type") == "ExternalName" {
                return Some(vec![Record::Cname(util::s(&service, "/spec/externalName").trim_end_matches('.').to_string())]);
            }
            let mut out: Vec<Record> = match service.pointer("/spec/clusterIP").and_then(Value::as_str).and_then(|ip| ip.parse::<Ipv4Addr>().ok()) {
                // A ClusterIP (the node's service proxy routes it); headless
                // services resolve to their ready pods.
                Some(ip) => vec![Record::A(ip)],
                None => endpoint_ips(api, ns, svc, None).into_iter().map(Record::A).collect(),
            };
            out.extend(srv_records(api, &service).into_iter().map(|(_, _, r)| r));
            Some(out)
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
                return ip.and_then(|p| util::s(p, "/status/podIP").parse().ok()).map(|ip| vec![Record::A(ip)]);
            }
            Some(ips.into_iter().map(Record::A).collect())
        }
        [dashed, _ns, "pod"] => dashed.replace('-', ".").parse().ok().map(|ip| vec![Record::A(ip)]),
        [_, "svc"] | ["svc"] | [_, "pod"] | ["pod"] | [] => Some(Vec::new()),
        _ => None,
    }
}

/// (port name, protocol, SRV record) for each named port of a Service: the
/// service name for a ClusterIP, each endpoint's name when headless.
fn srv_records(api: &ApiState, service: &Value) -> Vec<(String, String, Record)> {
    let (ns, svc) = util::ns_name(service);
    let headless = util::s(service, "/spec/clusterIP") == "None";
    let mut out = Vec::new();
    for port in service.pointer("/spec/ports").and_then(Value::as_array).into_iter().flatten() {
        let pname = util::s(port, "/name").to_string();
        if pname.is_empty() {
            continue;
        }
        let proto = match util::s(port, "/protocol") {
            "" => "TCP".to_string(),
            p => p.to_string(),
        };
        if !headless {
            let p = port.get("port").and_then(Value::as_u64).unwrap_or(0) as u16;
            out.push((pname.clone(), proto.clone(), Record::Srv { port: p, target: fqdn(&[&svc, &ns, "svc"]) }));
            continue;
        }
        let Some(ep) = api.get_object("", "endpoints", &ns, &svc) else { continue };
        for subset in ep.get("subsets").and_then(Value::as_array).into_iter().flatten() {
            let Some(p) = subset.get("ports").and_then(Value::as_array).into_iter().flatten().find(|p| util::s(p, "/name") == pname) else { continue };
            let p = p.get("port").and_then(Value::as_u64).unwrap_or(0) as u16;
            for a in subset.get("addresses").and_then(Value::as_array).into_iter().flatten() {
                let host = match a.get("hostname").and_then(Value::as_str) {
                    Some(h) => h.to_string(),
                    None => util::s(a, "/ip").replace('.', "-"),
                };
                out.push((pname.clone(), proto.clone(), Record::Srv { port: p, target: fqdn(&[&host, &svc, &ns, "svc"]) }));
            }
        }
    }
    out
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

fn encode_name(out: &mut Vec<u8>, name: &str) {
    for label in name.split('.').filter(|l| !l.is_empty()) {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}

fn build_reply(query: &[u8], question_end: usize, _qtype: u16, answers: Option<Vec<Record>>) -> Vec<u8> {
    let mut out = Vec::with_capacity(question_end + 64);
    out.extend_from_slice(&query[0..2]);
    let rd = query[2] & 0x01;
    let rcode = if answers.is_none() { 3 } else { 0 };
    out.push(0x84 | rd); // QR, AA, RD copied
    out.push(0x80 | rcode); // RA
    let records = answers.unwrap_or_default();
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&(records.len() as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&query[12..question_end]);
    for r in records {
        out.extend_from_slice(&[0xc0, 0x0c]);
        let (rtype, data) = match r {
            Record::A(ip) => (TYPE_A, ip.octets().to_vec()),
            Record::Cname(n) => {
                let mut d = Vec::new();
                encode_name(&mut d, &n);
                (TYPE_CNAME, d)
            }
            Record::Ptr(n) => {
                let mut d = Vec::new();
                encode_name(&mut d, &n);
                (TYPE_PTR, d)
            }
            Record::Srv { port, target } => {
                let mut d = vec![0, 0, 0, 100];
                d.extend_from_slice(&port.to_be_bytes());
                encode_name(&mut d, &target);
                (TYPE_SRV, d)
            }
        };
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&TTL.to_be_bytes());
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
        out.extend_from_slice(&data);
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
        let reply = build_reply(&q, end, qtype, Some(vec![Record::A(Ipv4Addr::new(10, 0, 0, 7))]));
        assert_eq!(&reply[0..2], &[0x12, 0x34]);
        assert_eq!(reply[3] & 0x0f, 0, "NOERROR");
        assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 1, "one answer");
        assert_eq!(&reply[reply.len() - 4..], &[10, 0, 0, 7]);
        let nx = build_reply(&q, end, qtype, None);
        assert_eq!(nx[3] & 0x0f, 3, "NXDOMAIN");
        let srv = build_reply(&q, end, TYPE_SRV, Some(vec![Record::Srv { port: 80, target: "a.b".into() }]));
        assert!(srv.ends_with(&[0, 0, 0, 100, 0, 80, 1, b'a', 1, b'b', 0]));
        assert!(reverse_ours("13.0.43.10.in-addr.arpa") && !reverse_ours("1.1.1.1.in-addr.arpa"));
    }
}
