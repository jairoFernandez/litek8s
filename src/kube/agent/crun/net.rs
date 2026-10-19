//! Pod networking without CNI plugins: one `litek8s0` bridge on the node
//! (the gateway of the node's pod CIDR), and per pod a network namespace
//! bind-mounted under `/run/netns` (so `ip netns` sees it) with a veth into
//! the bridge, an address from host-local style IPAM, and a default route
//! via the bridge. Egress is masqueraded; hostPorts are DNAT rules.
//!
//! There is no pause container: the namespaces are kept alive by their bind
//! mounts, and every container of the pod joins them by path.

use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub(crate) const BRIDGE: &str = "litek8s0";
pub(crate) const NETNS_DIR: &str = "/run/netns";

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Subnet {
    pub base: u32,
    pub prefix: u8,
}

impl Subnet {
    pub fn parse(cidr: &str) -> Result<Self> {
        let (addr, prefix) = cidr.split_once('/').context("CIDR without a prefix length")?;
        let addr: Ipv4Addr = addr.parse()?;
        let prefix: u8 = prefix.parse()?;
        if !(8..=30).contains(&prefix) {
            bail!("pod CIDR {cidr}: prefix must be between /8 and /30");
        }
        let mask = u32::MAX << (32 - prefix);
        Ok(Self { base: u32::from(addr) & mask, prefix })
    }

    pub fn size(&self) -> u32 {
        1 << (32 - self.prefix)
    }

    pub fn gateway(&self) -> Ipv4Addr {
        Ipv4Addr::from(self.base + 1)
    }

    pub fn cidr(&self) -> String {
        format!("{}/{}", Ipv4Addr::from(self.base), self.prefix)
    }
}

/// The next free address offset after `last` (round robin, like CNI's
/// host-local: a just-released address is not reused right away), skipping
/// the network address, the gateway (.1) and the broadcast address.
pub(crate) fn next_free(subnet: &Subnet, last: Option<u32>, used: &HashSet<u32>) -> Option<u32> {
    let size = subnet.size();
    let first = 2;
    let end = size - 1;
    let start = last.map(|l| l + 1).filter(|o| (first..end).contains(o)).unwrap_or(first);
    (start..end).chain(first..start).find(|o| !used.contains(o))
}

/// Address reservations as files named after the address (content: the
/// owning sandbox), so they survive agent restarts.
pub(crate) struct Ipam {
    dir: PathBuf,
    pub subnet: Subnet,
}

impl Ipam {
    pub fn open(dir: &Path, subnet: Subnet) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
        Ok(Self { dir: dir.to_path_buf(), subnet })
    }

    fn used(&self) -> HashSet<u32> {
        std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<Ipv4Addr>().ok())
            .map(|ip| u32::from(ip).wrapping_sub(self.subnet.base))
            .collect()
    }

    /// Reserve an address for `owner`. Callers serialize allocations.
    pub fn allocate(&self, owner: &str) -> Result<Ipv4Addr> {
        let last = std::fs::read_to_string(self.dir.join("last"))
            .ok()
            .and_then(|s| s.trim().parse::<Ipv4Addr>().ok())
            .map(|ip| u32::from(ip).wrapping_sub(self.subnet.base));
        let mut used = self.used();
        loop {
            let offset = next_free(&self.subnet, last, &used).with_context(|| format!("no free address in {}", self.subnet.cidr()))?;
            let ip = Ipv4Addr::from(self.subnet.base + offset);
            match std::fs::OpenOptions::new().write(true).create_new(true).open(self.dir.join(ip.to_string())) {
                Ok(mut f) => {
                    use std::io::Write;
                    f.write_all(owner.as_bytes())?;
                    std::fs::write(self.dir.join("last"), ip.to_string())?;
                    return Ok(ip);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    used.insert(offset);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Release every address `owner` holds.
    pub fn release(&self, owner: &str) {
        for e in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.parse::<Ipv4Addr>().is_ok() && std::fs::read_to_string(e.path()).is_ok_and(|o| o == owner) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    /// Release addresses whose owner is not in `live`.
    pub fn release_unless(&self, live: &HashSet<String>) {
        for e in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.parse::<Ipv4Addr>().is_ok() && std::fs::read_to_string(e.path()).is_ok_and(|o| !live.contains(&o)) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// The NAT and filter rules the bridge needs, for `iptables-restore
/// --noflush` (declaring a chain flushes it, so reloading is idempotent).
/// Traffic to other pods of the cluster (over the overlay) keeps its pod
/// source address; everything else leaving the node is masqueraded.
pub(crate) fn base_rules(subnet: &Subnet, cluster: &Subnet) -> String {
    let cidr = subnet.cidr();
    let cluster = cluster.cidr();
    format!(
        "*nat\n\
:LITEK8S-POSTROUTING - [0:0]\n\
-A LITEK8S-POSTROUTING -s {cidr} ! -d {cluster} ! -o {BRIDGE} -j MASQUERADE\n\
-A LITEK8S-POSTROUTING -s {cidr} -o {BRIDGE} -m conntrack --ctstate DNAT -j MASQUERADE\n\
COMMIT\n\
*filter\n\
:LITEK8S-FORWARD - [0:0]\n\
-A LITEK8S-FORWARD -i {BRIDGE} -j ACCEPT\n\
-A LITEK8S-FORWARD -o {BRIDGE} -j ACCEPT\n\
COMMIT\n"
    )
}

/// The hostPort chain for every running sandbox's ports:
/// (host port, protocol, pod ip, container port).
pub(crate) fn hostport_rules(ports: &[(u16, String, String, u16, String)]) -> String {
    let mut out = String::from("*nat\n:LITEK8S-HOSTPORTS - [0:0]\n");
    for (host_port, proto, ip, port, host_ip) in ports {
        let proto = proto.to_lowercase();
        // A hostIP narrows the port to that address of the node.
        let dst = match host_ip.as_str() {
            "" | "0.0.0.0" => String::new(),
            h => format!("-d {h}/32 "),
        };
        out.push_str(&format!("-A LITEK8S-HOSTPORTS {dst}-p {proto} -m {proto} --dport {host_port} -j DNAT --to-destination {ip}:{port}\n"));
    }
    out.push_str("COMMIT\n");
    out
}

/// Jumps from the built-in chains into ours, added once.
pub(crate) const HOOKS: &str = "\
iptables -t nat -C POSTROUTING -j LITEK8S-POSTROUTING 2>/dev/null || iptables -t nat -I POSTROUTING 1 -j LITEK8S-POSTROUTING; \
iptables -t filter -C FORWARD -j LITEK8S-FORWARD 2>/dev/null || iptables -t filter -I FORWARD 1 -j LITEK8S-FORWARD; \
iptables -t nat -N LITEK8S-HOSTPORTS 2>/dev/null; \
for c in PREROUTING OUTPUT; do iptables -t nat -C $c -m addrtype --dst-type LOCAL -j LITEK8S-HOSTPORTS 2>/dev/null || \
iptables -t nat -A $c -m addrtype --dst-type LOCAL -j LITEK8S-HOSTPORTS; done";

/// `ip -batch` commands run inside a new pod network namespace: loopback
/// up, a veth whose peer goes to the host (named `host_if`), the address
/// and the default route.
pub(crate) fn pod_link_commands(host_if: &str, ip: Ipv4Addr, subnet: &Subnet, mtu: u32) -> String {
    format!(
        "link set lo up\n\
link add eth0 mtu {mtu} type veth peer name {host_if} mtu {mtu} netns 1\n\
addr add {ip}/{prefix} dev eth0\n\
link set eth0 up\n\
route add default via {gw} dev eth0\n",
        prefix = subnet.prefix,
        gw = subnet.gateway()
    )
}

/// The bridge, its address and forwarding; idempotent.
pub(crate) fn bridge_commands(subnet: &Subnet) -> String {
    format!(
        "ip link show {BRIDGE} >/dev/null 2>&1 || ip link add {BRIDGE} type bridge; \
ip addr replace {gw}/{prefix} dev {BRIDGE}; ip link set {BRIDGE} up",
        gw = subnet.gateway(),
        prefix = subnet.prefix
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnet_math() {
        let s = Subnet::parse("10.42.0.7/24").unwrap();
        assert_eq!(s.cidr(), "10.42.0.0/24");
        assert_eq!(s.gateway(), Ipv4Addr::new(10, 42, 0, 1));
        assert_eq!(s.size(), 256);
        assert!(Subnet::parse("10.42.0.0/31").is_err());
    }

    #[test]
    fn allocation_is_round_robin_and_skips_reserved() {
        let s = Subnet::parse("10.42.0.0/29").unwrap();
        let mut used = HashSet::new();
        assert_eq!(next_free(&s, None, &used), Some(2));
        assert_eq!(next_free(&s, Some(2), &used), Some(3));
        used.insert(3);
        assert_eq!(next_free(&s, Some(2), &used), Some(4));
        // Wraps past the broadcast address (.7) back to .2.
        assert_eq!(next_free(&s, Some(6), &used), Some(2));
        used.extend([2, 4, 5, 6]);
        assert_eq!(next_free(&s, Some(6), &used), None);
    }

    #[test]
    fn ipam_persists_and_releases() {
        let tmp = tempfile::tempdir().unwrap();
        let ipam = Ipam::open(tmp.path(), Subnet::parse("10.42.0.0/29").unwrap()).unwrap();
        let a = ipam.allocate("pod-a").unwrap();
        let b = ipam.allocate("pod-b").unwrap();
        assert_eq!((a, b), (Ipv4Addr::new(10, 42, 0, 2), Ipv4Addr::new(10, 42, 0, 3)));
        ipam.release("pod-a");
        // Not reused right away.
        assert_eq!(ipam.allocate("pod-c").unwrap(), Ipv4Addr::new(10, 42, 0, 4));
        let reopened = Ipam::open(tmp.path(), ipam.subnet).unwrap();
        assert_eq!(reopened.allocate("pod-d").unwrap(), Ipv4Addr::new(10, 42, 0, 5));
        assert_eq!(reopened.allocate("pod-e").unwrap(), Ipv4Addr::new(10, 42, 0, 6));
        assert_eq!(reopened.allocate("pod-f").unwrap(), Ipv4Addr::new(10, 42, 0, 2));
        assert!(reopened.allocate("pod-g").is_err());
        reopened.release_unless(&HashSet::from(["pod-b".to_string()]));
        assert_eq!(reopened.used(), HashSet::from([3]));
    }

    #[test]
    fn rules_render() {
        let s = Subnet::parse("10.42.0.0/24").unwrap();
        let cluster = Subnet::parse("10.42.0.0/16").unwrap();
        assert!(base_rules(&s, &cluster).contains("-A LITEK8S-POSTROUTING -s 10.42.0.0/24 ! -d 10.42.0.0/16 ! -o litek8s0 -j MASQUERADE\n"));
        let r = hostport_rules(&[(8080, "TCP".into(), "10.42.0.5".into(), 80, String::new()), (53, "UDP".into(), "10.42.0.6".into(), 53, "192.168.1.7".into())]);
        assert!(r.contains("-A LITEK8S-HOSTPORTS -p tcp -m tcp --dport 8080 -j DNAT --to-destination 10.42.0.5:80\n"));
        assert!(r.contains("-A LITEK8S-HOSTPORTS -d 192.168.1.7/32 -p udp -m udp --dport 53 -j DNAT --to-destination 10.42.0.6:53\n"));
        let cmds = pod_link_commands("lk1234", Ipv4Addr::new(10, 42, 0, 5), &s, 1420);
        assert!(cmds.contains("link add eth0 mtu 1420 type veth peer name lk1234 mtu 1420 netns 1\n"));
        assert!(cmds.contains("addr add 10.42.0.5/24 dev eth0\n") && cmds.contains("route add default via 10.42.0.1 dev eth0\n"));
    }
}
