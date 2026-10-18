//! A Docker Engine API on litek8s's crun node runtime, so the real `docker`
//! CLI, `docker compose`, Dev Containers and Testcontainers work against
//! litek8s (`DOCKER_HOST=unix://$HOME/.litek8s/docker.sock`).
//!
//! It runs inside the dev cluster (`litek8s dev`): the Linux sandbox or the
//! macOS VM, next to the API server and the pod node agent, and shares the
//! node's image store with pods. See docs/docker.md.

pub(crate) mod api;
pub(crate) mod attach;
pub(crate) mod build;
pub(crate) mod engine;
pub(crate) mod error;
pub(crate) mod exec;
pub(crate) mod filters;
pub mod fs;
pub(crate) mod images;
pub(crate) mod model;
pub mod network;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

pub(crate) use engine::{Engine, EngineOptions};
pub use network::Publisher;

/// The Docker socket on the developer's machine.
pub fn host_socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("LITEK8S_DOCKER_SOCKET") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".litek8s").join("docker.sock")
}

/// Where the engine also listens inside the sandbox/VM (what containers get
/// when they mount the Docker socket).
pub const INTERNAL_SOCKET: &str = "/run/litek8s/docker.sock";

/// TCP port of the API on the VM's loopback (the Mac forwards its socket
/// there over vsock).
pub const GUEST_PORT: u16 = 2375;

/// How the engine is reached and publishes ports.
pub struct Settings {
    /// Data directory (containers, networks, volumes).
    pub root: PathBuf,
    /// An already bound listener for clients (the dev supervisor binds the
    /// host socket before the sandbox starts).
    pub listener: Option<std::os::unix::net::UnixListener>,
    /// Also serve on this TCP address (the VM).
    pub tcp: Option<std::net::SocketAddr>,
    pub publisher: Publisher,
    /// Paths that mean "the Docker socket" in bind mounts.
    pub socket_aliases: Vec<String>,
}

/// Start the engine and serve the API; returns once listening.
pub(crate) async fn start(settings: Settings, images: Arc<crate::kube::agent::crun::image::ImageStore>) -> Result<Arc<Engine>> {
    let engine = Engine::start(EngineOptions {
        root: settings.root,
        images,
        publisher: settings.publisher,
        socket_aliases: settings.socket_aliases,
        internal_socket: PathBuf::from(INTERNAL_SOCKET),
    })
    .await?;
    if let Some(l) = settings.listener {
        l.set_nonblocking(true)?;
        let l = tokio::net::UnixListener::from_std(l)?;
        tokio::spawn(api::serve_unix(engine.clone(), l));
    }
    let _ = std::fs::remove_file(INTERNAL_SOCKET);
    if let Some(parent) = std::path::Path::new(INTERNAL_SOCKET).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let internal = tokio::net::UnixListener::bind(INTERNAL_SOCKET).with_context(|| format!("failed to listen on {INTERNAL_SOCKET}"))?;
    tokio::spawn(api::serve_unix(engine.clone(), internal));
    if let Some(addr) = settings.tcp {
        let l = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("failed to listen on {addr}"))?;
        tokio::spawn(api::serve_tcp(engine.clone(), l));
    }
    Ok(engine)
}
