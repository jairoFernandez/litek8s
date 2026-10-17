//! One task per pod: brings the pod up (volumes, sandbox, init containers,
//! containers), restarts containers per `restartPolicy` with back-off, runs
//! probes, and reports status when it changes. It reacts to messages (pod
//! updates, container exits from the runtime's event stream, probe results)
//! and to its own back-off timers; it never polls the runtime.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::probe::{self, ProbeKind, ProbeResult, ProbeTarget};
use super::protocol::Message;
use super::runtime::{
    ContainerSpec, LABEL_ATTEMPT, LABEL_CONTAINER, LABEL_HASH, LABEL_POD_NAME, LABEL_POD_NAMESPACE, LABEL_POD_UID, LABEL_ROLE,
    MountSource, MountSpec, PodRuntime, SandboxInfo, SandboxSpec,
};
use super::volumes::{self, Prepared};
use super::{Agent, PodView};
use crate::kube::controllers::util::hash_of;
use crate::kube::quantity;

pub(crate) enum PodMsg {
    Update(Value),
    Delete,
    Died(String),
    Probe(ProbeResult),
    /// Something the worker waits for happened (an image pull finished).
    Wake,
    /// A ConfigMap or Secret changed; refresh volumes that project it.
    ObjectChanged { resource: String, namespace: String, name: String },
}

const MAX_BACKOFF: Duration = Duration::from_secs(300);
const SA_MOUNT: &str = crate::kube::defaults::SA_MOUNT_PATH;

#[derive(Debug, Clone)]
enum State {
    Waiting { reason: String, message: Option<String> },
    Running { started_at: String },
    Terminated { exit_code: i64, reason: String, message: Option<String>, started_at: String, finished_at: String, id: String },
}

struct Container {
    name: String,
    spec: Value,
    init: bool,
    /// An init container with `restartPolicy: Always` (a native sidecar).
    sidecar: bool,
    id: Option<String>,
    previous_id: Option<String>,
    attempts: u32,
    state: State,
    last_state: Option<State>,
    image_id: String,
    ready: bool,
    started: bool,
    started_tx: Option<watch::Sender<bool>>,
    probes: Vec<JoinHandle<()>>,
    counters: HashMap<ProbeKind, (u32, u32)>,
    backoff: Duration,
    next_start: Option<Instant>,
    running_since: Option<Instant>,
}

impl Container {
    fn new(spec: &Value, init: bool) -> Self {
        let sidecar = init && spec.get("restartPolicy").and_then(Value::as_str) == Some("Always");
        Self {
            name: spec.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
            spec: spec.clone(),
            init,
            sidecar,
            id: None,
            previous_id: None,
            attempts: 0,
            state: State::Waiting { reason: if init { "PodInitializing" } else { "ContainerCreating" }.into(), message: None },
            last_state: None,
            image_id: String::new(),
            ready: false,
            started: false,
            started_tx: None,
            probes: Vec::new(),
            counters: HashMap::new(),
            backoff: Duration::ZERO,
            next_start: None,
            running_since: None,
        }
    }

    fn image(&self) -> &str {
        self.spec.get("image").and_then(Value::as_str).unwrap_or_default()
    }

    fn running(&self) -> bool {
        matches!(self.state, State::Running { .. })
    }

    fn exit_code(&self) -> Option<i64> {
        match &self.state {
            State::Terminated { exit_code, .. } => Some(*exit_code),
            _ => None,
        }
    }

    fn stop_probes(&mut self) {
        for p in self.probes.drain(..) {
            p.abort();
        }
        self.started_tx = None;
        self.counters.clear();
    }

    fn due(&self) -> bool {
        self.next_start.is_none_or(|t| t <= Instant::now())
    }
}

struct Worker<R: PodRuntime> {
    agent: Arc<Agent<R>>,
    pod: Value,
    uid: String,
    ns: String,
    name: String,
    tx: mpsc::UnboundedSender<PodMsg>,
    dir: PathBuf,
    volumes: Option<HashMap<String, Prepared>>,
    sandbox: Option<SandboxInfo>,
    sandbox_stopped: bool,
    init: Vec<Container>,
    main: Vec<Container>,
    /// Set once every init container has completed.
    initialized: bool,
    failed: Option<(String, String)>,
    blocked: Option<(String, String)>,
    start_time: String,
    condition_times: HashMap<String, (String, String)>,
    last_sent: Option<Value>,
    wake_at: Option<Instant>,
    node: Value,
}

fn now_str() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn s<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or_default()
}

pub(crate) async fn run<R: PodRuntime>(
    agent: Arc<Agent<R>>,
    pod: Value,
    tx: mpsc::UnboundedSender<PodMsg>,
    mut rx: mpsc::UnboundedReceiver<PodMsg>,
) {
    let uid = s(&pod, "/metadata/uid").to_string();
    let containers = |field: &str, init: bool| -> Vec<Container> {
        pod.pointer(field)
            .and_then(Value::as_array)
            .map(|cs| cs.iter().map(|c| Container::new(c, init)).collect())
            .unwrap_or_default()
    };
    let mut w = Worker {
        dir: agent.config().root_dir.join("pods").join(&uid),
        node: agent.config().node.clone(),
        agent: agent.clone(),
        ns: s(&pod, "/metadata/namespace").to_string(),
        name: s(&pod, "/metadata/name").to_string(),
        init: containers("/spec/initContainers", true),
        main: containers("/spec/containers", false),
        initialized: false,
        uid,
        tx,
        volumes: None,
        sandbox: None,
        sandbox_stopped: false,
        failed: None,
        blocked: None,
        start_time: s(&pod, "/status/startTime").to_string(),
        condition_times: HashMap::new(),
        last_sent: None,
        wake_at: None,
        pod,
    };
    if w.start_time.is_empty() {
        w.start_time = now_str();
    }
    if w.init.is_empty() {
        w.initialized = true;
    }
    w.adopt().await;
    let mut link = agent.subscribe_link();
    loop {
        if w.pod.pointer("/metadata/deletionTimestamp").is_some_and(|v| !v.is_null()) {
            break;
        }
        w.step().await;
        w.publish_view();
        w.report().await;
        let wake = w.wake_at.take();
        tokio::select! {
            msg = rx.recv() => match msg {
                None | Some(PodMsg::Delete) => break,
                Some(PodMsg::Update(pod)) => w.update(pod).await,
                Some(PodMsg::Died(id)) => w.on_exit(&id).await,
                Some(PodMsg::Probe(result)) => w.on_probe(result).await,
                Some(PodMsg::Wake) => {}
                Some(PodMsg::ObjectChanged { resource, namespace, name }) => {
                    if namespace == w.ns && w.volumes.is_some() && volumes::uses_object(&w.pod, &resource, &name) {
                        if let Ok(v) = w.agent.prepare_volumes(&w.pod, &w.dir, &w.node).await {
                            w.volumes = Some(v);
                        }
                    }
                }
            },
            _ = sleep_until(wake) => {}
            changed = link.changed() => {
                if changed.is_err() {
                    break;
                }
                // Reconnected: send the full status again.
                w.last_sent = None;
            }
        }
    }
    w.teardown().await;
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

impl<R: PodRuntime> Worker<R> {
    fn wake_in(&mut self, delay: Duration) {
        let at = Instant::now() + delay;
        self.wake_at = Some(self.wake_at.map_or(at, |w| w.min(at)));
    }

    fn restart_policy(&self) -> &str {
        match s(&self.pod, "/spec/restartPolicy") {
            "" => "Always",
            p => p,
        }
    }

    fn host_network(&self) -> bool {
        self.pod.pointer("/spec/hostNetwork").and_then(Value::as_bool) == Some(true)
    }

    fn pod_ip(&self) -> String {
        if self.host_network() {
            return self.agent.config().node_ip.clone();
        }
        self.sandbox.as_ref().and_then(|s| s.ip.clone()).unwrap_or_default()
    }

    fn container_hash(spec: &Value) -> String {
        hash_of(spec)
    }

    fn container_name(&self, c: &Container) -> String {
        format!("k8s_{}_{}_{}_{}_{}", c.name, self.name, self.ns, &self.uid[..8.min(self.uid.len())], c.attempts)
    }

    fn labels(&self) -> HashMap<String, String> {
        HashMap::from([
            (LABEL_POD_UID.to_string(), self.uid.clone()),
            (LABEL_POD_NAME.to_string(), self.name.clone()),
            (LABEL_POD_NAMESPACE.to_string(), self.ns.clone()),
        ])
    }

    /// Pick up a sandbox and running containers left by a previous agent
    /// (the API server restarted with its pods intact).
    async fn adopt(&mut self) {
        let Ok(existing) = self.agent.runtime().list().await else { return };
        let mine: Vec<_> = existing.into_iter().filter(|c| c.labels.get(LABEL_POD_UID) == Some(&self.uid)).collect();
        let sandbox = mine.iter().find(|c| c.labels.get(LABEL_ROLE).map(String::as_str) == Some("sandbox") && c.running);
        let Some(sandbox) = sandbox else {
            for c in &mine {
                let _ = self.agent.runtime().remove(&c.id).await;
            }
            return;
        };
        let ip = self.agent.runtime().sandbox_ip(&sandbox.id).await.ok().flatten();
        self.sandbox = Some(SandboxInfo { id: sandbox.id.clone(), ip });
        let all_inits_done = !mine.is_empty();
        for c in &mine {
            if c.labels.get(LABEL_ROLE).map(String::as_str) == Some("sandbox") {
                continue;
            }
            let cname = c.labels.get(LABEL_CONTAINER).cloned().unwrap_or_default();
            let attempt: u32 = c.labels.get(LABEL_ATTEMPT).and_then(|a| a.parse().ok()).unwrap_or(0);
            let hash = c.labels.get(LABEL_HASH).cloned().unwrap_or_default();
            let slot = self.main.iter_mut().chain(self.init.iter_mut()).find(|x| x.name == cname);
            match slot {
                Some(slot) if c.running && Self::container_hash(&slot.spec) == hash && slot.id.is_none() => {
                    slot.id = Some(c.id.clone());
                    slot.attempts = attempt + 1;
                    slot.state = State::Running { started_at: now_str() };
                    slot.running_since = Some(Instant::now());
                    self.agent.track_container(&c.id, &self.uid);
                }
                _ => {
                    let _ = self.agent.runtime().remove(&c.id).await;
                }
            }
        }
        if all_inits_done && self.main.iter().any(|c| c.id.is_some()) {
            // The main containers ran, so init containers had completed.
            self.initialized = true;
            for c in self.init.iter_mut().filter(|c| !c.sidecar) {
                c.state = State::Terminated {
                    exit_code: 0,
                    reason: "Completed".into(),
                    message: None,
                    started_at: now_str(),
                    finished_at: now_str(),
                    id: String::new(),
                };
            }
        }
        let indices: Vec<(bool, usize)> = self
            .main
            .iter()
            .enumerate()
            .filter(|(_, c)| c.running())
            .map(|(i, _)| (false, i))
            .chain(self.init.iter().enumerate().filter(|(_, c)| c.running()).map(|(i, _)| (true, i)))
            .collect();
        for (init, i) in indices {
            self.start_probes(init, i);
        }
    }

    async fn update(&mut self, pod: Value) {
        // Image changes are the one mutable part of a pod's containers.
        let images = |p: &Value, field: &str| -> Vec<(String, String)> {
            p.pointer(field)
                .and_then(Value::as_array)
                .map(|cs| cs.iter().map(|c| (s(c, "/name").to_string(), s(c, "/image").to_string())).collect())
                .unwrap_or_default()
        };
        let new_images = images(&pod, "/spec/containers");
        let new_specs: Vec<Value> = pod.pointer("/spec/containers").and_then(Value::as_array).cloned().unwrap_or_default();
        for c in self.main.iter_mut() {
            if let Some((_, image)) = new_images.iter().find(|(n, _)| *n == c.name) {
                if image != c.image() {
                    if let Some(spec) = new_specs.iter().find(|sp| s(sp, "/name") == c.name) {
                        c.spec = spec.clone();
                    }
                    if let Some(id) = c.id.clone() {
                        let _ = self.agent.runtime().stop(&id, Duration::from_secs(10)).await;
                    }
                    c.next_start = None;
                    c.backoff = Duration::ZERO;
                }
            }
        }
        self.pod = pod;
    }

    async fn step(&mut self) {
        if self.failed.is_some() || self.succeeded() {
            // Finished: free the sandbox (and its address); exited
            // containers stay for `kubectl logs`.
            if let Some(sb) = self.sandbox.as_ref().filter(|_| !self.sandbox_stopped) {
                let id = sb.id.clone();
                self.sandbox_stopped = true;
                let rt = self.agent.runtime().clone();
                tokio::spawn(async move { rt.stop(&id, Duration::from_secs(1)).await });
            }
            return;
        }
        self.blocked = None;
        if self.volumes.is_none() {
            match self.agent.prepare_volumes(&self.pod, &self.dir, &self.node).await {
                Ok(v) => self.volumes = Some(v),
                Err(e) => {
                    self.blocked = Some(("ContainerCreating".into(), format!("MountVolume.SetUp failed: {e}")));
                    self.wake_in(Duration::from_secs(2));
                    return;
                }
            }
        }
        if self.sandbox.is_none() {
            match self.run_sandbox().await {
                Ok(sb) => self.sandbox = Some(sb),
                Err(e) => {
                    self.blocked = Some(("ContainerCreating".into(), format!("Failed to create pod sandbox: {e:#}")));
                    self.wake_in(Duration::from_secs(3));
                    return;
                }
            }
        }

        if !self.initialized {
            for i in 0..self.init.len() {
                let c = &self.init[i];
                if c.sidecar {
                    if !c.running() && c.due() {
                        if !self.start(true, i).await {
                            return;
                        }
                    } else if !c.running() {
                        self.wake_until(true, i);
                        return;
                    }
                    if !self.init[i].started {
                        return;
                    }
                    continue;
                }
                match c.exit_code() {
                    Some(0) => continue,
                    Some(_) if self.restart_policy() == "Never" => {
                        self.failed = Some(("Failed".into(), format!("init container {} failed", c.name)));
                        return;
                    }
                    _ if c.running() => return,
                    _ => {
                        if c.due() {
                            self.start(true, i).await;
                        } else {
                            self.wake_until(true, i);
                        }
                        return;
                    }
                }
            }
            self.initialized = true;
        }

        let policy = self.restart_policy().to_string();
        for i in 0..self.main.len() {
            let c = &self.main[i];
            let wants_start = match &c.state {
                State::Running { .. } => false,
                State::Waiting { .. } => true,
                State::Terminated { exit_code, .. } => policy == "Always" || (policy == "OnFailure" && *exit_code != 0),
            };
            if wants_start {
                if c.due() {
                    self.start(false, i).await;
                } else {
                    self.wake_until(false, i);
                }
            }
        }
        // Sidecars restart too, once the pod is up.
        for i in 0..self.init.len() {
            let c = &self.init[i];
            if c.sidecar && !c.running() {
                if c.due() {
                    self.start(true, i).await;
                } else {
                    self.wake_until(true, i);
                }
            }
        }
    }

    fn wake_until(&mut self, init: bool, i: usize) {
        let c = if init { &self.init[i] } else { &self.main[i] };
        if let Some(t) = c.next_start {
            let delay = t.saturating_duration_since(Instant::now());
            self.wake_in(delay);
        }
    }

    fn succeeded(&self) -> bool {
        self.phase() == "Succeeded"
    }

    async fn run_sandbox(&mut self) -> anyhow::Result<SandboxInfo> {
        let mut labels = self.labels();
        labels.insert(LABEL_ROLE.into(), "sandbox".into());
        let endpoint = self.agent.api_endpoint();
        let api_host = endpoint.as_ref().map(|e| e.host.clone()).unwrap_or_else(|| "host-gateway".into());
        let mut extra_hosts: Vec<String> = ["kubernetes.default.svc", "kubernetes.default", "kubernetes", "kubernetes.default.svc.cluster.local"]
            .iter()
            .map(|h| format!("{h}:{api_host}"))
            .collect();
        for alias in self.pod.pointer("/spec/hostAliases").and_then(Value::as_array).into_iter().flatten() {
            let ip = s(alias, "/ip");
            for host in alias.get("hostnames").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                extra_hosts.push(format!("{host}:{ip}"));
            }
        }
        let hostname = match s(&self.pod, "/spec/hostname") {
            "" => self.name.clone(),
            h => h.to_string(),
        };
        let hostname: String = hostname.chars().take(63).collect();
        let mut host_ports = Vec::new();
        for c in self.pod.pointer("/spec/containers").and_then(Value::as_array).into_iter().flatten() {
            for p in c.get("ports").and_then(Value::as_array).into_iter().flatten() {
                if let Some(hp) = p.get("hostPort").and_then(Value::as_u64).filter(|h| *h > 0) {
                    let cp = p.get("containerPort").and_then(Value::as_u64).unwrap_or(hp);
                    host_ports.push((cp as u16, hp as u16, p.get("protocol").and_then(Value::as_str).unwrap_or("TCP").to_string()));
                }
            }
        }
        let mut dns_servers = Vec::new();
        let mut dns_search = Vec::new();
        let mut dns_options = Vec::new();
        let policy = s(&self.pod, "/spec/dnsPolicy");
        let cluster_first = policy == "ClusterFirstWithHostNet" || (policy != "Default" && policy != "None" && !self.host_network());
        if let (true, Some(api)) = (cluster_first, endpoint.as_ref().filter(|e| e.cluster_dns)) {
            if let Some(dns) = self.agent.resolve_host(&api.host).await {
                dns_servers.push(dns.clone());
                let domain = crate::kube::dns::DOMAIN;
                dns_search.extend([format!("{}.svc.{domain}", self.ns), format!("svc.{domain}"), domain.to_string()]);
                dns_options.push("ndots:5".to_string());
                for name in ["kube-dns.kube-system.svc.cluster.local", "kube-dns.kube-system.svc", "kube-dns.kube-system"] {
                    extra_hosts.push(format!("{name}:{dns}"));
                }
            }
        }
        if policy == "None" {
            for ns in self.pod.pointer("/spec/dnsConfig/nameservers").and_then(Value::as_array).into_iter().flatten() {
                dns_servers.extend(ns.as_str().map(str::to_string));
            }
        }
        for sr in self.pod.pointer("/spec/dnsConfig/searches").and_then(Value::as_array).into_iter().flatten() {
            dns_search.extend(sr.as_str().map(str::to_string));
        }
        // The sandbox mounts the pod's runtime volumes to prepare them.
        let mut mounts = Vec::new();
        let mut fresh = Vec::new();
        for (name, prepared) in self.volumes.iter().flatten() {
            if let Prepared::Named { volume, fresh: is_fresh, empty_dir } = prepared {
                let target = format!("/.litek8s/volumes/{name}");
                mounts.push(MountSpec { source: MountSource::Volume { name: volume.clone(), subpath: None }, target: target.clone(), read_only: false });
                if *is_fresh || *empty_dir {
                    fresh.push(target);
                }
            }
        }
        let spec = SandboxSpec {
            name: format!("k8s_POD_{}_{}_{}", self.name, self.ns, &self.uid[..8.min(self.uid.len())]),
            hostname,
            labels,
            host_network: self.host_network(),
            extra_hosts,
            host_ports,
            dns_servers,
            dns_search,
            dns_options,
            mounts,
        };
        let sandbox = self.agent.runtime().run_sandbox(&spec).await?;
        self.agent.track_container(&sandbox.id, &self.uid);
        // emptyDir volumes are world-writable in Kubernetes, and fresh
        // persistent volumes start out that way too (like local-path).
        let mut script = String::new();
        for target in &fresh {
            script.push_str(&format!("chmod 0777 '{target}'; "));
        }
        for c in self.init.iter().chain(self.main.iter()) {
            for m in c.spec.get("volumeMounts").and_then(Value::as_array).into_iter().flatten() {
                let sub = m.get("subPath").and_then(Value::as_str).unwrap_or_default();
                if sub.is_empty() || sub.contains("..") {
                    continue;
                }
                if let Some(Prepared::Named { .. }) = self.volumes.as_ref().and_then(|v| v.get(s(m, "/name"))) {
                    script.push_str(&format!("mkdir -p -m 0777 '/.litek8s/volumes/{}/{}'; ", s(m, "/name"), sub.replace('\'', "")));
                }
            }
        }
        if !script.is_empty() {
            if let Ok(session) = self.agent.runtime().exec(&sandbox.id, vec!["sh".into(), "-c".into(), script], false, false).await {
                use futures_util::StreamExt;
                let mut out = session.output;
                while out.next().await.is_some() {}
            }
        }
        Ok(sandbox)
    }

    /// Start (or restart) a container. Returns whether it is running.
    async fn start(&mut self, init: bool, i: usize) -> bool {
        let tx = self.tx.clone();
        let (image, always) = {
            let c = if init { &self.init[i] } else { &self.main[i] };
            (c.image().to_string(), s(&c.spec, "/imagePullPolicy") == "Always")
        };
        match self.agent.image_ready(&image, always, &tx) {
            Ok(true) => {}
            Ok(false) => {
                let c = if init { &mut self.init[i] } else { &mut self.main[i] };
                if !matches!(c.state, State::Terminated { .. }) {
                    c.state = State::Waiting { reason: "ContainerCreating".into(), message: Some(format!("Pulling image \"{image}\"")) };
                }
                return false;
            }
            Err(error) => {
                let c = if init { &mut self.init[i] } else { &mut self.main[i] };
                let reason = if matches!(&c.state, State::Waiting { reason, .. } if reason == "ErrImagePull" || reason == "ImagePullBackOff") {
                    "ImagePullBackOff"
                } else {
                    "ErrImagePull"
                };
                c.state = State::Waiting { reason: reason.into(), message: Some(error) };
                self.wake_in(Duration::from_secs(10));
                return false;
            }
        }
        let spec = match self.container_spec(init, i).await {
            Ok(spec) => spec,
            Err(message) => {
                let c = if init { &mut self.init[i] } else { &mut self.main[i] };
                c.state = State::Waiting { reason: "CreateContainerConfigError".into(), message: Some(message) };
                self.wake_in(Duration::from_secs(3));
                return false;
            }
        };
        let sandbox = self.sandbox.as_ref().map(|s| s.id.clone()).unwrap_or_default();
        let result = self.agent.runtime().run_container(&sandbox, &spec).await;
        let uid = self.uid.clone();
        let c = if init { &mut self.init[i] } else { &mut self.main[i] };
        match result {
            Ok(id) => {
                if let Some(old) = c.previous_id.take() {
                    let rt = self.agent.runtime().clone();
                    self.agent.untrack_container(&old);
                    tokio::spawn(async move {
                        let _ = rt.remove(&old).await;
                    });
                }
                c.previous_id = c.id.take();
                c.attempts += 1;
                self.agent.track_container(&id, &uid);
                c.id = Some(id.clone());
                c.state = State::Running { started_at: now_str() };
                c.running_since = Some(Instant::now());
                c.next_start = None;
                c.ready = false;
                c.started = false;
                if let Ok(Some(info)) = self.agent.runtime().inspect(&id).await {
                    c.image_id = info.image_id;
                    if let Some(t) = info.started_at {
                        c.state = State::Running { started_at: normalize_time(&t) };
                    }
                    if !info.running {
                        // Exited before we looked: the exit event is on its way.
                    }
                }
                self.start_probes(init, i);
                true
            }
            Err(e) => {
                c.state = State::Waiting { reason: "CreateContainerError".into(), message: Some(format!("{e:#}")) };
                c.next_start = Some(Instant::now() + Duration::from_secs(5));
                self.wake_in(Duration::from_secs(5));
                false
            }
        }
    }

    fn start_probes(&mut self, init: bool, i: usize) {
        let host = if self.host_network() { "127.0.0.1".to_string() } else { self.pod_ip() };
        let rt = self.agent.runtime().clone();
        let tx = self.tx.clone();
        let sandbox_id = if self.host_network() { None } else { self.sandbox.as_ref().map(|s| s.id.clone()) };
        let c = if init { &mut self.init[i] } else { &mut self.main[i] };
        c.stop_probes();
        let Some(id) = c.id.clone() else { return };
        let (started_tx, started_rx) = watch::channel(false);
        let ports: Vec<(String, u16)> = c
            .spec
            .get("ports")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|p| Some((s(p, "/name").to_string(), p.get("containerPort")?.as_u64()? as u16)))
            .collect();
        let probes = [
            (ProbeKind::Startup, "startupProbe"),
            (ProbeKind::Readiness, "readinessProbe"),
            (ProbeKind::Liveness, "livenessProbe"),
        ];
        // Plain init containers are not probed.
        if !c.init || c.sidecar {
            for (kind, field) in probes {
                if let Some(probe) = c.spec.get(field).filter(|p| p.is_object()) {
                    let target = ProbeTarget {
                        container: c.name.clone(),
                        container_id: id.clone(),
                        sandbox_id: sandbox_id.clone(),
                        host: host.clone(),
                        ports: ports.clone(),
                    };
                    c.probes.push(tokio::spawn(probe::run(rt.clone(), kind, probe.clone(), target, started_rx.clone(), tx.clone())));
                }
            }
        }
        if c.spec.get("startupProbe").is_none_or(|p| !p.is_object()) {
            c.started = true;
            let _ = started_tx.send(true);
        }
        if c.spec.get("readinessProbe").is_none_or(|p| !p.is_object()) {
            c.ready = c.started;
        }
        c.started_tx = Some(started_tx);
    }

    async fn on_probe(&mut self, r: ProbeResult) {
        let Some(c) = self
            .main
            .iter_mut()
            .chain(self.init.iter_mut())
            .find(|c| c.name == r.container && c.id.as_deref() == Some(r.container_id.as_str()))
        else {
            return;
        };
        let probe_field = match r.kind {
            ProbeKind::Startup => "startupProbe",
            ProbeKind::Readiness => "readinessProbe",
            ProbeKind::Liveness => "livenessProbe",
        };
        let probe = c.spec.get(probe_field).cloned().unwrap_or(Value::Null);
        let success_threshold = probe.get("successThreshold").and_then(Value::as_u64).unwrap_or(1).max(1) as u32;
        let failure_threshold = probe.get("failureThreshold").and_then(Value::as_u64).unwrap_or(3).max(1) as u32;
        let counter = c.counters.entry(r.kind).or_insert((0, 0));
        if r.ok {
            *counter = (counter.0 + 1, 0);
        } else {
            *counter = (0, counter.1 + 1);
        }
        let (succ, fail) = *counter;
        match r.kind {
            ProbeKind::Readiness => {
                if succ >= success_threshold {
                    c.ready = true;
                } else if fail >= failure_threshold || (fail > 0 && !c.ready) {
                    c.ready = false;
                }
            }
            ProbeKind::Startup => {
                if r.ok && !c.started {
                    c.started = true;
                    if let Some(tx) = &c.started_tx {
                        let _ = tx.send(true);
                    }
                    if c.spec.get("readinessProbe").is_none_or(|p| !p.is_object()) {
                        c.ready = true;
                    }
                } else if !c.started && fail >= failure_threshold {
                    if let Some(id) = c.id.clone() {
                        c.stop_probes();
                        let grace = Duration::from_secs(probe.get("terminationGracePeriodSeconds").and_then(Value::as_u64).unwrap_or(2));
                        let rt = self.agent.runtime().clone();
                        tokio::spawn(async move { rt.stop(&id, grace).await });
                    }
                }
            }
            ProbeKind::Liveness => {
                if fail >= failure_threshold {
                    if let Some(id) = c.id.clone() {
                        c.stop_probes();
                        c.ready = false;
                        let grace = Duration::from_secs(
                            s(&self.pod, "/spec/terminationGracePeriodSeconds").parse().ok().unwrap_or(2u64).min(10),
                        );
                        let rt = self.agent.runtime().clone();
                        tokio::spawn(async move { rt.stop(&id, grace).await });
                    }
                }
            }
        }
    }

    async fn on_exit(&mut self, id: &str) {
        let info = self.agent.runtime().inspect(id).await.ok().flatten();
        if self.sandbox.as_ref().is_some_and(|s| s.id == id) {
            if self.sandbox_stopped {
                return;
            }
            // The sandbox died: recreate it and every container in it.
            self.agent.untrack_container(id);
            self.sandbox = None;
            for c in self.init.iter_mut().chain(self.main.iter_mut()) {
                if let Some(cid) = c.id.clone() {
                    if c.running() {
                        let _ = self.agent.runtime().stop(&cid, Duration::from_secs(1)).await;
                    }
                }
            }
            return;
        }
        let policy = self.restart_policy().to_string();
        let Some(c) = self.main.iter_mut().chain(self.init.iter_mut()).find(|c| c.id.as_deref() == Some(id)) else {
            return;
        };
        if !c.running() {
            return;
        }
        let info = info.unwrap_or_default();
        let exit_code = info.exit_code.unwrap_or(137);
        let reason = if info.oom_killed {
            "OOMKilled"
        } else if exit_code == 0 {
            "Completed"
        } else {
            "Error"
        };
        let started_at = match &c.state {
            State::Running { started_at } => started_at.clone(),
            _ => now_str(),
        };
        c.stop_probes();
        c.ready = false;
        c.started = false;
        let terminated = State::Terminated {
            exit_code,
            reason: reason.into(),
            message: info.error.clone(),
            started_at,
            finished_at: info.finished_at.as_deref().map(normalize_time).unwrap_or_else(now_str),
            id: id.to_string(),
        };
        c.last_state = Some(terminated.clone());
        c.state = terminated;
        let restarts = if c.init && !c.sidecar {
            exit_code != 0 && policy != "Never"
        } else if c.sidecar {
            true
        } else {
            policy == "Always" || (policy == "OnFailure" && exit_code != 0)
        };
        if restarts {
            // Back off 0s, 10s, 20s, ... up to 5 minutes; a container that ran
            // for 10 minutes starts over.
            if c.running_since.is_some_and(|t| t.elapsed() > Duration::from_secs(600)) {
                c.backoff = Duration::ZERO;
            }
            let delay = c.backoff;
            c.backoff = if c.backoff.is_zero() { Duration::from_secs(10) } else { (c.backoff * 2).min(MAX_BACKOFF) };
            c.next_start = Some(Instant::now() + delay);
        }
        c.running_since = None;
    }

    /// Environment for a container: Kubernetes service variables, then
    /// `envFrom`, then `env` (with `$(VAR)` expansion).
    async fn container_env(&self, spec: &Value) -> Result<Vec<String>, String> {
        let link = self.agent.link().ok_or("not connected to the API server")?;
        let mut env: Vec<(String, String)> = Vec::new();
        let set = |env: &mut Vec<(String, String)>, k: String, v: String| {
            if let Some(slot) = env.iter_mut().find(|(n, _)| *n == k) {
                slot.1 = v;
            } else {
                env.push((k, v));
            }
        };
        if let Some(api) = self.agent.api_endpoint() {
            let host = "kubernetes.default.svc";
            let port = api.port.to_string();
            for (k, v) in [
                ("KUBERNETES_SERVICE_HOST", host.to_string()),
                ("KUBERNETES_SERVICE_PORT", port.clone()),
                ("KUBERNETES_SERVICE_PORT_HTTPS", port.clone()),
                ("KUBERNETES_PORT", format!("tcp://{host}:{port}")),
                (&format!("KUBERNETES_PORT_{port}_TCP"), format!("tcp://{host}:{port}")),
                (&format!("KUBERNETES_PORT_{port}_TCP_PROTO"), "tcp".into()),
                (&format!("KUBERNETES_PORT_{port}_TCP_PORT"), port.clone()),
                (&format!("KUBERNETES_PORT_{port}_TCP_ADDR"), host.to_string()),
            ] {
                set(&mut env, k.to_string(), v);
            }
        }
        let b64 = base64::engine::general_purpose::STANDARD;
        let decode = |v: &Value| -> String {
            let raw = v.as_str().unwrap_or_default();
            b64.decode(raw).ok().and_then(|b| String::from_utf8(b).ok()).unwrap_or_default()
        };
        for from in spec.get("envFrom").and_then(Value::as_array).into_iter().flatten() {
            let prefix = s(from, "/prefix");
            let (resource, name, optional) = if let Some(r) = from.get("configMapRef") {
                ("configmaps", s(r, "/name"), r.get("optional").and_then(Value::as_bool) == Some(true))
            } else if let Some(r) = from.get("secretRef") {
                ("secrets", s(r, "/name"), r.get("optional").and_then(Value::as_bool) == Some(true))
            } else {
                continue;
            };
            match link.get(resource, &self.ns, name).await.map_err(|e| e.to_string())? {
                Some(obj) => {
                    for (k, v) in obj.get("data").and_then(Value::as_object).into_iter().flatten() {
                        let value = if resource == "secrets" { decode(v) } else { v.as_str().unwrap_or_default().to_string() };
                        set(&mut env, format!("{prefix}{k}"), value);
                    }
                }
                None if optional => {}
                None => return Err(format!("{} \"{name}\" not found", &resource[..resource.len() - 1])),
            }
        }
        let pod_ip = self.pod_ip();
        let host_ip = self.agent.config().node_ip.clone();
        for e in spec.get("env").and_then(Value::as_array).into_iter().flatten() {
            let name = s(e, "/name").to_string();
            let value = if let Some(v) = e.get("value").and_then(Value::as_str) {
                expand(v, &env)
            } else if let Some(from) = e.get("valueFrom") {
                if let Some(f) = from.pointer("/fieldRef/fieldPath").and_then(Value::as_str) {
                    volumes::field_ref(&self.pod, f, &pod_ip, &host_ip).unwrap_or_default()
                } else if let Some(r) = from.get("resourceFieldRef") {
                    let cname = match s(r, "/containerName") {
                        "" => s(spec, "/name"),
                        n => n,
                    };
                    volumes::resource_ref(&self.pod, cname, s(r, "/resource"), r.get("divisor"), &self.node)
                } else {
                    let (resource, r) = match (from.get("configMapKeyRef"), from.get("secretKeyRef")) {
                        (Some(r), _) => ("configmaps", r),
                        (_, Some(r)) => ("secrets", r),
                        _ => continue,
                    };
                    let optional = r.get("optional").and_then(Value::as_bool) == Some(true);
                    let (obj_name, key) = (s(r, "/name"), s(r, "/key"));
                    let obj = link.get(resource, &self.ns, obj_name).await.map_err(|e| e.to_string())?;
                    let value = obj.as_ref().and_then(|o| o.pointer(&format!("/data/{}", key.replace('~', "~0").replace('/', "~1"))));
                    match value {
                        Some(v) if resource == "secrets" => decode(v),
                        Some(v) => v.as_str().unwrap_or_default().to_string(),
                        None if optional => continue,
                        None if obj.is_none() => return Err(format!("{} \"{obj_name}\" not found", &resource[..resource.len() - 1])),
                        None => return Err(format!("couldn't find key {key} in {} {}/{obj_name}", &resource[..resource.len() - 1], self.ns)),
                    }
                }
            } else {
                String::new()
            };
            set(&mut env, name, value);
        }
        Ok(env.into_iter().map(|(k, v)| format!("{k}={v}")).collect())
    }

    async fn container_spec(&self, init: bool, i: usize) -> Result<ContainerSpec, String> {
        let c = if init { &self.init[i] } else { &self.main[i] };
        let spec = &c.spec;
        let env = self.container_env(spec).await?;
        let env_pairs: Vec<(String, String)> = env
            .iter()
            .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
            .collect();
        let strings = |field: &str| -> Option<Vec<String>> {
            spec.get(field)
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(|x| expand(x, &env_pairs)).collect())
        };
        let mut mounts = Vec::new();
        let volumes = self.volumes.as_ref();
        for m in spec.get("volumeMounts").and_then(Value::as_array).into_iter().flatten() {
            let vname = s(m, "/name");
            let target = s(m, "/mountPath").to_string();
            let read_only = m.get("readOnly").and_then(Value::as_bool) == Some(true);
            let sub = match m.get("subPathExpr").and_then(Value::as_str) {
                Some(expr) => expand(expr, &env_pairs),
                None => s(m, "/subPath").to_string(),
            };
            let sub = sub.trim_matches('/').to_string();
            let Some(prepared) = volumes.and_then(|v| v.get(vname)) else {
                return Err(format!("volume \"{vname}\" not found"));
            };
            let source = match prepared {
                Prepared::Dir(dir) => {
                    let path = if sub.is_empty() { dir.clone() } else { dir.join(&sub) };
                    MountSource::Bind(path.to_string_lossy().to_string())
                }
                Prepared::Named { volume, .. } => {
                    MountSource::Volume { name: volume.clone(), subpath: (!sub.is_empty()).then_some(sub.clone()) }
                }
                Prepared::Tmpfs => MountSource::Tmpfs,
                Prepared::HostPath(path) => {
                    let p = if sub.is_empty() { path.clone() } else { format!("{}/{sub}", path.trim_end_matches('/')) };
                    MountSource::Bind(p)
                }
            };
            let read_only = read_only || (matches!(prepared, Prepared::Dir(_)) && target == SA_MOUNT);
            mounts.push(MountSpec { source, target, read_only });
        }
        let security = spec.get("securityContext").cloned().unwrap_or(Value::Null);
        let pod_security = self.pod.pointer("/spec/securityContext").cloned().unwrap_or(Value::Null);
        let user = security
            .get("runAsUser")
            .or_else(|| pod_security.get("runAsUser"))
            .and_then(Value::as_i64)
            .map(|u| {
                match security.get("runAsGroup").or_else(|| pod_security.get("runAsGroup")).and_then(Value::as_i64) {
                    Some(g) => format!("{u}:{g}"),
                    None => u.to_string(),
                }
            });
        let caps = |field: &str| -> Vec<String> {
            security
                .pointer(&format!("/capabilities/{field}"))
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default()
        };
        let limit = |res: &str| spec.pointer(&format!("/resources/limits/{res}")).and_then(quantity::milli_of);
        let mut labels = self.labels();
        labels.insert(LABEL_CONTAINER.into(), c.name.clone());
        labels.insert(LABEL_ATTEMPT.into(), c.attempts.to_string());
        labels.insert(LABEL_HASH.into(), Self::container_hash(spec));
        labels.insert(LABEL_ROLE.into(), if init { "init" } else { "container" }.into());
        Ok(ContainerSpec {
            name: self.container_name(c),
            image: c.image().to_string(),
            entrypoint: strings("command"),
            cmd: strings("args"),
            env,
            working_dir: spec.get("workingDir").and_then(Value::as_str).filter(|w| !w.is_empty()).map(str::to_string),
            user,
            labels,
            mounts,
            tty: spec.get("tty").and_then(Value::as_bool) == Some(true),
            stdin: spec.get("stdin").and_then(Value::as_bool) == Some(true),
            privileged: security.get("privileged").and_then(Value::as_bool) == Some(true),
            cap_add: caps("add"),
            cap_drop: caps("drop"),
            read_only_rootfs: security.get("readOnlyRootFilesystem").and_then(Value::as_bool) == Some(true),
            memory_limit: limit("memory").map(|m| (m / 1000) as i64).filter(|m| *m >= 6 * 1024 * 1024),
            nano_cpus: limit("cpu").map(|m| (m * 1_000_000) as i64),
            // Namespaced sysctls belong to the sandbox's network namespace,
            // which joined containers cannot set; they are skipped.
            sysctls: HashMap::new(),
        })
    }

    fn phase(&self) -> &'static str {
        if self.failed.is_some() {
            return "Failed";
        }
        if !self.initialized {
            return "Pending";
        }
        let policy = self.restart_policy();
        let all_done = !self.main.is_empty()
            && self.main.iter().all(|c| match c.exit_code() {
                Some(code) => !(policy == "Always" || (policy == "OnFailure" && code != 0)),
                None => false,
            });
        if all_done {
            return if self.main.iter().all(|c| c.exit_code() == Some(0)) { "Succeeded" } else { "Failed" };
        }
        if self.main.iter().any(|c| c.running() || c.exit_code().is_some()) {
            "Running"
        } else {
            "Pending"
        }
    }

    fn container_status(&self, c: &Container) -> Value {
        let render = |state: &State| -> Value {
            match state {
                State::Waiting { reason, message } => {
                    let mut w = json!({"reason": reason});
                    if let Some(m) = message {
                        w["message"] = json!(m);
                    }
                    json!({"waiting": w})
                }
                State::Running { started_at } => json!({"running": {"startedAt": started_at}}),
                State::Terminated { exit_code, reason, message, started_at, finished_at, id } => {
                    let mut t = json!({"exitCode": exit_code, "reason": reason, "startedAt": started_at, "finishedAt": finished_at});
                    if !id.is_empty() {
                        t["containerID"] = json!(format!("docker://{id}"));
                    }
                    if let Some(m) = message {
                        t["message"] = json!(m);
                    }
                    json!({"terminated": t})
                }
            }
        };
        // A container waiting to be restarted shows CrashLoopBackOff, with
        // the exit as its last state.
        let state = match (&c.state, c.next_start) {
            (State::Terminated { .. }, Some(t)) if t > Instant::now() => State::Waiting {
                reason: "CrashLoopBackOff".into(),
                message: Some(format!(
                    "back-off {}s restarting failed container={} pod={}_{}({})",
                    (c.backoff.as_secs() / 2).max(10),
                    c.name,
                    self.name,
                    self.ns,
                    self.uid
                )),
            },
            (s, _) => s.clone(),
        };
        let state = match (&state, &self.blocked) {
            (State::Waiting { .. }, Some((reason, message))) if c.id.is_none() => {
                State::Waiting { reason: reason.clone(), message: Some(message.clone()) }
            }
            (State::Waiting { .. }, None) if !c.init && !self.initialized => {
                State::Waiting { reason: "PodInitializing".into(), message: None }
            }
            _ => state,
        };
        let last_state = match (&c.last_state, &state) {
            (Some(last), State::Waiting { .. } | State::Running { .. }) => render(last),
            _ => json!({}),
        };
        let mut status = json!({
            "name": c.name,
            "image": c.image(),
            "imageID": c.image_id,
            "ready": c.ready && c.running(),
            "started": c.running() && c.started,
            "restartCount": c.attempts.saturating_sub(1),
            "state": render(&state),
            "lastState": last_state,
        });
        if let Some(id) = &c.id {
            status["containerID"] = json!(format!("docker://{id}"));
        }
        status
    }

    fn condition(&mut self, type_: &str, ok: bool, reason: Option<&str>) -> Value {
        let status = if ok { "True" } else { "False" };
        let entry = self
            .condition_times
            .entry(type_.to_string())
            .or_insert_with(|| (status.to_string(), now_str()));
        if entry.0 != status {
            *entry = (status.to_string(), now_str());
        }
        let mut c = json!({"type": type_, "status": status, "lastProbeTime": null, "lastTransitionTime": entry.1});
        if let (Some(r), false) = (reason, ok) {
            c["reason"] = json!(r);
        }
        c
    }

    fn status(&mut self) -> Value {
        let phase = self.phase();
        let terminal = matches!(phase, "Succeeded" | "Failed");
        let main_ready = !self.main.is_empty()
            && self.main.iter().all(|c| c.ready && c.running())
            && self.init.iter().filter(|c| c.sidecar).all(|c| c.ready && c.running());
        let containers_ready = main_ready && !terminal;
        let sandbox_ok = self.sandbox.is_some() && !terminal;
        let initialized = self.initialized;
        let conditions = vec![
            self.condition("PodReadyToStartContainers", sandbox_ok, None),
            self.condition("Initialized", initialized, Some("ContainersNotInitialized")),
            self.condition("Ready", containers_ready, Some(if terminal { "PodCompleted" } else { "ContainersNotReady" })),
            self.condition("ContainersReady", containers_ready, Some(if terminal { "PodCompleted" } else { "ContainersNotReady" })),
        ];
        let pod_ip = self.pod_ip();
        let host_ip = self.agent.config().node_ip.clone();
        let mut status = json!({
            "phase": phase,
            "conditions": conditions,
            "hostIP": host_ip,
            "hostIPs": [{"ip": host_ip}],
            "startTime": self.start_time,
            "containerStatuses": self.main.iter().map(|c| self.container_status(c)).collect::<Vec<_>>(),
        });
        if !self.init.is_empty() {
            status["initContainerStatuses"] = json!(self.init.iter().map(|c| self.container_status(c)).collect::<Vec<_>>());
        }
        if !pod_ip.is_empty() {
            status["podIP"] = json!(pod_ip);
            status["podIPs"] = json!([{"ip": pod_ip}]);
        }
        if let Some((_, message)) = &self.failed {
            status["message"] = json!(message);
        }
        status
    }

    async fn report(&mut self) {
        let status = self.status();
        if self.last_sent.as_ref() == Some(&status) {
            return;
        }
        let Some(link) = self.agent.link() else { return };
        let msg = Message::PodStatus { namespace: self.ns.clone(), name: self.name.clone(), uid: self.uid.clone(), status: status.clone() };
        if link.send(msg).await {
            self.last_sent = Some(status);
        }
    }

    fn publish_view(&self) {
        let mut view = PodView {
            namespace: self.ns.clone(),
            name: self.name.clone(),
            ip: Some(self.pod_ip()).filter(|ip| !ip.is_empty()),
            sandbox: self.sandbox.as_ref().map(|s| s.id.clone()),
            containers: HashMap::new(),
        };
        for c in self.main.iter().chain(self.init.iter()) {
            view.containers.insert(c.name.clone(), (c.id.clone(), c.previous_id.clone()));
        }
        self.agent.set_view(&self.uid, view);
    }

    /// Stop and remove everything the pod had on this node.
    async fn teardown(&mut self) {
        let grace = self
            .pod
            .pointer("/spec/terminationGracePeriodSeconds")
            .and_then(Value::as_u64)
            .unwrap_or(30)
            .min(30);
        let rt = self.agent.runtime().clone();
        let mut ids = Vec::new();
        for c in self.main.iter_mut().chain(self.init.iter_mut()) {
            c.stop_probes();
            ids.extend(c.id.take());
            ids.extend(c.previous_id.take());
        }
        let stops: Vec<_> = ids
            .iter()
            .map(|id| {
                let rt = rt.clone();
                let id = id.clone();
                tokio::spawn(async move {
                    let _ = rt.stop(&id, Duration::from_secs(grace)).await;
                    let _ = rt.remove(&id).await;
                })
            })
            .collect();
        for s in stops {
            let _ = s.await;
        }
        for id in &ids {
            self.agent.untrack_container(id);
        }
        if let Some(sb) = self.sandbox.take() {
            let _ = rt.remove(&sb.id).await;
            self.agent.untrack_container(&sb.id);
        }
        for (_, prepared) in self.volumes.take().into_iter().flatten() {
            if let Prepared::Named { volume, empty_dir: true, .. } = prepared {
                let _ = rt.remove_volume(&volume).await;
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Docker reports nanosecond RFC 3339 times; Kubernetes uses seconds.
fn normalize_time(t: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(t)
        .map(|d| d.with_timezone(&chrono::Utc).format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|_| now_str())
}

/// Expand `$(VAR)` references; `$$` escapes a dollar sign.
fn expand(input: &str, env: &[(String, String)]) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find('$') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        if let Some(stripped) = after.strip_prefix('$') {
            out.push('$');
            rest = stripped;
        } else if let Some(inner) = after.strip_prefix('(') {
            match inner.find(')') {
                Some(end) => {
                    let name = &inner[..end];
                    match env.iter().rev().find(|(k, _)| k == name) {
                        Some((_, v)) => out.push_str(v),
                        None => {
                            out.push_str("$(");
                            out.push_str(name);
                            out.push(')');
                        }
                    }
                    rest = &inner[end + 1..];
                }
                None => {
                    out.push('$');
                    rest = after;
                }
            }
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_variables() {
        let env = vec![("A".to_string(), "1".to_string()), ("B".to_string(), "x".to_string())];
        assert_eq!(expand("$(A)-$(B)", &env), "1-x");
        assert_eq!(expand("$(MISSING)", &env), "$(MISSING)");
        assert_eq!(expand("$$(A)", &env), "$(A)");
        assert_eq!(expand("cost $5", &env), "cost $5");
    }
}
