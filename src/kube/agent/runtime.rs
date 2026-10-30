//! The container runtime the node agent drives, behind a small trait so a
//! daemonless backend (crun/litecontainer) can replace Docker later.
//!
//! A pod is a *sandbox* container that owns the network namespace (and the
//! hostname, `/etc/hosts`, `/etc/resolv.conf`) plus one container per pod
//! container joined to it. The trait speaks in those terms only; everything
//! Kubernetes-specific stays in the agent.

use std::collections::HashMap;
use std::sync::Arc;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bollard::Docker;
use sha2::{Digest, Sha256};
use bollard::container::{
    Config, CreateContainerOptions, InspectContainerOptions, KillContainerOptions, ListContainersOptions, LogOutput,
    LogsOptions, MemoryStatsStats, RemoveContainerOptions, StartContainerOptions, StatsOptions,
};
use bollard::errors::Error as BollardError;
use bollard::exec::{CreateExecOptions, ResizeExecOptions, StartExecOptions, StartExecResults};
use bollard::image::CreateImageOptions;
use bollard::models::{HostConfig, Mount, MountBindOptions, MountTmpfsOptions, MountTypeEnum, MountVolumeOptions, PortBinding};
use bollard::network::CreateNetworkOptions;
use bollard::system::EventsOptions;
use bollard::volume::{CreateVolumeOptions, RemoveVolumeOptions};
use futures_util::stream::{BoxStream, StreamExt};
use hyper::body::Bytes;
use tokio::io::{AsyncWrite, AsyncWriteExt};

pub const LABEL_POD_UID: &str = "litek8s.pod.uid";
pub const LABEL_POD_NAME: &str = "litek8s.pod.name";
pub const LABEL_POD_NAMESPACE: &str = "litek8s.pod.namespace";
pub const LABEL_CONTAINER: &str = "litek8s.container.name";
pub const LABEL_ROLE: &str = "litek8s.role";
pub const LABEL_ATTEMPT: &str = "litek8s.container.attempt";
pub const LABEL_HASH: &str = "litek8s.container.hash";
/// The node whose agent made a container or volume (several agents may
/// share one runtime).
pub const LABEL_NODE: &str = "litek8s.node";
/// A volume backed by memory (`emptyDir.medium: Memory`).
pub const LABEL_TMPFS: &str = "litek8s.tmpfs";

/// The network pods join, so they get an address of their own.
const POD_NETWORK: &str = "litek8s-pods";
const SANDBOX_IMAGE: &str = "busybox:1.36";
/// A small image with iptables (k3s's service load balancer), used as the
/// helper that edits NAT rules in Docker's host network namespace.
const PROXY_IMAGE: &str = "rancher/klipper-lb:v0.4.13";
const PROXY_CONTAINER: &str = "litek8s-service-proxy";

/// The local Docker (or Podman's Docker API): `$DOCKER_HOST`, else the
/// first socket of OrbStack, Docker Desktop or `/var/run/docker.sock` that
/// exists.
pub fn connect_docker() -> Result<Docker> {
    if let Some(host) = std::env::var("DOCKER_HOST").ok().filter(|h| !h.is_empty()) {
        if let Some(path) = host.strip_prefix("unix://") {
            return Docker::connect_with_unix(path, 120, bollard::API_DEFAULT_VERSION).with_context(|| format!("failed to connect to Docker on {host}"));
        }
        if host.starts_with("tcp://") || host.starts_with("http://") || host.starts_with("https://") {
            return Docker::connect_with_http(&host, 120, bollard::API_DEFAULT_VERSION).with_context(|| format!("failed to connect to Docker on {host}"));
        }
        bail!("unsupported DOCKER_HOST scheme in `{host}`");
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let candidates = home.iter().flat_map(|h| [h.join(".orbstack/run/docker.sock"), h.join(".docker/run/docker.sock")]).chain([std::path::PathBuf::from("/var/run/docker.sock")]);
    for candidate in candidates {
        if candidate.exists() {
            return Docker::connect_with_unix(&candidate.to_string_lossy(), 120, bollard::API_DEFAULT_VERSION)
                .with_context(|| format!("failed to connect to Docker on {}", candidate.display()));
        }
    }
    bail!("failed to locate a usable Docker socket; set DOCKER_HOST or ensure Docker is running")
}

#[derive(Debug, Clone)]
pub enum MountSource {
    /// A directory or file on the node.
    Bind(String),
    /// A named runtime volume (emptyDir, persistent volumes).
    Volume { name: String, subpath: Option<String> },
    Tmpfs,
}

#[derive(Debug, Clone)]
pub struct MountSpec {
    pub source: MountSource,
    pub target: String,
    pub read_only: bool,
    pub propagation: Propagation,
}

/// A volume mount's `mountPropagation`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Propagation {
    /// `None`: private.
    #[default]
    Private,
    /// Mounts the host makes later under the source show up in the container.
    HostToContainer,
    /// Both ways (privileged containers only).
    Bidirectional,
}

impl Propagation {
    pub fn parse(mode: &str) -> Self {
        match mode {
            "HostToContainer" => Self::HostToContainer,
            "Bidirectional" => Self::Bidirectional,
            _ => Self::Private,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub name: String,
    pub hostname: String,
    /// `<hostname>.<subdomain>.<ns>.svc.<domain>` when the pod has a
    /// subdomain: listed before the hostname in `/etc/hosts`.
    pub fqdn: Option<String>,
    pub labels: HashMap<String, String>,
    pub host_network: bool,
    /// `name:ip` entries for `/etc/hosts` (`ip` may be `host-gateway`).
    pub extra_hosts: Vec<String>,
    /// (container port, host port, protocol)
    pub host_ports: Vec<(u16, u16, String)>,
    /// The `hostIP` of each entry of `host_ports` (empty: all addresses).
    pub host_port_ips: Vec<String>,
    pub dns_servers: Vec<String>,
    pub dns_search: Vec<String>,
    pub dns_options: Vec<String>,
    /// Volumes mounted into the sandbox too, so the agent can prepare them.
    pub mounts: Vec<MountSpec>,
    /// Pod sysctls (`spec.securityContext.sysctls`), set in the pod's
    /// network and IPC namespaces.
    pub sysctls: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct SandboxInfo {
    pub id: String,
    pub ip: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub env: Vec<String>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub labels: HashMap<String, String>,
    pub mounts: Vec<MountSpec>,
    pub tty: bool,
    pub stdin: bool,
    pub privileged: bool,
    pub cap_add: Vec<String>,
    pub cap_drop: Vec<String>,
    pub read_only_rootfs: bool,
    pub memory_limit: Option<i64>,
    pub nano_cpus: Option<i64>,
    /// The CPU request, for the container's CPU weight.
    pub cpu_request_milli: Option<i64>,
    pub sysctls: HashMap<String, String>,
    /// `allowPrivilegeEscalation: false`.
    pub no_new_privileges: bool,
    /// `seccompProfile: Unconfined` (otherwise the runtime's default filter).
    pub seccomp_unconfined: bool,
    /// The pod's `fsGroup` and `supplementalGroups`.
    pub supplemental_groups: Vec<u32>,
    /// `spec.shareProcessNamespace`: join the sandbox's PID namespace.
    pub share_process_namespace: bool,
    /// `spec.hostPID`: use the node's PID namespace.
    pub host_pid: bool,
}

/// Capacity and identity of the machine the containers run on (for a
/// VM-backed Docker, the VM).
#[derive(Debug, Clone, Default)]
pub struct MachineInfo {
    pub cpus: i64,
    pub memory: i64,
    pub arch: String,
    pub kernel: String,
    pub os: String,
    /// `docker://29.1.0`, `crun://1.14.1`
    pub runtime_version: String,
}

#[derive(Debug, Clone, Default)]
pub struct ContainerInfo {
    pub running: bool,
    pub exit_code: Option<i64>,
    pub oom_killed: bool,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub image_id: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RuntimeContainer {
    pub id: String,
    pub labels: HashMap<String, String>,
    pub running: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LogOptions {
    pub follow: bool,
    pub tail_lines: Option<i64>,
    pub timestamps: bool,
    pub since_seconds: Option<i64>,
}

pub type ByteStream = BoxStream<'static, Result<(u8, Bytes)>>;

pub struct ExecSession {
    pub id: String,
    /// (channel, bytes): 1 stdout, 2 stderr.
    pub output: ByteStream,
    pub input: Pin<Box<dyn AsyncWrite + Send>>,
}

pub trait PodRuntime: Send + Sync + 'static {
    /// The prefix of container IDs in pod status (`docker://<id>`).
    fn scheme(&self) -> &'static str {
        "docker"
    }
    fn machine(&self) -> impl Future<Output = Result<MachineInfo>> + Send;
    fn pull_image(&self, image: &str, always: bool) -> impl Future<Output = Result<()>> + Send;
    fn ensure_volume(&self, name: &str, labels: HashMap<String, String>) -> impl Future<Output = Result<bool>> + Send;
    fn remove_volume(&self, name: &str) -> impl Future<Output = Result<()>> + Send;
    /// Volumes with a label: `key`, or `key=value`.
    fn list_volumes(&self, label: &str) -> impl Future<Output = Result<Vec<String>>> + Send;
    fn run_sandbox(&self, spec: &SandboxSpec) -> impl Future<Output = Result<SandboxInfo>> + Send;
    fn sandbox_ip(&self, id: &str) -> impl Future<Output = Result<Option<String>>> + Send;
    /// Make volume directories the sandbox mounts (targets as in
    /// `SandboxSpec::mounts`, optionally with a sub path) world-writable,
    /// creating them as needed.
    fn prepare_sandbox_dirs(&self, sandbox: &str, world_writable: Vec<String>, subdirs: Vec<String>) -> impl Future<Output = Result<()>> + Send;
    /// Create and start a container in a sandbox's namespaces.
    fn run_container(&self, sandbox: &str, spec: &ContainerSpec) -> impl Future<Output = Result<String>> + Send;
    fn inspect(&self, id: &str) -> impl Future<Output = Result<Option<ContainerInfo>>> + Send;
    fn stop(&self, id: &str, grace: Duration) -> impl Future<Output = Result<()>> + Send;
    fn remove(&self, id: &str) -> impl Future<Output = Result<()>> + Send;
    /// Containers carrying the pod label (sandboxes and pod containers).
    fn list(&self) -> impl Future<Output = Result<Vec<RuntimeContainer>>> + Send;
    fn exec(&self, id: &str, command: Vec<String>, tty: bool, stdin: bool) -> impl Future<Output = Result<ExecSession>> + Send;
    fn exec_exit_code(&self, exec_id: &str) -> impl Future<Output = Result<Option<i64>>> + Send;
    fn resize_exec(&self, exec_id: &str, width: u16, height: u16) -> impl Future<Output = Result<()>> + Send;
    fn logs(&self, id: &str, options: LogOptions) -> ByteStream;
    /// (cumulative CPU nanoseconds, memory working set bytes) of a container.
    fn stats(&self, id: &str) -> impl Future<Output = Result<Option<(u64, u64)>>> + Send;
    /// Load the service NAT chain (`iptables-restore` input) where pod
    /// traffic is routed.
    fn apply_service_rules(&self, rules: &str) -> impl Future<Output = Result<()>> + Send;
    /// The address containers reach the host at (Docker's `host-gateway`).
    fn host_gateway_ip(&self) -> impl Future<Output = Result<String>> + Send;
    /// Ids of pod containers as they exit, for as long as the runtime runs.
    fn exits(&self) -> BoxStream<'static, String>;
    /// Whether the node always reaches pod addresses (and a hostNetwork
    /// pod's loopback is the node's). False for Docker, which may run in a
    /// VM: then probes, NodePort relays and port-forwards fall back to
    /// running `nc`/`wget` inside the pod's sandbox.
    fn reaches_pods_directly(&self) -> bool {
        false
    }
    /// Whether pod traffic goes through the node's iptables, so
    /// NetworkPolicies are enforced (`netpol.rs`): crun.
    fn enforces_network_policy(&self) -> bool {
        false
    }
    /// Whether the runtime writes each pod's `/etc/hosts` and
    /// `/etc/resolv.conf` itself; otherwise the agent writes kubelet-style
    /// ones and bind-mounts them into the containers.
    fn writes_pod_files(&self) -> bool {
        false
    }
    /// Connect to `port` on the pod's loopback from inside its network
    /// namespace; `None` when the runtime cannot (callers then dial the pod
    /// IP).
    fn connect(&self, sandbox: &str, port: u16) -> impl Future<Output = std::io::Result<Option<tokio::net::TcpStream>>> + Send {
        let _ = (sandbox, port);
        async { Ok(None) }
    }
    /// A container's cgroup directory, for detailed metrics; `None`: only
    /// [`PodRuntime::stats`].
    fn container_cgroup(&self, id: &str) -> Option<std::path::PathBuf> {
        let _ = id;
        None
    }
    /// The host side of a pod sandbox's network interface.
    fn sandbox_netdev(&self, sandbox: &str) -> Option<String> {
        let _ = sandbox;
        None
    }
    /// A container's CRI log files, oldest first; empty: read them with
    /// [`PodRuntime::logs`].
    fn log_files(&self, id: &str) -> Vec<std::path::PathBuf> {
        let _ = id;
        Vec::new()
    }
    /// Where a named volume's data is on this node.
    fn volume_path(&self, name: &str) -> Option<std::path::PathBuf> {
        let _ = name;
        None
    }
}

// ---------------------------------------------------------------------------
// Docker

#[derive(Clone)]
pub struct DockerPods {
    docker: Docker,
    /// Round-robin pod address allocation in the pod network (see `next_ip`).
    ips: Arc<std::sync::Mutex<Option<IpRing>>>,
}

/// Named volumes standing in for an image's VOLUMEs (see `run_container`).
const IMAGE_VOLUME_PREFIX: &str = "litek8s-imgvol-";

/// The pod subnet and where the next allocation starts.
#[derive(Debug, Clone, Copy)]
struct IpRing {
    base: u32,
    size: u32,
    next: u32,
}

/// The pod network's preferred subnet (k3s's pod CIDR).
const POD_SUBNET: &str = "10.42.0.0/16";

impl DockerPods {
    pub fn new(docker: Docker) -> Self {
        Self { docker, ips: Arc::default() }
    }

    /// The next pod address, round robin over the subnet like CNI's
    /// host-local IPAM: a just-freed address is not handed out again right
    /// away (with a VM-backed Docker the host's path to a reused address can
    /// stay stale for a while).
    async fn next_ip(&self) -> Option<std::net::Ipv4Addr> {
        let ring = *self.ips.lock().unwrap();
        let mut ring = match ring {
            Some(r) => r,
            None => {
                let net = self.docker.inspect_network::<String>(POD_NETWORK, None).await.ok()?;
                let subnet = net.ipam?.config?.into_iter().find_map(|c| c.subnet)?;
                let (addr, prefix) = subnet.split_once('/')?;
                let addr: std::net::Ipv4Addr = addr.parse().ok()?;
                let prefix: u32 = prefix.parse().ok()?;
                let size = 1u32.checked_shl(32 - prefix)?;
                let mut seed = [0u8; 4];
                rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut seed);
                IpRing { base: u32::from(addr), size, next: u32::from_le_bytes(seed) % size.max(1) }
            }
        };
        if ring.size < 8 {
            return None;
        }
        // Skip the network address, the gateway (.1) and the broadcast.
        let mut offset = ring.next % ring.size;
        if offset < 2 || offset == ring.size - 1 {
            offset = 2;
        }
        ring.next = offset + 1;
        *self.ips.lock().unwrap() = Some(ring);
        Some(std::net::Ipv4Addr::from(ring.base + offset))
    }

    async fn ensure_network(&self) -> Result<()> {
        if self.docker.inspect_network::<String>(POD_NETWORK, None).await.is_ok() {
            return Ok(());
        }
        let options = |subnet: Option<&str>| CreateNetworkOptions {
            name: POD_NETWORK.to_string(),
            driver: "bridge".to_string(),
            labels: HashMap::from([(LABEL_ROLE.to_string(), "pod-network".to_string())]),
            ipam: bollard::models::Ipam {
                config: subnet.map(|s| vec![bollard::models::IpamConfig { subnet: Some(s.to_string()), ..Default::default() }]),
                ..Default::default()
            },
            ..Default::default()
        };
        // Prefer a /16 (plenty of room for round-robin allocation); fall
        // back to whatever Docker picks if that range is taken.
        let mut result = self.docker.create_network(options(Some(POD_SUBNET))).await;
        if matches!(result, Err(BollardError::DockerResponseServerError { status_code, .. }) if status_code != 409) {
            result = self.docker.create_network(options(None)).await;
        }
        match result {
            Ok(_) => Ok(()),
            Err(BollardError::DockerResponseServerError { status_code: 409, .. }) => Ok(()),
            Err(e) => Err(e).context("failed to create the pod network"),
        }
    }

    /// The long-running helper that owns the service NAT chain.
    async fn ensure_proxy_helper(&self) -> Result<String> {
        if let Ok(details) = self.docker.inspect_container(PROXY_CONTAINER, None::<InspectContainerOptions>).await {
            if details.state.and_then(|s| s.running) == Some(true) {
                return Ok(details.id.unwrap_or_default());
            }
            let _ = self.remove(PROXY_CONTAINER).await;
        }
        self.pull_image(PROXY_IMAGE, false).await?;
        let config = Config {
            image: Some(PROXY_IMAGE.to_string()),
            entrypoint: Some(vec!["sleep".into(), "infinity".into()]),
            // Not `litek8s.managed`: that label marks the daemon's deployment
            // containers, and the daemon expects a deployment on them.
            labels: Some(HashMap::from([(LABEL_ROLE.to_string(), "service-proxy".to_string())])),
            host_config: Some(HostConfig {
                network_mode: Some("host".into()),
                privileged: Some(true),
                restart_policy: Some(bollard::models::RestartPolicy {
                    name: Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED),
                    maximum_retry_count: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let created = self
            .docker
            .create_container(Some(CreateContainerOptions { name: PROXY_CONTAINER.to_string(), platform: None }), config)
            .await
            .context("failed to create the service proxy helper")?;
        self.docker.start_container(&created.id, None::<StartContainerOptions<String>>).await?;
        // Hook the chain into PREROUTING (pod traffic) and OUTPUT (host
        // network pods), once.
        let setup = format!(
            "iptables -t nat -N {chain} 2>/dev/null; for c in PREROUTING OUTPUT; do \
             iptables -t nat -C $c -j {chain} 2>/dev/null || iptables -t nat -I $c 1 -j {chain}; done",
            chain = super::proxy::CHAIN
        );
        let session = self.exec(&created.id, vec!["sh".into(), "-c".into(), setup], false, false).await?;
        let mut out = session.output;
        while out.next().await.is_some() {}
        Ok(created.id)
    }

    async fn image_present(&self, image: &str) -> bool {
        self.docker.inspect_image(image).await.is_ok()
    }
}

/// Whether service rules can go straight into this host's NAT table: a
/// Linux host where the agent is root, next to a local Docker. Otherwise
/// (Docker in a VM, rootless) a helper container in Docker's host network
/// namespace applies them.
fn host_iptables() -> bool {
    use std::os::unix::fs::MetadataExt;
    static ON_HOST: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON_HOST.get_or_init(|| {
        cfg!(target_os = "linux")
            && std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0)
            && std::env::var("DOCKER_HOST").map_or(true, |h| h.starts_with("unix://"))
            && std::process::Command::new("iptables-restore")
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success())
    })
}

pub(crate) async fn apply_rules_on_host(rules: &str) -> Result<()> {
    use tokio::process::Command;
    let chain = super::proxy::CHAIN;
    let setup = format!(
        "iptables -t nat -N {chain} 2>/dev/null; for c in PREROUTING OUTPUT; do \
         iptables -t nat -C $c -j {chain} 2>/dev/null || iptables -t nat -I $c 1 -j {chain}; done"
    );
    Command::new("sh").arg("-c").arg(setup).status().await.context("failed to run iptables")?;
    let mut child = Command::new("iptables-restore")
        .arg("--noflush")
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to run iptables-restore")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(rules.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!("iptables-restore failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn docker_mount(m: &MountSpec) -> Mount {
    let mut bind_options = None;
    // A memory-backed emptyDir is world-writable, as the kubelet makes it;
    // Docker would otherwise copy the mode of the image's directory there
    // (e.g. a root-owned 0775 /run for a non-root container).
    let tmpfs_options = matches!(m.source, MountSource::Tmpfs).then(|| MountTmpfsOptions { mode: Some(0o777), ..Default::default() });
    let (typ, source, volume_options) = match &m.source {
        MountSource::Bind(path) => {
            bind_options = Some(MountBindOptions { create_mountpoint: Some(true), ..Default::default() });
            (MountTypeEnum::BIND, Some(path.clone()), None)
        }
        MountSource::Volume { name, subpath } => (
            MountTypeEnum::VOLUME,
            Some(name.clone()),
            subpath.as_ref().map(|s| MountVolumeOptions { subpath: Some(s.clone()), ..Default::default() }),
        ),
        MountSource::Tmpfs => (MountTypeEnum::TMPFS, None, None),
    };
    Mount {
        target: Some(m.target.clone()),
        source,
        typ: Some(typ),
        read_only: Some(m.read_only),
        volume_options,
        bind_options,
        tmpfs_options,
        ..Default::default()
    }
}

fn not_found(e: &BollardError) -> bool {
    matches!(e, BollardError::DockerResponseServerError { status_code: 404, .. })
}

fn log_chunk(out: LogOutput) -> (u8, Bytes) {
    match out {
        LogOutput::StdErr { message } => (2, message),
        LogOutput::StdOut { message } | LogOutput::Console { message } => (1, message),
        LogOutput::StdIn { message } => (0, message),
    }
}

impl PodRuntime for DockerPods {
    async fn machine(&self) -> Result<MachineInfo> {
        let info = self.docker.info().await.context("failed to query Docker")?;
        let arch = match info.architecture.as_deref().unwrap_or("") {
            "aarch64" | "arm64" => "arm64",
            "x86_64" | "amd64" => "amd64",
            "armv7l" | "arm" => "arm",
            other => other,
        }
        .to_string();
        Ok(MachineInfo {
            cpus: info.ncpu.unwrap_or(1),
            memory: info.mem_total.unwrap_or(0),
            arch,
            kernel: info.kernel_version.unwrap_or_default(),
            os: info.operating_system.unwrap_or_default(),
            runtime_version: format!("docker://{}", info.server_version.unwrap_or_default()),
        })
    }

    async fn prepare_sandbox_dirs(&self, sandbox: &str, world_writable: Vec<String>, subdirs: Vec<String>) -> Result<()> {
        let mut script = String::new();
        for target in &world_writable {
            script.push_str(&format!("chmod 0777 '{}'; ", target.replace('\'', "")));
        }
        for dir in &subdirs {
            script.push_str(&format!("mkdir -p -m 0777 '{}'; ", dir.replace('\'', "")));
        }
        if script.is_empty() {
            return Ok(());
        }
        let session = self.exec(sandbox, vec!["sh".into(), "-c".into(), script], false, false).await?;
        let mut out = session.output;
        while out.next().await.is_some() {}
        Ok(())
    }

    async fn pull_image(&self, image: &str, always: bool) -> Result<()> {
        if !always && self.image_present(image).await {
            return Ok(());
        }
        let mut stream = self.docker.create_image(
            Some(CreateImageOptions { from_image: image.to_string(), ..Default::default() }),
            None,
            None,
        );
        let mut result = Ok(());
        while let Some(event) = stream.next().await {
            if let Err(e) = event {
                result = Err(anyhow::anyhow!("failed to pull image \"{image}\": {e}"));
                break;
            }
        }
        // With `Always`, a registry hiccup is not fatal if the image is here.
        if result.is_err() && always && self.image_present(image).await {
            return Ok(());
        }
        result
    }

    async fn ensure_volume(&self, name: &str, labels: HashMap<String, String>) -> Result<bool> {
        if self.docker.inspect_volume(name).await.is_ok() {
            return Ok(false);
        }
        // `medium: Memory` emptyDirs: a tmpfs-backed volume, so every
        // container of the pod (and each restart) sees the same files.
        let driver_opts = if labels.get(LABEL_TMPFS).is_some_and(|v| v == "true") {
            HashMap::from([("type".to_string(), "tmpfs".to_string()), ("device".to_string(), "tmpfs".to_string()), ("o".to_string(), "mode=1777".to_string())])
        } else {
            HashMap::new()
        };
        self.docker
            .create_volume(CreateVolumeOptions { name: name.to_string(), driver: "local".to_string(), labels, driver_opts })
            .await
            .with_context(|| format!("failed to create volume {name}"))?;
        Ok(true)
    }

    async fn remove_volume(&self, name: &str) -> Result<()> {
        match self.docker.remove_volume(name, Some(RemoveVolumeOptions { force: true })).await {
            Ok(()) => Ok(()),
            Err(e) if not_found(&e) => Ok(()),
            Err(e) => Err(e).with_context(|| format!("failed to remove volume {name}")),
        }
    }

    async fn list_volumes(&self, label: &str) -> Result<Vec<String>> {
        let filters = HashMap::from([("label".to_string(), vec![label.to_string()])]);
        let resp = self
            .docker
            .list_volumes(Some(bollard::volume::ListVolumesOptions { filters }))
            .await?;
        Ok(resp.volumes.unwrap_or_default().into_iter().map(|v| v.name).collect())
    }

    async fn run_sandbox(&self, spec: &SandboxSpec) -> Result<SandboxInfo> {
        if !spec.host_network {
            self.ensure_network().await?;
        }
        self.pull_image(SANDBOX_IMAGE, false).await?;
        let mut exposed = HashMap::new();
        let mut bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        for (i, (container_port, host_port, protocol)) in spec.host_ports.iter().enumerate() {
            let key = format!("{container_port}/{}", protocol.to_lowercase());
            let host_ip = spec.host_port_ips.get(i).filter(|ip| !ip.is_empty()).cloned().unwrap_or_else(|| "0.0.0.0".into());
            exposed.insert(key.clone(), HashMap::new());
            bindings
                .entry(key)
                .or_insert_with(|| Some(Vec::new()))
                .get_or_insert_with(Vec::new)
                .push(PortBinding { host_ip: Some(host_ip), host_port: Some(host_port.to_string()) });
        }
        let host_config = HostConfig {
            network_mode: Some(if spec.host_network { "host".into() } else { POD_NETWORK.into() }),
            extra_hosts: (!spec.extra_hosts.is_empty()).then(|| spec.extra_hosts.clone()),
            port_bindings: (!bindings.is_empty()).then_some(bindings),
            dns: (!spec.dns_servers.is_empty()).then(|| spec.dns_servers.clone()),
            dns_search: (!spec.dns_search.is_empty()).then(|| spec.dns_search.clone()),
            dns_options: (!spec.dns_options.is_empty()).then(|| spec.dns_options.clone()),
            mounts: (!spec.mounts.is_empty()).then(|| spec.mounts.iter().map(docker_mount).collect()),
            ipc_mode: Some("shareable".into()),
            init: Some(true),
            sysctls: (!spec.sysctls.is_empty()).then(|| spec.sysctls.iter().cloned().collect()),
            ..Default::default()
        };
        // A few attempts: a chosen address may be in use by a pod that
        // survived an agent restart.
        let mut last_error = None;
        for _ in 0..8 {
            let address = if spec.host_network { None } else { self.next_ip().await };
            let networking_config = address.map(|ip| bollard::container::NetworkingConfig {
                endpoints_config: HashMap::from([(
                    POD_NETWORK.to_string(),
                    bollard::models::EndpointSettings {
                        ipam_config: Some(bollard::models::EndpointIpamConfig {
                            ipv4_address: Some(ip.to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )]),
            });
            let config = Config {
                image: Some(SANDBOX_IMAGE.to_string()),
                cmd: Some(vec!["sleep".into(), "infinity".into()]),
                hostname: (!spec.host_network).then(|| spec.hostname.clone()),
                labels: Some(spec.labels.clone()),
                exposed_ports: (!exposed.is_empty()).then(|| exposed.clone()),
                host_config: Some(host_config.clone()),
                networking_config,
                ..Default::default()
            };
            let _ = self.remove(&spec.name).await;
            let created = self
                .docker
                .create_container(Some(CreateContainerOptions { name: spec.name.clone(), platform: None }), config)
                .await
                .context("failed to create pod sandbox")?;
            match self.docker.start_container(&created.id, None::<StartContainerOptions<String>>).await {
                Ok(()) => {
                    let ip = self.sandbox_ip(&created.id).await?;
                    return Ok(SandboxInfo { id: created.id, ip });
                }
                Err(e) => {
                    let _ = self.remove(&created.id).await;
                    let in_use = e.to_string().contains("in use") || e.to_string().contains("already");
                    last_error = Some(e);
                    if address.is_none() || !in_use {
                        break;
                    }
                }
            }
        }
        Err(anyhow::anyhow!("failed to start pod sandbox: {}", last_error.map(|e| e.to_string()).unwrap_or_default()))
    }

    async fn sandbox_ip(&self, id: &str) -> Result<Option<String>> {
        let details = self.docker.inspect_container(id, None::<InspectContainerOptions>).await?;
        Ok(details
            .network_settings
            .and_then(|n| n.networks)
            .and_then(|nets| nets.get(POD_NETWORK).and_then(|n| n.ip_address.clone()))
            .filter(|ip| !ip.is_empty()))
    }

    async fn run_container(&self, sandbox: &str, spec: &ContainerSpec) -> Result<String> {
        let ns = format!("container:{sandbox}");
        let mut mounts: Vec<Mount> = spec.mounts.iter().map(docker_mount).collect();
        // The image's VOLUMEs: Docker would make anonymous volumes, named by
        // 64 hex digits, which some apps (vaultwarden) take for "no persistent
        // volume" and refuse to start. Kubernetes runtimes never do that; give
        // each one a named volume that lives as long as this container.
        let image_volumes = match self.docker.inspect_image(&spec.image).await {
            Ok(image) => image.config.and_then(|c| c.volumes).map(|v| v.into_keys().collect::<Vec<_>>()).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        for path in image_volumes {
            if spec.mounts.iter().any(|m| m.target.trim_end_matches('/') == path.trim_end_matches('/')) {
                continue;
            }
            let digest = Sha256::digest(format!("{}\0{path}", spec.name));
            mounts.push(Mount {
                target: Some(path),
                source: Some(format!("{IMAGE_VOLUME_PREFIX}{}", hex::encode(&digest[..8]))),
                typ: Some(MountTypeEnum::VOLUME),
                ..Default::default()
            });
        }
        let pid_mode = if spec.host_pid {
            Some("host".to_string())
        } else {
            spec.share_process_namespace.then(|| ns.clone())
        };
        let host_config = HostConfig {
            network_mode: Some(ns.clone()),
            ipc_mode: Some(ns),
            pid_mode,
            mounts: (!mounts.is_empty()).then_some(mounts),
            privileged: Some(spec.privileged),
            cap_add: (!spec.cap_add.is_empty()).then(|| spec.cap_add.clone()),
            cap_drop: (!spec.cap_drop.is_empty()).then(|| spec.cap_drop.clone()),
            readonly_rootfs: Some(spec.read_only_rootfs),
            memory: spec.memory_limit,
            nano_cpus: spec.nano_cpus,
            cpu_shares: spec.cpu_request_milli.filter(|m| *m > 0).map(|m| (m * 1024 / 1000).max(2)),
            sysctls: (!spec.sysctls.is_empty()).then(|| spec.sysctls.clone()),
            group_add: (!spec.supplemental_groups.is_empty()).then(|| spec.supplemental_groups.iter().map(u32::to_string).collect()),
            security_opt: {
                let mut opts = Vec::new();
                if spec.no_new_privileges && !spec.privileged {
                    opts.push("no-new-privileges".to_string());
                }
                if spec.seccomp_unconfined {
                    opts.push("seccomp=unconfined".to_string());
                }
                (!opts.is_empty()).then_some(opts)
            },
            ..Default::default()
        };
        let config = Config {
            image: Some(spec.image.clone()),
            entrypoint: spec.entrypoint.clone(),
            cmd: spec.cmd.clone(),
            env: Some(spec.env.clone()),
            working_dir: spec.working_dir.clone(),
            user: spec.user.clone(),
            labels: Some(spec.labels.clone()),
            tty: Some(spec.tty),
            open_stdin: Some(spec.stdin),
            attach_stdin: Some(false),
            host_config: Some(host_config),
            ..Default::default()
        };
        let _ = self.remove(&spec.name).await;
        let created = self
            .docker
            .create_container(Some(CreateContainerOptions { name: spec.name.clone(), platform: None }), config)
            .await
            .map_err(|e| anyhow::anyhow!("failed to create container: {e}"))?;
        if let Err(e) = self
            .docker
            .start_container(&created.id, None::<StartContainerOptions<String>>)
            .await
        {
            let _ = self.remove(&created.id).await;
            bail!("failed to start container: {e}");
        }
        Ok(created.id)
    }

    async fn inspect(&self, id: &str) -> Result<Option<ContainerInfo>> {
        let details = match self.docker.inspect_container(id, None::<InspectContainerOptions>).await {
            Ok(d) => d,
            Err(e) if not_found(&e) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let state = details.state.unwrap_or_default();
        let clean = |t: Option<String>| t.filter(|t| !t.starts_with("0001-"));
        Ok(Some(ContainerInfo {
            running: state.running.unwrap_or(false),
            exit_code: state.exit_code,
            oom_killed: state.oom_killed.unwrap_or(false),
            started_at: clean(state.started_at),
            finished_at: clean(state.finished_at),
            image_id: details.image.unwrap_or_default(),
            error: state.error.filter(|e| !e.is_empty()),
        }))
    }

    async fn stop(&self, id: &str, grace: Duration) -> Result<()> {
        let _ = self
            .docker
            .kill_container(id, Some(KillContainerOptions { signal: "SIGTERM" }))
            .await;
        let deadline = tokio::time::Instant::now() + grace;
        while tokio::time::Instant::now() < deadline {
            match self.inspect(id).await {
                Ok(Some(info)) if info.running => tokio::time::sleep(Duration::from_millis(100)).await,
                _ => return Ok(()),
            }
        }
        let _ = self.docker.kill_container(id, Some(KillContainerOptions { signal: "SIGKILL" })).await;
        Ok(())
    }

    async fn remove(&self, id: &str) -> Result<()> {
        let image_volumes: Vec<String> = match self.docker.inspect_container(id, None).await {
            Ok(c) => c
                .mounts
                .unwrap_or_default()
                .into_iter()
                .filter_map(|m| m.name)
                .filter(|n| n.starts_with(IMAGE_VOLUME_PREFIX))
                .collect(),
            Err(_) => Vec::new(),
        };
        match self
            .docker
            .remove_container(id, Some(RemoveContainerOptions { force: true, v: true, ..Default::default() }))
            .await
        {
            Ok(()) => {
                for name in image_volumes {
                    let _ = self.docker.remove_volume(&name, None).await;
                }
                Ok(())
            }
            Err(e) if not_found(&e) => Ok(()),
            Err(BollardError::DockerResponseServerError { status_code: 409, .. }) => Ok(()),
            Err(e) => Err(e).with_context(|| format!("failed to remove container {id}")),
        }
    }

    async fn list(&self) -> Result<Vec<RuntimeContainer>> {
        let filters = HashMap::from([("label".to_string(), vec![LABEL_POD_UID.to_string()])]);
        let containers = self
            .docker
            .list_containers(Some(ListContainersOptions::<String> { all: true, filters, ..Default::default() }))
            .await?;
        Ok(containers
            .into_iter()
            .map(|c| RuntimeContainer {
                id: c.id.unwrap_or_default(),
                labels: c.labels.unwrap_or_default(),
                running: c.state.as_deref() == Some("running"),
            })
            .collect())
    }

    async fn exec(&self, id: &str, command: Vec<String>, tty: bool, stdin: bool) -> Result<ExecSession> {
        let created = self
            .docker
            .create_exec(
                id,
                CreateExecOptions {
                    attach_stdin: Some(stdin),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    tty: Some(tty),
                    cmd: Some(command),
                    ..Default::default()
                },
            )
            .await?;
        match self
            .docker
            .start_exec(&created.id, Some(StartExecOptions { detach: false, tty, output_capacity: None }))
            .await?
        {
            StartExecResults::Attached { output, input } => Ok(ExecSession {
                id: created.id,
                output: output.map(|r| r.map(log_chunk).map_err(anyhow::Error::from)).boxed(),
                input,
            }),
            StartExecResults::Detached => bail!("exec started detached"),
        }
    }

    async fn exec_exit_code(&self, exec_id: &str) -> Result<Option<i64>> {
        for _ in 0..50 {
            let inspect = self.docker.inspect_exec(exec_id).await?;
            if inspect.running != Some(true) {
                return Ok(inspect.exit_code);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(None)
    }

    async fn resize_exec(&self, exec_id: &str, width: u16, height: u16) -> Result<()> {
        self.docker
            .resize_exec(exec_id, ResizeExecOptions { width, height })
            .await?;
        Ok(())
    }

    fn logs(&self, id: &str, options: LogOptions) -> ByteStream {
        let since = options
            .since_seconds
            .map(|s| chrono::Utc::now().timestamp() - s)
            .unwrap_or(0);
        self.docker
            .logs(
                id,
                Some(LogsOptions::<String> {
                    follow: options.follow,
                    stdout: true,
                    stderr: true,
                    since,
                    until: 0,
                    timestamps: options.timestamps,
                    tail: options.tail_lines.filter(|t| *t >= 0).map(|t| t.to_string()).unwrap_or_else(|| "all".into()),
                }),
            )
            .map(|r| r.map(log_chunk).map_err(anyhow::Error::from))
            .boxed()
    }

    async fn stats(&self, id: &str) -> Result<Option<(u64, u64)>> {
        let mut stream = self.docker.stats(id, Some(StatsOptions { stream: false, one_shot: true }));
        let Some(stats) = stream.next().await else { return Ok(None) };
        let stats = match stats {
            Ok(s) => s,
            Err(e) if not_found(&e) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let inactive = match &stats.memory_stats.stats {
            Some(MemoryStatsStats::V1(v1)) => v1.total_inactive_file,
            Some(MemoryStatsStats::V2(v2)) => v2.inactive_file,
            None => 0,
        };
        let usage = stats.memory_stats.usage.unwrap_or(0);
        Ok(Some((stats.cpu_stats.cpu_usage.total_usage, usage.saturating_sub(inactive))))
    }

    async fn apply_service_rules(&self, rules: &str) -> Result<()> {
        if host_iptables() {
            return apply_rules_on_host(rules).await;
        }
        let id = self.ensure_proxy_helper().await?;
        // Re-check the PREROUTING/OUTPUT jumps every time: Docker (or its VM)
        // restarting rebuilds those chains without them.
        let script = format!(
            "iptables -t nat -N {chain} 2>/dev/null; for c in PREROUTING OUTPUT; do \
             iptables -t nat -C $c -j {chain} 2>/dev/null || iptables -t nat -I $c 1 -j {chain}; done; \
             exec iptables-restore --noflush",
            chain = super::proxy::CHAIN
        );
        let mut session = self.exec(&id, vec!["sh".into(), "-c".into(), script], false, true).await?;
        session.input.write_all(rules.as_bytes()).await?;
        session.input.shutdown().await?;
        let mut errors = String::new();
        while let Some(Ok((_, bytes))) = session.output.next().await {
            errors.push_str(&String::from_utf8_lossy(&bytes));
        }
        match self.exec_exit_code(&session.id).await? {
            Some(0) | None => Ok(()),
            Some(code) => bail!("iptables-restore exited with {code}: {}", errors.trim()),
        }
    }

    async fn host_gateway_ip(&self) -> Result<String> {
        self.pull_image(SANDBOX_IMAGE, false).await?;
        let config = Config {
            image: Some(SANDBOX_IMAGE.to_string()),
            cmd: Some(vec!["cat".into(), "/etc/hosts".into()]),
            host_config: Some(HostConfig { extra_hosts: Some(vec!["litek8s-gateway:host-gateway".into()]), ..Default::default() }),
            ..Default::default()
        };
        let created = self.docker.create_container(None::<CreateContainerOptions<String>>, config).await?;
        let result = async {
            self.docker.start_container(&created.id, None::<StartContainerOptions<String>>).await?;
            let mut wait = self.docker.wait_container(&created.id, None::<bollard::container::WaitContainerOptions<String>>);
            while wait.next().await.is_some() {}
            let mut out = String::new();
            let mut logs = self.logs(&created.id, LogOptions::default());
            while let Some(Ok((_, bytes))) = logs.next().await {
                out.push_str(&String::from_utf8_lossy(&bytes));
            }
            out.lines()
                .find(|l| l.contains("litek8s-gateway"))
                .and_then(|l| l.split_whitespace().next())
                .map(str::to_string)
                .context("Docker did not resolve host-gateway")
        }
        .await;
        let _ = self.remove(&created.id).await;
        result
    }

    fn exits(&self) -> BoxStream<'static, String> {
        let docker = self.docker.clone();
        futures_util::stream::unfold(None, move |state: Option<BoxStream<'static, String>>| {
            let docker = docker.clone();
            async move {
                let mut events = match state {
                    Some(s) => s,
                    None => {
                        let filters = HashMap::from([
                            ("type".to_string(), vec!["container".to_string()]),
                            ("event".to_string(), vec!["die".to_string()]),
                            ("label".to_string(), vec![LABEL_POD_UID.to_string()]),
                        ]);
                        docker
                            .events(Some(EventsOptions::<String> { since: None, until: None, filters }))
                            .filter_map(|e| async move { e.ok().and_then(|e| e.actor).and_then(|a| a.id) })
                            .boxed()
                    }
                };
                match events.next().await {
                    Some(id) => Some((id, Some(events))),
                    None => {
                        // The event stream broke (Docker restarted): reconnect.
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        Some((String::new(), None))
                    }
                }
            }
        })
        .filter(|id| std::future::ready(!id.is_empty()))
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// Either backend, chosen at startup

/// The runtime a node runs pods with: crun on Linux, Docker elsewhere (or
/// when asked for).
pub enum NodeRuntime {
    Docker(DockerPods),
    Crun(super::crun::CrunPods),
}

impl NodeRuntime {
    /// Where image archives for [`Self::import_images`] are staged.
    /// The crun runtime's image store (the Docker API shares it).
    pub(crate) fn image_store(&self) -> Option<std::sync::Arc<super::crun::image::ImageStore>> {
        match self {
            NodeRuntime::Crun(rt) => Some(rt.image_store()),
            NodeRuntime::Docker(_) => None,
        }
    }

    pub fn image_staging_dir(&self) -> std::path::PathBuf {
        match self {
            NodeRuntime::Crun(rt) => rt.image_staging_dir(),
            NodeRuntime::Docker(_) => std::env::temp_dir(),
        }
    }

    /// Load the images of an image archive (`docker save` output) so pods
    /// can use them without a registry.
    pub async fn import_images(&self, archive: std::path::PathBuf) -> Result<Vec<String>> {
        match self {
            NodeRuntime::Crun(rt) => rt.import_images(archive).await,
            NodeRuntime::Docker(rt) => {
                let body = Bytes::from(tokio::fs::read(&archive).await?);
                let mut stream = rt.docker.import_image(bollard::image::ImportImageOptions { quiet: true }, body, None);
                let mut names = Vec::new();
                while let Some(item) = stream.next().await {
                    let info = item.context("docker load failed")?;
                    if let Some(line) = info.stream {
                        if let Some(name) = line.trim().strip_prefix("Loaded image: ") {
                            names.push(name.to_string());
                        }
                    }
                }
                Ok(names)
            }
        }
    }
}

macro_rules! each {
    ($self:ident, $rt:ident => $body:expr) => {
        match $self {
            NodeRuntime::Docker($rt) => $body,
            NodeRuntime::Crun($rt) => $body,
        }
    };
}

impl PodRuntime for NodeRuntime {
    fn scheme(&self) -> &'static str {
        each!(self, rt => rt.scheme())
    }
    async fn machine(&self) -> Result<MachineInfo> {
        each!(self, rt => rt.machine().await)
    }
    async fn pull_image(&self, image: &str, always: bool) -> Result<()> {
        each!(self, rt => rt.pull_image(image, always).await)
    }
    async fn ensure_volume(&self, name: &str, labels: HashMap<String, String>) -> Result<bool> {
        each!(self, rt => rt.ensure_volume(name, labels).await)
    }
    async fn remove_volume(&self, name: &str) -> Result<()> {
        each!(self, rt => rt.remove_volume(name).await)
    }
    async fn list_volumes(&self, label: &str) -> Result<Vec<String>> {
        each!(self, rt => rt.list_volumes(label).await)
    }
    async fn run_sandbox(&self, spec: &SandboxSpec) -> Result<SandboxInfo> {
        each!(self, rt => rt.run_sandbox(spec).await)
    }
    async fn sandbox_ip(&self, id: &str) -> Result<Option<String>> {
        each!(self, rt => rt.sandbox_ip(id).await)
    }
    async fn prepare_sandbox_dirs(&self, sandbox: &str, world_writable: Vec<String>, subdirs: Vec<String>) -> Result<()> {
        each!(self, rt => rt.prepare_sandbox_dirs(sandbox, world_writable, subdirs).await)
    }
    async fn run_container(&self, sandbox: &str, spec: &ContainerSpec) -> Result<String> {
        each!(self, rt => rt.run_container(sandbox, spec).await)
    }
    async fn inspect(&self, id: &str) -> Result<Option<ContainerInfo>> {
        each!(self, rt => rt.inspect(id).await)
    }
    async fn stop(&self, id: &str, grace: Duration) -> Result<()> {
        each!(self, rt => rt.stop(id, grace).await)
    }
    async fn remove(&self, id: &str) -> Result<()> {
        each!(self, rt => rt.remove(id).await)
    }
    async fn list(&self) -> Result<Vec<RuntimeContainer>> {
        each!(self, rt => rt.list().await)
    }
    async fn exec(&self, id: &str, command: Vec<String>, tty: bool, stdin: bool) -> Result<ExecSession> {
        each!(self, rt => rt.exec(id, command, tty, stdin).await)
    }
    async fn exec_exit_code(&self, exec_id: &str) -> Result<Option<i64>> {
        each!(self, rt => rt.exec_exit_code(exec_id).await)
    }
    async fn resize_exec(&self, exec_id: &str, width: u16, height: u16) -> Result<()> {
        each!(self, rt => rt.resize_exec(exec_id, width, height).await)
    }
    fn logs(&self, id: &str, options: LogOptions) -> ByteStream {
        each!(self, rt => rt.logs(id, options))
    }
    async fn stats(&self, id: &str) -> Result<Option<(u64, u64)>> {
        each!(self, rt => rt.stats(id).await)
    }
    async fn apply_service_rules(&self, rules: &str) -> Result<()> {
        each!(self, rt => rt.apply_service_rules(rules).await)
    }
    async fn host_gateway_ip(&self) -> Result<String> {
        each!(self, rt => rt.host_gateway_ip().await)
    }
    fn exits(&self) -> BoxStream<'static, String> {
        each!(self, rt => rt.exits())
    }
    fn reaches_pods_directly(&self) -> bool {
        each!(self, rt => rt.reaches_pods_directly())
    }
    fn enforces_network_policy(&self) -> bool {
        each!(self, rt => rt.enforces_network_policy())
    }
    fn writes_pod_files(&self) -> bool {
        each!(self, rt => rt.writes_pod_files())
    }
    async fn connect(&self, sandbox: &str, port: u16) -> std::io::Result<Option<tokio::net::TcpStream>> {
        each!(self, rt => rt.connect(sandbox, port).await)
    }
    fn container_cgroup(&self, id: &str) -> Option<std::path::PathBuf> {
        each!(self, rt => rt.container_cgroup(id))
    }
    fn sandbox_netdev(&self, sandbox: &str) -> Option<String> {
        each!(self, rt => rt.sandbox_netdev(sandbox))
    }
    fn log_files(&self, id: &str) -> Vec<std::path::PathBuf> {
        each!(self, rt => rt.log_files(id))
    }
    fn volume_path(&self, name: &str) -> Option<std::path::PathBuf> {
        each!(self, rt => rt.volume_path(name))
    }
}
