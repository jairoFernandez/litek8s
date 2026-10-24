//! `litek8s mcp` end to end: the binary driven over stdio (JSON-RPC lines)
//! against an in-process API server, as Claude Code would. Hermetic: a
//! temporary kubeconfig and dev home, no Docker, no node (pods stay
//! Pending, which the diagnosis must explain). Also checks the guardrails
//! against fake servers that are not litek8s.

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use litek8s::kube::{ApiState, serve_listener};
use litek8s::obs::server::ObsConfig;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::TcpListener;
use tokio::process::{Child, ChildStdin, ChildStdout};

/// Argo CD's Application CRD, reduced to what the API server needs.
const APP_CRD: &str = "apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: {name: applications.argoproj.io}
spec:
  group: argoproj.io
  scope: Namespaced
  names: {kind: Application, plural: applications, singular: application, shortNames: [app, apps], listKind: ApplicationList}
  versions:
  - name: v1alpha1
    served: true
    storage: true
    schema: {openAPIV3Schema: {type: object, x-kubernetes-preserve-unknown-fields: true}}
    additionalPrinterColumns:
    - {name: Sync Status, type: string, jsonPath: .status.sync.status}
    - {name: Health Status, type: string, jsonPath: .status.health.status}
";

/// An app with the status a GitOps controller would have written.
const APP: &str = "apiVersion: v1
kind: Namespace
metadata: {name: argocd}
---
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata: {name: web, namespace: argocd}
spec:
  project: default
  source: {repoURL: 'https://git.example.com/web.git', path: deploy, targetRevision: main}
  destination: {server: 'https://kubernetes.default.svc', namespace: web}
  syncPolicy: {automated: {prune: false}, syncOptions: [CreateNamespace=true]}
status:
  sync: {status: OutOfSync, revision: 0123456789abcdef0123456789abcdef01234567}
  health: {status: Degraded}
  resources:
  - {group: apps, version: v1, kind: Deployment, namespace: web, name: web, status: OutOfSync, health: {status: Degraded, message: exceeded its progress deadline}}
  - {version: v1, kind: Service, namespace: web, name: web, status: Synced, health: {status: Healthy}}
  - {version: v1, kind: ConfigMap, namespace: web, name: old, status: OutOfSync, requiresPruning: true}
";

async fn start_api() -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    let state = Arc::new(ApiState::open(None, addr.clone(), false)?);
    let config = ObsConfig {
        tiers: litek8s::obs::tsdb::parse_tiers("1s:1h,10s:1d")?,
        interval: Duration::from_secs(1),
        eval_interval: Duration::from_secs(1),
        ..ObsConfig::default()
    };
    litek8s::obs::server::start(&state, config)?;
    // Controllers, but no node agent: pods are created and stay Pending.
    tokio::spawn(litek8s::kube::controllers::run(state.clone()));
    tokio::spawn(serve_listener(state, listener));
    Ok(addr)
}

/// An HTTP server that answers every request with `body`, and notes
/// whether any request carried credentials.
async fn fake_http(body: Value, saw_auth: Arc<AtomicBool>) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let body = body.to_string();
            let saw = saw_auth.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                if String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase().contains("\nauthorization:") {
                    saw.store(true, Ordering::SeqCst);
                }
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    Ok(addr)
}

/// A Unix-socket Docker API that is not litek8s's.
fn fake_docker(path: &std::path::Path) -> Result<()> {
    let listener = tokio::net::UnixListener::bind(path)?;
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let _ = s.read(&mut buf).await;
                let body = json!({"Platform": {"Name": "Docker Engine - Community"}, "Version": "28.0.0"}).to_string();
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    Ok(())
}

struct Mcp {
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next: i64,
}

impl Mcp {
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        let id = self.next;
        let mut line = serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        line.push(b'\n');
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await?;
        loop {
            let l = tokio::time::timeout(Duration::from_secs(60), self.stdout.next_line()).await.context("no answer within 60s")??.context("the server exited")?;
            let v: Value = serde_json::from_str(&l).with_context(|| format!("not JSON on stdout: {l}"))?;
            if v["id"] == id {
                return Ok(v);
            }
        }
    }

    async fn notify(&mut self, method: &str) -> Result<()> {
        let mut line = serde_json::to_vec(&json!({"jsonrpc": "2.0", "method": method}))?;
        line.push(b'\n');
        self.stdin.write_all(&line).await?;
        Ok(())
    }

    /// A tool call: (isError, structured result or the error text).
    async fn call(&mut self, name: &str, args: Value) -> Result<(bool, Value)> {
        let r = self.request("tools/call", json!({"name": name, "arguments": args})).await?;
        let res = &r["result"];
        if res.is_null() {
            bail!("tools/call {name}: {r}");
        }
        let err = res["isError"].as_bool().unwrap_or(false);
        let text = res["content"][0]["text"].as_str().unwrap_or_default().to_string();
        if err {
            return Ok((true, Value::String(text)));
        }
        // The text block is the same JSON as structuredContent.
        assert_eq!(serde_json::from_str::<Value>(&text)?, res["structuredContent"], "{name}");
        Ok((false, res["structuredContent"].clone()))
    }

    async fn ok(&mut self, name: &str, args: Value) -> Result<Value> {
        let (err, v) = self.call(name, args).await?;
        if err {
            bail!("{name} failed: {v}");
        }
        Ok(v)
    }
}

fn spawn(dir: &std::path::Path, extra: &[&str]) -> Result<Mcp> {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_litek8s"))
        .arg("mcp")
        .args(extra)
        .env("KUBECONFIG", dir.join("kubeconfig"))
        .env("LITEK8S_DEV_HOME", dir.join("dev"))
        .env("LITEK8S_DOCKER_SOCKET", dir.join("docker.sock"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child.stdin.take().context("stdin")?;
    let stdout = BufReader::new(child.stdout.take().context("stdout")?).lines();
    Ok(Mcp { _child: child, stdin, stdout, next: 0 })
}

const MANIFEST: &str = "
apiVersion: v1
kind: ConfigMap
metadata: {name: web-config}
data: {greeting: hello}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: web, labels: {app: web}}
spec:
  replicas: 1
  selector: {matchLabels: {app: web}}
  template:
    metadata: {labels: {app: web}}
    spec:
      containers:
      - name: web
        image: nginx:1.27
        readinessProbe: {httpGet: {path: /healthz, port: 80}}
";

#[tokio::test(flavor = "multi_thread")]
async fn drives_a_litek8s_cluster_over_stdio() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let api = start_api().await?;
    let saw_auth = Arc::new(AtomicBool::new(false));
    let k3s = fake_http(json!({"major": "1", "minor": "33", "gitVersion": "v1.33.4+k3s1"}), saw_auth.clone()).await?;
    std::fs::write(
        dir.path().join("kubeconfig"),
        format!(
            "apiVersion: v1\nkind: Config\ncurrent-context: prod\n\
             clusters:\n- name: t\n  cluster: {{server: 'http://{api}'}}\n- name: prod\n  cluster: {{server: 'http://{k3s}'}}\n\
             contexts:\n- name: t\n  context: {{cluster: t, user: u}}\n- name: prod\n  context: {{cluster: prod, user: p}}\n\
             users:\n- name: u\n  user: {{}}\n- name: p\n  user: {{token: very-secret}}\n"
        ),
    )?;
    fake_docker(&dir.path().join("docker.sock"))?;
    let mut m = spawn(dir.path(), &["--context", "t", "--max-output", "8000"])?;

    let init = m.request("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}})).await?;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert!(init["result"]["instructions"].as_str().unwrap().contains("diagnose_pod"));
    m.notify("notifications/initialized").await?;
    let tools = m.request("tools/list", json!({})).await?;
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for t in ["diagnose_pod", "apply", "delete", "logs_search", "query", "cluster_up", "docker_ps"] {
        assert!(names.contains(&t), "{t} missing from {names:?}");
    }

    // Apply, then apply again: nothing changes the second time.
    let r = m.ok("apply", json!({"manifest": MANIFEST})).await?;
    assert_eq!(r["objects"][0]["result"], "created", "{r}");
    assert_eq!(r["objects"][1]["result"], "created", "{r}");
    assert_eq!(r["objects"][1]["namespace"], "default");
    let r = m.ok("apply", json!({"manifest": MANIFEST})).await?;
    assert_eq!(r["objects"][1]["result"], "unchanged", "{r}");
    let r = m.ok("apply", json!({"manifest": "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: dry}\n", "dry_run": true})).await?;
    assert_eq!(r["objects"][0]["result"], "would be created", "{r}");
    let (err, _) = m.call("get", json!({"kind": "configmaps", "name": "dry"})).await?;
    assert!(err, "a dry run saved nothing");
    let r = m.ok("apply", json!({"manifest": "apiVersion: example.com/v1\nkind: Nope\nmetadata: {name: x}\n"})).await?;
    assert!(r["objects"][0]["error"].as_str().unwrap().contains("does not serve"), "{r}");

    // The table, then the controllers' pod with its owner chain.
    let r = m.ok("get", json!({"kind": "deploy", "namespace": "default"})).await?;
    assert_eq!(r["rows"][0]["Name"], "web", "{r}");
    let deadline = Instant::now() + Duration::from_secs(20);
    let pod = loop {
        let r = m.ok("get", json!({"kind": "pods", "namespace": "default", "label_selector": "app=web"})).await?;
        if let Some(name) = r["rows"][0]["Name"].as_str() {
            break name.to_string();
        }
        if Instant::now() > deadline {
            bail!("no pod for the deployment: {r}");
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    let r = m.ok("get", json!({"kind": "po", "name": pod})).await?;
    assert!(r["object"]["metadata"].get("managedFields").is_none(), "managedFields are left out");
    let owners: Vec<&str> = r["owners"].as_array().unwrap().iter().map(|o| o["kind"].as_str().unwrap()).collect();
    assert_eq!(owners, ["ReplicaSet", "Deployment"], "{r}");
    assert!(r["owners"][1]["status"].as_str().unwrap().starts_with("ready 0/1"), "{r}");
    let d = m.ok("describe", json!({"kind": "deployment", "name": "web"})).await?;
    assert!(d["describe"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("web")), "{d}");

    // No node: the diagnosis says it cannot be scheduled.
    let r = m.ok("diagnose_pod", json!({"selector": "app=web"})).await?;
    assert_eq!(r["ready"], false, "{r}");
    let reason = r["findings"][0]["reason"].as_str().unwrap();
    assert!(["Unschedulable", "NotScheduled"].contains(&reason), "{r}");
    assert!(r["findings"][0]["next_step"].as_str().unwrap().contains("nodes"), "{r}");
    assert_eq!(r["owners"][1]["kind"], "Deployment");
    assert!(r["containers"][0]["readinessProbe"].as_str().unwrap().contains("HTTP GET :80/healthz"), "{r}");
    let (err, msg) = m.call("diagnose_pod", json!({"selector": "app=nothing"})).await?;
    assert!(err && msg.as_str().unwrap().contains("no pods"), "{msg}");

    // Deleting needs confirm: first only a preview.
    let r = m.ok("delete", json!({"kind": "deployment", "name": "web"})).await?;
    assert_eq!(r["confirmed"], false, "{r}");
    assert_eq!(r["would_delete"], "Deployment/web");
    m.ok("get", json!({"kind": "deployment", "name": "web"})).await?;
    let r = m.ok("delete", json!({"kind": "deployment", "name": "web", "confirm": true})).await?;
    assert_eq!(r["deleted"], true, "{r}");
    let (err, _) = m.call("get", json!({"kind": "deployment", "name": "web"})).await?;
    assert!(err, "deleted");

    // Observability.
    let r = m.ok("query", json!({"query": "vector(42)"})).await?;
    assert_eq!(r["result"][0]["value"][1], "42", "{r}");
    let r = m.ok("query_range", json!({"query": "vector(1)", "range": "5m", "step": "1m"})).await?;
    assert_eq!(r["resultType"], "matrix", "{r}");
    let (err, msg) = m.call("query", json!({"query": "sum(("})).await?;
    assert!(err, "{msg}");
    let r = m.ok("alerts_list", json!({})).await?;
    assert!(r["alerts"].is_array() && r["silences"].is_array(), "{r}");
    let r = m.ok("alert_silence", json!({"matchers": ["alertname=KubePodCrashLooping", "namespace=default"], "duration": "1h"})).await?;
    let silence = r["silence"].as_str().context("silence name")?.to_string();
    let r = m.ok("alerts_list", json!({})).await?;
    assert_eq!(r["silences"][0]["name"], silence.as_str(), "{r}");
    m.ok("alert_unsilence", json!({"name": silence})).await?;
    let r = m.ok("logs_search", json!({"namespace": "default", "grep": "error"})).await?;
    assert_eq!(r["count"], 0, "no nodes, no logs: {r}");
    let r = m.ok("events", json!({"namespace": "default"})).await?;
    assert!(r["events"].is_array(), "{r}");

    // Argo CD Applications, with no controller: their status as one would
    // write it, a sync as the operation it leaves for one.
    let r = m.ok("applications_list", json!({})).await;
    assert!(r.is_err(), "no Application CRD yet");
    m.ok("apply", json!({"manifest": APP_CRD})).await?;
    let mut applied = Value::Null;
    for _ in 0..50 {
        applied = m.ok("apply", json!({"manifest": APP})).await?;
        if applied["objects"][1]["error"].is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(applied["objects"][1]["error"].is_null(), "{applied}");
    let r = m.ok("applications_list", json!({})).await?;
    assert_eq!(r["applications"][0]["name"], "web", "{r}");
    assert_eq!((r["applications"][0]["sync"].clone(), r["applications"][0]["health"].clone()), (json!("OutOfSync"), json!("Degraded")), "{r}");
    assert_eq!(r["not_synced_or_healthy"], 1);
    let r = m.ok("application_status", json!({"name": "web"})).await?;
    let attention = r["resources_needing_attention"].as_array().unwrap();
    assert_eq!(attention.len(), 2, "the Deployment and the ConfigMap to prune, not the Service: {r}");
    assert_eq!(attention[0]["message"], "exceeded its progress deadline", "{r}");
    assert_eq!(r["resources_total"], 3);
    let r = m.ok("application_sync", json!({"name": "web"})).await?;
    assert_eq!(r["confirmed"], false, "{r}");
    assert_eq!(r["out_of_sync"], json!(["Deployment/web/web"]), "{r}");
    assert!(r["note"].as_str().unwrap().contains("prune"), "{r}");
    let r = m.ok("application_sync", json!({"name": "web", "prune": true, "confirm": true})).await?;
    assert_eq!(r["started"], true, "{r}");
    let r = m.ok("get", json!({"kind": "applications.argoproj.io", "name": "web", "namespace": "argocd"})).await?;
    assert_eq!(r["object"]["operation"], json!({"initiatedBy": {"username": "litek8s-mcp"}, "sync": {"syncStrategy": {"hook": {}}, "syncOptions": ["CreateNamespace=true"], "prune": true}}), "{r}");
    let (err, msg) = m.call("application_sync", json!({"name": "web", "confirm": true})).await?;
    assert!(err && msg.as_str().unwrap().contains("already has an operation"), "{msg}");
    let r = m.ok("application_refresh", json!({"name": "web", "wait_seconds": 0})).await?;
    assert_eq!(r["refreshed"], false, "no controller takes the annotation off: {r}");

    // A big result is cut to --max-output with a hint.
    for i in 0..60 {
        m.ok("apply", json!({"manifest": format!("apiVersion: v1\nkind: ConfigMap\nmetadata: {{name: bulk-{i}}}\ndata: {{k: '{}'}}\n", "x".repeat(200))})).await?;
    }
    let r = m.ok("get", json!({"kind": "configmaps", "namespace": "default", "full": true})).await?;
    assert!(r["_truncated"]["hint"].as_str().unwrap().contains("label_selector"), "{}", r["_truncated"]);
    assert!(serde_json::to_string(&r)?.len() <= 8000);

    // Guardrails: a context whose server is not litek8s is refused, and its
    // token is never sent.
    let (err, msg) = m.call("get", json!({"kind": "pods", "context": "prod"})).await?;
    assert!(err, "{msg}");
    let msg = msg.as_str().unwrap();
    assert!(msg.contains("refusing") && msg.contains("k3s"), "{msg}");
    let (err, _) = m.call("delete", json!({"kind": "pods", "name": "x", "context": "prod", "confirm": true})).await?;
    assert!(err);
    assert!(!saw_auth.load(Ordering::SeqCst), "credentials went to a server that is not litek8s");
    let (err, msg) = m.call("docker_ps", json!({})).await?;
    assert!(err && msg.as_str().unwrap().contains("not litek8s"), "{msg}");
    let r = m.ok("contexts_list", json!({"probe": true})).await?;
    let probe: Vec<(String, Value)> = r["contexts"].as_array().unwrap().iter().map(|c| (c["name"].as_str().unwrap().to_string(), c["litek8s"].clone())).collect();
    assert!(probe.contains(&("t".into(), json!(true))) && probe.contains(&("prod".into(), json!(false))), "{probe:?}");

    // Dev clusters in an empty dev home.
    let r = m.ok("clusters_list", json!({})).await?;
    assert_eq!(r["clusters"], json!([]), "{r}");
    let r = m.ok("cluster_status", json!({"name": "nope"})).await?;
    assert_eq!((r["exists"].clone(), r["running"].clone()), (json!(false), json!(false)), "{r}");
    let (err, msg) = m.call("cluster_delete", json!({"name": "nope", "confirm": true})).await?;
    assert!(err && msg.as_str().unwrap().contains("no dev cluster"), "{msg}");
    let r = m.ok("cluster_down", json!({"name": "nope"})).await?;
    assert_eq!(r["confirmed"], false, "{r}");

    // Resources.
    let r = m.request("resources/read", json!({"uri": "litek8s://skill/SKILL.md"})).await?;
    assert!(r["result"]["contents"][0]["text"].as_str().unwrap().starts_with("---\nname: litek8s"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_hides_mutating_tools() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let api = start_api().await?;
    std::fs::write(dir.path().join("kubeconfig"), format!("apiVersion: v1\nkind: Config\ncurrent-context: t\nclusters:\n- name: t\n  cluster: {{server: 'http://{api}'}}\ncontexts:\n- name: t\n  context: {{cluster: t, user: u}}\nusers:\n- name: u\n  user: {{}}\n"))?;
    let mut m = spawn(dir.path(), &["--read-only"])?;
    m.request("initialize", json!({"protocolVersion": "2025-06-18"})).await?;
    let tools = m.request("tools/list", json!({})).await?;
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(!names.contains(&"apply") && !names.contains(&"delete") && !names.contains(&"cluster_up"), "{names:?}");
    let r = m.request("tools/call", json!({"name": "apply", "arguments": {"manifest": "x"}})).await?;
    assert_eq!(r["error"]["code"], -32602, "{r}");
    // The current context of $KUBECONFIG when none is given.
    let r = m.request("tools/call", json!({"name": "get", "arguments": {"kind": "namespaces"}})).await?;
    assert_eq!(r["result"]["isError"], false, "{r}");
    Ok(())
}
