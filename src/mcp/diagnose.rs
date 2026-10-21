//! "Why is this pod not ready?": what a person reads in `kubectl describe`,
//! `kubectl logs --previous` and the node's state, turned into findings
//! with the evidence and a next step. [`analyze`] is pure (tested below);
//! the MCP tool gathers its inputs.

use serde_json::{Value, json};

fn s<'a>(v: &'a Value, p: &str) -> &'a str {
    v.pointer(p).and_then(Value::as_str).unwrap_or_default()
}

fn finding(severity: &str, reason: &str, message: String, next: &str) -> Value {
    json!({"severity": severity, "reason": reason, "message": message, "next_step": next})
}

/// How a probe checks, in words: `HTTP GET :8080/healthz`.
pub fn describe_probe(p: &Value) -> String {
    let port = |v: &Value| match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => "?".into(),
    };
    let mut d = if let Some(h) = p.get("httpGet") {
        format!("HTTP GET {}:{}{}", h["scheme"].as_str().filter(|s| !s.eq_ignore_ascii_case("http")).map(|s| format!("{}://", s.to_lowercase())).unwrap_or_default(), port(&h["port"]), h["path"].as_str().unwrap_or("/"))
    } else if let Some(t) = p.get("tcpSocket") {
        format!("TCP connect :{}", port(&t["port"]))
    } else if let Some(e) = p.get("exec") {
        let cmd: Vec<&str> = e["command"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        format!("exec {cmd:?}")
    } else if let Some(g) = p.get("grpc") {
        format!("gRPC health :{}", port(&g["port"]))
    } else {
        "unknown probe".into()
    };
    let n = |k: &str, def: u64| p.get(k).and_then(Value::as_u64).unwrap_or(def);
    d.push_str(&format!(" (initialDelay {}s, period {}s, timeout {}s, failureThreshold {})", n("initialDelaySeconds", 0), n("periodSeconds", 10), n("timeoutSeconds", 1), n("failureThreshold", 3)));
    d
}

/// What an exit code usually means.
pub fn exit_meaning(code: i64, reason: &str) -> &'static str {
    match (code, reason) {
        (_, "OOMKilled") => "killed for exceeding its memory limit",
        (0, _) => "exited successfully (a long-running container should not exit)",
        (1, _) => "application error (see its logs)",
        (2, _) => "misuse of a shell builtin or bad arguments",
        (126, _) => "command found but not executable",
        (127, _) => "command not found (check command/args and the image)",
        (128..=255, _) if code == 137 => "killed with SIGKILL (memory limit, liveness probe or a stop that timed out)",
        (128..=255, _) if code == 143 => "terminated with SIGTERM",
        (128..=255, _) if code == 139 => "segmentation fault",
        _ => "non-zero exit",
    }
}

/// Container states in one line each, for the result.
pub fn container_summary(pod: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for (kind, field, spec_field) in [("init", "initContainerStatuses", "initContainers"), ("main", "containerStatuses", "containers")] {
        for spec in pod.pointer(&format!("/spec/{spec_field}")).and_then(Value::as_array).into_iter().flatten() {
            let name = s(spec, "/name");
            let st = pod.pointer(&format!("/status/{field}")).and_then(Value::as_array).and_then(|a| a.iter().find(|c| c["name"] == name)).cloned().unwrap_or(Value::Null);
            let state = match st.get("state") {
                Some(v) if v.get("running").is_some() => json!({"running": {"startedAt": v["running"]["startedAt"]}}),
                Some(v) if v.get("waiting").is_some() => json!({"waiting": v["waiting"]}),
                Some(v) if v.get("terminated").is_some() => json!({"terminated": v["terminated"]}),
                _ => Value::Null,
            };
            let mut c = json!({
                "name": name,
                "type": kind,
                "image": spec["image"],
                "ready": st["ready"],
                "restartCount": st["restartCount"],
                "state": state,
            });
            if let Some(t) = st.pointer("/lastState/terminated") {
                let mut t = t.clone();
                t.as_object_mut().map(|m| m.remove("containerID"));
                c["lastTerminated"] = t;
            }
            for p in ["startupProbe", "readinessProbe", "livenessProbe"] {
                if let Some(pr) = spec.get(p).filter(|v| v.is_object()) {
                    c[p] = json!(describe_probe(pr));
                }
            }
            if let Some(r) = spec.get("resources").filter(|r| r.as_object().is_some_and(|o| !o.is_empty())) {
                c["resources"] = r.clone();
            }
            out.push(c);
        }
    }
    out
}

/// Findings, most important first: why the pod is not ready (or that it
/// is). `node` is the pod's Node object when it has one, `events` the pod's.
pub fn analyze(pod: &Value, node: Option<&Value>, events: &[Value], now: i64) -> Vec<Value> {
    let mut f = Vec::new();
    let phase = s(pod, "/status/phase");
    let conditions: Vec<&Value> = pod.pointer("/status/conditions").and_then(Value::as_array).into_iter().flatten().collect();
    let cond = |t: &str| conditions.iter().find(|c| c["type"] == t).copied();
    let age = |t: &str| chrono::DateTime::parse_from_rfc3339(t).map(|d| now - d.timestamp()).ok();

    if let Some(t) = pod.pointer("/metadata/deletionTimestamp").and_then(Value::as_str) {
        f.push(finding("warning", "Terminating", format!("the pod is being deleted (since {t}); it will not become ready"), "If it hangs, check finalizers and whether its node is reachable."));
    }
    if matches!(phase, "Failed" | "Succeeded") {
        let reason = s(pod, "/status/reason");
        let msg = s(pod, "/status/message");
        let sev = if phase == "Failed" { "error" } else { "info" };
        f.push(finding(sev, if reason.is_empty() { phase } else { reason }, format!("the pod finished: phase {phase}{}", if msg.is_empty() { String::new() } else { format!(": {msg}") }), "A finished pod never becomes ready again; its controller (if any) makes a new one."));
    }

    // Scheduling.
    let node_name = s(pod, "/spec/nodeName");
    if node_name.is_empty() && phase == "Pending" {
        match cond("PodScheduled") {
            Some(c) if c["status"] == "False" => f.push(finding(
                "error",
                if s(c, "/reason").is_empty() { "Unschedulable" } else { s(c, "/reason") },
                format!("no node can run it: {}", s(c, "/message")),
                "Compare its requests, nodeSelector, affinity and tolerations with the nodes (get kind=nodes); a dev cluster gets more nodes with cluster_up nodes=N.",
            )),
            _ => f.push(finding("warning", "NotScheduled", "the pod has no node yet".into(), "Check that the cluster has Ready nodes (get kind=nodes) and look at the events.")),
        }
    }

    // The node.
    if let Some(n) = node {
        let ready = n.pointer("/status/conditions").and_then(Value::as_array).and_then(|cs| cs.iter().find(|c| c["type"] == "Ready")).cloned();
        if let Some(r) = ready.filter(|r| r["status"] != "True") {
            f.push(finding("error", "NodeNotReady", format!("its node {node_name} is not Ready ({}: {})", s(&r, "/reason"), s(&r, "/message")), "The node's agent stopped heartbeating; on a dev cluster see cluster_status (a stopped worker comes back with cluster_up node=<name>)."));
        }
        if n.pointer("/spec/unschedulable").and_then(Value::as_bool) == Some(true) {
            f.push(finding("info", "NodeCordoned", format!("its node {node_name} is cordoned"), "New pods will not go there."));
        }
    }

    // Containers.
    let specs = |field: &str| pod.pointer(&format!("/spec/{field}")).and_then(Value::as_array).cloned().unwrap_or_default();
    let statuses = |field: &str| pod.pointer(&format!("/status/{field}")).and_then(Value::as_array).cloned().unwrap_or_default();
    for (init, spec_field, status_field) in [(true, "initContainers", "initContainerStatuses"), (false, "containers", "containerStatuses")] {
        let st = statuses(status_field);
        for spec in specs(spec_field) {
            let name = s(&spec, "/name").to_string();
            let what = if init { format!("init container {name}") } else { format!("container {name}") };
            let Some(c) = st.iter().find(|c| c["name"] == name.as_str()) else { continue };
            let restarts = c["restartCount"].as_i64().unwrap_or(0);
            let last = c.pointer("/lastState/terminated");
            let last_desc = last.map(|t| {
                let code = t["exitCode"].as_i64().unwrap_or(-1);
                let reason = s(t, "/reason");
                format!("last exit code {code} ({}{}){}", if reason.is_empty() { String::new() } else { format!("{reason}: ") }, exit_meaning(code, reason), t["finishedAt"].as_str().map(|f| format!(" at {f}")).unwrap_or_default())
            });
            if let Some(w) = c.pointer("/state/waiting") {
                let reason = s(w, "/reason");
                let msg = s(w, "/message");
                let detail = if msg.is_empty() { String::new() } else { format!(": {msg}") };
                match reason {
                    "ErrImagePull" | "ImagePullBackOff" | "InvalidImageName" | "ErrImageNeverPull" => f.push(finding(
                        "error",
                        reason,
                        format!("{what} cannot pull image {}{detail}", spec["image"]),
                        "Check the image name and tag, the registry's CA and credentials (the nodes' registry configuration, registries.yaml: pods' imagePullSecrets are not used), and for a locally built image `litek8s dev load --name <cluster> <image>` (or build it with the cluster's Docker socket).",
                    )),
                    "CrashLoopBackOff" => f.push(finding(
                        "error",
                        reason,
                        format!("{what} keeps exiting ({restarts} restarts){}", last_desc.as_ref().map(|d| format!("; {d}")).unwrap_or_default()),
                        if last.is_some_and(|t| t["reason"] == "OOMKilled") { "Raise its memory limit or lower its usage." } else { "Read previous_logs below: the reason it exited is usually in its last lines." },
                    )),
                    "CreateContainerConfigError" | "CreateContainerError" | "RunContainerError" => f.push(finding("error", reason, format!("{what} cannot be created{detail}"), "Usually a missing ConfigMap/Secret (or key) referenced by env/envFrom/volumes, or a bad command: create what it names or fix the spec.")),
                    "PodInitializing" | "ContainerCreating" => {
                        let started = cond("PodScheduled").and_then(|c| c["lastTransitionTime"].as_str()).and_then(age).unwrap_or(0);
                        if started > 120 {
                            f.push(finding("warning", reason, format!("{what} has been {reason} for {started}s"), "Slow image pull or a volume that cannot be mounted (a PVC that is not Bound, a missing ConfigMap/Secret): see events."));
                        }
                    }
                    _ => f.push(finding("warning", if reason.is_empty() { "Waiting" } else { reason }, format!("{what} is waiting{detail}"), "See events and the container state.")),
                }
            } else if let Some(t) = c.pointer("/state/terminated") {
                let code = t["exitCode"].as_i64().unwrap_or(-1);
                let reason = s(t, "/reason");
                if !(init && code == 0) && !(phase == "Succeeded") {
                    f.push(finding(if code == 0 { "warning" } else { "error" }, if reason.is_empty() { "Terminated" } else { reason }, format!("{what} exited with code {code}: {}", exit_meaning(code, reason)), "Read its logs (previous_logs if it restarted)."));
                }
            } else if c.pointer("/state/running").is_some() && !init {
                let ready = c["ready"].as_bool().unwrap_or(false);
                let running_for = c.pointer("/state/running/startedAt").and_then(Value::as_str).and_then(age);
                if !ready {
                    if c["started"] == false && spec.get("startupProbe").is_some_and(Value::is_object) {
                        f.push(finding("warning", "StartupProbeFailing", format!("{what} runs but its startup probe has not passed yet: {}", describe_probe(&spec["startupProbe"])), "The app is slow to start or the probe points at the wrong port/path; after failureThreshold failures the container is restarted."));
                    } else if let Some(p) = spec.get("readinessProbe").filter(|p| p.is_object()) {
                        let delay = p["initialDelaySeconds"].as_i64().unwrap_or(0);
                        if running_for.is_some_and(|r| r < delay + p["periodSeconds"].as_i64().unwrap_or(10)) {
                            f.push(finding("info", "Starting", format!("{what} started {}s ago; its readiness probe has not run long enough", running_for.unwrap_or(0)), "Wait a probe period or two and check again."));
                        } else {
                            f.push(finding(
                                "error",
                                "ReadinessProbeFailing",
                                format!("{what} is running{} but its readiness probe fails: {}", running_for.map(|r| format!(" ({r}s)")).unwrap_or_default(), describe_probe(p)),
                                "Check that the app listens on that port and path inside the pod (its logs; exec), and that the probe's timeout fits. litek8s does not record probe output in events.",
                            ));
                        }
                    } else {
                        f.push(finding("warning", "NotReady", format!("{what} is running but not ready"), "It has no readiness probe: it should turn ready right after starting; see events."));
                    }
                }
                if restarts > 0 {
                    let liveness = spec.get("livenessProbe").filter(|p| p.is_object());
                    let killed = last.and_then(|t| t["exitCode"].as_i64()).is_some_and(|c| c == 137 || c == 143);
                    let hint = if killed && liveness.is_some() {
                        format!("Exit {} with a liveness probe ({}) suggests the probe killed it.", last.and_then(|t| t["exitCode"].as_i64()).unwrap_or(0), describe_probe(liveness.unwrap()))
                    } else {
                        "previous_logs shows the run before the last restart.".to_string()
                    };
                    f.push(finding("warning", "Restarted", format!("{what} restarted {restarts} times; {}", last_desc.clone().unwrap_or_else(|| "no last state recorded".into())), &hint));
                }
            }
        }
    }

    // Warning events not explained above.
    for e in events.iter().filter(|e| e["type"] == "Warning").rev().take(5) {
        let reason = s(e, "/reason");
        let msg = s(e, "/message");
        // Already explained (FailedScheduling repeats PodScheduled's message).
        if f.iter().any(|x| x["reason"] == reason || (!msg.is_empty() && x["message"].as_str().is_some_and(|m| m.contains(msg)))) {
            continue;
        }
        f.push(finding("warning", reason, format!("event: {}", s(e, "/message")), "See events."));
    }

    if f.iter().all(|x| x["severity"] == "info") {
        if let Some(c) = cond("Ready").filter(|c| c["status"] == "True") {
            f.insert(0, finding("info", "Ready", format!("the pod is Ready (since {})", s(c, "/lastTransitionTime")), "Nothing to fix here; if traffic still fails, check the Service selector and its endpoints (describe kind=service)."));
        } else if f.is_empty() {
            f.push(finding("warning", "Unknown", format!("phase {phase}, not ready, and no container state explains it"), "See conditions and events; the node agent may not have reported yet."));
        }
    }
    // Errors first, then warnings, then info.
    let rank = |v: &Value| match v["severity"].as_str() {
        Some("error") => 0,
        Some("warning") => 1,
        _ => 2,
    };
    f.sort_by_key(rank);
    f
}

/// Events in a compact form, newest last; repeats (the same type, reason,
/// message and object) folded into one with a count.
pub fn compact_events(events: &[Value]) -> Vec<Value> {
    let when = |e: &Value| e["lastTimestamp"].as_str().or(e["eventTime"].as_str()).or(e.pointer("/metadata/creationTimestamp").and_then(Value::as_str)).unwrap_or_default().to_string();
    let mut folded: Vec<Value> = Vec::new();
    for e in events {
        let k = s(e, "/involvedObject/kind");
        let object = if k.is_empty() { String::new() } else { format!("{}/{}", k.to_lowercase(), s(e, "/involvedObject/name")) };
        let count = e["count"].as_i64().unwrap_or(1).max(1);
        let last = when(e);
        if let Some(f) = folded.iter_mut().find(|f| f["type"] == e["type"] && f["reason"] == e["reason"] && f["message"] == e["message"] && f["object"].as_str().unwrap_or_default() == object) {
            f["count"] = json!(f["count"].as_i64().unwrap_or(1) + count);
            if last > f["last"].as_str().unwrap_or_default().to_string() {
                f["last"] = json!(last);
            }
            continue;
        }
        let mut c = json!({"type": e["type"], "reason": e["reason"], "message": e["message"], "last": last, "count": count});
        let from = e.pointer("/source/component").or(e.get("reportingController")).and_then(Value::as_str).unwrap_or_default();
        if !from.is_empty() {
            c["from"] = json!(from);
        }
        if !object.is_empty() {
            c["object"] = json!(object);
        }
        folded.push(c);
    }
    folded.sort_by_key(|c| c["last"].as_str().unwrap_or_default().to_string());
    for c in &mut folded {
        if c["count"] == 1 {
            c.as_object_mut().map(|m| m.remove("count"));
        }
    }
    folded
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn ts(ago: i64) -> String {
        chrono::DateTime::from_timestamp(NOW - ago, 0).unwrap().to_rfc3339()
    }

    fn pod(container_status: Value, spec_extra: Value) -> Value {
        let mut c = json!({"name": "app", "image": "web:1"});
        if let (Some(m), Some(e)) = (c.as_object_mut(), spec_extra.as_object()) {
            m.extend(e.clone());
        }
        json!({
            "metadata": {"name": "web-1", "namespace": "default"},
            "spec": {"nodeName": "n1", "containers": [c]},
            "status": {"phase": "Running", "conditions": [{"type": "PodScheduled", "status": "True", "lastTransitionTime": ts(600)}, {"type": "Ready", "status": "False"}], "containerStatuses": [container_status]},
        })
    }

    fn reasons(f: &[Value]) -> Vec<&str> {
        f.iter().map(|x| x["reason"].as_str().unwrap()).collect()
    }

    #[test]
    fn image_pull_errors_name_the_image_and_dev_load() {
        let p = pod(json!({"name": "app", "ready": false, "restartCount": 0, "state": {"waiting": {"reason": "ImagePullBackOff", "message": "pull web:1: not found"}}}), json!({}));
        let f = analyze(&p, None, &[], NOW);
        assert_eq!(reasons(&f)[0], "ImagePullBackOff");
        assert!(f[0]["message"].as_str().unwrap().contains("web:1"));
        assert!(f[0]["next_step"].as_str().unwrap().contains("dev load"));
    }

    #[test]
    fn crash_loops_explain_the_last_exit() {
        let p = pod(
            json!({"name": "app", "ready": false, "restartCount": 4, "state": {"waiting": {"reason": "CrashLoopBackOff"}}, "lastState": {"terminated": {"exitCode": 137, "reason": "OOMKilled", "finishedAt": ts(30)}}}),
            json!({}),
        );
        let f = analyze(&p, None, &[], NOW);
        assert_eq!(reasons(&f)[0], "CrashLoopBackOff");
        let m = f[0]["message"].as_str().unwrap();
        assert!(m.contains("4 restarts") && m.contains("137") && m.contains("memory limit"), "{m}");
        assert!(f[0]["next_step"].as_str().unwrap().contains("memory limit"));
        let p = pod(json!({"name": "app", "ready": false, "restartCount": 2, "state": {"waiting": {"reason": "CrashLoopBackOff"}}, "lastState": {"terminated": {"exitCode": 127}}}), json!({}));
        assert!(analyze(&p, None, &[], NOW)[0]["message"].as_str().unwrap().contains("command not found"));
    }

    #[test]
    fn failing_readiness_probes_are_described() {
        let probe = json!({"readinessProbe": {"httpGet": {"path": "/healthz", "port": 8080}, "periodSeconds": 5}});
        let p = pod(json!({"name": "app", "ready": false, "started": true, "restartCount": 0, "state": {"running": {"startedAt": ts(300)}}}), probe.clone());
        let f = analyze(&p, None, &[], NOW);
        assert_eq!(reasons(&f), ["ReadinessProbeFailing"]);
        assert!(f[0]["message"].as_str().unwrap().contains("HTTP GET :8080/healthz"), "{}", f[0]["message"]);
        // Just started: not a failure yet.
        let p = pod(json!({"name": "app", "ready": false, "started": true, "restartCount": 0, "state": {"running": {"startedAt": ts(2)}}}), probe);
        assert_eq!(reasons(&analyze(&p, None, &[], NOW)), ["Starting"]);
    }

    #[test]
    fn liveness_kills_show_up_as_restarts() {
        let p = pod(
            json!({"name": "app", "ready": true, "restartCount": 3, "state": {"running": {"startedAt": ts(20)}}, "lastState": {"terminated": {"exitCode": 137, "reason": "Error"}}}),
            json!({"livenessProbe": {"tcpSocket": {"port": "http"}}}),
        );
        let f = analyze(&p, None, &[], NOW);
        assert_eq!(reasons(&f), ["Restarted"]);
        assert!(f[0]["next_step"].as_str().unwrap().contains("liveness"), "{}", f[0]);
    }

    #[test]
    fn config_errors_unschedulable_pods_and_bad_nodes() {
        let p = pod(json!({"name": "app", "ready": false, "restartCount": 0, "state": {"waiting": {"reason": "CreateContainerConfigError", "message": "configmap \"cfg\" not found"}}}), json!({}));
        let f = analyze(&p, None, &[], NOW);
        assert_eq!(reasons(&f)[0], "CreateContainerConfigError");
        assert!(f[0]["message"].as_str().unwrap().contains("cfg"));

        let pending = json!({
            "metadata": {"name": "p"},
            "spec": {"containers": [{"name": "app", "image": "x"}]},
            "status": {"phase": "Pending", "conditions": [{"type": "PodScheduled", "status": "False", "reason": "Unschedulable", "message": "0/1 nodes are available: 1 Insufficient memory."}]},
        });
        let f = analyze(&pending, None, &[], NOW);
        assert_eq!(reasons(&f), ["Unschedulable"]);
        assert!(f[0]["message"].as_str().unwrap().contains("Insufficient memory"));
        let ev = json!({"type": "Warning", "reason": "FailedScheduling", "message": "0/1 nodes are available: 1 Insufficient memory."});
        assert_eq!(reasons(&analyze(&pending, None, &[ev], NOW)), ["Unschedulable"], "the event says the same");

        let p = pod(json!({"name": "app", "ready": false, "restartCount": 0, "state": {"running": {"startedAt": ts(100)}}}), json!({}));
        let node = json!({"spec": {}, "status": {"conditions": [{"type": "Ready", "status": "Unknown", "reason": "NodeStatusUnknown", "message": "Kubelet stopped posting node status."}]}});
        let f = analyze(&p, Some(&node), &[], NOW);
        assert_eq!(reasons(&f)[0], "NodeNotReady");
    }

    #[test]
    fn a_ready_pod_says_so_and_warnings_from_events_are_kept() {
        let mut p = pod(json!({"name": "app", "ready": true, "restartCount": 0, "state": {"running": {"startedAt": ts(100)}}}), json!({}));
        p["status"]["conditions"][1] = json!({"type": "Ready", "status": "True", "lastTransitionTime": ts(90)});
        assert_eq!(reasons(&analyze(&p, None, &[], NOW)), ["Ready"]);
        let ev = json!({"type": "Warning", "reason": "FailedMount", "message": "secret \"tls\" not found"});
        let f = analyze(&p, None, &[ev], NOW);
        assert_eq!(reasons(&f), ["FailedMount"]);
    }

    #[test]
    fn probes_and_containers_are_summarized() {
        assert_eq!(describe_probe(&json!({"exec": {"command": ["cat", "/ok"]}, "periodSeconds": 3})), "exec [\"cat\", \"/ok\"] (initialDelay 0s, period 3s, timeout 1s, failureThreshold 3)");
        assert!(describe_probe(&json!({"httpGet": {"scheme": "HTTPS", "port": 443, "path": "/"}})).starts_with("HTTP GET https://:443/"));
        assert!(describe_probe(&json!({"httpGet": {"scheme": "HTTP", "port": 80, "path": "/healthz"}})).starts_with("HTTP GET :80/healthz "));
        let p = pod(json!({"name": "app", "ready": false, "restartCount": 1, "state": {"running": {}}, "lastState": {"terminated": {"exitCode": 1}}}), json!({"readinessProbe": {"tcpSocket": {"port": 80}}}));
        let c = container_summary(&p);
        assert_eq!(c[0]["name"], "app");
        assert_eq!(c[0]["lastTerminated"]["exitCode"], 1);
        assert!(c[0]["readinessProbe"].as_str().unwrap().starts_with("TCP connect :80"));
        let ev = compact_events(&[json!({"type": "Normal", "reason": "Started", "message": "m", "lastTimestamp": "2026-01-01T00:00:00Z", "count": 3, "involvedObject": {"kind": "Pod", "name": "p"}})]);
        assert_eq!(ev[0]["object"], "pod/p");
        assert_eq!(ev[0]["count"], 3);
        let started = |t: &str| json!({"type": "Normal", "reason": "Started", "message": "Started container app", "lastTimestamp": t, "involvedObject": {"kind": "Pod", "name": "p"}});
        let ev = compact_events(&[started("2026-01-01T00:00:01Z"), json!({"type": "Warning", "reason": "X", "message": "y", "lastTimestamp": "2026-01-01T00:00:02Z"}), started("2026-01-01T00:00:03Z")]);
        assert_eq!(ev.len(), 2, "{ev:?}");
        assert_eq!((ev[1]["reason"].clone(), ev[1]["count"].clone(), ev[1]["last"].clone()), (json!("Started"), json!(2), json!("2026-01-01T00:00:03Z")));
        assert!(ev[0].get("count").is_none());
    }
}
