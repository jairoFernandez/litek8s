//! PID 1 of the macOS dev VM (`/init` in its initramfs is this binary).
//!
//! The VM is a kernel, an initramfs (Alpine's crun, iptables, iproute2 and
//! busybox around this binary) and a data disk. Init mounts the usual
//! filesystems, loads the kernel modules the cluster needs, formats the data
//! disk on first boot, mounts the Mac's shared directory (virtiofs) at the
//! same path as on the Mac, gets an address by DHCP and runs the API server
//! with a crun node as a child, reaping orphans and restarting it if it
//! dies. It talks to the host over the console (hvc0): it reports
//! `LITEK8S_GUEST ip=...` once the network is up and powers off on a
//! `poweroff` line from the host.

/// The dev cluster's node name (the sandbox's hostname on Linux, the VM's
/// on macOS).
pub const NODE_NAME: &str = "litek8s-dev";

/// The API server port inside the VM.
pub const GUEST_API_PORT: u16 = 6443;

/// Where the data disk is mounted in the VM.
pub const DATA_DIR: &str = "/var/lib/litek8s";

/// Kernel command line options init understands.
#[derive(Debug, Default, PartialEq)]
pub struct BootOptions {
    /// Mount the `share` virtiofs tag here (the Mac's path).
    pub share: Option<String>,
    /// Data disk device.
    pub disk: String,
}

pub fn parse_cmdline(cmdline: &str) -> BootOptions {
    let mut o = BootOptions { disk: "/dev/vda".into(), ..Default::default() };
    for word in cmdline.split_whitespace() {
        if let Some(v) = word.strip_prefix("litek8s.share=") {
            o.share = Some(v.to_string());
        } else if let Some(v) = word.strip_prefix("litek8s.disk=") {
            o.disk = v.to_string();
        }
    }
    o
}

#[cfg(target_os = "linux")]
pub use init::main;

#[cfg(target_os = "linux")]
mod init {
    use std::ffi::CString;
    use std::io::{BufRead, Read, Write};
    use std::time::{Duration, Instant};

    use super::*;

    /// Modules loaded up front; netfilter's come on demand through
    /// /sbin/modprobe (busybox) when iptables asks for them.
    const MODULES: &[&str] = &[
        "virtio_net", "virtio_blk", "virtiofs", "overlay", "ext4", "bridge", "veth", "br_netfilter", "nf_conntrack", "nf_nat",
        "xt_MASQUERADE", "xt_addrtype", "xt_statistic", "xt_conntrack", "xt_comment", "xt_mark", "xt_tcpudp", "nft_compat",
        "nft_chain_nat", "iptable_nat", "iptable_filter", "hw_random_virtio", "virtio_balloon",
    ];

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

    fn eth0_ipv4() -> Option<(String, String)> {
        let out = std::process::Command::new("ip").args(["-4", "-o", "addr", "show", "eth0"]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let ip = text.split_whitespace().skip_while(|w| *w != "inet").nth(1)?.split('/').next()?.to_string();
        let route = std::process::Command::new("ip").args(["-4", "route", "show", "default"]).output().ok()?;
        let route = String::from_utf8_lossy(&route.stdout);
        let gw = route.split_whitespace().skip_while(|w| *w != "via").nth(1).unwrap_or_default().to_string();
        Some((ip, gw))
    }

    fn spawn_apiserver(gateway: &str) -> Option<libc::pid_t> {
        let mut cmd = std::process::Command::new("/init");
        cmd.args(["dev", "__apiserver", "--data-dir", &format!("{DATA_DIR}/dev")]);
        cmd.env("LITEK8S_DEV_GUEST", "1").env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin").env("HOME", "/root");
        if !gateway.is_empty() {
            cmd.env("LITEK8S_HOST_LOOPBACK", gateway);
        }
        match cmd.spawn() {
            Ok(child) => Some(child.id() as libc::pid_t),
            Err(e) => {
                say(&format!("failed to start the API server: {e}"));
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
        for m in MODULES {
            let _ = std::process::Command::new("/sbin/modprobe").arg("-q").arg(m).status();
        }
        let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");
        let _ = run("ip", &["link", "set", "lo", "up"]);
        let _ = run("ip", &["link", "set", "eth0", "up"]);
        // DHCP in the background while the disk is prepared.
        let dhcp = std::process::Command::new("/bin/busybox")
            .args(["udhcpc", "-i", "eth0", "-q", "-n", "-t", "20", "-T", "1", "-s", "/usr/share/udhcpc/default.script"])
            .stdout(std::process::Stdio::null())
            .spawn();
        if !has_ext4(&opts.disk) {
            say(&format!("formatting {} (first boot)", opts.disk));
            run("mkfs.ext4", &["-q", "-F", "-L", "litek8s", "-E", "lazy_itable_init=1,lazy_journal_init=1", &opts.disk]);
        }
        if let Err(e) = mount(&opts.disk, DATA_DIR, "ext4", libc::MS_NOATIME, "") {
            say(&format!("cannot mount the data disk {}: {e}; using memory", opts.disk));
        }
        if let Some(share) = &opts.share {
            if let Err(e) = mount("share", share, "virtiofs", 0, "") {
                say(&format!("cannot mount the shared directory at {share}: {e}"));
            }
        }
        if let Ok(mut child) = dhcp {
            let _ = child.wait();
        }
        let (ip, gw) = eth0_ipv4().unwrap_or_default();
        say(&format!("booted in {} ms", started.elapsed().as_millis()));
        println!("LITEK8S_GUEST ip={ip} gw={gw}");
        let _ = std::io::stdout().flush();

        let mut main = spawn_apiserver(&gw);
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
                    say(&format!("API server exited ({status}); restarting ({restarts})"));
                    std::thread::sleep(Duration::from_secs(1));
                    main = spawn_apiserver(&gw);
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
        let o = parse_cmdline("console=hvc0 quiet litek8s.share=/Users/me litek8s.disk=/dev/vdb");
        assert_eq!(o, BootOptions { share: Some("/Users/me".into()), disk: "/dev/vdb".into() });
        assert_eq!(parse_cmdline("console=hvc0").disk, "/dev/vda");
    }
}
