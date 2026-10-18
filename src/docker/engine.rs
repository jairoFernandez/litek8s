//! The engine behind the Docker API: containers, networks and volumes on
//! the crun node runtime's pieces (image store, OCI bundle, shim, overlay
//! root filesystems, network namespaces).
//!
//! A Docker container is not a Pod: it can be created without starting,
//! stopped and started again with its files, attached to, copied into
//! before it runs, and it lives on networks of its own. So the engine
//! keeps its own records (`<root>/containers/<id>/container.json`) and
//! drives crun through the same shim as pods, with the writable layer kept
//! between runs.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};

use super::error::{ApiError, ApiResult};
use super::images;
use super::model::{Endpoint, Health, HealthLog, MountPoint, Network, Record, State, Volume, ZERO_TIME, now_rfc3339, random_name, valid_name};
use super::network::{self as dnet, DnsTable, LinkPlan, Publisher, RuleNet};
use crate::kube::agent::crun::image::{self as cimage, ImageStore, random_hex};
use crate::kube::agent::crun::{logs as clogs, spec as cspec, sys};
use crate::kube::agent::runtime::ContainerSpec;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
pub const RUN_DIR: &str = "/run/litek8s/docker";

pub struct EngineOptions {
    /// Where containers, networks and volumes are kept.
    pub root: PathBuf,
    pub images: Arc<ImageStore>,
    pub publisher: Publisher,
    /// Paths a client may bind-mount to mean "the Docker socket" (the
    /// host-side socket path, /var/run/docker.sock).
    pub socket_aliases: Vec<String>,
    /// The engine's socket as containers see it.
    pub internal_socket: PathBuf,
}

/// A container in memory.
pub struct Ctr {
    pub id: String,
    pub rec: Mutex<Record>,
    /// Serializes start/stop/remove.
    pub op: tokio::sync::Mutex<()>,
    /// Bumped on every state change (waiters, attachers).
    pub changed: watch::Sender<u64>,
    pub rt: Mutex<Runtime>,
}

#[derive(Default)]
pub struct Runtime {
    pub port_tasks: Vec<tokio::task::JoinHandle<()>>,
    pub health_task: Option<tokio::task::JoinHandle<()>>,
    /// Attach clients waiting for the next start.
    pub pending_attach: usize,
    /// Bumped when the shim for a start is up (attach socket ready).
    pub attach_gen: u64,
    pub shim_pid: Option<i32>,
    /// Terminal size asked for before the container ran.
    pub console_size: Option<(u16, u16)>,
    /// When it last started (for restart backoff).
    pub started: Option<std::time::Instant>,
    pub restart_delay: Duration,
    /// How many times the container's process ended (for `wait next-exit`).
    pub exits: u64,
}

impl Ctr {
    pub fn record(&self) -> Record {
        self.rec.lock().unwrap().clone()
    }

    fn bump(&self) {
        self.changed.send_modify(|v| *v += 1);
    }
}

pub struct Engine {
    pub root: PathBuf,
    pub crun: PathBuf,
    pub crun_root: PathBuf,
    pub images: Arc<ImageStore>,
    containers: RwLock<HashMap<String, Arc<Ctr>>>,
    networks: RwLock<BTreeMap<String, Network>>,
    net_lock: tokio::sync::Mutex<()>,
    vol_lock: Mutex<()>,
    pub events: broadcast::Sender<Value>,
    recent_events: Mutex<VecDeque<Value>>,
    pub publisher: Publisher,
    pub dns: Arc<RwLock<DnsTable>>,
    /// Host ports currently published (the Mac forwards these).
    pub published: watch::Sender<Vec<u16>>,
    pub socket_aliases: Vec<String>,
    pub internal_socket: PathBuf,
    pub execs: Mutex<HashMap<String, Arc<super::exec::ExecInst>>>,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

async fn run_quiet(program: &str, args: &[&str], stdin: Option<&str>) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(if stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run {program}"))?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!("{program} {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn pid_alive(pid: i32, marker: &str) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).contains(marker))
}

/// Resolve `path` inside `root` the way the container would see it:
/// symlinks (absolute ones too) stay within `root`.
pub fn secure_join(root: &Path, path: &str) -> PathBuf {
    let mut parts: Vec<String> = Vec::new();
    let mut todo: VecDeque<String> = path.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect();
    let mut hops = 0;
    while let Some(part) = todo.pop_front() {
        match part.as_str() {
            "." => continue,
            ".." => {
                parts.pop();
                continue;
            }
            _ => {}
        }
        let candidate = root.join(parts.iter().chain(std::iter::once(&part)).collect::<PathBuf>());
        match std::fs::read_link(&candidate) {
            Ok(target) if hops < 40 => {
                hops += 1;
                let t = target.to_string_lossy().to_string();
                if t.starts_with('/') {
                    parts.clear();
                }
                let mut new: VecDeque<String> = t.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect();
                new.extend(todo.drain(..));
                todo = new;
            }
            _ => parts.push(part),
        }
    }
    root.join(parts.into_iter().collect::<PathBuf>())
}

/// Copy a directory tree keeping owners, modes, times and links (an empty
/// volume gets the image's files, like Docker does).
pub fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = std::fs::symlink_metadata(src)?;
    let chown = |p: &Path, m: &std::fs::Metadata| {
        if let Ok(c) = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) {
            // SAFETY: valid string.
            unsafe { libc::lchown(c.as_ptr(), m.uid(), m.gid()) };
        }
    };
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(src)?;
        let _ = std::fs::remove_file(dst);
        std::os::unix::fs::symlink(target, dst)?;
        chown(dst, &meta);
    } else if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
        chown(dst, &meta);
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    } else if meta.is_file() {
        std::fs::copy(src, dst)?;
        chown(dst, &meta);
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    }
    Ok(())
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}

/// Docker's merge of a container's config with its image's
/// (moby daemon/commit.go `merge`).
pub fn merge_config(user: &Value, image: &Value) -> Value {
    let mut c = user.clone();
    if !c.is_object() {
        c = json!({});
    }
    let set = |c: &mut Value, k: &str, v: Value| {
        c[k] = v;
    };
    if is_empty(&c["User"]) {
        set(&mut c, "User", image["User"].clone());
    }
    let mut exposed = image["ExposedPorts"].as_object().cloned().unwrap_or_default();
    if let Some(u) = c["ExposedPorts"].as_object() {
        exposed.extend(u.clone());
    }
    set(&mut c, "ExposedPorts", if exposed.is_empty() { Value::Null } else { Value::Object(exposed) });
    let mut env: Vec<String> = strings(&c["Env"]);
    for kv in strings(&image["Env"]) {
        let key = kv.split('=').next().unwrap_or_default();
        if !env.iter().any(|e| e.split('=').next() == Some(key)) {
            env.insert(0, kv);
        }
    }
    // Image variables first, as Docker shows them.
    let (img, own): (Vec<String>, Vec<String>) = env.into_iter().partition(|e| strings(&image["Env"]).contains(e));
    set(&mut c, "Env", json!(img.into_iter().chain(own).collect::<Vec<_>>()));
    let mut labels = image["Labels"].as_object().cloned().unwrap_or_default();
    if let Some(u) = c["Labels"].as_object() {
        labels.extend(u.clone());
    }
    set(&mut c, "Labels", Value::Object(labels));
    if c["Entrypoint"].as_array().is_none_or(|a| a.is_empty()) {
        if is_empty(&c["Cmd"]) {
            set(&mut c, "Cmd", image["Cmd"].clone());
        }
        if c["Entrypoint"].is_null() {
            set(&mut c, "Entrypoint", image["Entrypoint"].clone());
        }
    }
    // `--entrypoint ""` resets the image's entrypoint.
    if c["Entrypoint"].as_array().is_some_and(|a| a.len() == 1 && a[0] == "") {
        set(&mut c, "Entrypoint", Value::Null);
    }
    if !image["Healthcheck"].is_null() {
        if c["Healthcheck"].is_null() {
            set(&mut c, "Healthcheck", image["Healthcheck"].clone());
        } else {
            for k in ["Test", "Interval", "Timeout", "StartPeriod", "StartInterval", "Retries"] {
                if is_empty(&c["Healthcheck"][k]) || c["Healthcheck"][k] == json!(0) {
                    c["Healthcheck"][k] = image["Healthcheck"][k].clone();
                }
            }
        }
    }
    if is_empty(&c["WorkingDir"]) {
        set(&mut c, "WorkingDir", image["WorkingDir"].clone());
    }
    let mut vols = image["Volumes"].as_object().cloned().unwrap_or_default();
    if let Some(u) = c["Volumes"].as_object() {
        vols.extend(u.clone());
    }
    set(&mut c, "Volumes", if vols.is_empty() { Value::Null } else { Value::Object(vols) });
    if is_empty(&c["StopSignal"]) && !image["StopSignal"].is_null() {
        set(&mut c, "StopSignal", image["StopSignal"].clone());
    }
    if c["OnBuild"].is_null() {
        set(&mut c, "OnBuild", image["OnBuild"].clone());
    }
    if c["Shell"].is_null() && !image["Shell"].is_null() {
        set(&mut c, "Shell", image["Shell"].clone());
    }
    for k in ["AttachStdin", "AttachStdout", "AttachStderr", "Tty", "OpenStdin", "StdinOnce"] {
        if c[k].is_null() {
            c[k] = json!(false);
        }
    }
    c
}

/// Nanoseconds (Docker's durations) to a Duration, with a default.
fn nanos(v: &Value, default: Duration) -> Duration {
    match v.as_i64() {
        Some(n) if n > 0 => Duration::from_nanos(n as u64),
        _ => default,
    }
}

/// "1g", "512m", "64k", "1024" -> bytes.
pub fn parse_size(s: &str) -> Option<i64> {
    let s = s.trim().to_lowercase();
    let (num, mult) = match s.chars().last()? {
        'k' => (&s[..s.len() - 1], 1 << 10),
        'm' => (&s[..s.len() - 1], 1 << 20),
        'g' => (&s[..s.len() - 1], 1 << 30),
        'b' => (&s[..s.len() - 1], 1),
        _ => (s.as_str(), 1),
    };
    num.trim_end_matches(['i', 'b']).parse::<f64>().ok().map(|n| (n * mult as f64) as i64)
}

impl Engine {
    pub async fn start(opts: EngineOptions) -> Result<Arc<Self>> {
        let root = opts.root;
        for d in ["containers", "networks", "volumes", "ipam"] {
            std::fs::create_dir_all(root.join(d))?;
        }
        std::fs::create_dir_all(RUN_DIR)?;
        let crun = crate::kube::agent::crun::find_crun().context("crun not found")?;
        let (events, _) = broadcast::channel(1024);
        let host_gateway = std::env::var("LITEK8S_HOST_LOOPBACK")
            .ok()
            .map(|v| if v.starts_with('/') { std::fs::read_to_string(&v).unwrap_or_default() } else { v })
            .and_then(|v| v.trim().parse::<Ipv4Addr>().ok());
        let mut table = DnsTable::default();
        table.host_gateway = host_gateway;
        let dns = Arc::new(RwLock::new(table));
        let engine = Arc::new(Self {
            crun,
            crun_root: PathBuf::from(RUN_DIR).join("crun"),
            images: opts.images,
            containers: RwLock::new(HashMap::new()),
            networks: RwLock::new(BTreeMap::new()),
            net_lock: tokio::sync::Mutex::new(()),
            vol_lock: Mutex::new(()),
            events,
            recent_events: Mutex::new(VecDeque::new()),
            publisher: opts.publisher,
            dns,
            published: watch::channel(Vec::new()).0,
            socket_aliases: opts.socket_aliases,
            internal_socket: opts.internal_socket,
            execs: Mutex::new(HashMap::new()),
            root,
        });
        std::fs::create_dir_all(&engine.crun_root)?;
        engine.load_networks().await?;
        let dns = engine.dns.clone();
        crate::kube::dns::set_extra_resolver(Box::new(move |client, name| dns.read().unwrap().resolve(client, name)));
        engine.load_containers().await;
        // The host gateway can come later (the VM learns it by DHCP).
        if engine.dns.read().unwrap().host_gateway.is_none() {
            let dns = engine.dns.clone();
            tokio::spawn(async move {
                for _ in 0..120 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    if let Some(gw) = std::env::var("LITEK8S_HOST_LOOPBACK")
                        .ok()
                        .map(|v| if v.starts_with('/') { std::fs::read_to_string(&v).unwrap_or_default() } else { v })
                        .and_then(|v| v.trim().parse::<Ipv4Addr>().ok())
                    {
                        dns.write().unwrap().host_gateway = Some(gw);
                        return;
                    }
                }
            });
        }
        Ok(engine)
    }

    pub fn container_dir(&self, id: &str) -> PathBuf {
        self.root.join("containers").join(id)
    }

    pub fn attach_socket(&self, id: &str) -> PathBuf {
        Path::new(RUN_DIR).join(format!("{}.attach", &id[..16]))
    }

    fn cgroup(&self, id: &str) -> String {
        format!("/litek8s/docker/{id}")
    }

    // -----------------------------------------------------------------
    // Events

    pub fn emit(&self, kind: &str, action: &str, id: &str, attrs: BTreeMap<String, String>) {
        let now = chrono::Utc::now();
        let mut ev = json!({
            "Type": kind,
            "Action": action,
            "Actor": {"ID": id, "Attributes": attrs},
            "scope": "local",
            "time": now.timestamp(),
            "timeNano": now.timestamp_nanos_opt().unwrap_or(0),
        });
        if kind == "container" || kind == "image" {
            ev["status"] = json!(action);
            ev["id"] = json!(id);
            if let Some(img) = attrs.get("image") {
                ev["from"] = json!(img);
            }
        }
        {
            let mut recent = self.recent_events.lock().unwrap();
            recent.push_back(ev.clone());
            while recent.len() > 1000 {
                recent.pop_front();
            }
        }
        let _ = self.events.send(ev);
    }

    pub fn recent_events(&self) -> Vec<Value> {
        self.recent_events.lock().unwrap().iter().cloned().collect()
    }

    fn container_attrs(rec: &Record) -> BTreeMap<String, String> {
        let mut a = rec.labels();
        a.insert("image".into(), rec.image.clone());
        a.insert("name".into(), rec.name.clone());
        a
    }

    pub fn emit_container(&self, rec: &Record, action: &str, extra: &[(&str, String)]) {
        let mut attrs = Self::container_attrs(rec);
        for (k, v) in extra {
            attrs.insert(k.to_string(), v.clone());
        }
        self.emit("container", action, &rec.id, attrs);
    }

    // -----------------------------------------------------------------
    // Lookup

    pub fn list(&self) -> Vec<Arc<Ctr>> {
        let mut v: Vec<Arc<Ctr>> = self.containers.read().unwrap().values().cloned().collect();
        v.sort_by_key(|c| std::cmp::Reverse(c.rec.lock().unwrap().created.clone()));
        v
    }

    /// By full ID, name (with or without the leading slash), or a unique
    /// ID prefix.
    pub fn find(&self, key: &str) -> Option<Arc<Ctr>> {
        let key = key.trim_start_matches('/');
        let all = self.containers.read().unwrap();
        if let Some(c) = all.get(key) {
            return Some(c.clone());
        }
        if let Some(c) = all.values().find(|c| c.rec.lock().unwrap().name == key) {
            return Some(c.clone());
        }
        let matches: Vec<&Arc<Ctr>> = all.values().filter(|c| c.id.starts_with(key)).collect();
        (matches.len() == 1 && !key.is_empty()).then(|| matches[0].clone())
    }

    pub fn get(&self, key: &str) -> ApiResult<Arc<Ctr>> {
        self.find(key).ok_or_else(|| ApiError::not_found(format!("No such container: {key}")))
    }

    fn save(&self, ctr: &Ctr) {
        let rec = ctr.rec.lock().unwrap().clone();
        let _ = write_json(&self.container_dir(&ctr.id).join("container.json"), &rec);
    }

    fn update(&self, ctr: &Ctr, f: impl FnOnce(&mut Record)) {
        f(&mut ctr.rec.lock().unwrap());
        self.save(ctr);
        ctr.bump();
    }

    // -----------------------------------------------------------------
    // Networks

    pub fn networks(&self) -> Vec<Network> {
        self.networks.read().unwrap().values().cloned().collect()
    }

    pub fn find_network(&self, key: &str) -> Option<Network> {
        let nets = self.networks.read().unwrap();
        if let Some(n) = nets.get(key) {
            return Some(n.clone());
        }
        if let Some(n) = nets.values().find(|n| n.name == key) {
            return Some(n.clone());
        }
        let m: Vec<&Network> = nets.values().filter(|n| n.id.starts_with(key)).collect();
        (m.len() == 1 && !key.is_empty()).then(|| m[0].clone())
    }

    async fn load_networks(&self) -> Result<()> {
        let mut nets = BTreeMap::new();
        for e in std::fs::read_dir(self.root.join("networks"))?.flatten() {
            if let Some(n) = read_json::<Network>(&e.path()) {
                nets.insert(n.id.clone(), n);
            }
        }
        for (name, driver) in [("bridge", "bridge"), ("host", "host"), ("none", "null")] {
            if !nets.values().any(|n| n.name == name) {
                let id = random_hex(32);
                let mut n = Network {
                    id: id.clone(),
                    name: name.into(),
                    driver: driver.into(),
                    created: now_rfc3339(),
                    subnet: String::new(),
                    gateway: String::new(),
                    bridge: String::new(),
                    internal: false,
                    attachable: false,
                    labels: BTreeMap::new(),
                    options: BTreeMap::new(),
                    predefined: true,
                };
                if name == "bridge" {
                    n.subnet = dnet::DEFAULT_SUBNET.into();
                    n.gateway = "172.17.0.1".into();
                    n.bridge = dnet::DEFAULT_BRIDGE.into();
                    n.options.insert("com.docker.network.bridge.default_bridge".into(), "true".into());
                    n.options.insert("com.docker.network.bridge.name".into(), dnet::DEFAULT_BRIDGE.into());
                }
                write_json(&self.root.join("networks").join(format!("{id}.json")), &n)?;
                nets.insert(id, n);
            }
        }
        *self.networks.write().unwrap() = nets;
        self.apply_network_rules().await
    }

    /// (Re)create every bridge and reload the iptables chains.
    async fn apply_network_rules(&self) -> Result<()> {
        let nets = self.networks();
        let mut script = String::new();
        let mut rule_nets = Vec::new();
        for n in nets.iter().filter(|n| n.driver == "bridge") {
            let prefix: u8 = n.subnet.split('/').nth(1).and_then(|p| p.parse().ok()).unwrap_or(16);
            script.push_str(&dnet::bridge_commands(&n.bridge, &n.gateway, prefix));
            script.push_str("; ");
            rule_nets.push(RuleNet { bridge: n.bridge.clone(), subnet: n.subnet.clone(), internal: n.internal });
        }
        let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");
        if !script.is_empty() {
            run_quiet("sh", &["-c", &script], None).await.context("failed to set up the Docker bridges")?;
        }
        run_quiet("sh", &["-c", &dnet::hooks()], None).await?;
        run_quiet("iptables-restore", &["--noflush"], Some(&dnet::rules(&rule_nets))).await?;
        Ok(())
    }

    pub async fn create_network(&self, body: &Value) -> ApiResult<(String, String)> {
        let name = body["Name"].as_str().unwrap_or_default().to_string();
        if name.is_empty() || !valid_name(&name) {
            return Err(ApiError::bad_request(format!("invalid network name {name:?}")));
        }
        let _held = self.net_lock.lock().await;
        if self.networks().iter().any(|n| n.name == name) {
            return Err(ApiError::conflict(format!("network with name {name} already exists")));
        }
        let driver = body["Driver"].as_str().filter(|d| !d.is_empty()).unwrap_or("bridge");
        if driver != "bridge" {
            return Err(ApiError::bad_request(format!("network driver {driver} is not supported (only bridge)")));
        }
        let id = random_hex(32);
        let requested = body.pointer("/IPAM/Config/0/Subnet").and_then(Value::as_str).map(str::to_string);
        let used: Vec<String> = self.networks().iter().map(|n| n.subnet.clone()).collect();
        let subnet = match requested {
            Some(s) => {
                let parsed = crate::kube::agent::crun::net::Subnet::parse(&s).map_err(|e| ApiError::bad_request(e.to_string()))?;
                parsed.cidr()
            }
            None => dnet::candidate_subnets().find(|s| !used.contains(s)).ok_or_else(|| ApiError::new(500, "no free subnet for the network"))?,
        };
        let parsed = crate::kube::agent::crun::net::Subnet::parse(&subnet).map_err(|e| ApiError::bad_request(e.to_string()))?;
        let gateway = body.pointer("/IPAM/Config/0/Gateway").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| parsed.gateway().to_string());
        let n = Network {
            id: id.clone(),
            name: name.clone(),
            driver: "bridge".into(),
            created: now_rfc3339(),
            subnet,
            gateway,
            bridge: dnet::bridge_name(&id, &name),
            internal: body["Internal"].as_bool().unwrap_or(false),
            attachable: body["Attachable"].as_bool().unwrap_or(false),
            labels: super::filters::labels_of(&body["Labels"]),
            options: body["Options"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect()).unwrap_or_default(),
            predefined: false,
        };
        write_json(&self.root.join("networks").join(format!("{id}.json")), &n)?;
        self.networks.write().unwrap().insert(id.clone(), n);
        self.apply_network_rules().await?;
        self.emit("network", "create", &id, BTreeMap::from([("name".to_string(), name.clone()), ("type".to_string(), "bridge".to_string())]));
        Ok((id, String::new()))
    }

    pub fn network_containers(&self, net: &Network) -> Value {
        let mut out = serde_json::Map::new();
        for c in self.list() {
            let r = c.record();
            if !r.state.running() {
                continue;
            }
            if let Some(e) = r.endpoints.iter().find(|e| e.network_id == net.id) {
                out.insert(
                    r.id.clone(),
                    json!({
                        "Name": r.name,
                        "EndpointID": e.endpoint_id,
                        "MacAddress": e.mac,
                        "IPv4Address": e.ip.as_ref().map(|ip| format!("{ip}/{}", e.prefix)).unwrap_or_default(),
                        "IPv6Address": "",
                    }),
                );
            }
        }
        Value::Object(out)
    }

    pub async fn remove_network(&self, key: &str) -> ApiResult<()> {
        let net = self.find_network(key).ok_or_else(|| ApiError::not_found(format!("network {key} not found")))?;
        if net.predefined {
            return Err(ApiError::new(403, format!("{} is a pre-defined network and cannot be removed", net.name)));
        }
        let _held = self.net_lock.lock().await;
        let users: Vec<String> = self.list().iter().map(|c| c.record()).filter(|r| r.state.running() && r.endpoints.iter().any(|e| e.network_id == net.id)).map(|r| r.name).collect();
        if !users.is_empty() {
            return Err(ApiError::new(403, format!("error while removing network: network {} has active endpoints (name:{:?})", net.name, users.join(","))));
        }
        let _ = std::fs::remove_file(self.root.join("networks").join(format!("{}.json", net.id)));
        let _ = std::fs::remove_dir_all(self.root.join("ipam").join(&net.id));
        self.networks.write().unwrap().remove(&net.id);
        let _ = run_quiet("ip", &["link", "del", &net.bridge], None).await;
        self.apply_network_rules().await?;
        // Stopped containers forget it.
        for c in self.list() {
            if c.rec.lock().unwrap().endpoints.iter().any(|e| e.network_id == net.id) {
                self.update(&c, |r| r.endpoints.retain(|e| e.network_id != net.id));
            }
        }
        self.emit("network", "destroy", &net.id, BTreeMap::from([("name".to_string(), net.name.clone()), ("type".to_string(), "bridge".to_string())]));
        Ok(())
    }

    fn endpoint_for(&self, net: &Network, config: &Value, rec_name: &str, id: &str) -> Endpoint {
        let mut aliases = strings(&config["Aliases"]);
        // Docker adds the short ID as an alias on user-defined networks.
        if net.name != "bridge" && !aliases.contains(&id[..12].to_string()) {
            aliases.push(id[..12].to_string());
        }
        let _ = rec_name;
        Endpoint {
            network_id: net.id.clone(),
            network_name: net.name.clone(),
            aliases,
            requested_ip: config.pointer("/IPAMConfig/IPv4Address").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string),
            driver_opts: config.get("DriverOpts").cloned().filter(|v| !v.is_null()),
            ..Default::default()
        }
    }

    pub async fn connect(&self, net_key: &str, ctr_key: &str, config: &Value) -> ApiResult<()> {
        let net = self.find_network(net_key).ok_or_else(|| ApiError::not_found(format!("network {net_key} not found")))?;
        let ctr = self.get(ctr_key)?;
        if net.driver != "bridge" {
            return Err(ApiError::bad_request(format!("container cannot be connected to network {}", net.name)));
        }
        let rec = ctr.record();
        if rec.endpoints.iter().any(|e| e.network_id == net.id) {
            return Err(ApiError::new(403, format!("endpoint with name {} already exists in network {}", rec.name, net.name)));
        }
        let mode = rec.network_mode();
        if mode == "host" || mode == "none" || mode.starts_with("container:") {
            return Err(ApiError::bad_request(format!("container sharing network namespace with another container or host cannot be connected to any other network")));
        }
        let mut ep = self.endpoint_for(&net, config, &rec.name, &rec.id);
        if rec.state.running() {
            let _held = self.net_lock.lock().await;
            let index = rec.endpoints.len();
            let netns = rec.state.netns.clone().ok_or_else(|| ApiError::new(500, "container has no network namespace"))?;
            let plan = self.plan_link(&rec.id, &net, &mut ep, index, false)?;
            dnet::ip_batch_in(PathBuf::from(netns), dnet::link_commands(std::slice::from_ref(&plan), false))?;
            sys::bridge_attach(&net.bridge, &plan.host_if)?;
        }
        self.update(&ctr, |r| r.endpoints.push(ep));
        self.refresh_dns(&ctr);
        self.emit("network", "connect", &net.id, BTreeMap::from([("container".to_string(), rec.id.clone()), ("name".to_string(), net.name.clone()), ("type".to_string(), "bridge".to_string())]));
        Ok(())
    }

    pub async fn disconnect(&self, net_key: &str, ctr_key: &str) -> ApiResult<()> {
        let net = self.find_network(net_key).ok_or_else(|| ApiError::not_found(format!("network {net_key} not found")))?;
        let ctr = self.get(ctr_key)?;
        let rec = ctr.record();
        let Some(ep) = rec.endpoints.iter().find(|e| e.network_id == net.id).cloned() else {
            return Err(ApiError::not_found(format!("container {} is not connected to network {}", rec.name, net.name)));
        };
        if rec.state.running() {
            if let Some(netns) = &rec.state.netns {
                let _ = dnet::ip_batch_in(PathBuf::from(netns), format!("link del {}\n", ep.interface));
            }
            if let Ok(ipam) = dnet::ipam_for(&self.root, &net.id, &net.subnet) {
                ipam.release(&rec.id);
            }
        }
        self.update(&ctr, |r| r.endpoints.retain(|e| e.network_id != net.id));
        self.refresh_dns(&ctr);
        self.emit("network", "disconnect", &net.id, BTreeMap::from([("container".to_string(), rec.id.clone()), ("name".to_string(), net.name.clone()), ("type".to_string(), "bridge".to_string())]));
        Ok(())
    }

    /// Reserve an address and name the interfaces for one endpoint.
    fn plan_link(&self, id: &str, net: &Network, ep: &mut Endpoint, index: usize, default_route: bool) -> Result<LinkPlan> {
        let ipam = dnet::ipam_for(&self.root, &net.id, &net.subnet)?;
        let ip = match &ep.requested_ip {
            Some(want) => {
                let ip: Ipv4Addr = want.parse().context("invalid IP address")?;
                let file = self.root.join("ipam").join(&net.id).join(ip.to_string());
                match std::fs::OpenOptions::new().write(true).create_new(true).open(&file) {
                    Ok(mut f) => {
                        use std::io::Write;
                        f.write_all(id.as_bytes())?;
                    }
                    Err(_) if std::fs::read_to_string(&file).is_ok_and(|o| o == id) => {}
                    Err(_) => bail!("Address already in use: {ip}"),
                }
                ip
            }
            None => ipam.allocate(id)?,
        };
        let prefix = ipam.subnet.prefix;
        let gateway: Ipv4Addr = net.gateway.parse().unwrap_or(ipam.subnet.gateway());
        ep.ip = Some(ip.to_string());
        ep.prefix = prefix;
        ep.gateway = gateway.to_string();
        ep.mac = dnet::mac_for(ip);
        ep.endpoint_id = random_hex(32);
        ep.interface = format!("eth{index}");
        Ok(LinkPlan {
            interface: ep.interface.clone(),
            // Unique per run: the previous run's namespace (and the veth in
            // it) is torn down asynchronously by the kernel.
            host_if: format!("vd{}{index}", random_hex(4)),
            ip,
            prefix,
            gateway,
            mac: ep.mac.clone(),
            default_route: default_route && !net.internal,
        })
    }

    fn refresh_dns(&self, ctr: &Ctr) {
        let r = ctr.record();
        if !r.state.running() {
            self.dns.write().unwrap().remove(&r.id);
            return;
        }
        let hostname = r.config["Hostname"].as_str().unwrap_or_default().to_string();
        let entries = r
            .endpoints
            .iter()
            .filter_map(|e| {
                let ip: Ipv4Addr = e.ip.as_ref()?.parse().ok()?;
                let mut names = vec![r.name.clone(), r.id[..12].to_string(), r.id.clone()];
                names.extend(e.aliases.iter().cloned());
                if !hostname.is_empty() {
                    names.push(hostname.clone());
                }
                Some((e.network_id.clone(), ip, names, e.network_name != "bridge"))
            })
            .collect();
        self.dns.write().unwrap().set(&r.id, entries);
    }

    // -----------------------------------------------------------------
    // Volumes

    pub fn volume_dir(&self, name: &str) -> PathBuf {
        self.root.join("volumes").join(name)
    }

    pub fn volumes(&self) -> Vec<Volume> {
        let mut v: Vec<Volume> = std::fs::read_dir(self.root.join("volumes"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| read_json(&e.path().join("volume.json")))
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn volume(&self, name: &str) -> Option<Volume> {
        if name.is_empty() || name.contains('/') || name.starts_with('.') {
            return None;
        }
        read_json(&self.volume_dir(name).join("volume.json"))
    }

    pub fn create_volume(&self, name: Option<&str>, labels: BTreeMap<String, String>, options: BTreeMap<String, String>, anonymous: bool) -> ApiResult<Volume> {
        let name = name.filter(|n| !n.is_empty()).map(str::to_string).unwrap_or_else(|| random_hex(32));
        if !valid_name(&name) {
            return Err(ApiError::bad_request(format!("create {name}: invalid volume name")));
        }
        let _held = self.vol_lock.lock().unwrap();
        if let Some(v) = self.volume(&name) {
            return Ok(v);
        }
        let dir = self.volume_dir(&name);
        let data = dir.join("_data");
        std::fs::create_dir_all(&data)?;
        let mountpoint = match (options.get("o"), options.get("device")) {
            (Some(o), Some(device)) if o.split(',').any(|x| x == "bind") => device.clone(),
            _ => data.to_string_lossy().to_string(),
        };
        let v = Volume { name: name.clone(), created: now_rfc3339(), labels, options, mountpoint, anonymous };
        write_json(&dir.join("volume.json"), &v)?;
        self.emit("volume", "create", &name, BTreeMap::from([("driver".to_string(), "local".to_string())]));
        Ok(v)
    }

    pub fn volume_users(&self, name: &str) -> Vec<String> {
        self.list()
            .iter()
            .map(|c| c.record())
            .filter(|r| r.mounts.iter().any(|m| m.name.as_deref() == Some(name)))
            .map(|r| r.id)
            .collect()
    }

    pub fn remove_volume(&self, name: &str, force: bool) -> ApiResult<()> {
        if self.volume(name).is_none() {
            return Err(ApiError::not_found(format!("get {name}: no such volume")));
        }
        let users = self.volume_users(name);
        if !users.is_empty() && !force {
            return Err(ApiError::conflict(format!("remove {name}: volume is in use - [{}]", users.join(", "))));
        }
        let dir = self.volume_dir(name);
        std::fs::remove_dir_all(&dir).with_context(|| format!("failed to remove volume {name}"))?;
        self.emit("volume", "destroy", name, BTreeMap::from([("driver".to_string(), "local".to_string())]));
        Ok(())
    }

    // -----------------------------------------------------------------
    // Create

    /// Resolve the container's mounts: binds, volumes (named, anonymous,
    /// the image's), tmpfs.
    fn resolve_mounts(&self, config: &Value, host: &Value) -> ApiResult<Vec<MountPoint>> {
        let mut out: Vec<MountPoint> = Vec::new();
        let socket = self.internal_socket.to_string_lossy().to_string();
        let add = |m: MountPoint, out: &mut Vec<MountPoint>| {
            out.retain(|x| x.destination != m.destination);
            out.push(m);
        };
        let map_source = |src: &str| -> String {
            let s = src.trim_end_matches('/');
            // The Docker socket by any of its names (Testcontainers' Ryuk,
            // docker-in-docker tooling): this engine's.
            if self.socket_aliases.iter().any(|a| a == s) || s.ends_with("/docker.sock") {
                return socket.clone();
            }
            if cfg!(target_os = "linux") && std::env::var("LITEK8S_DEV_GUEST").is_ok_and(|v| v == "1") {
                // macOS paths through the VM's shares.
                for (mac, guest) in [("/var/folders/", "/private/var/folders/"), ("/tmp/", "/private/tmp/")] {
                    if let Some(rest) = src.strip_prefix(mac) {
                        return format!("{guest}{rest}");
                    }
                }
            }
            src.to_string()
        };
        for bind in strings(&host["Binds"]) {
            let parts: Vec<&str> = bind.split(':').collect();
            let (src, dst, opts) = match parts.as_slice() {
                [s, d] => (*s, *d, ""),
                [s, d, o] => (*s, *d, *o),
                [d] => ("", *d, ""),
                _ => return Err(ApiError::bad_request(format!("invalid volume specification: '{bind}'"))),
            };
            let ro = opts.split(',').any(|o| o == "ro");
            let nocopy = opts.split(',').any(|o| o == "nocopy");
            if src.starts_with('/') {
                let src = map_source(src);
                if std::fs::symlink_metadata(&src).is_err() {
                    std::fs::create_dir_all(&src).map_err(|e| ApiError::bad_request(format!("invalid mount config: bind source path {src}: {e}")))?;
                }
                add(MountPoint { kind: "bind".into(), source: src, destination: dst.into(), rw: !ro, mode: opts.into(), ..Default::default() }, &mut out);
            } else {
                let anonymous = src.is_empty();
                let v = self.create_volume(Some(src).filter(|s| !s.is_empty()), BTreeMap::new(), BTreeMap::new(), anonymous)?;
                add(MountPoint { kind: "volume".into(), name: Some(v.name.clone()), source: v.mountpoint.clone(), destination: dst.into(), rw: !ro, mode: opts.into(), nocopy, anonymous, ..Default::default() }, &mut out);
            }
        }
        for m in host["Mounts"].as_array().into_iter().flatten() {
            let kind = m["Type"].as_str().unwrap_or("volume");
            let target = m["Target"].as_str().unwrap_or_default().to_string();
            if target.is_empty() {
                return Err(ApiError::bad_request("invalid mount config: Target must not be empty"));
            }
            let ro = m["ReadOnly"].as_bool().unwrap_or(false);
            match kind {
                "bind" => {
                    let src = map_source(m["Source"].as_str().unwrap_or_default());
                    if std::fs::symlink_metadata(&src).is_err() {
                        if m.pointer("/BindOptions/CreateMountpoint").and_then(Value::as_bool).unwrap_or(false) {
                            std::fs::create_dir_all(&src)?;
                        } else {
                            return Err(ApiError::bad_request(format!("invalid mount config for type \"bind\": bind source path does not exist: {src}")));
                        }
                    }
                    add(MountPoint { kind: "bind".into(), source: src, destination: target, rw: !ro, ..Default::default() }, &mut out);
                }
                "volume" => {
                    let src = m["Source"].as_str().unwrap_or_default();
                    let labels = super::filters::labels_of(&m.pointer("/VolumeOptions/Labels").cloned().unwrap_or(Value::Null));
                    let opts: BTreeMap<String, String> = m
                        .pointer("/VolumeOptions/DriverConfig/Options")
                        .and_then(Value::as_object)
                        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect())
                        .unwrap_or_default();
                    let anonymous = src.is_empty();
                    let v = self.create_volume(Some(src).filter(|s| !s.is_empty()), labels, opts, anonymous)?;
                    let nocopy = m.pointer("/VolumeOptions/NoCopy").and_then(Value::as_bool).unwrap_or(false);
                    add(MountPoint { kind: "volume".into(), name: Some(v.name.clone()), source: v.mountpoint.clone(), destination: target, rw: !ro, nocopy, anonymous, ..Default::default() }, &mut out);
                }
                "tmpfs" => {
                    let size = m.pointer("/TmpfsOptions/SizeBytes").and_then(Value::as_i64);
                    let mode = m.pointer("/TmpfsOptions/Mode").and_then(Value::as_u64).map(|m| m as u32);
                    add(MountPoint { kind: "tmpfs".into(), destination: target, rw: !ro, tmpfs_size: size, tmpfs_mode: mode, ..Default::default() }, &mut out);
                }
                other => return Err(ApiError::bad_request(format!("mount type {other} is not supported"))),
            }
        }
        if let Some(t) = host["Tmpfs"].as_object() {
            for (target, opts) in t {
                let opts = opts.as_str().unwrap_or_default();
                let size = opts.split(',').find_map(|o| o.strip_prefix("size=")).and_then(parse_size);
                let mode = opts.split(',').find_map(|o| o.strip_prefix("mode=")).and_then(|m| u32::from_str_radix(m, 8).ok());
                add(MountPoint { kind: "tmpfs".into(), destination: target.clone(), rw: !opts.split(',').any(|o| o == "ro"), tmpfs_size: size, tmpfs_mode: mode, ..Default::default() }, &mut out);
            }
        }
        if let Some(vols) = config["Volumes"].as_object() {
            for target in vols.keys() {
                if out.iter().any(|m| m.destination.trim_end_matches('/') == target.trim_end_matches('/')) {
                    continue;
                }
                let v = self.create_volume(None, BTreeMap::new(), BTreeMap::new(), true)?;
                out.push(MountPoint { kind: "volume".into(), name: Some(v.name.clone()), source: v.mountpoint.clone(), destination: target.clone(), rw: true, anonymous: true, ..Default::default() });
            }
        }
        Ok(out)
    }

    pub async fn create(self: &Arc<Self>, name: Option<String>, body: Value, platform: Option<String>) -> ApiResult<(String, Vec<String>)> {
        let image_ref = body["Image"].as_str().unwrap_or_default().to_string();
        if image_ref.is_empty() {
            return Err(ApiError::bad_request("config cannot be empty in order to create a container"));
        }
        let info = images::resolve(&self.images, &image_ref).ok_or_else(|| ApiError::not_found(format!("No such image: {image_ref}")))?;
        if let Some(p) = platform.as_deref().and_then(cimage::parse_platform) {
            let cfg = self.images.config_blob(&info.entry.config).unwrap_or(Value::Null);
            if cfg["architecture"].as_str().is_some_and(|a| a != p.0) {
                return Err(ApiError::not_found(format!("image with reference {image_ref} was found but does not match the specified platform: wanted linux/{}", p.0)));
            }
        }
        let name = match name.filter(|n| !n.is_empty()) {
            Some(n) => {
                let n = n.trim_start_matches('/').to_string();
                if !valid_name(&n) {
                    return Err(ApiError::bad_request(format!("Invalid container name ({n}), only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed")));
                }
                if let Some(other) = self.list().iter().find(|c| c.rec.lock().unwrap().name == n) {
                    return Err(ApiError::conflict(format!(
                        "Conflict. The container name \"/{n}\" is already in use by container \"{}\". You have to remove (or rename) that container to be able to reuse that name.",
                        other.id
                    )));
                }
                n
            }
            None => random_name(|n| self.find(n).is_some()),
        };
        let id = random_hex(32);
        let image_cfg = self.images.config_blob(&info.entry.config).unwrap_or(Value::Null);
        let mut user_config = body.clone();
        if let Some(o) = user_config.as_object_mut() {
            o.remove("HostConfig");
            o.remove("NetworkingConfig");
        }
        let mut config = merge_config(&user_config, image_cfg.get("config").unwrap_or(&Value::Null));
        if is_empty(&config["Hostname"]) {
            config["Hostname"] = json!(&id[..12]);
        }
        if config["Domainname"].is_null() {
            config["Domainname"] = json!("");
        }
        config["Image"] = json!(image_ref);
        let mut host = body.get("HostConfig").cloned().unwrap_or(json!({}));
        if !host.is_object() {
            host = json!({});
        }
        if is_empty(&host["NetworkMode"]) {
            host["NetworkMode"] = json!("bridge");
        }
        if host["RestartPolicy"].is_null() {
            host["RestartPolicy"] = json!({"Name": "no", "MaximumRetryCount": 0});
        }
        if host["LogConfig"].is_null() {
            host["LogConfig"] = json!({"Type": "json-file", "Config": {}});
        }
        let mounts = self.resolve_mounts(&config, &host)?;
        // Networks: the mode's network plus NetworkingConfig's endpoints.
        let mode = match host["NetworkMode"].as_str().unwrap_or("bridge") {
            "default" => "bridge".to_string(),
            m => m.to_string(),
        };
        let mut endpoints = Vec::new();
        let ep_configs = body.pointer("/NetworkingConfig/EndpointsConfig").and_then(Value::as_object).cloned().unwrap_or_default();
        if mode != "host" && mode != "none" && !mode.starts_with("container:") {
            let primary = self.find_network(&mode).ok_or_else(|| ApiError::not_found(format!("network {mode} not found")))?;
            if primary.driver == "host" || primary.driver == "null" {
                host["NetworkMode"] = json!(primary.name);
            } else {
                let cfg = ep_configs
                    .get(&primary.name)
                    .or_else(|| ep_configs.get(&mode))
                    .or_else(|| ep_configs.get("default"))
                    .or_else(|| ep_configs.get(&primary.id))
                    .cloned()
                    .unwrap_or(Value::Null);
                endpoints.push(self.endpoint_for(&primary, &cfg, &name, &id));
                for (net_name, cfg) in &ep_configs {
                    if *net_name == primary.name || *net_name == mode || net_name == "default" || *net_name == primary.id {
                        continue;
                    }
                    let net = self.find_network(net_name).ok_or_else(|| ApiError::not_found(format!("network {net_name} not found")))?;
                    endpoints.push(self.endpoint_for(&net, cfg, &name, &id));
                }
            }
        } else if let Some(target) = mode.strip_prefix("container:") {
            let other = self.get(target)?;
            host["NetworkMode"] = json!(format!("container:{}", other.id));
        }
        let dir = self.container_dir(&id);
        for d in ["upper", "work", "rootfs"] {
            std::fs::create_dir_all(dir.join(d))?;
        }
        let rec = Record {
            id: id.clone(),
            name,
            created: now_rfc3339(),
            image: image_ref,
            image_id: info.id.clone(),
            image_key: info.keys.first().cloned().unwrap_or_default(),
            config,
            host_config: host,
            endpoints,
            mounts,
            platform: platform.unwrap_or_default(),
            state: State::default(),
            restart_count: 0,
            manually_stopped: false,
        };
        write_json(&dir.join("container.json"), &rec)?;
        let ctr = Arc::new(Ctr { id: id.clone(), rec: Mutex::new(rec.clone()), op: tokio::sync::Mutex::new(()), changed: watch::channel(0).0, rt: Mutex::new(Runtime::default()) });
        self.containers.write().unwrap().insert(id.clone(), ctr);
        self.emit_container(&rec, "create", &[]);
        Ok((id, Vec::new()))
    }

    // -----------------------------------------------------------------
    // Start

    /// The image's layers for a container (its tag may be gone: the image
    /// stays under its ID while containers use it).
    fn layers_of(&self, rec: &Record) -> Result<Vec<PathBuf>> {
        let info = images::by_id(&self.images, &rec.image_id).with_context(|| format!("image {} of the container is gone", rec.image_id))?;
        let key = info.keys.first().cloned().unwrap_or_default();
        let image = self.images.image_by_key(&key).with_context(|| format!("image {} is incomplete", rec.image_id))?;
        Ok(image.layers)
    }

    pub fn overlay_options_for(&self, rec: &Record) -> Result<String> {
        self.overlay_options(rec)
    }

    fn overlay_options(&self, rec: &Record) -> Result<String> {
        let dir = self.container_dir(&rec.id);
        let lowers: Vec<PathBuf> = self.layers_of(rec)?.into_iter().rev().collect();
        let mut options = sys::overlay_options(&lowers, &dir.join("upper"), &dir.join("work"))?;
        if sys::in_user_namespace() {
            options.push_str(",userxattr");
        }
        Ok(options)
    }

    fn etc_files(&self, rec: &Record, dir: &Path, share_from: Option<&Record>) -> Result<()> {
        if let Some(other) = share_from {
            let odir = self.container_dir(&other.id);
            for f in ["hosts", "resolv.conf", "hostname"] {
                let _ = std::fs::copy(odir.join(f), dir.join(f));
            }
            return Ok(());
        }
        let hostname = rec.config["Hostname"].as_str().unwrap_or_default();
        let domain = rec.config["Domainname"].as_str().unwrap_or_default();
        let fqdn = if domain.is_empty() { hostname.to_string() } else { format!("{hostname}.{domain}") };
        let host_mode = rec.network_mode() == "host";
        let gateway = self.dns.read().unwrap().host_gateway.map(|g| g.to_string());
        let mut hosts = String::new();
        if host_mode {
            hosts.push_str(&std::fs::read_to_string("/etc/hosts").unwrap_or_else(|_| "127.0.0.1\tlocalhost\n".into()));
            if !hosts.ends_with('\n') {
                hosts.push('\n');
            }
        } else {
            hosts.push_str("127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\nfe00::0\tip6-localnet\nff00::0\tip6-mcastprefix\nff02::1\tip6-allnodes\nff02::2\tip6-allrouters\n");
            if let Some(ip) = rec.endpoints.iter().find_map(|e| e.ip.clone()) {
                hosts.push_str(&format!("{ip}\t{fqdn}{}\n", if fqdn != hostname { format!(" {hostname}") } else { String::new() }));
            }
        }
        for entry in strings(&rec.host_config["ExtraHosts"]) {
            let (name, addr) = match entry.split_once('=') {
                Some(x) => x,
                None => match entry.rsplit_once(':') {
                    Some(x) => x,
                    None => continue,
                },
            };
            let addr = if addr == "host-gateway" { gateway.clone().unwrap_or_else(|| "127.0.0.1".into()) } else { addr.to_string() };
            hosts.push_str(&format!("{addr}\t{name}\n"));
        }
        std::fs::write(dir.join("hosts"), hosts)?;
        let host_resolv = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
        let resolv = if host_mode {
            host_resolv
        } else {
            let mut r = String::new();
            let servers = strings(&rec.host_config["Dns"]);
            if servers.is_empty() {
                match rec.endpoints.iter().find_map(|e| e.ip.as_ref().map(|_| e.gateway.clone())) {
                    Some(gw) => r.push_str(&format!("nameserver {gw}\n")),
                    None => {
                        for l in host_resolv.lines().filter(|l| l.starts_with("nameserver")) {
                            r.push_str(l);
                            r.push('\n');
                        }
                    }
                }
            } else {
                for s in &servers {
                    r.push_str(&format!("nameserver {s}\n"));
                }
            }
            let search = strings(&rec.host_config["DnsSearch"]);
            let search = if search.is_empty() {
                host_resolv.lines().find_map(|l| l.strip_prefix("search ")).map(|s| s.split_whitespace().map(str::to_string).collect()).unwrap_or_default()
            } else {
                search
            };
            if !search.is_empty() {
                r.push_str(&format!("search {}\n", search.join(" ")));
            }
            let mut options = strings(&rec.host_config["DnsOptions"]);
            if !options.iter().any(|o| o.starts_with("ndots")) {
                options.insert(0, "ndots:0".into());
            }
            r.push_str(&format!("options {}\n", options.join(" ")));
            r
        };
        std::fs::write(dir.join("resolv.conf"), resolv)?;
        std::fs::write(dir.join("hostname"), format!("{hostname}\n"))?;
        Ok(())
    }

    /// The OCI bundle's config.json for a run.
    fn bundle(&self, rec: &Record, dir: &Path, netns: Option<String>, own_uts: bool, pid: cspec::PidNamespace) -> Result<Value> {
        let c = &rec.config;
        let h = &rec.host_config;
        let rootfs = dir.join("rootfs");
        let etc = |name: &str| {
            let p = secure_join(&rootfs, &format!("/etc/{name}"));
            match std::fs::symlink_metadata(&p) {
                Ok(m) if m.is_file() => std::fs::read_to_string(&p).unwrap_or_default(),
                _ => String::new(),
            }
        };
        let user_str = c["User"].as_str().unwrap_or_default();
        let user = cspec::resolve_user(user_str, &etc("passwd"), &etc("group")).map_err(|e| anyhow::anyhow!("{e:#}"))?;
        let security: Vec<String> = strings(&h["SecurityOpt"]);
        let mut spec = ContainerSpec {
            name: rec.name.clone(),
            image: rec.image.clone(),
            entrypoint: Some(strings(&c["Entrypoint"])),
            cmd: Some(strings(&c["Cmd"])),
            env: strings(&c["Env"]),
            working_dir: c["WorkingDir"].as_str().filter(|w| !w.is_empty()).map(str::to_string),
            user: Some(user_str.to_string()),
            tty: rec.tty(),
            stdin: c["OpenStdin"].as_bool().unwrap_or(false),
            privileged: h["Privileged"].as_bool().unwrap_or(false),
            cap_add: strings(&h["CapAdd"]),
            cap_drop: strings(&h["CapDrop"]),
            read_only_rootfs: h["ReadonlyRootfs"].as_bool().unwrap_or(false),
            memory_limit: h["Memory"].as_i64().filter(|m| *m > 0),
            nano_cpus: h["NanoCpus"].as_i64().filter(|n| *n > 0).or_else(|| {
                let quota = h["CpuQuota"].as_i64().filter(|q| *q > 0)?;
                let period = h["CpuPeriod"].as_i64().filter(|p| *p > 0).unwrap_or(100_000);
                Some(quota * 1_000_000_000 / period)
            }),
            no_new_privileges: security.iter().any(|s| s.starts_with("no-new-privileges") && !s.ends_with("false")),
            seccomp_unconfined: security.iter().any(|s| s == "seccomp=unconfined" || s == "seccomp:unconfined"),
            ..Default::default()
        };
        if let Some(shares) = h["CpuShares"].as_i64().filter(|s| *s > 0) {
            spec.cpu_request_milli = Some(shares * 1000 / 1024);
        }
        for g in strings(&h["GroupAdd"]) {
            if let Ok(gid) = g.parse::<u32>() {
                spec.supplemental_groups.push(gid);
            } else if let Some(gid) = etc("group").lines().find_map(|l| {
                let f: Vec<&str> = l.split(':').collect();
                (f.len() > 2 && f[0] == g).then(|| f[2].parse::<u32>().ok()).flatten()
            }) {
                spec.supplemental_groups.push(gid);
            }
        }
        let mounts: Vec<cspec::ResolvedMount> = rec
            .mounts
            .iter()
            .map(|m| cspec::ResolvedMount {
                source: m.source.clone(),
                target: m.destination.clone(),
                kind: if m.kind == "tmpfs" { cspec::MountKind::Tmpfs } else { cspec::MountKind::Bind },
                read_only: !m.rw,
            })
            .collect();
        let image = cspec::ImageConfig::default();
        let etc_files: Vec<(String, String)> =
            ["hosts", "resolv.conf", "hostname"].iter().map(|f| (format!("/etc/{f}"), dir.join(f).to_string_lossy().to_string())).collect();
        let hostname = c["Hostname"].as_str().unwrap_or_default().to_string();
        let params = cspec::BundleParams {
            spec: &spec,
            image: &image,
            user,
            hostname: &hostname,
            netns,
            ipcns: None,
            own_uts,
            pid,
            etc_files,
            shm: None,
            mounts,
            cgroups_path: self.cgroup(&rec.id),
        };
        let mut config = cspec::config(&params)?;
        // What the pod-oriented bundle does not cover.
        if let Some(arr) = config["mounts"].as_array_mut() {
            for m in arr.iter_mut() {
                let dest = m["destination"].as_str().unwrap_or_default().to_string();
                if dest == "/dev/shm" {
                    let size = h["ShmSize"].as_i64().filter(|s| *s > 0).unwrap_or(64 << 20);
                    m["options"] = json!(["nosuid", "noexec", "nodev", "mode=1777", format!("size={}k", size / 1024)]);
                }
                if let Some(mp) = rec.mounts.iter().find(|x| x.kind == "tmpfs" && x.destination.trim_end_matches('/') == dest) {
                    let mut opts = vec!["nosuid".to_string(), "nodev".to_string(), format!("mode={:o}", mp.tmpfs_mode.unwrap_or(0o1777))];
                    if let Some(s) = mp.tmpfs_size {
                        opts.push(format!("size={}k", (s / 1024).max(1)));
                    }
                    if !mp.rw {
                        opts.push("ro".into());
                    }
                    m["options"] = json!(opts);
                }
            }
        }
        if let Some(domain) = c["Domainname"].as_str().filter(|d| !d.is_empty() && own_uts) {
            config["domainname"] = json!(domain);
        }
        if let Some(ul) = h["Ulimits"].as_array().filter(|a| !a.is_empty()) {
            let mut rl: Vec<Value> = config["process"]["rlimits"].as_array().cloned().unwrap_or_default();
            for u in ul {
                let name = format!("RLIMIT_{}", u["Name"].as_str().unwrap_or_default().to_uppercase());
                rl.retain(|r| r["type"] != json!(name));
                rl.push(json!({"type": name, "soft": u["Soft"].as_i64().unwrap_or(0), "hard": u["Hard"].as_i64().unwrap_or(0)}));
            }
            config["process"]["rlimits"] = json!(rl);
        }
        if let Some(sysctls) = h["Sysctls"].as_object().filter(|m| !m.is_empty()) {
            config["linux"]["sysctl"] = Value::Object(sysctls.clone());
        }
        if let Some(pids) = h["PidsLimit"].as_i64().filter(|p| *p > 0) {
            config["linux"]["resources"]["pids"] = json!({"limit": pids});
        }
        if let Some(m) = h["MemoryReservation"].as_i64().filter(|m| *m > 0) {
            config["linux"]["resources"]["memory"]["reservation"] = json!(m);
        }
        Ok(config)
    }

    /// Bring a container's network up: its namespace and one veth per
    /// network, with addresses. Returns the namespace file.
    async fn network_up(&self, ctr: &Ctr) -> Result<Option<String>> {
        let rec = ctr.record();
        let mode = rec.network_mode();
        if mode == "host" {
            return Ok(None);
        }
        if let Some(target) = mode.strip_prefix("container:") {
            let other = self.find(target).context("the container whose network this one shares is gone")?;
            let o = other.record();
            if !o.state.running() {
                bail!("cannot join network of a non running container: {}", o.id);
            }
            return Ok(o.state.netns.clone());
        }
        let _held = self.net_lock.lock().await;
        let netns = format!("{}/lkd-{}", crate::kube::agent::crun::net::NETNS_DIR, &rec.id[..12]);
        let ipc = format!("{RUN_DIR}/ipc-{}", &rec.id[..12]);
        sys::remove_ns_file(Path::new(&netns));
        sys::remove_ns_file(Path::new(&ipc));
        let mut endpoints = rec.endpoints.clone();
        let mut plans = Vec::new();
        if mode != "none" {
            for (i, ep) in endpoints.iter_mut().enumerate() {
                let net = self.networks.read().unwrap().get(&ep.network_id).cloned();
                let Some(net) = net else { continue };
                plans.push((net.bridge.clone(), self.plan_link(&rec.id, &net, ep, i, i == 0)?));
            }
        }
        let links: Vec<LinkPlan> = plans.iter().map(|(_, p)| LinkPlan { interface: p.interface.clone(), host_if: p.host_if.clone(), ip: p.ip, prefix: p.prefix, gateway: p.gateway, mac: p.mac.clone(), default_route: p.default_route }).collect();
        let batch = dnet::link_commands(&links, true);
        let sysctls: Vec<(String, String)> = rec.host_config["Sysctls"]
            .as_object()
            .map(|m| m.iter().filter(|(k, _)| k.starts_with("net.")).map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect())
            .unwrap_or_default();
        let (n, i) = (PathBuf::from(&netns), PathBuf::from(&ipc));
        let result = tokio::task::spawn_blocking(move || sys::create_namespaces(Some(n), i, Some(batch), sysctls)).await?;
        if let Err(e) = result {
            self.release_addresses(&rec.id);
            return Err(e);
        }
        for (bridge, p) in &plans {
            sys::bridge_attach(bridge, &p.host_if)?;
        }
        self.update(ctr, |r| {
            r.endpoints = endpoints;
            r.state.netns = Some(netns.clone());
        });
        Ok(Some(netns))
    }

    fn release_addresses(&self, id: &str) {
        for net in self.networks() {
            if net.driver == "bridge" {
                if let Ok(ipam) = dnet::ipam_for(&self.root, &net.id, &net.subnet) {
                    ipam.release(id);
                }
            }
        }
    }

    fn network_down(&self, ctr: &Ctr) {
        let rec = ctr.record();
        self.dns.write().unwrap().remove(&rec.id);
        if !rec.network_mode().starts_with("container:") {
            if let Some(n) = &rec.state.netns {
                sys::remove_ns_file(Path::new(n));
            }
            sys::remove_ns_file(Path::new(&format!("{RUN_DIR}/ipc-{}", &rec.id[..12])));
        }
        self.release_addresses(&rec.id);
    }

    /// Bind the published ports and start relaying them.
    fn publish_ports(&self, ctr: &Ctr) -> Result<BTreeMap<String, Vec<(String, String)>>> {
        let rec = ctr.record();
        let mut bindings = dnet::port_bindings(&rec.host_config);
        if rec.host_config["PublishAllPorts"].as_bool().unwrap_or(false) {
            if let Some(exposed) = rec.config["ExposedPorts"].as_object() {
                for k in exposed.keys() {
                    let key = if k.contains('/') { k.clone() } else { format!("{k}/tcp") };
                    bindings.entry(key).or_insert_with(|| vec![(String::new(), 0)]);
                }
            }
        }
        let target_ip: Option<Ipv4Addr> = rec.endpoints.iter().find_map(|e| e.ip.as_ref()?.parse().ok());
        let mut out = BTreeMap::new();
        let mut tasks = Vec::new();
        for (key, list) in bindings {
            let (port, proto) = super::model::split_port(&key);
            if proto != "tcp" {
                // UDP is not published (yet); the port stays reachable on
                // the container's address.
                continue;
            }
            let mut done = Vec::new();
            for (host_ip, host_port) in list {
                if host_ip.contains(':') {
                    continue;
                }
                let listener = match self.publisher.bind(host_port) {
                    Ok(l) => l,
                    Err(e) => {
                        for t in &tasks {
                            tokio::task::JoinHandle::abort(t);
                        }
                        return Err(e);
                    }
                };
                let bound = listener.local_addr()?.port();
                let target = match (rec.network_mode().as_str(), target_ip) {
                    ("host", _) | (_, None) => std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                    (_, Some(ip)) => std::net::SocketAddr::from((ip, port)),
                };
                tasks.push(tokio::spawn(dnet::proxy(listener, target)));
                if !done.iter().any(|(_, p): &(String, String)| *p == bound.to_string()) {
                    done.push(("127.0.0.1".to_string(), bound.to_string()));
                }
            }
            out.insert(key, done);
        }
        ctr.rt.lock().unwrap().port_tasks = tasks;
        Ok(out)
    }

    fn unpublish(&self, ctr: &Ctr) {
        for t in ctr.rt.lock().unwrap().port_tasks.drain(..) {
            t.abort();
        }
    }

    pub fn refresh_published(&self) {
        let mut ports: Vec<u16> = self
            .list()
            .iter()
            .map(|c| c.record())
            .filter(|r| r.state.running())
            .flat_map(|r| r.state.ports.values().flatten().filter_map(|(_, p)| p.parse().ok()).collect::<Vec<u16>>())
            .collect();
        ports.sort();
        ports.dedup();
        self.published.send_if_modified(|v| {
            if *v != ports {
                *v = ports;
                true
            } else {
                false
            }
        });
    }

    /// Copy the image's files into empty volumes mounted over them.
    fn populate_volumes(&self, rec: &Record, rootfs: &Path) {
        for m in rec.mounts.iter().filter(|m| m.kind == "volume" && !m.nocopy) {
            let src = secure_join(rootfs, &m.destination);
            let dst = PathBuf::from(&m.source);
            let empty = std::fs::read_dir(&dst).map(|mut d| d.next().is_none()).unwrap_or(true);
            if !empty || !src.is_dir() {
                continue;
            }
            let _ = std::fs::create_dir_all(&dst);
            for e in std::fs::read_dir(&src).into_iter().flatten().flatten() {
                let _ = copy_tree(&e.path(), &dst.join(e.file_name()));
            }
            // The volume root takes the image directory's owner and mode.
            if let Ok(meta) = std::fs::metadata(&src) {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                if let Ok(c) = std::ffi::CString::new(dst.as_os_str().as_encoded_bytes()) {
                    // SAFETY: valid string.
                    unsafe { libc::chown(c.as_ptr(), meta.uid(), meta.gid()) };
                }
                let _ = std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(meta.mode() & 0o7777));
            }
        }
    }

    pub async fn start_container(self: &Arc<Self>, key: &str) -> ApiResult<()> {
        let ctr = self.get(key)?;
        let _op = ctr.op.lock().await;
        let rec = ctr.record();
        if rec.state.status == "running" || rec.state.status == "paused" {
            return Err(ApiError::not_modified());
        }
        if rec.state.status == "removing" {
            return Err(ApiError::conflict(format!("container {} is marked for removal and cannot be started", rec.id)));
        }
        self.update(&ctr, |r| r.manually_stopped = false);
        match self.start_locked(&ctr).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let msg = format!("{e:#}");
                self.update(&ctr, |r| {
                    r.state.status = if r.state.started_at == ZERO_TIME { "created".into() } else { "exited".into() };
                    r.state.exit_code = 128;
                    r.state.error = msg.clone();
                });
                Err(ApiError::new(500, msg))
            }
        }
    }

    async fn start_locked(self: &Arc<Self>, ctr: &Arc<Ctr>) -> Result<()> {
        let dir = self.container_dir(&ctr.id);
        let rec = ctr.record();
        // Leftovers of the previous run.
        let _ = std::fs::remove_file(dir.join("exit.json"));
        let _ = std::fs::remove_file(dir.join("shim.pid"));
        let _ = std::fs::remove_file(dir.join("init.pid"));
        sys::unmount(&dir.join("rootfs"));
        let _ = tokio::process::Command::new(&self.crun).arg("--root").arg(&self.crun_root).args(["delete", "-f", &ctr.id]).output().await;
        for d in ["upper", "work", "rootfs"] {
            std::fs::create_dir_all(dir.join(d))?;
        }
        // Volumes and binds must exist; named volumes might have been
        // removed with the container stopped.
        for m in &rec.mounts {
            if m.kind == "volume" {
                if let Some(name) = &m.name {
                    if self.volume(name).is_none() {
                        self.create_volume(Some(name), BTreeMap::new(), BTreeMap::new(), m.anonymous).map_err(|e| anyhow::anyhow!("{e}"))?;
                    }
                }
                std::fs::create_dir_all(&m.source)?;
            }
        }
        let options = self.overlay_options(&rec)?;
        sys::mount_overlay(&dir.join("rootfs"), &options)?;
        let started = async {
            let rootfs = dir.join("rootfs");
            self.populate_volumes(&rec, &rootfs);
            if let Some(wd) = rec.config["WorkingDir"].as_str().filter(|w| w.starts_with('/')) {
                let _ = std::fs::create_dir_all(secure_join(&rootfs, wd));
            }
            let netns = self.network_up(ctr).await?;
            let rec = ctr.record();
            let share_from = rec.network_mode().strip_prefix("container:").and_then(|t| self.find(t)).map(|c| c.record());
            self.etc_files(&rec, &dir, share_from.as_ref())?;
            let pid = match rec.host_config["PidMode"].as_str().unwrap_or_default() {
                "host" => cspec::PidNamespace::Host,
                m if m.starts_with("container:") => {
                    let other = self.find(&m["container:".len()..]).context("PID namespace container not found")?;
                    let p = other.record().state.pid;
                    cspec::PidNamespace::Shared(format!("/proc/{p}/ns/pid"))
                }
                _ => cspec::PidNamespace::New,
            };
            let own_uts = rec.network_mode() != "host";
            let mut config = self.bundle(&rec, &dir, netns.clone(), own_uts, pid)?;
            // The container's own IPC namespace, kept as a file like the
            // network one (so `docker exec` and archive helpers can join).
            let ipc = format!("{RUN_DIR}/ipc-{}", &rec.id[..12]);
            if netns.is_some() && sys::is_ns_file(Path::new(&ipc)) && !rec.network_mode().starts_with("container:") {
                if let Some(ns) = config["linux"]["namespaces"].as_array_mut() {
                    ns.retain(|n| n["type"] != "ipc");
                    ns.push(json!({"type": "ipc", "path": ipc}));
                }
            }
            std::fs::write(dir.join("config.json"), serde_json::to_vec(&config)?)?;
            // Published ports bind before the process starts, so a port that
            // is taken fails the start (as with Docker).
            let ports = self.publish_ports(ctr)?;
            let pending = ctr.rt.lock().unwrap().pending_attach;
            let mut cmd = tokio::process::Command::new(if cfg!(target_os = "linux") { "/proc/self/exe" } else { "litek8s" });
            cmd.arg0("litek8s")
                .arg("crun-shim")
                .args(["--id", &rec.id, "--bundle"])
                .arg(&dir)
                .arg("--crun")
                .arg(&self.crun)
                .arg("--root")
                .arg(&self.crun_root)
                .arg("--shim-cgroup")
                .arg(Path::new(CGROUP_ROOT).join("litek8s/shims"))
                .arg("--cgroup")
                .arg(Path::new(CGROUP_ROOT).join(self.cgroup(&rec.id).trim_start_matches('/')))
                .arg("--keep-rootfs")
                .arg("--attach")
                .arg(self.attach_socket(&rec.id))
                .arg("--attach-wait")
                .arg(pending.to_string());
            if rec.tty() {
                cmd.arg("--tty").arg("--console").arg(Path::new(RUN_DIR).join(format!("{}.console", &rec.id[..16])));
            }
            if rec.config["OpenStdin"].as_bool().unwrap_or(false) {
                cmd.arg("--stdin");
                if rec.config["StdinOnce"].as_bool().unwrap_or(false) {
                    cmd.arg("--stdin-once");
                }
            }
            cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
            let child = cmd.spawn().context("failed to start the container shim")?;
            {
                let mut rt = ctr.rt.lock().unwrap();
                rt.pending_attach = 0;
                rt.attach_gen += 1;
            }
            ctr.bump();
            let out = child.wait_with_output().await.context("container shim")?;
            if !out.status.success() {
                self.unpublish(ctr);
                bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
            }
            Ok::<_, anyhow::Error>(ports)
        }
        .await;
        let ports = match started {
            Ok(p) => p,
            Err(e) => {
                self.unpublish(ctr);
                self.network_down(ctr);
                sys::unmount(&dir.join("rootfs"));
                self.update(ctr, |r| r.state.netns = None);
                return Err(e);
            }
        };
        let shim_pid: Option<i32> = std::fs::read_to_string(dir.join("shim.pid")).ok().and_then(|p| p.trim().parse().ok());
        let init_pid: i64 = std::fs::read_to_string(dir.join("init.pid")).ok().and_then(|p| p.trim().parse().ok()).unwrap_or(0);
        let health = self.health_config(&ctr.record()).map(|_| Health { status: "starting".into(), failing_streak: 0, log: Vec::new() });
        self.update(ctr, |r| {
            r.state.status = "running".into();
            r.state.pid = init_pid;
            r.state.exit_code = 0;
            r.state.error = String::new();
            r.state.oom_killed = false;
            r.state.started_at = std::fs::read_to_string(dir.join("started")).ok().map(|s| s.trim().to_string()).unwrap_or_else(now_rfc3339);
            r.state.finished_at = ZERO_TIME.into();
            r.state.ports = ports;
            r.state.health = health;
        });
        {
            let mut rt = ctr.rt.lock().unwrap();
            rt.shim_pid = shim_pid;
            rt.started = Some(std::time::Instant::now());
        }
        self.refresh_dns(ctr);
        self.refresh_published();
        if let Some((rows, cols)) = ctr.rt.lock().unwrap().console_size {
            let sock = self.attach_socket(&ctr.id);
            tokio::spawn(async move { super::attach::send_resize(&sock, rows, cols).await });
        }
        let rec = ctr.record();
        self.emit_container(&rec, "start", &[]);
        match shim_pid {
            Some(pid) => self.watch_exit(ctr.clone(), pid),
            None => {
                let e = self.clone();
                let c = ctr.clone();
                tokio::spawn(async move { e.on_exit(&c).await });
            }
        }
        self.start_health(ctr);
        Ok(())
    }

    /// `start_locked` behind a box (it is reached from its own exit
    /// handler through restart policies).
    fn start_boxed(self: Arc<Self>, ctr: Arc<Ctr>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> {
        Box::pin(async move { self.start_locked(&ctr).await })
    }

    fn watch_exit(self: &Arc<Self>, ctr: Arc<Ctr>, pid: i32) {
        let engine = self.clone();
        tokio::spawn(async move {
            #[cfg(target_os = "linux")]
            match sys::pidfd_open(pid) {
                Ok(fd) => {
                    if let Ok(afd) = tokio::io::unix::AsyncFd::with_interest(fd, tokio::io::Interest::READABLE) {
                        let _ = afd.readable().await;
                    }
                }
                Err(_) => {
                    while pid_alive(pid, &ctr.id) {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
            #[cfg(not(target_os = "linux"))]
            let _ = pid;
            engine.on_exit(&ctr).await;
        });
    }

    /// The container's process ended: record it, release what it held,
    /// apply the restart policy.
    async fn on_exit(self: &Arc<Self>, ctr: &Arc<Ctr>) {
        let dir = self.container_dir(&ctr.id);
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ExitRecord {
            exit_code: i64,
            finished_at: String,
            #[serde(default)]
            oom_killed: bool,
        }
        let exit: Option<ExitRecord> = read_json(&dir.join("exit.json"));
        if let Some(h) = ctr.rt.lock().unwrap().health_task.take() {
            h.abort();
        }
        self.unpublish(ctr);
        self.network_down(ctr);
        sys::unmount(&dir.join("rootfs"));
        let _ = std::fs::remove_dir(Path::new(CGROUP_ROOT).join(self.cgroup(&ctr.id).trim_start_matches('/')));
        let (code, finished, oom) = match exit {
            Some(e) => (e.exit_code, e.finished_at, e.oom_killed),
            None => (255, now_rfc3339(), false),
        };
        let rec_before = ctr.record();
        let (policy, max) = rec_before.restart_policy();
        let ran_for = ctr.rt.lock().unwrap().started.map(|s| s.elapsed()).unwrap_or_default();
        let removing = rec_before.state.status == "removing";
        let restart = !removing
            && !rec_before.manually_stopped
            && match policy.as_str() {
                "always" | "unless-stopped" => true,
                "on-failure" => code != 0 && (max <= 0 || (rec_before.restart_count as i64) < max),
                _ => false,
            };
        self.update(ctr, |r| {
            if !removing {
                r.state.status = if restart { "restarting".into() } else { "exited".into() };
            }
            r.state.exit_code = code;
            r.state.finished_at = finished.clone();
            r.state.oom_killed = oom;
            r.state.pid = 0;
            r.state.netns = None;
            r.state.ports.clear();
            if let Some(h) = r.state.health.as_mut() {
                h.status = "unhealthy".into();
            }
            r.state.health = None;
            for e in r.endpoints.iter_mut() {
                e.ip = None;
            }
        });
        {
            let mut rt = ctr.rt.lock().unwrap();
            rt.shim_pid = None;
            rt.exits += 1;
        }
        ctr.bump();
        self.refresh_published();
        let rec = ctr.record();
        if oom {
            self.emit_container(&rec, "oom", &[]);
        }
        self.emit_container(&rec, "die", &[("exitCode", code.to_string())]);
        if removing {
            return;
        }
        if rec.host_config["AutoRemove"].as_bool().unwrap_or(false) && !restart {
            let e = self.clone();
            let id = ctr.id.clone();
            tokio::spawn(async move {
                let _ = e.remove_container(&id, true, true).await;
            });
            return;
        }
        if restart {
            let delay = {
                let mut rt = ctr.rt.lock().unwrap();
                rt.restart_delay = if ran_for > Duration::from_secs(10) || rt.restart_delay.is_zero() {
                    Duration::from_millis(100)
                } else {
                    (rt.restart_delay * 2).min(Duration::from_secs(60))
                };
                rt.restart_delay
            };
            let e = self.clone();
            let c = ctr.clone();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _op = c.op.lock().await;
                let r = c.record();
                if r.state.status != "restarting" || r.manually_stopped {
                    return;
                }
                e.update(&c, |r| r.restart_count += 1);
                if let Err(err) = e.clone().start_boxed(c.clone()).await {
                    let msg = format!("{err:#}");
                    e.update(&c, |r| {
                        r.state.status = "exited".into();
                        r.state.error = msg;
                    });
                }
            });
        }
    }

    // -----------------------------------------------------------------
    // Stop, kill, remove, wait

    async fn signal(&self, id: &str, signal: &str) -> Result<()> {
        let sig = signal.trim_start_matches("SIG").to_string();
        run_quiet(&self.crun.to_string_lossy(), &["--root", &self.crun_root.to_string_lossy(), "kill", id, &sig], None).await
    }

    async fn wait_not_running(&self, ctr: &Ctr, timeout: Duration) -> bool {
        let mut rx = ctr.changed.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if !matches!(ctr.rec.lock().unwrap().state.status.as_str(), "running" | "paused") {
                return true;
            }
            if tokio::time::timeout_at(deadline, rx.changed()).await.is_err() {
                return false;
            }
        }
    }

    pub async fn stop_container(self: &Arc<Self>, key: &str, timeout: Option<i64>, signal: Option<String>) -> ApiResult<()> {
        let ctr = self.get(key)?;
        let _op = ctr.op.lock().await;
        self.update(&ctr, |r| r.manually_stopped = true);
        let rec = ctr.record();
        if rec.state.status == "restarting" {
            self.update(&ctr, |r| r.state.status = "exited".into());
            return Ok(());
        }
        if !rec.state.running() {
            return Err(ApiError::not_modified());
        }
        self.stop_locked(&ctr, timeout, signal).await;
        Ok(())
    }

    async fn stop_locked(&self, ctr: &Ctr, timeout: Option<i64>, signal: Option<String>) {
        let rec = ctr.record();
        if rec.state.status == "paused" {
            let _ = run_quiet(&self.crun.to_string_lossy(), &["--root", &self.crun_root.to_string_lossy(), "resume", &ctr.id], None).await;
        }
        let signal = signal.filter(|s| !s.is_empty()).or_else(|| rec.config["StopSignal"].as_str().filter(|s| !s.is_empty()).map(str::to_string)).unwrap_or_else(|| "TERM".into());
        let timeout = timeout.or_else(|| rec.config["StopTimeout"].as_i64()).unwrap_or(10);
        let _ = self.signal(&ctr.id, &signal).await;
        if timeout < 0 || !self.wait_not_running(ctr, Duration::from_secs(timeout as u64)).await {
            let _ = self.signal(&ctr.id, "KILL").await;
            self.wait_not_running(ctr, Duration::from_secs(10)).await;
        }
    }

    pub async fn kill_container(&self, key: &str, signal: Option<String>) -> ApiResult<()> {
        let ctr = self.get(key)?;
        let rec = ctr.record();
        if !rec.state.running() {
            return Err(ApiError::conflict(format!("Cannot kill container: {}: container {} is not running", key, rec.id)));
        }
        let sig = signal.filter(|s| !s.is_empty()).unwrap_or_else(|| "KILL".into());
        let sig = match sig.parse::<i32>() {
            Ok(n) => n.to_string(),
            Err(_) => sig,
        };
        if matches!(sig.trim_start_matches("SIG"), "KILL" | "9") {
            self.update(&ctr, |r| r.manually_stopped = true);
        }
        self.signal(&ctr.id, &sig).await?;
        self.emit_container(&rec, "kill", &[("signal", sig.clone())]);
        Ok(())
    }

    pub async fn restart_container(self: &Arc<Self>, key: &str, timeout: Option<i64>, signal: Option<String>) -> ApiResult<()> {
        let ctr = self.get(key)?;
        {
            let _op = ctr.op.lock().await;
            if ctr.record().state.running() {
                self.update(&ctr, |r| r.manually_stopped = true);
                self.stop_locked(&ctr, timeout, signal).await;
            }
        }
        match self.start_container(key).await {
            Err(e) if e.status == 304 => Ok(()),
            other => other,
        }?;
        let rec = ctr.record();
        self.emit_container(&rec, "restart", &[]);
        Ok(())
    }

    pub async fn pause(&self, key: &str, pause: bool) -> ApiResult<()> {
        let ctr = self.get(key)?;
        let rec = ctr.record();
        if pause && rec.state.status != "running" {
            return Err(ApiError::conflict(format!("container {} is not running", rec.id)));
        }
        if !pause && rec.state.status != "paused" {
            return Err(ApiError::conflict(format!("container {} is not paused", rec.id)));
        }
        run_quiet(&self.crun.to_string_lossy(), &["--root", &self.crun_root.to_string_lossy(), if pause { "pause" } else { "resume" }, &ctr.id], None).await?;
        self.update(&ctr, |r| r.state.status = if pause { "paused".into() } else { "running".into() });
        self.emit_container(&rec, if pause { "pause" } else { "unpause" }, &[]);
        Ok(())
    }

    pub async fn remove_container(self: &Arc<Self>, key: &str, force: bool, volumes: bool) -> ApiResult<()> {
        let ctr = self.get(key)?;
        let _op = ctr.op.lock().await;
        let rec = ctr.record();
        if rec.state.running() && rec.state.status != "restarting" {
            if !force {
                return Err(ApiError::conflict(format!(
                    "cannot remove container \"/{}\": container is running: stop the container before removing or force remove",
                    rec.name
                )));
            }
            self.update(&ctr, |r| {
                r.manually_stopped = true;
                r.state.status = "removing".into();
            });
            let _ = self.signal(&ctr.id, "KILL").await;
            // on_exit leaves a removing container alone; wait for the shim.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while ctr.rt.lock().unwrap().shim_pid.is_some_and(|p| pid_alive(p, &ctr.id)) && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        } else {
            self.update(&ctr, |r| {
                r.manually_stopped = true;
                r.state.status = "removing".into();
            });
        }
        self.unpublish(&ctr);
        self.network_down(&ctr);
        let dir = self.container_dir(&ctr.id);
        sys::unmount(&dir.join("rootfs"));
        let _ = tokio::process::Command::new(&self.crun).arg("--root").arg(&self.crun_root).args(["delete", "-f", &ctr.id]).output().await;
        let _ = std::fs::remove_dir(Path::new(CGROUP_ROOT).join(self.cgroup(&ctr.id).trim_start_matches('/')));
        let _ = std::fs::remove_file(self.attach_socket(&ctr.id));
        let removed = tokio::task::spawn_blocking({
            let dir = dir.clone();
            move || std::fs::remove_dir_all(&dir)
        })
        .await;
        if let Ok(Err(e)) = removed {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(ApiError::new(500, format!("unable to remove filesystem for {}: {e}", rec.id)));
            }
        }
        self.containers.write().unwrap().remove(&ctr.id);
        if volumes {
            for m in rec.mounts.iter().filter(|m| m.anonymous) {
                if let Some(n) = &m.name {
                    if self.volume_users(n).is_empty() {
                        let _ = self.remove_volume(n, false);
                    }
                }
            }
        }
        self.refresh_published();
        self.emit_container(&rec, "destroy", &[]);
        ctr.changed.send_modify(|v| *v = u64::MAX);
        Ok(())
    }

    /// `POST /containers/{id}/wait`: (exit code, error).
    pub async fn wait_container(&self, key: &str, condition: &str) -> ApiResult<(i64, Option<String>)> {
        let ctr = self.get(key)?;
        let mut rx = ctr.changed.subscribe();
        let exits_at_call = ctr.rt.lock().unwrap().exits;
        loop {
            let rec = ctr.record();
            let removed = *rx.borrow() == u64::MAX;
            let exits = ctr.rt.lock().unwrap().exits;
            let done = removed
                || match condition {
                    "removed" => false,
                    "next-exit" => exits > exits_at_call && !rec.state.running(),
                    _ => !rec.state.running() && rec.state.status != "removing",
                };
            if done {
                let err = (!rec.state.error.is_empty()).then(|| rec.state.error.clone());
                return Ok((rec.state.exit_code, err));
            }
            if rx.changed().await.is_err() {
                let rec = ctr.record();
                return Ok((rec.state.exit_code, None));
            }
        }
    }

    pub fn rename(&self, key: &str, new_name: &str) -> ApiResult<()> {
        let ctr = self.get(key)?;
        let new_name = new_name.trim_start_matches('/');
        if !valid_name(new_name) {
            return Err(ApiError::bad_request(format!("Invalid container name ({new_name})")));
        }
        if self.find(new_name).is_some_and(|c| c.id != ctr.id) {
            return Err(ApiError::conflict(format!("Conflict. The container name \"/{new_name}\" is already in use")));
        }
        let old = ctr.record().name;
        self.update(&ctr, |r| r.name = new_name.to_string());
        self.refresh_dns(&ctr);
        let rec = ctr.record();
        self.emit_container(&rec, "rename", &[("oldName", format!("/{old}"))]);
        Ok(())
    }

    pub fn resize(&self, key: &str, rows: u16, cols: u16) -> ApiResult<()> {
        let ctr = self.get(key)?;
        ctr.rt.lock().unwrap().console_size = Some((rows, cols));
        if ctr.record().state.running() {
            let sock = self.attach_socket(&ctr.id);
            tokio::spawn(async move { super::attach::send_resize(&sock, rows, cols).await });
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Startup recovery

    async fn load_containers(self: &Arc<Self>) {
        let mut loaded = Vec::new();
        for e in std::fs::read_dir(self.root.join("containers")).into_iter().flatten().flatten() {
            let Some(rec) = read_json::<Record>(&e.path().join("container.json")) else { continue };
            let ctr = Arc::new(Ctr { id: rec.id.clone(), rec: Mutex::new(rec), op: tokio::sync::Mutex::new(()), changed: watch::channel(0).0, rt: Mutex::new(Runtime::default()) });
            self.containers.write().unwrap().insert(ctr.id.clone(), ctr.clone());
            loaded.push(ctr);
        }
        for ctr in loaded {
            let rec = ctr.record();
            let dir = self.container_dir(&ctr.id);
            if rec.state.status == "removing" {
                let _ = self.remove_container(&ctr.id, true, false).await;
                continue;
            }
            if !rec.state.running() {
                continue;
            }
            let shim: Option<i32> = std::fs::read_to_string(dir.join("shim.pid")).ok().and_then(|p| p.trim().parse().ok()).filter(|p| pid_alive(*p, &ctr.id));
            match shim.filter(|_| !dir.join("exit.json").exists()) {
                Some(pid) => {
                    // The engine restarted, the container did not.
                    ctr.rt.lock().unwrap().shim_pid = Some(pid);
                    if let Ok(ports) = self.publish_ports(&ctr) {
                        self.update(&ctr, |r| r.state.ports = ports);
                    }
                    self.refresh_dns(&ctr);
                    self.watch_exit(ctr.clone(), pid);
                    self.start_health(&ctr);
                }
                None => {
                    self.network_down(&ctr);
                    sys::unmount(&dir.join("rootfs"));
                    let (policy, _) = rec.restart_policy();
                    let restart = !rec.manually_stopped && matches!(policy.as_str(), "always" | "unless-stopped");
                    self.update(&ctr, |r| {
                        r.state.status = "exited".into();
                        r.state.exit_code = if dir.join("exit.json").exists() { r.state.exit_code } else { 255 };
                        r.state.finished_at = now_rfc3339();
                        r.state.pid = 0;
                        r.state.netns = None;
                        r.state.ports.clear();
                        r.state.health = None;
                    });
                    if restart {
                        let e = self.clone();
                        let id = ctr.id.clone();
                        tokio::spawn(async move {
                            if let Err(err) = e.start_container(&id).await {
                                eprintln!("docker: restarting {id}: {err}");
                            }
                        });
                    }
                }
            }
        }
        self.refresh_published();
    }

    // -----------------------------------------------------------------
    // Health checks

    pub fn health_config(&self, rec: &Record) -> Option<Value> {
        let h = &rec.config["Healthcheck"];
        let test = strings(&h["Test"]);
        if test.is_empty() || test[0] == "NONE" {
            return None;
        }
        Some(h.clone())
    }

    fn start_health(self: &Arc<Self>, ctr: &Arc<Ctr>) {
        let rec = ctr.record();
        let Some(h) = self.health_config(&rec) else { return };
        let engine = self.clone();
        let c = ctr.clone();
        let task = tokio::spawn(async move {
            let test = strings(&h["Test"]);
            let argv: Vec<String> = match test[0].as_str() {
                "CMD" => test[1..].to_vec(),
                "CMD-SHELL" => vec!["/bin/sh".into(), "-c".into(), test[1..].join(" ")],
                _ => test.clone(),
            };
            let interval = nanos(&h["Interval"], Duration::from_secs(30));
            let timeout = nanos(&h["Timeout"], Duration::from_secs(30));
            let start_period = nanos(&h["StartPeriod"], Duration::ZERO);
            let start_interval = nanos(&h["StartInterval"], Duration::from_secs(5));
            let retries = h["Retries"].as_u64().filter(|r| *r > 0).unwrap_or(3);
            let begun = std::time::Instant::now();
            loop {
                let in_start = begun.elapsed() < start_period;
                let starting = c.record().state.health.as_ref().is_some_and(|h| h.status == "starting");
                tokio::time::sleep(if in_start && starting { start_interval } else { interval }).await;
                if !c.record().state.running() {
                    return;
                }
                if c.record().state.status == "paused" {
                    continue;
                }
                let start = now_rfc3339();
                let (code, output) = match super::exec::run_capture(&engine, &c, argv.clone(), timeout).await {
                    Ok(r) => r,
                    Err(e) => (-1, format!("{e:#}")),
                };
                let end = now_rfc3339();
                let in_start = begun.elapsed() < start_period;
                let mut changed_to = None;
                engine.update(&c, |r| {
                    let Some(hs) = r.state.health.as_mut() else { return };
                    hs.log.push(HealthLog { start, end, exit_code: code, output });
                    if hs.log.len() > 5 {
                        hs.log.remove(0);
                    }
                    let before = hs.status.clone();
                    if code == 0 {
                        hs.failing_streak = 0;
                        hs.status = "healthy".into();
                    } else if !(in_start && hs.status == "starting") {
                        hs.failing_streak += 1;
                        if hs.failing_streak >= retries {
                            hs.status = "unhealthy".into();
                        }
                    }
                    if hs.status != before {
                        changed_to = Some(hs.status.clone());
                    }
                });
                let rec = c.record();
                if let Some(status) = changed_to {
                    engine.emit_container(&rec, &format!("health_status: {status}"), &[]);
                }
            }
        });
        ctr.rt.lock().unwrap().health_task = Some(task);
    }

    // -----------------------------------------------------------------
    // Logs

    /// Log records of a container: (stream 1/2, bytes) chunks.
    pub fn logs(self: &Arc<Self>, ctr: Arc<Ctr>, opts: LogOpts) -> futures_util::stream::BoxStream<'static, (u8, Vec<u8>)> {
        use futures_util::StreamExt;
        struct St {
            dir: PathBuf,
            ctr: Arc<Ctr>,
            opts: LogOpts,
            offset: Option<u64>,
            pending: VecDeque<(u8, Vec<u8>)>,
            done: bool,
            idle: u32,
        }
        let st = St { dir: self.container_dir(&ctr.id), ctr, opts, offset: None, pending: VecDeque::new(), done: false, idle: 0 };
        futures_util::stream::unfold(st, |mut st| async move {
            loop {
                if let Some(c) = st.pending.pop_front() {
                    return Some((c, st));
                }
                if st.done {
                    return None;
                }
                match st.offset {
                    None => {
                        let mut data = std::fs::read(st.dir.join("log.1")).unwrap_or_default();
                        let old = data.len();
                        data.extend(std::fs::read(st.dir.join("log")).unwrap_or_default());
                        let complete = data.iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
                        let start = st.opts.tail.map_or(0, |n| clogs::tail_offset(&data[..complete], n));
                        render(&st.opts, &data[start..complete], &mut st.pending);
                        st.offset = Some(complete.saturating_sub(old) as u64);
                        st.done = !st.opts.follow || !st.ctr.record().state.running();
                    }
                    Some(offset) => {
                        let running = st.ctr.record().state.running() && st.dir.exists();
                        let data = std::fs::read(st.dir.join("log")).unwrap_or_default();
                        let offset = if (data.len() as u64) < offset { 0 } else { offset as usize };
                        let fresh = &data[offset..];
                        let complete = fresh.iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
                        render(&st.opts, &fresh[..complete], &mut st.pending);
                        st.offset = Some((offset + complete) as u64);
                        if st.pending.is_empty() {
                            if !running {
                                // One last read after the shim's final flush.
                                st.idle += 1;
                                if st.idle > 2 {
                                    return None;
                                }
                            }
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        } else {
                            st.idle = 0;
                        }
                    }
                }
            }
        })
        .boxed()
    }
}

#[derive(Clone, Debug, Default)]
pub struct LogOpts {
    pub follow: bool,
    pub stdout: bool,
    pub stderr: bool,
    pub timestamps: bool,
    pub tail: Option<usize>,
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    pub until: Option<chrono::DateTime<chrono::Utc>>,
}

/// CRI records -> Docker log chunks (partial records joined).
fn render(opts: &LogOpts, data: &[u8], out: &mut VecDeque<(u8, Vec<u8>)>) {
    let mut partial: Option<(u8, Vec<u8>, String)> = None;
    for line in data.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Some(r) = clogs::parse(line) else { continue };
        let time = super::model::parse_time(r.time);
        if let (Some(since), Some(t)) = (opts.since, time) {
            if t < since {
                continue;
            }
        }
        if let (Some(until), Some(t)) = (opts.until, time) {
            if t > until {
                continue;
            }
        }
        if (r.stream == 1 && !opts.stdout) || (r.stream == 2 && !opts.stderr) {
            continue;
        }
        let entry = partial.get_or_insert_with(|| (r.stream, Vec::new(), r.time.to_string()));
        entry.1.extend_from_slice(r.content);
        if r.full {
            let (stream, mut content, time) = partial.take().unwrap();
            content.push(b'\n');
            if opts.timestamps {
                let mut with = time.into_bytes();
                with.push(b' ');
                with.extend(content);
                content = with;
            }
            out.push_back((stream, content));
        }
    }
    if let Some((stream, content, _)) = partial {
        out.push_back((stream, content));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_like_docker() {
        let image = json!({"Env": ["PATH=/usr/bin", "A=image"], "Cmd": ["nginx"], "Entrypoint": ["/e.sh"], "WorkingDir": "/app", "Labels": {"x": "1"}, "ExposedPorts": {"80/tcp": {}}});
        let c = merge_config(&json!({"Image": "nginx", "Env": ["A=mine"], "Labels": {"y": "2"}}), &image);
        assert_eq!(c["Env"], json!(["PATH=/usr/bin", "A=mine"]));
        assert_eq!(c["Cmd"], json!(["nginx"]));
        assert_eq!(c["Entrypoint"], json!(["/e.sh"]));
        assert_eq!(c["Labels"], json!({"x": "1", "y": "2"}));
        assert_eq!(c["WorkingDir"], "/app");
        // A command replaces Cmd only.
        let c = merge_config(&json!({"Cmd": ["sh"]}), &image);
        assert_eq!((c["Entrypoint"].clone(), c["Cmd"].clone()), (json!(["/e.sh"]), json!(["sh"])));
        // An entrypoint drops the image's Cmd.
        let c = merge_config(&json!({"Entrypoint": ["/bin/sh"]}), &image);
        assert_eq!((c["Entrypoint"].clone(), c["Cmd"].clone()), (json!(["/bin/sh"]), Value::Null));
        // --entrypoint "" resets it.
        let c = merge_config(&json!({"Entrypoint": [""], "Cmd": ["ls"]}), &image);
        assert_eq!((c["Entrypoint"].clone(), c["Cmd"].clone()), (Value::Null, json!(["ls"])));
    }

    #[test]
    fn secure_join_stays_inside() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("real")).unwrap();
        std::os::unix::fs::symlink("/real", root.path().join("abs")).unwrap();
        std::os::unix::fs::symlink("../../..", root.path().join("real/up")).unwrap();
        assert_eq!(secure_join(root.path(), "/abs/x"), root.path().join("real/x"));
        assert_eq!(secure_join(root.path(), "/real/up/etc"), root.path().join("etc"));
        assert_eq!(secure_join(root.path(), "../../etc/passwd"), root.path().join("etc/passwd"));
    }

    #[test]
    fn renders_logs() {
        let data = b"2026-01-01T00:00:01.000000000Z stdout F one\n2026-01-01T00:00:02.000000000Z stderr P tw\n2026-01-01T00:00:02.100000000Z stderr F o\n";
        let mut out = VecDeque::new();
        render(&LogOpts { stdout: true, stderr: true, ..Default::default() }, data, &mut out);
        assert_eq!(out, VecDeque::from(vec![(1, b"one\n".to_vec()), (2, b"two\n".to_vec())]));
        let mut out = VecDeque::new();
        render(&LogOpts { stdout: true, timestamps: true, ..Default::default() }, data, &mut out);
        assert_eq!(out, VecDeque::from(vec![(1, b"2026-01-01T00:00:01.000000000Z one\n".to_vec())]));
        assert_eq!(parse_size("64m"), Some(64 << 20));
        assert_eq!(parse_size("1g"), Some(1 << 30));
    }
}
