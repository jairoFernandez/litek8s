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
    call_accept(addr, method, path, "application/json", content_type, body).await
}

async fn call_accept(addr: &str, method: &str, path: &str, accept: &str, content_type: &str, body: &str) -> Result<(u16, Value)> {
    let mut stream = TcpStream::connect(addr).await?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAccept: {accept}\r\n\
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
async fn reports_where_its_nodes_memory_goes() -> Result<()> {
    let addr = start().await?;
    let (code, mem) = get(&addr, "/litek8s/v1/memory").await?;
    assert_eq!(code, 200);
    assert!(mem["node"].as_str().is_some_and(|n| !n.is_empty()), "{mem}");
    for key in ["litek8s", "containers", "cache"] {
        assert!(mem[key].is_u64(), "{key}: {mem}");
    }
    if cfg!(target_os = "linux") {
        assert!(mem["meminfo"]["total"].as_u64().is_some_and(|t| t > 0), "{mem}");
    }
    let (code, cpu) = get(&addr, "/litek8s/v1/cpu").await?;
    assert_eq!(code, 200);
    assert!(cpu["litek8s_usec"].is_u64() && cpu["cgroups"].is_array(), "{cpu}");
    // Every node, with `*` as clients send it: literal (the UI) or
    // percent-encoded (kubectl get --raw '...?node=*').
    for q in ["*", "%2A"] {
        let (code, all) = get(&addr, &format!("/litek8s/v1/memory?node={q}")).await?;
        assert_eq!(code, 200, "node={q}: {all}");
        assert!(all.as_array().is_some_and(|a| !a.is_empty() && a[0]["litek8s"].is_u64()), "node={q}: {all}");
    }
    // An in-memory server keeps nothing on disk.
    let (code, _) = get(&addr, "/litek8s/v1/disk").await?;
    assert_eq!(code, 404);
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
async fn watch_from_an_expired_resource_version_ends_with_an_error_event() -> Result<()> {
    // A restarted server has no event history: older versions are expired.
    let dir = tempfile::tempdir()?;
    let addr = {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?.to_string();
        let state = Arc::new(ApiState::open(Some(dir.path()), addr.clone(), false)?);
        tokio::spawn(serve_listener(state, listener));
        addr
    };
    let base = "/api/v1/namespaces/default/configmaps";
    post(&addr, base, cm("a", json!({}))).await?;
    post(&addr, base, cm("b", json!({}))).await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let restarted = listener.local_addr()?.to_string();
    let state = Arc::new(ApiState::open(Some(dir.path()), restarted.clone(), false)?);
    tokio::spawn(serve_listener(state, listener));

    // Like kube-apiserver: 200 and an ERROR event, which makes client-go
    // relist (a 410 response makes its RetryWatcher retry forever).
    let (code, event) = get(&restarted, &format!("{base}?watch=true&resourceVersion=1")).await?;
    assert_eq!(code, 200);
    assert_eq!((event["type"].as_str(), event["object"]["code"].as_i64()), (Some("ERROR"), Some(410)));
    assert_eq!(event["object"]["reason"], "Expired");
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

/// A server with the controllers running (no node agent: pods do not run,
/// the test plays the kubelet by writing pod status).
async fn start_with_controllers() -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    let state = Arc::new(ApiState::open(None, addr.clone(), false)?);
    tokio::spawn(litek8s::kube::controllers::run(state.clone()));
    tokio::spawn(serve_listener(state, listener));
    Ok(addr)
}

async fn eventually<F, Fut>(what: &str, mut check: F) -> Result<Value>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Option<Value>>>,
{
    for _ in 0..100 {
        if let Some(v) = check().await? {
            return Ok(v);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("timed out waiting for {what}")
}

#[tokio::test]
async fn foreground_deletion_of_a_service_finishes() -> Result<()> {
    // Argo CD deletes an Application's resources in the foreground; the
    // Service's EndpointSlice (blockOwnerDeletion) must not come back.
    let addr = start_with_controllers().await?;
    let pod = json!({"metadata": {"name": "p", "labels": {"app": "x"}}, "spec": {"containers": [{"name": "c", "image": "nginx:1"}]}});
    let (_, pod) = post(&addr, "/api/v1/namespaces/default/pods", pod).await?;
    let mut status = pod.clone();
    status["status"] = json!({"phase": "Running", "podIP": "10.42.0.9",
        "conditions": [{"type": "Ready", "status": "True", "lastTransitionTime": "2026-01-01T00:00:00Z"}]});
    call(&addr, "PUT", "/api/v1/namespaces/default/pods/p/status", "application/json", &status.to_string()).await?;
    let svc = json!({"metadata": {"name": "web"}, "spec": {"selector": {"app": "x"}, "ports": [{"port": 80}]}});
    post(&addr, "/api/v1/namespaces/default/services", svc).await?;
    eventually("an EndpointSlice", || async {
        let (_, l) = get(&addr, "/apis/discovery.k8s.io/v1/namespaces/default/endpointslices?labelSelector=kubernetes.io%2Fservice-name%3Dweb").await?;
        Ok(l["items"].as_array().is_some_and(|a| !a.is_empty()).then_some(l))
    })
    .await?;
    let (code, _) = call(&addr, "DELETE", "/api/v1/namespaces/default/services/web", "application/json", r#"{"propagationPolicy":"Foreground"}"#).await?;
    assert_eq!(code, 200);
    eventually("the Service gone", || async {
        let (code, _) = get(&addr, "/api/v1/namespaces/default/services/web").await?;
        Ok((code == 404).then_some(Value::Null))
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn controllers_finish_foreground_deletions_started_before_they_ran() -> Result<()> {
    // As after a restart: the deletion began while no controller watched.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    let state = Arc::new(ApiState::open(None, addr.clone(), false)?);
    tokio::spawn(serve_listener(state.clone(), listener));
    let (_, cm_owner) = post(&addr, "/api/v1/namespaces/default/configmaps", cm("owner", json!({}))).await?;
    let mut dependent = cm("dependent", json!({}));
    dependent["metadata"]["ownerReferences"] = json!([{"apiVersion": "v1", "kind": "ConfigMap", "name": "owner",
        "uid": cm_owner["metadata"]["uid"], "blockOwnerDeletion": true}]);
    post(&addr, "/api/v1/namespaces/default/configmaps", dependent).await?;
    call(&addr, "DELETE", "/api/v1/namespaces/default/configmaps/owner", "application/json", r#"{"propagationPolicy":"Foreground"}"#).await?;
    let (code, _) = get(&addr, "/api/v1/namespaces/default/configmaps/owner").await?;
    assert_eq!(code, 200, "held by its foregroundDeletion finalizer");

    tokio::spawn(litek8s::kube::controllers::run(state));
    eventually("owner and dependent gone", || async {
        let (owner, _) = get(&addr, "/api/v1/namespaces/default/configmaps/owner").await?;
        let (dep, _) = get(&addr, "/api/v1/namespaces/default/configmaps/dependent").await?;
        Ok((owner == 404 && dep == 404).then_some(Value::Null))
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn deployment_rolls_out_through_replicasets_and_scheduler() -> Result<()> {
    let addr = start_with_controllers().await?;
    let node = json!({"metadata": {"name": "n1", "labels": {"kubernetes.io/hostname": "n1"}}});
    post(&addr, "/api/v1/nodes", node).await?;
    let status = json!({"metadata": {"name": "n1"}, "status": {
        "conditions": [{"type": "Ready", "status": "True"}],
        "allocatable": {"cpu": "4", "memory": "8Gi", "pods": "110"},
        "addresses": [{"type": "InternalIP", "address": "10.0.0.5"}],
    }});
    call(&addr, "PUT", "/api/v1/nodes/n1/status", "application/json", &status.to_string()).await?;

    let deploy = json!({"metadata": {"name": "web"}, "spec": {
        "replicas": 2,
        "selector": {"matchLabels": {"app": "web"}},
        "template": {"metadata": {"labels": {"app": "web"}}, "spec": {"containers": [{"name": "c", "image": "nginx:1"}]}},
    }});
    let (code, _) = post(&addr, "/apis/apps/v1/namespaces/default/deployments", deploy).await?;
    assert_eq!(code, 201);

    // Two pods, owned by the ReplicaSet, bound to the node.
    let pods = eventually("scheduled pods", || async {
        let (_, list) = get(&addr, "/api/v1/namespaces/default/pods?labelSelector=app%3Dweb").await?;
        let items = list["items"].as_array().cloned().unwrap_or_default();
        let bound = items.iter().filter(|p| p["spec"]["nodeName"] == "n1").count();
        Ok((bound == 2).then_some(json!(items)))
    })
    .await?;
    let rs_name = pods[0]["metadata"]["ownerReferences"][0]["name"].as_str().unwrap().to_string();
    assert!(rs_name.starts_with("web-"));
    assert!(pods[0]["metadata"]["labels"]["pod-template-hash"].is_string());
    assert_eq!(pods[0]["spec"]["serviceAccountName"], "default");

    // Play the kubelet: mark both pods Running and Ready.
    for pod in pods.as_array().unwrap() {
        let name = pod["metadata"]["name"].as_str().unwrap();
        // A status PUT replaces labels too, so send the whole pod.
        let mut status = pod.clone();
        status["status"] = json!({
            "phase": "Running",
            "conditions": [{"type": "Ready", "status": "True", "lastTransitionTime": "2026-01-01T00:00:00Z"}],
        });
        call(&addr, "PUT", &format!("/api/v1/namespaces/default/pods/{name}/status"), "application/json", &status.to_string()).await?;
    }
    let deploy = eventually("an available deployment", || async {
        let (_, d) = get(&addr, "/apis/apps/v1/namespaces/default/deployments/web").await?;
        Ok((d["status"]["availableReplicas"] == 2).then_some(d))
    })
    .await?;
    assert_eq!(deploy["status"]["observedGeneration"], deploy["metadata"]["generation"]);
    assert_eq!(deploy["metadata"]["annotations"]["deployment.kubernetes.io/revision"], "1");
    let conditions = deploy["status"]["conditions"].as_array().unwrap();
    assert!(conditions.iter().any(|c| c["type"] == "Available" && c["status"] == "True"));

    // Deleting the Deployment garbage-collects its ReplicaSet and pods.
    call(&addr, "DELETE", "/apis/apps/v1/namespaces/default/deployments/web", "application/json", "").await?;
    eventually("garbage collection", || async {
        let (_, pods) = get(&addr, "/api/v1/namespaces/default/pods").await?;
        let (_, rs) = get(&addr, "/apis/apps/v1/namespaces/default/replicasets").await?;
        // Bound pods terminate gracefully: with no kubelet on n1 they
        // stay with a deletionTimestamp, as in Kubernetes.
        let all_terminating = |l: &Value| l["items"].as_array().is_some_and(|a| a.iter().all(|o| o["metadata"]["deletionTimestamp"].is_string()));
        let empty = all_terminating(&pods) && rs["items"].as_array().is_some_and(|a| a.is_empty());
        Ok(empty.then_some(Value::Null))
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn jobs_complete_and_no_op_updates_keep_the_resource_version() -> Result<()> {
    let addr = start_with_controllers().await?;
    let job = json!({"metadata": {"name": "once"}, "spec": {"template": {"spec": {
        "restartPolicy": "Never", "containers": [{"name": "c", "image": "busybox"}],
    }}}});
    post(&addr, "/apis/batch/v1/namespaces/default/jobs", job).await?;
    let pod = eventually("a job pod", || async {
        let (_, list) = get(&addr, "/api/v1/namespaces/default/pods?labelSelector=job-name%3Donce").await?;
        Ok(list["items"].as_array().and_then(|a| a.first().cloned()))
    })
    .await?;
    let name = pod["metadata"]["name"].as_str().unwrap();
    let mut status = pod.clone();
    status["status"] = json!({"phase": "Succeeded"});
    call(&addr, "PUT", &format!("/api/v1/namespaces/default/pods/{name}/status"), "application/json", &status.to_string()).await?;
    eventually("a complete job", || async {
        let (_, j) = get(&addr, "/apis/batch/v1/namespaces/default/jobs/once").await?;
        let done = j["status"]["conditions"].as_array().is_some_and(|cs| cs.iter().any(|c| c["type"] == "Complete" && c["status"] == "True"));
        Ok(done.then_some(j))
    })
    .await?;

    let (_, a) = post(&addr, "/api/v1/namespaces/default/configmaps", cm("same", json!({"k": "v"}))).await?;
    let (code, b) = call(&addr, "PUT", "/api/v1/namespaces/default/configmaps/same", "application/json", &a.to_string()).await?;
    assert_eq!(code, 200);
    assert_eq!(a["metadata"]["resourceVersion"], b["metadata"]["resourceVersion"], "an update that changes nothing is not a write");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_service_creates_get_distinct_addresses() -> Result<()> {
    let addr = start().await?;
    // Helm creates a chart's Services in parallel.
    let creates = (0..16).map(|i| {
        let addr = addr.clone();
        tokio::spawn(async move {
            let svc = json!({
                "apiVersion": "v1", "kind": "Service", "metadata": {"name": format!("s{i}")},
                "spec": {"type": "NodePort", "ports": [{"port": 80}]}
            });
            post(&addr, "/api/v1/namespaces/default/services", svc).await
        })
    });
    let mut ips = std::collections::HashSet::new();
    let mut ports = std::collections::HashSet::new();
    for create in creates.collect::<Vec<_>>() {
        let (code, svc) = create.await??;
        assert_eq!(code, 201, "{svc}");
        assert!(ips.insert(svc["spec"]["clusterIP"].as_str().unwrap().to_string()), "duplicate ClusterIP: {svc}");
        assert!(ports.insert(svc["spec"]["ports"][0]["nodePort"].as_i64().unwrap()), "duplicate nodePort: {svc}");
    }
    Ok(())
}

/// An API server with the built-in observability store and the aggregator's
/// availability loop, and the store to feed node samples into.
async fn start_with_metrics() -> Result<(String, Arc<litek8s::obs::server::Obs>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    let state = Arc::new(ApiState::open(None, addr.clone(), false)?);
    let config = litek8s::obs::server::ObsConfig {
        tiers: litek8s::obs::tsdb::parse_tiers("1s:1h,10s:1d")?,
        interval: Duration::from_secs(1),
        ..Default::default()
    };
    let obs = litek8s::obs::server::start(&state, config)?;
    tokio::spawn(litek8s::kube::aggregator::run(state.clone()));
    tokio::spawn(serve_listener(state, listener));
    Ok((addr, obs))
}

/// Two samples a node agent would have sent, one interval apart: n1 at
/// 2 busy cores and 3 GiB used, pods web (default) and db (data).
fn feed_node_samples(obs: &litek8s::obs::server::Obs) -> Result<()> {
    let l = |pairs: &[(&str, &str)]| -> Vec<(String, String)> { pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() };
    let now = litek8s::obs::tsdb::now_ms();
    for (i, t) in [now - 1000, now].into_iter().enumerate() {
        let i = i as f64;
        let c = |name: &str, ns: &str, pod: &str| l(&[("__name__", name), ("node", "n1"), ("namespace", ns), ("pod", pod), ("container", "app")]);
        let cpu = |mode: &str| l(&[("__name__", "node_cpu_seconds_total"), ("node", "n1"), ("mode", mode)]);
        obs.db.append(
            "node/n1",
            t,
            vec![
                (cpu("user"), 100.0 + i * 1.5),
                (cpu("system"), 50.0 + i * 0.5),
                (cpu("idle"), 900.0 + i * 2.0),
                (l(&[("__name__", "node_memory_MemTotal_bytes"), ("node", "n1")]), 4.0 * (1u64 << 30) as f64),
                (l(&[("__name__", "node_memory_MemAvailable_bytes"), ("node", "n1")]), (1u64 << 30) as f64),
                (c("container_cpu_usage_seconds_total", "default", "web"), 10.0 + i * 0.25),
                (c("container_memory_working_set_bytes", "default", "web"), (64u64 << 20) as f64),
                (c("container_cpu_usage_seconds_total", "data", "db"), 20.0 + i * 0.5),
                (c("container_memory_working_set_bytes", "data", "db"), (128u64 << 20) as f64),
            ],
        )?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_api_serves_node_and_pod_metrics_from_the_store() -> Result<()> {
    let (addr, obs) = start_with_metrics().await?;
    let node = json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "n1", "labels": {"role": "edge"}}});
    assert_eq!(post(&addr, "/api/v1/nodes", node).await?.0, 201);
    assert_eq!(post(&addr, "/api/v1/namespaces", json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "data"}})).await?.0, 201);
    for (ns, name, app) in [("default", "web", "web"), ("data", "db", "db"), ("default", "idle", "idle")] {
        let pod = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name, "namespace": ns, "labels": {"app": app}},
            "spec": {"containers": [{"name": "app", "image": "nginx"}]}});
        assert_eq!(post(&addr, &format!("/api/v1/namespaces/{ns}/pods"), pod).await?.0, 201);
    }

    // Discovery, plain and aggregated, as kubectl top checks it.
    let (_, groups) = get(&addr, "/apis").await?;
    let group = groups["groups"].as_array().unwrap().iter().find(|g| g["name"] == "metrics.k8s.io").cloned().context("no metrics.k8s.io group")?;
    assert_eq!(group["preferredVersion"]["version"], "v1beta1");
    let (_, list) = get(&addr, "/apis/metrics.k8s.io/v1beta1").await?;
    let resources = list["resources"].as_array().unwrap();
    let nodes = resources.iter().find(|r| r["name"] == "nodes").context("no nodes")?;
    assert_eq!((nodes["kind"].as_str(), nodes["namespaced"].as_bool(), &nodes["verbs"]), (Some("NodeMetrics"), Some(false), &json!(["get", "list"])));
    assert!(resources.iter().any(|r| r["name"] == "pods" && r["kind"] == "PodMetrics" && r["namespaced"] == true));
    let (_, v2) = call_accept(&addr, "GET", "/apis", "application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList", "", "").await?;
    assert!(v2["items"].as_array().unwrap().iter().any(|g| g["metadata"]["name"] == "metrics.k8s.io"), "{v2}");
    // A built-in group: no APIService object (so metrics-server's Helm
    // chart can create its own).
    let (code, _) = get(&addr, "/apis/apiregistration.k8s.io/v1/apiservices/v1beta1.metrics.k8s.io").await?;
    assert_eq!(code, 404);

    // Nothing sampled yet: empty lists, not errors.
    let (code, v) = get(&addr, "/apis/metrics.k8s.io/v1beta1/nodes").await?;
    assert_eq!((code, v["kind"].as_str(), v["items"].as_array().map(Vec::len)), (200, Some("NodeMetricsList"), Some(0)));

    feed_node_samples(&obs)?;
    let (code, v) = get(&addr, "/apis/metrics.k8s.io/v1beta1/nodes").await?;
    assert_eq!(code, 200, "{v}");
    let n1 = &v["items"][0];
    assert_eq!((n1["kind"].as_str(), n1["metadata"]["name"].as_str(), n1["metadata"]["labels"]["role"].as_str()), (Some("NodeMetrics"), Some("n1"), Some("edge")));
    assert_eq!((n1["usage"]["cpu"].as_str(), n1["usage"]["memory"].as_str(), n1["window"].as_str()), (Some("2000000000n"), Some("3145728Ki"), Some("1s")));
    let (code, v) = get(&addr, "/apis/metrics.k8s.io/v1beta1/nodes/n1").await?;
    assert_eq!((code, v["usage"]["cpu"].as_str()), (200, Some("2000000000n")));
    let (code, v) = get(&addr, "/apis/metrics.k8s.io/v1beta1/nodes/n2").await?;
    assert_eq!((code, v["reason"].as_str(), v["message"].as_str()), (404, Some("NotFound"), Some("nodemetrics.metrics.k8s.io \"n2\" not found")));

    // Pods: all namespaces, one namespace, by label, by field, one by name.
    let names = |v: &Value| -> Vec<String> { v["items"].as_array().unwrap().iter().map(|i| format!("{}/{}", i["metadata"]["namespace"].as_str().unwrap(), i["metadata"]["name"].as_str().unwrap())).collect() };
    let (code, v) = get(&addr, "/apis/metrics.k8s.io/v1beta1/pods").await?;
    assert_eq!((code, v["kind"].as_str()), (200, Some("PodMetricsList")));
    assert_eq!(names(&v), ["data/db", "default/web"], "a pod without samples (idle) is left out");
    let (_, v) = get(&addr, "/apis/metrics.k8s.io/v1beta1/namespaces/default/pods").await?;
    assert_eq!(names(&v), ["default/web"]);
    let web = &v["items"][0];
    assert_eq!(web["containers"], json!([{"name": "app", "usage": {"cpu": "250000000n", "memory": "65536Ki"}}]));
    assert_eq!((web["metadata"]["labels"]["app"].as_str(), web["window"].as_str()), (Some("web"), Some("1s")));
    let (_, v) = get(&addr, "/apis/metrics.k8s.io/v1beta1/pods?labelSelector=app%3Ddb").await?;
    assert_eq!(names(&v), ["data/db"]);
    let (_, v) = get(&addr, "/apis/metrics.k8s.io/v1beta1/pods?fieldSelector=metadata.namespace%3Ddefault").await?;
    assert_eq!(names(&v), ["default/web"]);
    let (code, v) = get(&addr, "/apis/metrics.k8s.io/v1beta1/namespaces/data/pods/db").await?;
    assert_eq!((code, v["containers"][0]["usage"]["cpu"].as_str()), (200, Some("500000000n")));
    let (code, v) = get(&addr, "/apis/metrics.k8s.io/v1beta1/namespaces/default/pods/idle").await?;
    assert_eq!((code, v["message"].as_str()), (404, Some("podmetrics.metrics.k8s.io \"default/idle\" not found")));
    // kubectl get pods.metrics.k8s.io: a server-side table.
    let (_, t) = call_accept(&addr, "GET", "/apis/metrics.k8s.io/v1beta1/namespaces/default/pods", "application/json;as=Table;v=v1;g=meta.k8s.io,application/json", "", "").await?;
    assert_eq!((t["kind"].as_str(), &t["rows"][0]["cells"]), (Some("Table"), &json!(["web", "250000000n", "65536Ki", "1s"])));
    // Read-only, no watch.
    let (code, _) = call(&addr, "DELETE", "/apis/metrics.k8s.io/v1beta1/nodes/n1", "application/json", "").await?;
    assert_eq!(code, 405);

    // metrics-server's APIService takes the group over: requests go to its
    // Service (missing here: unavailable, out of discovery).
    let takeover = json!({"apiVersion": "apiregistration.k8s.io/v1", "kind": "APIService", "metadata": {"name": "v1beta1.metrics.k8s.io"},
        "spec": {"group": "metrics.k8s.io", "version": "v1beta1", "service": {"name": "metrics-server", "namespace": "kube-system"},
                 "insecureSkipTLSVerify": true, "groupPriorityMinimum": 100, "versionPriority": 100}});
    assert_eq!(post(&addr, "/apis/apiregistration.k8s.io/v1/apiservices", takeover).await?.0, 201);
    let (code, v) = get(&addr, "/apis/metrics.k8s.io/v1beta1/nodes").await?;
    assert_eq!((code, v["reason"].as_str()), (503, Some("ServiceUnavailable")), "{v}");
    eventually("metrics.k8s.io out of discovery", || async {
        let (code, _) = get(&addr, "/apis/metrics.k8s.io/v1beta1").await?;
        Ok((code == 404).then_some(Value::Null))
    })
    .await?;
    let (_, api) = get(&addr, "/apis/apiregistration.k8s.io/v1/apiservices/v1beta1.metrics.k8s.io").await?;
    assert_eq!((api["status"]["conditions"][0]["status"].as_str(), api["status"]["conditions"][0]["reason"].as_str()), (Some("False"), Some("ServiceNotFound")));
    // metrics-server uninstalled: the store answers again.
    let (code, _) = call(&addr, "DELETE", "/apis/apiregistration.k8s.io/v1/apiservices/v1beta1.metrics.k8s.io", "application/json", "").await?;
    assert_eq!(code, 200);
    eventually("metrics.k8s.io served again", || async {
        let (code, v) = get(&addr, "/apis/metrics.k8s.io/v1beta1").await?;
        Ok((code == 200).then_some(v))
    })
    .await?;
    feed_node_samples(&obs)?;
    let (code, v) = get(&addr, "/apis/metrics.k8s.io/v1beta1/nodes/n1").await?;
    assert_eq!((code, v["kind"].as_str()), (200, Some("NodeMetrics")), "{v}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Authentication and RBAC

/// A server in `mode`; the join token is an administrator's credential.
async fn start_authorized(mode: litek8s::kube::rbac::Mode) -> Result<(String, String, Arc<ApiState>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    let mut state = ApiState::open(None, addr.clone(), false)?;
    state.set_authorization_mode(mode);
    let state = Arc::new(state);
    let token = state.join_token();
    tokio::spawn(serve_listener(state.clone(), listener));
    Ok((addr, token, state))
}

/// One request with extra headers; (status, JSON body).
async fn call_as(addr: &str, method: &str, path: &str, headers: &[(&str, &str)], body: Option<Value>) -> Result<(u16, Value)> {
    let mut stream = TcpStream::connect(addr).await?;
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        request.push_str(&format!("{k}: {v}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(&body);
    stream.write_all(request.as_bytes()).await?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").context("malformed response")?;
    let status: u16 = head.split(' ').nth(1).context("no status")?.parse()?;
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { dechunk(body) } else { body.to_string() };
    Ok((status, serde_json::from_str(&body).unwrap_or(Value::String(body))))
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

async fn sa_token(addr: &str, admin: &str, ns: &str, sa: &str, audiences: &[&str]) -> Result<String> {
    let auth = bearer(admin);
    let h = [("Authorization", auth.as_str())];
    let (code, _) = call_as(addr, "POST", &format!("/api/v1/namespaces/{ns}/serviceaccounts"), &h, Some(json!({"metadata": {"name": sa}}))).await?;
    assert!(code == 201 || code == 409, "{code}");
    let spec = if audiences.is_empty() { json!({}) } else { json!({"audiences": audiences}) };
    let (code, tr) = call_as(addr, "POST", &format!("/api/v1/namespaces/{ns}/serviceaccounts/{sa}/token"), &h, Some(json!({"spec": spec}))).await?;
    assert_eq!(code, 201, "{tr}");
    Ok(tr["status"]["token"].as_str().context("no token")?.to_string())
}

#[tokio::test]
async fn rbac_authenticates_and_authorizes_requests() -> Result<()> {
    let (addr, admin, _state) = start_authorized(litek8s::kube::rbac::Mode::Rbac).await?;
    let admin_h = bearer(&admin);
    let admin_h = [("Authorization", admin_h.as_str())];
    // Anonymous: the public paths only.
    for path in ["/version", "/healthz", "/readyz", "/openid/v1/jwks", "/.well-known/openid-configuration"] {
        assert_eq!(get(&addr, path).await?.0, 200, "{path}");
    }
    for path in ["/api/v1/namespaces/default/secrets", "/api", "/metrics", "/litek8s/v1/memory"] {
        assert_eq!(get(&addr, path).await?.0, 401, "{path}");
    }
    assert_eq!(call_as(&addr, "GET", "/api/v1/secrets", &[("Authorization", "Bearer litek8s-no-client-auth")], None).await?.0, 401);
    assert_eq!(call_as(&addr, "GET", "/api/v1/secrets", &admin_h, None).await?.0, 200);
    // The identity HA servers forward is only trusted with the cluster secret.
    let forged = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, br#"{"username":"x","groups":["system:masters"]}"#);
    let spoof = [("x-litek8s-forwarded-user", forged.as_str()), ("x-litek8s-forwarded-auth", "guess")];
    assert_eq!(call_as(&addr, "GET", "/api/v1/secrets", &spoof, None).await?.0, 401);

    // A service account: discovery and self reviews, nothing else.
    let token = sa_token(&addr, &admin, "default", "ci", &[]).await?;
    let ci = bearer(&token);
    let ci = [("Authorization", ci.as_str())];
    assert_eq!(call_as(&addr, "GET", "/api", &ci, None).await?.0, 200);
    assert_eq!(call_as(&addr, "GET", "/apis/apps/v1", &ci, None).await?.0, 200);
    let (code, denied) = call_as(&addr, "GET", "/api/v1/namespaces/default/pods", &ci, None).await?;
    assert_eq!(code, 403);
    assert_eq!(
        denied["message"],
        "pods is forbidden: User \"system:serviceaccount:default:ci\" cannot list resource \"pods\" in API group \"\" in the namespace \"default\""
    );
    assert_eq!(denied["details"]["kind"], "pods");
    let (code, me) = call_as(&addr, "POST", "/apis/authentication.k8s.io/v1/selfsubjectreviews", &ci, Some(json!({}))).await?;
    assert_eq!(code, 201, "{me}");
    assert_eq!(me["status"]["userInfo"]["username"], "system:serviceaccount:default:ci");
    assert!(me["status"]["userInfo"]["groups"].as_array().unwrap().contains(&json!("system:serviceaccounts:default")));

    // Granted through a Role and RoleBinding.
    let role = json!({"metadata": {"name": "pods"}, "rules": [
        {"apiGroups": [""], "resources": ["pods"], "verbs": ["get", "list", "watch"]},
        {"apiGroups": [""], "resources": ["pods/exec"], "verbs": ["create"]},
        {"apiGroups": ["rbac.authorization.k8s.io"], "resources": ["roles", "rolebindings"], "verbs": ["create"]},
    ]});
    assert_eq!(call_as(&addr, "POST", "/apis/rbac.authorization.k8s.io/v1/namespaces/default/roles", &admin_h, Some(role)).await?.0, 201);
    let binding = json!({"metadata": {"name": "ci-pods"}, "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": "pods"}, "subjects": [{"kind": "ServiceAccount", "name": "ci", "namespace": "default"}]});
    assert_eq!(call_as(&addr, "POST", "/apis/rbac.authorization.k8s.io/v1/namespaces/default/rolebindings", &admin_h, Some(binding)).await?.0, 201);
    assert_eq!(call_as(&addr, "GET", "/api/v1/namespaces/default/pods", &ci, None).await?.0, 200);
    assert_eq!(call_as(&addr, "GET", "/api/v1/namespaces/default/pods?watch=true&timeoutSeconds=1", &ci, None).await?.0, 200);
    assert_eq!(call_as(&addr, "GET", "/api/v1/namespaces/kube-system/pods", &ci, None).await?.0, 403);
    assert_eq!(call_as(&addr, "GET", "/api/v1/namespaces/default/secrets", &ci, None).await?.0, 403);
    assert_eq!(call_as(&addr, "DELETE", "/api/v1/namespaces/default/pods/x", &ci, None).await?.0, 403);

    // kubectl auth can-i
    let can = |verb: &str, resource: &str, sub: &str| json!({"spec": {"resourceAttributes": {"namespace": "default", "verb": verb, "resource": resource, "subresource": sub}}});
    let (_, r) = call_as(&addr, "POST", "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews", &ci, Some(can("create", "pods", "exec"))).await?;
    assert_eq!(r["status"]["allowed"], true, "{r}");
    let (_, r) = call_as(&addr, "POST", "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews", &ci, Some(can("get", "secrets", ""))).await?;
    assert_eq!(r["status"]["allowed"], false, "{r}");
    let (_, r) = call_as(&addr, "POST", "/apis/authorization.k8s.io/v1/selfsubjectrulesreviews", &ci, Some(json!({"spec": {"namespace": "default"}}))).await?;
    assert!(r["status"]["resourceRules"].as_array().unwrap().iter().any(|rule| rule["resources"] == json!(["pods/exec"])), "{r}");
    // SubjectAccessReview needs system:auth-delegator (the admin has it).
    let sar = json!({"spec": {"user": "system:serviceaccount:default:ci", "groups": ["system:serviceaccounts"], "resourceAttributes": {"namespace": "default", "verb": "list", "resource": "pods"}}});
    assert_eq!(call_as(&addr, "POST", "/apis/authorization.k8s.io/v1/subjectaccessreviews", &ci, Some(sar.clone())).await?.0, 403);
    let (_, r) = call_as(&addr, "POST", "/apis/authorization.k8s.io/v1/subjectaccessreviews", &admin_h, Some(sar)).await?;
    assert_eq!(r["status"]["allowed"], true, "{r}");

    // Escalation: a role or binding may only grant what its author holds.
    let wider = json!({"metadata": {"name": "secrets"}, "rules": [{"apiGroups": [""], "resources": ["secrets"], "verbs": ["get"]}]});
    let (code, e) = call_as(&addr, "POST", "/apis/rbac.authorization.k8s.io/v1/namespaces/default/roles", &ci, Some(wider)).await?;
    assert_eq!(code, 403, "{e}");
    assert!(e["message"].as_str().unwrap().contains("not currently held"), "{e}");
    let narrower = json!({"metadata": {"name": "read-pods"}, "rules": [{"apiGroups": [""], "resources": ["pods"], "verbs": ["get"]}]});
    assert_eq!(call_as(&addr, "POST", "/apis/rbac.authorization.k8s.io/v1/namespaces/default/roles", &ci, Some(narrower)).await?.0, 201);
    let to_admin = json!({"metadata": {"name": "root"}, "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "cluster-admin"}, "subjects": [{"kind": "ServiceAccount", "name": "ci"}]});
    assert_eq!(call_as(&addr, "POST", "/apis/rbac.authorization.k8s.io/v1/namespaces/default/rolebindings", &ci, Some(to_admin)).await?.0, 403);

    // Impersonation needs the impersonate verb.
    let imp = [("Authorization", admin_h[0].1), ("Impersonate-User", "system:serviceaccount:default:ci")];
    assert_eq!(call_as(&addr, "GET", "/api/v1/namespaces/default/secrets", &imp, None).await?.0, 403);
    assert_eq!(call_as(&addr, "GET", "/api/v1/namespaces/default/pods", &imp, None).await?.0, 200);
    let imp = [("Authorization", ci[0].1), ("Impersonate-User", "admin"), ("Impersonate-Group", "system:masters")];
    assert_eq!(call_as(&addr, "GET", "/api/v1/namespaces/default/secrets", &imp, None).await?.0, 403);

    // Tokens for other audiences, or of a deleted account, are rejected.
    let vault = sa_token(&addr, &admin, "default", "ci", &["vault"]).await?;
    assert_eq!(call_as(&addr, "GET", "/api", &[("Authorization", bearer(&vault).as_str())], None).await?.0, 401);
    assert_eq!(call_as(&addr, "DELETE", "/api/v1/namespaces/default/serviceaccounts/ci", &admin_h, None).await?.0, 200);
    assert_eq!(call_as(&addr, "GET", "/api", &ci, None).await?.0, 401);
    Ok(())
}

#[tokio::test]
async fn rbac_audit_logs_but_allows() -> Result<()> {
    let (addr, admin, _state) = start_authorized(litek8s::kube::rbac::Mode::RbacAudit).await?;
    assert_eq!(get(&addr, "/api/v1/namespaces/default/secrets").await?.0, 200);
    let token = sa_token(&addr, &admin, "default", "app", &[]).await?;
    assert_eq!(call_as(&addr, "GET", "/api/v1/secrets", &[("Authorization", bearer(&token).as_str())], None).await?.0, 200);
    // Reviews still answer what RBAC decides.
    let ssar = json!({"spec": {"resourceAttributes": {"namespace": "default", "verb": "get", "resource": "secrets"}}});
    let (_, r) = call_as(&addr, "POST", "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews", &[("Authorization", bearer(&token).as_str())], Some(ssar)).await?;
    assert_eq!(r["status"]["allowed"], false, "{r}");
    Ok(())
}

#[tokio::test]
async fn default_cluster_roles_and_aggregation() -> Result<()> {
    let (addr, admin, state) = start_authorized(litek8s::kube::rbac::Mode::Rbac).await?;
    tokio::spawn(litek8s::kube::rbac::run_aggregation(state));
    let h = bearer(&admin);
    let h = [("Authorization", h.as_str())];
    for name in ["cluster-admin", "admin", "edit", "view", "system:auth-delegator", "system:basic-user", "system:discovery", "system:public-info-viewer"] {
        assert_eq!(call_as(&addr, "GET", &format!("/apis/rbac.authorization.k8s.io/v1/clusterroles/{name}"), &h, None).await?.0, 200, "{name}");
    }
    let (_, view) = call_as(&addr, "GET", "/apis/rbac.authorization.k8s.io/v1/clusterroles/view", &h, None).await?;
    assert!(view["rules"].as_array().unwrap().iter().any(|r| r["resources"].as_array().unwrap().contains(&json!("pods"))), "{view}");
    let extra = json!({"metadata": {"name": "widgets-view", "labels": {"rbac.authorization.k8s.io/aggregate-to-view": "true"}}, "rules": [{"apiGroups": ["example.com"], "resources": ["widgets"], "verbs": ["get"]}]});
    assert_eq!(call_as(&addr, "POST", "/apis/rbac.authorization.k8s.io/v1/clusterroles", &h, Some(extra)).await?.0, 201);
    // view, then edit (aggregates view), then admin (aggregates edit).
    for role in ["view", "edit", "admin"] {
        let mut found = false;
        for _ in 0..100 {
            let (_, r) = call_as(&addr, "GET", &format!("/apis/rbac.authorization.k8s.io/v1/clusterroles/{role}"), &h, None).await?;
            if r["rules"].as_array().unwrap().iter().any(|r| r["resources"] == json!(["widgets"])) {
                found = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(found, "{role} did not aggregate widgets-view");
    }
    // A user bound to `view` in a namespace reads pods there.
    let token = sa_token(&addr, &admin, "default", "viewer", &[]).await?;
    let binding = json!({"metadata": {"name": "viewer"}, "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "view"}, "subjects": [{"kind": "ServiceAccount", "name": "viewer"}]});
    assert_eq!(call_as(&addr, "POST", "/apis/rbac.authorization.k8s.io/v1/namespaces/default/rolebindings", &h, Some(binding)).await?.0, 201);
    let v = bearer(&token);
    assert_eq!(call_as(&addr, "GET", "/api/v1/namespaces/default/pods", &[("Authorization", v.as_str())], None).await?.0, 200);
    assert_eq!(call_as(&addr, "GET", "/api/v1/namespaces/default/secrets", &[("Authorization", v.as_str())], None).await?.0, 403);
    Ok(())
}

/// docs/specs/09-pod-certificates.md: what Agent Substrate needs from the
/// API: pods with podCertificate / clusterTrustBundle projections,
/// ClusterTrustBundles and PodCertificateRequests in v1 and v1beta1, and a
/// signer issuing through `/status`.
#[tokio::test]
async fn pod_certificates_and_cluster_trust_bundles() -> Result<()> {
    use base64::Engine;
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let addr = start().await?;
    let (_, discovery) = get(&addr, "/apis/certificates.k8s.io").await?;
    let versions: Vec<_> = discovery["versions"].as_array().unwrap().iter().map(|v| v["version"].clone()).collect();
    assert!(versions.contains(&json!("v1")) && versions.contains(&json!("v1beta1")), "{discovery}");

    let (code, node) = post(&addr, "/api/v1/nodes", json!({"metadata": {"name": "n1"}})).await?;
    assert_eq!(code, 201, "{node}");
    let (code, sa) = post(&addr, "/api/v1/namespaces/default/serviceaccounts", json!({"metadata": {"name": "agent"}})).await?;
    assert_eq!(code, 201, "{sa}");
    let pod = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "worker"}, "spec": {
        "nodeName": "n1", "serviceAccountName": "agent",
        "containers": [{"name": "c", "image": "busybox"}],
        "volumes": [{"name": "identity", "projected": {"sources": [
            {"podCertificate": {"signerName": "podidentity.podcert.ate.dev/identity", "keyType": "ECDSAP256",
                "maxExpirationSeconds": 86400, "credentialBundlePath": "credentials.pem",
                "userAnnotations": {"ate.dev/role": "worker"}}},
            {"clusterTrustBundle": {"signerName": "podidentity.podcert.ate.dev/identity", "labelSelector": {}, "path": "ca.pem"}},
            {"clusterTrustBundle": {"name": "egress-mitm", "path": "egress.pem", "optional": true}},
        ]}}],
    }});
    let (code, created) = post(&addr, "/api/v1/namespaces/default/pods?fieldValidation=Strict", pod.clone()).await?;
    assert_eq!(code, 201, "{created}");
    assert_eq!(created.pointer("/spec/volumes/0/projected/sources/0/podCertificate/keyType"), Some(&json!("ECDSAP256")));
    let mut bad = pod.clone();
    bad["metadata"]["name"] = json!("bad");
    bad["spec"]["volumes"][0]["projected"]["sources"][0]["podCertificate"]["keyType"] = json!("DSA");
    let (code, err) = post(&addr, "/api/v1/namespaces/default/pods", bad).await?;
    assert_eq!(code, 422, "{err}");
    assert!(err["message"].as_str().unwrap().contains("podCertificate.keyType: Unsupported value: \"DSA\""), "{err}");

    // A ClusterTrustBundle through v1beta1, read back through v1.
    let ca_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)?;
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key)?;
    let ctb = json!({"apiVersion": "certificates.k8s.io/v1beta1", "kind": "ClusterTrustBundle",
        "metadata": {"name": "podidentity.podcert.ate.dev:identity:v1"},
        "spec": {"signerName": "podidentity.podcert.ate.dev/identity", "trustBundle": ca.pem()}});
    let (code, out) = post(&addr, "/apis/certificates.k8s.io/v1beta1/clustertrustbundles", ctb.clone()).await?;
    assert_eq!(code, 201, "{out}");
    let (code, v1) = get(&addr, "/apis/certificates.k8s.io/v1/clustertrustbundles/podidentity.podcert.ate.dev:identity:v1").await?;
    assert_eq!((code, v1["apiVersion"].clone()), (200, json!("certificates.k8s.io/v1")));
    let (_, list) = get(&addr, "/apis/certificates.k8s.io/v1beta1/clustertrustbundles?fieldSelector=spec.signerName%3Dpodidentity.podcert.ate.dev%2Fidentity").await?;
    assert_eq!(list["items"].as_array().map(Vec::len), Some(1));
    let mut misnamed = ctb.clone();
    misnamed["metadata"]["name"] = json!("other");
    let (code, err) = post(&addr, "/apis/certificates.k8s.io/v1/clustertrustbundles", misnamed).await?;
    assert_eq!(code, 422, "{err}");

    // The kubelet's request: a stub CSR for the pod's new key.
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let mut csr = rcgen::CertificateParams::default();
    csr.distinguished_name = rcgen::DistinguishedName::new();
    let csr = csr.serialize_request(&key)?;
    let pcr = json!({"apiVersion": "certificates.k8s.io/v1beta1", "kind": "PodCertificateRequest",
        "metadata": {"generateName": "worker-"},
        "spec": {"signerName": "podidentity.podcert.ate.dev/identity", "podName": "worker",
            "podUID": created["metadata"]["uid"], "serviceAccountName": "agent", "serviceAccountUID": sa["metadata"]["uid"],
            "nodeName": "n1", "nodeUID": node["metadata"]["uid"], "stubPKCS10Request": b64(csr.der()),
            "unverifiedUserAnnotations": {"ate.dev/role": "worker"}}});
    let base = "/apis/certificates.k8s.io/v1beta1/namespaces/default/podcertificaterequests";
    // As upstream, only nodes are held to real pods (litek8s nodes ask
    // through the hub): a client may name any pod, as Substrate's CLI does.
    let mut outside = pcr.clone();
    outside["spec"]["podName"] = json!("kubectl-ate");
    outside["spec"]["podUID"] = json!("not-a-pod");
    let (code, out) = post(&addr, base, outside).await?;
    assert_eq!(code, 201, "{out}");
    let (code, req) = post(&addr, &format!("{base}?fieldValidation=Strict"), pcr).await?;
    assert_eq!(code, 201, "{req}");
    assert_eq!(req["spec"]["maxExpirationSeconds"], json!(86400));
    let name = req["metadata"]["name"].as_str().unwrap().to_string();

    // A signer issues it through /status (v1, as Substrate does when it can).
    let now = chrono::Utc::now().timestamp();
    let mut leaf = rcgen::CertificateParams::new(Vec::<String>::new())?;
    leaf.not_before = time::OffsetDateTime::from_unix_timestamp(now)?;
    leaf.not_after = time::OffsetDateTime::from_unix_timestamp(now + 3 * 3600)?;
    let leaf = leaf.signed_by(&key, &ca, &ca_key)?;
    let ts = |t: i64| chrono::DateTime::from_timestamp(t, 0).unwrap().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let v1_path = format!("/apis/certificates.k8s.io/v1/namespaces/default/podcertificaterequests/{name}");
    let (_, mut current) = get(&addr, &v1_path).await?;
    assert_eq!(current["apiVersion"], json!("certificates.k8s.io/v1"));
    current["status"] = json!({
        "conditions": [{"type": "Issued", "status": "True", "reason": "Issued", "message": "", "lastTransitionTime": ts(now)}],
        "certificateChain": leaf.pem(), "notBefore": ts(now), "notAfter": ts(now + 3 * 3600), "beginRefreshAt": ts(now + 2 * 3600),
    });
    let (code, issued) = call(&addr, "PUT", &format!("{v1_path}/status"), "application/json", &current.to_string()).await?;
    assert_eq!(code, 200, "{issued}");
    assert_eq!(issued["status"]["conditions"][0]["type"], json!("Issued"));
    // Issued is final.
    let mut again = issued.clone();
    again["status"]["beginRefreshAt"] = json!(ts(now + 2 * 3600 + 60));
    let (code, err) = call(&addr, "PUT", &format!("{v1_path}/status"), "application/json", &again.to_string()).await?;
    assert_eq!(code, 422, "{err}");
    // And the spec never changes.
    let mut respec = issued.clone();
    respec["spec"]["signerName"] = json!("example.com/other");
    let (code, err) = call(&addr, "PUT", &v1_path, "application/json", &respec.to_string()).await?;
    assert_eq!(code, 422, "{err}");
    Ok(())
}
