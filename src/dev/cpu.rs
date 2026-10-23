//! Where a node's CPU time goes: litek8s itself, each pod, container and
//! build, and (for the macOS VM) the guest kernel and the VM's own
//! overhead. Counters are cumulative (µs); the UI turns two samples into
//! percentages.
//!
//! A node's cgroup (see [`super::memory`]) holds litek8s's processes and,
//! under `litek8s/`, a cgroup per pod (`pod*`), Docker container
//! (`docker/<id>`) and build (`build/<id>`); `cpu.stat` `usage_usec` of
//! each is what its processes ran, kernel time on their behalf included.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// One node's (or a cluster's) CPU time by owner, cumulative.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Split {
    /// litek8s's own processes.
    pub litek8s_usec: u64,
    /// Pods, containers and builds: (`pod<uid>`, `docker/<id>`,
    /// `build/<id>`, as in the cgroup tree; µs).
    pub cgroups: Vec<(String, u64)>,
}

impl Split {
    pub fn containers_usec(&self) -> u64 {
        self.cgroups.iter().map(|(_, u)| u).sum()
    }

    fn merge(mut self, o: Split) -> Split {
        self.litek8s_usec += o.litek8s_usec;
        self.cgroups.extend(o.cgroups);
        self
    }
}

pub fn usage_usec(dir: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(dir.join("cpu.stat")).ok()?;
    text.lines().find_map(|l| l.strip_prefix("usage_usec ")?.trim().parse().ok())
}

fn children(dir: &Path) -> Vec<(String, std::path::PathBuf)> {
    std::fs::read_dir(dir).into_iter().flatten().flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| (e.file_name().to_string_lossy().into_owned(), e.path())).collect()
}

/// One node: `root` is its cgroup.
pub fn node(root: &Path) -> Option<Split> {
    let total = usage_usec(root)?;
    let base = root.join("litek8s");
    let mut cgroups = Vec::new();
    for (name, path) in children(&base) {
        if name.starts_with("pod") {
            if let Some(u) = usage_usec(&path) {
                cgroups.push((name, u));
            }
        } else if name == "docker" || name == "build" {
            for (id, p) in children(&path) {
                // Compose projects: docker/projects/<project>/<id>.
                let inner = if name == "docker" && id == "projects" { children(&p).into_iter().flat_map(|(_, d)| children(&d)).collect() } else { vec![(id, p)] };
                for (id, p) in inner {
                    if let Some(u) = usage_usec(&p) {
                        cgroups.push((format!("{name}/{id}"), u));
                    }
                }
            }
        }
    }
    let containers: u64 = cgroups.iter().map(|(_, u)| u).sum();
    Some(Split { litek8s_usec: total.saturating_sub(containers), cgroups })
}

/// A cluster: one cgroup per node under `dir` (`cells/<cluster>`).
pub fn cluster(dir: &Path) -> Option<Split> {
    let nodes: Vec<_> = children(dir).into_iter().filter(|(n, _)| n.parse::<u32>().is_ok()).collect();
    if nodes.is_empty() {
        return None;
    }
    Some(nodes.iter().filter_map(|(_, p)| node(p)).fold(Split::default(), Split::merge))
}

/// The machine's (or VM's) CPU time from `/proc/stat`, µs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcStat {
    /// Not idle and not waiting for I/O, all CPUs.
    pub busy_usec: u64,
    pub total_usec: u64,
    pub cpus: u32,
}

pub fn parse_proc_stat(text: &str) -> Option<ProcStat> {
    // USER_HZ is 100 on every Linux litek8s runs on.
    const TICK_USEC: u64 = 10_000;
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    if v.len() < 5 {
        return None;
    }
    // user nice system idle iowait irq softirq steal (guest time is in user).
    let total: u64 = v.iter().take(8).sum();
    let idle = v[3] + v[4];
    let cpus = text.lines().filter(|l| l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(u8::is_ascii_digit)).count() as u32;
    Some(ProcStat { busy_usec: (total - idle) * TICK_USEC, total_usec: total * TICK_USEC, cpus })
}

pub fn proc_stat() -> Option<ProcStat> {
    parse_proc_stat(&std::fs::read_to_string("/proc/stat").ok()?)
}

/// What `GET /litek8s/v1/cpu` returns: the API server's node, cumulative.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeCpu {
    pub node: String,
    #[serde(flatten)]
    pub split: Split,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proc_stat: Option<ProcStat>,
}

impl NodeCpu {
    /// The counters as a remote machine's sample, taken now.
    pub fn sample(&self) -> Option<Sample> {
        let p = self.proc_stat?;
        Some(Sample {
            at: Some(std::time::Instant::now()),
            total_usec: p.busy_usec,
            shown: self.split.clone(),
            cpus: p.cpus.max(1) as f64,
            kind: Kind::Remote,
            ..Default::default()
        })
    }
}

/// Several nodes as one: their counters added up (cumulative, so the
/// sum's rate is the sum of their rates). A node without /proc/stat leaves
/// the whole without it.
pub fn sum(nodes: &[NodeCpu], name: &str) -> NodeCpu {
    let split = nodes.iter().fold(Split::default(), |a, n| a.merge(n.split.clone()));
    let proc_stat = nodes.iter().map(|n| n.proc_stat).try_fold(ProcStat::default(), |a, p| {
        p.map(|p| ProcStat { busy_usec: a.busy_usec + p.busy_usec, total_usec: a.total_usec + p.total_usec, cpus: a.cpus + p.cpus })
    });
    NodeCpu { node: name.to_string(), split, proc_stat }
}

pub fn this_node(name: &str) -> NodeCpu {
    NodeCpu { node: name.to_string(), split: node(Path::new("/sys/fs/cgroup")).unwrap_or_default(), proc_stat: proc_stat() }
}

/// One sample of every counter the breakdown needs (cumulative).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    pub at: Option<std::time::Instant>,
    /// What the figure measures: the VM process, the sandbox's cgroup, or
    /// the remote machine's busy time (µs).
    pub total_usec: u64,
    pub shown: Split,
    /// Every other cluster, together.
    pub others_usec: u64,
    pub others: usize,
    /// macOS: the guest's busy time.
    pub guest_busy_usec: Option<u64>,
    /// What the CPUs can give per second, in CPUs.
    pub cpus: f64,
    pub kind: Kind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    /// The macOS VM.
    #[default]
    Vm,
    /// The Linux sandbox.
    Sandbox,
    /// A remote machine: its /proc/stat.
    Remote,
}

/// One line: label, % of one CPU, what it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub label: String,
    pub pct: f64,
    pub note: String,
    /// A sub-line (a top container).
    pub indent: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Breakdown {
    /// % of one CPU the figure is.
    pub total_pct: f64,
    pub cpus: f64,
    pub what: String,
    pub lines: Vec<Line>,
}

/// Percentages between two samples; `name` turns a cgroup id into a pod or
/// container name.
pub fn breakdown(prev: &Sample, now: &Sample, cluster: &str, name: &dyn Fn(&str) -> String) -> Option<Breakdown> {
    let wall = now.at?.checked_duration_since(prev.at?)?.as_micros() as f64;
    if wall <= 0.0 || prev.kind != now.kind {
        return None;
    }
    let pct = |a: u64, b: u64| a.saturating_sub(b) as f64 / wall * 100.0;
    let total = pct(now.total_usec, prev.total_usec);
    let litek8s = pct(now.shown.litek8s_usec, prev.shown.litek8s_usec);
    let mut per: Vec<(String, f64)> = now
        .shown
        .cgroups
        .iter()
        .map(|(id, u)| {
            let before = prev.shown.cgroups.iter().find(|(p, _)| p == id).map(|(_, u)| *u).unwrap_or(*u);
            (id.clone(), pct(*u, before))
        })
        .collect();
    per.sort_by(|a, b| b.1.total_cmp(&a.1));
    // fold from 0.0: summing an empty f64 iterator yields -0.0, shown as "-0.0%".
    let containers: f64 = per.iter().fold(0.0, |a, (_, p)| a + p);
    let line = |label: String, pct: f64, note: &str| Line { label, pct, note: note.to_string(), indent: false };
    // A remote litek8s is not one of this machine's dev clusters: no name.
    let label = if now.kind == Kind::Remote { "litek8s".to_string() } else { format!("litek8s ({cluster})") };
    let mut lines = vec![line(label, litek8s, "API server, controllers, node agent")];
    lines.push(line("pods & containers".into(), containers, ""));
    for (id, p) in per.iter().take(3).filter(|(_, p)| *p >= 0.05) {
        lines.push(Line { label: name(id), pct: *p, note: String::new(), indent: true });
    }
    if now.others > 0 {
        lines.push(line(format!("other clusters ({})", now.others), pct(now.others_usec, prev.others_usec), "litek8s and pods of the rest"));
    }
    let clusters = litek8s + containers + if now.others > 0 { pct(now.others_usec, prev.others_usec) } else { 0.0 };
    let what = match now.kind {
        Kind::Vm => {
            let busy = match (now.guest_busy_usec, prev.guest_busy_usec) {
                (Some(a), Some(b)) => pct(a, b),
                _ => clusters,
            };
            lines.push(line("guest kernel & other".into(), (busy - clusters).max(0.0), "interrupts, network, the VM's init"));
            lines.push(line("VM overhead".into(), (total - busy).max(0.0), "virtual devices, vCPU exits (macOS side)"));
            "the VM process"
        }
        Kind::Sandbox => {
            lines.push(line("sandbox".into(), (total - clusters).max(0.0), "supervisor, pasta, cell manager"));
            "the sandbox"
        }
        Kind::Remote => {
            lines.push(line("rest of the machine".into(), (total - clusters).max(0.0), "kernel and other processes"));
            "the machine, busy"
        }
    };
    Some(Breakdown { total_pct: total, cpus: now.cpus, what: what.to_string(), lines })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cg(dir: &Path, usec: u64) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("cpu.stat"), format!("usage_usec {usec}\nuser_usec 1\nsystem_usec 1\n")).unwrap();
    }

    #[test]
    fn a_remote_node_is_not_named_after_a_dev_cluster() {
        let at = std::time::Instant::now();
        let sample = |dt: u64, busy: u64, own: u64| Sample {
            at: Some(at + std::time::Duration::from_secs(dt)),
            total_usec: busy,
            shown: Split { litek8s_usec: own, ..Default::default() },
            cpus: 2.0,
            kind: Kind::Remote,
            ..Default::default()
        };
        let b = breakdown(&sample(0, 0, 0), &sample(1, 100_000, 20_000), "dev", &|id| id.to_string()).unwrap();
        assert_eq!(b.lines[0].label, "litek8s");
        assert!((b.lines[0].pct - 2.0).abs() < 1e-9 && (b.total_pct - 10.0).abs() < 1e-9);
        assert_eq!(b.lines.last().unwrap().label, "rest of the machine");
    }

    #[test]
    fn splits_a_node() {
        let d = tempfile::tempdir().unwrap();
        let n = d.path().join("cells/ci/0");
        cg(&n, 1000);
        cg(&n.join("litek8s/pod_a_b"), 300);
        cg(&n.join("litek8s/docker/abc"), 100);
        cg(&n.join("litek8s/shims"), 50);
        // A compose project's containers, each counted once.
        cg(&n.join("litek8s/docker/projects/shop"), 150);
        cg(&n.join("litek8s/docker/projects/shop/def"), 100);
        cg(&n.join("litek8s/docker/projects/shop/ghi"), 50);
        let s = node(&n).unwrap();
        assert_eq!(s.litek8s_usec, 450);
        assert_eq!(s.containers_usec(), 550);
        assert!(s.cgroups.iter().any(|(id, u)| id == "docker/def" && *u == 100));
        cg(&d.path().join("cells/ci/1"), 200);
        assert_eq!(cluster(&d.path().join("cells/ci")).unwrap().litek8s_usec, 650);
    }

    #[test]
    fn reads_proc_stat() {
        let p = parse_proc_stat("cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 50 0 25 400 25 0 0 0 0 0\ncpu1 50 0 25 400 25 0 0 0 0 0\nintr 1\n").unwrap();
        assert_eq!(p, ProcStat { busy_usec: 150 * 10_000, total_usec: 1000 * 10_000, cpus: 2 });
    }

    #[test]
    fn percentages_between_samples() {
        let t0 = std::time::Instant::now();
        let t1 = t0 + std::time::Duration::from_secs(1);
        let split = |l: u64, pod: u64, web: u64| Split { litek8s_usec: l, cgroups: vec![("pod_x".into(), pod), ("docker/w".into(), web)] };
        let prev = Sample { at: Some(t0), total_usec: 0, shown: split(0, 0, 0), others_usec: 0, others: 1, guest_busy_usec: Some(0), cpus: 4.0, kind: Kind::Vm };
        let now = Sample { at: Some(t1), total_usec: 400_000, shown: split(50_000, 200_000, 10_000), others_usec: 20_000, others: 1, guest_busy_usec: Some(300_000), cpus: 4.0, kind: Kind::Vm };
        let b = breakdown(&prev, &now, "dev", &|id| format!("n:{id}")).unwrap();
        assert!((b.total_pct - 40.0).abs() < 1e-6);
        let get = |l: &str| b.lines.iter().find(|x| x.label == l).unwrap().pct;
        assert!((get("litek8s (dev)") - 5.0).abs() < 1e-6);
        assert!((get("pods & containers") - 21.0).abs() < 1e-6);
        assert!((get("n:pod_x") - 20.0).abs() < 1e-6);
        assert!((get("guest kernel & other") - 2.0).abs() < 1e-6);
        assert!((get("VM overhead") - 10.0).abs() < 1e-6);
        assert_eq!(b.lines[2].label, "n:pod_x");
        assert!(breakdown(&now, &now, "dev", &|i| i.into()).is_none());
    }
}
