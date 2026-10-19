//! Plain parsing of host files for the dev sandbox (platform independent,
//! so it is unit-tested everywhere).

use std::path::Path;

///
/// `unreachable` are servers the sandbox cannot use either: pasta maps the
/// host's default gateway address to the host's loopback, so a home
/// router that is both gateway and DNS server is not reachable as such.
pub fn resolv_conf_without_loopback(host: &str, fallback: &str, unreachable: &[&str]) -> String {
    let mut out = String::new();
    let mut servers = 0;
    for line in host.lines() {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("nameserver") => {
                let addr = words.next().unwrap_or_default();
                if addr.starts_with("127.") || addr == "::1" || unreachable.contains(&addr) {
                    continue;
                }
                servers += 1;
                out.push_str(line.trim());
                out.push('\n');
            }
            Some("search" | "options" | "domain") => {
                out.push_str(line.trim());
                out.push('\n');
            }
            _ => {}
        }
    }
    if servers == 0 {
        out.insert_str(0, &format!("nameserver {fallback}\n"));
    }
    out
}

pub fn parse_default_gateway(table: &str) -> Option<String> {
    table.lines().skip(1).find_map(|l| {
        let cols: Vec<&str> = l.split_whitespace().collect();
        if cols.len() > 2 && cols[1] == "00000000" {
            let gw = u32::from_str_radix(cols[2], 16).ok()?;
            Some(std::net::Ipv4Addr::from(gw.to_le_bytes()).to_string())
        } else {
            None
        }
    })
}

pub fn parse_subordinate(content: &str, user: &str, uid: u32) -> Option<(u32, u32)> {
    content.lines().find_map(|l| {
        let mut f = l.trim().split(':');
        let (name, start, count) = (f.next()?, f.next()?, f.next()?);
        (name == user || name == uid.to_string()).then(|| Some((start.parse().ok()?, count.parse().ok()?)))?
    })
}

pub fn apparmor_profile(exe: &Path) -> String {
    format!(
        "# litek8s dev: lets litek8s create the user namespace its rootless dev\n\
         # cluster runs in (Ubuntu restricts unprivileged user namespaces).\n\
         # Written by `litek8s dev setup`.\n\
         abi <abi/4.0>,\n\
         include <tunables/global>\n\n\
         profile litek8s {} flags=(unconfined) {{\n  userns,\n\n  include if exists <local/litek8s>\n}}\n",
        exe.display()
    )
}

pub const APPARMOR_PROFILE_PATH: &str = "/etc/apparmor.d/litek8s";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolv_conf_drops_loopback_stubs() {
        let host = "# managed\nnameserver 127.0.0.53\noptions edns0 trust-ad\nsearch lan\n";
        assert_eq!(resolv_conf_without_loopback(host, "169.254.1.1", &[]), "nameserver 169.254.1.1\noptions edns0 trust-ad\nsearch lan\n");
        assert_eq!(resolv_conf_without_loopback("nameserver 192.168.0.1\nsearch .\n", "169.254.1.1", &["192.168.0.1"]), "nameserver 169.254.1.1\nsearch .\n");
        let upstream = "nameserver 10.0.2.3\nnameserver fec0::3\n";
        assert_eq!(resolv_conf_without_loopback(upstream, "169.254.1.1", &[]), upstream);
    }

    #[test]
    fn parses_route_table_and_subuid() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\nenp0s1\t00000000\t0202000A\t0003\t0\t0\t100\t00000000\nenp0s1\t0002000A\t00000000\t0001\t0\t0\t100\t00FFFFFF\n";
        assert_eq!(parse_default_gateway(table).as_deref(), Some("10.0.2.2"));
        let subuid = "alice:100000:65536\nbench:165536:65536\n";
        assert_eq!(parse_subordinate(subuid, "bench", 1000), Some((165536, 65536)));
        assert_eq!(parse_subordinate("1000:200000:1000\n", "x", 1000), Some((200000, 1000)));
        assert_eq!(parse_subordinate(subuid, "carol", 1001), None);
    }
}
