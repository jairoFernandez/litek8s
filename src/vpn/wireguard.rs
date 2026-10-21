//! Kernel WireGuard, configured over generic netlink.
//!
//! The interface itself is created with iproute2 (`ip link add ... type
//! wireguard`); keys, the listen port and peers are set with the
//! `wireguard` generic netlink family, so no `wg` binary is needed on the
//! node. The message layout follows `include/uapi/linux/wireguard.h`.
//!
//! Only what the pod overlay needs is implemented: set the device (private
//! key, listen port, the full peer list), change some peers in place (their
//! sessions survive) and read peers back (handshake times, endpoints and
//! byte counters).

#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use std::net::{Ipv4Addr, SocketAddrV4};

use anyhow::{Context, Result, bail};

use super::keys::{KeyPair, PublicKey};

/// One peer of the device: who it is, where to send to, what it routes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub public_key: PublicKey,
    pub endpoint: Option<SocketAddrV4>,
    /// (address, prefix length)
    pub allowed_ips: Vec<(Ipv4Addr, u8)>,
    /// Seconds; 0 disables.
    pub keepalive: u16,
}

/// A peer as the kernel reports it.
#[derive(Debug, Clone, Default)]
pub struct PeerStatus {
    pub public_key: String,
    pub endpoint: Option<SocketAddrV4>,
    /// Unix seconds of the last completed handshake (0: never).
    pub last_handshake: i64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

pub fn decode_key(key: &PublicKey) -> Result<[u8; 32]> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD.decode(key.as_str())?;
    raw.try_into().map_err(|_| anyhow::anyhow!("WireGuard keys are 32 bytes"))
}

// Netlink and the WireGuard family (linux/netlink.h, linux/genetlink.h,
// linux/wireguard.h).
const NETLINK_GENERIC: i32 = 16;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 4;
const NLM_F_DUMP: u16 = 0x300;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLA_F_NESTED: u16 = 1 << 15;
const GENL_ID_CTRL: u16 = 0x10;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;

const WG_CMD_GET_DEVICE: u8 = 0;
const WG_CMD_SET_DEVICE: u8 = 1;
const WGDEVICE_A_IFNAME: u16 = 2;
const WGDEVICE_A_PRIVATE_KEY: u16 = 3;
const WGDEVICE_A_FLAGS: u16 = 5;
const WGDEVICE_A_LISTEN_PORT: u16 = 6;
const WGDEVICE_A_PEERS: u16 = 8;
const WGDEVICE_F_REPLACE_PEERS: u32 = 1;
const WGPEER_A_PUBLIC_KEY: u16 = 1;
const WGPEER_A_FLAGS: u16 = 3;
const WGPEER_A_ENDPOINT: u16 = 4;
const WGPEER_A_PERSISTENT_KEEPALIVE_INTERVAL: u16 = 5;
const WGPEER_A_LAST_HANDSHAKE_TIME: u16 = 6;
const WGPEER_A_RX_BYTES: u16 = 7;
const WGPEER_A_TX_BYTES: u16 = 8;
const WGPEER_A_ALLOWEDIPS: u16 = 9;
const WGPEER_F_REMOVE_ME: u32 = 1;
const WGPEER_F_REPLACE_ALLOWEDIPS: u32 = 2;
const WGALLOWEDIP_A_FAMILY: u16 = 1;
const WGALLOWEDIP_A_IPADDR: u16 = 2;
const WGALLOWEDIP_A_CIDR_MASK: u16 = 3;
const AF_INET: u16 = 2;

/// A netlink message under construction: attributes appended in order,
/// nested ones closed by patching their length.
struct MsgBuf {
    buf: Vec<u8>,
}

impl MsgBuf {
    fn new(family: u16, flags: u16, cmd: u8) -> Self {
        let mut buf = Vec::with_capacity(512);
        buf.extend_from_slice(&0u32.to_ne_bytes()); // length, patched in finish()
        buf.extend_from_slice(&family.to_ne_bytes());
        buf.extend_from_slice(&flags.to_ne_bytes());
        buf.extend_from_slice(&1u32.to_ne_bytes()); // seq
        buf.extend_from_slice(&0u32.to_ne_bytes()); // pid: the kernel
        buf.extend_from_slice(&[cmd, 1, 0, 0]); // genlmsghdr: cmd, version 1
        Self { buf }
    }

    fn pad(&mut self) {
        while self.buf.len() % 4 != 0 {
            self.buf.push(0);
        }
    }

    fn attr(&mut self, kind: u16, payload: &[u8]) {
        let len = (4 + payload.len()) as u16;
        self.buf.extend_from_slice(&len.to_ne_bytes());
        self.buf.extend_from_slice(&kind.to_ne_bytes());
        self.buf.extend_from_slice(payload);
        self.pad();
    }

    fn begin_nested(&mut self, kind: u16) -> usize {
        let at = self.buf.len();
        self.buf.extend_from_slice(&0u16.to_ne_bytes());
        self.buf.extend_from_slice(&(kind | NLA_F_NESTED).to_ne_bytes());
        at
    }

    fn end_nested(&mut self, at: usize) {
        let len = (self.buf.len() - at) as u16;
        self.buf[at..at + 2].copy_from_slice(&len.to_ne_bytes());
    }

    fn finish(mut self) -> Vec<u8> {
        let len = self.buf.len() as u32;
        self.buf[..4].copy_from_slice(&len.to_ne_bytes());
        self.buf
    }
}

/// Netlink attributes of one level: (type without flags, payload).
fn attrs(mut data: &[u8]) -> Vec<(u16, &[u8])> {
    let mut out = Vec::new();
    while data.len() >= 4 {
        let len = u16::from_ne_bytes([data[0], data[1]]) as usize;
        let kind = u16::from_ne_bytes([data[2], data[3]]) & !(NLA_F_NESTED | (1 << 14));
        if len < 4 || len > data.len() {
            break;
        }
        out.push((kind, &data[4..len]));
        let next = (len + 3) & !3;
        data = &data[next.min(data.len())..];
    }
    out
}

fn sockaddr_in(addr: &SocketAddrV4) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..2].copy_from_slice(&AF_INET.to_ne_bytes());
    out[2..4].copy_from_slice(&addr.port().to_be_bytes());
    out[4..8].copy_from_slice(&addr.ip().octets());
    out
}

fn parse_sockaddr(data: &[u8]) -> Option<SocketAddrV4> {
    if data.len() < 8 || u16::from_ne_bytes([data[0], data[1]]) != AF_INET {
        return None;
    }
    let port = u16::from_be_bytes([data[2], data[3]]);
    Some(SocketAddrV4::new(Ipv4Addr::new(data[4], data[5], data[6], data[7]), port))
}

/// A change to one peer, for [`update`]: added or changed (allowed IPs
/// replaced; the endpoint only when given, so one the kernel learned from
/// the peer's packets is kept), or removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerUpdate {
    pub peer: Peer,
    pub remove: bool,
}

fn peer_attrs(m: &mut MsgBuf, peer: &Peer, remove: bool) -> Result<()> {
    let p = m.begin_nested(0);
    m.attr(WGPEER_A_PUBLIC_KEY, &decode_key(&peer.public_key)?);
    if remove {
        m.attr(WGPEER_A_FLAGS, &WGPEER_F_REMOVE_ME.to_ne_bytes());
        m.end_nested(p);
        return Ok(());
    }
    m.attr(WGPEER_A_FLAGS, &WGPEER_F_REPLACE_ALLOWEDIPS.to_ne_bytes());
    if let Some(ep) = &peer.endpoint {
        m.attr(WGPEER_A_ENDPOINT, &sockaddr_in(ep));
    }
    m.attr(WGPEER_A_PERSISTENT_KEEPALIVE_INTERVAL, &peer.keepalive.to_ne_bytes());
    let ips = m.begin_nested(WGPEER_A_ALLOWEDIPS);
    for (ip, prefix) in &peer.allowed_ips {
        let a = m.begin_nested(0);
        m.attr(WGALLOWEDIP_A_FAMILY, &AF_INET.to_ne_bytes());
        m.attr(WGALLOWEDIP_A_IPADDR, &ip.octets());
        m.attr(WGALLOWEDIP_A_CIDR_MASK, &[*prefix]);
        m.end_nested(a);
    }
    m.end_nested(ips);
    m.end_nested(p);
    Ok(())
}

fn set_device_message(family: u16, ifname: &str, key: &KeyPair, listen_port: u16, peers: &[Peer]) -> Result<Vec<u8>> {
    let mut m = MsgBuf::new(family, NLM_F_REQUEST | NLM_F_ACK, WG_CMD_SET_DEVICE);
    let mut name = ifname.as_bytes().to_vec();
    name.push(0);
    m.attr(WGDEVICE_A_IFNAME, &name);
    m.attr(WGDEVICE_A_PRIVATE_KEY, &key.secret_bytes());
    m.attr(WGDEVICE_A_LISTEN_PORT, &listen_port.to_ne_bytes());
    m.attr(WGDEVICE_A_FLAGS, &WGDEVICE_F_REPLACE_PEERS.to_ne_bytes());
    let list = m.begin_nested(WGDEVICE_A_PEERS);
    for peer in peers {
        peer_attrs(&mut m, peer, false)?;
    }
    m.end_nested(list);
    Ok(m.finish())
}

fn update_message(family: u16, ifname: &str, updates: &[PeerUpdate]) -> Result<Vec<u8>> {
    let mut m = MsgBuf::new(family, NLM_F_REQUEST | NLM_F_ACK, WG_CMD_SET_DEVICE);
    let mut name = ifname.as_bytes().to_vec();
    name.push(0);
    m.attr(WGDEVICE_A_IFNAME, &name);
    let list = m.begin_nested(WGDEVICE_A_PEERS);
    for u in updates {
        peer_attrs(&mut m, &u.peer, u.remove)?;
    }
    m.end_nested(list);
    Ok(m.finish())
}

fn parse_peers(payload: &[u8], out: &mut Vec<PeerStatus>) {
    for (kind, data) in attrs(payload) {
        if kind != WGDEVICE_A_PEERS {
            continue;
        }
        for (_, peer) in attrs(data) {
            let mut status = PeerStatus::default();
            for (k, v) in attrs(peer) {
                match k {
                    WGPEER_A_PUBLIC_KEY => {
                        use base64::Engine;
                        status.public_key = base64::engine::general_purpose::STANDARD.encode(v);
                    }
                    WGPEER_A_ENDPOINT => status.endpoint = parse_sockaddr(v),
                    WGPEER_A_LAST_HANDSHAKE_TIME if v.len() >= 8 => {
                        status.last_handshake = i64::from_ne_bytes(v[..8].try_into().unwrap_or_default());
                    }
                    WGPEER_A_RX_BYTES if v.len() >= 8 => status.rx_bytes = u64::from_ne_bytes(v[..8].try_into().unwrap_or_default()),
                    WGPEER_A_TX_BYTES if v.len() >= 8 => status.tx_bytes = u64::from_ne_bytes(v[..8].try_into().unwrap_or_default()),
                    _ => {}
                }
            }
            // A large device is split over messages; a peer continued from
            // the previous one has no key and is merged into it.
            if status.public_key.is_empty() {
                if let Some(last) = out.last_mut() {
                    last.rx_bytes = last.rx_bytes.max(status.rx_bytes);
                }
                continue;
            }
            out.push(status);
        }
    }
}

#[cfg(target_os = "linux")]
mod sys {
    use super::*;

    pub struct Socket(i32);

    impl Drop for Socket {
        fn drop(&mut self) {
            // SAFETY: we own the descriptor.
            unsafe { libc::close(self.0) };
        }
    }

    impl Socket {
        pub fn open() -> Result<Self> {
            // SAFETY: plain socket(2)/bind(2) calls on a zeroed sockaddr_nl.
            unsafe {
                let fd = libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, NETLINK_GENERIC);
                if fd < 0 {
                    bail!("netlink socket: {}", std::io::Error::last_os_error());
                }
                let socket = Socket(fd);
                let mut addr: libc::sockaddr_nl = std::mem::zeroed();
                addr.nl_family = libc::AF_NETLINK as u16;
                let rc = libc::bind(fd, &addr as *const _ as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_nl>() as u32);
                if rc < 0 {
                    bail!("netlink bind: {}", std::io::Error::last_os_error());
                }
                let timeout = libc::timeval { tv_sec: 5, tv_usec: 0 };
                libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, &timeout as *const _ as *const libc::c_void, std::mem::size_of::<libc::timeval>() as u32);
                Ok(socket)
            }
        }

        fn send(&self, msg: &[u8]) -> Result<()> {
            // SAFETY: msg is a valid buffer for its length.
            let n = unsafe { libc::send(self.0, msg.as_ptr() as *const libc::c_void, msg.len(), 0) };
            if n < 0 {
                bail!("netlink send: {}", std::io::Error::last_os_error());
            }
            Ok(())
        }

        /// Send a request and collect the payloads (after the genetlink
        /// header) of every reply until the ack or the end of a dump.
        pub fn request(&self, msg: &[u8]) -> Result<Vec<Vec<u8>>> {
            self.send(msg)?;
            let mut out = Vec::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                // SAFETY: buf is valid for its length.
                let n = unsafe { libc::recv(self.0, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
                if n < 0 {
                    bail!("netlink recv: {}", std::io::Error::last_os_error());
                }
                let mut data = &buf[..n as usize];
                while data.len() >= 16 {
                    let len = u32::from_ne_bytes(data[..4].try_into().unwrap()) as usize;
                    let kind = u16::from_ne_bytes([data[4], data[5]]);
                    if len < 16 || len > data.len() {
                        bail!("malformed netlink reply");
                    }
                    let body = &data[16..len];
                    match kind {
                        NLMSG_DONE => return Ok(out),
                        NLMSG_ERROR => {
                            let code = i32::from_ne_bytes(body[..4].try_into().unwrap_or_default());
                            if code == 0 {
                                return Ok(out);
                            }
                            return Err(std::io::Error::from_raw_os_error(-code).into());
                        }
                        _ => out.push(body.get(4..).unwrap_or_default().to_vec()),
                    }
                    data = &data[((len + 3) & !3).min(data.len())..];
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn family_id(socket: &sys::Socket) -> Result<u16> {
    // With an ack requested, the reply is followed by the ack that ends
    // `request`'s read loop.
    let mut m = MsgBuf::new(GENL_ID_CTRL, NLM_F_REQUEST | NLM_F_ACK, CTRL_CMD_GETFAMILY);
    m.attr(CTRL_ATTR_FAMILY_NAME, b"wireguard\0");
    for reply in socket.request(&m.finish()).context("the kernel has no WireGuard support")? {
        for (kind, data) in attrs(&reply) {
            if kind == CTRL_ATTR_FAMILY_ID && data.len() >= 2 {
                return Ok(u16::from_ne_bytes([data[0], data[1]]));
            }
        }
    }
    bail!("the kernel has no WireGuard support (no wireguard netlink family)")
}

/// Set the device's key, port and its whole peer list (others are removed).
#[cfg(target_os = "linux")]
pub fn configure(ifname: &str, key: &KeyPair, listen_port: u16, peers: &[Peer]) -> Result<()> {
    let socket = sys::Socket::open()?;
    let family = family_id(&socket)?;
    socket
        .request(&set_device_message(family, ifname, key, listen_port, peers)?)
        .with_context(|| format!("failed to configure WireGuard device {ifname}"))?;
    Ok(())
}

/// Change some peers, leaving the others (and their sessions) alone.
#[cfg(target_os = "linux")]
pub fn update(ifname: &str, updates: &[PeerUpdate]) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    let socket = sys::Socket::open()?;
    let family = family_id(&socket)?;
    // Netlink messages are limited to 64 KiB: a few peers at a time.
    for chunk in updates.chunks(64) {
        socket
            .request(&update_message(family, ifname, chunk)?)
            .with_context(|| format!("failed to update the peers of WireGuard device {ifname}"))?;
    }
    Ok(())
}

/// The device's peers as the kernel sees them.
#[cfg(target_os = "linux")]
pub fn peers(ifname: &str) -> Result<Vec<PeerStatus>> {
    let socket = sys::Socket::open()?;
    let family = family_id(&socket)?;
    let mut m = MsgBuf::new(family, NLM_F_REQUEST | NLM_F_DUMP, WG_CMD_GET_DEVICE);
    let mut name = ifname.as_bytes().to_vec();
    name.push(0);
    m.attr(WGDEVICE_A_IFNAME, &name);
    let mut out = Vec::new();
    for reply in socket.request(&m.finish())? {
        parse_peers(&reply, &mut out);
    }
    Ok(out)
}

#[cfg(not(target_os = "linux"))]
pub fn configure(_ifname: &str, _key: &KeyPair, _listen_port: u16, _peers: &[Peer]) -> Result<()> {
    bail!("kernel WireGuard needs Linux")
}

#[cfg(not(target_os = "linux"))]
pub fn update(_ifname: &str, _updates: &[PeerUpdate]) -> Result<()> {
    bail!("kernel WireGuard needs Linux")
}

#[cfg(not(target_os = "linux"))]
pub fn peers(_ifname: &str) -> Result<Vec<PeerStatus>> {
    bail!("kernel WireGuard needs Linux")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_device_message_layout() {
        let key = KeyPair::generate();
        let peer = Peer {
            public_key: KeyPair::generate().public(),
            endpoint: Some("192.168.1.2:51820".parse().unwrap()),
            allowed_ips: vec![(Ipv4Addr::new(10, 42, 1, 0), 24)],
            keepalive: 25,
        };
        let msg = set_device_message(30, "litek8s-wg", &key, 51820, std::slice::from_ref(&peer)).unwrap();
        assert_eq!(u32::from_ne_bytes(msg[..4].try_into().unwrap()) as usize, msg.len());
        assert_eq!(u16::from_ne_bytes([msg[4], msg[5]]), 30);
        assert_eq!(msg[16], WG_CMD_SET_DEVICE);
        let top = attrs(&msg[20..]);
        let kinds: Vec<u16> = top.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec![WGDEVICE_A_IFNAME, WGDEVICE_A_PRIVATE_KEY, WGDEVICE_A_LISTEN_PORT, WGDEVICE_A_FLAGS, WGDEVICE_A_PEERS]);
        assert_eq!(top[0].1, b"litek8s-wg\0");
        assert_eq!(top[2].1, 51820u16.to_ne_bytes());
        let peers = attrs(top[4].1);
        assert_eq!(peers.len(), 1);
        let fields = attrs(peers[0].1);
        assert_eq!(fields[0], (WGPEER_A_PUBLIC_KEY, &decode_key(&peer.public_key).unwrap()[..]));
        let endpoint = fields.iter().find(|(k, _)| *k == WGPEER_A_ENDPOINT).unwrap().1;
        assert_eq!(parse_sockaddr(endpoint), peer.endpoint);
        let ips = attrs(fields.iter().find(|(k, _)| *k == WGPEER_A_ALLOWEDIPS).unwrap().1);
        let ip = attrs(ips[0].1);
        assert_eq!(ip[1], (WGALLOWEDIP_A_IPADDR, &[10u8, 42, 1, 0][..]));
        assert_eq!(ip[2], (WGALLOWEDIP_A_CIDR_MASK, &[24u8][..]));
    }

    #[test]
    fn update_message_keeps_the_device_and_removes_peers() {
        let changed = Peer { public_key: KeyPair::generate().public(), endpoint: None, allowed_ips: vec![], keepalive: 25 };
        let gone = Peer { public_key: KeyPair::generate().public(), endpoint: None, allowed_ips: vec![(Ipv4Addr::new(10, 42, 2, 0), 24)], keepalive: 25 };
        let msg = update_message(30, "litek8s-wg", &[PeerUpdate { peer: changed.clone(), remove: false }, PeerUpdate { peer: gone, remove: true }]).unwrap();
        let top = attrs(&msg[20..]);
        // No key, port or replace-peers flag: other peers stay as they are.
        assert_eq!(top.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec![WGDEVICE_A_IFNAME, WGDEVICE_A_PEERS]);
        let peers = attrs(top[1].1);
        let first = attrs(peers[0].1);
        assert!(first.iter().all(|(k, _)| *k != WGPEER_A_ENDPOINT), "no endpoint given: the kernel's is kept");
        assert_eq!(first.iter().find(|(k, _)| *k == WGPEER_A_FLAGS).unwrap().1, WGPEER_F_REPLACE_ALLOWEDIPS.to_ne_bytes());
        let ips = attrs(first.iter().find(|(k, _)| *k == WGPEER_A_ALLOWEDIPS).unwrap().1);
        assert!(ips.is_empty());
        let second = attrs(peers[1].1);
        assert_eq!(second.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec![WGPEER_A_PUBLIC_KEY, WGPEER_A_FLAGS]);
        assert_eq!(second[1].1, WGPEER_F_REMOVE_ME.to_ne_bytes());
    }

    #[test]
    fn parses_peer_status() {
        // A GET_DEVICE reply body: one peer with a key, endpoint, handshake.
        let mut m = MsgBuf::new(30, 0, WG_CMD_GET_DEVICE);
        let list = m.begin_nested(WGDEVICE_A_PEERS);
        let p = m.begin_nested(0);
        m.attr(WGPEER_A_PUBLIC_KEY, &[7u8; 32]);
        m.attr(WGPEER_A_ENDPOINT, &sockaddr_in(&"10.0.0.9:51820".parse().unwrap()));
        let mut ts = 1_700_000_000i64.to_ne_bytes().to_vec();
        ts.extend_from_slice(&0i64.to_ne_bytes());
        m.attr(WGPEER_A_LAST_HANDSHAKE_TIME, &ts);
        m.attr(WGPEER_A_RX_BYTES, &123u64.to_ne_bytes());
        m.end_nested(p);
        m.end_nested(list);
        let msg = m.finish();
        let mut out = Vec::new();
        parse_peers(&msg[20..], &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].last_handshake, 1_700_000_000);
        assert_eq!(out[0].rx_bytes, 123);
        assert_eq!(out[0].endpoint, Some("10.0.0.9:51820".parse().unwrap()));
    }
}
