use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};

use crate::config;
use crate::daemon::{self, ServerConfig};
use crate::protocol::{self, Request, Response};

#[derive(Debug, Parser)]
#[command(
    name = "litek8s",
    about = "A tiny k8s-like orchestrator backed by Docker"
)]
struct Cli {
    #[arg(long, global = true)]
    socket_path: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve {
        #[arg(long)]
        state_file: Option<PathBuf>,
        #[arg(long, default_value_t = 2)]
        poll_interval_secs: u64,
    },
    Apply {
        #[arg(short = 'f', long)]
        file: PathBuf,
    },
    Get {
        resource: String,
    },
    Describe {
        resource: String,
        name: String,
    },
    Logs {
        name: String,
        #[arg(long, default_value_t = 100)]
        tail: usize,
    },
    Top {
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 2)]
        interval_secs: u64,
    },
    Delete {
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,
        resource: Option<String>,
        name: Option<String>,
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
            ensure_resource(&resource, &["deployment", "deployments"])?;
            let response = protocol::request(&socket_path, Request::GetDeployments).await?;
            print_response(response)
        }
        Command::Describe { resource, name } => {
            ensure_resource(&resource, &["deployment", "deployments"])?;
            let response =
                protocol::request(&socket_path, Request::DescribeDeployment { name }).await?;
            print_response(response)
        }
        Command::Logs { name, tail } => {
            let response = protocol::request(&socket_path, Request::Logs { name, tail }).await?;
            print_response(response)
        }
        Command::Top {
            watch,
            interval_secs,
        } => run_top(&socket_path, watch, Duration::from_secs(interval_secs)).await,
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
    }
}

fn read_manifest(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))
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
            println!("{message}");
            Ok(())
        }
        Response::Deployments { items } => {
            println!("{:<24} {:<12} {:<24} CONTAINER", "NAME", "STATUS", "IMAGE");
            for item in items {
                println!(
                    "{:<24} {:<12} {:<24} {}",
                    item.name,
                    item.status,
                    truncate(&item.image, 24),
                    item.container_id.unwrap_or_else(|| "-".to_string())
                );
            }
            Ok(())
        }
        Response::Description { item } => {
            println!("Name: {}", item.name);
            println!("API Version: {}", item.api_version);
            println!("Status: {}", item.status);
            println!("Image: {}", item.image);
            println!("Container Name: {}", item.container_name);
            println!(
                "Container ID: {}",
                item.container_id.unwrap_or_else(|| "-".to_string())
            );
            if let Some(message) = item.message {
                println!("Message: {message}");
            }
            println!(
                "Command: {}",
                if item.command.is_empty() {
                    "-".to_string()
                } else {
                    item.command.join(" ")
                }
            );
            println!(
                "Args: {}",
                if item.args.is_empty() {
                    "-".to_string()
                } else {
                    item.args.join(" ")
                }
            );
            println!(
                "CPU Limit: {}",
                item.cpu_limit.unwrap_or_else(|| "-".to_string())
            );
            println!(
                "Memory Limit: {}",
                item.memory_limit.unwrap_or_else(|| "-".to_string())
            );
            println!(
                "Ports: {}",
                if item.ports.is_empty() {
                    "-".to_string()
                } else {
                    item.ports.join(", ")
                }
            );
            Ok(())
        }
        Response::Logs { content } => {
            print!("{content}");
            Ok(())
        }
        Response::Top { daemon, items } => {
            println!(
                "daemon litek8s: pid={} cpu={} mem={} uptime={}",
                daemon.pid,
                format_cpu_compact(daemon.cpu_percent),
                human_bytes(daemon.memory_bytes),
                format_uptime(daemon.uptime_secs),
            );
            println!();
            println!(
                "{:<24} {:<12} {:>8} {:>14} {:>18} {:<12} IMAGE",
                "NAME", "STATUS", "CPU", "MEMORY", "NET I/O", "CONTAINER"
            );
            for item in items {
                println!(
                    "{:<24} {:<12} {:>8} {:>14} {:>18} {:<12} {}",
                    item.name,
                    item.status,
                    format_cpu(item.cpu_percent),
                    format_memory(item.memory_usage_bytes, item.memory_limit_bytes),
                    format_net_io(item.network_rx_bytes, item.network_tx_bytes),
                    short_id(item.container_id.as_deref()),
                    truncate(&item.image, 32)
                );
            }
            Ok(())
        }
        Response::Error { message } => Err(anyhow!(message)),
    }
}

async fn run_top(socket_path: &Path, watch: bool, interval: Duration) -> Result<()> {
    loop {
        let response = protocol::request(socket_path, Request::Top).await?;
        if watch {
            print!("\x1B[2J\x1B[H");
            println!(
                "litek8s top  interval={}s  ctrl+c to exit\n",
                interval.as_secs()
            );
        }
        print_response(response)?;

        if !watch {
            return Ok(());
        }

        tokio::time::sleep(interval).await;
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

fn short_id(value: Option<&str>) -> String {
    value
        .map(|id| id.chars().take(12).collect::<String>())
        .unwrap_or_else(|| "-".to_string())
}

fn format_cpu(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:>6.2}%"))
        .unwrap_or_else(|| "   n/a".to_string())
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

#[cfg(test)]
mod tests {
    use super::format_uptime;

    #[test]
    fn formats_uptime_compactly() {
        assert_eq!(format_uptime(9), "9s");
        assert_eq!(format_uptime(75), "1m15s");
        assert_eq!(format_uptime(3723), "1h02m03s");
    }
}
