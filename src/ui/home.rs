//! The Home view's numbers: the shown cluster's state (its state file and
//! API), CPU and memory of the VM or sandbox the clusters share, the disk
//! use, and where the user's tools point. Sampled every 2 s while Home or Free space
//! is on screen, every 10 s on Settings (its restart note), every 60 s
//! otherwise (to notice the cluster's API port moving).

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use super::app::{DevStatus, HomeSample, Msg};

/// Seconds between samples, by what is on screen.
pub const VISIBLE: u64 = 2;
pub const SETTINGS: u64 = 10;
pub const HIDDEN: u64 = 60;

pub fn spawn(
    tx: UnboundedSender<Msg>,
    local: bool,
    pace: Arc<AtomicU64>,
    wake: Arc<Notify>,
    active: Arc<Mutex<String>>,
    remote: Option<super::client::Endpoint>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut cpu_prev: Option<(u64, Instant)> = None;
        let mut disk: Option<(u64, Instant, String)> = None;
        let mut disk_node: Option<(crate::dev::disk::NodeDisk, u64, Option<u64>)> = None;
        let dev_home = crate::dev::dev_home().unwrap_or_default();
        loop {
            let name = active.lock().unwrap().clone();
            let cluster = crate::dev::cluster::Cluster { name: name.clone() };
            // macOS: the clusters' data is in the VM's disk, in the dev data
            // directory; Linux: each cluster's own directory.
            let home = if cfg!(target_os = "macos") { dev_home.clone() } else { cluster.data_dir(&dev_home) };
            let state = crate::dev::cluster_state(&name);
            let env = crate::dev::env_state();
            let mut s = HomeSample { data_dir: home.display().to_string(), ..Default::default() };
            match &state {
                None => s.status = if local { DevStatus::Down } else { DevStatus::Remote },
                Some(st) => {
                    s.status = if crate::dev::ready(st).await { DevStatus::Up } else { DevStatus::Starting };
                    s.mode = st.mode.clone();
                    s.uptime_s = crate::dev::now_ms().saturating_sub(st.started_unix_ms) / 1000;
                    s.api = format!("http://127.0.0.1:{}", st.port);
                    s.vm_ip = st.vm_ip.clone();
                    s.mem_used = env.as_ref().and_then(crate::dev::env_memory).or_else(|| crate::dev::memory_used(st));
                    if let Some(ns) = env.as_ref().and_then(crate::dev::env_cpu_time_ns).or_else(|| crate::dev::cpu_time_ns(st)) {
                        let now = Instant::now();
                        if let Some((prev, at)) = cpu_prev.filter(|(p, _)| *p <= ns) {
                            let wall = now.duration_since(at).as_nanos().max(1) as f64;
                            s.cpu_pct = Some((ns - prev) as f64 / wall * 100.0);
                        }
                        cpu_prev = Some((ns, now));
                    }
                }
            }
            if state.is_none() {
                cpu_prev = None;
            }
            let every = pace.load(Ordering::Relaxed);
            // Walking the data directory is not free: every 30 s, and only
            // while its number is on screen.
            let fresh = disk.as_ref().is_some_and(|(_, at, n)| *n == name && at.elapsed() < Duration::from_secs(30));
            if every == VISIBLE && !fresh {
                // The node's own account (by category) when its API server
                // gives one; else a walk of the data directory.
                let node = match (&remote, &state) {
                    (Some(ep), _) => remote_disk(ep).await,
                    (None, Some(st)) => local_disk(st.port).await,
                    (None, None) => None,
                };
                let own = if remote.is_some() { 0 } else { crate::dev::disk::litek8s_files(&dev_home) };
                let image = if remote.is_none() && cfg!(target_os = "macos") { crate::dev::disk::vm_disk(&dev_home).map(|(alloc, _)| alloc) } else { None };
                match node {
                    Some(n) => disk_node = Some((n, own, image)),
                    None => disk_node = None,
                }
                let total = match &disk_node {
                    Some((n, own, Some(img))) => Some(own + img.max(&n.fs_used)),
                    Some((n, own, None)) => Some(own + n.total()),
                    None if home.is_dir() => {
                        let h = home.clone();
                        tokio::task::spawn_blocking(move || crate::dev::data_size(&h)).await.ok()
                    }
                    None => None,
                };
                if let Some(n) = total {
                    disk = Some((n, Instant::now(), name.clone()));
                }
            }
            s.disk_used = disk.as_ref().filter(|(_, _, n)| *n == name).map(|(n, _, _)| *n);
            s.disk_node = disk_node.clone();
            s.disk_free = free_space(if home.is_dir() { &home } else if dev_home.is_dir() { &dev_home } else { std::path::Path::new("/") });
            s.contexts = super::ctx::detect(&cluster, state.as_ref().map(|st| st.port));
            // Where the memory goes: only while it is on screen.
            if every == VISIBLE {
                (s.memory, s.cpu_sample) = match (&remote, &state) {
                    (Some(ep), _) => (remote_memory(ep).await, remote_cpu(ep).await),
                    (None, Some(_)) => crate::dev::resources(&name).await,
                    (None, None) => (None, None),
                };
            }
            s.running = state;
            if tx.send(Msg::Home(Box::new(s))).is_err() {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(every)) => {}
                _ = wake.notified() => {}
            }
        }
    })
}

/// A remote litek8s's node, from its API server.
async fn remote_memory(ep: &super::client::Endpoint) -> Option<crate::dev::memory::Breakdown> {
    let v = tokio::time::timeout(Duration::from_secs(3), ep.json(hyper::Method::GET, crate::kube::server::MEMORY_PATH, None)).await.ok()?.ok()?;
    let node: crate::dev::memory::NodeMemory = serde_json::from_value(v).ok()?;
    Some(crate::dev::memory::node_breakdown(&node))
}

/// A remote litek8s's node's disk, from its API server.
async fn remote_disk(ep: &super::client::Endpoint) -> Option<crate::dev::disk::NodeDisk> {
    let v = tokio::time::timeout(Duration::from_secs(60), ep.json(hyper::Method::GET, crate::kube::server::DISK_PATH, None)).await.ok()?.ok()?;
    serde_json::from_value(v).ok()
}

/// The local cluster's node's disk, from its API server.
async fn local_disk(port: u16) -> Option<crate::dev::disk::NodeDisk> {
    let (status, body) = tokio::time::timeout(Duration::from_secs(60), crate::dev::http(port, hyper::Method::GET, crate::kube::server::DISK_PATH, None)).await.ok()?.ok()?;
    if !status.is_success() {
        return None;
    }
    serde_json::from_slice(&body).ok()
}

/// A remote litek8s's node's CPU counters, from its API server.
async fn remote_cpu(ep: &super::client::Endpoint) -> Option<crate::dev::cpu::Sample> {
    let v = tokio::time::timeout(Duration::from_secs(3), ep.json(hyper::Method::GET, crate::kube::server::CPU_PATH, None)).await.ok()?.ok()?;
    let node: crate::dev::cpu::NodeCpu = serde_json::from_value(v).ok()?;
    let p = node.proc_stat?;
    Some(crate::dev::cpu::Sample {
        at: Some(std::time::Instant::now()),
        total_usec: p.busy_usec,
        shown: node.split,
        cpus: p.cpus.max(1) as f64,
        kind: crate::dev::cpu::Kind::Remote,
        ..Default::default()
    })
}

/// Bytes free on the filesystem holding `path`.
pub fn free_space(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs fills the struct we own.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        Some(st.f_bavail as u64 * st.f_frsize as u64)
    }
}
