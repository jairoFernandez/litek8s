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
            set_default(p, "timeoutSeconds", json!(1));
            set_default(p, "periodSeconds", json!(10));
            set_default(p, "successThreshold", json!(1));
            set_default(p, "failureThreshold", json!(3));
            if let Some(h) = p.get_mut("httpGet").and_then(Value::as_object_mut) {
                set_default(h, "scheme", json!("HTTP"));
                set_default(h, "path", json!("/"));
            }
        }
    }
}

/// Guaranteed when every container sets equal cpu and memory requests and
/// limits, BestEffort when none sets any, Burstable otherwise.
fn qos_class(spec: &Map<String, Value>) -> &'static str {
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

pub fn statefulset(obj: &mut Value) {
    let spec = obj_entry(obj, "spec");
    set_default(spec, "replicas", json!(1));
    set_default(spec, "podManagementPolicy", json!("OrderedReady"));
    set_default(spec, "updateStrategy", json!({"type": "RollingUpdate", "rollingUpdate": {"partition": 0}}));
    set_default(spec, "revisionHistoryLimit", json!(10));
    set_default(
        spec,
        "persistentVolumeClaimRetentionPolicy",
        json!({"whenDeleted": "Retain", "whenScaled": "Retain"}),
    );
}

pub fn daemonset(obj: &mut Value) {
    let spec = obj_entry(obj, "spec");
    set_default(
        spec,
        "updateStrategy",
        json!({"type": "RollingUpdate", "rollingUpdate": {"maxUnavailable": 1, "maxSurge": 0}}),
    );
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
