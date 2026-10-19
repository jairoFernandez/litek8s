//! Where a node's memory goes, in words a user can act on: litek8s itself,
//! pods and containers, page cache; and, for the macOS VM, what the guest
//! kernel costs and what the guest freed but macOS still charges for.
//!
//! The numbers come from cgroup v2 files and `/proc/meminfo` (Linux only;
//! elsewhere everything is `None`/zero):
//!
//! - a node's cgroup (a cell's root, see [`super::cells`]) holds litek8s's
//!   processes (API server, node agent, crun shims, the cell's init) and,
//!   under `litek8s/`, every pod (`pod*`), Docker container (`docker/*`)
//!   and build (`build/*`);
//! - "in use" of a cgroup is `memory.current` less its file pages
//!   (`memory.stat` `file`): what would not come back if the kernel dropped
//!   its caches; the file pages are the cache.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// A node's (or several nodes') memory by owner, in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// litek8s's own processes: API server, controllers, node agent, shims.
    pub litek8s: u64,
    /// Pods, Docker containers and image builds.
    pub containers: u64,
    /// File pages (page cache) charged to the node: reclaimable.
    pub cache: u64,
}

impl std::ops::Add for Usage {
    type Output = Usage;
    fn add(self, o: Usage) -> Usage {
        Usage { litek8s: self.litek8s + o.litek8s, containers: self.containers + o.containers, cache: self.cache + o.cache }
    }
}

fn read_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn stat_field(dir: &Path, field: &str) -> Option<u64> {
    let text = std::fs::read_to_string(dir.join("memory.stat")).ok()?;
    text.lines().find_map(|l| l.strip_prefix(field)?.strip_prefix(' ')?.trim().parse().ok())
}

/// (in use, file pages) of one cgroup.
pub fn cgroup(dir: &Path) -> Option<(u64, u64)> {
    let current = read_u64(&dir.join("memory.current"))?;
    let file = stat_field(dir, "file").unwrap_or(0);
    Some((current.saturating_sub(file), file))
}

/// In use by the cgroups of pods, containers and builds under `root`.
fn containers(root: &Path) -> u64 {
    let base = root.join("litek8s");
    let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir(&base)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("pod"))
        .map(|e| e.path())
        .collect();
    for sub in ["docker", "build"] {
        dirs.extend(std::fs::read_dir(base.join(sub)).into_iter().flatten().flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()));
    }
    dirs.iter().filter_map(|d| cgroup(d)).map(|(used, _)| used).sum()
}

/// One node: `root` is its cgroup (a cell's, as the host sees it, or
/// `/sys/fs/cgroup` from inside the cell).
pub fn node(root: &Path) -> Option<Usage> {
    let (used, file) = cgroup(root)?;
    let containers = containers(root);
    Some(Usage { litek8s: used.saturating_sub(containers), containers, cache: file })
}

/// A cluster's nodes: `dir` holds one cgroup per node (`cells/<cluster>`).
pub fn cluster(dir: &Path) -> Option<Usage> {
    let mut nodes = std::fs::read_dir(dir).ok()?.flatten().filter(|e| e.file_name().to_string_lossy().parse::<u32>().is_ok()).peekable();
    nodes.peek()?;
    Some(nodes.filter_map(|e| node(&e.path())).fold(Usage::default(), |a, b| a + b))
}

/// What `/proc/meminfo` says, in bytes (the fields the breakdown uses).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct MemInfo {
    pub total: u64,
    pub free: u64,
    pub cached: u64,
    pub buffers: u64,
    pub shmem: u64,
    pub slab_reclaimable: u64,
    pub slab_unreclaimable: u64,
    pub kernel_stack: u64,
    pub page_tables: u64,
}

pub fn parse_meminfo(text: &str) -> MemInfo {
    let mut m = MemInfo::default();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else { continue };
        let kb: u64 = rest.split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let v = kb * 1024;
        match key {
            "MemTotal" => m.total = v,
            "MemFree" => m.free = v,
            "Cached" => m.cached = v,
            "Buffers" => m.buffers = v,
            "Shmem" => m.shmem = v,
            "SReclaimable" => m.slab_reclaimable = v,
            "SUnreclaim" => m.slab_unreclaimable = v,
            "KernelStack" => m.kernel_stack = v,
            "PageTables" => m.page_tables = v,
            _ => {}
        }
    }
    m
}

pub fn meminfo() -> Option<MemInfo> {
    Some(parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)).filter(|m| m.total > 0)
}

/// What `GET /litek8s/v1/memory` returns: the API server's node.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeMemory {
    pub node: String,
    #[serde(flatten)]
    pub usage: Usage,
    /// The machine's (or VM's) `/proc/meminfo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meminfo: Option<MemInfo>,
}

/// This process's node, from inside: its cgroup namespace root is the node
/// (a dev cluster's cell); on a plain host, the litek8s process and the
/// pods under `/sys/fs/cgroup/litek8s`.
pub fn this_node(name: &str) -> NodeMemory {
    let root = Path::new("/sys/fs/cgroup");
    let usage = node(root).unwrap_or_else(|| {
        let own = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("RssAnon:")?.split_whitespace().next()?.parse::<u64>().ok()))
            .unwrap_or(0)
            * 1024;
        Usage { litek8s: own, containers: containers(root), cache: 0 }
    });
    NodeMemory { node: name.to_string(), usage, meminfo: meminfo() }
}

/// One line of the breakdown: label, bytes, what it means.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub label: String,
    pub bytes: u64,
    pub note: &'static str,
}

/// The Home view's memory breakdown.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Breakdown {
    /// The figure being explained, and what it is.
    pub total: u64,
    pub what: &'static str,
    pub lines: Vec<Line>,
}

fn line(label: impl Into<String>, bytes: u64, note: &'static str) -> Line {
    Line { label: label.into(), bytes, note }
}

/// The macOS VM: `footprint` is what macOS charges the VM process,
/// `vm_bytes` the VM's memory size, `mem` the guest's meminfo, `shown` and
/// `others` the clusters' usage (the shown one and the rest).
pub fn vm_breakdown(footprint: u64, vm_bytes: u64, mem: &MemInfo, cluster: &str, shown: Usage, others: &[Usage]) -> Breakdown {
    let other = others.iter().fold(Usage::default(), |a, b| a + *b);
    // Memory the kernel keeps before it counts any (its image, the page
    // structures), its unreclaimable slab, stacks, page tables, and the
    // root filesystem held in RAM (the initramfs: litek8s's binary, crun,
    // iptables): the VM's fixed cost.
    let reserved = vm_bytes.saturating_sub(mem.total);
    let mut kernel = reserved + mem.slab_unreclaimable + mem.kernel_stack + mem.page_tables + mem.shmem;
    let cache = (mem.cached + mem.buffers).saturating_sub(mem.shmem) + mem.slab_reclaimable;
    let in_guest = vm_bytes.saturating_sub(mem.free);
    // What is left (the VM's init, cell manager, DHCP client; the cgroups'
    // own kernel memory) goes with the fixed cost.
    let attributed = kernel + cache + shown.litek8s + shown.containers + other.litek8s + other.containers;
    kernel += in_guest.saturating_sub(attributed);
    let mut lines = vec![line(format!("litek8s ({cluster})"), shown.litek8s, "API server, controllers, node agent"), line("pods & containers", shown.containers, "")];
    if !others.is_empty() {
        lines.push(line(format!("other clusters ({})", others.len()), other.litek8s + other.containers, "litek8s and pods of the rest"));
    }
    lines.push(line("Linux kernel + base", kernel, "fixed cost: kernel, root fs in RAM"));
    lines.push(line("cache", cache, "reclaimable"));
    let freed = footprint.saturating_sub(in_guest.max(attributed));
    if freed >= 1 << 20 {
        lines.push(line("freed in the VM", freed, "macOS takes it back when it needs it"));
    }
    Breakdown { total: footprint, what: "what macOS charges the VM", lines }
}

/// The Linux sandbox: `sandbox` is its whole cgroup (supervisor, pasta,
/// cell manager, clusters), `shown` and `others` the clusters.
pub fn sandbox_breakdown(sandbox_used: u64, sandbox_cache: u64, cluster: &str, shown: Usage, others: &[Usage]) -> Breakdown {
    let other = others.iter().fold(Usage::default(), |a, b| a + *b);
    let rest = sandbox_used.saturating_sub(shown.litek8s + shown.containers + other.litek8s + other.containers);
    let mut lines = vec![line(format!("litek8s ({cluster})"), shown.litek8s, "API server, controllers, node agent"), line("pods & containers", shown.containers, "")];
    if !others.is_empty() {
        lines.push(line(format!("other clusters ({})", others.len()), other.litek8s + other.containers, "litek8s and pods of the rest"));
    }
    lines.push(line("sandbox", rest, "supervisor, pasta, cell manager"));
    lines.push(line("cache", sandbox_cache, "reclaimable (file pages)"));
    Breakdown { total: sandbox_used + sandbox_cache, what: "the sandbox's cgroup", lines }
}

/// A remote litek8s: what its API server's node reports.
pub fn node_breakdown(n: &NodeMemory) -> Breakdown {
    let u = n.usage;
    Breakdown {
        total: u.litek8s + u.containers + u.cache,
        what: "node of the API server",
        lines: vec![line("litek8s", u.litek8s, "API server, controllers, node agent"), line("pods & containers", u.containers, ""), line("cache", u.cache, "reclaimable")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, current: u64, file: u64) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("memory.current"), current.to_string()).unwrap();
        std::fs::write(dir.join("memory.stat"), format!("anon 1\nfile {file}\nfile_mapped 3\n")).unwrap();
    }

    #[test]
    fn splits_a_node_into_litek8s_pods_and_cache() {
        let d = tempfile::tempdir().unwrap();
        let cell = d.path().join("cells/ci/0");
        write(&cell, 100 << 20, 30 << 20);
        write(&cell.join("litek8s/pod_abc"), 40 << 20, 10 << 20);
        write(&cell.join("litek8s/docker/1234"), 6 << 20, 1 << 20);
        write(&cell.join("litek8s/shims"), 2 << 20, 0);
        let u = node(&cell).unwrap();
        assert_eq!(u, Usage { litek8s: 35 << 20, containers: 35 << 20, cache: 30 << 20 });
        write(&d.path().join("cells/ci/1"), 10 << 20, 2 << 20);
        let all = cluster(&d.path().join("cells/ci")).unwrap();
        assert_eq!(all, Usage { litek8s: 43 << 20, containers: 35 << 20, cache: 32 << 20 });
        assert!(cluster(&d.path().join("cells/none")).is_none());
    }

    #[test]
    fn explains_the_vm_footprint() {
        let mem = parse_meminfo("MemTotal: 3934000 kB\nMemFree: 3700000 kB\nBuffers: 0 kB\nCached: 125000 kB\nShmem: 51200 kB\nSReclaimable: 10000 kB\nSUnreclaim: 35000 kB\nKernelStack: 2000 kB\nPageTables: 1000 kB\n");
        assert_eq!(mem.total, 3934000 * 1024);
        let shown = Usage { litek8s: 12 << 20, containers: 0, cache: 3 << 20 };
        let b = vm_breakdown(600 << 20, 4096 << 20, &mem, "dev", shown, &[]);
        assert_eq!(b.total, 600 << 20);
        let get = |l: &str| b.lines.iter().find(|x| x.label.starts_with(l)).map(|x| x.bytes);
        assert_eq!(get("litek8s (dev)"), Some(12 << 20));
        assert_eq!(get("cache"), Some((125000 - 51200 + 10000) * 1024));
        // Everything the guest uses is attributed; the rest of the
        // footprint is what it touched and freed.
        let in_guest = (4096u64 << 20) - mem.free;
        let sum: u64 = b.lines.iter().filter(|l| !l.label.starts_with("freed")).map(|l| l.bytes).sum();
        assert_eq!(sum, in_guest);
        assert_eq!(get("freed"), Some((600u64 << 20) - in_guest));
        let b = vm_breakdown(421 << 20, 4096 << 20, &mem, "dev", shown, &[Usage { litek8s: 2 << 20, containers: 1 << 20, cache: 0 }]);
        assert_eq!(b.lines[2].label, "other clusters (1)");
        assert_eq!(b.lines[2].bytes, 3 << 20);
    }

    #[test]
    fn explains_the_sandbox() {
        let b = sandbox_breakdown(100 << 20, 40 << 20, "dev", Usage { litek8s: 8 << 20, containers: 50 << 20, cache: 5 << 20 }, &[]);
        assert_eq!(b.total, 140 << 20);
        assert_eq!(b.lines.iter().find(|l| l.label == "sandbox").unwrap().bytes, 42 << 20);
        let n = NodeMemory { node: "x".into(), usage: Usage { litek8s: 1, containers: 2, cache: 3 }, meminfo: None };
        let json = serde_json::to_value(&n).unwrap();
        assert_eq!(json["litek8s"], 1);
        assert_eq!(node_breakdown(&n).total, 6);
    }
}
