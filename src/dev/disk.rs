//! Where a node's disk goes: images, writable layers, volumes, logs, builds,
//! the object store; and, on macOS, litek8s's own files and what the VM's
//! disk image keeps after the guest deleted it.
//!
//! One walk of the node's data directory (allocated blocks, hard links
//! once) puts every file in a category by where it is (see
//! [`category`]), so the parts add up to the whole. The walk is cached
//! ([`cached`]): at most one every [`REFRESH`], in the background.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The API server's node's disk by category, in bytes (allocated).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeDisk {
    pub node: String,
    /// Unpacked layers of every image and of the build cache: one store,
    /// shared by pods and Docker.
    pub images: u64,
    /// Writable layers of pod containers and Docker containers.
    pub container_layers: u64,
    /// Docker volumes, pods' emptyDirs, local PVs, ConfigMap/Secret files.
    pub volumes: u64,
    /// Pod and container logs (CRI format, 10 MiB + one rotated file each).
    pub logs: u64,
    /// Builds in progress and `RUN --mount=type=cache` directories.
    pub builds: u64,
    /// Kubernetes objects (the WAL) and the cluster's keys.
    pub objects: u64,
    /// The data of the cluster's other nodes (multi-node dev clusters).
    pub other_nodes: u64,
    /// Everything else: metadata, IP allocations, staging.
    pub other: u64,
    /// The data directory and the filesystem it is on.
    pub data_dir: String,
    #[serde(default)]
    pub fs_used: u64,
    #[serde(default)]
    pub fs_size: u64,
    /// When the walk ran (unix ms).
    #[serde(default)]
    pub computed_unix_ms: u64,
}

/// Several nodes as one: every category and the filesystems added up.
pub fn sum(nodes: &[NodeDisk], name: &str) -> NodeDisk {
    let mut d = NodeDisk { node: name.to_string(), ..Default::default() };
    for n in nodes {
        d.images += n.images;
        d.container_layers += n.container_layers;
        d.volumes += n.volumes;
        d.logs += n.logs;
        d.builds += n.builds;
        d.objects += n.objects;
        d.other_nodes += n.other_nodes;
        d.other += n.other;
        d.fs_used += n.fs_used;
        d.fs_size += n.fs_size;
        d.computed_unix_ms = d.computed_unix_ms.max(n.computed_unix_ms);
    }
    d
}

impl NodeDisk {
    pub fn total(&self) -> u64 {
        self.images + self.container_layers + self.volumes + self.logs + self.builds + self.objects + self.other_nodes + self.other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Images,
    ContainerLayers,
    Volumes,
    Logs,
    Builds,
    Objects,
    OtherNodes,
    Other,
}

/// The category of `rel`, a path under a dev cluster's data directory
/// (`state/`, `node/`, `docker/`, `workers/`).
pub fn category(rel: &Path) -> Category {
    let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
    let file = parts.last().copied().unwrap_or_default();
    let is_log = file == "log" || file.starts_with("log.");
    match parts.as_slice() {
        ["state", ..] | ["objects.wal" | "objects.wal.tmp" | "tls.json" | "sa.key" | "join-token"] => Category::Objects,
        ["workers", ..] => Category::OtherNodes,
        // `node-agent/`: `litek8s apiserver --data-dir`'s default node directory.
        ["node" | "node-agent", "crun", "images", ..] => Category::Images,
        ["node" | "node-agent", "crun", "containers", _, "upper" | "work", ..] => Category::ContainerLayers,
        ["node" | "node-agent", "crun", "containers", _, _] if is_log => Category::Logs,
        ["node" | "node-agent", "crun", "volumes", ..] | ["node" | "node-agent", "pods", _, "volumes", ..] => Category::Volumes,
        ["docker", "containers", _, "upper" | "work", ..] => Category::ContainerLayers,
        ["docker", "containers", _, _] if is_log => Category::Logs,
        ["docker", "volumes", ..] => Category::Volumes,
        ["docker", "build" | "build-cache-mounts", ..] => Category::Builds,
        // Logs of removed containers (crate::obs::logs's archive).
        ["node" | "node-agent", "logs", ..] => Category::Logs,
        // The same, relative to a node agent's own directory (what the
        // agent's metrics walk).
        ["crun", "images", ..] => Category::Images,
        ["crun", "containers", _, "upper" | "work", ..] => Category::ContainerLayers,
        ["crun", "containers", _, _] if is_log => Category::Logs,
        ["crun", "volumes", ..] | ["pods", _, "volumes", ..] => Category::Volumes,
        ["logs", ..] => Category::Logs,
        _ => Category::Other,
    }
}

/// Walk `dir` (not across filesystems), sizes by category.
pub fn walk(dir: &Path, node: &str) -> NodeDisk {
    use std::collections::HashSet;
    let mut d = NodeDisk { node: node.to_string(), data_dir: dir.display().to_string(), ..Default::default() };
    #[cfg(unix)]
    let root_dev = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(dir).map(|m| m.dev()).unwrap_or(0)
    };
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        for e in std::fs::read_dir(&p).into_iter().flatten().flatten() {
            let Ok(m) = e.metadata() else { continue };
            let path = e.path();
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                // Mounts inside (a pod's tmpfs, a bind mount) are not this disk.
                if m.dev() != root_dev {
                    continue;
                }
                if m.is_dir() {
                    stack.push(path.clone());
                }
                if m.nlink() > 1 && !m.is_dir() && !seen.insert((m.dev(), m.ino())) {
                    continue;
                }
                let bytes = m.blocks() * 512;
                let rel = path.strip_prefix(dir).unwrap_or(&path);
                let slot = match category(rel) {
                    Category::Images => &mut d.images,
                    Category::ContainerLayers => &mut d.container_layers,
                    Category::Volumes => &mut d.volumes,
                    Category::Logs => &mut d.logs,
                    Category::Builds => &mut d.builds,
                    Category::Objects => &mut d.objects,
                    Category::OtherNodes => &mut d.other_nodes,
                    Category::Other => &mut d.other,
                };
                *slot += bytes;
            }
            #[cfg(not(unix))]
            {
                let _ = (m, path);
            }
        }
    }
    if let Some((used, size)) = fs_usage(dir) {
        d.fs_used = used;
        d.fs_size = size;
    }
    d.computed_unix_ms = super::now_ms();
    d
}

/// (used, size) of the filesystem holding `path`.
pub fn fs_usage(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs fills the struct we own.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        let frsize = st.f_frsize as u64;
        let size = st.f_blocks as u64 * frsize;
        Some((size.saturating_sub(st.f_bfree as u64 * frsize), size))
    }
}

/// At most one walk per this long: a data directory with many images is a
/// big tree.
pub const REFRESH: Duration = Duration::from_secs(60);

struct Cache {
    last: Option<(Instant, NodeDisk)>,
    running: bool,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache { last: None, running: false });

/// The node's disk, from the last walk; a walk starts in the background
/// when that one is older than [`REFRESH`] (the first request waits for it).
pub async fn cached(dir: PathBuf, node: String) -> NodeDisk {
    let (have, stale) = {
        let c = CACHE.lock().unwrap();
        let have = c.last.as_ref().filter(|(_, d)| d.data_dir == dir.display().to_string()).map(|(_, d)| d.clone());
        let stale = c.last.as_ref().is_none_or(|(at, d)| at.elapsed() > REFRESH || d.data_dir != dir.display().to_string());
        (have, stale && !c.running)
    };
    if stale {
        CACHE.lock().unwrap().running = true;
        let job = tokio::task::spawn_blocking(move || {
            let d = walk(&dir, &node);
            let mut c = CACHE.lock().unwrap();
            c.last = Some((Instant::now(), d.clone()));
            c.running = false;
            d
        });
        if have.is_none() {
            return job.await.unwrap_or_default();
        }
    }
    match have {
        Some(d) => d,
        None => {
            // Another request is walking: wait for it.
            for _ in 0..600 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if let Some((_, d)) = CACHE.lock().unwrap().last.clone() {
                    return d;
                }
            }
            NodeDisk::default()
        }
    }
}

/// litek8s's own files on this machine: the binary and, on macOS, the
/// VM's kernel, root filesystem and the signed copy the VM runs from.
pub fn litek8s_files(dev_home: &Path) -> u64 {
    let alloc = |p: &Path| -> u64 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(p).map(|m| m.blocks() * 512).unwrap_or(0)
        }
        #[cfg(not(unix))]
        {
            std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
        }
    };
    let mut total = std::env::current_exe().ok().and_then(|e| e.canonicalize().ok()).map(|e| alloc(&e)).unwrap_or(0);
    if cfg!(target_os = "macos") {
        let assets = super::vm_assets_dir(dev_home);
        total += ["Image", "initramfs.gz", "initrd.gz", "root.img"].iter().map(|f| alloc(&assets.join(f))).sum::<u64>();
        total += alloc(&dev_home.join("vm-state").join("litek8s-vm"));
    }
    total
}

/// (allocated, apparent) size of the VM's disk image.
pub fn vm_disk(dev_home: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(dev_home.join("vm-state").join("disk.img")).ok()?;
    Some((m.blocks() * 512, m.len()))
}

/// One line of the breakdown.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub label: String,
    pub bytes: u64,
    pub note: String,
    pub indent: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Breakdown {
    pub total: u64,
    pub what: String,
    pub lines: Vec<Line>,
}

/// What a prune would free, from the Docker API's `/system/df` (what the
/// Free space view frees): unused images and build cache, stopped
/// containers, unused volumes.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Reclaimable {
    pub images: u64,
    pub containers: u64,
    pub volumes: u64,
}

fn line(label: &str, bytes: u64, note: String) -> Line {
    Line { label: label.into(), bytes, note, indent: false }
}

/// 1.2 GB, 340 MB (decimal, as the UI shows sizes).
pub fn human(b: u64) -> String {
    match b {
        0..1_000 => format!("{b} B"),
        1_000..1_000_000 => format!("{:.1} kB", b as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1} MB", b as f64 / 1e6),
        _ => format!("{:.1} GB", b as f64 / 1e9),
    }
}

fn reclaim(bytes: u64) -> String {
    if bytes == 0 { String::new() } else { format!("{} reclaimable (6 Free space)", human(bytes)) }
}

/// The lines every platform shows for a node.
fn node_lines(n: &NodeDisk, r: &Reclaimable) -> Vec<Line> {
    let mut lines = vec![
        line("images", n.images, if r.images > 0 { reclaim(r.images) } else { "layers shared by pods, Docker, builds".into() }),
        line("container layers", n.container_layers, if r.containers > 0 { reclaim(r.containers) } else { "what containers wrote".into() }),
        line("volumes", n.volumes, if r.volumes > 0 { reclaim(r.volumes) } else { "Docker volumes, emptyDirs, PVs".into() }),
        line("logs", n.logs, "pods and containers, 10 MiB each".into()),
    ];
    if n.builds > 0 {
        lines.push(line("builds", n.builds, "running builds, cache mounts".into()));
    }
    lines.push(line("objects", n.objects, "Kubernetes objects, keys".into()));
    if n.other_nodes > 0 {
        lines.push(line("other nodes", n.other_nodes, "the workers' images and pods".into()));
    }
    lines.push(line("other", n.other, "metadata, IP allocations".into()));
    lines
}

/// The macOS VM: litek8s's files, the shown cluster's data, the rest of
/// the VM's filesystem (other clusters, ext4), and what the disk image
/// still holds of what the guest deleted (until the guest trims its disk,
/// within a minute or so, see `super::guest`).
pub fn vm_breakdown(own: u64, image_alloc: u64, n: &NodeDisk, r: &Reclaimable, cluster: &str) -> Breakdown {
    let mut lines = vec![line("litek8s itself", own, "binary, VM kernel and root fs".into())];
    let mut node = node_lines(n, r);
    for l in &mut node {
        l.indent = true;
    }
    lines.push(line(&format!("cluster {cluster}"), n.total(), String::new()));
    lines.extend(node);
    lines.push(line("rest of the VM disk", n.fs_used.saturating_sub(n.total()), "other clusters, filesystem".into()));
    lines.push(line("deleted in the VM", image_alloc.saturating_sub(n.fs_used), "disk.img gives it back within a minute".into()));
    Breakdown { total: own + image_alloc.max(n.fs_used), what: "litek8s and its VM disk".into(), lines }
}

/// The Linux sandbox: litek8s's binary and the cluster's data directory.
pub fn host_breakdown(own: u64, n: &NodeDisk, r: &Reclaimable) -> Breakdown {
    let mut lines = vec![line("litek8s itself", own, "the binary".into())];
    lines.extend(node_lines(n, r));
    Breakdown { total: own + n.total(), what: "litek8s and the cluster's data".into(), lines }
}

/// A remote litek8s: what its API server's node reports.
pub fn node_breakdown(n: &NodeDisk, r: &Reclaimable, what: &str) -> Breakdown {
    Breakdown { total: n.total(), what: what.into(), lines: node_lines(n, r) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_by_place() {
        let c = |p: &str| category(Path::new(p));
        assert_eq!(c("state/objects.wal"), Category::Objects);
        assert_eq!(c("node/crun/images/l/abcd/usr/bin/x"), Category::Images);
        assert_eq!(c("node/crun/containers/0a1b/upper/etc/x"), Category::ContainerLayers);
        assert_eq!(c("node/crun/containers/0a1b/log"), Category::Logs);
        assert_eq!(c("node/crun/containers/0a1b/log.1"), Category::Logs);
        assert_eq!(c("node/crun/containers/0a1b/config.json"), Category::Other);
        assert_eq!(c("node/crun/volumes/litek8s-pv-pvc-1/data"), Category::Volumes);
        assert_eq!(c("node/pods/uid/volumes/cfg/..data"), Category::Volumes);
        assert_eq!(c("docker/containers/abc/upper/x"), Category::ContainerLayers);
        assert_eq!(c("docker/containers/abc/log"), Category::Logs);
        assert_eq!(c("docker/volumes/v/_data/x"), Category::Volumes);
        assert_eq!(c("docker/build-cache-mounts/x/y"), Category::Builds);
        assert_eq!(c("workers/1/crun/images/l/x"), Category::OtherNodes);
        assert_eq!(c("node/crun/ipam/10.42.0.2"), Category::Other);
        assert_eq!(c("objects.wal"), Category::Objects);
        assert_eq!(c("node-agent/crun/images/l/x"), Category::Images);
        // Logs of removed containers; paths relative to an agent's own dir.
        assert_eq!(c("node-agent/logs/default/web/nginx.0.ab12cd34.log"), Category::Logs);
        assert_eq!(c("crun/containers/0a1b/log"), Category::Logs);
        assert_eq!(c("crun/images/l/x"), Category::Images);
        assert_eq!(c("logs/default/web/labels.json"), Category::Logs);
    }

    #[test]
    fn walks_and_adds_up() {
        let d = tempfile::tempdir().unwrap();
        let w = |p: &str, n: usize| {
            let path = d.path().join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, vec![7u8; n]).unwrap();
        };
        w("state/objects.wal", 10_000);
        w("node/crun/images/l/aa/bin/sh", 50_000);
        w("node/crun/containers/c1/log", 20_000);
        w("docker/volumes/v/_data/db", 30_000);
        // A hard link counts once.
        std::fs::hard_link(d.path().join("node/crun/images/l/aa/bin/sh"), d.path().join("node/crun/images/l/aa/bin/ash")).unwrap();
        let n = walk(d.path(), "n");
        assert!(n.images >= 50_000 && n.images < 100_000, "{n:?}");
        assert!(n.objects >= 10_000 && n.logs >= 20_000 && n.volumes >= 30_000, "{n:?}");
        assert!(n.fs_size > 0 && n.fs_used > 0);
        let b = host_breakdown(1 << 20, &n, &Reclaimable::default());
        assert_eq!(b.lines.iter().map(|l| l.bytes).sum::<u64>(), b.total);
    }

    #[test]
    fn vm_disk_lines_add_up() {
        let n = NodeDisk { images: 300, container_layers: 50, volumes: 20, logs: 10, objects: 5, other: 15, fs_used: 600, ..Default::default() };
        let b = vm_breakdown(100, 1000, &n, &Reclaimable { images: 40, ..Default::default() }, "dev");
        let top: u64 = b.lines.iter().filter(|l| !l.indent).map(|l| l.bytes).sum();
        assert_eq!(top, b.total);
        assert_eq!(b.total, 1100);
        assert!(b.lines.iter().any(|l| l.label == "images" && l.note.contains("reclaimable")));
        assert_eq!(b.lines.last().unwrap().bytes, 400);
    }
}
