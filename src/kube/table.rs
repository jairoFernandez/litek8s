//! Server-side printing: the `meta.k8s.io/v1` Table kubectl asks for with
//! `Accept: application/json;as=Table`. kubectl prints these columns as-is.

use serde_json::{Value, json};

use super::registry::ResourceType;

struct Column {
    name: &'static str,
    type_: &'static str,
    format: &'static str,
    /// Extracts the cell from the object.
    cell: fn(&Value) -> Value,
}

fn s(v: &Value, ptr: &str) -> String {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn count(v: &Value, ptr: &str) -> Value {
    let n = v
        .pointer(ptr)
        .map(|x| match x {
            Value::Object(m) => m.len(),
            Value::Array(a) => a.len(),
            _ => 0,
        })
        .unwrap_or(0);
    json!(n)
}

fn int(v: &Value, ptr: &str) -> i64 {
    v.pointer(ptr).and_then(Value::as_i64).unwrap_or(0)
}

fn name_cell(v: &Value) -> Value {
    json!(s(v, "/metadata/name"))
}

fn age_cell(v: &Value) -> Value {
    json!(age(&s(v, "/metadata/creationTimestamp")))
}

const NAME: Column = Column { name: "Name", type_: "string", format: "name", cell: name_cell };
const AGE: Column = Column { name: "Age", type_: "string", format: "", cell: age_cell };

fn columns_for(kind: &str) -> Vec<Column> {
    match kind {
        "ConfigMap" => vec![
            NAME,
            Column {
                name: "Data",
                type_: "integer",
                format: "",
                cell: |v| {
                    let n = count(v, "/data").as_u64().unwrap_or(0) + count(v, "/binaryData").as_u64().unwrap_or(0);
                    json!(n)
                },
            },
            AGE,
        ],
        "Secret" => vec![
            NAME,
            Column { name: "Type", type_: "string", format: "", cell: |v| json!(s(v, "/type")) },
            Column { name: "Data", type_: "integer", format: "", cell: |v| count(v, "/data") },
            AGE,
        ],
        "Namespace" => vec![
            NAME,
            Column { name: "Status", type_: "string", format: "", cell: |v| json!(s(v, "/status/phase")) },
            AGE,
        ],
        "ServiceAccount" => vec![
            NAME,
            Column { name: "Secrets", type_: "integer", format: "", cell: |v| count(v, "/secrets") },
            AGE,
        ],
        "Deployment" => vec![
            NAME,
            Column {
                name: "Ready",
                type_: "string",
                format: "",
                cell: |v| json!(format!("{}/{}", int(v, "/status/readyReplicas"), replicas(v))),
            },
            Column { name: "Up-to-date", type_: "integer", format: "", cell: |v| json!(int(v, "/status/updatedReplicas")) },
            Column { name: "Available", type_: "integer", format: "", cell: |v| json!(int(v, "/status/availableReplicas")) },
            AGE,
        ],
        "Pod" => vec![
            NAME,
            Column {
                name: "Ready",
                type_: "string",
                format: "",
                cell: |v| {
                    let statuses = v.pointer("/status/containerStatuses").and_then(Value::as_array);
                    let ready = statuses
                        .map(|s| s.iter().filter(|c| c.get("ready") == Some(&json!(true))).count())
                        .unwrap_or(0);
                    json!(format!("{ready}/{}", count(v, "/spec/containers")))
                },
            },
            Column { name: "Status", type_: "string", format: "", cell: |v| json!(pod_status(v)) },
            Column {
                name: "Restarts",
                type_: "string",
                format: "",
                cell: |v| {
                    let n: i64 = v
                        .pointer("/status/containerStatuses")
                        .and_then(Value::as_array)
                        .map(|s| s.iter().map(|c| int(c, "/restartCount")).sum())
                        .unwrap_or(0);
                    json!(n.to_string())
                },
            },
            AGE,
            Column { name: "IP", type_: "string", format: "", cell: |v| json!(or_none(&s(v, "/status/podIP"))) },
            Column { name: "Node", type_: "string", format: "", cell: |v| json!(or_none(&s(v, "/spec/nodeName"))) },
        ],
        "Node" => vec![
            NAME,
            Column {
                name: "Status",
                type_: "string",
                format: "",
                cell: |v| {
                    let ready = v
                        .pointer("/status/conditions")
                        .and_then(Value::as_array)
                        .and_then(|cs| cs.iter().find(|c| c["type"] == "Ready"))
                        .map(|c| c["status"] == "True")
                        .unwrap_or(false);
                    let mut status = if ready { "Ready".to_string() } else { "NotReady".to_string() };
                    if v.pointer("/spec/unschedulable") == Some(&json!(true)) {
                        status.push_str(",SchedulingDisabled");
                    }
                    json!(status)
                },
            },
            Column {
                name: "Roles",
                type_: "string",
                format: "",
                cell: |v| {
                    let roles: Vec<String> = v
                        .pointer("/metadata/labels")
                        .and_then(Value::as_object)
                        .map(|l| l.keys().filter_map(|k| k.strip_prefix("node-role.kubernetes.io/").map(str::to_string)).collect())
                        .unwrap_or_default();
                    json!(if roles.is_empty() { "<none>".to_string() } else { roles.join(",") })
                },
            },
            AGE,
            Column { name: "Version", type_: "string", format: "", cell: |v| json!(s(v, "/status/nodeInfo/kubeletVersion")) },
            Column {
                name: "Internal-IP",
                type_: "string",
                format: "",
                cell: |v| {
                    let ip = v
                        .pointer("/status/addresses")
                        .and_then(Value::as_array)
                        .and_then(|a| a.iter().find(|x| x["type"] == "InternalIP"))
                        .map(|x| s(x, "/address"))
                        .unwrap_or_default();
                    json!(or_none(&ip))
                },
            },
            Column { name: "OS-Image", type_: "string", format: "", cell: |v| json!(s(v, "/status/nodeInfo/osImage")) },
            Column { name: "Container-Runtime", type_: "string", format: "", cell: |v| json!(s(v, "/status/nodeInfo/containerRuntimeVersion")) },
        ],
        "ReplicaSet" => vec![
            NAME,
            Column { name: "Desired", type_: "integer", format: "", cell: |v| json!(int(v, "/spec/replicas")) },
            Column { name: "Current", type_: "integer", format: "", cell: |v| json!(int(v, "/status/replicas")) },
            Column { name: "Ready", type_: "integer", format: "", cell: |v| json!(int(v, "/status/readyReplicas")) },
            AGE,
        ],
        "StatefulSet" => vec![
            NAME,
            Column {
                name: "Ready",
                type_: "string",
                format: "",
                cell: |v| json!(format!("{}/{}", int(v, "/status/readyReplicas"), replicas(v))),
            },
            AGE,
        ],
        "DaemonSet" => vec![
            NAME,
            Column { name: "Desired", type_: "integer", format: "", cell: |v| json!(int(v, "/status/desiredNumberScheduled")) },
            Column { name: "Current", type_: "integer", format: "", cell: |v| json!(int(v, "/status/currentNumberScheduled")) },
            Column { name: "Ready", type_: "integer", format: "", cell: |v| json!(int(v, "/status/numberReady")) },
            Column { name: "Up-to-date", type_: "integer", format: "", cell: |v| json!(int(v, "/status/updatedNumberScheduled")) },
            Column { name: "Available", type_: "integer", format: "", cell: |v| json!(int(v, "/status/numberAvailable")) },
            AGE,
        ],
        "Job" => vec![
            NAME,
            Column {
                name: "Status",
                type_: "string",
                format: "",
                cell: |v| {
                    let has = |t: &str| {
                        v.pointer("/status/conditions")
                            .and_then(Value::as_array)
                            .is_some_and(|cs| cs.iter().any(|c| c["type"] == t && c["status"] == "True"))
                    };
                    json!(if has("Complete") {
                        "Complete"
                    } else if has("Failed") {
                        "Failed"
                    } else if has("Suspended") {
                        "Suspended"
                    } else {
                        "Running"
                    })
                },
            },
            Column {
                name: "Completions",
                type_: "string",
                format: "",
                cell: |v| json!(format!("{}/{}", int(v, "/status/succeeded"), v.pointer("/spec/completions").and_then(Value::as_i64).unwrap_or(1))),
            },
            AGE,
        ],
        "CronJob" => vec![
            NAME,
            Column { name: "Schedule", type_: "string", format: "", cell: |v| json!(s(v, "/spec/schedule")) },
            Column {
                name: "Suspend",
                type_: "boolean",
                format: "",
                cell: |v| json!(v.pointer("/spec/suspend").and_then(Value::as_bool).unwrap_or(false)),
            },
            Column { name: "Active", type_: "integer", format: "", cell: |v| count(v, "/status/active") },
            Column {
                name: "Last Schedule",
                type_: "string",
                format: "",
                cell: |v| {
                    let t = s(v, "/status/lastScheduleTime");
                    json!(if t.is_empty() { "<none>".to_string() } else { age(&t) })
                },
            },
            AGE,
        ],
        "PersistentVolumeClaim" => vec![
            NAME,
            Column { name: "Status", type_: "string", format: "", cell: |v| json!(s(v, "/status/phase")) },
            Column { name: "Volume", type_: "string", format: "", cell: |v| json!(s(v, "/spec/volumeName")) },
            Column { name: "Capacity", type_: "string", format: "", cell: |v| json!(s(v, "/status/capacity/storage")) },
            Column { name: "Storage Class", type_: "string", format: "", cell: |v| json!(s(v, "/spec/storageClassName")) },
            AGE,
        ],
        "PersistentVolume" => vec![
            NAME,
            Column { name: "Capacity", type_: "string", format: "", cell: |v| json!(s(v, "/spec/capacity/storage")) },
            Column { name: "Reclaim Policy", type_: "string", format: "", cell: |v| json!(s(v, "/spec/persistentVolumeReclaimPolicy")) },
            Column { name: "Status", type_: "string", format: "", cell: |v| json!(s(v, "/status/phase")) },
            Column {
                name: "Claim",
                type_: "string",
                format: "",
                cell: |v| json!(format!("{}/{}", s(v, "/spec/claimRef/namespace"), s(v, "/spec/claimRef/name"))),
            },
            AGE,
        ],
        "Service" => vec![
            NAME,
            Column { name: "Type", type_: "string", format: "", cell: |v| json!(s(v, "/spec/type")) },
            Column {
                name: "Cluster-IP",
                type_: "string",
                format: "",
                cell: |v| {
                    let ip = s(v, "/spec/clusterIP");
                    json!(if ip.is_empty() { "<none>".to_string() } else { ip })
                },
            },
            Column {
                name: "External-IP",
                type_: "string",
                format: "",
                cell: |v| {
                    let mut ips: Vec<String> = v
                        .pointer("/status/loadBalancer/ingress")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|i| i.get("ip").or_else(|| i.get("hostname")).and_then(Value::as_str).map(str::to_string))
                        .collect();
                    ips.extend(v.pointer("/spec/externalIPs").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string));
                    json!(match (ips.is_empty(), s(v, "/spec/type").as_str()) {
                        (false, _) => ips.join(","),
                        (true, "LoadBalancer") => "<pending>".to_string(),
                        (true, "ExternalName") => s(v, "/spec/externalName"),
                        _ => "<none>".to_string(),
                    })
                },
            },
            Column {
                name: "Port(s)",
                type_: "string",
                format: "",
                cell: |v| {
                    let ports: Vec<String> = v
                        .pointer("/spec/ports")
                        .and_then(Value::as_array)
                        .map(|ps| {
                            ps.iter()
                                .map(|p| {
                                    let proto = p.get("protocol").and_then(Value::as_str).unwrap_or("TCP");
                                    match p.get("nodePort").and_then(Value::as_u64) {
                                        Some(np) => format!("{}:{np}/{proto}", int(p, "/port")),
                                        None => format!("{}/{proto}", int(p, "/port")),
                                    }
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    json!(if ports.is_empty() { "<none>".to_string() } else { ports.join(",") })
                },
            },
            AGE,
            // `-o wide` (as kubectl): the pods it selects.
            Column {
                name: "Selector",
                type_: "string",
                format: "",
                cell: |v| {
                    let sel: Vec<String> = v.pointer("/spec/selector").and_then(Value::as_object).into_iter().flatten().map(|(k, x)| format!("{k}={}", x.as_str().unwrap_or_default())).collect();
                    json!(if sel.is_empty() { "<none>".to_string() } else { sel.join(",") })
                },
            },
        ],
        _ => vec![NAME, AGE],
    }
}

fn or_none(v: &str) -> String {
    if v.is_empty() { "<none>".to_string() } else { v.to_string() }
}

/// The STATUS kubectl shows for a pod (its printPod logic, simplified).
fn pod_status(v: &Value) -> String {
    let phase = s(v, "/status/phase");
    let mut reason = match s(v, "/status/reason").as_str() {
        "" if phase.is_empty() => "Pending".to_string(),
        "" => phase.clone(),
        r => r.to_string(),
    };
    let describe = |c: &Value, init: bool| -> Option<String> {
        let prefix = if init { "Init:" } else { "" };
        if let Some(t) = c.pointer("/state/terminated") {
            let code = t.get("exitCode").and_then(Value::as_i64).unwrap_or(0);
            if init && code == 0 {
                return None;
            }
            let r = s(t, "/reason");
            return Some(if !r.is_empty() { format!("{prefix}{r}") } else { format!("{prefix}ExitCode:{code}") });
        }
        if let Some(w) = c.pointer("/state/waiting") {
            let r = s(w, "/reason");
            if !r.is_empty() && !(init && r == "PodInitializing") {
                return Some(format!("{prefix}{r}"));
            }
        }
        None
    };
    let inits = v.pointer("/status/initContainerStatuses").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut initializing = false;
    for (i, c) in inits.iter().enumerate() {
        let done = c.pointer("/state/terminated/exitCode").and_then(Value::as_i64) == Some(0);
        let sidecar_running = c.pointer("/state/running").is_some() && c.get("started") == Some(&json!(true));
        if done || sidecar_running {
            continue;
        }
        initializing = true;
        reason = describe(c, true).unwrap_or_else(|| format!("Init:{i}/{}", inits.len()));
        break;
    }
    if !initializing {
        let mut has_running = false;
        let containers = v.pointer("/status/containerStatuses").and_then(Value::as_array).cloned().unwrap_or_default();
        for c in containers.iter().rev() {
            if let Some(r) = describe(c, false) {
                reason = r;
            } else if c.pointer("/state/running").is_some() && c.get("ready") == Some(&json!(true)) {
                has_running = true;
            }
        }
        if reason == "Completed" && has_running {
            reason = "Running".into();
        }
    }
    if v.pointer("/metadata/deletionTimestamp").is_some_and(|t| !t.is_null()) {
        reason = "Terminating".into();
    }
    reason
}

fn replicas(v: &Value) -> i64 {
    v.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or(1)
}

/// Evaluate a simple dotted JSONPath (`.spec.size`, `.status.conditions[0].type`).
fn json_path(v: &Value, path: &str) -> Value {
    let mut cur = v;
    for seg in path.trim_start_matches('.').split('.').filter(|s| !s.is_empty()) {
        let (field, index) = match seg.split_once('[') {
            Some((f, rest)) => (f, rest.trim_end_matches(']').parse::<usize>().ok()),
            None => (seg, None),
        };
        let Some(next) = cur.get(field) else { return Value::Null };
        cur = match index {
            Some(i) => match next.get(i) {
                Some(x) => x,
                None => return Value::Null,
            },
            None => next,
        };
    }
    cur.clone()
}

/// Build a Table for `objects`. `include_object` adds each row's
/// PartialObjectMetadata, which kubectl needs for -A and for watch output.
pub fn build(rt: &ResourceType, objects: &[Value], rv: u64, include_object: bool) -> Value {
    let builtin = columns_for(&rt.kind);
    let mut defs: Vec<Value> = Vec::new();
    let mut extractors: Vec<Box<dyn Fn(&Value) -> Value>> = Vec::new();
    if rt.custom && !rt.printer_columns.is_empty() {
        defs.push(json!({"name": "Name", "type": "string", "format": "name", "description": "", "priority": 0}));
        extractors.push(Box::new(name_cell));
        for col in &rt.printer_columns {
            let path = col.json_path.clone();
            let is_age = col.type_ == "date";
            defs.push(json!({"name": col.name, "type": if is_age { "string" } else { col.type_.as_str() }, "format": "", "description": "", "priority": 0}));
            extractors.push(Box::new(move |v| {
                let cell = json_path(v, &path);
                if is_age {
                    json!(age(cell.as_str().unwrap_or_default()))
                } else {
                    cell
                }
            }));
        }
        if !rt.printer_columns.iter().any(|c| c.name == "Age") {
            defs.push(json!({"name": "Age", "type": "string", "format": "", "description": "", "priority": 0}));
            extractors.push(Box::new(age_cell));
        }
    } else {
        for col in builtin {
            // Columns kubectl shows only with `-o wide`.
            let wide = matches!(
                (rt.kind.as_str(), col.name),
                ("Pod", "IP" | "Node") | ("Node", "Internal-IP" | "OS-Image" | "Container-Runtime") | ("Service", "Selector")
            );
            defs.push(json!({"name": col.name, "type": col.type_, "format": col.format, "description": "", "priority": if wide { 1 } else { 0 }}));
            extractors.push(Box::new(col.cell));
        }
    }
    let rows: Vec<Value> = objects
        .iter()
        .map(|obj| {
            let cells: Vec<Value> = extractors.iter().map(|f| f(obj)).collect();
            let mut row = json!({"cells": cells});
            if include_object {
                row["object"] = json!({
                    "kind": "PartialObjectMetadata",
                    "apiVersion": "meta.k8s.io/v1",
                    "metadata": obj.get("metadata").cloned().unwrap_or(json!({})),
                });
            }
            row
        })
        .collect();
    json!({
        "kind": "Table",
        "apiVersion": "meta.k8s.io/v1",
        "metadata": {"resourceVersion": rv.to_string()},
        "columnDefinitions": defs,
        "rows": rows,
    })
}

/// kubectl's age format (`45s`, `3m12s`, `5h`, `2d3h`, ...), approximately
/// matching k8s.io/apimachinery's HumanDuration.
pub fn age(timestamp: &str) -> String {
    let Ok(created) = chrono::DateTime::parse_from_rfc3339(timestamp) else {
        return "<unknown>".into();
    };
    let secs = (chrono::Utc::now() - created.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    let (m, h, d) = (secs / 60, secs / 3600, secs / 86400);
    match secs {
        s if s < 120 => format!("{s}s"),
        _ if m < 10 => format!("{m}m{}s", secs % 60),
        _ if h < 3 => format!("{m}m"),
        _ if h < 8 => format!("{h}h{}m", m % 60),
        _ if h < 48 => format!("{h}h"),
        _ if d < 8 => format!("{d}d{}h", h % 24),
        _ if d < 365 * 2 => format!("{d}d"),
        _ => format!("{}y", d / 365),
    }
}
