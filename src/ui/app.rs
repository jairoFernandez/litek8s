//! The UI's state and what keys do to it. Nothing here does I/O: a key
//! changes the state and returns [`Effect`]s the runtime carries out, and
//! what the backends learn arrives as [`Msg`]s. That keeps all of it
//! testable without a terminal, Docker or a cluster.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::kube::{KEffect, KMsg, KubeUi};
use super::model::{self, Container, DiskUsage, Image, Pod, ProjectUse, Volume};
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
    Observe,
    /// Argo CD's Applications (shown when the cluster serves them).
    Apps,
}

pub const TABS: [Tab; 10] = [Tab::Home, Tab::Clusters, Tab::Containers, Tab::Images, Tab::Volumes, Tab::Space, Tab::Kube, Tab::Settings, Tab::Observe, Tab::Apps];

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
            Tab::Observe => "Observe",
            Tab::Apps => "Apps",
        }
    }

    /// The key that shows it (and the number the tab bar gives it):
    /// `1`-`9`, then `0`.
    pub fn key(self) -> char {
        let i = self.index();
        if i == 9 { '0' } else { char::from(b'1' + i as u8) }
    }

    /// This machine's Docker (the dev clusters' engine): not part of a
    /// remote litek8s.
    pub fn docker(self) -> bool {
        matches!(self, Tab::Containers | Tab::Images | Tab::Volumes | Tab::Space)
    }

    pub(crate) fn index(self) -> usize {
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
    /// A search across every pod (the API server's log search).
    Search { namespace: Option<String>, pod: Option<String>, grep: String },
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
    /// Show this kubeconfig context in the Kubernetes view (read-only
    /// unless it is a litek8s cluster or in `writableContexts`).
    UseContext(String),
    /// Point kubectl and docker at this local cluster.
    SwitchTools(String),
    /// The Kubernetes view's.
    Kube(KEffect),
    /// Silence these alerts until they resolve.
    AckAlerts(Vec<AlertRow>),
    /// The Apps view drills into this app (namespace, name), or back out:
    /// its resources' health is read from the live objects while it does.
    ArgoFocus(Option<(String, String)>),
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
    /// Compose projects' use against their budgets, and the containers the
    /// OOM killer stopped (litek8s's Docker API).
    Projects { at: std::time::Instant, projects: Vec<ProjectUse>, oom_killed: Vec<String> },
    /// The engine's `oom` event.
    Oom { name: String, project: Option<String>, scope: String, memory: String },
    Images(Vec<Image>),
    Volumes(Vec<Volume>),
    Disk(DiskUsage),
    DockerInfo { version: String, cpus: Option<u64>, memory: Option<u64>, os: String },
    DockerDown(String),
    Pods(Vec<Pod>),
    /// The nodes' allocatable resources (Home's reserved figures).
    Nodes(Vec<super::model::NodeAlloc>),
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
    Obs(Box<ObsSample>),
    Argo(Box<super::argo::ArgoSample>),
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
    /// Which of `others` are litek8s API servers (by their `/version`);
    /// missing while unknown or unreachable.
    pub others_litek8s: std::collections::BTreeMap<String, bool>,
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

/// A pod's CPU (cores) and memory (bytes) over the Observe view's window,
/// one value per step (`None`: no sample then); the last is now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PodUsage {
    pub namespace: String,
    pub name: String,
    pub cpu: Vec<Option<f64>>,
    pub mem: Vec<Option<f64>>,
    /// From the pod's spec: CPU in cores, memory in bytes.
    pub cpu_bounds: Bounds,
    pub mem_bounds: Bounds,
}

/// A pod's limit and request for one resource, summed over the containers
/// that run side by side. `limit` is `None` when any of them has no limit
/// (the pod is not capped); `request` is `None` when none asks for any.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Bounds {
    pub limit: Option<f64>,
    pub request: Option<f64>,
}

impl Bounds {
    /// What `used` is measured against: the limit, else the request
    /// (`true`: it is the request).
    pub fn of(&self) -> Option<(f64, bool)> {
        match (self.limit, self.request) {
            (Some(l), _) if l > 0.0 => Some((l, false)),
            (_, Some(r)) if r > 0.0 => Some((r, true)),
            _ => None,
        }
    }
}

impl PodUsage {
    /// Sampled in the last two steps (a pod deleted in the window is not).
    pub fn live(&self) -> bool {
        [&self.cpu, &self.mem].iter().any(|v| v.iter().rev().take(2).any(Option::is_some))
    }

    pub fn cpu_now(&self) -> f64 {
        self.cpu.iter().rev().take(2).find_map(|v| *v).unwrap_or(0.0)
    }

    pub fn mem_now(&self) -> f64 {
        self.mem.iter().rev().take(2).find_map(|v| *v).unwrap_or(0.0)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AlertRow {
    /// firing, pending
    pub state: String,
    pub name: String,
    pub target: String,
    pub severity: String,
    /// Active since (unix seconds).
    pub since: i64,
    pub summary: String,
    pub silenced: bool,
    pub fingerprint: String,
    /// (namespace, pod) when it is about a pod.
    pub pod: Option<(String, String)>,
    pub labels: std::collections::BTreeMap<String, String>,
}

/// One sample of the Observe view.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ObsSample {
    pub pods: Vec<PodUsage>,
    pub alerts: Vec<AlertRow>,
    pub error: Option<String>,
    /// When (unix seconds).
    pub at: i64,
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
    /// What the VM or sandbox has: CPUs and memory (bytes), what the
    /// cluster's figures are a share of (never the Docker engine's /info,
    /// which may be another machine's).
    pub env_cpus: Option<f64>,
    pub env_memory: Option<u64>,
    /// macOS: the VM's data disk image, (bytes the Mac has allocated to
    /// it, its size).
    pub vm_disk: Option<(u64, u64)>,
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
    /// A remote litek8s: every node's machine (the App adds them up or
    /// picks one, see [`App::scope`]).
    pub nodes: Option<NodeReports>,
    /// A remote cluster that is not litek8s (k3s, kubeadm, a managed
    /// one): its nodes from the public API (see `ui::generic`).
    pub generic: Option<super::generic::Cluster>,
}

/// What each node of a remote litek8s reports about its machine.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeReports {
    pub memory: Vec<crate::dev::memory::NodeMemory>,
    pub cpu: Vec<crate::dev::cpu::NodeCpu>,
    /// From the last walk (every 30 s while on screen).
    pub disk: Vec<crate::dev::disk::NodeDisk>,
    /// Nodes that did not report, and why.
    pub errors: Vec<(String, String)>,
}

impl NodeReports {
    /// Every node named in a report, in the API server's order.
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let all = self.memory.iter().map(|m| &m.node).chain(self.cpu.iter().map(|c| &c.node)).chain(self.disk.iter().map(|d| &d.node)).chain(self.errors.iter().map(|(n, _)| n));
        for n in all {
            if !out.contains(n) {
                out.push(n.clone());
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputKind {
    Filter,
    /// A regex for a log search across pods.
    LogSearch,
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
    Disk,
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
    /// Sync Argo CD apps: `p` and `f` turn prune and force on and off.
    Sync { objs: Vec<super::kube::state::ObjRef>, prune: bool, force: bool },
}

/// What the Apps view's detail lists under the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ArgoPane {
    #[default]
    Resources,
    History,
    /// The last operation's result per resource.
    Result,
}

impl ArgoPane {
    pub fn title(self) -> &'static str {
        match self {
            ArgoPane::Resources => "resources",
            ArgoPane::History => "sync history",
            ArgoPane::Result => "last sync result",
        }
    }
}

/// The Apps view: Argo CD's Applications (or ApplicationSets), and one
/// app's resources, history and last sync.
#[derive(Debug, Clone, Default)]
pub struct ArgoView {
    pub sample: super::argo::ArgoSample,
    /// The app drilled into (namespace, name).
    pub detail: Option<(String, String)>,
    pub pane: ArgoPane,
    /// ApplicationSets instead of Applications.
    pub sets: bool,
    /// The list's selection while the detail shows.
    list_sel: usize,
    /// A describe or diff opened in the Kubernetes view from here: closing
    /// it (back to this many views) comes back.
    back_to: Option<usize>,
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
    /// The Docker views read this machine's dev cluster while the kube
    /// context is a remote litek8s (`--context` without `--docker-host`):
    /// its Docker API being down is no error of the cluster on screen.
    pub docker_is_local_dev: bool,
    /// The Docker views show the shown cluster's own Docker API (its
    /// socket), whose containers run on the cluster's node: Home can say
    /// what they take. Not with `--docker-host` another engine.
    pub docker_is_cluster: bool,
    /// `--context` of a remote litek8s: Clusters starts on its row.
    pub focus_context: Option<String>,
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
    /// Compose projects by name (budgets, use) and when they were sampled.
    pub projects: BTreeMap<String, ProjectUse>,
    projects_at: Option<std::time::Instant>,
    /// Containers whose last run ended in an OOM kill.
    pub oom_killed: BTreeSet<String>,

    pub kube_error: Option<String>,
    /// Every pod (Home counts them, names cgroups and images by them).
    pub pods: Vec<Pod>,
    /// The nodes' allocatable resources, for what the pods reserve.
    pub nodes_alloc: Vec<super::model::NodeAlloc>,
    /// The Kubernetes view: a cluster UI of its own (src/ui/kube).
    pub kube: KubeUi,

    pub home: HomeSample,
    /// The local cluster the views show (its Docker socket and API).
    pub cluster: String,
    /// A kubeconfig context the Kubernetes view shows instead.
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

    pub selected: [usize; TABS.len()],
    pub filters: [String; TABS.len()],
    /// The Observe view: pods' use and alerts; sorted by memory or CPU.
    pub obs: ObsSample,
    pub obs_by_mem: bool,
    /// The Observe view's namespace (`None`: all).
    pub obs_namespace: Option<String>,
    /// The Apps view (Argo CD Applications).
    pub argo: ArgoView,
    pub space_checked: BTreeSet<Reclaim>,
    pub mouse: bool,
    /// What the header shows next to the name when this is not the plain
    /// release: `DEV <commit>` and the `$LITEK8S_HOME` in use (empty: none).
    pub build: String,
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
    /// A remote litek8s with several nodes: Home shows the whole cluster
    /// (`None`) or one node (`n`/`N` cycle).
    pub scope: Option<String>,
    /// The last reports of every node (see [`HomeSample::nodes`]).
    pub node_reports: NodeReports,
    /// Per scope ("" the whole cluster, else a node): the last CPU sample,
    /// its breakdown and the sparklines' history, kept for every scope so
    /// switching shows them at once.
    scoped_cpu_prev: HashMap<String, crate::dev::cpu::Sample>,
    pub scoped_cpu: HashMap<String, crate::dev::cpu::Breakdown>,
    scoped_history: HashMap<String, VecDeque<(f32, f32)>>,
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
            docker_is_local_dev: false,
            docker_is_cluster: true,
            focus_context: None,
            docker_version: String::new(),
            docker_os: String::new(),
            vm_cpus: None,
            vm_memory: None,
            containers: Vec::new(),
            images: Vec::new(),
            volumes: Vec::new(),
            disk: DiskUsage::default(),
            collapsed: BTreeSet::new(),
            projects: BTreeMap::new(),
            projects_at: None,
            oom_killed: BTreeSet::new(),
            kube_error: None,
            pods: Vec::new(),
            nodes_alloc: Vec::new(),
            kube: KubeUi::new(None),
            home: HomeSample::default(),
            cluster: crate::dev::cluster::DEFAULT.into(),
            context_override: None,
            clusters: ClustersSample::default(),
            cpu: None,
            cpu_prev: None,
            scope: None,
            node_reports: NodeReports::default(),
            scoped_cpu_prev: HashMap::new(),
            scoped_cpu: HashMap::new(),
            scoped_history: HashMap::new(),
            explain: 0,
            dev_busy: None,
            dev_output: VecDeque::new(),
            config: DevConfig::default(),
            config_error: None,
            config_path: crate::dev::config::path().display().to_string(),
            selected: [0; TABS.len()],
            filters: Default::default(),
            obs: ObsSample::default(),
            obs_by_mem: false,
            obs_namespace: None,
            argo: ArgoView::default(),
            space_checked: [Reclaim::StoppedContainers, Reclaim::DanglingImages, Reclaim::BuildCache].into_iter().collect(),
            mouse: false,
            build: String::new(),
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
    /// The tabs in the bar: a remote litek8s has no use for this machine's
    /// Docker tabs (they come back with a dev cluster, in 2 Clusters).
    /// Each keeps its number either way.
    pub fn visible_tabs(&self) -> Vec<Tab> {
        TABS.iter().copied().filter(|t| !(t.docker() && self.home.status == DevStatus::Remote) && (*t != Tab::Apps || self.argo_res().is_some())).collect()
    }

    fn go_tab(&mut self, tab: Tab) {
        if tab.docker() && self.home.status == DevStatus::Remote {
            self.flash(format!("{} is this machine's Docker, not {}'s: pick a dev cluster in 2 Clusters to use it", tab.title(), self.kube_context), false);
            return;
        }
        if tab == Tab::Apps && self.argo_res().is_none() {
            let why = if self.kube.discovered { format!("{} serves no Argo CD Applications (applications.argoproj.io): no Argo CD or litecd there", self.kube_context) } else { "Apps shows once the cluster's API is known (it serves applications.argoproj.io)".to_string() };
            self.flash(why, false);
            return;
        }
        self.tab = tab;
    }

    /// Argo CD's Application type, when the cluster serves it.
    pub fn argo_res(&self) -> Option<super::kube::resources::Resource> {
        self.kube.catalog.list.iter().find(|r| super::kube::gitops::is_app(r)).cloned()
    }

    fn step_tab(&mut self, step: isize) {
        let tabs = self.visible_tabs();
        let pos = tabs.iter().position(|t| *t == self.tab).unwrap_or(0) as isize;
        self.tab = tabs[(pos + step).rem_euclid(tabs.len() as isize) as usize];
    }

    /// The CPUs Home's figure is a share of: the sandbox's limit, else the
    /// VM's or the sandbox's machine's. Not the Docker engine's: the Docker
    /// views may show another engine (`--docker-host`).
    pub fn home_cpus(&self) -> Option<f64> {
        let limits = self.home.running.as_ref().filter(|s| s.mode != "vm");
        limits.and_then(|s| s.cpus).map(|c| c as f64).or(self.home.env_cpus)
    }

    /// The memory Home's figure is a share of (bytes), the same way.
    pub fn home_memory(&self) -> Option<u64> {
        let limits = self.home.running.as_ref().filter(|s| s.mode != "vm");
        limits.and_then(|s| s.memory_mib).map(|m| m << 20).or(self.home.env_memory)
    }

    /// The cluster's CPU, % of one CPU: the VM or sandbox less what the
    /// Docker API's containers took (they run there, but are not the
    /// cluster's).
    pub fn cluster_cpu_pct(&self) -> Option<f64> {
        let docker = self.cpu.as_ref().filter(|_| self.home.status != DevStatus::Remote).map(|c| c.docker_pct).unwrap_or(0.0);
        self.home.cpu_pct.map(|p| (p - docker).max(0.0))
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
            Tab::Observe => self.visible_obs_pods().len(),
            Tab::Apps => self.argo_len(),
        }
    }

    /// The Observe view's pods: filtered, top consumers first.
    pub fn visible_obs_pods(&self) -> Vec<usize> {
        let mut v: Vec<usize> = (0..self.obs.pods.len())
            .filter(|&i| {
                let p = &self.obs.pods[i];
                self.obs_namespace.as_ref().is_none_or(|n| *n == p.namespace) && self.matches(&[&p.name, &p.namespace])
            })
            .collect();
        // Live pods first, top consumers first; pods gone in the window last.
        let key = |i: &usize| {
            let p = &self.obs.pods[*i];
            (p.live(), if self.obs_by_mem { p.mem_now() } else { p.cpu_now() })
        };
        v.sort_by(|a, b| key(b).partial_cmp(&key(a)).unwrap_or(std::cmp::Ordering::Equal));
        v
    }

    pub fn selected_obs_pod(&self) -> Option<&PodUsage> {
        self.visible_obs_pods().get(self.sel()).map(|i| &self.obs.pods[*i])
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
            // The image grows when the VM starts; a smaller size is ignored.
            if self.home.vm_disk.is_some_and(|(_, size)| self.config.disk.is_some_and(|d| d << 30 > size)) {
                out.push("disk");
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
            Field::Disk => self.config.disk.map(|c| c.to_string()).unwrap_or_default(),
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
            Field::Disk => c.disk = num("disk")?,
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
            Msg::Projects { at, projects, oom_killed } => {
                let secs = self.projects_at.map(|p| at.saturating_duration_since(p).as_secs_f64()).filter(|s| *s > 0.2);
                let mut next = BTreeMap::new();
                for mut p in projects {
                    if let (Some(secs), Some(prev)) = (secs, self.projects.get(&p.name)) {
                        p.cpu_rate = (p.cpu_usec >= prev.cpu_usec).then(|| (p.cpu_usec - prev.cpu_usec) as f64 / 1e6 / secs);
                    }
                    next.insert(p.name.clone(), p);
                }
                self.projects = next;
                self.projects_at = Some(at);
                self.oom_killed = oom_killed.into_iter().collect();
            }
            Msg::Oom { name, project, scope, memory } => {
                let why = match (scope.as_str(), project) {
                    ("project", Some(p)) if !memory.is_empty() => format!("compose project {p} hit its {memory} budget"),
                    ("project", Some(p)) => format!("compose project {p} hit its memory budget"),
                    ("container", _) => "it hit its memory limit".to_string(),
                    _ => "the machine ran out of memory".to_string(),
                };
                self.flash(format!("{name} was OOM-killed: {why}"), true);
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
                self.publish_node_alloc();
            }
            Msg::Nodes(n) => {
                self.nodes_alloc = n;
                self.publish_node_alloc();
            }
            Msg::KubeDown(e) => {
                self.kube_error = Some(e);
                self.pods.clear();
                // Another cluster (or none): the Apps view starts over.
                self.argo = ArgoView::default();
                self.relink();
            }
            Msg::Kube(m) => {
                if matches!(&m, KMsg::Config(Ok(c)) if !c.show_other_contexts) {
                    self.clusters.others.clear();
                }
                self.kube.now = self.now;
                self.kube.apply(m);
                if let Some((text, error)) = self.kube.flash.take() {
                    self.flash(text, error);
                }
                // A cluster without Argo CD's Applications has no Apps view.
                if self.tab == Tab::Apps && self.kube.discovered && self.argo_res().is_none() {
                    self.tab = Tab::Home;
                }
            }
            Msg::Home(sample) => {
                let mut sample = *sample;
                if let Some(op) = self.dev_busy.as_ref().filter(|op| op.cluster() == self.cluster) {
                    sample.status = if op.starts() { DevStatus::Starting } else { DevStatus::Stopping };
                }
                if sample.status == DevStatus::Remote && self.tab.docker() {
                    self.tab = Tab::Home;
                }
                if sample.generic.is_some() {
                    self.home = sample;
                    self.absorb_generic();
                    self.apply_scope();
                    return;
                }
                if let Some(reports) = sample.nodes.take() {
                    self.absorb_nodes(reports);
                    self.home = sample;
                    self.apply_scope();
                    return;
                } else if sample.status != DevStatus::Remote {
                    self.node_reports = NodeReports::default();
                    self.scope = None;
                }
                if let Some(now) = sample.cpu_sample.take() {
                    if let Some(prev) = &self.cpu_prev {
                        let name = |id: &str| self.cgroup_name(id);
                        if let Some(b) = crate::dev::cpu::breakdown(prev, &now, &self.cluster, &name) {
                            self.cpu = Some(b);
                        }
                    }
                    self.cpu_prev = Some(now);
                } else if !matches!(sample.status, DevStatus::Up | DevStatus::Remote) {
                    self.cpu = None;
                    self.cpu_prev = None;
                }
                self.home = sample;
                // Only while Home shows them: samples come every 2 s then,
                // once a minute otherwise (no gaps of a minute in a line
                // of 2 s steps).
                // A remote litek8s graphs too: its CPU from the breakdown
                // of its API server's counters (no local process to time).
                let sampled = matches!(self.home.status, DevStatus::Up | DevStatus::Remote);
                if self.tab == Tab::Home && self.logs.is_none() && sampled {
                    let remote_cpu = || self.cpu.as_ref().filter(|_| self.home.status == DevStatus::Remote).map(|c| (c.total_pct / 100.0 / c.cpus.max(1.0)) as f32);
                    let cpu = self.cluster_cpu_pct().zip(self.home_cpus()).map(|(p, c)| (p / 100.0 / c.max(1.0)) as f32).or_else(remote_cpu);
                    // Memory in use against its limit when the breakdown says
                    // (the footprint also counts pages the guest freed).
                    let in_use = self.home.memory.as_ref().and_then(|b| b.limit.filter(|l| *l > 0).map(|l| (b.cluster_in_use() as f64 / l as f64) as f32));
                    let mem = in_use.or_else(|| self.home.mem_used.zip(self.home_memory()).map(|(u, t)| (u as f64 / t.max(1) as f64) as f32));
                    if let (Some(c), Some(m)) = (cpu, mem) {
                        if self.history.len() == HISTORY {
                            self.history.pop_front();
                        }
                        self.history.push_back((c, m));
                    }
                } else if !sampled {
                    self.history.clear();
                }
            }
            Msg::Obs(sample) => {
                self.obs = *sample;
                if self.tab == Tab::Observe {
                    self.clamp();
                }
            }
            Msg::Argo(sample) => {
                self.argo.sample = *sample;
                if self.argo.sets && self.argo.sample.appsets.as_ref().is_none_or(Vec::is_empty) {
                    self.argo.sets = false;
                }
                if self.tab == Tab::Apps {
                    self.clamp();
                }
            }
            Msg::Clusters(sample) => {
                let mut sample = *sample;
                // `showOtherContexts: false`: litek8s's clusters only.
                if !self.kube.config.show_other_contexts {
                    sample.others.clear();
                }
                if let Some(op) = &self.dev_busy {
                    if let Some(row) = sample.local.iter_mut().find(|r| r.name == op.cluster()) {
                        row.status = if op.starts() { DevStatus::Starting } else { DevStatus::Stopping };
                    }
                }
                self.clusters = sample;
                // Started on a remote context: select it once its row is listed.
                if let Some(ctx) = self.focus_context.take() {
                    let local = self.clusters.local.iter().position(|r| r.context == ctx);
                    let other = self.clusters.others.iter().position(|(c, _)| *c == ctx).map(|i| i + self.clusters.local.len());
                    match local.or(other) {
                        Some(i) => self.selected[Tab::Clusters.index()] = i,
                        None => self.focus_context = Some(ctx),
                    }
                }
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
    /// Take in every node's reports: CPU percentages per scope (from the
    /// previous sample of each), and the sparklines of every scope.
    fn absorb_nodes(&mut self, r: NodeReports) {
        use crate::dev::{cpu, memory};
        let names = r.names();
        // The whole cluster ("") and each node.
        let mut cpus: Vec<(String, cpu::NodeCpu)> = vec![(String::new(), cpu::sum(&r.cpu, "cluster"))];
        cpus.extend(r.cpu.iter().map(|c| (c.node.clone(), c.clone())));
        for (key, node) in cpus {
            let Some(now) = node.sample() else { continue };
            if let Some(prev) = self.scoped_cpu_prev.get(&key) {
                let name = |id: &str| self.cgroup_name(id);
                if let Some(mut b) = cpu::breakdown(prev, &now, &self.cluster, &name) {
                    b.what = self.scope_what(&key, &names);
                    self.scoped_cpu.insert(key.clone(), b);
                }
            }
            self.scoped_cpu_prev.insert(key, now);
        }
        let mut mems: Vec<(String, memory::NodeMemory)> = vec![(String::new(), memory::sum(&r.memory, "cluster"))];
        mems.extend(r.memory.iter().map(|m| (m.node.clone(), m.clone())));
        // Every scope's sparklines, while Home is on screen.
        if self.tab == Tab::Home && self.logs.is_none() {
            for (key, m) in mems {
                let b = memory::node_breakdown(&m);
                let mem = b.limit.filter(|l| *l > 0).map(|l| (b.in_use() as f64 / l as f64) as f32);
                let cpu = self.scoped_cpu.get(&key).map(|c| (c.total_pct / 100.0 / c.cpus.max(1.0)) as f32);
                if let (Some(c), Some(m)) = (cpu, mem) {
                    let h = self.scoped_history.entry(key).or_default();
                    if h.len() == HISTORY {
                        h.pop_front();
                    }
                    h.push_back((c, m));
                }
            }
        }
        // Forget nodes that left.
        self.scoped_cpu_prev.retain(|k, _| k.is_empty() || names.contains(k));
        self.scoped_cpu.retain(|k, _| k.is_empty() || names.contains(k));
        self.scoped_history.retain(|k, _| k.is_empty() || names.contains(k));
        if self.scope.as_ref().is_some_and(|s| !names.contains(s)) {
            self.scope = None;
        }
        self.node_reports = r;
    }

    /// A cluster that is not litek8s: every scope's history (the whole
    /// cluster and each node: use against allocatable), while Home is on
    /// screen; forget nodes that left.
    fn absorb_generic(&mut self) {
        let Some(g) = &self.home.generic else { return };
        let names = g.names();
        if self.tab == Tab::Home && self.logs.is_none() {
            let scopes = std::iter::once(None).chain(names.iter().map(|n| Some(n.as_str())));
            for scope in scopes {
                let t = g.totals(scope);
                if let (Some(c), Some(m)) = (t.cpu_ratio(), t.mem_ratio()) {
                    let h = self.scoped_history.entry(scope.unwrap_or_default().to_string()).or_default();
                    if h.len() == HISTORY {
                        h.pop_front();
                    }
                    h.push_back((c as f32, m as f32));
                }
            }
        }
        self.scoped_history.retain(|k, _| k.is_empty() || names.contains(k));
        if self.scope.as_ref().is_some_and(|s| !names.contains(s)) {
            self.scope = None;
        }
    }

    /// The nodes `n` cycles through: a litek8s's reports, or the Nodes of
    /// a cluster that is not litek8s.
    pub fn scope_names(&self) -> Vec<String> {
        match &self.home.generic {
            Some(g) => g.names(),
            None => self.node_reports.names(),
        }
    }

    /// Whose numbers Home shows: `cluster · 3 nodes` or `node <name>`.
    fn scope_what(&self, key: &str, names: &[String]) -> String {
        match key {
            "" if names.len() == 1 => format!("node {}", names[0]),
            "" => format!("cluster · {} nodes", names.len()),
            n => format!("node {n}"),
        }
    }

    /// Home's figures for the scope on screen: the cluster's (every node
    /// added up) or one node's.
    pub fn apply_scope(&mut self) {
        use crate::dev::{disk, memory};
        if self.home.status != DevStatus::Remote {
            return;
        }
        if self.home.generic.is_some() {
            // Its figures are worked out when drawn (`generic::Cluster::totals`).
            self.cpu = None;
            self.history = self.scoped_history.get(self.scope.as_deref().unwrap_or_default()).cloned().unwrap_or_default();
            return;
        }
        let r = &self.node_reports;
        let names = r.names();
        let key = self.scope.clone().unwrap_or_default();
        let what = self.scope_what(&key, &names);
        let one = |n: &String| self.scope.as_ref().is_none_or(|s| s == n);
        let mem: Vec<memory::NodeMemory> = r.memory.iter().filter(|m| one(&m.node)).cloned().collect();
        let limit_what = if self.scope.is_none() && names.len() > 1 { "cluster RAM" } else { "node RAM" };
        self.home.memory = (!mem.is_empty()).then(|| memory::scoped_breakdown(&memory::sum(&mem, &what), &what, limit_what));
        let disks: Vec<disk::NodeDisk> = r.disk.iter().filter(|d| one(&d.node)).cloned().collect();
        if disks.is_empty() {
            self.home.disk_node = None;
            self.home.disk_used = None;
            self.home.disk_free = None;
        } else {
            let d = disk::sum(&disks, &what);
            self.home.disk_used = Some(d.total());
            // The nodes' own filesystems (not this machine's).
            self.home.disk_free = (d.fs_size > 0).then(|| d.fs_size.saturating_sub(d.fs_used));
            self.home.disk_node = Some((d, 0, None));
        }
        self.cpu = self.scoped_cpu.get(&key).cloned();
        self.history = self.scoped_history.get(&key).cloned().unwrap_or_default();
    }

    /// The next (`step` 1) or previous (-1) scope: the cluster, then each node.
    /// What the pods reserve and may use on the nodes Home shows: the
    /// whole cluster, or the node picked with `n`. `None` before the nodes
    /// are known.
    pub fn allocated(&self) -> Option<super::model::Allocated> {
        if self.nodes_alloc.is_empty() || self.kube_error.is_some() {
            return None;
        }
        let node = self.scope.as_deref().filter(|n| self.nodes_alloc.iter().any(|x| x.name == *n));
        Some(super::model::Allocated::of(&self.pods, &self.nodes_alloc, node))
    }

    /// The Kubernetes view's nodes table gets each node's requests and
    /// limits (with their share of its allocatable), as `kubectl describe
    /// node` sums them: columns CPU REQ, CPU LIM, MEM REQ, MEM LIM.
    fn publish_node_alloc(&mut self) {
        if self.nodes_alloc.is_empty() {
            return;
        }
        let mut cols: [(&str, HashMap<String, String>); 4] = [("CPU REQ", HashMap::new()), ("CPU LIM", HashMap::new()), ("MEM REQ", HashMap::new()), ("MEM LIM", HashMap::new())];
        for n in &self.nodes_alloc {
            let a = super::model::Allocated::of(&self.pods, &self.nodes_alloc, Some(&n.name));
            let share = |v: f64, of: f64| if of > 0.0 { format!("{:.0}%", v / of * 100.0) } else { "–".into() };
            let cpu = |v: f64| format!("{}m {}", (v * 1000.0).round() as u64, share(v, a.cpu));
            let mem = |v: f64| format!("{}Mi {}", (v / 1048576.0).round() as u64, share(v, a.memory));
            cols[0].1.insert(n.name.clone(), cpu(a.cpu_req));
            cols[1].1.insert(n.name.clone(), cpu(a.cpu_lim));
            cols[2].1.insert(n.name.clone(), mem(a.mem_req));
            cols[3].1.insert(n.name.clone(), mem(a.mem_lim));
        }
        for (column, values) in cols {
            self.kube.apply(KMsg::Ext { kind: "Node".into(), column: column.into(), values });
        }
    }

    pub fn cycle_scope(&mut self, step: isize) {
        let names = self.scope_names();
        if names.len() < 2 {
            self.flash("one node: the cluster is that node".to_string(), false);
            return;
        }
        let mut all: Vec<Option<String>> = vec![None];
        all.extend(names.into_iter().map(Some));
        let pos = all.iter().position(|s| *s == self.scope).unwrap_or(0) as isize;
        let next = (pos + step).rem_euclid(all.len() as isize) as usize;
        self.scope = all[next].clone();
        self.apply_scope();
    }

    pub fn disk_breakdown(&self) -> Option<crate::dev::disk::Breakdown> {
        let (n, own, image) = self.home.disk_node.as_ref()?;
        let r = crate::dev::disk::Reclaimable {
            images: self.reclaimable(Reclaim::DanglingImages).0 + self.reclaimable(Reclaim::UnusedImages).0 + self.reclaimable(Reclaim::BuildCache).0,
            containers: self.reclaimable(Reclaim::StoppedContainers).0,
            volumes: self.reclaimable(Reclaim::UnusedVolumes).0,
        };
        Some(match (self.home.status, image) {
            // What a prune would free is this machine's Docker's, not the remote nodes'.
            (DevStatus::Remote, _) => crate::dev::disk::node_breakdown(n, &crate::dev::disk::Reclaimable::default(), &n.node),
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
            LogSource::Search { namespace, pod, grep } => {
                let scope = match (namespace, pod) {
                    (Some(n), Some(p)) => format!("{n}/{p} (every container, gone ones too)"),
                    (Some(n), None) => format!("namespace {n}"),
                    _ => "every pod".to_string(),
                };
                if grep.is_empty() { format!("logs: {scope}, last hour") } else { format!("logs matching /{grep}/: {scope}, last hour") }
            }
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
            Overlay::Sync { objs, prune, force } => {
                return match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => vec![Effect::Kube(KEffect::AppSync { objs, prune, force })],
                    KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') => Vec::new(),
                    KeyCode::Char('p') => {
                        self.overlay = Overlay::Sync { objs, prune: !prune, force };
                        Vec::new()
                    }
                    KeyCode::Char('f') => {
                        let force = !force && !self.argo.sample.litecd();
                        self.overlay = Overlay::Sync { objs, prune, force };
                        Vec::new()
                    }
                    _ => {
                        self.overlay = Overlay::Sync { objs, prune, force };
                        Vec::new()
                    }
                };
            }
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
        if self.tab != Tab::Kube {
            self.argo.back_to = None;
        }
        if self.tab == Tab::Kube {
            self.kube.now = self.now;
            let handled = self.kube.key(key);
            if let Some((text, error)) = self.kube.flash.take() {
                self.flash(text, error);
            }
            // Closing what the Apps view opened goes back to it.
            if self.argo.back_to.is_some_and(|d| self.kube.views.len() <= d) {
                self.argo.back_to = None;
                self.tab = Tab::Apps;
            }
            if let Some(effects) = handled {
                return effects;
            }
        }
        match key.code {
            KeyCode::Char('q') => return vec![Effect::Quit],
            KeyCode::Char('?') | KeyCode::F(1) => self.overlay = Overlay::Help,
            KeyCode::Char(c @ '1'..='9') => self.go_tab(TABS[c as usize - '1' as usize]),
            KeyCode::Char('0') => self.go_tab(Tab::Apps),
            KeyCode::Tab => self.step_tab(1),
            KeyCode::BackTab => self.step_tab(-1),
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
                // The whole cluster or one node (a remote litek8s).
                KeyCode::Char('n') if self.home.status == DevStatus::Remote => self.cycle_scope(1),
                KeyCode::Char('N') if self.home.status == DevStatus::Remote => self.cycle_scope(-1),
                KeyCode::Char('c') => return self.copy(self.kube_endpoint.clone(), "API address"),
                // u, d and R act on this machine's dev clusters: not on the
                // remote litek8s on screen.
                KeyCode::Char('u' | 'd' | 'R') if self.home.status == DevStatus::Remote => {
                    self.flash(format!("{} is a remote litek8s: start, stop or delete it where it was created (u, d and R act on this machine's dev clusters, in 2)", self.kube_context), true);
                }
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
            Tab::Observe => return self.obs_key(code),
            Tab::Apps => return self.argo_key(code),
        }
        Vec::new()
    }

    fn obs_key(&mut self, code: KeyCode) -> Vec<Effect> {
        match code {
            KeyCode::Char('s') => {
                self.obs_by_mem = !self.obs_by_mem;
                return Vec::new();
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                // The namespaces the metrics show: all → each → all.
                let mut seen: Vec<String> = self.obs.pods.iter().map(|p| p.namespace.clone()).collect();
                seen.sort();
                seen.dedup();
                let mut all: Vec<Option<String>> = vec![None];
                all.extend(seen.into_iter().map(Some));
                let pos = all.iter().position(|n| *n == self.obs_namespace).unwrap_or(0);
                let next = if code == KeyCode::Char('n') { (pos + 1) % all.len() } else { (pos + all.len() - 1) % all.len() };
                self.obs_namespace = all[next].clone();
                self.clamp();
                return Vec::new();
            }
            KeyCode::Char('L') => {
                self.input("search logs of every pod, last hour (regex; empty: all lines)", String::new(), InputKind::LogSearch);
                return Vec::new();
            }
            KeyCode::Char('A') => {
                let firing: Vec<AlertRow> = self.obs.alerts.iter().filter(|a| !a.silenced).cloned().collect();
                if firing.is_empty() {
                    self.flash("no unacknowledged alerts", false);
                } else {
                    let lines = firing.iter().map(|a| format!("{} {}", a.name, a.target)).collect();
                    self.confirm("Acknowledge every alert (silence each until it resolves)?", lines, Effect::AckAlerts(firing));
                }
                return Vec::new();
            }
            _ => {}
        }
        let Some(p) = self.selected_obs_pod().cloned() else { return Vec::new() };
        match code {
            KeyCode::Enter | KeyCode::Char('l') => {
                return self.open_logs(LogSource::Search { namespace: Some(p.namespace), pod: Some(p.name), grep: String::new() });
            }
            KeyCode::Char('a') => {
                let mine: Vec<AlertRow> = self.obs.alerts.iter().filter(|a| !a.silenced && a.pod.as_ref() == Some(&(p.namespace.clone(), p.name.clone()))).cloned().collect();
                if mine.is_empty() {
                    self.flash(format!("no unacknowledged alerts about {}/{}", p.namespace, p.name), false);
                } else {
                    let lines = mine.iter().map(|a| format!("{} {}", a.name, a.target)).collect();
                    self.confirm("Acknowledge (silence until resolved)?", lines, Effect::AckAlerts(mine));
                }
            }
            _ => {}
        }
        Vec::new()
    }

    // ---- the Apps view ----

    /// The Applications the list shows (indices into the sample), filtered
    /// by name, namespace, project, sync or health (`/OutOfSync`).
    pub fn visible_argo_apps(&self) -> Vec<usize> {
        let apps = &self.argo.sample.apps;
        (0..apps.len())
            .filter(|&i| {
                let a = &apps[i];
                let project = a.pointer("/spec/project").and_then(serde_json::Value::as_str).unwrap_or_default();
                self.matches(&[super::argo::name(a), super::argo::namespace(a), project, super::argo::sync_status(a), super::argo::health_status(a)])
            })
            .collect()
    }

    /// The ApplicationSets the list shows, filtered.
    pub fn visible_argo_sets(&self) -> Vec<usize> {
        let sets = self.argo.sample.appsets.as_deref().unwrap_or_default();
        (0..sets.len()).filter(|&i| self.matches(&[super::argo::name(&sets[i]), super::argo::namespace(&sets[i])])).collect()
    }

    /// The app the detail shows.
    pub fn argo_detail_app(&self) -> Option<&serde_json::Value> {
        let (ns, name) = self.argo.detail.as_ref()?;
        self.argo.sample.apps.iter().find(|a| super::argo::namespace(a) == ns && super::argo::name(a) == name)
    }

    /// The app the keys act on: the detail's, else the list's selection.
    pub fn argo_selected(&self) -> Option<&serde_json::Value> {
        if self.argo.detail.is_some() {
            return self.argo_detail_app();
        }
        if self.argo.sets {
            return None;
        }
        let i = *self.visible_argo_apps().get(self.selected[Tab::Apps.index()])?;
        self.argo.sample.apps.get(i)
    }

    fn argo_len(&self) -> usize {
        if self.argo.detail.is_some() {
            let Some(app) = self.argo_detail_app() else { return 0 };
            return match self.argo.pane {
                ArgoPane::Resources => super::argo::resources(app).len(),
                ArgoPane::History => super::argo::history(app).len(),
                ArgoPane::Result => super::argo::sync_result(app).len(),
            };
        }
        if self.argo.sets { self.visible_argo_sets().len() } else { self.visible_argo_apps().len() }
    }

    fn argo_obj(&self, app: &serde_json::Value) -> Option<super::kube::state::ObjRef> {
        Some(super::kube::state::ObjRef { res: self.argo_res()?, ns: super::argo::namespace(app).into(), name: super::argo::name(app).into() })
    }

    /// Whether changes may go to this cluster (as the Kubernetes view
    /// judges it); says why not.
    fn argo_writable(&mut self) -> bool {
        let a = self.kube.access();
        if a.writable() {
            return true;
        }
        let why = self.kube.refusal(a);
        self.flash(why, true);
        false
    }

    /// Back from the detail to the list (where it was).
    fn argo_back(&mut self) -> Vec<Effect> {
        self.argo.detail = None;
        self.argo.pane = ArgoPane::Resources;
        self.selected[Tab::Apps.index()] = self.argo.list_sel;
        self.clamp();
        vec![Effect::ArgoFocus(None)]
    }

    fn argo_key(&mut self, code: KeyCode) -> Vec<Effect> {
        let detail = self.argo.detail.is_some();
        match code {
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left if detail => return self.argo_back(),
            KeyCode::Char('A') if !detail => {
                if self.argo.sample.appsets.as_ref().is_none_or(Vec::is_empty) {
                    self.flash("no ApplicationSets on this cluster", false);
                } else {
                    self.argo.sets = !self.argo.sets;
                    self.selected[Tab::Apps.index()] = 0;
                }
                return Vec::new();
            }
            KeyCode::Char('h') if detail => {
                self.argo.pane = match self.argo.pane {
                    ArgoPane::Resources => ArgoPane::History,
                    ArgoPane::History => ArgoPane::Result,
                    ArgoPane::Result => ArgoPane::Resources,
                };
                self.selected[Tab::Apps.index()] = 0;
                return Vec::new();
            }
            _ => {}
        }
        let Some(app) = self.argo_selected().cloned() else { return Vec::new() };
        let Some(obj) = self.argo_obj(&app) else { return Vec::new() };
        match code {
            KeyCode::Enter | KeyCode::Right if !detail => {
                self.argo.list_sel = self.sel();
                self.argo.detail = Some((obj.ns.clone(), obj.name.clone()));
                self.argo.pane = ArgoPane::Resources;
                self.selected[Tab::Apps.index()] = 0;
                return vec![Effect::ArgoFocus(Some((obj.ns, obj.name)))];
            }
            // A resource of the app: describe it in the Kubernetes view.
            KeyCode::Enter if self.argo.pane == ArgoPane::Resources => {
                let Some(r) = super::argo::resources(&app).into_iter().nth(self.sel()) else { return Vec::new() };
                let Some(res) = self.kube.catalog.list.iter().find(|x| x.group == r.group && x.kind == r.kind).cloned() else {
                    self.flash(format!("{} is not a type this cluster serves", r.kind), true);
                    return Vec::new();
                };
                self.argo.back_to = Some(self.kube.views.len());
                self.tab = Tab::Kube;
                return self.kube.describe(super::kube::state::ObjRef { res, ns: r.namespace, name: r.name });
            }
            KeyCode::Char('d') => {
                self.argo.back_to = Some(self.kube.views.len());
                self.tab = Tab::Kube;
                return self.kube.describe(obj);
            }
            KeyCode::Char('D') => {
                self.argo.back_to = Some(self.kube.views.len());
                self.tab = Tab::Kube;
                return self.kube.app_diff(obj);
            }
            KeyCode::Char('s') => {
                if self.argo_writable() {
                    self.overlay = Overlay::Sync { objs: vec![obj], prune: false, force: false };
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                if self.argo_writable() {
                    return vec![Effect::Kube(KEffect::AppRefresh { objs: vec![obj], hard: code == KeyCode::Char('R') })];
                }
            }
            KeyCode::Char('T') => {
                if !super::argo::running(&app) {
                    self.flash(format!("{} has no sync running", obj.label()), false);
                } else if self.argo.sample.litecd() {
                    self.flash("litecd cannot terminate a running sync (Argo CD only): it ends on its own or at its retry limit".to_string(), false);
                } else if self.argo_writable() {
                    let msg = super::argo::operation(&app).map(|(_, m)| m).unwrap_or_default();
                    let mut lines = vec!["Stops its running operation (as argocd app terminate-op): what it applied stays.".to_string()];
                    if !msg.is_empty() {
                        lines.push(format!("Now: {msg}"));
                    }
                    self.confirm(format!("Terminate the sync of {}?", obj.label()), lines, Effect::Kube(KEffect::AppTerminate(obj)));
                }
            }
            _ => {}
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

    /// Put `text` on the clipboard (OSC 52, and pbcopy on a Mac) and say so:
    /// a TUI that redraws every 2 s leaves the terminal's own selection
    /// little time.
    fn copy(&mut self, text: String, what: &str) -> Vec<Effect> {
        if text.is_empty() {
            self.flash(format!("no {what} to copy"), true);
            return Vec::new();
        }
        self.flash(format!("copied {what}: {text}"), false);
        vec![Effect::Kube(super::kube::state::KEffect::Copy(text))]
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
        if code == KeyCode::Char('c') {
            let context = match &sel {
                Ok(row) => row.context.clone(),
                Err((context, _)) => context.clone(),
            };
            return self.copy(context, "context name");
        }
        let row = match sel {
            Ok(row) => row.clone(),
            Err((context, _)) => {
                if code == KeyCode::Char('t') {
                    // Straight into its tree.
                    let context = context.clone();
                    self.context_override = Some(context.clone());
                    self.kube.tree_mode();
                    self.tab = Tab::Kube;
                    return vec![Effect::UseContext(context)];
                }
                if code == KeyCode::Enter {
                    let context = context.clone();
                    self.context_override = Some(context.clone());
                    self.flash(format!("Kubernetes view: context {context} (read-only unless in writableContexts; enter on a local cluster to go back)"), false);
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
            KeyCode::Char('t') => {
                // Its tree, without touching kubectl's or docker's context.
                self.context_override = None;
                self.cluster = row.name.clone();
                self.kube.tree_mode();
                self.tab = Tab::Kube;
                return vec![Effect::UseCluster(row.name.clone())];
            }
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
            InputKind::LogSearch => {
                if let Err(e) = regex::Regex::new(value.trim()) {
                    return fail(self, format!("not a regex: {e}"));
                }
                self.open_logs(LogSource::Search { namespace: self.obs_namespace.clone(), pod: None, grep: value.trim().to_string() })
            }
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
                        if let Some(tab) = self.visible_tabs().get(i) {
                            self.tab = *tab;
                        }
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
    crate::dev::cluster::Cluster { name: cluster.to_string() }.context()
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

pub const SETTINGS: [Setting; 9] = [
    Setting::Field(Field::Cpus),
    Setting::Field(Field::Memory),
    Setting::Field(Field::Disk),
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
        Field::Disk => "VM data disk in GiB, grows in place, never shrinks (empty: 64)",
        Field::Shares => "shared directories, comma-separated (empty: $HOME)",
        Field::Port => "API server port of the cluster dev (empty: 6443)",
        Field::Publish => "published ports: localhost or all",
    }
}
