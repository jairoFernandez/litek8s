//! Load test of the Kubernetes API server: an in-process `apiserver` (WAL on
//! disk, controllers running) driven over real HTTP/1.1 keep-alive
//! connections, the way kubectl, Helm and controllers talk to it.
//!
//!   cargo bench --bench api_load                      # run, compare with bench/perf-baseline.json
//!   cargo bench --bench api_load -- --save-baseline   # run and make this the baseline
//!   cargo bench --bench api_load -- list watch        # only scenarios whose name contains a filter
//!
//! Knobs (env): PERF_SECS=3 (per run), PERF_RUNS=3 (runs per scenario, the
//! median is kept), PERF_CONC=32 (clients),
//! PERF_OBJECTS=1000 (objects listed), PERF_REPLICAS=100 (Deployment scaled),
//! PERF_TOLERANCE=0.15 (regression threshold), PERF_MEMORY=1 (no WAL),
//! PERF_BASELINE=<file> (instead of bench/perf-baseline.json),
//! PERF_REGRESSIONS=<file> (the regressed scenarios' names are written there).
//! A regression: ops/s down by more than the tolerance, p99 up by more than
//! twice the tolerance (and 0.5 ms), or any failed request.
//! Results: target/perf/latest.json. See bench/PERF.md.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http1::{self, SendRequest};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use litek8s::kube::{ApiState, serve_listener};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};

type Client = SendRequest<Full<Bytes>>;

/// Names of created objects, unique for the whole run.
static NEXT: AtomicU64 = AtomicU64::new(0);

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

async fn connect(addr: &str) -> Result<Client> {
    let stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    let (tx, conn) = http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(conn);
    Ok(tx)
}

fn request(method: &Method, path: &str, content_type: &str, accept: &str, body: Bytes) -> Request<Full<Bytes>> {
    Request::builder()
        .method(method.clone())
        .uri(path)
        .header("host", "bench")
        .header("accept", accept)
        .header("content-type", content_type)
        .body(Full::new(body))
        .unwrap()
}

async fn call(tx: &mut Client, method: &Method, path: &str, content_type: &str, accept: &str, body: Bytes) -> Result<(u16, Bytes)> {
    tx.ready().await?;
    let resp = tx.send_request(request(method, path, content_type, accept, body)).await?;
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await?.to_bytes();
    Ok((status, body))
}

async fn must(tx: &mut Client, method: Method, path: &str, content_type: &str, body: Value) -> Result<Value> {
    let body = if body.is_null() { Bytes::new() } else { Bytes::from(body.to_string()) };
    let (code, out) = call(tx, &method, path, content_type, JSON, body).await?;
    if !(200..300).contains(&code) {
        bail!("{method} {path}: {code} {}", String::from_utf8_lossy(&out));
    }
    Ok(serde_json::from_slice(&out).unwrap_or(Value::Null))
}

fn configmap(name: &str, group: usize) -> Value {
    json!({"apiVersion": "v1", "kind": "ConfigMap",
           "metadata": {"name": name, "labels": {"app": "bench", "group": format!("g{group}")}},
           "data": {"key": "value", "config.yaml": "a: 1\nb: [1, 2, 3]\n"}})
}

/// One request a worker sends: (method, path, content type, body, expected status).
struct Op {
    method: Method,
    path: String,
    content_type: &'static str,
    accept: &'static str,
    body: Bytes,
    expect: u16,
}

const JSON: &str = "application/json";
/// What kubectl get asks for: server-side Table rendering.
const TABLE: &str = "application/json;as=Table;v=v1;g=meta.k8s.io,application/json";

type OpFn = Arc<dyn Fn(usize, u64) -> Op + Send + Sync>;

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
struct Outcome {
    /// Completed requests (or events) per second.
    ops_per_sec: f64,
    p50_ms: f64,
    p90_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    count: u64,
    errors: u64,
    /// For one-shot scenarios (scheduling): wall time of the whole thing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total_ms: Option<f64>,
}

fn summarize(mut lat_us: Vec<u64>, elapsed: Duration, errors: u64) -> Outcome {
    lat_us.sort_unstable();
    let pct = |p: f64| {
        if lat_us.is_empty() {
            return 0.0;
        }
        let i = ((lat_us.len() as f64 - 1.0) * p).round() as usize;
        lat_us[i] as f64 / 1000.0
    };
    Outcome {
        ops_per_sec: lat_us.len() as f64 / elapsed.as_secs_f64(),
        p50_ms: pct(0.50),
        p90_ms: pct(0.90),
        p99_ms: pct(0.99),
        max_ms: lat_us.last().map_or(0.0, |&v| v as f64 / 1000.0),
        count: lat_us.len() as u64,
        errors,
        total_ms: None,
    }
}

/// Each figure's median over several runs (taken separately: the run with
/// the median ops/s can have the worst p99), errors and counts summed.
fn median(runs: Vec<Outcome>) -> Outcome {
    let mid = |f: fn(&Outcome) -> f64| {
        let mut v: Vec<f64> = runs.iter().map(f).collect();
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    Outcome {
        ops_per_sec: mid(|o| o.ops_per_sec),
        p50_ms: mid(|o| o.p50_ms),
        p90_ms: mid(|o| o.p90_ms),
        p99_ms: mid(|o| o.p99_ms),
        max_ms: runs.iter().map(|o| o.max_ms).fold(0.0, f64::max),
        count: runs.iter().map(|o| o.count).sum(),
        errors: runs.iter().map(|o| o.errors).sum(),
        total_ms: None,
    }
}

/// `conc` clients send `op`s back to back for `secs` seconds.
async fn throughput(addr: &str, conc: usize, secs: f64, op: OpFn) -> Result<Outcome> {
    let deadline = Instant::now() + Duration::from_secs_f64(secs);
    let start = Instant::now();
    let mut tasks = Vec::new();
    for w in 0..conc {
        let addr = addr.to_string();
        let op = op.clone();
        tasks.push(tokio::spawn(async move {
            let mut tx = connect(&addr).await?;
            let mut lat = Vec::with_capacity(4096);
            let mut errors = 0u64;
            let mut i = 0u64;
            while Instant::now() < deadline {
                let o = op(w, i);
                i += 1;
                let t = Instant::now();
                match call(&mut tx, &o.method, &o.path, o.content_type, o.accept, o.body).await {
                    Ok((code, _)) if code == o.expect => lat.push(t.elapsed().as_micros() as u64),
                    Ok(_) => errors += 1,
                    Err(_) => {
                        errors += 1;
                        tx = connect(&addr).await?;
                    }
                }
            }
            anyhow::Ok((lat, errors))
        }));
    }
    let mut all = Vec::new();
    let mut errors = 0;
    for t in tasks {
        let (lat, e) = t.await??;
        all.extend(lat);
        errors += e;
    }
    Ok(summarize(all, start.elapsed(), errors))
}

/// Create → watch event latency: one watcher, `n` sequential creates.
async fn watch_latency(addr: &str, n: usize) -> Result<Outcome> {
    let ns = "bench-watch";
    let mut watcher = connect(addr).await?;
    watcher.ready().await?;
    let resp = watcher
        .send_request(request(&Method::GET, &format!("/api/v1/namespaces/{ns}/configmaps?watch=true"), JSON, JSON, Bytes::new()))
        .await?;
    let mut body = resp.into_body();
    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Instant)>();
    tokio::spawn(async move {
        let mut buf = Vec::new();
        while let Some(Ok(frame)) = body.frame().await {
            let Ok(data) = frame.into_data() else { continue };
            buf.extend_from_slice(&data);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                if let Ok(ev) = serde_json::from_slice::<Value>(&line)
                    && ev["type"] == "ADDED"
                    && let Some(name) = ev["object"]["metadata"]["name"].as_str()
                {
                    let _ = seen_tx.send((name.to_string(), Instant::now()));
                }
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut tx = connect(addr).await?;
    let mut sent = HashMap::new();
    let start = Instant::now();
    for i in 0..n {
        let name = format!("w-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let path = format!("/api/v1/namespaces/{ns}/configmaps");
        sent.insert(name.clone(), Instant::now());
        must(&mut tx, Method::POST, &path, "application/json", configmap(&name, i % 10)).await?;
    }
    let mut lat = Vec::with_capacity(n);
    while lat.len() < n {
        let (name, at) = tokio::time::timeout(Duration::from_secs(10), seen_rx.recv())
            .await
            .context("watch events stopped arriving")?
            .context("watch closed")?;
        if let Some(t0) = sent.get(&name) {
            lat.push(at.duration_since(*t0).as_micros() as u64);
        }
    }
    Ok(summarize(lat, start.elapsed(), 0))
}

/// Deployment with `replicas` → every Pod created and bound to a Node:
/// the Deployment, ReplicaSet and scheduler controllers end to end.
async fn schedule(addr: &str, replicas: usize) -> Result<Outcome> {
    let mut tx = connect(addr).await?;
    for n in 0..5 {
        let name = format!("bench-node-{n}");
        let node = json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": name}});
        must(&mut tx, Method::POST, "/api/v1/nodes", JSON, node).await?;
        let status = json!({"status": {"allocatable": {"cpu": "64", "memory": "256Gi", "pods": "1000"},
                                "capacity": {"cpu": "64", "memory": "256Gi", "pods": "1000"},
                                "conditions": [{"type": "Ready", "status": "True"}]}});
        must(&mut tx, Method::PATCH, &format!("/api/v1/nodes/{name}/status"), "application/merge-patch+json", status).await?;
    }
    let ns = "bench-sched";
    let deploy = json!({"apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": "web"},
        "spec": {"replicas": replicas, "selector": {"matchLabels": {"app": "web"}},
                 "template": {"metadata": {"labels": {"app": "web"}},
                              "spec": {"containers": [{"name": "c", "image": "nginx:alpine",
                                       "resources": {"requests": {"cpu": "10m", "memory": "16Mi"}}}]}}}});
    let start = Instant::now();
    must(&mut tx, Method::POST, &format!("/apis/apps/v1/namespaces/{ns}/deployments"), "application/json", deploy).await?;
    let mut created_at = None;
    loop {
        let pods = must(&mut tx, Method::GET, &format!("/api/v1/namespaces/{ns}/pods?labelSelector=app%3Dweb"), "application/json", Value::Null).await?;
        let items = pods["items"].as_array().cloned().unwrap_or_default();
        if created_at.is_none() && items.len() >= replicas {
            created_at = Some(start.elapsed());
        }
        let bound = items.iter().filter(|p| p["spec"]["nodeName"].is_string()).count();
        if bound >= replicas {
            break;
        }
        if start.elapsed() > Duration::from_secs(60) {
            bail!("only {bound}/{replicas} pods bound after 60 s ({} created)", items.len());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let total = start.elapsed();
    let created = created_at.unwrap_or(total);
    Ok(Outcome {
        ops_per_sec: replicas as f64 / total.as_secs_f64(),
        p50_ms: created.as_secs_f64() * 1000.0,
        count: replicas as u64,
        total_ms: Some(total.as_secs_f64() * 1000.0),
        ..Default::default()
    })
}

struct Report {
    results: Vec<(String, Outcome)>,
}

/// bench/perf-baseline.json, or `PERF_BASELINE` (CI compares a pull
/// request with its base on the same runner through a file of its own).
fn baseline_path() -> PathBuf {
    std::env::var_os("PERF_BASELINE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bench/perf-baseline.json"))
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).filter(|a| a != "--bench").collect();
    let save = args.iter().any(|a| a == "--save-baseline");
    let filters: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let wanted = |name: &str| filters.is_empty() || filters.iter().any(|f| name.contains(f.as_str()));

    let secs: f64 = env("PERF_SECS", 3.0);
    let runs: usize = env("PERF_RUNS", 3).max(1);
    let conc: usize = env("PERF_CONC", 32);
    let objects: usize = env("PERF_OBJECTS", 1000);
    let replicas: usize = env("PERF_REPLICAS", 100);
    let tolerance: f64 = env("PERF_TOLERANCE", 0.15);

    let mut load = [0f64; 3];
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    if unsafe { libc::getloadavg(load.as_mut_ptr(), 3) } == 3 && load[0] > cpus / 2.0 {
        eprintln!("api_load: WARNING load average {:.1} on {cpus} CPUs: other work skews the numbers; don't save a baseline now", load[0]);
    }

    let dir = tempfile::tempdir()?;
    let data_dir = (env("PERF_MEMORY", 0) == 0).then(|| dir.path().to_path_buf());
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    let state = Arc::new(ApiState::open(data_dir.as_deref(), addr.clone(), false)?);
    tokio::spawn(litek8s::kube::controllers::run(state.clone()));
    tokio::spawn(serve_listener(state, listener));

    let mut tx = connect(&addr).await?;
    for ns in ["bench", "bench-create", "bench-watch", "bench-sched"] {
        must(&mut tx, Method::POST, "/api/v1/namespaces", "application/json", json!({"metadata": {"name": ns}})).await?;
    }
    for i in 0..objects {
        must(&mut tx, Method::POST, "/api/v1/namespaces/bench/configmaps", "application/json", configmap(&format!("cm-{i}"), i % 10)).await?;
    }

    eprintln!(
        "api_load: {conc} clients, {runs}×{secs} s per scenario, {objects} objects, store {}",
        if data_dir.is_some() { "with WAL" } else { "in memory" }
    );
    let mut report = Report { results: Vec::new() };
    let n = objects as u64;

    let scenarios: Vec<(&str, usize, OpFn)> = vec![
        ("get", conc, Arc::new(move |w, i| Op {
            method: Method::GET,
            path: format!("/api/v1/namespaces/bench/configmaps/cm-{}", (w as u64 * 7919 + i * 31) % n),
            content_type: JSON,
            accept: JSON,
            body: Bytes::new(),
            expect: 200,
        })),
        ("patch", conc, Arc::new(move |w, i| Op {
            method: Method::PATCH,
            path: format!("/api/v1/namespaces/bench/configmaps/cm-{}", (w as u64 * 7919 + i * 31) % n),
            content_type: "application/merge-patch+json",
            accept: JSON,
            body: Bytes::from(json!({"data": {"counter": i.to_string()}}).to_string()),
            expect: 200,
        })),
        ("list_selector", conc, Arc::new(|_, i| Op {
            method: Method::GET,
            path: format!("/api/v1/namespaces/bench/configmaps?labelSelector=group%3Dg{}", i % 10),
            content_type: JSON,
            accept: JSON,
            body: Bytes::new(),
            expect: 200,
        })),
        ("list_all", conc.min(8), Arc::new(|_, _| Op {
            method: Method::GET,
            path: "/api/v1/namespaces/bench/configmaps".into(),
            content_type: JSON,
            accept: JSON,
            body: Bytes::new(),
            expect: 200,
        })),
        ("list_table", conc.min(8), Arc::new(|_, _| Op {
            method: Method::GET,
            path: "/api/v1/namespaces/bench/configmaps".into(),
            content_type: JSON,
            accept: TABLE,
            body: Bytes::new(),
            expect: 200,
        })),
        // Last: it grows the store, which every later scenario would pay for.
        ("create", conc, Arc::new(|w, i| Op {
            method: Method::POST,
            path: "/api/v1/namespaces/bench-create/configmaps".into(),
            content_type: JSON,
            accept: JSON,
            // Unique across runs: a repeated name would be a 409.
            body: Bytes::from(configmap(&format!("c-{}", NEXT.fetch_add(1, Ordering::Relaxed)), (w + i as usize) % 10).to_string()),
            expect: 201,
        })),
    ];

    if wanted("schedule") {
        eprintln!("  schedule ({replicas} replicas)…");
        report.results.push(("schedule".into(), schedule(&addr, replicas).await?));
    }
    for (name, c, op) in scenarios {
        if !wanted(name) {
            continue;
        }
        eprintln!("  {name}…");
        // One run on a laptop varies by ±30 %: keep the median of `runs`.
        let mut outs = Vec::new();
        for _ in 0..runs {
            outs.push(throughput(&addr, c, secs, op.clone()).await?);
        }
        report.results.push((name.into(), median(outs)));
    }
    if wanted("watch") {
        eprintln!("  watch…");
        let mut outs = Vec::new();
        for _ in 0..runs {
            outs.push(watch_latency(&addr, 500).await?);
        }
        report.results.push(("watch_latency".into(), median(outs)));
    }

    print_and_compare(&report, save, tolerance)
}

fn print_and_compare(report: &Report, save: bool, tolerance: f64) -> Result<()> {
    let baseline: HashMap<String, Outcome> = std::fs::read(baseline_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();

    println!();
    println!("{:<15} {:>11} {:>9} {:>9} {:>9} {:>9} {:>7}  vs baseline", "scenario", "ops/s", "p50 ms", "p90 ms", "p99 ms", "max ms", "errors");
    let mut regressions = Vec::new();
    for (name, o) in &report.results {
        if o.errors > 0 {
            regressions.push(format!("{name}: {} failed requests", o.errors));
        }
        let delta = baseline.get(name).map(|b| {
            let thr = (o.ops_per_sec - b.ops_per_sec) / b.ops_per_sec.max(1e-9);
            // Its ops/s is only how fast 500 sequential creates went: judged by latency.
            let judge_throughput = name != "watch_latency";
            if judge_throughput && thr < -tolerance {
                regressions.push(format!("{name}: ops/s {:.0} → {:.0} ({:+.0}%)", b.ops_per_sec, o.ops_per_sec, thr * 100.0));
            }
            // The tail is noisier than throughput: twice the margin.
            if b.p99_ms > 0.0 && o.p99_ms > b.p99_ms * (1.0 + 2.0 * tolerance) && o.p99_ms - b.p99_ms > 0.5 {
                regressions.push(format!("{name}: p99 {:.2} → {:.2} ms", b.p99_ms, o.p99_ms));
            }
            format!("{:+.1}% ops/s", thr * 100.0)
        });
        let extra = o.total_ms.map(|t| format!("  (all bound in {t:.0} ms, created in {:.0} ms)", o.p50_ms)).unwrap_or_default();
        println!(
            "{:<15} {:>11.0} {:>9.2} {:>9.2} {:>9.2} {:>9.2} {:>7}  {}{}",
            name, o.ops_per_sec, o.p50_ms, o.p90_ms, o.p99_ms, o.max_ms, o.errors,
            delta.unwrap_or_else(|| "–".into()), extra
        );
    }

    let results: HashMap<&String, &Outcome> = report.results.iter().map(|(n, o)| (n, o)).collect();
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/perf");
    std::fs::create_dir_all(&out_dir)?;
    std::fs::write(out_dir.join("latest.json"), serde_json::to_vec_pretty(&results)?)?;
    if save {
        let mut merged = baseline.clone();
        for (n, o) in &report.results {
            merged.insert(n.clone(), o.clone());
        }
        let sorted: std::collections::BTreeMap<_, _> = merged.into_iter().collect();
        std::fs::write(baseline_path(), serde_json::to_vec_pretty(&sorted)?)?;
        println!("\nbaseline saved to {}", baseline_path().display());
    }
    // For bench/perf-compare.sh: which scenarios regressed, one per line.
    if let Some(file) = std::env::var_os("PERF_REGRESSIONS") {
        let mut names: Vec<&str> = regressions.iter().map(|r| r.split(':').next().unwrap_or_default()).collect();
        names.dedup();
        std::fs::write(file, names.iter().map(|n| format!("{n}\n")).collect::<String>())?;
    }
    if !regressions.is_empty() && !save {
        println!("\nREGRESSIONS (> {:.0}%):", tolerance * 100.0);
        for r in &regressions {
            println!("  {r}");
        }
        std::process::exit(1);
    }
    Ok(())
}
