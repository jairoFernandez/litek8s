//! Resource budgets for Docker containers in local development, what
//! Docker and OrbStack cannot give a compose stack:
//!
//! - **Project budgets**: every container of a compose project (label
//!   `com.docker.compose.project`) runs in one cgroup,
//!   `/litek8s/docker/projects/<project>/<id>`, whose `memory.max`,
//!   `cpu.max` and `pids.max` cap the whole stack like a server of that
//!   size. One runaway service hits the project's ceiling (and is the one
//!   the kernel's OOM killer picks) instead of eating the VM. Per-container
//!   limits still apply inside it.
//! - **Defaults** for containers that set no limit (off unless configured).
//!
//! Budgets come from `~/.litek8s/config.yaml` (the `docker:` section, sent
//! to the engine by `litek8s dev up`) and from labels on any service of the
//! project (`dev.litek8s.project.memory: 2Gi`). Field by field, the
//! config's entry for the project wins over labels, labels over
//! `default_project`:
//!
//! ```yaml
//! docker:
//!   projects:
//!     myapp: {memory: 2Gi, cpus: 2}
//!   default_project: {memory: 4Gi}
//!   defaults: {memory: 1Gi, cpus: 1}
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

pub const PROJECT_LABEL: &str = "com.docker.compose.project";
/// `dev.litek8s.project.memory`, `.cpus`, `.pids`, `.oom-group`.
pub const LABEL_PREFIX: &str = "dev.litek8s.project.";

/// Bytes, written `2Gi`, `512Mi`, `1g` (binary units, as Docker reads
/// them) or a bare number of MiB (like the top-level `memory:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Memory(pub u64);

impl Memory {
    pub fn parse(s: &str) -> Result<Self, String> {
        let t = s.trim();
        let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
        let (num, unit) = t.split_at(split);
        let n: f64 = num.parse().map_err(|_| format!("{s:?} is not a size (2Gi, 512Mi, 1g)"))?;
        let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
            "" | "mi" | "m" | "mib" | "mb" => 1 << 20,
            "b" => 1,
            "k" | "ki" | "kib" | "kb" => 1 << 10,
            "g" | "gi" | "gib" | "gb" => 1 << 30,
            "t" | "ti" | "tib" | "tb" => 1 << 40,
            u => return Err(format!("{s:?}: unknown unit {u:?} (use Ki, Mi, Gi or Ti)")),
        };
        let bytes = (n * mult as f64) as u64;
        if bytes < 6 << 20 {
            return Err(format!("{s:?} is below the 6Mi minimum"));
        }
        Ok(Memory(bytes))
    }

    /// `2Gi`, `384Mi`, or bytes when no unit divides them.
    pub fn display(self) -> String {
        for (unit, shift) in [("Ti", 40), ("Gi", 30), ("Mi", 20), ("Ki", 10)] {
            if self.0 % (1 << shift) == 0 {
                return format!("{}{unit}", self.0 >> shift);
            }
        }
        format!("{}b", self.0)
    }
}

impl Serialize for Memory {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.display())
    }
}

impl<'de> Deserialize<'de> for Memory {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_yaml::Value::deserialize(d)?;
        let s = match &v {
            serde_yaml::Value::String(s) => s.clone(),
            serde_yaml::Value::Number(n) => n.to_string(),
            _ => return Err(serde::de::Error::custom("memory must be a size such as 2Gi")),
        };
        Memory::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// CPUs: `2`, `1.5`, `"500m"`.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Cpus(pub f64);

impl Cpus {
    pub fn parse(s: &str) -> Result<Self, String> {
        let t = s.trim();
        let n = match t.strip_suffix('m') {
            Some(milli) => milli.parse::<f64>().map(|m| m / 1000.0),
            None => t.parse::<f64>(),
        }
        .map_err(|_| format!("{s:?} is not a number of CPUs (2, 1.5, 500m)"))?;
        if !(n.is_finite() && n >= 0.01) {
            return Err(format!("{s:?}: CPUs must be at least 0.01"));
        }
        Ok(Cpus(n))
    }

    /// The `cpu.max` quota per 100 ms period.
    pub fn quota(self) -> u64 {
        (self.0 * 100_000.0).round() as u64
    }

    pub fn nanos(self) -> i64 {
        (self.0 * 1e9).round() as i64
    }
}

impl Serialize for Cpus {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.0.fract() == 0.0 { s.serialize_u64(self.0 as u64) } else { s.serialize_f64(self.0) }
    }
}

impl<'de> Deserialize<'de> for Cpus {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_yaml::Value::deserialize(d)?;
        let s = match &v {
            serde_yaml::Value::String(s) => s.clone(),
            serde_yaml::Value::Number(n) => n.to_string(),
            _ => return Err(serde::de::Error::custom("cpus must be a number such as 2 or 0.5")),
        };
        Cpus::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Limits for a project (or, as `defaults`, for one container).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Budget {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<Memory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<Cpus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pids: Option<u64>,
    /// When the project hits its memory budget, kill every container of
    /// it (`memory.oom.group`) instead of the one process the kernel picks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oom_group: Option<bool>,
}

impl Budget {
    pub fn is_empty(&self) -> bool {
        *self == Budget::default()
    }

    /// Fields of `self`, the rest from `other`.
    pub fn or(self, other: Budget) -> Budget {
        Budget {
            memory: self.memory.or(other.memory),
            cpus: self.cpus.or(other.cpus),
            pids: self.pids.or(other.pids),
            oom_group: self.oom_group.or(other.oom_group),
        }
    }

    /// `memory 256Mi, cpus 2`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(m) = self.memory {
            parts.push(format!("memory {}", m.display()));
        }
        if let Some(c) = self.cpus {
            parts.push(format!("cpus {}", c.0));
        }
        if let Some(p) = self.pids {
            parts.push(format!("pids {p}"));
        }
        if self.oom_group == Some(true) {
            parts.push("oom group".into());
        }
        if parts.is_empty() { "no budget".into() } else { parts.join(", ") }
    }
}

/// The `docker:` section of `~/.litek8s/config.yaml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DockerSettings {
    /// Budgets by compose project name.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub projects: BTreeMap<String, Budget>,
    /// The budget of a project nothing else sets one for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_project: Option<Budget>,
    /// Limits of a container that sets none of its own (memory, cpus, pids).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defaults: Option<Budget>,
}

impl DockerSettings {
    pub fn is_empty(&self) -> bool {
        *self == DockerSettings::default()
    }

    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        for name in self.projects.keys() {
            if project_dir_name(name) != *name {
                out.push(format!("docker.projects: {name:?} is not a compose project name (lowercase letters, digits, - and _)"));
            }
        }
        if self.defaults.is_some_and(|d| d.oom_group.is_some()) {
            out.push("docker.defaults: oom_group is for projects (docker.projects or docker.default_project)".into());
        }
        if self.projects.values().chain(self.default_project.iter()).chain(self.defaults.iter()).any(|b| b.pids == Some(0)) {
            out.push("docker: pids must be at least 1".into());
        }
        out
    }

    /// A project's budget: the config's entry, then labels of its
    /// containers, then `default_project`, field by field.
    pub fn project_budget(&self, project: &str, labels: &[&Value]) -> Budget {
        let configured = self.projects.get(project).copied().unwrap_or_default();
        configured.or(label_budget(labels).0).or(self.default_project.unwrap_or_default())
    }
}

/// The budget the containers' `dev.litek8s.project.*` labels ask for (the
/// smallest when several say different things), and what could not be read.
pub fn label_budget(labels: &[&Value]) -> (Budget, Vec<String>) {
    let mut b = Budget::default();
    let mut problems = Vec::new();
    for l in labels {
        let Some(map) = l.as_object() else { continue };
        for (k, v) in map {
            let Some(field) = k.strip_prefix(LABEL_PREFIX) else { continue };
            let v = v.as_str().unwrap_or_default();
            let bad = |e: String| format!("label {k}: {e}");
            match field {
                "memory" => match Memory::parse(v) {
                    Ok(m) => b.memory = Some(b.memory.map_or(m, |o| o.min(m))),
                    Err(e) => problems.push(bad(e)),
                },
                "cpus" => match Cpus::parse(v) {
                    Ok(c) => b.cpus = Some(b.cpus.map_or(c, |o| if c.0 < o.0 { c } else { o })),
                    Err(e) => problems.push(bad(e)),
                },
                "pids" => match v.trim().parse::<u64>() {
                    Ok(p) if p > 0 => b.pids = Some(b.pids.map_or(p, |o| o.min(p))),
                    _ => problems.push(bad(format!("{v:?} is not a positive number"))),
                },
                "oom-group" | "oom_group" => match v.trim() {
                    "true" | "1" | "yes" => b.oom_group = Some(true),
                    "false" | "0" | "no" => b.oom_group = b.oom_group.or(Some(false)),
                    _ => problems.push(bad(format!("{v:?} is not true or false"))),
                },
                _ => problems.push(format!("unknown label {k} (memory, cpus, pids, oom-group)")),
            }
        }
    }
    (b, problems)
}

/// A project name as a cgroup directory: compose names are already
/// `[a-z0-9_-]`; anything else becomes `_`.
pub fn project_dir_name(project: &str) -> String {
    let s: String = project.chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' { c } else { '_' }).collect();
    if s.is_empty() || s.starts_with(['-', '_']) { format!("p{s}") } else { s }
}

/// The compose project of a container's labels.
pub fn project_of(labels: &Value) -> Option<&str> {
    labels[PROJECT_LABEL].as_str().filter(|p| !p.is_empty())
}

/// Where Docker containers live, under the cgroup root (the cell's).
pub const DOCKER_CGROUP: &str = "litek8s/docker";

/// A project's cgroup, relative to the cgroup root.
pub fn project_cgroup(project: &str) -> String {
    format!("{DOCKER_CGROUP}/projects/{}", project_dir_name(project))
}

/// A container's cgroup path as crun takes it (`cgroupsPath`).
pub fn container_cgroup(project: Option<&str>, id: &str) -> String {
    match project {
        Some(p) => format!("/{}/{id}", project_cgroup(p)),
        None => format!("/{DOCKER_CGROUP}/{id}"),
    }
}

/// The interface files a budget sets; unset fields go back to `max`.
pub fn limit_files(b: &Budget) -> Vec<(&'static str, String)> {
    vec![
        ("memory.max", b.memory.map(|m| m.0.to_string()).unwrap_or_else(|| "max".into())),
        // No swap beyond the budget (containers get none either).
        ("memory.swap.max", if b.memory.is_some() { "0".into() } else { "max".into() }),
        ("cpu.max", format!("{} 100000", b.cpus.map(|c| c.quota().to_string()).unwrap_or_else(|| "max".into()))),
        ("pids.max", b.pids.map(|p| p.to_string()).unwrap_or_else(|| "max".into())),
        ("memory.oom.group", if b.oom_group == Some(true) { "1".into() } else { "0".into() }),
    ]
}

/// Enable the memory, cpu and pids controllers for the children of `dir`
/// (those it has).
fn delegate(dir: &Path) {
    let have = std::fs::read_to_string(dir.join("cgroup.controllers")).unwrap_or_default();
    let on = std::fs::read_to_string(dir.join("cgroup.subtree_control")).unwrap_or_default();
    let want: Vec<String> = ["memory", "cpu", "pids"]
        .iter()
        .filter(|c| have.split_whitespace().any(|h| h == **c) && !on.split_whitespace().any(|h| h == **c))
        .map(|c| format!("+{c}"))
        .collect();
    if !want.is_empty() {
        let _ = std::fs::write(dir.join("cgroup.subtree_control"), want.join(" "));
    }
}

/// Create a project's cgroup under `root` (with controllers handed down to
/// it) and set its budget. Returns the directory.
pub fn apply_project(root: &Path, project: &str, b: &Budget) -> Result<PathBuf> {
    let dir = root.join(project_cgroup(project));
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create cgroup {}", dir.display()))?;
    let mut p = root.to_path_buf();
    for part in ["litek8s", "docker", "projects", &project_dir_name(project)] {
        delegate(&p);
        p.push(part);
    }
    for (file, value) in limit_files(b) {
        let path = dir.join(file);
        // memory.swap.max is missing without swap accounting; the rest only
        // matter when the budget sets them.
        let needed = match file {
            "memory.max" => b.memory.is_some(),
            "cpu.max" => b.cpus.is_some(),
            "pids.max" => b.pids.is_some(),
            "memory.oom.group" => b.oom_group == Some(true),
            _ => false,
        };
        if let Err(e) = std::fs::write(&path, &value) {
            if needed {
                return Err(e).with_context(|| format!("failed to set {} to {value} for project {project}", path.display()));
            }
        }
    }
    Ok(dir)
}

/// Remove a project's cgroup (it must be empty: no container cgroups left).
pub fn remove_project(root: &Path, project: &str) {
    let _ = std::fs::remove_dir(root.join(project_cgroup(project)));
}

fn read_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn keyed(path: &Path, key: &str) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.trim().parse().ok())
}

/// The smallest `file` limit from `dir` up to `root` (`memory.max`,
/// `pids.max`): what really caps a container.
pub fn effective_limit(root: &Path, dir: &Path, file: &str) -> Option<u64> {
    let mut best: Option<u64> = None;
    let mut d = dir.to_path_buf();
    while d.starts_with(root) && d != root {
        if let Some(v) = read_u64(&d.join(file)) {
            best = Some(best.map_or(v, |b| b.min(v)));
        }
        if !d.pop() {
            break;
        }
    }
    best
}

/// A project's usage and limits, as its cgroup says.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Usage {
    pub memory: u64,
    pub memory_max: Option<u64>,
    /// µs of CPU time so far.
    pub cpu_usec: u64,
    pub cpus: Option<f64>,
    pub pids: u64,
    pub pids_max: Option<u64>,
    /// Processes the OOM killer took in the project.
    pub oom_kills: u64,
    /// Times the project hit its memory budget.
    pub oom_events: u64,
}

pub fn read_usage(dir: &Path) -> Option<Usage> {
    let memory = read_u64(&dir.join("memory.current"))?;
    let cpus = std::fs::read_to_string(dir.join("cpu.max")).ok().and_then(|s| {
        let mut it = s.split_whitespace();
        let quota: f64 = it.next()?.parse().ok()?;
        let period: f64 = it.next()?.parse().ok()?;
        Some(quota / period)
    });
    Some(Usage {
        memory,
        memory_max: read_u64(&dir.join("memory.max")),
        cpu_usec: keyed(&dir.join("cpu.stat"), "usage_usec").unwrap_or(0),
        cpus,
        pids: read_u64(&dir.join("pids.current")).unwrap_or(0),
        pids_max: read_u64(&dir.join("pids.max")),
        oom_kills: keyed(&dir.join("memory.events"), "oom_kill").unwrap_or(0),
        oom_events: keyed(&dir.join("memory.events.local"), "oom").unwrap_or(0),
    })
}

/// Limits a container that sets none gets from `defaults`: (memory bytes,
/// nano CPUs, pids).
pub fn container_defaults(defaults: Option<&Budget>, memory: Option<i64>, nano_cpus: Option<i64>, pids: Option<i64>) -> (Option<i64>, Option<i64>, Option<i64>) {
    let d = defaults.copied().unwrap_or_default();
    (
        memory.or(d.memory.map(|m| m.0 as i64)),
        nano_cpus.or(d.cpus.map(Cpus::nanos)),
        pids.or(d.pids.map(|p| p as i64)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_sizes_and_cpus() {
        assert_eq!(Memory::parse("2Gi").unwrap().0, 2 << 30);
        assert_eq!(Memory::parse("512m").unwrap().0, 512 << 20);
        assert_eq!(Memory::parse("1.5g").unwrap().0, 3 << 29);
        assert_eq!(Memory::parse("256").unwrap().0, 256 << 20);
        assert_eq!(Memory::parse("268435456b").unwrap().0, 256 << 20);
        assert!(Memory::parse("1Mi").is_err());
        assert!(Memory::parse("lots").is_err());
        assert!(Memory::parse("2Xi").is_err());
        assert_eq!(Memory(256 << 20).display(), "256Mi");
        assert_eq!(Memory(3 << 29).display(), "1536Mi");
        assert_eq!(Cpus::parse("500m").unwrap().0, 0.5);
        assert_eq!(Cpus::parse("2").unwrap().quota(), 200_000);
        assert_eq!(Cpus::parse("1.5").unwrap().nanos(), 1_500_000_000);
        assert!(Cpus::parse("0").is_err());
    }

    #[test]
    fn parses_the_docker_section() {
        let s: DockerSettings = serde_yaml::from_str(
            "projects:\n  myapp: {memory: 2Gi, cpus: 2}\n  big: {memory: 8192, cpus: \"500m\", pids: 300, oom_group: true}\n\
             default_project: {memory: 4Gi}\ndefaults: {memory: 1Gi, cpus: 1}\n",
        )
        .unwrap();
        assert_eq!(s.projects["myapp"].memory, Some(Memory(2 << 30)));
        assert_eq!(s.projects["big"].memory, Some(Memory(8 << 30)));
        assert_eq!(s.projects["big"].cpus, Some(Cpus(0.5)));
        assert_eq!(s.projects["big"].oom_group, Some(true));
        assert_eq!(s.default_project.unwrap().memory, Some(Memory(4 << 30)));
        assert!(s.problems().is_empty());
        // Round trip, as the config file is rewritten by the UI.
        let back: DockerSettings = serde_yaml::from_str(&serde_yaml::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        assert!(serde_yaml::from_str::<DockerSettings>("projects: {a: {memroy: 1Gi}}").is_err());
        assert!(serde_yaml::from_str::<DockerSettings>("projects: {a: {memory: 1Mi}}").is_err());
        let bad: DockerSettings = serde_yaml::from_str("projects: {My.App: {memory: 1Gi}}\ndefaults: {oom_group: true}").unwrap();
        assert_eq!(bad.problems().len(), 2);
    }

    #[test]
    fn budget_precedence_config_then_labels_then_default() {
        let s: DockerSettings = serde_yaml::from_str("projects:\n  shop: {cpus: 2}\ndefault_project: {memory: 4Gi, pids: 500}\n").unwrap();
        let web = json!({"com.docker.compose.project": "shop", "dev.litek8s.project.memory": "512Mi", "dev.litek8s.project.cpus": "4"});
        let db = json!({"com.docker.compose.project": "shop", "dev.litek8s.project.memory": "256Mi"});
        let b = s.project_budget("shop", &[&web, &db]);
        assert_eq!(b.cpus, Some(Cpus(2.0)), "config wins over labels");
        assert_eq!(b.memory, Some(Memory(256 << 20)), "the smallest label wins");
        assert_eq!(b.pids, Some(500), "default_project fills the rest");
        assert_eq!(s.project_budget("other", &[]).memory, Some(Memory(4 << 30)));
        assert_eq!(DockerSettings::default().project_budget("x", &[]), Budget::default());
        let (_, problems) = label_budget(&[&json!({"dev.litek8s.project.memory": "huge", "dev.litek8s.project.colour": "red"})]);
        assert_eq!(problems.len(), 2);
    }

    #[test]
    fn cgroup_layout() {
        assert_eq!(container_cgroup(Some("shop"), "abc"), "/litek8s/docker/projects/shop/abc");
        assert_eq!(container_cgroup(None, "abc"), "/litek8s/docker/abc");
        assert_eq!(project_dir_name("My App"), "p_y__pp");
        assert_eq!(project_dir_name("../x"), "p___x");
        assert_eq!(project_dir_name("lk-budget_1"), "lk-budget_1");
        let files = limit_files(&Budget { memory: Some(Memory(256 << 20)), cpus: Some(Cpus(1.5)), pids: None, oom_group: Some(true) });
        assert_eq!(
            files,
            vec![
                ("memory.max", "268435456".to_string()),
                ("memory.swap.max", "0".into()),
                ("cpu.max", "150000 100000".into()),
                ("pids.max", "max".into()),
                ("memory.oom.group", "1".into())
            ]
        );
        assert_eq!(limit_files(&Budget::default())[0].1, "max");
    }

    #[test]
    fn applies_and_reads_a_project_cgroup() {
        let root = std::env::temp_dir().join(format!("lk-budget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("litek8s/docker")).unwrap();
        std::fs::write(root.join("cgroup.controllers"), "cpuset cpu io memory pids").unwrap();
        std::fs::write(root.join("litek8s/cgroup.controllers"), "cpu memory pids").unwrap();
        let b = Budget { memory: Some(Memory(256 << 20)), cpus: Some(Cpus(2.0)), ..Default::default() };
        let dir = apply_project(&root, "shop", &b).unwrap();
        assert_eq!(dir, root.join("litek8s/docker/projects/shop"));
        assert_eq!(std::fs::read_to_string(dir.join("memory.max")).unwrap(), "268435456");
        assert_eq!(std::fs::read_to_string(dir.join("cpu.max")).unwrap(), "200000 100000");
        assert_eq!(std::fs::read_to_string(root.join("cgroup.subtree_control")).unwrap(), "+memory +cpu +pids");
        assert_eq!(std::fs::read_to_string(root.join("litek8s/cgroup.subtree_control")).unwrap(), "+memory +cpu +pids");
        // A container inside with a looser limit of its own.
        let ctr = dir.join("abc");
        std::fs::create_dir_all(&ctr).unwrap();
        std::fs::write(ctr.join("memory.max"), (1u64 << 30).to_string()).unwrap();
        assert_eq!(effective_limit(&root, &ctr, "memory.max"), Some(256 << 20));
        std::fs::write(ctr.join("memory.max"), "max").unwrap();
        assert_eq!(effective_limit(&root, &ctr, "memory.max"), Some(256 << 20));
        assert_eq!(effective_limit(&root, &root.join("litek8s/docker/x"), "memory.max"), None);
        std::fs::write(dir.join("memory.current"), "1048576\n").unwrap();
        std::fs::write(dir.join("cpu.stat"), "usage_usec 1500\nuser_usec 1000\n").unwrap();
        std::fs::write(dir.join("memory.events"), "low 0\nhigh 0\nmax 3\noom 1\noom_kill 2\n").unwrap();
        std::fs::write(dir.join("memory.events.local"), "low 0\nhigh 0\nmax 3\noom 1\noom_kill 2\n").unwrap();
        let u = read_usage(&dir).unwrap();
        assert_eq!((u.memory, u.memory_max, u.cpu_usec, u.cpus, u.oom_kills, u.oom_events), (1 << 20, Some(256 << 20), 1500, Some(2.0), 2, 1));
        // Taking the budget away resets the files.
        apply_project(&root, "shop", &Budget::default()).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("memory.max")).unwrap(), "max");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn defaults_only_fill_what_is_unset() {
        let d = Budget { memory: Some(Memory(1 << 30)), cpus: Some(Cpus(1.0)), ..Default::default() };
        assert_eq!(container_defaults(Some(&d), None, None, None), (Some(1 << 30), Some(1_000_000_000), None));
        assert_eq!(container_defaults(Some(&d), Some(5 << 20), None, Some(10)), (Some(5 << 20), Some(1_000_000_000), Some(10)));
        assert_eq!(container_defaults(None, None, None, None), (None, None, None));
    }
}
