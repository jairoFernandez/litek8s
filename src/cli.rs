use std::collections::BTreeMap;
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
  litek8s serve --state-file /tmp/litek8s-state.json";

const APPLY_AFTER_HELP: &str = "\
Examples:
  litek8s apply -f examples/nginx.yaml
  litek8s apply -f ./my-app.yaml";

const TOP_AFTER_HELP: &str = "\
Examples:
  litek8s top
  litek8s top --watch
  litek8s top --watch --interval-secs 1";

const GET_AFTER_HELP: &str = "\
Examples:
  litek8s get deployments
  litek8s get nodes";

const DESCRIBE_AFTER_HELP: &str = "\
Examples:
  litek8s describe deployment demo-nginx
  litek8s describe deployments demo-nginx";

const LOGS_AFTER_HELP: &str = "\
Examples:
  litek8s logs demo-nginx
  litek8s logs demo-nginx --tail 200";

const EVENTS_AFTER_HELP: &str = "\
Examples:
  litek8s events
  litek8s events --watch
  litek8s events --deployment echo-edge
  litek8s events --node server77 --watch";

const DELETE_AFTER_HELP: &str = "\
Examples:
  litek8s delete deployment demo-nginx
  litek8s delete -f examples/nginx.yaml";

const NODE_AFTER_HELP: &str = "\
Examples:
  litek8s node add server77 --ssh server77
  litek8s node add server77 --ssh server77 --install
  litek8s node add server77 --ssh server77 --label role=edge --label zone=lab
  litek8s node cordon server77
  litek8s node uncordon server77
  litek8s node add edge-1 --ssh ubuntu@10.0.0.7 --remote-binary ~/.local/bin/litek8s";

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
    },
    #[command(
        about = "Apply a reduced Deployment manifest",
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
        #[arg(help = "Resource type, currently `deployment`, `deployments`, `node`, or `nodes`")]
        resource: String,
    },
    #[command(
        about = "Show a single deployment in detail",
        help_template = HELP_TEMPLATE,
        after_help = DESCRIBE_AFTER_HELP
    )]
    Describe {
        #[arg(help = "Resource type, currently `deployment`, `deployments`, `node`, or `nodes`")]
        resource: String,
        #[arg(help = "Deployment name")]
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
        about = "Register a remote node reachable over ssh",
        help_template = HELP_TEMPLATE,
        after_help = NODE_AFTER_HELP
    )]
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },
    #[command(
        about = "Delete a deployment by name or manifest",
        help_template = HELP_TEMPLATE,
        after_help = DELETE_AFTER_HELP
    )]
    Delete {
        #[arg(
            short = 'f',
            long,
            help = "Delete the deployment defined in this manifest"
        )]
        file: Option<PathBuf>,
        #[arg(help = "Resource type when deleting by name")]
        resource: Option<String>,
        #[arg(help = "Deployment name when deleting by name")]
        name: Option<String>,
    },
    #[command(hide = true)]
    Agent,
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
    },
    #[command(about = "Mark a node unschedulable for new selector-based or default placements")]
    Cordon {
        #[arg(help = "Logical node name")]
        name: String,
    },
    #[command(about = "Re-open a node for scheduling")]
    Uncordon {
        #[arg(help = "Logical node name")]
        name: String,
    },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let socket_path = cli.socket_path.unwrap_or_else(config::default_socket_path);

    match cli.command {
        Command::Serve {
            state_file,
            poll_interval_secs,
        } => {
            let config = ServerConfig {
                socket_path,
                state_file: match state_file {
                    Some(path) => path,
                    None => config::default_state_file()?,
                },
                poll_interval: Duration::from_secs(poll_interval_secs),
                cleanup_orphans: true,
            };
            daemon::serve(config).await
        }
        Command::Apply { file } => {
            let manifest = read_manifest(&file)?;
            let response = protocol::request(&socket_path, Request::Apply { manifest }).await?;
            print_response(response)
        }
        Command::Get { resource } => {
            let response = match resource.as_str() {
                "deployment" | "deployments" => {
                    protocol::request(&socket_path, Request::GetDeployments).await?
                }
                "node" | "nodes" => protocol::request(&socket_path, Request::GetNodes).await?,
                _ => {
                    ensure_resource(&resource, &["deployment", "deployments", "node", "nodes"])?;
                    unreachable!()
                }
            };
            print_response(response)
        }
        Command::Describe { resource, name } => {
            let response = match resource.as_str() {
                "deployment" | "deployments" => {
                    protocol::request(&socket_path, Request::DescribeDeployment { name }).await?
                }
                "node" | "nodes" => {
                    protocol::request(&socket_path, Request::DescribeNode { name }).await?
                }
                _ => {
                    ensure_resource(&resource, &["deployment", "deployments", "node", "nodes"])?;
                    unreachable!()
                }
            };
            print_response(response)
        }
        Command::Logs { name, tail } => {
            let response = protocol::request(&socket_path, Request::Logs { name, tail }).await?;
            print_response(response)
        }
        Command::Events {
            watch,
            limit,
            deployment,
            node,
            interval_secs,
        } => {
            run_events(
                &socket_path,
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
        } => run_top(&socket_path, watch, Duration::from_secs(interval_secs)).await,
        Command::Node { command } => match command {
            NodeCommand::Add {
                name,
                ssh,
                install,
                labels,
                remote_binary,
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
                let response = protocol::request(
                    &socket_path,
                    Request::AddNode {
                        name,
                        ssh_target: ssh,
                        remote_binary: Some(remote_binary),
                        labels,
                    },
                )
                .await?;
                print_response(response)
            }
            NodeCommand::Cordon { name } => {
                let response = protocol::request(
                    &socket_path,
                    Request::SetNodeScheduling {
                        name,
                        unschedulable: true,
                    },
                )
                .await?;
                print_response(response)
            }
            NodeCommand::Uncordon { name } => {
                let response = protocol::request(
                    &socket_path,
                    Request::SetNodeScheduling {
                        name,
                        unschedulable: false,
                    },
                )
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
                ensure_resource(&resource, &["deployment", "deployments"])?;
                let name = name.context("deployment name is required")?;
                Request::DeleteDeployment { name }
            };

            let response = protocol::request(&socket_path, request).await?;
            print_response(response)
        }
        Command::Agent => crate::node::run_agent_stdio().await,
    }
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
        Response::Nodes { items } => {
            let total = items.len();
            println!(
                "{} {}",
                title("Cluster Nodes"),
                muted(&format!("({total} total)"))
            );
            println!();
            println!(
                "{} {} {} {} {} {}",
                header_left("NAME", 18),
                header_left("TYPE", 8),
                header_left("STATUS", 10),
                header_left("SCHED", 12),
                header_left("ENDPOINT", 24),
                label("DEPLOYMENTS")
            );
            for item in items {
                println!(
                    "{} {} {} {} {} {}",
                    left_cell(&item.name, 18),
                    left_cell(&item.transport, 8),
                    status_cell(&item.status, 10),
                    scheduling_cell(&item.scheduling, 12),
                    left_cell(&truncate(&item.endpoint, 24), 24),
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
        Response::Top { daemon, items } => {
            println!("{} {}", title("Daemon"), muted("local control loop"));
            println!(
                "{} pid={} cpu={} mem={} uptime={}",
                label("litek8s"),
                daemon.pid,
                format_cpu_compact(daemon.cpu_percent),
                human_bytes(daemon.memory_bytes),
                format_uptime(daemon.uptime_secs),
            );
            println!();
            println!(
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
            );
            for item in items {
                println!(
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
                );
            }
            Ok(())
        }
        Response::Error { message } => Err(anyhow!(message)),
    }
}

async fn run_top(socket_path: &Path, watch: bool, interval: Duration) -> Result<()> {
    let mut watch_display = watch
        .then(|| WatchDisplay::new(build_watch_banner("top", interval, None, None)))
        .transpose()?;

    loop {
        let response = protocol::request(socket_path, Request::Top).await?;
        if let Some(display) = watch_display.as_mut() {
            display.begin_frame(interval)?;
        }
        print_response(response)?;
        io::stdout().flush().context("failed to flush top output")?;

        if !watch {
            return Ok(());
        }

        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

async fn run_events(
    socket_path: &Path,
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
        let response = protocol::request(
            socket_path,
            Request::GetEvents {
                limit,
                since_id: None,
                deployment_name: deployment.clone(),
                node_name: node.clone(),
            },
        )
        .await?;
        if let Some(display) = watch_display.as_mut() {
            display.begin_frame(interval)?;
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
        "Running" | "Ready" | "Normal" => paint(status, "1;92"),
        "Pending" | "Restarting" | "Warning" | "Cordoned" => paint(status, "1;93"),
        "Failed" | "Deleted" | "Offline" => paint(status, "1;91"),
        _ => status.to_string(),
    }
}

fn status_cell(status: &str, width: usize) -> String {
    let padded = left_cell(status, width);
    match status {
        "Running" | "Ready" | "Normal" => paint(&padded, "1;92"),
        "Pending" | "Restarting" | "Warning" | "Cordoned" => paint(&padded, "1;93"),
        "Failed" | "Deleted" | "Offline" => paint(&padded, "1;91"),
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
        })
    }

    fn begin_frame(&mut self, _interval: Duration) -> Result<()> {
        if self.interactive {
            print!("\x1B[H\x1B[J");
        } else {
            println!();
            println!("---");
        }

        println!("{}\n", self.banner);
        Ok(())
    }
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
    use super::{format_retry_countdown, format_top_cpu, format_uptime};

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
}
