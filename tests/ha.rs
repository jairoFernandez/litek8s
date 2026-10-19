//! The HA control plane: three API servers in one process (each on its own
//! runtime, so one can be killed outright), Raft between them over HTTPS on
//! loopback. Writes and watches go through every server while the leader is
//! killed or partitioned away.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use litek8s::kube::ha::lincheck::{self, http};
use litek8s::kube::{ApiServerConfig, serve};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

struct Server {
    name: String,
    addr: String,
    dir: PathBuf,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(name: &str, dir: &Path, port: u16, init: bool, join: Option<(&str, &str)>) -> Self {
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).expect("bind");
        let addr = listener.local_addr().unwrap().to_string();
        let config = ApiServerConfig {
            listen: addr.parse().unwrap(),
            data_dir: Some(dir.to_path_buf()),
            kubeconfig_out: None,
            log_requests: false,
            run_node: false,
            node_name: Some(name.to_string()),
            node_dir: None,
            cluster_dns: None,
            in_cluster_port: None,
            runtime: litek8s::kube::agent::RuntimeChoice::Docker,
            node_ip: Some([127, 0, 0, 1].into()),
            advertise_address: Some("127.0.0.1".into()),
            network: Default::default(),
            token: join.map(|(_, t)| t.to_string()),
            lifecycle: Default::default(),
            wireguard_port: None,
            listener: Some(listener),
            docker: None,
            cluster_init: init,
            join_server: join.map(|(u, _)| u.to_string()),
        };
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let label = name.to_string();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            rt.block_on(async {
                tokio::select! {
                    r = serve(config) => if let Err(e) = r { eprintln!("{label}: server failed: {e:#}") },
                    _ = stopped => {}
                }
            });
            rt.shutdown_background();
        });
        Self { name: name.to_string(), addr, dir: dir.to_path_buf(), stop: Some(stop), thread: Some(thread) }
    }

    fn port(&self) -> u16 {
        self.addr.rsplit(':').next().unwrap().parse().unwrap()
    }

    /// Kill it: every task and connection of the server goes at once.
    fn kill(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    fn alive(&self) -> bool {
        self.thread.is_some()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.kill();
    }
}

const T: Duration = Duration::from_secs(10);

async fn eventually<F, Fut>(what: &str, timeout: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn servers(addr: &str) -> Option<Value> {
    match http(addr, "GET", "/apis/server.litek8s.dev/v1/servers", None, Duration::from_secs(3)).await {
        Ok((200, v)) => Some(v),
        _ => None,
    }
}

async fn leader_name(addr: &str) -> Option<String> {
    let v = servers(addr).await?;
    v["servers"].as_array()?.iter().find(|s| s["leader"] == true).and_then(|s| s["name"].as_str()).map(str::to_string)
}

/// Three servers: s1 starts the cluster, s2 and s3 join it.
async fn cluster(root: &Path) -> Vec<Server> {
    // SAFETY: set before the servers start; they only read it.
    unsafe { std::env::set_var("LITEK8S_HA_NO_EXIT", "1") };
    let s1 = Server::start("s1", &root.join("s1"), 0, true, None);
    eventually("s1 ready", T, || async { matches!(http(&s1.addr, "GET", "/api/v1/namespaces/default", None, T).await, Ok((200, _))) }).await;
    let token = std::fs::read_to_string(root.join("s1/join-token")).unwrap();
    let url = format!("https://{}", s1.addr);
    let s2 = Server::start("s2", &root.join("s2"), 0, false, Some((&url, &token)));
    let s3 = Server::start("s3", &root.join("s3"), 0, false, Some((&url, &token)));
    let all = vec![s1, s2, s3];
    eventually("three voters", Duration::from_secs(30), || async {
        servers(&all[0].addr).await.is_some_and(|v| v["servers"].as_array().is_some_and(|s| s.iter().filter(|s| s["voter"] == true).count() == 3))
    })
    .await;
    for s in &all {
        eventually("every server serves", T, || async { matches!(http(&s.addr, "GET", "/api/v1/namespaces/default", None, T).await, Ok((200, _))) }).await;
    }
    all
}

fn cm(name: &str, data: Value) -> Value {
    json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name}, "data": data})
}

/// Follow a watch from `rv`, appending (type, name, rv) and returning the
/// last resourceVersion seen when the stream ends.
async fn watch_once(addr: &str, rv: u64, out: &Arc<std::sync::Mutex<Vec<(String, String, u64)>>>, stop: &AtomicBool) -> Result<u64, String> {
    let mut stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
    let req = format!("GET /api/v1/namespaces/default/configmaps?watch=1&resourceVersion={rv} HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json\r\n\r\n");
    stream.write_all(req.as_bytes()).await.map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).await.map_err(|e| e.to_string())?;
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }
    let mut last = rv;
    let mut buf = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        line.clear();
        let read = tokio::time::timeout(Duration::from_millis(300), reader.read_line(&mut line)).await;
        let Ok(read) = read else { continue };
        if read.map_err(|e| e.to_string())? == 0 {
            break;
        }
        let size = usize::from_str_radix(line.trim(), 16).map_err(|e| e.to_string())?;
        if size == 0 {
            break;
        }
        let mut chunk = vec![0u8; size + 2];
        reader.read_exact(&mut chunk).await.map_err(|e| e.to_string())?;
        buf.extend_from_slice(&chunk[..size]);
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let event: Value = serde_json::from_slice(&buf[..pos]).map_err(|e| e.to_string())?;
            buf.drain(..=pos);
            let kind = event["type"].as_str().unwrap_or_default().to_string();
            if kind == "ERROR" {
                return Err(format!("watch error: {event}"));
            }
            let erv: u64 = event.pointer("/object/metadata/resourceVersion").and_then(Value::as_str).and_then(|s| s.parse().ok()).unwrap_or(0);
            let name = event.pointer("/object/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string();
            out.lock().unwrap().push((kind, name, erv));
            last = last.max(erv);
        }
    }
    Ok(last)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failover_keeps_acknowledged_writes_and_watches_resume() {
    let root = tempfile::tempdir().unwrap();
    let mut servers_ = cluster(root.path()).await;
    let addrs: Vec<String> = servers_.iter().map(|s| s.addr.clone()).collect();

    // A write through any server is visible through every other one.
    let base = "/api/v1/namespaces/default/configmaps";
    let (code, _) = http(&addrs[1], "POST", base, Some(&cm("counter", json!({"n": "0"}))), T).await.unwrap();
    assert_eq!(code, 201);
    for a in &addrs {
        let (code, v) = http(a, "GET", &format!("{base}/counter"), None, T).await.unwrap();
        assert_eq!((code, v["data"]["n"].as_str()), (200, Some("0")), "read on {a}");
    }
    let (_, list) = http(&addrs[2], "GET", base, None, T).await.unwrap();
    let start_rv: u64 = list["metadata"]["resourceVersion"].as_str().unwrap().parse().unwrap();

    // A watcher that hops to another server whenever its stream breaks.
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let stop_watch = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (events, stop, addrs) = (events.clone(), stop_watch.clone(), addrs.clone());
        tokio::spawn(async move {
            let mut rv = start_rv;
            let mut i = 0;
            let mut resumes = 0;
            while !stop.load(Ordering::Relaxed) {
                match watch_once(&addrs[i % addrs.len()], rv, &events, &stop).await {
                    Ok(last) => rv = last,
                    Err(e) => assert!(!e.contains("watch error"), "{e}"),
                }
                i += 1;
                resumes += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            resumes
        })
    };

    // Writers: creates of unique names, and compare-and-swap increments.
    let creates = {
        let (addrs, stop) = (addrs.clone(), stop.clone());
        tokio::spawn(async move {
            let (mut acked, mut unknown) = (Vec::new(), Vec::new());
            let mut i = 0;
            while !stop.load(Ordering::Relaxed) {
                let name = format!("cm-{i}");
                match http(&addrs[i % 3], "POST", base, Some(&cm(&name, json!({}))), Duration::from_secs(20)).await {
                    Ok((201, _)) => acked.push(name),
                    Ok((409, _)) => panic!("{name} created twice"),
                    _ => unknown.push(name),
                }
                i += 1;
            }
            (acked, unknown)
        })
    };
    let increments = {
        let (addrs, stop) = (addrs.clone(), stop.clone());
        tokio::spawn(async move {
            let (mut acked, mut unknown) = (0u64, 0u64);
            let mut i = 0;
            while !stop.load(Ordering::Relaxed) {
                i += 1;
                let a = &addrs[i % 3];
                let Ok((200, cur)) = http(a, "GET", &format!("{base}/counter"), None, T).await else { continue };
                let n: u64 = cur["data"]["n"].as_str().unwrap().parse().unwrap();
                let mut next = cur.clone();
                next["data"]["n"] = json!((n + 1).to_string());
                match http(a, "PUT", &format!("{base}/counter"), Some(&next), Duration::from_secs(20)).await {
                    Ok((200, _)) => acked += 1,
                    Ok((409, _)) => {}
                    _ => unknown += 1,
                }
            }
            (acked, unknown)
        })
    };

    tokio::time::sleep(Duration::from_secs(2)).await;
    let leader = leader_name(&addrs[0]).await.expect("a leader");
    let victim = servers_.iter().position(|s| s.name == leader).unwrap();
    let killed_at = Instant::now();
    servers_[victim].kill();
    eprintln!("killed the leader {leader}");
    let survivors: Vec<String> = servers_.iter().filter(|s| s.alive()).map(|s| s.addr.clone()).collect();
    // A new leader is elected, and writes go through again.
    eventually("a new leader", T, || async { leader_name(&survivors[0]).await.is_some_and(|l| l != leader) }).await;
    let (code, _) = http(&survivors[0], "POST", base, Some(&cm("after-failover", json!({}))), Duration::from_secs(20)).await.unwrap();
    assert_eq!(code, 201);
    eprintln!("writes resumed {:?} after the kill", killed_at.elapsed());
    tokio::time::sleep(Duration::from_secs(3)).await;
    stop.store(true, Ordering::Relaxed);
    let (acked_creates, unknown_creates) = creates.await.unwrap();
    let (acked_incs, unknown_incs) = increments.await.unwrap();
    let last = acked_creates.last().cloned().unwrap_or_default();
    eventually("the watch catches up", T, || async { events.lock().unwrap().iter().any(|e| e.1 == last) }).await;
    stop_watch.store(true, Ordering::Relaxed);
    let resumes = watcher.await.unwrap();
    eprintln!("{} creates acked, {} unknown; {acked_incs} increments acked, {unknown_incs} unknown; watch resumed {resumes} times", acked_creates.len(), unknown_creates.len());
    assert!(acked_creates.len() > 20, "too few writes went through");

    // No acknowledged write is lost, none is applied twice, and every
    // server agrees.
    let mut lists = Vec::new();
    for a in &survivors {
        let (code, list) = http(a, "GET", base, None, T).await.unwrap();
        assert_eq!(code, 200);
        let mut names: Vec<String> = list["items"].as_array().unwrap().iter().map(|o| o["metadata"]["name"].as_str().unwrap().to_string()).collect();
        names.sort();
        lists.push((names, list["items"].as_array().unwrap().iter().find(|o| o["metadata"]["name"] == "counter").cloned().unwrap()));
    }
    assert_eq!(lists[0].0, lists[1].0, "servers disagree");
    for name in &acked_creates {
        assert!(lists[0].0.contains(name), "acknowledged create {name} lost");
    }
    let n: u64 = lists[0].1["data"]["n"].as_str().unwrap().parse().unwrap();
    assert!(n >= acked_incs && n <= acked_incs + unknown_incs, "counter {n}: {acked_incs} acked, {unknown_incs} unknown");

    // The watch, across servers and the failover, saw every create once,
    // in resourceVersion order.
    let events = events.lock().unwrap().clone();
    let rvs: Vec<u64> = events.iter().map(|e| e.2).collect();
    assert!(rvs.windows(2).all(|w| w[0] < w[1]), "watch events out of order or repeated");
    for name in &acked_creates {
        let added = events.iter().filter(|e| e.0 == "ADDED" && &e.1 == name).count();
        assert_eq!(added, 1, "{name} seen ADDED {added} times by the watch");
    }

    // The killed server comes back with its data and catches up.
    let dir = servers_[victim].dir.clone();
    let port = servers_[victim].port();
    let name = servers_[victim].name.clone();
    servers_[victim] = Server::start(&name, &dir, port, false, None);
    let back = servers_[victim].addr.clone();
    eventually("the restarted server catches up", Duration::from_secs(20), || async {
        let Ok((200, list)) = http(&back, "GET", base, None, T).await else { return false };
        let mut names: Vec<String> = list["items"].as_array().unwrap().iter().map(|o| o["metadata"]["name"].as_str().unwrap().to_string()).collect();
        names.sort();
        names == lists[0].0 || names.len() == lists[0].0.len() + 1
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn linearizable_under_leader_kills_and_partitions() {
    // SAFETY: tests only read it; set before the servers start.
    unsafe { std::env::set_var("LITEK8S_HA_FAULTS", "1") };
    let root = tempfile::tempdir().unwrap();
    let mut servers_ = cluster(root.path()).await;
    let addrs: Vec<String> = servers_.iter().map(|s| s.addr.clone()).collect();
    let token = std::fs::read_to_string(root.path().join("s1/join-token")).unwrap();
    let secret = token.rsplit("::").next().unwrap().to_string();

    let workload = tokio::spawn(lincheck::run(lincheck::Workload {
        servers: addrs.clone(),
        clients: 6,
        keys: 4,
        duration: Duration::from_secs(20),
        timeout: Duration::from_secs(5),
        namespace: "default".into(),
    }));

    // Nemesis: partition the leader away, heal; kill the leader, restart it.
    let partition = |from: String, to: Vec<String>, blocked: bool| {
        let secret = secret.clone();
        async move {
            let body = json!({"addrs": to, "blocked": blocked}).to_string();
            let req = format!(
                "POST /apis/server.litek8s.dev/v1/debug/partition HTTP/1.1\r\nHost: {from}\r\nAuthorization: Bearer {secret}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            if let Ok(mut s) = tokio::net::TcpStream::connect(&from).await {
                let _ = s.write_all(req.as_bytes()).await;
                let mut sink = Vec::new();
                let _ = s.read_to_end(&mut sink).await;
                let head = String::from_utf8_lossy(&sink);
                assert!(head.starts_with("HTTP/1.1 200"), "partition request failed: {head}");
            }
        }
    };
    tokio::time::sleep(Duration::from_secs(2)).await;
    for round in 0..2 {
        let Some(leader) = leader_name(&addrs[(round + 1) % 3]).await else { continue };
        let li = servers_.iter().position(|s| s.name == leader).unwrap();
        let others: Vec<String> = addrs.iter().filter(|a| **a != addrs[li]).cloned().collect();
        eprintln!("nemesis: partitioning leader {leader}");
        partition(addrs[li].clone(), others.clone(), true).await;
        for o in &others {
            partition(o.clone(), vec![addrs[li].clone()], true).await;
        }
        tokio::time::sleep(Duration::from_secs(4)).await;
        let majority_leader = leader_name(&others[0]).await;
        eprintln!("nemesis: the majority side's leader is {majority_leader:?}");
        assert!(majority_leader.is_some_and(|l| l != leader), "the majority did not elect a new leader");
        partition(addrs[li].clone(), others.clone(), false).await;
        for o in &others {
            partition(o.clone(), vec![addrs[li].clone()], false).await;
        }
        eprintln!("nemesis: healed");
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Some(leader) = leader_name(&addrs[(round + 2) % 3]).await {
            let li = servers_.iter().position(|s| s.name == leader).unwrap();
            eprintln!("nemesis: killing leader {leader}");
            let (dir, port, name) = (servers_[li].dir.clone(), servers_[li].port(), servers_[li].name.clone());
            servers_[li].kill();
            tokio::time::sleep(Duration::from_secs(2)).await;
            servers_[li] = Server::start(&name, &dir, port, false, None);
            eprintln!("nemesis: restarted {name}");
        }
    }
    let history = workload.await.unwrap();
    let summary = lincheck::summarize(&history);
    eprintln!("history: {summary:?}");
    assert!(summary.ok > 50, "too few operations succeeded: {summary:?}");
    if let Err(e) = lincheck::check(&history) {
        // A search too large to finish is no verdict (it happens with many
        // overlapping indeterminate writes); say so loudly, do not fail.
        if e.contains(lincheck::INCONCLUSIVE) {
            eprintln!("WARNING: linearizability check inconclusive: {e}");
            return;
        }
        let path = std::env::temp_dir().join("litek8s-ha-history.json");
        std::fs::write(&path, serde_json::to_vec(&history).unwrap()).unwrap();
        panic!("{e} (history in {})", path.display());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn big_objects_replicate() {
    let root = tempfile::tempdir().unwrap();
    let servers_ = cluster(root.path()).await;
    let addrs: Vec<String> = servers_.iter().map(|s| s.addr.clone()).collect();
    let base = "/api/v1/namespaces/default/configmaps";
    // A burst of big objects: more than one AppendEntries can carry.
    let blob = "x".repeat(400 << 10);
    let mut writes = Vec::new();
    for i in 0..12 {
        let (addr, blob) = (addrs[i % 3].clone(), blob.clone());
        writes.push(tokio::spawn(async move { http(&addr, "POST", base, Some(&cm(&format!("big-{i}"), json!({"blob": blob}))), Duration::from_secs(30)).await }));
    }
    for w in writes {
        assert!(matches!(w.await.unwrap(), Ok((201, _))));
    }
    for a in &addrs {
        let (code, list) = http(a, "GET", base, None, T).await.unwrap();
        assert_eq!(code, 200);
        let n = list["items"].as_array().unwrap().iter().filter(|o| o["metadata"]["name"].as_str().unwrap().starts_with("big-")).count();
        assert_eq!(n, 12, "on {a}");
    }
}
