//! What each node agent samples every interval: the machine (CPU, memory,
//! filesystem, network, disks, load: node-exporter's names), each running
//! container (cgroup v2 CPU, memory split into anonymous and file pages,
//! I/O, OOM kills, throttling: cAdvisor's names, as the kubelet serves
//! them), each pod's network (its veth on the host), persistent volume use
//! (the kubelet's `kubelet_volume_stats_*`) and where the node's memory, CPU
//! and disk go between litek8s, pods and cache (the same split `litek8s ui`
//! shows, from [`crate::dev::memory`], [`crate::dev::cpu`] and
//! [`crate::dev::disk`]).
//!
//! Everything here reads files; the agent passes in what it knows about its
//! pods. Series get small ids per connection: the label set goes over the
//! wire once, then only (id, value) pairs.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::tsdb::Labels;

/// A container the agent runs.
#[derive(Clone, Debug, Default)]
pub struct ContainerRef {
    pub name: String,
    pub image: String,
    /// Its cgroup directory (crun); `None`: ask the runtime (Docker).
    pub cgroup: Option<PathBuf>,
    /// (CPU ns, working set bytes) from the runtime when there is no cgroup.
    pub runtime_stats: Option<(u64, u64)>,
}

#[derive(Clone, Debug, Default)]
pub struct PodRef {
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub containers: Vec<ContainerRef>,
    /// The host side of the pod's veth.
    pub host_if: Option<String>,
}

/// A persistent volume claim mounted by a pod on this node.
#[derive(Clone, Debug, Default)]
pub struct ClaimRef {
    pub namespace: String,
    pub claim: String,
    pub path: PathBuf,
    pub capacity: Option<u64>,
}

pub struct Collector {
    node: String,
    root: PathBuf,
    disk: Option<(Instant, crate::dev::disk::NodeDisk)>,
    volumes: HashMap<PathBuf, (Instant, u64)>,
}

/// The disk walk reads every file's metadata (an image store is a big
/// tree): rarely.
const DISK_EVERY: Duration = Duration::from_secs(900);
const VOLUME_EVERY: Duration = Duration::from_secs(60);

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn read_u64(path: &Path) -> Option<u64> {
    read(path)?.trim().parse().ok()
}

/// `key value` lines (cpu.stat, memory.stat, memory.events).
fn kv(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.trim().parse().ok())
}

fn l(pairs: &[(&str, &str)]) -> Labels {
    pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
}

impl Collector {
    pub fn new(node: &str, root: &Path) -> Self {
        Collector { node: node.to_string(), root: root.to_path_buf(), disk: None, volumes: HashMap::new() }
    }

    /// One sample of everything (blocking: reads files; run it off the
    /// async workers).
    pub fn sample(&mut self, pods: &[PodRef], claims: &[ClaimRef]) -> Vec<(Labels, f64)> {
        let mut out = Vec::new();
        self.node_metrics(&mut out);
        for pod in pods {
            self.pod_metrics(pod, &mut out);
        }
        self.volume_metrics(claims, &mut out);
        self.breakdown(&mut out);
        out
    }

    fn node_labels(&self, name: &str, extra: &[(&str, &str)]) -> Labels {
        let mut v = l(&[("__name__", name), ("job", "node-exporter"), ("instance", &self.node), ("node", &self.node)]);
        v.extend(l(extra));
        v
    }

    fn node_metrics(&mut self, out: &mut Vec<(Labels, f64)>) {
        // CPU by mode, all CPUs together (USER_HZ = 100).
        if let Some(stat) = read(Path::new("/proc/stat")) {
            if let Some(line) = stat.lines().find(|l| l.starts_with("cpu ")) {
                let v: Vec<f64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
                for (i, mode) in ["user", "nice", "system", "idle", "iowait", "irq", "softirq", "steal"].iter().enumerate() {
                    if let Some(ticks) = v.get(i) {
                        out.push((self.node_labels("node_cpu_seconds_total", &[("mode", mode)]), ticks / 100.0));
                    }
                }
            }
            let cpus = stat.lines().filter(|l| l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(u8::is_ascii_digit)).count();
            out.push((self.node_labels("machine_cpu_cores", &[]), cpus as f64));
        } else {
            out.push((self.node_labels("machine_cpu_cores", &[]), std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64));
        }
        if let Some(mem) = read(Path::new("/proc/meminfo")) {
            for (key, name) in [
                ("MemTotal", "node_memory_MemTotal_bytes"),
                ("MemAvailable", "node_memory_MemAvailable_bytes"),
                ("MemFree", "node_memory_MemFree_bytes"),
                ("Cached", "node_memory_Cached_bytes"),
                ("Buffers", "node_memory_Buffers_bytes"),
                ("SwapTotal", "node_memory_SwapTotal_bytes"),
                ("SwapFree", "node_memory_SwapFree_bytes"),
            ] {
                let v = mem.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(':')?.split_whitespace().next()?.parse::<f64>().ok());
                if let Some(kb) = v {
                    out.push((self.node_labels(name, &[]), kb * 1024.0));
                }
            }
        }
        if let Some(load) = read(Path::new("/proc/loadavg")) {
            let v: Vec<f64> = load.split_whitespace().take(3).filter_map(|x| x.parse().ok()).collect();
            for (i, name) in ["node_load1", "node_load5", "node_load15"].iter().enumerate() {
                if let Some(x) = v.get(i) {
                    out.push((self.node_labels(name, &[]), *x));
                }
            }
        }
        if let Some((size, free, avail)) = statvfs(&self.root) {
            let mp = self.root.display().to_string();
            for (name, v) in [("node_filesystem_size_bytes", size), ("node_filesystem_free_bytes", free), ("node_filesystem_avail_bytes", avail)] {
                out.push((self.node_labels(name, &[("mountpoint", &mp), ("fstype", "")]), v as f64));
            }
        }
        if let Some(dev) = read(Path::new("/proc/net/dev")) {
            for line in dev.lines().skip(2) {
                let Some((name, rest)) = line.split_once(':') else { continue };
                let name = name.trim();
                // Pod veths are reported per pod; the bridges add them up again.
                if name == "lo" || name.starts_with("lk") || name.starts_with("veth") || name == "litek8s0" || name == "lkcells0" {
                    continue;
                }
                let f: Vec<f64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                if f.len() < 16 {
                    continue;
                }
                for (i, metric) in [(0, "node_network_receive_bytes_total"), (1, "node_network_receive_packets_total"), (2, "node_network_receive_errs_total"), (8, "node_network_transmit_bytes_total"), (9, "node_network_transmit_packets_total"), (10, "node_network_transmit_errs_total")] {
                    out.push((self.node_labels(metric, &[("device", name)]), f[i]));
                }
            }
        }
        if let Some(stats) = read(Path::new("/proc/diskstats")) {
            for line in stats.lines() {
                let f: Vec<&str> = line.split_whitespace().collect();
                if f.len() < 14 || !whole_disk(f[2]) {
                    continue;
                }
                let n = |i: usize| f[i].parse::<f64>().unwrap_or(0.0);
                out.push((self.node_labels("node_disk_read_bytes_total", &[("device", f[2])]), n(5) * 512.0));
                out.push((self.node_labels("node_disk_written_bytes_total", &[("device", f[2])]), n(9) * 512.0));
                out.push((self.node_labels("node_disk_io_time_seconds_total", &[("device", f[2])]), n(12) / 1000.0));
            }
        }
        if let Some((cpu, memory)) = node_usage(Path::new("/sys/fs/cgroup"), read(Path::new("/proc/stat")).as_deref(), read(Path::new("/proc/meminfo")).as_deref()) {
            let resource = |name: &str| l(&[("__name__", name), ("job", "kubelet"), ("metrics_path", "/metrics/resource"), ("instance", &self.node), ("node", &self.node)]);
            out.push((resource("node_cpu_usage_seconds_total"), cpu));
            out.push((resource("node_memory_working_set_bytes"), memory));
        }
        out.push((self.node_labels("up", &[]), 1.0));
    }

    fn pod_metrics(&mut self, pod: &PodRef, out: &mut Vec<(Labels, f64)>) {
        let base = |name: &str, container: &str, image: &str| -> Labels {
            l(&[
                ("__name__", name),
                ("job", "kubelet"),
                ("metrics_path", "/metrics/cadvisor"),
                ("instance", &self.node),
                ("node", &self.node),
                ("namespace", &pod.namespace),
                ("pod", &pod.name),
                ("container", container),
                ("image", image),
            ])
        };
        for c in &pod.containers {
            let mut push = |name: &str, v: f64| out.push((base(name, &c.name, &c.image), v));
            match &c.cgroup {
                Some(dir) => {
                    let Some(cpu) = read(&dir.join("cpu.stat")) else { continue };
                    push("container_cpu_usage_seconds_total", kv(&cpu, "usage_usec").unwrap_or(0) as f64 / 1e6);
                    push("container_cpu_user_seconds_total", kv(&cpu, "user_usec").unwrap_or(0) as f64 / 1e6);
                    push("container_cpu_system_seconds_total", kv(&cpu, "system_usec").unwrap_or(0) as f64 / 1e6);
                    if let Some(p) = kv(&cpu, "nr_periods").filter(|p| *p > 0) {
                        push("container_cpu_cfs_periods_total", p as f64);
                        push("container_cpu_cfs_throttled_periods_total", kv(&cpu, "nr_throttled").unwrap_or(0) as f64);
                        push("container_cpu_cfs_throttled_seconds_total", kv(&cpu, "throttled_usec").unwrap_or(0) as f64 / 1e6);
                    }
                    let current = read_u64(&dir.join("memory.current")).unwrap_or(0);
                    let stat = read(&dir.join("memory.stat")).unwrap_or_default();
                    let inactive = kv(&stat, "inactive_file").unwrap_or(0);
                    push("container_memory_usage_bytes", current as f64);
                    push("container_memory_working_set_bytes", current.saturating_sub(inactive) as f64);
                    push("container_memory_rss", kv(&stat, "anon").unwrap_or(0) as f64);
                    push("container_memory_cache", kv(&stat, "file").unwrap_or(0) as f64);
                    if let Some(max) = read(&dir.join("memory.max")).and_then(|m| m.trim().parse::<f64>().ok()) {
                        push("container_spec_memory_limit_bytes", max);
                    }
                    if let Some(ev) = read(&dir.join("memory.events")) {
                        push("container_oom_events_total", kv(&ev, "oom_kill").unwrap_or(0) as f64);
                    }
                    if let Some(io) = read(&dir.join("io.stat")) {
                        let sum = |key: &str| -> f64 {
                            io.lines().flat_map(|l| l.split_whitespace()).filter_map(|f| f.strip_prefix(key)?.parse::<f64>().ok()).sum()
                        };
                        push("container_fs_reads_bytes_total", sum("rbytes="));
                        push("container_fs_writes_bytes_total", sum("wbytes="));
                    }
                }
                None => {
                    if let Some((cpu_ns, ws)) = c.runtime_stats {
                        push("container_cpu_usage_seconds_total", cpu_ns as f64 / 1e9);
                        push("container_memory_working_set_bytes", ws as f64);
                    }
                }
            }
        }
        // The host end of the veth sees the pod's traffic reversed.
        if let Some(dev) = &pod.host_if {
            let stat = |f: &str| read_u64(&Path::new("/sys/class/net").join(dev).join("statistics").join(f));
            for (metric, file) in [
                ("container_network_receive_bytes_total", "tx_bytes"),
                ("container_network_transmit_bytes_total", "rx_bytes"),
                ("container_network_receive_packets_total", "tx_packets"),
                ("container_network_transmit_packets_total", "rx_packets"),
                ("container_network_receive_packets_dropped_total", "tx_dropped"),
                ("container_network_transmit_packets_dropped_total", "rx_dropped"),
            ] {
                if let Some(v) = stat(file) {
                    let mut labels = base(metric, "", "");
                    labels.push(("interface".into(), "eth0".into()));
                    out.push((labels, v as f64));
                }
            }
        }
    }

    fn volume_metrics(&mut self, claims: &[ClaimRef], out: &mut Vec<(Labels, f64)>) {
        let wanted: HashSet<&PathBuf> = claims.iter().map(|c| &c.path).collect();
        self.volumes.retain(|p, _| wanted.contains(p));
        for c in claims {
            let used = match self.volumes.get(&c.path) {
                Some((at, used)) if at.elapsed() < VOLUME_EVERY => *used,
                _ => {
                    let used = dir_size(&c.path, 200_000);
                    self.volumes.insert(c.path.clone(), (Instant::now(), used));
                    used
                }
            };
            let fs = statvfs(&c.path);
            let capacity = c.capacity.or(fs.map(|f| f.0)).unwrap_or(0);
            let available = capacity.saturating_sub(used).min(fs.map_or(u64::MAX, |f| f.2));
            let labels = |name: &str| {
                l(&[("__name__", name), ("job", "kubelet"), ("metrics_path", "/metrics"), ("instance", &self.node), ("node", &self.node), ("namespace", &c.namespace), ("persistentvolumeclaim", &c.claim)])
            };
            out.push((labels("kubelet_volume_stats_used_bytes"), used as f64));
            out.push((labels("kubelet_volume_stats_capacity_bytes"), capacity as f64));
            out.push((labels("kubelet_volume_stats_available_bytes"), available as f64));
        }
    }

    /// Where the node's memory, CPU and disk go (as `litek8s ui` shows).
    fn breakdown(&mut self, out: &mut Vec<(Labels, f64)>) {
        let labels = |name: &str, key: &str, value: &str| l(&[("__name__", name), ("job", "litek8s"), ("instance", &self.node), ("node", &self.node), (key, value)]);
        if Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
            let mem = crate::dev::memory::this_node(&self.node);
            for (owner, v) in [("litek8s", mem.usage.litek8s), ("pods", mem.usage.containers), ("cache", mem.usage.cache)] {
                out.push((labels("litek8s_node_memory_bytes", "owner", owner), v as f64));
            }
            if let Some(split) = crate::dev::cpu::node(Path::new("/sys/fs/cgroup")) {
                out.push((labels("litek8s_node_cpu_seconds_total", "owner", "litek8s"), split.litek8s_usec as f64 / 1e6));
                out.push((labels("litek8s_node_cpu_seconds_total", "owner", "pods"), split.containers_usec() as f64 / 1e6));
            }
        }
        let fresh = self.disk.as_ref().is_some_and(|(at, _)| at.elapsed() < DISK_EVERY);
        if !fresh {
            self.disk = Some((Instant::now(), crate::dev::disk::walk(&self.root, &self.node)));
        }
        if let Some((_, d)) = &self.disk {
            for (cat, v) in [
                ("images", d.images),
                ("container_layers", d.container_layers),
                ("volumes", d.volumes),
                ("logs", d.logs),
                ("builds", d.builds),
                ("objects", d.objects),
                ("other", d.other + d.other_nodes),
            ] {
                out.push((labels("litek8s_node_disk_bytes", "category", cat), v as f64));
            }
        }
    }
}

/// Whole disks, not partitions or loop/ram devices.
fn whole_disk(name: &str) -> bool {
    if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("zram") || name.starts_with("dm-") || name.starts_with("sr") {
        return false;
    }
    if name.starts_with("nvme") || name.starts_with("mmcblk") {
        return !name.contains('p');
    }
    !name.ends_with(|c: char| c.is_ascii_digit())
}

/// (size, free, available to unprivileged users) of the filesystem.
pub fn statvfs(path: &Path) -> Option<(u64, u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs fills the struct we own.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        let f = st.f_frsize as u64;
        Some((st.f_blocks as u64 * f, st.f_bfree as u64 * f, st.f_bavail as u64 * f))
    }
}

/// Allocated bytes under `dir`, giving up after `max_entries` files.
pub fn dir_size(dir: &Path, max_entries: usize) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut total = 0u64;
    let mut seen = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > max_entries {
                return total;
            }
            let Ok(m) = e.metadata() else { continue };
            total += m.blocks() * 512;
            if m.is_dir() {
                stack.push(e.path());
            }
        }
    }
    total
}

/// The agent's side of the wire: ids for label sets, sent once per
/// connection.
#[derive(Default)]
pub struct SeriesIds {
    ids: HashMap<Labels, u32>,
    sent: HashSet<u32>,
    next: u32,
}

impl SeriesIds {
    /// A new connection: every series is announced again.
    pub fn reset_sent(&mut self) {
        self.sent.clear();
    }

    /// (new series to announce, values).
    pub fn encode(&mut self, samples: Vec<(Labels, f64)>) -> (Vec<(u32, Labels)>, Vec<(u32, f64)>) {
        let mut new = Vec::new();
        let mut values = Vec::with_capacity(samples.len());
        let mut live = HashSet::with_capacity(samples.len());
        for (labels, v) in samples {
            if !v.is_finite() {
                continue;
            }
            let id = match self.ids.get(&labels) {
                Some(id) => *id,
                None => {
                    // Ids are never reused within a process: gaps are fine.
                    let id = self.next;
                    self.next += 1;
                    self.ids.insert(labels.clone(), id);
                    id
                }
            };
            if self.sent.insert(id) {
                new.push((id, labels));
            }
            live.insert(id);
            values.push((id, v));
        }
        // Forget series that are gone (a deleted pod's), so the map stays
        // as big as what is running.
        if self.ids.len() > live.len() * 2 + 64 {
            self.ids.retain(|_, id| live.contains(id));
            self.sent.retain(|id| live.contains(id));
        }
        (new, values)
    }
}

/// The node's CPU seconds and memory working set as the kubelet reports
/// them on `/metrics/resource` (what `kubectl top nodes` shows). A node
/// with a cgroup of its own (a dev cluster's cell, an agent in a
/// container: its cgroup namespace root has `memory.current`) is that
/// cgroup; on a host's root cgroup it is the machine, from `/proc` (busy
/// CPU modes; `MemTotal - MemAvailable`). In the macOS dev VM the second
/// would count the balloon's pages as used and every cell would see the
/// whole VM.
fn node_usage(cgroup: &Path, stat: Option<&str>, meminfo: Option<&str>) -> Option<(f64, f64)> {
    if let Some(current) = read_u64(&cgroup.join("memory.current")) {
        let cpu = kv(&read(&cgroup.join("cpu.stat"))?, "usage_usec")? as f64 / 1e6;
        let inactive = read(&cgroup.join("memory.stat")).and_then(|m| kv(&m, "inactive_file")).unwrap_or(0);
        return Some((cpu, current.saturating_sub(inactive) as f64));
    }
    let line = stat?.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<f64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    // user nice system idle iowait irq softirq steal
    let busy: f64 = [0, 1, 2, 5, 6].iter().filter_map(|i| v.get(*i)).sum::<f64>() / 100.0;
    let mem = meminfo?;
    let field = |key: &str| mem.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(':')?.split_whitespace().next()?.parse::<f64>().ok());
    Some((busy, (field("MemTotal")? - field("MemAvailable")?).max(0.0) * 1024.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_this_machine() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Collector::new("n1", dir.path());
        let s = c.sample(&[PodRef { namespace: "d".into(), name: "p".into(), containers: vec![ContainerRef { name: "c".into(), runtime_stats: Some((2_000_000_000, 5)), ..Default::default() }], ..Default::default() }], &[]);
        let find = |name: &str| s.iter().find(|(l, _)| l.iter().any(|(k, v)| k == "__name__" && v == name)).map(|x| x.1);
        assert_eq!(find("container_cpu_usage_seconds_total"), Some(2.0));
        assert_eq!(find("container_memory_working_set_bytes"), Some(5.0));
        assert!(find("node_filesystem_size_bytes").unwrap() > 0.0);
        assert!(find("machine_cpu_cores").unwrap() >= 1.0);
        #[cfg(target_os = "linux")]
        assert!(find("node_memory_MemTotal_bytes").unwrap() > 0.0);
    }

    #[test]
    fn ids_are_announced_once_per_connection() {
        let mut ids = SeriesIds::default();
        let a = l(&[("__name__", "a")]);
        let b = l(&[("__name__", "b")]);
        let (new, vals) = ids.encode(vec![(a.clone(), 1.0), (b.clone(), f64::NAN)]);
        assert_eq!((new.len(), vals.len()), (1, 1));
        let (new, vals) = ids.encode(vec![(a.clone(), 2.0), (b.clone(), 3.0)]);
        assert_eq!(new.len(), 1);
        assert_eq!(vals.len(), 2);
        ids.reset_sent();
        let (new, _) = ids.encode(vec![(a, 2.0)]);
        assert_eq!(new.len(), 1);
    }

    #[test]
    fn node_usage_from_its_cgroup_or_the_machine() {
        let dir = tempfile::tempdir().unwrap();
        let stat = "cpu  1000 0 500 9000 100 0 0 0 0 0\ncpu0 1000 0 500 9000 100 0 0 0 0 0\n";
        let meminfo = "MemTotal:        4000 kB\nMemFree:          500 kB\nMemAvailable:    1000 kB\n";
        // A host's root cgroup: no memory.current, the machine.
        assert_eq!(node_usage(dir.path(), Some(stat), Some(meminfo)), Some((15.0, 3_072_000.0)));
        // A cell: its own cgroup.
        std::fs::write(dir.path().join("memory.current"), "10000\n").unwrap();
        std::fs::write(dir.path().join("memory.stat"), "anon 6000\ninactive_file 4000\n").unwrap();
        std::fs::write(dir.path().join("cpu.stat"), "usage_usec 2500000\nuser_usec 2000000\n").unwrap();
        assert_eq!(node_usage(dir.path(), Some(stat), Some(meminfo)), Some((2.5, 6000.0)));
    }

    #[test]
    fn whole_disks_only() {
        assert!(whole_disk("sda") && whole_disk("nvme0n1") && whole_disk("vda") && whole_disk("mmcblk0"));
        assert!(!whole_disk("sda1") && !whole_disk("nvme0n1p2") && !whole_disk("loop3") && !whole_disk("mmcblk0p1"));
    }
}
