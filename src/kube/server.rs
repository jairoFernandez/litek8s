//! HTTP front end of the Kubernetes-compatible API server.
//!
//! Serves discovery, the generic object API (get/list/watch/create/update/
//! patch/delete/deletecollection) for every registered resource type, and the
//! `status` and `scale` subresources. Objects are stored as JSON in
//! [`Store`]; nothing here runs containers yet, so pods stay Pending until the
//! node agent is wired to this object model.
//!
//! Not yet: TLS and authentication (bind to localhost only), protobuf,
//! admission webhooks, schema validation, managedFields, garbage collection
//! through ownerReferences, and the pod `log`/`exec`/`portforward` subresources.

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

use super::discovery;
use super::patch;
use super::protobuf;
use super::registry::{Registry, ResourceType};
use super::selector::{FieldSelector, LabelSelector};
use super::store::{ObjectKey, Store, StoreError, WatchEvent, resource_version};
use super::table;

type Body = UnsyncBoxBody<Bytes, std::io::Error>;

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
}

pub struct ApiState {
    store: Store,
    registry: Registry,
    server_address: String,
    log_requests: bool,
}

impl ApiState {
    pub fn open(data_dir: Option<&Path>, server_address: String, log_requests: bool) -> Result<Self> {
        let state = Self {
            store: Store::open(data_dir)?,
            registry: Registry::with_builtins(),
            server_address,
            log_requests,
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

pub async fn serve(config: ApiServerConfig) -> Result<()> {
    let listener = TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("failed to bind apiserver on {}", config.listen))?;
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
    eprintln!("apiserver: listening on http://{addr} (no TLS, no authentication)");
    serve_listener(state, listener).await
}

/// Accept connections on an already-bound listener (used by tests).
pub async fn serve_listener(state: Arc<ApiState>, listener: TcpListener) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let _ = stream.set_nodelay(true);
        let state = state.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(handle(state, req).await) }
            });
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
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

struct Req {
    method: Method,
    query: HashMap<String, String>,
    accept: String,
    content_type: String,
    body: Bytes,
}

impl Req {
    fn q(&self, key: &str) -> Option<&str> {
        self.query.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }

    fn dry_run(&self) -> bool {
        self.q("dryRun").is_some()
    }

    fn wants_table(&self) -> bool {
        self.accept.contains("as=Table")
    }
}

#[derive(Debug)]
struct ApiError {
    code: StatusCode,
    reason: &'static str,
    message: String,
    details: Option<Value>,
}

impl ApiError {
    fn new(code: StatusCode, reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            reason,
            message: message.into(),
            details: None,
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
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

fn full(bytes: impl Into<Bytes>) -> Body {
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

async fn handle(state: Arc<ApiState>, req: Request<Incoming>) -> Response<Body> {
    let started = Instant::now();
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
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
        accept: header(ACCEPT),
        content_type: header(CONTENT_TYPE),
        body,
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
            let mut req2 = Req {
                method: req.method.clone(),
                query: req.query.clone(),
                accept: req.accept.clone(),
                content_type: req.content_type.clone(),
                body: req.body.clone(),
            };
            req2.query
                .insert("fieldSelector".into(), format!("metadata.name={name}"));
            state.watch(rt, ns, &req2)
        }
        (&Method::POST, None, None) => {
            if rt.namespaced && ns.is_none() {
                return Err(ApiError::not_found_path());
            }
            state.create(rt, ns, req)
        }
        (&Method::DELETE, None, None) => state.delete_collection(rt, ns, req),
        (&Method::GET, Some(name), None | Some("status")) => state.get(rt, ns, name, req),
        (&Method::PUT, Some(name), None | Some("status") | Some("finalize")) => {
            state.update(rt, ns, name, target.sub.as_deref(), req)
        }
        (&Method::PATCH, Some(name), None | Some("status")) => {
            state.patch(rt, ns, name, target.sub.as_deref(), req)
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

    fn get(&self, rt: &ResourceType, ns: Option<&str>, name: &str, req: &Req) -> Result<Response<Body>, ApiError> {
        let obj = self
            .store
            .get(&key_for(rt, ns, name))
            .ok_or_else(|| ApiError::from_store(StoreError::NotFound, rt, name))?;
        if req.wants_table() {
            let rv = resource_version(&obj).unwrap_or(0);
            return Ok(json_response(StatusCode::OK, &table::build(rt, &[obj], rv, true)));
        }
        Ok(json_response(StatusCode::OK, &obj))
    }

    fn create(&self, rt: &ResourceType, ns: Option<&str>, req: &Req) -> Result<Response<Body>, ApiError> {
        let mut obj = parse_body(req)?;
        let generated = meta(&obj).get("name").and_then(Value::as_str).unwrap_or_default().is_empty();
        self.prepare_create(rt, ns, &mut obj, true)?;
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

    fn update(
        &self,
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
        if req.dry_run() {
            return Ok(json_response(StatusCode::OK, &new));
        }
        let stored = self
            .commit_update(rt, key, new, expected)
            .map_err(|e| ApiError::from_store(e, rt, name))?;
        Ok(json_response(StatusCode::OK, &stored))
    }

    fn patch(
        &self,
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
                    obj["status"] = json!({"phase": "Pending", "qosClass": "BestEffort"});
                }
            }
            ("apps", "Deployment") => {
                let spec = obj.as_object_mut().unwrap().entry("spec").or_insert_with(|| json!({}));
                if spec.get("replicas").is_none() {
                    spec["replicas"] = json!(1);
                }
                if spec.get("strategy").is_none() {
                    spec["strategy"] = json!({"type": "RollingUpdate", "rollingUpdate": {"maxSurge": "25%", "maxUnavailable": "25%"}});
                }
                if spec.get("revisionHistoryLimit").is_none() {
                    spec["revisionHistoryLimit"] = json!(10);
                }
                if spec.get("progressDeadlineSeconds").is_none() {
                    spec["progressDeadlineSeconds"] = json!(600);
                }
            }
            ("apps", "StatefulSet" | "ReplicaSet") => {
                let spec = obj.as_object_mut().unwrap().entry("spec").or_insert_with(|| json!({}));
                if spec.get("replicas").is_none() {
                    spec["replicas"] = json!(1);
                }
            }
            ("apiextensions.k8s.io", "CustomResourceDefinition") => {
                let types = super::registry::crd_types(obj).map_err(|e| ApiError::invalid(rt, &name, e.to_string()))?;
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
            let mut used = self.used_node_ports();
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

    fn allocate_cluster_ip(&self) -> String {
        let (services, _) = self.store.list("", "services", None);
        let used: Vec<String> = services
            .iter()
            .filter_map(|s| s.pointer("/spec/clusterIP").and_then(Value::as_str).map(str::to_string))
            .collect();
        for n in 10u32..65_000 {
            let ip = format!("{SERVICE_CIDR_PREFIX}.{}.{}", n / 256, n % 256);
            if !used.contains(&ip) {
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
        let rt = rt.clone();
        let (tx, rx) = mpsc::channel::<Bytes>(64);

        tokio::spawn(async move {
            let encode = |mut event: Value| -> Bytes {
                if as_table && event["type"] != "BOOKMARK" {
                    let obj = event["object"].take();
                    let rv = resource_version(&obj).unwrap_or(0);
                    event["object"] = table::build(&rt, &[obj], rv, true);
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
