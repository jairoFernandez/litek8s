//! The slice of the kubelet's own HTTPS API that cluster add-ons scrape:
//! `/metrics/resource` (what metrics-server reads), `/healthz` and `/pods`.

use std::convert::Infallible;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rcgen::{CertificateParams, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::json;
use sysinfo::System;
use tokio_rustls::TlsAcceptor;

use super::Agent;
use super::runtime::PodRuntime;

pub(crate) const PORT: u16 = 10250;

/// Node CPU as a monotonic counter, integrated from usage samples.
struct NodeCpu {
    system: System,
    total_seconds: f64,
    last: Instant,
}

fn node_cpu() -> &'static Mutex<NodeCpu> {
    static CPU: OnceLock<Mutex<NodeCpu>> = OnceLock::new();
    CPU.get_or_init(|| {
        let mut system = System::new();
        system.refresh_cpu_usage();
        system.refresh_memory();
        Mutex::new(NodeCpu { system, total_seconds: 0.0, last: Instant::now() })
    })
}

fn acceptor(names: Vec<String>) -> anyhow::Result<TlsAcceptor> {
    let key = KeyPair::generate()?;
    let cert = CertificateParams::new(names)?.self_signed(&key)?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert.der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

pub(crate) async fn serve<R: PodRuntime>(agent: Arc<Agent<R>>, port: u16) {
    let names = vec![agent.config().node_name.clone(), agent.config().node_ip.clone(), "localhost".into(), "127.0.0.1".into()];
    let tls = match acceptor(names) {
        Ok(t) => t,
        Err(e) => return eprintln!("node-agent: kubelet endpoint disabled: {e:#}"),
    };
    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => return eprintln!("node-agent: kubelet endpoint disabled: cannot listen on :{port}: {e}"),
    };
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let (tls, agent) = (tls.clone(), agent.clone());
        tokio::spawn(async move {
            let Ok(stream) = tls.accept(stream).await else { return };
            let service = service_fn(move |req| {
                let agent = agent.clone();
                async move { Ok::<_, Infallible>(handle(agent, req).await) }
            });
            let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
        });
    }
}

fn text(code: StatusCode, content_type: &'static str, body: String) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from(body)));
    *resp.status_mut() = code;
    resp.headers_mut().insert(hyper::header::CONTENT_TYPE, hyper::header::HeaderValue::from_static(content_type));
    resp
}

async fn handle<R: PodRuntime>(agent: Arc<Agent<R>>, req: Request<hyper::body::Incoming>) -> Response<Full<Bytes>> {
    match req.uri().path() {
        "/healthz" => text(StatusCode::OK, "text/plain", "ok".into()),
        "/metrics/resource" => text(StatusCode::OK, "text/plain; version=0.0.4", resource_metrics(&agent).await),
        "/pods" => {
            let views = agent.views.lock().unwrap().clone();
            let items: Vec<_> = views
                .iter()
                .map(|(uid, v)| json!({"metadata": {"name": v.name, "namespace": v.namespace, "uid": uid}, "status": {"podIP": v.ip}}))
                .collect();
            text(StatusCode::OK, "application/json", json!({"kind": "PodList", "apiVersion": "v1", "items": items}).to_string())
        }
        other => {
            eprintln!("node-agent: kubelet endpoint: no handler for {} {other}", req.method());
            text(StatusCode::NOT_FOUND, "text/plain", "404 page not found".into())
        }
    }
}

/// The kubelet's `/metrics/resource` in Prometheus text format.
async fn resource_metrics<R: PodRuntime>(agent: &Agent<R>) -> String {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let (cpu_seconds, memory) = {
        let mut cpu = node_cpu().lock().unwrap();
        cpu.system.refresh_cpu_usage();
        cpu.system.refresh_memory();
        let elapsed = cpu.last.elapsed().as_secs_f64();
        cpu.last = Instant::now();
        let cores = cpu.system.cpus().len().max(1) as f64;
        let usage = cpu.system.global_cpu_usage() as f64 / 100.0;
        cpu.total_seconds += usage * cores * elapsed;
        (cpu.total_seconds, cpu.system.used_memory())
    };
    let views = agent.views.lock().unwrap().clone();
    let mut containers = Vec::new();
    for view in views.values() {
        for (name, (id, _)) in &view.containers {
            if let Some(id) = id {
                containers.push((view.namespace.clone(), view.name.clone(), name.clone(), id.clone()));
            }
        }
    }
    let rt = agent.runtime().clone();
    let stats = futures_util::future::join_all(containers.iter().map(|(_, _, _, id)| {
        let rt = rt.clone();
        let id = id.clone();
        async move { rt.stats(&id).await.ok().flatten() }
    }))
    .await;

    let mut out = String::new();
    fn metric(out: &mut String, name: &str, kind: &str, help: &str) {
        let _ = writeln!(out, "# HELP {name} [STABLE] {help}\n# TYPE {name} {kind}");
    }
    metric(&mut out, "node_cpu_usage_seconds_total", "counter", "Cumulative cpu time consumed by the node in core-seconds");
    let _ = writeln!(out, "node_cpu_usage_seconds_total {cpu_seconds} {now_ms}");
    metric(&mut out, "node_memory_working_set_bytes", "gauge", "Current working set of the node in bytes");
    let _ = writeln!(out, "node_memory_working_set_bytes {memory} {now_ms}");
    let mut pods: std::collections::BTreeMap<(String, String), (f64, u64)> = Default::default();
    let mut cpu_lines = String::new();
    let mut mem_lines = String::new();
    for ((ns, pod, container, _), stat) in containers.iter().zip(&stats) {
        let Some((cpu_ns, mem)) = stat else { continue };
        let labels = format!("container=\"{container}\",namespace=\"{ns}\",pod=\"{pod}\"");
        let cpu = *cpu_ns as f64 / 1e9;
        let _ = writeln!(cpu_lines, "container_cpu_usage_seconds_total{{{labels}}} {cpu} {now_ms}");
        let _ = writeln!(mem_lines, "container_memory_working_set_bytes{{{labels}}} {mem} {now_ms}");
        let entry = pods.entry((ns.clone(), pod.clone())).or_default();
        entry.0 += cpu;
        entry.1 += mem;
    }
    metric(&mut out, "container_cpu_usage_seconds_total", "counter", "Cumulative cpu time consumed by the container in core-seconds");
    out.push_str(&cpu_lines);
    metric(&mut out, "container_memory_working_set_bytes", "gauge", "Current working set of the container in bytes");
    out.push_str(&mem_lines);
    metric(&mut out, "pod_cpu_usage_seconds_total", "counter", "Cumulative cpu time consumed by the pod in core-seconds");
    for ((ns, pod), (cpu, _)) in &pods {
        let _ = writeln!(out, "pod_cpu_usage_seconds_total{{namespace=\"{ns}\",pod=\"{pod}\"}} {cpu} {now_ms}");
    }
    metric(&mut out, "pod_memory_working_set_bytes", "gauge", "Current working set of the pod in bytes");
    for ((ns, pod), (_, mem)) in &pods {
        let _ = writeln!(out, "pod_memory_working_set_bytes{{namespace=\"{ns}\",pod=\"{pod}\"}} {mem} {now_ms}");
    }
    metric(&mut out, "scrape_error", "gauge", "1 if there was an error while getting container metrics, 0 otherwise");
    out.push_str("scrape_error 0\n");
    out
}
