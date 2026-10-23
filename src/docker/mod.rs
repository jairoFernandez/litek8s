//! A Docker Engine API on litek8s's crun node runtime, so the real `docker`
//! CLI, `docker compose`, Dev Containers and Testcontainers work against
//! litek8s (`DOCKER_HOST=unix://$HOME/.litek8s/docker.sock`).
//!
//! It runs inside the dev cluster (`litek8s dev`): the Linux sandbox or the
//! macOS VM, next to the API server and the pod node agent, and shares the
//! node's image store with pods. See docs/docker.md.

pub(crate) mod api;
pub(crate) mod attach;
pub mod budget;
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

/// The Docker socket on the developer's machine: `$LITEK8S_DOCKER_SOCKET`,
/// else `~/.litek8s/docker.sock` (`$LITEK8S_HOME/docker.sock`).
pub fn host_socket_path() -> PathBuf {
    crate::config::paths().docker_socket
}

/// Where the engine also listens on the node (inside the sandbox/VM for dev
/// clusters): what containers get when they mount the Docker socket. It is
/// alone in its directory so that pods mount the directory (hostPath): a
/// restart replaces the socket file, and a mount of the file itself keeps
/// the old, dead one until the pod restarts.
pub const INTERNAL_SOCKET_DIR: &str = "/run/litek8s/docker-api";
pub const INTERNAL_SOCKET: &str = "/run/litek8s/docker-api/docker.sock";

/// The socket's former path, still served for pods that mount it as a file
/// (they lose it when the server restarts).
pub const LEGACY_SOCKET: &str = "/run/litek8s/docker.sock";

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
    // Only the socket file is replaced: the directory (maybe created first
    // by a pod's DirectoryOrCreate hostPath) stays, so mounts of it see the
    // new socket.
    std::fs::create_dir_all(INTERNAL_SOCKET_DIR)?;
    for path in [INTERNAL_SOCKET, LEGACY_SOCKET] {
        let _ = std::fs::remove_file(path);
        let l = tokio::net::UnixListener::bind(path).with_context(|| format!("failed to listen on {path}"))?;
        tokio::spawn(api::serve_unix(engine.clone(), l));
    }
    if let Some(addr) = settings.tcp {
        let l = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("failed to listen on {addr}"))?;
        tokio::spawn(api::serve_tcp(engine.clone(), l));
    }
    Ok(engine)
}
