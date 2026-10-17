//! End-to-end tests of the Kubernetes-compatible API server over real HTTP.
//! Hermetic: an in-memory server on an ephemeral port, plain HTTP/1.1 from a
//! TcpStream. The kubectl/Helm level checks live in compat/.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use litek8s::kube::{ApiState, serve_listener};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

async fn start() -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    let state = Arc::new(ApiState::open(None, addr.clone(), false)?);
    tokio::spawn(serve_listener(state, listener));
    Ok(addr)
}

/// One request on a fresh connection; returns (status, parsed JSON body).
async fn call(addr: &str, method: &str, path: &str, content_type: &str, body: &str) -> Result<(u16, Value)> {
    let mut stream = TcpStream::connect(addr).await?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAccept: application/json\r\n\
         Content-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").context("malformed response")?;
    let status: u16 = head.split(' ').nth(1).context("no status")?.parse()?;
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        dechunk(body)
    } else {
        body.to_string()
    };
    let value = serde_json::from_str(&body).unwrap_or(Value::String(body));
    Ok((status, value))
}

fn dechunk(body: &str) -> String {
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
}

async fn post(addr: &str, path: &str, body: Value) -> Result<(u16, Value)> {
    call(addr, "POST", path, "application/json", &body.to_string()).await
}

async fn get(addr: &str, path: &str) -> Result<(u16, Value)> {
    call(addr, "GET", path, "application/json", "").await
}

fn cm(name: &str, data: Value) -> Value {
    json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name, "labels": {"app": "x"}}, "data": data})
}

#[tokio::test]
async fn discovery_and_default_objects() -> Result<()> {
    let addr = start().await?;
    let (code, version) = get(&addr, "/version").await?;
    assert_eq!(code, 200);
    assert_eq!(version["major"], "1");
    let (_, groups) = get(&addr, "/apis").await?;
    let names: Vec<_> = groups["groups"].as_array().unwrap().iter().map(|g| g["name"].clone()).collect();
    assert!(names.contains(&json!("apps")));
    let (_, ns) = get(&addr, "/api/v1/namespaces/default").await?;
    assert_eq!(ns["status"]["phase"], "Active");
    let (code, _) = get(&addr, "/api/v1/namespaces/default/serviceaccounts/default").await?;
    assert_eq!(code, 200);
    let (_, svc) = get(&addr, "/api/v1/namespaces/default/services/kubernetes").await?;
    assert_eq!(svc["spec"]["clusterIP"], "10.43.0.1");
    Ok(())
}

#[tokio::test]
async fn crud_patch_and_conflicts() -> Result<()> {
    let addr = start().await?;
    let base = "/api/v1/namespaces/default/configmaps";
    let (code, created) = post(&addr, base, cm("a", json!({"k": "1"}))).await?;
    assert_eq!(code, 201);
    assert!(created["metadata"]["uid"].as_str().is_some_and(|u| u.len() == 36));
    let (code, dup) = post(&addr, base, cm("a", json!({}))).await?;
    assert_eq!((code, dup["reason"].clone()), (409, json!("AlreadyExists")));

    let (code, patched) = call(&addr, "PATCH", &format!("{base}/a"), "application/merge-patch+json", r#"{"data":{"j":"2"}}"#).await?;
    assert_eq!(code, 200);
    assert_eq!(patched["data"], json!({"k": "1", "j": "2"}));

    let mut stale = created.clone();
    stale["data"] = json!({"k": "stale"});
    let (code, conflict) = call(&addr, "PUT", &format!("{base}/a"), "application/json", &stale.to_string()).await?;
    assert_eq!((code, conflict["reason"].clone()), (409, json!("Conflict")));

    let (code, _) = call(&addr, "PATCH", &format!("{base}/a"), "application/json-patch+json", r#"[{"op":"remove","path":"/data/k"}]"#).await?;
    assert_eq!(code, 200);
    let (_, current) = get(&addr, &format!("{base}/a")).await?;
    assert_eq!(current["data"], json!({"j": "2"}));

    post(&addr, base, json!({"metadata": {"name": "b", "labels": {"app": "y"}}})).await?;
    let (_, list) = get(&addr, &format!("{base}?labelSelector=app%3Dy")).await?;
    let names: Vec<_> = list["items"].as_array().unwrap().iter().map(|i| i["metadata"]["name"].clone()).collect();
    assert_eq!(names, vec![json!("b")]);

    let (code, _) = call(&addr, "DELETE", &format!("{base}/a"), "application/json", "").await?;
    assert_eq!(code, 200);
    let (code, missing) = get(&addr, &format!("{base}/a")).await?;
    assert_eq!((code, missing["message"].clone()), (404, json!("configmaps \"a\" not found")));
    Ok(())
}

#[tokio::test]
async fn watch_resumes_from_resource_version() -> Result<()> {
    let addr = start().await?;
    let base = "/api/v1/namespaces/default/configmaps";
    let (_, first) = post(&addr, base, cm("before", json!({}))).await?;
    let rv = first["metadata"]["resourceVersion"].as_str().unwrap().to_string();

    let mut stream = TcpStream::connect(&addr).await?;
    let request = format!(
        "GET {base}?watch=true&resourceVersion={rv}&timeoutSeconds=5 HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    post(&addr, base, cm("after", json!({}))).await?;

    let mut reader = BufReader::new(stream);
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !seen.contains("\"after\"") && tokio::time::Instant::now() < deadline {
        let mut line = String::new();
        if tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line)).await.is_err() || line.is_empty() {
            break;
        }
        seen.push_str(&line);
    }
    assert!(seen.contains(r#""type":"ADDED""#), "watch output: {seen}");
    assert!(seen.contains("\"after\""), "watch output: {seen}");
    assert!(!seen.contains("\"before\""), "replayed an older object: {seen}");
    Ok(())
}

#[tokio::test]
async fn crds_serve_custom_resources() -> Result<()> {
    let addr = start().await?;
    let crd = json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": "widgets.example.dev"},
        "spec": {
            "group": "example.dev",
            "scope": "Namespaced",
            "names": {"plural": "widgets", "singular": "widget", "kind": "Widget"},
            "versions": [{"name": "v1", "served": true, "storage": true, "subresources": {"status": {}}}],
        },
    });
    let (code, stored) = post(&addr, "/apis/apiextensions.k8s.io/v1/customresourcedefinitions", crd).await?;
    assert_eq!(code, 201);
    assert_eq!(stored["status"]["conditions"][1]["type"], "Established");
    let path = "/apis/example.dev/v1/namespaces/default/widgets";
    let (code, w) = post(&addr, path, json!({"apiVersion": "example.dev/v1", "kind": "Widget", "metadata": {"name": "w"}, "spec": {"size": 3}, "status": {"ignored": true}})).await?;
    assert_eq!(code, 201);
    assert_eq!(w["status"], Value::Null, "status is dropped on create when it is a subresource");
    let (code, _) = call(&addr, "PUT", &format!("{path}/w/status"), "application/json", &json!({"metadata": {"name": "w"}, "status": {"ready": true}}).to_string()).await?;
    assert_eq!(code, 200);
    let (_, w) = get(&addr, &format!("{path}/w")).await?;
    assert_eq!((w["spec"]["size"].clone(), w["status"]["ready"].clone()), (json!(3), json!(true)));
    let (code, _) = call(&addr, "PATCH", &format!("{path}/w"), "application/strategic-merge-patch+json", "{}").await?;
    assert_eq!(code, 415, "strategic merge patch is rejected for custom resources");

    call(&addr, "DELETE", "/apis/apiextensions.k8s.io/v1/customresourcedefinitions/widgets.example.dev", "application/json", "").await?;
    let (code, _) = get(&addr, &format!("{path}/w")).await?;
    assert_eq!(code, 404);
    Ok(())
}

#[tokio::test]
async fn namespace_deletion_cascades_and_finalizers_block() -> Result<()> {
    let addr = start().await?;
    post(&addr, "/api/v1/namespaces", json!({"metadata": {"name": "tmp"}})).await?;
    post(&addr, "/api/v1/namespaces/tmp/configmaps", cm("inside", json!({}))).await?;
    let held = json!({"metadata": {"name": "held", "finalizers": ["example.dev/hold"]}});
    post(&addr, "/api/v1/namespaces/default/configmaps", held).await?;

    let (code, _) = call(&addr, "DELETE", "/api/v1/namespaces/default/configmaps/held", "application/json", "").await?;
    assert_eq!(code, 200);
    let (code, still) = get(&addr, "/api/v1/namespaces/default/configmaps/held").await?;
    assert_eq!(code, 200, "an object with finalizers survives delete");
    assert!(still["metadata"]["deletionTimestamp"].is_string());
    call(&addr, "PATCH", "/api/v1/namespaces/default/configmaps/held", "application/merge-patch+json", r#"{"metadata":{"finalizers":null}}"#).await?;
    let (code, _) = get(&addr, "/api/v1/namespaces/default/configmaps/held").await?;
    assert_eq!(code, 404, "removing the last finalizer completes the delete");

    call(&addr, "DELETE", "/api/v1/namespaces/tmp", "application/json", "").await?;
    let (code, _) = get(&addr, "/api/v1/namespaces/tmp/configmaps/inside").await?;
    assert_eq!(code, 404);
    let (code, _) = post(&addr, "/api/v1/namespaces/tmp/configmaps", cm("late", json!({}))).await?;
    assert_eq!(code, 404, "creating in a missing namespace fails");
    Ok(())
}
