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

pub mod broker;
pub mod hostinfo;
pub mod kubeconfig;
#[cfg(target_os = "linux")]
pub mod sandbox;
#[cfg(target_os = "macos")]
pub mod vm;
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
    /// Start the dev cluster and point kubectl at it (context litek8s-dev)
    Up {
        #[arg(long, help = "API server port on localhost (default 6443, or a free one if that is taken)")]
        port: Option<u16>,
        #[arg(long, help = "Do not touch the kubeconfig")]
        no_kubeconfig: bool,
        #[arg(long, default_value_t = 120, help = "Seconds to wait for the node to be Ready")]
        timeout: u64,
        #[arg(long, help = "macOS: VM memory in MiB (default 2048)")]
        memory: Option<u64>,
        #[arg(long, help = "macOS: VM CPUs (default: half of the Mac's)")]
        cpus: Option<usize>,
        #[arg(long, help = "macOS: directory shared with the VM at the same path (default: $HOME)")]
        share: Option<PathBuf>,
    },
    /// Stop the dev cluster (objects and images are kept for the next `up`)
    Down,
    /// Show whether the dev cluster runs, where, and what it uses
    Status,
    /// Stop the dev cluster and delete its objects, images and kube context
    Reset,
    /// Load images into the dev cluster: an image of the local Docker/Podman, or a `docker save` / OCI archive
    Load {
        #[arg(required = true, help = "Image names (exported from the local Docker or Podman) or paths to image archives (.tar)")]
        images: Vec<String>,
    },
    /// Linux: check (and, as root, set up) what the rootless dev cluster needs
    Setup,
    #[command(hide = true)]
    Supervise {
        #[arg(long)]
        port: Option<u16>,
    },
    #[command(hide = true, name = "__apiserver")]
    InnerApiserver {
        #[arg(long)]
        data_dir: PathBuf,
    },
}

/// What a running dev cluster recorded about itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

pub fn dev_home() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("LITEK8S_DEV_HOME") {
        return Ok(PathBuf::from(dir));
    }
    let dirs = directories::ProjectDirs::from("dev", "civojairo", "litek8s").context("failed to resolve a data directory")?;
    Ok(dirs.data_local_dir().join("dev"))
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

fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// The running cluster's state, if one runs.
fn running(home: &Path) -> Option<DevState> {
    read_state(home).filter(|s| pid_alive(s.pid))
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

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
        DevCommand::Up { port, no_kubeconfig, timeout, memory, cpus, share } => {
            up(UpOptions { port, kubeconfig: !no_kubeconfig, timeout: Duration::from_secs(timeout), memory, cpus, share }).await
        }
        DevCommand::Down => down().await,
        DevCommand::Status => status().await,
        DevCommand::Reset => reset().await,
        DevCommand::Load { images } => load(images).await,
        DevCommand::Setup => setup(),
        DevCommand::Supervise { port } => supervise(port).await,
        DevCommand::InnerApiserver { data_dir } => inner_apiserver(data_dir).await,
    }
}

pub struct UpOptions {
    pub port: Option<u16>,
    pub kubeconfig: bool,
    pub timeout: Duration,
    pub memory: Option<u64>,
    pub cpus: Option<usize>,
    pub share: Option<PathBuf>,
}

async fn up(opts: UpOptions) -> Result<()> {
    let started = Instant::now();
    let home = dev_home()?;
    std::fs::create_dir_all(&home).with_context(|| format!("failed to create {}", home.display()))?;
    if let Some(state) = running(&home) {
        let server = format!("http://127.0.0.1:{}", state.port);
        if opts.kubeconfig {
            kubeconfig::install(&kubeconfig::default_path(), &server)?;
        }
        println!("litek8s-dev is already running ({}, {server}, context {})", state.mode, kubeconfig::CONTEXT);
        return Ok(());
    }
    let _ = std::fs::remove_file(state_path(&home));
    let pid = start(&home, &opts)?;
    let state = wait_ready(&home, pid, opts.timeout).await?;
    let server = format!("http://127.0.0.1:{}", state.port);
    if opts.kubeconfig {
        kubeconfig::install(&kubeconfig::default_path(), &server)?;
    }
    println!(
        "litek8s-dev ready in {:.2}s ({}, {server}, kubectl context {})",
        started.elapsed().as_secs_f64(),
        state.mode,
        kubeconfig::CONTEXT
    );
    Ok(())
}

/// Start the cluster's long-running process in the background; its pid.
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
    let log = std::fs::File::create(log_path(home))?;
    // Rootless, the cluster needs a cgroup of its own the user may manage:
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
    cmd.args(["dev", "supervise"]);
    if let Some(p) = opts.port {
        cmd.arg("--port").arg(p.to_string());
    }
    cmd.stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // SAFETY: setsid only.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().context("failed to start the dev cluster")?;
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

async fn wait_ready(home: &Path, pid: u32, timeout: Duration) -> Result<DevState> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() > deadline {
            bail!("the dev cluster did not get ready in {}s; log ({}):\n{}", timeout.as_secs(), log_path(home).display(), log_tail(home, 30));
        }
        if !pid_alive(pid) {
            // A zombie still counts as alive for kill(0); reap it if it is ours.
            bail!("the dev cluster exited during startup; log ({}):\n{}", log_path(home).display(), log_tail(home, 30));
        }
        if let Some(state) = read_state(home).filter(|s| s.pid == pid) {
            if node_ready(state.port).await {
                return Ok(state);
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn node_ready(port: u16) -> bool {
    let Ok((status, body)) = http(port, hyper::Method::GET, "/api/v1/nodes", None).await else { return false };
    if !status.is_success() {
        return false;
    }
    let Ok(list) = serde_json::from_slice::<Value>(&body) else { return false };
    list["items"].as_array().into_iter().flatten().any(|n| {
        n.pointer("/status/conditions")
            .and_then(Value::as_array)
            .is_some_and(|c| c.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"))
    })
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

async fn down() -> Result<()> {
    let home = dev_home()?;
    let Some(state) = running(&home) else {
        println!("litek8s-dev is not running");
        return Ok(());
    };
    stop(&state).await?;
    println!("litek8s-dev stopped (data kept in {}; `litek8s dev reset` deletes it)", home.display());
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

async fn status() -> Result<()> {
    let home = dev_home()?;
    let Some(state) = running(&home) else {
        println!("litek8s-dev is not running (data: {})", home.display());
        return Ok(());
    };
    let uptime = Duration::from_millis(now_ms().saturating_sub(state.started_unix_ms));
    println!("litek8s-dev: running ({}), pid {}, up {}s", state.mode, state.pid, uptime.as_secs());
    println!("  server:   http://127.0.0.1:{} (kubectl context {})", state.port, kubeconfig::CONTEXT);
    if let Some(ip) = &state.vm_ip {
        println!("  vm:       {ip}");
    }
    if let Ok((_, body)) = http(state.port, hyper::Method::GET, "/api/v1/pods", None).await {
        if let Ok(list) = serde_json::from_slice::<Value>(&body) {
            let pods = list["items"].as_array().cloned().unwrap_or_default();
            let running = pods.iter().filter(|p| p.pointer("/status/phase").and_then(Value::as_str) == Some("Running")).count();
            println!("  pods:     {running} running, {} total", pods.len());
        }
    }
    if let Some(mem) = memory_bytes(&state) {
        println!("  memory:   {:.1} MiB (everything the cluster runs, pods included)", mem as f64 / 1048576.0);
    }
    println!("  data:     {} ({:.1} MiB)", home.display(), dir_size(&home) as f64 / 1048576.0);
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
        return vm::footprint(state.pid);
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

async fn reset() -> Result<()> {
    let home = dev_home()?;
    if let Some(state) = running(&home) {
        stop(&state).await?;
    }
    remove_data(&home)?;
    kubeconfig::uninstall(&kubeconfig::default_path())?;
    println!("litek8s-dev reset: data in {} deleted, context {} removed", home.display(), kubeconfig::CONTEXT);
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

async fn load(images: Vec<String>) -> Result<()> {
    let home = dev_home()?;
    let state = running(&home).context("litek8s-dev is not running (`litek8s dev up` first)")?;
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
async fn supervise(port: Option<u16>) -> Result<()> {
    supervisor::run(port).await
}

#[cfg(not(target_os = "linux"))]
async fn supervise(_: Option<u16>) -> Result<()> {
    bail!("the dev supervisor runs on Linux")
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

/// The API server inside the sandbox or the VM.
async fn inner_apiserver(data_dir: PathBuf) -> Result<()> {
    let guest = std::env::var("LITEK8S_DEV_GUEST").is_ok_and(|v| v == "1");
    let (listener, listen): (Option<std::net::TcpListener>, std::net::SocketAddr) = if guest {
        // In the VM: the Mac forwards its localhost to this port.
        (None, std::net::SocketAddr::from(([0, 0, 0, 0], guest::GUEST_API_PORT)))
    } else {
        sandbox_listener()?
    };
    // SAFETY: set before any other thread reads the environment.
    unsafe { std::env::set_var("LITEK8S_LOADBALANCER_IP", "127.0.0.1") };
    crate::kube::serve(crate::kube::ApiServerConfig {
        listen,
        data_dir: Some(data_dir.join("state")),
        kubeconfig_out: None,
        log_requests: false,
        run_node: true,
        node_name: Some(guest::NODE_NAME.into()),
        node_dir: Some(data_dir.join("node")),
        cluster_dns: Some(std::net::SocketAddr::from(([0, 0, 0, 0], 53))),
        in_cluster_port: Some(443),
        runtime: crate::kube::agent::RuntimeChoice::Crun,
        listener,
    })
    .await
}
