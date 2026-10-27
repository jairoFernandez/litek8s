//! The host side of the Linux dev sandbox (`litek8s dev supervise`):
//! prepares the cgroups, maps the user namespace, attaches the userspace
//! network, runs the cell manager in the sandbox ([`super::cells`]) and
//! then starts and stops clusters in it on request ([`super::env`]): for
//! each it binds the API port on the host's loopback and the Docker socket,
//! and hands them to the cluster's cell. The sandbox goes when the last
//! cluster stops (or on SIGTERM). See [`super::sandbox`] for the inside.

use std::io::{BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::cluster::Cluster;
use super::env::{CellRequest, EnvState, Request, Response};
use super::sandbox::{self, FD_CONTROL_IN, FD_CONTROL_OUT};
use super::{DEFAULT_PORT, DevState, dev_home, now_ms, which, write_state};

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    Ok(sandbox::pipe()?)
}

use sandbox::remap_fds;

fn is_root() -> bool {
    // SAFETY: no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// The id ranges of the sandbox's user namespace for our `id`: our own id
/// keeps its number (files a container creates as that uid in a bind
/// mount belong to the user, as with Docker), the subordinate range backs
/// every other id up to 65535, root included. `(inside, outside, count)`.
/// With `LITEK8S_DEV_IDMAP=root` (or an id above 65535) the user is root
/// inside instead (Docker rootless semantics).
pub(crate) fn id_ranges(id: u32, start: u32, count: u32) -> Vec<(u32, u32, u32)> {
    let root_mode = std::env::var("LITEK8S_DEV_IDMAP").is_ok_and(|v| v == "root");
    if root_mode || id == 0 || id >= 65536 || count < 65535 {
        return vec![(0, id, 1), (1, start, count.min(65536))];
    }
    let mut v = vec![(0, start, id), (id, id, 1)];
    let rest = 65535 - id;
    if rest > 0 {
        v.push((id + 1, start + id, rest));
    }
    v
}

/// Write the user and group id maps of `pid`'s new user namespace (see
/// [`id_ranges`]).
fn map_ids(pid: u32) -> Result<()> {
    // SAFETY: no preconditions.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let user = sandbox::user_name(uid);
    for (tool, file, id) in [("newuidmap", "/etc/subuid", uid), ("newgidmap", "/etc/subgid", gid)] {
        let (start, count) = sandbox::subordinate_range(file, &user, uid).with_context(|| format!("{file} has no range for {user}"))?;
        let mut args = vec![pid.to_string()];
        for (inside, outside, n) in id_ranges(id, start, count) {
            args.extend([inside.to_string(), outside.to_string(), n.to_string()]);
        }
        let out = std::process::Command::new(tool).args(&args).output().with_context(|| format!("failed to run {tool}"))?;
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

/// CPU and memory limits for everything the sandbox runs (the settings'
/// `cpus` and `memory`; none by default).
fn limit(sandbox: &Path, cpus: Option<usize>, memory_mib: Option<u64>) {
    if let Some(c) = cpus {
        if let Err(e) = std::fs::write(sandbox.join("cpu.max"), format!("{} 100000", c * 100_000)) {
            eprintln!("dev: could not limit CPUs to {c}: {e}");
        }
    }
    if let Some(m) = memory_mib {
        if let Err(e) = std::fs::write(sandbox.join("memory.max"), (m << 20).to_string()) {
            eprintln!("dev: could not limit memory to {m} MiB: {e}");
        }
    }
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

/// pasta, unless `LITEK8S_DEV_NETWORK=slirp4netns` asks for slirp4netns
/// (some pasta releases drop TCP flows now and then, see docs/dev.md).
fn pasta() -> Option<PathBuf> {
    if std::env::var("LITEK8S_DEV_NETWORK").is_ok_and(|v| v == "slirp4netns") && which("slirp4netns").is_some() {
        return None;
    }
    which("pasta")
}

/// The userspace network of the sandbox's network namespace.
enum Network {
    Pasta { pid_file: PathBuf, netns_file: Option<PathBuf> },
    Slirp { child: std::process::Child, _exit: OwnedFd },
    None,
}

impl Network {
    fn attach(pid1: u32, rootless: bool, home: &Path) -> Result<Self> {
        if let Some(pasta) = pasta() {
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

/// A cluster's Docker API socket on the host (`~/.litek8s/docker.sock` for
/// the default one), only for this user.
pub(super) fn bind_docker_socket(path: &Path) -> Result<std::os::unix::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        bail!("{} is in use (another litek8s dev?)", path.display());
    }
    let _ = std::fs::remove_file(path);
    let l = std::os::unix::net::UnixListener::bind(path).with_context(|| format!("failed to listen on {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

/// A running cluster, as the supervisor tracks it.
struct Running {
    socket: PathBuf,
}

struct Env {
    home: PathBuf,
    /// The cell manager's socket (one request at a time).
    cells: Mutex<OwnedFd>,
    clusters: Mutex<std::collections::BTreeMap<String, Running>>,
    base: PathBuf,
    mode: String,
    publish: super::config::Publish,
    cpus: Option<usize>,
    memory_mib: Option<u64>,
    /// Set when the last cluster stops: the sandbox goes too.
    done: tokio::sync::Notify,
}

impl Env {
    fn cell_request(&self, req: &CellRequest, fds: &[RawFd]) -> Result<()> {
        let sock = self.cells.lock().unwrap();
        let reply = super::cells::request_fd(sock.as_raw_fd(), req, fds)?;
        match reply.error {
            Some(e) => bail!("{e}"),
            None => Ok(()),
        }
    }

    fn start(&self, name: &str, port: Option<u16>, nodes: u32, registries: Option<String>) -> Result<u16> {
        let cluster = Cluster::new(name)?;
        if self.clusters.lock().unwrap().contains_key(name) {
            bail!("cluster {name} is running already");
        }
        let chome = cluster.home(&self.home);
        std::fs::create_dir_all(&chome)?;
        let preferred = cluster.meta(&self.home).and_then(|m| m.port).or(cluster.is_default().then_some(DEFAULT_PORT));
        let listener = super::bind_api(port, preferred)?;
        let port = listener.local_addr()?.port();
        let socket = cluster.docker_socket();
        let docker = bind_docker_socket(&socket)?;
        let (broker_host, broker_sandbox) = std::os::unix::net::UnixStream::pair()?;
        let log = std::fs::File::create(super::log_path(&chome))?;
        let data = cluster.data_dir(&self.home);
        std::fs::create_dir_all(&data)?;
        let req = CellRequest::Start { name: name.into(), nodes, data_dir: data, docker_alias: socket.to_string_lossy().into(), registries };
        if let Err(e) = self.cell_request(&req, &[listener.as_raw_fd(), docker.as_raw_fd(), broker_sandbox.as_raw_fd(), log.as_raw_fd()]) {
            let _ = std::fs::remove_file(&socket);
            return Err(e);
        }
        drop((listener, docker, broker_sandbox, log));
        let ip = self.publish.ipv4();
        std::thread::spawn(move || super::broker::serve(broker_host, ip.into()));
        write_state(
            &chome,
            &DevState {
                pid: std::process::id(),
                port,
                mode: self.mode.clone(),
                started_unix_ms: now_ms(),
                cgroup: Some(self.base.join("sandbox/cells").join(name)),
                vm_ip: None,
                helper_pid: None,
                cpus: self.cpus,
                memory_mib: self.memory_mib,
                shares: Vec::new(),
                publish: Some(self.publish.as_str().into()),
                name: Some(name.into()),
                nodes: Some(nodes),
                docker_socket: Some(socket.clone()),
            },
        )?;
        self.clusters.lock().unwrap().insert(name.into(), Running { socket });
        eprintln!("dev: cluster {name} started, API on 127.0.0.1:{port}");
        Ok(port)
    }

    fn stop(&self, name: &str) -> Result<()> {
        let running = self.clusters.lock().unwrap().remove(name);
        let result = self.cell_request(&CellRequest::Stop { name: name.into() }, &[]);
        if let Ok(cluster) = Cluster::new(name) {
            let _ = std::fs::remove_file(super::state_path(&cluster.home(&self.home)));
        }
        if let Some(r) = running {
            let _ = std::fs::remove_file(&r.socket);
        }
        eprintln!("dev: cluster {name} stopped");
        result
    }

    fn names(&self) -> Vec<String> {
        self.clusters.lock().unwrap().keys().cloned().collect()
    }

    /// The reply, and whether it is the last (no cluster left: the
    /// sandbox goes once it is written).
    fn handle(self: &Arc<Self>, req: Request) -> (Response, bool) {
        let result = match req {
            Request::Start { name, port, nodes, registries } => self.start(&name, port, nodes.max(1), registries).map(Some),
            Request::Stop { name } => self.stop(&name).map(|_| None),
            Request::Node { name, node, up } => {
                let r = self.cell_request(&CellRequest::Node { name, node, up }, &[]);
                return match r {
                    Ok(()) => (Response { running: self.names(), ..Default::default() }, false),
                    Err(e) => (Response::error(format!("{e:#}")), false),
                };
            }
            // The host deletes a cluster's files itself.
            Request::Purge { .. } => return (Response { running: self.names(), ..Default::default() }, false),
            Request::Stats => {
                let names = self.names();
                let stats = super::env::cell_stats(&self.base.join("sandbox")).into_iter().filter(|s| names.contains(&s.name)).collect();
                return (Response { stats, running: names, ..Default::default() }, false);
            }
            Request::Shutdown => {
                for n in self.names() {
                    let _ = self.stop(&n);
                }
                Ok(None)
            }
        };
        let running = self.names();
        let last = running.is_empty();
        let reply = match result {
            Ok(port) => Response { port, running, ..Default::default() },
            Err(e) => Response { running, ..Response::error(format!("{e:#}")) },
        };
        (reply, last)
    }
}

/// `litek8s dev supervise`: the sandbox every local cluster runs in, until
/// the last one stops.
pub async fn run(publish: super::config::Publish, cpus: Option<usize>, memory_mib: Option<u64>) -> Result<()> {
    let home = dev_home()?;
    std::fs::create_dir_all(&home)?;
    let rootless = !is_root();
    let started = now_ms();
    let cgroup = setup_cgroups(rootless)?;
    limit(&cgroup.join("sandbox"), cpus, memory_mib);

    let (in_r, in_w) = pipe()?;
    let (out_r, out_w) = pipe()?;
    let (cells_host, cells_sandbox) = seqpacket_pair()?;
    let mut cmd = std::process::Command::new("/proc/self/exe");
    cmd.arg("dev-sandbox").args(["dev", "__cells"]);
    cmd.env("LITEK8S_DEV_ROOTLESS", if rootless { "1" } else { "0" });
    if pasta().is_some() {
        if let Some(gw) = std::fs::read_to_string("/proc/net/route").ok().and_then(|t| super::hostinfo::parse_default_gateway(&t)) {
            cmd.env("LITEK8S_DEV_HOST_GATEWAY", gw);
        }
    }
    if rootless {
        // SAFETY: no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        if id_ranges(uid, 1, 65536).iter().any(|r| r.0 == uid && r.0 != 0) {
            cmd.env("LITEK8S_DEV_HOLDER_IDS", format!("{uid}:{gid}"));
        }
    }
    cmd.stdin(std::process::Stdio::null());
    remap_fds(&mut cmd, vec![in_r.as_raw_fd(), out_w.as_raw_fd(), cells_sandbox.as_raw_fd()], vec![FD_CONTROL_IN, FD_CONTROL_OUT, super::cells::FD_CELLS]);
    let mut stage1 = cmd.spawn().context("failed to start the sandbox")?;
    drop((in_r, out_w, cells_sandbox));
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
        let (mut pid1, mut holder, mut mounted) = (None, None, false);
        while pid1.is_none() || !mounted {
            let line = sandbox::read_control(&mut output)?;
            if let Some(p) = line.strip_prefix("P ") {
                pid1 = p.trim().parse::<u32>().ok();
            } else if let Some(p) = line.strip_prefix("N ") {
                holder = p.trim().parse::<u32>().ok();
            } else if line == "S" {
                mounted = true;
            }
        }
        let pid1 = pid1.unwrap();
        let net = Network::attach(holder.filter(|_| rootless).unwrap_or(pid1), rootless, &setup_home)?;
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
    let mode = if rootless { "rootless" } else { "root" };
    let control = super::env::control_path(&home);
    let env = Arc::new(Env {
        home: home.clone(),
        cells: Mutex::new(cells_host),
        clusters: Mutex::default(),
        base: cgroup.clone(),
        mode: mode.into(),
        publish,
        cpus,
        memory_mib,
        done: tokio::sync::Notify::new(),
    });
    let handler = env.clone();
    let served = env.clone();
    let path = control.clone();
    let server = tokio::spawn(async move {
        let r = super::env::serve(path, move |req| {
            let env = handler.clone();
            async move { tokio::task::spawn_blocking(move || env.handle(req)).await.unwrap_or_else(|e| (Response::error(e), false)) }
        })
        .await;
        if r.is_ok() {
            // The last reply went out: the sandbox stops now.
            served.done.notify_one();
        }
        r
    });
    super::env::write_state(
        &home,
        &EnvState {
            pid: std::process::id(),
            mode: mode.into(),
            started_unix_ms: started,
            control: control.clone(),
            cgroup: Some(cgroup.clone()),
            vm_ip: None,
            helper_pid: None,
            cpus,
            memory_mib,
            disk_gib: None,
            shares: Vec::new(),
            publish: Some(publish.as_str().into()),
        },
    )?;
    eprintln!("dev: sandbox up in {} ms", now_ms() - started);

    let waiter = tokio::task::spawn_blocking(move || stage1.wait());
    tokio::pin!(waiter);
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    // Nothing started within a minute (the `dev up` that started us died):
    // do not linger.
    let idle = {
        let env = env.clone();
        async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            loop {
                if env.names().is_empty() {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    };
    let status = tokio::select! {
        s = &mut waiter => Some(s),
        _ = term.recv() => None,
        _ = int.recv() => None,
        _ = hup.recv() => None,
        _ = env.done.notified() => None,
        _ = idle => None,
    };
    server.abort();
    let _ = std::fs::remove_file(super::env::state_path(&home));
    let _ = std::fs::remove_file(&control);
    let status = match status {
        Some(s) => s,
        None => {
            eprintln!("dev: stopping");
            for name in env.names() {
                let e = env.clone();
                let _ = tokio::task::spawn_blocking(move || e.stop(&name)).await;
            }
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
    for name in env.names() {
        if let Ok(c) = Cluster::new(&name) {
            let _ = std::fs::remove_file(super::state_path(&c.home(&home)));
            let _ = std::fs::remove_file(c.docker_socket());
        }
    }
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

fn seqpacket_pair() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: socketpair fills both descriptors.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: just created, owned here.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
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

#[cfg(test)]
mod tests {
    use super::id_ranges;

    #[test]
    fn user_keeps_its_id() {
        assert_eq!(id_ranges(1000, 100000, 65536), vec![(0, 100000, 1000), (1000, 1000, 1), (1001, 101000, 64535)]);
        // Every id below 65536 is mapped once.
        let total: u32 = id_ranges(1000, 100000, 65536).iter().map(|r| r.2).sum();
        assert_eq!(total, 65536);
        assert_eq!(id_ranges(70000, 100000, 65536), vec![(0, 70000, 1), (1, 100000, 65536)]);
    }
}
