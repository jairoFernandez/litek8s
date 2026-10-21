use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

const HELP_TEMPLATE: &str = "\
{before-help}{name} {version}
{about-with-newline}
{usage-heading} {usage}

{all-args}{after-help}";

/// `litek8s` with no arguments: commands grouped by what they talk to.
/// Every visible subcommand is listed with its `about` (checked by
/// `main_help_lists_every_command`).
const MAIN_HELP_TEMPLATE: &str = "\
{before-help}{name} {version}
{about-with-newline}
{usage-heading} {usage}

Kubernetes:
  dev            Local Kubernetes clusters for development (like kind/k3d), no Docker needed
  ui             Terminal UI: clusters, containers, images, volumes, Kubernetes, metrics and alerts
  apiserver      Run the Kubernetes API server (kubectl and Helm talk to it)
  agent          Run a node agent that joins a litek8s API server
  server         List or remove the API servers of a highly available control plane
  lb             A TCP load balancer in front of the API servers of an HA control plane
  tree           A tree of the cluster: nodes, workloads down their owners, services
  install        Run litek8s as a systemd service on this Linux server, or on another over --ssh
  uninstall      Stop and remove the litek8s systemd service and its pods
  cluster        Clusters of machines over SSH from a cluster file (apply, status, delete)

Observability (built into the API server):
  logs           Search and follow pod logs, including containers that are gone
  query          Run a PromQL query against the cluster's built-in metrics
  alerts         List, silence and acknowledge the cluster's alerts
  top            Live CPU and memory of nodes and pods (metrics.k8s.io)

Agents:
  mcp            MCP server for LLM agents on stdin/stdout (claude mcp add litek8s -- litek8s mcp)
  skill          Print or install the litek8s agent skill (SKILL.md and references)

  completion     Print a shell completion script (bash, zsh, fish, powershell, elvish)
  help           Print this message or the help of the given subcommand(s)

Options:
{options}{after-help}";

const COMPLETION_AFTER_HELP: &str = "\
Load it once per shell session, or install it:
  zsh:   litek8s completion zsh > \"${fpath[1]}/_litek8s\"   (then restart the shell)
         or in ~/.zshrc: source <(litek8s completion zsh)
  bash:  litek8s completion bash > ~/.local/share/bash-completion/completions/litek8s
  fish:  litek8s completion fish > ~/.config/fish/completions/litek8s.fish
Homebrew installs them for you.";

const MAIN_AFTER_HELP: &str = "\
Quickstart:
  litek8s dev up                    a local cluster; kubectl context litek8s-dev
  kubectl get nodes
  litek8s ui                        clusters, containers, pods, logs, metrics
  litek8s query 'sum by (namespace) (container_memory_working_set_bytes)'
  litek8s dev down

Docker clients use the same cluster: export DOCKER_HOST=unix://$HOME/.litek8s/docker.sock";

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

const INSTALL_AFTER_HELP: &str = "\
Examples (as root, on a Linux server with systemd):
  litek8s install server
  litek8s install agent --join https://192.168.1.10:6443 --token K10...::...
  litek8s install server --cluster-init                 first of 3 HA servers
  litek8s install server --server https://192.168.1.10:6443 --token K10...::...
  litek8s install --skip-checks server --runtime docker --node-ip 192.168.1.10

Writes /etc/systemd/system/litek8s-server.service (or litek8s-agent.service)
running /usr/local/bin/litek8s (copied there if needed), enables and starts
it, and waits until the server answers /readyz or the node is Ready. The
token goes to /etc/litek8s/<server|agent>.env (0600), never into the unit.
Defaults a server gets unless given: --listen 0.0.0.0:6443 --data-dir
/var/lib/litek8s/apiserver --node-dir /var/lib/litek8s/node-agent --runtime
crun --node-ip <detected> --kubeconfig /etc/litek8s/kubeconfig.yaml (an agent:
--node-dir, --runtime, --node-ip). Re-running with other flags (or another
binary) replaces them and restarts the service; pods keep running across
restarts.

Straight from a release: curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh -s -- server

From another machine, over ssh (sudo there; no root needed here):
  litek8s install server --ssh pi@192.168.1.10
  litek8s install agent --ssh me@old-laptop --join https://192.168.1.10:6443 --token K10...::...
SSH flags (anywhere after server/agent): --ssh USER@HOST, -i KEY, --ssh-port N,
--ssh-option K=V; --binary PATH (a Linux build to upload; default: this one if
it is a static Linux build, else the GitHub release of this version, fetched
by the machine), --release vX.Y.Z; for a server --context NAME (default: its
node name) and --api-address HOST[:PORT] for the kubeconfig context added here.
An unchanged binary and flags leave the service running as it is.
Several machines at once: `litek8s cluster apply -f cluster.yaml`.";

const UNINSTALL_AFTER_HELP: &str = "\
Examples:
  sudo litek8s uninstall
  sudo litek8s uninstall --purge
  litek8s uninstall --ssh pi@192.168.1.10              from this machine, over ssh

Stops and removes litek8s-server.service / litek8s-agent.service, kills every
pod and removes the pod network (litek8s0, litek8s-wg, LITEK8S-* iptables
chains). The data directories stay unless --purge; /usr/local/bin/litek8s stays.";

const SERVER_AFTER_HELP: &str = "\
Examples:
  litek8s server list --server http://192.168.1.10:6443
  litek8s server remove pi-3 --server https://192.168.1.10:6443 --token K10...::...

Servers join with `litek8s apiserver --server https://<existing>:6443 --token <token>`
(the first one starts with --cluster-init). Remove a server before reinstalling it.";

const LB_AFTER_HELP: &str = "\
Examples:
  litek8s lb --listen 127.0.0.1:6443 --server http://192.168.1.10:6443
  kubectl --server http://127.0.0.1:6443 get nodes

Connections go whole (HTTP or TLS) to a server whose /readyz answers 200.";

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
pub(crate) fn parse_duration(raw: &str) -> Result<Duration, String> {
    let raw = raw.trim();
    let (number, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len()));
    let n: u64 = number.parse().map_err(|_| format!("invalid duration {raw:?} (use e.g. 40s, 5m, 1h)"))?;
    let secs = match unit {
        "ms" => return Ok(Duration::from_millis(n)),
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => return Err(format!("invalid duration unit in {raw:?} (ms, s, m or h)")),
    };
    Ok(Duration::from_secs(secs))
}

const CLUSTER_AFTER_HELP: &str = "\
Examples:
  litek8s cluster apply -f cluster.yaml
  litek8s cluster apply -f cluster.yaml --prune      also remove agents taken out of the file
  litek8s cluster status -f cluster.yaml
  litek8s cluster delete -f cluster.yaml --yes

A cluster file:
  apiVersion: litek8s.dev/v1
  kind: Cluster
  metadata: {name: home}
  servers: [{ssh: pi@192.168.1.10}]               # 1, or 3/5 for HA
  agents:
    - {ssh: me@old-laptop, labels: {disk: ssd}}
    - {ssh: ubuntu@ec2-host, nodeIp: 10.0.0.7, taints: ['gpu=true:NoSchedule']}

apply runs `litek8s install server|agent --ssh` on each machine (unchanged
ones are left alone), adds the kubeconfig context <name>, and sets the labels
and taints on the Nodes. Local clusters for development: `litek8s dev up`.
See docs/clusters.md.";

const TOP_AFTER_HELP: &str = "\
Nodes and pods of the current kube context from its metrics API
(metrics.k8s.io): live on a terminal, once when piped.


Examples:
  litek8s top
  litek8s top --watch
  litek8s top --watch --interval-secs 1
  litek8s top --context litek8s-dev -n kube-system --containers
  litek8s top --once --sort-by memory";

const TREE_AFTER_HELP: &str = "\
The current kube context's cluster, nodes and then each namespace's
workloads down their owners (Deployment, ReplicaSet, Pod), and its Services
with the pods they select.


Examples:
  litek8s tree
  litek8s tree -n argocd
  litek8s tree --context litek8s-dev2 --watch
  litek8s tree --watch --interval-secs 1";

const LOGS_AFTER_HELP: &str = "\
Searches the logs of every pod on every node (also of containers that are
gone, kept per node for --obs-log-retention), newest last.

Examples:
  litek8s logs -l app=web --since 1h --grep 'ERROR|panic'
  litek8s logs -n prod api- -f
  litek8s logs --grep '(?i)timeout' --since 2026-10-19T08:00:00Z --until 30m --json";

const ALERTS_AFTER_HELP: &str = "\
Examples:
  litek8s alerts
  litek8s alerts ack 3f2a9c1d04b7e6a1
  litek8s alerts silence alertname=KubePodCrashLooping namespace=dev --for 4h --comment 'deploying'
  litek8s alerts silences
  litek8s alerts unsilence silence-x7k2p
Rules, receivers (webhook, ntfy, Telegram) and silences are objects: see
docs/observability.md (kubectl get alertrules,alertreceivers,silences).";

const QUERY_AFTER_HELP: &str = "\
Examples:
  litek8s query 'topk(5, sum by (namespace, pod) (rate(container_cpu_usage_seconds_total[5m])))'
  litek8s query 'container_memory_working_set_bytes{namespace=\"default\"}' --range 1h --step 5m
Grafana: add a Prometheus data source with URL http://<server>:6443/prometheus.";

const MCP_AFTER_HELP: &str = "\
Add it to Claude Code:
  claude mcp add litek8s -- litek8s mcp
  claude mcp add litek8s-ro -- litek8s mcp --read-only --context litek8s-dev
Tools: clusters_list, cluster_status/up/down/delete, contexts_list, get, describe, events,
diagnose_pod, apply, delete, pod_logs, logs_search, query, query_range, alerts_list,
alert_silence/unsilence, docker_ps/images/logs. Resources: the skill and docs/*.md.
It refuses kube contexts whose server is not litek8s; delete, cluster_down and
cluster_delete need confirm: true. See docs/agents.md.";

const SKILL_AFTER_HELP: &str = "\
Examples:
  litek8s skill                               # SKILL.md
  litek8s skill observability                 # PromQL recipes, log search, alerts
  litek8s skill --install ~/.claude/skills    # writes ~/.claude/skills/litek8s/";

#[derive(Debug, Parser)]
#[command(
    name = "litek8s",
    version,
    about = "A lightweight Kubernetes for home, edge and local development: the real API, one binary.",
    help_template = MAIN_HELP_TEMPLATE,
    after_help = MAIN_AFTER_HELP,
    styles = cli_styles(),
    next_line_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// `litek8s apiserver`'s observability flags (docs/observability.md).
#[derive(Debug, clap::Args)]
struct ObsArgs {
    #[arg(long, help = "Do not collect metrics, search logs or evaluate alerts")]
    no_observability: bool,
    #[arg(long, default_value = "10s:6h,1m:7d,10m:90d", help = "Metric tiers, <resolution>:<retention>,...: the first is the sampling interval; each later tier keeps a downsampled copy")]
    obs_retention: String,
    #[arg(long, default_value = "24Mi", help = "Memory the metrics store may use for series and head chunks (new series beyond it are refused)")]
    obs_memory: String,
    #[arg(long, default_value = "7d", value_parser = |s: &str| crate::obs::tsdb::parse_duration(s).map_err(|e| e.to_string()), help = "How long logs of removed containers are kept on each node")]
    obs_log_retention: Duration,
    #[arg(long, default_value = "512Mi", help = "Most disk the log archive of removed containers may use on each node")]
    obs_log_max_size: String,
    #[arg(long, help = "Do not evaluate the built-in alert rules (CrashLoopBackOff, OOMKilled, NotReady nodes, full disks, ...)")]
    no_builtin_alerts: bool,
    #[arg(long, help = "Also serve the Prometheus HTTP API at the root of this address (e.g. 0.0.0.0:9090); it is always under /prometheus on the API port")]
    prometheus_listen: Option<std::net::SocketAddr>,
}

impl ObsArgs {
    fn config(&self) -> Result<Option<crate::obs::server::ObsConfig>> {
        if self.no_observability {
            return Ok(None);
        }
        let bytes = |s: &str, what: &str| -> Result<u64> {
            crate::kube::quantity::parse_units(s).filter(|b| *b > 0).map(|b| b as u64).with_context(|| format!("{what}: not a size: {s:?} (e.g. 24Mi)"))
        };
        let tiers = crate::obs::tsdb::parse_tiers(&self.obs_retention).context("--obs-retention")?;
        Ok(Some(crate::obs::server::ObsConfig {
            dir: None,
            interval: tiers[0].resolution,
            tiers,
            memory_budget: bytes(&self.obs_memory, "--obs-memory")?,
            log_retention: self.obs_log_retention,
            log_max_bytes: bytes(&self.obs_log_max_size, "--obs-log-max-size")?,
            builtin_alerts: !self.no_builtin_alerts,
            eval_interval: Duration::from_secs(30),
        }))
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(
        about = "Run the Kubernetes API server (kubectl and Helm talk to it)",
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
        #[arg(long, env = "LITEK8S_TOKEN", hide_env_values = true, help = "Join secret (or a whole token) instead of the stored or a generated one")]
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
        #[arg(long, help = "Public address of this server's node when its own is private (a cloud VM): the overlay endpoint other nodes use")]
        node_external_ip: Option<std::net::IpAddr>,
        #[arg(long, help = "Also serve the Docker Engine API on this Unix socket (crun runtime; published ports bind on 127.0.0.1)")]
        docker_socket: Option<PathBuf>,
        #[arg(long, requires = "data_dir", help = "Start a highly available control plane with this server as its first member (a single server's objects move into it)")]
        cluster_init: bool,
        #[arg(long, value_name = "URL", requires_all = ["data_dir", "token"], conflicts_with = "cluster_init", help = "Join the control plane of this server as another server, e.g. https://192.168.1.10:6443 (needs --token)")]
        server: Option<String>,
        #[arg(long, help = "HA: port of the Raft listener between servers (default: the --listen port + 1)")]
        raft_port: Option<u16>,
        #[command(flatten)]
        obs: ObsArgs,
    },
    #[command(
        about = "Run litek8s as a systemd service on this Linux server, or on another over --ssh",
        help_template = HELP_TEMPLATE,
        after_help = INSTALL_AFTER_HELP
    )]
    Install {
        #[arg(long, help = "Do not check for crun, cgroup v2, WireGuard and free ports first")]
        skip_checks: bool,
        #[arg(long, default_value = "3m", value_parser = parse_duration, help = "How long to wait for the server to be ready or the node to join")]
        timeout: Duration,
        #[command(subcommand)]
        role: InstallRole,
    },
    #[command(
        about = "Stop and remove the litek8s systemd service and its pods",
        help_template = HELP_TEMPLATE,
        after_help = UNINSTALL_AFTER_HELP
    )]
    Uninstall {
        #[arg(long, help = "Also delete the data directories (the cluster's objects, images, volumes) and /etc/litek8s")]
        purge: bool,
        #[arg(long, value_name = "USER@HOST", help = "Uninstall from this machine over ssh (with sudo there)")]
        ssh: Option<String>,
        #[arg(short = 'i', long, value_name = "FILE", requires = "ssh", help = "With --ssh: the private key (ssh -i)")]
        identity_file: Option<PathBuf>,
        #[arg(long, value_name = "PORT", requires = "ssh", help = "With --ssh: the SSH port")]
        ssh_port: Option<u16>,
        #[arg(long, value_name = "KEY=VALUE", requires = "ssh", help = "With --ssh: an ssh -o option (repeatable)")]
        ssh_option: Vec<String>,
    },
    #[command(
        about = "Local Kubernetes clusters for development (like kind/k3d), no Docker needed",
        help_template = HELP_TEMPLATE,
        after_help = DEV_AFTER_HELP
    )]
    Dev {
        #[command(subcommand)]
        command: crate::dev::DevCommand,
    },
    #[command(
        about = "MCP server for LLM agents on stdin/stdout (claude mcp add litek8s -- litek8s mcp)",
        help_template = HELP_TEMPLATE,
        after_help = MCP_AFTER_HELP
    )]
    Mcp(crate::mcp::McpArgs),
    #[command(
        about = "Print a shell completion script (bash, zsh, fish, powershell, elvish)",
        help_template = HELP_TEMPLATE,
        after_help = COMPLETION_AFTER_HELP
    )]
    Completion {
        #[arg(value_enum, help = "The shell")]
        shell: clap_complete::Shell,
    },
    #[command(
        about = "Print or install the litek8s agent skill (SKILL.md and references)",
        help_template = HELP_TEMPLATE,
        after_help = SKILL_AFTER_HELP
    )]
    Skill {
        #[arg(help = "A reference to print instead of SKILL.md: dev-clusters, observability, docker, ha-and-join, differences, troubleshooting")]
        reference: Option<String>,
        #[arg(long, help = "List the skill's files")]
        list: bool,
        #[arg(long, value_name = "DIR", help = "Write the skill into DIR/litek8s (e.g. ~/.claude/skills or .claude/skills)")]
        install: Option<PathBuf>,
    },
    #[command(
        about = "Terminal UI: clusters, containers, images, volumes, Kubernetes, metrics and alerts",
        help_template = HELP_TEMPLATE,
        after_help = UI_AFTER_HELP
    )]
    Ui(crate::ui::UiArgs),
    #[command(
        about = "Run a node agent that joins a litek8s API server",
        help_template = HELP_TEMPLATE,
        after_help = NODE_AGENT_AFTER_HELP
    )]
    #[command(alias = "node-agent")]
    Agent {
        #[arg(long, alias = "server", help = "API server URL, e.g. https://192.168.1.10:6443")]
        join: String,
        #[arg(long, env = "LITEK8S_TOKEN", hide_env_values = true, help = "Join token printed by the API server")]
        token: String,
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
        #[arg(long, help = "Public address of this node when its own is private (a cloud VM with a public IP): the overlay endpoint other nodes use")]
        node_external_ip: Option<std::net::IpAddr>,
        #[arg(long, value_parser = parse_duration, help = "Heartbeat grace for this node when longer than the cluster's (e.g. 15m for a laptop that sleeps)")]
        node_grace_period: Option<Duration>,
    },
    #[command(
        about = "Search and follow pod logs, including containers that are gone",
        help_template = HELP_TEMPLATE,
        after_help = LOGS_AFTER_HELP
    )]
    Logs(crate::obs::cli::LogsArgs),
    #[command(
        about = "List, silence and acknowledge the cluster's alerts",
        help_template = HELP_TEMPLATE,
        after_help = ALERTS_AFTER_HELP
    )]
    Alerts(crate::obs::cli::AlertsArgs),
    #[command(
        about = "Run a PromQL query against the cluster's built-in metrics",
        help_template = HELP_TEMPLATE,
        after_help = QUERY_AFTER_HELP
    )]
    Query(crate::obs::cli::QueryArgs),
    #[command(
        about = "Live CPU and memory of nodes and pods (metrics.k8s.io)",
        help_template = HELP_TEMPLATE,
        after_help = TOP_AFTER_HELP
    )]
    Top {
        #[arg(long, help = "Refresh the table continuously")]
        watch: bool,
        #[arg(long, default_value_t = 2, help = "Refresh interval in seconds")]
        interval_secs: u64,
        #[command(flatten)]
        kube: crate::obs::cli::TopArgs,
    },
    #[command(
        about = "A tree of the cluster: nodes, workloads down their owners, services",
        help_template = HELP_TEMPLATE,
        after_help = TREE_AFTER_HELP
    )]
    Tree {
        #[arg(long, help = "Refresh the tree continuously")]
        watch: bool,
        #[arg(long, default_value_t = 2, help = "Refresh interval in seconds")]
        interval_secs: u64,
        #[command(flatten)]
        kube: crate::tree::TreeArgs,
    },
    #[command(
        about = "List or remove the API servers of a highly available control plane",
        help_template = HELP_TEMPLATE,
        after_help = SERVER_AFTER_HELP
    )]
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    #[command(
        about = "A TCP load balancer in front of the API servers of an HA control plane",
        help_template = HELP_TEMPLATE,
        after_help = LB_AFTER_HELP
    )]
    Lb {
        #[arg(long, default_value = "127.0.0.1:6443", help = "Address to listen on")]
        listen: std::net::SocketAddr,
        #[arg(long = "server", required = true, value_name = "URL", help = "An API server (repeat for more; the others are learned from it)")]
        servers: Vec<String>,
        #[arg(long, help = "Check /readyz over HTTPS without verifying the certificate (a kube-apiserver with anonymous /readyz)")]
        https_check: bool,
        #[arg(long, conflicts_with = "https_check", help = "Consider a server ready when it accepts TCP connections (for servers whose /readyz needs credentials)")]
        tcp_check: bool,
    },
    #[command(hide = true, about = "Linearizability check of an HA control plane: a CRUD register workload, recorded and checked")]
    HaCheck {
        #[arg(long = "server", required = true, value_name = "HOST:PORT")]
        servers: Vec<String>,
        #[arg(long, default_value = "60s", value_parser = parse_duration)]
        duration: Duration,
        #[arg(long, default_value_t = 8)]
        clients: u32,
        #[arg(long, default_value_t = 6)]
        keys: u32,
        #[arg(long, default_value = "default")]
        namespace: String,
        #[arg(long, help = "Write the history (JSON) here")]
        out: Option<PathBuf>,
    },
    #[command(hide = true, about = "Write one ConfigMap repeatedly: write latency and unavailability windows (HA failover)")]
    HaProbe {
        #[arg(long, default_value = "127.0.0.1:6443", value_name = "HOST:PORT")]
        server: String,
        #[arg(long, default_value = "100ms", value_parser = parse_duration)]
        interval: Duration,
        #[arg(long, default_value = "60s", value_parser = parse_duration)]
        duration: Duration,
        #[arg(long, default_value = "default")]
        namespace: String,
        #[arg(long, help = "Write every sample (JSON) here")]
        out: Option<PathBuf>,
    },
    #[command(
        about = "Clusters of machines over SSH from a cluster file (apply, status, delete)",
        help_template = HELP_TEMPLATE,
        after_help = CLUSTER_AFTER_HELP
    )]
    Cluster {
        #[command(subcommand)]
        command: ClusterCommand,
    },
}

#[derive(Debug, Subcommand)]
enum InstallRole {
    #[command(about = "Install litek8s-server.service: `litek8s apiserver` with these flags (see `litek8s apiserver --help`)")]
    Server {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "APISERVER FLAGS")]
        args: Vec<String>,
    },
    #[command(about = "Install litek8s-agent.service: `litek8s agent --join URL --token T` with these flags (see `litek8s agent --help`)")]
    Agent {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "AGENT FLAGS")]
        args: Vec<String>,
    },
}

/// Parse the flags `litek8s install` writes into a unit as the service will
/// parse them, so a typo fails the install instead of the service.
pub(crate) fn parse_service_args(role: crate::install::Role, args: &[String]) -> Result<crate::install::ServiceArgs> {
    use crate::install::ServiceArgs;
    let sub = role.subcommand();
    let argv = ["litek8s", sub].into_iter().map(String::from).chain(args.iter().cloned());
    let cli = Cli::try_parse_from(argv).map_err(|e| anyhow!("invalid flags for `litek8s {sub}`:\n{}", e.render()))?;
    let crun = |r: crate::kube::agent::RuntimeChoice| !matches!(r, crate::kube::agent::RuntimeChoice::Docker);
    Ok(match cli.command {
        Command::Apiserver { listen, data_dir, kubeconfig, no_node, node_name, node_dir, runtime, node_ip, advertise_address, no_wireguard, cluster_init, server, raft_port, .. } => {
            ensure!(data_dir.is_some(), "a server installed as a service needs --data-dir");
            ServiceArgs {
                listen: Some(listen),
                raft_port,
                ha: cluster_init || server.is_some(),
                data_dir,
                node_dir,
                kubeconfig,
                run_node: !no_node,
                node_name,
                node_ip,
                advertise_address,
                crun: crun(runtime),
                wireguard: !no_wireguard,
                join: None,
            }
        }
        Command::Agent { join, node_name, node_ip, node_dir, runtime, no_wireguard, .. } => ServiceArgs {
            node_dir,
            run_node: true,
            node_name,
            node_ip,
            crun: crun(runtime),
            wireguard: !no_wireguard,
            join: Some(join),
            ..Default::default()
        },
        _ => bail!("`litek8s install {sub}`: not a service"),
    })
}

#[derive(Debug, Subcommand)]
enum ServerCommand {
    #[command(about = "List the API servers, the leader and how far behind each is")]
    List {
        #[arg(long, default_value = "http://127.0.0.1:6443", value_name = "URL", help = "Any API server of the cluster")]
        server: String,
    },
    #[command(about = "Remove an API server from the cluster (then stop it)")]
    Remove {
        #[arg(help = "Its name, as `litek8s server list` shows it")]
        name: String,
        #[arg(long, default_value = "https://127.0.0.1:6443", value_name = "URL", help = "Any API server of the cluster")]
        server: String,
        #[arg(long, env = "LITEK8S_TOKEN", hide_env_values = true, help = "The cluster's join token")]
        token: String,
    },
}

#[derive(Debug, Subcommand)]
enum ClusterCommand {
    #[command(about = "Install or update every machine of a cluster file over SSH; idempotent", help_template = HELP_TEMPLATE)]
    Apply {
        #[arg(short = 'f', long, value_name = "FILE", help = "The cluster file (kind: Cluster)")]
        file: PathBuf,
        #[arg(long, help = "Drain, uninstall and delete agents that are no longer in the file")]
        prune: bool,
        #[arg(long, requires = "prune", help = "With --prune: also delete the removed agents' data")]
        purge: bool,
        #[arg(long, value_name = "PATH", help = "Install this Linux litek8s binary (overrides the file's binary/release)")]
        binary: Option<PathBuf>,
        #[arg(long, value_name = "TAG", help = "Install this release (v0.2.0) instead of this litek8s's version")]
        release: Option<String>,
        #[arg(long, help = "Do not check the machines for crun, cgroup v2, WireGuard and free ports")]
        skip_checks: bool,
        #[arg(long, default_value = "3m", value_parser = parse_duration, help = "How long each server and node may take to be ready")]
        timeout: Duration,
    },
    #[command(about = "Show each machine of a cluster file: service, version, node, readiness, pods", help_template = HELP_TEMPLATE)]
    Status {
        #[arg(short = 'f', long, value_name = "FILE", help = "The cluster file (kind: Cluster)")]
        file: PathBuf,
    },
    #[command(about = "Uninstall litek8s from every machine of a cluster file", help_template = HELP_TEMPLATE)]
    Delete {
        #[arg(short = 'f', long, value_name = "FILE", help = "The cluster file (kind: Cluster)")]
        file: PathBuf,
        #[arg(long, help = "Go ahead (every pod of the cluster stops)")]
        yes: bool,
        #[arg(long, help = "Also delete the data directories on each machine")]
        purge: bool,
    },
}

/// `--version` with the build and the `$LITEK8S_HOME` in use.
fn command() -> clap::Command {
    Cli::command().version(Box::leak(crate::config::version_line().into_boxed_str()) as &'static str)
}

pub async fn run() -> Result<()> {
    let cli = Cli::from_arg_matches(&command().get_matches()).unwrap_or_else(|e| e.exit());

    match cli.command {
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
            node_external_ip,
            docker_socket,
            cluster_init,
            server,
            raft_port,
            obs,
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
                node_external_ip,
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
                cluster_init,
                join_server: server,
                raft_port,
                raft_listener: None,
                prometheus_listen: obs.prometheus_listen,
                obs: obs.config()?,
            })
            .await
        }
        Command::Install { skip_checks, timeout, role } => {
            let (role, mut args) = match role {
                InstallRole::Server { args } => (crate::install::Role::Server, args),
                InstallRole::Agent { args } => (crate::install::Role::Agent, args),
            };
            let flags = crate::remote::take_ssh_flags(&mut args)?;
            if flags.ssh.is_some() {
                return crate::remote::install_cmd(role, args, flags, skip_checks, timeout).await;
            }
            crate::install::install(crate::install::InstallOptions { role, args, skip_checks, timeout }).await
        }
        Command::Uninstall { purge, ssh: None, .. } => crate::install::uninstall(purge),
        Command::Uninstall { purge, ssh: Some(dest), identity_file, ssh_port, ssh_option } => {
            let ssh = crate::remote::Ssh { dest, port: ssh_port, identity_file, options: ssh_option };
            crate::remote::uninstall_cmd(ssh, purge).await
        }
        Command::Dev { command } => crate::dev::run(command).await,
        Command::Server { command } => match command {
            ServerCommand::List { server } => crate::kube::ha::lb::list(&server).await,
            ServerCommand::Remove { name, server, token } => crate::kube::ha::lb::remove(&server, &token, &name).await,
        },
        Command::Lb { listen, servers, https_check, tcp_check } => {
            use crate::kube::ha::lb::Check;
            let check = if https_check { Check::Https } else if tcp_check { Check::Tcp } else { Check::Http };
            crate::kube::ha::lb::run(listen, servers, check).await
        }
        Command::HaProbe { server, interval, duration, namespace, out } => {
            let samples = crate::kube::ha::lincheck::probe(&server, &namespace, interval, duration, Duration::from_secs(15)).await;
            if let Some(path) = &out {
                std::fs::write(path, serde_json::to_vec(&samples)?)?;
            }
            println!("{}", serde_json::to_string_pretty(&crate::kube::ha::lincheck::probe_report(&samples, 500.0))?);
            Ok(())
        }
        Command::HaCheck { servers, duration, clients, keys, namespace, out } => {
            let history = crate::kube::ha::lincheck::run(crate::kube::ha::lincheck::Workload {
                servers,
                clients,
                keys,
                duration,
                timeout: Duration::from_secs(5),
                namespace,
            })
            .await;
            if let Some(path) = &out {
                std::fs::write(path, serde_json::to_vec(&history)?)?;
            }
            let summary = crate::kube::ha::lincheck::summarize(&history);
            println!("{}", serde_json::to_string(&summary)?);
            let started = std::time::Instant::now();
            match crate::kube::ha::lincheck::check(&history) {
                Ok(()) => {
                    println!("linearizable ({} operations, checked in {:?})", history.len(), started.elapsed());
                    Ok(())
                }
                Err(e) => anyhow::bail!("NOT linearizable: {e}"),
            }
        }
        Command::Ui(args) => crate::ui::run(args).await,
        Command::Mcp(args) => crate::mcp::run(args).await,
        Command::Completion { shell } => {
            let mut script = Vec::new();
            clap_complete::generate(shell, &mut Cli::command(), "litek8s", &mut script);
            // `litek8s completion zsh | head` closes the pipe early: not an error.
            match io::stdout().write_all(&script) {
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
                r => r.context("writing the completion script"),
            }
        }
        Command::Skill { reference, list, install } => skill(reference, list, install),
        Command::Agent { join, token, node_name, node_ip, node_dir, runtime, wireguard_port, no_wireguard, node_external_ip, node_grace_period } => {
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
                external_ip: node_external_ip.map(|ip| ip.to_string()),
            };
            let agent = crate::kube::agent::node_agent_with(runtime, Some(node_name), Some(node_dir), network, node_grace_period).await?;
            crate::kube::metrics::spawn_memory_trim();
            crate::kube::agent::run_remote(agent, client).await
        }
        Command::Alerts(args) => crate::obs::cli::alerts(args).await,
        Command::Query(args) => crate::obs::cli::query(args).await,
        Command::Logs(args) => crate::obs::cli::logs(args).await,
        Command::Top { watch, interval_secs, kube } => run_kube_top(kube, watch, Duration::from_secs(interval_secs)).await,
        Command::Tree { watch, interval_secs, kube } => run_kube_tree(kube, watch, Duration::from_secs(interval_secs)).await,
        Command::Cluster { command } => match command {
            ClusterCommand::Apply { file, prune, purge, binary, release, skip_checks, timeout } => {
                let f = crate::cluster::ClusterFile::load(&file)?;
                crate::cluster::apply(&f, &crate::cluster::ApplyOptions { prune, purge, binary, release, skip_checks, timeout: Some(timeout) }).await
            }
            ClusterCommand::Status { file } => crate::cluster::status(&crate::cluster::ClusterFile::load(&file)?).await,
            ClusterCommand::Delete { file, yes, purge } => crate::cluster::delete(&crate::cluster::ClusterFile::load(&file)?, yes, purge).await,
        },
    }
}

/// `litek8s top` of a Kubernetes cluster: live on a terminal (as `top`),
/// once when piped or with `--once`.
async fn run_kube_top(args: crate::obs::cli::TopArgs, watch: bool, interval: Duration) -> Result<()> {
    let ep = crate::obs::cli::endpoint(args.context.as_deref())?;
    if !(watch || (io::stdout().is_terminal() && !args.once)) {
        print!("{}", crate::obs::cli::top_frame(&ep, &args).await?);
        return Ok(());
    }
    let banner = format!("{}  {}", build_watch_banner("top", interval), muted(&ep.display));
    let mut display = WatchDisplay::new(banner)?;
    // One listener for the whole loop: a ^C during a fetch is not lost.
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        let body = tokio::select! {
            frame = crate::obs::cli::top_frame(&ep, &args) => frame.unwrap_or_else(|e| format_error_body(&format!("{e:#}"))),
            _ = &mut ctrl_c => return Ok(()),
        };
        display.render_frame(&body)?;
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = &mut ctrl_c => return Ok(()),
        }
    }
}

async fn run_kube_tree(args: crate::tree::TreeArgs, watch: bool, interval: Duration) -> Result<()> {
    let ep = crate::obs::cli::endpoint(args.context.as_deref())?;
    if !watch || args.once {
        print!("{}", crate::tree::frame(&ep, &args).await?);
        return Ok(());
    }
    let banner = format!("{}  {}", build_watch_banner("tree", interval), muted(&ep.display));
    let mut display = WatchDisplay::new(banner)?;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        let body = tokio::select! {
            frame = crate::tree::frame(&ep, &args) => frame.unwrap_or_else(|e| format_error_body(&format!("{e:#}"))),
            _ = &mut ctrl_c => return Ok(()),
        };
        display.render_frame(&body)?;
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = &mut ctrl_c => return Ok(()),
        }
    }
}

fn format_error_body(message: &str) -> String {
    format!("{}\n\n{}", title("Error"), message)
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

fn muted(text: &str) -> String {
    paint(text, "2")
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

fn build_watch_banner(kind: &str, interval: Duration) -> String {
    format!("litek8s {kind}  live  interval={}s  ctrl+c to exit", interval.as_secs())
}

impl Drop for WatchDisplay {
    fn drop(&mut self) {
        if self.interactive {
            print!("\x1B[?25h\x1B[?1049l");
            let _ = io::stdout().flush();
        }
    }
}

/// `litek8s skill`: the agent skill compiled into this binary.
fn skill(reference: Option<String>, list: bool, install: Option<PathBuf>) -> Result<()> {
    use crate::mcp::resources::{SKILL, skill_file};
    if list {
        for d in SKILL {
            println!("{:<34} {}", d.path, d.description);
        }
        return Ok(());
    }
    if let Some(dir) = install {
        let root = dir.join("litek8s");
        for d in SKILL {
            let p = root.join(d.path);
            std::fs::create_dir_all(p.parent().unwrap_or(&root)).with_context(|| format!("cannot create {}", root.display()))?;
            std::fs::write(&p, d.text).with_context(|| format!("cannot write {}", p.display()))?;
        }
        println!("installed the litek8s skill in {}", root.display());
        return Ok(());
    }
    let name = reference.unwrap_or_else(|| "SKILL.md".into());
    let doc = skill_file(&name).with_context(|| format!("no skill file {name:?} (litek8s skill --list)"))?;
    print!("{}", doc.text);
    Ok(())
}

#[cfg(test)]
mod main_help_tests {
    use clap::CommandFactory;

    use super::{Cli, MAIN_HELP_TEMPLATE};

    /// `(name, description)` of each command line in the grouped list.
    fn listed() -> Vec<(String, String)> {
        let commands = MAIN_HELP_TEMPLATE.split("{usage}").nth(1).unwrap().split("Options:").next().unwrap();
        commands
            .lines()
            .filter(|l| l.starts_with("  "))
            .map(|l| {
                let (name, about) = l.trim().split_once(' ').unwrap();
                (name.to_string(), about.trim().to_string())
            })
            .collect()
    }

    #[test]
    fn main_help_lists_every_command() {
        let mut cmd = Cli::command();
        cmd.build();
        let mut visible: Vec<(String, String)> = cmd
            .get_subcommands()
            .filter(|c| !c.is_hide_set())
            .map(|c| (c.get_name().to_string(), c.get_about().map(|a| a.to_string()).unwrap_or_default()))
            .collect();
        let mut listed = listed();
        visible.sort();
        listed.sort();
        assert_eq!(listed, visible, "the grouped list in MAIN_HELP_TEMPLATE must match the visible subcommands and their about");
    }

    #[test]
    fn main_help_renders_groups_and_options() {
        let help = Cli::command().render_help().to_string();
        for heading in ["Kubernetes:", "Observability", "Agents:", "Options:", "Quickstart:", "--version"] {
            assert!(help.contains(heading), "{heading} missing from:\n{help}");
        }
        for stale in ["Tiny k8s-like", "Legacy daemon", "legacy daemon", "--socket-path", "Distributed compute"] {
            assert!(!help.contains(stale), "stale {stale:?} in:\n{help}");
        }
    }

    #[test]
    fn quickstart_commands_parse() {
        let quickstart = super::MAIN_AFTER_HELP.split("Quickstart:").nth(1).unwrap();
        let mut checked = 0;
        for line in quickstart.lines().map(str::trim).filter(|l| l.starts_with("litek8s ")) {
            let mut args: Vec<String> = Vec::new();
            let (cmd, rest) = match line.split_once('\'') {
                Some((before, quoted)) => (before, Some(quoted.trim_end_matches('\'').split('\'').next().unwrap())),
                None => (line, None),
            };
            // Drop the trailing comment (two or more spaces, then prose).
            let cmd = cmd.split("  ").next().unwrap();
            args.extend(cmd.split_whitespace().map(String::from));
            if let Some(q) = rest {
                args.push(q.to_string());
            }
            Cli::try_parse_from_args(&args).unwrap_or_else(|e| panic!("{line}: {e}"));
            checked += 1;
        }
        assert!(checked >= 4, "quickstart has litek8s commands");
    }

    impl Cli {
        fn try_parse_from_args(args: &[String]) -> Result<Self, clap::Error> {
            <Self as clap::Parser>::try_parse_from(args)
        }
    }

    #[test]
    fn node_agent_server_flag_parses() {
        let cli = <Cli as clap::Parser>::try_parse_from(["litek8s", "node-agent", "--server", "http://127.0.0.1:6443", "--token", "t", "--node-name", "n2"]).unwrap();
        assert!(matches!(cli.command, super::Command::Agent { ref join, .. } if join == "http://127.0.0.1:6443"));
        let cli = <Cli as clap::Parser>::try_parse_from(["litek8s", "lb", "--server", "http://a:6443", "--server", "http://b:6443"]).unwrap();
        assert!(matches!(cli.command, super::Command::Lb { ref servers, .. } if servers.len() == 2));
        let cli = <Cli as clap::Parser>::try_parse_from(["litek8s", "agent", "--join", "https://10.0.0.1:6443", "--token", "t"]).unwrap();
        assert!(matches!(cli.command, super::Command::Agent { .. }));
        // The legacy daemon's commands and global flags are gone.
        for args in [&["litek8s", "agent"][..], &["litek8s", "serve"], &["litek8s", "get", "deployments"], &["litek8s", "--server", "127.0.0.1:6443", "top"], &["litek8s", "--socket-path", "/tmp/s", "tree"], &["litek8s", "tensor", "multiply"]] {
            assert!(<Cli as clap::Parser>::try_parse_from(args).is_err(), "{args:?} still parses");
        }
    }

    #[test]
    fn completion_scripts_cover_every_shell() {
        use clap::{CommandFactory, ValueEnum};
        for shell in clap_complete::Shell::value_variants() {
            let mut out = Vec::new();
            clap_complete::generate(*shell, &mut Cli::command(), "litek8s", &mut out);
            let script = String::from_utf8(out).unwrap();
            assert!(script.contains("litek8s") && script.contains("dev"), "{shell}: {script}");
        }
        assert!(<Cli as clap::Parser>::try_parse_from(["litek8s", "completion", "zsh"]).is_ok());
    }

    #[test]
    fn no_arguments_prints_the_grouped_help() {
        let err = <Cli as clap::Parser>::try_parse_from(["litek8s"]).unwrap_err();
        let text = err.render().to_string();
        assert!(text.contains("Kubernetes:") && text.contains("dev ") && text.contains("Quickstart:"), "{text}");
    }
}

#[cfg(test)]
mod tests {
    use super::{build_watch_frame_lines, render_watch_frame_patch};

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

}
