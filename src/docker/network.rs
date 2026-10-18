//! Docker networks on crun: one Linux bridge per bridge network (the
//! predefined `bridge` on `lkdocker0`, user-defined ones on `br-<id>`),
//! a veth per container and network, addresses from file-backed IPAM,
//! masquerading for egress and isolation between networks with iptables
//! (all of it inside the dev sandbox's or VM's network namespace).
//!
//! Containers on user-defined networks find each other by name, alias and
//! short ID through the cluster DNS server, which asks [`DnsTable`] first
//! (Docker's embedded DNS server does the same in each container).
//! Published ports are userspace listeners on the host's loopback (through
//! the dev sandbox's broker on Linux; the Mac forwards them into the VM).

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
#[cfg(target_os = "linux")]
use anyhow::Context;

use crate::kube::agent::crun::net::{Ipam, Subnet};

pub const DEFAULT_BRIDGE: &str = "lkdocker0";
pub const DEFAULT_SUBNET: &str = "172.17.0.0/16";
const NAT_CHAIN: &str = "LITEK8S-DOCKER-POST";
const FWD_CHAIN: &str = "LITEK8S-DOCKER-FWD";

/// Subnets handed to new networks, in order (Docker's default pools, minus
/// the pod and service ranges litek8s uses).
pub fn candidate_subnets() -> impl Iterator<Item = String> {
    (18..=31).map(|b| format!("172.{b}.0.0/16")).chain((0..16).map(|i| format!("192.168.{}.0/20", i * 16)))
}

pub fn bridge_name(id: &str, name: &str) -> String {
    if name == "bridge" { DEFAULT_BRIDGE.to_string() } else { format!("br-{}", &id[..12]) }
}

/// MAC address Docker derives from the IPv4 address.
pub fn mac_for(ip: Ipv4Addr) -> String {
    let o = ip.octets();
    format!("02:42:{:02x}:{:02x}:{:02x}:{:02x}", o[0], o[1], o[2], o[3])
}

pub(crate) fn ipam_for(root: &Path, network_id: &str, subnet: &str) -> Result<Ipam> {
    Ipam::open(&root.join("ipam").join(network_id), Subnet::parse(subnet)?)
}

/// Shell commands that create (idempotently) a network's bridge.
pub fn bridge_commands(bridge: &str, gateway: &str, prefix: u8) -> String {
    format!(
        "ip link show {bridge} >/dev/null 2>&1 || ip link add {bridge} type bridge; \
ip addr replace {gateway}/{prefix} dev {bridge}; ip link set {bridge} up"
    )
}

/// One network's part in the container's `ip -batch`.
pub struct LinkPlan {
    pub interface: String,
    pub host_if: String,
    pub ip: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Ipv4Addr,
    pub mac: String,
    pub default_route: bool,
}

pub fn link_commands(links: &[LinkPlan], with_lo: bool) -> String {
    let mut out = String::new();
    if with_lo {
        out.push_str("link set lo up\n");
    }
    for l in links {
        out.push_str(&format!(
            "link add {i} type veth peer name {h} netns 1\naddr add {ip}/{p} dev {i}\nlink set {i} address {mac}\nlink set {i} up\n",
            i = l.interface,
            h = l.host_if,
            ip = l.ip,
            p = l.prefix,
            mac = l.mac
        ));
        if l.default_route {
            out.push_str(&format!("route add default via {} dev {}\n", l.gateway, l.interface));
        }
    }
    out
}

/// What iptables needs to know about a bridge network.
pub struct RuleNet {
    pub bridge: String,
    pub subnet: String,
    pub internal: bool,
}

/// The engine's chains for `iptables-restore --noflush`: egress NAT for
/// external networks, forwarding inside each bridge, no traffic between
/// different networks or out of internal ones.
pub fn rules(nets: &[RuleNet]) -> String {
    let mut nat = format!("*nat\n:{NAT_CHAIN} - [0:0]\n");
    let mut filter = format!("*filter\n:{FWD_CHAIN} - [0:0]\n");
    for a in nets {
        for b in nets.iter().filter(|b| b.bridge != a.bridge) {
            filter.push_str(&format!("-A {FWD_CHAIN} -i {} -o {} -j DROP\n", a.bridge, b.bridge));
        }
    }
    for n in nets {
        if n.internal {
            filter.push_str(&format!("-A {FWD_CHAIN} -i {b} -o {b} -j ACCEPT\n", b = n.bridge));
            filter.push_str(&format!("-A {FWD_CHAIN} -i {} -j DROP\n", n.bridge));
            filter.push_str(&format!("-A {FWD_CHAIN} -o {} -j DROP\n", n.bridge));
        } else {
            nat.push_str(&format!("-A {NAT_CHAIN} -s {} ! -o {} -j MASQUERADE\n", n.subnet, n.bridge));
            filter.push_str(&format!("-A {FWD_CHAIN} -i {} -j ACCEPT\n", n.bridge));
            filter.push_str(&format!("-A {FWD_CHAIN} -o {} -j ACCEPT\n", n.bridge));
        }
    }
    nat.push_str("COMMIT\n");
    filter.push_str("COMMIT\n");
    nat + &filter
}

/// Jumps from the built-in chains to the engine's, added once.
pub fn hooks() -> String {
    format!(
        "iptables -t nat -N {NAT_CHAIN} 2>/dev/null; iptables -t filter -N {FWD_CHAIN} 2>/dev/null; \
iptables -t nat -C POSTROUTING -j {NAT_CHAIN} 2>/dev/null || iptables -t nat -I POSTROUTING 1 -j {NAT_CHAIN}; \
iptables -t filter -C FORWARD -j {FWD_CHAIN} 2>/dev/null || iptables -t filter -I FORWARD 1 -j {FWD_CHAIN}"
    )
}

/// Names containers answer to on their user-defined networks.
#[derive(Default)]
pub struct DnsTable {
    /// container address -> (container id, network id)
    by_ip: HashMap<Ipv4Addr, (String, String)>,
    /// network id -> [(lowercased names, address)]
    names: HashMap<String, Vec<(Vec<String>, Ipv4Addr)>>,
    /// Where `host.docker.internal` points.
    pub host_gateway: Option<Ipv4Addr>,
}

impl DnsTable {
    /// Replace everything known about a container: its addresses and
    /// names per network (`dns` false for the default bridge, where Docker
    /// has no name resolution).
    pub fn set(&mut self, container: &str, entries: Vec<(String, Ipv4Addr, Vec<String>, bool)>) {
        self.remove(container);
        for (network, ip, names, dns) in entries {
            self.by_ip.insert(ip, (container.to_string(), network.clone()));
            if dns {
                let names = names.into_iter().map(|n| n.to_lowercase()).collect();
                self.names.entry(network).or_default().push((names, ip));
            }
        }
    }

    pub fn remove(&mut self, container: &str) {
        let ips: Vec<Ipv4Addr> = self.by_ip.iter().filter(|(_, (c, _))| c == container).map(|(ip, _)| *ip).collect();
        for ip in &ips {
            self.by_ip.remove(ip);
        }
        for list in self.names.values_mut() {
            list.retain(|(_, ip)| !ips.contains(ip));
        }
    }

    /// `None`: not a name for this client (cluster DNS / upstream answer).
    pub fn resolve(&self, client: IpAddr, name: &str) -> Option<Vec<Ipv4Addr>> {
        let IpAddr::V4(client) = client else { return None };
        let (container, _) = self.by_ip.get(&client)?;
        let name = name.trim_end_matches('.').to_lowercase();
        if matches!(name.as_str(), "host.docker.internal" | "gateway.docker.internal") {
            return self.host_gateway.map(|g| vec![g]);
        }
        let networks: Vec<&String> = self.by_ip.iter().filter(|(_, (c, _))| c == container).map(|(_, (_, n))| n).collect();
        let mut out = Vec::new();
        for n in networks {
            for (names, ip) in self.names.get(n).into_iter().flatten() {
                if names.iter().any(|x| *x == name) && !out.contains(ip) {
                    out.push(*ip);
                }
            }
        }
        (!out.is_empty()).then_some(out)
    }
}

/// Where host listeners for published ports come from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Publisher {
    /// The dev sandbox's broker binds them on the host's loopback.
    Broker,
    /// Bound here (the Mac's dev VM: the Mac forwards its localhost ports
    /// into the VM's loopback).
    Local,
}

impl Publisher {
    /// A listener for host port `port` (0: any free one).
    pub fn bind(self, port: u16) -> Result<std::net::TcpListener> {
        match self {
            Publisher::Broker => {
                if let Some(l) = crate::dev::broker::bind_host_port(port) {
                    return Ok(l);
                }
                bail!("Bind for 127.0.0.1:{port} failed: port is already allocated")
            }
            Publisher::Local => {
                let bind = |p: u16| -> std::io::Result<std::net::TcpListener> {
                    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], p));
                    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)?;
                    socket.set_reuse_address(true)?;
                    socket.bind(&addr.into())?;
                    socket.listen(1024)?;
                    let l: std::net::TcpListener = socket.into();
                    l.set_nonblocking(true)?;
                    Ok(l)
                };
                if port != 0 {
                    return bind(port).map_err(|e| anyhow::anyhow!("Bind for 127.0.0.1:{port} failed: {e}"));
                }
                // Below the Mac's ephemeral range (49152+), so the same port
                // is free on the Mac side too.
                for _ in 0..200 {
                    let mut b = [0u8; 2];
                    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut b);
                    let p = 32768 + (u16::from_le_bytes(b) % 16384);
                    if let Ok(l) = bind(p) {
                        return Ok(l);
                    }
                }
                bail!("no free port for publishing")
            }
        }
    }
}

/// Relay connections on `listener` to `target` until aborted.
pub async fn proxy(listener: std::net::TcpListener, target: std::net::SocketAddr) {
    let _ = listener.set_nonblocking(true);
    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else { return };
    loop {
        let Ok((mut client, _)) = listener.accept().await else {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            continue;
        };
        tokio::spawn(async move {
            let _ = client.set_nodelay(true);
            let connect = tokio::time::timeout(std::time::Duration::from_secs(5), tokio::net::TcpStream::connect(target)).await;
            if let Ok(Ok(mut upstream)) = connect {
                let _ = upstream.set_nodelay(true);
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            }
        });
    }
}

/// Run `ip -batch` commands inside an existing network namespace (hot
/// `docker network connect`).
#[cfg(target_os = "linux")]
pub fn ip_batch_in(netns: PathBuf, batch: String) -> Result<()> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    std::thread::spawn(move || -> Result<()> {
        let f = std::fs::File::open(&netns).with_context(|| format!("no network namespace {}", netns.display()))?;
        // SAFETY: setns on this short-lived thread only.
        if unsafe { libc::setns(f.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
            return Err(std::io::Error::last_os_error()).context("setns");
        }
        let mut child = std::process::Command::new("ip")
            .args(["-batch", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("failed to run ip")?;
        child.stdin.take().unwrap().write_all(batch.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("network setup failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    })
    .join()
    .map_err(|_| anyhow::anyhow!("netns thread panicked"))?
}

#[cfg(not(target_os = "linux"))]
pub fn ip_batch_in(_: PathBuf, _: String) -> Result<()> {
    bail!("needs Linux")
}

/// Parse `--publish`-style bindings from HostConfig.PortBindings:
/// "80/tcp" -> [(host ip, host port)].
pub fn port_bindings(host_config: &serde_json::Value) -> BTreeMap<String, Vec<(String, u16)>> {
    let mut out = BTreeMap::new();
    if let Some(map) = host_config["PortBindings"].as_object() {
        for (key, list) in map {
            let key = if key.contains('/') { key.clone() } else { format!("{key}/tcp") };
            let mut v = Vec::new();
            for b in list.as_array().into_iter().flatten() {
                let ip = b["HostIp"].as_str().unwrap_or_default().to_string();
                let port = b["HostPort"].as_str().and_then(|p| p.split('-').next()?.parse().ok()).unwrap_or(0);
                v.push((ip, port));
            }
            if v.is_empty() {
                v.push((String::new(), 0));
            }
            out.insert(key, v);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_answers_per_network() {
        let mut t = DnsTable::default();
        t.set("db", vec![("n1".into(), Ipv4Addr::new(172, 18, 0, 2), vec!["db".into(), "proj-db-1".into()], true)]);
        t.set("web", vec![("n1".into(), Ipv4Addr::new(172, 18, 0, 3), vec!["web".into()], true)]);
        t.set("other", vec![("n2".into(), Ipv4Addr::new(172, 19, 0, 2), vec!["db".into()], true)]);
        let web: IpAddr = Ipv4Addr::new(172, 18, 0, 3).into();
        assert_eq!(t.resolve(web, "DB."), Some(vec![Ipv4Addr::new(172, 18, 0, 2)]));
        assert_eq!(t.resolve(web, "proj-db-1"), Some(vec![Ipv4Addr::new(172, 18, 0, 2)]));
        assert_eq!(t.resolve(web, "example.com"), None);
        // Not a container: no answer from the table.
        assert_eq!(t.resolve(Ipv4Addr::new(10, 42, 0, 5).into(), "db"), None);
        t.remove("db");
        assert_eq!(t.resolve(web, "db"), None);
    }

    #[test]
    fn rules_isolate_networks() {
        let r = rules(&[
            RuleNet { bridge: "lkdocker0".into(), subnet: "172.17.0.0/16".into(), internal: false },
            RuleNet { bridge: "br-abc".into(), subnet: "172.18.0.0/16".into(), internal: true },
        ]);
        assert!(r.contains("-A LITEK8S-DOCKER-POST -s 172.17.0.0/16 ! -o lkdocker0 -j MASQUERADE"));
        assert!(!r.contains("172.18.0.0/16 ! -o"));
        assert!(r.contains("-A LITEK8S-DOCKER-FWD -i lkdocker0 -o br-abc -j DROP"));
        assert_eq!(mac_for(Ipv4Addr::new(172, 17, 0, 2)), "02:42:ac:11:00:02");
        let hc = serde_json::json!({"PortBindings": {"80/tcp": [{"HostIp": "", "HostPort": "8080"}], "53/udp": []}});
        let b = port_bindings(&hc);
        assert_eq!(b["80/tcp"], vec![(String::new(), 8080)]);
        assert_eq!(b["53/udp"], vec![(String::new(), 0)]);
    }
}
