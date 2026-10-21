//! The cell manager: several clusters, and several nodes of one cluster,
//! in one Linux environment (the macOS dev VM or the Linux dev sandbox).
//!
//! Each node is a "cell": the same namespaces the dev sandbox gives a
//! cluster ([`super::sandbox`], here without a user namespace of its own),
//! made by the same stage 1 and PID 1:
//!
//! - network: its own `litek8s0` pod bridge, iptables rules, ports (API,
//!   DNS, kubelet, NodePorts) and an `eth0` on the cells' bridge
//!   (`lkcells0`, 10.231.0.0/24 in the environment's namespace, masqueraded
//!   out), so clusters never see each other's rules or ports and every one
//!   can use the usual 10.42.0.0/16 pods and 10.43.0.0/16 Services;
//! - mount: its own `/run` (crun state, pod network namespaces);
//! - cgroup: `/cells/<cluster>/<node>` of the environment, the root of the
//!   cell's cgroup namespace (memory per cluster is that directory's);
//! - PID: stopping the cell's PID 1 stops every process of the node.
//!
//! The manager (`litek8s dev __cells`) runs inside the environment. On
//! Linux the supervisor talks to it over a SEQPACKET socket and hands it
//! each cluster's host sockets (descriptors); in the VM the Mac connects
//! over vsock to [`CELLS_PORT`] on the VM's loopback. Requests are
//! [`CellRequest`]s, one JSON message each.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::cluster::Cluster;
use super::env::{CellReply, CellRequest};
use super::sandbox::{self, FD_API_LISTENER, FD_BROKER, FD_CONTROL_IN, FD_CONTROL_OUT, FD_DOCKER};

/// The bridge cells hang off, in the environment's network namespace.
pub const BRIDGE: &str = "lkcells0";
/// The cells' network: .1 is the bridge, cells get .2 to .254.
pub const SUBNET: [u8; 3] = [10, 231, 0];
/// The manager's TCP port on the VM's loopback.
pub const CELLS_PORT: u16 = 2381;
/// The Linux sandbox hands the manager its socket as this descriptor.
pub const FD_CELLS: RawFd = 5;
/// Bind mounts of the cells' network namespaces (the VM's splicer enters
/// them): `<cluster>.net` for the control-plane node, `<cluster>.<i>.net`
/// for the others.
pub const NETNS_DIR: &str = "/run/litek8s/cells";
/// The API port inside a cell (the VM's; on Linux the listener comes from
/// the host).
pub const CELL_API_PORT: u16 = 6443;
const WIREGUARD_PORT: u16 = 51820;
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

pub fn netns_path(cluster: &str, node: u32) -> PathBuf {
    match node {
        0 => Path::new(NETNS_DIR).join(format!("{cluster}.net")),
        i => Path::new(NETNS_DIR).join(format!("{cluster}.{i}.net")),
    }
}

fn cell_ip(slot: u8) -> String {
    format!("{}.{}.{}.{slot}", SUBNET[0], SUBNET[1], SUBNET[2])
}

fn bridge_ip() -> String {
    cell_ip(1)
}

/// One node of a cluster.
struct Node {
    index: u32,
    slot: u8,
    /// Stage 1's pid (the cell's PID 1 is its child).
    pid: u32,
}

struct ClusterCells {
    nodes: Vec<Node>,
    stopping: bool,
    /// Nodes stopped on request (not restarted until asked).
    held: std::collections::HashSet<u32>,
    spec: Option<Arc<NodeSpec>>,
}

/// What the manager knows; cells' waiter threads update it.
#[derive(Default)]
struct State {
    clusters: HashMap<String, ClusterCells>,
    /// Addresses taken by cells, running or starting.
    slots: std::collections::HashSet<u8>,
}

struct Manager {
    guest: bool,
    state: Mutex<State>,
    changed: Condvar,
    /// The host's address as cells reach it (a value or a file holding it).
    host_loopback: Option<String>,
}

/// `litek8s dev __cells`: set up the cells' bridge and serve requests.
pub fn run(guest: bool) -> Result<()> {
    let host_loopback = std::env::var("LITEK8S_HOST_LOOPBACK").ok().or_else(sandbox::default_gateway);
    let manager = Arc::new(Manager { guest, state: Mutex::default(), changed: Condvar::new(), host_loopback });
    manager.setup()?;
    eprintln!("cells: ready ({})", if guest { "VM" } else { "sandbox" });
    if guest {
        let listener = std::net::TcpListener::bind(("127.0.0.1", CELLS_PORT)).context("cells: cannot listen")?;
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let m = manager.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(&conn);
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let reply = match serde_json::from_str::<CellRequest>(&line) {
                    Ok(req) => m.handle(req, Vec::new()),
                    Err(e) => CellReply { error: Some(format!("bad request: {e}")) },
                };
                let mut out = serde_json::to_vec(&reply).unwrap_or_default();
                out.push(b'\n');
                let _ = (&conn).write_all(&out);
            });
        }
        return Ok(());
    }
    // Linux: requests (with descriptors) on the supervisor's socket, one
    // at a time; the manager ends when the supervisor goes.
    let sock = FD_CELLS;
    // SAFETY: the descriptor the sandbox inherited for us; cells must not.
    unsafe { libc::fcntl(sock, libc::F_SETFD, libc::FD_CLOEXEC) };
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let (n, fds) = match super::broker::recv_msg(sock, &mut buf) {
            Ok((0, _)) => return Ok(()),
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("cells: control socket"),
        };
        let reply = match serde_json::from_slice::<CellRequest>(&buf[..n]) {
            Ok(req) => manager.handle(req, fds),
            Err(e) => CellReply { error: Some(format!("bad request: {e}")) },
        };
        let out = serde_json::to_vec(&reply).unwrap_or_default();
        if let Err(e) = super::broker::send_msg(sock, &out, &[]) {
            return Err(e).context("cells: control socket");
        }
    }
}

fn run_cmd(program: &str, args: &[&str]) -> Result<()> {
    let out = std::process::Command::new(program).args(args).stdin(std::process::Stdio::null()).output().with_context(|| format!("failed to run {program}"))?;
    if !out.status.success() {
        bail!("{program} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Run `f` on a thread in the network namespace at `path` (commands it
/// starts are in that namespace too).
fn in_netns<T: Send + 'static>(path: &Path, f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    let file = std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    std::thread::spawn(move || {
        // SAFETY: setns on this thread with a namespace file we hold open.
        if unsafe { libc::setns(file.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
            return Err(std::io::Error::last_os_error()).context("cannot enter the cell's network namespace");
        }
        f()
    })
    .join()
    .map_err(|_| anyhow::anyhow!("netns thread panicked"))?
}

fn bind_mount(src: &str, dst: &Path) -> Result<()> {
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)?;
    }
    unbind(dst);
    std::fs::File::create(dst)?;
    let (s, d) = (std::ffi::CString::new(src)?, std::ffi::CString::new(dst.as_os_str().as_encoded_bytes())?);
    // SAFETY: valid strings.
    if unsafe { libc::mount(s.as_ptr(), d.as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null()) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("cannot bind {src} at {}", dst.display()));
    }
    Ok(())
}

fn unbind(path: &Path) {
    if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
        // SAFETY: valid string; failure (not mounted) is fine.
        unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
    }
    let _ = std::fs::remove_file(path);
}

/// Hand every controller of `dir` down to its children.
fn delegate(dir: &Path) {
    let controllers = std::fs::read_to_string(dir.join("cgroup.controllers")).unwrap_or_default();
    let enable: Vec<String> = controllers.split_whitespace().map(|c| format!("+{c}")).collect();
    if !enable.is_empty() {
        if let Err(e) = std::fs::write(dir.join("cgroup.subtree_control"), enable.join(" ")) {
            eprintln!("cells: cannot enable controllers in {}: {e}", dir.display());
        }
    }
}

/// Kill everything in a cgroup tree and remove it.
fn remove_cgroup(dir: &Path) {
    if !dir.exists() {
        return;
    }
    let _ = std::fs::write(dir.join("cgroup.kill"), "1");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        remove_tree(dir);
        if !dir.exists() || Instant::now() > deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn remove_tree(dir: &Path) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            remove_tree(&e.path());
        }
    }
    let _ = std::fs::remove_dir(dir);
}

impl Manager {
    fn cgroup(&self, cluster: &str) -> PathBuf {
        Path::new(CGROUP_ROOT).join("cells").join(cluster)
    }

    /// The bridge, NAT, and a clean slate (cells a previous manager left).
    fn setup(&self) -> Result<()> {
        let root = Path::new(CGROUP_ROOT);
        delegate(root);
        let cells = root.join("cells");
        for e in std::fs::read_dir(&cells).into_iter().flatten().flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                eprintln!("cells: removing a stale cell {}", e.file_name().to_string_lossy());
                remove_cgroup(&e.path());
            }
        }
        std::fs::create_dir_all(&cells).context("cannot create the cells' cgroup")?;
        delegate(&cells);
        for e in std::fs::read_dir(NETNS_DIR).into_iter().flatten().flatten() {
            unbind(&e.path());
        }
        let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");
        if run_cmd("ip", &["link", "show", BRIDGE]).is_err() {
            run_cmd("ip", &["link", "add", BRIDGE, "type", "bridge"])?;
        }
        run_cmd("ip", &["addr", "replace", &format!("{}/24", bridge_ip()), "dev", BRIDGE])?;
        run_cmd("ip", &["link", "set", BRIDGE, "up"])?;
        let net = format!("{}.{}.{}.0/24", SUBNET[0], SUBNET[1], SUBNET[2]);
        let _ = run_cmd("iptables", &["-t", "nat", "-N", "LITEK8S-CELLS"]);
        run_cmd("iptables", &["-t", "nat", "-F", "LITEK8S-CELLS"])?;
        run_cmd("iptables", &["-t", "nat", "-A", "LITEK8S-CELLS", "-s", &net, "!", "-o", BRIDGE, "-j", "MASQUERADE"])?;
        if run_cmd("iptables", &["-t", "nat", "-C", "POSTROUTING", "-j", "LITEK8S-CELLS"]).is_err() {
            run_cmd("iptables", &["-t", "nat", "-A", "POSTROUTING", "-j", "LITEK8S-CELLS"])?;
        }
        Ok(())
    }

    fn handle(self: &Arc<Self>, req: CellRequest, fds: Vec<OwnedFd>) -> CellReply {
        let result = match req {
            CellRequest::Start { name, nodes, data_dir, docker_alias, registries } => self.start(&name, nodes.max(1), &data_dir, &docker_alias, registries.as_deref(), fds),
            CellRequest::Stop { name } => self.stop(&name),
            CellRequest::Node { name, node, up } => self.node(&name, node, up),
            CellRequest::Purge { name } => self.purge(&name),
        };
        CellReply { error: result.err().map(|e| format!("{e:#}")) }
    }

    fn take_slot(&self) -> Result<u8> {
        let mut state = self.state.lock().unwrap();
        let slot = (2..=254u8).find(|s| !state.slots.contains(s)).context("no free address for another cell (250 nodes at most)")?;
        state.slots.insert(slot);
        Ok(slot)
    }

    fn release_slot(&self, slot: u8) {
        self.state.lock().unwrap().slots.remove(&slot);
    }

    fn start(self: &Arc<Self>, name: &str, nodes: u32, data_dir: &Path, docker_alias: &str, registries: Option<&str>, fds: Vec<OwnedFd>) -> Result<()> {
        let cluster = Cluster::new(name)?;
        {
            let state = self.state.lock().unwrap();
            if state.clusters.contains_key(name) {
                bail!("cluster {name} is running already");
            }
        }
        if !self.guest && fds.len() < 4 {
            bail!("cluster {name}: expected the API listener, Docker socket, broker and log descriptors");
        }
        std::fs::create_dir_all(data_dir).with_context(|| format!("cannot create {}", data_dir.display()))?;
        // Every node reads it (an empty one: no mirrors, no credentials).
        write_private(&registries_path(data_dir), registries.unwrap_or_default())?;
        let cg = self.cgroup(name);
        remove_cgroup(&cg);
        std::fs::create_dir_all(&cg).with_context(|| format!("cannot create cgroup {}", cg.display()))?;
        delegate(&cg);
        let fds = Arc::new(fds);
        let spec = Arc::new(NodeSpec { cluster, nodes, data_dir: data_dir.to_path_buf(), docker_alias: docker_alias.to_string(), fds });
        self.state.lock().unwrap().clusters.insert(name.to_string(), ClusterCells { nodes: Vec::new(), stopping: false, held: Default::default(), spec: Some(spec.clone()) });
        if let Err(e) = self.start_node(&spec, 0) {
            self.cleanup(name);
            return Err(e);
        }
        if nodes > 1 {
            // Workers join once the control plane has written its token.
            let m = self.clone();
            std::thread::spawn(move || {
                let token = spec.data_dir.join("state").join("join-token");
                let deadline = Instant::now() + Duration::from_secs(60);
                while !token.exists() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                }
                for i in 1..spec.nodes {
                    if m.state.lock().unwrap().clusters.get(&spec.cluster.name).is_none_or(|c| c.stopping) {
                        return;
                    }
                    if let Err(e) = m.start_node(&spec, i) {
                        eprintln!("cells: {}: node {i}: {e:#}", spec.cluster.name);
                    }
                }
            });
        }
        Ok(())
    }

    /// Start node `index` of a cluster in a new cell and watch it.
    fn start_node(self: &Arc<Self>, spec: &Arc<NodeSpec>, index: u32) -> Result<()> {
        let slot = self.take_slot()?;
        let ip = cell_ip(slot);
        let started = (|| {
            let server_ip = if index == 0 { ip.clone() } else { self.node_ip(&spec.cluster.name, 0).context("the control-plane node is gone")? };
            let cg = self.cgroup(&spec.cluster.name).join(index.to_string());
            std::fs::create_dir_all(&cg)?;
            self.spawn_cell(spec, index, slot, &ip, &server_ip, &cg)
        })();
        let pid = match started {
            Ok(pid) => pid,
            Err(e) => {
                self.release_slot(slot);
                return Err(e);
            }
        };
        {
            let mut state = self.state.lock().unwrap();
            match state.clusters.get_mut(&spec.cluster.name) {
                Some(c) => c.nodes.push(Node { index, slot, pid }),
                None => {
                    // SAFETY: plain signal to our child.
                    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                    bail!("cluster {} was stopped", spec.cluster.name);
                }
            }
        }
        eprintln!("cells: {} node {index} ({}) up at {ip}", spec.cluster.name, spec.cluster.node_name_at(index));
        Ok(())
    }

    fn node_ip(&self, cluster: &str, index: u32) -> Option<String> {
        let state = self.state.lock().unwrap();
        state.clusters.get(cluster)?.nodes.iter().find(|n| n.index == index).map(|n| cell_ip(n.slot))
    }

    /// Stage 1 of a new cell, through the handshake: cgroup, network, go.
    fn spawn_cell(self: &Arc<Self>, spec: &Arc<NodeSpec>, index: u32, slot: u8, ip: &str, server_ip: &str, cg: &Path) -> Result<u32> {
        let (in_r, in_w) = sandbox::pipe()?;
        let (out_r, out_w) = sandbox::pipe()?;
        let node_name = spec.cluster.node_name_at(index);
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/proc/self/exe"));
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("dev-sandbox");
        if index == 0 {
            cmd.args(["dev", "__apiserver", "--data-dir"]).arg(&spec.data_dir);
            cmd.args(["--node-name", &node_name, "--docker-alias", &spec.docker_alias]);
            cmd.args(["--node-port-offset", &spec.cluster.node_port_offset().to_string()]);
            cmd.arg("--registry-config").arg(registries_path(&spec.data_dir));
            if spec.nodes > 1 {
                cmd.args(["--listen-also", &format!("{ip}:{CELL_API_PORT}"), "--wireguard-port", &WIREGUARD_PORT.to_string()]);
            }
        } else {
            let token = std::fs::read_to_string(spec.data_dir.join("state").join("join-token")).context("no join token from the control plane")?;
            cmd.env("LITEK8S_TOKEN", token.trim());
            cmd.args(["agent", "--join", &format!("https://{server_ip}:{CELL_API_PORT}"), "--node-name", &node_name, "--node-ip", ip]);
            cmd.arg("--node-dir").arg(spec.data_dir.join("workers").join(index.to_string()));
            cmd.args(["--runtime", "crun", "--wireguard-port", &WIREGUARD_PORT.to_string()]);
            cmd.arg("--registry-config").arg(registries_path(&spec.data_dir));
        }
        cmd.env("LITEK8S_DEV_ROOTLESS", "0").env("LITEK8S_DEV_HOSTNAME", &node_name).env("LITEK8S_NODE_IP", ip);
        cmd.env_remove("LITEK8S_DEV_HOLDER_IDS");
        if let Some(h) = &self.host_loopback {
            cmd.env("LITEK8S_HOST_LOOPBACK", h);
        }
        if self.guest {
            cmd.env("LITEK8S_DEV_KEEP_RESOLV", "1");
        }
        cmd.stdin(std::process::Stdio::null());
        let mut sources = vec![in_r.as_raw_fd(), out_w.as_raw_fd()];
        let mut targets = vec![FD_CONTROL_IN, FD_CONTROL_OUT];
        let mut log_reader = None;
        if self.guest {
            // Output goes to the console, each line tagged with the cluster.
            let (r, w) = sandbox::pipe()?;
            cmd.stdout(std::process::Stdio::from(w.try_clone()?)).stderr(std::process::Stdio::from(w));
            log_reader = Some(r);
        } else {
            let [api, docker, broker, log] = [&spec.fds[0], &spec.fds[1], &spec.fds[2], &spec.fds[3]];
            cmd.stdout(std::process::Stdio::from(log.try_clone()?)).stderr(std::process::Stdio::from(log.try_clone()?));
            if index == 0 {
                sources.extend([api.as_raw_fd(), broker.as_raw_fd(), docker.as_raw_fd()]);
                targets.extend([FD_API_LISTENER, FD_BROKER, FD_DOCKER]);
            }
        }
        sandbox::remap_fds(&mut cmd, sources, targets);
        let mut child = cmd.spawn().context("failed to start a cell")?;
        drop(cmd);
        drop((in_r, out_w));
        let pid = child.id();
        if let Some(r) = log_reader {
            let tag = spec.cluster.name.clone();
            let node = if index == 0 { String::new() } else { format!("[{node_name}] ") };
            std::thread::spawn(move || {
                let reader = BufReader::new(std::fs::File::from(r));
                for line in reader.split(b'\n').map_while(Result::ok) {
                    let text = String::from_utf8_lossy(&line);
                    let mut out = std::io::stdout().lock();
                    let _ = writeln!(out, "LITEK8S_LOG {tag} {node}{}", text.trim_end());
                    let _ = out.flush();
                }
            });
        }
        let result = (|| -> Result<()> {
            let mut input = std::fs::File::from(in_w);
            let mut output = BufReader::new(std::fs::File::from(out_r));
            if sandbox::read_control(&mut output)? != "U" {
                bail!("unexpected cell handshake");
            }
            std::fs::write(cg.join("cgroup.procs"), pid.to_string()).context("failed to move the cell into its cgroup")?;
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
            let netns = netns_path(&spec.cluster.name, index);
            bind_mount(&format!("/proc/{pid1}/ns/net"), &netns)?;
            let host_if = format!("lkv{}", ip.rsplit('.').next().unwrap_or("0"));
            let _ = run_cmd("ip", &["link", "del", &host_if]);
            run_cmd("ip", &["link", "add", &host_if, "type", "veth", "peer", "name", "eth0", "netns", &pid1.to_string()])?;
            run_cmd("ip", &["link", "set", &host_if, "master", BRIDGE, "up"])?;
            let addr = format!("{ip}/24");
            in_netns(&netns, move || {
                run_cmd("ip", &["addr", "add", &addr, "dev", "eth0"])?;
                run_cmd("ip", &["link", "set", "eth0", "up"])?;
                run_cmd("ip", &["route", "add", "default", "via", &bridge_ip()])
            })?;
            writeln!(input, "R")?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = child.kill();
            let _ = child.wait();
            remove_cgroup(cg);
            return Err(e);
        }
        // Watch it: restart a node that dies on its own.
        let m = self.clone();
        let spec = spec.clone();
        std::thread::spawn(move || {
            let status = child.wait();
            m.exited(&spec, index, slot, pid, status.ok().and_then(|s| s.code()));
        });
        Ok(pid)
    }

    fn exited(self: &Arc<Self>, spec: &Arc<NodeSpec>, index: u32, slot: u8, pid: u32, code: Option<i32>) {
        let name = &spec.cluster.name;
        let restart = {
            let mut state = self.state.lock().unwrap();
            state.slots.remove(&slot);
            let Some(c) = state.clusters.get_mut(name) else { return };
            let before = c.nodes.len();
            c.nodes.retain(|n| n.pid != pid);
            let gone = before != c.nodes.len();
            let restart = gone && !c.stopping && !c.held.contains(&index);
            self.changed.notify_all();
            restart
        };
        unbind(&netns_path(name, index));
        if !restart {
            return;
        }
        eprintln!("cells: {name} node {index} exited ({code:?}); restarting");
        remove_cgroup(&self.cgroup(name).join(index.to_string()));
        std::thread::sleep(Duration::from_secs(1));
        if let Err(e) = self.start_node(spec, index) {
            eprintln!("cells: {name} node {index}: {e:#}");
        }
    }

    fn stop(&self, name: &str) -> Result<()> {
        let pids: Vec<(u32, u32)> = {
            let mut state = self.state.lock().unwrap();
            let Some(c) = state.clusters.get_mut(name) else { return Ok(()) };
            c.stopping = true;
            // Workers first, the control plane last.
            let mut v: Vec<(u32, u32)> = c.nodes.iter().map(|n| (n.index, n.pid)).collect();
            v.sort_by(|a, b| b.0.cmp(&a.0));
            v
        };
        for (_, pid) in &pids {
            // SAFETY: plain signal; stage 1 passes it to the cell's PID 1.
            unsafe { libc::kill(*pid as i32, libc::SIGTERM) };
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut state = self.state.lock().unwrap();
        while state.clusters.get(name).is_some_and(|c| !c.nodes.is_empty()) && Instant::now() < deadline {
            state = self.changed.wait_timeout(state, Duration::from_millis(200)).unwrap().0;
        }
        drop(state);
        self.cleanup(name);
        Ok(())
    }

    /// Stop or start again one worker node.
    fn node(self: &Arc<Self>, name: &str, index: u32, up: bool) -> Result<()> {
        if index == 0 {
            bail!("the control-plane node stops with its cluster (`litek8s dev down`)");
        }
        let (spec, pid) = {
            let mut state = self.state.lock().unwrap();
            let c = state.clusters.get_mut(name).with_context(|| format!("cluster {name} is not running"))?;
            let spec = c.spec.clone().context("cluster is starting")?;
            if index >= spec.nodes {
                bail!("cluster {name} has {} nodes", spec.nodes);
            }
            if up {
                c.held.remove(&index);
            } else {
                c.held.insert(index);
            }
            (spec, c.nodes.iter().find(|n| n.index == index).map(|n| n.pid))
        };
        match (up, pid) {
            (true, Some(_)) | (false, None) => Ok(()),
            (true, None) => self.start_node(&spec, index),
            (false, Some(pid)) => {
                // SAFETY: plain signal; stage 1 passes it to the cell's PID 1.
                unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                let deadline = Instant::now() + Duration::from_secs(20);
                let mut state = self.state.lock().unwrap();
                while state.clusters.get(name).is_some_and(|c| c.nodes.iter().any(|n| n.index == index)) && Instant::now() < deadline {
                    state = self.changed.wait_timeout(state, Duration::from_millis(200)).unwrap().0;
                }
                drop(state);
                remove_cgroup(&self.cgroup(name).join(index.to_string()));
                Ok(())
            }
        }
    }

    fn cleanup(&self, name: &str) {
        remove_cgroup(&self.cgroup(name));
        for e in std::fs::read_dir(NETNS_DIR).into_iter().flatten().flatten() {
            let f = e.file_name().to_string_lossy().to_string();
            if f == format!("{name}.net") || (f.starts_with(&format!("{name}.")) && f.ends_with(".net")) {
                unbind(&e.path());
            }
        }
        self.state.lock().unwrap().clusters.remove(name);
        self.changed.notify_all();
    }

    fn purge(&self, name: &str) -> Result<()> {
        let cluster = Cluster::new(name)?;
        if self.state.lock().unwrap().clusters.contains_key(name) {
            bail!("cluster {name} is running");
        }
        if !self.guest {
            bail!("purge is for the VM");
        }
        let dir = cluster.guest_data_dir();
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).with_context(|| format!("cannot delete {}", dir.display())),
            _ => Ok(()),
        }
    }
}

/// The registry configuration `dev up` sent, for the cluster's nodes.
fn registries_path(data_dir: &Path) -> PathBuf {
    data_dir.join("registries.yaml")
}

/// Write `text` to `path`, readable by its owner only (credentials).
fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp).with_context(|| format!("cannot write {}", tmp.display()))?;
    f.write_all(text.as_bytes())?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))
}

struct NodeSpec {
    cluster: Cluster,
    nodes: u32,
    data_dir: PathBuf,
    docker_alias: String,
    /// Linux: API listener, Docker listener, broker, log.
    fds: Arc<Vec<OwnedFd>>,
}

/// The Linux supervisor's side of the socket: send `req` with `fds`, wait
/// for the reply.
pub fn request_fd(sock: RawFd, req: &CellRequest, fds: &[RawFd]) -> Result<CellReply> {
    super::broker::send_msg(sock, &serde_json::to_vec(req)?, fds)?;
    let mut buf = vec![0u8; 64 * 1024];
    let (n, _) = super::broker::recv_msg(sock, &mut buf)?;
    if n == 0 {
        bail!("the cell manager is gone");
    }
    Ok(serde_json::from_slice(&buf[..n])?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_addresses() {
        assert_eq!(netns_path("dev", 0), PathBuf::from("/run/litek8s/cells/dev.net"));
        assert_eq!(netns_path("ci", 2), PathBuf::from("/run/litek8s/cells/ci.2.net"));
        assert_eq!((cell_ip(2), bridge_ip()), ("10.231.0.2".to_string(), "10.231.0.1".to_string()));
    }
}
