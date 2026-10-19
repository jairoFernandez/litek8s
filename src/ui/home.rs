//! The Home view's numbers: the dev cluster's state (its supervisor's state
//! file and API), CPU and memory of everything it runs, its disk use, and
//! where the user's tools point. Sampled every 2 s while Home or Free space
//! is on screen, every 10 s otherwise.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use super::app::{DevStatus, HomeSample, Msg};

pub fn spawn(tx: UnboundedSender<Msg>, local: bool, fast: Arc<AtomicBool>, wake: Arc<Notify>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut cpu_prev: Option<(u64, Instant)> = None;
        let mut disk: Option<(u64, Instant)> = None;
        let home = crate::dev::dev_home().unwrap_or_default();
        loop {
            let state = crate::dev::running_state();
            let mut s = HomeSample { data_dir: home.display().to_string(), ..Default::default() };
            match &state {
                None => s.status = if local { DevStatus::Down } else { DevStatus::Remote },
                Some(st) => {
                    s.status = if crate::dev::ready(st).await { DevStatus::Up } else { DevStatus::Starting };
                    s.mode = st.mode.clone();
                    s.uptime_s = crate::dev::now_ms().saturating_sub(st.started_unix_ms) / 1000;
                    s.api = format!("http://127.0.0.1:{}", st.port);
                    s.vm_ip = st.vm_ip.clone();
                    s.mem_used = crate::dev::memory_used(st);
                    if let Some(ns) = crate::dev::cpu_time_ns(st) {
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
            // Walking the data directory is not free: every 30 s.
            if disk.is_none_or(|(_, at)| at.elapsed() > Duration::from_secs(30)) && home.is_dir() {
                let h = home.clone();
                if let Ok(n) = tokio::task::spawn_blocking(move || crate::dev::data_size(&h)).await {
                    disk = Some((n, Instant::now()));
                }
            }
            s.disk_used = disk.map(|(n, _)| n);
            s.disk_free = free_space(if home.is_dir() { &home } else { std::path::Path::new("/") });
            s.contexts = super::ctx::detect(state.as_ref().map(|st| st.port));
            s.running = state;
            if tx.send(Msg::Home(Box::new(s))).is_err() {
                return;
            }
            let every = if fast.load(Ordering::Relaxed) { Duration::from_secs(2) } else { Duration::from_secs(10) };
            tokio::select! {
                _ = tokio::time::sleep(every) => {}
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
