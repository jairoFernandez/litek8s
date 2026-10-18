//! The host side of the Linux dev sandbox (`litek8s dev supervise`): binds
//! the API port on the host's loopback, prepares the cgroups, maps the user
//! namespace, attaches the userspace network, then waits for the sandbox
//! and tears it down on SIGTERM. See [`super::sandbox`] for the inside.

use std::io::{BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::sandbox::{self, FD_API_LISTENER, FD_BROKER, FD_CONTROL_IN, FD_CONTROL_OUT};
use super::{DEFAULT_PORT, DevState, dev_home, now_ms, which, write_state};

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: pipe2 fills both descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: just created, owned here.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Make `sources[i]` descriptor `targets[i]` in the child (no CLOEXEC),
/// going through high numbers first so sources and targets may overlap.
fn remap_fds(cmd: &mut std::process::Command, sources: Vec<RawFd>, targets: Vec<RawFd>) {
    // SAFETY: only async-signal-safe calls (fcntl, dup2) after fork.
    unsafe {
        cmd.pre_exec(move || {
            let mut tmp = Vec::with_capacity(sources.len());
            for &s in &sources {
                let t = libc::fcntl(s, libc::F_DUPFD_CLOEXEC, 100);
                if t < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                tmp.push(t);
            }
            for (&t, &target) in tmp.iter().zip(&targets) {
                if libc::dup2(t, target) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

fn is_root() -> bool {
    // SAFETY: no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Write the user and group id maps of `pid`'s new user namespace: our
/// own id becomes root, the subordinate range backs ids 1..65536.
fn map_ids(pid: u32) -> Result<()> {
    // SAFETY: no preconditions.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let user = sandbox::user_name(uid);
    for (tool, file, id) in [("newuidmap", "/etc/subuid", uid), ("newgidmap", "/etc/subgid", gid)] {
        let (start, count) = sandbox::subordinate_range(file, &user, uid).with_context(|| format!("{file} has no range for {user}"))?;
        let count = count.min(65536);
        let out = std::process::Command::new(tool)
            .args([pid.to_string(), "0".into(), id.to_string(), "1".into(), "1".into(), start.to_string(), count.to_string()])
            .output()
            .with_context(|| format!("failed to run {tool}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if sandbox::apparmor_restricts_userns() {
                bail!("{tool} failed ({err}): AppArmor restricts user namespaces; run `sudo litek8s dev setup` once");
            }
            bail!("{tool} failed: {err}");
        }
    }
    Ok(())
}

/// The cgroups the cluster lives in: `<base>/supervisor` for this process
/// and its helpers, `<base>/sandbox` as the root of the sandbox's cgroup
/// namespace. Rootless, `<base>` is the systemd scope `dev up` started us
/// in (delegated to the user); as root, /sys/fs/cgroup/litek8s-dev.
fn setup_cgroups(rootless: bool) -> Result<PathBuf> {
    let base = if rootless {
        sandbox::own_cgroup().context("cannot find this process's cgroup (cgroup v2 needed)")?
    } else {
        let b = PathBuf::from("/sys/fs/cgroup/litek8s-dev");
        std::fs::create_dir_all(&b).with_context(|| format!("failed to create {}", b.display()))?;
        b
    };
    let sup = base.join("supervisor");
    std::fs::create_dir_all(&sup)
        .and_then(|_| std::fs::write(sup.join("cgroup.procs"), std::process::id().to_string()))
        .with_context(|| {
            format!(
                "cannot manage cgroup {} (rootless needs a delegated cgroup: run `litek8s dev up` in a systemd user session)",
                base.display()
            )
        })?;
    let controllers = std::fs::read_to_string(base.join("cgroup.controllers")).unwrap_or_default();
    let enable: Vec<String> = controllers.split_whitespace().map(|c| format!("+{c}")).collect();
    if let Err(e) = std::fs::write(base.join("cgroup.subtree_control"), enable.join(" ")) {
        eprintln!("dev: could not enable cgroup controllers {}: {e}", enable.join(" "));
    }
    std::fs::create_dir_all(base.join("sandbox"))?;
    Ok(base)
}

/// Remove a cgroup tree (children first); the processes must be gone.
fn remove_cgroup(dir: &Path) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            remove_cgroup(&e.path());
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// The userspace network of the sandbox's network namespace.
enum Network {
    Pasta { pid_file: PathBuf, netns_file: Option<PathBuf> },
    Slirp { child: std::process::Child, _exit: OwnedFd },
    None,
}

impl Network {
    fn attach(pid1: u32, rootless: bool, home: &Path) -> Result<Self> {
        if let Some(pasta) = which("pasta") {
            let pid_file = home.join("pasta.pid");
            let _ = std::fs::remove_file(&pid_file);
            let mut cmd = std::process::Command::new(pasta);
            cmd.args(["--config-net", "--quiet", "-t", "none", "-u", "none", "-T", "none", "-U", "none"])
                .args(["--dns-forward", sandbox::DNS_FORWARD])
                .arg("--pid")
                .arg(&pid_file);
            let mut netns_file = None;
            if rootless {
                // pasta joins the sandbox's user and network namespaces.
                cmd.arg(pid1.to_string());
            } else {
                // As root pasta drops its privileges first and may then not
                // open /proc/<pid>/ns/net; a bind mount of it it can.
                let file = home.join("netns");
                bind_netns(pid1, &file)?;
                cmd.args(["--runas", "0:0", "--netns"]).arg(&file);
                netns_file = Some(file);
            }
            let out = cmd.output().context("failed to run pasta")?;
            if !out.status.success() {
                bail!("pasta failed: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            return Ok(Network::Pasta { pid_file, netns_file });
        }
        if let Some(slirp) = which("slirp4netns") {
            let (exit_r, exit_w) = pipe()?;
            let (ready_r, ready_w) = pipe()?;
            let mut cmd = std::process::Command::new(slirp);
            cmd.args(["--configure", "--mtu=65520", "--exit-fd=3", "--ready-fd=4"]);
            if rootless {
                cmd.arg(format!("--userns-path=/proc/{pid1}/ns/user"));
            }
            cmd.arg(pid1.to_string()).arg("tap0");
            cmd.stdout(std::process::Stdio::null());
            remap_fds(&mut cmd, vec![exit_r.as_raw_fd(), ready_w.as_raw_fd()], vec![3, 4]);
            let child = cmd.spawn().context("failed to run slirp4netns")?;
            drop((exit_r, ready_w));
            let mut ready = [0u8; 1];
            let mut f = std::fs::File::from(ready_r);
            use std::io::Read;
            if f.read(&mut ready)? != 1 {
                bail!("slirp4netns did not get ready");
            }
            return Ok(Network::Slirp { child, _exit: exit_w });
        }
        eprintln!("dev: neither pasta nor slirp4netns found; pods have no network outside the cluster");
        Ok(Network::None)
    }

    fn stop(self) {
        match self {
            Network::Pasta { pid_file, netns_file } => {
                if let Some(pid) = std::fs::read_to_string(&pid_file).ok().and_then(|p| p.trim().parse::<i32>().ok()) {
                    // SAFETY: plain signal.
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                }
                let _ = std::fs::remove_file(pid_file);
                if let Some(file) = netns_file {
                    unbind_netns(&file);
                }
            }
            Network::Slirp { mut child, _exit } => {
                drop(_exit);
                let _ = child.kill();
                let _ = child.wait();
            }
            Network::None => {}
        }
    }
}

/// Bind-mount `pid`'s network namespace at `file` (root mode).
fn bind_netns(pid: u32, file: &Path) -> Result<()> {
    unbind_netns(file);
    std::fs::File::create(file)?;
    let (src, dst) = (std::ffi::CString::new(format!("/proc/{pid}/ns/net"))?, std::ffi::CString::new(file.as_os_str().as_encoded_bytes())?);
    // SAFETY: valid strings.
    if unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to bind the sandbox network namespace");
    }
    Ok(())
}

fn unbind_netns(file: &Path) {
    if let Ok(c) = std::ffi::CString::new(file.as_os_str().as_encoded_bytes()) {
        // SAFETY: valid string; failure (not mounted) is fine.
        unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
    }
    let _ = std::fs::remove_file(file);
}

fn bind_api(port: Option<u16>) -> Result<std::net::TcpListener> {
    let want = port.unwrap_or(DEFAULT_PORT);
    match std::net::TcpListener::bind(("127.0.0.1", want)) {
        Ok(l) => Ok(l),
        Err(e) if port.is_none() && e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("dev: port {want} is taken, using a free one");
            Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?)
        }
        Err(e) => Err(e).with_context(|| format!("failed to listen on 127.0.0.1:{want}")),
    }
}

pub async fn run(port: Option<u16>) -> Result<()> {
    let home = dev_home()?;
    let data = home.join("cluster");
    std::fs::create_dir_all(&data)?;
    let rootless = !is_root();
    let started = now_ms();
    let listener = bind_api(port)?;
    let port = listener.local_addr()?.port();
    let cgroup = setup_cgroups(rootless)?;

    let (in_r, in_w) = pipe()?;
    let (out_r, out_w) = pipe()?;
    let (broker_host, broker_sandbox) = std::os::unix::net::UnixStream::pair()?;
    let mut cmd = std::process::Command::new("/proc/self/exe");
    cmd.arg("dev-sandbox").args(["dev", "__apiserver", "--data-dir"]).arg(&data);
    cmd.env("LITEK8S_DEV_ROOTLESS", if rootless { "1" } else { "0" });
    cmd.stdin(std::process::Stdio::null());
    remap_fds(
        &mut cmd,
        vec![in_r.as_raw_fd(), out_w.as_raw_fd(), listener.as_raw_fd(), broker_sandbox.as_raw_fd()],
        vec![FD_CONTROL_IN, FD_CONTROL_OUT, FD_API_LISTENER, FD_BROKER],
    );
    let mut stage1 = cmd.spawn().context("failed to start the sandbox")?;
    drop((in_r, out_w, listener, broker_sandbox));
    let stage1_pid = stage1.id();

    let setup_home = home.clone();
    let cg = cgroup.clone();
    let setup = tokio::task::spawn_blocking(move || -> Result<(u32, Network)> {
        let mut input = std::fs::File::from(in_w);
        let mut output = BufReader::new(std::fs::File::from(out_r));
        if sandbox::read_control(&mut output)? != "U" {
            bail!("unexpected sandbox handshake");
        }
        if rootless {
            map_ids(stage1_pid)?;
        }
        std::fs::write(cg.join("sandbox/cgroup.procs"), stage1_pid.to_string()).context("failed to move the sandbox into its cgroup")?;
        writeln!(input, "M")?;
        let (mut pid1, mut mounted) = (None, false);
        while pid1.is_none() || !mounted {
            let line = sandbox::read_control(&mut output)?;
            if let Some(p) = line.strip_prefix("P ") {
                pid1 = p.trim().parse::<u32>().ok();
            } else if line == "S" {
                mounted = true;
            }
        }
        let pid1 = pid1.unwrap();
        let net = Network::attach(pid1, rootless, &setup_home)?;
        writeln!(input, "R")?;
        Ok((pid1, net))
    });
    let (_, network) = match setup.await? {
        Ok(v) => v,
        Err(e) => {
            let _ = stage1.kill();
            let _ = stage1.wait();
            if !rootless {
                let _ = std::fs::write("/sys/fs/cgroup/cgroup.procs", std::process::id().to_string());
                remove_cgroup(&cgroup);
            }
            return Err(e);
        }
    };
    write_state(
        &home,
        &DevState {
            pid: std::process::id(),
            port,
            mode: if rootless { "rootless" } else { "root" }.into(),
            started_unix_ms: started,
            cgroup: Some(cgroup.clone()),
            vm_ip: None,
            helper_pid: None,
        },
    )?;
    eprintln!("dev: sandbox up in {} ms, API on 127.0.0.1:{port}", now_ms() - started);
    std::thread::spawn(move || super::broker::serve(broker_host, [127, 0, 0, 1].into()));

    let waiter = tokio::task::spawn_blocking(move || stage1.wait());
    tokio::pin!(waiter);
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let status = tokio::select! {
        s = &mut waiter => Some(s),
        _ = term.recv() => None,
        _ = int.recv() => None,
        _ = hup.recv() => None,
    };
    let status = match status {
        Some(s) => s,
        None => {
            eprintln!("dev: stopping");
            // SAFETY: plain signal to our child.
            unsafe { libc::kill(stage1_pid as i32, libc::SIGTERM) };
            match tokio::time::timeout(Duration::from_secs(20), &mut waiter).await {
                Ok(s) => s,
                Err(_) => {
                    // SAFETY: as above.
                    unsafe { libc::kill(stage1_pid as i32, libc::SIGKILL) };
                    waiter.await
                }
            }
        }
    };
    network.stop();
    let _ = std::fs::remove_file(super::state_path(&home));
    if !rootless {
        // Let the kernel finish tearing down the PID namespace first, then
        // leave the cluster's cgroup so all of it can go.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = std::fs::write("/sys/fs/cgroup/cgroup.procs", std::process::id().to_string());
        remove_cgroup(&cgroup);
    }
    eprintln!("dev: stopped ({:?})", status.ok().and_then(|r| r.ok()).and_then(|s| s.code()));
    Ok(())
}

/// Delete `dir` from inside a user namespace mapped like the sandbox's, so
/// files owned by subordinate ids go too.
pub fn remove_in_userns(dir: &Path) -> Result<()> {
    let (in_r, in_w) = pipe()?;
    let (out_r, out_w) = pipe()?;
    let mut cmd = std::process::Command::new("/proc/self/exe");
    cmd.arg("dev-sandbox").arg("--rm").arg(dir);
    cmd.env("LITEK8S_DEV_ROOTLESS", "1");
    remap_fds(&mut cmd, vec![in_r.as_raw_fd(), out_w.as_raw_fd()], vec![FD_CONTROL_IN, FD_CONTROL_OUT]);
    let mut child = cmd.spawn()?;
    drop((in_r, out_w));
    let mut input = std::fs::File::from(in_w);
    let mut output = BufReader::new(std::fs::File::from(out_r));
    let result = (|| -> Result<()> {
        sandbox::read_control(&mut output)?;
        map_ids(child.id())?;
        writeln!(input, "M")?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("failed to delete {}", dir.display());
    }
    Ok(())
}
