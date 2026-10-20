//! The UI's state and what keys do to it. Nothing here does I/O: a key
//! changes the state and returns [`Effect`]s the runtime carries out, and
//! what the backends learn arrives as [`Msg`]s. That keeps all of it
//! testable without a terminal, Docker or a cluster.

use std::collections::{BTreeSet, VecDeque};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::kube::{KEffect, KMsg, KubeUi};
use super::model::{self, Container, DiskUsage, Image, Pod, Volume};
use crate::dev::config::{DevConfig, Publish};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Home,
    Clusters,
    Containers,
    Images,
    Volumes,
    Space,
    Kube,
    Settings,
}

pub const TABS: [Tab; 8] = [Tab::Home, Tab::Clusters, Tab::Containers, Tab::Images, Tab::Volumes, Tab::Space, Tab::Kube, Tab::Settings];

impl Tab {
    pub fn title(self) -> &'static str {
        match self {
            Tab::Home => "Home",
            Tab::Clusters => "Clusters",
            Tab::Containers => "Containers",
            Tab::Images => "Images",
            Tab::Volumes => "Volumes",
            Tab::Space => "Free space",
            Tab::Kube => "Kubernetes",
            Tab::Settings => "Settings",
        }
    }

    fn index(self) -> usize {
        TABS.iter().position(|t| *t == self).unwrap_or(0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ContainerOp {
    Start,
    Stop,
    Restart,
    Remove,
}

impl ContainerOp {
    pub fn verb(&self) -> &'static str {
        match self {
            ContainerOp::Start => "start",
            ContainerOp::Stop => "stop",
            ContainerOp::Restart => "restart",
            ContainerOp::Remove => "remove",
        }
    }
}

/// `litek8s dev …` on a local cluster.
#[derive(Debug, Clone, PartialEq)]
pub enum DevOp {
    /// Start it; `switch`: make its context kubectl's current one.
    Up { cluster: String, switch: bool },
    Down(String),
    /// Stop it and delete its data and context.
    Delete(String),
    /// A new cluster with this many nodes.
    Create { cluster: String, nodes: u32 },
}

impl DevOp {
    pub fn cluster(&self) -> &str {
        match self {
            DevOp::Up { cluster, .. } | DevOp::Down(cluster) | DevOp::Delete(cluster) | DevOp::Create { cluster, .. } => cluster,
        }
    }

    pub fn starts(&self) -> bool {
        matches!(self, DevOp::Up { .. } | DevOp::Create { .. })
    }

    /// The `litek8s` arguments.
    pub fn args(&self) -> Vec<String> {
        let name = |c: &str| vec!["--name".to_string(), c.to_string()];
        let mut v: Vec<String> = match self {
            DevOp::Up { cluster, switch } => {
                let mut v = vec!["dev".into(), "up".into()];
                v.extend(name(cluster));
                if !switch {
                    v.push("--no-switch".into());
                }
                v
            }
            DevOp::Down(c) => [vec!["dev".into(), "down".into()], name(c)].concat(),
            DevOp::Delete(c) => [vec!["dev".into(), "delete".into()], name(c)].concat(),
            DevOp::Create { cluster, nodes } => [vec!["dev".into(), "up".into()], name(cluster), vec!["--nodes".into(), nodes.to_string(), "--no-switch".into()]].concat(),
        };
        v.shrink_to_fit();
        v
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LogSource {
    Container { id: String, name: String },
}

/// What the runtime is asked to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Containers(ContainerOp, Vec<String>),
    RemoveImages(Vec<String>),
    RemoveVolumes(Vec<String>),
    PruneContainers,
    PruneBuildCache,
    Logs(LogSource),
    StopLogs,
    Exec(String),
    Dev(DevOp),
    /// Show this local cluster in every view.
    UseCluster(String),
    /// Show this kubeconfig context in the Kubernetes view (read-only).
    UseContext(String),
    /// Point kubectl and docker at this local cluster.
    SwitchTools(String),
    /// The Kubernetes view's.
    Kube(KEffect),
    SaveConfig(DevConfig),
    EditConfig,
    DockerContext(bool),
    KubeContext(bool),
    Refresh,
    Quit,
    /// One after the other.
    Batch(Vec<Effect>),
}

/// What the backends report.
#[derive(Debug, Clone)]
pub enum Msg {
    Containers(Vec<Container>),
    Images(Vec<Image>),
    Volumes(Vec<Volume>),
    Disk(DiskUsage),
    DockerInfo { version: String, cpus: Option<u64>, memory: Option<u64>, os: String },
    DockerDown(String),
    Pods(Vec<Pod>),
    KubeDown(String),
    /// The Kubernetes view's.
    Kube(KMsg),
    Home(Box<HomeSample>),
    LogLines(u64, Vec<String>),
    LogEnd(u64, Option<String>),
    /// A finished action: `Ok(message)` or `Err(error)`.
    Done(Result<String, String>),
    /// A line of output of a running `dev up/down/reset`.
    DevOutput(String),
    DevFinished(Result<String, String>),
    Config(Result<DevConfig, String>),
    Clusters(Box<ClustersSample>),
}

/// A local cluster in the Clusters view.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClusterRow {
    pub name: String,
    pub context: String,
    pub status: DevStatus,
    pub port: Option<u16>,
    pub nodes: u32,
    pub ready_nodes: Option<usize>,
    pub memory: Option<u64>,
    pub age: Option<String>,
}

/// What the Clusters view shows: local clusters, the VM or sandbox they
/// share, and the kubeconfig's other contexts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClustersSample {
    pub local: Vec<ClusterRow>,
    /// (context, server)
    pub others: Vec<(String, Option<String>)>,
    /// `vm`, `rootless`, `root` when one runs.
    pub env_mode: Option<String>,
    pub env_memory: Option<u64>,
    pub env_cpus: Option<usize>,
    pub env_memory_mib: Option<u64>,
}

/// The dev cluster as the supervisor's state file and API tell it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DevStatus {
    #[default]
    Unknown,
    Down,
    Starting,
    Up,
    Stopping,
    /// No local dev cluster; the endpoints point elsewhere.
    Remote,
}

impl DevStatus {
    pub fn label(self) -> &'static str {
        match self {
            DevStatus::Unknown => "checking",
            DevStatus::Down => "down",
            DevStatus::Starting => "starting",
            DevStatus::Up => "up",
            DevStatus::Stopping => "stopping",
            DevStatus::Remote => "remote",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Contexts {
    pub docker_host_env: Option<String>,
    pub docker_host_is_litek8s: bool,
    pub docker_context: String,
    pub docker_context_is_litek8s: bool,
    pub kube_context: String,
    pub kube_is_litek8s: bool,
    pub kubeconfig: String,
}

/// One sample of the dev cluster's state and use.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HomeSample {
    pub status: DevStatus,
    pub mode: String,
    pub uptime_s: u64,
    pub api: String,
    pub vm_ip: Option<String>,
    pub cpu_pct: Option<f64>,
    pub mem_used: Option<u64>,
    pub disk_used: Option<u64>,
    pub disk_free: Option<u64>,
    pub data_dir: String,
    /// What the running cluster was started with.
    pub running: Option<crate::dev::DevState>,
    /// The memory figure explained, when it can be.
    pub memory: Option<crate::dev::memory::Breakdown>,
    /// CPU counters (the App turns two into percentages: it knows the
    /// pods' and containers' names).
    pub cpu_sample: Option<crate::dev::cpu::Sample>,
    /// The node's disk by category, litek8s's own files, and (macOS) the
    /// VM disk image's allocated bytes.
    pub disk_node: Option<(crate::dev::disk::NodeDisk, u64, Option<u64>)>,
    pub contexts: Contexts,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputKind {
    Filter,
    Setting(Field),
}

/// The fields of the new-cluster form.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NewCluster {
    pub name: String,
    pub nodes: String,
    /// 0: name, 1: nodes.
    pub field: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Cpus,
    Memory,
    Shares,
    Port,
    Publish,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    None,
    Help,
    Confirm { title: String, lines: Vec<String>, effect: Effect },
    Input { prompt: String, value: String, kind: InputKind, error: Option<String> },
    NewCluster(NewCluster),
}

pub struct LogView {
    pub id: u64,
    pub title: String,
    pub lines: VecDeque<String>,
    pub follow: bool,
    /// Lines up from the bottom when not following.
    pub offset: usize,
    pub ended: Option<String>,
    pub wrap: bool,
}

pub const LOG_LINES: usize = 5000;

#[derive(Debug, Clone, PartialEq)]
pub enum CRow {
    Group { name: String, count: usize, running: usize, collapsed: bool },
    Item(usize),
}

/// A reclaimable category of the free-space view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reclaim {
    StoppedContainers,
    DanglingImages,
    UnusedImages,
    BuildCache,
    UnusedVolumes,
}

pub const RECLAIMS: [Reclaim; 5] = [Reclaim::StoppedContainers, Reclaim::DanglingImages, Reclaim::BuildCache, Reclaim::UnusedImages, Reclaim::UnusedVolumes];

impl Reclaim {
    pub fn title(self) -> &'static str {
        match self {
            Reclaim::StoppedContainers => "Stopped containers",
            Reclaim::DanglingImages => "Dangling images (untagged, unused)",
            Reclaim::UnusedImages => "Unused images (no container or pod)",
            Reclaim::BuildCache => "Build cache",
            Reclaim::UnusedVolumes => "Unused volumes (data is lost)",
        }
    }
}

pub struct App {
    pub tab: Tab,
    pub overlay: Overlay,
    pub logs: Option<LogView>,
    next_log: u64,
    pub status: Option<(String, bool)>,
    pub now: i64,
    pub docker_endpoint: String,
    pub kube_endpoint: String,
    pub kube_context: String,

    pub docker_error: Option<String>,
    pub docker_version: String,
    pub docker_os: String,
    pub vm_cpus: Option<u64>,
    pub vm_memory: Option<u64>,
    pub containers: Vec<Container>,
    pub images: Vec<Image>,
    pub volumes: Vec<Volume>,
    pub disk: DiskUsage,
    pub collapsed: BTreeSet<String>,

    pub kube_error: Option<String>,
    /// Every pod (Home counts them, names cgroups and images by them).
    pub pods: Vec<Pod>,
    /// The Kubernetes view: a cluster UI of its own (src/ui/kube).
    pub kube: KubeUi,

    pub home: HomeSample,
    /// The local cluster the views show (its Docker socket and API).
    pub cluster: String,
    /// A kubeconfig context the Kubernetes view shows instead (read-only).
    pub context_override: Option<String>,
    pub clusters: ClustersSample,
    /// The CPU figure explained (from the last two samples).
    pub cpu: Option<crate::dev::cpu::Breakdown>,
    cpu_prev: Option<crate::dev::cpu::Sample>,
    /// Which breakdown Home shows first: 0 memory, 1 CPU, 2 disk (`b`).
    pub explain: usize,
    pub dev_busy: Option<DevOp>,
    pub dev_output: VecDeque<String>,

    pub config: DevConfig,
    pub config_error: Option<String>,
    pub config_path: String,

    pub selected: [usize; 8],
    pub filters: [String; 8],
    pub space_checked: BTreeSet<Reclaim>,
    pub mouse: bool,
    /// Screen rows of the current list's first row and tab titles' spans
    /// (for mouse clicks), set while drawing.
    pub list_top: std::cell::Cell<u16>,
    /// The first list row on screen (the table scrolls with the selection).
    pub list_offset: std::cell::Cell<usize>,
    pub tab_spans: std::cell::RefCell<Vec<(u16, u16)>>,
    /// The screen row of the tab titles and the header's height (one line
    /// on short terminals, a box of three on tall ones).
    pub tab_row: std::cell::Cell<u16>,
    pub header_h: std::cell::Cell<u16>,
    /// Home's CPU and memory over the last minutes (shares of the
    /// cluster's CPUs and memory), one per sample taken while Home is on
    /// screen: the sparklines.
    pub history: VecDeque<(f32, f32)>,
}

/// Samples the sparklines keep (8 minutes at Home's 2 s).
pub const HISTORY: usize = 240;

impl App {
    pub fn new(docker_endpoint: String, kube_endpoint: String, kube_context: String) -> Self {
        App {
            tab: Tab::Home,
            overlay: Overlay::None,
            logs: None,
            next_log: 1,
            status: None,
            now: chrono::Utc::now().timestamp(),
            docker_endpoint,
            kube_endpoint,
            kube_context,
            docker_error: None,
            docker_version: String::new(),
            docker_os: String::new(),
            vm_cpus: None,
            vm_memory: None,
            containers: Vec::new(),
            images: Vec::new(),
            volumes: Vec::new(),
            disk: DiskUsage::default(),
            collapsed: BTreeSet::new(),
            kube_error: None,
            pods: Vec::new(),
            kube: KubeUi::new(None),
            home: HomeSample::default(),
            cluster: crate::dev::cluster::DEFAULT.into(),
            context_override: None,
            clusters: ClustersSample::default(),
            cpu: None,
            cpu_prev: None,
            explain: 0,
            dev_busy: None,
            dev_output: VecDeque::new(),
            config: DevConfig::default(),
            config_error: None,
            config_path: crate::dev::config::path().display().to_string(),
            selected: [0; 8],
            filters: Default::default(),
            space_checked: [Reclaim::StoppedContainers, Reclaim::DanglingImages, Reclaim::BuildCache].into_iter().collect(),
            mouse: false,
            list_top: std::cell::Cell::new(0),
            list_offset: std::cell::Cell::new(0),
            tab_spans: std::cell::RefCell::new(Vec::new()),
            tab_row: std::cell::Cell::new(0),
            header_h: std::cell::Cell::new(1),
            history: VecDeque::new(),
        }
    }

    pub fn filter(&self) -> &str {
        &self.filters[self.tab.index()]
    }

    fn matches(&self, text: &[&str]) -> bool {
        let f = self.filter().to_lowercase();
        f.is_empty() || text.iter().any(|t| t.to_lowercase().contains(&f))
    }

    /// The look (ui.yaml's theme, this terminal's colors).
    pub fn theme(&self) -> &super::theme::Theme {
        &self.kube.theme
    }

    /// The CPUs Home's figures are shares of: a Linux sandbox's limit, else
    /// what the engine sees (the VM's).
    pub fn home_cpus(&self) -> Option<f64> {
        let limits = self.home.running.as_ref().filter(|s| s.mode != "vm");
        limits.and_then(|s| s.cpus).map(|c| c as u64).or(self.vm_cpus).map(|c| c as f64)
    }

    /// The memory Home's figure is a share of (bytes), the same way.
    pub fn home_memory(&self) -> Option<u64> {
        let limits = self.home.running.as_ref().filter(|s| s.mode != "vm");
        limits.and_then(|s| s.memory_mib).map(|m| m << 20).or(self.vm_memory)
    }

    pub fn sel(&self) -> usize {
        self.selected[self.tab.index()]
    }

    // ---- what each list shows ----

    pub fn container_rows(&self) -> Vec<CRow> {
        let visible: Vec<usize> = (0..self.containers.len())
            .filter(|&i| {
                let c = &self.containers[i];
                self.matches(&[&c.name, &c.image, c.project.as_deref().unwrap_or(""), &c.state])
            })
            .collect();
        let mut rows: Vec<CRow> = visible.iter().filter(|&&i| self.containers[i].project.is_none()).map(|&i| CRow::Item(i)).collect();
        let mut projects: Vec<&str> = visible.iter().filter_map(|&i| self.containers[i].project.as_deref()).collect();
        projects.dedup();
        for p in projects {
            let members: Vec<usize> = visible.iter().copied().filter(|&i| self.containers[i].project.as_deref() == Some(p)).collect();
            let collapsed = self.collapsed.contains(p);
            rows.push(CRow::Group { name: p.to_string(), count: members.len(), running: members.iter().filter(|&&i| self.containers[i].running()).count(), collapsed });
            if !collapsed {
                rows.extend(members.into_iter().map(CRow::Item));
            }
        }
        rows
    }

    pub fn visible_images(&self) -> Vec<usize> {
        (0..self.images.len()).filter(|&i| self.matches(&[&self.images[i].name(), self.images[i].short_id()])).collect()
    }

    pub fn visible_volumes(&self) -> Vec<usize> {
        (0..self.volumes.len()).filter(|&i| self.matches(&[&self.volumes[i].name, self.volumes[i].project.as_deref().unwrap_or("")])).collect()
    }

    /// Bytes and items each free-space category would reclaim.
    pub fn reclaimable(&self, r: Reclaim) -> (u64, usize) {
        match r {
            Reclaim::StoppedContainers => {
                let stopped: Vec<&Container> = self.containers.iter().filter(|c| !c.running()).collect();
                (stopped.iter().map(|c| self.disk.container_sizes.get(&c.id).copied().or(c.size_rw).unwrap_or(0)).sum(), stopped.len())
            }
            Reclaim::DanglingImages => {
                let imgs: Vec<&Image> = self.images.iter().filter(|i| i.dangling() && i.unused()).collect();
                (imgs.iter().map(|i| i.size).sum(), imgs.len())
            }
            Reclaim::UnusedImages => {
                let imgs: Vec<&Image> = self.images.iter().filter(|i| !i.dangling() && i.unused()).collect();
                (imgs.iter().map(|i| i.size).sum(), imgs.len())
            }
            Reclaim::BuildCache => (self.disk.build_cache, self.disk.build_cache_records),
            Reclaim::UnusedVolumes => {
                let vols: Vec<&Volume> = self.volumes.iter().filter(|v| v.users.is_empty()).collect();
                (vols.iter().map(|v| v.size.unwrap_or(0)).sum(), vols.len())
            }
        }
    }

    fn prune_effects(&self) -> Vec<Effect> {
        let mut out = Vec::new();
        for r in &self.space_checked {
            if self.reclaimable(*r).1 == 0 {
                continue;
            }
            out.push(match r {
                Reclaim::StoppedContainers => Effect::PruneContainers,
                Reclaim::DanglingImages => Effect::RemoveImages(self.images.iter().filter(|i| i.dangling() && i.unused()).map(|i| i.id.clone()).collect()),
                Reclaim::UnusedImages => Effect::RemoveImages(self.images.iter().filter(|i| !i.dangling() && i.unused()).map(|i| i.id.clone()).collect()),
                Reclaim::BuildCache => Effect::PruneBuildCache,
                Reclaim::UnusedVolumes => Effect::RemoveVolumes(self.volumes.iter().filter(|v| v.users.is_empty()).map(|v| v.name.clone()).collect()),
            });
        }
        out
    }

    pub fn list_len(&self) -> usize {
        match self.tab {
            Tab::Home => 0,
            Tab::Clusters => self.clusters.local.len() + self.clusters.others.len(),
            Tab::Containers => self.container_rows().len(),
            Tab::Images => self.visible_images().len(),
            Tab::Volumes => self.visible_volumes().len(),
            Tab::Space => RECLAIMS.len(),
            Tab::Kube => 0,
            Tab::Settings => SETTINGS.len(),
        }
    }

    fn clamp(&mut self) {
        let len = self.list_len();
        let i = self.tab.index();
        if self.selected[i] >= len {
            self.selected[i] = len.saturating_sub(1);
        }
    }

    fn move_sel(&mut self, delta: isize) {
        let len = self.list_len();
        if len == 0 {
            return;
        }
        let i = self.tab.index();
        self.selected[i] = (self.selected[i] as isize + delta).clamp(0, len as isize - 1) as usize;
    }

    pub fn selected_container(&self) -> Option<&Container> {
        match self.container_rows().get(self.sel())? {
            CRow::Item(i) => self.containers.get(*i),
            CRow::Group { .. } => None,
        }
    }

    /// The containers the selection stands for: one, or a whole project.
    fn selected_containers(&self) -> (String, Vec<&Container>) {
        match self.container_rows().get(self.sel()) {
            Some(CRow::Item(i)) => {
                let c = &self.containers[*i];
                (c.name.clone(), vec![c])
            }
            Some(CRow::Group { name, .. }) => (format!("project {name}"), self.containers.iter().filter(|c| c.project.as_deref() == Some(name)).collect()),
            None => (String::new(), Vec::new()),
        }
    }

    pub fn selected_image(&self) -> Option<&Image> {
        self.visible_images().get(self.sel()).map(|&i| &self.images[i])
    }

    pub fn selected_volume(&self) -> Option<&Volume> {
        self.visible_volumes().get(self.sel()).map(|&i| &self.volumes[i])
    }

    /// Settings that differ from what the running cluster started with.
    pub fn restart_needed(&self) -> Vec<&'static str> {
        let Some(state) = &self.home.running else { return Vec::new() };
        let mut out = Vec::new();
        if self.config.port.is_some_and(|p| p != state.port) {
            out.push("port");
        }
        if state.publish.as_deref().unwrap_or("localhost") != self.config.publish.as_str() {
            out.push("published ports");
        }
        if state.mode != "vm" {
            // Linux: limits on the sandbox, none unless set.
            if state.cpus != self.config.cpus {
                out.push("CPUs");
            }
            if state.memory_mib != self.config.memory {
                out.push("memory");
            }
        } else {
            if state.cpus.is_some_and(|c| c != self.config.cpus.unwrap_or_else(crate::dev::config::default_cpus)) {
                out.push("CPUs");
            }
            if state.memory_mib.is_some_and(|m| m != self.config.memory.unwrap_or(crate::dev::config::DEFAULT_MEMORY_MIB)) {
                out.push("memory");
            }
            let canon = |p: &std::path::PathBuf| p.canonicalize().unwrap_or_else(|_| p.clone());
            let want: Vec<_> = self.config.effective_shares().iter().map(canon).collect();
            let have: Vec<_> = state.shares.iter().map(canon).collect();
            if !have.is_empty() && want != have {
                out.push("shared directories");
            }
        }
        out
    }

    pub fn setting_value(&self, f: Field) -> String {
        match f {
            Field::Cpus => self.config.cpus.map(|c| c.to_string()).unwrap_or_default(),
            Field::Memory => self.config.memory.map(|c| c.to_string()).unwrap_or_default(),
            Field::Shares => self.config.shares.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
            Field::Port => self.config.port.map(|c| c.to_string()).unwrap_or_default(),
            Field::Publish => self.config.publish.as_str().to_string(),
        }
    }

    /// The settings with `f` set from `value`, validated.
    pub fn with_setting(&self, f: Field, value: &str) -> Result<DevConfig, String> {
        let mut c = self.config.clone();
        let v = value.trim();
        let num = |what: &str| -> Result<Option<u64>, String> { if v.is_empty() { Ok(None) } else { v.parse::<u64>().map(Some).map_err(|_| format!("{what} must be a number")) } };
        match f {
            Field::Cpus => c.cpus = num("CPUs")?.map(|n| n as usize),
            Field::Memory => c.memory = num("memory")?,
            Field::Port => c.port = num("port")?.map(|n| u16::try_from(n).map_err(|_| "port must be at most 65535".to_string())).transpose()?,
            Field::Shares => {
                c.shares = v
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        let home = || std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
                        match s.strip_prefix("~/") {
                            Some(rest) => home().join(rest),
                            None if s == "~" => home(),
                            None => std::path::PathBuf::from(s),
                        }
                    })
                    .collect()
            }
            Field::Publish => c.publish = Publish::parse(v).map_err(|e| e.to_string())?,
        }
        let problems = c.problems();
        if problems.is_empty() { Ok(c) } else { Err(problems.join("; ")) }
    }

    pub fn flash(&mut self, text: impl Into<String>, error: bool) {
        self.status = Some((text.into(), error));
    }

    // ---- messages ----

    pub fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Containers(c) => {
                self.docker_error = None;
                self.containers = c;
                self.relink();
            }
            Msg::Images(i) => {
                self.images = i;
                self.relink();
            }
            Msg::Volumes(v) => {
                self.volumes = v;
                self.relink();
            }
            Msg::Disk(d) => {
                self.disk = d;
                self.relink();
            }
            Msg::DockerInfo { version, cpus, memory, os } => {
                self.docker_version = version;
                self.vm_cpus = cpus;
                self.vm_memory = memory;
                self.docker_os = os;
            }
            Msg::DockerDown(e) => {
                self.docker_error = Some(e);
                self.vm_cpus = None;
                self.vm_memory = None;
                self.docker_version.clear();
                self.containers.clear();
                self.images.clear();
                self.volumes.clear();
                self.disk = DiskUsage::default();
            }
            Msg::Pods(p) => {
                self.kube_error = None;
                self.pods = p;
                self.relink();
            }
            Msg::KubeDown(e) => {
                self.kube_error = Some(e);
                self.pods.clear();
                self.relink();
            }
            Msg::Kube(m) => {
                self.kube.now = self.now;
                self.kube.apply(m);
                if let Some((text, error)) = self.kube.flash.take() {
                    self.flash(text, error);
                }
            }
            Msg::Home(sample) => {
                let mut sample = *sample;
                if let Some(op) = self.dev_busy.as_ref().filter(|op| op.cluster() == self.cluster) {
                    sample.status = if op.starts() { DevStatus::Starting } else { DevStatus::Stopping };
                }
                if let Some(now) = sample.cpu_sample.take() {
                    if let Some(prev) = &self.cpu_prev {
                        let name = |id: &str| self.cgroup_name(id);
                        if let Some(b) = crate::dev::cpu::breakdown(prev, &now, &self.cluster, &name) {
                            self.cpu = Some(b);
                        }
                    }
                    self.cpu_prev = Some(now);
                } else if sample.status != DevStatus::Up {
                    self.cpu = None;
                    self.cpu_prev = None;
                }
                self.home = sample;
                // Only while Home shows them: samples come every 2 s then,
                // once a minute otherwise (no gaps of a minute in a line
                // of 2 s steps).
                if self.tab == Tab::Home && self.logs.is_none() && self.home.status == DevStatus::Up {
                    let cpu = self.home.cpu_pct.zip(self.home_cpus()).map(|(p, c)| (p / 100.0 / c.max(1.0)) as f32);
                    let mem = self.home.mem_used.zip(self.home_memory()).map(|(u, t)| (u as f64 / t.max(1) as f64) as f32);
                    if let (Some(c), Some(m)) = (cpu, mem) {
                        if self.history.len() == HISTORY {
                            self.history.pop_front();
                        }
                        self.history.push_back((c, m));
                    }
                } else if self.home.status != DevStatus::Up {
                    self.history.clear();
                }
            }
            Msg::Clusters(sample) => {
                let mut sample = *sample;
                if let Some(op) = &self.dev_busy {
                    if let Some(row) = sample.local.iter_mut().find(|r| r.name == op.cluster()) {
                        row.status = if op.starts() { DevStatus::Starting } else { DevStatus::Stopping };
                    }
                }
                self.clusters = sample;
            }
            Msg::LogLines(id, lines) => {
                if let Some(v) = self.logs.as_mut().filter(|v| v.id == id) {
                    let added = lines.len();
                    v.lines.extend(lines);
                    while v.lines.len() > LOG_LINES {
                        v.lines.pop_front();
                    }
                    if !v.follow {
                        v.offset = (v.offset + added).min(v.lines.len());
                    }
                }
            }
            Msg::LogEnd(id, err) => {
                if let Some(v) = self.logs.as_mut().filter(|v| v.id == id) {
                    v.ended = Some(err.unwrap_or_else(|| "stream ended".into()));
                }
            }
            Msg::Done(Ok(m)) => self.flash(m, false),
            Msg::Done(Err(e)) => self.flash(e, true),
            Msg::DevOutput(line) => {
                self.dev_output.push_back(line);
                while self.dev_output.len() > 50 {
                    self.dev_output.pop_front();
                }
            }
            Msg::DevFinished(r) => {
                // The shown cluster is gone: back to the default one.
                if let Some(DevOp::Delete(c)) = &self.dev_busy {
                    if *c == self.cluster && r.is_ok() {
                        self.cluster = crate::dev::cluster::DEFAULT.into();
                    }
                }
                self.dev_busy = None;
                match r {
                    Ok(m) => self.flash(m, false),
                    Err(e) => self.flash(e, true),
                }
            }
            Msg::Config(Ok(c)) => {
                self.config = c;
                self.config_error = None;
            }
            Msg::Config(Err(e)) => self.config_error = Some(e),
        }
        self.clamp();
    }

    /// The disk figure explained: the node's categories, with what the
    /// Free space view can reclaim.
    pub fn disk_breakdown(&self) -> Option<crate::dev::disk::Breakdown> {
        let (n, own, image) = self.home.disk_node.as_ref()?;
        let r = crate::dev::disk::Reclaimable {
            images: self.reclaimable(Reclaim::DanglingImages).0 + self.reclaimable(Reclaim::UnusedImages).0 + self.reclaimable(Reclaim::BuildCache).0,
            containers: self.reclaimable(Reclaim::StoppedContainers).0,
            volumes: self.reclaimable(Reclaim::UnusedVolumes).0,
        };
        Some(match (self.home.status, image) {
            (DevStatus::Remote, _) => crate::dev::disk::node_breakdown(n, &r),
            (_, Some(img)) => crate::dev::disk::vm_breakdown(*own, *img, n, &r, &self.cluster),
            (_, None) => crate::dev::disk::host_breakdown(*own, n, &r),
        })
    }

    /// A pod's or container's name for its cgroup (`pod<uid>`,
    /// `docker/<id>`, `build/<id>`).
    pub fn cgroup_name(&self, id: &str) -> String {
        if let Some(uid) = id.strip_prefix("pod") {
            let uid = uid.replace('_', "-");
            return match self.pods.iter().find(|p| p.uid == uid) {
                Some(p) => format!("pod {}/{}", p.namespace, p.name),
                None => format!("pod {}", &uid[..uid.len().min(8)]),
            };
        }
        if let Some(cid) = id.strip_prefix("docker/") {
            return match self.containers.iter().find(|c| c.id.starts_with(cid) || cid.starts_with(&c.id)) {
                Some(c) => format!("container {}", c.name),
                None => format!("container {}", &cid[..cid.len().min(12)]),
            };
        }
        match id.strip_prefix("build/") {
            Some(b) => format!("build {}", &b[..b.len().min(12)]),
            None => id.to_string(),
        }
    }

    fn relink(&mut self) {
        model::link_images(&mut self.images, &self.containers, &self.pods);
        model::link_volumes(&mut self.volumes, &self.disk, &self.containers);
    }

    fn open_logs(&mut self, source: LogSource) -> Vec<Effect> {
        let title = match &source {
            LogSource::Container { name, .. } => format!("logs: {name}"),
        };
        let id = self.next_log;
        self.next_log += 1;
        self.logs = Some(LogView { id, title, lines: VecDeque::new(), follow: true, offset: 0, ended: None, wrap: false });
        vec![Effect::Logs(source)]
    }

    /// The id of the log view's stream (the runtime tags lines with it).
    pub fn log_id(&self) -> Option<u64> {
        self.logs.as_ref().map(|l| l.id)
    }

    fn confirm(&mut self, title: impl Into<String>, lines: Vec<String>, effect: Effect) {
        self.overlay = Overlay::Confirm { title: title.into(), lines, effect };
    }

    fn input(&mut self, prompt: impl Into<String>, value: String, kind: InputKind) {
        self.overlay = Overlay::Input { prompt: prompt.into(), value, kind, error: None };
    }

    // ---- keys ----

    pub fn key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return vec![Effect::Quit];
        }
        match std::mem::replace(&mut self.overlay, Overlay::None) {
            Overlay::None => {}
            Overlay::Help => {
                // Any key closes the help.
                return Vec::new();
            }
            Overlay::Confirm { title, lines, effect } => {
                return match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                        if let Effect::Dev(op) = &effect {
                            self.busy(op.clone());
                        }
                        vec![effect]
                    }
                    KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') => Vec::new(),
                    _ => {
                        self.overlay = Overlay::Confirm { title, lines, effect };
                        Vec::new()
                    }
                };
            }
            o @ Overlay::NewCluster(_) => self.overlay = o,
            Overlay::Input { prompt, mut value, kind, error } => {
                match key.code {
                    KeyCode::Esc => {
                        if kind == InputKind::Filter {
                            self.filters[self.tab.index()].clear();
                        }
                        return Vec::new();
                    }
                    KeyCode::Enter => return self.submit(prompt, value, kind),
                    KeyCode::Backspace => {
                        value.pop();
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => value.clear(),
                    KeyCode::Char(c) => value.push(c),
                    _ => {}
                }
                if kind == InputKind::Filter {
                    self.filters[self.tab.index()] = value.clone();
                    self.clamp();
                }
                self.overlay = Overlay::Input { prompt, value, kind, error: if key.code == KeyCode::Enter { error } else { None } };
                return Vec::new();
            }
        }
        if let Overlay::NewCluster(form) = std::mem::replace(&mut self.overlay, Overlay::None) {
            return self.new_cluster_key(form, key);
        }
        if self.logs.is_some() {
            return self.log_key(key);
        }
        if self.tab == Tab::Kube {
            self.kube.now = self.now;
            let handled = self.kube.key(key);
            if let Some((text, error)) = self.kube.flash.take() {
                self.flash(text, error);
            }
            if let Some(effects) = handled {
                return effects;
            }
        }
        match key.code {
            KeyCode::Char('q') => return vec![Effect::Quit],
            KeyCode::Char('?') | KeyCode::F(1) => self.overlay = Overlay::Help,
            KeyCode::Char(c @ '1'..='8') => self.tab = TABS[c as usize - '1' as usize],
            KeyCode::Tab => self.tab = TABS[(self.tab.index() + 1) % TABS.len()],
            KeyCode::BackTab => self.tab = TABS[(self.tab.index() + TABS.len() - 1) % TABS.len()],
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(-(self.list_len() as isize)),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(self.list_len() as isize),
            KeyCode::Char('/') if !matches!(self.tab, Tab::Home | Tab::Settings | Tab::Space | Tab::Clusters) => {
                let current = self.filter().to_string();
                self.input("filter", current, InputKind::Filter);
            }
            KeyCode::Esc if !self.filter().is_empty() => {
                self.filters[self.tab.index()].clear();
                self.clamp();
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => return vec![Effect::Refresh],
            _ => return self.tab_key(key),
        }
        Vec::new()
    }

    fn log_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(v) = self.logs.as_mut() else { return Vec::new() };
        let len = v.lines.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.logs = None;
                return vec![Effect::StopLogs];
            }
            KeyCode::Char('f') => {
                v.follow = !v.follow;
                v.offset = 0;
            }
            KeyCode::Char('w') => v.wrap = !v.wrap,
            KeyCode::Up | KeyCode::Char('k') => {
                v.follow = false;
                v.offset = (v.offset + 1).min(len);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                v.offset = v.offset.saturating_sub(1);
                v.follow = v.offset == 0;
            }
            KeyCode::PageUp => {
                v.follow = false;
                v.offset = (v.offset + 20).min(len);
            }
            KeyCode::PageDown => {
                v.offset = v.offset.saturating_sub(20);
                v.follow = v.offset == 0;
            }
            KeyCode::Char('g') | KeyCode::Home => {
                v.follow = false;
                v.offset = len;
            }
            KeyCode::Char('G') | KeyCode::End => {
                v.follow = true;
                v.offset = 0;
            }
            KeyCode::Char('c') => v.lines.clear(),
            KeyCode::Char('?') => self.overlay = Overlay::Help,
            _ => {}
        }
        Vec::new()
    }

    fn tab_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let code = key.code;
        match self.tab {
            Tab::Home => match code {
                KeyCode::Char('b') => self.explain = (self.explain + 1) % 3,
                KeyCode::Char('u') if self.dev_busy.is_none() => {
                    let op = DevOp::Up { cluster: self.cluster.clone(), switch: true };
                    self.busy(op.clone());
                    return vec![Effect::Dev(op)];
                }
                KeyCode::Char('d') if self.dev_busy.is_none() => {
                    let n = self.containers.iter().filter(|c| c.running()).count();
                    self.confirm_down(self.cluster.clone(), n);
                }
                KeyCode::Char('R') if self.dev_busy.is_none() => self.confirm_delete(self.cluster.clone()),
                _ => {}
            },
            Tab::Clusters => return self.clusters_key(code),
            Tab::Containers => return self.containers_key(code),
            Tab::Images => {
                if matches!(code, KeyCode::Char('d') | KeyCode::Delete) {
                    if let Some(img) = self.selected_image() {
                        let mut lines = vec![format!("{} ({}, {})", img.name(), img.short_id(), model::bytes(img.size))];
                        if !img.containers.is_empty() {
                            lines.push(format!("Used by containers: {}", img.containers.join(", ")));
                            lines.push("Remove those containers first.".into());
                        }
                        if !img.pods.is_empty() {
                            lines.push(format!("Used by pods: {} (they will pull it again)", img.pods.join(", ")));
                        }
                        let id = img.id.clone();
                        self.confirm("Remove image?", lines, Effect::RemoveImages(vec![id]));
                    }
                }
            }
            Tab::Volumes => {
                if matches!(code, KeyCode::Char('d') | KeyCode::Delete) {
                    if let Some(v) = self.selected_volume() {
                        let mut lines = vec![format!("{} ({})", v.name, v.size.map(model::bytes).unwrap_or_else(|| "size unknown".into())), "Its data is deleted.".into()];
                        if !v.users.is_empty() {
                            lines.push(format!("In use by: {} (remove them first)", v.users.join(", ")));
                        }
                        let name = v.name.clone();
                        self.confirm("Remove volume?", lines, Effect::RemoveVolumes(vec![name]));
                    }
                }
            }
            Tab::Space => match code {
                KeyCode::Char(' ') | KeyCode::Char('x') => {
                    let r = RECLAIMS[self.sel().min(RECLAIMS.len() - 1)];
                    if !self.space_checked.remove(&r) {
                        self.space_checked.insert(r);
                    }
                }
                KeyCode::Char('p') | KeyCode::Enter => {
                    let effects = self.prune_effects();
                    if effects.is_empty() {
                        self.flash("nothing to reclaim in the selected categories", false);
                    } else {
                        let mut lines: Vec<String> = Vec::new();
                        let mut total = 0;
                        for r in &self.space_checked {
                            let (b, n) = self.reclaimable(*r);
                            if n > 0 {
                                total += b;
                                lines.push(format!("{}: {n} ({})", r.title(), model::bytes(b)));
                            }
                        }
                        lines.push(format!("About {} freed. This cannot be undone.", model::bytes(total)));
                        self.confirm("Free space?", lines, Effect::Batch(effects));
                    }
                }
                _ => {}
            },
            Tab::Kube => {}
            Tab::Settings => return self.settings_key(code),
        }
        Vec::new()
    }

    fn busy(&mut self, op: DevOp) {
        if op.cluster() == self.cluster {
            self.home.status = if op.starts() { DevStatus::Starting } else { DevStatus::Stopping };
        }
        if let Some(row) = self.clusters.local.iter_mut().find(|r| r.name == op.cluster()) {
            row.status = if op.starts() { DevStatus::Starting } else { DevStatus::Stopping };
        }
        self.dev_busy = Some(op);
        self.dev_output.clear();
    }

    fn confirm_down(&mut self, cluster: String, running_containers: usize) {
        let mut lines = vec![
            format!("Stops {}: its nodes, pods and Docker containers.", context_of(&cluster)),
            "Objects, images and volumes are kept for the next `up`.".to_string(),
        ];
        if running_containers > 0 {
            lines.push(format!("{running_containers} running container(s) will stop."));
        }
        self.confirm(format!("Stop cluster {cluster}?"), lines, Effect::Dev(DevOp::Down(cluster)));
    }

    fn confirm_delete(&mut self, cluster: String) {
        let place = data_place(&cluster);
        self.confirm(
            format!("Delete cluster {cluster}?"),
            vec![
                "Stops it and DELETES all of its data:".into(),
                "Kubernetes objects, pods, Docker containers, images, volumes".into(),
                format!("and the build cache, in {place}."),
                format!("The kubectl context {} is removed. This cannot be undone.", context_of(&cluster)),
            ],
            Effect::Dev(DevOp::Delete(cluster)),
        );
    }

    /// The Clusters view's selection: a local cluster or another context.
    pub fn selected_cluster(&self) -> Option<Result<&ClusterRow, &(String, Option<String>)>> {
        let i = self.sel();
        match self.clusters.local.get(i) {
            Some(r) => Some(Ok(r)),
            None => self.clusters.others.get(i - self.clusters.local.len()).map(Err),
        }
    }

    fn clusters_key(&mut self, code: KeyCode) -> Vec<Effect> {
        if code == KeyCode::Char('n') {
            if self.dev_busy.is_some() {
                self.flash("wait for the running `litek8s dev` to finish", true);
                return Vec::new();
            }
            let taken: Vec<&str> = self.clusters.local.iter().map(|r| r.name.as_str()).collect();
            let name = (2..).map(|i| format!("dev{i}")).find(|n| !taken.contains(&n.as_str())).unwrap_or_default();
            self.overlay = Overlay::NewCluster(NewCluster { name, nodes: "1".into(), field: 0, error: None });
            return Vec::new();
        }
        let Some(sel) = self.selected_cluster() else { return Vec::new() };
        let row = match sel {
            Ok(row) => row.clone(),
            Err((context, _)) => {
                if code == KeyCode::Enter {
                    let context = context.clone();
                    self.context_override = Some(context.clone());
                    self.flash(format!("Kubernetes view: context {context} (read-only here; enter on a local cluster to go back)"), false);
                    return vec![Effect::UseContext(context)];
                }
                if matches!(code, KeyCode::Char('u' | 'd' | 'D')) {
                    self.flash("only local clusters can be started, stopped or deleted here", true);
                }
                return Vec::new();
            }
        };
        let idle = self.dev_busy.is_none();
        match code {
            KeyCode::Enter => {
                self.context_override = None;
                let switch = self.cluster != row.name;
                self.cluster = row.name.clone();
                let mut effects = vec![Effect::UseCluster(row.name.clone())];
                if switch || !self.home.contexts.kube_is_litek8s {
                    self.confirm(
                        format!("Point kubectl and docker at {}?", row.name),
                        vec![
                            format!("The views show {} now.", row.name),
                            format!("Also make {} kubectl's current context", row.context),
                            format!("and select the docker context {}?", crate::dev::cluster::Cluster { name: row.name.clone() }.docker_context()),
                        ],
                        Effect::SwitchTools(row.name.clone()),
                    );
                } else {
                    effects.push(Effect::Refresh);
                }
                return effects;
            }
            KeyCode::Char('u') if idle && matches!(row.status, DevStatus::Down) => {
                let op = DevOp::Up { cluster: row.name.clone(), switch: false };
                self.busy(op.clone());
                return vec![Effect::Dev(op)];
            }
            KeyCode::Char('u') if idle => self.flash(format!("{} is {}", row.name, row.status.label()), false),
            KeyCode::Char('d') if idle && !matches!(row.status, DevStatus::Down) => {
                let n = if row.name == self.cluster { self.containers.iter().filter(|c| c.running()).count() } else { 0 };
                self.confirm_down(row.name.clone(), n);
            }
            KeyCode::Char('d') if idle => self.flash(format!("{} is not running", row.name), false),
            KeyCode::Char('D') | KeyCode::Delete if idle => self.confirm_delete(row.name.clone()),
            KeyCode::Char('u' | 'd' | 'D') => self.flash("wait for the running `litek8s dev` to finish", true),
            _ => {}
        }
        Vec::new()
    }

    fn new_cluster_key(&mut self, mut form: NewCluster, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Esc => return Vec::new(),
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Down | KeyCode::Up => form.field = 1 - form.field.min(1),
            KeyCode::Backspace => {
                if form.field == 0 { form.name.pop() } else { form.nodes.pop() };
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if form.field == 0 { form.name.clear() } else { form.nodes.clear() }
            }
            KeyCode::Char(c) if form.field == 0 => form.name.push(c),
            KeyCode::Char(c) if c.is_ascii_digit() => form.nodes.push(c),
            KeyCode::Enter => {
                let name = form.name.trim().to_string();
                let nodes = form.nodes.trim().parse::<u32>().ok().filter(|n| (1..=10).contains(n));
                let error = match (crate::dev::cluster::validate(&name), nodes) {
                    (Err(e), _) => Some(e.to_string()),
                    (_, None) => Some("nodes: 1 to 10".into()),
                    _ if self.clusters.local.iter().any(|r| r.name == name) => Some(format!("{name} exists already (u starts it)")),
                    _ => None,
                };
                if let (None, Some(nodes)) = (&error, nodes) {
                    let op = DevOp::Create { cluster: name, nodes };
                    self.busy(op.clone());
                    return vec![Effect::Dev(op)];
                }
                form.error = error;
            }
            _ => {}
        }
        if key.code != KeyCode::Enter {
            form.error = None;
        }
        self.overlay = Overlay::NewCluster(form);
        Vec::new()
    }

    fn containers_key(&mut self, code: KeyCode) -> Vec<Effect> {
        let (what, list) = self.selected_containers();
        if list.is_empty() {
            return Vec::new();
        }
        let ids: Vec<String> = list.iter().map(|c| c.id.clone()).collect();
        let single = self.selected_container().cloned();
        match code {
            KeyCode::Enter | KeyCode::Char(' ') if single.is_none() => {
                if let Some(CRow::Group { name, .. }) = self.container_rows().get(self.sel()) {
                    let name = name.clone();
                    if !self.collapsed.remove(&name) {
                        self.collapsed.insert(name);
                    }
                }
            }
            KeyCode::Enter | KeyCode::Char('l') => {
                if let Some(c) = single {
                    return self.open_logs(LogSource::Container { id: c.id, name: c.name });
                }
            }
            KeyCode::Char('s') | KeyCode::Char('e') => match single {
                Some(c) if c.state == "running" => return vec![Effect::Exec(c.id)],
                Some(c) => self.flash(format!("{} is not running", c.name), true),
                None => {}
            },
            KeyCode::Char('u') => return vec![Effect::Containers(ContainerOp::Start, ids)],
            KeyCode::Char('x') => self.confirm(format!("Stop {what}?"), list_lines(&list), Effect::Containers(ContainerOp::Stop, ids)),
            KeyCode::Char('r') => self.confirm(format!("Restart {what}?"), list_lines(&list), Effect::Containers(ContainerOp::Restart, ids)),
            KeyCode::Char('d') | KeyCode::Delete => {
                let mut lines = list_lines(&list);
                if list.iter().any(|c| c.running()) {
                    lines.push("Running containers are stopped first.".into());
                }
                lines.push("Their files outside volumes are deleted.".into());
                self.confirm(format!("Remove {what}?"), lines, Effect::Containers(ContainerOp::Remove, ids));
            }
            _ => {}
        }
        Vec::new()
    }

    fn settings_key(&mut self, code: KeyCode) -> Vec<Effect> {
        let row = SETTINGS[self.sel().min(SETTINGS.len() - 1)];
        match (code, row) {
            (KeyCode::Enter | KeyCode::Char('e'), Setting::Field(Field::Publish)) | (KeyCode::Char(' '), Setting::Field(Field::Publish)) => {
                let mut c = self.config.clone();
                c.publish = if c.publish == Publish::All { Publish::Localhost } else { Publish::All };
                return vec![Effect::SaveConfig(c)];
            }
            (KeyCode::Enter | KeyCode::Char('e'), Setting::Field(f)) => {
                let value = self.setting_value(f);
                self.input(field_prompt(f), value, InputKind::Setting(f));
            }
            (KeyCode::Backspace | KeyCode::Delete, Setting::Field(f)) => {
                // Back to the default.
                if let Ok(c) = self.with_setting(f, if f == Field::Publish { "localhost" } else { "" }) {
                    return vec![Effect::SaveConfig(c)];
                }
            }
            (KeyCode::Enter | KeyCode::Char(' '), Setting::DockerContext) => return vec![Effect::DockerContext(!self.home.contexts.docker_context_is_litek8s)],
            (KeyCode::Enter | KeyCode::Char(' '), Setting::KubeContext) => return vec![Effect::KubeContext(!self.home.contexts.kube_is_litek8s)],
            (KeyCode::Enter | KeyCode::Char('e'), Setting::EditFile) => return vec![Effect::EditConfig],
            _ => {}
        }
        Vec::new()
    }

    fn submit(&mut self, prompt: String, value: String, kind: InputKind) -> Vec<Effect> {
        let fail = |app: &mut App, e: String| {
            app.overlay = Overlay::Input { prompt: prompt.clone(), value: value.clone(), kind: kind.clone(), error: Some(e) };
            Vec::new()
        };
        match &kind {
            InputKind::Filter => Vec::new(),
            InputKind::Setting(f) => match self.with_setting(*f, &value) {
                Ok(c) => vec![Effect::SaveConfig(c)],
                Err(e) => fail(self, e),
            },
        }
    }

    // ---- mouse ----

    pub fn mouse(&mut self, ev: MouseEvent) -> Vec<Effect> {
        if self.overlay != Overlay::None {
            return Vec::new();
        }
        if self.tab == Tab::Kube && self.logs.is_none() && ev.row >= self.header_h.get() {
            return self.kube.mouse(ev, self.list_top.get());
        }
        match ev.kind {
            MouseEventKind::ScrollDown => {
                if self.logs.is_some() {
                    return self.log_key(KeyEvent::from(KeyCode::Down));
                }
                self.move_sel(3);
            }
            MouseEventKind::ScrollUp => {
                if self.logs.is_some() {
                    return self.log_key(KeyEvent::from(KeyCode::Up));
                }
                self.move_sel(-3);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if ev.row == self.tab_row.get() {
                    let spans = self.tab_spans.borrow().clone();
                    if let Some(i) = spans.iter().position(|(a, b)| ev.column >= *a && ev.column < *b) {
                        self.tab = TABS[i];
                    }
                    return Vec::new();
                }
                let top = self.list_top.get();
                if self.logs.is_none() && top > 0 && ev.row >= top {
                    let i = (ev.row - top) as usize + self.list_offset.get();
                    if i < self.list_len() {
                        let idx = self.tab.index();
                        self.selected[idx] = i;
                    }
                }
            }
            _ => {}
        }
        Vec::new()
    }
}

fn context_of(cluster: &str) -> String {
    format!("litek8s-{cluster}")
}

/// Where a cluster's data is, for the delete confirmation.
fn data_place(cluster: &str) -> String {
    let c = crate::dev::cluster::Cluster { name: cluster.to_string() };
    if cfg!(target_os = "macos") {
        return format!("the shared VM's disk ({})", c.guest_data_dir().display());
    }
    let home = std::env::var("HOME").unwrap_or_default();
    match crate::dev::dev_home() {
        Ok(h) if !home.is_empty() => c.data_dir(&h).display().to_string().replacen(&home, "~", 1),
        Ok(h) => c.data_dir(&h).display().to_string(),
        Err(_) => "the dev data directory".into(),
    }
}

fn list_lines(list: &[&Container]) -> Vec<String> {
    let mut lines: Vec<String> = list.iter().take(6).map(|c| format!("{} ({}, {})", c.name, c.image, c.state)).collect();
    if list.len() > 6 {
        lines.push(format!("and {} more", list.len() - 6));
    }
    lines
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Setting {
    Field(Field),
    DockerContext,
    KubeContext,
    EditFile,
}

pub const SETTINGS: [Setting; 8] = [
    Setting::Field(Field::Cpus),
    Setting::Field(Field::Memory),
    Setting::Field(Field::Shares),
    Setting::Field(Field::Port),
    Setting::Field(Field::Publish),
    Setting::DockerContext,
    Setting::KubeContext,
    Setting::EditFile,
];

pub fn field_prompt(f: Field) -> &'static str {
    match f {
        Field::Cpus => if cfg!(target_os = "macos") { "VM CPUs (empty: half the Mac's)" } else { "CPU limit (empty: none)" },
        Field::Memory => if cfg!(target_os = "macos") { "VM memory in MiB (empty: 4096)" } else { "memory limit in MiB (empty: none)" },
        Field::Shares => "shared directories, comma-separated (empty: $HOME)",
        Field::Port => "API server port of the cluster dev (empty: 6443)",
        Field::Publish => "published ports: localhost or all",
    }
}
