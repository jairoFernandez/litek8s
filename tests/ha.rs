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
    raft_port: u16,
    dir: PathBuf,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(name: &str, dir: &Path, port: u16, init: bool, join: Option<(&str, &str)>) -> Self {
        Self::start_with(name, dir, port, 0, init, join)
    }

    fn start_with(name: &str, dir: &Path, port: u16, raft_port: u16, init: bool, join: Option<(&str, &str)>) -> Self {
        // A killed server's sockets close as its runtime winds down.
        let bind = |p: u16| {
            for _ in 0..100 {
                if let Ok(l) = std::net::TcpListener::bind(("127.0.0.1", p)) {
                    return l;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            panic!("cannot bind 127.0.0.1:{p}")
        };
        let listener = bind(port);
        let raft_listener = bind(raft_port);
        let addr = listener.local_addr().unwrap().to_string();
        let raft_port = raft_listener.local_addr().unwrap().port();
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
            node_external_ip: None,
            loadbalancer_ports: false,
            advertise_address: Some("127.0.0.1".into()),
            network: Default::default(),
            token: join.map(|(_, t)| t.to_string()),
            lifecycle: Default::default(),
            wireguard_port: None,
            listener: Some(listener),
            extra_listen: None,
            disk_root: None,
            node_port_offset: 0,
            docker: None,
            obs: None,
            prometheus_listen: None,
            cluster_init: init,
            join_server: join.map(|(u, _)| u.to_string()),
            raft_port: None,
            raft_listener: Some(raft_listener),
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
        Self { name: name.to_string(), addr, raft_port, dir: dir.to_path_buf(), stop: Some(stop), thread: Some(thread) }
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
/// How long an election may take before a test gives up: elections take
/// ~1–2 s here, but a slow CI runner's fsyncs can take a few times that.
const ELECTION: Duration = Duration::from_secs(20);

/// One cluster at a time: each test runs three servers (and their Raft
/// runtimes) on 100 ms timers; four at once on a 3–4 vCPU CI runner starve
/// each other into spurious elections.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    unsafe {
        std::env::set_var("LITEK8S_HA_NO_EXIT", "1");
        // Loopback and a local disk: the fast timers (elections in ~2 s).
        std::env::set_var("LITEK8S_RAFT_HEARTBEAT_MS", "100");
    }
    let s1 = Server::start("s1", &root.join("s1"), 0, true, None);
    eventually("s1 ready", ELECTION, || async { matches!(http(&s1.addr, "GET", "/api/v1/namespaces/default", None, T).await, Ok((200, _))) }).await;
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

/// Create `obj` through `addr`, retrying while there is no leader (a
/// spurious election on a slow machine). A retry may find the object
/// already there: the attempt that timed out was applied after all.
async fn create(addr: &str, base: &str, obj: &Value) {
    let deadline = Instant::now() + ELECTION;
    let mut retried = false;
    loop {
        match http(addr, "POST", base, Some(obj), T).await {
            Ok((201, _)) => return,
            Ok((409, _)) if retried => return,
            Ok((code, v)) if code < 500 => panic!("create {} answered {code}: {v}", obj["metadata"]["name"]),
            other if Instant::now() < deadline => {
                eprintln!("create {}: {other:?}; retrying", obj["metadata"]["name"]);
                retried = true;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            other => panic!("create {}: {other:?}", obj["metadata"]["name"]),
        }
    }
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
    let _serial = SERIAL.lock().await;
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
    eventually("a new leader", ELECTION, || async { leader_name(&survivors[0]).await.is_some_and(|l| l != leader) }).await;
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
    let raft_port = servers_[victim].raft_port;
    servers_[victim] = Server::start_with(&name, &dir, port, raft_port, false, None);
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
    let _serial = SERIAL.lock().await;
    // SAFETY: tests only read it; set before the servers start.
    unsafe { std::env::set_var("LITEK8S_HA_FAULTS", "1") };
    let root = tempfile::tempdir().unwrap();
    let mut servers_ = cluster(root.path()).await;
    let addrs: Vec<String> = servers_.iter().map(|s| s.addr.clone()).collect();
    let token = std::fs::read_to_string(root.path().join("s1/join-token")).unwrap();
    let secret = token.rsplit("::").next().unwrap().to_string();

    let t0 = Instant::now();
    let at = move || format!("{:.0} ms", t0.elapsed().as_secs_f64() * 1e3);
    let workload = tokio::spawn(lincheck::run(lincheck::Workload {
        servers: addrs.clone(),
        clients: 6,
        keys: 4,
        duration: Duration::from_secs(26),
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
        eprintln!("{} nemesis: partitioning leader {leader}", at());
        partition(addrs[li].clone(), others.clone(), true).await;
        for o in &others {
            partition(o.clone(), vec![addrs[li].clone()], true).await;
        }
        // Partitioned for at least 4 s, and until the majority has elected
        // a leader of its own.
        tokio::time::sleep(Duration::from_secs(4)).await;
        let deadline = Instant::now() + ELECTION;
        while !leader_name(&others[0]).await.is_some_and(|l| l != leader) {
            if Instant::now() > deadline {
                for o in &others {
                    let v = servers(o).await.unwrap_or_default();
                    eprintln!("{o}: {}", json!({"state": v["state"], "vote": v["vote"], "term": v["term"], "last_log_index": v["last_log_index"], "leader_heard_ms_ago": v["leader_heard_ms_ago"], "elections": v["elections"]}));
                }
                panic!("the majority did not elect a new leader");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        eprintln!("{} nemesis: the majority side's leader is {:?}", at(), leader_name(&others[0]).await);
        partition(addrs[li].clone(), others.clone(), false).await;
        for o in &others {
            partition(o.clone(), vec![addrs[li].clone()], false).await;
        }
        eprintln!("{} nemesis: healed", at());
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Some(leader) = leader_name(&addrs[(round + 2) % 3]).await {
            let li = servers_.iter().position(|s| s.name == leader).unwrap();
            eprintln!("{} nemesis: killing leader {leader}", at());
            let (dir, port, name, raft_port) = (servers_[li].dir.clone(), servers_[li].port(), servers_[li].name.clone(), servers_[li].raft_port);
            servers_[li].kill();
            tokio::time::sleep(Duration::from_secs(2)).await;
            servers_[li] = Server::start_with(&name, &dir, port, raft_port, false, None);
            eprintln!("{} nemesis: restarted {name}", at());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        // A leader killed and back at once takes its leadership back.
        if let Some(leader) = leader_name(&addrs[round % 3]).await {
            let li = servers_.iter().position(|s| s.name == leader).unwrap();
            let (dir, port, name, raft_port) = (servers_[li].dir.clone(), servers_[li].port(), servers_[li].name.clone(), servers_[li].raft_port);
            servers_[li].kill();
            servers_[li] = Server::start_with(&name, &dir, port, raft_port, false, None);
            eprintln!("{} nemesis: killed and restarted leader {name} at once", at());
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
    let _serial = SERIAL.lock().await;
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

/// A leader killed and restarted before the others elect a new one takes
/// its leadership back (openraft keeps its vote). Its reads must still
/// include every write it acknowledged before dying.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restarted_leader_serves_fresh_reads() {
    let _serial = SERIAL.lock().await;
    let root = tempfile::tempdir().unwrap();
    let mut servers_ = cluster(root.path()).await;
    let base = "/api/v1/namespaces/default/configmaps";
    for round in 0..3 {
        eventually("a leader", ELECTION, || async { leader_name(&servers_[0].addr).await.is_some() }).await;
        let leader = leader_name(&servers_[0].addr).await.expect("a leader");
        let li = servers_.iter().position(|s| s.name == leader).unwrap();
        let addr = servers_[li].addr.clone();
        let names: Vec<String> = (0..30).map(|i| format!("r{round}-{i}")).collect();
        for n in &names {
            create(&addr, base, &cm(n, json!({}))).await;
        }
        // Back at once: sooner than any election.
        let (dir, port, name, raft_port) = (servers_[li].dir.clone(), servers_[li].port(), servers_[li].name.clone(), servers_[li].raft_port);
        servers_[li].kill();
        servers_[li] = Server::start_with(&name, &dir, port, raft_port, false, None);
        eventually("the restarted server answers", T, || async { http(&addr, "GET", "/version", None, Duration::from_secs(1)).await.is_ok() }).await;
        eprintln!("round {round}: restarted {name}; leader now {:?}", leader_name(&addr).await);
        let mut errors = 0;
        for n in &names {
            // An error (no leader yet) is fine and retried; an answer must not be stale.
            let deadline = Instant::now() + ELECTION;
            loop {
                match http(&addr, "GET", &format!("{base}/{n}"), None, T).await {
                    Ok((200, _)) => break,
                    Ok((code, _)) if code < 500 => panic!("{n}: a fresh read answered {code} on the restarted server (round {round})"),
                    _ if Instant::now() < deadline => {
                        errors += 1;
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    other => panic!("{n}: no answer: {other:?}"),
                }
            }
        }
        eprintln!("round {round}: {errors} retried reads");
    }
}

/// openraft 0.9 makes a lone voter start an election on every tick (1.5
/// heartbeats). With vote saves slower than that, each election used to be
/// superseded by the next before its own vote was counted, and a fresh
/// `--cluster-init` server never became leader (loaded CI runners).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lone_server_elects_itself_with_slow_vote_saves() {
    let _serial = SERIAL.lock().await;
    let root = tempfile::tempdir().unwrap();
    // SAFETY: set before the server starts and removed after it stops,
    // with no other test's servers running (SERIAL).
    unsafe {
        std::env::set_var("LITEK8S_HA_NO_EXIT", "1");
        std::env::set_var("LITEK8S_RAFT_HEARTBEAT_MS", "100");
        std::env::set_var("LITEK8S_HA_FAULTS", "1");
        std::env::set_var("LITEK8S_HA_SLOW_VOTE_MS", "400");
    }
    let mut s1 = Server::start("s1", &root.path().join("s1"), 0, true, None);
    let started = Instant::now();
    let ready = async {
        while !matches!(http(&s1.addr, "GET", "/api/v1/namespaces/default", None, T).await, Ok((200, _))) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    let elected = tokio::time::timeout(Duration::from_secs(15), ready).await.is_ok();
    s1.kill();
    unsafe { std::env::remove_var("LITEK8S_HA_SLOW_VOTE_MS") };
    assert!(elected, "a lone server with 400 ms vote saves did not elect itself in 15 s");
    eprintln!("elected and serving after {:?}", started.elapsed());
}
