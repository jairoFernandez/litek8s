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
        Response::Error { message } => Err(anyhow!(message)),
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
