//! Defaulting the real API server applies to workload objects, plus the two
//! admission plugins every pod goes through (ServiceAccount token injection
//! and DefaultTolerationSeconds). Controllers and the node agent rely on these
//! fields being present, as their Kubernetes counterparts do.

use serde_json::{Map, Value, json};

use super::quantity;

pub const SA_MOUNT_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount";
pub const ROOT_CA_CONFIGMAP: &str = "kube-root-ca.crt";
pub const DEFAULT_STORAGE_CLASS: &str = "local-path";

fn obj_entry<'a>(map: &'a mut Value, key: &str) -> &'a mut Map<String, Value> {
    if !map.get(key).is_some_and(Value::is_object) {
        map[key] = json!({});
    }
    map[key].as_object_mut().unwrap()
}

fn set_default(map: &mut Map<String, Value>, key: &str, value: Value) {
    if map.get(key).is_none_or(Value::is_null) {
        map.insert(key.into(), value);
    }
}

/// `serviceAccount` is the pod's ServiceAccount object, if it exists (for
/// `automountServiceAccountToken: false`). `suffix` names the token volume.
/// `toleration_seconds` are the default not-ready and unreachable
/// tolerations (DefaultTolerationSeconds).
pub fn pod_on_create(pod: &mut Value, service_account: Option<&Value>, suffix: &str, toleration_seconds: (u64, u64)) {
    let spec = obj_entry(pod, "spec");
    set_default(spec, "restartPolicy", json!("Always"));
    set_default(spec, "dnsPolicy", json!("ClusterFirst"));
    set_default(spec, "schedulerName", json!("default-scheduler"));
    set_default(spec, "terminationGracePeriodSeconds", json!(30));
    set_default(spec, "securityContext", json!({}));
    set_default(spec, "enableServiceLinks", json!(true));
    set_default(spec, "priority", json!(0));
    set_default(spec, "preemptionPolicy", json!("PreemptLowerPriority"));
    let sa = spec
        .get("serviceAccountName")
        .or_else(|| spec.get("serviceAccount"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .to_string();
    spec.insert("serviceAccountName".into(), json!(sa));
    spec.insert("serviceAccount".into(), json!(sa));

    let tolerations = spec
        .entry("tolerations")
        .or_insert_with(|| json!([]));
    if let Some(list) = tolerations.as_array_mut() {
        let (not_ready, unreachable) = toleration_seconds;
        for (taint, seconds) in [("node.kubernetes.io/not-ready", not_ready), ("node.kubernetes.io/unreachable", unreachable)] {
            let present = list.iter().any(|t| {
                t.get("key").and_then(Value::as_str) == Some(taint)
                    || (t.get("operator").and_then(Value::as_str) == Some("Exists") && t.get("key").is_none())
            });
            if !present {
                list.push(json!({"key": taint, "operator": "Exists", "effect": "NoExecute", "tolerationSeconds": seconds}));
            }
        }
    }

    for field in ["initContainers", "containers", "ephemeralContainers"] {
        if let Some(containers) = spec.get_mut(field).and_then(Value::as_array_mut) {
            for c in containers {
                container(c);
            }
        }
    }

    let automount = spec
        .get("automountServiceAccountToken")
        .and_then(Value::as_bool)
        .or_else(|| service_account.and_then(|sa| sa.get("automountServiceAccountToken")).and_then(Value::as_bool))
        .unwrap_or(true);
    if automount {
        inject_service_account_token(spec, suffix);
    }

    let qos = qos_class(spec);
    pod["status"] = json!({"phase": "Pending", "qosClass": qos});
}

fn inject_service_account_token(spec: &mut Map<String, Value>, suffix: &str) {
    let already = ["containers", "initContainers"].iter().any(|field| {
        spec.get(*field).and_then(Value::as_array).is_some_and(|cs| {
            cs.iter().any(|c| {
                c.get("volumeMounts")
                    .and_then(Value::as_array)
                    .is_some_and(|ms| ms.iter().any(|m| m.get("mountPath").and_then(Value::as_str) == Some(SA_MOUNT_PATH)))
            })
        })
    });
    if already {
        return;
    }
    let volume = format!("kube-api-access-{suffix}");
    let volumes = spec.entry("volumes").or_insert_with(|| json!([]));
    if let Some(vs) = volumes.as_array_mut() {
        vs.push(json!({
            "name": volume,
            "projected": {
                "defaultMode": 420,
                "sources": [
                    {"serviceAccountToken": {"expirationSeconds": 3607, "path": "token"}},
                    {"configMap": {"name": ROOT_CA_CONFIGMAP, "items": [{"key": "ca.crt", "path": "ca.crt"}]}},
                    {"downwardAPI": {"items": [{"path": "namespace", "fieldRef": {"apiVersion": "v1", "fieldPath": "metadata.namespace"}}]}},
                ],
            },
        }));
    }
    for field in ["initContainers", "containers"] {
        if let Some(cs) = spec.get_mut(field).and_then(Value::as_array_mut) {
            for c in cs {
                let mounts = c
                    .as_object_mut()
                    .map(|m| m.entry("volumeMounts").or_insert_with(|| json!([])));
                if let Some(Value::Array(ms)) = mounts {
                    ms.push(json!({"name": volume, "mountPath": SA_MOUNT_PATH, "readOnly": true}));
                }
            }
        }
    }
}

/// Container defaults for every element of a container list.
pub fn containers(list: &mut Value) {
    for c in list.as_array_mut().into_iter().flatten() {
        container(c);
    }
}

/// The defaults the API server gives a pod template (Deployment,
/// ReplicaSet, StatefulSet, DaemonSet, Job, CronJob) at `ptr`, as for pods
/// minus what admission adds to pods only (service account, tolerations).
/// Clients rely on them: `kubectl patch` with a JSON patch `replace` of
/// `.../containers/0/imagePullPolicy` needs the field to exist.
pub fn pod_template(obj: &mut Value, ptr: &str) {
    let Some(spec) = obj.pointer_mut(&format!("{ptr}/spec")).and_then(Value::as_object_mut) else { return };
    set_default(spec, "restartPolicy", json!("Always"));
    set_default(spec, "dnsPolicy", json!("ClusterFirst"));
    set_default(spec, "schedulerName", json!("default-scheduler"));
    set_default(spec, "terminationGracePeriodSeconds", json!(30));
    set_default(spec, "securityContext", json!({}));
    for field in ["initContainers", "containers"] {
        if let Some(containers) = spec.get_mut(field).and_then(Value::as_array_mut) {
            for c in containers {
                container(c);
            }
        }
    }
}

fn container(c: &mut Value) {
    let Some(m) = c.as_object_mut() else { return };
    let image = m.get("image").and_then(Value::as_str).unwrap_or_default().to_string();
    let tagged = image
        .rsplit('/')
        .next()
        .is_some_and(|last| last.contains(':') || last.contains('@'));
    let latest = !tagged || image.ends_with(":latest");
    set_default(m, "imagePullPolicy", json!(if latest { "Always" } else { "IfNotPresent" }));
    set_default(m, "terminationMessagePath", json!("/dev/termination-log"));
    set_default(m, "terminationMessagePolicy", json!("File"));
    set_default(m, "resources", json!({}));
    if let Some(ports) = m.get_mut("ports").and_then(Value::as_array_mut) {
        for p in ports {
            if let Some(pm) = p.as_object_mut() {
                set_default(pm, "protocol", json!("TCP"));
            }
        }
    }
    for probe in ["readinessProbe", "livenessProbe", "startupProbe"] {
        if let Some(p) = m.get_mut(probe).and_then(Value::as_object_mut) {
            // 0 is "unset" too: protobuf clients send these non-pointer fields.
            for (k, v) in [("timeoutSeconds", 1), ("periodSeconds", 10), ("successThreshold", 1), ("failureThreshold", 3)] {
                if p.get(k).is_none_or(|x| *x == json!(0)) {
                    p.insert(k.into(), json!(v));
                }
            }
            if let Some(h) = p.get_mut("httpGet").and_then(Value::as_object_mut) {
                set_default(h, "scheme", json!("HTTP"));
                set_default(h, "path", json!("/"));
            }
        }
    }
}

/// Guaranteed when every container sets equal cpu and memory requests and
/// limits, BestEffort when none sets any, Burstable otherwise.
pub fn qos_class(spec: &Map<String, Value>) -> &'static str {
    let containers: Vec<&Value> = ["initContainers", "containers"]
        .iter()
        .filter_map(|f| spec.get(*f).and_then(Value::as_array))
        .flatten()
        .collect();
    let mut any = false;
    let mut guaranteed = !containers.is_empty();
    for c in &containers {
        let requests = c.pointer("/resources/requests").and_then(Value::as_object);
        let limits = c.pointer("/resources/limits").and_then(Value::as_object);
        if requests.is_some_and(|r| !r.is_empty()) || limits.is_some_and(|l| !l.is_empty()) {
            any = true;
        }
        for res in ["cpu", "memory"] {
            let limit = limits.and_then(|l| l.get(res)).and_then(quantity::milli_of);
            // A request defaults to the limit.
            let request = requests.and_then(|r| r.get(res)).and_then(quantity::milli_of).or(limit);
            if limit.is_none() || request != limit {
                guaranteed = false;
            }
        }
    }
    if !any {
        "BestEffort"
    } else if guaranteed {
        "Guaranteed"
    } else {
        "Burstable"
    }
}

pub fn job(obj: &mut Value, old: Option<&Value>) {
    let uid = obj.pointer("/metadata/uid").and_then(Value::as_str).unwrap_or_default().to_string();
    let name = obj.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string();
    let spec = obj_entry(obj, "spec");
    if spec.get("parallelism").is_none() && spec.get("completions").is_none() {
        spec.insert("completions".into(), json!(1));
    }
    set_default(spec, "parallelism", json!(1));
    set_default(spec, "backoffLimit", json!(6));
    set_default(spec, "completionMode", json!("NonIndexed"));
    set_default(spec, "suspend", json!(false));
    set_default(spec, "podReplacementPolicy", json!("TerminatingOrFailed"));
    let manual = spec.get("manualSelector").and_then(Value::as_bool) == Some(true);
    if manual {
        return;
    }
    // The selector and the template labels are generated once and immutable.
    if let Some(old_selector) = old.and_then(|o| o.pointer("/spec/selector")) {
        spec.insert("selector".into(), old_selector.clone());
    } else {
        spec.insert(
            "selector".into(),
            json!({"matchLabels": {"batch.kubernetes.io/controller-uid": uid}}),
        );
    }
    let uid = spec
        .get("selector")
        .and_then(|s| s.pointer("/matchLabels/batch.kubernetes.io~1controller-uid"))
        .and_then(Value::as_str)
        .unwrap_or(&uid)
        .to_string();
    if !spec.get("template").is_some_and(Value::is_object) {
        spec.insert("template".into(), json!({}));
    }
    let template = &mut spec["template"];
    let labels = obj_entry(obj_entry_value(template, "metadata"), "labels");
    for (k, v) in [
        ("batch.kubernetes.io/controller-uid", &uid),
        ("batch.kubernetes.io/job-name", &name),
        ("controller-uid", &uid),
        ("job-name", &name),
    ] {
        labels.insert(k.into(), json!(v));
    }
}

fn obj_entry_value<'a>(map: &'a mut Value, key: &str) -> &'a mut Value {
    if !map.get(key).is_some_and(Value::is_object) {
        map[key] = json!({});
    }
    &mut map[key]
}

pub fn cronjob(obj: &mut Value) {
    let spec = obj_entry(obj, "spec");
    set_default(spec, "concurrencyPolicy", json!("Allow"));
    set_default(spec, "suspend", json!(false));
    set_default(spec, "successfulJobsHistoryLimit", json!(3));
    set_default(spec, "failedJobsHistoryLimit", json!(1));
}

/// Fill in a RollingUpdate strategy's parameters (`defaults`) when the
/// object names the type, or the whole strategy when it names nothing.
fn rolling_update(spec: &mut Map<String, Value>, field: &str, defaults: Value) {
    let strategy = spec.entry(field).or_insert_with(|| json!({}));
    if !strategy.is_object() {
        *strategy = json!({});
    }
    let s = strategy.as_object_mut().unwrap();
    if s.get("type").and_then(Value::as_str).is_none_or(str::is_empty) {
        s.insert("type".into(), json!("RollingUpdate"));
    }
    if s.get("type").and_then(Value::as_str) != Some("RollingUpdate") {
        return;
    }
    let ru = s.entry("rollingUpdate").or_insert_with(|| json!({}));
    if !ru.is_object() {
        *ru = json!({});
    }
    let ru = ru.as_object_mut().unwrap();
    for (k, v) in defaults.as_object().into_iter().flatten() {
        if ru.get(k).is_none_or(Value::is_null) {
            ru.insert(k.clone(), v.clone());
        }
    }
}

pub fn statefulset(obj: &mut Value) {
    let spec = obj_entry(obj, "spec");
    set_default(spec, "replicas", json!(1));
    set_default(spec, "podManagementPolicy", json!("OrderedReady"));
    rolling_update(spec, "updateStrategy", json!({"partition": 0}));
    set_default(spec, "revisionHistoryLimit", json!(10));
    set_default(
        spec,
        "persistentVolumeClaimRetentionPolicy",
        json!({"whenDeleted": "Retain", "whenScaled": "Retain"}),
    );
}

pub fn daemonset(obj: &mut Value) {
    let spec = obj_entry(obj, "spec");
    rolling_update(spec, "updateStrategy", json!({"maxUnavailable": 1, "maxSurge": 0}));
    set_default(spec, "revisionHistoryLimit", json!(10));
}

pub fn pvc(obj: &mut Value, old: Option<&Value>) {
    let spec = obj_entry(obj, "spec");
    set_default(spec, "volumeMode", json!("Filesystem"));
    if old.is_none() {
        set_default(spec, "storageClassName", json!(DEFAULT_STORAGE_CLASS));
        obj["status"] = json!({"phase": "Pending"});
    }
}

/// A core/v1 Event as events.k8s.io/v1.
pub fn event_to_v1(core: &Value) -> Value {
    let mut out = json!({"apiVersion": "events.k8s.io/v1", "kind": "Event", "metadata": core.get("metadata").cloned().unwrap_or_else(|| json!({}))});
    let copy = |out: &mut Value, from: &str, to: &str| {
        if let Some(v) = core.get(from).filter(|v| !v.is_null()) {
            out[to] = v.clone();
        }
    };
    copy(&mut out, "involvedObject", "regarding");
    copy(&mut out, "related", "related");
    copy(&mut out, "message", "note");
    copy(&mut out, "reason", "reason");
    copy(&mut out, "type", "type");
    copy(&mut out, "action", "action");
    copy(&mut out, "eventTime", "eventTime");
    copy(&mut out, "series", "series");
    copy(&mut out, "reportingComponent", "reportingController");
    copy(&mut out, "reportingInstance", "reportingInstance");
    copy(&mut out, "source", "deprecatedSource");
    copy(&mut out, "firstTimestamp", "deprecatedFirstTimestamp");
    copy(&mut out, "lastTimestamp", "deprecatedLastTimestamp");
    copy(&mut out, "count", "deprecatedCount");
    out
}

/// An events.k8s.io/v1 Event as core/v1.
pub fn event_to_core(v1: &Value) -> Value {
    let mut out = json!({"apiVersion": "v1", "kind": "Event", "metadata": v1.get("metadata").cloned().unwrap_or_else(|| json!({}))});
    let copy = |out: &mut Value, from: &str, to: &str| {
        if let Some(v) = v1.get(from).filter(|v| !v.is_null()) {
            out[to] = v.clone();
        }
    };
    copy(&mut out, "regarding", "involvedObject");
    copy(&mut out, "related", "related");
    copy(&mut out, "note", "message");
    copy(&mut out, "reason", "reason");
    copy(&mut out, "type", "type");
    copy(&mut out, "action", "action");
    copy(&mut out, "eventTime", "eventTime");
    copy(&mut out, "series", "series");
    copy(&mut out, "reportingController", "reportingComponent");
    copy(&mut out, "reportingInstance", "reportingInstance");
    copy(&mut out, "deprecatedSource", "source");
    // core/v1 selects events by source component: the reporting controller.
    if out.pointer("/source/component").is_none() {
        if let Some(rc) = v1.get("reportingController").filter(|v| !v.is_null()) {
            out["source"] = json!({"component": rc});
        }
    }
    copy(&mut out, "deprecatedFirstTimestamp", "firstTimestamp");
    copy(&mut out, "deprecatedLastTimestamp", "lastTimestamp");
    copy(&mut out, "deprecatedCount", "count");
    if out.get("involvedObject").is_none() {
        out["involvedObject"] = json!({});
    }
    out
}
