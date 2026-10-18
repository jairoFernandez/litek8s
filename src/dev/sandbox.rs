//! The Linux sandbox the dev cluster runs in.
//!
//! Everything the cluster creates lives in namespaces of its own, so the
//! host's network, iptables rules, /run and cgroup tree are never touched:
//!
//! - user namespace (rootless only): the invoking user is root inside, the
//!   user's /etc/subuid range backs the other ids (via the setuid
//!   `newuidmap`/`newgidmap` helpers), so image layers keep their owners;
//! - network namespace: the `litek8s0` bridge, pod veths and every iptables
//!   rule (pod NAT, Service DNAT) are in it; egress goes through `pasta`
//!   (or `slirp4netns`), a userspace TCP/IP stack on the host side;
//! - mount namespace: a fresh /run (pod netns files, crun state), sysfs and a
//!   cgroup2 mount rooted at the sandbox's delegated cgroup;
//! - PID namespace: PID 1 is a small init here, so stopping it stops every
//!   pod process; UTS and IPC namespaces too.
//!
//! The host side ([`super::supervise`]) keeps the API listener (bound on
//! the host's loopback before the sandbox exists, so kubectl reaches it
//! without any forwarding) and a socket over which the cluster asks for more
//! host listeners (NodePorts; see [`super::broker`]).
//!
//! Process tree, all of it this binary:
//!
//! ```text
//! litek8s dev supervise          host namespaces, delegated cgroup .../supervisor
//!  ├─ pasta                      egress for the sandbox network namespace
//!  └─ litek8s dev-sandbox        "stage 1": unshares, forks PID 1, waits
//!      └─ litek8s dev-sandbox    PID 1: mounts, then runs and reaps
//!          └─ litek8s dev __apiserver   API server, controllers, crun node agent
//! ```
//!
//! Stage 1 and PID 1 run before (and without) the async runtime: unsharing a
//! user namespace needs a single-threaded process.

use std::ffi::CString;
use std::io::{BufRead, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::path::{Path, PathBuf};

/// File descriptors the supervisor hands to stage 1 (and on to the API
/// server process).
pub const FD_CONTROL_IN: RawFd = 3;
pub const FD_CONTROL_OUT: RawFd = 4;
pub const FD_API_LISTENER: RawFd = 5;
pub const FD_BROKER: RawFd = 6;

/// Hostname (and node name) inside the sandbox.
pub const HOSTNAME: &str = "litek8s-dev";

/// The nameserver pasta forwards to the host's resolver when the host only
/// has a loopback stub (systemd-resolved's 127.0.0.53).
pub const DNS_FORWARD: &str = "169.254.1.1";

fn fail(msg: &str) -> ! {
    eprintln!("dev-sandbox: {msg}: {}", std::io::Error::last_os_error());
    std::process::exit(111)
}

fn check(rc: libc::c_int, msg: &str) {
    if rc != 0 {
        fail(msg);
    }
}

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

fn mount(source: &str, target: &str, fstype: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> std::io::Result<()> {
    let (s, t) = (cstr(source), cstr(target));
    let f = fstype.map(cstr);
    let d = data.map(cstr);
    // SAFETY: valid NUL-terminated strings or null pointers.
    let rc = unsafe {
        libc::mount(
            s.as_ptr(),
            t.as_ptr(),
            f.as_ref().map_or(std::ptr::null(), |f| f.as_ptr()),
            flags,
            d.as_ref().map_or(std::ptr::null(), |d| d.as_ptr().cast()),
        )
    };
    if rc != 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

/// `litek8s dev-sandbox <args...>`: stage 1. `args` is the command PID 1
/// runs once the sandbox is set up (the API server). With `--rm <dir>` it
/// only enters the user namespace and removes `dir` (files owned by
/// subordinate ids cannot be removed from outside).
pub fn main(args: &[String]) -> ! {
    // SAFETY: inherited descriptors, owned from here on.
    let mut input = std::io::BufReader::new(unsafe { std::fs::File::from_raw_fd(FD_CONTROL_IN) });
    let mut output = unsafe { std::fs::File::from_raw_fd(FD_CONTROL_OUT) };
    let rootless = std::env::var("LITEK8S_DEV_ROOTLESS").is_ok_and(|v| v == "1");
    let mut line = String::new();

    if rootless {
        // SAFETY: single-threaded process.
        check(unsafe { libc::unshare(libc::CLONE_NEWUSER) }, "failed to create a user namespace");
    }
    // The supervisor writes the id maps and moves us into the sandbox cgroup.
    let _ = writeln!(output, "U");
    if input.read_line(&mut line).unwrap_or(0) == 0 || line.trim() != "M" {
        eprintln!("dev-sandbox: supervisor did not map the user namespace ({})", line.trim());
        std::process::exit(111);
    }
    if rootless {
        // SAFETY: plain id calls; root in the namespace now that it is mapped.
        unsafe {
            if libc::setresgid(0, 0, 0) != 0 || libc::setresuid(0, 0, 0) != 0 {
                fail("failed to become root in the user namespace");
            }
            libc::setgroups(0, std::ptr::null());
        }
    }
    if args.first().map(String::as_str) == Some("--rm") {
        let mut code = 0;
        for dir in &args[1..] {
            if let Err(e) = std::fs::remove_dir_all(dir) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("dev-sandbox: failed to remove {dir}: {e}");
                    code = 1;
                }
            }
        }
        std::process::exit(code);
    }

    let flags = libc::CLONE_NEWNS | libc::CLONE_NEWNET | libc::CLONE_NEWCGROUP | libc::CLONE_NEWPID | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC;
    // SAFETY: single-threaded process.
    check(unsafe { libc::unshare(flags) }, "failed to create the sandbox namespaces");
    // SAFETY: single-threaded; the child continues as PID 1.
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => fail("fork failed"),
        0 => pid1(input, output, args),
        child => {
            let _ = writeln!(output, "P {child}");
            drop(output);
            drop(input);
            wait_forwarding(child)
        }
    }
}

/// Block the termination signals and wait for `child`, passing SIGTERM and
/// SIGINT on to it; exit with its status.
fn wait_forwarding(child: libc::pid_t) -> ! {
    // SAFETY: signal mask and waiting on this single-threaded process.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGCHLD, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
        libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        loop {
            let mut status = 0;
            let r = libc::waitpid(child, &mut status, libc::WNOHANG);
            if r == child {
                let code = if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 128 + libc::WTERMSIG(status) };
                std::process::exit(code);
            }
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let sig = libc::sigwaitinfo(&set, &mut info);
            if matches!(sig, libc::SIGTERM | libc::SIGINT | libc::SIGHUP) {
                libc::kill(child, libc::SIGTERM);
            }
        }
    }
}

/// The resolver configuration pods and the API server use inside the
/// sandbox: the host's upstream servers (loopback stubs do not exist
/// there), or pasta's DNS forwarder.
fn sandbox_resolv_conf() -> String {
    let host = ["/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    super::hostinfo::resolv_conf_without_loopback(&host, DNS_FORWARD)
}

fn pid1(mut input: std::io::BufReader<std::fs::File>, mut output: std::fs::File, args: &[String]) -> ! {
    let resolv = sandbox_resolv_conf();
    let report = |output: &mut std::fs::File, what: &str, e: std::io::Error| {
        let _ = writeln!(output, "E {what}: {e}");
        std::process::exit(111);
    };
    if let Err(e) = mount("none", "/", None, libc::MS_REC | libc::MS_PRIVATE, None) {
        report(&mut output, "making / private", e);
    }
    let nosuid = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    if let Err(e) = mount("proc", "/proc", Some("proc"), nosuid, None) {
        report(&mut output, "mounting /proc", e);
    }
    if let Err(e) = mount("sysfs", "/sys", Some("sysfs"), nosuid, None) {
        report(&mut output, "mounting /sys", e);
    }
    if let Err(e) = mount("cgroup2", "/sys/fs/cgroup", Some("cgroup2"), nosuid, None) {
        report(&mut output, "mounting the sandbox cgroup", e);
    }
    if let Err(e) = mount("tmpfs", "/run", Some("tmpfs"), libc::MS_NOSUID | libc::MS_NODEV, Some("mode=755")) {
        report(&mut output, "mounting /run", e);
    }
    let _ = std::fs::create_dir_all("/run/systemd/resolve");
    let _ = std::fs::write("/run/systemd/resolve/resolv.conf", &resolv);
    let _ = std::fs::write("/run/systemd/resolve/stub-resolv.conf", &resolv);
    // /etc/resolv.conf itself, when it is not a link into /run.
    if std::fs::read_link("/etc/resolv.conf").is_err() {
        let _ = std::fs::write("/run/resolv.conf", &resolv);
        let _ = mount("/run/resolv.conf", "/etc/resolv.conf", None, libc::MS_BIND, None);
    }
    if let Err(e) = mount("shm", "/dev/shm", Some("tmpfs"), nosuid, Some("mode=1777,size=65536k")) {
        let _ = writeln!(output, "W /dev/shm: {e}");
    }
    // cgroup v2 allows processes only in leaves: PID 1 (and so the API
    // server) moves to /init, and the root hands its controllers down.
    let cg = Path::new("/sys/fs/cgroup");
    let _ = std::fs::create_dir_all(cg.join("init"));
    if let Err(e) = std::fs::write(cg.join("init/cgroup.procs"), "0") {
        report(&mut output, "joining the sandbox cgroup", e);
    }
    let controllers = std::fs::read_to_string(cg.join("cgroup.controllers")).unwrap_or_default();
    let enable: Vec<String> = controllers.split_whitespace().map(|c| format!("+{c}")).collect();
    if let Err(e) = std::fs::write(cg.join("cgroup.subtree_control"), enable.join(" ")) {
        let _ = writeln!(output, "W enabling cgroup controllers {}: {e}", enable.join(" "));
    }
    // SAFETY: plain call with a valid buffer.
    unsafe { libc::sethostname(HOSTNAME.as_ptr().cast(), HOSTNAME.len()) };
    loopback_up();
    let _ = writeln!(output, "S");

    // Network ready (pasta attached) before the API server starts: it picks
    // its node address and upstream DNS at startup.
    let mut line = String::new();
    if input.read_line(&mut line).unwrap_or(0) == 0 || line.trim() != "R" {
        std::process::exit(111);
    }
    drop(input);
    drop(output);
    if let Some(gw) = default_gateway() {
        // SAFETY: still single-threaded.
        unsafe { std::env::set_var("LITEK8S_HOST_LOOPBACK", gw) };
    }

    let program = cstr("/proc/self/exe");
    let argv: Vec<CString> = std::iter::once("litek8s".to_string()).chain(args.iter().cloned()).map(|a| cstr(&a)).collect();
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    // SAFETY: fork in a single-threaded process; the child only execs.
    let child = unsafe { libc::fork() };
    match child {
        -1 => fail("fork failed"),
        0 => unsafe {
            libc::execv(program.as_ptr(), argv_ptrs.as_ptr());
            fail("failed to start the API server")
        },
        _ => {}
    }
    init_loop(child)
}

/// PID 1's job: reap everything, forward SIGTERM to the API server, and
/// leave (taking the whole namespace with it) when the API server exits.
fn init_loop(main: libc::pid_t) -> ! {
    // SAFETY: signal mask and waiting on this single-threaded process.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGCHLD, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
        libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        let mut deadline: Option<std::time::Instant> = None;
        loop {
            loop {
                let mut status = 0;
                let r = libc::waitpid(-1, &mut status, libc::WNOHANG);
                if r <= 0 {
                    break;
                }
                if r == main {
                    let code = if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 128 + libc::WTERMSIG(status) };
                    std::process::exit(code);
                }
            }
            let timeout = libc::timespec { tv_sec: 1, tv_nsec: 0 };
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let sig = libc::sigtimedwait(&set, &mut info, &timeout);
            if matches!(sig, libc::SIGTERM | libc::SIGINT | libc::SIGHUP) && deadline.is_none() {
                libc::kill(main, libc::SIGTERM);
                deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(10));
            }
            if deadline.is_some_and(|d| std::time::Instant::now() > d) {
                std::process::exit(137);
            }
        }
    }
}

fn loopback_up() {
    // SAFETY: a plain socket for two ioctls on "lo".
    unsafe {
        let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if sock < 0 {
            return;
        }
        let mut req: libc::ifreq = std::mem::zeroed();
        req.ifr_name[0] = b'l' as libc::c_char;
        req.ifr_name[1] = b'o' as libc::c_char;
        if libc::ioctl(sock, libc::SIOCGIFFLAGS as _, &mut req) == 0 {
            req.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
            libc::ioctl(sock, libc::SIOCSIFFLAGS as _, &mut req);
        }
        libc::close(sock);
    }
}

/// The default IPv4 gateway (pasta and slirp4netns map it to the host's
/// loopback).
fn default_gateway() -> Option<String> {
    super::hostinfo::parse_default_gateway(&std::fs::read_to_string("/proc/net/route").ok()?)
}

// ---------------------------------------------------------------------------
// Host side helpers (used by the supervisor)

/// The user's subordinate id range from /etc/subuid or /etc/subgid.
pub fn subordinate_range(file: &str, user: &str, uid: u32) -> Option<(u32, u32)> {
    super::hostinfo::parse_subordinate(&std::fs::read_to_string(file).ok()?, user, uid)
}

/// The user name of `uid`, from /etc/passwd (falling back to $USER).
pub fn user_name(uid: u32) -> String {
    std::fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|p| {
            p.lines().find_map(|l| {
                let f: Vec<&str> = l.split(':').collect();
                (f.len() > 2 && f[2] == uid.to_string()).then(|| f[0].to_string())
            })
        })
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_default()
}

/// Read one control line from stage 1 / PID 1, skipping warnings.
pub fn read_control(reader: &mut impl BufRead) -> std::io::Result<String> {
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "sandbox exited during setup"));
        }
        let line = line.trim().to_string();
        if let Some(w) = line.strip_prefix("W ") {
            eprintln!("dev: warning: {w}");
            continue;
        }
        if let Some(e) = line.strip_prefix("E ") {
            return Err(std::io::Error::other(format!("sandbox setup failed: {e}")));
        }
        return Ok(line);
    }
}

/// Whether Ubuntu's AppArmor restriction on unprivileged user namespaces
/// is on (then litek8s needs a profile; see `litek8s dev setup`).
pub fn apparmor_restricts_userns() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns").is_ok_and(|v| v.trim() == "1")
}

/// The cgroup this process is in, as a path under /sys/fs/cgroup.
pub fn own_cgroup() -> Option<PathBuf> {
    let content = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = content.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/')))
}
