//! A daemonless pod runtime: crun runs the containers, litek8s does what
//! dockerd and containerd did around it.
//!
//! - Images: pulled for the host's platform into a content-addressed store
//!   and unpacked once per layer ([`image`]).
//! - Root filesystems: overlayfs with the image's layers as shared lower
//!   directories and a per-container upper directory ([`sys`]).
//! - Pod sandbox: no pause container. The pod's network and IPC namespaces
//!   are created on a thread and kept alive as bind mounts; a veth joins the
//!   `litek8s0` bridge ([`net`]). Every container of the pod joins them by
//!   path in its OCI bundle ([`spec`]).
//! - Lifecycle: one small shim process per container ([`shim`]) creates and
//!   starts it with crun, owns its output (CRI log files, [`logs`]) and its
//!   exit status, so containers and their exit codes survive an agent
//!   restart. The agent learns about exits from a pidfd per shim.
//!
//! State lives under `<node dir>/crun` (metadata, bundles, logs, volumes,
//! images, address reservations) and `/run/litek8s` (crun's state, the IPC
//! namespace files, console sockets); network namespaces are in /run/netns.

pub(crate) mod image;
pub(crate) mod logs;
pub(crate) mod net;
pub(crate) mod shim;
pub(crate) mod spec;
pub(crate) mod sys;

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::stream::{BoxStream, StreamExt};
use hyper::body::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::sync::{mpsc, watch};

use super::runtime::{
    ByteStream, ContainerInfo, ContainerSpec, ExecSession, LABEL_POD_UID, LogOptions, MachineInfo, MountSource, PodRuntime, RuntimeContainer,
    SandboxInfo, SandboxSpec,
};

const RUN_DIR: &str = "/run/litek8s";
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContainerMeta {
    id: String,
    name: String,
    labels: HashMap<String, String>,
    sandbox: String,
    image_id: String,
    /// cgroup path relative to the cgroup root.
    cgroup: String,
    /// The process spec, reused for exec.
    process: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SandboxMeta {
    id: String,
    name: String,
    labels: HashMap<String, String>,
    hostname: String,
    ip: Option<String>,
    host_network: bool,
    netns: Option<String>,
    ipc: String,
    host_ports: Vec<(u16, u16, String)>,
    /// The hostIP of each host port ("" for every address).
    #[serde(default)]
    host_port_ips: Vec<String>,
    /// (target in the old sandbox container, directory on the node) of the
    /// pod's runtime volumes.
    volumes: Vec<(String, String)>,
    stopped: bool,
    /// PID 1 of the pod's shared PID namespace (`shareProcessNamespace`),
    /// started with the first container that needs it.
    #[serde(default)]
    pause_pid: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExitRecord {
    exit_code: i64,
    finished_at: String,
    #[serde(default)]
    oom_killed: bool,
}

struct Exec {
    code: watch::Receiver<Option<i64>>,
    pty: Option<Arc<tokio::io::unix::AsyncFd<OwnedFd>>>,
}

struct Inner {
    root: PathBuf,
    crun: PathBuf,
    state_root: PathBuf,
    images: Arc<image::ImageStore>,
    ipam: Mutex<net::Ipam>,
    subnet: net::Subnet,
    /// MTU of pod interfaces (lower on the WireGuard overlay).
    mtu: u32,
    exits_tx: mpsc::UnboundedSender<String>,
    exits_rx: Mutex<Option<mpsc::UnboundedReceiver<String>>>,
    execs: Mutex<HashMap<String, Exec>>,
    /// Serializes changes to the hostPort chain.
    hostports: tokio::sync::Mutex<()>,
}

#[derive(Clone)]
pub struct CrunPods {
    inner: Arc<Inner>,
}

pub(crate) fn find_crun() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LITEK8S_CRUN") {
        return Some(PathBuf::from(p));
    }
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/bin", "/usr/local/bin", "/usr/sbin"].map(PathBuf::from))
        .map(|d| d.join("crun"))
        .find(|p| p.is_file())
}

fn on_path(program: &str) -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::split_paths(&path).chain(["/usr/sbin", "/sbin"].map(PathBuf::from)).any(|d| d.join(program).is_file())
}

/// Whether this machine can run pods with crun: Linux, root, cgroup v2,
/// crun, iproute2 and iptables.
pub fn available() -> std::result::Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("crun needs Linux".into());
    }
    // SAFETY: no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err("the crun runtime needs root".into());
    }
    if !Path::new(CGROUP_ROOT).join("cgroup.controllers").exists() {
        return Err("the crun runtime needs cgroup v2".into());
    }
    if find_crun().is_none() {
        return Err("crun not found (install the crun package)".into());
    }
    for tool in ["ip", "iptables-restore"] {
        if !on_path(tool) {
            return Err(format!("{tool} not found"));
        }
    }
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn pid_alive(pid: i32, marker: &str) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).contains(marker))
}

async fn run_quiet(program: &str, args: &[&str], stdin: Option<&str>) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(if stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run {program}"))?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!("{program} {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

impl CrunPods {
    /// Set up the runtime under `node_dir`: directories, the bridge for the
    /// node's `pod_cidr` (a block of `cluster_cidr`) and its rules, and
    /// watches on containers that kept running while the agent was away.
    pub async fn new(node_dir: &Path, pod_cidr: &str, cluster_cidr: &str, mtu: u32) -> Result<Self> {
        available().map_err(anyhow::Error::msg)?;
        let root = node_dir.join("crun");
        for d in ["containers", "sandboxes", "volumes"] {
            std::fs::create_dir_all(root.join(d))?;
        }
        let run = PathBuf::from(RUN_DIR);
        for d in ["crun", "console"] {
            std::fs::create_dir_all(run.join(d))?;
        }
        sys::ensure_shared_dir(Path::new(net::NETNS_DIR))?;
        sys::ensure_shared_dir(&run.join("ipc"))?;
        let subnet = net::Subnet::parse(pod_cidr)?;
        let cluster = net::Subnet::parse(cluster_cidr)?;
        run_quiet("sh", &["-c", &net::bridge_commands(&subnet)], None).await.context("failed to set up the pod bridge")?;
        let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");
        run_quiet("sh", &["-c", crate::kube::agent::netpol::CREATE_CHAIN], None).await?;
        run_quiet("iptables-restore", &["--noflush"], Some(&net::base_rules(&subnet, &cluster))).await?;
        run_quiet("sh", &["-c", net::HOOKS], None).await?;
        let (exits_tx, exits_rx) = mpsc::unbounded_channel();
        let pods = Self {
            inner: Arc::new(Inner {
                crun: find_crun().context("crun not found")?,
                state_root: run.join("crun"),
                images: Arc::new(image::ImageStore::open(&root.join("images"))?),
                ipam: Mutex::new(net::Ipam::open(&root.join("ipam"), subnet)?),
                subnet,
                mtu,
                exits_tx,
                exits_rx: Mutex::new(Some(exits_rx)),
                execs: Mutex::new(HashMap::new()),
                hostports: tokio::sync::Mutex::new(()),
                root,
            }),
        };
        let live: HashSet<String> = pods.sandboxes().into_iter().map(|s| s.id).collect();
        pods.inner.ipam.lock().unwrap().release_unless(&live);
        for meta in pods.containers() {
            if let Some(pid) = pods.shim_pid(&meta.id) {
                pods.watch_exit(meta.id.clone(), pid);
            }
        }
        let _ = pods.sync_hostports().await;
        let gc = pods.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(600)).await;
                gc.gc().await;
            }
        });
        Ok(pods)
    }

    /// The node's image store (shared with the Docker API, see `crate::docker`).
    pub(crate) fn image_store(&self) -> Arc<image::ImageStore> {
        self.inner.images.clone()
    }

    /// Where image archives are staged before [`Self::import_images`].
    pub fn image_staging_dir(&self) -> PathBuf {
        self.inner.images.staging_dir()
    }

    /// Import the images of a `docker save`/OCI archive into the store.
    pub async fn import_images(&self, archive: PathBuf) -> Result<Vec<String>> {
        let pods = self.clone();
        tokio::task::spawn_blocking(move || pods.inner.images.import_archive(&archive)).await?
    }

    fn container_dir(&self, id: &str) -> PathBuf {
        self.inner.root.join("containers").join(id)
    }

    fn sandbox_dir(&self, id: &str) -> PathBuf {
        self.inner.root.join("sandboxes").join(id)
    }

    /// Where logs of removed containers go: `<node dir>/logs`.
    fn log_archive(&self) -> PathBuf {
        self.inner.root.parent().unwrap_or(&self.inner.root).join("logs")
    }

    fn volume_dir(&self, name: &str) -> PathBuf {
        self.inner.root.join("volumes").join(name)
    }

    fn container_meta(&self, id: &str) -> Option<ContainerMeta> {
        if id.is_empty() || id.contains('/') {
            return None;
        }
        read_json(&self.container_dir(id).join("meta.json"))
    }

    fn sandbox_meta(&self, id: &str) -> Option<SandboxMeta> {
        if id.is_empty() || id.contains('/') {
            return None;
        }
        read_json(&self.sandbox_dir(id).join("meta.json"))
    }

    fn containers(&self) -> Vec<ContainerMeta> {
        std::fs::read_dir(self.inner.root.join("containers"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| read_json(&e.path().join("meta.json")))
            .collect()
    }

    fn sandboxes(&self) -> Vec<SandboxMeta> {
        std::fs::read_dir(self.inner.root.join("sandboxes"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| read_json(&e.path().join("meta.json")))
            .collect()
    }

    /// The shim's pid while it (and so the container) is running.
    fn shim_pid(&self, id: &str) -> Option<i32> {
        let dir = self.container_dir(id);
        if dir.join("exit.json").exists() {
            return None;
        }
        let pid: i32 = std::fs::read_to_string(dir.join("shim.pid")).ok()?.trim().parse().ok()?;
        pid_alive(pid, id).then_some(pid)
    }

    fn sandbox_running(meta: &SandboxMeta) -> bool {
        !meta.stopped
            && meta.netns.as_ref().is_none_or(|n| sys::is_ns_file(Path::new(n)))
            && sys::is_ns_file(Path::new(&meta.ipc))
            && meta.pause_pid.is_none_or(|p| pid_alive(p, "crun-pause"))
    }

    /// The pod's shared PID namespace, holding it with a `crun-pause`
    /// process the first time a container asks for it.
    async fn shared_pid_namespace(&self, meta: &mut SandboxMeta) -> Result<String> {
        if let Some(pid) = meta.pause_pid.filter(|p| pid_alive(*p, "crun-pause")) {
            return Ok(format!("/proc/{pid}/ns/pid"));
        }
        let out = tokio::process::Command::new(if cfg!(target_os = "linux") { "/proc/self/exe" } else { "litek8s" })
            .arg0("litek8s")
            .arg("crun-pause")
            .arg("--shim-cgroup")
            .arg(Self::cgroup_dir("/litek8s/shims"))
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .context("failed to start the pod's PID namespace holder")?;
        let pid: i32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .with_context(|| format!("crun-pause failed: {}", String::from_utf8_lossy(&out.stderr).trim()))?;
        meta.pause_pid = Some(pid);
        write_json(&self.sandbox_dir(&meta.id).join("meta.json"), meta)?;
        Ok(format!("/proc/{pid}/ns/pid"))
    }

    /// Tell the agent when the container's shim exits.
    fn watch_exit(&self, id: String, pid: i32) {
        let tx = self.inner.exits_tx.clone();
        #[cfg(target_os = "linux")]
        {
            match sys::pidfd_open(pid) {
                Ok(fd) => {
                    tokio::spawn(async move {
                        if let Ok(afd) = tokio::io::unix::AsyncFd::with_interest(fd, tokio::io::Interest::READABLE) {
                            let _ = afd.readable().await;
                        }
                        let _ = tx.send(id);
                    });
                }
                Err(_) => {
                    let _ = tx.send(id);
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (pid, tx.send(id));
        }
    }

    async fn crun(&self, args: &[&str]) -> Result<()> {
        let root = self.inner.state_root.to_string_lossy().to_string();
        let mut all = vec!["--root", root.as_str()];
        all.extend_from_slice(args);
        run_quiet(&self.inner.crun.to_string_lossy(), &all, None).await
    }

    async fn wait_stopped(&self, id: &str, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.shim_pid(id).is_none() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Reload the hostPort chain from the running sandboxes.
    async fn sync_hostports(&self) -> Result<()> {
        let _held = self.inner.hostports.lock().await;
        let mut ports = Vec::new();
        for s in self.sandboxes().iter().filter(|s| Self::sandbox_running(s)) {
            if let Some(ip) = &s.ip {
                for (i, (container_port, host_port, proto)) in s.host_ports.iter().enumerate() {
                    let host_ip = s.host_port_ips.get(i).cloned().unwrap_or_default();
                    ports.push((*host_port, proto.clone(), ip.clone(), *container_port, host_ip));
                }
            }
        }
        run_quiet("iptables-restore", &["--noflush"], Some(&net::hostport_rules(&ports))).await
    }

    /// Release what a sandbox holds on the node (namespaces, address,
    /// shared memory); its directory stays until it is removed.
    async fn stop_sandbox(&self, meta: &mut SandboxMeta) {
        if let Some(pid) = meta.pause_pid.take().filter(|p| pid_alive(*p, "crun-pause")) {
            // Ends the shared PID namespace (and anything left in it).
            // SAFETY: signalling a process we started.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        if let Some(n) = &meta.netns {
            sys::remove_ns_file(Path::new(n));
        }
        sys::remove_ns_file(Path::new(&meta.ipc));
        sys::unmount(&self.sandbox_dir(&meta.id).join("shm"));
        self.inner.ipam.lock().unwrap().release(&meta.id);
        let had_ports = !meta.host_ports.is_empty() && !meta.stopped;
        meta.stopped = true;
        let _ = write_json(&self.sandbox_dir(&meta.id).join("meta.json"), meta);
        if had_ports {
            let _ = self.sync_hostports().await;
        }
    }

    fn pod_files(&self, spec: &SandboxSpec, dir: &Path, ip: Option<&str>) -> Result<()> {
        let gateway = self.inner.subnet.gateway().to_string();
        let mut hosts = String::new();
        if spec.host_network {
            hosts.push_str(&std::fs::read_to_string("/etc/hosts").unwrap_or_default());
            if !hosts.ends_with('\n') && !hosts.is_empty() {
                hosts.push('\n');
            }
        } else {
            hosts.push_str("# Kubernetes-managed hosts file.\n127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n");
            hosts.push_str("fe00::0\tip6-localnet\nff00::0\tip6-mcastprefix\nff02::1\tip6-allnodes\nff02::2\tip6-allrouters\n");
            if let Some(ip) = ip {
                match &spec.fqdn {
                    None => hosts.push_str(&format!("{ip}\t{}\n", spec.hostname)),
                    Some(fqdn) => hosts.push_str(&format!("{ip}\t{fqdn}\t{}\n", spec.hostname)),
                }
            }
        }
        for entry in &spec.extra_hosts {
            if let Some((name, addr)) = entry.rsplit_once(':') {
                let addr = if addr == "host-gateway" { gateway.as_str() } else { addr };
                hosts.push_str(&format!("{addr}\t{name}\n"));
            }
        }
        std::fs::write(dir.join("hosts"), hosts)?;
        let resolv = if spec.dns_servers.is_empty() && spec.dns_search.is_empty() {
            let host = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
            // systemd-resolved's stub address is not reachable from a pod.
            if host.contains("nameserver 127.0.0.53") && !spec.host_network {
                std::fs::read_to_string("/run/systemd/resolve/resolv.conf").unwrap_or(host)
            } else {
                host
            }
        } else {
            let mut r = String::new();
            for s in &spec.dns_servers {
                r.push_str(&format!("nameserver {s}\n"));
            }
            if spec.dns_servers.is_empty() {
                let host = std::fs::read_to_string("/run/systemd/resolve/resolv.conf")
                    .or_else(|_| std::fs::read_to_string("/etc/resolv.conf"))
                    .unwrap_or_default();
                for l in host.lines().filter(|l| l.starts_with("nameserver")) {
                    r.push_str(l);
                    r.push('\n');
                }
            }
            if !spec.dns_search.is_empty() {
                r.push_str(&format!("search {}\n", spec.dns_search.join(" ")));
            }
            if !spec.dns_options.is_empty() {
                r.push_str(&format!("options {}\n", spec.dns_options.join(" ")));
            }
            r
        };
        std::fs::write(dir.join("resolv.conf"), resolv)?;
        let hostname = if spec.host_network { std::fs::read_to_string("/etc/hostname").unwrap_or_default() } else { format!("{}\n", spec.hostname) };
        std::fs::write(dir.join("hostname"), hostname)?;
        Ok(())
    }

    async fn gc(&self) {
        let in_use: HashSet<String> = self
            .containers()
            .into_iter()
            .filter_map(|m| m.image_id.split_once('@').map(|(_, d)| d.to_string()))
            .collect();
        let pods = self.clone();
        let _ = tokio::task::spawn_blocking(move || pods.inner.images.gc(&in_use)).await;
    }

    /// The image config a container runs with.
    fn resolve_mounts(&self, spec: &ContainerSpec) -> Result<Vec<spec::ResolvedMount>> {
        let mut out = Vec::new();
        for m in &spec.mounts {
            let (source, kind) = match &m.source {
                MountSource::Bind(path) => {
                    if std::fs::symlink_metadata(path).is_err() {
                        std::fs::create_dir_all(path).with_context(|| format!("failed to create {path}"))?;
                    }
                    (path.clone(), spec::MountKind::Bind)
                }
                MountSource::Volume { name, subpath } => {
                    let mut dir = self.volume_dir(name);
                    if let Some(sub) = subpath.as_deref().filter(|s| !s.is_empty()) {
                        if sub.split('/').any(|seg| seg == "..") {
                            bail!("invalid subPath {sub}");
                        }
                        dir = dir.join(sub);
                    }
                    std::fs::create_dir_all(&dir)?;
                    (dir.to_string_lossy().to_string(), spec::MountKind::Bind)
                }
                MountSource::Tmpfs => (String::new(), spec::MountKind::Tmpfs),
            };
            out.push(spec::ResolvedMount { source, target: m.target.clone(), kind, read_only: m.read_only, propagation: m.propagation });
        }
        Ok(out)
    }

    async fn create_container(&self, id: &str, dir: &Path, sandbox: &SandboxMeta, spec: &ContainerSpec, pid: spec::PidNamespace) -> Result<ContainerMeta> {
        // The agent pulled it, but image GC may have removed it since.
        let image = match self.inner.images.get(&spec.image) {
            Some(image) => image,
            None => self.inner.images.pull(&spec.image, false).await?,
        };
        for d in ["upper", "work", "rootfs"] {
            std::fs::create_dir_all(dir.join(d))?;
        }
        let lowers: Vec<PathBuf> = image.layers.iter().rev().cloned().collect();
        let mut options = sys::overlay_options(&lowers, &dir.join("upper"), &dir.join("work"))?;
        if sys::in_user_namespace() {
            options.push_str(",userxattr");
        }
        sys::mount_overlay(&dir.join("rootfs"), &options)?;
        // Users resolve against the image's own files (never through a
        // symlink, which would point at the node's).
        let etc = |name: &str| {
            let p = dir.join("rootfs/etc").join(name);
            match std::fs::symlink_metadata(&p) {
                Ok(m) if m.is_file() => std::fs::read_to_string(&p).unwrap_or_default(),
                _ => String::new(),
            }
        };
        let user_str = spec.user.clone().or_else(|| image.config.user.clone()).unwrap_or_default();
        let user = spec::resolve_user(&user_str, &etc("passwd"), &etc("group"))?;
        let sandbox_dir = self.sandbox_dir(&sandbox.id);
        let pod_uid = spec.labels.get(LABEL_POD_UID).cloned().unwrap_or_else(|| sandbox.id.clone());
        let cgroup = format!("/litek8s/pod{}/{id}", pod_uid.replace('-', "_"));
        let etc_files = ["hosts", "resolv.conf", "hostname"]
            .iter()
            .map(|f| (format!("/etc/{f}"), sandbox_dir.join(f).to_string_lossy().to_string()))
            .collect();
        let params = spec::BundleParams {
            spec,
            image: &image.config,
            user,
            hostname: &sandbox.hostname,
            netns: sandbox.netns.clone(),
            ipcns: Some(sandbox.ipc.clone()),
            own_uts: !sandbox.host_network,
            pid,
            etc_files,
            shm: Some(sandbox_dir.join("shm").to_string_lossy().to_string()),
            mounts: self.resolve_mounts(spec)?,
            cgroups_path: cgroup.clone(),
        };
        let config = spec::config(&params)?;
        std::fs::write(dir.join("config.json"), serde_json::to_vec(&config)?)?;
        Ok(ContainerMeta {
            id: id.to_string(),
            name: spec.name.clone(),
            labels: spec.labels.clone(),
            sandbox: sandbox.id.clone(),
            image_id: image.id,
            cgroup,
            process: config["process"].clone(),
        })
    }

    fn console_path(name: &str) -> PathBuf {
        Path::new(RUN_DIR).join("console").join(format!("{name}.sock"))
    }

    fn cgroup_dir(rel: &str) -> PathBuf {
        Path::new(CGROUP_ROOT).join(rel.trim_start_matches('/'))
    }
}

fn read_stream<R: AsyncRead + Unpin + Send + 'static>(reader: R, channel: u8) -> ByteStream {
    futures_util::stream::unfold(Some(reader), move |state| async move {
        let mut r = state?;
        let mut buf = vec![0u8; 32 * 1024];
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok((channel, Bytes::from(buf))), Some(r)))
            }
        }
    })
    .boxed()
}

/// The write side of an exec's pty.
struct PtyWriter(Arc<tokio::io::unix::AsyncFd<OwnedFd>>);

impl AsyncWrite for PtyWriter {
    fn poll_write(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        use std::os::fd::AsRawFd;
        loop {
            let mut guard = std::task::ready!(self.0.poll_write_ready(cx))?;
            // SAFETY: writing from the caller's buffer to our fd.
            match guard.try_io(|fd| {
                let n = unsafe { libc::write(fd.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
                if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
            }) {
                Ok(result) => return std::task::Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        // End of input on a terminal is ^D.
        use std::os::fd::AsRawFd;
        // SAFETY: one byte from a static buffer.
        unsafe { libc::write(self.0.as_raw_fd(), [4u8].as_ptr().cast(), 1) };
        std::task::Poll::Ready(Ok(()))
    }
}

fn pty_output(fd: Arc<tokio::io::unix::AsyncFd<OwnedFd>>) -> ByteStream {
    use std::os::fd::AsRawFd;
    futures_util::stream::unfold(fd, |fd| async move {
        loop {
            let mut guard = fd.readable().await.ok()?;
            let mut buf = vec![0u8; 32 * 1024];
            // SAFETY: reading into our buffer.
            match guard.try_io(|f| {
                let n = unsafe { libc::read(f.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
            }) {
                // EIO: the other side of the terminal closed.
                Ok(Ok(0)) | Ok(Err(_)) => return None,
                Ok(Ok(n)) => {
                    buf.truncate(n);
                    return Some((Ok((1u8, Bytes::from(buf))), fd));
                }
                Err(_would_block) => continue,
            }
        }
    })
    .boxed()
}

fn set_nonblocking(fd: &OwnedFd) {
    use std::os::fd::AsRawFd;
    // SAFETY: fcntl on an fd we own.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

impl PodRuntime for CrunPods {
    fn scheme(&self) -> &'static str {
        "crun"
    }

    fn writes_pod_files(&self) -> bool {
        true
    }

    fn enforces_network_policy(&self) -> bool {
        true
    }

    async fn machine(&self) -> Result<MachineInfo> {
        let mem_kb: i64 = std::fs::read_to_string("/proc/meminfo")
            .unwrap_or_default()
            .lines()
            .find_map(|l| l.strip_prefix("MemTotal:")?.trim().trim_end_matches("kB").trim().parse().ok())
            .unwrap_or(0);
        let os = std::fs::read_to_string("/etc/os-release")
            .unwrap_or_default()
            .lines()
            .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
            .unwrap_or_else(|| "Linux".into());
        let version = tokio::process::Command::new(&self.inner.crun)
            .arg("--version")
            .output()
            .await
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.lines().next().map(|l| l.trim_start_matches("crun version ").trim().to_string()))
            .unwrap_or_default();
        Ok(MachineInfo {
            cpus: std::thread::available_parallelism().map(|n| n.get() as i64).unwrap_or(1),
            memory: mem_kb * 1024,
            arch: image::host_platform().0.to_string(),
            kernel: std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim().to_string(),
            os,
            runtime_version: format!("crun://{version}"),
        })
    }

    async fn pull_image(&self, image: &str, always: bool) -> Result<()> {
        // Make room first (a no-op below the GC threshold), and again after:
        // a pull that failed for lack of space must not wait 10 minutes.
        self.gc().await;
        let result = self.inner.images.pull(image, always).await;
        let pods = self.clone();
        tokio::spawn(async move { pods.gc().await });
        result.map(|_| ())
    }

    async fn ensure_volume(&self, name: &str, labels: HashMap<String, String>) -> Result<bool> {
        let dir = self.volume_dir(name);
        if dir.is_dir() {
            return Ok(false);
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("failed to create volume {name}"))?;
        write_json(&self.inner.root.join("volumes").join(format!("{name}.labels")), &labels)?;
        Ok(true)
    }

    async fn remove_volume(&self, name: &str) -> Result<()> {
        if name.is_empty() || name.contains('/') {
            return Ok(());
        }
        let dir = self.volume_dir(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("failed to remove volume {name}"))?;
        }
        let _ = std::fs::remove_file(self.inner.root.join("volumes").join(format!("{name}.labels")));
        Ok(())
    }

    async fn list_volumes(&self, label: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(self.inner.root.join("volumes"))?.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(volume) = name.strip_suffix(".labels") {
                let labels: HashMap<String, String> = read_json(&e.path()).unwrap_or_default();
                let hit = match label.split_once('=') {
                    Some((k, v)) => labels.get(k).is_some_and(|x| x == v),
                    None => labels.contains_key(label),
                };
                if hit {
                    out.push(volume.to_string());
                }
            }
        }
        Ok(out)
    }

    async fn run_sandbox(&self, spec: &SandboxSpec) -> Result<SandboxInfo> {
        for old in self.sandboxes().into_iter().filter(|s| s.name == spec.name) {
            let _ = self.remove(&old.id).await;
        }
        let id = image::random_hex(16);
        let short = &id[..12];
        let dir = self.sandbox_dir(&id);
        std::fs::create_dir_all(dir.join("shm"))?;
        let mut meta = SandboxMeta {
            id: id.clone(),
            name: spec.name.clone(),
            labels: spec.labels.clone(),
            hostname: spec.hostname.clone(),
            ip: None,
            host_network: spec.host_network,
            netns: (!spec.host_network).then(|| format!("{}/litek8s-{short}", net::NETNS_DIR)),
            ipc: format!("{RUN_DIR}/ipc/{short}"),
            host_ports: spec.host_ports.clone(),
            host_port_ips: spec.host_port_ips.clone(),
            volumes: spec
                .mounts
                .iter()
                .filter_map(|m| match &m.source {
                    MountSource::Volume { name, .. } => Some((m.target.clone(), self.volume_dir(name).to_string_lossy().to_string())),
                    _ => None,
                })
                .collect(),
            stopped: false,
            pause_pid: None,
        };
        write_json(&dir.join("meta.json"), &meta)?;
        let result = async {
            sys::mount_tmpfs(&dir.join("shm"), "mode=1777,size=65536k")?;
            let ip = if spec.host_network { None } else { Some(self.inner.ipam.lock().unwrap().allocate(&id)?) };
            let host_if = format!("lk{short}");
            let batch = ip.map(|ip| net::pod_link_commands(&host_if, ip, &self.inner.subnet, self.inner.mtu));
            sys::create_namespaces(meta.netns.as_ref().map(PathBuf::from), PathBuf::from(&meta.ipc), batch, spec.sysctls.clone())?;
            if ip.is_some() {
                sys::bridge_attach(net::BRIDGE, &host_if)?;
            }
            meta.ip = ip.map(|i| i.to_string());
            self.pod_files(spec, &dir, meta.ip.as_deref())?;
            write_json(&dir.join("meta.json"), &meta)?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(e) = result {
            let _ = self.remove(&id).await;
            return Err(e.context("failed to set up the pod sandbox"));
        }
        if !spec.host_ports.is_empty() {
            self.sync_hostports().await?;
        }
        Ok(SandboxInfo { id, ip: meta.ip })
    }

    async fn sandbox_ip(&self, id: &str) -> Result<Option<String>> {
        Ok(self.sandbox_meta(id).and_then(|m| m.ip))
    }

    async fn prepare_sandbox_dirs(&self, sandbox: &str, world_writable: Vec<String>, subdirs: Vec<String>) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let meta = self.sandbox_meta(sandbox).context("sandbox not found")?;
        let host_path = |target: &str| -> Option<PathBuf> {
            meta.volumes.iter().find_map(|(t, host)| {
                let rest = target.strip_prefix(t.as_str())?;
                (rest.is_empty() || rest.starts_with('/')).then(|| PathBuf::from(host).join(rest.trim_start_matches('/')))
            })
        };
        for target in world_writable.iter().chain(subdirs.iter()) {
            if target.split('/').any(|s| s == "..") {
                continue;
            }
            if let Some(path) = host_path(target) {
                std::fs::create_dir_all(&path)?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777))?;
            }
        }
        Ok(())
    }

    async fn run_container(&self, sandbox: &str, spec: &ContainerSpec) -> Result<String> {
        let mut sandbox = self.sandbox_meta(sandbox).context("pod sandbox not found")?;
        let pid = if spec.host_pid {
            spec::PidNamespace::Host
        } else if spec.share_process_namespace {
            spec::PidNamespace::Shared(self.shared_pid_namespace(&mut sandbox).await?)
        } else {
            spec::PidNamespace::New
        };
        for old in self.containers().into_iter().filter(|c| c.name == spec.name) {
            let _ = self.remove(&old.id).await;
        }
        let id = image::random_hex(16);
        let dir = self.container_dir(&id);
        std::fs::create_dir_all(&dir)?;
        let meta = match self.create_container(&id, &dir, &sandbox, spec, pid).await {
            Ok(m) => m,
            Err(e) => {
                sys::unmount(&dir.join("rootfs"));
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        write_json(&dir.join("meta.json"), &meta)?;
        let mut cmd = tokio::process::Command::new(if cfg!(target_os = "linux") { "/proc/self/exe" } else { "litek8s" });
        cmd.arg0("litek8s")
            .arg("crun-shim")
            .args(["--id", &id, "--bundle"])
            .arg(&dir)
            .arg("--crun")
            .arg(&self.inner.crun)
            .arg("--root")
            .arg(&self.inner.state_root)
            .arg("--shim-cgroup")
            .arg(Self::cgroup_dir("/litek8s/shims"))
            .arg("--cgroup")
            .arg(Self::cgroup_dir(&meta.cgroup));
        if spec.tty {
            cmd.arg("--tty").arg("--console").arg(Self::console_path(&id[..16]));
        }
        if spec.stdin {
            cmd.arg("--stdin");
        }
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
        let out = cmd.output().await.context("failed to start the container shim")?;
        if !out.status.success() {
            sys::unmount(&dir.join("rootfs"));
            let _ = std::fs::remove_dir_all(&dir);
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        match std::fs::read_to_string(dir.join("shim.pid")).ok().and_then(|p| p.trim().parse().ok()) {
            Some(pid) => self.watch_exit(id.clone(), pid),
            None => {
                let _ = self.inner.exits_tx.send(id.clone());
            }
        }
        Ok(id)
    }

    async fn inspect(&self, id: &str) -> Result<Option<ContainerInfo>> {
        if let Some(meta) = self.container_meta(id) {
            let dir = self.container_dir(id);
            let started_at = std::fs::read_to_string(dir.join("started")).ok();
            let exit: Option<ExitRecord> = read_json(&dir.join("exit.json"));
            let running = exit.is_none() && self.shim_pid(id).is_some();
            let mut info = ContainerInfo { running, started_at, image_id: meta.image_id, ..Default::default() };
            match exit {
                Some(e) => {
                    info.exit_code = Some(e.exit_code);
                    info.finished_at = Some(e.finished_at);
                    info.oom_killed = e.oom_killed;
                }
                None if !running => {
                    info.exit_code = Some(255);
                    info.error = Some("container shim exited unexpectedly".into());
                }
                None => {}
            }
            return Ok(Some(info));
        }
        Ok(self.sandbox_meta(id).map(|s| ContainerInfo { running: Self::sandbox_running(&s), ..Default::default() }))
    }

    async fn stop(&self, id: &str, grace: Duration) -> Result<()> {
        if let Some(mut meta) = self.sandbox_meta(id) {
            if !meta.stopped {
                self.stop_sandbox(&mut meta).await;
            }
            return Ok(());
        }
        if self.shim_pid(id).is_none() {
            return Ok(());
        }
        let _ = self.crun(&["kill", id, "TERM"]).await;
        if !self.wait_stopped(id, grace).await {
            let _ = self.crun(&["kill", id, "KILL"]).await;
            self.wait_stopped(id, Duration::from_secs(10)).await;
        }
        Ok(())
    }

    async fn remove(&self, id: &str) -> Result<()> {
        if let Some(mut meta) = self.sandbox_meta(id) {
            self.stop_sandbox(&mut meta).await;
            let pod_uid = meta.labels.get(LABEL_POD_UID).cloned().unwrap_or_else(|| meta.id.clone());
            let _ = std::fs::remove_dir(Self::cgroup_dir(&format!("/litek8s/pod{}", pod_uid.replace('-', "_"))));
            let _ = std::fs::remove_dir_all(self.sandbox_dir(id));
            return Ok(());
        }
        let Some(meta) = self.container_meta(id) else { return Ok(()) };
        if self.shim_pid(id).is_some() {
            let _ = self.crun(&["kill", id, "KILL"]).await;
            self.wait_stopped(id, Duration::from_secs(10)).await;
        }
        let dir = self.container_dir(id);
        if !dir.join("exit.json").exists() {
            // The shim is gone without cleaning up (killed, or a reboot).
            let _ = self.crun(&["delete", "-f", id]).await;
        }
        sys::unmount(&dir.join("rootfs"));
        let _ = std::fs::remove_dir(Self::cgroup_dir(&meta.cgroup));
        // Its logs outlive it, in the node's log archive (crate::obs::logs).
        if let Err(e) = crate::obs::logs::archive(&self.log_archive(), &meta.labels, &[dir.join("log.1"), dir.join("log")]) {
            eprintln!("node-agent: cannot archive the logs of container {id}: {e:#}");
        }
        std::fs::remove_dir_all(&dir).with_context(|| format!("failed to remove container {id}"))?;
        Ok(())
    }

    async fn list(&self) -> Result<Vec<RuntimeContainer>> {
        let mut out: Vec<RuntimeContainer> = self
            .sandboxes()
            .into_iter()
            .map(|s| RuntimeContainer { running: Self::sandbox_running(&s), id: s.id, labels: s.labels })
            .collect();
        for c in self.containers() {
            out.push(RuntimeContainer { running: self.shim_pid(&c.id).is_some(), id: c.id, labels: c.labels });
        }
        Ok(out)
    }

    async fn exec(&self, id: &str, command: Vec<String>, tty: bool, stdin: bool) -> Result<ExecSession> {
        let meta = self.container_meta(id).context("container not found")?;
        if command.is_empty() {
            // `kubectl attach` / `run -i`: the shim owns the container's
            // output (CRI log files), so attaching follows it; stdin is not
            // connected (what kubectl's own fallback to logs gives too).
            let output = self.logs(id, LogOptions { follow: true, tail_lines: None, timestamps: false, since_seconds: None });
            return Ok(ExecSession { id: format!("attach-{}", image::random_hex(8)), output, input: Box::pin(tokio::io::sink()) });
        }
        if self.shim_pid(id).is_none() {
            bail!("container {} is not running", meta.name);
        }
        let exec_id = image::random_hex(8);
        let dir = self.container_dir(id);
        let mut process = meta.process.clone();
        process["args"] = json!(command);
        process["terminal"] = json!(tty);
        let process_file = dir.join(format!("exec-{exec_id}.json"));
        std::fs::write(&process_file, serde_json::to_vec(&process)?)?;
        let mut cmd = tokio::process::Command::new(&self.inner.crun);
        cmd.arg("--root").arg(&self.inner.state_root).arg("exec").arg("--process").arg(&process_file);
        let console = Self::console_path(&format!("x{exec_id}"));
        let listener = if tty {
            let _ = std::fs::remove_file(&console);
            let l = std::os::unix::net::UnixListener::bind(&console).context("failed to create the console socket")?;
            cmd.arg("--console-socket").arg(&console);
            cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null());
            Some(l)
        } else {
            cmd.stdin(if stdin { std::process::Stdio::piped() } else { std::process::Stdio::null() })
                .stdout(std::process::Stdio::piped());
            None
        };
        cmd.stderr(std::process::Stdio::piped()).arg(id);
        let mut child = cmd.spawn().context("failed to run crun exec")?;
        let (code_tx, code_rx) = watch::channel(None);
        let (output, input, pty): (ByteStream, std::pin::Pin<Box<dyn AsyncWrite + Send>>, _) = match listener {
            Some(l) => {
                let fd = match tokio::task::spawn_blocking(move || sys::receive_fd(&l, 10_000)).await? {
                    Ok(fd) => fd,
                    Err(e) => {
                        let _ = child.kill().await;
                        let _ = std::fs::remove_file(&console);
                        let _ = std::fs::remove_file(&process_file);
                        bail!("exec: no terminal: {e}");
                    }
                };
                let _ = std::fs::remove_file(&console);
                set_nonblocking(&fd);
                let afd = Arc::new(tokio::io::unix::AsyncFd::new(fd)?);
                let stderr = child.stderr.take().map(|e| read_stream(e, 2));
                let mut out = pty_output(afd.clone());
                if let Some(e) = stderr {
                    out = futures_util::stream::select(out, e).boxed();
                }
                (out, Box::pin(PtyWriter(afd.clone())), Some(afd))
            }
            None => {
                let stdout = read_stream(child.stdout.take().context("no stdout")?, 1);
                let stderr = read_stream(child.stderr.take().context("no stderr")?, 2);
                let input: std::pin::Pin<Box<dyn AsyncWrite + Send>> = match child.stdin.take() {
                    Some(w) => Box::pin(w),
                    None => Box::pin(tokio::io::sink()),
                };
                (futures_util::stream::select(stdout, stderr).boxed(), input, None)
            }
        };
        self.inner.execs.lock().unwrap().insert(exec_id.clone(), Exec { code: code_rx, pty });
        let pods = self.clone();
        let eid = exec_id.clone();
        tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|s| s.code()).map(i64::from);
            let _ = code_tx.send(Some(code.unwrap_or(-1)));
            let _ = std::fs::remove_file(&process_file);
            // Keep the code around for the caller, then forget it.
            tokio::time::sleep(Duration::from_secs(60)).await;
            pods.inner.execs.lock().unwrap().remove(&eid);
        });
        Ok(ExecSession { id: exec_id, output, input })
    }

    async fn exec_exit_code(&self, exec_id: &str) -> Result<Option<i64>> {
        let rx = self.inner.execs.lock().unwrap().get(exec_id).map(|e| e.code.clone());
        let Some(mut rx) = rx else { return Ok(None) };
        let _ = tokio::time::timeout(Duration::from_secs(1), rx.wait_for(|c| c.is_some())).await;
        Ok(*rx.borrow())
    }

    async fn resize_exec(&self, exec_id: &str, width: u16, height: u16) -> Result<()> {
        use std::os::fd::AsRawFd;
        let pty = self.inner.execs.lock().unwrap().get(exec_id).and_then(|e| e.pty.clone());
        if let Some(pty) = pty {
            let ws = libc::winsize { ws_row: height, ws_col: width, ws_xpixel: 0, ws_ypixel: 0 };
            // SAFETY: TIOCSWINSZ reads the winsize we pass.
            unsafe { libc::ioctl(pty.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) };
        }
        Ok(())
    }

    fn logs(&self, id: &str, options: LogOptions) -> ByteStream {
        struct State {
            dir: PathBuf,
            filter: logs::Filter,
            follow: bool,
            tail: Option<usize>,
            offset: Option<u64>,
            pending: VecDeque<(u8, Bytes)>,
            done: bool,
        }
        let state = State {
            dir: self.container_dir(id),
            filter: logs::Filter {
                timestamps: options.timestamps,
                since: options.since_seconds.map(|s| chrono::Utc::now() - chrono::Duration::seconds(s)),
            },
            follow: options.follow,
            tail: options.tail_lines.filter(|t| *t >= 0).map(|t| t as usize),
            offset: None,
            pending: VecDeque::new(),
            done: false,
        };
        let render = |filter: &logs::Filter, data: &[u8], pending: &mut VecDeque<(u8, Bytes)>| {
            let mut out = Vec::new();
            filter.render(data, &mut out);
            pending.extend(out.into_iter().map(|(c, b)| (c, Bytes::from(b))));
        };
        futures_util::stream::unfold(state, move |mut st| async move {
            loop {
                if let Some(chunk) = st.pending.pop_front() {
                    return Some((Ok(chunk), st));
                }
                if st.done {
                    return None;
                }
                match st.offset {
                    None => {
                        if !st.dir.join("meta.json").exists() {
                            st.done = true;
                            return Some((Err(anyhow::anyhow!("container not found")), st));
                        }
                        let mut data = std::fs::read(st.dir.join("log.1")).unwrap_or_default();
                        let old = data.len();
                        data.extend(std::fs::read(st.dir.join("log")).unwrap_or_default());
                        let complete = data.iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
                        let start = st.tail.map_or(0, |n| logs::tail_offset(&data[..complete], n));
                        render(&st.filter, &data[start..complete], &mut st.pending);
                        st.offset = Some(complete.saturating_sub(old) as u64);
                        st.done = !st.follow;
                    }
                    Some(offset) => {
                        let exited = st.dir.join("exit.json").exists() || !st.dir.exists();
                        let data = std::fs::read(st.dir.join("log")).unwrap_or_default();
                        // Rotated: start over on the new file.
                        let offset = if (data.len() as u64) < offset { 0 } else { offset as usize };
                        let fresh = &data[offset..];
                        let complete = fresh.iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
                        render(&st.filter, &fresh[..complete], &mut st.pending);
                        st.offset = Some((offset + complete) as u64);
                        if st.pending.is_empty() {
                            if exited {
                                return None;
                            }
                            tokio::time::sleep(Duration::from_millis(250)).await;
                        }
                    }
                }
            }
        })
        .boxed()
    }

    async fn stats(&self, id: &str) -> Result<Option<(u64, u64)>> {
        let Some(meta) = self.container_meta(id) else { return Ok(None) };
        let dir = Self::cgroup_dir(&meta.cgroup);
        let Ok(cpu) = std::fs::read_to_string(dir.join("cpu.stat")) else { return Ok(None) };
        let usage_usec: u64 = cpu.lines().find_map(|l| l.strip_prefix("usage_usec ")?.trim().parse().ok()).unwrap_or(0);
        let current: u64 = std::fs::read_to_string(dir.join("memory.current")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let inactive: u64 = std::fs::read_to_string(dir.join("memory.stat"))
            .unwrap_or_default()
            .lines()
            .find_map(|l| l.strip_prefix("inactive_file ")?.trim().parse().ok())
            .unwrap_or(0);
        Ok(Some((usage_usec * 1000, current.saturating_sub(inactive))))
    }

    async fn apply_service_rules(&self, rules: &str) -> Result<()> {
        super::runtime::apply_rules_on_host(rules).await
    }

    async fn host_gateway_ip(&self) -> Result<String> {
        Ok(self.inner.subnet.gateway().to_string())
    }

    fn exits(&self) -> BoxStream<'static, String> {
        match self.inner.exits_rx.lock().unwrap().take() {
            Some(rx) => futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|id| (id, rx)) }).boxed(),
            None => futures_util::stream::empty().boxed(),
        }
    }

    fn container_cgroup(&self, id: &str) -> Option<PathBuf> {
        Some(Self::cgroup_dir(&self.container_meta(id)?.cgroup))
    }

    fn sandbox_netdev(&self, sandbox: &str) -> Option<String> {
        let meta = self.sandbox_meta(sandbox)?;
        (!meta.host_network && !meta.stopped && sandbox.len() >= 12).then(|| format!("lk{}", &sandbox[..12]))
    }

    fn log_files(&self, id: &str) -> Vec<PathBuf> {
        let dir = self.container_dir(id);
        vec![dir.join("log.1"), dir.join("log")]
    }

    fn volume_path(&self, name: &str) -> Option<PathBuf> {
        (!name.contains('/')).then(|| self.volume_dir(name))
    }

    async fn connect(&self, sandbox: &str, port: u16) -> std::io::Result<Option<tokio::net::TcpStream>> {
        let Some(meta) = self.sandbox_meta(sandbox) else { return Ok(None) };
        let Some(netns) = meta.netns.filter(|_| !meta.stopped) else { return Ok(None) };
        let stream = tokio::task::spawn_blocking(move || sys::connect_in_netns(PathBuf::from(netns), port))
            .await
            .map_err(std::io::Error::other)??;
        stream.set_nonblocking(true)?;
        Ok(Some(tokio::net::TcpStream::from_std(stream)?))
    }
}
