//! The built-in observability over real HTTP: the Prometheus API (what
//! Grafana calls), federation, alert rules / receivers / silences as
//! objects, an Alertmanager-style webhook, the log search endpoint.
//! Hermetic: an in-memory API server with no node, fast intervals.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use litek8s::kube::{ApiState, serve_listener};
use litek8s::obs::server::ObsConfig;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn start() -> Result<String> {
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
    tokio::spawn(serve_listener(state, listener));
    Ok(addr)
}

async fn call(addr: &str, method: &str, path: &str, content_type: &str, body: &str) -> Result<(u16, String)> {
    let mut stream = TcpStream::connect(addr).await?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAccept: application/json\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").context("malformed response")?;
    let status: u16 = head.split(' ').nth(1).context("no status")?.parse()?;
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        let mut out = String::new();
        let mut rest = body;
        while let Some((size, tail)) = rest.split_once("\r\n") {
            let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
            if n == 0 {
                break;
            }
            out.push_str(&tail[..n]);
            rest = &tail[n + 2..];
        }
        out
    } else {
        body.to_string()
    };
    Ok((status, body))
}

async fn get(addr: &str, path: &str) -> Result<(u16, Value)> {
    let (code, body) = call(addr, "GET", path, "application/json", "").await?;
    Ok((code, serde_json::from_str(&body).unwrap_or(Value::String(body))))
}

async fn post(addr: &str, path: &str, body: Value) -> Result<(u16, Value)> {
    let (code, body) = call(addr, "POST", path, "application/json", &body.to_string()).await?;
    Ok((code, serde_json::from_str(&body).unwrap_or(Value::String(body))))
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

async fn until<F: Fn(&Value) -> bool>(addr: &str, path: &str, what: &str, ok: F) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (code, v) = get(addr, path).await?;
        if code == 200 && ok(&v) {
            return Ok(v);
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {what}: last {code} {v}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn result(v: &Value) -> Vec<Value> {
    v["data"]["result"].as_array().cloned().unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn prometheus_api_answers_like_prometheus() -> Result<()> {
    let addr = start().await?;
    let pod = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "web", "namespace": "default", "labels": {"app": "web"}},
        "spec": {"containers": [{"name": "c", "image": "nginx", "resources": {"requests": {"cpu": "250m", "memory": "64Mi"}}}]}});
    assert_eq!(post(&addr, "/api/v1/namespaces/default/pods", pod).await?.0, 201);
    // kube-state metrics of the store: the pod is Pending (no node).
    let q = enc(r#"kube_pod_status_phase{namespace="default",pod="web",phase="Pending"}"#);
    let v = until(&addr, &format!("/prometheus/api/v1/query?query={q}"), "the pod's phase", |v| !result(v).is_empty()).await?;
    assert_eq!(v["data"]["resultType"], "vector");
    assert_eq!(result(&v)[0]["value"][1], "1");
    assert_eq!(result(&v)[0]["metric"]["job"], "kube-state-metrics");
    let q = enc(r#"sum(kube_pod_container_resource_requests{resource="cpu"})"#);
    let (_, v) = get(&addr, &format!("/prometheus/api/v1/query?query={q}")).await?;
    assert_eq!(result(&v)[0]["value"][1], "0.25");
    // Grafana's health check, scalars and POSTed forms.
    let (code, body) = call(&addr, "POST", "/prometheus/api/v1/query", "application/x-www-form-urlencoded", "query=1%2B1").await?;
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body)?;
    assert_eq!((v["data"]["resultType"].as_str(), v["data"]["result"][1].as_str()), (Some("scalar"), Some("2")));
    // A range query over the samples so far: one point per step.
    until(&addr, "/prometheus/api/v1/query?query=up", "two samples of up", |_| true).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let now = chrono::Utc::now().timestamp();
    let q = enc(r#"up{job="apiserver"}"#);
    let (_, v) = get(&addr, &format!("/prometheus/api/v1/query_range?query={q}&start={}&end={now}&step=1", now - 2)).await?;
    assert_eq!(v["data"]["resultType"], "matrix");
    assert!(result(&v)[0]["values"].as_array().unwrap().len() >= 2, "{v}");
    // Series, labels, values, metadata, buildinfo.
    let (_, v) = get(&addr, &format!("/prometheus/api/v1/series?match[]={}", enc("kube_pod_info"))).await?;
    assert_eq!(v["data"][0]["pod"], "web");
    let (_, v) = get(&addr, "/prometheus/api/v1/labels").await?;
    assert!(v["data"].as_array().unwrap().contains(&json!("namespace")));
    let (_, v) = get(&addr, "/prometheus/api/v1/label/__name__/values").await?;
    let names = v["data"].as_array().unwrap();
    for n in ["apiserver_request_total", "kube_pod_info", "litek8s_tsdb_series", "process_resident_memory_bytes"] {
        assert!(names.contains(&json!(n)), "{n} missing from {names:?}");
    }
    let (_, v) = get(&addr, "/prometheus/api/v1/metadata").await?;
    assert_eq!(v["data"]["apiserver_request_total"][0]["type"], "counter");
    let (_, v) = get(&addr, "/prometheus/api/v1/status/buildinfo").await?;
    assert!(v["data"]["version"].as_str().unwrap().starts_with('2'));
    // Errors as Prometheus words them.
    let (code, v) = get(&addr, &format!("/prometheus/api/v1/query?query={}", enc("sum(rate(x[5m]"))).await?;
    assert_eq!((code, v["status"].as_str(), v["errorType"].as_str()), (400, Some("error"), Some("bad_data")));
    // Federation for a real Prometheus.
    let (code, text) = call(&addr, "GET", &format!("/prometheus/federate?match[]={}", enc("kube_pod_info")), "", "").await?;
    assert_eq!(code, 200);
    assert!(text.starts_with("kube_pod_info{"), "{text}");
    // No node: the log search finds nothing, without failing.
    let (code, body) = call(&addr, "GET", "/litek8s/v1/logs?since=1h&grep=x", "", "").await?;
    assert_eq!((code, body.as_str()), (200, ""));
    let (_, v) = get(&addr, "/litek8s/v1/observability").await?;
    assert!(v["store"]["series"].as_u64().unwrap() > 20, "{v}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn alert_rules_receivers_and_silences() -> Result<()> {
    let addr = start().await?;
    // A webhook receiver that records what it gets.
    let hook = TcpListener::bind("127.0.0.1:0").await?;
    let hook_addr = hook.local_addr()?;
    let (got_tx, mut got) = tokio::sync::mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = hook.accept().await else { return };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = s.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(i) = text.find("\r\n\r\n") {
                    let len: usize = text[..i].lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length: ").map(|v| v.trim().parse().unwrap_or(0))).unwrap_or(0);
                    if buf.len() >= i + 4 + len {
                        let _ = got_tx.send(serde_json::from_slice(&buf[i + 4..i + 4 + len]).unwrap_or_default());
                        break;
                    }
                }
            }
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
        }
    });
    // The CRDs are installed by the server.
    until(&addr, "/apis/observability.litek8s.dev/v1", "the alerting CRDs", |v| v["resources"].as_array().is_some_and(|r| r.len() >= 3)).await?;
    let receiver = json!({"apiVersion": "observability.litek8s.dev/v1", "kind": "AlertReceiver", "metadata": {"name": "hook"},
        "spec": {"webhook": {"url": format!("http://{hook_addr}/alerts")}, "matchers": [{"name": "team", "value": "web"}]}});
    assert_eq!(post(&addr, "/apis/observability.litek8s.dev/v1/alertreceivers", receiver).await?.0, 201);
    let rule = json!({"apiVersion": "observability.litek8s.dev/v1", "kind": "AlertRule", "metadata": {"name": "web", "namespace": "default"},
        "spec": {"groups": [{"name": "web", "rules": [
            {"alert": "AlwaysOn", "expr": "vector(42)", "labels": {"severity": "warning", "team": "web"}, "annotations": {"summary": "value {{ $value }}"}},
            {"alert": "Broken", "expr": "sum(", "labels": {"team": "web"}},
        ]}]}});
    assert_eq!(post(&addr, "/apis/observability.litek8s.dev/v1/namespaces/default/alertrules", rule).await?.0, 201);
    let v = until(&addr, "/litek8s/v1/alerts", "the rule firing", |v| v["alerts"].as_array().is_some_and(|a| a.iter().any(|a| a["labels"]["alertname"] == "AlwaysOn" && a["state"] == "firing"))).await?;
    assert!(v["ruleErrors"]["default/web/Broken"].as_str().is_some(), "{v}");
    let alert = v["alerts"].as_array().unwrap().iter().find(|a| a["labels"]["alertname"] == "AlwaysOn").unwrap().clone();
    assert_eq!(alert["annotations"]["summary"], "value 42");
    // The webhook gets Alertmanager's payload.
    let payload = tokio::time::timeout(Duration::from_secs(20), got.recv()).await?.context("no webhook call")?;
    assert_eq!(payload["version"], "4");
    assert_eq!(payload["status"], "firing");
    assert_eq!(payload["alerts"][0]["labels"]["alertname"], "AlwaysOn");
    // Prometheus's own alerts and rules endpoints, and the ALERTS series.
    let (_, v) = get(&addr, "/prometheus/api/v1/alerts").await?;
    assert!(v["data"]["alerts"].as_array().unwrap().iter().any(|a| a["labels"]["alertname"] == "AlwaysOn"));
    let (_, v) = get(&addr, "/prometheus/api/v1/rules").await?;
    let groups = v["data"]["groups"].as_array().unwrap();
    assert!(groups.iter().any(|g| g["name"] == "builtin"), "the built-in rules are listed too");
    until(&addr, &format!("/prometheus/api/v1/query?query={}", enc(r#"ALERTS{alertname="AlwaysOn",alertstate="firing"}"#)), "ALERTS", |v| !result(v).is_empty()).await?;
    // A silence mutes it.
    let silence = json!({"apiVersion": "observability.litek8s.dev/v1", "kind": "Silence", "metadata": {"name": "quiet"},
        "spec": {"matchers": [{"name": "alertname", "value": "Always.*", "isRegex": true}], "endsAt": "2099-01-01T00:00:00Z", "comment": "test"}});
    assert_eq!(post(&addr, "/apis/observability.litek8s.dev/v1/silences", silence).await?.0, 201);
    until(&addr, "/litek8s/v1/alerts", "the silence applied", |v| {
        v["alerts"].as_array().is_some_and(|a| a.iter().any(|a| a["labels"]["alertname"] == "AlwaysOn" && a["silenced_by"] == json!(["quiet"])))
    })
    .await?;
    Ok(())
}
