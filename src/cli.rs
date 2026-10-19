use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Parser, Subcommand};

use crate::config;
use crate::daemon::{self, ServerConfig};
use crate::node::{self, validate_label_key, validate_label_value};
use crate::protocol::{self, Request, Response};
use crate::proxy;

const HELP_TEMPLATE: &str = "\
{before-help}{name} {version}
{about-with-newline}
{usage-heading} {usage}

{all-args}{after-help}";

const MAIN_AFTER_HELP: &str = "\
Quickstart:
  litek8s serve
  litek8s apply -f examples/nginx.yaml
  litek8s top --watch
  litek8s describe deployment demo-nginx

Mental model:
  `serve` runs the control loop.
  Everything else talks to that daemon over a local Unix socket.";

const APISERVER_AFTER_HELP: &str = "\
Examples:
  litek8s apiserver --kubeconfig /tmp/litek8s.kubeconfig
  KUBECONFIG=/tmp/litek8s.kubeconfig kubectl get namespaces
  litek8s apiserver --data-dir ~/.local/share/litek8s/apiserver --listen 127.0.0.1:6443

Controllers (Deployment, ReplicaSet, StatefulSet, DaemonSet, Job, CronJob,
garbage collection), a scheduler and a Docker-backed node agent run in the
same process, so pods run on this machine (`--no-node` to only serve).
Plain HTTP serves kubectl; pods reach the same port over TLS. There is no
authentication for API clients, so keep the port on a trusted network.

More nodes join with the command the server prints at startup (the token is
also kept in <data-dir>/join-token):
  litek8s apiserver --data-dir /var/lib/litek8s/apiserver --listen 0.0.0.0:6443 --node-ip 192.168.1.10
  litek8s agent --join https://192.168.1.10:6443 --token K10...::...

Pods of all nodes share one network (each node gets a /24 of --cluster-cidr)
over a WireGuard mesh (UDP --wireguard-port, kernel WireGuard). A node whose
heartbeats stop is marked Unknown after --node-monitor-grace-period and its
pods are evicted after their tolerationSeconds (default 300 s, see
--default-unreachable-toleration-seconds).";

const DEV_AFTER_HELP: &str = "\
Examples:
  litek8s dev up                 # API server, controllers, DNS, proxy and a node; kubectl context litek8s-dev
  kubectl create deployment web --image=nginx && kubectl expose deployment web --type=NodePort --port 80
  litek8s dev load myapp:dev     # from the local Docker/Podman, or a `docker save` archive
  litek8s dev status | down | delete
  litek8s dev up --name ci --nodes 3   # another cluster (context litek8s-ci), 3 nodes, same VM/sandbox
  litek8s dev list               # every local cluster: state, API port, nodes, memory

Linux: rootless by default, in namespaces of its own (no host iptables, interfaces or
/run changes); needs crun, uidmap, pasta or slirp4netns (`litek8s dev setup` checks).
macOS: a small Linux VM on Virtualization.framework; NodePorts and LoadBalancers appear
on localhost and $HOME is shared at the same path for hostPath volumes. Every cluster
runs in that one VM or sandbox, in namespaces of its own.";

const UI_AFTER_HELP: &str = "\
Examples:
  litek8s ui                     # the local dev cluster: Home, Containers, Images, Volumes, Free space, Kubernetes, Settings
  ssh -t box litek8s ui          # the same over SSH
  litek8s ui --docker-host tcp://10.0.0.5:2375 --context remote   # another litek8s
  litek8s ui --mouse             # clicks and the wheel too

Keyboard only by default; ? lists the keys. Fits 80x24. See docs/ui.md.";

const NODE_AGENT_AFTER_HELP: &str = "\
Examples:
  litek8s agent --join https://192.168.1.10:6443 --token K10<hash>::<secret>
  litek8s agent --join https://apiserver:6443 --token $(cat token) --node-name pi-4 --node-ip 192.168.1.21
  litek8s agent --join https://home.example:6443 --token ... --node-grace-period 15m   # a laptop that sleeps

The token comes from the API server (printed at startup, kept in
<data-dir>/join-token); it pins the cluster CA, so the agent checks it is
talking to the right server. The agent keeps one connection to the API
server: pods bound to this node are pushed over it, status and heartbeats go
back on it, and kubectl logs/exec are relayed through it. It reconnects on
its own; running pods keep running meanwhile, also across agent restarts.
Pods reach pods on other nodes over WireGuard (UDP --wireguard-port must be
reachable from the other nodes, or this node must reach theirs).";

/// `40s`, `5m`, `1h`, or plain seconds.
fn parse_duration(raw: &str) -> Result<Duration, String> {
    let raw = raw.trim();
    let (number, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len()));
    let n: u64 = number.parse().map_err(|_| format!("invalid duration {raw:?} (use e.g. 40s, 5m, 1h)"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => return Err(format!("invalid duration unit in {raw:?} (s, m or h)")),
    };
    Ok(Duration::from_secs(secs))
}

const SERVE_AFTER_HELP: &str = "\
Examples:
  litek8s serve
  litek8s serve --poll-interval-secs 1
  litek8s serve --state-file /tmp/litek8s-state.json
  litek8s serve --peer-addr 0.0.0.0:4002";

const CONTROL_PLANE_AFTER_HELP: &str = "\
Examples:
  litek8s control-plane join cp2 --ssh dev@192.168.0.77 --install
  litek8s control-plane join cp2 --ssh dev@192.168.0.77 --peer-port 4002
  litek8s control-plane status";

const CLUSTER_AFTER_HELP: &str = "\
Examples:
  litek8s cluster create my-cluster --control-planes 3
  litek8s cluster create dev --control-planes 3 --workers 2
  litek8s cluster list
  litek8s cluster delete my-cluster";

const APPLY_AFTER_HELP: &str = "\
Examples:
  litek8s apply -f examples/nginx.yaml
  litek8s apply -f examples/http-echo-service.yaml
  litek8s apply -f ./my-app.yaml";

const TOP_AFTER_HELP: &str = "\
Examples:
  litek8s top
  litek8s top --watch
  litek8s top --watch --interval-secs 1";

const TENSOR_AFTER_HELP: &str = "\
Examples:
  litek8s tensor multiply -f examples/tensor-demo.yaml --show-result
  litek8s tensor multiply --size 256
  litek8s tensor multiply --size 512 --seed 7";

const PIPELINE_AFTER_HELP: &str = "\
Examples:
  litek8s pipeline run --layers 6 --dim 512
  litek8s pipeline run --layers 4 --dim 1024 --batch 2
  litek8s pipeline run -f examples/pipeline-demo.yaml --show-output";

const TREE_AFTER_HELP: &str = "\
Examples:
  litek8s tree
  litek8s tree --watch
  litek8s tree --watch --interval-secs 1";

const GET_AFTER_HELP: &str = "\
Examples:
  litek8s get deployments
  litek8s get services
  litek8s get nodes";

const DESCRIBE_AFTER_HELP: &str = "\
Examples:
  litek8s describe deployment demo-nginx
  litek8s describe service echo-svc
  litek8s describe deployments demo-nginx";

const LOGS_AFTER_HELP: &str = "\
Examples:
  litek8s logs demo-nginx
  litek8s logs demo-nginx --tail 200";

const PROXY_AFTER_HELP: &str = "\
Examples:
  litek8s proxy echo-demo
  litek8s proxy echo-edge --listen 127.0.0.1:9002
  litek8s proxy echo-demo --port 8090";

const EVENTS_AFTER_HELP: &str = "\
Examples:
  litek8s events
  litek8s events --watch
  litek8s events --deployment echo-edge
  litek8s events --node server77 --watch";

const DELETE_AFTER_HELP: &str = "\
Examples:
  litek8s delete deployment demo-nginx
  litek8s delete service echo-svc
  litek8s delete -f examples/http-echo-service.yaml
  litek8s delete -f examples/nginx.yaml";

const NODE_AFTER_HELP: &str = "\
Examples:
  litek8s node add server77 --ssh server77
  litek8s node add server77 --ssh server77 --install
  litek8s node add server77 --ssh server77 --label role=edge --label zone=lab
  litek8s node cordon server77
  litek8s node drain server77
  litek8s node uncordon server77
  litek8s node add edge-1 --ssh ubuntu@10.0.0.7 --remote-binary ~/.local/bin/litek8s";

const SERVICE_AFTER_HELP: &str = "\
Examples:
  litek8s service add echo-svc --deployment echo-demo --listen 127.0.0.1:9100
  litek8s service add edge-svc --deployment echo-edge --listen 127.0.0.1:9102
  litek8s get services
  litek8s describe service echo-svc";

#[derive(Debug, Parser)]
#[command(
    name = "litek8s",
    version,
    about = "Tiny k8s-like orchestration for one machine, one daemon, and Docker underneath.",
    long_about = "Tiny k8s-like orchestration for one machine, one daemon, and Docker underneath.\n\nlitek8s keeps the UX familiar on purpose: apply a reduced Deployment manifest, inspect it, watch it, and let a small control loop keep the container alive.",
    help_template = HELP_TEMPLATE,
    after_help = MAIN_AFTER_HELP,
    styles = cli_styles(),
    next_line_help = true
)]
struct Cli {
    #[arg(long, global = true, help = "Use a custom daemon socket path")]
    socket_path: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        help = "Connect to a daemon over TCP (e.g. 127.0.0.1:6443)"
    )]
    server: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(
        about = "Start the local litek8s daemon",
        help_template = HELP_TEMPLATE,
        after_help = SERVE_AFTER_HELP
    )]
    Serve {
        #[arg(
            long,
            help = "Write daemon state to this file instead of the default path"
        )]
        state_file: Option<PathBuf>,
        #[arg(
            long,
            default_value_t = 2,
            help = "How often the reconciler polls, in seconds"
        )]
        poll_interval_secs: u64,
        #[arg(
            long,
            default_value_t = 1,
            help = "Numeric ID for this control-plane node"
        )]
        peer_id: u64,
        #[arg(
            long,
            default_value = "0.0.0.0:4001",
            help = "TCP address for Raft peer communication"
        )]
        peer_addr: String,
        #[arg(
            long,
            hide = true,
            help = "Join an existing cluster at this address instead of self-initializing"
        )]
        join: Option<String>,
        #[arg(
            long,
            hide = true,
            help = "Expose the daemon API on a TCP address (e.g. 0.0.0.0:6443)"
        )]
        api_addr: Option<String>,
        #[arg(
            long,
            default_value_t = 5,
            help = "How often to refresh per-node host resources in `top`, in seconds"
        )]
        node_resource_interval_secs: u64,
    },
    #[command(
        about = "Run the Kubernetes-compatible API server (kubectl/Helm talk to it)",
        help_template = HELP_TEMPLATE,
        after_help = APISERVER_AFTER_HELP
    )]
    Apiserver {
        #[arg(long, default_value = "127.0.0.1:6443", help = "Address to listen on (plain HTTP, no auth)")]
        listen: std::net::SocketAddr,
        #[arg(long, help = "Directory for the object log; omit to keep objects in memory only")]
        data_dir: Option<PathBuf>,
        #[arg(long, help = "Write a kubeconfig that points at this server to this path")]
        kubeconfig: Option<PathBuf>,
        #[arg(long, help = "Log one line per HTTP request to stderr")]
        log_requests: bool,
        #[arg(long, help = "Do not run this machine as a node (no pods run here)")]
        no_node: bool,
        #[arg(long, help = "Name of the local node (default: the hostname)")]
        node_name: Option<String>,
        #[arg(long, help = "Directory for the local node agent's pod files")]
        node_dir: Option<PathBuf>,
        #[arg(long, default_value = "0.0.0.0:53", help = "UDP address for cluster DNS (pods use port 53)")]
        cluster_dns: std::net::SocketAddr,
        #[arg(long, help = "Do not serve cluster DNS")]
        no_cluster_dns: bool,
        #[arg(long, default_value_t = 443, help = "Extra port on the listen address for pods (kubernetes.default.svc:443); 0 disables it")]
        in_cluster_port: u16,
        #[arg(long, value_enum, default_value = "auto", help = "Container runtime of the local node: auto (crun on Linux when installed, else Docker), crun or docker")]
        runtime: crate::kube::agent::RuntimeChoice,
        #[arg(long, help = "The local node's address (default: the source address of the default route)")]
        node_ip: Option<std::net::IpAddr>,
        #[arg(long, help = "Address joining nodes reach this server at, for the printed join command (default: --node-ip)")]
        advertise_address: Option<String>,
        #[arg(long, default_value = "10.42.0.0/16", help = "Pod network of the cluster; each node gets one block of it")]
        cluster_cidr: String,
        #[arg(long, default_value_t = 24, help = "Prefix length of each node's pod CIDR")]
        node_cidr_mask_size: u8,
        #[arg(long, help = "Join secret (or a whole token) instead of the stored or a generated one")]
        token: Option<String>,
        #[arg(long, default_value = "40s", value_parser = parse_duration, help = "Mark a node Unknown when it has not heartbeated for this long (e.g. 40s, 5m)")]
        node_monitor_grace_period: Duration,
        #[arg(long, default_value_t = 300, help = "tolerationSeconds every pod gets for the not-ready taint")]
        default_not_ready_toleration_seconds: u64,
        #[arg(long, default_value_t = 300, help = "tolerationSeconds every pod gets for the unreachable taint")]
        default_unreachable_toleration_seconds: u64,
        #[arg(long, default_value_t = 51820, help = "UDP port of the WireGuard pod overlay")]
        wireguard_port: u16,
        #[arg(long, help = "Do not set up the WireGuard pod overlay (single node)")]
        no_wireguard: bool,
        #[arg(long, help = "Also serve the Docker Engine API on this Unix socket (crun runtime; published ports bind on 127.0.0.1)")]
        docker_socket: Option<PathBuf>,
    },
    #[command(
        about = "A local Kubernetes cluster for development (like kind/k3d), without Docker",
        help_template = HELP_TEMPLATE,
        after_help = DEV_AFTER_HELP
    )]
    Dev {
        #[command(subcommand)]
        command: crate::dev::DevCommand,
    },
    #[command(
        about = "A terminal UI for the dev environment: cluster, containers, images, volumes, settings, pods",
        help_template = HELP_TEMPLATE,
        after_help = UI_AFTER_HELP
    )]
    Ui(crate::ui::UiArgs),
    #[command(
        about = "Run a node agent that joins a remote litek8s API server",
        help_template = HELP_TEMPLATE,
        after_help = NODE_AGENT_AFTER_HELP
    )]
    #[command(alias = "node-agent")]
    Agent {
        #[arg(long, alias = "server", help = "API server URL, e.g. https://192.168.1.10:6443")]
        join: Option<String>,
        #[arg(long, env = "LITEK8S_TOKEN", hide_env_values = true, requires = "join", help = "Join token printed by the API server")]
        token: Option<String>,
        #[arg(long, help = "Name of this node (default: the hostname)")]
        node_name: Option<String>,
        #[arg(long, help = "This node's address for other nodes (default: the source address of the default route)")]
        node_ip: Option<std::net::IpAddr>,
        #[arg(long, help = "Directory for pod files")]
        node_dir: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "auto", help = "Container runtime: auto (crun on Linux when installed, else Docker), crun or docker")]
        runtime: crate::kube::agent::RuntimeChoice,
        #[arg(long, default_value_t = 51820, help = "UDP port of the WireGuard pod overlay")]
        wireguard_port: u16,
        #[arg(long, help = "Do not join the WireGuard pod overlay")]
        no_wireguard: bool,
        #[arg(long, value_parser = parse_duration, help = "Heartbeat grace for this node when longer than the cluster's (e.g. 15m for a laptop that sleeps)")]
        node_grace_period: Option<Duration>,
        /// Without --join: the legacy daemon's SSH node agent. With this
        /// flag it runs as the persistent agent daemon (Unix socket, weight
        /// shards resident in RAM); without, it proxies one stdin request to
        /// that daemon (starting it if needed).
        #[arg(long, hide = true, conflicts_with = "join")]
        serve: bool,
    },
    #[command(
        about = "Apply a reduced Deployment or Service manifest",
        help_template = HELP_TEMPLATE,
        after_help = APPLY_AFTER_HELP
    )]
    Apply {
        #[arg(short = 'f', long, help = "Path to a YAML deployment manifest")]
        file: PathBuf,
    },
    #[command(
        about = "List the deployments managed by litek8s",
        help_template = HELP_TEMPLATE,
        after_help = GET_AFTER_HELP
    )]
    Get {
        #[arg(
            help = "Resource type, currently `deployment`, `deployments`, `service`, `services`, `node`, or `nodes`"
        )]
        resource: String,
    },
    #[command(
        about = "Show a single deployment in detail",
        help_template = HELP_TEMPLATE,
        after_help = DESCRIBE_AFTER_HELP
    )]
    Describe {
        #[arg(
            help = "Resource type, currently `deployment`, `deployments`, `service`, `services`, `node`, or `nodes`"
        )]
        resource: String,
        #[arg(help = "Resource name")]
        name: String,
    },
    #[command(
        about = "Read recent logs from the managed container",
        help_template = HELP_TEMPLATE,
        after_help = LOGS_AFTER_HELP
    )]
    Logs {
        #[arg(help = "Deployment name")]
        name: String,
        #[arg(long, default_value_t = 100, help = "How many log lines to request")]
        tail: usize,
    },
    #[command(
        about = "Open a local TCP proxy to a published deployment port",
        help_template = HELP_TEMPLATE,
        after_help = PROXY_AFTER_HELP
    )]
    Proxy {
        #[arg(help = "Deployment name")]
        name: String,
        #[arg(
            long,
            default_value = "127.0.0.1:9000",
            help = "Local address to listen on"
        )]
        listen: String,
        #[arg(
            long,
            help = "Select a specific published port by containerPort or hostPort"
        )]
        port: Option<u16>,
    },
    #[command(
        about = "Show recent control-plane events",
        help_template = HELP_TEMPLATE,
        after_help = EVENTS_AFTER_HELP
    )]
    Events {
        #[arg(long, help = "Refresh the event view continuously")]
        watch: bool,
        #[arg(long, default_value_t = 50, help = "How many events to fetch")]
        limit: usize,
        #[arg(long, help = "Only show events for this deployment")]
        deployment: Option<String>,
        #[arg(long, help = "Only show events for this node")]
        node: Option<String>,
        #[arg(
            long,
            default_value_t = 2,
            help = "Refresh interval in seconds while watching"
        )]
        interval_secs: u64,
    },
    #[command(
        about = "Watch live resource usage for the daemon and deployments",
        help_template = HELP_TEMPLATE,
        after_help = TOP_AFTER_HELP
    )]
    Top {
        #[arg(long, help = "Refresh the table continuously")]
        watch: bool,
        #[arg(long, default_value_t = 2, help = "Refresh interval in seconds")]
        interval_secs: u64,
    },
    #[command(
        about = "Show a relationship tree of the control plane, nodes, deployments, and services",
        help_template = HELP_TEMPLATE,
        after_help = TREE_AFTER_HELP
    )]
    Tree {
        #[arg(long, help = "Refresh the tree continuously")]
        watch: bool,
        #[arg(long, default_value_t = 2, help = "Refresh interval in seconds")]
        interval_secs: u64,
    },
    #[command(
        about = "Register a remote node reachable over ssh",
        help_template = HELP_TEMPLATE,
        after_help = NODE_AFTER_HELP
    )]
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },
    #[command(
        about = "Scaffold Terraform + deploy projects for provisioning litek8s nodes",
        help_template = HELP_TEMPLATE
    )]
    Infra {
        #[command(subcommand)]
        command: InfraCommand,
    },
    #[command(
        about = "Manage stable local service listeners backed by deployments",
        help_template = HELP_TEMPLATE,
        after_help = SERVICE_AFTER_HELP
    )]
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    #[command(
        about = "Delete a deployment or service by name or manifest",
        help_template = HELP_TEMPLATE,
        after_help = DELETE_AFTER_HELP
    )]
    Delete {
        #[arg(
            short = 'f',
            long,
            help = "Delete the resource defined in this manifest"
        )]
        file: Option<PathBuf>,
        #[arg(help = "Resource type when deleting by name")]
        resource: Option<String>,
        #[arg(help = "Resource name when deleting by name")]
        name: Option<String>,
    },
    #[command(
        about = "Manage control-plane HA cluster membership",
        help_template = HELP_TEMPLATE,
        after_help = CONTROL_PLANE_AFTER_HELP
    )]
    ControlPlane {
        #[command(subcommand)]
        command: ControlPlaneCommand,
    },
    #[command(
        about = "Create and manage local multi-node clusters using Docker",
        help_template = HELP_TEMPLATE,
        after_help = CLUSTER_AFTER_HELP
    )]
    Cluster {
        #[command(subcommand)]
        command: ClusterCommand,
    },
    #[command(
        about = "Manage the WireGuard mesh that connects nodes across clouds",
        help_template = HELP_TEMPLATE
    )]
    Vpn {
        #[command(subcommand)]
        command: VpnCommand,
    },
    #[command(
        about = "Run distributed tensor (matrix) compute across the cluster nodes",
        help_template = HELP_TEMPLATE,
        after_help = TENSOR_AFTER_HELP
    )]
    Tensor {
        #[command(subcommand)]
        command: TensorCommand,
    },
    #[command(
        about = "Run a feed-forward layer pipeline sharded across the cluster",
        help_template = HELP_TEMPLATE,
        after_help = PIPELINE_AFTER_HELP
    )]
    Pipeline {
        #[command(subcommand)]
        command: PipelineCommand,
    },
    #[cfg(feature = "llm")]
    #[command(
        about = "Inspect and distribute model weights (safetensors)",
        help_template = HELP_TEMPLATE
    )]
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    #[cfg(feature = "llm")]
    #[command(
        about = "Generate text with a local LLM (safetensors model dir)",
        help_template = HELP_TEMPLATE
    )]
    Llm {
        #[command(subcommand)]
        command: LlmCommand,
    },
}

#[cfg(feature = "llm")]
#[derive(Debug, Subcommand)]
enum LlmCommand {
    #[command(
        about = "Generate text from a prompt using SafeTensors or GGUF weights",
        help_template = HELP_TEMPLATE
    )]
    Generate {
        #[arg(
            long,
            help = "Model directory containing config.json and tokenizer.json"
        )]
        model_dir: PathBuf,
        #[arg(help = "Prompt text")]
        prompt: String,
        #[arg(long, default_value_t = 20, help = "Max new tokens to generate")]
        max_new: usize,
        #[arg(
            long,
            help = "Stream native weights from mmap instead of loading all weights as f64"
        )]
        mmap: bool,
        #[arg(
            long,
            value_name = "FILE",
            conflicts_with = "mmap",
            help = "Use a mixed-quantized GGUF file through mmap (Q4_K_M recipe)"
        )]
        gguf: Option<PathBuf>,
        #[arg(
            long,
            value_name = "DIR",
            value_delimiter = ',',
            help = "Enable disk-backed KV cache; repeat or comma-separate directories to stripe layers"
        )]
        kv_cache_dir: Vec<PathBuf>,
        #[arg(
            long,
            default_value_t = 32768,
            help = "Maximum token capacity of the grow-on-demand disk KV cache"
        )]
        kv_max_tokens: usize,
        #[arg(
            long,
            default_value_t = 16,
            help = "RAM tile buffer used while scanning the disk KV cache"
        )]
        kv_buffer_mb: usize,
        #[arg(
            long,
            help = "Keep KV cache files after generation instead of deleting them"
        )]
        keep_kv_cache: bool,
    },
    #[command(
        about = "Run a model distributed across peer agents over the LAN (run on a coordinator node)",
        help_template = HELP_TEMPLATE
    )]
    Distributed {
        #[arg(
            long,
            help = "Model directory (config.json, model.safetensors, tokenizer.json)"
        )]
        model_dir: PathBuf,
        #[arg(
            long,
            help = "Peer private IPs to shard weights across",
            value_delimiter = ','
        )]
        peers: Vec<String>,
        #[arg(help = "Prompt text")]
        prompt: String,
        #[arg(long, default_value_t = 8, help = "Max new tokens to generate")]
        max_new: usize,
        #[arg(long, help = "Write a manifest here so later runs skip distribution")]
        save_manifest: Option<PathBuf>,
    },
    #[command(
        about = "Generate from a preloaded distributed model (skips the cold distribution)",
        help_template = HELP_TEMPLATE
    )]
    Preloaded {
        #[arg(long, help = "Model directory (for tokenizer + embedding)")]
        model_dir: PathBuf,
        #[arg(long, help = "Manifest written by `llm distributed --save-manifest`")]
        manifest: PathBuf,
        #[arg(help = "Prompt text")]
        prompt: String,
        #[arg(long, default_value_t = 8, help = "Max new tokens to generate")]
        max_new: usize,
    },
}

#[cfg(feature = "llm")]
#[derive(Debug, Subcommand)]
enum ModelCommand {
    #[command(
        about = "List the tensors in a safetensors file (name, dtype, shape)",
        help_template = HELP_TEMPLATE
    )]
    Inspect {
        #[arg(help = "Path to a .safetensors file")]
        file: PathBuf,
    },
    #[command(
        about = "Distribute a safetensors model across the cluster nodes",
        help_template = HELP_TEMPLATE
    )]
    Load {
        #[arg(help = "Model name to register")]
        name: String,
        #[arg(short = 'f', long, help = "Path to a .safetensors file")]
        file: PathBuf,
        #[arg(
            long,
            help = "Tensor names in layer order (default: every 2D tensor, sorted)"
        )]
        tensors: Vec<String>,
        #[arg(long, help = "Do not apply ReLU between layers")]
        no_relu: bool,
    },
    #[command(
        about = "List models distributed across the cluster",
        help_template = HELP_TEMPLATE
    )]
    List,
    #[command(
        about = "Forget a model manifest (cached shards stay on nodes)",
        help_template = HELP_TEMPLATE
    )]
    Rm {
        #[arg(help = "Model name")]
        name: String,
    },
    #[command(
        about = "Run an input through a loaded model's resident weights",
        help_template = HELP_TEMPLATE
    )]
    Run {
        #[arg(help = "Model name")]
        name: String,
        #[arg(
            long,
            default_value_t = 1,
            help = "Batch size (rows of generated input)"
        )]
        batch: usize,
        #[arg(long, default_value_t = 7, help = "Seed for the generated input")]
        seed: u64,
        #[arg(long, help = "Print the full output activation")]
        show_output: bool,
    },
}

#[derive(Debug, Subcommand)]
enum PipelineCommand {
    #[command(
        about = "Run an input through stacked weight layers, each column-sharded by node",
        help_template = HELP_TEMPLATE
    )]
    Run {
        #[arg(
            short = 'f',
            long,
            help = "YAML model file with `input:` and `layers:` (in_dim/out_dim/seed/relu)"
        )]
        file: Option<PathBuf>,
        #[arg(long, help = "Generate a deterministic model: number of layers")]
        layers: Option<usize>,
        #[arg(
            long,
            default_value_t = 512,
            help = "Hidden dimension for generated layers (square)"
        )]
        dim: usize,
        #[arg(
            long,
            default_value_t = 1,
            help = "Batch size (rows of the generated input)"
        )]
        batch: usize,
        #[arg(long, default_value_t = 7, help = "Seed for the generated model")]
        seed: u64,
        #[arg(long, help = "Print the full output activation")]
        show_output: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TensorCommand {
    #[command(
        about = "Multiply two matrices, fanning row bands across the cluster",
        help_template = HELP_TEMPLATE
    )]
    Multiply {
        #[arg(
            short = 'f',
            long,
            help = "YAML file with `left:` and `right:` matrices (lists of number rows)"
        )]
        file: Option<PathBuf>,
        #[arg(
            long,
            help = "Generate a deterministic NxN input instead of reading a file"
        )]
        size: Option<usize>,
        #[arg(
            long,
            default_value_t = 42,
            help = "Seed for --size generation (reproducible)"
        )]
        seed: u64,
        #[arg(
            long,
            help = "Print the full result matrix (off by default for large sizes)"
        )]
        show_result: bool,
        #[arg(
            long,
            help = "Re-send the right matrix on every call instead of caching it per node"
        )]
        no_cache: bool,
    },
}

#[derive(Debug, Subcommand)]
enum VpnCommand {
    #[command(
        about = "Generate a hub key pair and print a join token for spokes",
        help_template = HELP_TEMPLATE
    )]
    Init {
        #[arg(
            long,
            default_value = "10.42.0.0/24",
            help = "Private IPv4 CIDR for the mesh (e.g. 10.42.0.0/24)"
        )]
        cidr: String,
        #[arg(
            long,
            help = "Publicly reachable hub endpoint as `host:port` (e.g. 54.123.45.67:51820)"
        )]
        public_endpoint: String,
    },
    #[command(
        about = "Show the current VPN state (hub network, peers, spoke membership)",
        help_template = HELP_TEMPLATE
    )]
    Status,
    #[command(
        about = "Register a spoke pub key on the hub (escape hatch when SSH bootstrap is unavailable)",
        help_template = HELP_TEMPLATE
    )]
    AddPeer {
        #[arg(long, help = "Logical node name for the spoke")]
        name: String,
        #[arg(long, help = "Spoke base64 public key")]
        pubkey: String,
    },
    #[command(
        about = "Remove a registered spoke from the hub",
        help_template = HELP_TEMPLATE
    )]
    RemovePeer {
        #[arg(long, help = "Logical node name for the spoke")]
        name: String,
    },
    #[command(
        about = "Write a wg-quick-compatible config file for the hub or current spoke",
        help_template = HELP_TEMPLATE
    )]
    ShowConfig {
        #[arg(
            long,
            value_parser = ["hub", "spoke"],
            default_value = "spoke",
            help = "Which side of the mesh to dump the config for"
        )]
        role: String,
        #[arg(
            long,
            default_value_t = 51820,
            help = "Hub UDP listen port (only used for --role hub)"
        )]
        listen_port: u16,
        #[arg(long, help = "Path to write the config. Defaults to stdout if omitted")]
        output: Option<PathBuf>,
    },
    #[command(
        about = "Apply a join token on this spoke (generates local key, persists pending state)",
        help_template = HELP_TEMPLATE
    )]
    Join {
        #[arg(long, help = "Join token previously printed by `vpn init`")]
        token: String,
        #[arg(
            long,
            help = "SSH target for the hub (e.g. ec2-user@54.123.45.67); enables one-shot SSH bootstrap"
        )]
        ssh_hub: Option<String>,
        #[arg(
            long,
            help = "Logical node name to register this spoke as on the hub",
            default_value = ""
        )]
        node_name: String,
        #[arg(
            long,
            default_value = "~/.local/bin/litek8s",
            help = "Path to the litek8s binary on the hub (only used with --ssh-hub)"
        )]
        remote_binary: String,
    },
    #[command(
        about = "Finalize the spoke after the hub has assigned an IP",
        help_template = HELP_TEMPLATE
    )]
    Complete {
        #[arg(long, help = "IPv4 address the hub assigned to this spoke")]
        ip: String,
    },
}

#[derive(Debug, Subcommand)]
enum NodeCommand {
    #[command(about = "Add or update a remote node reachable over ssh")]
    Add {
        #[arg(help = "Logical node name used from manifests via spec.template.spec.nodeName")]
        name: String,
        #[arg(long, help = "SSH target such as `server77` or `ubuntu@10.0.0.7`")]
        ssh: String,
        #[arg(
            long,
            help = "Build a compatible litek8s binary locally, copy it to the remote host, and verify it before registering the node"
        )]
        install: bool,
        #[arg(
            long = "label",
            value_name = "KEY=VALUE",
            help = "Attach a label to the node; can be repeated"
        )]
        labels: Vec<String>,
        #[arg(
            long,
            help = "Path to the litek8s binary on the remote machine",
            default_value = "~/.local/bin/litek8s"
        )]
        remote_binary: String,
        #[arg(
            long = "ssh-key",
            value_name = "PATH",
            help = "Path to the SSH private key (sets IdentitiesOnly=yes)"
        )]
        ssh_key: Option<String>,
        #[arg(
            long,
            help = "VPN IPv4 of the remote node (e.g. 10.42.0.2). Once set, the daemon prefers this over the public SSH target."
        )]
        vpn_ip: Option<String>,
    },
    #[command(about = "Mark a node unschedulable for new selector-based or default placements")]
    Cordon {
        #[arg(help = "Logical node name")]
        name: String,
    },
    #[command(about = "Cordon a node and evacuate movable workloads to other schedulable nodes")]
    Drain {
        #[arg(help = "Logical node name")]
        name: String,
    },
    #[command(about = "Re-open a node for scheduling")]
    Uncordon {
        #[arg(help = "Logical node name")]
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum InfraCommand {
    #[command(about = "Scaffold a Terraform + deploy project for provisioning litek8s nodes")]
    Init {
        #[arg(
            long,
            default_value = ".",
            help = "Directory to scaffold into (created if missing)"
        )]
        dir: PathBuf,
        #[arg(
            long,
            help = "Project name/prefix used in generated files",
            default_value = "litek8s-app"
        )]
        name: String,
        #[arg(long, help = "Overwrite existing files instead of skipping them")]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ControlPlaneCommand {
    #[command(about = "Install litek8s on a remote host and join it to the HA cluster")]
    Join {
        #[arg(help = "Logical name for the new control-plane peer")]
        name: String,
        #[arg(long, help = "SSH target such as `dev@192.168.0.77`")]
        ssh: String,
        #[arg(
            long,
            help = "Build a compatible binary locally, copy it to the remote host"
        )]
        install: bool,
        #[arg(
            long,
            default_value_t = 4001,
            help = "TCP port the remote peer will use for Raft communication"
        )]
        peer_port: u16,
        #[arg(
            long,
            help = "Path to the litek8s binary on the remote machine",
            default_value = "~/.local/bin/litek8s"
        )]
        remote_binary: String,
        #[arg(
            long = "ssh-key",
            value_name = "PATH",
            help = "Path to the SSH private key (sets IdentitiesOnly=yes)"
        )]
        ssh_key: Option<String>,
    },
    #[command(about = "Show the current HA cluster status")]
    Status,
}

#[derive(Debug, Subcommand)]
enum ClusterCommand {
    #[command(about = "Create a local multi-node cluster in Docker containers")]
    Create {
        #[arg(help = "Cluster name")]
        name: String,
        #[arg(long, default_value_t = 1, help = "Number of control-plane nodes")]
        control_planes: u32,
        #[arg(long, default_value_t = 0, help = "Number of worker nodes")]
        workers: u32,
        #[arg(
            long,
            help = "Path to a pre-built Linux litek8s binary (cross-compiles automatically if omitted)"
        )]
        binary: Option<PathBuf>,
    },
    #[command(about = "Delete a local cluster and its containers")]
    Delete {
        #[arg(help = "Cluster name")]
        name: String,
    },
    #[command(about = "List running local clusters")]
    List,
    #[command(
        about = "Apply a declarative cluster manifest (kind: Cluster) idempotently",
        help_template = HELP_TEMPLATE
    )]
    Apply {
        #[arg(short = 'f', long, help = "Path to a cluster manifest YAML")]
        file: PathBuf,
    },
    #[command(
        about = "Show what `cluster apply` would change without mutating state",
        help_template = HELP_TEMPLATE
    )]
    Diff {
        #[arg(short = 'f', long, help = "Path to a cluster manifest YAML")]
        file: PathBuf,
    },
    #[command(
        about = "Tear down workloads and worker nodes declared in a cluster manifest (preserves VPN state)",
        help_template = HELP_TEMPLATE
    )]
    Destroy {
        #[arg(short = 'f', long, help = "Path to a cluster manifest YAML")]
        file: PathBuf,
    },
    #[command(
        about = "Dump the current cluster state as a kind: Cluster manifest on stdout",
        help_template = HELP_TEMPLATE
    )]
    Export {
        #[arg(
            long,
            default_value = "exported-cluster",
            help = "metadata.name to embed in the exported manifest"
        )]
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    #[command(about = "Create or update a local service listener")]
    Add {
        #[arg(help = "Logical service name")]
        name: String,
        #[arg(long, help = "Target deployment name")]
        deployment: String,
        #[arg(
            long,
            default_value = "127.0.0.1:9100",
            help = "Local listen address for the service"
        )]
        listen: String,
        #[arg(
            long,
            help = "Select a specific published deployment port by containerPort or hostPort"
        )]
        port: Option<u16>,
    },
}

enum DaemonEndpoint {
    Unix(PathBuf),
    Tcp(String),
}

impl DaemonEndpoint {
    async fn request(&self, request: Request) -> Result<Response> {
        match self {
            DaemonEndpoint::Unix(path) => protocol::request(path, request).await,
            DaemonEndpoint::Tcp(addr) => protocol::request_tcp(addr, request).await,
        }
    }
}

impl crate::cluster_orchestrator::DaemonEndpoint for DaemonEndpoint {
    async fn request(&self, request: Request) -> Result<Response> {
        DaemonEndpoint::request(self, request).await
    }
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let endpoint = if let Some(server) = cli.server {
        DaemonEndpoint::Tcp(server)
    } else {
        DaemonEndpoint::Unix(cli.socket_path.unwrap_or_else(config::default_socket_path))
    };

    match cli.command {
        // `infra init` is purely local filesystem work — it never talks to the
        // daemon, so it is handled here without using `endpoint`.
        Command::Infra { command } => match command {
            InfraCommand::Init { dir, name, force } => crate::infra::scaffold(&dir, &name, force),
        },
        Command::Apiserver {
            listen,
            data_dir,
            kubeconfig,
            log_requests,
            no_node,
            node_name,
            node_dir,
            cluster_dns,
            no_cluster_dns,
            in_cluster_port,
            runtime,
            node_ip,
            advertise_address,
            cluster_cidr,
            node_cidr_mask_size,
            token,
            node_monitor_grace_period,
            default_not_ready_toleration_seconds,
            default_unreachable_toleration_seconds,
            wireguard_port,
            no_wireguard,
            docker_socket,
        } => {
            let docker = match &docker_socket {
                Some(path) => {
                    let _ = std::fs::remove_file(path);
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let listener = std::os::unix::net::UnixListener::bind(path).with_context(|| format!("failed to listen on {}", path.display()))?;
                    Some(crate::docker::Settings {
                        root: data_dir.clone().unwrap_or_else(|| std::env::temp_dir().join("litek8s")).join("docker"),
                        listener: Some(listener),
                        tcp: None,
                        publisher: crate::docker::Publisher::Local,
                        socket_aliases: vec![path.to_string_lossy().to_string()],
                    })
                }
                None => None,
            };
            crate::kube::serve(crate::kube::ApiServerConfig {
                listen,
                data_dir,
                kubeconfig_out: kubeconfig,
                log_requests,
                run_node: !no_node,
                node_name,
                node_dir,
                cluster_dns: (!no_cluster_dns).then_some(cluster_dns),
                in_cluster_port: (in_cluster_port != 0).then_some(in_cluster_port),
                runtime,
                node_ip,
                advertise_address,
                network: crate::kube::join::ClusterNetwork { cluster_cidr, node_mask: node_cidr_mask_size },
                token,
                lifecycle: crate::kube::controllers::NodeLifecycle {
                    grace: node_monitor_grace_period,
                    not_ready_toleration_secs: default_not_ready_toleration_seconds,
                    unreachable_toleration_secs: default_unreachable_toleration_seconds,
                },
                wireguard_port: (!no_wireguard).then_some(wireguard_port),
                listener: None,
                extra_listen: None,
                disk_root: None,
                node_port_offset: 0,
                docker,
            })
            .await
        }
        Command::Dev { command } => crate::dev::run(command).await,
        Command::Ui(args) => crate::ui::run(args).await,
        Command::Agent { join: Some(join), token, node_name, node_ip, node_dir, runtime, wireguard_port, no_wireguard, node_grace_period, .. } => {
            let token = token.context("--token is required with --join")?;
            let node_name = node_name.unwrap_or_else(crate::kube::agent::default_node_name);
            let node_dir = node_dir.unwrap_or_else(crate::kube::agent::default_root_dir);
            let mut client = crate::kube::join::Client::new(&join, &token, &node_name, &node_dir)?;
            // Without a saved join the node cannot set up its pod network:
            // wait for the server.
            let joined = loop {
                match client.join().await {
                    Ok(j) => break j,
                    Err(e) if format!("{e:#}").contains("refused (") => return Err(e),
                    Err(e) => {
                        eprintln!("node-agent: cannot join yet: {e:#}");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            };
            eprintln!("node-agent: joined as {node_name} (pod CIDR {})", joined.pod_cidr);
            let network = crate::kube::agent::NodeNetwork {
                pod_cidr: joined.pod_cidr,
                cluster_cidr: joined.cluster_cidr,
                node_ip: node_ip.map(|ip| ip.to_string()).unwrap_or_else(crate::kube::agent::local_ip),
                wireguard_port: (!no_wireguard).then_some(wireguard_port),
            };
            let agent = crate::kube::agent::node_agent_with(runtime, Some(node_name), Some(node_dir), network, node_grace_period).await?;
            crate::kube::metrics::spawn_memory_trim();
            crate::kube::agent::run_remote(agent, client).await
        }
        Command::Serve {
            state_file,
            poll_interval_secs,
            peer_id,
            peer_addr,
            join,
            api_addr,
            node_resource_interval_secs,
        } => {
            let socket_path = match &endpoint {
                DaemonEndpoint::Unix(path) => path.clone(),
                DaemonEndpoint::Tcp(_) => config::default_socket_path(),
            };
            let config = ServerConfig {
                socket_path,
                state_file: match state_file {
                    Some(path) => path,
                    None => config::default_state_file()?,
                },
                poll_interval: Duration::from_secs(poll_interval_secs),
                cleanup_orphans: true,
                peer_id,
                peer_addr,
                join_addr: join,
                api_addr,
                node_resource_interval: Duration::from_secs(node_resource_interval_secs),
            };
            daemon::serve(config).await
        }
        Command::Apply { file } => {
            let manifest = read_manifest(&file)?;
            let response = endpoint.request(Request::Apply { manifest }).await?;
            print_response(response)
        }
        Command::Get { resource } => {
            let response = match resource.as_str() {
                "deployment" | "deployments" => endpoint.request(Request::GetDeployments).await?,
                "service" | "services" => endpoint.request(Request::GetServices).await?,
                "node" | "nodes" => endpoint.request(Request::GetNodes).await?,
                _ => {
                    ensure_resource(
                        &resource,
                        &[
                            "deployment",
                            "deployments",
                            "service",
                            "services",
                            "node",
                            "nodes",
                        ],
                    )?;
                    unreachable!()
                }
            };
            print_response(response)
        }
        Command::Describe { resource, name } => {
            let response = match resource.as_str() {
                "deployment" | "deployments" => {
                    endpoint
                        .request(Request::DescribeDeployment { name })
                        .await?
                }
                "service" | "services" => {
                    endpoint.request(Request::DescribeService { name }).await?
                }
                "node" | "nodes" => endpoint.request(Request::DescribeNode { name }).await?,
                _ => {
                    ensure_resource(
                        &resource,
                        &[
                            "deployment",
                            "deployments",
                            "service",
                            "services",
                            "node",
                            "nodes",
                        ],
                    )?;
                    unreachable!()
                }
            };
            print_response(response)
        }
        Command::Logs { name, tail } => {
            let response = endpoint.request(Request::Logs { name, tail }).await?;
            print_response(response)
        }
        Command::Proxy { name, listen, port } => run_proxy(&endpoint, &name, &listen, port).await,
        Command::Events {
            watch,
            limit,
            deployment,
            node,
            interval_secs,
        } => {
            run_events(
                &endpoint,
                watch,
                limit,
                deployment,
                node,
                Duration::from_secs(interval_secs),
            )
            .await
        }
        Command::Top {
            watch,
            interval_secs,
        } => run_top(&endpoint, watch, Duration::from_secs(interval_secs)).await,
        Command::Tree {
            watch,
            interval_secs,
        } => run_tree(&endpoint, watch, Duration::from_secs(interval_secs)).await,
        Command::Node { command } => match command {
            NodeCommand::Add {
                name,
                ssh,
                install,
                labels,
                remote_binary,
                ssh_key,
                vpn_ip,
            } => {
                let labels = parse_labels(&labels)?;
                if install {
                    let project_root = resolve_project_root()?;
                    println!("{}", title("Remote Bootstrap"));
                    println!(
                        "{} {} {}",
                        muted("building and installing a compatible litek8s binary for"),
                        label(&ssh),
                        muted("before node registration")
                    );
                    let summary = node::install_remote_agent(
                        &ssh,
                        &remote_binary,
                        ssh_key.as_deref(),
                        &project_root,
                    )
                    .await?;
                    println!("{} {}", label("platform"), summary.platform_label);
                    println!("{} {}", label("artifact"), summary.artifact_path.display());
                    println!("{} {}", label("remote"), remote_binary);
                    println!();
                }
                let response = endpoint
                    .request(Request::AddNode {
                        name,
                        ssh_target: ssh,
                        remote_binary: Some(remote_binary),
                        identity_file: ssh_key,
                        labels,
                        vpn_ip,
                    })
                    .await?;
                print_response(response)
            }
            NodeCommand::Cordon { name } => {
                let response = endpoint
                    .request(Request::SetNodeScheduling {
                        name,
                        unschedulable: true,
                    })
                    .await?;
                print_response(response)
            }
            NodeCommand::Drain { name } => {
                let response = endpoint.request(Request::DrainNode { name }).await?;
                print_response(response)
            }
            NodeCommand::Uncordon { name } => {
                let response = endpoint
                    .request(Request::SetNodeScheduling {
                        name,
                        unschedulable: false,
                    })
                    .await?;
                print_response(response)
            }
        },
        Command::Service { command } => match command {
            ServiceCommand::Add {
                name,
                deployment,
                listen,
                port,
            } => {
                let response = endpoint
                    .request(Request::AddService {
                        name,
                        deployment_name: deployment,
                        listen_addr: listen,
                        target_port: port,
                    })
                    .await?;
                print_response(response)
            }
        },
        Command::Delete {
            file,
            resource,
            name,
        } => {
            let request = if let Some(file) = file {
                Request::DeleteManifest {
                    manifest: read_manifest(&file)?,
                }
            } else {
                let resource = resource.context("resource is required when -f is not used")?;
                ensure_resource(
                    &resource,
                    &["deployment", "deployments", "service", "services"],
                )?;
                let name = name.context("deployment name is required")?;
                match resource.as_str() {
                    "deployment" | "deployments" => Request::DeleteDeployment { name },
                    "service" | "services" => Request::DeleteService { name },
                    _ => unreachable!(),
                }
            };

            let response = endpoint.request(request).await?;
            print_response(response)
        }
        Command::ControlPlane { command } => match command {
            ControlPlaneCommand::Status => {
                let response = endpoint.request(Request::GetClusterStatus).await?;
                match response {
                    Response::ClusterStatus {
                        ha_enabled,
                        node_id,
                        leader_id,
                        state,
                        members,
                    } => {
                        if !ha_enabled {
                            println!("{}", muted("HA is not enabled (standalone mode)"));
                            return Ok(());
                        }
                        println!("{}", title("Control Plane HA"));
                        println!(
                            "{} {}",
                            label("node id"),
                            node_id.map(|n| n.to_string()).unwrap_or_default()
                        );
                        println!("{} {}", label("state"), state);
                        println!(
                            "{} {}",
                            label("leader"),
                            leader_id
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| "(none)".into())
                        );
                        println!();
                        println!(
                            "{:<10} {:<25} {}",
                            muted("NODE ID"),
                            muted("ADDRESS"),
                            muted("ROLE")
                        );
                        for member in &members {
                            let role = if member.is_leader {
                                "leader"
                            } else {
                                "follower"
                            };
                            println!("{:<10} {:<25} {}", member.node_id, member.addr, role);
                        }
                        Ok(())
                    }
                    other => print_response(other),
                }
            }
            ControlPlaneCommand::Join {
                name,
                ssh,
                install,
                peer_port,
                remote_binary,
                ssh_key,
            } => {
                println!("{}", title("Control Plane Join"));

                // Step 1: install binary on remote if requested
                if install {
                    let project_root = resolve_project_root()?;
                    println!(
                        "{} {} {}",
                        muted("installing litek8s on"),
                        label(&ssh),
                        muted("...")
                    );
                    let summary = node::install_remote_agent(
                        &ssh,
                        &remote_binary,
                        ssh_key.as_deref(),
                        &project_root,
                    )
                    .await?;
                    println!("{} {}", label("platform"), summary.platform_label);
                    println!("{} {}", label("remote"), remote_binary);
                }

                // Step 2: get cluster status to determine next node ID
                let status_resp = endpoint.request(Request::GetClusterStatus).await?;
                let next_id = match &status_resp {
                    Response::ClusterStatus { members, .. } => {
                        members.iter().map(|m| m.node_id).max().unwrap_or(0) + 1
                    }
                    _ => bail!("unexpected response from daemon"),
                };
                let leader_port = match &status_resp {
                    Response::ClusterStatus { members, .. } => members
                        .iter()
                        .find(|m| m.is_leader)
                        .and_then(|m| m.addr.rsplit_once(':').map(|(_, p)| p.to_string()))
                        .unwrap_or_else(|| "4001".into()),
                    _ => unreachable!(),
                };

                // Step 3: detect remote IP by extracting the host from --ssh target
                // and detect local IP as seen from the remote
                let ssh_host = ssh.rsplit_once('@').map(|(_, h)| h).unwrap_or(&ssh);
                let remote_addr = format!("{ssh_host}:{peer_port}");
                println!("{} {}", label("remote addr"), remote_addr);

                // Detect local IP as seen from the remote (the route to us)
                let local_ip = node::run_ssh_capture_pub(
                    &ssh,
                    &format!("ip route get {} | awk '{{for(i=1;i<=NF;i++) if($i==\"src\") print $(i+1)}}'", ssh_host),
                    ssh_key.as_deref(),
                )
                .await
                .unwrap_or_else(|_| "127.0.0.1".into());
                let local_peer_addr = if local_ip.is_empty() {
                    format!("127.0.0.1:{leader_port}")
                } else {
                    format!("{local_ip}:{leader_port}")
                };
                println!("{} {}", label("leader addr"), local_peer_addr);

                // Step 4: start remote daemon with --join
                println!(
                    "{} {} {}",
                    muted("starting remote daemon as node"),
                    label(&next_id.to_string()),
                    muted("...")
                );
                let serve_cmd = format!(
                    "{remote_binary} serve --peer-id {next_id} --peer-addr 0.0.0.0:{peer_port} --join {local_peer_addr}"
                );
                node::run_ssh_capture_pub(
                    &ssh,
                    &format!("{serve_cmd} </dev/null >/tmp/litek8s-cp.log 2>&1 & echo started"),
                    ssh_key.as_deref(),
                )
                .await
                .context("failed to start remote daemon")?;

                // Step 5: wait for remote to be listening
                println!("{}", muted("waiting for remote peer..."));
                for _ in 0..15 {
                    if tokio::net::TcpStream::connect(&remote_addr).await.is_ok() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }

                // Step 6: tell leader to add the new member
                let join_resp = endpoint
                    .request(Request::JoinCluster {
                        node_id: next_id,
                        addr: remote_addr.clone(),
                    })
                    .await?;
                print_response(join_resp)?;

                println!(
                    "\n{} {} {} {}",
                    ok("ok"),
                    label(&name),
                    muted("joined as node"),
                    label(&next_id.to_string()),
                );
                Ok(())
            }
        },
        Command::Cluster { command } => match command {
            ClusterCommand::Create {
                name,
                control_planes,
                workers,
                binary,
            } => {
                println!("{}", title("Cluster Create"));
                let config = crate::cluster::ClusterConfig {
                    name,
                    control_planes,
                    workers,
                    binary,
                };
                let info = crate::cluster::create_cluster(config).await?;
                println!();
                println!(
                    "{} cluster {} with {} node(s)",
                    ok("ok"),
                    label(&info.name),
                    info.nodes.len()
                );
                println!();
                println!("{}", muted("connect with:"));
                println!("  litek8s --server {} get deployments", info.api_addr);
                Ok(())
            }
            ClusterCommand::Delete { name } => {
                crate::cluster::delete_cluster(&name).await?;
                println!("{} cluster {} deleted", ok("ok"), label(&name));
                Ok(())
            }
            ClusterCommand::List => {
                let entries = crate::cluster::list_clusters().await?;
                if entries.is_empty() {
                    println!("{}", muted("no clusters found"));
                    return Ok(());
                }
                println!(
                    "{:<20} {:<5} {:<8} {}",
                    muted("NAME"),
                    muted("CP"),
                    muted("WORKERS"),
                    muted("API")
                );
                for entry in &entries {
                    println!(
                        "{:<20} {:<5} {:<8} {}",
                        entry.name, entry.control_planes, entry.workers, entry.api_addr
                    );
                }
                Ok(())
            }
            ClusterCommand::Apply { file } => run_cluster_apply(&endpoint, &file).await,
            ClusterCommand::Diff { file } => run_cluster_diff(&endpoint, &file).await,
            ClusterCommand::Destroy { file } => run_cluster_destroy(&endpoint, &file).await,
            ClusterCommand::Export { name } => run_cluster_export(&endpoint, &name).await,
        },
        Command::Agent { join: None, serve, .. } => {
            if serve {
                crate::node::run_agent_serve().await
            } else {
                crate::node::run_agent_proxy().await
            }
        }
        Command::Vpn { command } => match command {
            VpnCommand::Init {
                cidr,
                public_endpoint,
            } => run_vpn_init(&cidr, &public_endpoint),
            VpnCommand::Status => run_vpn_status(),
            VpnCommand::AddPeer { name, pubkey } => run_vpn_add_peer(&name, &pubkey),
            VpnCommand::RemovePeer { name } => run_vpn_remove_peer(&name),
            VpnCommand::ShowConfig {
                role,
                listen_port,
                output,
            } => run_vpn_show_config(&role, listen_port, output.as_deref()),
            VpnCommand::Join {
                token,
                ssh_hub,
                node_name,
                remote_binary,
            } => run_vpn_join(&token, ssh_hub.as_deref(), &node_name, &remote_binary).await,
            VpnCommand::Complete { ip } => run_vpn_complete(&ip),
        },
        Command::Tensor { command } => match command {
            TensorCommand::Multiply {
                file,
                size,
                seed,
                show_result,
                no_cache,
            } => {
                run_tensor_multiply(
                    &endpoint,
                    file.as_deref(),
                    size,
                    seed,
                    show_result,
                    !no_cache,
                )
                .await
            }
        },
        Command::Pipeline { command } => match command {
            PipelineCommand::Run {
                file,
                layers,
                dim,
                batch,
                seed,
                show_output,
            } => {
                run_pipeline(
                    &endpoint,
                    file.as_deref(),
                    layers,
                    dim,
                    batch,
                    seed,
                    show_output,
                )
                .await
            }
        },
        #[cfg(feature = "llm")]
        Command::Llm { command } => match command {
            LlmCommand::Generate {
                model_dir,
                prompt,
                max_new,
                mmap,
                gguf,
                kv_cache_dir,
                kv_max_tokens,
                kv_buffer_mb,
                keep_kv_cache,
            } => run_llm_generate(
                &model_dir,
                &prompt,
                max_new,
                mmap,
                gguf.as_deref(),
                &kv_cache_dir,
                kv_max_tokens,
                kv_buffer_mb,
                keep_kv_cache,
            ),
            LlmCommand::Distributed {
                model_dir,
                peers,
                prompt,
                max_new,
                save_manifest,
            } => {
                run_llm_distributed(
                    &model_dir,
                    &peers,
                    &prompt,
                    max_new,
                    save_manifest.as_deref(),
                )
                .await
            }
            LlmCommand::Preloaded {
                model_dir,
                manifest,
                prompt,
                max_new,
            } => run_llm_preloaded(&model_dir, &manifest, &prompt, max_new).await,
        },
        #[cfg(feature = "llm")]
        Command::Model { command } => match command {
            ModelCommand::Inspect { file } => run_model_inspect(&file),
            ModelCommand::Load {
                name,
                file,
                tensors,
                no_relu,
            } => run_model_load(&endpoint, &name, &file, tensors, !no_relu).await,
            ModelCommand::List => run_model_list(&endpoint).await,
            ModelCommand::Rm { name } => {
                let response = endpoint.request(Request::DeleteModel { name }).await?;
                print_response(response)
            }
            ModelCommand::Run {
                name,
                batch,
                seed,
                show_output,
            } => run_model_run(&endpoint, &name, batch, seed, show_output).await,
        },
    }
}

#[cfg(feature = "llm")]
async fn run_model_load(
    endpoint: &DaemonEndpoint,
    name: &str,
    file: &Path,
    tensors: Vec<String>,
    relu: bool,
) -> Result<()> {
    // Default layer order: every 2D tensor in the file, sorted by name.
    let layers: Vec<protocol::ModelLayerSpec> = if tensors.is_empty() {
        crate::model::inspect(file)?
            .into_iter()
            .filter(|t| t.shape.len() == 2)
            .map(|t| protocol::ModelLayerSpec {
                tensor: t.name,
                relu,
            })
            .collect()
    } else {
        tensors
            .into_iter()
            .map(|tensor| protocol::ModelLayerSpec { tensor, relu })
            .collect()
    };
    ensure!(!layers.is_empty(), "no 2D tensors to distribute");

    // Resolve the path to absolute so the daemon (same host) can read it.
    let abs = std::fs::canonicalize(file)
        .with_context(|| format!("failed to resolve model path {}", file.display()))?;

    println!("{}", title("Distribute Model"));
    println!(
        "{} {} {} {} layers",
        label("model"),
        name,
        muted("—"),
        layers.len()
    );
    println!();

    let response = endpoint
        .request(Request::LoadModel {
            name: name.to_string(),
            file_path: abs.to_string_lossy().into_owned(),
            layers,
        })
        .await?;

    match response {
        Response::ModelLoaded {
            name,
            layers,
            wall_ms,
        } => {
            for l in &layers {
                println!(
                    "{} {} {} {}x{}",
                    label(&format!("layer {}", l.index)),
                    muted(&l.tensor),
                    muted("—"),
                    l.in_dim,
                    l.out_dim
                );
                for s in &l.shards {
                    let tag = if s.already_cached {
                        ok("[cached]")
                    } else {
                        muted("[uploaded]")
                    };
                    println!(
                        "    {:<16} {:>2}c  cols {:>6}..{:<6} ({:>5})  {:>8} KB  {:>7}  {}",
                        s.node_name,
                        s.cpu_cores,
                        s.col_start,
                        s.col_start + s.cols,
                        s.cols,
                        s.bytes / 1024,
                        muted(&format!("{}ms", s.elapsed_ms)),
                        tag
                    );
                }
            }
            println!();
            println!("{} model/{} distributed", ok("ok"), name);
            println!("{} {}", label("wall"), ok(&format!("{wall_ms}ms")));
            Ok(())
        }
        Response::Error { message } => bail!(message),
        other => bail!("unexpected response to model load: {other:?}"),
    }
}

#[cfg(feature = "llm")]
async fn run_model_list(endpoint: &DaemonEndpoint) -> Result<()> {
    let response = endpoint.request(Request::ListModels).await?;
    match response {
        Response::Models { items } => {
            if items.is_empty() {
                println!("{}", muted("no models loaded"));
                return Ok(());
            }
            println!("{}", title(&format!("Models ({} total)", items.len())));
            println!();
            println!(
                "{:<20} {:>6} {:>8} {:>8} {:>10}  {}",
                "NAME", "LAYERS", "IN", "OUT", "PARAMS", "NODES"
            );
            for m in &items {
                println!(
                    "{:<20} {:>6} {:>8} {:>8} {:>9.1}M  {}",
                    m.name,
                    m.layers,
                    m.input_dim,
                    m.output_dim,
                    m.total_params as f64 / 1_000_000.0,
                    m.nodes.join(",")
                );
            }
            Ok(())
        }
        Response::Error { message } => bail!(message),
        other => bail!("unexpected response to model list: {other:?}"),
    }
}

#[cfg(feature = "llm")]
async fn run_model_run(
    endpoint: &DaemonEndpoint,
    name: &str,
    batch: usize,
    seed: u64,
    show_output: bool,
) -> Result<()> {
    ensure!(batch > 0, "--batch must be greater than zero");
    // Ask the daemon for the model's input dim by listing; generate input.
    let models = match endpoint.request(Request::ListModels).await? {
        Response::Models { items } => items,
        other => bail!("unexpected response to model list: {other:?}"),
    };
    let model = models
        .iter()
        .find(|m| m.name == name)
        .with_context(|| format!("model `{name}` is not loaded"))?;
    let input = crate::matrix::deterministic_matrix(batch, model.input_dim, seed);

    println!("{}", title("Run Model"));
    println!(
        "{} {} {} input {}x{}",
        label("model"),
        name,
        muted("—"),
        batch,
        model.input_dim
    );
    println!();

    let response = endpoint
        .request(Request::RunModel {
            name: name.to_string(),
            input,
        })
        .await?;

    match response {
        Response::Pipeline {
            output,
            layers,
            wall_ms,
        } => {
            for l in &layers {
                let relu = if l.relu { " +relu" } else { "" };
                println!(
                    "{} {} {}x{}{} {} {}",
                    label(&format!("layer {}", l.index)),
                    muted("—"),
                    l.in_dim,
                    l.out_dim,
                    relu,
                    muted("took"),
                    ok(&format!("{}ms", l.elapsed_ms))
                );
                for s in &l.shards {
                    println!(
                        "    {:<16} cols {:>6}..{:<6} ({:>5})  {}",
                        s.node_name,
                        s.col_start,
                        s.col_start + s.cols,
                        s.cols,
                        muted(&format!("{}ms", s.elapsed_ms))
                    );
                }
            }
            println!();
            let checksum: f64 = output.iter().flat_map(|r| r.iter()).sum();
            println!(
                "{} {}x{}",
                label("output"),
                output.len(),
                output.first().map(|r| r.len()).unwrap_or(0)
            );
            println!("{} {:.6}", label("checksum (Σ)"), checksum);
            println!("{} {}", label("wall"), ok(&format!("{wall_ms}ms")));
            if show_output {
                println!();
                for row in &output {
                    let cells: Vec<String> = row.iter().map(|v| format!("{v:8.3}")).collect();
                    println!("  [{}]", cells.join(", "));
                }
            }
            Ok(())
        }
        Response::Error { message } => bail!(message),
        other => bail!("unexpected response to model run: {other:?}"),
    }
}

#[cfg(feature = "llm")]
async fn run_llm_distributed(
    model_dir: &Path,
    peers: &[String],
    prompt: &str,
    max_new: usize,
    save_manifest: Option<&Path>,
) -> Result<()> {
    use std::time::Instant;

    ensure!(!peers.is_empty(), "need at least one --peers IP");
    println!("{}", title("Distributed LLM"));
    println!("{} {}", label("model"), model_dir.display());
    println!("{} {}", label("peers"), peers.join(", "));
    println!("{} {}", label("prompt"), prompt);

    let cfg = crate::llm::LlmConfig::load(&model_dir.join("config.json"))?;
    let tokenizer = tokenizers::Tokenizer::from_file(model_dir.join("tokenizer.json"))
        .map_err(|e| anyhow!("failed to load tokenizer: {e}"))?;

    println!("\n{} distributing weights to peers...", muted("·"));
    let t0 = Instant::now();
    let model = crate::coordinator::distribute_qwen(
        &model_dir.join("model.safetensors"),
        &cfg,
        peers,
        crate::matrix::BinDtype::F16,
    )
    .await?;
    println!(
        "{} model distributed in {}",
        ok("ok"),
        muted(&format!("{}ms", t0.elapsed().as_millis()))
    );

    // Optionally persist the manifest so later `llm preloaded` runs skip the
    // cold distribution entirely.
    if let Some(path) = save_manifest {
        let manifest = model.clone().into_manifest();
        let json = serde_json::to_vec(&manifest).context("failed to serialize manifest")?;
        std::fs::write(path, json)
            .with_context(|| format!("failed to write manifest {}", path.display()))?;
        println!("{} manifest saved to {}", ok("ok"), path.display());
    }

    generate_distributed(&model, &tokenizer, prompt, max_new).await
}

#[cfg(feature = "llm")]
async fn run_llm_preloaded(
    model_dir: &Path,
    manifest_path: &Path,
    prompt: &str,
    max_new: usize,
) -> Result<()> {
    println!("{}", title("Preloaded Distributed LLM"));
    println!("{} {}", label("manifest"), manifest_path.display());
    println!("{} {}", label("prompt"), prompt);

    let tokenizer = tokenizers::Tokenizer::from_file(model_dir.join("tokenizer.json"))
        .map_err(|e| anyhow!("failed to load tokenizer: {e}"))?;
    let raw = std::fs::read(manifest_path)
        .with_context(|| format!("failed to read manifest {}", manifest_path.display()))?;
    let manifest: crate::coordinator::DistManifest =
        serde_json::from_slice(&raw).context("failed to parse manifest")?;
    // Re-attach the embedding from the model file (mmap, one tensor) — no cold
    // distribution; the peers already hold the sharded weights in RAM.
    let model = crate::coordinator::DistModel::from_manifest(
        manifest,
        &model_dir.join("model.safetensors"),
    )?;

    generate_distributed(&model, &tokenizer, prompt, max_new).await
}

#[cfg(feature = "llm")]
/// Greedy autoregressive generation with a KV cache, streaming tokens. The
/// prompt is prefilled token-by-token into the caches, then each new token is a
/// single decode step (one row through the sharded matmuls, attending over the
/// cache) — so per-token cost no longer grows with the prefix length.
async fn generate_distributed(
    model: &crate::coordinator::DistModel,
    tokenizer: &tokenizers::Tokenizer,
    prompt: &str,
    max_new: usize,
) -> Result<()> {
    use std::io::Write;
    use std::time::Instant;

    let enc = tokenizer
        .encode(prompt, false)
        .map_err(|e| anyhow!("tokenize failed: {e}"))?;
    let prompt_ids: Vec<u32> = enc.get_ids().to_vec();
    ensure!(!prompt_ids.is_empty(), "empty prompt");

    print!("\n{prompt}");
    std::io::stdout().flush().ok();

    let mut caches = model.new_caches();
    let mut pos = 0usize;
    let t0 = Instant::now();

    // Prefill: feed every prompt token, keeping the last logits to sample from.
    let mut logits = Vec::new();
    for &tok in &prompt_ids {
        logits = model.decode_step(tok, pos, &mut caches).await?;
        pos += 1;
    }
    let prefill_ms = t0.elapsed().as_millis();

    // Decode: sample, feed back, repeat.
    let t1 = Instant::now();
    for _ in 0..max_new {
        let next = crate::coordinator::argmax(&logits) as u32;
        if let Ok(piece) = tokenizer.decode(&[next], false) {
            print!("{piece}");
            std::io::stdout().flush().ok();
        }
        logits = model.decode_step(next, pos, &mut caches).await?;
        pos += 1;
    }
    let decode_ms = t1.elapsed().as_millis();
    println!();
    println!(
        "{} prefill {} tok {}ms, decode {} tok {}ms ({}ms/tok)",
        muted("·"),
        prompt_ids.len(),
        prefill_ms,
        max_new,
        decode_ms,
        if max_new > 0 {
            decode_ms / max_new as u128
        } else {
            0
        }
    );
    Ok(())
}

#[cfg(feature = "llm")]
fn run_llm_generate(
    model_dir: &Path,
    prompt: &str,
    max_new: usize,
    mmap: bool,
    gguf: Option<&Path>,
    kv_cache_dirs: &[PathBuf],
    kv_max_tokens: usize,
    kv_buffer_mb: usize,
    keep_kv_cache: bool,
) -> Result<()> {
    use std::io::Write;
    ensure!(
        kv_cache_dirs.is_empty() || gguf.is_some(),
        "--kv-cache-dir currently requires --gguf"
    );
    ensure!(
        !keep_kv_cache || !kv_cache_dirs.is_empty(),
        "--keep-kv-cache requires --kv-cache-dir"
    );
    ensure!(kv_max_tokens > 0, "--kv-max-tokens must be positive");
    ensure!(kv_buffer_mb > 0, "--kv-buffer-mb must be positive");
    println!("{}", title("LLM Generate"));
    println!("{} {}", label("model"), model_dir.display());
    let backend = if gguf.is_some() && !kv_cache_dirs.is_empty() {
        "gguf-mmap+kv-disk"
    } else if gguf.is_some() {
        "gguf-mmap"
    } else if mmap {
        "mmap"
    } else {
        "resident"
    };
    println!("{} {backend}", label("backend"));
    println!("{} {}", label("prompt"), prompt);
    print!("\n{prompt}");
    std::io::stdout().flush().ok();

    if let Some(gguf_path) = gguf {
        let runner = crate::llm::GgufLlmRunner::load(model_dir, gguf_path)?;
        if kv_cache_dirs.is_empty() {
            runner.generate(prompt, max_new, |piece| {
                print!("{piece}");
                std::io::stdout().flush().ok();
            })?;
        } else {
            let cfg = runner.weights().config();
            let buffer_bytes = kv_buffer_mb
                .checked_mul(1024 * 1024)
                .context("KV buffer size overflow")?;
            let result = runner.generate_with_disk_cache(
                prompt,
                max_new,
                crate::kv_cache::DiskKvConfig {
                    directories: kv_cache_dirs.to_vec(),
                    num_layers: cfg.num_hidden_layers,
                    num_kv_heads: cfg.num_key_value_heads,
                    head_dim: cfg.head_dim(),
                    max_tokens: kv_max_tokens,
                    buffer_bytes,
                    keep_files: keep_kv_cache,
                },
                |piece| {
                    print!("{piece}");
                    std::io::stdout().flush().ok();
                },
            )?;
            println!();
            println!(
                "{} prefill {:.3}s, decode {:.3}s, KV {} tok / {:.2} MiB used, \
                 {:.2} MiB mapped now / {:.2} MiB max across {} file(s), {} dir(s), \
                 {} MiB buffer",
                muted("·"),
                result.prefill.as_secs_f64(),
                result.decode.as_secs_f64(),
                result.cache.tokens,
                result.cache.used_bytes as f64 / (1024.0 * 1024.0),
                result.cache.mapped_bytes as f64 / (1024.0 * 1024.0),
                result.cache.logical_capacity_bytes as f64 / (1024.0 * 1024.0),
                result.cache.files,
                result.cache.directories,
                kv_buffer_mb,
            );
            if keep_kv_cache {
                for path in result.cache.paths {
                    println!("{} {}", muted("· KV"), path.display());
                }
            }
            return Ok(());
        }
    } else if mmap {
        let runner = crate::llm::MappedLlmRunner::load(model_dir)?;
        runner.generate(prompt, max_new, |piece| {
            print!("{piece}");
            std::io::stdout().flush().ok();
        })?;
    } else {
        let runner = crate::llm::LlmRunner::load(model_dir)?;
        runner.generate(prompt, max_new, |piece| {
            print!("{piece}");
            std::io::stdout().flush().ok();
        })?;
    }
    println!();
    Ok(())
}

#[cfg(feature = "llm")]
fn run_model_inspect(file: &Path) -> Result<()> {
    let infos = crate::model::inspect(file)?;
    println!("{}", title("Model Tensors"));
    println!(
        "{} {} {} {} tensors",
        label("file"),
        file.display(),
        muted("—"),
        infos.len()
    );
    println!();
    let mut total_params: u128 = 0;
    for info in &infos {
        let params: u128 = info.shape.iter().map(|&d| d as u128).product();
        total_params += params;
        let shape = info
            .shape
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join("x");
        println!("  {:<40} {:<6} {}", info.name, info.dtype, muted(&shape));
    }
    println!();
    println!(
        "{} {} ({:.1}M)",
        label("total params"),
        total_params,
        total_params as f64 / 1_000_000.0
    );
    Ok(())
}

#[derive(serde::Deserialize)]
struct TensorInput {
    left: Vec<Vec<f64>>,
    right: Vec<Vec<f64>>,
}

async fn run_tensor_multiply(
    endpoint: &DaemonEndpoint,
    file: Option<&Path>,
    size: Option<usize>,
    seed: u64,
    show_result: bool,
    use_cache: bool,
) -> Result<()> {
    let (left, right) = match (file, size) {
        (Some(path), _) => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read tensor file {}", path.display()))?;
            let input: TensorInput = serde_yaml::from_str(&raw)
                .with_context(|| format!("failed to parse tensor file {}", path.display()))?;
            (input.left, input.right)
        }
        (None, Some(n)) => {
            ensure!(n > 0, "--size must be greater than zero");
            // Square A·B so both bands and the product are NxN — easy to reason
            // about while still scaling work with N^3.
            let left = crate::matrix::deterministic_matrix(n, n, seed);
            let right = crate::matrix::deterministic_matrix(n, n, seed ^ 0xABCD);
            (left, right)
        }
        (None, None) => bail!("provide either -f <file> or --size <N>"),
    };

    let rows = left.len();
    let cols = right.first().map(|r| r.len()).unwrap_or(0);
    let shared = right.len();

    println!("{}", title("Distributed Tensor Multiply"));
    println!(
        "{} {}x{} {} {}x{} {} {}x{}",
        label("shapes"),
        rows,
        shared,
        muted("·"),
        shared,
        cols,
        muted("="),
        rows,
        cols
    );
    println!();

    println!(
        "{} {}",
        label("weight cache"),
        if use_cache {
            ok("on")
        } else {
            muted("off (re-send right each call)")
        }
    );
    println!();

    let response = endpoint
        .request(Request::DistributedMatmul {
            left,
            right,
            use_cache,
        })
        .await?;

    match response {
        Response::Matmul {
            result,
            placements,
            wall_ms,
        } => {
            println!(
                "{} {} {} {} {}",
                label("placement"),
                muted("— row bands fanned across"),
                ok(&placements.len().to_string()),
                muted("node(s), wall"),
                ok(&format!("{wall_ms}ms"))
            );
            for p in &placements {
                let cache_tag = match p.weight_cached {
                    Some(true) => ok("[cache hit]"),
                    Some(false) => muted("[uploaded]"),
                    None => String::new(),
                };
                println!(
                    "  {:<16} {:<6} rows {:>5}..{:<5} ({:>4})  {:>8}  {}",
                    p.node_name,
                    color_status(if p.transport == "local" {
                        "Schedulable"
                    } else {
                        "Ready"
                    }),
                    p.start_row,
                    p.start_row + p.rows,
                    p.rows,
                    muted(&format!("{}ms", p.elapsed_ms)),
                    cache_tag
                );
            }
            println!();
            // Checksum lets you confirm every node agreed on the math without
            // dumping a huge matrix; --show-result prints the full product.
            let checksum: f64 = result.iter().flat_map(|r| r.iter()).sum();
            println!("{} {}", label("result rows"), result.len());
            println!("{} {:.6}", label("checksum (Σ)"), checksum);
            if show_result {
                println!();
                for row in &result {
                    let cells: Vec<String> = row.iter().map(|v| format!("{v:8.3}")).collect();
                    println!("  [{}]", cells.join(", "));
                }
            }
            Ok(())
        }
        Response::Error { message } => bail!(message),
        other => bail!("unexpected response to tensor multiply: {other:?}"),
    }
}

#[derive(serde::Deserialize)]
struct PipelineInput {
    input: Vec<Vec<f64>>,
    layers: Vec<protocol::LayerSpec>,
}

async fn run_pipeline(
    endpoint: &DaemonEndpoint,
    file: Option<&Path>,
    layers: Option<usize>,
    dim: usize,
    batch: usize,
    seed: u64,
    show_output: bool,
) -> Result<()> {
    let (input, specs) = match (file, layers) {
        (Some(path), _) => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read model file {}", path.display()))?;
            let parsed: PipelineInput = serde_yaml::from_str(&raw)
                .with_context(|| format!("failed to parse model file {}", path.display()))?;
            (parsed.input, parsed.layers)
        }
        (None, Some(n)) => {
            ensure!(n > 0, "--layers must be greater than zero");
            ensure!(batch > 0, "--batch must be greater than zero");
            // A square stack: batch x dim input through n layers of dim x dim,
            // each with a distinct seed and ReLU between them.
            let input = crate::matrix::deterministic_matrix(batch, dim, seed);
            let specs = (0..n)
                .map(|i| protocol::LayerSpec {
                    in_dim: dim,
                    out_dim: dim,
                    seed: seed.wrapping_add(i as u64 + 1),
                    relu: true,
                })
                .collect();
            (input, specs)
        }
        (None, None) => bail!("provide either -f <file> or --layers <N>"),
    };

    let total_weights: usize = specs.iter().map(|l| l.in_dim * l.out_dim).sum();
    println!("{}", title("Distributed Layer Pipeline"));
    println!(
        "{} {} layers, input {}x{}, weights ~{:.1} MB (f64)",
        label("model"),
        specs.len(),
        input.len(),
        input.first().map(|r| r.len()).unwrap_or(0),
        (total_weights * 8) as f64 / 1_048_576.0
    );
    println!();

    let response = endpoint
        .request(Request::DistributedPipeline {
            input,
            layers: specs,
        })
        .await?;

    match response {
        Response::Pipeline {
            output,
            layers,
            wall_ms,
        } => {
            for l in &layers {
                let relu = if l.relu { " +relu" } else { "" };
                println!(
                    "{} {} {}x{}{} {} {}",
                    label(&format!("layer {}", l.index)),
                    muted("—"),
                    l.in_dim,
                    l.out_dim,
                    relu,
                    muted("took"),
                    ok(&format!("{}ms", l.elapsed_ms))
                );
                for s in &l.shards {
                    let cache_tag = if s.weight_cached {
                        ok("[cache hit]")
                    } else {
                        muted("[uploaded]")
                    };
                    println!(
                        "    {:<16} {:>2}c  cols {:>5}..{:<5} ({:>4})  {:>8}  {}",
                        s.node_name,
                        s.cpu_cores,
                        s.col_start,
                        s.col_start + s.cols,
                        s.cols,
                        muted(&format!("{}ms", s.elapsed_ms)),
                        cache_tag
                    );
                }
            }
            println!();
            let checksum: f64 = output.iter().flat_map(|r| r.iter()).sum();
            println!(
                "{} {}x{}",
                label("output"),
                output.len(),
                output.first().map(|r| r.len()).unwrap_or(0)
            );
            println!("{} {:.6}", label("checksum (Σ)"), checksum);
            println!("{} {}", label("wall"), ok(&format!("{wall_ms}ms")));
            if show_output {
                println!();
                for row in &output {
                    let cells: Vec<String> = row.iter().map(|v| format!("{v:8.3}")).collect();
                    println!("  [{}]", cells.join(", "));
                }
            }
            Ok(())
        }
        Response::Error { message } => bail!(message),
        other => bail!("unexpected response to pipeline run: {other:?}"),
    }
}

fn run_vpn_init(cidr: &str, public_endpoint: &str) -> Result<()> {
    use crate::vpn;

    let (cidr_base, cidr_prefix) = parse_cidr(cidr)?;
    let (network, _keypair, token) =
        vpn::init_hub(cidr_base, cidr_prefix, public_endpoint.to_string())
            .context("failed to initialize hub")?;
    let encoded = token.encode().context("failed to encode join token")?;

    let state_path = config::default_vpn_state_file()?;
    let key_path = config::default_vpn_private_key_file()?;

    println!("Hub initialized (state persisted).");
    println!("  hub pubkey   : {}", network.hub_pubkey.as_str());
    println!("  hub IP       : {}", network.hub_ip());
    println!("  hub endpoint : {}", network.hub_endpoint);
    println!(
        "  CIDR         : {}/{}",
        network.cidr_base, network.cidr_prefix
    );
    println!("  state file   : {}", state_path.display());
    println!("  private key  : {} (chmod 0600)", key_path.display());
    println!();
    println!("Share this join token with spokes:");
    println!("  {encoded}");
    Ok(())
}

fn run_vpn_status() -> Result<()> {
    use crate::vpn::{HubState, SpokeState};

    let hub = HubState::load()?;
    let spoke = SpokeState::load()?;
    let mut shown = false;

    if let Some(network) = hub.network.as_ref() {
        shown = true;
        println!("Hub:");
        println!("  endpoint : {}", network.hub_endpoint);
        println!("  hub pub  : {}", network.hub_pubkey.as_str());
        println!("  CIDR     : {}/{}", network.cidr_base, network.cidr_prefix);
        println!();
        println!("Peers ({}):", hub.peers.len());
        println!(
            "  {:<14} {:<8} {:<14} {:<6} {}",
            "NAME", "ROLE", "VPN-IP", "STATUS", "PUBKEY"
        );
        for peer in &hub.peers {
            println!(
                "  {:<14} {:<8} {:<14} {:<6} {}",
                peer.node_name,
                format!("{:?}", peer.membership.role),
                peer.membership.ip,
                format!("{:?}", peer.membership.status),
                truncate(peer.membership.pubkey.as_str(), 22)
            );
        }
        println!();
    }

    if let Some(spoke) = spoke {
        shown = true;
        println!("Spoke (local):");
        println!("  hub      : {}", spoke.network.hub_endpoint);
        println!("  vpn IP   : {}", spoke.membership.ip);
        println!("  pubkey   : {}", spoke.membership.pubkey.as_str());
        println!("  status   : {:?}", spoke.membership.status);
    }

    if !shown {
        println!("No VPN state on this host. Run `litek8s vpn init` or `litek8s vpn join`.");
    }
    Ok(())
}

fn run_vpn_add_peer(name: &str, pubkey: &str) -> Result<()> {
    use crate::vpn::{self, PublicKey};

    let pubkey = PublicKey::parse(pubkey).context("invalid spoke public key")?;
    let membership = vpn::register_spoke(name, pubkey).context("failed to register spoke")?;
    println!("Registered `{name}` at {} (role Spoke).", membership.ip);
    println!("Tell the spoke to run:");
    println!("  litek8s vpn complete --ip {}", membership.ip);
    Ok(())
}

fn run_vpn_remove_peer(name: &str) -> Result<()> {
    use crate::vpn;

    let removed = vpn::remove_spoke(name)?;
    if removed {
        println!("Removed peer `{name}`.");
    } else {
        println!("No peer named `{name}` was registered.");
    }
    Ok(())
}

fn run_vpn_show_config(role: &str, listen_port: u16, output: Option<&Path>) -> Result<()> {
    use crate::vpn::{
        HubState, SpokeState, load_or_generate_keypair, write_wg_quick_config,
        write_wg_quick_hub_config,
    };

    let keypair = load_or_generate_keypair()?;
    match role {
        "hub" => {
            let hub = HubState::load()?;
            let target = output
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("/tmp/litek8s-hub.conf"));
            let written =
                write_wg_quick_hub_config(&target, &hub, &keypair.secret_base64(), listen_port)?;
            println!("Wrote hub wg-quick config to {}", written.display());
            println!("Bring it up with: sudo wg-quick up {}", written.display());
        }
        "spoke" => {
            let spoke = SpokeState::load()?
                .context("no spoke state on disk — run `litek8s vpn join --token ...` first")?;
            let target = output
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("/tmp/litek8s-spoke.conf"));
            let written = write_wg_quick_config(&target, &spoke, &keypair.secret_base64())?;
            println!("Wrote spoke wg-quick config to {}", written.display());
            println!("Bring it up with: sudo wg-quick up {}", written.display());
        }
        other => bail!("unknown role `{other}` (use `hub` or `spoke`)"),
    }
    Ok(())
}

async fn run_vpn_join(
    token: &str,
    ssh_hub: Option<&str>,
    node_name: &str,
    remote_binary: &str,
) -> Result<()> {
    use crate::node::{AgentRequest, AgentResponse, SshNodeClient};
    use crate::vpn::{self, JoinToken};

    let token = JoinToken::decode(token).context("failed to decode join token")?;
    let (state, _keypair) = vpn::prepare_spoke(&token).context("failed to prepare spoke")?;

    let Some(ssh_target) = ssh_hub else {
        println!("Spoke prepared (manual mode — no --ssh-hub supplied).");
        println!("  hub endpoint : {}", state.network.hub_endpoint);
        println!("  hub pubkey   : {}", state.network.hub_pubkey.as_str());
        println!("  spoke pubkey : {}", state.membership.pubkey.as_str());
        println!();
        println!("Hand the spoke pubkey to the hub operator. On the hub, run:");
        println!(
            "  litek8s vpn add-peer --name <name> --pubkey {}",
            state.membership.pubkey.as_str()
        );
        println!("The hub will reply with an assigned IP; finish here with:");
        println!("  litek8s vpn complete --ip <assigned-ip>");
        return Ok(());
    };

    let resolved_name = if node_name.is_empty() {
        hostname_fallback()
    } else {
        node_name.to_string()
    };

    println!("Contacting hub via SSH ({ssh_target}) as `{resolved_name}`…");
    let ssh = SshNodeClient::new(ssh_target.to_string(), remote_binary.to_string(), None);
    let response = ssh
        .send_agent_request(AgentRequest::VpnAttach {
            node_name: resolved_name.clone(),
            spoke_pubkey: state.membership.pubkey.clone(),
        })
        .await
        .context("SSH bootstrap failed")?;

    let (assigned, _hub_token) = match response {
        AgentResponse::VpnAssignment { membership, token } => (membership, token),
        other => bail!("unexpected agent response from hub: {other:?}"),
    };

    let completed = vpn::complete_spoke(assigned).context("failed to persist assignment")?;
    println!("Spoke registered.");
    println!("  vpn IP   : {}", completed.membership.ip);
    println!("  hub      : {}", completed.network.hub_endpoint);
    println!();
    println!("Write the wg-quick config and bring it up:");
    println!("  litek8s vpn show-config --role spoke --output /tmp/wg0.conf");
    println!("  sudo wg-quick up /tmp/wg0.conf");
    Ok(())
}

async fn run_cluster_apply(endpoint: &DaemonEndpoint, file: &Path) -> Result<()> {
    use crate::cluster_manifest::ClusterManifest;

    let manifest = ClusterManifest::parse_file(file)
        .with_context(|| format!("failed to parse {}", file.display()))?;
    println!(
        "{}",
        title(&format!("Cluster Apply ({})", manifest.metadata.name))
    );

    let report = crate::cluster_orchestrator::apply(&manifest, endpoint).await?;
    if let Some(message) = report.vpn_initialized.as_deref() {
        println!("  {} {}", ok("vpn"), message);
    }
    for name in &report.nodes_added {
        println!("  {} node/{}", ok("added"), name);
    }
    for name in &report.nodes_unchanged {
        println!("  {} node/{}", muted("kept"), name);
    }
    for path in &report.workloads_applied {
        println!("  {} workload {}", ok("applied"), path.display());
    }
    if report.nodes_added.is_empty()
        && report.nodes_unchanged.is_empty()
        && report.workloads_applied.is_empty()
        && report.vpn_initialized.is_none()
    {
        println!("  {}", muted("nothing to do"));
    }
    Ok(())
}

async fn run_cluster_diff(endpoint: &DaemonEndpoint, file: &Path) -> Result<()> {
    use crate::cluster_manifest::ClusterManifest;

    let manifest = ClusterManifest::parse_file(file)
        .with_context(|| format!("failed to parse {}", file.display()))?;
    println!(
        "{}",
        title(&format!("Cluster Diff ({})", manifest.metadata.name))
    );

    let report = crate::cluster_orchestrator::diff(&manifest, endpoint).await?;
    if let Some(action) = report.vpn_to_initialize.as_deref() {
        println!("  {} vpn → {}", ok("init"), action);
    } else if report.vpn_already_initialized {
        println!("  {} vpn (already initialized)", muted("="));
    }
    for name in &report.nodes_to_add {
        println!("  {} node/{}", ok("+"), name);
    }
    for name in &report.nodes_already_present {
        println!("  {} node/{}", muted("="), name);
    }
    for name in &report.nodes_extra {
        println!("  {} node/{} (extra, not in manifest)", muted("?"), name);
    }
    for path in &report.workloads_to_apply {
        println!("  {} workload {}", ok("apply"), path.display());
    }
    Ok(())
}

async fn run_cluster_destroy(endpoint: &DaemonEndpoint, file: &Path) -> Result<()> {
    use crate::cluster_manifest::ClusterManifest;

    let manifest = ClusterManifest::parse_file(file)
        .with_context(|| format!("failed to parse {}", file.display()))?;
    println!(
        "{}",
        title(&format!("Cluster Destroy ({})", manifest.metadata.name))
    );

    let report = crate::cluster_orchestrator::destroy(&manifest, endpoint).await?;
    for path in &report.workloads_deleted {
        println!("  {} workload {}", ok("deleted"), path.display());
    }
    for name in &report.nodes_drained {
        println!("  {} node/{}", ok("drained"), name);
    }
    Ok(())
}

async fn run_cluster_export(endpoint: &DaemonEndpoint, name: &str) -> Result<()> {
    let manifest = crate::cluster_orchestrator::export(name, endpoint).await?;
    let yaml = serde_yaml::to_string(&manifest).context("failed to serialize cluster manifest")?;
    print!("{yaml}");
    Ok(())
}

fn hostname_fallback() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|output| {
                    String::from_utf8(output.stdout)
                        .ok()
                        .map(|s| s.trim().to_string())
                })
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "spoke".to_string())
}

fn run_vpn_complete(ip: &str) -> Result<()> {
    use crate::vpn::{self, SpokeState, VpnMembership};

    let ip: std::net::Ipv4Addr = ip
        .parse()
        .with_context(|| format!("invalid IPv4 address `{ip}`"))?;
    let pending = SpokeState::load()?
        .context("no pending spoke state — run `litek8s vpn join --token ...` first")?;

    let mut assigned = VpnMembership::new_spoke(ip, pending.membership.pubkey.clone());
    assigned.endpoint = pending.membership.endpoint.clone();
    let state = vpn::complete_spoke(assigned).context("failed to complete spoke")?;

    println!("Spoke completed.");
    println!("  vpn IP     : {}", state.membership.ip);
    println!("  hub        : {}", state.network.hub_endpoint);
    println!();
    println!("Generate a wg-quick config with:");
    println!("  litek8s vpn show-config --role spoke --output /tmp/wg0.conf");
    println!("Then bring it up with: sudo wg-quick up /tmp/wg0.conf");
    Ok(())
}

fn parse_cidr(value: &str) -> Result<(std::net::Ipv4Addr, u8)> {
    let (base, prefix) = value
        .split_once('/')
        .with_context(|| format!("CIDR `{value}` must be in `A.B.C.D/N` form"))?;
    let base: std::net::Ipv4Addr = base
        .parse()
        .with_context(|| format!("CIDR `{value}` has invalid IPv4 base"))?;
    let prefix: u8 = prefix
        .parse()
        .with_context(|| format!("CIDR `{value}` has invalid prefix length"))?;
    Ok((base, prefix))
}

fn read_manifest(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))
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

fn parse_labels(values: &[String]) -> Result<BTreeMap<String, String>> {
    let mut labels = BTreeMap::new();
    for value in values {
        let (key, label_value) = value
            .split_once('=')
            .with_context(|| format!("label `{value}` must use KEY=VALUE syntax"))?;
        validate_label_key(key)?;
        validate_label_value(label_value)?;
        labels.insert(key.to_string(), label_value.to_string());
    }
    Ok(labels)
}

fn ensure_resource(resource: &str, supported: &[&str]) -> Result<()> {
    if supported.iter().any(|candidate| candidate == &resource) {
        Ok(())
    } else {
        bail!(
            "unsupported resource `{resource}`; expected one of {}",
            supported.join(", ")
        )
    }
}

fn print_response(response: Response) -> Result<()> {
    match response {
        Response::Ack { message } => {
            println!("{} {}", ok("ok"), message);
            Ok(())
        }
        Response::Deployments { items } => {
            let total = items.len();
            println!(
                "{} {}",
                title("Managed Deployments"),
                muted(&format!("({total} total)"))
            );
            println!();
            println!(
                "{} {} {} {} {} {}",
                header_left("NAME", 24),
                header_left("NODE", 14),
                header_left("STATUS", 12),
                header_left("READY", 7),
                header_left("IMAGE", 24),
                label("CONTAINER")
            );
            for item in items {
                println!(
                    "{} {} {} {} {} {}",
                    left_cell(&item.name, 24),
                    left_cell(&truncate(&item.node_name, 14), 14),
                    status_cell(&item.status, 12),
                    left_cell(
                        &format!("{}/{}", item.ready_replicas, item.desired_replicas),
                        7
                    ),
                    left_cell(&truncate(&item.image, 24), 24),
                    item.container_id.unwrap_or_else(|| "-".to_string())
                );
            }
            Ok(())
        }
        Response::Services { items } => {
            let total = items.len();
            println!(
                "{} {}",
                title("Managed Services"),
                muted(&format!("({total} total)"))
            );
            println!();
            println!(
                "{} {} {} {} {}",
                header_left("NAME", 18),
                header_left("DEPLOYMENT", 24),
                header_left("LISTEN", 22),
                header_left("PORT", 8),
                label("STATUS")
            );
            for item in items {
                println!(
                    "{} {} {} {} {}",
                    left_cell(&item.name, 18),
                    left_cell(&truncate(&item.deployment_name, 24), 24),
                    left_cell(&truncate(&item.listen_addr, 22), 22),
                    left_cell(
                        &item
                            .target_port
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "auto".to_string()),
                        8,
                    ),
                    status_cell(&item.status, 10)
                );
            }
            Ok(())
        }
        Response::Nodes { items } => {
            let total = items.len();
            println!(
                "{} {}",
                title("Cluster Nodes"),
                muted(&format!("({total} total)"))
            );
            println!();
            println!(
                "{} {} {} {} {} {} {} {} {}",
                header_left("NAME", 18),
                header_left("TYPE", 8),
                header_left("STATUS", 10),
                header_left("SCHED", 12),
                header_left("CPU", 12),
                header_left("MEM", 18),
                header_left("DISK", 18),
                header_right("DEPLOY", 6),
                label("ENDPOINT")
            );
            for item in items {
                let (cpu_cell, mem_cell, disk_cell) = match item.host_resources.as_ref() {
                    Some(r) => (
                        format!("{:.0}% ({}c)", r.cpu_usage_percent, r.cpu_cores),
                        format!(
                            "{}/{}",
                            human_bytes(r.memory_used_bytes),
                            human_bytes(r.memory_total_bytes)
                        ),
                        format!(
                            "{}/{}",
                            human_bytes(r.disk_total_bytes.saturating_sub(r.disk_free_bytes)),
                            human_bytes(r.disk_total_bytes)
                        ),
                    ),
                    None => ("-".to_string(), "-".to_string(), "-".to_string()),
                };
                println!(
                    "{} {} {} {} {} {} {} {} {}",
                    left_cell(&item.name, 18),
                    left_cell(&item.transport, 8),
                    status_cell(&item.status, 10),
                    scheduling_cell(&item.scheduling, 12),
                    left_cell(&truncate(&cpu_cell, 12), 12),
                    left_cell(&truncate(&mem_cell, 18), 18),
                    left_cell(&truncate(&disk_cell, 18), 18),
                    right_cell(&item.deployment_count.to_string(), 6),
                    truncate(&item.endpoint, 24)
                );
            }
            Ok(())
        }
        Response::Description { item } => {
            let command = if item.command.is_empty() {
                "-".to_string()
            } else {
                item.command.join(" ")
            };
            let args = if item.args.is_empty() {
                "-".to_string()
            } else {
                item.args.join(" ")
            };
            let ports = if item.ports.is_empty() {
                "-".to_string()
            } else {
                item.ports.join(", ")
            };

            println!("{}", title(&format!("Deployment {}", item.name)));
            println!(
                "{}",
                muted("single-container workload kept alive by the local daemon")
            );
            println!();
            print_detail("apiVersion", &item.api_version);
            print_detail("node", &item.node_name);
            print_detail(
                "requested node",
                item.requested_node_name.as_deref().unwrap_or("-"),
            );
            let node_selector = if item.node_selector.is_empty() {
                "-".to_string()
            } else {
                item.node_selector.join(", ")
            };
            print_detail("node selector", &node_selector);
            print_detail("status", &color_status(&item.status));
            print_detail("ready", if item.ready { "true" } else { "false" });
            print_detail(
                "replicas",
                &format!("{}/{} ready", item.ready_replicas, item.desired_replicas),
            );
            print_detail("image", &item.image);
            print_detail("container", &item.container_name);
            print_detail(
                "container id",
                &item.container_id.unwrap_or_else(|| "-".to_string()),
            );
            // Per-replica breakdown when there's more than one.
            if item.replicas.len() > 1 {
                for replica in &item.replicas {
                    print_detail(
                        &format!("  replica {}", replica.ordinal),
                        &format!(
                            "{} on {} ({}){}",
                            color_status(&replica.status),
                            replica.node_name,
                            if replica.ready { "ready" } else { "not ready" },
                            replica
                                .container_id
                                .as_deref()
                                .map(|id| format!(" {}", &id[..id.len().min(12)]))
                                .unwrap_or_default(),
                        ),
                    );
                }
            }
            if let Some(message) = item.message {
                print_detail("message", &message);
            }
            print_detail("command", &command);
            print_detail("args", &args);
            print_detail("readiness", item.readiness_probe.as_deref().unwrap_or("-"));
            print_detail("cpu limit", item.cpu_limit.as_deref().unwrap_or("-"));
            print_detail("memory limit", item.memory_limit.as_deref().unwrap_or("-"));
            print_detail("restarts", &item.restart_count.to_string());
            print_detail("streak", &item.restart_streak.to_string());
            print_detail("last exit", item.last_exit.as_deref().unwrap_or("-"));
            print_detail(
                "next retry",
                &format_retry_countdown(item.next_retry_in_secs),
            );
            print_detail("ports", &ports);
            if !item.conditions.is_empty() {
                println!("{}", label("conditions:"));
                for condition in item.conditions {
                    let condition_status = if condition.status == "True" {
                        ok("True")
                    } else {
                        muted("False")
                    };
                    println!(
                        "  {} {} {} @{}",
                        left_cell(&condition.type_name, 10),
                        condition_status,
                        muted(&format!("reason={}", condition.reason)),
                        condition.last_transition_secs
                    );
                    if let Some(message) = condition.message {
                        println!("  {}", muted(&message));
                    }
                }
            }
            Ok(())
        }
        Response::ServiceDescription { item } => {
            println!("{}", title(&format!("Service {}", item.name)));
            println!(
                "{}",
                muted("local control-plane listener backed by a deployment")
            );
            println!();
            print_detail("service host", "local-control-plane");
            print_detail("deployment", &item.deployment_name);
            print_detail("listen", &item.listen_addr);
            print_detail(
                "target port",
                &item
                    .target_port
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "auto".to_string()),
            );
            print_detail("status", &color_status(&item.status));
            print_detail(
                "backend node",
                item.current_node_name.as_deref().unwrap_or("-"),
            );
            print_detail(
                "backend upstream",
                item.current_upstream.as_deref().unwrap_or("-"),
            );
            if let Some(message) = item.message {
                print_detail("message", &message);
            }
            Ok(())
        }
        Response::NodeDescription { item } => {
            println!("{}", title(&format!("Node {}", item.name)));
            println!(
                "{}",
                muted("execution target registered in the local control plane")
            );
            println!();
            print_detail("type", &item.transport);
            print_detail("status", &color_status(&item.status));
            print_detail("scheduling", &color_scheduling(&item.scheduling));
            print_detail("endpoint", &item.endpoint);
            print_detail("remote bin", item.remote_binary.as_deref().unwrap_or("-"));
            let labels = if item.labels.is_empty() {
                "-".to_string()
            } else {
                item.labels.join(", ")
            };
            print_detail("labels", &labels);
            print_detail("deployments", &item.deployment_count.to_string());
            if let Some(message) = item.message {
                print_detail("message", &message);
            }
            if let Some(vpn) = item.vpn {
                println!();
                println!("{}", title("VPN"));
                print_detail("role", &vpn.role);
                print_detail("ip", &vpn.ip);
                print_detail("pubkey", &vpn.pubkey);
                print_detail("endpoint", vpn.endpoint.as_deref().unwrap_or("-"));
                print_detail("status", &vpn.status);
                if let Some(ts) = vpn.last_handshake_unix {
                    print_detail("last handshake", &format!("unix {ts}"));
                }
            }
            Ok(())
        }
        Response::Events { items } => {
            let total = items.len();
            println!(
                "{} {}",
                title("Cluster Events"),
                muted(&format!("({total} shown)"))
            );
            println!();
            println!(
                "{} {} {} {} {} {}",
                header_right("ID", 5),
                header_left("LEVEL", 8),
                header_left("REASON", 16),
                header_left("OBJECT", 28),
                header_left("NODE", 14),
                label("MESSAGE")
            );
            for item in items {
                println!(
                    "{} {} {} {} {} {}",
                    right_cell(&item.id.to_string(), 5),
                    status_cell(&item.level, 8),
                    left_cell(&truncate(&item.reason, 16), 16),
                    left_cell(
                        &truncate(&format!("{}/{}", item.object_kind, item.object_name), 28),
                        28,
                    ),
                    left_cell(item.node_name.as_deref().unwrap_or("-"), 14),
                    item.message
                );
            }
            Ok(())
        }
        Response::Logs { content } => {
            print!("{content}");
            Ok(())
        }
        Response::ProxyTarget { item } => {
            println!(
                "{}",
                title(&format!("Proxy target {}", item.deployment_name))
            );
            println!();
            print_detail("node", &item.node_name);
            print_detail("transport", &item.transport);
            print_detail("container port", &item.container_port.to_string());
            if let Some(host_port) = item.host_port {
                print_detail("host port", &host_port.to_string());
            }
            if let Some(ip) = &item.container_ip {
                print_detail("container ip", ip);
            }
            Ok(())
        }
        Response::Top {
            daemon,
            items,
            nodes,
        } => {
            print!("{}", format_top_body(&daemon, &items, &nodes));
            Ok(())
        }
        Response::ClusterStatus {
            ha_enabled, state, ..
        } => {
            if ha_enabled {
                println!("{} HA {}", ok("ok"), state);
            } else {
                println!("{}", muted("standalone mode (HA not enabled)"));
            }
            Ok(())
        }
        Response::Matmul { .. } => {
            bail!("tensor multiply results are rendered by `tensor multiply`, not print_response")
        }
        Response::Pipeline { .. } => {
            bail!("pipeline results are rendered by `pipeline run`, not print_response")
        }
        Response::ModelLoaded { .. } => {
            bail!("model load results are rendered by `model load`, not print_response")
        }
        Response::Models { .. } => {
            bail!("model list results are rendered by `model list`, not print_response")
        }
        Response::Error { message } => Err(anyhow!(message)),
    }
}

async fn run_top(endpoint: &DaemonEndpoint, watch: bool, interval: Duration) -> Result<()> {
    let mut watch_display = watch
        .then(|| WatchDisplay::new(build_watch_banner("top", interval, None, None)))
        .transpose()?;

    loop {
        let Some(response) = request_with_loading(
            endpoint,
            Request::Top,
            watch_display.as_mut(),
            interval,
            "loading snapshot...",
        )
        .await?
        else {
            return Ok(());
        };
        if let Some(display) = watch_display.as_mut() {
            let body = match response {
                Response::Top {
                    daemon,
                    items,
                    nodes,
                } => format_top_body(&daemon, &items, &nodes),
                Response::Error { message } if message.contains("warming up") => {
                    let loading = muted("loading snapshot...");
                    display.render_frame(&loading)?;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                Response::Error { message } => format_error_body(&message),
                other => {
                    print_response(other)?;
                    io::stdout().flush().context("failed to flush top output")?;
                    continue;
                }
            };
            display.render_frame(&body)?;
        } else {
            print_response(response)?;
            io::stdout().flush().context("failed to flush top output")?;
        }

        if !watch {
            return Ok(());
        }

        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

#[derive(Debug, Clone)]
struct TreeSnapshot {
    daemon: protocol::DaemonTopSummary,
    deployments: Vec<protocol::DeploymentTopRow>,
    nodes: Vec<protocol::NodeSummary>,
    services: Vec<protocol::ServiceSummary>,
}

#[derive(Debug, Clone)]
struct TreeEntry {
    text: String,
    children: Vec<TreeEntry>,
}

async fn run_tree(endpoint: &DaemonEndpoint, watch: bool, interval: Duration) -> Result<()> {
    let mut watch_display = watch
        .then(|| WatchDisplay::new(build_watch_banner("tree", interval, None, None)))
        .transpose()?;

    loop {
        let snapshot = match fetch_tree_snapshot(endpoint, watch_display.as_mut(), interval).await {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return Ok(()),
            Err(error) if error.to_string().contains("warming up") => {
                if let Some(display) = watch_display.as_mut() {
                    let loading = muted("loading snapshot...");
                    display.render_frame(&loading)?;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        let body = render_tree(&snapshot);
        if let Some(display) = watch_display.as_mut() {
            display.render_frame(&body)?;
        } else {
            println!("{body}");
            io::stdout()
                .flush()
                .context("failed to flush tree output")?;
        }

        if !watch {
            return Ok(());
        }

        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

async fn fetch_tree_snapshot(
    endpoint: &DaemonEndpoint,
    watch_display: Option<&mut WatchDisplay>,
    interval: Duration,
) -> Result<Option<TreeSnapshot>> {
    let (top_response, nodes_response, services_response) = tokio::try_join!(
        request_with_loading(
            endpoint,
            Request::Top,
            None,
            interval,
            "loading snapshot..."
        ),
        request_with_loading(
            endpoint,
            Request::GetNodes,
            None,
            interval,
            "loading snapshot..."
        ),
        request_with_loading(
            endpoint,
            Request::GetServices,
            watch_display,
            interval,
            "loading snapshot..."
        ),
    )?;

    let (Some(top_response), Some(nodes_response), Some(services_response)) =
        (top_response, nodes_response, services_response)
    else {
        return Ok(None);
    };

    let (daemon, deployments) = match top_response {
        Response::Top { daemon, items, .. } => (daemon, items),
        Response::Error { message } => return Err(anyhow!(message)),
        _ => bail!("unexpected response to Request::Top"),
    };

    let nodes = match nodes_response {
        Response::Nodes { items } => items,
        Response::Error { message } => return Err(anyhow!(message)),
        _ => bail!("unexpected response to Request::GetNodes"),
    };

    let services = match services_response {
        Response::Services { items } => items,
        Response::Error { message } => return Err(anyhow!(message)),
        _ => bail!("unexpected response to Request::GetServices"),
    };

    Ok(Some(TreeSnapshot {
        daemon,
        deployments,
        nodes,
        services,
    }))
}

fn render_tree(snapshot: &TreeSnapshot) -> String {
    let mut lines = vec![
        title("Cluster Map"),
        muted("real relationships currently managed by the control plane"),
        String::new(),
    ];

    let summary = format!(
        "[nodes={} deployments={} services={} cpu={} mem={} uptime={}]",
        snapshot.nodes.len(),
        snapshot.deployments.len(),
        snapshot.services.len(),
        format_cpu_compact(snapshot.daemon.cpu_percent),
        human_bytes(snapshot.daemon.memory_bytes),
        format_uptime(snapshot.daemon.uptime_secs),
    );
    lines.push(format!(
        "{} {}",
        label("litek8s/control-plane"),
        muted(&summary)
    ));

    let entries = build_tree_entries(snapshot);
    if entries.is_empty() {
        lines.push("└─ no managed resources yet".to_string());
    } else {
        append_tree_entries(&mut lines, "", &entries);
    }

    lines.join("\n")
}

/// Alignment for a top-table column.
enum TopAlign {
    Left,
    Right,
}

/// One column of `litek8s top`, with the data needed to lay it out responsively.
struct TopColumn {
    header: &'static str,
    width: usize,
    align: TopAlign,
    /// Lower runs first when the terminal is too narrow to fit every column.
    /// Essential columns (NAME/STATUS/CPU/MEMORY) use 0 and are never dropped.
    priority: u8,
    /// Renders the already-truncated, already-padded cell for a row. Status uses
    /// its own colorizer, so the cell renderer owns padding + color per column.
    cell: fn(&protocol::DeploymentTopRow, usize) -> String,
}

/// Detect the usable terminal width, falling back to a wide default when output
/// is not a tty (pipes, files) so redirected output keeps every column.
fn terminal_width() -> usize {
    match terminal_size::terminal_size() {
        Some((terminal_size::Width(cols), _)) => cols as usize,
        None => 200,
    }
}

fn format_top_body(
    daemon: &protocol::DaemonTopSummary,
    items: &[protocol::DeploymentTopRow],
    nodes: &[protocol::NodeSummary],
) -> String {
    format_top_body_for_width(daemon, items, nodes, terminal_width())
}

fn format_top_body_for_width(
    daemon: &protocol::DaemonTopSummary,
    items: &[protocol::DeploymentTopRow],
    nodes: &[protocol::NodeSummary],
    available_width: usize,
) -> String {
    // Columns in display order. priority 0 = essential (never dropped); higher
    // numbers are shed first as the terminal narrows.
    let columns: &[TopColumn] = &[
        TopColumn {
            header: "NAME",
            width: 24,
            align: TopAlign::Left,
            priority: 0,
            cell: |item, w| left_cell(&truncate(&item.name, w), w),
        },
        TopColumn {
            header: "NODE",
            width: 14,
            align: TopAlign::Left,
            priority: 3,
            cell: |item, w| left_cell(&truncate(&item.node_name, w), w),
        },
        TopColumn {
            header: "STATUS",
            width: 12,
            align: TopAlign::Left,
            priority: 0,
            cell: |item, w| status_cell(&item.status, w),
        },
        TopColumn {
            header: "RESTARTS",
            width: 8,
            align: TopAlign::Right,
            priority: 4,
            cell: |item, w| right_cell(&item.restart_count.to_string(), w),
        },
        TopColumn {
            header: "STREAK",
            width: 6,
            align: TopAlign::Right,
            priority: 6,
            cell: |item, w| right_cell(&item.restart_streak.to_string(), w),
        },
        TopColumn {
            header: "NEXT RETRY",
            width: 10,
            align: TopAlign::Left,
            priority: 5,
            cell: |item, w| left_cell(&format_retry_countdown(item.next_retry_in_secs), w),
        },
        TopColumn {
            header: "LAST EXIT",
            width: 16,
            align: TopAlign::Left,
            priority: 7,
            cell: |item, w| left_cell(&truncate(item.last_exit.as_deref().unwrap_or("-"), w), w),
        },
        TopColumn {
            header: "CPU",
            width: 8,
            align: TopAlign::Right,
            priority: 0,
            cell: |item, w| {
                right_cell(
                    &format_top_cpu(
                        item.cpu_percent,
                        item.memory_usage_bytes.is_some()
                            || item.network_rx_bytes.is_some()
                            || item.network_tx_bytes.is_some(),
                    ),
                    w,
                )
            },
        },
        TopColumn {
            header: "MEMORY",
            width: 14,
            align: TopAlign::Right,
            priority: 0,
            cell: |item, w| {
                right_cell(
                    &format_memory(item.memory_usage_bytes, item.memory_limit_bytes),
                    w,
                )
            },
        },
        TopColumn {
            header: "NET I/O",
            width: 18,
            align: TopAlign::Right,
            priority: 2,
            cell: |item, w| {
                right_cell(
                    &format_net_io(item.network_rx_bytes, item.network_tx_bytes),
                    w,
                )
            },
        },
        TopColumn {
            header: "CONTAINER ID",
            width: 12,
            align: TopAlign::Left,
            priority: 8,
            cell: |item, w| left_cell(&short_id(item.container_id.as_deref()), w),
        },
        TopColumn {
            header: "IMAGE",
            width: 32,
            align: TopAlign::Left,
            priority: 1,
            cell: |item, w| truncate(&item.image, w),
        },
    ];

    // Greedily keep essentials, then add columns by ascending priority while they
    // fit (each column costs its width + one separating space).
    let mut order: Vec<usize> = (0..columns.len()).collect();
    order.sort_by_key(|&idx| columns[idx].priority);

    let mut keep = vec![false; columns.len()];
    let mut used = 0usize;
    for &idx in &order {
        let cost = columns[idx].width + if used == 0 { 0 } else { 1 };
        let essential = columns[idx].priority == 0;
        if essential || used + cost <= available_width {
            keep[idx] = true;
            used += cost;
        }
    }

    let visible: Vec<&TopColumn> = columns
        .iter()
        .enumerate()
        .filter(|(idx, _)| keep[*idx])
        .map(|(_, col)| col)
        .collect();

    let mut lines = vec![
        format!("{} {}", title("Daemon"), muted("local control loop")),
        format!(
            "{} pid={} cpu={} mem={} uptime={}",
            label("litek8s"),
            daemon.pid,
            format_cpu_compact(daemon.cpu_percent),
            human_bytes(daemon.memory_bytes),
            format_uptime(daemon.uptime_secs),
        ),
        String::new(),
    ];

    // Per-node host resources, rendered above the deployment table.
    if !nodes.is_empty() {
        lines.extend(format_node_section(nodes, available_width));
        lines.push(String::new());
    }

    let header = visible
        .iter()
        .map(|col| match col.align {
            TopAlign::Left => header_left(col.header, col.width),
            TopAlign::Right => header_right(col.header, col.width),
        })
        .collect::<Vec<_>>()
        .join(" ");
    lines.push(header);

    for item in items {
        let row = visible
            .iter()
            .map(|col| (col.cell)(item, col.width))
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(row);
    }

    lines.join("\n")
}

/// A responsive column for the node-resources section of `top`.
struct NodeColumn {
    header: &'static str,
    width: usize,
    align: TopAlign,
    /// 0 = essential (never dropped); higher numbers shed first when narrow.
    priority: u8,
    cell: fn(&protocol::NodeSummary, usize) -> String,
}

/// Render the "Nodes" section: a responsive table of real host resources per
/// node. Columns drop by priority on narrow terminals; nodes without resources
/// (offline / old agent) render "-".
fn format_node_section(nodes: &[protocol::NodeSummary], available_width: usize) -> Vec<String> {
    let columns: &[NodeColumn] = &[
        NodeColumn {
            header: "NAME",
            width: 18,
            align: TopAlign::Left,
            priority: 0,
            cell: |node, w| left_cell(&truncate(&node.name, w), w),
        },
        NodeColumn {
            header: "STATUS",
            width: 10,
            align: TopAlign::Left,
            priority: 0,
            cell: |node, w| status_cell(&node.status, w),
        },
        NodeColumn {
            header: "CPU",
            width: 14,
            align: TopAlign::Left,
            priority: 0,
            cell: |node, w| {
                let text = node
                    .host_resources
                    .as_ref()
                    .map(|r| format!("{:.0}% ({}c)", r.cpu_usage_percent, r.cpu_cores))
                    .unwrap_or_else(|| "-".to_string());
                left_cell(&truncate(&text, w), w)
            },
        },
        NodeColumn {
            header: "MEM",
            width: 20,
            align: TopAlign::Left,
            priority: 0,
            cell: |node, w| {
                let text = node
                    .host_resources
                    .as_ref()
                    .map(|r| {
                        format!(
                            "{}/{}",
                            human_bytes(r.memory_used_bytes),
                            human_bytes(r.memory_total_bytes)
                        )
                    })
                    .unwrap_or_else(|| "-".to_string());
                left_cell(&truncate(&text, w), w)
            },
        },
        NodeColumn {
            header: "DISK",
            width: 20,
            align: TopAlign::Left,
            priority: 1,
            cell: |node, w| {
                let text = node
                    .host_resources
                    .as_ref()
                    .map(|r| {
                        let used = r.disk_total_bytes.saturating_sub(r.disk_free_bytes);
                        format!("{}/{}", human_bytes(used), human_bytes(r.disk_total_bytes))
                    })
                    .unwrap_or_else(|| "-".to_string());
                left_cell(&truncate(&text, w), w)
            },
        },
        NodeColumn {
            header: "LOAD",
            width: 8,
            align: TopAlign::Right,
            priority: 2,
            cell: |node, w| {
                let text = node
                    .host_resources
                    .as_ref()
                    .map(|r| format!("{:.2}", r.load_avg_1m))
                    .unwrap_or_else(|| "-".to_string());
                right_cell(&text, w)
            },
        },
        NodeColumn {
            header: "UPTIME",
            width: 10,
            align: TopAlign::Right,
            priority: 3,
            cell: |node, w| {
                let text = node
                    .host_resources
                    .as_ref()
                    .map(|r| format_uptime(r.uptime_secs))
                    .unwrap_or_else(|| "-".to_string());
                right_cell(&text, w)
            },
        },
    ];

    let mut order: Vec<usize> = (0..columns.len()).collect();
    order.sort_by_key(|&idx| columns[idx].priority);

    let mut keep = vec![false; columns.len()];
    let mut used = 0usize;
    for &idx in &order {
        let cost = columns[idx].width + if used == 0 { 0 } else { 1 };
        let essential = columns[idx].priority == 0;
        if essential || used + cost <= available_width {
            keep[idx] = true;
            used += cost;
        }
    }

    let visible: Vec<&NodeColumn> = columns
        .iter()
        .enumerate()
        .filter(|(idx, _)| keep[*idx])
        .map(|(_, col)| col)
        .collect();

    let mut lines = vec![title("Nodes")];

    let header = visible
        .iter()
        .map(|col| match col.align {
            TopAlign::Left => header_left(col.header, col.width),
            TopAlign::Right => header_right(col.header, col.width),
        })
        .collect::<Vec<_>>()
        .join(" ");
    lines.push(header);

    for node in nodes {
        let row = visible
            .iter()
            .map(|col| (col.cell)(node, col.width))
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(row);
    }

    lines
}

fn format_error_body(message: &str) -> String {
    format!("{}\n\n{}", title("Error"), message)
}

fn build_tree_entries(snapshot: &TreeSnapshot) -> Vec<TreeEntry> {
    let mut deployments_by_node: BTreeMap<String, Vec<protocol::DeploymentTopRow>> =
        BTreeMap::new();
    let mut deployment_names = BTreeSet::new();
    for deployment in &snapshot.deployments {
        deployment_names.insert(deployment.name.clone());
        deployments_by_node
            .entry(deployment.node_name.clone())
            .or_default()
            .push(deployment.clone());
    }
    for deployments in deployments_by_node.values_mut() {
        deployments.sort_by(|left, right| left.name.cmp(&right.name));
    }

    let mut services_by_deployment: BTreeMap<String, Vec<protocol::ServiceSummary>> =
        BTreeMap::new();
    let mut unresolved_services = Vec::new();
    for service in &snapshot.services {
        if deployment_names.contains(&service.deployment_name) {
            services_by_deployment
                .entry(service.deployment_name.clone())
                .or_default()
                .push(service.clone());
        } else {
            unresolved_services.push(service.clone());
        }
    }
    for services in services_by_deployment.values_mut() {
        services.sort_by(|left, right| left.name.cmp(&right.name));
    }
    unresolved_services.sort_by(|left, right| left.name.cmp(&right.name));

    let known_nodes: BTreeSet<String> = snapshot
        .nodes
        .iter()
        .map(|node| node.name.clone())
        .collect();
    let mut nodes = snapshot.nodes.clone();
    for node_name in deployments_by_node.keys() {
        if !known_nodes.contains(node_name) {
            nodes.push(protocol::NodeSummary {
                name: node_name.clone(),
                transport: "unknown".to_string(),
                endpoint: "missing from node registry".to_string(),
                status: "Offline".to_string(),
                scheduling: "-".to_string(),
                deployment_count: deployments_by_node
                    .get(node_name)
                    .map(|items| items.len())
                    .unwrap_or(0),
                vpn_ip: None,
                vpn_role: None,
                host_resources: None,
            });
        }
    }
    nodes.sort_by(|left, right| {
        let left_rank = u8::from(left.name != crate::node::LOCAL_NODE_NAME);
        let right_rank = u8::from(right.name != crate::node::LOCAL_NODE_NAME);
        (left_rank, left.name.as_str()).cmp(&(right_rank, right.name.as_str()))
    });

    let mut entries = Vec::new();
    for node in nodes {
        let deployments = deployments_by_node.remove(&node.name).unwrap_or_default();
        let deployment_count = deployments.len();
        let children = deployments
            .into_iter()
            .map(|deployment| {
                let services = services_by_deployment
                    .remove(&deployment.name)
                    .unwrap_or_default();
                let text = format_tree_deployment(&deployment, services.len());
                let mut children = deployment
                    .ports
                    .iter()
                    .map(|port| TreeEntry {
                        text: format_tree_port(port),
                        children: Vec::new(),
                    })
                    .collect::<Vec<_>>();
                children.extend(services.into_iter().map(|service| TreeEntry {
                    text: format_tree_service(&service, true),
                    children: Vec::new(),
                }));
                TreeEntry { text, children }
            })
            .collect();
        entries.push(TreeEntry {
            text: format_tree_node(&node, deployment_count),
            children,
        });
    }

    if !unresolved_services.is_empty() {
        let unresolved_count = unresolved_services.len();
        let children = unresolved_services
            .into_iter()
            .map(|service| TreeEntry {
                text: format_tree_service(&service, false),
                children: Vec::new(),
            })
            .collect();
        entries.push(TreeEntry {
            text: format!(
                "{} {}",
                label("services/unresolved"),
                muted(&format!("[count={unresolved_count}]"))
            ),
            children,
        });
    }

    entries
}

fn format_tree_node(node: &protocol::NodeSummary, deployment_count: usize) -> String {
    format!(
        "{} [{}, {}, {}, deployments={}] endpoint={}",
        label(&format!("node/{}", node.name)),
        color_status(&node.status),
        color_scheduling(&node.scheduling),
        node.transport,
        deployment_count,
        node.endpoint
    )
}

fn format_tree_deployment(deployment: &protocol::DeploymentTopRow, service_count: usize) -> String {
    let mut details = vec![
        color_status(&deployment.status),
        format!("image={}", deployment.image),
    ];
    if deployment.container_id.is_some() {
        details.push(format!(
            "ctr={}",
            short_id(deployment.container_id.as_deref())
        ));
    }
    if deployment.restart_count > 0 {
        details.push(format!("restarts={}", deployment.restart_count));
    }
    if let Some(last_exit) = deployment.last_exit.as_deref() {
        details.push(format!("last_exit={last_exit}"));
    }
    if deployment.next_retry_in_secs.is_some() {
        details.push(format!(
            "next_retry={}",
            format_retry_countdown(deployment.next_retry_in_secs)
        ));
    }
    if !deployment.ports.is_empty() {
        details.push(format!("published_ports={}", deployment.ports.len()));
    }
    if service_count > 0 {
        details.push(format!("services={service_count}"));
    }

    format!(
        "{} [{}]",
        label(&format!("deployment/{}", deployment.name)),
        details.join(", ")
    )
}

fn format_tree_port(port: &str) -> String {
    format!("{} {}", label("published-port"), muted(port))
}

fn format_tree_service(service: &protocol::ServiceSummary, attached: bool) -> String {
    let mut details = vec![
        color_status(&service.status),
        format!("listen={}", service.listen_addr),
        format!(
            "target={}",
            service
                .target_port
                .map(|value| value.to_string())
                .unwrap_or_else(|| "auto".to_string())
        ),
    ];
    if !attached {
        details.push(format!("backend={}", service.deployment_name));
    }

    format!(
        "{} [{}]",
        label(&format!("service/{}", service.name)),
        details.join(", ")
    )
}

fn append_tree_entries(lines: &mut Vec<String>, prefix: &str, entries: &[TreeEntry]) {
    for (index, entry) in entries.iter().enumerate() {
        let is_last = index + 1 == entries.len();
        let connector = if is_last { "└─" } else { "├─" };
        lines.push(format!("{prefix}{connector} {}", entry.text));

        if !entry.children.is_empty() {
            let next_prefix = if is_last {
                format!("{prefix}   ")
            } else {
                format!("{prefix}│  ")
            };
            append_tree_entries(lines, &next_prefix, &entry.children);
        }
    }
}

async fn run_proxy(
    endpoint: &DaemonEndpoint,
    name: &str,
    listen: &str,
    port: Option<u16>,
) -> Result<()> {
    let response = endpoint
        .request(Request::ResolveProxyTarget {
            name: name.to_string(),
            port,
        })
        .await?;

    let item = match response {
        Response::ProxyTarget { item } => item,
        other => {
            print_response(other)?;
            return Ok(());
        }
    };

    let listener = proxy::bind_listener(listen).await?;
    let bound_addr = listener
        .local_addr()
        .context("failed to read bound proxy address")?;

    println!("{}", title("Local Proxy"));
    println!("{}", muted("tcp forwarding through the local litek8s CLI"));
    println!();
    print_detail("deployment", &item.deployment_name);
    print_detail("node", &item.node_name);
    print_detail("transport", &item.transport);
    print_detail("listen", &bound_addr.to_string());
    let upstream = if let Some(ip) = &item.container_ip {
        format!("{}:{} ({})", ip, item.container_port, item.protocol)
    } else {
        format!(
            "127.0.0.1:{} ({})",
            item.host_port.map(|p| p.to_string()).unwrap_or_default(),
            item.protocol
        )
    };
    print_detail("upstream", &upstream);
    if let Some(ssh_target) = item.ssh_target.as_deref() {
        print_detail("ssh target", ssh_target);
    }
    println!();
    println!("{}", muted("proxy live; ctrl+c to stop"));

    proxy::serve_listener(listener, item).await
}

async fn run_events(
    endpoint: &DaemonEndpoint,
    watch: bool,
    limit: usize,
    deployment: Option<String>,
    node: Option<String>,
    interval: Duration,
) -> Result<()> {
    let mut watch_display = watch
        .then(|| {
            WatchDisplay::new(build_watch_banner(
                "events",
                interval,
                deployment.as_deref(),
                node.as_deref(),
            ))
        })
        .transpose()?;

    loop {
        let Some(response) = request_with_loading(
            endpoint,
            Request::GetEvents {
                limit,
                since_id: None,
                deployment_name: deployment.clone(),
                node_name: node.clone(),
            },
            watch_display.as_mut(),
            interval,
            "loading events...",
        )
        .await?
        else {
            return Ok(());
        };
        if watch_display.is_some() {
            watch_display
                .as_mut()
                .expect("watch display should exist")
                .begin_frame(interval)?;
        }
        print_response(response)?;
        io::stdout()
            .flush()
            .context("failed to flush events output")?;

        if !watch {
            return Ok(());
        }

        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

async fn request_with_loading(
    endpoint: &DaemonEndpoint,
    request: Request,
    mut watch_display: Option<&mut WatchDisplay>,
    _interval: Duration,
    loading_label: &str,
) -> Result<Option<Response>> {
    const LOADING_THRESHOLD: Duration = Duration::from_millis(180);

    let response = endpoint.request(request);
    tokio::pin!(response);

    let loading_delay = tokio::time::sleep(LOADING_THRESHOLD);
    tokio::pin!(loading_delay);

    tokio::select! {
        response = &mut response => response.map(Some),
        _ = tokio::signal::ctrl_c() => Ok(None),
        _ = &mut loading_delay => {
            if let Some(display) = watch_display.as_mut() {
                display.render_frame(&muted(loading_label))?;
            }

            tokio::select! {
                response = &mut response => response.map(Some),
                _ = tokio::signal::ctrl_c() => Ok(None),
            }
        }
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }

    value
        .chars()
        .take(max.saturating_sub(1))
        .collect::<String>()
        + "…"
}

fn print_detail(name: &str, value: &str) {
    println!("{} {}", label(&left_cell(&format!("{name}:"), 14)), value);
}

fn left_cell(value: &str, width: usize) -> String {
    format!("{value:<width$}")
}

fn right_cell(value: &str, width: usize) -> String {
    format!("{value:>width$}")
}

fn header_left(value: &str, width: usize) -> String {
    label(&left_cell(value, width))
}

fn header_right(value: &str, width: usize) -> String {
    label(&right_cell(value, width))
}

fn use_color() -> bool {
    io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn paint(text: &str, ansi: &str) -> String {
    if use_color() {
        format!("\x1b[{ansi}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn title(text: &str) -> String {
    paint(text, "1;96")
}

fn label(text: &str) -> String {
    paint(text, "1;94")
}

fn ok(text: &str) -> String {
    paint(text, "1;92")
}

fn muted(text: &str) -> String {
    paint(text, "2")
}

fn color_status(status: &str) -> String {
    match status {
        "Running" | "Ready" | "Normal" | "Listening" | "Schedulable" => paint(status, "1;92"),
        "Pending" | "Restarting" | "Warning" | "Cordoned" | "Starting" | "NoEndpoints"
        | "Degraded" => paint(status, "1;93"),
        "Failed" | "Deleted" | "Offline" | "Error" => paint(status, "1;91"),
        _ => status.to_string(),
    }
}

fn status_cell(status: &str, width: usize) -> String {
    let padded = left_cell(status, width);
    match status {
        "Running" | "Ready" | "Normal" | "Listening" | "Schedulable" => paint(&padded, "1;92"),
        "Pending" | "Restarting" | "Warning" | "Cordoned" | "Starting" | "NoEndpoints"
        | "Degraded" => paint(&padded, "1;93"),
        "Failed" | "Deleted" | "Offline" | "Error" => paint(&padded, "1;91"),
        _ => padded,
    }
}

fn color_scheduling(value: &str) -> String {
    match value {
        "Schedulable" => paint(value, "1;92"),
        "Cordoned" => paint(value, "1;93"),
        _ => value.to_string(),
    }
}

fn scheduling_cell(value: &str, width: usize) -> String {
    let padded = left_cell(value, width);
    match value {
        "Schedulable" => paint(&padded, "1;92"),
        "Cordoned" => paint(&padded, "1;93"),
        _ => padded,
    }
}

fn short_id(value: Option<&str>) -> String {
    value
        .map(|id| id.chars().take(12).collect::<String>())
        .unwrap_or_else(|| "-".to_string())
}

fn format_top_cpu(value: Option<f64>, has_live_metrics: bool) -> String {
    match value {
        Some(value) => format!("{value:>6.2}%"),
        None if has_live_metrics => "warmup".to_string(),
        None => "n/a".to_string(),
    }
}

fn format_retry_countdown(value: Option<u64>) -> String {
    match value {
        Some(0) => "now".to_string(),
        Some(seconds) => format!("{seconds}s"),
        None => "-".to_string(),
    }
}

fn format_cpu_compact(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.2}%"))
        .unwrap_or_else(|| "n/a".to_string())
}

fn format_memory(usage: Option<u64>, limit: Option<u64>) -> String {
    match (usage, limit) {
        (Some(usage), Some(limit)) if limit > 0 => {
            format!("{}/{}", human_bytes(usage), human_bytes(limit))
        }
        (Some(usage), _) => human_bytes(usage),
        _ => "n/a".to_string(),
    }
}

fn format_net_io(rx: Option<u64>, tx: Option<u64>) -> String {
    match (rx, tx) {
        (Some(rx), Some(tx)) => format!("{}/{}", human_bytes(rx), human_bytes(tx)),
        _ => "n/a".to_string(),
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

fn format_uptime(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;

    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn cli_styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::BrightBlue.on_default() | Effects::BOLD)
        .usage(AnsiColor::BrightGreen.on_default() | Effects::BOLD)
        .literal(AnsiColor::BrightCyan.on_default())
        .placeholder(AnsiColor::BrightYellow.on_default())
        .error(AnsiColor::BrightRed.on_default() | Effects::BOLD)
        .valid(AnsiColor::BrightGreen.on_default() | Effects::BOLD)
        .invalid(AnsiColor::BrightRed.on_default() | Effects::BOLD)
}

struct WatchDisplay {
    interactive: bool,
    banner: String,
    previous_lines: Vec<String>,
}

impl WatchDisplay {
    fn new(banner: String) -> Result<Self> {
        let interactive = io::stdout().is_terminal();
        if interactive {
            print!("\x1B[?1049h\x1B[?25l");
            io::stdout()
                .flush()
                .context("failed to enter interactive watch view")?;
        }

        Ok(Self {
            interactive,
            banner,
            previous_lines: Vec::new(),
        })
    }

    fn begin_frame(&mut self, _interval: Duration) -> Result<()> {
        if self.interactive {
            print!("\x1B[H\x1B[J");
        } else {
            println!();
            println!("---");
        }

        self.previous_lines.clear();
        println!("{}\n", self.banner);
        Ok(())
    }

    fn render_frame(&mut self, body: &str) -> Result<()> {
        if self.interactive {
            let next_lines = build_watch_frame_lines(&self.banner, body);
            let patch = render_watch_frame_patch(&self.previous_lines, &next_lines);
            print!("{patch}");
            self.previous_lines = next_lines;
        } else {
            println!();
            println!("---");
            println!("{}\n", self.banner);
            print!("{body}");
            if !body.ends_with('\n') {
                println!();
            }
        }
        io::stdout()
            .flush()
            .context("failed to flush watch frame output")
    }
}

fn build_watch_frame_lines(banner: &str, body: &str) -> Vec<String> {
    let mut lines = vec![banner.to_string(), String::new()];
    lines.extend(body.lines().map(|line| line.to_string()));
    lines
}

fn render_watch_frame_patch(previous: &[String], next: &[String]) -> String {
    if previous.is_empty() {
        let mut full = String::from("\x1B[H\x1B[J");
        for (index, line) in next.iter().enumerate() {
            if index > 0 {
                full.push('\n');
            }
            full.push_str(line);
        }
        full.push_str("\x1B[J");
        return full;
    }

    let mut patch = String::new();
    let max_lines = previous.len().max(next.len());
    for index in 0..max_lines {
        let old = previous.get(index);
        let new = next.get(index);
        if old == new {
            continue;
        }

        patch.push_str(&format!("\x1B[{};1H\x1B[2K", index + 1));
        if let Some(line) = new {
            patch.push_str(line);
        }
    }

    patch
}

fn build_watch_banner(
    kind: &str,
    interval: Duration,
    deployment: Option<&str>,
    node: Option<&str>,
) -> String {
    let mut parts = vec![
        format!("litek8s {kind}"),
        "live".to_string(),
        format!("interval={}s", interval.as_secs()),
    ];
    if let Some(deployment) = deployment {
        parts.push(format!("deployment={deployment}"));
    }
    if let Some(node) = node {
        parts.push(format!("node={node}"));
    }
    parts.push("ctrl+c to exit".to_string());
    parts.join("  ")
}

impl Drop for WatchDisplay {
    fn drop(&mut self) {
        if self.interactive {
            print!("\x1B[?25h\x1B[?1049l");
            let _ = io::stdout().flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::protocol::{DaemonTopSummary, DeploymentTopRow, NodeSummary, ServiceSummary};

    use super::{
        TreeSnapshot, build_watch_frame_lines, format_retry_countdown, format_top_body_for_width,
        format_top_cpu, format_uptime, render_tree, render_watch_frame_patch,
    };

    fn sample_top_row() -> DeploymentTopRow {
        DeploymentTopRow {
            name: "echo-demo".to_string(),
            node_name: "litek8s-node-0".to_string(),
            status: "Running".to_string(),
            ready: true,
            image: "hashicorp/http-echo:1.0".to_string(),
            container_id: Some("abcd1234efgh5678".to_string()),
            ports: vec!["8090->5678/TCP".to_string()],
            restart_count: 2,
            restart_streak: 1,
            last_exit: Some("spec changed".to_string()),
            next_retry_in_secs: None,
            ready_replicas: 1,
            desired_replicas: 1,
            cpu_percent: Some(1.5),
            memory_usage_bytes: Some(20 * 1024 * 1024),
            memory_limit_bytes: None,
            network_rx_bytes: Some(2300),
            network_tx_bytes: Some(126),
        }
    }

    fn sample_daemon() -> DaemonTopSummary {
        DaemonTopSummary {
            pid: 10513,
            cpu_percent: Some(2.5),
            memory_bytes: 20 * 1024 * 1024,
            uptime_secs: 797,
        }
    }

    fn sample_node(name: &str, with_resources: bool) -> NodeSummary {
        NodeSummary {
            name: name.to_string(),
            transport: "ssh".to_string(),
            endpoint: name.to_string(),
            status: "Ready".to_string(),
            scheduling: "Schedulable".to_string(),
            deployment_count: 0,
            vpn_ip: None,
            vpn_role: None,
            host_resources: with_resources.then(|| crate::protocol::HostResources {
                cpu_cores: 2,
                cpu_usage_percent: 12.0,
                memory_used_bytes: 600 * 1024 * 1024,
                memory_total_bytes: 2 * 1024 * 1024 * 1024,
                disk_free_bytes: 4 * 1024 * 1024 * 1024,
                disk_total_bytes: 8 * 1024 * 1024 * 1024,
                load_avg_1m: 0.34,
                load_avg_5m: 0.20,
                load_avg_15m: 0.10,
                uptime_secs: 3920,
            }),
        }
    }

    #[test]
    fn top_table_drops_columns_when_terminal_is_narrow() {
        let daemon = sample_daemon();
        let rows = vec![sample_top_row()];

        // Wide terminal keeps every column.
        let wide = format_top_body_for_width(&daemon, &rows, &[], 200);
        for header in [
            "NAME",
            "NODE",
            "STATUS",
            "RESTARTS",
            "STREAK",
            "NEXT RETRY",
            "LAST EXIT",
            "CPU",
            "MEMORY",
            "NET I/O",
            "CONTAINER ID",
            "IMAGE",
        ] {
            assert!(wide.contains(header), "wide layout should keep `{header}`");
        }

        // Narrow terminal keeps the essentials and sheds the lowest-priority columns.
        let narrow = format_top_body_for_width(&daemon, &rows, &[], 60);
        for header in ["NAME", "STATUS", "CPU", "MEMORY"] {
            assert!(
                narrow.contains(header),
                "narrow layout must keep `{header}`"
            );
        }
        for header in ["CONTAINER ID", "LAST EXIT", "STREAK"] {
            assert!(
                !narrow.contains(header),
                "narrow layout should drop low-priority `{header}`"
            );
        }
    }

    #[test]
    fn top_renders_node_resources_and_degrades_when_absent() {
        let daemon = sample_daemon();
        let rows = vec![sample_top_row()];
        let nodes = vec![
            sample_node("litek8s-node-0", true),
            sample_node("offline", false),
        ];

        let body = format_top_body_for_width(&daemon, &rows, &nodes, 200);

        // Nodes section appears with its headers and real values.
        assert!(body.contains("Nodes"), "should render a Nodes section");
        assert!(body.contains("litek8s-node-0"));
        assert!(body.contains("(2c)"), "should show core count");
        assert!(body.contains("0.34"), "should show load average");

        // The node without resources still lists, showing `-` placeholders.
        assert!(body.contains("offline"));

        // Narrow terminal drops low-priority node columns (UPTIME, LOAD) but keeps NAME/CPU/MEM.
        let narrow = format_top_body_for_width(&daemon, &rows, &nodes, 55);
        assert!(narrow.contains("CPU") && narrow.contains("MEM"));
        assert!(
            !narrow.contains("UPTIME"),
            "narrow node section should drop UPTIME"
        );
    }

    #[test]
    fn formats_uptime_compactly() {
        assert_eq!(format_uptime(9), "9s");
        assert_eq!(format_uptime(75), "1m15s");
        assert_eq!(format_uptime(3723), "1h02m03s");
    }

    #[test]
    fn formats_top_cpu_warmup_when_metrics_exist_without_cpu_delta() {
        assert_eq!(format_top_cpu(None, true), "warmup");
        assert_eq!(format_top_cpu(None, false), "n/a");
    }

    #[test]
    fn formats_retry_countdown() {
        assert_eq!(format_retry_countdown(None), "-");
        assert_eq!(format_retry_countdown(Some(0)), "now");
        assert_eq!(format_retry_countdown(Some(13)), "13s");
    }

    #[test]
    fn first_interactive_watch_frame_clears_once() {
        let next = build_watch_frame_lines("banner", "line1\nline2");
        let patch = render_watch_frame_patch(&[], &next);
        assert!(patch.starts_with("\x1B[H\x1B[J"));
        assert!(patch.contains("banner\n\nline1\nline2"));
    }

    #[test]
    fn interactive_watch_patch_updates_only_changed_lines() {
        let previous = build_watch_frame_lines("banner", "same\nold");
        let next = build_watch_frame_lines("banner", "same\nnew");
        let patch = render_watch_frame_patch(&previous, &next);
        assert!(!patch.contains("\x1B[H\x1B[J"));
        assert!(patch.contains("\x1B[4;1H\x1B[2Knew"));
        assert!(!patch.contains("\x1B[3;1H"));
    }

    #[test]
    fn interactive_watch_patch_clears_removed_trailing_lines() {
        let previous = build_watch_frame_lines("banner", "line1\nline2");
        let next = build_watch_frame_lines("banner", "line1");
        let patch = render_watch_frame_patch(&previous, &next);
        assert!(patch.contains("\x1B[4;1H\x1B[2K"));
    }

    #[test]
    fn renders_tree_with_real_relationships() {
        let snapshot = TreeSnapshot {
            daemon: DaemonTopSummary {
                pid: 42,
                cpu_percent: Some(0.7),
                memory_bytes: 18 * 1024 * 1024,
                uptime_secs: 75,
            },
            nodes: vec![
                NodeSummary {
                    name: "local".to_string(),
                    transport: "local".to_string(),
                    endpoint: "local Docker socket".to_string(),
                    status: "Ready".to_string(),
                    scheduling: "Schedulable".to_string(),
                    deployment_count: 1,
                    vpn_ip: None,
                    vpn_role: None,
                    host_resources: None,
                },
                NodeSummary {
                    name: "server77".to_string(),
                    transport: "ssh".to_string(),
                    endpoint: "server77".to_string(),
                    status: "Ready".to_string(),
                    scheduling: "Schedulable".to_string(),
                    deployment_count: 1,
                    vpn_ip: None,
                    vpn_role: None,
                    host_resources: None,
                },
            ],
            deployments: vec![
                DeploymentTopRow {
                    name: "echo-demo".to_string(),
                    node_name: "local".to_string(),
                    status: "Running".to_string(),
                    ready: true,
                    image: "hashicorp/http-echo:1.0".to_string(),
                    container_id: Some("abcd1234efgh5678".to_string()),
                    ports: vec!["8090->5678/TCP".to_string()],
                    restart_count: 0,
                    restart_streak: 0,
                    last_exit: None,
                    next_retry_in_secs: None,
                    ready_replicas: 1,
                    desired_replicas: 1,
                    cpu_percent: Some(0.0),
                    memory_usage_bytes: None,
                    memory_limit_bytes: None,
                    network_rx_bytes: None,
                    network_tx_bytes: None,
                },
                DeploymentTopRow {
                    name: "echo-edge".to_string(),
                    node_name: "server77".to_string(),
                    status: "Running".to_string(),
                    ready: true,
                    image: "hashicorp/http-echo:1.0".to_string(),
                    container_id: Some("1234abcd5678efgh".to_string()),
                    ports: vec!["8092->5678/TCP".to_string()],
                    restart_count: 1,
                    restart_streak: 0,
                    last_exit: Some("spec changed".to_string()),
                    next_retry_in_secs: None,
                    ready_replicas: 1,
                    desired_replicas: 1,
                    cpu_percent: Some(0.0),
                    memory_usage_bytes: None,
                    memory_limit_bytes: None,
                    network_rx_bytes: None,
                    network_tx_bytes: None,
                },
            ],
            services: vec![
                ServiceSummary {
                    name: "echo-svc".to_string(),
                    deployment_name: "echo-demo".to_string(),
                    listen_addr: "127.0.0.1:9100".to_string(),
                    target_port: Some(5678),
                    status: "Listening".to_string(),
                },
                ServiceSummary {
                    name: "edge-svc".to_string(),
                    deployment_name: "echo-edge".to_string(),
                    listen_addr: "127.0.0.1:9102".to_string(),
                    target_port: Some(5678),
                    status: "Listening".to_string(),
                },
            ],
        };

        let rendered = render_tree(&snapshot);
        assert!(rendered.contains("litek8s/control-plane"));
        assert!(rendered.contains("node/local"));
        assert!(rendered.contains("node/server77"));
        assert!(rendered.contains("deployment/echo-demo"));
        assert!(rendered.contains("deployment/echo-edge"));
        assert!(rendered.contains("service/echo-svc"));
        assert!(rendered.contains("service/edge-svc"));
        assert!(rendered.contains("services=1"));
        assert!(rendered.contains("published-port"));
        assert!(rendered.contains("8090->5678/TCP"));
    }

    #[test]
    fn renders_unresolved_services_bucket() {
        let snapshot = TreeSnapshot {
            daemon: DaemonTopSummary {
                pid: 42,
                cpu_percent: None,
                memory_bytes: 8 * 1024 * 1024,
                uptime_secs: 5,
            },
            nodes: vec![NodeSummary {
                name: "local".to_string(),
                transport: "local".to_string(),
                endpoint: "local Docker socket".to_string(),
                status: "Ready".to_string(),
                scheduling: "Schedulable".to_string(),
                deployment_count: 0,
                vpn_ip: None,
                vpn_role: None,
                host_resources: None,
            }],
            deployments: Vec::new(),
            services: vec![ServiceSummary {
                name: "ghost-svc".to_string(),
                deployment_name: "selector:app=ghost".to_string(),
                listen_addr: "127.0.0.1:9191".to_string(),
                target_port: None,
                status: "Error".to_string(),
            }],
        };

        let rendered = render_tree(&snapshot);
        assert!(rendered.contains("services/unresolved"));
        assert!(rendered.contains("service/ghost-svc"));
        assert!(rendered.contains("backend=selector:app=ghost"));
    }
}
