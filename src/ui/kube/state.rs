//! The Kubernetes view's state and keys: a stack of views (resource
//! tables, pagers for describe / YAML / kubectl output / diffs, logs, xray
//! trees, the port-forwards), the `:` and `/` prompts, pickers and
//! confirmations. Like the rest of the UI it does no I/O: keys return
//! effects, the runtime's findings arrive as [`KMsg`]s.
//!
//! Keys follow k9s where k9s has one (d describe, y YAML, e edit, ctrl-d
//! delete, l logs, s shell or scale, shift-f port-forward, enter drills
//! down, esc goes back, 0 all namespaces, ctrl-w wide, ctrl-z faults).

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

use super::command::{self, Cmd, History, Ns};
use super::filter::{Filter, Mode};
use super::resources::{Catalog, Resource, builtins};
use super::table::{Column, Health, Row, Snapshot, compare_cells};
use super::uiconfig::{Plugin, Theme, UiConfig, key_matches};
use crate::kube::selector::LabelSelector;
use crate::ui::app::Effect;

/// An object the UI acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjRef {
    pub res: Resource,
    pub ns: String,
    pub name: String,
}

impl ObjRef {
    pub fn label(&self) -> String {
        let k = self.res.short.first().cloned().unwrap_or_else(|| self.res.singular.clone());
        if self.ns.is_empty() { format!("{k}/{}", self.name) } else { format!("{k}/{}/{}", self.ns, self.name) }
    }
}

/// One informer: a resource type, a namespace (`None`: all, or
/// cluster-scoped) and a server-side field selector.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WatchKey {
    pub res: String,
    pub ns: Option<String>,
    pub fields: String,
}

/// The synthetic type of a pod's containers view.
pub const CONTAINERS: &str = "containers";

pub fn containers_resource() -> Resource {
    Resource { group: String::new(), version: String::new(), plural: CONTAINERS.into(), singular: "container".into(), kind: "Container".into(), namespaced: true, verbs: vec!["get".into(), "list".into()], short: vec!["co".into()] }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Propagation {
    Background,
    Foreground,
    Orphan,
}

impl Propagation {
    pub fn next(self) -> Propagation {
        match self {
            Propagation::Background => Propagation::Foreground,
            Propagation::Foreground => Propagation::Orphan,
            Propagation::Orphan => Propagation::Background,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Propagation::Background => "Background",
            Propagation::Foreground => "Foreground",
            Propagation::Orphan => "Orphan",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LogTarget {
    Pod { ns: String, name: String, container: Option<String> },
    /// A workload's (or service's) pods, by its selector.
    Owner(ObjRef),
}

/// What a container pick is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Logs { previous: bool },
    Shell,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContainerInfo {
    pub name: String,
    /// `init`, `ephemeral` or empty.
    pub kind: String,
    pub running: bool,
}

/// A running port-forward.
#[derive(Debug, Clone, PartialEq)]
pub struct FwdInfo {
    pub local: u16,
    /// `svc/web`, `pod/web-1`.
    pub target: String,
    pub ns: String,
    pub pod: String,
    pub remote: u16,
    pub connections: u64,
    pub error: Option<String>,
    pub started: i64,
}

/// A node of an xray tree.
#[derive(Debug, Clone, PartialEq)]
pub struct XNode {
    pub depth: u8,
    pub kind: String,
    pub name: String,
    pub status: String,
    pub health: Health,
    /// What the actions act on (none for containers).
    pub obj: Option<ObjRef>,
    /// The pod of a container node.
    pub container_of: Option<(String, String)>,
}

/// What the runtime is asked to do.
#[derive(Debug, Clone, PartialEq)]
pub enum KEffect {
    /// The informers wanted now (the runtime starts and stops them).
    Watch(Vec<(Resource, WatchKey)>),
    /// List and watch again.
    Relist(WatchKey),
    Discover,
    LoadContexts,
    Describe { id: u64, obj: ObjRef },
    Yaml { id: u64, obj: ObjRef, managed: bool },
    /// A secret with its data decoded.
    Decode { id: u64, obj: ObjRef },
    Edit { obj: ObjRef, text: Option<String> },
    Apply { obj: ObjRef, body: String },
    Delete { objs: Vec<ObjRef>, propagation: Propagation, force: bool },
    Scale { obj: ObjRef, replicas: i64 },
    Restart(Vec<ObjRef>),
    Undo(ObjRef),
    History { id: u64, obj: ObjRef },
    Cordon { nodes: Vec<String>, on: bool },
    Drain { nodes: Vec<String> },
    Trigger(ObjRef),
    Containers { obj: ObjRef, purpose: Purpose },
    Logs { id: u64, target: LogTarget, previous: bool, tail: u32, max_streams: usize },
    StopLogs(u64),
    Shell { ns: String, pod: String, container: Option<String> },
    PortForwardPrepare(ObjRef),
    PortForward { obj: ObjRef, local: u16, remote: String },
    StopForward(u16),
    Kubectl { id: u64, args: String },
    StopKubectl(u64),
    DrillDown(ObjRef),
    Xray { id: u64, res: Resource, ns: Option<String> },
    Copy(String),
    CopyYaml(ObjRef),
    Save { name: String, lines: Vec<String> },
    Plugin { plugin: Plugin, vars: Vec<(String, String)> },
    SaveHistory(Vec<String>),
}

/// What the runtime reports.
#[derive(Debug, Clone)]
pub enum KMsg {
    Discovery(Result<Vec<Resource>, String>),
    Data(WatchKey, Arc<Snapshot>),
    Contexts { list: Vec<String>, current: Option<String> },
    /// Text for a pager (`append`: more of a streaming one).
    Pager { id: u64, lines: Vec<String>, append: bool, done: bool, error: Option<String> },
    /// An edit validated by the server (dry run): the diff to apply.
    Diff { obj: ObjRef, lines: Vec<String>, body: String },
    Containers { obj: ObjRef, list: Vec<ContainerInfo>, purpose: Purpose },
    Ports { obj: ObjRef, ports: Vec<(u16, String)> },
    Forwards(Vec<FwdInfo>),
    LogSources { id: u64, sources: Vec<String> },
    LogLines { id: u64, src: usize, lines: Vec<String> },
    LogEnd { id: u64, src: usize, error: Option<String> },
    /// A table to show (a drill-down's result).
    Open { res: String, ns: Option<String>, labels: Option<String>, fields: Option<String>, title: String },
    Xray { id: u64, nodes: Vec<XNode>, error: Option<String> },
    History(Vec<String>),
    /// Values of an extra column (metrics, observability) by row key.
    Ext { kind: String, column: String, values: HashMap<String, String> },
    Config(Result<UiConfig, String>),
}

// ---- views ----

/// A drill-down's fixed part of a table (not the user's filter).
#[derive(Debug, Clone, Default)]
pub struct Scope {
    pub labels: Option<(String, LabelSelector)>,
    pub fields: Option<String>,
    pub owner_uid: Option<String>,
    /// Just this object (a jump to an owner or a node).
    pub name: Option<String>,
    pub title: Option<String>,
}

/// A column as drawn.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Namespace,
    Cell(usize),
    Age,
    Ext(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DCol {
    pub name: String,
    pub source: Source,
    pub priority: i64,
    pub width: usize,
    /// Numbers right-aligned.
    pub numeric: bool,
}

pub struct TableView {
    pub res: Resource,
    /// `None`: all namespaces.
    pub ns: Option<String>,
    pub scope: Scope,
    pub filter: Filter,
    /// Column index into `cols` (by name, so it survives column changes).
    pub sort: Option<(String, bool)>,
    pub sel: usize,
    sel_key: Option<String>,
    pub offset: Cell<usize>,
    pub height: Cell<usize>,
    pub marks: BTreeSet<String>,
    pub wide: bool,
    pub faults: bool,
    /// What is drawn: filtered and sorted.
    pub rows: Vec<Arc<Row>>,
    pub cols: Vec<DCol>,
    pub total: usize,
    pub synced: bool,
    pub error: Option<String>,
    /// The snapshot `rows` came from (recompute when it changes).
    seen: Option<Arc<Snapshot>>,
    ext_seen: u64,
    /// Extra columns' values by row key.
    pub ext: Vec<(String, Arc<HashMap<String, String>>)>,
    pub fixed_ns: bool,
}

impl TableView {
    pub fn new(res: Resource, ns: Option<String>) -> TableView {
        TableView {
            res,
            ns,
            scope: Scope::default(),
            filter: Filter::default(),
            sort: None,
            sel: 0,
            sel_key: None,
            offset: Cell::new(0),
            height: Cell::new(10),
            marks: BTreeSet::new(),
            wide: false,
            faults: false,
            rows: Vec::new(),
            cols: Vec::new(),
            total: 0,
            synced: false,
            error: None,
            seen: None,
            ext_seen: 0,
            ext: Vec::new(),
            fixed_ns: false,
        }
    }

    pub fn key(&self) -> WatchKey {
        let mut fields: Vec<&str> = Vec::new();
        if let Some(f) = &self.scope.fields {
            fields.push(f);
        }
        if let Some(f) = &self.filter.fields {
            fields.push(f);
        }
        WatchKey { res: self.res.id(), ns: self.ns.clone().filter(|_| self.res.namespaced), fields: fields.join(",") }
    }

    pub fn selected(&self) -> Option<&Arc<Row>> {
        self.rows.get(self.sel)
    }

    fn select(&mut self, i: usize) {
        self.sel = i.min(self.rows.len().saturating_sub(1));
        self.sel_key = self.rows.get(self.sel).map(|r| r.key());
    }

    pub fn all_namespaces(&self) -> bool {
        self.res.namespaced && self.ns.is_none()
    }

    pub fn title(&self) -> String {
        let mut t = self.scope.title.clone().unwrap_or_else(|| self.res.name());
        if self.res.namespaced {
            t.push_str(&format!("({})", self.ns.as_deref().unwrap_or("all")));
        }
        t
    }

    fn obj(&self, r: &Row) -> ObjRef {
        ObjRef { res: self.res.clone(), ns: r.ns.clone(), name: r.name.clone() }
    }

    /// The marked rows, else the selected one.
    pub fn targets(&self) -> Vec<ObjRef> {
        if !self.marks.is_empty() {
            let rows: HashMap<String, &Arc<Row>> = self.rows.iter().map(|r| (r.key(), r)).collect();
            return self.marks.iter().filter_map(|k| rows.get(k)).map(|r| self.obj(r)).collect();
        }
        self.selected().map(|r| vec![self.obj(r)]).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagerKind {
    Describe,
    Yaml,
    Text,
    Diff,
    Kubectl,
    Help,
}

pub struct Pager {
    pub id: u64,
    pub title: String,
    pub kind: PagerKind,
    pub lines: Vec<String>,
    pub top: usize,
    pub left: usize,
    pub wrap: bool,
    pub search: Option<Filter>,
    pub matches: Vec<usize>,
    pub obj: Option<ObjRef>,
    pub loading: bool,
    pub error: Option<String>,
    pub managed: bool,
    /// An edit to apply (`y`).
    pub pending: Option<String>,
    pub height: Cell<usize>,
    /// Follow the end while output streams in (kubectl).
    pub tail: bool,
}

impl Pager {
    fn new(id: u64, title: impl Into<String>, kind: PagerKind) -> Pager {
        Pager { id, title: title.into(), kind, lines: Vec::new(), top: 0, left: 0, wrap: false, search: None, matches: Vec::new(), obj: None, loading: true, error: None, managed: false, pending: None, height: Cell::new(20), tail: kind == PagerKind::Kubectl }
    }

    fn max_top(&self) -> usize {
        self.lines.len().saturating_sub(self.height.get().max(1))
    }

    fn research(&mut self) {
        self.matches = match &self.search {
            Some(f) if !f.pattern.is_empty() => self.lines.iter().enumerate().filter(|(_, l)| f.matches_text(l)).map(|(i, _)| i).collect(),
            _ => Vec::new(),
        };
    }

    fn jump(&mut self, forward: bool) {
        let next = if forward { self.matches.iter().find(|&&i| i > self.top) } else { self.matches.iter().rev().find(|&&i| i < self.top) };
        if let Some(&i) = next.or(if forward { self.matches.first() } else { self.matches.last() }) {
            self.top = i;
        }
    }
}

pub struct LogLine {
    pub src: u16,
    /// RFC 3339 timestamp from the server, then the text.
    pub text: String,
    pub ts_len: u8,
}

impl LogLine {
    pub fn body(&self) -> &str {
        &self.text[self.ts_len as usize..]
    }
    pub fn ts(&self) -> &str {
        self.text[..self.ts_len as usize].trim_end()
    }
}

pub struct LogsView {
    pub id: u64,
    pub title: String,
    pub target: LogTarget,
    pub sources: Vec<String>,
    pub ended: Vec<Option<String>>,
    pub lines: VecDeque<LogLine>,
    pub follow: bool,
    /// Lines up from the end (of what the filter shows) when not following.
    pub offset: usize,
    pub wrap: bool,
    pub timestamps: bool,
    pub previous: bool,
    pub filter: Option<Filter>,
    pub cap: usize,
    pub dropped: u64,
    pub height: Cell<usize>,
}

impl LogsView {
    pub fn visible(&self) -> Vec<usize> {
        match &self.filter {
            Some(f) if !f.pattern.is_empty() => (0..self.lines.len()).filter(|&i| f.matches_text(self.lines[i].body())).collect(),
            _ => (0..self.lines.len()).collect(),
        }
    }
}

pub struct XrayView {
    pub id: u64,
    pub res: Resource,
    pub ns: Option<String>,
    pub nodes: Vec<XNode>,
    pub folded: BTreeSet<usize>,
    pub sel: usize,
    pub offset: Cell<usize>,
    pub height: Cell<usize>,
    pub loading: bool,
    pub error: Option<String>,
}

impl XrayView {
    /// Node indices shown (children of folded nodes hidden).
    pub fn shown(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut hide_below: Option<u8> = None;
        for (i, n) in self.nodes.iter().enumerate() {
            if let Some(d) = hide_below {
                if n.depth > d {
                    continue;
                }
                hide_below = None;
            }
            out.push(i);
            if self.folded.contains(&i) {
                hide_below = Some(n.depth);
            }
        }
        out
    }
}

pub enum View {
    Table(TableView),
    Pager(Pager),
    Logs(LogsView),
    Xray(XrayView),
    Forwards { sel: usize },
}

// ---- overlays ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Command,
    Filter,
    Kubectl,
    Search,
    LogFilter,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    pub kind: PromptKind,
    pub text: String,
    pub mode: Mode,
    pub suggestions: Vec<String>,
    /// Which suggestion tab put in (tab again: the next one).
    pub tab: Option<usize>,
    /// The text before tabbing started.
    pub typed: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfirmAction {
    Delete { objs: Vec<ObjRef>, propagation: Propagation, force: bool },
    Run(Vec<KEffect>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Confirm {
    pub title: String,
    pub lines: Vec<String>,
    pub action: ConfirmAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputKind {
    Scale(ObjRef),
    PortForward(ObjRef),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    pub prompt: String,
    pub value: String,
    pub hint: String,
    pub kind: InputKind,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    Namespace,
    Context,
    Container { obj: ObjRef, purpose: Purpose },
    Resource,
    Action,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Picker {
    pub title: String,
    /// (label, value)
    pub items: Vec<(String, String)>,
    pub filter: String,
    pub sel: usize,
    pub pick: Pick,
}

impl Picker {
    pub fn shown(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        let mut v: Vec<(i64, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, (l, _))| if f.is_empty() { Some((0, i)) } else { super::filter::fuzzy(&f, &l.to_lowercase()).map(|s| (s, i)) })
            .collect();
        if !f.is_empty() {
            v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        v.into_iter().map(|(_, i)| i).collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    None,
    Prompt(Prompt),
    Confirm(Confirm),
    Input(Input),
    Picker(Picker),
}

/// An action of the `m` menu: its key, what it says, and the key event it
/// stands for.
pub struct Action {
    pub key: &'static str,
    pub label: &'static str,
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

const fn act(key: &'static str, label: &'static str, code: KeyCode, mods: KeyModifiers) -> Action {
    Action { key, label, code, mods }
}

const N: KeyModifiers = KeyModifiers::NONE;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;

pub struct KubeUi {
    pub catalog: Catalog,
    pub discovered: bool,
    pub discovery_error: Option<String>,
    pub contexts: Vec<String>,
    /// `None`: all namespaces.
    pub namespace: Option<String>,
    pub views: Vec<View>,
    pub data: HashMap<WatchKey, Arc<Snapshot>>,
    pub overlay: Overlay,
    pub history: History,
    /// The last filter of each resource type, restored with it.
    filters: HashMap<String, (String, Mode)>,
    pub config: UiConfig,
    pub config_error: Option<String>,
    pub theme: Theme,
    pub forwards: Vec<FwdInfo>,
    /// Extra columns by (kind, column) → row key → value.
    pub ext: HashMap<(String, String), Arc<HashMap<String, String>>>,
    /// The namespace before `0` showed all.
    last_ns: Option<String>,
    ext_gen: u64,
    /// Effects to run without a key (after a message).
    pub pending: Vec<KEffect>,
    /// A status line for the app to show.
    pub flash: Option<(String, bool)>,
    watched: Vec<WatchKey>,
    next_id: u64,
    pub now: i64,
    pub mode: Mode,
}

impl Default for KubeUi {
    fn default() -> Self {
        Self::new(None)
    }
}

/// What an app-level key handler gets back: effects, or "not mine".
pub type Handled = Option<Vec<Effect>>;

fn k(e: KEffect) -> Effect {
    Effect::Kube(e)
}

impl KubeUi {
    pub fn new(namespace: Option<String>) -> KubeUi {
        let catalog = Catalog::new(builtins());
        let pods = catalog.find("pods").cloned().unwrap_or_default();
        let mut ui = KubeUi {
            catalog,
            discovered: false,
            discovery_error: None,
            contexts: Vec::new(),
            namespace: namespace.clone(),
            views: Vec::new(),
            data: HashMap::new(),
            overlay: Overlay::None,
            history: History::default(),
            filters: HashMap::new(),
            config: UiConfig::default(),
            config_error: None,
            theme: Theme::default(),
            forwards: Vec::new(),
            ext: HashMap::new(),
            last_ns: None,
            ext_gen: 0,
            pending: Vec::new(),
            flash: None,
            watched: Vec::new(),
            next_id: 1,
            now: chrono::Utc::now().timestamp(),
            mode: Mode::Substring,
        };
        ui.views.push(View::Table(TableView::new(pods, namespace)));
        ui
    }

    /// A new cluster (context switch): forget what the old one had.
    pub fn reset(&mut self, namespace: Option<String>) {
        let top = self.top_table().map(|t| t.res.name());
        self.data.clear();
        self.ext.clear();
        self.forwards.clear();
        self.discovered = false;
        self.discovery_error = None;
        self.catalog = Catalog::new(builtins());
        self.namespace = namespace;
        let res = top.and_then(|n| self.catalog.find(&n).cloned()).unwrap_or_else(|| self.catalog.find("pods").cloned().unwrap_or_default());
        self.views = vec![View::Table(TableView::new(res, self.namespace.clone()))];
        self.overlay = Overlay::None;
        self.watched.clear();
        self.pending.push(KEffect::Discover);
        self.sync_watches();
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub fn top(&self) -> &View {
        self.views.last().expect("a view")
    }

    fn top_mut(&mut self) -> &mut View {
        self.views.last_mut().expect("a view")
    }

    pub fn top_table(&self) -> Option<&TableView> {
        match self.top() {
            View::Table(t) => Some(t),
            _ => None,
        }
    }

    fn top_table_mut(&mut self) -> Option<&mut TableView> {
        match self.top_mut() {
            View::Table(t) => Some(t),
            _ => None,
        }
    }

    fn say(&mut self, text: impl Into<String>, error: bool) {
        self.flash = Some((text.into(), error));
    }

    /// The informers the views need (tables in the stack, namespaces).
    pub fn wanted(&self) -> Vec<(Resource, WatchKey)> {
        let mut out: Vec<(Resource, WatchKey)> = Vec::new();
        for v in &self.views {
            if let View::Table(t) = v {
                let key = t.key();
                if !out.iter().any(|(_, k)| *k == key) {
                    out.push((t.res.clone(), key));
                }
            }
        }
        if let Some(ns) = self.catalog.find("namespaces") {
            let key = WatchKey { res: ns.id(), ns: None, fields: String::new() };
            if !out.iter().any(|(_, k)| *k == key) {
                out.push((ns.clone(), key));
            }
        }
        out
    }

    /// Ask the runtime for the informers the views need, when that changed.
    pub fn sync_watches(&mut self) {
        let wanted = self.wanted();
        let keys: Vec<WatchKey> = wanted.iter().map(|(_, k)| k.clone()).collect();
        if keys != self.watched {
            self.watched = keys;
            self.pending.push(KEffect::Watch(wanted));
        }
    }

    pub fn take_effects(&mut self) -> Vec<KEffect> {
        std::mem::take(&mut self.pending)
    }

    pub fn namespaces(&self) -> Vec<String> {
        let Some(res) = self.catalog.find("namespaces") else { return Vec::new() };
        let key = WatchKey { res: res.id(), ns: None, fields: String::new() };
        self.data.get(&key).map(|s| s.rows.iter().map(|r| r.name.clone()).collect()).unwrap_or_default()
    }

    // ---- messages ----

    pub fn apply(&mut self, msg: KMsg) {
        match msg {
            KMsg::Discovery(Ok(list)) => {
                let mut list = list;
                // Keep built-ins a broken aggregated API does not list.
                for b in builtins() {
                    if !list.iter().any(|r| r.group == b.group && r.plural == b.plural) {
                        list.push(b);
                    }
                }
                self.catalog = Catalog::new(list);
                self.discovered = true;
                self.discovery_error = None;
                // Tables keep their type, now with its served version.
                for v in &mut self.views {
                    if let View::Table(t) = v {
                        if let Some(r) = self.catalog.find(&t.res.name()).filter(|r| r.group == t.res.group) {
                            if r.version != t.res.version {
                                t.res = r.clone();
                                t.seen = None;
                            }
                        }
                    }
                }
                self.sync_watches();
            }
            KMsg::Discovery(Err(e)) => {
                self.discovery_error = Some(e);
            }
            KMsg::Data(key, snap) => {
                self.data.insert(key.clone(), snap);
                // Only what is on screen is recomputed now; the rest when shown.
                if let Some(View::Table(_)) = self.views.last() {
                    self.refresh_top();
                }
            }
            KMsg::Contexts { list, current } => {
                self.contexts = list;
                if let Overlay::Picker(p) = &mut self.overlay {
                    if p.pick == Pick::Context {
                        p.items = self.contexts.iter().map(|c| (if Some(c) == current.as_ref() { format!("{c}  (kubectl's current)") } else { c.clone() }, c.clone())).collect();
                    }
                }
            }
            KMsg::Pager { id, lines, append, done, error } => {
                let found = self.views.iter_mut().find_map(|v| match v {
                    View::Pager(p) if p.id == id => Some(p),
                    _ => None,
                });
                {
                    if let Some(p) = found {
                        {
                            if append {
                                p.lines.extend(lines);
                                if p.lines.len() > 200_000 {
                                    let cut = p.lines.len() - 200_000;
                                    p.lines.drain(..cut);
                                }
                            } else {
                                p.lines = lines;
                                p.top = p.top.min(p.lines.len().saturating_sub(1));
                            }
                            if p.tail {
                                p.top = p.max_top();
                            }
                            p.loading = !done;
                            if error.is_some() {
                                p.error = error.clone();
                            }
                            p.research();
                        }
                    }
                }
            }
            KMsg::Diff { obj, lines, body } => {
                let id = self.id();
                let mut p = Pager::new(id, format!("edit {} (validated by the server; not applied yet)", obj.label()), PagerKind::Diff);
                p.lines = if lines.is_empty() { vec!["(no changes)".into()] } else { lines };
                p.loading = false;
                p.obj = Some(obj);
                p.pending = Some(body);
                self.push(View::Pager(p));
            }
            KMsg::Containers { obj, list, purpose } => self.containers_known(obj, list, purpose),
            KMsg::Ports { obj, ports } => {
                let (local, remote) = match ports.first() {
                    Some((p, _)) => (if *p < 1024 { *p as u32 + 8000 } else { *p as u32 }, p.to_string()),
                    None => (8080, "80".to_string()),
                };
                let hint = if ports.is_empty() { "no ports declared; local:remote".to_string() } else { format!("ports: {}", ports.iter().map(|(p, n)| if n.is_empty() { p.to_string() } else { format!("{p} ({n})") }).collect::<Vec<_>>().join(", ")) };
                self.overlay = Overlay::Input(Input { prompt: format!("port-forward {} (local:remote)", obj.label()), value: format!("{local}:{remote}"), hint, kind: InputKind::PortForward(obj), error: None });
            }
            KMsg::Forwards(f) => self.forwards = f,
            KMsg::LogSources { id, sources } => {
                if let Some(l) = self.logs_mut(id) {
                    l.ended = vec![None; sources.len()];
                    l.sources = sources;
                }
            }
            KMsg::LogLines { id, src, lines } => {
                if let Some(l) = self.logs_mut(id) {
                    let added = lines.len();
                    for text in lines {
                        let ts_len = ts_prefix(&text);
                        // Streams interleave by time (their first lines come
                        // as one batch each): a line older than the end goes
                        // where it belongs among the last few thousand.
                        let mut at = l.lines.len();
                        if ts_len > 0 {
                            let ts = &text[..ts_len as usize];
                            let floor = at.saturating_sub(5000);
                            while at > floor {
                                let prev = &l.lines[at - 1];
                                if prev.ts_len == 0 || prev.text[..prev.ts_len as usize] <= *ts {
                                    break;
                                }
                                at -= 1;
                            }
                        }
                        l.lines.insert(at, LogLine { src: src as u16, text, ts_len });
                    }
                    while l.lines.len() > l.cap {
                        l.lines.pop_front();
                        l.dropped += 1;
                    }
                    if !l.follow {
                        l.offset = (l.offset + added).min(l.lines.len());
                    }
                }
            }
            KMsg::LogEnd { id, src, error } => {
                if let Some(l) = self.logs_mut(id) {
                    if let Some(e) = l.ended.get_mut(src) {
                        *e = Some(error.unwrap_or_else(|| "ended".into()));
                    }
                }
            }
            KMsg::Open { res, ns, labels, fields, title } => {
                let Some(r) = self.catalog.by_id(&res).cloned().or_else(|| self.catalog.find(&res).cloned()) else {
                    self.say(format!("unknown resource {res}"), true);
                    return;
                };
                let mut t = TableView::new(r, ns.clone().or_else(|| self.namespace.clone()));
                if ns.is_some() {
                    t.fixed_ns = true;
                }
                if labels.is_none() && fields.is_some() && ns.is_none() {
                    // A node's pods: all namespaces.
                    t.ns = None;
                    t.fixed_ns = true;
                }
                if let Some(l) = labels {
                    match LabelSelector::parse(&l) {
                        Ok(s) => t.scope.labels = Some((l, s)),
                        Err(e) => self.say(format!("selector {l}: {e}"), true),
                    }
                }
                t.scope.fields = fields;
                t.scope.title = Some(title);
                self.push(View::Table(t));
            }
            KMsg::Xray { id, nodes, error } => {
                for v in &mut self.views {
                    if let View::Xray(x) = v {
                        if x.id == id {
                            x.nodes = nodes.clone();
                            x.loading = false;
                            x.error = error.clone();
                            x.sel = x.sel.min(x.nodes.len().saturating_sub(1));
                        }
                    }
                }
            }
            KMsg::History(h) => {
                if self.history.entries.is_empty() {
                    self.history.entries = h;
                }
            }
            KMsg::Ext { kind, column, values } => {
                self.ext.insert((kind, column), Arc::new(values));
                self.ext_gen += 1;
                self.refresh_top();
            }
            KMsg::Config(Ok(c)) => {
                self.theme = Theme::new(&c.skin);
                self.config = c;
                self.config_error = None;
            }
            KMsg::Config(Err(e)) => {
                self.say(format!("ui.yaml: {e}"), true);
                self.config_error = Some(e);
            }
        }
    }

    fn logs_mut(&mut self, id: u64) -> Option<&mut LogsView> {
        self.views.iter_mut().find_map(|v| match v {
            View::Logs(l) if l.id == id => Some(l),
            _ => None,
        })
    }

    fn containers_known(&mut self, obj: ObjRef, list: Vec<ContainerInfo>, purpose: Purpose) {
        let candidates: Vec<&ContainerInfo> = match purpose {
            Purpose::Shell => list.iter().filter(|c| c.running).collect(),
            Purpose::Logs { .. } => list.iter().collect(),
        };
        if candidates.is_empty() {
            self.say(format!("{} has no running containers", obj.label()), true);
            return;
        }
        let main: Vec<&&ContainerInfo> = candidates.iter().filter(|c| c.kind.is_empty()).collect();
        if candidates.len() == 1 || (main.len() == 1 && purpose == Purpose::Shell) {
            let c = if main.len() == 1 { main[0].name.clone() } else { candidates[0].name.clone() };
            self.container_chosen(obj, purpose, Some(c));
            return;
        }
        let mut items: Vec<(String, String)> = Vec::new();
        if matches!(purpose, Purpose::Logs { .. }) {
            items.push(("all containers".into(), String::new()));
        }
        items.extend(candidates.iter().map(|c| (if c.kind.is_empty() { c.name.clone() } else { format!("{} ({})", c.name, c.kind) }, c.name.clone())));
        let verb = if purpose == Purpose::Shell { "shell" } else { "logs" };
        self.overlay = Overlay::Picker(Picker { title: format!("{verb}: which container of {}?", obj.name), items, filter: String::new(), sel: 0, pick: Pick::Container { obj, purpose } });
    }

    fn container_chosen(&mut self, obj: ObjRef, purpose: Purpose, container: Option<String>) {
        match purpose {
            Purpose::Shell => self.pending.push(KEffect::Shell { ns: obj.ns, pod: obj.name, container }),
            Purpose::Logs { previous } => {
                let target = LogTarget::Pod { ns: obj.ns.clone(), name: obj.name.clone(), container: container.clone() };
                let title = match &container {
                    Some(c) => format!("{}/{} [{c}]", obj.ns, obj.name),
                    None => format!("{}/{}", obj.ns, obj.name),
                };
                self.open_logs(target, title, previous);
            }
        }
    }

    fn open_logs(&mut self, target: LogTarget, title: String, previous: bool) {
        let id = self.id();
        let c = &self.config.logs;
        let v = LogsView {
            id,
            title,
            target: target.clone(),
            sources: Vec::new(),
            ended: Vec::new(),
            lines: VecDeque::new(),
            follow: true,
            offset: 0,
            wrap: c.wrap,
            timestamps: c.timestamps,
            previous,
            filter: None,
            cap: c.buffer.max(100),
            dropped: 0,
            height: Cell::new(20),
        };
        self.pending.push(KEffect::Logs { id, target, previous, tail: c.tail, max_streams: c.max_streams.max(1) });
        self.push(View::Logs(v));
    }

    fn push(&mut self, v: View) {
        self.views.push(v);
        if self.views.len() > 20 {
            // Old views go, not the root.
            let old = self.views.remove(1);
            self.closed(old);
        }
        self.refresh_top();
        self.sync_watches();
    }

    fn pop(&mut self) {
        if self.views.len() > 1 {
            let v = self.views.pop().unwrap();
            self.closed(v);
            self.refresh_top();
            self.sync_watches();
        }
    }

    fn closed(&mut self, v: View) {
        match v {
            View::Logs(l) => self.pending.push(KEffect::StopLogs(l.id)),
            View::Pager(p) if p.kind == PagerKind::Kubectl && p.loading => self.pending.push(KEffect::StopKubectl(p.id)),
            _ => {}
        }
    }

    /// Recompute the top table's rows from its informer's snapshot.
    pub fn refresh_top(&mut self) {
        let ext_gen = self.ext_gen;
        let Some(View::Table(t)) = self.views.last() else { return };
        let key = t.key();
        let snap = self.data.get(&key).cloned();
        let ext = self.ext_for(&t.res.kind);
        let Some(View::Table(t)) = self.views.last_mut() else { return };
        let same = match (&t.seen, &snap) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if same && t.ext_seen == ext_gen && !t.rows.is_empty() {
            return;
        }
        t.ext_seen = ext_gen;
        recompute(t, snap, &ext);
    }

    fn ext_for(&self, kind: &str) -> Vec<(String, Arc<HashMap<String, String>>)> {
        let mut v: Vec<(String, Arc<HashMap<String, String>>)> = self.ext.iter().filter(|((k, _), _)| k == kind).map(|((_, c), m)| (c.clone(), m.clone())).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    fn refilter(&mut self) {
        let ext_gen = self.ext_gen;
        let Some(View::Table(t)) = self.views.last() else { return };
        let snap = self.data.get(&t.key()).cloned();
        let ext = self.ext_for(&t.res.kind);
        if let Some(View::Table(t)) = self.views.last_mut() {
            t.ext_seen = ext_gen;
            recompute(t, snap, &ext);
        }
        self.sync_watches();
    }

    // ---- keys ----

    /// A key for the Kubernetes view; `None` when it is the app's (tabs,
    /// quit).
    pub fn key(&mut self, key: KeyEvent) -> Handled {
        let out = self.key_inner(key);
        self.sync_watches();
        out
    }

    fn key_inner(&mut self, key: KeyEvent) -> Handled {
        match std::mem::replace(&mut self.overlay, Overlay::None) {
            Overlay::None => {}
            Overlay::Prompt(p) => return Some(self.prompt_key(p, key)),
            Overlay::Confirm(c) => return Some(self.confirm_key(c, key)),
            Overlay::Input(i) => return Some(self.input_key(i, key)),
            Overlay::Picker(p) => return Some(self.picker_key(p, key)),
        }
        // The user's hotkeys and plugins first: they may take any key.
        if let Some(cmd) = self.config.hotkeys.iter().find(|(spec, _)| key_matches(spec, &key)).map(|(_, c)| c.clone()) {
            return Some(self.run_line(&cmd));
        }
        if let Some(effects) = self.plugin_key(&key) {
            return Some(effects);
        }
        match key.code {
            KeyCode::Char(':') => {
                self.open_prompt(PromptKind::Command, String::new());
                return Some(Vec::new());
            }
            KeyCode::Char('?') => {
                self.help();
                return Some(Vec::new());
            }
            KeyCode::Char('!') if !matches!(self.top(), View::Pager(p) if p.pending.is_some()) => {
                self.open_prompt(PromptKind::Kubectl, String::new());
                return Some(Vec::new());
            }
            _ => {}
        }
        match self.top() {
            View::Table(_) => self.table_key(key),
            View::Pager(_) => self.pager_key(key),
            View::Logs(_) => self.logs_key(key),
            View::Xray(_) => self.xray_key(key),
            View::Forwards { .. } => self.forwards_key(key),
        }
    }

    fn plugin_key(&mut self, key: &KeyEvent) -> Option<Vec<Effect>> {
        let (names, obj, cells) = match self.top() {
            View::Table(t) => {
                let r = t.selected()?;
                let mut names = vec![t.res.plural.clone(), t.res.singular.clone(), t.res.name()];
                names.extend(t.res.short.iter().cloned());
                let cells: Vec<(String, String)> = t.cols.iter().map(|c| (c.name.to_uppercase().replace(' ', "-"), cell_value(t, c, r, self.now))).collect();
                (names, Some(t.obj(r)), cells)
            }
            _ => return None,
        };
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let plugin = self.config.plugins.iter().find(|p| p.applies(&names) && key_matches(&p.key, key))?.clone();
        let obj = obj?;
        let mut vars = vec![("NAME".to_string(), obj.name.clone()), ("NAMESPACE".to_string(), obj.ns.clone()), ("RESOURCE".to_string(), obj.res.name()), ("RESOURCE_NAME".to_string(), obj.res.plural.clone()), ("RESOURCE_GROUP".to_string(), obj.res.group.clone()), ("RESOURCE_VERSION".to_string(), obj.res.version.clone())];
        if obj.res.plural == CONTAINERS {
            vars.push(("CONTAINER".into(), obj.name.clone()));
        }
        vars.extend(cells.into_iter().map(|(k, v)| (format!("COL-{k}"), v)));
        let effect = KEffect::Plugin { plugin: plugin.clone(), vars };
        if plugin.confirm {
            self.overlay = Overlay::Confirm(Confirm { title: format!("Run {}?", plugin.name), lines: vec![format!("{} {}", plugin.command, plugin.args.join(" ")), format!("on {}", obj.label())], action: ConfirmAction::Run(vec![effect]) });
            return Some(Vec::new());
        }
        Some(vec![k(effect)])
    }

    fn open_prompt(&mut self, kind: PromptKind, text: String) {
        let mut p = Prompt { kind, text: text.clone(), mode: self.mode, suggestions: Vec::new(), tab: None, typed: text };
        if kind == PromptKind::Filter {
            if let Some(t) = self.top_table() {
                p.text = t.filter.raw.clone();
                p.mode = t.filter.mode;
            }
        }
        if kind == PromptKind::LogFilter {
            if let View::Logs(l) = self.top() {
                if let Some(f) = &l.filter {
                    p.text = f.raw.clone();
                    p.mode = f.mode;
                }
            }
        }
        self.history.pos = None;
        self.suggest(&mut p);
        self.overlay = Overlay::Prompt(p);
    }

    fn suggest(&self, p: &mut Prompt) {
        if p.kind != PromptKind::Command {
            return;
        }
        p.suggestions = command::complete(&p.text, &self.catalog.names(), &self.namespaces(), &self.contexts, &self.config.aliases);
    }

    fn prompt_key(&mut self, mut p: Prompt, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let live = |ui: &mut KubeUi, p: &Prompt| ui.prompt_live(p);
        match key.code {
            KeyCode::Esc => {
                // Esc on a filter clears it (as k9s).
                match p.kind {
                    PromptKind::Filter => {
                        p.text.clear();
                        live(self, &p);
                    }
                    PromptKind::LogFilter => {
                        if let Some(View::Logs(l)) = self.views.last_mut() {
                            l.filter = None;
                        }
                    }
                    PromptKind::Search => {
                        if let Some(View::Pager(pg)) = self.views.last_mut() {
                            pg.search = None;
                            pg.research();
                        }
                    }
                    _ => {}
                }
                return Vec::new();
            }
            KeyCode::Enter => return self.prompt_submit(p),
            KeyCode::Tab if p.kind == PromptKind::Command => {
                if p.suggestions.is_empty() && p.tab.is_none() {
                    return self.keep(p);
                }
                if p.tab.is_none() {
                    p.typed = p.text.clone();
                }
                let typed_sugg = command::complete(&p.typed, &self.catalog.names(), &self.namespaces(), &self.contexts, &self.config.aliases);
                if typed_sugg.is_empty() {
                    return self.keep(p);
                }
                let i = p.tab.map(|i| (i + 1) % typed_sugg.len()).unwrap_or(0);
                p.tab = Some(i);
                p.text = replace_last_word(&p.typed, &typed_sugg[i]);
                p.suggestions = typed_sugg;
                return self.keep(p);
            }
            KeyCode::Tab | KeyCode::BackTab if matches!(p.kind, PromptKind::Filter | PromptKind::LogFilter | PromptKind::Search) => {
                p.mode = p.mode.next();
                self.mode = p.mode;
            }
            KeyCode::Right if p.kind == PromptKind::Command => {
                if let Some(s) = p.suggestions.first() {
                    p.text = replace_last_word(&p.text, s);
                }
            }
            KeyCode::Up if p.kind == PromptKind::Command => {
                if let Some(h) = self.history.older() {
                    p.text = h.to_string();
                }
            }
            KeyCode::Down if p.kind == PromptKind::Command => {
                p.text = self.history.newer().unwrap_or_default().to_string();
            }
            KeyCode::Backspace => {
                if p.text.is_empty() && p.kind == PromptKind::Command {
                    return Vec::new();
                }
                p.text.pop();
            }
            KeyCode::Char('u') if ctrl => p.text.clear(),
            KeyCode::Char('w') if ctrl => {
                let t = p.text.trim_end();
                let cut = t.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
                p.text.truncate(cut);
            }
            KeyCode::Char(c) if !ctrl => p.text.push(c),
            _ => return self.keep(p),
        }
        if key.code != KeyCode::Tab {
            p.tab = None;
        }
        self.suggest(&mut p);
        live(self, &p);
        self.keep(p)
    }

    fn keep(&mut self, p: Prompt) -> Vec<Effect> {
        self.overlay = Overlay::Prompt(p);
        Vec::new()
    }

    /// Filters apply as they are typed.
    fn prompt_live(&mut self, p: &Prompt) {
        match p.kind {
            PromptKind::Filter => {
                let f = Filter::parse(&p.text, p.mode);
                if let Some(t) = self.top_table_mut() {
                    if t.filter != f {
                        t.filter = f;
                        t.sel = 0;
                        t.sel_key = None;
                    }
                }
                self.refilter();
            }
            PromptKind::LogFilter => {
                if let Some(View::Logs(l)) = self.views.last_mut() {
                    l.filter = Some(Filter::parse(&p.text, p.mode)).filter(|f| !f.pattern.is_empty());
                    l.offset = 0;
                }
            }
            PromptKind::Search => {
                if let Some(View::Pager(pg)) = self.views.last_mut() {
                    pg.search = Some(Filter::parse(&p.text, p.mode)).filter(|f| !f.pattern.is_empty());
                    pg.research();
                    let top = pg.top;
                    if let Some(&i) = pg.matches.iter().find(|&&i| i >= top).or(pg.matches.first()) {
                        pg.top = i;
                    }
                }
            }
            _ => {}
        }
    }

    fn prompt_submit(&mut self, p: Prompt) -> Vec<Effect> {
        match p.kind {
            PromptKind::Command => {
                self.history.push(&p.text);
                let mut effects = self.run_line(&format!(":{}", p.text));
                effects.push(k(KEffect::SaveHistory(self.history.entries.clone())));
                effects
            }
            PromptKind::Kubectl => {
                self.history.push(&format!("!{}", p.text));
                let mut e = self.kubectl(p.text.trim().to_string());
                e.push(k(KEffect::SaveHistory(self.history.entries.clone())));
                e
            }
            PromptKind::Filter => {
                if let Some(t) = self.top_table() {
                    let (raw, mode, id) = (t.filter.raw.clone(), t.filter.mode, t.res.id());
                    if let Some(e) = &t.filter.error {
                        self.say(e.clone(), true);
                    }
                    self.filters.insert(id, (raw, mode));
                }
                Vec::new()
            }
            PromptKind::LogFilter | PromptKind::Search => Vec::new(),
        }
    }

    /// Run a command line: `:pods -A`, `!get events` (hotkeys too).
    pub fn run_line(&mut self, line: &str) -> Vec<Effect> {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('!') {
            return self.kubectl(rest.trim().to_string());
        }
        let line = line.strip_prefix(':').unwrap_or(line);
        match command::parse(line, &self.config.aliases) {
            Ok(cmd) => self.command(cmd),
            Err(e) => {
                self.say(e, true);
                Vec::new()
            }
        }
    }

    fn kubectl(&mut self, args: String) -> Vec<Effect> {
        if args.is_empty() {
            self.say("kubectl what? e.g. !get events --sort-by=.lastTimestamp", true);
            return Vec::new();
        }
        let id = self.id();
        let mut p = Pager::new(id, format!("kubectl {args}"), PagerKind::Kubectl);
        p.lines.clear();
        self.push(View::Pager(p));
        vec![k(KEffect::Kubectl { id, args })]
    }

    fn command(&mut self, cmd: Cmd) -> Vec<Effect> {
        match cmd {
            Cmd::Quit => return vec![Effect::Quit],
            Cmd::Help => self.help(),
            Cmd::Kubectl(args) => return self.kubectl(args),
            Cmd::Forwards => self.push(View::Forwards { sel: 0 }),
            Cmd::Aliases => self.resource_picker(),
            Cmd::Context(None) => {
                self.overlay = Overlay::Picker(Picker { title: "context".into(), items: self.contexts.iter().map(|c| (c.clone(), c.clone())).collect(), filter: String::new(), sel: 0, pick: Pick::Context });
                return vec![k(KEffect::LoadContexts)];
            }
            Cmd::Context(Some(c)) => return self.use_context(c),
            Cmd::Namespace(n) => self.use_namespace(n.filter(|n| n != "all" && n != "-A")),
            Cmd::Xray { name, ns } => {
                let Some(res) = self.catalog.find(&name).cloned() else {
                    self.say(format!("unknown resource {name}"), true);
                    return Vec::new();
                };
                let ns = match ns {
                    Some(Ns::All) => None,
                    Some(Ns::Named(n)) => Some(n),
                    None => self.namespace.clone(),
                };
                let id = self.id();
                self.views.truncate(1);
                let ns_eff = ns.clone().filter(|_| res.namespaced);
                self.push(View::Xray(XrayView { id, res: res.clone(), ns: ns_eff.clone(), nodes: Vec::new(), folded: BTreeSet::new(), sel: 0, offset: Cell::new(0), height: Cell::new(10), loading: true, error: None }));
                return vec![k(KEffect::Xray { id, res, ns: ns_eff })];
            }
            Cmd::Resource { name, ns, filter } => {
                if name == "pf" || name == "portforwards" {
                    self.push(View::Forwards { sel: 0 });
                    return Vec::new();
                }
                let Some(res) = self.catalog.find(&name).cloned() else {
                    let more = if self.discovered { " (:aliases lists them)" } else { " (still discovering the API; try again)" };
                    self.say(format!("unknown resource {name:?}{more}"), true);
                    return Vec::new();
                };
                match ns {
                    Some(Ns::All) => self.namespace = None,
                    Some(Ns::Named(n)) => self.namespace = Some(n),
                    None => {}
                }
                let mut t = TableView::new(res.clone(), self.namespace.clone());
                let (raw, mode) = if filter.is_empty() { self.filters.get(&res.id()).cloned().unwrap_or_default() } else { (filter, self.mode) };
                t.filter = Filter::parse(&raw, mode);
                // A new command starts a new stack, as in k9s.
                while self.views.len() > 1 {
                    self.pop();
                }
                self.views[0] = View::Table(t);
                self.refresh_top();
            }
        }
        Vec::new()
    }

    fn use_context(&mut self, c: String) -> Vec<Effect> {
        self.say(format!("context {c}"), false);
        vec![Effect::UseContext(c)]
    }

    fn use_namespace(&mut self, ns: Option<String>) {
        self.namespace = ns.clone();
        for v in &mut self.views {
            if let View::Table(t) = v {
                if t.res.namespaced && !t.fixed_ns {
                    t.ns = ns.clone();
                    t.seen = None;
                    t.marks.clear();
                }
            }
        }
        self.refresh_top();
        self.say(format!("namespace {}", ns.as_deref().unwrap_or("all")), false);
    }

    fn resource_picker(&mut self) {
        let mut items: Vec<(String, String)> = self
            .catalog
            .list
            .iter()
            .map(|r| {
                let short = if r.short.is_empty() { String::new() } else { format!(" ({})", r.short.join(",")) };
                (format!("{}{short}  {}  {}", r.name(), r.api_version(), if r.namespaced { "" } else { "cluster" }), r.name())
            })
            .collect();
        items.sort();
        self.overlay = Overlay::Picker(Picker { title: format!("resources ({})", items.len()), items, filter: String::new(), sel: 0, pick: Pick::Resource });
    }

    /// A pager for a command's output (a background plugin); its id.
    pub fn open_output(&mut self, title: String) -> u64 {
        let id = self.id();
        let mut p = Pager::new(id, title, PagerKind::Kubectl);
        p.lines.clear();
        self.push(View::Pager(p));
        id
    }

    fn help(&mut self) {
        let id = self.id();
        let mut p = Pager::new(id, "help", PagerKind::Help);
        p.lines = help_lines(self);
        p.loading = false;
        self.push(View::Pager(p));
    }

    fn picker_key(&mut self, mut p: Picker, key: KeyEvent) -> Vec<Effect> {
        let shown = p.shown();
        match key.code {
            KeyCode::Esc => return Vec::new(),
            KeyCode::Down | KeyCode::Tab => p.sel = (p.sel + 1).min(shown.len().saturating_sub(1)),
            KeyCode::Up | KeyCode::BackTab => p.sel = p.sel.saturating_sub(1),
            KeyCode::PageDown => p.sel = (p.sel + 10).min(shown.len().saturating_sub(1)),
            KeyCode::PageUp => p.sel = p.sel.saturating_sub(10),
            KeyCode::Backspace => {
                p.filter.pop();
                p.sel = 0;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                p.filter.clear();
                p.sel = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if p.pick == Pick::Action && p.filter.is_empty() {
                    // The menu's keys work as themselves.
                    let acts = self.actions();
                    if let Some(a) = acts.iter().find(|a| a.code == KeyCode::Char(c) && a.mods == N) {
                        let ev = KeyEvent::new(a.code, a.mods);
                        return self.key_inner(ev).unwrap_or_default();
                    }
                }
                p.filter.push(c);
                p.sel = 0;
            }
            KeyCode::Enter => {
                let Some(&i) = shown.get(p.sel) else { return Vec::new() };
                let value = p.items[i].1.clone();
                return match p.pick {
                    Pick::Namespace => {
                        self.use_namespace(if value.is_empty() { None } else { Some(value) });
                        Vec::new()
                    }
                    Pick::Context => self.use_context(value),
                    Pick::Resource => self.command(Cmd::Resource { name: value, ns: None, filter: String::new() }),
                    Pick::Container { obj, purpose } => {
                        self.container_chosen(obj, purpose, if value.is_empty() { None } else { Some(value) });
                        Vec::new()
                    }
                    Pick::Action => {
                        let acts = self.actions();
                        match acts.iter().find(|a| a.key == value) {
                            Some(a) => {
                                let ev = KeyEvent::new(a.code, a.mods);
                                self.key_inner(ev).unwrap_or_default()
                            }
                            None => Vec::new(),
                        }
                    }
                };
            }
            _ => {}
        }
        self.overlay = Overlay::Picker(p);
        Vec::new()
    }

    fn confirm_key(&mut self, mut c: Confirm, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                let effects = match c.action {
                    ConfirmAction::Delete { objs, propagation, force } => {
                        if let Some(t) = self.top_table_mut() {
                            t.marks.clear();
                        }
                        vec![k(KEffect::Delete { objs, propagation, force })]
                    }
                    ConfirmAction::Run(list) => list.into_iter().map(k).collect(),
                };
                return effects;
            }
            KeyCode::Char('p') => {
                if let ConfirmAction::Delete { propagation, .. } = &mut c.action {
                    *propagation = propagation.next();
                }
            }
            KeyCode::Char('f') => {
                if let ConfirmAction::Delete { force, .. } = &mut c.action {
                    *force = !*force;
                }
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') => return Vec::new(),
            _ => {}
        }
        self.overlay = Overlay::Confirm(c);
        Vec::new()
    }

    fn input_key(&mut self, mut i: Input, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Esc => return Vec::new(),
            KeyCode::Backspace => {
                i.value.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => i.value.clear(),
            KeyCode::Char(c) => i.value.push(c),
            KeyCode::Enter => {
                let v = i.value.trim().to_string();
                match &i.kind {
                    InputKind::Scale(obj) => match v.parse::<i64>() {
                        Ok(n) if (0..=10_000).contains(&n) => return vec![k(KEffect::Scale { obj: obj.clone(), replicas: n })],
                        _ => i.error = Some("replicas: a number from 0 to 10000".into()),
                    },
                    InputKind::PortForward(obj) => {
                        let parsed = match v.split_once(':') {
                            Some((l, r)) => l.trim().parse::<u16>().ok().filter(|p| *p > 0).map(|l| (l, r.trim().to_string())),
                            None => v.parse::<u16>().ok().filter(|p| *p > 0).map(|p| (p, p.to_string())),
                        };
                        match parsed {
                            Some((local, remote)) if !remote.is_empty() => return vec![k(KEffect::PortForward { obj: obj.clone(), local, remote })],
                            _ => i.error = Some("local:remote, e.g. 8080:80 or 8080:http".into()),
                        }
                    }
                }
                self.overlay = Overlay::Input(i);
                return Vec::new();
            }
            _ => {}
        }
        i.error = None;
        self.overlay = Overlay::Input(i);
        Vec::new()
    }

    // ---- tables ----

    fn read_only(&mut self) -> bool {
        if self.config.read_only {
            self.say("read-only (ui.yaml readOnly: true)", true);
        }
        self.config.read_only
    }

    /// The actions the selected row's type has (the `m` menu, the help).
    pub fn actions(&self) -> Vec<Action> {
        let Some(t) = self.top_table() else { return Vec::new() };
        let kind = t.res.kind.as_str();
        let mut v = vec![act("enter", if drills(kind) { "show what it runs" } else { "describe" }, KeyCode::Enter, N), act("d", "describe", KeyCode::Char('d'), N), act("y", "YAML", KeyCode::Char('y'), N)];
        if kind != "Container" {
            v.push(act("e", "edit", KeyCode::Char('e'), N));
            v.push(act("ctrl-d", "delete", KeyCode::Char('d'), CTRL));
        }
        match kind {
            "Pod" => {
                v.push(act("l", "logs", KeyCode::Char('l'), N));
                v.push(act("p", "previous logs", KeyCode::Char('p'), N));
                v.push(act("s", "shell", KeyCode::Char('s'), N));
                v.push(act("F", "port-forward", KeyCode::Char('F'), N));
                v.push(act("ctrl-k", "kill (force delete)", KeyCode::Char('k'), CTRL));
                v.push(act("o", "its node", KeyCode::Char('o'), N));
            }
            "Container" => {
                v.push(act("l", "logs", KeyCode::Char('l'), N));
                v.push(act("p", "previous logs", KeyCode::Char('p'), N));
                v.push(act("s", "shell", KeyCode::Char('s'), N));
            }
            "Deployment" | "StatefulSet" | "ReplicaSet" | "ReplicationController" => {
                v.push(act("l", "logs of its pods", KeyCode::Char('l'), N));
                v.push(act("s", "scale", KeyCode::Char('s'), N));
                if kind != "ReplicaSet" && kind != "ReplicationController" {
                    v.push(act("r", "rollout restart", KeyCode::Char('r'), N));
                }
                if kind == "Deployment" {
                    v.push(act("u", "rollout undo", KeyCode::Char('u'), N));
                    v.push(act("H", "rollout history", KeyCode::Char('H'), N));
                }
                v.push(act("F", "port-forward", KeyCode::Char('F'), N));
                v.push(act("x", "xray", KeyCode::Char('x'), N));
            }
            "DaemonSet" => {
                v.push(act("l", "logs of its pods", KeyCode::Char('l'), N));
                v.push(act("r", "rollout restart", KeyCode::Char('r'), N));
                v.push(act("x", "xray", KeyCode::Char('x'), N));
            }
            "Job" => {
                v.push(act("l", "logs of its pods", KeyCode::Char('l'), N));
                v.push(act("x", "xray", KeyCode::Char('x'), N));
            }
            "CronJob" => {
                v.push(act("t", "trigger now", KeyCode::Char('t'), N));
                v.push(act("x", "xray", KeyCode::Char('x'), N));
            }
            "Service" => {
                v.push(act("l", "logs of its pods", KeyCode::Char('l'), N));
                v.push(act("F", "port-forward", KeyCode::Char('F'), N));
            }
            "Node" => {
                v.push(act("c", "cordon", KeyCode::Char('c'), N));
                v.push(act("u", "uncordon", KeyCode::Char('u'), N));
                v.push(act("r", "drain", KeyCode::Char('r'), N));
            }
            "Namespace" => v.push(act("u", "use it", KeyCode::Char('u'), N)),
            "Secret" => v.push(act("x", "decode", KeyCode::Char('x'), N)),
            _ => {}
        }
        if kind != "Container" && kind != "Node" {
            v.push(act("c", "copy name", KeyCode::Char('c'), N));
        }
        v.push(act("C", "copy YAML", KeyCode::Char('C'), N));
        v.push(act("E", "related events", KeyCode::Char('E'), N));
        if t.selected().is_some_and(|r| r.owners.iter().any(|o| o.controller)) {
            v.push(act("J", "jump to owner", KeyCode::Char('J'), N));
        }
        v
    }

    fn table_key(&mut self, key: KeyEvent) -> Handled {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(View::Table(t)) = self.views.last_mut() else { return None };
        let len = t.rows.len();
        let page = t.height.get().max(1);
        let movement = match key.code {
            KeyCode::Down | KeyCode::Char('j') if !ctrl => Some(t.sel as isize + 1),
            KeyCode::Up | KeyCode::Char('k') if !ctrl => Some(t.sel as isize - 1),
            KeyCode::PageDown => Some((t.sel + page) as isize),
            KeyCode::Char('f') if ctrl => Some((t.sel + page) as isize),
            KeyCode::PageUp => Some(t.sel as isize - page as isize),
            KeyCode::Char('b') if ctrl => Some(t.sel as isize - page as isize),
            KeyCode::Home | KeyCode::Char('g') => Some(0),
            KeyCode::End | KeyCode::Char('G') => Some(len as isize - 1),
            _ => None,
        };
        if let Some(i) = movement {
            t.select(i.clamp(0, len.saturating_sub(1) as isize) as usize);
            return Some(Vec::new());
        }
        let row = t.selected().cloned();
        let kind = t.res.kind.clone();
        let res = t.res.clone();
        match key.code {
            KeyCode::Esc => {
                if !t.marks.is_empty() {
                    t.marks.clear();
                } else if !t.filter.raw.is_empty() {
                    let id = t.res.id();
                    t.filter = Filter::default();
                    self.filters.remove(&id);
                    self.refilter();
                } else if self.views.len() > 1 {
                    self.pop();
                }
                return Some(Vec::new());
            }
            KeyCode::Char('/') => {
                self.open_prompt(PromptKind::Filter, String::new());
                return Some(Vec::new());
            }
            KeyCode::Char('w') if ctrl => {
                t.wide = !t.wide;
                return Some(Vec::new());
            }
            KeyCode::Char('z') if ctrl => {
                t.faults = !t.faults;
                let on = t.faults;
                self.refilter();
                self.say(if on { "faults only: rows that are not fine" } else { "all rows" }, false);
                return Some(Vec::new());
            }
            KeyCode::Char('r') if ctrl => {
                let key = t.key();
                return Some(vec![k(KEffect::Relist(key)), Effect::Refresh]);
            }
            KeyCode::Char('\\') if ctrl => {
                t.marks.clear();
                return Some(Vec::new());
            }
            KeyCode::Char(' ') => {
                if let Some(r) = &row {
                    let key = r.key();
                    if !t.marks.remove(&key) {
                        t.marks.insert(key);
                    }
                    let next = (t.sel + 1).min(len.saturating_sub(1));
                    t.select(next);
                }
                return Some(Vec::new());
            }
            KeyCode::Char('0') | KeyCode::Char('a') if !ctrl && res.namespaced => {
                // All namespaces, and back to the one before.
                let ns = match t.ns.clone() {
                    Some(n) => {
                        self.last_ns = Some(n);
                        None
                    }
                    None => Some(self.last_ns.clone().unwrap_or_else(|| "default".into())),
                };
                self.use_namespace(ns);
                return Some(Vec::new());
            }
            KeyCode::Char('n') if !ctrl => {
                let mut items = vec![("all namespaces".to_string(), String::new())];
                items.extend(self.namespaces().into_iter().map(|n| (n.clone(), n)));
                let cur = self.namespace.clone().unwrap_or_default();
                let sel = items.iter().position(|(_, v)| *v == cur).unwrap_or(0);
                self.overlay = Overlay::Picker(Picker { title: "namespace".into(), items, filter: String::new(), sel, pick: Pick::Namespace });
                return Some(Vec::new());
            }
            KeyCode::Char('m') if !ctrl => {
                let items: Vec<(String, String)> = self.actions().iter().map(|a| (format!("{:<7} {}", a.key, a.label), a.key.to_string())).collect();
                if row.is_some() {
                    self.overlay = Overlay::Picker(Picker { title: "actions".into(), items, filter: String::new(), sel: 0, pick: Pick::Action });
                }
                return Some(Vec::new());
            }
            KeyCode::Char('<') | KeyCode::Char('>') | KeyCode::Char('O') | KeyCode::Char('N') | KeyCode::Char('A') | KeyCode::Char('S') => {
                sort_key(t, key.code);
                self.refilter();
                return Some(Vec::new());
            }
            KeyCode::Char('f') if !ctrl => {
                self.push(View::Forwards { sel: 0 });
                return Some(Vec::new());
            }
            _ => {}
        }
        let Some(r) = row else {
            // Keys that need a row do nothing without one; others are the app's.
            return match key.code {
                KeyCode::Char(c) if c.is_ascii_digit() || c == 'q' => None,
                KeyCode::Tab | KeyCode::BackTab => None,
                _ => Some(Vec::new()),
            };
        };
        let obj = ObjRef { res: res.clone(), ns: r.ns.clone(), name: r.name.clone() };
        let targets = t.targets();
        let effects = match (key.code, ctrl, kind.as_str()) {
            (KeyCode::Enter, false, _) => return Some(self.drill(obj, &r)),
            (KeyCode::Char('d'), false, "Container") => return Some(self.describe(self.container_pod().unwrap_or(obj))),
            (KeyCode::Char('d'), false, _) => return Some(self.describe(obj)),
            (KeyCode::Char('y'), false, "Container") => return Some(self.yaml(self.container_pod().unwrap_or(obj))),
            (KeyCode::Char('y'), false, _) => return Some(self.yaml(obj)),
            (KeyCode::Char('e'), false, k) if k != "Container" => {
                if self.read_only() {
                    return Some(Vec::new());
                }
                vec![KEffect::Edit { obj, text: None }]
            }
            (KeyCode::Char('d'), true, k) | (KeyCode::Delete, _, k) if k != "Container" => {
                if self.read_only() {
                    return Some(Vec::new());
                }
                self.confirm_delete(targets, false);
                Vec::new()
            }
            (KeyCode::Char('k'), true, "Pod") => {
                if self.read_only() {
                    return Some(Vec::new());
                }
                self.confirm_delete(targets, true);
                Vec::new()
            }
            (KeyCode::Char('l'), false, "Pod") => vec![KEffect::Containers { obj, purpose: Purpose::Logs { previous: false } }],
            (KeyCode::Char('p'), false, "Pod") => vec![KEffect::Containers { obj, purpose: Purpose::Logs { previous: true } }],
            (KeyCode::Char('l' | 'p'), false, "Container") => {
                let previous = key.code == KeyCode::Char('p');
                if let Some(pod) = self.container_pod() {
                    self.container_chosen(pod, Purpose::Logs { previous }, Some(obj.name));
                }
                Vec::new()
            }
            (KeyCode::Char('l'), false, "Deployment" | "StatefulSet" | "ReplicaSet" | "DaemonSet" | "Job" | "Service" | "ReplicationController") => {
                let title = format!("{} (all pods)", obj.label());
                self.open_logs(LogTarget::Owner(obj), title, false);
                Vec::new()
            }
            (KeyCode::Char('s'), false, "Pod") => {
                if self.read_only() {
                    return Some(Vec::new());
                }
                vec![KEffect::Containers { obj, purpose: Purpose::Shell }]
            }
            (KeyCode::Char('s'), false, "Container") => {
                if self.read_only() {
                    return Some(Vec::new());
                }
                match self.container_pod() {
                    Some(pod) => vec![KEffect::Shell { ns: pod.ns, pod: pod.name, container: Some(obj.name) }],
                    None => Vec::new(),
                }
            }
            (KeyCode::Char('s'), false, "Deployment" | "StatefulSet" | "ReplicaSet" | "ReplicationController") => {
                if self.read_only() {
                    return Some(Vec::new());
                }
                let current = r.cell(&self.top_table().map(|t| table_columns(t)).unwrap_or_default(), "Ready").and_then(|s| s.split('/').nth(1)).unwrap_or("1").to_string();
                self.overlay = Overlay::Input(Input { prompt: format!("scale {} to", obj.label()), value: current.clone(), hint: format!("now {current} replicas"), kind: InputKind::Scale(obj), error: None });
                Vec::new()
            }
            (KeyCode::Char('r'), false, "Deployment" | "StatefulSet" | "DaemonSet") => {
                if self.read_only() {
                    return Some(Vec::new());
                }
                let lines = targets.iter().take(6).map(ObjRef::label).collect::<Vec<_>>();
                self.overlay = Overlay::Confirm(Confirm { title: "Rollout restart?".into(), lines: [lines, vec!["Its pods are replaced one by one.".into()]].concat(), action: ConfirmAction::Run(vec![KEffect::Restart(targets)]) });
                Vec::new()
            }
            (KeyCode::Char('u'), false, "Deployment") => {
                if self.read_only() {
                    return Some(Vec::new());
                }
                self.overlay = Overlay::Confirm(Confirm { title: format!("Roll back {}?", obj.label()), lines: vec!["Back to the previous revision's pod template (as kubectl rollout undo).".into()], action: ConfirmAction::Run(vec![KEffect::Undo(obj)]) });
                Vec::new()
            }
            (KeyCode::Char('H'), false, "Deployment") => {
                let id = self.id();
                let mut p = Pager::new(id, format!("rollout history {}", obj.label()), PagerKind::Text);
                p.obj = Some(obj.clone());
                self.push(View::Pager(p));
                vec![KEffect::History { id, obj }]
            }
            (KeyCode::Char('u'), false, "Namespace") => {
                self.use_namespace(Some(obj.name));
                Vec::new()
            }
            (KeyCode::Char('c'), false, "Node") | (KeyCode::Char('u'), false, "Node") => {
                if self.read_only() {
                    return Some(Vec::new());
                }
                let nodes: Vec<String> = targets.iter().map(|o| o.name.clone()).collect();
                vec![KEffect::Cordon { nodes, on: key.code == KeyCode::Char('c') }]
            }
            (KeyCode::Char('r'), false, "Node") => {
                if self.read_only() {
                    return Some(Vec::new());
                }
                let nodes: Vec<String> = targets.iter().map(|o| o.name.clone()).collect();
                self.overlay = Overlay::Confirm(Confirm {
                    title: format!("Drain {}?", nodes.join(", ")),
                    lines: vec!["Cordons it, then evicts its pods (not DaemonSet or mirror pods),".into(), "honoring PodDisruptionBudgets, as kubectl drain --ignore-daemonsets.".into()],
                    action: ConfirmAction::Run(vec![KEffect::Drain { nodes }]),
                });
                Vec::new()
            }
            (KeyCode::Char('t'), false, "CronJob") => {
                if self.read_only() {
                    return Some(Vec::new());
                }
                self.overlay = Overlay::Confirm(Confirm { title: format!("Trigger {} now?", obj.label()), lines: vec!["Creates a Job from its template (kubectl create job --from).".into()], action: ConfirmAction::Run(vec![KEffect::Trigger(obj)]) });
                Vec::new()
            }
            (KeyCode::Char('x'), false, "Secret") => {
                let id = self.id();
                let mut p = Pager::new(id, format!("{} (decoded)", obj.label()), PagerKind::Yaml);
                p.obj = Some(obj.clone());
                self.push(View::Pager(p));
                vec![KEffect::Decode { id, obj }]
            }
            (KeyCode::Char('x'), false, "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet" | "Job" | "CronJob") => {
                let id = self.id();
                let ns = Some(obj.ns.clone());
                let mut x = XrayView { id, res: res.clone(), ns: ns.clone(), nodes: Vec::new(), folded: BTreeSet::new(), sel: 0, offset: Cell::new(0), height: Cell::new(10), loading: true, error: None };
                x.nodes.clear();
                self.push(View::Xray(x));
                // The tree of all of them; the selected one first.
                vec![KEffect::Xray { id, res, ns }]
            }
            (KeyCode::Char('F'), false, "Pod" | "Service" | "Deployment" | "StatefulSet" | "ReplicaSet") => vec![KEffect::PortForwardPrepare(obj)],
            (KeyCode::Char('o'), false, "Pod") => {
                let cols = self.top_table().map(table_columns).unwrap_or_default();
                match r.cell(&cols, "Node").filter(|n| !n.is_empty() && *n != "<none>") {
                    Some(node) => {
                        if let Some(nodes) = self.catalog.find("nodes").cloned() {
                            let mut t2 = TableView::new(nodes, None);
                            t2.scope.name = Some(node.to_string());
                            t2.scope.title = Some(format!("node of {}", obj.name));
                            self.push(View::Table(t2));
                        }
                    }
                    None => self.say("not scheduled on a node yet", true),
                }
                Vec::new()
            }
            (KeyCode::Char('J'), false, _) => {
                match r.owners.iter().find(|o| o.controller).or(r.owners.first()) {
                    Some(o) => {
                        let owner = o.clone();
                        match self.catalog.list.iter().find(|x| x.kind == owner.kind).cloned() {
                            Some(ores) => {
                                let mut t2 = TableView::new(ores, if obj.ns.is_empty() { None } else { Some(obj.ns.clone()) });
                                t2.fixed_ns = true;
                                t2.scope.name = Some(owner.name.clone());
                                t2.scope.title = Some(format!("owner of {}", obj.name));
                                self.push(View::Table(t2));
                            }
                            None => self.say(format!("owner {}/{} is of an unknown type", owner.kind, owner.name), true),
                        }
                    }
                    None => self.say("it has no owner", false),
                }
                Vec::new()
            }
            (KeyCode::Char('E'), false, _) => {
                if let Some(ev) = self.catalog.find("events").cloned() {
                    let mut t2 = TableView::new(ev, if obj.ns.is_empty() { None } else { Some(obj.ns.clone()) });
                    t2.fixed_ns = true;
                    let mut fields = format!("involvedObject.name={}", obj.name);
                    if res.plural != CONTAINERS {
                        fields.push_str(&format!(",involvedObject.kind={}", res.kind));
                    }
                    t2.scope.fields = Some(fields);
                    t2.scope.title = Some(format!("events of {}", obj.label()));
                    t2.sort = Some(("LAST SEEN".into(), false));
                    self.push(View::Table(t2));
                }
                Vec::new()
            }
            (KeyCode::Char('c'), false, _) => {
                self.say(format!("copied {}", obj.name), false);
                vec![KEffect::Copy(obj.name)]
            }
            (KeyCode::Char('C'), false, _) => vec![KEffect::CopyYaml(obj)],
            (KeyCode::Char(c), false, _) if c.is_ascii_digit() || c == 'q' => return None,
            (KeyCode::Tab | KeyCode::BackTab, _, _) => return None,
            _ => Vec::new(),
        };
        Some(effects.into_iter().map(k).collect())
    }

    /// The pod of the containers view on top.
    fn container_pod(&self) -> Option<ObjRef> {
        let t = self.top_table()?;
        if t.res.plural != CONTAINERS {
            return None;
        }
        let name = t.scope.fields.as_deref()?.strip_prefix("metadata.name=")?.to_string();
        Some(ObjRef { res: self.catalog.find("pods")?.clone(), ns: t.ns.clone().unwrap_or_default(), name })
    }

    fn confirm_delete(&mut self, objs: Vec<ObjRef>, force: bool) {
        if objs.is_empty() {
            return;
        }
        let mut lines: Vec<String> = objs.iter().take(6).map(ObjRef::label).collect();
        if objs.len() > 6 {
            lines.push(format!("and {} more", objs.len() - 6));
        }
        if objs.iter().any(|o| o.res.kind == "Namespace") {
            lines.push("Everything in the namespace is deleted.".into());
        }
        let title = if force { format!("Kill {} (no grace period)?", plural(objs.len(), "object")) } else { format!("Delete {}?", plural(objs.len(), "object")) };
        self.overlay = Overlay::Confirm(Confirm { title, lines, action: ConfirmAction::Delete { objs, propagation: Propagation::Background, force } });
    }

    fn describe(&mut self, obj: ObjRef) -> Vec<Effect> {
        let id = self.id();
        let mut p = Pager::new(id, format!("describe {}", obj.label()), PagerKind::Describe);
        p.obj = Some(obj.clone());
        self.push(View::Pager(p));
        vec![k(KEffect::Describe { id, obj })]
    }

    fn yaml(&mut self, obj: ObjRef) -> Vec<Effect> {
        let id = self.id();
        let mut p = Pager::new(id, obj.label(), PagerKind::Yaml);
        p.obj = Some(obj.clone());
        self.push(View::Pager(p));
        vec![k(KEffect::Yaml { id, obj, managed: false })]
    }

    fn drill(&mut self, obj: ObjRef, r: &Row) -> Vec<Effect> {
        match obj.res.kind.as_str() {
            "Pod" => {
                let mut t = TableView::new(containers_resource(), Some(obj.ns.clone()));
                t.fixed_ns = true;
                t.scope.fields = Some(format!("metadata.name={}", obj.name));
                t.scope.title = Some(format!("containers of {}", obj.name));
                self.push(View::Table(t));
                Vec::new()
            }
            "Container" => {
                if let Some(pod) = self.container_pod() {
                    self.container_chosen(pod, Purpose::Logs { previous: false }, Some(obj.name));
                }
                Vec::new()
            }
            "Namespace" => {
                self.use_namespace(Some(obj.name));
                self.command(Cmd::Resource { name: "pods".into(), ns: None, filter: String::new() })
            }
            "CustomResourceDefinition" => self.command(Cmd::Resource { name: obj.name.clone(), ns: None, filter: String::new() }),
            "CronJob" => {
                if let Some(jobs) = self.catalog.find("jobs").cloned() {
                    let mut t = TableView::new(jobs, Some(obj.ns.clone()));
                    t.fixed_ns = true;
                    t.scope.owner_uid = Some(r.uid.clone());
                    t.scope.title = Some(format!("jobs of {}", obj.name));
                    self.push(View::Table(t));
                }
                Vec::new()
            }
            k2 if drills(k2) => vec![k(KEffect::DrillDown(obj))],
            _ => self.describe(obj),
        }
    }

    // ---- pagers ----

    fn pager_key(&mut self, key: KeyEvent) -> Handled {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(View::Pager(p)) = self.views.last_mut() else { return None };
        let page = p.height.get().max(1);
        let max = p.max_top();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if p.search.is_some() && key.code == KeyCode::Esc {
                    p.search = None;
                    p.research();
                } else {
                    self.pop();
                }
            }
            KeyCode::Down | KeyCode::Char('j') if !ctrl => p.top = (p.top + 1).min(max),
            KeyCode::Up | KeyCode::Char('k') if !ctrl => {
                p.top = p.top.saturating_sub(1);
                p.tail = false;
            }
            KeyCode::PageDown | KeyCode::Char(' ') => p.top = (p.top + page).min(max),
            KeyCode::Char('f') if ctrl => p.top = (p.top + page).min(max),
            KeyCode::PageUp | KeyCode::Char('b') => {
                p.top = p.top.saturating_sub(page);
                p.tail = false;
            }
            KeyCode::Home | KeyCode::Char('g') => {
                p.top = 0;
                p.tail = false;
            }
            KeyCode::End | KeyCode::Char('G') => {
                p.top = max;
                p.tail = p.kind == PagerKind::Kubectl;
            }
            KeyCode::Left | KeyCode::Char('h') => p.left = p.left.saturating_sub(8),
            KeyCode::Right | KeyCode::Char('l') => p.left += 8,
            KeyCode::Char('w') => p.wrap = !p.wrap,
            KeyCode::Char('/') => {
                self.open_prompt(PromptKind::Search, String::new());
            }
            KeyCode::Char('n') => p.jump(true),
            KeyCode::Char('N') => p.jump(false),
            KeyCode::Char('s') if ctrl => {
                let name = p.title.clone();
                let lines = p.lines.clone();
                return Some(vec![k(KEffect::Save { name, lines })]);
            }
            KeyCode::Char('c') if !ctrl => {
                let text = p.lines.join("\n");
                let n = p.lines.len();
                self.say(format!("copied {n} lines"), false);
                return Some(vec![k(KEffect::Copy(text))]);
            }
            KeyCode::Char('y') | KeyCode::Enter if p.pending.is_some() => {
                let body = p.pending.take().unwrap_or_default();
                let obj = p.obj.clone();
                self.pop();
                if let Some(obj) = obj {
                    return Some(vec![k(KEffect::Apply { obj, body })]);
                }
            }
            KeyCode::Char('e') if p.pending.is_some() => {
                let body = p.pending.take().unwrap_or_default();
                let obj = p.obj.clone();
                self.pop();
                if let Some(obj) = obj {
                    return Some(vec![k(KEffect::Edit { obj, text: Some(body) })]);
                }
            }
            KeyCode::Char('e') if p.obj.is_some() && matches!(p.kind, PagerKind::Yaml | PagerKind::Describe) => {
                if self.read_only() {
                    return Some(Vec::new());
                }
                let Some(View::Pager(p)) = self.views.last() else { return Some(Vec::new()) };
                let obj = p.obj.clone().unwrap();
                return Some(vec![k(KEffect::Edit { obj, text: None })]);
            }
            KeyCode::Char('y') if p.obj.is_some() && p.kind == PagerKind::Describe => {
                let obj = p.obj.clone().unwrap();
                self.pop();
                return Some(self.yaml(obj));
            }
            KeyCode::Char('d') if p.obj.is_some() && p.kind == PagerKind::Yaml => {
                let obj = p.obj.clone().unwrap();
                self.pop();
                return Some(self.describe(obj));
            }
            KeyCode::Char('m') if p.obj.is_some() && p.kind == PagerKind::Yaml => {
                p.managed = !p.managed;
                p.loading = true;
                let (id, obj, managed) = (p.id, p.obj.clone().unwrap(), p.managed);
                return Some(vec![k(KEffect::Yaml { id, obj, managed })]);
            }
            KeyCode::Char('r') if ctrl && p.obj.is_some() => {
                let (id, obj, managed) = (p.id, p.obj.clone().unwrap(), p.managed);
                p.loading = true;
                return Some(vec![k(match p.kind {
                    PagerKind::Describe => KEffect::Describe { id, obj },
                    PagerKind::Text => KEffect::History { id, obj },
                    _ => KEffect::Yaml { id, obj, managed },
                })]);
            }
            KeyCode::Char(c) if (c.is_ascii_digit() && c != '0') => return None,
            KeyCode::Tab | KeyCode::BackTab => return None,
            _ => {}
        }
        Some(Vec::new())
    }

    // ---- logs ----

    fn logs_key(&mut self, key: KeyEvent) -> Handled {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(View::Logs(l)) = self.views.last_mut() else { return None };
        let len = l.visible().len();
        let page = l.height.get().max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if key.code == KeyCode::Esc && l.filter.is_some() {
                    l.filter = None;
                    l.offset = 0;
                } else {
                    self.pop();
                }
            }
            KeyCode::Char('f') | KeyCode::Char('s') if !ctrl => {
                l.follow = !l.follow;
                l.offset = 0;
                let on = l.follow;
                self.say(if on { "following" } else { "paused (f follows again)" }, false);
            }
            KeyCode::Char('w') => l.wrap = !l.wrap,
            KeyCode::Char('t') => l.timestamps = !l.timestamps,
            KeyCode::Up | KeyCode::Char('k') if !ctrl => {
                l.follow = false;
                l.offset = (l.offset + 1).min(len);
            }
            KeyCode::Down | KeyCode::Char('j') if !ctrl => {
                l.offset = l.offset.saturating_sub(1);
                l.follow = l.offset == 0;
            }
            KeyCode::PageUp | KeyCode::Char('b') => {
                l.follow = false;
                l.offset = (l.offset + page).min(len);
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                l.offset = l.offset.saturating_sub(page);
                l.follow = l.offset == 0;
            }
            KeyCode::Char('g') | KeyCode::Home => {
                l.follow = false;
                l.offset = len;
            }
            KeyCode::Char('G') | KeyCode::End => {
                l.follow = true;
                l.offset = 0;
            }
            KeyCode::Char('x') => {
                l.lines.clear();
                l.offset = 0;
            }
            KeyCode::Char('/') => self.open_prompt(PromptKind::LogFilter, String::new()),
            KeyCode::Char('p') => {
                // Again, with (or without) the previous containers' logs.
                let previous = !l.previous;
                let (target, title) = (l.target.clone(), l.title.clone());
                self.pop();
                self.open_logs(target, title, previous);
                return Some(Vec::new());
            }
            KeyCode::Char('s') if ctrl => {
                let lines: Vec<String> = l.visible().into_iter().map(|i| log_text(l, &l.lines[i], true)).collect();
                return Some(vec![k(KEffect::Save { name: format!("logs {}", l.title), lines })]);
            }
            KeyCode::Char('c') if !ctrl => {
                let shown = l.visible();
                let take = shown.len().min(2000);
                let lines: Vec<String> = shown[shown.len() - take..].iter().map(|&i| log_text(l, &l.lines[i], l.timestamps)).collect();
                self.say(format!("copied the last {take} lines"), false);
                return Some(vec![k(KEffect::Copy(lines.join("\n")))]);
            }
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => return None,
            KeyCode::Tab | KeyCode::BackTab => return None,
            _ => {}
        }
        Some(Vec::new())
    }

    // ---- xray ----

    fn xray_key(&mut self, key: KeyEvent) -> Handled {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(View::Xray(x)) = self.views.last_mut() else { return None };
        let shown = x.shown();
        let page = x.height.get().max(1);
        let cur = shown.iter().position(|&i| i == x.sel).unwrap_or(0);
        let mv = |x: &mut XrayView, to: isize| {
            let to = to.clamp(0, shown.len().saturating_sub(1) as isize) as usize;
            if let Some(&i) = shown.get(to) {
                x.sel = i;
            }
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.pop(),
            KeyCode::Down | KeyCode::Char('j') => mv(x, cur as isize + 1),
            KeyCode::Up | KeyCode::Char('k') => mv(x, cur as isize - 1),
            KeyCode::PageDown => mv(x, (cur + page) as isize),
            KeyCode::PageUp => mv(x, cur as isize - page as isize),
            KeyCode::Char('g') | KeyCode::Home => mv(x, 0),
            KeyCode::Char('G') | KeyCode::End => mv(x, shown.len() as isize),
            KeyCode::Enter | KeyCode::Char(' ') => {
                let s = x.sel;
                if !x.folded.remove(&s) {
                    x.folded.insert(s);
                }
            }
            KeyCode::Char('r') if ctrl => {
                x.loading = true;
                let (id, res, ns) = (x.id, x.res.clone(), x.ns.clone());
                return Some(vec![k(KEffect::Xray { id, res, ns })]);
            }
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => return None,
            KeyCode::Tab | KeyCode::BackTab => return None,
            _ => {
                let node = x.nodes.get(x.sel).cloned();
                let Some(n) = node else { return Some(Vec::new()) };
                if let Some((ns, pod)) = &n.container_of {
                    let pod_res = self.catalog.find("pods").cloned().unwrap_or_default();
                    let pod = ObjRef { res: pod_res, ns: ns.clone(), name: pod.clone() };
                    return Some(match key.code {
                        KeyCode::Char('l') => {
                            self.container_chosen(pod, Purpose::Logs { previous: false }, Some(n.name));
                            Vec::new()
                        }
                        KeyCode::Char('s') if !self.read_only() => vec![k(KEffect::Shell { ns: ns.clone(), pod: pod.name, container: Some(n.name) })],
                        _ => Vec::new(),
                    });
                }
                let Some(obj) = n.obj else { return Some(Vec::new()) };
                return Some(match (key.code, ctrl) {
                    (KeyCode::Char('d'), false) => self.describe(obj),
                    (KeyCode::Char('y'), false) => self.yaml(obj),
                    (KeyCode::Char('e'), false) if !self.read_only() => vec![k(KEffect::Edit { obj, text: None })],
                    (KeyCode::Char('d'), true) if !self.read_only() => {
                        self.confirm_delete(vec![obj], false);
                        Vec::new()
                    }
                    (KeyCode::Char('l'), false) if obj.res.kind == "Pod" => vec![k(KEffect::Containers { obj, purpose: Purpose::Logs { previous: false } })],
                    (KeyCode::Char('l'), false) if drills(&obj.res.kind) => {
                        let title = format!("{} (all pods)", obj.label());
                        self.open_logs(LogTarget::Owner(obj), title, false);
                        Vec::new()
                    }
                    (KeyCode::Char('s'), false) if obj.res.kind == "Pod" && !self.read_only() => vec![k(KEffect::Containers { obj, purpose: Purpose::Shell })],
                    _ => Vec::new(),
                });
            }
        }
        Some(Vec::new())
    }

    fn forwards_key(&mut self, key: KeyEvent) -> Handled {
        let n = self.forwards.len();
        let Some(View::Forwards { sel }) = self.views.last_mut() else { return None };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.pop(),
            KeyCode::Down | KeyCode::Char('j') => *sel = (*sel + 1).min(n.saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(f) = self.forwards.get(*sel) {
                    let local = f.local;
                    return Some(vec![k(KEffect::StopForward(local))]);
                }
            }
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => return None,
            KeyCode::Tab | KeyCode::BackTab => return None,
            _ => {}
        }
        Some(Vec::new())
    }

    // ---- mouse ----

    /// Wheel scrolls; a click selects the row under it (`top`: the first
    /// list row's screen line).
    pub fn mouse(&mut self, ev: MouseEvent, list_top: u16) -> Vec<Effect> {
        let down = KeyEvent::from(KeyCode::Down);
        let up = KeyEvent::from(KeyCode::Up);
        match ev.kind {
            MouseEventKind::ScrollDown => {
                for _ in 0..3 {
                    self.key(down);
                }
            }
            MouseEventKind::ScrollUp => {
                for _ in 0..3 {
                    self.key(up);
                }
            }
            MouseEventKind::Down(_) => {
                if let Some(View::Table(t)) = self.views.last_mut() {
                    if list_top > 0 && ev.row >= list_top {
                        let i = (ev.row - list_top) as usize + t.offset.get();
                        if i < t.rows.len() {
                            t.select(i);
                        }
                    }
                }
            }
            _ => {}
        }
        Vec::new()
    }
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

/// Types whose Enter shows the pods they run.
pub fn drills(kind: &str) -> bool {
    matches!(kind, "Deployment" | "ReplicaSet" | "StatefulSet" | "DaemonSet" | "Job" | "Service" | "ReplicationController" | "Node")
}

fn replace_last_word(text: &str, word: &str) -> String {
    if text.ends_with(' ') || text.is_empty() {
        return format!("{text}{word}");
    }
    match text.rfind(' ') {
        Some(i) => format!("{}{word}", &text[..=i]),
        None => word.to_string(),
    }
}

/// The length of a leading RFC 3339 timestamp and its space.
fn ts_prefix(line: &str) -> u8 {
    let b = line.as_bytes();
    if b.len() > 20 && b[4] == b'-' && b[10] == b'T' {
        if let Some(i) = line[..b.len().min(40)].find(' ') {
            return (i + 1) as u8;
        }
    }
    0
}

/// A log line as text: with its source's prefix when there are several,
/// and the timestamp when shown.
pub fn log_text(l: &LogsView, line: &LogLine, ts: bool) -> String {
    let mut s = String::new();
    if ts && line.ts_len > 0 {
        s.push_str(line.ts());
        s.push(' ');
    }
    if l.sources.len() > 1 {
        s.push_str(&format!("[{}] ", l.sources.get(line.src as usize).map(String::as_str).unwrap_or("?")));
    }
    s.push_str(line.body());
    s
}

/// The table's columns as the server gave them.
fn table_columns(t: &TableView) -> Vec<Column> {
    t.seen.as_ref().map(|s| s.columns.as_ref().clone()).unwrap_or_default()
}

/// The text of a drawn cell.
pub fn cell_value(t: &TableView, c: &DCol, r: &Row, now: i64) -> String {
    match &c.source {
        Source::Namespace => r.ns.clone(),
        Source::Cell(i) => r.cells.get(*i).cloned().unwrap_or_default(),
        Source::Age => {
            if r.created > 0 {
                super::describe::human((now - r.created).max(0))
            } else {
                String::new()
            }
        }
        Source::Ext(name) => t.ext.iter().find(|(n, _)| n == name).and_then(|(_, m)| m.get(&r.key())).cloned().unwrap_or_default(),
    }
}

/// `<` `>` move the sort column, `O` reverses it; `N`, `A`, `S` sort by
/// name, age, status (as k9s).
fn sort_key(t: &mut TableView, code: KeyCode) {
    let names: Vec<String> = t.cols.iter().map(|c| c.name.clone()).collect();
    if names.is_empty() {
        return;
    }
    let cur = t.sort.as_ref().and_then(|(n, _)| names.iter().position(|x| x == n));
    let desc = t.sort.as_ref().map(|s| s.1).unwrap_or(false);
    let by = |want: &[&str]| names.iter().find(|n| want.iter().any(|w| n.eq_ignore_ascii_case(w))).cloned();
    t.sort = match code {
        KeyCode::Char('>') => Some((names[cur.map(|i| (i + 1) % names.len()).unwrap_or(0)].clone(), desc)),
        KeyCode::Char('<') => Some((names[cur.map(|i| (i + names.len() - 1) % names.len()).unwrap_or(names.len() - 1)].clone(), desc)),
        KeyCode::Char('O') => Some((cur.map(|i| names[i].clone()).unwrap_or_else(|| names[0].clone()), !desc)),
        KeyCode::Char('N') => by(&["NAME"]).map(|n| (n, t.sort.as_ref().is_some_and(|(s, d)| s.eq_ignore_ascii_case("NAME") && !d))),
        // Newest first, as k9s.
        KeyCode::Char('A') => by(&["AGE", "LAST SEEN"]).map(|n| (n, !t.sort.as_ref().is_some_and(|(s, d)| (s == "AGE" || s == "LAST SEEN") && *d))),
        KeyCode::Char('S') => by(&["STATUS", "PHASE", "TYPE"]).map(|n| (n, t.sort.as_ref().is_some_and(|(s, d)| s.eq_ignore_ascii_case("STATUS") && !d))),
        _ => t.sort.clone(),
    };
}

/// Filter, sort and size a table's rows from its snapshot.
fn recompute(t: &mut TableView, snap: Option<Arc<Snapshot>>, ext: &[(String, Arc<HashMap<String, String>>)]) {
    t.seen = snap.clone();
    t.ext = ext.to_vec();
    let Some(snap) = snap else {
        t.rows.clear();
        t.total = 0;
        t.synced = false;
        t.cols = Vec::new();
        return;
    };
    t.synced = snap.synced;
    t.error = snap.error.clone();
    t.total = snap.rows.len();
    // Columns: namespace first across namespaces, the server's (Age from
    // the creation time), then extras.
    let mut cols: Vec<DCol> = Vec::new();
    if t.all_namespaces() {
        cols.push(DCol { name: "NAMESPACE".into(), source: Source::Namespace, priority: 0, width: 9, numeric: false });
    }
    for (i, c) in snap.columns.iter().enumerate() {
        let source = if c.is_age() { Source::Age } else { Source::Cell(i) };
        cols.push(DCol { name: c.name.to_uppercase(), source, priority: c.priority, width: c.name.len(), numeric: matches!(c.kind.as_str(), "integer" | "number") });
    }
    for (name, _) in ext {
        cols.push(DCol { name: name.to_uppercase(), source: Source::Ext(name.clone()), priority: 0, width: name.len(), numeric: true });
    }
    let owner = t.scope.owner_uid.clone();
    let mut kept: Vec<(i64, Arc<Row>)> = snap
        .rows
        .iter()
        .filter(|r| t.scope.labels.as_ref().is_none_or(|(_, s)| s.matches(&r.labels)))
        .filter(|r| owner.as_ref().is_none_or(|u| r.owners.iter().any(|o| o.uid == *u)))
        .filter(|r| t.scope.name.as_ref().is_none_or(|n| r.name == *n))
        .filter(|r| !t.faults || matches!(r.health, Health::Warn | Health::Bad))
        .filter_map(|r| t.filter.matches(r).map(|s| (s, r.clone())))
        .collect();
    let ext_value = |name: &str, r: &Row| ext.iter().find(|(n, _)| n == name).and_then(|(_, m)| m.get(&r.key())).cloned().unwrap_or_default();
    if let Some((name, desc)) = &t.sort {
        if let Some(c) = cols.iter().find(|c| c.name == *name).cloned() {
            kept.sort_by(|(_, a), (_, b)| {
                let o = match &c.source {
                    Source::Age => b.created.cmp(&a.created),
                    Source::Namespace => a.ns.cmp(&b.ns),
                    Source::Cell(i) => compare_cells(a.cells.get(*i).map(String::as_str).unwrap_or(""), b.cells.get(*i).map(String::as_str).unwrap_or("")),
                    Source::Ext(n) => compare_cells(&ext_value(n, a), &ext_value(n, b)),
                };
                let o = o.then_with(|| a.key().cmp(&b.key()));
                if *desc { o.reverse() } else { o }
            });
        }
    } else if t.filter.mode == Mode::Fuzzy && !t.filter.pattern.is_empty() {
        kept.sort_by(|a, b| b.0.cmp(&a.0));
    }
    t.rows = kept.into_iter().map(|(_, r)| r).collect();
    // Widths from what is shown (capped).
    for c in &mut cols {
        let w = t
            .rows
            .iter()
            .map(|r| match &c.source {
                Source::Namespace => r.ns.chars().count(),
                Source::Cell(i) => r.cells.get(*i).map(|s| s.chars().count()).unwrap_or(0),
                Source::Age => 6,
                Source::Ext(n) => ext_value(n, r).chars().count(),
            })
            .max()
            .unwrap_or(0);
        c.width = c.width.max(w).min(80);
    }
    t.cols = cols;
    // The selection follows its row.
    let sel = t.sel_key.as_ref().and_then(|k| {
        let (ns, name) = k.split_once('/').unwrap_or(("", k));
        t.rows.iter().position(|r| r.ns == ns && r.name == name)
    });
    match sel {
        Some(i) => t.sel = i,
        None => {
            t.sel = t.sel.min(t.rows.len().saturating_sub(1));
            t.sel_key = t.rows.get(t.sel).map(|r| r.key());
        }
    }
    if !t.marks.is_empty() {
        let keys: BTreeSet<String> = t.rows.iter().map(|r| r.key()).collect();
        t.marks.retain(|m| keys.contains(m));
    }
}

/// The help text for the view on top.
fn help_lines(ui: &KubeUi) -> Vec<String> {
    let mut l: Vec<String> = Vec::new();
    let mut section = |title: &str, keys: &[(&str, &str)]| {
        l.push(String::new());
        l.push(format!("## {title}"));
        for (k, v) in keys {
            l.push(format!("  {k:<14} {v}"));
        }
    };
    if let Some(t) = ui.top_table() {
        let acts: Vec<(String, String)> = ui.actions().iter().map(|a| (a.key.to_string(), a.label.to_string())).collect();
        let acts: Vec<(&str, &str)> = acts.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        section(&format!("{} (m: these as a menu)", t.res.name()), &acts);
    }
    section(
        "Tables",
        &[
            (":", "command: a resource (pods, deploy, crd, any CRD), ctx, ns, xray, pf"),
            ("/", "filter (tab: substring, regex, fuzzy); !x inverts; -l app=web; -f k=v"),
            ("esc", "clear marks, then the filter, then back"),
            ("n", "pick a namespace;  0 or a: all namespaces"),
            ("space", "mark (actions apply to all marked);  ctrl-\\ clears"),
            ("< > O", "sort column left/right, reverse;  N A S: name, age, status"),
            ("ctrl-w", "wide columns;  ctrl-z: faults only;  ctrl-r: reload"),
            ("m", "actions of the selected row"),
            ("f", "port-forwards;  ! kubectl;  ? this help"),
        ],
    );
    section(
        "Commands (:)",
        &[
            (":pods -n ns", "a list in a namespace (:pods ns too); -A all namespaces"),
            (":deploy -l x", "with a label selector;  -f field selector (server side)"),
            (":ctx [name]", "switch context (no name: pick one)"),
            (":ns [name]", "switch namespace (no name: the namespaces list)"),
            (":xray deploy", "owner tree: deploy/sts/ds/job/cronjob/rs → pods → containers"),
            (":kubectl ...", "run kubectl on this context and namespace (also :! and !)"),
            (":aliases", "every resource type the cluster serves"),
            (":pf", "running port-forwards;  :q quit"),
            ("tab / →", "complete;  ↑↓ history"),
        ],
    );
    section("Pagers (describe, YAML, kubectl)", &[("/ n N", "search, next, previous"), ("w", "wrap;  ← → scroll sideways"), ("e", "edit;  y YAML;  d describe;  m managedFields"), ("c", "copy;  ctrl-s save to a file")]);
    section("Logs", &[("f", "follow / pause"), ("/", "show matching lines"), ("t w", "timestamps, wrap"), ("p", "previous containers' logs"), ("c x", "copy, clear;  ctrl-s save")]);
    section("Edit", &[("e", "opens $EDITOR; the server validates it (dry run)"), ("y", "on the diff: apply it;  e edit again;  esc drop it")]);
    l.push(String::new());
    l.push(format!("Hotkeys, aliases, plugins, skin: {}", super::uiconfig::path().display()));
    if let Some(e) = &ui.config_error {
        l.push(format!("  (it has an error: {e})"));
    }
    l.remove(0);
    l
}

#[cfg(test)]
pub(crate) fn test_snapshot(cols: &[(&str, i64)], rows: Vec<Row>) -> Arc<Snapshot> {
    Arc::new(Snapshot { columns: Arc::new(cols.iter().map(|(n, p)| Column::new(n, *p)).collect()), rows: rows.into_iter().map(Arc::new).collect(), synced: true, error: None })
}
