//! `litek8s dev`: a local Kubernetes cluster for development, in the spirit
//! of kind, k3d and minikube, without Docker.
//!
//! - Linux: the API server, controllers, DNS, service proxy and a crun node
//!   agent run in a sandbox of their own namespaces ([`sandbox`]), rootless
//!   by default. Nothing on the host changes: no interfaces, no iptables
//!   rules, no files outside the dev data directory.
//! - macOS: the same binary runs as PID 1 of a small Linux VM on Apple's
//!   Virtualization.framework ([`vm`]); its API server and NodePorts appear
//!   on the Mac's localhost and the home directory is shared with virtiofs.
//!
//! `dev up` merges a `litek8s-dev` context into the kubeconfig (like kind),
//! `dev load` puts local images into the cluster (like `kind load`).
//!
//! Clusters have names (`--name`, default `dev`; see [`cluster`]). They all
//! share one VM or sandbox ([`env`]), each in cells of its own ([`cells`]).

pub mod balloon;
pub mod broker;
#[cfg(target_os = "linux")]
pub mod cells;
pub mod cluster;
pub mod config;
pub mod cpu;
pub mod disk;
pub mod env;
pub mod memory;
pub mod hostinfo;
pub mod kubeconfig;
#[cfg(target_os = "linux")]
pub mod sandbox;
#[cfg(target_os = "macos")]
pub mod vm;
#[cfg(target_os = "macos")]
pub mod fsevents;
pub mod guest;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use http_body_util::BodyExt;
use hyper::body::{Bytes, Frame};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DEFAULT_PORT: u16 = 6443;

#[derive(Debug, Subcommand)]
pub enum DevCommand {
    /// Start a dev cluster and point kubectl at it (context litek8s-<name>)
    Up {
        #[arg(long, default_value = cluster::DEFAULT, help = "Cluster name (each has its own context litek8s-<name>, data and Docker socket)")]
        name: String,
        #[arg(long, help = "Nodes: the control plane and N-1 workers, joined like real nodes (default 1; remembered)")]
        nodes: Option<u32>,
        #[arg(long, help = "API server port on localhost (default: 6443 for the default cluster, else the last one; a free one if taken)")]
        port: Option<u16>,
        #[arg(long, help = "Do not touch the kubeconfig")]
        no_kubeconfig: bool,
        #[arg(long, help = "Add the context to the kubeconfig but leave the current context alone")]
        no_switch: bool,
        #[arg(long, default_value_t = 120, help = "Seconds to wait for the nodes to be Ready")]
        timeout: u64,
        #[arg(long, value_name = "NODE", help = "Start one stopped worker node of a running cluster again (see `down --node`)")]
        node: Option<String>,
        #[arg(long, help = "Memory in MiB: the macOS VM's (default 4096; only what the guest touches is used), a limit on Linux (default none); shared by all clusters")]
        memory: Option<u64>,
        #[arg(long, help = "CPUs: the macOS VM's (default: half of the Mac's), a limit on Linux (default none); shared by all clusters")]
        cpus: Option<usize>,
        #[arg(long, help = "macOS: directory shared with the VM at the same path (default: $HOME; repeat for more)")]
        share: Vec<PathBuf>,
        #[arg(long, help = "Where published ports and NodePorts listen: localhost (default) or all")]
        publish: Option<String>,
    },
    /// Stop a dev cluster (objects and images are kept for the next `up`)
    Down {
        #[arg(long, default_value = cluster::DEFAULT)]
        name: String,
        #[arg(long, help = "Stop every cluster (and the VM or sandbox)")]
        all: bool,
        #[arg(long, value_name = "NODE", help = "Stop only this worker node (node failure); `up --node` starts it again")]
        node: Option<String>,
    },
    /// Show whether a dev cluster runs, where, and what it uses
    Status {
        #[arg(long, default_value = cluster::DEFAULT)]
        name: String,
        #[arg(short = 'o', long, value_parser = ["text", "json"], default_value = "text", help = "Output format")]
        output: String,
    },
    /// List the local clusters
    #[command(alias = "ls")]
    List {
        #[arg(short = 'o', long, value_parser = ["text", "json"], default_value = "text", help = "Output format")]
        output: String,
    },
    /// Stop a cluster and delete its objects, images, volumes and kube context (same as `delete`)
    Reset {
        #[arg(long, default_value = cluster::DEFAULT)]
        name: String,
    },
    /// Stop a cluster and delete its objects, images, volumes and kube context
    Delete {
        #[arg(long, default_value = cluster::DEFAULT)]
        name: String,
    },
    /// Load images into a dev cluster: an image of the local Docker/Podman, or a `docker save` / OCI archive
    Load {
        #[arg(long, default_value = cluster::DEFAULT)]
        name: String,
        #[arg(required = true, help = "Image names (exported from the local Docker or Podman) or paths to image archives (.tar)")]
        images: Vec<String>,
    },
    /// Linux: check (and, as root, set up) what the rootless dev cluster needs
    Setup,
    #[command(hide = true)]
    Supervise {
        #[arg(long, default_value = "localhost")]
        publish: String,
        #[arg(long)]
        cpus: Option<usize>,
        #[arg(long)]
        memory: Option<u64>,
    },
    #[command(hide = true, name = "__apiserver")]
    InnerApiserver {
        #[arg(long)]
        data_dir: PathBuf,
        #[arg(long)]
        node_name: Option<String>,
        #[arg(long)]
        docker_alias: Option<String>,
        #[arg(long)]
        listen_also: Option<std::net::SocketAddr>,
        #[arg(long)]
        wireguard_port: Option<u16>,
        #[arg(long, default_value_t = 0)]
        node_port_offset: u16,
    },
    #[command(hide = true, name = "__cells")]
    Cells {
        #[arg(long)]
        guest: bool,
    },
}

/// What a running dev cluster recorded about itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DevState {
    /// The supervisor (Linux) or VM process (macOS).
    pub pid: u32,
    pub port: u16,
    /// `rootless`, `root` or `vm`.
    pub mode: String,
    pub started_unix_ms: u64,
    /// The cgroup holding everything the cluster runs (Linux).
    #[serde(default)]
    pub cgroup: Option<PathBuf>,
    /// The VM's address on the host's virtual network (macOS).
    #[serde(default)]
    pub vm_ip: Option<String>,
    /// Virtualization.framework's process running the VM (macOS).
    #[serde(default)]
    pub helper_pid: Option<u32>,
    /// What the cluster was started with (to tell whether changed settings
    /// need a restart): the VM's CPUs, memory (MiB) and shared directories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shares: Vec<PathBuf>,
    /// `localhost` or `all`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish: Option<String>,
    /// The cluster's name (absent: a cluster from before named clusters).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_socket: Option<PathBuf>,
}

pub fn dev_home() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("LITEK8S_DEV_HOME") {
        return Ok(PathBuf::from(dir));
    }
    Ok(crate::config::data_dir()?.join("dev"))
}

/// The macOS dev VM's kernel and root filesystem: `$LITEK8S_DEV_VM_ASSETS`,
/// else `<prefix>/share/litek8s/vm` beside an installed `<prefix>/bin/litek8s`
/// (how the release archives and install.sh lay them out, so the guest
/// matches the binary), else `vm/` next to the dev data directory (where
/// dev/vm/build.sh puts a guest built from source). An installation, not
/// cluster data, so `dev reset` keeps it.
pub(crate) fn vm_assets_dir(home: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("LITEK8S_DEV_VM_ASSETS") {
        return PathBuf::from(dir);
    }
    let installed = std::env::current_exe()
        .and_then(|e| e.canonicalize())
        .ok()
        .and_then(|e| Some(e.parent()?.parent()?.join("share/litek8s/vm")))
        .filter(|d| d.join("Image").is_file());
    installed.unwrap_or_else(|| home.parent().unwrap_or(home).join("vm"))
}

pub(crate) fn state_path(home: &Path) -> PathBuf {
    home.join("dev.json")
}

pub(crate) fn log_path(home: &Path) -> PathBuf {
    home.join("dev.log")
}

pub(crate) fn write_state(home: &Path, state: &DevState) -> Result<()> {
    let path = state_path(home);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn read_state(home: &Path) -> Option<DevState> {
    serde_json::from_slice(&std::fs::read(state_path(home)).ok()?).ok()
}

pub(crate) fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// The running cluster's state, if one runs.
fn running(home: &Path) -> Option<DevState> {
    read_state(home).filter(|s| pid_alive(s.pid))
}

/// The default cluster's state if it runs (what `dev status` reads).
pub fn running_state() -> Option<DevState> {
    running(&dev_home().ok()?)
}

/// A cluster's state if it runs.
pub fn cluster_state(name: &str) -> Option<DevState> {
    let home = dev_home().ok()?;
    running(&cluster::Cluster::new(name).ok()?.home(&home))
}

/// Whether the API server of a running cluster answers and its nodes are
/// Ready.
pub async fn ready(state: &DevState) -> bool {
    nodes_ready(state.port).await >= state.nodes.unwrap_or(1) as usize
}

/// Ready nodes of the cluster on `port` (0 when its API does not answer).
pub async fn ready_nodes(port: u16) -> usize {
    nodes_ready(port).await
}

/// CPU time (ns) everything the cluster runs has used: the cgroup on
/// Linux, the VM process on macOS.
pub fn cpu_time_ns(state: &DevState) -> Option<u64> {
    if let Some(cg) = &state.cgroup {
        let stat = std::fs::read_to_string(cg.join("cpu.stat")).ok()?;
        return stat.lines().find_map(|l| l.strip_prefix("usage_usec ")?.trim().parse::<u64>().ok()).map(|us| us * 1000);
    }
    #[cfg(target_os = "macos")]
    if state.mode == "vm" {
        return vm::cpu_time_ns(state);
    }
    None
}

/// Memory everything the cluster runs uses, in bytes.
pub fn memory_used(state: &DevState) -> Option<u64> {
    memory_bytes(state)
}

/// Bytes the dev data directory takes on disk (walks it: not cheap).
pub fn data_size(home: &Path) -> u64 {
    dir_size(home)
}

/// The tail of the default cluster's log.
pub fn log_lines(lines: usize) -> String {
    dev_home().map(|h| log_tail(&h, lines)).unwrap_or_default()
}

/// Where the VM or sandbox logs (the clusters' logs are their own).
pub(crate) fn env_log_path(home: &Path) -> PathBuf {
    home.join("env.log")
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/bin", "/usr/sbin", "/usr/local/bin", "/sbin", "/bin"].map(PathBuf::from))
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

fn log_tail(home: &Path, lines: usize) -> String {
    let log = std::fs::read_to_string(log_path(home)).unwrap_or_default();
    let all: Vec<&str> = log.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

pub async fn run(command: DevCommand) -> Result<()> {
    match command {
        DevCommand::Up { name, node: Some(node), .. } => node_up_down(&cluster::Cluster::new(&name)?, &node, true).await,
        DevCommand::Up { name, nodes, port, no_kubeconfig, no_switch, timeout, memory, cpus, share, publish, node: None } => {
            let cluster = cluster::Cluster::new(&name)?;
            if nodes.is_some_and(|n| !(1..=10).contains(&n)) {
                bail!("--nodes: 1 to 10");
            }
            // Flags win over ~/.litek8s/config.yaml (its port is the default
            // cluster's).
            let config = config::DevConfig::load()?;
            let publish = match publish {
                Some(p) => config::Publish::parse(&p)?,
                None => config.publish,
            };
            let shares = if share.is_empty() { config.shares.clone() } else { share };
            up(
                &cluster,
                nodes,
                UpOptions {
                    port: port.or(config.port.filter(|_| cluster.is_default())),
                    kubeconfig: !no_kubeconfig,
                    switch: !no_switch,
                    timeout: Duration::from_secs(timeout),
                    memory: memory.or(config.memory),
                    cpus: cpus.or(config.cpus),
                    shares,
                    publish,
                },
            )
            .await
        }
        DevCommand::Down { all: true, .. } => down_all().await,
        DevCommand::Down { name, node: Some(node), .. } => node_up_down(&cluster::Cluster::new(&name)?, &node, false).await,
        DevCommand::Down { name, .. } => down(&cluster::Cluster::new(&name)?).await,
        DevCommand::Status { name, output } if output == "json" => print_json(&status_json(&cluster::Cluster::new(&name)?).await?),
        DevCommand::Status { name, .. } => status(&cluster::Cluster::new(&name)?).await,
        DevCommand::List { output } if output == "json" => print_json(&list_json().await),
        DevCommand::List { .. } => list().await,
        DevCommand::Reset { name } | DevCommand::Delete { name } => delete(&cluster::Cluster::new(&name)?).await,
        DevCommand::Load { name, images } => load(&cluster::Cluster::new(&name)?, images).await,
        DevCommand::Setup => setup(),
        DevCommand::Supervise { publish, cpus, memory } => supervise(config::Publish::parse(&publish)?, cpus, memory).await,
        DevCommand::InnerApiserver { data_dir, node_name, docker_alias, listen_also, wireguard_port, node_port_offset } => {
            inner_apiserver(InnerOptions { data_dir, node_name, docker_alias, listen_also, wireguard_port, node_port_offset }).await
        }
        DevCommand::Cells { guest } => cells_main(guest).await,
    }
}

pub struct UpOptions {
    pub port: Option<u16>,
    pub kubeconfig: bool,
    /// Make the cluster's context the current one.
    pub switch: bool,
    pub timeout: Duration,
    pub memory: Option<u64>,
    pub cpus: Option<usize>,
    /// Shared directories (macOS; empty: $HOME).
    pub shares: Vec<PathBuf>,
    pub publish: config::Publish,
}

fn server(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

async fn up(cluster: &cluster::Cluster, nodes: Option<u32>, opts: UpOptions) -> Result<()> {
    let started = Instant::now();
    let home = dev_home()?;
    std::fs::create_dir_all(&home).with_context(|| format!("failed to create {}", home.display()))?;
    let chome = cluster.home(&home);
    let context = cluster.context();
    if let Some(state) = running(&chome) {
        if opts.kubeconfig {
            kubeconfig::install(&kubeconfig::default_path(), &context, &server(state.port), opts.switch)?;
        }
        println!("{context} is already running ({}, {}, context {context})", state.mode, server(state.port));
        return Ok(());
    }
    let lock = env::lock(&home)?;
    let env_state = match env::running(&home) {
        Some(e) => {
            warn_settings(&e, &opts);
            e
        }
        None => {
            if let Some(old) = read_state(&home).filter(|s| pid_alive(s.pid) && s.name.is_none()) {
                bail!("a dev cluster started by an older litek8s is running (pid {}): stop it with `litek8s dev down` first", old.pid);
            }
            start_env(&home, &opts).await?
        }
    };
    let mut meta = cluster.meta(&home).unwrap_or(cluster::Meta { nodes: 1, created_unix_ms: now_ms(), port: None });
    if meta.created_unix_ms == 0 {
        meta.created_unix_ms = now_ms();
    }
    if let Some(n) = nodes {
        meta.nodes = n;
    }
    cluster.save_meta(&home, &meta)?;
    let reply = env::request(&home, &env::Request::Start { name: cluster.name.clone(), port: opts.port, nodes: meta.nodes }, Duration::from_secs(90)).await?;
    drop(lock);
    if let Some(e) = reply.error {
        bail!("{context} failed to start: {e}\n{}", logs_tail(&home, cluster, 20));
    }
    let port = reply.port.context("the dev environment did not say which port")?;
    meta.port = Some(port);
    cluster.save_meta(&home, &meta)?;
    wait_ready(&home, cluster, port, meta.nodes, opts.timeout).await?;
    if opts.kubeconfig {
        kubeconfig::install(&kubeconfig::default_path(), &context, &server(port), opts.switch)?;
    }
    let nodes = if meta.nodes > 1 { format!("{} nodes, ", meta.nodes) } else { String::new() };
    println!("{context} ready in {:.2}s ({}{}, {}, kubectl context {context})", started.elapsed().as_secs_f64(), nodes, env_state.mode, server(port));
    Ok(())
}

/// Settings of a running VM or sandbox apply to every cluster in it.
fn warn_settings(env: &env::EnvState, opts: &UpOptions) {
    let mut differ = Vec::new();
    if opts.cpus.is_some() && opts.cpus != env.cpus {
        differ.push("CPUs");
    }
    if opts.memory.is_some() && opts.memory != env.memory_mib {
        differ.push("memory");
    }
    if env.publish.as_deref().is_some_and(|p| p != opts.publish.as_str()) {
        differ.push("publish");
    }
    if !differ.is_empty() {
        let what = if env.mode == "vm" { "VM" } else { "sandbox" };
        eprintln!("note: the {what} the clusters share is running with other {}; they apply once every cluster is down", differ.join(", "));
    }
}

/// Start the VM or sandbox and wait until it takes requests.
async fn start_env(home: &Path, opts: &UpOptions) -> Result<env::EnvState> {
    let _ = std::fs::remove_file(env::state_path(home));
    let pid = start(home, opts)?;
    let deadline = Instant::now() + opts.timeout;
    loop {
        if Instant::now() > deadline {
            bail!("the dev environment did not start in {}s; log ({}):\n{}", opts.timeout.as_secs(), env_log_path(home).display(), tail(&env_log_path(home), 30));
        }
        if !pid_alive(pid) {
            bail!("the dev environment exited during startup; log ({}):\n{}", env_log_path(home).display(), tail(&env_log_path(home), 30));
        }
        if let Some(state) = env::read_state(home).filter(|s| s.pid == pid) {
            if tokio::net::UnixStream::connect(&state.control).await.is_ok() {
                return Ok(state);
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Start the environment's long-running process in the background; its pid.
#[cfg(target_os = "linux")]
fn start(home: &Path, opts: &UpOptions) -> Result<u32> {
    use std::os::unix::process::CommandExt;
    let rootless = !is_root();
    if rootless {
        let problems = preflight();
        if !problems.is_empty() {
            bail!("the rootless dev cluster cannot start:\n  - {}\nSee `litek8s dev setup`.", problems.join("\n  - "));
        }
    }
    let exe = std::env::current_exe()?;
    let log = std::fs::File::create(env_log_path(home))?;
    // Rootless, the sandbox needs a cgroup of its own the user may manage:
    // a systemd scope with delegation (what rootless Podman does too).
    let mut cmd = if rootless && which("systemd-run").is_some() {
        let mut c = std::process::Command::new("systemd-run");
        c.args(["--user", "--scope", "--quiet", "--collect", "-p", "Delegate=yes"]);
        c.arg(format!("--unit=litek8s-dev-{}", now_ms()));
        c.arg(&exe);
        c
    } else {
        std::process::Command::new(&exe)
    };
    cmd.args(["dev", "supervise", "--publish", opts.publish.as_str()]);
    if let Some(c) = opts.cpus {
        cmd.arg("--cpus").arg(c.to_string());
    }
    if let Some(m) = opts.memory {
        cmd.arg("--memory").arg(m.to_string());
    }
    cmd.stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // SAFETY: setsid only.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().context("failed to start the dev sandbox")?;
    Ok(child.id())
}

#[cfg(target_os = "macos")]
fn start(home: &Path, opts: &UpOptions) -> Result<u32> {
    vm::start(home, opts)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn start(_: &Path, _: &UpOptions) -> Result<u32> {
    bail!("litek8s dev runs on Linux and macOS")
}

fn tail(path: &Path, lines: usize) -> String {
    let log = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = log.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// The ends of a cluster's log and of the environment's.
fn logs_tail(home: &Path, cluster: &cluster::Cluster, lines: usize) -> String {
    let clog = log_path(&cluster.home(home));
    format!("log ({}):\n{}\nVM/sandbox log ({}):\n{}", clog.display(), tail(&clog, lines), env_log_path(home).display(), tail(&env_log_path(home), lines))
}

async fn wait_ready(home: &Path, cluster: &cluster::Cluster, port: u16, nodes: u32, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let chome = cluster.home(home);
    loop {
        if Instant::now() > deadline {
            let ready = nodes_ready(port).await;
            bail!("{} did not get ready in {}s ({ready} of {nodes} nodes Ready); {}", cluster.context(), timeout.as_secs(), logs_tail(home, cluster, 30));
        }
        if running(&chome).is_none() {
            bail!("{} stopped during startup; {}", cluster.context(), logs_tail(home, cluster, 30));
        }
        if nodes_ready(port).await >= nodes as usize {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn nodes_ready(port: u16) -> usize {
    let Ok((status, body)) = http(port, hyper::Method::GET, "/api/v1/nodes", None).await else { return 0 };
    if !status.is_success() {
        return 0;
    }
    let Ok(list) = serde_json::from_slice::<Value>(&body) else { return 0 };
    list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|n| {
            n.pointer("/status/conditions")
                .and_then(Value::as_array)
                .is_some_and(|c| c.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"))
        })
        .count()
}

/// The API listener on 127.0.0.1: the port asked for; else the one the
/// cluster had (6443 for the default one) when it is free; else any.
pub(crate) fn bind_api(port: Option<u16>, preferred: Option<u16>) -> Result<std::net::TcpListener> {
    if let Some(p) = port {
        return std::net::TcpListener::bind(("127.0.0.1", p)).with_context(|| format!("failed to listen on 127.0.0.1:{p}"));
    }
    if let Some(p) = preferred {
        match std::net::TcpListener::bind(("127.0.0.1", p)) {
            Ok(l) => return Ok(l),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => eprintln!("dev: port {p} is taken, using a free one"),
            Err(e) => return Err(e).with_context(|| format!("failed to listen on 127.0.0.1:{p}")),
        }
    }
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?)
}

type UploadBody = http_body_util::combinators::UnsyncBoxBody<Bytes, std::io::Error>;

/// One request to the dev API server on localhost.
pub(crate) async fn http(port: u16, method: hyper::Method, path: &str, body: Option<UploadBody>) -> Result<(hyper::StatusCode, Bytes)> {
    let tcp = tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(("127.0.0.1", port))).await??;
    let _ = tcp.set_nodelay(true);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tcp)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let body = body.unwrap_or_else(|| http_body_util::Empty::new().map_err(|never| match never {}).boxed_unsync());
    let req = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, format!("127.0.0.1:{port}"))
        .header(hyper::header::CONTENT_TYPE, "application/x-tar")
        .body(body)?;
    let resp = sender.send_request(req).await?;
    let status = resp.status();
    let bytes = resp.into_body().collect().await?.to_bytes();
    Ok((status, bytes))
}

async fn down(cluster: &cluster::Cluster) -> Result<()> {
    match stop_cluster(cluster).await? {
        None => println!("{} is not running", cluster.context()),
        Some(rest) => {
            let flag = if cluster.is_default() { String::new() } else { format!(" --name {}", cluster.name) };
            println!("{} stopped (data kept; `litek8s dev delete{flag}` deletes it){rest}", cluster.context());
        }
    }
    Ok(())
}

/// Stop a cluster; `None` if it was not running, else what still runs.
async fn stop_cluster(cluster: &cluster::Cluster) -> Result<Option<String>> {
    let home = dev_home()?;
    let chome = cluster.home(&home);
    let context = cluster.context();
    let Some(state) = running(&chome) else {
        return Ok(None);
    };
    if env::running(&home).is_none() || state.name.is_none() {
        // Started by an older litek8s: its own process.
        stop(&state).await?;
        return Ok(Some(String::new()));
    }
    let env_pid = env::running(&home).map(|e| e.pid);
    let reply = env::request(&home, &env::Request::Stop { name: cluster.name.clone() }, Duration::from_secs(60)).await?;
    if let Some(e) = reply.error {
        bail!("stopping {context}: {e}");
    }
    // The last one: the VM or sandbox goes too; wait for it.
    if let Some(pid) = env_pid.filter(|_| reply.running.is_empty()) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while pid_alive(pid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    Ok(Some(if reply.running.is_empty() {
        format!("; the {} stopped too", if state.mode == "vm" { "VM" } else { "sandbox" })
    } else {
        format!("; still running: {}", reply.running.join(", "))
    }))
}

/// Stop or start one worker node of a running cluster.
async fn node_up_down(cluster: &cluster::Cluster, node: &str, up: bool) -> Result<()> {
    let home = dev_home()?;
    let state = running(&cluster.home(&home)).with_context(|| format!("{} is not running", cluster.context()))?;
    let nodes = state.nodes.unwrap_or(1);
    let index = (0..nodes)
        .find(|i| cluster.node_name_at(*i) == node || i.to_string() == node)
        .with_context(|| format!("{} has no node {node} (its nodes: {})", cluster.context(), (0..nodes).map(|i| cluster.node_name_at(i)).collect::<Vec<_>>().join(", ")))?;
    let reply = env::request(&home, &env::Request::Node { name: cluster.name.clone(), node: index, up }, Duration::from_secs(60)).await?;
    if let Some(e) = reply.error {
        bail!("{e}");
    }
    let name = cluster.node_name_at(index);
    if up {
        println!("{name} started again; it is Ready once it reconnects");
    } else {
        println!("{name} stopped: its pods stop, and the node goes NotReady after the grace period, as when a machine fails");
    }
    Ok(())
}

async fn down_all() -> Result<()> {
    let home = dev_home()?;
    if env::running(&home).is_none() {
        if let Some(state) = running(&home) {
            stop(&state).await?;
        }
        println!("no local cluster is running");
        return Ok(());
    }
    let pid = env::running(&home).map(|e| e.pid);
    let _ = env::request(&home, &env::Request::Shutdown, Duration::from_secs(120)).await;
    if let Some(pid) = pid {
        let deadline = Instant::now() + Duration::from_secs(30);
        while pid_alive(pid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    println!("every local cluster stopped");
    Ok(())
}

async fn stop(state: &DevState) -> Result<()> {
    // SAFETY: plain signal to the cluster's process.
    unsafe { libc::kill(state.pid as i32, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(25);
    while pid_alive(state.pid) {
        if Instant::now() > deadline {
            // SAFETY: as above.
            unsafe { libc::kill(state.pid as i32, libc::SIGKILL) };
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

/// The shown cluster's memory explained and a CPU sample (Home): the VM's
/// footprint and CPU time on macOS, the sandbox's cgroup on Linux, split
/// by owner. `None`s without a running VM or sandbox.
pub async fn resources(cluster: &str) -> (Option<memory::Breakdown>, Option<cpu::Sample>) {
    let Some((home, e)) = dev_home().ok().and_then(|h| Some((h.clone(), env::running(&h)?))) else { return (None, None) };
    let split = |stats: &[env::CellStats]| {
        let shown = stats.iter().find(|s| s.name == cluster).and_then(|s| s.usage).unwrap_or_default();
        let others: Vec<memory::Usage> = stats.iter().filter(|s| s.name != cluster).filter_map(|s| s.usage).collect();
        (shown, others)
    };
    let cpu_split = |stats: &[env::CellStats]| {
        let shown = stats.iter().find(|s| s.name == cluster).and_then(|s| s.cpu.clone()).unwrap_or_default();
        let others: Vec<&env::CellStats> = stats.iter().filter(|s| s.name != cluster).collect();
        let others_usec = others.iter().filter_map(|s| s.cpu.as_ref()).map(|c| c.litek8s_usec + c.containers_usec()).sum();
        (shown, others_usec, others.len())
    };
    let host_cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
    if e.mode == "vm" {
        let Ok(r) = env::request(&home, &env::Request::Stats, Duration::from_secs(3)).await else { return (None, None) };
        let (shown, others) = split(&r.stats);
        let mem = (|| Some(memory::vm_breakdown(env_memory(&e)?, e.memory_mib? << 20, r.meminfo.as_ref()?, r.balloon.unwrap_or(0), r.memory_limit, cluster, shown, &others)))();
        let (cshown, others_usec, n) = cpu_split(&r.stats);
        let cpu = env_cpu_time_ns(&e).map(|ns| cpu::Sample {
            at: Some(std::time::Instant::now()),
            total_usec: ns / 1000,
            shown: cshown,
            others_usec,
            others: n,
            guest_busy_usec: r.proc_stat.map(|p| p.busy_usec),
            cpus: e.cpus.map(|c| c as f64).unwrap_or(host_cpus),
            kind: cpu::Kind::Vm,
        });
        return (mem, cpu);
    }
    let Some(base) = e.cgroup.clone() else { return (None, None) };
    let stats = env::cell_stats(&base.join("sandbox"));
    let mem = memory::cgroup(&base).map(|(used, cache)| {
        let (shown, others) = split(&stats);
        let limit = std::fs::read_to_string(base.join("memory.max")).ok().and_then(|v| v.trim().parse::<u64>().ok());
        memory::sandbox_breakdown(used, cache, cluster, shown, &others, limit, memory::meminfo().as_ref())
    });
    let (cshown, others_usec, n) = cpu_split(&stats);
    let cpu = cpu::usage_usec(&base).map(|total| cpu::Sample {
        at: Some(std::time::Instant::now()),
        total_usec: total,
        shown: cshown,
        others_usec,
        others: n,
        guest_busy_usec: None,
        cpus: e.cpus.map(|c| c as f64).unwrap_or(host_cpus),
        kind: cpu::Kind::Sandbox,
    });
    (mem, cpu)
}

/// Memory and CPU time of each running cluster, from the VM or sandbox.
pub async fn stats() -> Vec<env::CellStats> {
    let Ok(home) = dev_home() else { return Vec::new() };
    match env::running(&home) {
        Some(e) if e.mode != "vm" => e.cgroup.map(|c| env::cell_stats(&c.join("sandbox"))).unwrap_or_default(),
        Some(_) => env::request(&home, &env::Request::Stats, Duration::from_secs(3)).await.map(|r| r.stats).unwrap_or_default(),
        None => Vec::new(),
    }
}

/// The running VM or sandbox the clusters share.
pub fn env_state() -> Option<env::EnvState> {
    env::running(&dev_home().ok()?)
}

/// Memory the whole VM or sandbox uses, in bytes.
pub fn env_memory(e: &env::EnvState) -> Option<u64> {
    if let Some(cg) = &e.cgroup {
        return std::fs::read_to_string(cg.join("memory.current")).ok()?.trim().parse().ok();
    }
    #[cfg(target_os = "macos")]
    if e.mode == "vm" {
        return vm::footprint_of(e.helper_pid?);
    }
    None
}

/// CPU time (ns) the whole VM or sandbox has used.
pub fn env_cpu_time_ns(e: &env::EnvState) -> Option<u64> {
    if let Some(cg) = &e.cgroup {
        let stat = std::fs::read_to_string(cg.join("cpu.stat")).ok()?;
        return stat.lines().find_map(|l| l.strip_prefix("usage_usec ")?.trim().parse::<u64>().ok()).map(|us| us * 1000);
    }
    #[cfg(target_os = "macos")]
    if e.mode == "vm" {
        return vm::cpu_time_ns_of(e.helper_pid?);
    }
    None
}

fn mib(b: u64) -> String {
    format!("{:.1} MiB", b as f64 / 1048576.0)
}

async fn status(cluster: &cluster::Cluster) -> Result<()> {
    use std::fmt::Write as _;
    use std::io::Write as _;
    let mut text = String::new();
    // Collected and written at once: a closed pipe (`| head`) is not an error.
    macro_rules! out {
        ($($arg:tt)*) => {{
            let _ = writeln!(text, $($arg)*);
        }};
    }
    let home = dev_home()?;
    let chome = cluster.home(&home);
    let context = cluster.context();
    let Some(state) = running(&chome) else {
        if cluster.exists(&home) {
            out!("{context} is not running (data: {})", chome.display());
        } else {
            out!("{context} does not exist (`litek8s dev up{}` creates it)", if cluster.is_default() { String::new() } else { format!(" --name {}", cluster.name) });
        }
        let _ = std::io::stdout().write_all(text.as_bytes());
        return Ok(());
    };
    let uptime = Duration::from_millis(now_ms().saturating_sub(state.started_unix_ms));
    out!("{context}: running ({}), pid {}, up {}s", state.mode, state.pid, uptime.as_secs());
    out!("  server:   {} (kubectl context {context})", server(state.port));
    if let Some(s) = &state.docker_socket {
        out!("  docker:   unix://{}", s.display());
    }
    if let Some(ip) = &state.vm_ip {
        out!("  vm:       {ip}");
    }
    if let Ok((_, body)) = http(state.port, hyper::Method::GET, "/api/v1/pods", None).await {
        if let Ok(list) = serde_json::from_slice::<Value>(&body) {
            let pods = list["items"].as_array().cloned().unwrap_or_default();
            let running = pods.iter().filter(|p| p.pointer("/status/phase").and_then(Value::as_str) == Some("Running")).count();
            out!("  nodes:    {} Ready of {}", nodes_ready(state.port).await, state.nodes.unwrap_or(1));
            out!("  pods:     {running} running, {} total", pods.len());
        }
    }
    let own = stats().await.into_iter().find(|s| s.name == cluster.name);
    if let Some(s) = own {
        out!("  memory:   {} (this cluster's nodes and pods)", mib(s.memory_bytes));
    } else if let Some(mem) = memory_bytes(&state) {
        out!("  memory:   {} (everything the cluster runs, pods included)", mib(mem));
    }
    if let Some(e) = env::running(&home) {
        let others: Vec<String> = cluster::list(&home).into_iter().filter(|c| c.name != cluster.name && running(&c.home(&home)).is_some()).map(|c| c.name).collect();
        let what = if e.mode == "vm" { "VM" } else { "sandbox" };
        let mut mem = env_memory(&e).map(|m| format!(", {}", mib(m))).unwrap_or_default();
        // The VM's ceiling, when the guest may not use all of its memory.
        if e.mode == "vm" {
            let limit = env::request(&home, &env::Request::Stats, Duration::from_secs(3)).await.ok().and_then(|r| r.memory_limit);
            if let (Some(l), Some(m)) = (limit, e.memory_mib) {
                if l < m << 20 {
                    mem.push_str(&format!(", may use {} of {m} MiB now", l >> 20));
                }
            }
        }
        let with = if others.is_empty() { "no other cluster".to_string() } else { others.join(", ") };
        out!("  shared:   the {what} (pid {}{mem}) with {with}", e.pid);
    }
    let data = if cfg!(target_os = "macos") { format!("{} (in the VM's disk)", cluster.guest_data_dir().display()) } else { format!("{} ({})", cluster.data_dir(&home).display(), mib(dir_size(&cluster.data_dir(&home)))) };
    out!("  data:     {data}");
    let _ = std::io::stdout().write_all(text.as_bytes());
    Ok(())
}

fn print_json(v: &Value) -> Result<()> {
    use std::io::Write as _;
    // A closed pipe (`| head`) is not an error.
    let _ = writeln!(std::io::stdout(), "{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// `dev status -o json`: what `dev status` prints, for scripts and agents.
pub async fn status_json(cluster: &cluster::Cluster) -> Result<Value> {
    let home = dev_home()?;
    let chome = cluster.home(&home);
    let mut v = serde_json::json!({
        "name": cluster.name,
        "context": cluster.context(),
        "exists": cluster.exists(&home),
        "running": false,
        "docker_socket": cluster.docker_socket(),
    });
    let Some(state) = running(&chome) else { return Ok(v) };
    v["running"] = true.into();
    v["mode"] = state.mode.clone().into();
    v["pid"] = state.pid.into();
    v["uptime_seconds"] = (now_ms().saturating_sub(state.started_unix_ms) / 1000).into();
    v["server"] = server(state.port).into();
    if let Some(s) = &state.docker_socket {
        v["docker_socket"] = serde_json::json!(s);
    }
    if let Some(ip) = &state.vm_ip {
        v["vm_ip"] = ip.clone().into();
    }
    v["nodes"] = state.nodes.unwrap_or(1).into();
    if let Ok((_, body)) = http(state.port, hyper::Method::GET, "/api/v1/pods", None).await {
        if let Ok(list) = serde_json::from_slice::<Value>(&body) {
            let pods = list["items"].as_array().cloned().unwrap_or_default();
            let running = pods.iter().filter(|p| p.pointer("/status/phase").and_then(Value::as_str) == Some("Running")).count();
            v["nodes_ready"] = nodes_ready(state.port).await.into();
            v["pods_running"] = running.into();
            v["pods_total"] = pods.len().into();
        }
    }
    let mem = stats().await.into_iter().find(|s| s.name == cluster.name).map(|s| s.memory_bytes).or_else(|| memory_bytes(&state));
    if let Some(m) = mem {
        v["memory_bytes"] = m.into();
    }
    v["data_dir"] = if cfg!(target_os = "macos") { serde_json::json!(cluster.guest_data_dir()) } else { serde_json::json!(cluster.data_dir(&home)) };
    Ok(v)
}

/// `dev list -o json`.
pub async fn list_json() -> Value {
    let rows: Vec<Value> = clusters()
        .await
        .into_iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "context": c.context,
                "running": c.running.is_some(),
                "server": c.running.as_ref().map(|s| server(s.port)),
                "nodes": c.nodes,
                "memory_bytes": c.memory,
                "created_unix_ms": c.created_unix_ms,
                "docker_socket": c.docker_socket,
            })
        })
        .collect();
    let env = env_state().map(|e| serde_json::json!({"mode": e.mode, "pid": e.pid, "cpus": e.cpus, "memory_mib": e.memory_mib, "memory_bytes": env_memory(&e)}));
    serde_json::json!({"clusters": rows, "environment": env})
}

/// One row of `dev list` and of the UI's Clusters view.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClusterInfo {
    pub name: String,
    pub context: String,
    pub running: Option<DevState>,
    pub nodes: u32,
    pub memory: Option<u64>,
    pub created_unix_ms: Option<u64>,
    pub docker_socket: PathBuf,
}

/// The local clusters and what they use (one Stats request for all).
pub async fn clusters() -> Vec<ClusterInfo> {
    let Ok(home) = dev_home() else { return Vec::new() };
    let stats = stats().await;
    cluster::list(&home)
        .into_iter()
        .map(|c| {
            let chome = c.home(&home);
            let meta = c.meta(&home);
            let running = running(&chome);
            let created = meta.as_ref().map(|m| m.created_unix_ms).filter(|t| *t > 0).or_else(|| {
                let m = std::fs::metadata(log_path(&chome)).ok()?.modified().ok()?;
                Some(m.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64)
            });
            ClusterInfo {
                context: c.context(),
                memory: running.as_ref().and_then(|r| stats.iter().find(|s| s.name == c.name).map(|s| s.memory_bytes).or_else(|| if r.name.is_none() { memory_bytes(r) } else { None })),
                nodes: running.as_ref().and_then(|r| r.nodes).or(meta.map(|m| m.nodes)).unwrap_or(1),
                created_unix_ms: created,
                docker_socket: c.docker_socket(),
                running,
                name: c.name,
            }
        })
        .collect()
}

pub fn age(ms: u64) -> String {
    let s = now_ms().saturating_sub(ms) / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

async fn list() -> Result<()> {
    use std::fmt::Write as _;
    use std::io::Write as _;
    let all = clusters().await;
    let mut text = String::new();
    let _ = writeln!(text, "{:<12} {:<8} {:<17} {:<6} {:<10} {:<5} CONTEXT", "NAME", "STATE", "API", "NODES", "MEMORY", "AGE");
    for c in &all {
        let (state, api) = match &c.running {
            Some(s) => ("running", format!("127.0.0.1:{}", s.port)),
            None => ("stopped", "-".to_string()),
        };
        let mem = c.memory.map(mib).unwrap_or_else(|| "-".into());
        let age = c.created_unix_ms.map(age).unwrap_or_else(|| "-".into());
        let _ = writeln!(text, "{:<12} {:<8} {:<17} {:<6} {:<10} {:<5} {}", c.name, state, api, c.nodes, mem, age, c.context);
    }
    if all.is_empty() {
        text = "no local clusters (`litek8s dev up` creates one)\n".into();
    } else if let Some(e) = env_state() {
        let what = if e.mode == "vm" { "VM" } else { "sandbox" };
        let size = match (e.cpus, e.memory_mib) {
            (Some(c), Some(m)) => format!(", {c} CPUs, {m} MiB"),
            (None, Some(m)) => format!(", {m} MiB"),
            (Some(c), None) => format!(", {c} CPUs"),
            _ => String::new(),
        };
        let used = env_memory(&e).map(|m| format!(", {} in use", mib(m))).unwrap_or_default();
        let _ = writeln!(text, "\nshared {what} ({}{size}{used})", e.mode);
    }
    let _ = std::io::stdout().write_all(text.as_bytes());
    Ok(())
}

/// Memory the whole cluster uses: its cgroup on Linux, the VM process's
/// footprint on macOS.
fn memory_bytes(state: &DevState) -> Option<u64> {
    if let Some(cg) = &state.cgroup {
        return std::fs::read_to_string(cg.join("memory.current")).ok()?.trim().parse().ok();
    }
    #[cfg(target_os = "macos")]
    if state.mode == "vm" {
        return vm::footprint(state);
    }
    None
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    // Hard links count once (busybox images link hundreds of names to one file).
    #[cfg(unix)]
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            match e.metadata() {
                Ok(m) if m.is_dir() => stack.push(e.path()),
                #[cfg(unix)]
                Ok(m) => {
                    use std::os::unix::fs::MetadataExt;
                    if m.nlink() < 2 || seen.insert((m.dev(), m.ino())) {
                        total += m.blocks() * 512;
                    }
                }
                #[cfg(not(unix))]
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

async fn delete(cluster: &cluster::Cluster) -> Result<()> {
    let home = dev_home()?;
    let chome = cluster.home(&home);
    let context = cluster.context();
    if !cluster.exists(&home) && !chome.exists() {
        println!("{context} does not exist");
        kubeconfig::uninstall(&kubeconfig::default_path(), &context)?;
        return Ok(());
    }
    stop_cluster(cluster).await?;
    // Its files on this machine.
    if cluster.is_default() {
        remove_data(&cluster.data_dir(&home))?;
        for f in ["dev.json", "dev.log", "cluster.json"] {
            let _ = std::fs::remove_file(home.join(f));
        }
    } else {
        remove_data(&chome)?;
    }
    if !cluster.is_default() {
        if let Some(dir) = cluster.docker_socket().parent() {
            let _ = std::fs::remove_dir(dir);
        }
    }
    // Its data inside the VM.
    #[cfg(target_os = "macos")]
    vm::purge(&home, cluster).await?;
    kubeconfig::uninstall(&kubeconfig::default_path(), &context)?;
    println!("{context} deleted: its objects, images and volumes are gone, context {context} removed");
    Ok(())
}

fn remove_data(home: &Path) -> Result<()> {
    match std::fs::remove_dir_all(home) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        #[cfg(target_os = "linux")]
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && !is_root() => {
            // Image layers are owned by the user's subordinate ids.
            supervisor::remove_in_userns(home)
        }
        Err(e) => Err(e).with_context(|| format!("failed to delete {}", home.display())),
    }
}

async fn load(cluster: &cluster::Cluster, images: Vec<String>) -> Result<()> {
    let home = dev_home()?;
    let state = running(&cluster.home(&home)).with_context(|| format!("{} is not running (`litek8s dev up` first)", cluster.context()))?;
    for image in images {
        let started = Instant::now();
        let path = Path::new(&image);
        let (body, what): (UploadBody, String) = if path.is_file() {
            let file = tokio::fs::File::open(path).await?;
            (file_body(file), path.display().to_string())
        } else {
            (export_local_image(&image).await?, image.clone())
        };
        let (status, reply) = http(state.port, hyper::Method::POST, crate::kube::server::IMAGES_PATH, Some(body)).await?;
        let reply: Value = serde_json::from_slice(&reply).unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("loading {what} failed: {}", reply["message"].as_str().unwrap_or(status.as_str()));
        }
        let names: Vec<&str> = reply["images"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        println!("loaded {what} in {:.1}s: {}", started.elapsed().as_secs_f64(), names.join(", "));
    }
    Ok(())
}

fn file_body(file: tokio::fs::File) -> UploadBody {
    use tokio::io::AsyncReadExt;
    let stream = futures_util::stream::unfold(file, |mut file| async move {
        let mut buf = vec![0u8; 1 << 20];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(Frame::data(Bytes::from(buf))), file))
            }
            Err(e) => Some((Err(e), file)),
        }
    });
    BodyExt::boxed_unsync(http_body_util::StreamBody::new(stream))
}

/// `docker save <image>` from the local Docker (or Podman's Docker API),
/// streamed.
async fn export_local_image(image: &str) -> Result<UploadBody> {
    use futures_util::StreamExt;
    let docker = crate::runtime::DockerRuntime::connect()
        .and_then(|d| Ok(d.client()))
        .with_context(|| format!("{image} is not a file and there is no local Docker to export it from"))?;
    docker
        .inspect_image(image)
        .await
        .with_context(|| format!("image {image} is not in the local Docker (pull or build it, or pass a `docker save` archive)"))?;
    let stream = docker.export_image(image).map(|chunk| chunk.map(Frame::data).map_err(std::io::Error::other));
    Ok(BodyExt::boxed_unsync(http_body_util::StreamBody::new(stream)))
}

#[cfg(target_os = "linux")]
fn is_root() -> bool {
    // SAFETY: no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// What the rootless sandbox needs and this machine lacks.
#[cfg(target_os = "linux")]
pub fn preflight() -> Vec<String> {
    let mut problems = Vec::new();
    if crate::kube::agent::crun_binary().is_none() {
        problems.push("crun is not installed (package `crun`)".into());
    }
    for tool in ["newuidmap", "newgidmap"] {
        if which(tool).is_none() {
            problems.push(format!("{tool} is not installed (package `uidmap` / `shadow-utils`)"));
        }
    }
    if which("pasta").is_none() && which("slirp4netns").is_none() {
        problems.push("neither pasta (package `passt`) nor slirp4netns is installed: pods would have no network".into());
    }
    for tool in ["ip", "iptables-restore"] {
        if which(tool).is_none() {
            problems.push(format!("{tool} is not installed"));
        }
    }
    // SAFETY: no preconditions.
    let uid = unsafe { libc::getuid() };
    let user = sandbox::user_name(uid);
    for file in ["/etc/subuid", "/etc/subgid"] {
        match sandbox::subordinate_range(file, &user, uid) {
            Some((_, count)) if count >= 65536 => {}
            Some((_, count)) => problems.push(format!("{file} gives {user} only {count} ids (65536 needed)")),
            None => problems.push(format!("{file} has no range for {user} (`sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 {user}`)")),
        }
    }
    if !Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        problems.push("cgroup v2 is not mounted at /sys/fs/cgroup".into());
    }
    if sandbox::apparmor_restricts_userns() && !apparmor_profile_installed() {
        problems.push("AppArmor restricts unprivileged user namespaces here: run `sudo litek8s dev setup` once to allow litek8s".into());
    }
    problems
}

#[cfg(target_os = "linux")]
fn apparmor_profile_installed() -> bool {
    let exe = std::env::current_exe().ok().and_then(|e| e.canonicalize().ok()).unwrap_or_default();
    std::fs::read_to_string(hostinfo::APPARMOR_PROFILE_PATH).is_ok_and(|p| p.contains(&format!("profile litek8s {} ", exe.display())))
}

#[cfg(target_os = "linux")]
fn setup() -> Result<()> {
    if is_root() {
        if sandbox::apparmor_restricts_userns() {
            let exe = std::env::current_exe()?.canonicalize()?;
            std::fs::write(hostinfo::APPARMOR_PROFILE_PATH, hostinfo::apparmor_profile(&exe))?;
            let status = std::process::Command::new("apparmor_parser").arg("-r").arg(hostinfo::APPARMOR_PROFILE_PATH).status()?;
            if !status.success() {
                bail!("apparmor_parser failed to load {}", hostinfo::APPARMOR_PROFILE_PATH);
            }
            println!("installed AppArmor profile {} (user namespaces for {})", hostinfo::APPARMOR_PROFILE_PATH, exe.display());
        } else {
            println!("AppArmor does not restrict user namespaces here; nothing to install");
        }
        println!("run `litek8s dev up` as your user (root runs the cluster without a user namespace)");
        return Ok(());
    }
    let problems = preflight();
    if problems.is_empty() {
        println!("ready for the rootless dev cluster: `litek8s dev up`");
    } else {
        println!("the rootless dev cluster needs:\n  - {}", problems.join("\n  - "));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn setup() -> Result<()> {
    println!("nothing to set up on this platform; `litek8s dev up` starts the dev VM");
    Ok(())
}

#[cfg(target_os = "linux")]
mod supervisor;

#[cfg(target_os = "linux")]
async fn supervise(publish: config::Publish, cpus: Option<usize>, memory: Option<u64>) -> Result<()> {
    supervisor::run(publish, cpus, memory).await
}

#[cfg(not(target_os = "linux"))]
async fn supervise(_: config::Publish, _: Option<usize>, _: Option<u64>) -> Result<()> {
    bail!("the dev supervisor runs on Linux")
}

#[cfg(target_os = "linux")]
async fn cells_main(guest: bool) -> Result<()> {
    tokio::task::spawn_blocking(move || cells::run(guest)).await?
}

#[cfg(not(target_os = "linux"))]
async fn cells_main(_: bool) -> Result<()> {
    bail!("the cell manager runs on Linux")
}

/// The API listener and host-port broker the supervisor hands the sandbox
/// as inherited descriptors.
#[cfg(target_os = "linux")]
fn sandbox_listener() -> Result<(Option<std::net::TcpListener>, std::net::SocketAddr)> {
    use std::os::fd::FromRawFd;
    broker::install(sandbox::FD_BROKER);
    // SAFETY: the supervisor passes the API listener as this descriptor.
    let listener = unsafe { std::net::TcpListener::from_raw_fd(sandbox::FD_API_LISTENER) };
    let addr = listener.local_addr().context("no API listener from the supervisor")?;
    Ok((Some(listener), addr))
}

#[cfg(not(target_os = "linux"))]
fn sandbox_listener() -> Result<(Option<std::net::TcpListener>, std::net::SocketAddr)> {
    bail!("the dev sandbox runs on Linux")
}

struct InnerOptions {
    data_dir: PathBuf,
    node_name: Option<String>,
    docker_alias: Option<String>,
    /// Another address to serve the API on (the cell's, for joining nodes).
    listen_also: Option<std::net::SocketAddr>,
    wireguard_port: Option<u16>,
    node_port_offset: u16,
}

/// The API server inside a cell of the sandbox or the VM.
async fn inner_apiserver(o: InnerOptions) -> Result<()> {
    let guest = std::env::var("LITEK8S_DEV_GUEST").is_ok_and(|v| v == "1");
    let (listener, listen): (Option<std::net::TcpListener>, std::net::SocketAddr) = if guest {
        // In the VM: the Mac reaches it over vsock (see guest::VSOCK_PORT).
        (None, std::net::SocketAddr::from(([127, 0, 0, 1], guest::GUEST_API_PORT)))
    } else {
        sandbox_listener()?
    };
    // SAFETY: set before any other thread reads the environment.
    unsafe { std::env::set_var("LITEK8S_LOADBALANCER_IP", "127.0.0.1") };
    let docker = docker_settings(guest, &o.data_dir, o.docker_alias);
    crate::kube::serve(crate::kube::ApiServerConfig {
        listen,
        data_dir: Some(o.data_dir.join("state")),
        kubeconfig_out: None,
        log_requests: false,
        run_node: true,
        node_name: Some(o.node_name.unwrap_or_else(|| guest::NODE_NAME.into())),
        node_dir: Some(o.data_dir.join("node")),
        cluster_dns: Some(std::net::SocketAddr::from(([0, 0, 0, 0], 53))),
        in_cluster_port: Some(443),
        runtime: crate::kube::agent::RuntimeChoice::Crun,
        node_ip: None,
        advertise_address: None,
        network: crate::kube::join::ClusterNetwork::default(),
        token: None,
        lifecycle: crate::kube::controllers::NodeLifecycle::default(),
        // One node: no pod overlay; more: WireGuard between the cells.
        wireguard_port: o.wireguard_port,
        listener,
        extra_listen: o.listen_also,
        disk_root: Some(o.data_dir.clone()),
        node_port_offset: o.node_port_offset,
        docker,
        cluster_init: false,
        join_server: None,
        raft_port: None,
        raft_listener: None,
        // Metrics, logs and alerts for `litek8s ui`, on a smaller budget
        // than a server's: a dev cluster should stay nearly free.
        obs: Some(crate::obs::server::ObsConfig { memory_budget: 8 << 20, ..Default::default() }),
        prometheus_listen: None,
    })
    .await
}

/// The Docker API of a dev cluster: on the socket the supervisor bound
/// (Linux) or on the cell's loopback, where the Mac forwards its socket.
/// `alias` is the socket's path on the host (bind mounts of it get the
/// engine's own).
fn docker_settings(guest: bool, data_dir: &Path, alias: Option<String>) -> Option<crate::docker::Settings> {
    let root = data_dir.join("docker");
    if guest {
        let alias = alias.or_else(|| parse_share().map(|s| format!("{s}/.litek8s/docker.sock")));
        return Some(crate::docker::Settings {
            root,
            listener: None,
            tcp: Some(std::net::SocketAddr::from(([127, 0, 0, 1], crate::docker::GUEST_PORT))),
            publisher: crate::docker::Publisher::Local,
            socket_aliases: alias.into_iter().collect(),
        });
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::FromRawFd;
        // SAFETY: F_GETFD only checks the descriptor exists.
        if unsafe { libc::fcntl(sandbox::FD_DOCKER, libc::F_GETFD) } < 0 {
            return None;
        }
        // SAFETY: the supervisor passes the Docker socket as this descriptor.
        let listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(sandbox::FD_DOCKER) };
        // SAFETY: as above; not inherited by what the API server starts.
        unsafe { libc::fcntl(sandbox::FD_DOCKER, libc::F_SETFD, libc::FD_CLOEXEC) };
        Some(crate::docker::Settings {
            root,
            listener: Some(listener),
            tcp: None,
            publisher: crate::docker::Publisher::Broker,
            socket_aliases: vec![alias.unwrap_or_else(|| crate::docker::host_socket_path().to_string_lossy().to_string())],
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = alias;
        None
    }
}

fn parse_share() -> Option<String> {
    guest::parse_cmdline(&std::fs::read_to_string("/proc/cmdline").ok()?).share
}
