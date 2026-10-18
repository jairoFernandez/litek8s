//! HTTP front end of the Kubernetes-compatible API server.
//!
//! Serves discovery, the generic object API (get/list/watch/create/update/
//! patch/delete/deletecollection) for every registered resource type, the
//! `status` and `scale` subresources, the streaming pod subresources (`log`,
//! `exec`, `attach`, `portforward`, see `pod_streams`), and the node agent
//! connection endpoint. Objects are stored as JSON in [`Store`]; the
//! controllers and node agents act on them.
//!
//! Not yet: authentication and authorization (any client is admin), schema
//! validation, managedFields, and admission webhooks for writes made by the
//! in-process controllers.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rand_core::{OsRng, RngCore};
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};

use super::admission;
use super::agent::AgentHub;
use super::defaults;
use super::discovery;
use super::patch;
use super::protobuf;
use super::registry::{Registry, ResourceType};
use super::selector::{FieldSelector, LabelSelector};
use super::store::{ObjectKey, Store, StoreError, WatchEvent, resource_version};
use super::table;
use super::tls;

pub(crate) type Body = UnsyncBoxBody<Bytes, std::io::Error>;

const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_WATCH_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const SERVICE_CIDR_PREFIX: &str = "10.43";
const KUBERNETES_SERVICE_IP: &str = "10.43.0.1";

pub struct ApiServerConfig {
    pub listen: SocketAddr,
    /// Where the object WAL lives; `None` keeps everything in memory.
    pub data_dir: Option<PathBuf>,
    /// Write a kubeconfig pointing at this server here.
    pub kubeconfig_out: Option<PathBuf>,
    /// Log one line per request to stderr.
    pub log_requests: bool,
    /// Run this machine as a node (controllers always run).
    pub run_node: bool,
    /// The local node's name (default: the hostname).
    pub node_name: Option<String>,
    /// Where the local node agent keeps pod files.
    pub node_dir: Option<PathBuf>,
    /// UDP address for cluster DNS (pods use port 53); `None` disables it.
    pub cluster_dns: Option<SocketAddr>,
    /// Also serve the API on this port of the listen address, so pods reach
    /// it at `https://kubernetes.default.svc` (port 443) as in Kubernetes.
    pub in_cluster_port: Option<u16>,
    /// The container runtime of the local node.
    pub runtime: super::agent::RuntimeChoice,
    /// Serve on this already-bound listener instead of binding `listen`
    /// (the dev sandbox binds it on the host before entering its network
    /// namespace).
    pub listener: Option<std::net::TcpListener>,
}

/// Loads an image archive into the local node (`litek8s dev load`).
pub(crate) struct ImageLoader {
    /// Where uploads are staged (on the image store's filesystem).
    pub staging: PathBuf,
    pub load: Box<dyn Fn(PathBuf) -> futures_util::future::BoxFuture<'static, Result<Vec<String>>> + Send + Sync>,
}

pub struct ApiState {
    store: Store,
    registry: Registry,
    server_address: String,
    log_requests: bool,
    /// Node agents connected to this server; pod log/exec/port-forward
    /// requests are relayed through them.
    pub(crate) hub: Arc<AgentHub>,
    /// Cluster DNS is being served (pods get it as their resolver).
    pub(crate) cluster_dns: std::sync::atomic::AtomicBool,
    /// Serves `https://` on the same port when set (in-cluster clients).
    tls: Option<tls::ServerTls>,
    /// ClusterIPs and nodePorts handed out but maybe not stored yet. Helm
    /// creates a chart's Services concurrently; each allocation looks at the
    /// stored Services, so without this two could get the same address.
    allocated: std::sync::Mutex<Vec<(String, std::time::Instant)>>,
    /// Imports image archives into the local node, when there is one.
    pub(crate) image_loader: std::sync::OnceLock<ImageLoader>,
}

impl ApiState {
    pub fn open(data_dir: Option<&Path>, server_address: String, log_requests: bool) -> Result<Self> {
        let tls = match tls::ServerTls::load_or_create(data_dir) {
            Ok(tls) => Some(tls),
            Err(error) => {
                eprintln!("apiserver: TLS disabled: {error:#}");
                None
            }
        };
        let state = Self {
            store: Store::open(data_dir)?,
            registry: Registry::with_builtins(),
            server_address,
            log_requests,
            hub: Arc::new(AgentHub::default()),
            cluster_dns: std::sync::atomic::AtomicBool::new(false),
            tls,
            allocated: std::sync::Mutex::default(),
            image_loader: std::sync::OnceLock::new(),
        };
        state.bootstrap()?;
        Ok(state)
    }

    /// Re-register stored CRDs and create the objects every cluster has.
    fn bootstrap(&self) -> Result<()> {
        let (crds, _) = self
            .store
            .list("apiextensions.k8s.io", "customresourcedefinitions", None);
        for crd in &crds {
            if let Err(error) = self.registry.register_crd(crd) {
                eprintln!("apiserver: skipping stored CRD: {error:#}");
            }
        }
        let ns_type = self.builtin("", "namespaces");
        for ns in ["default", "kube-system", "kube-public", "kube-node-lease"] {
            self.ensure(&ns_type, None, json!({"metadata": {"name": ns}}))?;
        }
        let (namespaces, _) = self.store.list("", "namespaces", None);
        for ns in &namespaces {
            if let Some(name) = ns.pointer("/metadata/name").and_then(Value::as_str) {
                self.ensure_default_service_account(name)?;
            }
        }
        self.ensure(
            &self.builtin("", "services"),
            Some("default"),
            json!({
                "metadata": {"name": "kubernetes", "labels": {"component": "apiserver", "provider": "kubernetes"}},
                "spec": {
                    "clusterIP": KUBERNETES_SERVICE_IP,
                    "ports": [{"name": "https", "port": 443, "protocol": "TCP", "targetPort": 6443}],
                },
            }),
        )?;
        self.ensure(
            &self.builtin("storage.k8s.io", "storageclasses"),
            None,
            json!({
                "metadata": {
                    "name": defaults::DEFAULT_STORAGE_CLASS,
                    "annotations": {"storageclass.kubernetes.io/is-default-class": "true"},
                },
                "provisioner": "litek8s.dev/local-path",
                "reclaimPolicy": "Delete",
                "volumeBindingMode": "WaitForFirstConsumer",
            }),
        )?;
        self.ensure(
            &self.builtin("", "services"),
            Some("kube-system"),
            json!({
                "metadata": {"name": "kube-dns", "labels": {"k8s-app": "kube-dns", "kubernetes.io/name": "CoreDNS"}},
                "spec": {
                    "clusterIP": "10.43.0.10",
                    "ports": [
                        {"name": "dns", "port": 53, "protocol": "UDP", "targetPort": 53},
                        {"name": "dns-tcp", "port": 53, "protocol": "TCP", "targetPort": 53},
                    ],
                },
            }),
        )?;
        let pc = self.builtin("scheduling.k8s.io", "priorityclasses");
        for (name, value) in [("system-cluster-critical", 2_000_000_000), ("system-node-critical", 2_000_001_000)] {
            self.ensure(&pc, None, json!({"metadata": {"name": name}, "value": value}))?;
        }
        Ok(())
    }

    fn builtin(&self, group: &str, plural: &str) -> ResourceType {
        self.registry
            .find(group, "v1", plural)
            .unwrap_or_else(|| panic!("built-in resource {plural} is not registered"))
    }

    /// Create an object unless one with that name exists.
    fn ensure(&self, rt: &ResourceType, ns: Option<&str>, mut obj: Value) -> Result<()> {
        let name = obj["metadata"]["name"].as_str().unwrap_or_default().to_string();
        let key = key_for(rt, ns, &name);
        if self.store.get(&key).is_some() {
            return Ok(());
        }
        self.prepare_create(rt, ns, &mut obj, false)
            .map_err(|e| anyhow::anyhow!(e.message))?;
        match self.store.create(key, obj) {
            Ok(stored) => {
                self.after_write(rt, &stored);
                Ok(())
            }
            Err(StoreError::AlreadyExists) => Ok(()),
            Err(other) => Err(anyhow::anyhow!("bootstrap create failed: {other:?}")),
        }
    }

    fn ensure_default_service_account(&self, namespace: &str) -> Result<()> {
        self.ensure(
            &self.builtin("", "serviceaccounts"),
            Some(namespace),
            json!({"metadata": {"name": "default"}}),
        )
    }
}

pub async fn serve(mut config: ApiServerConfig) -> Result<()> {
    let listener = match config.listener.take() {
        Some(l) => {
            l.set_nonblocking(true)?;
            TcpListener::from_std(l)?
        }
        None => TcpListener::bind(config.listen)
            .await
            .with_context(|| format!("failed to bind apiserver on {}", config.listen))?,
    };
    let addr = listener.local_addr()?;
    let client_addr = if addr.ip().is_unspecified() {
        SocketAddr::new([127, 0, 0, 1].into(), addr.port())
    } else {
        addr
    };
    let state = Arc::new(ApiState::open(
        config.data_dir.as_deref(),
        client_addr.to_string(),
        config.log_requests,
    )?);
    if let Some(path) = &config.kubeconfig_out {
        write_kubeconfig(path, &format!("http://{client_addr}"))?;
        eprintln!("apiserver: wrote kubeconfig to {}", path.display());
    }
    eprintln!("apiserver: listening on http://{addr} (and https for in-cluster clients; no authentication)");
    tokio::spawn(super::controllers::run(state.clone()));

    // The local node comes first: where pods reach this host (Docker's
    // host-gateway, crun's bridge address) decides where DNS and the in-cluster port can listen.
    let agent = if config.run_node {
        let node_dir = config
            .node_dir
            .clone()
            .or_else(|| config.data_dir.as_ref().map(|d| d.join("node-agent")));
        match super::agent::node_agent(config.runtime, config.node_name.clone(), node_dir).await {
            Ok(agent) => Some(agent),
            Err(error) => {
                eprintln!("apiserver: not running a local node: {error:#}");
                None
            }
        }
    } else {
        None
    };
    let gateway: Option<std::net::IpAddr> = match &agent {
        Some(agent) => agent.resolve_host("host-gateway").await.and_then(|ip| ip.parse().ok()),
        None => None,
    };

    if let Some(dns_addr) = config.cluster_dns {
        // The wildcard address may be taken (systemd-resolved holds
        // 127.0.0.53:53); the gateway address is all pods need.
        let mut socket = super::dns::bind(dns_addr).await;
        if socket.is_none() {
            if let Some(ip) = gateway {
                socket = super::dns::bind(SocketAddr::new(ip, dns_addr.port())).await;
            }
        }
        if let Some(socket) = socket {
            state.cluster_dns.store(true, std::sync::atomic::Ordering::Relaxed);
            tokio::spawn(super::dns::serve(state.clone(), socket));
        }
    }

    // The port pods use: the in-cluster one when it could be bound.
    let mut pod_port = addr.port();
    if let Some(port) = config.in_cluster_port.filter(|p| *p != addr.port()) {
        pod_port = bind_in_cluster(&state, addr, gateway, port).await.unwrap_or(addr.port());
    } else if let Some(ip) = gateway.filter(|_| addr.ip().is_loopback()) {
        // Pods cannot reach a loopback-only listener on a Linux host.
        if let Ok(extra) = TcpListener::bind(SocketAddr::new(ip, addr.port())).await {
            tokio::spawn(serve_listener(state.clone(), extra));
        }
    }
    if let Some(agent) = agent {
        eprintln!("apiserver: running local node {}", agent.config().node_name);
        let rt = agent.runtime().clone();
        let _ = state.image_loader.set(ImageLoader {
            staging: rt.image_staging_dir(),
            load: Box::new(move |path| {
                let rt = rt.clone();
                Box::pin(async move { rt.import_images(path).await })
            }),
        });
        tokio::spawn(super::agent::run_local(state.clone(), agent, pod_port));
    }
    serve_listener(state, listener).await
}

/// Serve the API on `port` for pods. On a Linux host pods reach the host at
/// the Docker gateway address, so that is tried first; with a VM-backed
/// Docker they arrive through loopback.
async fn bind_in_cluster(state: &Arc<ApiState>, addr: SocketAddr, gateway: Option<std::net::IpAddr>, port: u16) -> Option<u16> {
    let serve_also_on = |listener: TcpListener, loopback_only: bool| {
        tokio::spawn(serve_listener_filtered(state.clone(), listener, loopback_only));
    };
    let mut bound = false;
    if let Some(ip) = gateway.filter(|_| addr.ip().is_loopback()) {
        if let Ok(l) = TcpListener::bind(SocketAddr::new(ip, port)).await {
            serve_also_on(l, false);
            bound = true;
        }
    }
    match TcpListener::bind(SocketAddr::new(addr.ip(), port)).await {
        Ok(l) => {
            serve_also_on(l, false);
            return Some(port);
        }
        // macOS lets unprivileged processes bind low ports only on the
        // wildcard address. Keep the API as private as the main listener:
        // when that is loopback, accept loopback peers only.
        Err(_) if addr.ip().is_loopback() && !bound => {
            match TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await {
                Ok(l) => {
                    serve_also_on(l, true);
                    return Some(port);
                }
                Err(e) => eprintln!("apiserver: in-cluster port {port} unavailable ({e}); pods use {}", addr.port()),
            }
        }
        Err(e) if !bound => eprintln!("apiserver: in-cluster port {port} unavailable ({e}); pods use {}", addr.port()),
        Err(_) => {}
    }
    bound.then_some(port)
}

/// Accept connections on an already-bound listener (used by tests).
pub async fn serve_listener(state: Arc<ApiState>, listener: TcpListener) -> Result<()> {
    serve_listener_filtered(state, listener, false).await
}

async fn serve_listener_filtered(state: Arc<ApiState>, listener: TcpListener, loopback_only: bool) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        if loopback_only && !peer.ip().is_loopback() {
            continue;
        }
        let _ = stream.set_nodelay(true);
        let state = state.clone();
        tokio::spawn(async move {
            // TLS and plain HTTP share the port: a TLS ClientHello starts
            // with 0x16, an HTTP request with a method name.
            let mut first = [0u8; 1];
            let is_tls = matches!(stream.peek(&mut first).await, Ok(1)) && first[0] == 0x16;
            let service_state = state.clone();
            let service = service_fn(move |req| {
                let state = service_state.clone();
                async move { Ok::<_, Infallible>(handle(state, req).await) }
            });
            match state.tls().filter(|_| is_tls) {
                Some(tls) => {
                    let tls_stream = match tls.accept(crate::kube::tls::OneMessagePerRecord::new(stream)).await {
                        Ok(s) => s,
                        Err(e) => {
                            if state.log_requests {
                                eprintln!("apiserver: TLS handshake failed: {e}");
                            }
                            return;
                        }
                    };
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(tls_stream), service)
                        .with_upgrades()
                        .await;
                }
                None => {
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await;
                }
            }
        });
    }
}

pub fn write_kubeconfig(path: &Path, server: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let config = format!(
        "apiVersion: v1\nkind: Config\nclusters:\n- name: litek8s\n  cluster:\n    server: {server}\n\
         contexts:\n- name: litek8s\n  context:\n    cluster: litek8s\n    user: litek8s\n\
         current-context: litek8s\nusers:\n- name: litek8s\n  user: {{}}\n"
    );
    std::fs::write(path, config).with_context(|| format!("failed to write {}", path.display()))
}

// ---------------------------------------------------------------------------
// Request plumbing

pub(crate) struct Req {
    method: Method,
    query: HashMap<String, String>,
    raw_query: String,
    accept: String,
    content_type: String,
    body: Bytes,
    headers: hyper::HeaderMap,
    upgrade: std::sync::Mutex<Option<hyper::upgrade::OnUpgrade>>,
}

impl Req {
    pub(crate) fn q(&self, key: &str) -> Option<&str> {
        self.query.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }

    /// Every value of a repeated query parameter (`command=a&command=b`).
    pub(crate) fn query_all(&self, key: &str) -> Vec<String> {
        self.raw_query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .filter(|(k, _)| percent_decode(k) == key)
            .map(|(_, v)| percent_decode(v))
            .collect()
    }

    pub(crate) fn header(&self, name: &str) -> &str {
        self.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default()
    }

    pub(crate) fn take_upgrade(&self) -> Option<hyper::upgrade::OnUpgrade> {
        self.upgrade.lock().unwrap().take()
    }

    fn clone_with_query(&self, query: HashMap<String, String>) -> Self {
        Self {
            method: self.method.clone(),
            query,
            raw_query: self.raw_query.clone(),
            accept: self.accept.clone(),
            content_type: self.content_type.clone(),
            body: self.body.clone(),
            headers: self.headers.clone(),
            upgrade: std::sync::Mutex::new(None),
        }
    }

    fn dry_run(&self) -> bool {
        self.q("dryRun").is_some()
    }

    /// Metadata-only clients (client-go's metadata informers).
    fn wants_partial(&self) -> bool {
        self.accept.contains("as=PartialObjectMetadata")
    }

    fn wants_table(&self) -> bool {
        self.accept.contains("as=Table")
    }
}

#[derive(Debug)]
pub(crate) struct ApiError {
    pub(crate) code: StatusCode,
    pub(crate) reason: &'static str,
    pub(crate) message: String,
    details: Option<Value>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code.as_u16())
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    pub(crate) fn new(code: StatusCode, reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            reason,
            message: message.into(),
            details: None,
        }
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "BadRequest", message)
    }

    fn invalid(rt: &ResourceType, name: &str, message: impl Into<String>) -> Self {
        let mut e = Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid",
            format!("{} \"{name}\" is invalid: {}", qualified(rt), message.into()),
        );
        e.details = Some(json!({"name": name, "group": rt.group, "kind": rt.kind}));
        e
    }

    fn not_found_path() -> Self {
        Self::new(StatusCode::NOT_FOUND, "NotFound", "the server could not find the requested resource")
    }

    fn from_store(error: StoreError, rt: &ResourceType, name: &str) -> Self {
        let q = qualified(rt);
        let mut e = match error {
            StoreError::NotFound => Self::new(StatusCode::NOT_FOUND, "NotFound", format!("{q} \"{name}\" not found")),
            StoreError::AlreadyExists => {
                Self::new(StatusCode::CONFLICT, "AlreadyExists", format!("{q} \"{name}\" already exists"))
            }
            StoreError::Conflict => Self::new(
                StatusCode::CONFLICT,
                "Conflict",
                format!(
                    "Operation cannot be fulfilled on {q} \"{name}\": the object has been modified; \
                     please apply your changes to the latest version and try again"
                ),
            ),
            StoreError::Gone => Self::new(StatusCode::GONE, "Expired", "too old resource version"),
        };
        e.details = Some(json!({"name": name, "group": rt.group, "kind": rt.plural}));
        e
    }

    fn into_response(self) -> Response<Body> {
        let mut status = json!({
            "kind": "Status",
            "apiVersion": "v1",
            "metadata": {},
            "status": "Failure",
            "message": self.message,
            "reason": self.reason,
            "code": self.code.as_u16(),
        });
        if let Some(details) = self.details {
            status["details"] = details;
        }
        json_response(self.code, &status)
    }
}

fn qualified(rt: &ResourceType) -> String {
    if rt.group.is_empty() {
        rt.plural.clone()
    } else {
        format!("{}.{}", rt.plural, rt.group)
    }
}

pub(crate) fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into()).map_err(|never| match never {}).boxed_unsync()
}

fn json_response(code: StatusCode, value: &Value) -> Response<Body> {
    json_response_typed(code, value, "application/json")
}

fn json_response_typed(code: StatusCode, value: &Value, content_type: &'static str) -> Response<Body> {
    let mut resp = Response::new(full(serde_json::to_vec(value).unwrap_or_default()));
    *resp.status_mut() = code;
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp
}

fn text_response(body: &'static str) -> Response<Body> {
    let mut resp = Response::new(full(body));
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    resp
}

fn parse_query(raw: Option<&str>) -> HashMap<String, String> {
    raw.unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push(hi << 4 | lo);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn handle(state: Arc<ApiState>, mut req: Request<Incoming>) -> Response<Body> {
    let started = Instant::now();
    let upgrade = req
        .headers()
        .contains_key(hyper::header::UPGRADE)
        .then(|| hyper::upgrade::on(&mut req));
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    if path == IMAGES_PATH && parts.method == Method::POST {
        return load_images(&state, body).await;
    }
    let header = |name| {
        parts
            .headers
            .get(name)
            .and_then(|v: &HeaderValue| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    let body = match Limited::new(body, MAX_BODY_BYTES).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => {
            return ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "RequestEntityTooLarge", "request body too large")
                .into_response();
        }
    };
    let req = Req {
        method: parts.method.clone(),
        query: parse_query(parts.uri.query()),
        raw_query: parts.uri.query().unwrap_or_default().to_string(),
        accept: header(ACCEPT),
        content_type: header(CONTENT_TYPE),
        body,
        headers: parts.headers.clone(),
        upgrade: std::sync::Mutex::new(upgrade),
    };
    let resp = match route(&state, &path, &req).await {
        Ok(resp) => resp,
        Err(error) => error.into_response(),
    };
    if state.log_requests {
        eprintln!(
            "apiserver: {} {}{} -> {} ({} µs)",
            parts.method,
            path,
            parts.uri.query().map(|q| format!("?{q}")).unwrap_or_default(),
            resp.status().as_u16(),
            started.elapsed().as_micros()
        );
    }
    resp
}

/// litek8s's own endpoint for loading image archives into the local node
/// (`litek8s dev load`), like `kind load image-archive`.
pub const IMAGES_PATH: &str = "/litek8s/v1/images";

/// Stream an uploaded image archive to disk (it can be far larger than an
/// API request body) and import it.
async fn load_images(state: &Arc<ApiState>, mut body: Incoming) -> Response<Body> {
    use tokio::io::AsyncWriteExt;
    let Some(loader) = state.image_loader.get() else {
        return ApiError::new(StatusCode::NOT_FOUND, "NotFound", "this server runs no local node to load images into").into_response();
    };
    let _ = std::fs::create_dir_all(&loader.staging);
    let mut name = [0u8; 8];
    OsRng.fill_bytes(&mut name);
    let path = loader.staging.join(format!("upload-{}.tar", hex::encode(name)));
    let result: Result<Vec<String>> = async {
        let mut file = tokio::fs::File::create(&path).await.with_context(|| format!("failed to create {}", path.display()))?;
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame?.into_data() {
                file.write_all(&data).await?;
            }
        }
        file.flush().await?;
        drop(file);
        (loader.load)(path.clone()).await
    }
    .await;
    let _ = tokio::fs::remove_file(&path).await;
    match result {
        Ok(images) => json_response(StatusCode::OK, &json!({"images": images})),
        Err(e) => ApiError::new(StatusCode::BAD_REQUEST, "BadRequest", &format!("{e:#}")).into_response(),
    }
}

async fn route(state: &Arc<ApiState>, path: &str, req: &Req) -> Result<Response<Body>, ApiError> {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let get = req.method == Method::GET;
    let aggregated = req
        .accept
        .replace(' ', "")
        .contains("g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList");
    const AGGREGATED_TYPE: &str = "application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList";
    match segs.as_slice() {
        ["version"] if get => Ok(json_response(StatusCode::OK, &discovery::version())),
        [probe, ..] if get && matches!(*probe, "healthz" | "livez" | "readyz") => Ok(text_response("ok")),
        ["api"] if get && aggregated => Ok(json_response_typed(
            StatusCode::OK,
            &discovery::aggregated(&state.registry, true),
            AGGREGATED_TYPE,
        )),
        ["api"] if get => Ok(json_response(StatusCode::OK, &discovery::api_versions(&state.server_address))),
        ["apis"] if get && aggregated => Ok(json_response_typed(
            StatusCode::OK,
            &discovery::aggregated(&state.registry, false),
            AGGREGATED_TYPE,
        )),
        ["apis"] if get => Ok(json_response(StatusCode::OK, &discovery::group_list(&state.registry))),
        ["api", "v1"] if get => Ok(json_response(
            StatusCode::OK,
            &discovery::resource_list(&state.registry, "", "v1"),
        )),
        ["apis", group] if get => discovery::group(&state.registry, group)
            .map(|g| json_response(StatusCode::OK, &g))
            .ok_or_else(ApiError::not_found_path),
        ["apis", group, version] if get => {
            if state.registry.has_group_version(group, version) {
                Ok(json_response(
                    StatusCode::OK,
                    &discovery::resource_list(&state.registry, group, version),
                ))
            } else {
                Err(ApiError::not_found_path())
            }
        }
        ["openapi", "v3"] if get => Ok(json_response(StatusCode::OK, &discovery::openapi_root(&state.registry))),
        ["openapi", "v3", "api", version] if get => discovery::openapi_group_version(&state.registry, "", version)
            .map(|d| json_response(StatusCode::OK, &d))
            .ok_or_else(ApiError::not_found_path),
        ["openapi", "v3", "apis", group, version] if get => {
            discovery::openapi_group_version(&state.registry, group, version)
                .map(|d| json_response(StatusCode::OK, &d))
                .ok_or_else(ApiError::not_found_path)
        }
        // kubectl falls back to OpenAPI v2 for kinds the v3 index does not
        // list (e.g. `kind: List`) and asks for the gnostic protobuf form. An
        // empty message is a valid empty document: no schemas, so kubectl
        // skips client-side checks and the server validates instead.
        ["openapi", "v2"] if get && req.accept.contains("protobuf") => {
            let mut resp = Response::new(full(Bytes::new()));
            resp.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/com.github.proto-openapi.spec.v2.v1.0+protobuf"),
            );
            Ok(resp)
        }
        ["openapi", "v2"] if get => Ok(json_response(
            StatusCode::OK,
            &json!({"swagger": "2.0", "info": {"title": "Kubernetes", "version": discovery::KUBE_GIT_VERSION}, "paths": {}, "definitions": {}}),
        )),
        ["apis", "agent.litek8s.dev", "v1", "connect"] if get => agent_connect(state, req),
        ["api", "v1", rest @ ..] => resource_request(state, req, "", "v1", rest).await,
        ["apis", group, version, rest @ ..] => resource_request(state, req, group, version, rest).await,
        _ => Err(ApiError::not_found_path()),
    }
}

struct Target {
    rt: ResourceType,
    namespace: Option<String>,
    name: Option<String>,
    sub: Option<String>,
}

fn resolve_target(state: &ApiState, group: &str, version: &str, rest: &[&str]) -> Result<Target, ApiError> {
    let find = |plural: &str| state.registry.find(group, version, plural);
    let (namespace, rest) = match rest {
        ["namespaces", ns, plural, tail @ ..] if find(plural).is_some_and(|rt| rt.namespaced) => {
            (Some(ns.to_string()), [&[*plural][..], tail].concat())
        }
        other => (None, other.to_vec()),
    };
    let (plural, name, sub) = match rest.as_slice() {
        [plural] => (*plural, None, None),
        [plural, name] => (*plural, Some(name.to_string()), None),
        [plural, name, sub] => (*plural, Some(name.to_string()), Some(sub.to_string())),
        _ => return Err(ApiError::not_found_path()),
    };
    let rt = find(plural).ok_or_else(ApiError::not_found_path)?;
    if rt.namespaced && namespace.is_none() && name.is_some() {
        return Err(ApiError::not_found_path());
    }
    Ok(Target { rt, namespace, name, sub })
}

fn key_for(rt: &ResourceType, ns: Option<&str>, name: &str) -> ObjectKey {
    ObjectKey::new(&rt.group, &rt.plural, if rt.namespaced { ns.unwrap_or("") } else { "" }, name)
}

fn parse_body(req: &Req) -> Result<Value, ApiError> {
    if req.body.is_empty() {
        return Err(ApiError::bad_request("the request body is empty"));
    }
    if protobuf::is_protobuf(&req.body) {
        return protobuf::decode(&req.body).map_err(|e| ApiError::bad_request(format!("{e:#}")));
    }
    if req.content_type.contains("yaml") {
        serde_yaml::from_slice(&req.body).map_err(|e| ApiError::bad_request(format!("invalid YAML body: {e}")))
    } else {
        serde_json::from_slice(&req.body).map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))
    }
}

async fn resource_request(
    state: &Arc<ApiState>,
    req: &Req,
    group: &str,
    version: &str,
    rest: &[&str],
) -> Result<Response<Body>, ApiError> {
    // Legacy `/api/v1/watch/...` form.
    let (legacy_watch, rest) = match rest {
        ["watch", tail @ ..] => (true, tail),
        other => (false, other),
    };
    let target = resolve_target(state, group, version, rest)?;
    let ns = target.namespace.as_deref();
    let rt = &target.rt;
    match (&req.method, target.name.as_deref(), target.sub.as_deref()) {
        (&Method::GET, None, None) => {
            if legacy_watch || matches!(req.q("watch"), Some("true" | "1")) {
                state.watch(rt, ns, req)
            } else {
                state.list(rt, ns, req)
            }
        }
        (&Method::GET, Some(name), None) if legacy_watch => {
            let mut query = req.query.clone();
            query.insert("fieldSelector".into(), format!("metadata.name={name}"));
            state.watch(rt, ns, &req.clone_with_query(query))
        }
        (&Method::POST, None, None) if is_review(rt) => state.review(rt, req),
        (&Method::GET, Some(name), Some("log")) if is_pods(rt) => {
            super::pod_streams::log(state, ns.unwrap_or_default(), name, req).await
        }
        (&Method::GET | &Method::POST, Some(name), Some(sub @ ("exec" | "attach"))) if is_pods(rt) => {
            super::pod_streams::exec(state, ns.unwrap_or_default(), name, req, sub == "attach").await
        }
        (&Method::GET | &Method::POST, Some(name), Some("portforward")) if is_pods(rt) => {
            super::pod_streams::port_forward(state, ns.unwrap_or_default(), name, req).await
        }
        (&Method::POST, Some(name), Some("binding")) if is_pods(rt) => state.bind(ns.unwrap_or_default(), name, req),
        (&Method::POST, Some(name), Some("eviction")) if is_pods(rt) => {
            let key = key_for(rt, ns, name);
            let old = self_get(state, &key, rt, name)?;
            let gone = state.delete_object(rt, &key, old, None).map_err(|e| ApiError::from_store(e, rt, name))?;
            let _ = gone;
            Ok(json_response(StatusCode::CREATED, &json!({"kind": "Status", "apiVersion": "v1", "metadata": {}, "status": "Success", "code": 201})))
        }
        (&Method::POST, None, None) => {
            if rt.namespaced && ns.is_none() {
                return Err(ApiError::not_found_path());
            }
            state.create(rt, ns, req).await
        }
        (&Method::DELETE, None, None) => state.delete_collection(rt, ns, req),
        (&Method::GET, Some(name), None | Some("status")) => state.get(rt, ns, name, req),
        (&Method::PUT, Some(name), None | Some("status") | Some("finalize")) => {
            state.update(rt, ns, name, target.sub.as_deref(), req).await
        }
        (&Method::PATCH, Some(name), None | Some("status")) => {
            state.patch(rt, ns, name, target.sub.as_deref(), req).await
        }
        (&Method::DELETE, Some(name), None) => state.delete(rt, ns, name, req),
        (m, Some(name), Some("scale")) if rt.has_scale => state.scale(rt, ns, name, m, req),
        (_, _, Some(sub)) => Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "MethodNotAllowed",
            format!("the {sub} subresource of {} is not supported by this server yet", rt.plural),
        )),
        _ => Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "MethodNotAllowed",
            "the server does not allow this method on the requested resource",
        )),
    }
}

fn is_pods(rt: &ResourceType) -> bool {
    rt.group.is_empty() && rt.plural == "pods"
}

/// Access and token reviews: answered, never stored. Without
/// authentication or authorization every review is allowed.
fn is_review(rt: &ResourceType) -> bool {
    matches!(rt.group.as_str(), "authorization.k8s.io" | "authentication.k8s.io")
}

fn self_get(state: &ApiState, key: &ObjectKey, rt: &ResourceType, name: &str) -> Result<Value, ApiError> {
    state.store.get(key).ok_or_else(|| ApiError::from_store(StoreError::NotFound, rt, name))
}

/// A node agent dialing in: upgrade the connection to the agent protocol.
fn agent_connect(state: &Arc<ApiState>, req: &Req) -> Result<Response<Body>, ApiError> {
    if !req.header("upgrade").eq_ignore_ascii_case(super::agent::protocol::PROTOCOL_VERSION) {
        return Err(ApiError::bad_request(format!("expected Upgrade: {}", super::agent::protocol::PROTOCOL_VERSION)));
    }
    let on_upgrade = req.take_upgrade().ok_or_else(|| ApiError::bad_request("connection cannot be upgraded"))?;
    // Pods on that node reach this server at the address the agent used.
    let host_header = req.header("host").to_string();
    let (api_host, api_port) = match host_header.rsplit_once(':') {
        Some((h, p)) => (h.trim_matches(['[', ']']).to_string(), p.parse().unwrap_or(6443)),
        None => (host_header.clone(), 80),
    };
    let state = state.clone();
    tokio::spawn(async move {
        let Ok(upgraded) = on_upgrade.await else { return };
        if let Err(e) = AgentHub::serve_agent(state, TokioIo::new(upgraded), api_host, api_port).await {
            eprintln!("apiserver: agent connection failed: {e:#}");
        }
    });
    let mut resp = Response::new(full(Bytes::new()));
    *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    resp.headers_mut().insert(hyper::header::UPGRADE, HeaderValue::from_static(super::agent::protocol::PROTOCOL_VERSION));
    resp.headers_mut().insert(hyper::header::CONNECTION, HeaderValue::from_static("Upgrade"));
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Verbs

fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn random_uid() -> String {
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// The suffix alphabet Kubernetes uses for generateName (no vowels, no
/// look-alike digits).
fn random_suffix() -> String {
    const ALPHABET: &[u8] = b"bcdfghjklmnpqrstvwxz2456789";
    (0..5)
        .map(|_| ALPHABET[(OsRng.next_u32() as usize) % ALPHABET.len()] as char)
        .collect()
}

fn meta(obj: &Value) -> &Map<String, Value> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    obj.get("metadata")
        .and_then(Value::as_object)
        .unwrap_or_else(|| EMPTY.get_or_init(Map::new))
}

fn name_of(obj: &Value) -> String {
    meta(obj).get("name").and_then(Value::as_str).unwrap_or_default().to_string()
}

fn has_finalizers(obj: &Value) -> bool {
    meta(obj)
        .get("finalizers")
        .and_then(Value::as_array)
        .is_some_and(|f| !f.is_empty())
}

/// Everything but metadata and status, to decide whether `generation` moves.
fn spec_part(obj: &Value) -> Value {
    let mut v = obj.clone();
    if let Some(m) = v.as_object_mut() {
        m.remove("metadata");
        m.remove("status");
    }
    v
}

impl ApiState {
    fn list(&self, rt: &ResourceType, ns: Option<&str>, req: &Req) -> Result<Response<Body>, ApiError> {
        let labels = LabelSelector::parse(req.q("labelSelector").unwrap_or_default())
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        let fields = FieldSelector::parse(req.q("fieldSelector").unwrap_or_default())
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        let (mut items, rv) = match fields.exact_name() {
            Some(name) if ns.is_some() || !rt.namespaced => {
                let found = self.store.get(&key_for(rt, ns, name));
                (found.into_iter().collect(), self.store.current_rv())
            }
            _ => self.store.list(&rt.group, &rt.plural, if rt.namespaced { ns } else { None }),
        };
        items.retain(|o| labels.matches(o) && fields.matches(o));

        // Pagination: the continue token is the base64 "namespace/name" of the
        // last item returned; the store lists in that order.
        if let Some(token) = req.q("continue") {
            let after = base64::engine::general_purpose::STANDARD
                .decode(token)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .ok_or_else(|| ApiError::bad_request("invalid continue token"))?;
            items.retain(|o| item_cursor(o) > after);
        }
        let mut cont = None;
        let mut remaining = None;
        if let Some(limit) = req.q("limit").and_then(|l| l.parse::<usize>().ok()).filter(|l| *l > 0) {
            if items.len() > limit {
                remaining = Some(items.len() - limit);
                items.truncate(limit);
                cont = items.last().map(|o| base64::engine::general_purpose::STANDARD.encode(item_cursor(o)));
            }
        }

        if req.wants_table() {
            let include = req.q("includeObject") != Some("None");
            let mut t = table::build(rt, &items, rv, include);
            if let Some(c) = cont {
                t["metadata"]["continue"] = json!(c);
            }
            return Ok(json_response(StatusCode::OK, &t));
        }
        let mut list_meta = json!({"resourceVersion": rv.to_string()});
        if let Some(c) = cont {
            list_meta["continue"] = json!(c);
            list_meta["remainingItemCount"] = json!(remaining.unwrap_or(0));
        }
        if req.wants_partial() {
            let items: Vec<Value> = items.iter().map(partial_metadata).collect();
            return Ok(json_response(
                StatusCode::OK,
                &json!({"kind": "PartialObjectMetadataList", "apiVersion": "meta.k8s.io/v1", "metadata": list_meta, "items": items}),
            ));
        }
        Ok(json_response(
            StatusCode::OK,
            &json!({
                "kind": rt.list_kind(),
                "apiVersion": rt.api_version(),
                "metadata": list_meta,
                "items": items,
            }),
        ))
    }

    fn review(&self, rt: &ResourceType, req: &Req) -> Result<Response<Body>, ApiError> {
        let mut obj = parse_body(req)?;
        obj["kind"] = json!(rt.kind);
        obj["apiVersion"] = json!(rt.api_version());
        if !obj.get("metadata").is_some_and(Value::is_object) {
            obj["metadata"] = json!({});
        }
        obj["metadata"]["creationTimestamp"] = Value::Null;
        obj["status"] = match rt.kind.as_str() {
            "TokenReview" => {
                let user = token_user(obj.pointer("/spec/token").and_then(Value::as_str).unwrap_or_default());
                json!({"authenticated": true, "user": user, "audiences": obj.pointer("/spec/audiences").cloned().unwrap_or(json!([]))})
            }
            "SelfSubjectRulesReview" => json!({
                "resourceRules": [{"verbs": ["*"], "apiGroups": ["*"], "resources": ["*"]}],
                "nonResourceRules": [{"verbs": ["*"], "nonResourceURLs": ["*"]}],
                "incomplete": false,
            }),
            "SelfSubjectReview" => json!({"userInfo": {"username": "system:admin", "groups": ["system:masters", "system:authenticated"]}}),
            _ => json!({"allowed": true, "reason": "litek8s has no authorization yet"}),
        };
        Ok(json_response(StatusCode::CREATED, &obj))
    }

    /// `pods/binding`: what an external scheduler posts.
    fn bind(&self, ns: &str, name: &str, req: &Req) -> Result<Response<Body>, ApiError> {
        let binding = parse_body(req)?;
        let node = binding.pointer("/target/name").and_then(Value::as_str).unwrap_or_default();
        if node.is_empty() {
            return Err(ApiError::bad_request("binding target name is required"));
        }
        let rt = self.builtin("", "pods");
        let mut pod = self
            .store
            .get(&key_for(&rt, Some(ns), name))
            .ok_or_else(|| ApiError::from_store(StoreError::NotFound, &rt, name))?;
        if pod.pointer("/spec/nodeName").and_then(Value::as_str).is_some_and(|n| !n.is_empty()) {
            return Err(ApiError::new(StatusCode::CONFLICT, "Conflict", format!("pod {name} is already assigned to a node")));
        }
        pod["spec"]["nodeName"] = json!(node);
        self.update_object("", "pods", pod)?;
        Ok(json_response(StatusCode::CREATED, &json!({"kind": "Status", "apiVersion": "v1", "metadata": {}, "status": "Success", "code": 201})))
    }

    fn get(&self, rt: &ResourceType, ns: Option<&str>, name: &str, req: &Req) -> Result<Response<Body>, ApiError> {
        let obj = self
            .store
            .get(&key_for(rt, ns, name))
            .ok_or_else(|| ApiError::from_store(StoreError::NotFound, rt, name))?;
        if req.wants_table() {
            let rv = resource_version(&obj).unwrap_or(0);
            return Ok(json_response(StatusCode::OK, &table::build(rt, &[obj], rv, true)));
        }
        if req.wants_partial() {
            return Ok(json_response(StatusCode::OK, &partial_metadata(&obj)));
        }
        Ok(json_response(StatusCode::OK, &obj))
    }

    async fn create(self: &Arc<Self>, rt: &ResourceType, ns: Option<&str>, req: &Req) -> Result<Response<Body>, ApiError> {
        let mut obj = parse_body(req)?;
        let generated = meta(&obj).get("name").and_then(Value::as_str).unwrap_or_default().is_empty();
        self.prepare_create(rt, ns, &mut obj, true)?;
        let name = name_of(&obj);
        let attrs = admission::Attributes {
            rt,
            operation: "CREATE",
            namespace: ns.unwrap_or_default(),
            name: &name,
            sub: None,
            old: None,
            dry_run: req.dry_run(),
        };
        let mut obj = admission::admit(self, &attrs, obj).await?;
        if req.dry_run() {
            return Ok(json_response(StatusCode::CREATED, &obj));
        }
        let mut attempt = 0;
        loop {
            let name = name_of(&obj);
            match self.store.create(key_for(rt, ns, &name), obj.clone()) {
                Ok(stored) => {
                    self.after_write(rt, &stored);
                    return Ok(json_response(StatusCode::CREATED, &stored));
                }
                Err(StoreError::AlreadyExists) if generated && attempt < 5 => {
                    attempt += 1;
                    let base = meta(&obj).get("generateName").and_then(Value::as_str).unwrap_or_default().to_string();
                    obj["metadata"]["name"] = json!(format!("{base}{}", random_suffix()));
                }
                Err(e) => return Err(ApiError::from_store(e, rt, &name)),
            }
        }
    }

    async fn update(
        self: &Arc<Self>,
        rt: &ResourceType,
        ns: Option<&str>,
        name: &str,
        sub: Option<&str>,
        req: &Req,
    ) -> Result<Response<Body>, ApiError> {
        let obj = parse_body(req)?;
        let key = key_for(rt, ns, name);
        let old = self
            .store
            .get(&key)
            .ok_or_else(|| ApiError::from_store(StoreError::NotFound, rt, name))?;
        let expected = meta(&obj)
            .get("resourceVersion")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<u64>().map_err(|_| ApiError::bad_request("invalid resourceVersion")))
            .transpose()?;
        let new = self.prepare_update(rt, &old, obj, sub, name)?;
        let attrs = admission::Attributes {
            rt,
            operation: "UPDATE",
            namespace: ns.unwrap_or_default(),
            name,
            sub,
            old: Some(&old),
            dry_run: req.dry_run(),
        };
        let new = admission::admit(self, &attrs, new).await?;
        if req.dry_run() {
            return Ok(json_response(StatusCode::OK, &new));
        }
        let stored = self
            .commit_update(rt, key, new, expected)
            .map_err(|e| ApiError::from_store(e, rt, name))?;
        Ok(json_response(StatusCode::OK, &stored))
    }

    async fn patch(
        self: &Arc<Self>,
        rt: &ResourceType,
        ns: Option<&str>,
        name: &str,
        sub: Option<&str>,
        req: &Req,
    ) -> Result<Response<Body>, ApiError> {
        let ct = req.content_type.split(';').next().unwrap_or_default().trim();
        let is_apply = ct == "application/apply-patch+yaml" || ct == "application/apply-patch+cbor";
        let body: Value = if is_apply || ct.contains("yaml") {
            serde_yaml::from_slice(&req.body).map_err(|e| ApiError::bad_request(format!("invalid patch: {e}")))?
        } else {
            serde_json::from_slice(&req.body).map_err(|e| ApiError::bad_request(format!("invalid patch: {e}")))?
        };
        let key = key_for(rt, ns, name);
        let explicit_rv = body
            .pointer("/metadata/resourceVersion")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<u64>().ok());

        for _ in 0..8 {
            let Some(old) = self.store.get(&key) else {
                if is_apply && sub.is_none() {
                    // Server-side apply creates missing objects.
                    let mut obj = body.clone();
                    if name_of(&obj).is_empty() {
                        obj["metadata"]["name"] = json!(name);
                    }
                    self.prepare_create(rt, ns, &mut obj, true)?;
                    let attrs = admission::Attributes {
                        rt,
                        operation: "CREATE",
                        namespace: ns.unwrap_or_default(),
                        name,
                        sub: None,
                        old: None,
                        dry_run: req.dry_run(),
                    };
                    let obj = admission::admit(self, &attrs, obj).await?;
                    if req.dry_run() {
                        return Ok(json_response(StatusCode::CREATED, &obj));
                    }
                    let stored = self
                        .store
                        .create(key.clone(), obj)
                        .map_err(|e| ApiError::from_store(e, rt, name))?;
                    self.after_write(rt, &stored);
                    return Ok(json_response(StatusCode::CREATED, &stored));
                }
                return Err(ApiError::from_store(StoreError::NotFound, rt, name));
            };
            let mut target = old.clone();
            match ct {
                "application/json-patch+json" => patch::json_patch(&mut target, &body)
                    .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "Invalid", e.to_string()))?,
                "application/merge-patch+json" => patch::merge_patch(&mut target, &body),
                "application/strategic-merge-patch+json" => {
                    if rt.custom {
                        return Err(ApiError::new(
                            StatusCode::UNSUPPORTED_MEDIA_TYPE,
                            "UnsupportedMediaType",
                            "strategic merge patch is not supported for custom resources; use --type merge",
                        ));
                    }
                    patch::strategic_merge_patch(&mut target, &body)
                }
                _ if is_apply => patch::apply_merge(&mut target, &body),
                other => {
                    return Err(ApiError::new(
                        StatusCode::UNSUPPORTED_MEDIA_TYPE,
                        "UnsupportedMediaType",
                        format!("unsupported patch content type `{other}`"),
                    ));
                }
            }
            let new = self.prepare_update(rt, &old, target, sub, name)?;
            let attrs = admission::Attributes {
                rt,
                operation: "UPDATE",
                namespace: ns.unwrap_or_default(),
                name,
                sub,
                old: Some(&old),
                dry_run: req.dry_run(),
            };
            let new = admission::admit(self, &attrs, new).await?;
            if req.dry_run() {
                return Ok(json_response(StatusCode::OK, &new));
            }
            let expected = explicit_rv.or_else(|| resource_version(&old));
            match self.commit_update(rt, key.clone(), new, expected) {
                Ok(stored) => return Ok(json_response(StatusCode::OK, &stored)),
                // Lost a race with another writer: re-read and re-apply.
                Err(StoreError::Conflict) if explicit_rv.is_none() => continue,
                Err(e) => return Err(ApiError::from_store(e, rt, name)),
            }
        }
        Err(ApiError::from_store(StoreError::Conflict, rt, name))
    }

    fn delete(&self, rt: &ResourceType, ns: Option<&str>, name: &str, req: &Req) -> Result<Response<Body>, ApiError> {
        let key = key_for(rt, ns, name);
        let old = self
            .store
            .get(&key)
            .ok_or_else(|| ApiError::from_store(StoreError::NotFound, rt, name))?;
        let opts: Value = if req.body.is_empty() {
            Value::Null
        } else {
            parse_body(req).unwrap_or(Value::Null)
        };
        if let Some(uid) = opts.pointer("/preconditions/uid").and_then(Value::as_str) {
            if meta(&old).get("uid").and_then(Value::as_str) != Some(uid) {
                return Err(ApiError::from_store(StoreError::Conflict, rt, name));
            }
        }
        let expected = opts
            .pointer("/preconditions/resourceVersion")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok());
        if req.dry_run() || opts.get("dryRun").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
            return Ok(json_response(StatusCode::OK, &old));
        }
        let orphan = opts.get("propagationPolicy").and_then(Value::as_str) == Some("Orphan")
            || req.q("propagationPolicy") == Some("Orphan")
            || opts.get("orphanDependents").and_then(Value::as_bool) == Some(true);
        if orphan {
            self.orphan_dependents(&old);
        }
        let result = self
            .delete_object(rt, &key, old, expected)
            .map_err(|e| ApiError::from_store(e, rt, name))?;
        Ok(json_response(StatusCode::OK, &result))
    }

    fn delete_collection(&self, rt: &ResourceType, ns: Option<&str>, req: &Req) -> Result<Response<Body>, ApiError> {
        let labels = LabelSelector::parse(req.q("labelSelector").unwrap_or_default())
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        let fields = FieldSelector::parse(req.q("fieldSelector").unwrap_or_default())
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        let (items, _) = self.store.list(&rt.group, &rt.plural, if rt.namespaced { ns } else { None });
        let mut deleted = Vec::new();
        for obj in items.into_iter().filter(|o| labels.matches(o) && fields.matches(o)) {
            if req.dry_run() {
                deleted.push(obj);
                continue;
            }
            let o_ns = meta(&obj).get("namespace").and_then(Value::as_str).map(str::to_string);
            let key = key_for(rt, o_ns.as_deref(), &name_of(&obj));
            if let Ok(d) = self.delete_object(rt, &key, obj, None) {
                deleted.push(d);
            }
        }
        Ok(json_response(
            StatusCode::OK,
            &json!({"kind": rt.list_kind(), "apiVersion": rt.api_version(), "metadata": {}, "items": deleted}),
        ))
    }

    /// Delete honouring finalizers, with cascades for namespaces and CRDs.
    fn delete_object(
        &self,
        rt: &ResourceType,
        key: &ObjectKey,
        old: Value,
        expected: Option<u64>,
    ) -> Result<Value, StoreError> {
        let is_namespace = rt.group.is_empty() && rt.plural == "namespaces";
        if is_namespace {
            for child in self.store.keys_in_namespace(&key.name) {
                let _ = self.store.delete(&child, None);
            }
            let mut gone = self.store.delete(key, expected)?;
            gone["status"]["phase"] = json!("Terminating");
            return Ok(gone);
        }
        if has_finalizers(&old) {
            if meta(&old).contains_key("deletionTimestamp") {
                return Ok(old);
            }
            let mut marked = old.clone();
            marked["metadata"]["deletionTimestamp"] = json!(now_rfc3339());
            marked["metadata"]["deletionGracePeriodSeconds"] = json!(0);
            return self.store.update(key.clone(), marked, expected);
        }
        let gone = self.store.delete(key, expected)?;
        self.after_delete(rt, &gone);
        Ok(gone)
    }

    /// Drop the ownerReferences other objects hold to `owner`, so the
    /// garbage collector leaves them alone (`--cascade=orphan`).
    fn orphan_dependents(&self, owner: &Value) {
        let Some(uid) = meta(owner).get("uid").and_then(Value::as_str) else { return };
        let ns = meta(owner).get("namespace").and_then(Value::as_str).unwrap_or_default();
        for (key, mut obj) in self.store.objects_in_namespace(ns) {
            let Some(refs) = obj.pointer_mut("/metadata/ownerReferences").and_then(Value::as_array_mut) else { continue };
            let before = refs.len();
            refs.retain(|r| r.get("uid").and_then(Value::as_str) != Some(uid));
            if refs.len() != before {
                let rv = resource_version(&obj);
                let _ = self.store.update(key, obj, rv);
            }
        }
    }

    /// Store an update; an object that is being deleted and has just lost its
    /// last finalizer is removed instead.
    fn commit_update(
        &self,
        rt: &ResourceType,
        key: ObjectKey,
        new: Value,
        expected: Option<u64>,
    ) -> Result<Value, StoreError> {
        if meta(&new).contains_key("deletionTimestamp") && !has_finalizers(&new) {
            let gone = self.store.delete(&key, expected)?;
            self.after_delete(rt, &gone);
            return Ok(gone);
        }
        // A write that changes nothing is not a write: no new
        // resourceVersion, no watch event (watchers would react to it).
        if let Some(current) = self.store.get(&key) {
            let expected_ok = expected.is_none_or(|rv| resource_version(&current) == Some(rv));
            if expected_ok && same_content(&current, &new) {
                return Ok(current);
            }
        }
        let stored = self.store.update(key, new, expected)?;
        self.after_write(rt, &stored);
        Ok(stored)
    }

    fn scale(
        &self,
        rt: &ResourceType,
        ns: Option<&str>,
        name: &str,
        method: &Method,
        req: &Req,
    ) -> Result<Response<Body>, ApiError> {
        let key = key_for(rt, ns, name);
        let old = self
            .store
            .get(&key)
            .ok_or_else(|| ApiError::from_store(StoreError::NotFound, rt, name))?;
        let current = scale_of(&old);
        let desired = match *method {
            Method::GET => return Ok(json_response(StatusCode::OK, &current)),
            Method::PUT => parse_body(req)?,
            Method::PATCH => {
                let body: Value = serde_json::from_slice(&req.body)
                    .map_err(|e| ApiError::bad_request(format!("invalid patch: {e}")))?;
                let mut s = current.clone();
                if req.content_type.starts_with("application/json-patch+json") {
                    patch::json_patch(&mut s, &body).map_err(|e| ApiError::bad_request(e.to_string()))?;
                } else {
                    patch::merge_patch(&mut s, &body);
                }
                s
            }
            _ => {
                return Err(ApiError::new(StatusCode::METHOD_NOT_ALLOWED, "MethodNotAllowed", "unsupported verb on scale"));
            }
        };
        let replicas = desired
            .pointer("/spec/replicas")
            .and_then(Value::as_i64)
            .ok_or_else(|| ApiError::bad_request("scale needs spec.replicas"))?;
        let mut updated = old.clone();
        updated["spec"]["replicas"] = json!(replicas);
        let new = self.prepare_update(rt, &old, updated, None, name)?;
        if req.dry_run() {
            return Ok(json_response(StatusCode::OK, &scale_of(&new)));
        }
        let stored = self
            .commit_update(rt, key, new, resource_version(&old))
            .map_err(|e| ApiError::from_store(e, rt, name))?;
        Ok(json_response(StatusCode::OK, &scale_of(&stored)))
    }

    // -----------------------------------------------------------------------
    // Admission-like defaulting and invariants

    fn prepare_create(
        &self,
        rt: &ResourceType,
        ns: Option<&str>,
        obj: &mut Value,
        check_namespace: bool,
    ) -> Result<(), ApiError> {
        let Some(map) = obj.as_object_mut() else {
            return Err(ApiError::bad_request("the request body must be a JSON object"));
        };
        if let Some(kind) = map.get("kind").and_then(Value::as_str) {
            if kind != rt.kind {
                return Err(ApiError::bad_request(format!(
                    "the kind `{kind}` of the object does not match the expected `{}` for this endpoint",
                    rt.kind
                )));
            }
        }
        map.insert("kind".into(), json!(rt.kind));
        map.insert("apiVersion".into(), json!(rt.api_version()));
        let meta = map.entry("metadata").or_insert_with(|| json!({}));
        let Some(meta) = meta.as_object_mut() else {
            return Err(ApiError::bad_request("metadata must be an object"));
        };
        let mut name = meta.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
        if name.is_empty() {
            match meta.get("generateName").and_then(Value::as_str) {
                Some(base) if !base.is_empty() => {
                    name = format!("{base}{}", random_suffix());
                    meta.insert("name".into(), json!(name));
                }
                _ => return Err(ApiError::invalid(rt, "", "metadata.name: Required value: name or generateName is required")),
            }
        }
        if name.contains('/') || name.contains('%') || name == "." || name == ".." {
            return Err(ApiError::invalid(rt, &name, "metadata.name: Invalid value: may not contain '/' or '%'"));
        }
        if rt.namespaced {
            let ns = ns.ok_or_else(|| ApiError::bad_request("a namespace is required for this resource"))?;
            if let Some(body_ns) = meta.get("namespace").and_then(Value::as_str) {
                if !body_ns.is_empty() && body_ns != ns {
                    return Err(ApiError::bad_request(
                        "the namespace of the provided object does not match the namespace sent on the request",
                    ));
                }
            }
            meta.insert("namespace".into(), json!(ns));
            if check_namespace {
                let ns_key = ObjectKey::new("", "namespaces", "", ns);
                match self.store.get(&ns_key) {
                    None => {
                        return Err(ApiError::new(
                            StatusCode::NOT_FOUND,
                            "NotFound",
                            format!("namespaces \"{ns}\" not found"),
                        ));
                    }
                    Some(n) if n.get("metadata").is_some_and(|m| m.get("deletionTimestamp").is_some()) => {
                        return Err(ApiError::new(
                            StatusCode::FORBIDDEN,
                            "Forbidden",
                            format!("unable to create new content in namespace {ns} because it is being terminated"),
                        ));
                    }
                    Some(_) => {}
                }
            }
        } else {
            meta.remove("namespace");
        }
        meta.insert("uid".into(), json!(random_uid()));
        meta.insert("creationTimestamp".into(), json!(now_rfc3339()));
        for transient in ["resourceVersion", "deletionTimestamp", "deletionGracePeriodSeconds", "managedFields", "selfLink"] {
            meta.remove(transient);
        }
        if rt.has_status {
            meta.insert("generation".into(), json!(1));
            map.remove("status");
        }
        self.default_by_kind(rt, obj, None)?;
        Ok(())
    }

    fn prepare_update(
        &self,
        rt: &ResourceType,
        old: &Value,
        mut new: Value,
        sub: Option<&str>,
        url_name: &str,
    ) -> Result<Value, ApiError> {
        if !new.is_object() {
            return Err(ApiError::bad_request("the request body must be a JSON object"));
        }
        let body_name = name_of(&new);
        if !body_name.is_empty() && body_name != url_name {
            return Err(ApiError::bad_request(format!(
                "the name of the object ({body_name}) does not match the name on the URL ({url_name})"
            )));
        }
        if sub == Some("status") {
            let mut result = old.clone();
            match new.get("status") {
                Some(status) => result["status"] = status.clone(),
                None => {
                    if let Some(m) = result.as_object_mut() {
                        m.remove("status");
                    }
                }
            }
            return Ok(result);
        }
        new["kind"] = json!(rt.kind);
        new["apiVersion"] = json!(rt.api_version());
        if !new.get("metadata").is_some_and(Value::is_object) {
            new["metadata"] = json!({});
        }
        let old_meta = meta(old).clone();
        {
            let m = new["metadata"].as_object_mut().unwrap();
            m.insert("name".into(), json!(url_name));
            for preserved in ["uid", "creationTimestamp", "namespace", "deletionTimestamp", "deletionGracePeriodSeconds"] {
                match old_meta.get(preserved) {
                    Some(v) => m.insert(preserved.into(), v.clone()),
                    None => m.remove(preserved),
                };
            }
            m.remove("resourceVersion");
            m.remove("managedFields");
        }
        if rt.has_status {
            match old.get("status") {
                Some(status) => new["status"] = status.clone(),
                None => {
                    new.as_object_mut().unwrap().remove("status");
                }
            }
            let generation = old_meta.get("generation").and_then(Value::as_i64).unwrap_or(1);
            let bumped = if spec_part(old) != spec_part(&new) { generation + 1 } else { generation };
            new["metadata"]["generation"] = json!(bumped);
        }
        self.default_by_kind(rt, &mut new, Some(old))?;
        Ok(new)
    }

    /// Per-kind defaults the real apiserver applies (a small subset).
    fn default_by_kind(&self, rt: &ResourceType, obj: &mut Value, old: Option<&Value>) -> Result<(), ApiError> {
        let name = name_of(obj);
        match (rt.group.as_str(), rt.kind.as_str()) {
            ("", "Namespace") => {
                if !obj["metadata"].get("labels").is_some_and(Value::is_object) {
                    obj["metadata"]["labels"] = json!({});
                }
                obj["metadata"]["labels"]["kubernetes.io/metadata.name"] = json!(name);
                if old.is_none() {
                    obj["spec"]["finalizers"] = json!(["kubernetes"]);
                    obj["status"] = json!({"phase": "Active"});
                }
            }
            ("", "Secret") => {
                if let Some(Value::Object(string_data)) = obj.as_object_mut().and_then(|m| m.remove("stringData")) {
                    let data = obj
                        .as_object_mut()
                        .unwrap()
                        .entry("data")
                        .or_insert_with(|| json!({}));
                    for (k, v) in string_data {
                        let raw = v.as_str().unwrap_or_default();
                        data[k] = json!(base64::engine::general_purpose::STANDARD.encode(raw));
                    }
                }
                if obj.get("type").and_then(Value::as_str).unwrap_or_default().is_empty() {
                    obj["type"] = json!("Opaque");
                }
            }
            ("", "Service") => self.default_service(obj, old),
            ("", "Pod") => {
                if old.is_none() {
                    let ns = meta(obj).get("namespace").and_then(Value::as_str).unwrap_or_default().to_string();
                    let sa_name = obj
                        .pointer("/spec/serviceAccountName")
                        .or_else(|| obj.pointer("/spec/serviceAccount"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("default")
                        .to_string();
                    let sa = self.store.get(&ObjectKey::new("", "serviceaccounts", &ns, &sa_name));
                    defaults::pod_on_create(obj, sa.as_ref(), &random_suffix());
                }
            }
            ("", "PersistentVolumeClaim") => defaults::pvc(obj, old),
            ("batch", "Job") => {
                defaults::job(obj, old);
                defaults::pod_template(obj, "/spec/template");
            }
            ("batch", "CronJob") => {
                defaults::cronjob(obj);
                defaults::pod_template(obj, "/spec/jobTemplate/spec/template");
            }
            ("apps", "DaemonSet") => {
                defaults::daemonset(obj);
                defaults::pod_template(obj, "/spec/template");
            }
            ("apps", "Deployment") => {
                defaults::pod_template(obj, "/spec/template");
                let spec = obj.as_object_mut().unwrap().entry("spec").or_insert_with(|| json!({}));
                if spec.get("replicas").is_none() {
                    spec["replicas"] = json!(1);
                }
                // As SetDefaults_Deployment: a RollingUpdate strategy always
                // has both bounds (Helm's readiness check dereferences them).
                if !spec.get("strategy").is_some_and(Value::is_object) {
                    spec["strategy"] = json!({});
                }
                let strategy = &mut spec["strategy"];
                if strategy.get("type").and_then(Value::as_str).is_none_or(str::is_empty) {
                    strategy["type"] = json!("RollingUpdate");
                }
                if strategy["type"] == "RollingUpdate" {
                    if !strategy.get("rollingUpdate").is_some_and(Value::is_object) {
                        strategy["rollingUpdate"] = json!({});
                    }
                    for key in ["maxSurge", "maxUnavailable"] {
                        if strategy["rollingUpdate"].get(key).is_none_or(Value::is_null) {
                            strategy["rollingUpdate"][key] = json!("25%");
                        }
                    }
                }
                if spec.get("revisionHistoryLimit").is_none() {
                    spec["revisionHistoryLimit"] = json!(10);
                }
                if spec.get("progressDeadlineSeconds").is_none() {
                    spec["progressDeadlineSeconds"] = json!(600);
                }
            }
            ("apps", "StatefulSet") => {
                defaults::statefulset(obj);
                defaults::pod_template(obj, "/spec/template");
            }
            ("apps", "ReplicaSet") => {
                defaults::pod_template(obj, "/spec/template");
                let spec = obj.as_object_mut().unwrap().entry("spec").or_insert_with(|| json!({}));
                if spec.get("replicas").is_none() {
                    spec["replicas"] = json!(1);
                }
            }
            ("apiextensions.k8s.io", "CustomResourceDefinition") => {
                let types = super::registry::crd_types(obj).map_err(|e| ApiError::invalid(rt, &name, e.to_string()))?;
                if !obj.pointer("/spec/conversion").is_some_and(Value::is_object) {
                    obj["spec"]["conversion"] = json!({"strategy": "None"});
                }
                let expected = format!("{}.{}", types[0].plural, types[0].group);
                if name != expected {
                    return Err(ApiError::invalid(
                        rt,
                        &name,
                        format!("metadata.name: Invalid value: must be spec.names.plural+\".\"+spec.group ({expected})"),
                    ));
                }
                let storage: Vec<Value> = obj
                    .pointer("/spec/versions")
                    .and_then(Value::as_array)
                    .map(|vs| {
                        vs.iter()
                            .filter(|v| v.get("storage").and_then(Value::as_bool) == Some(true))
                            .filter_map(|v| v.get("name").cloned())
                            .collect()
                    })
                    .unwrap_or_default();
                let now = now_rfc3339();
                obj["status"] = json!({
                    "acceptedNames": obj["spec"]["names"].clone(),
                    "storedVersions": storage,
                    "conditions": [
                        {"type": "NamesAccepted", "status": "True", "reason": "NoConflicts", "message": "no conflicts found", "lastTransitionTime": now},
                        {"type": "Established", "status": "True", "reason": "InitialNamesAccepted", "message": "the initial names have been accepted", "lastTransitionTime": now},
                    ],
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn default_service(&self, obj: &mut Value, old: Option<&Value>) {
        let spec = obj.as_object_mut().unwrap().entry("spec").or_insert_with(|| json!({}));
        if spec.get("type").and_then(Value::as_str).unwrap_or_default().is_empty() {
            spec["type"] = json!("ClusterIP");
        }
        let svc_type = spec["type"].as_str().unwrap_or("ClusterIP").to_string();
        if let Some(ports) = spec.get_mut("ports").and_then(Value::as_array_mut) {
            for port in ports {
                if port.get("protocol").is_none() {
                    port["protocol"] = json!("TCP");
                }
                if port.get("targetPort").is_none() {
                    port["targetPort"] = port.get("port").cloned().unwrap_or(Value::Null);
                }
            }
        }
        if svc_type == "ExternalName" {
            return;
        }
        // Keep the allocated ClusterIP across updates that omit it.
        let old_ip = old.and_then(|o| o.pointer("/spec/clusterIP")).cloned();
        let current = spec.get("clusterIP").and_then(Value::as_str).unwrap_or_default().to_string();
        if current.is_empty() {
            let ip = match old_ip {
                Some(Value::String(ip)) if !ip.is_empty() => ip,
                _ => self.allocate_cluster_ip(),
            };
            spec["clusterIP"] = json!(ip);
        }
        let ip = spec["clusterIP"].clone();
        spec["clusterIPs"] = json!([ip]);
        if spec["clusterIP"] != json!("None") {
            spec["ipFamilies"] = json!(["IPv4"]);
            spec["ipFamilyPolicy"] = json!("SingleStack");
        }
        if spec.get("sessionAffinity").is_none() {
            spec["sessionAffinity"] = json!("None");
        }
        if svc_type == "NodePort" || svc_type == "LoadBalancer" {
            let old_ports: Vec<Value> = old
                .and_then(|o| o.pointer("/spec/ports"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut allocated = self.recent_allocations();
            let mut used = self.used_node_ports();
            used.extend(allocated.iter().filter_map(|(a, _)| a.strip_prefix("port:")?.parse::<i64>().ok()));
            if let Some(ports) = spec.get_mut("ports").and_then(Value::as_array_mut) {
                for port in ports {
                    if port.get("nodePort").and_then(Value::as_i64).unwrap_or(0) != 0 {
                        continue;
                    }
                    let reuse = old_ports
                        .iter()
                        .find(|p| p.get("port") == port.get("port"))
                        .and_then(|p| p.get("nodePort"))
                        .and_then(Value::as_i64);
                    let np = reuse.unwrap_or_else(|| {
                        let np = (30000..=32767).find(|p| !used.contains(p)).unwrap_or(30000);
                        used.push(np);
                        allocated.push((format!("port:{np}"), std::time::Instant::now()));
                        np
                    });
                    port["nodePort"] = json!(np);
                }
            }
        }
        if old.is_none() {
            obj["status"] = json!({"loadBalancer": {}});
        }
    }

    /// The recent allocations, locked: allocate while holding it. Entries
    /// expire after a minute, long after the Service is stored or rejected.
    fn recent_allocations(&self) -> std::sync::MutexGuard<'_, Vec<(String, std::time::Instant)>> {
        let mut allocated = self.allocated.lock().unwrap();
        allocated.retain(|(_, at)| at.elapsed() < std::time::Duration::from_secs(60));
        allocated
    }

    fn allocate_cluster_ip(&self) -> String {
        let mut allocated = self.recent_allocations();
        let (services, _) = self.store.list("", "services", None);
        let mut used: Vec<String> = services
            .iter()
            .filter_map(|s| s.pointer("/spec/clusterIP").and_then(Value::as_str).map(str::to_string))
            .collect();
        used.extend(allocated.iter().filter_map(|(a, _)| a.strip_prefix("ip:").map(str::to_string)));
        for n in 10u32..65_000 {
            let ip = format!("{SERVICE_CIDR_PREFIX}.{}.{}", n / 256, n % 256);
            if !used.contains(&ip) {
                allocated.push((format!("ip:{ip}"), std::time::Instant::now()));
                return ip;
            }
        }
        format!("{SERVICE_CIDR_PREFIX}.255.254")
    }

    fn used_node_ports(&self) -> Vec<i64> {
        let (services, _) = self.store.list("", "services", None);
        services
            .iter()
            .flat_map(|s| {
                s.pointer("/spec/ports")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|p| p.get("nodePort").and_then(Value::as_i64))
            .collect()
    }

    fn after_write(&self, rt: &ResourceType, stored: &Value) {
        match (rt.group.as_str(), rt.plural.as_str()) {
            ("apiextensions.k8s.io", "customresourcedefinitions") => {
                if let Err(error) = self.registry.register_crd(stored) {
                    eprintln!("apiserver: failed to register CRD {}: {error:#}", name_of(stored));
                }
            }
            ("", "namespaces") => {
                let _ = self.ensure_default_service_account(&name_of(stored));
            }
            _ => {}
        }
    }

    fn after_delete(&self, rt: &ResourceType, gone: &Value) {
        if rt.group == "apiextensions.k8s.io" && rt.plural == "customresourcedefinitions" {
            if let Ok(types) = super::registry::crd_types(gone) {
                let (group, plural) = (types[0].group.clone(), types[0].plural.clone());
                self.registry.unregister_crd(&group, &plural);
                for key in self.store.keys_of_resource(&group, &plural) {
                    let _ = self.store.delete(&key, None);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Watch

    fn watch(&self, rt: &ResourceType, ns: Option<&str>, req: &Req) -> Result<Response<Body>, ApiError> {
        let labels = LabelSelector::parse(req.q("labelSelector").unwrap_or_default())
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        let fields = FieldSelector::parse(req.q("fieldSelector").unwrap_or_default())
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        let send_initial = req.q("sendInitialEvents") == Some("true");
        let since = match req.q("resourceVersion") {
            None | Some("0") => None,
            Some(_) if send_initial => None,
            Some(raw) => Some(raw.parse::<u64>().map_err(|_| ApiError::bad_request("invalid resourceVersion"))?),
        };
        let filter_ns = if rt.namespaced { ns.map(str::to_string) } else { None };
        let matches = {
            let (group, plural, labels, fields, filter_ns) =
                (rt.group.clone(), rt.plural.clone(), labels.clone(), fields.clone(), filter_ns.clone());
            move |e: &WatchEvent| {
                e.key.group == group
                    && e.key.resource == plural
                    && filter_ns.as_ref().is_none_or(|ns| &e.key.namespace == ns)
                    && labels.matches(&e.object)
                    && fields.matches(&e.object)
            }
        };

        let mut initial: VecDeque<Value> = VecDeque::new();
        let (subscription, floor) = match since {
            Some(rv) => (
                self.store.subscribe(Some(rv)).map_err(|e| ApiError::from_store(e, rt, ""))?,
                rv,
            ),
            None => {
                // Subscribe before listing so nothing falls in between; events
                // already covered by the list are skipped by rv below.
                let sub = self.store.subscribe(None).map_err(|e| ApiError::from_store(e, rt, ""))?;
                let (items, list_rv) = self.store.list(&rt.group, &rt.plural, filter_ns.as_deref());
                for obj in items.into_iter().filter(|o| labels.matches(o) && fields.matches(o)) {
                    initial.push_back(json!({"type": "ADDED", "object": obj}));
                }
                if send_initial {
                    initial.push_back(json!({"type": "BOOKMARK", "object": {
                        "kind": rt.kind, "apiVersion": rt.api_version(),
                        "metadata": {"resourceVersion": list_rv.to_string(), "annotations": {"k8s.io/initial-events-end": "true"}},
                    }}));
                }
                (sub, list_rv)
            }
        };

        let timeout = req
            .q("timeoutSeconds")
            .and_then(|t| t.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_WATCH_TIMEOUT);
        let bookmarks = req.q("allowWatchBookmarks") == Some("true");
        let as_table = req.wants_table();
        let as_partial = req.wants_partial();
        let rt = rt.clone();
        let (tx, rx) = mpsc::channel::<Bytes>(64);

        tokio::spawn(async move {
            let encode = |mut event: Value| -> Bytes {
                if as_table && event["type"] != "BOOKMARK" {
                    let obj = event["object"].take();
                    let rv = resource_version(&obj).unwrap_or(0);
                    event["object"] = table::build(&rt, &[obj], rv, true);
                } else if as_partial {
                    event["object"] = partial_metadata(&event["object"]);
                }
                let mut line = serde_json::to_vec(&event).unwrap_or_default();
                line.push(b'\n');
                Bytes::from(line)
            };
            for event in initial {
                if tx.send(encode(event)).await.is_err() {
                    return;
                }
            }
            let mut last_rv = floor;
            for e in &subscription.backlog {
                last_rv = last_rv.max(e.rv);
                if e.rv > floor && matches(e) {
                    let event = json!({"type": e.event.as_str(), "object": e.object});
                    if tx.send(encode(event)).await.is_err() {
                        return;
                    }
                }
            }
            let mut live = subscription.live;
            let deadline = tokio::time::Instant::now() + timeout;
            let mut tick = tokio::time::interval(Duration::from_secs(if bookmarks { 30 } else { 5 }));
            tick.tick().await;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => return,
                    _ = tick.tick() => {
                        if tx.is_closed() {
                            return;
                        }
                        if bookmarks {
                            let bookmark = json!({"type": "BOOKMARK", "object": {
                                "kind": rt.kind, "apiVersion": rt.api_version(),
                                "metadata": {"resourceVersion": last_rv.to_string()},
                            }});
                            if tx.send(encode(bookmark)).await.is_err() {
                                return;
                            }
                        }
                    }
                    received = live.recv() => match received {
                        Ok(e) => {
                            if e.rv <= last_rv {
                                continue;
                            }
                            last_rv = e.rv;
                            if matches(&e) {
                                let event = json!({"type": e.event.as_str(), "object": e.object});
                                if tx.send(encode(event)).await.is_err() {
                                    return;
                                }
                            }
                        }
                        // Fell behind or the store went away: end the watch;
                        // clients re-list and re-watch.
                        Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => return,
                    },
                }
            }
        });

        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|bytes| (Ok::<_, std::io::Error>(Frame::data(bytes)), rx))
        });
        let mut resp = Response::new(StreamBody::new(stream).boxed_unsync());
        resp.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(resp)
    }
}

// ---------------------------------------------------------------------------
// In-process API for controllers, the scheduler and the agent hub. Same
// defaulting and invariants as HTTP requests, without the HTTP.

impl ApiState {
    pub fn store(&self) -> &Store {
        &self.store
    }

    pub(crate) fn resource(&self, group: &str, plural: &str) -> Option<ResourceType> {
        self.registry.find_plural(group, plural)
    }

    fn internal_rt(&self, group: &str, plural: &str) -> Result<ResourceType, ApiError> {
        self.resource(group, plural).ok_or_else(ApiError::not_found_path)
    }

    pub(crate) fn get_object(&self, group: &str, plural: &str, ns: &str, name: &str) -> Option<Value> {
        self.store.get(&ObjectKey::new(group, plural, ns, name))
    }

    /// Create an object; `metadata.generateName` is honoured.
    pub(crate) fn create_object(&self, group: &str, plural: &str, ns: Option<&str>, mut obj: Value) -> Result<Value, ApiError> {
        let rt = self.internal_rt(group, plural)?;
        let generated = name_of(&obj).is_empty();
        self.prepare_create(&rt, ns, &mut obj, true)?;
        for _ in 0..5 {
            let name = name_of(&obj);
            match self.store.create(key_for(&rt, ns, &name), obj.clone()) {
                Ok(stored) => {
                    self.after_write(&rt, &stored);
                    return Ok(stored);
                }
                Err(StoreError::AlreadyExists) if generated => {
                    let base = meta(&obj).get("generateName").and_then(Value::as_str).unwrap_or_default().to_string();
                    obj["metadata"]["name"] = json!(format!("{base}{}", random_suffix()));
                }
                Err(e) => return Err(ApiError::from_store(e, &rt, &name)),
            }
        }
        Err(ApiError::from_store(StoreError::AlreadyExists, &rt, &name_of(&obj)))
    }

    /// Replace an object's spec and metadata (status is kept), guarded by
    /// the resourceVersion `obj` carries.
    pub(crate) fn update_object(&self, group: &str, plural: &str, obj: Value) -> Result<Value, ApiError> {
        self.internal_update(group, plural, obj, None)
    }

    /// Replace an object's status, guarded by the resourceVersion `obj` carries.
    pub(crate) fn update_status(&self, group: &str, plural: &str, obj: Value) -> Result<Value, ApiError> {
        self.internal_update(group, plural, obj, Some("status"))
    }

    fn internal_update(&self, group: &str, plural: &str, obj: Value, sub: Option<&str>) -> Result<Value, ApiError> {
        let rt = self.internal_rt(group, plural)?;
        let name = name_of(&obj);
        let ns = meta(&obj).get("namespace").and_then(Value::as_str).map(str::to_string);
        let key = key_for(&rt, ns.as_deref(), &name);
        let old = self
            .store
            .get(&key)
            .ok_or_else(|| ApiError::from_store(StoreError::NotFound, &rt, &name))?;
        let expected = resource_version(&obj);
        let new = self.prepare_update(&rt, &old, obj, sub, &name)?;
        self.commit_update(&rt, key, new, expected)
            .map_err(|e| ApiError::from_store(e, &rt, &name))
    }

    /// Delete an object (background propagation: the garbage collector
    /// removes dependents).
    pub(crate) fn delete_named(&self, group: &str, plural: &str, ns: &str, name: &str) -> Result<Value, ApiError> {
        let rt = self.internal_rt(group, plural)?;
        let key = key_for(&rt, Some(ns), name);
        let old = self
            .store
            .get(&key)
            .ok_or_else(|| ApiError::from_store(StoreError::NotFound, &rt, name))?;
        self.delete_object(&rt, &key, old, None)
            .map_err(|e| ApiError::from_store(e, &rt, name))
    }

    pub(crate) fn tls(&self) -> Option<&tls::ServerTls> {
        self.tls.as_ref()
    }
}

impl ApiError {
    pub(crate) fn is_conflict(&self) -> bool {
        self.code == StatusCode::CONFLICT && self.reason == "Conflict"
    }

    pub(crate) fn is_not_found(&self) -> bool {
        self.code == StatusCode::NOT_FOUND
    }

    pub(crate) fn is_already_exists(&self) -> bool {
        self.reason == "AlreadyExists"
    }
}

/// The user a service account token names (its `sub` claim), or admin.
fn token_user(token: &str) -> Value {
    let claims = token
        .split('.')
        .nth(1)
        .and_then(|p| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    match claims.as_ref().and_then(|c| c.get("sub")).and_then(Value::as_str) {
        Some(sub) => {
            let ns = sub.split(':').nth(2).unwrap_or_default();
            json!({"username": sub, "groups": ["system:serviceaccounts", format!("system:serviceaccounts:{ns}"), "system:authenticated"]})
        }
        None => json!({"username": "system:admin", "groups": ["system:masters", "system:authenticated"]}),
    }
}

/// Equal apart from the resourceVersion.
fn same_content(a: &Value, b: &Value) -> bool {
    let strip = |v: &Value| {
        let mut v = v.clone();
        if let Some(m) = v.get_mut("metadata").and_then(Value::as_object_mut) {
            m.remove("resourceVersion");
        }
        v
    };
    strip(a) == strip(b)
}

fn partial_metadata(obj: &Value) -> Value {
    json!({
        "kind": "PartialObjectMetadata",
        "apiVersion": "meta.k8s.io/v1",
        "metadata": obj.get("metadata").cloned().unwrap_or_else(|| json!({})),
    })
}

fn item_cursor(obj: &Value) -> String {
    let m = meta(obj);
    format!(
        "{}/{}",
        m.get("namespace").and_then(Value::as_str).unwrap_or_default(),
        m.get("name").and_then(Value::as_str).unwrap_or_default()
    )
}

/// The `autoscaling/v1` Scale view of a workload.
fn scale_of(obj: &Value) -> Value {
    let m = meta(obj);
    let selector = obj
        .pointer("/spec/selector/matchLabels")
        .and_then(Value::as_object)
        .map(|labels| {
            labels
                .iter()
                .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    json!({
        "kind": "Scale",
        "apiVersion": "autoscaling/v1",
        "metadata": {
            "name": m.get("name"),
            "namespace": m.get("namespace"),
            "uid": m.get("uid"),
            "resourceVersion": m.get("resourceVersion"),
            "creationTimestamp": m.get("creationTimestamp"),
        },
        "spec": {"replicas": obj.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or(0)},
        "status": {
            "replicas": obj.pointer("/status/replicas").and_then(Value::as_i64).unwrap_or(0),
            "selector": selector,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_query_strings() {
        let q = parse_query(Some("labelSelector=app%3Dweb%2Ctier+in+%28a%29&watch=true&x"));
        assert_eq!(q["labelSelector"], "app=web,tier in (a)");
        assert_eq!(q["watch"], "true");
        assert_eq!(q["x"], "");
    }
}
