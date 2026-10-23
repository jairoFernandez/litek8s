//! PID 1 of the macOS dev VM (`/init` in its initramfs is this binary).
//!
//! The VM is a kernel, a read-only root disk (Alpine's crun, iptables,
//! iproute2 and busybox around this binary; an initrd mounts it under a
//! tmpfs overlay and hands over) and a data disk. Init mounts the usual
//! filesystems, loads the kernel modules the cluster needs, formats the data
//! disk on first boot, mounts the Mac's shared directory (virtiofs) at the
//! same path as on the Mac, gets an address by DHCP and runs the cell
//! manager ([`super::cells`]) as a child, which runs each cluster in cells
//! of its own; init reaps orphans and restarts the manager if it dies. It
//! talks to the host over the console (hvc0): it reports
//! `LITEK8S_GUEST ip=...` once the network is up and powers off on a
//! `poweroff` line from the host; cells' output comes through it too, as
//! `LITEK8S_LOG <cluster> <line>`, and its memory, as `LITEK8S_MEM ...`
//! (the host moves the VM's memory ceiling with it, see
//! [`super::balloon`]).

/// The default dev cluster's node name (the VM's hostname too).
pub const NODE_NAME: &str = "litek8s-dev";

/// The API server port inside the VM.
pub const GUEST_API_PORT: u16 = 6443;

/// Where the data disk is mounted in the VM.
pub const DATA_DIR: &str = "/var/lib/litek8s";

/// The vsock port the guest forwards from: the host connects, sends
/// `tcp <port> [<cell>]\n` and is spliced to 127.0.0.1:<port> in the VM or
/// in a cluster's cell (the API server, NodePorts, the Docker API). The API
/// then needs neither the VM's address nor its network, and listens on
/// loopback only. `stats\n` gets every cluster's memory and CPU time.
pub const VSOCK_PORT: u32 = 1024;

/// Written by init once DHCP has found the Mac's address on the VM network
/// (see `LITEK8S_HOST_LOOPBACK`); outside /run, which every cell has its own of.
pub const HOST_ADDRESS_FILE: &str = "/etc/litek8s-host-address";

/// Kernel command line options init understands.
#[derive(Debug, Default, PartialEq)]
pub struct BootOptions {
    /// Mount the `share` virtiofs tag here (the Mac's path).
    pub share: Option<String>,
    /// Data disk device.
    pub disk: String,
    /// Random bytes from the host, credited to the guest's entropy pool so
    /// getrandom() does not block early in boot (hex).
    pub seed: Option<Vec<u8>>,
    /// More virtiofs shares: (tag, path to mount it at).
    pub mounts: Vec<(String, String)>,
    /// The Mac shares Rosetta (tag `rosetta`): run linux/amd64 binaries
    /// with it.
    pub rosetta: bool,
    /// Clusters deleted while the VM was down: their data goes at boot.
    pub purge: Vec<String>,
}

/// The size of the ext4 filesystem whose superblock (the 1024 bytes at
/// offset 1024 of the device) is `sb`, in bytes.
pub fn ext4_bytes(sb: &[u8]) -> Option<u64> {
    let u32_at = |o: usize| Some(u32::from_le_bytes(sb.get(o..o + 4)?.try_into().ok()?) as u64);
    if sb.get(0x38..0x3a)? != [0x53, 0xef] {
        return None;
    }
    let block = 1024u64 << u32_at(0x18)?;
    // The high half of the block count only with the 64bit feature.
    let high = if u32_at(0x60)? & 0x80 != 0 { u32_at(0x150)? } else { 0 };
    Some((high << 32 | u32_at(0x04)?) * block)
}

/// Whether the data disk is bigger than its filesystem by more than a
/// block group (the host grew the image: `disk` in the settings).
pub fn should_grow(device: u64, fs: u64) -> bool {
    device > fs + (128 << 20)
}

pub fn parse_cmdline(cmdline: &str) -> BootOptions {
    let mut o = BootOptions { disk: "/dev/vda".into(), ..Default::default() };
    for word in cmdline.split_whitespace() {
        if let Some(v) = word.strip_prefix("litek8s.share=") {
            o.share = Some(v.to_string());
        } else if let Some(v) = word.strip_prefix("litek8s.disk=") {
            o.disk = v.to_string();
        } else if let Some(v) = word.strip_prefix("litek8s.seed=") {
            o.seed = hex::decode(v).ok();
        } else if let Some(v) = word.strip_prefix("litek8s.mounts=") {
            o.mounts = v.split(',').filter_map(|m| m.split_once(':')).map(|(t, p)| (t.to_string(), p.to_string())).collect();
        } else if word == "litek8s.rosetta=1" {
            o.rosetta = true;
        } else if let Some(v) = word.strip_prefix("litek8s.purge=") {
            o.purge = v.split(',').filter(|n| super::cluster::validate(n).is_ok()).map(str::to_string).collect();
        }
    }
    o
}


/// A DHCPv4 lease, as the in-init client gets it (vmnet's DHCP server
/// answers in ~10 ms; busybox udhcpc retries only every second, which made
/// some boots take seconds longer).
#[derive(Debug, Clone, PartialEq)]
pub struct Lease {
    pub ip: std::net::Ipv4Addr,
    pub prefix: u8,
    pub router: Option<std::net::Ipv4Addr>,
    pub dns: Vec<std::net::Ipv4Addr>,
    pub server: std::net::Ipv4Addr,
}

const DHCP_MAGIC: [u8; 4] = [99, 130, 83, 99];

/// A DHCPDISCOVER (`request` None) or DHCPREQUEST for (address, server).
pub fn dhcp_packet(xid: u32, mac: [u8; 6], request: Option<(std::net::Ipv4Addr, std::net::Ipv4Addr)>) -> Vec<u8> {
    let mut p = vec![0u8; 236];
    p[0] = 1; // BOOTREQUEST
    p[1] = 1; // ethernet
    p[2] = 6;
    p[4..8].copy_from_slice(&xid.to_be_bytes());
    p[10] = 0x80; // broadcast replies: we have no address yet
    p[28..34].copy_from_slice(&mac);
    p.extend_from_slice(&DHCP_MAGIC);
    match request {
        None => p.extend_from_slice(&[53, 1, 1]),
        Some((ip, server)) => {
            p.extend_from_slice(&[53, 1, 3, 50, 4]);
            p.extend_from_slice(&ip.octets());
            p.extend_from_slice(&[54, 4]);
            p.extend_from_slice(&server.octets());
        }
    }
    p.extend_from_slice(&[55, 4, 1, 3, 6, 51, 255]);
    p
}

/// (message type, lease) of a reply to `xid`.
pub fn parse_dhcp_reply(buf: &[u8], xid: u32) -> Option<(u8, Lease)> {
    if buf.len() < 240 || buf[0] != 2 || buf[4..8] != xid.to_be_bytes() || buf[236..240] != DHCP_MAGIC {
        return None;
    }
    let ip = std::net::Ipv4Addr::new(buf[16], buf[17], buf[18], buf[19]);
    let mut lease = Lease { ip, prefix: 24, router: None, dns: Vec::new(), server: std::net::Ipv4Addr::UNSPECIFIED };
    let mut kind = 0;
    let mut i = 240;
    while i + 1 < buf.len() && buf[i] != 255 {
        if buf[i] == 0 {
            i += 1;
            continue;
        }
        let (code, len) = (buf[i], buf[i + 1] as usize);
        let v = buf.get(i + 2..i + 2 + len)?;
        let addr = |o: usize| std::net::Ipv4Addr::new(v[o], v[o + 1], v[o + 2], v[o + 3]);
        match code {
            53 if len == 1 => kind = v[0],
            1 if len == 4 => lease.prefix = u32::from_be_bytes([v[0], v[1], v[2], v[3]]).count_ones() as u8,
            3 if len >= 4 => lease.router = Some(addr(0)),
            6 => lease.dns = (0..len / 4).map(|k| addr(k * 4)).collect(),
            54 if len == 4 => lease.server = addr(0),
            _ => {}
        }
        i += 2 + len;
    }
    Some((kind, lease))
}

#[cfg(target_os = "linux")]
pub use init::main;

#[cfg(target_os = "linux")]
mod init {
    use std::ffi::CString;
    use std::io::{BufRead, Read, Write};
    use std::os::fd::FromRawFd;
    use std::time::{Duration, Instant};

    use super::*;

    /// Modules loaded up front; netfilter's come on demand through
    /// /sbin/modprobe (busybox) when iptables asks for them.
    const MODULES: &[&str] = &[
        "virtio_net", "virtio_blk", "virtiofs", "overlay", "ext4", "bridge", "veth", "br_netfilter", "nf_conntrack", "nf_nat",
        "xt_MASQUERADE", "xt_addrtype", "xt_statistic", "xt_conntrack", "xt_comment", "xt_mark", "xt_tcpudp", "nft_compat",
        "nft_chain_nat", "iptable_nat", "iptable_filter", "hw_random_virtio", "virtio_balloon", "vmw_vsock_virtio_transport",
    ];

    /// Accept host connections on vsock and splice each to the local TCP
    /// port its first line names.
    fn serve_vsock() -> std::io::Result<()> {
        // SAFETY: plain socket calls with a sockaddr_vm we own.
        let fd = unsafe {
            let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut addr: libc::sockaddr_vm = std::mem::zeroed();
            addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
            addr.svm_port = VSOCK_PORT;
            addr.svm_cid = libc::VMADDR_CID_ANY;
            if libc::bind(fd, (&addr as *const libc::sockaddr_vm).cast(), std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t) != 0
                || libc::listen(fd, 128) != 0
            {
                let e = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(e);
            }
            fd
        };
        std::thread::spawn(move || {
            loop {
                // SAFETY: accept on our listening socket.
                let conn = unsafe { libc::accept4(fd, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
                if conn < 0 {
                    continue;
                }
                // SAFETY: we own the accepted descriptor.
                let conn = unsafe { std::os::unix::net::UnixStream::from_raw_fd(conn) };
                std::thread::spawn(move || splice_vsock(conn));
            }
        });
        Ok(())
    }

    fn splice_vsock(conn: std::os::unix::net::UnixStream) {
        // Read the request line byte by byte so nothing after it is lost.
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        let mut reader = &conn;
        while line.len() < 64 {
            match reader.read(&mut byte) {
                Ok(1) if byte[0] == b'\n' => break,
                Ok(1) => line.push(byte[0]),
                _ => return,
            }
        }
        let text = String::from_utf8_lossy(&line).to_string();
        if text.trim() == "stats" {
            let stats = super::super::env::GuestStats { cells: super::super::env::cell_stats(std::path::Path::new("/sys/fs/cgroup")), meminfo: super::super::memory::meminfo(), proc_stat: super::super::cpu::proc_stat() };
            let _ = (&conn).write_all(&serde_json::to_vec(&stats).unwrap_or_default());
            return;
        }
        let mut words = text.strip_prefix("tcp ").unwrap_or_default().split_whitespace();
        let Some(port) = words.next().and_then(|p| p.parse::<u16>().ok()) else { return };
        // In a cell: connect from its network namespace (this thread's only).
        if let Some(cell) = words.next() {
            if !cell.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') {
                return;
            }
            let Ok(ns) = std::fs::File::open(format!("{}/{cell}.net", super::super::cells::NETNS_DIR)) else { return };
            use std::os::fd::AsRawFd;
            // SAFETY: setns on this thread with a namespace file we hold open.
            if unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
                return;
            }
        }
        let Ok(tcp) = std::net::TcpStream::connect(("127.0.0.1", port)) else { return };
        let _ = tcp.set_nodelay(true);
        let (Ok(mut conn_r), Ok(mut tcp_w)) = (conn.try_clone(), tcp.try_clone()) else { return };
        let up = std::thread::spawn(move || {
            let _ = std::io::copy(&mut conn_r, &mut tcp_w);
            let _ = tcp_w.shutdown(std::net::Shutdown::Write);
        });
        let (mut tcp_r, mut conn_w) = (tcp, conn);
        let _ = std::io::copy(&mut tcp_r, &mut conn_w);
        let _ = conn_w.shutdown(std::net::Shutdown::Write);
        let _ = up.join();
    }

    /// Register Rosetta (shared by the Mac) as the interpreter of x86-64 ELF
    /// binaries. `F` opens it now, so it works inside containers' mount
    /// namespaces too.
    fn setup_rosetta() {
        const REGISTER: &str = r":rosetta:M::\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00:\xff\xff\xff\xff\xff\xfe\xfe\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff:/run/rosetta/rosetta:CF";
        if let Err(e) = mount("rosetta", "/run/rosetta", "virtiofs", libc::MS_RDONLY, "") {
            say(&format!("no Rosetta share: {e}"));
            return;
        }
        let _ = std::process::Command::new("/sbin/modprobe").args(["-q", "binfmt_misc"]).status();
        let _ = mount("binfmt_misc", "/proc/sys/fs/binfmt_misc", "binfmt_misc", libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, "");
        if std::path::Path::new("/proc/sys/fs/binfmt_misc/rosetta").exists() {
            return;
        }
        if let Err(e) = std::fs::write("/proc/sys/fs/binfmt_misc/register", REGISTER) {
            say(&format!("cannot register Rosetta with binfmt_misc: {e}"));
        }
    }

    /// Report the guest's memory to the host (`LITEK8S_MEM ...`, see
    /// [`super::super::balloon`]): every 2 s if it changed (every 250 ms
    /// while little is available), every 30 s anyway, and at once when
    /// memory stalls 100 ms within a second (a PSI trigger), so the host
    /// can grow the VM's ceiling before the workload notices.
    fn watch_memory() {
        use std::os::fd::AsRawFd;
        use super::super::balloon;
        // Under the ceiling, a burst of allocations would otherwise make
        // the kernel evict the code everything runs (the root filesystem
        // is a disk now, not RAM) and thrash for minutes instead of
        // running out of memory: with the working set of the last second
        // protected, it calls the OOM path at once, where the balloon
        // gives memory back before anything is killed.
        if let Err(e) = std::fs::write("/sys/kernel/mm/lru_gen/min_ttl_ms", "1000") {
            say(&format!("cannot protect the working set (lru_gen min_ttl_ms: {e})"));
        }
        let trigger = std::fs::OpenOptions::new().read(true).write(true).open("/proc/pressure/memory").and_then(|mut f| {
            f.write_all(b"some 100000 1000000\0")?;
            Ok(f)
        });
        if let Err(e) = &trigger {
            say(&format!("no memory pressure trigger ({e}); the host polls only"));
        }
        let mut last: Option<balloon::Report> = None;
        let mut sent = Instant::now();
        loop {
            let mut fired = false;
            match &trigger {
                Ok(f) => {
                    let mut pfd = libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLPRI, revents: 0 };
                    // SAFETY: one pollfd we own.
                    let n = unsafe { libc::poll(&mut pfd, 1, interval(&last)) };
                    fired = n > 0 && pfd.revents & libc::POLLPRI != 0;
                    if n > 0 && pfd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(interval(&last) as u64)),
            }
            let Some(r) = balloon::read(fired) else { continue };
            if last.is_none_or(|l| r.differs(&l)) || sent.elapsed() >= Duration::from_secs(30) {
                println!("LITEK8S_MEM {}", r.to_line());
                let _ = std::io::stdout().flush();
                last = Some(r);
                sent = Instant::now();
            }
        }
    }

    /// Discard the data disk's free blocks (FITRIM): the framework punches
    /// them out of the sparse disk.img on the Mac, so deleted images and
    /// containers give their space back. Bytes discarded.
    fn trim(dir: &str) -> std::io::Result<u64> {
        #[repr(C)]
        struct Range {
            start: u64,
            len: u64,
            minlen: u64,
        }
        const FITRIM: libc::c_ulong = 0xC018_5879; // _IOWR('X', 121, struct fstrim_range)
        let f = std::fs::File::open(dir)?;
        let mut range = Range { start: 0, len: u64::MAX, minlen: 0 };
        use std::os::fd::AsRawFd;
        // SAFETY: the ioctl reads and updates the struct we own.
        if unsafe { libc::ioctl(f.as_raw_fd(), FITRIM as _, &mut range) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(range.len)
    }

    /// Sectors written to `disk` so far and requests in flight
    /// (/sys/block/<disk>/stat).
    fn disk_activity(disk: &str) -> Option<(u64, u64)> {
        let name = disk.rsplit('/').next()?;
        let stat = std::fs::read_to_string(format!("/sys/block/{name}/stat")).ok()?;
        let f: Vec<u64> = stat.split_whitespace().filter_map(|v| v.parse().ok()).collect();
        Some((*f.get(6)?, *f.get(8)?))
    }

    /// Grow the data disk's filesystem to the disk (online, mounted) when
    /// the host made the disk image bigger; nothing otherwise.
    fn grow_filesystem(disk: &str) {
        let Some(name) = disk.rsplit('/').next() else { return };
        let Some(device) = std::fs::read_to_string(format!("/sys/block/{name}/size")).ok().and_then(|s| s.trim().parse::<u64>().ok()).map(|sectors| sectors * 512) else { return };
        let mut sb = [0u8; 1024];
        let read = std::fs::File::open(disk).and_then(|mut f| {
            use std::io::Seek;
            f.seek(std::io::SeekFrom::Start(1024))?;
            f.read_exact(&mut sb)
        });
        let Some(fs) = read.ok().and_then(|_| super::ext4_bytes(&sb)) else { return };
        if !super::should_grow(device, fs) {
            return;
        }
        let gib = |b: u64| b as f64 / (1u64 << 30) as f64;
        say(&format!("growing the data disk's filesystem from {:.1} to {:.1} GiB", gib(fs), gib(device)));
        if !run("resize2fs", &[disk]) {
            say("resize2fs failed: the data disk keeps its old size");
        }
    }

    /// Trim the data disk at boot and then whenever it has been quiet for
    /// a while (checked every 30 s, at most once a minute): ext4 only
    /// discards block groups freed since the last trim, so a trim with
    /// nothing new to give back costs next to nothing. Mounting with
    /// `discard` would do it on every delete instead, in the middle of
    /// image pulls and container churn.
    fn trim_when_idle(disk: String) {
        std::thread::sleep(Duration::from_secs(10));
        let mut last_trim: Option<Instant> = None;
        let mut last = disk_activity(&disk);
        loop {
            let now = disk_activity(&disk);
            // Under 8 MiB written in the last 30 s, nothing in flight.
            let quiet = match (last, now) {
                (Some((w0, _)), Some((w1, inflight))) => w1.saturating_sub(w0) < 16384 && inflight == 0,
                _ => true,
            };
            last = now;
            if quiet && last_trim.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                match trim(DATA_DIR) {
                    Ok(n) if n >= 64 << 20 && last_trim.is_some() => say(&format!("gave back {} MiB of deleted data on the disk", n >> 20)),
                    Ok(_) => {}
                    Err(e) => {
                        say(&format!("cannot trim {DATA_DIR} ({e}); disk.img keeps deleted data"));
                        return;
                    }
                }
                last_trim = Some(Instant::now());
            }
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    /// How long the memory watcher sleeps (ms) after `last`.
    fn interval(last: &Option<super::super::balloon::Report>) -> i32 {
        if last.is_some_and(|r| r.available < 512 << 20) { 250 } else { 2000 }
    }

    fn say(msg: &str) {
        let mut out = std::io::stdout();
        let _ = writeln!(out, "litek8s-guest: {msg}");
        let _ = out.flush();
    }

    fn cstr(s: &str) -> CString {
        CString::new(s).unwrap_or_default()
    }

    fn mount(src: &str, target: &str, fstype: &str, flags: libc::c_ulong, data: &str) -> std::io::Result<()> {
        let _ = std::fs::create_dir_all(target);
        let (s, t, f, d) = (cstr(src), cstr(target), cstr(fstype), cstr(data));
        // SAFETY: valid strings.
        let rc = unsafe { libc::mount(s.as_ptr(), t.as_ptr(), f.as_ptr(), flags, if data.is_empty() { std::ptr::null() } else { d.as_ptr().cast() }) };
        if rc != 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
    }

    fn run(program: &str, args: &[&str]) -> bool {
        match std::process::Command::new(program).args(args).status() {
            Ok(s) => s.success(),
            Err(e) => {
                say(&format!("{program}: {e}"));
                false
            }
        }
    }

    fn has_ext4(disk: &str) -> bool {
        let mut buf = [0u8; 2];
        std::fs::File::open(disk)
            .and_then(|mut f| {
                use std::io::Seek;
                f.seek(std::io::SeekFrom::Start(1024 + 0x38))?;
                f.read_exact(&mut buf)
            })
            .is_ok()
            && u16::from_le_bytes(buf) == 0xEF53
    }

    /// Get and configure a lease for eth0, retransmitting every 100 ms.
    fn dhcp(iface: &str) -> Option<Lease> {
        use std::os::fd::AsRawFd;
        let mac_text = std::fs::read_to_string(format!("/sys/class/net/{iface}/address")).ok()?;
        let mut mac = [0u8; 6];
        for (i, part) in mac_text.trim().split(':').enumerate().take(6) {
            mac[i] = u8::from_str_radix(part, 16).ok()?;
        }
        let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None).ok()?;
        sock.set_reuse_address(true).ok()?;
        sock.set_broadcast(true).ok()?;
        let name = cstr(iface);
        // SAFETY: SO_BINDTODEVICE with a NUL-terminated name.
        unsafe {
            libc::setsockopt(sock.as_raw_fd(), libc::SOL_SOCKET, libc::SO_BINDTODEVICE, name.as_ptr().cast(), iface.len() as libc::socklen_t + 1);
        }
        sock.bind(&std::net::SocketAddr::from(([0, 0, 0, 0], 68)).into()).ok()?;
        let sock: std::net::UdpSocket = sock.into();
        sock.set_read_timeout(Some(Duration::from_millis(100))).ok()?;
        let xid = u32::from_le_bytes(mac[2..6].try_into().ok()?) ^ std::process::id() ^ (Instant::now().elapsed().subsec_nanos());
        let dest = std::net::SocketAddr::from(([255, 255, 255, 255], 67));
        let mut buf = [0u8; 1500];
        let mut offer: Option<Lease> = None;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let packet = dhcp_packet(xid, mac, offer.as_ref().map(|o| (o.ip, o.server)));
            let _ = sock.send_to(&packet, dest);
            let until = Instant::now() + Duration::from_millis(100);
            while Instant::now() < until {
                let Ok((n, _)) = sock.recv_from(&mut buf) else { break };
                match parse_dhcp_reply(&buf[..n], xid) {
                    Some((2, lease)) if offer.is_none() => {
                        offer = Some(lease);
                        break;
                    }
                    Some((5, lease)) => {
                        let lease = Lease { server: offer.map(|o| o.server).unwrap_or(lease.server), ..lease };
                        run("ip", &["addr", "replace", &format!("{}/{}", lease.ip, lease.prefix), "dev", iface]);
                        if let Some(r) = lease.router {
                            run("ip", &["route", "replace", "default", "via", &r.to_string(), "dev", iface]);
                        }
                        let resolv: String = lease.dns.iter().map(|d| format!("nameserver {d}\n")).collect();
                        let _ = std::fs::write("/etc/resolv.conf", resolv);
                        return Some(lease);
                    }
                    Some((6, _)) => offer = None, // NAK: start over
                    _ => {}
                }
            }
        }
        None
    }

    /// Credit the host's seed to the entropy pool (RNDADDENTROPY).
    fn seed_entropy(seed: &[u8]) {
        #[repr(C)]
        struct PoolInfo {
            entropy_count: libc::c_int,
            buf_size: libc::c_int,
            buf: [u8; 64],
        }
        let mut info = PoolInfo { entropy_count: (seed.len().min(64) * 8) as libc::c_int, buf_size: seed.len().min(64) as libc::c_int, buf: [0; 64] };
        info.buf[..seed.len().min(64)].copy_from_slice(&seed[..seed.len().min(64)]);
        const RNDADDENTROPY: libc::c_ulong = 0x4008_5203;
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open("/dev/urandom") {
            use std::os::fd::AsRawFd;
            // SAFETY: the ioctl reads the struct we own.
            unsafe { libc::ioctl(f.as_raw_fd(), RNDADDENTROPY as _, &info) };
        }
    }

    fn eth0_ipv4() -> Option<(String, String)> {
        let out = std::process::Command::new("ip").args(["-4", "-o", "addr", "show", "eth0"]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let ip = text.split_whitespace().skip_while(|w| *w != "inet").nth(1)?.split('/').next()?.to_string();
        let route = std::process::Command::new("ip").args(["-4", "route", "show", "default"]).output().ok()?;
        let route = String::from_utf8_lossy(&route.stdout);
        let gw = route.split_whitespace().skip_while(|w| *w != "via").nth(1).unwrap_or_default().to_string();
        Some((ip, gw))
    }

    /// The cell manager: the clusters the Mac asks for, each in cells of
    /// its own.
    fn spawn_manager() -> Option<libc::pid_t> {
        let mut cmd = std::process::Command::new("/init");
        cmd.args(["dev", "__cells", "--guest"]);
        cmd.env("LITEK8S_DEV_GUEST", "1").env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin").env("HOME", "/root");
        cmd.env("LITEK8S_HOST_LOOPBACK", HOST_ADDRESS_FILE);
        match cmd.spawn() {
            Ok(child) => Some(child.id() as libc::pid_t),
            Err(e) => {
                say(&format!("failed to start the cell manager: {e}"));
                None
            }
        }
    }

    fn power_off(main: Option<libc::pid_t>) -> ! {
        // SAFETY: signals and reboot from PID 1.
        unsafe {
            if let Some(pid) = main {
                libc::kill(pid, libc::SIGTERM);
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline && libc::kill(pid, 0) == 0 {
                    while libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) > 0 {}
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            libc::kill(-1, libc::SIGTERM);
            std::thread::sleep(Duration::from_millis(300));
            libc::kill(-1, libc::SIGKILL);
            while libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) > 0 {}
            libc::sync();
            let d = cstr(DATA_DIR);
            libc::umount2(d.as_ptr(), libc::MNT_DETACH);
            libc::sync();
            say("halted");
            libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
        }
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    pub fn main() -> ! {
        let started = Instant::now();
        // SAFETY: single-threaded so far; the kernel starts init without PATH.
        unsafe {
            std::env::set_var("PATH", "/usr/sbin:/usr/bin:/sbin:/bin");
            std::env::set_var("HOME", "/root");
        }
        // The initramfs is the kernel's rootfs, which pivot_root (crun's
        // way into a container) refuses as the current root. A bind mount
        // of it moved over / and chrooted into is an ordinary mount.
        if let Err(e) = (|| -> std::io::Result<()> {
            mount("/", "/newroot", "", libc::MS_BIND | libc::MS_REC, "")?;
            std::env::set_current_dir("/newroot")?;
            mount("/newroot", "/", "", libc::MS_MOVE, "")?;
            std::os::unix::fs::chroot(".")?;
            std::env::set_current_dir("/")
        })() {
            say(&format!("cannot leave the initramfs root ({e}); containers will not start"));
        }
        let nosuid = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
        let _ = mount("proc", "/proc", "proc", nosuid, "");
        let _ = mount("sysfs", "/sys", "sysfs", nosuid, "");
        let _ = mount("devtmpfs", "/dev", "devtmpfs", libc::MS_NOSUID, "mode=0755");
        let _ = mount("devpts", "/dev/pts", "devpts", libc::MS_NOSUID | libc::MS_NOEXEC, "gid=5,mode=620,ptmxmode=666");
        let _ = mount("shm", "/dev/shm", "tmpfs", nosuid, "mode=1777");
        let _ = mount("tmpfs", "/run", "tmpfs", libc::MS_NOSUID | libc::MS_NODEV, "mode=755");
        let _ = mount("tmpfs", "/tmp", "tmpfs", libc::MS_NOSUID | libc::MS_NODEV, "mode=1777");
        let _ = mount("cgroup2", "/sys/fs/cgroup", "cgroup2", nosuid, "");
        // SAFETY: plain calls.
        unsafe {
            libc::sethostname(NODE_NAME.as_ptr().cast(), NODE_NAME.len());
            libc::setsid();
        }
        let opts = parse_cmdline(&std::fs::read_to_string("/proc/cmdline").unwrap_or_default());
        if let Some(seed) = &opts.seed {
            seed_entropy(seed);
        }
        let t_mounts = started.elapsed().as_millis();
        let _ = std::process::Command::new("/sbin/modprobe").arg("-qa").args(MODULES).status();
        let t_modules = started.elapsed().as_millis();
        let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");
        let _ = run("ip", &["link", "set", "lo", "up"]);
        let _ = run("ip", &["link", "set", "eth0", "up"]);
        // DHCP on a thread while the disk is prepared.
        let dhcp_thread = std::thread::spawn(|| dhcp("eth0"));
        let t_disk_start = started.elapsed().as_millis();
        if !has_ext4(&opts.disk) {
            say(&format!("formatting {} (first boot)", opts.disk));
            run("mkfs.ext4", &["-q", "-F", "-L", "litek8s", "-E", "lazy_itable_init=1,lazy_journal_init=1", &opts.disk]);
        }
        match mount(&opts.disk, DATA_DIR, "ext4", libc::MS_NOATIME, "") {
            Ok(()) => grow_filesystem(&opts.disk),
            Err(e) => say(&format!("cannot mount the data disk {}: {e}; using memory", opts.disk)),
        }
        if let Some(share) = &opts.share {
            if let Err(e) = mount("share", share, "virtiofs", 0, "") {
                say(&format!("cannot mount the shared directory at {share}: {e}"));
            }
        }
        for (tag, path) in &opts.mounts {
            if let Err(e) = mount(tag, path, "virtiofs", 0, "") {
                say(&format!("cannot mount {tag} at {path}: {e}"));
            }
        }
        if opts.rosetta {
            setup_rosetta();
        }
        for name in &opts.purge {
            let dir = format!("{DATA_DIR}/{name}");
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => say(&format!("deleted {dir} (cluster {name} was deleted)")),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => say(&format!("cannot delete {dir}: {e}")),
            }
        }
        let t_disk = started.elapsed().as_millis() - t_disk_start;
        if let Err(e) = serve_vsock() {
            say(&format!("no vsock ({e}); the host cannot reach the cluster"));
        }
        std::thread::spawn(watch_memory);
        if std::fs::read_to_string("/proc/mounts").is_ok_and(|m| m.lines().any(|l| l.split_whitespace().nth(1) == Some(DATA_DIR))) {
            let disk = opts.disk.clone();
            std::thread::spawn(move || trim_when_idle(disk));
        }
        let mut main = spawn_manager();
        say(&format!("booted in {} ms (mounts {t_mounts}, modules {}, disk+share {t_disk})", started.elapsed().as_millis(), t_modules - t_mounts));
        println!("LITEK8S_GUEST vsock={VSOCK_PORT}");
        let _ = std::io::stdout().flush();

        let lease = dhcp_thread.join().ok().flatten();
        let (ip, gw) = match &lease {
            Some(l) => (l.ip.to_string(), l.router.map(|r| r.to_string()).unwrap_or_default()),
            None => {
                say("no DHCP lease; trying udhcpc");
                run("/bin/busybox", &["udhcpc", "-i", "eth0", "-q", "-n", "-t", "10", "-s", "/usr/share/udhcpc/default.script"]);
                eth0_ipv4().unwrap_or_default()
            }
        };
        let _ = std::fs::write(HOST_ADDRESS_FILE, &gw);
        // busybox renews the lease from here on (the hook leaves an
        // unchanged address alone).
        if !ip.is_empty() {
            let _ = std::process::Command::new("/bin/busybox")
                .args(["udhcpc", "-i", "eth0", "-f", "-S", "-r", &ip, "-s", "/usr/share/udhcpc/default.script"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
        say(&format!("network up at {} ms", started.elapsed().as_millis()));
        println!("LITEK8S_GUEST ip={ip} gw={gw}");
        let _ = std::io::stdout().flush();

        let mut restarts = 0u32;
        // Console commands from the host, read on a thread; PID 1 reaps.
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        loop {
            if let Ok(line) = rx.recv_timeout(Duration::from_secs(1)) {
                if line.trim() == "poweroff" {
                    say("powering off");
                    power_off(main);
                }
            }
            loop {
                let mut status = 0;
                // SAFETY: reaping children of PID 1.
                let r = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
                if r <= 0 {
                    break;
                }
                if Some(r) == main {
                    restarts += 1;
                    say(&format!("cell manager exited ({status}); restarting ({restarts})"));
                    std::thread::sleep(Duration::from_secs(1));
                    main = spawn_manager();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_boot_options() {
        let o = parse_cmdline("console=hvc0 quiet litek8s.share=/Users/me litek8s.disk=/dev/vdb litek8s.seed=00ff");
        assert_eq!(o, BootOptions { share: Some("/Users/me".into()), disk: "/dev/vdb".into(), seed: Some(vec![0, 255]), ..Default::default() });
        let o = parse_cmdline("litek8s.mounts=tmp:/private/tmp,folders:/private/var/folders litek8s.rosetta=1 litek8s.purge=ci,../x,b");
        assert_eq!(o.purge, vec!["ci".to_string(), "b".to_string()]);
        assert_eq!(o.mounts, vec![("tmp".to_string(), "/private/tmp".to_string()), ("folders".to_string(), "/private/var/folders".to_string())]);
        assert!(o.rosetta);
        assert_eq!(parse_cmdline("console=hvc0").disk, "/dev/vda");
    }

    #[test]
    fn reads_the_filesystem_size_from_its_superblock() {
        let mut sb = [0u8; 1024];
        assert_eq!(ext4_bytes(&sb), None, "not ext4");
        sb[0x38..0x3a].copy_from_slice(&[0x53, 0xef]);
        // 4 KiB blocks (log 2), 16 Mi blocks: 64 GiB.
        sb[0x18..0x1c].copy_from_slice(&2u32.to_le_bytes());
        sb[0x04..0x08].copy_from_slice(&(16u32 << 20).to_le_bytes());
        assert_eq!(ext4_bytes(&sb), Some(64 << 30));
        // The high half counts only with the 64bit feature.
        sb[0x150..0x154].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(ext4_bytes(&sb), Some(64 << 30));
        sb[0x60..0x64].copy_from_slice(&0x80u32.to_le_bytes());
        assert_eq!(ext4_bytes(&sb), Some((1u64 << 32 | 16 << 20) * 4096));
        assert!(should_grow(128 << 30, 64 << 30));
        assert!(!should_grow(64 << 30, 64 << 30) && !should_grow((64 << 30) + (64 << 20), 64 << 30));
    }

    #[test]
    fn dhcp_round_trip() {
        let mac = [2, 0, 0, 0, 0, 1];
        let d = dhcp_packet(7, mac, None);
        assert_eq!((d[0], d[10], &d[28..34]), (1, 0x80, &mac[..]));
        assert_eq!(&d[240..243], &[53, 1, 1]);
        // An ACK as vmnet sends it.
        let mut ack = vec![0u8; 236];
        ack[0] = 2;
        ack[4..8].copy_from_slice(&7u32.to_be_bytes());
        ack[16..20].copy_from_slice(&[192, 168, 64, 3]);
        ack.extend_from_slice(&DHCP_MAGIC);
        ack.extend_from_slice(&[53, 1, 5, 1, 4, 255, 255, 255, 0, 3, 4, 192, 168, 64, 1, 6, 4, 192, 168, 64, 1, 54, 4, 192, 168, 64, 1, 255]);
        let (kind, lease) = parse_dhcp_reply(&ack, 7).unwrap();
        assert_eq!(kind, 5);
        assert_eq!(lease.ip.to_string(), "192.168.64.3");
        assert_eq!((lease.prefix, lease.router.unwrap().to_string()), (24, "192.168.64.1".to_string()));
        assert!(parse_dhcp_reply(&ack, 8).is_none());
        let r = dhcp_packet(7, mac, Some((lease.ip, lease.server)));
        assert_eq!(&r[240..248], &[53, 1, 3, 50, 4, 192, 168, 64]);
    }
}
