use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, LogOutput, LogsOptions,
    RemoveContainerOptions, StartContainerOptions,
};
use bollard::models::HostConfig;
use bollard::network::{CreateNetworkOptions, ListNetworksOptions};
use futures_util::stream::StreamExt;

const LABEL_CLUSTER: &str = "litek8s.cluster";
const LABEL_CLUSTER_ROLE: &str = "litek8s.cluster-role";
const LABEL_CLUSTER_NODE_ID: &str = "litek8s.cluster-node-id";
const BASE_IMAGE: &str = "debian:bookworm-slim";
const RAFT_PORT: u16 = 4001;
const API_PORT: u16 = 6443;
const RUST_BUILD_IMAGE: &str = "rust:1.90-bookworm";

fn cluster_base_dir() -> Result<PathBuf> {
    let home = env::var("HOME").context("$HOME is not set")?;
    Ok(PathBuf::from(home).join(".litek8s").join("clusters"))
}

fn cluster_dir(name: &str) -> Result<PathBuf> {
    Ok(cluster_base_dir()?.join(name))
}

fn node_dir(cluster_name: &str, node_id: u64) -> Result<PathBuf> {
    Ok(cluster_dir(cluster_name)?.join(format!("node-{node_id}")))
}

fn network_name(cluster_name: &str) -> String {
    format!("litek8s-{cluster_name}")
}

fn container_name(cluster_name: &str, role: &str, node_id: u64) -> String {
    format!("litek8s-{cluster_name}-{role}-{node_id}")
}

/// Detect the Docker socket path on the host.
fn detect_docker_socket() -> Result<PathBuf> {
    if let Ok(host) = env::var("DOCKER_HOST") {
        if let Some(path) = host.strip_prefix("unix://") {
            return Ok(PathBuf::from(path));
        }
    }

    let home = env::var("HOME").ok().map(PathBuf::from);
    let candidates = [
        home.as_ref().map(|h| h.join(".orbstack/run/docker.sock")),
        home.as_ref().map(|h| h.join(".docker/run/docker.sock")),
        Some(PathBuf::from("/var/run/docker.sock")),
    ];

    for candidate in candidates.into_iter().flatten() {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    bail!("failed to locate Docker socket; set DOCKER_HOST or ensure Docker is running")
}

fn connect_docker() -> Result<Docker> {
    let socket = detect_docker_socket()?;
    Docker::connect_with_unix(
        socket.to_string_lossy().as_ref(),
        120,
        bollard::API_DEFAULT_VERSION,
    )
    .with_context(|| format!("failed to connect to Docker on {}", socket.display()))
}

/// Detect the host architecture and return the appropriate Docker platform
/// and artifact tag for cross-compilation.
fn detect_host_platform() -> Result<(&'static str, &'static str)> {
    let arch = std::env::consts::ARCH;
    match arch {
        "x86_64" => Ok(("linux/amd64", "linux-amd64")),
        "aarch64" => Ok(("linux/arm64/v8", "linux-arm64")),
        _ => bail!("unsupported host architecture `{arch}` for cluster containers"),
    }
}

/// Build a Linux binary via cross-compilation in Docker.
async fn build_linux_binary(project_root: &Path) -> Result<PathBuf> {
    let (docker_platform, artifact_tag) = detect_host_platform()?;
    let target_dir = format!("/work/target/bootstrap/{artifact_tag}");

    let uid = run_capture("id", &["-u"]).await?;
    let gid = run_capture("id", &["-g"]).await?;

    let volume = format!("{}:/work", project_root.display());
    eprintln!("building linux binary for {docker_platform}...");

    let status = tokio::process::Command::new("docker")
        .args([
            "run",
            "--rm",
            "--platform",
            docker_platform,
            "-u",
            &format!("{uid}:{gid}"),
            "-e",
            &format!("CARGO_TARGET_DIR={target_dir}"),
            "-v",
            &volume,
            "-w",
            "/work",
            RUST_BUILD_IMAGE,
            "cargo",
            "build",
            "--release",
        ])
        .status()
        .await
        .context("failed to run docker build")?;

    if !status.success() {
        bail!(
            "cross-compilation failed with exit code {:?}",
            status.code()
        );
    }

    let artifact = project_root
        .join("target")
        .join("bootstrap")
        .join(artifact_tag)
        .join("release")
        .join("litek8s");

    if !tokio::fs::try_exists(&artifact).await.unwrap_or(false) {
        bail!(
            "build completed but `{}` was not produced",
            artifact.display()
        );
    }

    Ok(artifact)
}

/// Resolve the litek8s binary for use inside Linux containers.
/// On Linux, uses the current executable. On other platforms, cross-compiles.
async fn resolve_binary(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            bail!("specified binary `{}` does not exist", path.display());
        }
        return Ok(path.to_path_buf());
    }

    if cfg!(target_os = "linux") {
        return std::env::current_exe().context("failed to resolve current executable path");
    }

    // Cross-compile for Linux.
    let project_root = resolve_project_root()?;
    build_linux_binary(&project_root).await
}

fn resolve_project_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read the current working directory")?;
    if cwd.join("Cargo.toml").exists() {
        return Ok(cwd);
    }
    let fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if fallback.join("Cargo.toml").exists() {
        return Ok(fallback);
    }
    bail!("failed to locate the litek8s project root; run this command from the repository root")
}

async fn run_capture(cmd: &str, args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to run `{cmd}`"))?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Pull an image if it is not already present locally.
async fn ensure_image(docker: &Docker, image: &str) -> Result<()> {
    if docker.inspect_image(image).await.is_ok() {
        return Ok(());
    }

    eprintln!("pulling {image}...");
    let (docker_platform, _) = detect_host_platform()?;
    let mut stream = docker.create_image(
        Some(bollard::image::CreateImageOptions {
            from_image: image,
            platform: docker_platform,
            ..Default::default()
        }),
        None,
        None,
    );

    while let Some(item) = stream.next().await {
        item.with_context(|| format!("failed to pull image {image}"))?;
    }

    Ok(())
}

// -------------------------------------------------------------------------
// Public API
// -------------------------------------------------------------------------

pub struct ClusterConfig {
    pub name: String,
    pub control_planes: u32,
    pub workers: u32,
    pub binary: Option<PathBuf>,
}

pub struct ClusterInfo {
    pub name: String,
    pub network: String,
    pub api_addr: String,
    pub nodes: Vec<ClusterNodeInfo>,
}

pub struct ClusterNodeInfo {
    pub container_name: String,
    pub container_id: String,
    pub role: String,
    pub node_id: u64,
}

pub struct ClusterListEntry {
    pub name: String,
    pub control_planes: usize,
    pub workers: usize,
    pub api_addr: String,
}

pub async fn create_cluster(config: ClusterConfig) -> Result<ClusterInfo> {
    let docker = connect_docker()?;

    // Check for existing cluster.
    let network = network_name(&config.name);
    let existing = docker
        .list_networks(Some(ListNetworksOptions {
            filters: HashMap::from([("name".to_string(), vec![network.clone()])]),
        }))
        .await
        .context("failed to list Docker networks")?;
    if existing.iter().any(|n| n.name.as_deref() == Some(&network)) {
        bail!(
            "cluster `{}` already exists; delete it first with `litek8s cluster delete {}`",
            config.name,
            config.name
        );
    }

    let binary = resolve_binary(config.binary.as_deref()).await?;
    ensure_image(&docker, BASE_IMAGE).await?;

    // Create cluster directory structure.
    let base = cluster_dir(&config.name)?;
    tokio::fs::create_dir_all(&base)
        .await
        .with_context(|| format!("failed to create cluster directory {}", base.display()))?;

    // Create Docker network.
    docker
        .create_network(CreateNetworkOptions {
            name: network.as_str(),
            driver: "bridge",
            ..Default::default()
        })
        .await
        .with_context(|| format!("failed to create Docker network `{network}`"))?;
    eprintln!("created network {network}");

    let docker_socket = detect_docker_socket()?;
    let binary_str = binary.to_string_lossy().to_string();
    let docker_socket_str = docker_socket.to_string_lossy().to_string();

    let total_cp = config.control_planes.max(1);
    let total_workers = config.workers;
    let mut nodes = Vec::new();

    // Start control-plane nodes.
    for i in 1..=total_cp {
        let role = "cp";
        let name = container_name(&config.name, role, i as u64);
        let dir = node_dir(&config.name, i as u64)?;
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("failed to create node directory {}", dir.display()))?;
        let dir_str = dir.to_string_lossy().to_string();

        let mut cmd = vec![
            "/usr/local/bin/litek8s".to_string(),
            "serve".to_string(),
            "--peer-id".to_string(),
            i.to_string(),
            "--peer-addr".to_string(),
            format!("0.0.0.0:{RAFT_PORT}"),
            "--api-addr".to_string(),
            format!("0.0.0.0:{API_PORT}"),
        ];

        if i > 1 {
            let leader = container_name(&config.name, "cp", 1);
            cmd.push("--join".to_string());
            cmd.push(format!("{leader}:{RAFT_PORT}"));
        }

        // State and socket live under /data inside the container, bind-mounted
        // to the host cluster directory so the host CLI can reach the socket.
        let node_id_str = i.to_string();
        let api_port_str = API_PORT.to_string();

        // Expose the API port from the leader to the host.
        let port_bindings = if i == 1 {
            Some(HashMap::from([(
                format!("{API_PORT}/tcp"),
                Some(vec![bollard::models::PortBinding {
                    host_ip: Some("127.0.0.1".to_string()),
                    host_port: Some(api_port_str.clone()),
                }]),
            )]))
        } else {
            None
        };

        let api_port_key = format!("{API_PORT}/tcp");
        let exposed_ports: Option<HashMap<&str, HashMap<(), ()>>> = if i == 1 {
            Some(HashMap::from([(api_port_key.as_str(), HashMap::new())]))
        } else {
            None
        };

        let container_config = Config {
            image: Some(BASE_IMAGE),
            hostname: Some(&name),
            cmd: Some(cmd.iter().map(|s| s.as_str()).collect()),
            exposed_ports,
            labels: Some(HashMap::from([
                (LABEL_CLUSTER, config.name.as_str()),
                (LABEL_CLUSTER_ROLE, role),
                (LABEL_CLUSTER_NODE_ID, node_id_str.as_str()),
                ("com.docker.compose.project", config.name.as_str()),
                ("com.docker.compose.service", role),
                ("com.docker.compose.oneoff", "False"),
            ])),
            host_config: Some(HostConfig {
                binds: Some(vec![
                    format!("{binary_str}:/usr/local/bin/litek8s:ro"),
                    format!("{docker_socket_str}:/var/run/docker.sock"),
                    format!("{dir_str}:/data"),
                ]),
                port_bindings,
                ..Default::default()
            }),
            env: Some(vec![
                "HOME=/data",
                "XDG_RUNTIME_DIR=/data",
                "XDG_DATA_HOME=/data",
            ]),
            ..Default::default()
        };

        let created = docker
            .create_container(
                Some(CreateContainerOptions {
                    name: name.as_str(),
                    platform: None,
                }),
                container_config,
            )
            .await
            .with_context(|| format!("failed to create container {name}"))?;

        // Connect to the cluster network.
        docker
            .connect_network(
                &network,
                bollard::network::ConnectNetworkOptions {
                    container: name.as_str(),
                    endpoint_config: bollard::models::EndpointSettings {
                        aliases: Some(vec![name.clone()]),
                        ..Default::default()
                    },
                },
            )
            .await
            .with_context(|| format!("failed to connect {name} to network {network}"))?;

        docker
            .start_container(&created.id, None::<StartContainerOptions<String>>)
            .await
            .with_context(|| format!("failed to start container {name}"))?;

        eprintln!("started {name} (node {i})");

        // For the leader, wait until the API TCP port is reachable from the host.
        if i == 1 {
            let api_addr = format!("127.0.0.1:{API_PORT}");
            if let Err(e) = wait_for_tcp(&api_addr, Duration::from_secs(30)).await {
                if let Ok(logs) = container_logs(&docker, &created.id).await {
                    eprintln!("--- container logs ---\n{logs}\n--- end logs ---");
                }
                return Err(e);
            }
            eprintln!("leader API ready at {api_addr}");
        }

        // For joiners, wait a moment for them to start, then tell the leader.
        if i > 1 {
            // Wait for the joiner's Raft listener to be reachable.
            tokio::time::sleep(Duration::from_secs(2)).await;

            let leader_api = format!("127.0.0.1:{API_PORT}");
            let joiner_addr = format!("{name}:{RAFT_PORT}");
            crate::protocol::request_tcp(
                &leader_api,
                crate::protocol::Request::JoinCluster {
                    node_id: i as u64,
                    addr: joiner_addr,
                },
            )
            .await
            .with_context(|| format!("failed to join node {i} to cluster"))?;
            eprintln!("node {i} joined the cluster");
        }

        nodes.push(ClusterNodeInfo {
            container_name: name,
            container_id: created.id,
            role: role.to_string(),
            node_id: i as u64,
        });
    }

    // Start worker nodes.
    for w in 1..=total_workers {
        let node_id = total_cp as u64 + w as u64;
        let role = "worker";
        let name = container_name(&config.name, role, node_id);
        let dir = node_dir(&config.name, node_id)?;
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("failed to create node directory {}", dir.display()))?;
        let dir_str = dir.to_string_lossy().to_string();

        let leader = container_name(&config.name, "cp", 1);
        let cmd = vec![
            "/usr/local/bin/litek8s".to_string(),
            "serve".to_string(),
            "--peer-id".to_string(),
            node_id.to_string(),
            "--peer-addr".to_string(),
            format!("0.0.0.0:{RAFT_PORT}"),
            "--api-addr".to_string(),
            format!("0.0.0.0:{API_PORT}"),
            "--join".to_string(),
            format!("{leader}:{RAFT_PORT}"),
        ];

        let node_id_str = node_id.to_string();
        let container_config = Config {
            image: Some(BASE_IMAGE),
            hostname: Some(&name),
            cmd: Some(cmd.iter().map(|s| s.as_str()).collect()),
            labels: Some(HashMap::from([
                (LABEL_CLUSTER, config.name.as_str()),
                (LABEL_CLUSTER_ROLE, role),
                (LABEL_CLUSTER_NODE_ID, node_id_str.as_str()),
                ("com.docker.compose.project", config.name.as_str()),
                ("com.docker.compose.service", role),
                ("com.docker.compose.oneoff", "False"),
            ])),
            host_config: Some(HostConfig {
                binds: Some(vec![
                    format!("{binary_str}:/usr/local/bin/litek8s:ro"),
                    format!("{docker_socket_str}:/var/run/docker.sock"),
                    format!("{dir_str}:/data"),
                ]),
                ..Default::default()
            }),
            env: Some(vec![
                "HOME=/data",
                "XDG_RUNTIME_DIR=/data",
                "XDG_DATA_HOME=/data",
            ]),
            ..Default::default()
        };

        let created = docker
            .create_container(
                Some(CreateContainerOptions {
                    name: name.as_str(),
                    platform: None,
                }),
                container_config,
            )
            .await
            .with_context(|| format!("failed to create container {name}"))?;

        docker
            .connect_network(
                &network,
                bollard::network::ConnectNetworkOptions {
                    container: name.as_str(),
                    endpoint_config: bollard::models::EndpointSettings {
                        aliases: Some(vec![name.clone()]),
                        ..Default::default()
                    },
                },
            )
            .await
            .with_context(|| format!("failed to connect {name} to network {network}"))?;

        docker
            .start_container(&created.id, None::<StartContainerOptions<String>>)
            .await
            .with_context(|| format!("failed to start container {name}"))?;

        eprintln!("started {name} (worker node {node_id})");

        // Wait and join.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let leader_api = format!("127.0.0.1:{API_PORT}");
        let joiner_addr = format!("{name}:{RAFT_PORT}");
        crate::protocol::request_tcp(
            &leader_api,
            crate::protocol::Request::JoinCluster {
                node_id,
                addr: joiner_addr,
            },
        )
        .await
        .with_context(|| format!("failed to join worker node {node_id} to cluster"))?;
        eprintln!("worker node {node_id} joined the cluster");

        nodes.push(ClusterNodeInfo {
            container_name: name,
            container_id: created.id,
            role: role.to_string(),
            node_id,
        });
    }

    let api_addr = format!("127.0.0.1:{API_PORT}");

    Ok(ClusterInfo {
        name: config.name,
        network,
        api_addr,
        nodes,
    })
}

pub async fn delete_cluster(name: &str) -> Result<()> {
    let docker = connect_docker()?;
    let network = network_name(name);

    // Find and remove all containers in this cluster.
    let containers = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            filters: HashMap::from([(
                "label".to_string(),
                vec![format!("{LABEL_CLUSTER}={name}")],
            )]),
            ..Default::default()
        }))
        .await
        .context("failed to list cluster containers")?;

    for container in &containers {
        let id = container.id.as_deref().unwrap_or("unknown");
        let cname = container
            .names
            .as_ref()
            .and_then(|n| n.first())
            .map(|n| n.trim_start_matches('/'))
            .unwrap_or(id);

        docker
            .remove_container(
                id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .with_context(|| format!("failed to remove container {cname}"))?;

        eprintln!("removed container {cname}");
    }

    // Remove network.
    let networks = docker
        .list_networks(Some(ListNetworksOptions {
            filters: HashMap::from([("name".to_string(), vec![network.clone()])]),
        }))
        .await
        .unwrap_or_default();

    if networks.iter().any(|n| n.name.as_deref() == Some(&network)) {
        docker
            .remove_network(&network)
            .await
            .with_context(|| format!("failed to remove network {network}"))?;
        eprintln!("removed network {network}");
    }

    // Clean up local state directory.
    let dir = cluster_dir(name)?;
    if dir.exists() {
        tokio::fs::remove_dir_all(&dir)
            .await
            .with_context(|| format!("failed to remove cluster directory {}", dir.display()))?;
        eprintln!("removed {}", dir.display());
    }

    Ok(())
}

pub async fn list_clusters() -> Result<Vec<ClusterListEntry>> {
    let docker = connect_docker()?;

    let containers = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            filters: HashMap::from([("label".to_string(), vec![LABEL_CLUSTER.to_string()])]),
            ..Default::default()
        }))
        .await
        .context("failed to list cluster containers")?;

    let mut clusters: HashMap<String, (usize, usize)> = HashMap::new();
    for container in &containers {
        let labels = match container.labels.as_ref() {
            Some(l) => l,
            None => continue,
        };
        let name = match labels.get(LABEL_CLUSTER) {
            Some(n) => n.clone(),
            None => continue,
        };
        let role = labels
            .get(LABEL_CLUSTER_ROLE)
            .map(|s| s.as_str())
            .unwrap_or("unknown");

        let entry = clusters.entry(name).or_insert((0, 0));
        match role {
            "cp" => entry.0 += 1,
            "worker" => entry.1 += 1,
            _ => {}
        }
    }

    let mut entries: Vec<ClusterListEntry> = clusters
        .into_iter()
        .map(|(name, (cp, workers))| ClusterListEntry {
            name,
            control_planes: cp,
            workers,
            api_addr: format!("127.0.0.1:{API_PORT}"),
        })
        .collect();

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

async fn container_logs(docker: &Docker, container_id: &str) -> Result<String> {
    let mut output = String::new();
    let mut stream = docker.logs(
        container_id,
        Some(LogsOptions::<String> {
            stdout: true,
            stderr: true,
            tail: "50".to_string(),
            ..Default::default()
        }),
    );
    while let Some(item) = stream.next().await {
        match item {
            Ok(LogOutput::StdOut { message }) | Ok(LogOutput::StdErr { message }) => {
                output.push_str(&String::from_utf8_lossy(&message));
            }
            _ => {}
        }
    }
    Ok(output)
}

async fn wait_for_tcp(addr: &str, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out waiting for API at {addr}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
