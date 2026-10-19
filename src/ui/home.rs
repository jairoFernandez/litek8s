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

pub fn spawn(tx: UnboundedSender<Msg>, local: bool, pace: Arc<AtomicU64>, wake: Arc<Notify>, active: Arc<Mutex<String>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut cpu_prev: Option<(u64, Instant)> = None;
        let mut disk: Option<(u64, Instant, String)> = None;
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
            if every == VISIBLE && !fresh && home.is_dir() {
                let h = home.clone();
                if let Ok(n) = tokio::task::spawn_blocking(move || crate::dev::data_size(&h)).await {
                    disk = Some((n, Instant::now(), name.clone()));
                }
            }
            s.disk_used = disk.as_ref().filter(|(_, _, n)| *n == name).map(|(n, _, _)| *n);
            s.disk_free = free_space(if home.is_dir() { &home } else if dev_home.is_dir() { &dev_home } else { std::path::Path::new("/") });
            s.contexts = super::ctx::detect(&cluster, state.as_ref().map(|st| st.port));
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
