use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
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
    #[command(hide = true)]
    Agent,
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

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let endpoint = if let Some(server) = cli.server {
        DaemonEndpoint::Tcp(server)
    } else {
        DaemonEndpoint::Unix(cli.socket_path.unwrap_or_else(config::default_socket_path))
    };

    match cli.command {
        Command::Serve {
            state_file,
            poll_interval_secs,
            peer_id,
            peer_addr,
            join,
            api_addr,
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
                    let summary =
                        node::install_remote_agent(&ssh, &remote_binary, &project_root).await?;
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
                    let summary =
                        node::install_remote_agent(&ssh, &remote_binary, &project_root).await?;
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
        },
        Command::Agent => crate::node::run_agent_stdio().await,
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
            } => {
                run_vpn_join(
                    &token,
                    ssh_hub.as_deref(),
                    &node_name,
                    &remote_binary,
                )
                .await
            }
            VpnCommand::Complete { ip } => run_vpn_complete(&ip),
        },
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
    println!("  CIDR         : {}/{}", network.cidr_base, network.cidr_prefix);
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
    use crate::vpn::{HubState, SpokeState, load_or_generate_keypair, write_wg_quick_config,
        write_wg_quick_hub_config};

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
    let ssh = SshNodeClient::new(ssh_target.to_string(), remote_binary.to_string());
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
                "{} {} {} {} {}",
                header_left("NAME", 24),
                header_left("NODE", 14),
                header_left("STATUS", 12),
                header_left("IMAGE", 24),
                label("CONTAINER")
            );
            for item in items {
                println!(
                    "{} {} {} {} {}",
                    left_cell(&item.name, 24),
                    left_cell(&truncate(&item.node_name, 14), 14),
                    status_cell(&item.status, 12),
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
                "{} {} {} {} {} {} {}",
                header_left("NAME", 18),
                header_left("TYPE", 8),
                header_left("STATUS", 10),
                header_left("SCHED", 12),
                header_left("ENDPOINT", 24),
                header_left("VPN-IP", 14),
                label("DEPLOYMENTS")
            );
            for item in items {
                let vpn_cell = item.vpn_ip.clone().unwrap_or_else(|| "-".to_string());
                println!(
                    "{} {} {} {} {} {} {}",
                    left_cell(&item.name, 18),
                    left_cell(&item.transport, 8),
                    status_cell(&item.status, 10),
                    scheduling_cell(&item.scheduling, 12),
                    left_cell(&truncate(&item.endpoint, 24), 24),
                    left_cell(&vpn_cell, 14),
                    item.deployment_count
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
            print_detail("image", &item.image);
            print_detail("container", &item.container_name);
            print_detail(
                "container id",
                &item.container_id.unwrap_or_else(|| "-".to_string()),
            );
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
            print_detail("host port", &item.host_port.to_string());
            Ok(())
        }
        Response::Top { daemon, items } => {
            print!("{}", format_top_body(&daemon, &items));
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
                Response::Top { daemon, items } => format_top_body(&daemon, &items),
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
        Response::Top { daemon, items } => (daemon, items),
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

fn format_top_body(
    daemon: &protocol::DaemonTopSummary,
    items: &[protocol::DeploymentTopRow],
) -> String {
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
        format!(
            "{} {} {} {} {} {} {} {} {} {} {} {}",
            header_left("NAME", 24),
            header_left("NODE", 14),
            header_left("STATUS", 12),
            header_right("RESTARTS", 8),
            header_right("STREAK", 6),
            header_left("NEXT RETRY", 10),
            header_left("LAST EXIT", 16),
            header_right("CPU", 8),
            header_right("MEMORY", 14),
            header_right("NET I/O", 18),
            header_left("CONTAINER ID", 12),
            label("IMAGE")
        ),
    ];

    for item in items {
        lines.push(format!(
            "{} {} {} {} {} {} {} {} {} {} {} {}",
            left_cell(&item.name, 24),
            left_cell(&truncate(&item.node_name, 14), 14),
            status_cell(&item.status, 12),
            right_cell(&item.restart_count.to_string(), 8),
            right_cell(&item.restart_streak.to_string(), 6),
            left_cell(&format_retry_countdown(item.next_retry_in_secs), 10),
            left_cell(&truncate(item.last_exit.as_deref().unwrap_or("-"), 16), 16),
            right_cell(
                &format_top_cpu(
                    item.cpu_percent,
                    item.memory_usage_bytes.is_some()
                        || item.network_rx_bytes.is_some()
                        || item.network_tx_bytes.is_some(),
                ),
                8,
            ),
            right_cell(
                &format_memory(item.memory_usage_bytes, item.memory_limit_bytes),
                14,
            ),
            right_cell(
                &format_net_io(item.network_rx_bytes, item.network_tx_bytes),
                18,
            ),
            left_cell(&short_id(item.container_id.as_deref()), 12),
            truncate(&item.image, 32)
        ));
    }

    lines.join("\n")
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
    print_detail(
        "upstream",
        &format!("127.0.0.1:{} ({})", item.host_port, item.protocol),
    );
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
        TreeSnapshot, build_watch_frame_lines, format_retry_countdown, format_top_cpu,
        format_uptime, render_tree, render_watch_frame_patch,
    };

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
