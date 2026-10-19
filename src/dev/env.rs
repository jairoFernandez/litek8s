//! The environment local clusters share: one VM on macOS, one sandbox on
//! Linux, started with the first cluster and stopped with the last.
//!
//! Each cluster is a "cell" in it (its own network, mount, PID, cgroup,
//! UTS and IPC namespaces; see [`super::cells`]), so a second cluster costs
//! its own processes and nothing else: no second kernel, no second copy of
//! the VM's page cache, no second userspace network stack.
//!
//! The process that owns the environment (`litek8s dev supervise` on Linux,
//! `litek8s dev-vm` on macOS) records itself in `env.json` and takes
//! requests on a Unix socket next to it (`env.sock`), one JSON line each:
//! start, stop or purge a cluster, report memory, shut down.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// What the running environment recorded about itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvState {
    /// The supervisor (Linux) or VM process (macOS).
    pub pid: u32,
    /// `rootless`, `root` or `vm`.
    pub mode: String,
    pub started_unix_ms: u64,
    pub control: PathBuf,
    /// The cgroup of the whole sandbox (Linux).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shares: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    /// Start a cluster (`port`: the API port wanted, else a free one).
    Start { name: String, port: Option<u16>, nodes: u32 },
    Stop { name: String },
    /// Stop (`up: false`) or start again one node of a running cluster
    /// (not the control plane): node failure, on a laptop.
    Node { name: String, node: u32, up: bool },
    /// Delete a stopped cluster's data inside the environment (macOS VM).
    Purge { name: String },
    /// Memory and CPU time of every running cluster.
    Stats,
    /// Stop every cluster and the environment.
    Shutdown,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Response {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stats: Vec<CellStats>,
    /// The VM's `/proc/meminfo` (macOS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meminfo: Option<super::memory::MemInfo>,
    /// The VM's `/proc/stat` (macOS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proc_stat: Option<super::cpu::ProcStat>,
    /// Clusters still running after the request.
    #[serde(default)]
    pub running: Vec<String>,
}

impl Response {
    pub fn error(e: impl std::fmt::Display) -> Self {
        Response { error: Some(e.to_string()), ..Default::default() }
    }
}

/// What a cluster's processes use (all of its nodes, pods included), the
/// way `docker stats` counts a container: memory less inactive page cache.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CellStats {
    pub name: String,
    pub memory_bytes: u64,
    pub cpu_usec: u64,
    /// By owner: litek8s, pods and containers, cache (all its nodes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<super::memory::Usage>,
    /// CPU time by owner (all its nodes), cumulative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<super::cpu::Split>,
}

/// What the VM's init answers to `stats` over vsock.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GuestStats {
    pub cells: Vec<CellStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meminfo: Option<super::memory::MemInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proc_stat: Option<super::cpu::ProcStat>,
}

/// Requests from the environment's owner to the cell manager inside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum CellRequest {
    /// Start a cluster's nodes. On Linux the message carries the API
    /// listener, the Docker socket's listener, the host-port broker and
    /// the log file, in that order (SCM_RIGHTS).
    Start { name: String, nodes: u32, data_dir: PathBuf, docker_alias: String },
    Stop { name: String },
    Node { name: String, node: u32, up: bool },
    Purge { name: String },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CellReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One request to the cell manager over a stream (the VM's), one reply.
pub fn cell_request(stream: &mut (impl std::io::Read + std::io::Write), req: &CellRequest) -> Result<CellReply> {
    use std::io::BufRead;
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    let mut reply = String::new();
    std::io::BufReader::new(stream).read_line(&mut reply)?;
    if reply.is_empty() {
        bail!("the cell manager closed the connection");
    }
    Ok(serde_json::from_str(&reply)?)
}

/// Memory and CPU time of each cluster under `<cgroup>/cells`.
pub fn cell_stats(cgroup: &Path) -> Vec<CellStats> {
    let mut out: Vec<CellStats> = std::fs::read_dir(cgroup.join("cells"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| {
            let dir = e.path();
            // What `docker stats` shows for a container: usage less the
            // page cache the kernel would drop first.
            let current: u64 = std::fs::read_to_string(dir.join("memory.current")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            let inactive: u64 = std::fs::read_to_string(dir.join("memory.stat"))
                .ok()
                .and_then(|s| s.lines().find_map(|l| l.strip_prefix("inactive_file ")?.trim().parse().ok()))
                .unwrap_or(0);
            let memory_bytes = current.saturating_sub(inactive);
            let cpu_usec = std::fs::read_to_string(dir.join("cpu.stat"))
                .ok()
                .and_then(|s| s.lines().find_map(|l| l.strip_prefix("usage_usec ")?.trim().parse().ok()))
                .unwrap_or(0);
            CellStats { name: e.file_name().to_string_lossy().into(), memory_bytes, cpu_usec, usage: super::memory::cluster(&dir), cpu: super::cpu::cluster(&dir) }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn state_path(home: &Path) -> PathBuf {
    home.join("env.json")
}

/// The control socket: in the dev data directory, unless that path is too
/// long for a Unix socket (104 bytes on macOS).
pub fn control_path(home: &Path) -> PathBuf {
    let p = home.join("env.sock");
    if p.as_os_str().len() < 100 {
        return p;
    }
    use sha2::Digest;
    let hash = hex::encode(sha2::Sha256::digest(home.as_os_str().as_encoded_bytes()));
    std::env::temp_dir().join(format!("litek8s-{}.sock", &hash[..12]))
}

pub fn write_state(home: &Path, state: &EnvState) -> Result<()> {
    let path = state_path(home);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn read_state(home: &Path) -> Option<EnvState> {
    serde_json::from_slice(&std::fs::read(state_path(home)).ok()?).ok()
}

/// The running environment, if there is one.
pub fn running(home: &Path) -> Option<EnvState> {
    read_state(home).filter(|s| super::pid_alive(s.pid))
}

/// One request to the running environment.
pub async fn request(home: &Path, req: &Request, timeout: Duration) -> Result<Response> {
    let env = running(home).context("no local cluster is running")?;
    let mut stream = tokio::net::UnixStream::connect(&env.control).await.with_context(|| format!("cannot reach the dev environment at {}", env.control.display()))?;
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    let mut reader = tokio::io::BufReader::new(stream);
    let mut reply = String::new();
    tokio::time::timeout(timeout, reader.read_line(&mut reply)).await.context("the dev environment did not answer in time")??;
    if reply.is_empty() {
        bail!("the dev environment closed the connection");
    }
    Ok(serde_json::from_str(&reply)?)
}

/// Serve control requests on `path` with `handle` (one at a time: starting
/// and stopping cells is quick, and ordering keeps them simple).
pub async fn serve<F, Fut>(path: PathBuf, handle: F) -> Result<()>
where
    F: Fn(Request) -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path).with_context(|| format!("failed to listen on {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let (read, mut write) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(read);
        let mut line = String::new();
        if tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line)).await.is_err() {
            continue;
        }
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle(req).await,
            Err(e) => Response::error(format!("bad request: {e}")),
        };
        let mut out = serde_json::to_vec(&reply).unwrap_or_default();
        out.push(b'\n');
        let _ = write.write_all(&out).await;
    }
}

/// An exclusive lock on the dev data directory while an environment is
/// being started, so two `dev up`s do not start two.
pub struct Lock(#[allow(dead_code)] std::fs::File);

pub fn lock(home: &Path) -> Result<Lock> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(home)?;
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(home.join("env.lock"))?;
    // SAFETY: flock on a descriptor we own; released when it closes.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(Lock(f))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let r = Request::Start { name: "ci".into(), port: None, nodes: 3 };
        let text = serde_json::to_string(&r).unwrap();
        assert_eq!(text, r#"{"op":"start","name":"ci","port":null,"nodes":3}"#);
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), r);
        assert_eq!(serde_json::from_str::<Request>(r#"{"op":"stats"}"#).unwrap(), Request::Stats);
        let c = CellRequest::Start { name: "dev".into(), nodes: 1, data_dir: "/x".into(), docker_alias: "/s".into() };
        assert_eq!(serde_json::from_str::<CellRequest>(&serde_json::to_string(&c).unwrap()).unwrap(), c);
    }

    #[test]
    fn long_homes_get_a_short_socket() {
        assert_eq!(control_path(Path::new("/h/dev")), PathBuf::from("/h/dev/env.sock"));
        let long = PathBuf::from(format!("/{}", "x".repeat(120)));
        let p = control_path(&long);
        assert!(p.as_os_str().len() < 100 && p.to_string_lossy().ends_with(".sock"), "{}", p.display());
    }

    #[tokio::test]
    async fn serves_requests() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("env.sock");
        write_state(dir.path(), &EnvState { pid: std::process::id(), mode: "test".into(), started_unix_ms: 0, control: sock.clone(), cgroup: None, vm_ip: None, helper_pid: None, cpus: None, memory_mib: None, shares: Vec::new(), publish: None }).unwrap();
        tokio::spawn(serve(sock.clone(), |req| async move {
            match req {
                Request::Start { port, .. } => Response { port: port.or(Some(1)), ..Default::default() },
                _ => Response::error("no"),
            }
        }));
        for _ in 0..100 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let r = request(dir.path(), &Request::Start { name: "a".into(), port: Some(7), nodes: 1 }, Duration::from_secs(2)).await.unwrap();
        assert_eq!(r.port, Some(7));
        let r = request(dir.path(), &Request::Stats, Duration::from_secs(2)).await.unwrap();
        assert_eq!(r.error.as_deref(), Some("no"));
    }
}
