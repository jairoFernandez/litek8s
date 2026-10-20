//! The Docker Engine API over HTTP/1.1 (on a Unix socket or loopback TCP):
//! routing, JSON shapes, streaming bodies and hijacked connections for
//! attach and exec.

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::{Method, Request, Response};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::attach;
use super::engine::{Engine, LogOpts};
use super::error::{ApiError, ApiResult};
use super::filters::{self, Filters};
use super::images;
use super::model::parse_time;
use crate::kube::agent::crun::image::{self as cimage, PullOptions};

pub const API_VERSION: &str = "1.47";
pub const MIN_API_VERSION: &str = "1.24";
pub const ENGINE_VERSION: &str = "27.5.1";

pub type Body = http_body_util::combinators::UnsyncBoxBody<Bytes, std::io::Error>;

fn full(bytes: impl Into<Bytes>) -> Body {
    http_body_util::Full::new(bytes.into()).map_err(|never: Infallible| match never {}).boxed_unsync()
}

fn empty() -> Body {
    full(Bytes::new())
}

fn base(status: u16) -> hyper::http::response::Builder {
    Response::builder()
        .status(status)
        .header("Api-Version", API_VERSION)
        .header("Docker-Experimental", "false")
        .header("Ostype", "linux")
        .header("Server", format!("Docker/{ENGINE_VERSION} (linux)"))
}

fn json_resp(status: u16, v: &Value) -> Response<Body> {
    base(status).header("Content-Type", "application/json").body(full(format!("{v}\n"))).unwrap()
}

fn no_content() -> Response<Body> {
    base(204).body(empty()).unwrap()
}

fn error_resp(e: &ApiError) -> Response<Body> {
    if e.status == 304 {
        return base(304).body(empty()).unwrap();
    }
    json_resp(e.status, &json!({"message": e.message}))
}

fn stream_resp(content_type: &str, s: impl Stream<Item = Bytes> + Send + 'static) -> Response<Body> {
    let body = StreamBody::new(s.map(|b| Ok::<_, std::io::Error>(Frame::data(b))));
    base(200).header("Content-Type", content_type).body(BodyExt::boxed_unsync(body)).unwrap()
}

fn query_of(req: &Request<Incoming>) -> HashMap<String, String> {
    let mut q = HashMap::new();
    for pair in req.uri().query().unwrap_or_default().split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        q.insert(decode(k), decode(v));
    }
    q
}

/// Percent-decoding of a query component (`+` is a space).
pub fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn qbool(q: &HashMap<String, String>, k: &str) -> bool {
    q.get(k).is_some_and(|v| matches!(v.as_str(), "1" | "true" | "True" | "yes"))
}

fn qint(q: &HashMap<String, String>, k: &str) -> Option<i64> {
    q.get(k).and_then(|v| v.parse().ok())
}

async fn body_bytes(req: Request<Incoming>) -> ApiResult<Bytes> {
    Ok(req.into_body().collect().await.map_err(|e| ApiError::bad_request(e.to_string()))?.to_bytes())
}

async fn body_json(req: Request<Incoming>) -> ApiResult<Value> {
    let b = body_bytes(req).await?;
    if b.iter().all(|c| c.is_ascii_whitespace()) {
        return Ok(json!({}));
    }
    Ok(serde_json::from_slice(&b)?)
}

/// Registry credentials from `X-Registry-Auth`.
fn registry_auth(req: &Request<Incoming>) -> Option<(String, String)> {
    use base64::Engine as _;
    let raw = req.headers().get("x-registry-auth")?.to_str().ok()?;
    let bytes = base64::engine::general_purpose::URL_SAFE.decode(raw.trim()).or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(raw.trim())).or_else(|_| base64::engine::general_purpose::STANDARD.decode(raw.trim())).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    if let Some(token) = v["identitytoken"].as_str().filter(|t| !t.is_empty()) {
        return Some(("<token>".into(), token.into()));
    }
    let user = v["username"].as_str().filter(|u| !u.is_empty())?;
    Some((user.to_string(), v["password"].as_str().unwrap_or_default().to_string()))
}

pub async fn serve_unix(engine: Arc<Engine>, listener: tokio::net::UnixListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let engine = engine.clone();
        tokio::spawn(serve_conn(engine, stream));
    }
}

pub async fn serve_tcp(engine: Arc<Engine>, listener: tokio::net::TcpListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let _ = stream.set_nodelay(true);
        let engine = engine.clone();
        tokio::spawn(serve_conn(engine, stream));
    }
}

async fn serve_conn<IO>(engine: Arc<Engine>, io: IO)
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let svc = hyper::service::service_fn(move |req| {
        let engine = engine.clone();
        async move { Ok::<_, Infallible>(route(engine, req).await) }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .preserve_header_case(true)
        .serve_connection(hyper_util::rt::TokioIo::new(io), svc)
        .with_upgrades()
        .await;
}

/// Path segments after the optional `/v1.xx` prefix.
fn segments(path: &str) -> Vec<String> {
    let mut segs: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(decode).collect();
    if segs.first().is_some_and(|s| s.starts_with('v') && s[1..].chars().all(|c| c.is_ascii_digit() || c == '.') && s.len() > 1) {
        segs.remove(0);
    }
    segs
}

async fn route(engine: Arc<Engine>, req: Request<Incoming>) -> Response<Body> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let segs = segments(&path);
    let s: Vec<&str> = segs.iter().map(String::as_str).collect();
    let verbose = std::env::var("LITEK8S_DOCKER_LOG").is_ok();
    if verbose {
        eprintln!("docker-api: {method} {}", req.uri());
    }
    let result = dispatch(&engine, method.clone(), &s, req).await;
    match result {
        Ok(r) => r,
        Err(e) => {
            if verbose || e.status >= 500 {
                eprintln!("docker-api: {method} {path}: {} {}", e.status, e.message);
            }
            error_resp(&e)
        }
    }
}

async fn dispatch(engine: &Arc<Engine>, method: Method, s: &[&str], req: Request<Incoming>) -> ApiResult<Response<Body>> {
    let q = query_of(&req);
    match (method.clone(), s) {
        (Method::GET | Method::HEAD, ["_ping"]) => Ok(base(200)
            .header("Content-Type", "text/plain; charset=utf-8")
            .header("Builder-Version", "1")
            .header("Cache-Control", "no-cache, no-store, must-revalidate")
            .header("Pragma", "no-cache")
            .body(if method == Method::HEAD { empty() } else { full("OK") })
            .unwrap()),
        (Method::GET, ["version"]) => Ok(json_resp(200, &version())),
        (Method::GET, ["info"]) => Ok(json_resp(200, &info(engine))),
        (Method::GET, ["events"]) => Ok(events(engine, &q)),
        (Method::GET, ["system", "df"]) => Ok(json_resp(200, &system_df(engine))),
        (Method::POST, ["auth"]) => {
            let _ = body_bytes(req).await;
            Ok(json_resp(200, &json!({"Status": "Login Succeeded", "IdentityToken": ""})))
        }
        (Method::GET, ["plugins"]) => Ok(json_resp(200, &json!([]))),
        (Method::GET, ["litek8s", "v1", "ports"]) => Ok(ports_watch(engine, qbool(&q, "watch"))),
        (Method::GET, ["litek8s", "v1", "binds"]) => Ok(binds_watch(engine)),
        (Method::POST, ["litek8s", "v1", "fsevents"]) => {
            let body = body_json(req).await?;
            let paths: Vec<String> = body.as_array().into_iter().flatten().filter_map(|p| p.as_str().map(str::to_string)).collect();
            let engine = engine.clone();
            tokio::task::spawn_blocking(move || engine.touch_paths(&paths));
            Ok(no_content())
        }

        // Containers
        (Method::GET, ["containers", "json"]) => Ok(json_resp(200, &list_containers(engine, &q))),
        (Method::POST, ["containers", "create"]) => {
            let body = body_json(req).await?;
            let (id, warnings) = engine.create(q.get("name").cloned(), body, q.get("platform").cloned().filter(|p| !p.is_empty())).await?;
            Ok(json_resp(201, &json!({"Id": id, "Warnings": warnings})))
        }
        (Method::POST, ["containers", "prune"]) => Ok(json_resp(200, &prune_containers(engine, &q).await)),
        (Method::GET, ["containers", id, "json"]) => {
            let c = engine.get(id)?;
            let rec = c.record();
            let mut v = rec.inspect(&engine.container_dir(&rec.id).to_string_lossy());
            let execs: Vec<String> = engine.execs.lock().unwrap().values().filter(|e| e.container == rec.id && *e.running.lock().unwrap()).map(|e| e.id.clone()).collect();
            if !execs.is_empty() {
                v["ExecIDs"] = json!(execs);
            }
            if qbool(&q, "size") {
                let dir = engine.container_dir(&rec.id).join("upper");
                v["SizeRw"] = json!(cimage::dir_bytes(&dir));
                v["SizeRootFs"] = json!(cimage::dir_bytes(&dir));
            }
            Ok(json_resp(200, &v))
        }
        (Method::GET, ["containers", id, "logs"]) => logs(engine, id, &q),
        (Method::POST, ["containers", id, "start"]) => {
            engine.start_container(id).await?;
            Ok(no_content())
        }
        (Method::POST, ["containers", id, "stop"]) => {
            engine.stop_container(id, qint(&q, "t").or_else(|| qint(&q, "timeout")), q.get("signal").cloned()).await?;
            Ok(no_content())
        }
        (Method::POST, ["containers", id, "restart"]) => {
            engine.restart_container(id, qint(&q, "t").or_else(|| qint(&q, "timeout")), q.get("signal").cloned()).await?;
            Ok(no_content())
        }
        (Method::POST, ["containers", id, "kill"]) => {
            engine.kill_container(id, q.get("signal").cloned()).await?;
            Ok(no_content())
        }
        (Method::POST, ["containers", id, "pause"]) => {
            engine.pause(id, true).await?;
            Ok(no_content())
        }
        (Method::POST, ["containers", id, "unpause"]) => {
            engine.pause(id, false).await?;
            Ok(no_content())
        }
        (Method::POST, ["containers", id, "wait"]) => {
            let condition = q.get("condition").cloned().unwrap_or_else(|| "not-running".into());
            // Docker answers the headers right away and the body when the
            // condition is met (the CLI waits before starting the container).
            engine.get(id)?;
            let engine = engine.clone();
            let id = id.to_string();
            let s = futures_util::stream::once(async move {
                let v = match engine.wait_container(&id, &condition).await {
                    Ok((code, err)) => json!({"StatusCode": code, "Error": err.map(|m| json!({"Message": m}))}),
                    Err(e) => json!({"StatusCode": -1, "Error": {"Message": e.message}}),
                };
                Bytes::from(format!("{v}\n"))
            });
            Ok(stream_resp("application/json", s))
        }
        (Method::DELETE, ["containers", id]) => {
            engine.remove_container(id, qbool(&q, "force"), qbool(&q, "v")).await?;
            Ok(no_content())
        }
        (Method::POST, ["containers", id, "rename"]) => {
            engine.rename(id, q.get("name").map(String::as_str).unwrap_or_default())?;
            Ok(no_content())
        }
        (Method::POST, ["containers", id, "resize"]) => {
            engine.resize(id, qint(&q, "h").unwrap_or(24) as u16, qint(&q, "w").unwrap_or(80) as u16)?;
            Ok(base(200).body(empty()).unwrap())
        }
        (Method::POST, ["containers", id, "update"]) => {
            let _ = body_bytes(req).await;
            engine.get(id)?;
            Ok(json_resp(200, &json!({"Warnings": []})))
        }
        (Method::POST, ["containers", id, "attach"]) => attach_container(engine, id, &q, req).await,
        (Method::POST, ["containers", id, "exec"]) => {
            let body = body_json(req).await?;
            let exec_id = engine.create_exec(id, &body)?;
            Ok(json_resp(201, &json!({"Id": exec_id})))
        }
        (Method::GET, ["containers", id, "top"]) => Ok(json_resp(200, &top(engine, id)?)),
        (Method::GET, ["containers", id, "stats"]) => stats(engine, id, &q),
        (Method::GET, ["containers", id, "changes"]) => {
            engine.get(id)?;
            Ok(json_resp(200, &json!([])))
        }
        (Method::HEAD | Method::GET | Method::PUT, ["containers", id, "archive"]) => archive(engine, id, method, &q, req).await,

        // Exec
        (Method::POST, ["exec", id, "start"]) => exec_start(engine, id, req).await,
        (Method::POST, ["exec", id, "resize"]) => {
            engine.resize_exec(id, qint(&q, "h").unwrap_or(24) as u16, qint(&q, "w").unwrap_or(80) as u16)?;
            Ok(base(200).body(empty()).unwrap())
        }
        (Method::GET, ["exec", id, "json"]) => Ok(json_resp(200, &engine.find_exec(id)?.inspect())),

        // Images
        (Method::GET, ["images", "json"]) => Ok(json_resp(200, &list_images(engine, &q))),
        (Method::POST, ["images", "create"]) => pull(engine, &q, &req),
        (Method::POST, ["images", "prune"]) => Ok(json_resp(200, &prune_images(engine, &q))),
        (Method::POST, ["images", "load"]) => load_images(engine, &q, req).await,
        (Method::GET, ["images", "search"]) => Ok(json_resp(200, &json!([]))),
        (Method::GET, ["images", "get"]) => {
            let names: Vec<String> = req.uri().query().unwrap_or_default().split('&').filter_map(|p| p.strip_prefix("names=")).map(decode).collect();
            save_images(engine, names)
        }
        (Method::GET, ["images", rest @ .., "get"]) if !rest.is_empty() => save_images(engine, vec![rest.join("/")]),
        (Method::GET, ["images", rest @ .., "json"]) if !rest.is_empty() => {
            let name = rest.join("/");
            let info = images::resolve(&engine.images, &name).ok_or_else(|| ApiError::not_found(format!("No such image: {name}")))?;
            Ok(json_resp(200, &images::inspect(&engine.images, &info)))
        }
        (Method::GET, ["images", rest @ .., "history"]) if !rest.is_empty() => {
            let name = rest.join("/");
            let info = images::resolve(&engine.images, &name).ok_or_else(|| ApiError::not_found(format!("No such image: {name}")))?;
            Ok(json_resp(200, &images::history(&engine.images, &info)))
        }
        (Method::POST, ["images", rest @ .., "tag"]) if !rest.is_empty() => {
            tag_image(engine, &rest.join("/"), q.get("repo").cloned().unwrap_or_default(), q.get("tag").cloned())?;
            Ok(base(201).body(empty()).unwrap())
        }
        (Method::POST, ["images", rest @ .., "push"]) if !rest.is_empty() => Err(ApiError::new(501, "litek8s cannot push images yet")),
        (Method::DELETE, ["images", rest @ ..]) if !rest.is_empty() => Ok(json_resp(200, &remove_image(engine, &rest.join("/"), qbool(&q, "force"))?)),
        (Method::GET, ["distribution", rest @ .., "json"]) if !rest.is_empty() => distribution(engine, &rest.join("/"), &req).await,

        // Build
        (Method::POST, ["build"]) => super::build::handle(engine.clone(), q, req).await,
        (Method::POST, ["build", "prune"]) => Ok(json_resp(200, &super::build::prune(engine))),
        (Method::POST, ["session"]) => Err(ApiError::new(
            501,
            "BuildKit is not supported by litek8s; build with the classic builder (DOCKER_BUILDKIT=0, and COMPOSE_BAKE=false for compose)",
        )),

        // Networks
        (Method::GET, ["networks"]) => Ok(json_resp(200, &list_networks(engine, &q))),
        (Method::POST, ["networks", "create"]) => {
            let body = body_json(req).await?;
            let (id, warning) = engine.create_network(&body).await?;
            Ok(json_resp(201, &json!({"Id": id, "Warning": warning})))
        }
        (Method::POST, ["networks", "prune"]) => Ok(json_resp(200, &prune_networks(engine, &q).await)),
        (Method::GET, ["networks", id]) => {
            let n = engine.find_network(id).ok_or_else(|| ApiError::not_found(format!("network {id} not found")))?;
            Ok(json_resp(200, &n.inspect(engine.network_containers(&n))))
        }
        (Method::DELETE, ["networks", id]) => {
            engine.remove_network(id).await?;
            Ok(no_content())
        }
        (Method::POST, ["networks", id, "connect"]) => {
            let body = body_json(req).await?;
            let ctr = body["Container"].as_str().unwrap_or_default().to_string();
            engine.connect(id, &ctr, &body["EndpointConfig"]).await?;
            Ok(base(200).body(empty()).unwrap())
        }
        (Method::POST, ["networks", id, "disconnect"]) => {
            let body = body_json(req).await?;
            let ctr = body["Container"].as_str().unwrap_or_default().to_string();
            engine.disconnect(id, &ctr).await?;
            Ok(base(200).body(empty()).unwrap())
        }

        // Volumes
        (Method::GET, ["volumes"]) => Ok(json_resp(200, &list_volumes(engine, &q))),
        (Method::POST, ["volumes", "create"]) => {
            let body = body_json(req).await?;
            let name = body["Name"].as_str().map(str::to_string);
            let opts: BTreeMap<String, String> = body["DriverOpts"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect()).unwrap_or_default();
            let v = engine.create_volume(name.as_deref(), super::filters::labels_of(&body["Labels"]), opts, false)?;
            Ok(json_resp(201, &v.inspect()))
        }
        (Method::POST, ["volumes", "prune"]) => Ok(json_resp(200, &prune_volumes(engine, &q))),
        (Method::GET, ["volumes", name]) => {
            let v = engine.volume(name).ok_or_else(|| ApiError::not_found(format!("get {name}: no such volume")))?;
            Ok(json_resp(200, &v.inspect()))
        }
        (Method::DELETE, ["volumes", name]) => {
            engine.remove_volume(name, qbool(&q, "force"))?;
            Ok(no_content())
        }
        (Method::GET, ["swarm"]) | (Method::GET, ["nodes"]) | (Method::GET, ["services"]) => {
            Err(ApiError::new(503, "This node is not a swarm manager. Use \"docker swarm init\" or \"docker swarm join\" to connect this node to swarm and try again."))
        }
        _ => Err(ApiError::not_found(format!("page not found: {method} /{}", s.join("/")))),
    }
}

fn version() -> Value {
    let (arch, _) = cimage::host_platform();
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim().to_string();
    json!({
        "Platform": {"Name": "litek8s"},
        "Components": [{"Name": "Engine", "Version": ENGINE_VERSION, "Details": {"ApiVersion": API_VERSION, "MinAPIVersion": MIN_API_VERSION, "Os": "linux", "Arch": arch, "KernelVersion": kernel, "Experimental": "false", "GitCommit": "litek8s", "GoVersion": "n/a"}}],
        "Version": ENGINE_VERSION,
        "ApiVersion": API_VERSION,
        "MinAPIVersion": MIN_API_VERSION,
        "GitCommit": "litek8s",
        "GoVersion": "n/a",
        "Os": "linux",
        "Arch": arch,
        "KernelVersion": kernel,
        "BuildTime": "",
    })
}

fn info(engine: &Engine) -> Value {
    let recs: Vec<_> = engine.list().iter().map(|c| c.record()).collect();
    let running = recs.iter().filter(|r| r.state.status == "running").count();
    let paused = recs.iter().filter(|r| r.state.status == "paused").count();
    let mem_kb: i64 = std::fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:")?.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0);
    let (arch, _) = cimage::host_platform();
    let arch = match arch {
        "amd64" => "x86_64",
        "arm64" => "aarch64",
        a => a,
    };
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim().to_string();
    json!({
        "ID": "litek8s",
        "Containers": recs.len(),
        "ContainersRunning": running,
        "ContainersPaused": paused,
        "ContainersStopped": recs.len() - running - paused,
        "Images": images::all(&engine.images).len(),
        "Driver": "overlay2",
        "DriverStatus": [["Backing Filesystem", "overlay"]],
        "Plugins": {"Volume": ["local"], "Network": ["bridge", "host", "null"], "Authorization": null, "Log": ["json-file"]},
        "MemoryLimit": true,
        "SwapLimit": false,
        "CpuCfsPeriod": true,
        "CpuCfsQuota": true,
        "CPUShares": true,
        "CPUSet": false,
        "PidsLimit": true,
        "IPv4Forwarding": true,
        "Debug": false,
        "NFd": 0,
        "NGoroutines": 0,
        "SystemTime": super::model::now_rfc3339(),
        "LoggingDriver": "json-file",
        "CgroupDriver": "cgroupfs",
        "CgroupVersion": "2",
        "NEventsListener": 0,
        "KernelVersion": std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim(),
        "OperatingSystem": "litek8s",
        "OSVersion": env!("CARGO_PKG_VERSION"),
        "OSType": "linux",
        "Architecture": arch,
        "IndexServerAddress": "https://index.docker.io/v1/",
        "RegistryConfig": {"IndexConfigs": {"docker.io": {"Name": "docker.io", "Mirrors": [], "Secure": true, "Official": true}}, "InsecureRegistryCIDRs": ["127.0.0.0/8"]},
        "NCPU": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "MemTotal": mem_kb * 1024,
        "DockerRootDir": engine.root,
        "HttpProxy": "",
        "HttpsProxy": "",
        "NoProxy": "",
        "Name": name,
        "Labels": [],
        "ExperimentalBuild": false,
        "ServerVersion": ENGINE_VERSION,
        "Runtimes": {"crun": {"path": engine.crun}},
        "DefaultRuntime": "crun",
        "Swarm": {"NodeID": "", "NodeAddr": "", "LocalNodeState": "inactive", "ControlAvailable": false, "Error": "", "RemoteManagers": null},
        "LiveRestoreEnabled": false,
        "Isolation": "",
        "InitBinary": "",
        "SecurityOptions": ["name=seccomp,profile=builtin", "name=cgroupns"],
        "Warnings": [],
    })
}

fn system_df(engine: &Engine) -> Value {
    let all = images::all(&engine.images);
    // As Docker counts them: LayersSize is the store's unique bytes, an
    // image's SharedSize what it shares with other images.
    let mut users: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for i in &all {
        let mine: std::collections::BTreeSet<&str> = i.entry.layers.iter().map(String::as_str).collect();
        for l in mine {
            *users.entry(l).or_default() += 1;
        }
    }
    let layers: u64 = users.keys().map(|l| engine.images.layer_size(l)).sum();
    let imgs: Vec<Value> = all
        .iter()
        .map(|i| {
            let mut v = images::summary(&engine.images, i, -1);
            let mine: std::collections::BTreeSet<&str> = i.entry.layers.iter().map(String::as_str).collect();
            v["SharedSize"] = json!(mine.iter().filter(|l| users.get(*l).is_some_and(|n| *n > 1)).map(|l| engine.images.layer_size(l)).sum::<u64>());
            v
        })
        .collect();
    let ctrs: Vec<Value> = engine
        .list()
        .iter()
        .map(|c| {
            let r = c.record();
            let mut s = r.summary();
            s["SizeRw"] = json!(cimage::dir_bytes(&engine.container_dir(&r.id).join("upper")));
            s
        })
        .collect();
    let vols: Vec<Value> = engine
        .volumes()
        .iter()
        .map(|v| {
            let mut i = v.inspect();
            i["UsageData"] = json!({"Size": cimage::dir_bytes(std::path::Path::new(&v.mountpoint)), "RefCount": engine.volume_users(&v.name).len()});
            i
        })
        .collect();
    json!({"LayersSize": layers, "Images": imgs, "Containers": ctrs, "Volumes": vols, "BuildCache": super::build::usage(engine)})
}

fn events(engine: &Arc<Engine>, q: &HashMap<String, String>) -> Response<Body> {
    let filters = Filters::parse(q.get("filters").map(String::as_str));
    let since = q.get("since").and_then(|s| s.split('.').next()?.parse::<i64>().ok());
    let until = q.get("until").and_then(|s| s.split('.').next()?.parse::<i64>().ok());
    let matches = move |ev: &Value| -> bool {
        let kind = ev["Type"].as_str().unwrap_or_default();
        let action = ev["Action"].as_str().unwrap_or_default();
        let attrs = super::filters::labels_of(&ev["Actor"]["Attributes"]);
        let id = ev["Actor"]["ID"].as_str().unwrap_or_default();
        filters.any("type", |t| t == kind)
            && filters.any("event", |e| action == e || action.starts_with(&format!("{e}:")))
            && filters.labels_match("label", &attrs)
            && filters.any("container", |c| kind == "container" && (id.starts_with(c) || attrs.get("name").is_some_and(|n| n == c)))
            && filters.any("image", |i| attrs.get("image").is_some_and(|n| n == i) || id == i)
            && filters.any("network", |n| kind == "network" && (id.starts_with(n) || attrs.get("name").is_some_and(|x| x == n)))
            && filters.any("volume", |v| kind == "volume" && id == v)
    };
    let rx = engine.events.subscribe();
    let past: Vec<Value> = match since {
        Some(t) => engine.recent_events().into_iter().filter(|e| e["time"].as_i64().unwrap_or(0) >= t).collect(),
        None => Vec::new(),
    };
    let now = chrono::Utc::now().timestamp();
    let only_past = until.is_some_and(|u| u <= now);
    let m2 = matches.clone();
    let past_stream = futures_util::stream::iter(past.into_iter().filter(move |e| m2(e) && until.is_none_or(|u| e["time"].as_i64().unwrap_or(0) <= u)).map(|e| Bytes::from(format!("{e}\n"))));
    let live = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => return Some((ev, rx)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return None,
            }
        }
    });
    let deadline = until.map(|u| tokio::time::Instant::now() + Duration::from_secs((u - now).max(0) as u64));
    let live = live.filter(move |e| futures_util::future::ready(matches(e))).map(|e| Bytes::from(format!("{e}\n")));
    let live: futures_util::stream::BoxStream<'static, Bytes> = match deadline {
        Some(d) => live.take_until(tokio::time::sleep_until(d)).boxed(),
        None => live.boxed(),
    };
    let body = if only_past { past_stream.boxed() } else { past_stream.chain(live).boxed() };
    stream_resp("application/json", body)
}

/// The published host ports, as a stream of JSON arrays when watching (the
/// Mac side of the dev VM forwards them).
fn ports_watch(engine: &Engine, watch: bool) -> Response<Body> {
    let rx = engine.published.subscribe();
    if !watch {
        return json_resp(200, &json!(*rx.borrow()));
    }
    let s = futures_util::stream::unfold((rx, true), |(mut rx, first)| async move {
        if !first && rx.changed().await.is_err() {
            return None;
        }
        let v = json!(*rx.borrow_and_update());
        Some((Bytes::from(format!("{v}\n")), (rx, false)))
    });
    stream_resp("application/json", s)
}

fn binds_watch(engine: &Engine) -> Response<Body> {
    let rx = engine.binds.subscribe();
    let s = futures_util::stream::unfold((rx, true), |(mut rx, first)| async move {
        if !first && rx.changed().await.is_err() {
            return None;
        }
        let v = json!(*rx.borrow_and_update());
        Some((Bytes::from(format!("{v}\n")), (rx, false)))
    });
    stream_resp("application/json", s)
}

fn list_containers(engine: &Engine, q: &HashMap<String, String>) -> Value {
    let f = Filters::parse(q.get("filters").map(String::as_str));
    let all = qbool(q, "all");
    let recs: Vec<_> = engine.list().iter().map(|c| c.record()).collect();
    let created_of = |key: &str| recs.iter().find(|r| r.id.starts_with(key) || r.name == key.trim_start_matches('/')).map(|r| r.created.clone());
    let before = f.get("before").first().and_then(|k| created_of(k));
    let since = f.get("since").first().and_then(|k| created_of(k));
    let mut out = Vec::new();
    for r in &recs {
        if !all && !r.state.running() && !f.has("status") && !f.has("exited") {
            continue;
        }
        let labels = r.labels();
        let ok = f.any("id", |v| filters::id_matches(v, &r.id))
            && f.any("name", |v| filters::name_matches(v, &r.name))
            && f.labels_match("label", &labels)
            && f.any("status", |v| r.state.status == v)
            && f.any("exited", |v| r.state.status == "exited" && v.parse::<i64>().ok() == Some(r.state.exit_code))
            && f.any("ancestor", |v| r.image == v || r.image_id.trim_start_matches("sha256:").starts_with(v.trim_start_matches("sha256:")) || images::key_for(v).is_some_and(|k| images::key_for(&r.image).as_deref() == Some(k.as_str())))
            && f.any("network", |v| r.endpoints.iter().any(|e| e.network_name == v || e.network_id.starts_with(v)))
            && f.any("volume", |v| r.mounts.iter().any(|m| m.name.as_deref() == Some(v) || m.destination == v))
            && f.any("health", |v| match &r.state.health {
                Some(h) => h.status == v,
                None => v == "none",
            })
            && before.as_ref().is_none_or(|b| r.created < *b)
            && since.as_ref().is_none_or(|s| r.created > *s);
        if ok {
            let mut s = r.summary();
            if qbool(q, "size") {
                s["SizeRw"] = json!(cimage::dir_bytes(&engine.container_dir(&r.id).join("upper")));
            }
            out.push(s);
        }
    }
    if let Some(limit) = qint(q, "limit").filter(|l| *l > 0) {
        out.truncate(limit as usize);
    }
    Value::Array(out)
}

async fn prune_containers(engine: &Arc<Engine>, q: &HashMap<String, String>) -> Value {
    let f = Filters::parse(q.get("filters").map(String::as_str));
    let mut deleted = Vec::new();
    let mut space = 0u64;
    for c in engine.list() {
        let r = c.record();
        if r.state.running() || !f.labels_match("label", &r.labels()) {
            continue;
        }
        let size = cimage::dir_bytes(&engine.container_dir(&r.id).join("upper"));
        if engine.remove_container(&r.id, false, false).await.is_ok() {
            deleted.push(r.id);
            space += size;
        }
    }
    json!({"ContainersDeleted": deleted, "SpaceReclaimed": space})
}

fn logs(engine: &Arc<Engine>, id: &str, q: &HashMap<String, String>) -> ApiResult<Response<Body>> {
    let ctr = engine.get(id)?;
    let tty = ctr.record().tty();
    let ts = |k: &str| -> Option<chrono::DateTime<chrono::Utc>> {
        let v = q.get(k)?;
        if v.is_empty() || v == "0" {
            return None;
        }
        let (secs, frac) = v.split_once('.').unwrap_or((v, "0"));
        let secs: i64 = secs.parse().ok()?;
        let nanos: u32 = format!("{frac:0<9}")[..9].parse().unwrap_or(0);
        chrono::DateTime::from_timestamp(secs, nanos).or_else(|| parse_time(v))
    };
    let opts = LogOpts {
        follow: qbool(q, "follow"),
        stdout: qbool(q, "stdout"),
        stderr: qbool(q, "stderr"),
        timestamps: qbool(q, "timestamps"),
        tail: q.get("tail").and_then(|t| t.parse::<usize>().ok()),
        since: ts("since"),
        until: ts("until"),
    };
    let stream = engine.logs(ctr, opts).map(move |(stream, data)| {
        if tty {
            Bytes::from(data)
        } else {
            let mut framed = attach::mux_header(stream, data.len()).to_vec();
            framed.extend(data);
            Bytes::from(framed)
        }
    });
    Ok(stream_resp(if tty { "application/vnd.docker.raw-stream" } else { "application/vnd.docker.multiplexed-stream" }, stream))
}

fn top(engine: &Engine, id: &str) -> ApiResult<Value> {
    let ctr = engine.get(id)?;
    let rec = ctr.record();
    if !rec.state.running() {
        return Err(ApiError::conflict(format!("container {} is not running", rec.id)));
    }
    let procs = std::fs::read_to_string(format!("/sys/fs/cgroup/litek8s/docker/{}/cgroup.procs", rec.id)).unwrap_or_default();
    let mut rows = Vec::new();
    for pid in procs.split_whitespace() {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let ppid = stat.rsplit(')').next().and_then(|r| r.split_whitespace().nth(1)).unwrap_or("0").to_string();
        let uid = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default().lines().find_map(|l| l.strip_prefix("Uid:").map(|u| u.split_whitespace().next().unwrap_or("0").to_string())).unwrap_or_default();
        let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).map(|c| String::from_utf8_lossy(&c).replace('\0', " ").trim().to_string()).unwrap_or_default();
        rows.push(json!([uid, pid, ppid, "0", "", "?", "00:00:00", cmd]));
    }
    Ok(json!({"Titles": ["UID", "PID", "PPID", "C", "STIME", "TTY", "TIME", "CMD"], "Processes": rows}))
}

fn stats(engine: &Arc<Engine>, id: &str, q: &HashMap<String, String>) -> ApiResult<Response<Body>> {
    let ctr = engine.get(id)?;
    let stream = q.get("stream").is_none_or(|v| v != "0" && v != "false") && !qbool(q, "one-shot");
    let id = ctr.id.clone();
    let name = ctr.record().name;
    let sample = move |id: &str| -> (u64, u64, u64, u64) {
        let dir = format!("/sys/fs/cgroup/litek8s/docker/{id}");
        let cpu = std::fs::read_to_string(format!("{dir}/cpu.stat")).unwrap_or_default().lines().find_map(|l| l.strip_prefix("usage_usec ")?.trim().parse::<u64>().ok()).unwrap_or(0) * 1000;
        let mem = std::fs::read_to_string(format!("{dir}/memory.current")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let limit = std::fs::read_to_string(format!("{dir}/memory.max")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or_else(|| {
            std::fs::read_to_string("/proc/meminfo").unwrap_or_default().lines().find_map(|l| l.strip_prefix("MemTotal:")?.trim().trim_end_matches("kB").trim().parse::<u64>().ok()).unwrap_or(0) * 1024
        });
        let system = std::fs::read_to_string("/proc/stat").unwrap_or_default().lines().next().map(|l| l.split_whitespace().skip(1).filter_map(|n| n.parse::<u64>().ok()).sum::<u64>() * 10_000_000).unwrap_or(0);
        (cpu, system, mem, limit)
    };
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let s = futures_util::stream::unfold((None::<(u64, u64)>, true), move |(prev, first)| {
        let id = id.clone();
        let name = name.clone();
        async move {
            if !first {
                if !stream {
                    return None;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let (cpu, system, mem, limit) = sample(&id);
            let (pcpu, psys) = prev.unwrap_or((0, 0));
            let v = json!({
                "read": super::model::now_rfc3339(),
                "preread": super::model::ZERO_TIME,
                "id": id,
                "name": format!("/{name}"),
                "pids_stats": {},
                "cpu_stats": {"cpu_usage": {"total_usage": cpu}, "system_cpu_usage": system, "online_cpus": cpus},
                "precpu_stats": {"cpu_usage": {"total_usage": pcpu}, "system_cpu_usage": psys, "online_cpus": cpus},
                "memory_stats": {"usage": mem, "limit": limit, "stats": {}},
                "networks": {},
            });
            Some((Bytes::from(format!("{v}\n")), (Some((cpu, system)), false)))
        }
    });
    Ok(stream_resp("application/json", s))
}

/// Whether the client asked to take over the connection.
fn wants_upgrade(req: &Request<Incoming>) -> bool {
    req.headers().get(hyper::header::UPGRADE).is_some()
        || req.headers().get(hyper::header::CONNECTION).and_then(|c| c.to_str().ok()).is_some_and(|c| c.to_ascii_lowercase().contains("upgrade"))
}

fn hijack_response(tty: bool) -> Response<Body> {
    base(101)
        .header("Content-Type", if tty { "application/vnd.docker.raw-stream" } else { "application/vnd.docker.multiplexed-stream" })
        .header(hyper::header::CONNECTION, "Upgrade")
        .header(hyper::header::UPGRADE, "tcp")
        .body(empty())
        .unwrap()
}

async fn attach_container(engine: &Arc<Engine>, id: &str, q: &HashMap<String, String>, req: Request<Incoming>) -> ApiResult<Response<Body>> {
    let ctr = engine.get(id)?;
    let rec = ctr.record();
    let tty = rec.tty();
    let want_stdin = qbool(q, "stdin") && rec.config["OpenStdin"].as_bool().unwrap_or(false);
    let want_stdout = qbool(q, "stdout") || !q.contains_key("stdout");
    let want_stderr = qbool(q, "stderr") || !q.contains_key("stderr");
    let stream = qbool(q, "stream");
    let replay = qbool(q, "logs");
    let detach_keys = q.get("detachKeys").cloned().unwrap_or_else(|| "ctrl-p,ctrl-q".into());
    // Before the container runs, the attach waits for its start: the shim
    // then waits for us before starting the process.
    let gen_before = {
        let mut rt = ctr.rt.lock().unwrap();
        if !rec.state.running() && stream {
            rt.pending_attach += 1;
        }
        rt.attach_gen
    };
    let pending = !rec.state.running() && stream;
    let upgrade = wants_upgrade(&req);
    let engine = engine.clone();
    let sock = engine.attach_socket(&ctr.id);
    let ctr2 = ctr.clone();
    let connect = async move {
        if pending {
            let mut rx = ctr2.changed.subscribe();
            loop {
                if ctr2.rt.lock().unwrap().attach_gen != gen_before {
                    break;
                }
                if *rx.borrow() == u64::MAX || rx.changed().await.is_err() {
                    return None;
                }
            }
        }
        if !stream {
            return None;
        }
        attach::connect(&sock, Duration::from_secs(10)).await.ok()
    };
    let log_replay = if replay {
        let opts = LogOpts { stdout: want_stdout, stderr: want_stderr, ..Default::default() };
        Some(engine.logs(ctr.clone(), opts))
    } else {
        None
    };
    // Output the client gets: shim frames -> Docker frames (or raw).
    let to_client = move |kind: u8, data: &[u8]| -> Option<Vec<u8>> {
        if (kind == 1 && !want_stdout) || (kind == 2 && !want_stderr) {
            return Some(Vec::new());
        }
        if tty {
            Some(data.to_vec())
        } else {
            let mut v = attach::mux_header(kind, data.len()).to_vec();
            v.extend_from_slice(data);
            Some(v)
        }
    };
    if upgrade {
        let on_upgrade = hyper::upgrade::on(req);
        let ctr3 = ctr.clone();
        tokio::spawn(async move {
            let Ok(upgraded) = on_upgrade.await else {
                if pending {
                    let mut rt = ctr3.rt.lock().unwrap();
                    rt.pending_attach = rt.pending_attach.saturating_sub(1);
                }
                return;
            };
            let client = hyper_util::rt::TokioIo::new(upgraded);
            let (mut client_r, mut client_w) = tokio::io::split(client);
            if let Some(mut replay) = log_replay {
                while let Some((kind, data)) = replay.next().await {
                    if let Some(out) = to_client(kind, &data) {
                        if client_w.write_all(&out).await.is_err() {
                            return;
                        }
                    }
                }
            }
            let debug = std::env::var("LITEK8S_DOCKER_LOG").is_ok();
            if debug {
                eprintln!("docker-api: attach upgraded, connecting to the shim");
            }
            let Some(shim) = connect.await else {
                if debug {
                    eprintln!("docker-api: attach: no shim");
                }
                let _ = client_w.shutdown().await;
                return;
            };
            if debug {
                eprintln!("docker-api: attach: connected");
            }
            let (mut shim_r, mut shim_w) = shim.into_split();
            let mut detach = attach::DetachKeys::parse(if want_stdin && tty { &detach_keys } else { "" });
            // Ends (true) only on the detach keys; end of input closes the
            // container's stdin and output keeps coming.
            let input = async move {
                if !want_stdin {
                    // The write half stays open: closing it would tell the
                    // shim this client is gone.
                    let _keep = shim_w;
                    let mut buf = [0u8; 1024];
                    while matches!(client_r.read(&mut buf).await, Ok(n) if n > 0) {}
                    return false;
                }
                let mut buf = vec![0u8; 32 * 1024];
                loop {
                    match client_r.read(&mut buf).await {
                        Ok(0) | Err(_) => {
                            let _ = shim_w.write_all(&attach::frame(1, b"")).await;
                            // Dropping the write half after this is fine:
                            // the shim keeps sending output.
                            std::future::pending::<()>().await;
                        }
                        Ok(n) => {
                            let mut out = Vec::new();
                            let detached = detach.feed(&buf[..n], &mut out);
                            if !out.is_empty() && shim_w.write_all(&attach::frame(0, &out)).await.is_err() {
                                return false;
                            }
                            if detached {
                                return true;
                            }
                        }
                    }
                }
            };
            let output = async move {
                while let Some((kind, data)) = attach::read_frame(&mut shim_r).await {
                    if debug {
                        eprintln!("docker-api: attach frame {kind} {} bytes", data.len());
                    }
                    if kind == 3 {
                        break;
                    }
                    if let Some(out) = to_client(kind, &data) {
                        if !out.is_empty() && client_w.write_all(&out).await.is_err() {
                            break;
                        }
                    }
                }
                let _ = client_w.flush().await;
                let _ = client_w.shutdown().await;
            };
            tokio::select! {
                _ = output => {}
                _ = async { if !input.await { std::future::pending::<()>().await } } => {}
            }
        });
        return Ok(hijack_response(tty));
    }
    // A plain (non-upgraded) attach: output only, as a streamed body.
    let s = async_stream_attach(connect, log_replay, to_client);
    Ok(stream_resp(if tty { "application/vnd.docker.raw-stream" } else { "application/vnd.docker.multiplexed-stream" }, s))
}

fn async_stream_attach(
    connect: impl std::future::Future<Output = Option<tokio::net::UnixStream>> + Send + 'static,
    replay: Option<futures_util::stream::BoxStream<'static, (u8, Vec<u8>)>>,
    to_client: impl Fn(u8, &[u8]) -> Option<Vec<u8>> + Send + Sync + 'static,
) -> impl Stream<Item = Bytes> + Send + 'static {
    let to_client = Arc::new(to_client);
    let t1 = to_client.clone();
    let replayed = futures_util::stream::iter(replay).flatten().map(move |(k, d)| Bytes::from(t1(k, &d).unwrap_or_default()));
    let live = futures_util::stream::once(connect)
        .filter_map(|s| async move { s })
        .flat_map(move |shim| {
            let t = to_client.clone();
            futures_util::stream::unfold(Some(shim), move |state| {
                let t = t.clone();
                async move {
                    let mut shim = state?;
                    loop {
                        let (kind, data) = attach::read_frame(&mut shim).await?;
                        if kind == 3 {
                            return None;
                        }
                        if let Some(out) = t(kind, &data).filter(|o| !o.is_empty()) {
                            return Some((Bytes::from(out), Some(shim)));
                        }
                    }
                }
            })
        });
    replayed.chain(live).filter(|b| futures_util::future::ready(!b.is_empty()))
}

async fn exec_start(engine: &Arc<Engine>, id: &str, req: Request<Incoming>) -> ApiResult<Response<Body>> {
    let upgrade = wants_upgrade(&req);
    let (parts, body) = req.into_parts();
    let bytes = body.collect().await.map_err(|e| ApiError::bad_request(e.to_string()))?.to_bytes();
    let cfg: Value = if bytes.iter().all(|b| b.is_ascii_whitespace()) { json!({}) } else { serde_json::from_slice(&bytes)? };
    let detach = cfg["Detach"].as_bool().unwrap_or(false);
    let tty = cfg["Tty"].as_bool();
    let inst = engine.find_exec(id)?;
    if let Some(size) = cfg["ConsoleSize"].as_array().and_then(|a| Some((a.first()?.as_u64()? as u16, a.get(1)?.as_u64()? as u16))) {
        *inst.size.lock().unwrap() = Some(size);
    }
    let io = engine.start_exec(id, detach, tty).await?;
    if detach {
        return Ok(base(200).header("Content-Type", "application/json").body(empty()).unwrap());
    }
    let tty = io.tty;
    let frame = move |kind: u8, data: &[u8]| -> Vec<u8> {
        if tty {
            data.to_vec()
        } else {
            let mut v = attach::mux_header(kind, data.len()).to_vec();
            v.extend_from_slice(data);
            v
        }
    };
    if upgrade {
        let mut req = Request::from_parts(parts, ());
        let on_upgrade = hyper::upgrade::on(&mut req);
        tokio::spawn(async move {
            let Ok(upgraded) = on_upgrade.await else { return };
            let client = hyper_util::rt::TokioIo::new(upgraded);
            let (mut client_r, mut client_w) = tokio::io::split(client);
            let mut input = io.input;
            let mut output = io.output;
            let pump_in = async move {
                let mut buf = vec![0u8; 32 * 1024];
                loop {
                    match client_r.read(&mut buf).await {
                        Ok(0) | Err(_) => {
                            let _ = input.shutdown().await;
                            return;
                        }
                        Ok(n) => {
                            if input.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                            let _ = input.flush().await;
                        }
                    }
                }
            };
            let pump_out = async move {
                while let Some((kind, data)) = output.next().await {
                    if client_w.write_all(&frame(kind, &data)).await.is_err() {
                        break;
                    }
                }
                let _ = client_w.flush().await;
                let _ = client_w.shutdown().await;
            };
            tokio::select! {
                _ = pump_out => {}
                _ = async { pump_in.await; std::future::pending::<()>().await } => {}
            }
        });
        return Ok(hijack_response(tty));
    }
    let s = io.output.map(move |(k, d)| Bytes::from(frame(k, &d)));
    Ok(stream_resp(if tty { "application/vnd.docker.raw-stream" } else { "application/vnd.docker.multiplexed-stream" }, s))
}

async fn archive(engine: &Arc<Engine>, id: &str, method: Method, q: &HashMap<String, String>, req: Request<Incoming>) -> ApiResult<Response<Body>> {
    use base64::Engine as _;
    let ctr = engine.get(id)?;
    let path = q.get("path").cloned().unwrap_or_default();
    if path.is_empty() {
        return Err(ApiError::bad_request("path is required"));
    }
    let rec = ctr.record();
    let mut cmd = tokio::process::Command::new(if cfg!(target_os = "linux") { "/proc/self/exe" } else { "litek8s" });
    cmd.arg("docker-fs");
    if rec.state.running() && rec.state.pid > 0 {
        cmd.arg("--pid").arg(rec.state.pid.to_string());
    } else {
        let dir = engine.container_dir(&rec.id);
        let opts = engine.overlay_options_for(&rec).map_err(ApiError::from)?;
        cmd.arg("--rootfs").arg(dir.join("rootfs")).arg("--overlay").arg(opts);
        for m in rec.mounts.iter().filter(|m| m.kind != "tmpfs") {
            let _ = std::fs::create_dir_all(&m.source);
            cmd.arg("--bind").arg(format!("{}={}{}", m.source, m.destination, if m.rw { "" } else { ":ro" }));
        }
    }
    let stat = {
        let mut c = std::process::Command::new(cmd.as_std().get_program());
        c.args(cmd.as_std().get_args()).arg("stat").arg(&path);
        tokio::process::Command::from(c).output().await?
    };
    if stat.status.code() == Some(2) {
        return Err(ApiError::not_found(format!("Could not find the file {path} in container {}", rec.name)));
    }
    if !stat.status.success() {
        return Err(ApiError::new(500, String::from_utf8_lossy(&stat.stderr).trim().to_string()));
    }
    let header = base64::engine::general_purpose::STANDARD.encode(String::from_utf8_lossy(&stat.stdout).trim());
    match method {
        Method::HEAD => Ok(base(200).header("X-Docker-Container-Path-Stat", header).body(empty()).unwrap()),
        Method::GET => {
            let mut child = cmd.arg("get").arg(&path).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn()?;
            let out = child.stdout.take().unwrap();
            let s = futures_util::stream::unfold((out, child), |(mut out, child)| async move {
                let mut buf = vec![0u8; 256 * 1024];
                match out.read(&mut buf).await {
                    Ok(0) | Err(_) => None,
                    Ok(n) => {
                        buf.truncate(n);
                        Some((Bytes::from(buf), (out, child)))
                    }
                }
            });
            let body = StreamBody::new(s.map(|b| Ok::<_, std::io::Error>(Frame::data(b))));
            Ok(base(200).header("Content-Type", "application/x-tar").header("X-Docker-Container-Path-Stat", header).body(BodyExt::boxed_unsync(body)).unwrap())
        }
        _ => {
            cmd.arg("put").arg(&path);
            if qbool(q, "noOverwriteDirNonDir") {
                cmd.arg("--no-overwrite-dir-non-dir");
            }
            if qbool(q, "copyUIDGID") {
                if let Some(u) = rec.config["User"].as_str().filter(|u| !u.is_empty()) {
                    if let (Ok(uid), gid) = (u.split(':').next().unwrap_or_default().parse::<u32>(), u.split(':').nth(1).and_then(|g| g.parse::<u32>().ok())) {
                        cmd.arg("--chown").arg(format!("{uid}:{}", gid.unwrap_or(uid)));
                    }
                }
            }
            let mut child = cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn()?;
            let mut stdin = child.stdin.take().unwrap();
            let mut body = req.into_body();
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|e| ApiError::bad_request(e.to_string()))?;
                if let Ok(data) = frame.into_data() {
                    if stdin.write_all(&data).await.is_err() {
                        break;
                    }
                }
            }
            drop(stdin);
            let out = child.wait_with_output().await?;
            if !out.status.success() {
                let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
                return Err(ApiError::new(if msg.contains("not a directory") { 400 } else { 500 }, msg));
            }
            Ok(base(200).body(empty()).unwrap())
        }
    }
}

// ---------------------------------------------------------------------------
// Images

fn list_images(engine: &Engine, q: &HashMap<String, String>) -> Value {
    let f = Filters::parse(q.get("filters").map(String::as_str));
    let users: Vec<String> = engine.list().iter().map(|c| c.record().image_id).collect();
    let mut out = Vec::new();
    for info in images::all(&engine.images) {
        let tags = info.tags();
        let dangling = tags.is_empty();
        if let Some(d) = f.bool("dangling") {
            if d != dangling {
                continue;
            }
        }
        let config = engine.images.config_blob(&info.entry.config).unwrap_or(Value::Null);
        let labels = super::filters::labels_of(config.pointer("/config/Labels").unwrap_or(&Value::Null));
        if !f.labels_match("label", &labels) {
            continue;
        }
        if !f.any("reference", |r| {
            let pat = r.split(':').next().unwrap_or(r);
            tags.iter().any(|t| t == r || t.split(':').next() == Some(pat) || glob(r, t) || glob(pat, t.split(':').next().unwrap_or(t)))
        }) {
            continue;
        }
        let count = users.iter().filter(|u| **u == info.id).count() as i64;
        out.push(images::summary(&engine.images, &info, count));
    }
    out.sort_by_key(|v| std::cmp::Reverse(v["Created"].as_i64().unwrap_or(0)));
    Value::Array(out)
}

/// `*` wildcards (image reference filters).
fn glob(pattern: &str, s: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == s;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rest = s;
    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            if !rest.starts_with(p) {
                return false;
            }
            rest = &rest[p.len()..];
        } else if let Some(pos) = rest.find(p) {
            rest = &rest[pos + p.len()..];
        } else {
            return false;
        }
    }
    parts.last().is_none_or(|p| p.is_empty() || s.ends_with(p))
}

fn pull(engine: &Arc<Engine>, q: &HashMap<String, String>, req: &Request<Incoming>) -> ApiResult<Response<Body>> {
    let from = q.get("fromImage").cloned().unwrap_or_default();
    if from.is_empty() {
        return Err(ApiError::new(501, "importing images (fromSrc) is not supported; use docker load"));
    }
    let tag = q.get("tag").cloned().filter(|t| !t.is_empty());
    let reference = match tag {
        Some(t) if t.starts_with("sha256:") => format!("{from}@{t}"),
        Some(t) => format!("{from}:{t}"),
        None if from.contains(':') && !from.rsplit_once(':').is_some_and(|(_, t)| t.contains('/')) || from.contains('@') => from.clone(),
        None => format!("{from}:latest"),
    };
    let platform = q.get("platform").and_then(|p| cimage::parse_platform(p));
    let auth = registry_auth(req);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let progress_tx = tx.clone();
    let opts = PullOptions {
        always: true,
        platform,
        auth,
        pinned: true,
        progress: Some(Arc::new(move |id: &str, msg: &str| {
            let v = if id.is_empty() { json!({"status": msg}) } else { json!({"status": msg, "progressDetail": {}, "id": id}) };
            let _ = progress_tx.send(v);
        })),
    };
    let engine = engine.clone();
    tokio::spawn(async move {
        let short = images::short_name(&images::key_for(&reference).unwrap_or_default()).unwrap_or(reference.clone());
        match engine.images.pull_with(&reference, &opts).await {
            Ok(_) => {
                let _ = tx.send(json!({"status": format!("Status: Downloaded newer image for {short}")}));
                engine.emit("image", "pull", &short, BTreeMap::from([("name".to_string(), short.clone())]));
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let msg = if msg.contains("not found") || msg.contains("404") {
                    format!("pull access denied for {from}, repository does not exist or may require 'docker login': {msg}", from = short)
                } else {
                    msg
                };
                let _ = tx.send(json!({"errorDetail": {"message": msg}, "error": msg}));
            }
        }
    });
    let s = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (Bytes::from(format!("{v}\r\n")), rx)) });
    Ok(stream_resp("application/json", s))
}

fn tag_image(engine: &Engine, source: &str, repo: String, tag: Option<String>) -> ApiResult<()> {
    let info = images::resolve(&engine.images, source).ok_or_else(|| ApiError::not_found(format!("No such image: {source}")))?;
    let name = match tag.filter(|t| !t.is_empty()) {
        Some(t) => format!("{repo}:{t}"),
        None => repo.clone(),
    };
    let key = images::key_for(&name).ok_or_else(|| ApiError::bad_request(format!("invalid reference format: {name}")))?;
    let mut entry = info.entry.clone();
    entry.pinned = true;
    engine.images.set_key(&key, Some(entry))?;
    // A dangling key for this image is no longer needed.
    if info.keys.iter().any(|k| *k == info.id) {
        engine.images.set_key(&info.id, None)?;
    }
    engine.emit("image", "tag", &info.id, BTreeMap::from([("name".to_string(), images::short_name(&key).unwrap_or(name))]));
    Ok(())
}

fn remove_image(engine: &Engine, name: &str, force: bool) -> ApiResult<Value> {
    let info = images::resolve(&engine.images, name).ok_or_else(|| ApiError::not_found(format!("No such image: {name}")))?;
    let users: Vec<String> = engine.list().iter().map(|c| c.record()).filter(|r| r.image_id == info.id).map(|r| r.id[..12].to_string()).collect();
    let by_name = images::key_for(name).filter(|k| info.keys.contains(k));
    let mut out = Vec::new();
    let remove_keys: Vec<String> = match &by_name {
        Some(k) if info.keys.len() > 1 => vec![k.clone()],
        _ => {
            if info.keys.iter().filter(|k| !k.starts_with("sha256:")).count() > 1 && by_name.is_none() && !force {
                return Err(ApiError::conflict(format!(
                    "conflict: unable to delete {} (must be forced) - image is referenced in multiple repositories",
                    &info.id.trim_start_matches("sha256:")[..12]
                )));
            }
            if !users.is_empty() && !force {
                return Err(ApiError::conflict(format!(
                    "conflict: unable to remove repository reference \"{name}\" (must force) - container {} is using its referenced image {}",
                    users[0],
                    &info.id.trim_start_matches("sha256:")[..12]
                )));
            }
            info.keys.clone()
        }
    };
    for k in &remove_keys {
        engine.images.set_key(k, None)?;
        if let Some(n) = images::short_name(k) {
            out.push(json!({"Untagged": n}));
            engine.emit("image", "untag", &info.id, BTreeMap::from([("name".to_string(), n)]));
        }
    }
    let remaining = images::by_id(&engine.images, &info.id);
    if remaining.is_none() {
        if users.is_empty() {
            out.push(json!({"Deleted": info.id}));
            engine.emit("image", "delete", &info.id, BTreeMap::new());
        } else {
            // Containers still need the layers: keep it untagged.
            let mut e = info.entry.clone();
            e.pinned = true;
            engine.images.set_key(&info.id, Some(e))?;
        }
    }
    // Free the layers now, as Docker does, not at the node's next image
    // GC (layers written in the last minutes are kept: a pull may still
    // need them).
    let _ = engine.images.sweep();
    Ok(Value::Array(out))
}

fn prune_images(engine: &Engine, q: &HashMap<String, String>) -> Value {
    let f = Filters::parse(q.get("filters").map(String::as_str));
    let all = f.bool("dangling") == Some(false);
    let users: Vec<String> = engine.list().iter().map(|c| c.record().image_id).collect();
    let mut deleted = Vec::new();
    let mut space = 0u64;
    for info in images::all(&engine.images) {
        if users.contains(&info.id) || (!all && !info.tags().is_empty()) {
            continue;
        }
        let config = engine.images.config_blob(&info.entry.config).unwrap_or(Value::Null);
        if !f.labels_match("label", &super::filters::labels_of(config.pointer("/config/Labels").unwrap_or(&Value::Null))) {
            continue;
        }
        space += info.entry.layers.iter().map(|l| engine.images.layer_size(l)).sum::<u64>();
        for k in &info.keys {
            let _ = engine.images.set_key(k, None);
            if let Some(n) = images::short_name(k) {
                deleted.push(json!({"Untagged": n}));
            }
        }
        deleted.push(json!({"Deleted": info.id}));
    }
    if !deleted.is_empty() {
        let _ = engine.images.sweep();
    }
    json!({"ImagesDeleted": deleted, "SpaceReclaimed": space})
}

/// `docker save`: a docker-archive tar (manifest.json, configs, one
/// layer.tar per layer), written on a thread and streamed.
fn save_images(engine: &Arc<Engine>, names: Vec<String>) -> ApiResult<Response<Body>> {
    let mut found = Vec::new();
    for n in &names {
        let info = images::resolve(&engine.images, n).ok_or_else(|| ApiError::not_found(format!("No such image: {n}")))?;
        let tag = images::key_for(n).filter(|k| info.keys.contains(k)).and_then(|k| images::short_name(&k));
        found.push((info, tag));
    }
    let store = engine.images.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(16);
    struct ChanWriter(tokio::sync::mpsc::Sender<Bytes>);
    impl std::io::Write for ChanWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.blocking_send(Bytes::copy_from_slice(buf)).map_err(|_| std::io::Error::other("client went away"))?;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut b = tar::Builder::new(std::io::BufWriter::with_capacity(1 << 20, ChanWriter(tx)));
        let mut manifest = Vec::new();
        let mut written = std::collections::HashSet::new();
        let append = |b: &mut tar::Builder<_>, name: &str, data: &mut dyn std::io::Read, size: u64| -> std::io::Result<()> {
            let mut h = tar::Header::new_ustar();
            h.set_size(size);
            h.set_mode(0o644);
            h.set_mtime(0);
            h.set_entry_type(tar::EntryType::Regular);
            b.append_data(&mut h, name, data)
        };
        for (info, tag) in found {
            let mut config = store.config_blob(&info.entry.config).unwrap_or(Value::Null);
            // Layers are kept unpacked: their tars are regenerated, and the
            // config names those tars' digests (a pulled image's ID changes
            // on the way through docker save/load; content does not).
            let mut layers = Vec::new();
            let mut diff_ids = Vec::new();
            for diff in &info.entry.layers {
                let tmp = store.staging_dir().join(format!("save-{}.tar", cimage::random_hex(6)));
                let result = (|| -> anyhow::Result<()> {
                    cimage::write_layer_tar(&store.layer_dir(diff), std::io::BufWriter::new(std::fs::File::create(&tmp)?))?;
                    let digest = {
                        use sha2::Digest;
                        let mut h = sha2::Sha256::new();
                        std::io::copy(&mut std::fs::File::open(&tmp)?, &mut h)?;
                        format!("sha256:{}", hex::encode(h.finalize()))
                    };
                    let name = format!("{}/layer.tar", digest.trim_start_matches("sha256:"));
                    if written.insert(name.clone()) {
                        let size = std::fs::metadata(&tmp)?.len();
                        append(&mut b, &name, &mut std::fs::File::open(&tmp)?, size)?;
                    }
                    layers.push(name);
                    diff_ids.push(digest);
                    Ok(())
                })();
                let _ = std::fs::remove_file(&tmp);
                result?;
            }
            config["rootfs"] = json!({"type": "layers", "diff_ids": diff_ids});
            let config_bytes = serde_json::to_vec(&config)?;
            let config_name = {
                use sha2::Digest;
                format!("{}.json", hex::encode(sha2::Sha256::digest(&config_bytes)))
            };
            if written.insert(config_name.clone()) {
                append(&mut b, &config_name, &mut &config_bytes[..], config_bytes.len() as u64)?;
            }
            manifest.push(json!({"Config": config_name, "RepoTags": tag.into_iter().collect::<Vec<_>>(), "Layers": layers}));
        }
        let m = serde_json::to_vec(&manifest)?;
        append(&mut b, "manifest.json", &mut &m[..], m.len() as u64)?;
        b.into_inner()?.into_inner().map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    });
    let s = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|b| (b, rx)) });
    Ok(stream_resp("application/x-tar", s))
}

async fn load_images(engine: &Arc<Engine>, q: &HashMap<String, String>, req: Request<Incoming>) -> ApiResult<Response<Body>> {
    let staging = engine.images.staging_dir().join(format!("load-{}.tar", cimage::random_hex(8)));
    let mut file = tokio::fs::File::create(&staging).await?;
    let mut body = req.into_body();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| ApiError::bad_request(e.to_string()))?;
        if let Ok(data) = frame.into_data() {
            file.write_all(&data).await?;
        }
    }
    file.flush().await?;
    drop(file);
    let images = engine.images.clone();
    let path = staging.clone();
    let result = tokio::task::spawn_blocking(move || images.import_archive(&path)).await.map_err(|e| ApiError::new(500, e.to_string()))?;
    let _ = std::fs::remove_file(&staging);
    let names = result?;
    for n in &names {
        if let Some(info) = images::resolve(&engine.images, n) {
            let mut e = info.entry.clone();
            e.pinned = true;
            let _ = engine.images.set_key(n, Some(e));
        }
    }
    let quiet = qbool(q, "quiet");
    let mut text = String::new();
    for n in &names {
        let short = images::short_name(n).unwrap_or(n.clone());
        text.push_str(&format!("{}\n", json!({"stream": format!("Loaded image: {short}\n")})));
        engine.emit("image", "load", &short, BTreeMap::new());
    }
    let _ = quiet;
    Ok(base(200).header("Content-Type", "application/json").body(full(text)).unwrap())
}

async fn distribution(engine: &Engine, name: &str, req: &Request<Incoming>) -> ApiResult<Response<Body>> {
    let _ = (req, engine);
    let info = images::resolve(&engine.images, name);
    match info {
        Some(i) => {
            let config = engine.images.config_blob(&i.entry.config).unwrap_or(Value::Null);
            Ok(json_resp(
                200,
                &json!({
                    "Descriptor": {"mediaType": "application/vnd.oci.image.index.v1+json", "digest": i.entry.top, "size": 0},
                    "Platforms": [{"architecture": config["architecture"], "os": "linux"}],
                }),
            ))
        }
        None => Err(ApiError::not_found(format!("No such image: {name}"))),
    }
}

// ---------------------------------------------------------------------------
// Networks and volumes

fn list_networks(engine: &Engine, q: &HashMap<String, String>) -> Value {
    let f = Filters::parse(q.get("filters").map(String::as_str));
    let mut out = Vec::new();
    for n in engine.networks() {
        let ok = f.any("name", |v| filters::name_matches(v, &n.name))
            && f.any("id", |v| filters::id_matches(v, &n.id))
            && f.any("driver", |v| n.driver == v)
            && f.labels_match("label", &n.labels)
            && f.any("type", |v| (v == "builtin") == n.predefined)
            && f.any("scope", |v| v == "local");
        if ok {
            out.push(n.inspect(engine.network_containers(&n)));
        }
    }
    Value::Array(out)
}

async fn prune_networks(engine: &Engine, q: &HashMap<String, String>) -> Value {
    let f = Filters::parse(q.get("filters").map(String::as_str));
    let mut deleted = Vec::new();
    for n in engine.networks() {
        if n.predefined || !f.labels_match("label", &n.labels) {
            continue;
        }
        let used = engine.list().iter().any(|c| c.record().endpoints.iter().any(|e| e.network_id == n.id));
        if !used && engine.remove_network(&n.id).await.is_ok() {
            deleted.push(n.name);
        }
    }
    json!({"NetworksDeleted": deleted})
}

fn list_volumes(engine: &Engine, q: &HashMap<String, String>) -> Value {
    let f = Filters::parse(q.get("filters").map(String::as_str));
    let mut out = Vec::new();
    for v in engine.volumes() {
        let dangling = engine.volume_users(&v.name).is_empty();
        let ok = f.any("name", |x| v.name.contains(x))
            && f.labels_match("label", &v.labels)
            && f.any("driver", |d| d == "local")
            && f.bool("dangling").is_none_or(|d| d == dangling);
        if ok {
            out.push(v.inspect());
        }
    }
    json!({"Volumes": out, "Warnings": []})
}

fn prune_volumes(engine: &Engine, q: &HashMap<String, String>) -> Value {
    let f = Filters::parse(q.get("filters").map(String::as_str));
    let all = f.bool("all").unwrap_or(false);
    let mut deleted = Vec::new();
    let mut space = 0u64;
    for v in engine.volumes() {
        if !engine.volume_users(&v.name).is_empty() || !f.labels_match("label", &v.labels) || (!all && !v.anonymous) {
            continue;
        }
        let size = cimage::dir_bytes(std::path::Path::new(&v.mountpoint));
        if engine.remove_volume(&v.name, false).is_ok() {
            deleted.push(v.name.clone());
            space += size;
        }
    }
    json!({"VolumesDeleted": deleted, "SpaceReclaimed": space})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_queries() {
        assert_eq!(segments("/v1.47/containers/abc/json"), ["containers", "abc", "json"]);
        assert_eq!(segments("/images/bitnami%2Fredis:7/json"), ["images", "bitnami/redis:7", "json"]);
        assert_eq!(segments("/version"), ["version"]);
        assert_eq!(decode("a%20b+c"), "a b c");
        assert_eq!(decode("100%"), "100%");
        assert!(glob("my*", "myapp") && !glob("x*", "myapp") && glob("*app", "myapp"));
    }
}
